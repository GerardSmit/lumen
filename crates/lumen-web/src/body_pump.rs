//! One blocking reader per response body. A dedicated thread reads the transport ahead into a
//! bounded queue; the JS side takes everything queued (up to a batch cap) per delivery, so a fast
//! network costs few loop iterations and no thread is spawned per read.
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

/// Stop reading ahead once this many bytes are queued and unconsumed.
pub(crate) const HIGH_WATER: usize = 4 << 20;
/// The most bytes handed to the consumer by one delivery.
pub(crate) const MAX_DELIVERY: usize = 1 << 20;

pub(crate) enum Take {
    Data(Vec<u8>),
    End,
    Failed(String),
}

type Deliver = Box<dyn FnOnce(Take) + Send>;

#[derive(Default)]
struct State {
    queue: VecDeque<Vec<u8>>,
    queued: usize,
    finished: Option<Result<(), String>>,
    cancelled: bool,
    started: bool,
    waiter: Option<Deliver>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    space: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poison| poison.into_inner())
    }
}

#[derive(Clone, Default)]
pub(crate) struct BodyPump {
    shared: Arc<Shared>,
}

fn take(state: &mut State) -> Option<Take> {
    if let Some(first) = state.queue.pop_front() {
        state.queued -= first.len();
        if first.len() >= MAX_DELIVERY {
            return Some(Take::Data(first));
        }
        let mut batch = first;
        while let Some(next) = state.queue.front() {
            if batch.len() + next.len() > MAX_DELIVERY {
                break;
            }
            let next = state.queue.pop_front().expect("front exists");
            state.queued -= next.len();
            batch.extend_from_slice(&next);
        }
        return Some(Take::Data(batch));
    }
    match &state.finished {
        Some(Ok(())) => Some(Take::End),
        Some(Err(message)) => Some(Take::Failed(message.clone())),
        None => None,
    }
}

impl BodyPump {
    /// Run `source` on a thread started by `spawn`, once. `source` returns the next chunk or
    /// `None` at the end of the body.
    pub(crate) fn start(
        &self,
        mut source: impl FnMut() -> io::Result<Option<Vec<u8>>> + Send + 'static,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> Result<(), String>,
    ) {
        {
            let mut state = self.shared.lock();
            if state.started {
                return;
            }
            state.started = true;
        }
        let shared = self.shared.clone();
        let job = Box::new(move || {
            loop {
                {
                    let mut state = shared.lock();
                    while state.queued >= HIGH_WATER && !state.cancelled {
                        state = shared.space.wait(state).unwrap_or_else(|p| p.into_inner());
                    }
                    if state.cancelled {
                        return;
                    }
                }
                let result = source();
                let mut state = shared.lock();
                match result {
                    Ok(Some(chunk)) => {
                        if chunk.is_empty() {
                            continue;
                        }
                        state.queued += chunk.len();
                        state.queue.push_back(chunk);
                    }
                    Ok(None) => state.finished = Some(Ok(())),
                    Err(error) => {
                        state.finished = Some(Err(format!("HTTP body read failed: {error}")));
                    }
                }
                let done = state.finished.is_some();
                if state.waiter.is_some() {
                    if let Some(taken) = take(&mut state) {
                        let waiter = state.waiter.take().expect("checked above");
                        drop(state);
                        waiter(taken);
                    }
                }
                if done {
                    return;
                }
            }
        });
        if let Err(message) = spawn(job) {
            let mut state = self.shared.lock();
            state.finished = Some(Err(format!("HTTP body reader: {message}")));
        }
    }

    /// Deliver the next batch to `deliver`: immediately when data (or the end) is already queued,
    /// otherwise when the reader produces it. A request replaces an undelivered earlier one.
    pub(crate) fn request(&self, deliver: impl FnOnce(Take) + Send + 'static) {
        let mut state = self.shared.lock();
        if state.cancelled {
            drop(state);
            deliver(Take::Failed("HTTP body read cancelled".into()));
            return;
        }
        match take(&mut state) {
            Some(taken) => {
                drop(state);
                self.shared.space.notify_all();
                deliver(taken);
            }
            None => state.waiter = Some(Box::new(deliver)),
        }
    }

    pub(crate) fn cancel(&self) {
        let waiter = {
            let mut state = self.shared.lock();
            state.cancelled = true;
            state.queue.clear();
            state.queued = 0;
            state.waiter.take()
        };
        self.shared.space.notify_all();
        if let Some(waiter) = waiter {
            waiter(Take::Failed("HTTP body read cancelled".into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn spawn_plain(job: Box<dyn FnOnce() + Send>) -> Result<(), String> {
        std::thread::spawn(job);
        Ok(())
    }

    fn chunks(count: usize, size: usize) -> impl FnMut() -> io::Result<Option<Vec<u8>>> + Send {
        let mut left = count;
        move || {
            if left == 0 {
                return Ok(None);
            }
            left -= 1;
            Ok(Some(vec![7u8; size]))
        }
    }

    fn read_all(pump: &BodyPump, pause: Duration) -> (usize, usize, Option<String>) {
        let (tx, rx) = mpsc::channel();
        let (mut total, mut deliveries) = (0, 0);
        loop {
            let tx = tx.clone();
            pump.request(move |taken| {
                let _ = tx.send(taken);
            });
            match rx.recv_timeout(Duration::from_secs(10)).expect("delivery") {
                Take::Data(bytes) => {
                    total += bytes.len();
                    deliveries += 1;
                    std::thread::sleep(pause);
                }
                Take::End => return (total, deliveries, None),
                Take::Failed(message) => return (total, deliveries, Some(message)),
            }
        }
    }

    #[test]
    fn large_body_arrives_in_batches_not_per_read() {
        let pump = BodyPump::default();
        pump.start(chunks(1024, 8192), spawn_plain);
        let (total, deliveries, error) = read_all(&pump, Duration::from_millis(2));
        assert!(error.is_none());
        assert_eq!(total, 1024 * 8192);
        assert!(deliveries <= 64, "8 MiB of 8 KiB reads took {deliveries} deliveries");
    }

    #[test]
    fn deliveries_never_exceed_the_batch_cap_for_small_chunks() {
        let pump = BodyPump::default();
        pump.start(chunks(512, 16 << 10), spawn_plain);
        let (tx, rx) = mpsc::channel();
        std::thread::sleep(Duration::from_millis(200));
        pump.request(move |taken| {
            let _ = tx.send(taken);
        });
        match rx.recv_timeout(Duration::from_secs(5)).expect("delivery") {
            Take::Data(bytes) => assert!(bytes.len() <= MAX_DELIVERY && bytes.len() >= 16 << 10),
            _ => panic!("expected data"),
        }
    }

    #[test]
    fn reader_stops_at_the_high_water_mark_until_the_consumer_catches_up() {
        let pump = BodyPump::default();
        let produced = Arc::new(Mutex::new(0usize));
        let counter = produced.clone();
        pump.start(
            move || {
                *counter.lock().unwrap() += 1;
                Ok(Some(vec![0u8; 64 << 10]))
            },
            spawn_plain,
        );
        std::thread::sleep(Duration::from_millis(300));
        let first = *produced.lock().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let second = *produced.lock().unwrap();
        assert_eq!(first, second, "reader kept reading past the high-water mark");
        assert!(first * (64 << 10) <= HIGH_WATER + (64 << 10));
        pump.cancel();
    }

    #[test]
    fn a_failure_is_delivered_after_the_queued_data() {
        let pump = BodyPump::default();
        let mut sent = false;
        pump.start(
            move || {
                if !sent {
                    sent = true;
                    return Ok(Some(vec![1; 10]));
                }
                Err(io::Error::other("boom"))
            },
            spawn_plain,
        );
        let (total, _, error) = read_all(&pump, Duration::ZERO);
        assert_eq!(total, 10);
        assert!(error.unwrap().contains("boom"));
    }

    #[test]
    fn cancel_wakes_a_waiting_request_and_stops_the_reader() {
        let pump = BodyPump::default();
        let (release, gate) = mpsc::channel::<()>();
        pump.start(
            move || {
                let _ = gate.recv();
                Ok(None)
            },
            spawn_plain,
        );
        let (tx, rx) = mpsc::channel();
        pump.request(move |taken| {
            let _ = tx.send(taken);
        });
        pump.cancel();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Take::Failed(_)
        ));
        drop(release);
    }

    #[test]
    fn a_spawn_failure_surfaces_as_a_read_error() {
        let pump = BodyPump::default();
        pump.start(chunks(1, 1), |_| Err("no threads".into()));
        let (_, _, error) = read_all(&pump, Duration::ZERO);
        assert!(error.unwrap().contains("no threads"));
    }
}
