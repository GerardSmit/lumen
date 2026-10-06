//! One-shot timers for [`OsScheduler`](super::OsScheduler): a single lazily started `lumen-driver`
//! thread owns a [`DeadlineQueue`] and fires due jobs. The thread is created by the first `after`;
//! until then, and while the queue is empty, nothing is armed. The driver blocks in
//! [`Idle::wait`] with the next deadline as its timeout, or with no timeout when the queue is
//! empty. `Idle` is the one place that decides how the driver sleeps and is woken: a reactor
//! [`Poller`] turn where one exists, so I/O registrations run on this same thread, else a condition
//! variable.

use super::{Job, SchedError, TimerCancel};
use crate::reactor::{LoopWaker, Poller};
use lumen_common::deadline::DeadlineQueue;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The driver's sleep: blocks until `notify` was called since the last wait returned, or until the
/// deadline. A notify with no waiter is remembered, so a wake that races the driver going to sleep
/// is never lost.
///
/// Where the platform has a reactor the sleep is a [`Poller`] turn, so timers and I/O
/// registrations share the one driver thread and a notify is a coalesced [`LoopWaker`] wake.
/// Elsewhere (Windows until its backend lands) it is a condition variable.
enum Idle {
    Poll { poller: Poller, waker: LoopWaker },
    Cond(CondIdle),
}

#[derive(Default)]
struct CondIdle {
    notified: Mutex<bool>,
    wake: Condvar,
}

impl CondIdle {
    fn notify(&self) {
        let mut notified = self.notified.lock().unwrap_or_else(|e| e.into_inner());
        if !*notified {
            *notified = true;
            self.wake.notify_one();
        }
    }

    fn wait(&self, until: Option<Instant>) {
        let mut notified = self.notified.lock().unwrap_or_else(|e| e.into_inner());
        while !*notified {
            notified = match until {
                None => self.wake.wait(notified).unwrap_or_else(|e| e.into_inner()),
                Some(until) => {
                    let now = Instant::now();
                    if now >= until {
                        return;
                    }
                    self.wake
                        .wait_timeout(notified, until - now)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
            };
        }
        *notified = false;
    }
}

impl Idle {
    fn new() -> Idle {
        match Poller::new() {
            Ok(poller) => {
                let waker = poller.waker();
                Idle::Poll { poller, waker }
            }
            Err(_) => Idle::Cond(CondIdle::default()),
        }
    }

    fn notify(&self) {
        match self {
            Idle::Poll { waker, .. } => waker.wake(),
            Idle::Cond(idle) => idle.notify(),
        }
    }

    fn wait(&self, until: Option<Instant>) {
        match self {
            Idle::Poll { poller, .. } => {
                let timeout = until.map(|u| u.saturating_duration_since(Instant::now()));
                if poller.turn(timeout).is_err() {
                    std::thread::sleep(timeout.unwrap_or(Duration::from_millis(10)).min(Duration::from_millis(10)));
                }
            }
            Idle::Cond(idle) => idle.wait(until),
        }
    }
}

#[derive(Default)]
struct State {
    queue: DeadlineQueue<Instant, Job>,
    shutdown: bool,
}

pub(super) struct Driver {
    state: Mutex<State>,
    idle: Idle,
}

impl Driver {
    /// The reactor that shares this driver's thread, when the platform has one.
    pub(super) fn poller(&self) -> Option<&Poller> {
        match &self.idle {
            Idle::Poll { poller, .. } => Some(poller),
            Idle::Cond(_) => None,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Spawns the driver thread. It exits when [`Driver::shutdown`] is called.
    pub(super) fn start() -> Result<Arc<Self>, SchedError> {
        let driver = Arc::new(Self { state: Mutex::default(), idle: Idle::new() });
        let thread = driver.clone();
        std::thread::Builder::new()
            .name("lumen-driver".into())
            .spawn(move || thread.run())
            .map_err(|e| SchedError::Os(e.into()))?;
        Ok(driver)
    }

    pub(super) fn shutdown(&self) {
        self.lock().shutdown = true;
        self.idle.notify();
    }

    /// Queues `fire` for `delay` from now. Wakes the driver only when this became the earliest
    /// deadline.
    pub(super) fn after(self: &Arc<Self>, delay: Duration, fire: Job) -> Result<Arc<Cancel>, SchedError> {
        let deadline = Instant::now()
            .checked_add(delay)
            .ok_or_else(|| SchedError::Exhausted("timer delay exceeds the clock range".into()))?;
        let mut state = self.lock();
        if state.shutdown {
            return Err(SchedError::Shutdown);
        }
        let earliest = state.queue.next_deadline();
        let id = state.queue.insert(deadline, fire);
        drop(state);
        if earliest.is_none_or(|earliest| deadline < earliest) {
            self.idle.notify();
        }
        Ok(Arc::new(Cancel { driver: self.clone(), id }))
    }

    /// Whether the timer was still queued. Its heap node is skipped lazily by the driver.
    fn cancel(&self, id: u64) -> bool {
        let mut state = self.lock();
        let removed = state.queue.remove(id).is_some();
        state.queue.compact();
        if state.queue.is_empty() {
            state.queue.release_idle_capacity();
        }
        removed
    }

    /// Jobs run here, on the driver thread, so they must be short and must not block: a slow job
    /// delays every other timer. A panicking job is contained.
    fn run(&self) {
        loop {
            let mut state = self.lock();
            if state.shutdown {
                return;
            }
            if let Some((_, job)) = state.queue.take_due(Instant::now()) {
                drop(state);
                let _ = catch_unwind(AssertUnwindSafe(job));
                continue;
            }
            let next = state.queue.next_deadline();
            drop(state);
            self.idle.wait(next);
        }
    }
}

pub(super) struct Cancel {
    driver: Arc<Driver>,
    id: u64,
}

impl TimerCancel for Cancel {
    fn cancel(&self) -> bool {
        self.driver.cancel(self.id)
    }
}

/// Runs a closure once `limit` has passed, on the scheduler's timer driver, so `fire` must be
/// short and must not block. Dropping the deadline before then cancels it; [`Deadline::detach`]
/// lets it fire unattended. Replaces `lumen_common::limits::Deadline`, which spent a thread per
/// deadline.
///
/// Where the scheduler has no timers (wasm32, bare metal before installation) nothing is armed and
/// `fire` never runs; [`Deadline::try_start`] reports that.
pub struct Deadline(Option<super::Timer>);

impl Deadline {
    /// `name` only documents the caller; the driver thread has its own name.
    pub fn start(name: &str, limit: Duration, fire: impl FnOnce() + Send + 'static) -> Deadline {
        Self::try_start(name, limit, fire).unwrap_or(Deadline(None))
    }

    pub fn try_start(
        _name: &str,
        limit: Duration,
        fire: impl FnOnce() + Send + 'static,
    ) -> Result<Deadline, SchedError> {
        super::current().after(limit, Box::new(fire)).map(|timer| Deadline(Some(timer)))
    }

    /// Leaves the timer armed to its end, whatever happens to the owner.
    pub fn detach(mut self) {
        if let Some(timer) = self.0.take() {
            timer.detach();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sched::{OsScheduler, Scheduler};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
    use std::sync::mpsc;

    const LONG: Duration = Duration::from_secs(30);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn driver_is_not_started_before_the_first_after() {
        let sched = OsScheduler::new();
        assert!(!sched.driver_started());
        let _ = sched.parker().unwrap();
        sched.spawn_blocking(Box::new(|| {})).ok();
        assert!(!sched.driver_started());
        sched.after(LONG, Box::new(|| {})).unwrap();
        assert!(sched.driver_started());
    }

    #[test]
    fn timers_fire_in_deadline_order() {
        let sched = OsScheduler::new();
        let (tx, rx) = mpsc::channel();
        for (label, delay) in [("c", 90), ("a", 10), ("b", 50)] {
            let tx = tx.clone();
            sched.after(ms(delay), Box::new(move || tx.send(label).unwrap())).unwrap().detach();
        }
        let order: Vec<_> = (0..3).map(|_| rx.recv_timeout(LONG).unwrap()).collect();
        assert_eq!(order, ["a", "b", "c"]);
    }

    #[test]
    fn timer_fires_off_the_callers_thread_after_its_delay() {
        let sched = OsScheduler::new();
        let (tx, rx) = mpsc::channel();
        let start = Instant::now();
        sched
            .after(ms(40), Box::new(move || tx.send(std::thread::current().name().map(str::to_owned)).unwrap()))
            .unwrap()
            .detach();
        let name = rx.recv_timeout(LONG).unwrap();
        assert!(start.elapsed() >= ms(40));
        assert_eq!(name.as_deref(), Some("lumen-driver"));
    }

    #[test]
    fn dropping_a_timer_prevents_it_firing() {
        let sched = OsScheduler::new();
        let fired = Arc::new(AtomicBool::new(false));
        let flag = fired.clone();
        let timer = sched.after(ms(20), Box::new(move || flag.store(true, SeqCst))).unwrap();
        drop(timer);
        let (tx, rx) = mpsc::channel();
        sched.after(ms(80), Box::new(move || tx.send(()).unwrap())).unwrap().detach();
        rx.recv_timeout(LONG).unwrap();
        assert!(!fired.load(SeqCst));
    }

    #[test]
    fn cancel_reports_whether_the_timer_was_pending() {
        let sched = OsScheduler::new();
        let (tx, rx) = mpsc::channel();
        let timer = sched.after(ms(10), Box::new(move || tx.send(()).unwrap())).unwrap();
        rx.recv_timeout(LONG).unwrap();
        assert!(!timer.cancel());
        let pending = sched.after(LONG, Box::new(|| {})).unwrap();
        assert!(pending.cancel());
        assert!(!pending.cancel());
    }

    #[test]
    fn detached_timer_fires_after_its_handle_is_gone() {
        let sched = OsScheduler::new();
        let (tx, rx) = mpsc::channel();
        sched.after(ms(10), Box::new(move || tx.send(()).unwrap())).unwrap().detach();
        rx.recv_timeout(LONG).unwrap();
    }

    #[test]
    fn earlier_insert_wakes_a_driver_waiting_on_a_later_deadline() {
        let sched = OsScheduler::new();
        let _far = sched.after(LONG, Box::new(|| {})).unwrap();
        std::thread::sleep(ms(50));
        let (tx, rx) = mpsc::channel();
        let start = Instant::now();
        sched.after(ms(20), Box::new(move || tx.send(()).unwrap())).unwrap().detach();
        rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn insert_wakes_a_driver_with_an_empty_queue() {
        let sched = OsScheduler::new();
        let (tx, rx) = mpsc::channel();
        let first = tx.clone();
        sched.after(ms(1), Box::new(move || first.send(1).unwrap())).unwrap().detach();
        assert_eq!(rx.recv_timeout(LONG).unwrap(), 1);
        std::thread::sleep(ms(50));
        sched.after(ms(1), Box::new(move || tx.send(2).unwrap())).unwrap().detach();
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), 2);
    }

    #[test]
    fn panicking_job_does_not_stop_the_driver() {
        let sched = OsScheduler::new();
        sched.after(ms(1), Box::new(|| panic!("expected test panic"))).unwrap().detach();
        let (tx, rx) = mpsc::channel();
        sched.after(ms(30), Box::new(move || tx.send(()).unwrap())).unwrap().detach();
        rx.recv_timeout(LONG).unwrap();
    }

    #[test]
    fn dropping_the_scheduler_stops_the_driver() {
        let ran = Arc::new(AtomicUsize::new(0));
        let counter = ran.clone();
        let sched = OsScheduler::new();
        sched.after(ms(200), Box::new(move || { counter.fetch_add(1, SeqCst); })).unwrap().detach();
        drop(sched);
        std::thread::sleep(ms(400));
        assert_eq!(ran.load(SeqCst), 0);
    }

    #[test]
    fn deadline_triggers_after_the_limit() {
        let (tx, rx) = mpsc::channel();
        let deadline = Deadline::start("test-deadline", ms(10), move || tx.send(()).unwrap());
        rx.recv_timeout(LONG).unwrap();
        drop(deadline);
    }

    #[test]
    fn dropping_a_deadline_cancels_it() {
        let fired = Arc::new(AtomicBool::new(false));
        let flag = fired.clone();
        drop(Deadline::start("test-deadline", ms(30), move || flag.store(true, SeqCst)));
        std::thread::sleep(ms(150));
        assert!(!fired.load(SeqCst));
    }

    #[test]
    fn detached_deadline_still_fires() {
        let (tx, rx) = mpsc::channel();
        Deadline::start("test-deadline", ms(10), move || tx.send(()).unwrap()).detach();
        rx.recv_timeout(LONG).unwrap();
    }
}
