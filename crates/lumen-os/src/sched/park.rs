use super::{Park, Unpark, Woke};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

/// A park slot on a mutex and condition variable: a single token, set by `unpark` (any number of
/// times, coalesced) and consumed by the next `park`. Nothing is armed beyond the wait itself.
pub struct OsPark {
    token: Mutex<bool>,
    wake: Condvar,
}

impl OsPark {
    pub fn new() -> Self {
        Self { token: Mutex::new(false), wake: Condvar::new() }
    }

    fn lock(&self) -> MutexGuard<'_, bool> {
        self.token.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for OsPark {
    fn default() -> Self {
        Self::new()
    }
}

impl Unpark for OsPark {
    fn unpark(&self) {
        let mut token = self.lock();
        if !*token {
            *token = true;
            self.wake.notify_one();
        }
    }
}

impl Park for OsPark {
    fn park(&self, deadline: Option<Instant>) -> Woke {
        let mut token = self.lock();
        loop {
            if *token {
                *token = false;
                return Woke::Unparked;
            }
            token = match deadline {
                None => self.wake.wait(token).unwrap_or_else(|e| e.into_inner()),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Woke::TimedOut;
                    }
                    self.wake
                        .wait_timeout(token, deadline - now)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
            };
        }
    }

    fn unparker(self: Arc<Self>) -> Arc<dyn Unpark> {
        self
    }
}
