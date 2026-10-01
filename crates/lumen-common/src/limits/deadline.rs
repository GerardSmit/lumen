use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

/// Runs a closure on a timer thread once `limit` has passed. Dropping it before then cancels the
/// timer and joins the thread; [`Deadline::detach`] lets it run unattended instead.
pub struct Deadline {
    cancel: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl Deadline {
    pub fn start(name: &str, limit: Duration, fire: impl FnOnce() + Send + 'static) -> Deadline {
        let cancel = Arc::new((Mutex::new(false), Condvar::new()));
        let watcher = Arc::clone(&cancel);
        let thread = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                let (lock, cvar) = &*watcher;
                let cancelled = lock.lock().unwrap_or_else(PoisonError::into_inner);
                let (cancelled, _) = cvar
                    .wait_timeout_while(cancelled, limit, |c| !*c)
                    .unwrap_or_else(PoisonError::into_inner);
                if !*cancelled {
                    drop(cancelled);
                    fire();
                }
            })
            .expect("spawn deadline thread");
        Deadline { cancel, thread: Some(thread) }
    }

    /// Leaves the timer running to its end, whatever happens to the owner.
    pub fn detach(mut self) {
        self.thread.take();
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else { return };
        let (lock, cvar) = &*self.cancel;
        *lock.lock().unwrap_or_else(PoisonError::into_inner) = true;
        cvar.notify_all();
        let _ = thread.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

    #[test]
    fn fires_after_the_limit() {
        let fired = Arc::new(AtomicBool::new(false));
        let flag = fired.clone();
        let deadline = Deadline::start("test-deadline", Duration::from_millis(10), move || flag.store(true, SeqCst));
        std::thread::sleep(Duration::from_millis(200));
        assert!(fired.load(SeqCst));
        drop(deadline);
    }

    #[test]
    fn dropping_cancels() {
        let fired = Arc::new(AtomicBool::new(false));
        let flag = fired.clone();
        drop(Deadline::start("test-deadline", Duration::from_secs(3600), move || flag.store(true, SeqCst)));
        assert!(!fired.load(SeqCst));
    }
}
