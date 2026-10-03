//! Blocking waits shared by the engines: a [`Waiter`] is a one-shot wake handle that a thread
//! blocks on, with an optional timeout and an optional interrupt flag, and that another thread
//! wakes. The JavaScript `Atomics.wait` futex table and the Python thread locks both queue these.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How often a blocked wait looks at its interrupt flag.
pub const INTERRUPT_POLL: Duration = Duration::from_millis(10);

#[derive(Default)]
pub struct Waiter {
    woken: Mutex<bool>,
    cv: Condvar,
}

impl Waiter {
    pub fn new() -> Arc<Waiter> {
        Arc::new(Waiter::default())
    }

    /// Wakes the thread blocked in [`Waiter::block`] (or the next one to block).
    pub fn wake(&self) {
        *self.woken.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.cv.notify_all();
    }

    pub fn is_woken(&self) -> bool {
        *self.woken.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Blocks until woken (`true`), until `timeout` has passed (`None`: no limit) or until
    /// `interrupt` is raised (`false`).
    pub fn block(&self, timeout: Option<Duration>, interrupt: Option<&AtomicBool>) -> bool {
        let mut woken = self.woken.lock().unwrap_or_else(PoisonError::into_inner);
        let deadline = timeout.map(|dur| Instant::now() + dur);
        loop {
            if *woken {
                return true;
            }
            if interrupt.is_some_and(|f| f.load(Ordering::SeqCst)) {
                return false;
            }
            let left = match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    // The loop head decides the timeout, not `timed_out()`: a platform wait can
                    // end a little before the deadline (Windows condition variables count whole
                    // milliseconds), and a wait must never report a timeout before its time.
                    if now >= deadline {
                        return false;
                    }
                    Some(deadline - now)
                }
                None => None,
            };
            woken = match (left, interrupt) {
                (Some(left), Some(_)) => {
                    self.cv.wait_timeout(woken, left.min(INTERRUPT_POLL)).unwrap_or_else(PoisonError::into_inner).0
                }
                (Some(left), None) => self.cv.wait_timeout(woken, left).unwrap_or_else(PoisonError::into_inner).0,
                (None, Some(_)) => self.cv.wait_timeout(woken, INTERRUPT_POLL).unwrap_or_else(PoisonError::into_inner).0,
                (None, None) => self.cv.wait(woken).unwrap_or_else(PoisonError::into_inner),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_out_and_wakes() {
        let w = Waiter::new();
        assert!(!w.block(Some(Duration::from_millis(5)), None));
        let other = w.clone();
        let t = std::thread::spawn(move || other.wake());
        assert!(w.block(None, None));
        t.join().unwrap();
        assert!(w.is_woken());
    }

    #[test]
    fn interrupt_ends_the_wait() {
        let w = Waiter::new();
        let flag = AtomicBool::new(true);
        assert!(!w.block(None, Some(&flag)));
    }
}
