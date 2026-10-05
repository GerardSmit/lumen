//! A thread-safe message queue with a close flag: the shared core of every cross-thread channel
//! (JS `MessagePort` / `BroadcastChannel` endpoints in `lumen-runtime`, Python
//! `_xxinterpchannels` channels in `lumen-py`). The engines differ in what a message is and in
//! how a receiver is woken (an event-loop task, a blocking wait); the queue itself is the same.
//!
//! Closing never drops what is already queued: a receiver drains the queue and only then sees the
//! close ([`Pop::Closed`]).

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// A drained queue larger than this releases its buffer instead of keeping a burst's peak.
const RETAINED_CAPACITY: usize = 64;

struct State<M> {
    items: VecDeque<M>,
    closed: bool,
}

/// The outcome of a receive.
#[derive(Debug, PartialEq, Eq)]
pub enum Pop<M> {
    /// The oldest queued message.
    Message(M),
    /// Nothing queued (the queue is open).
    Empty,
    /// Nothing queued and the queue is closed.
    Closed,
}

pub struct Queue<M> {
    state: Mutex<State<M>>,
    ready: Condvar,
}

impl<M> Default for Queue<M> {
    fn default() -> Queue<M> {
        Queue::new()
    }
}

impl<M> Queue<M> {
    pub fn new() -> Queue<M> {
        Queue { state: Mutex::new(State { items: VecDeque::new(), closed: false }), ready: Condvar::new() }
    }

    fn lock(&self) -> MutexGuard<'_, State<M>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queue `message`; hands it back when the queue is closed.
    pub fn push(&self, message: M) -> Result<(), M> {
        let mut s = self.lock();
        if s.closed {
            return Err(message);
        }
        s.items.push_back(message);
        drop(s);
        self.ready.notify_one();
        Ok(())
    }

    /// The oldest message, without waiting.
    pub fn pop(&self) -> Pop<M> {
        let mut s = self.lock();
        match s.items.pop_front() {
            Some(m) => {
                if s.items.is_empty() && s.items.capacity() > RETAINED_CAPACITY {
                    s.items = VecDeque::new();
                }
                Pop::Message(m)
            }
            None if s.closed => Pop::Closed,
            None => Pop::Empty,
        }
    }

    /// The oldest message, waiting until one arrives, the queue closes, or `timeout` passes
    /// (`None`: no limit). `Pop::Empty` means the timeout passed.
    pub fn pop_wait(&self, timeout: Option<Duration>) -> Pop<M> {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut s = self.lock();
        loop {
            if let Some(m) = s.items.pop_front() {
                if s.items.is_empty() && s.items.capacity() > RETAINED_CAPACITY {
                    s.items = VecDeque::new();
                }
                return Pop::Message(m);
            }
            if s.closed {
                return Pop::Closed;
            }
            match deadline {
                None => s = self.ready.wait(s).unwrap_or_else(|e| e.into_inner()),
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return Pop::Empty;
                    }
                    s = self.ready.wait_timeout(s, d - now).unwrap_or_else(|e| e.into_inner()).0;
                }
            }
        }
    }

    /// Stop accepting messages and wake every waiting receiver. Queued messages stay.
    pub fn close(&self) {
        self.lock().closed = true;
        self.ready.notify_all();
    }

    /// Drop every queued message.
    pub fn clear(&self) {
        self.lock().items.clear();
    }

    /// Remove and return every queued message that `drop_it` selects.
    pub fn remove_where(&self, mut drop_it: impl FnMut(&M) -> bool) -> Vec<M> {
        let mut s = self.lock();
        let mut removed = Vec::new();
        let mut kept = VecDeque::with_capacity(s.items.len());
        while let Some(m) = s.items.pop_front() {
            if drop_it(&m) {
                removed.push(m);
            } else {
                kept.push_back(m);
            }
        }
        s.items = kept;
        removed
    }

    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    pub fn is_empty(&self) -> bool {
        self.lock().items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.lock().items.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn close_keeps_queued_messages() {
        let q = Queue::new();
        q.push(1).unwrap();
        q.close();
        assert_eq!(q.push(2), Err(2));
        assert_eq!(q.pop(), Pop::Message(1));
        assert_eq!(q.pop(), Pop::Closed);
    }

    #[test]
    fn pop_wait_times_out_and_wakes() {
        let q = Arc::new(Queue::new());
        assert_eq!(q.pop_wait(Some(Duration::from_millis(5))), Pop::<i32>::Empty);
        let sender = Arc::clone(&q);
        let t = std::thread::spawn(move || sender.push(7).unwrap());
        assert_eq!(q.pop_wait(None), Pop::Message(7));
        t.join().unwrap();
    }

    #[test]
    fn remove_where_filters_in_place() {
        let q = Queue::new();
        for i in 0..5 {
            q.push(i).unwrap();
        }
        assert_eq!(q.remove_where(|m| m % 2 == 0), vec![0, 2, 4]);
        assert_eq!(q.len(), 2);
    }
}
