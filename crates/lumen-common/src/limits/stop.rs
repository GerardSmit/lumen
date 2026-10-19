use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed, Ordering::SeqCst};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

/// Why a run was cut short beyond its own budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Abort {
    None,
    Interrupt,
    Deadline,
    Heap,
}

type Wake = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct Subscribers {
    next: u64,
    wakes: Vec<(u64, Wake)>,
}

/// Requests that a running engine stop. Cloneable and `Send`, so a watchdog thread or a signal
/// handler can hold one. The request stays raised until [`InterruptHandle::clear`]. A host that
/// blocks somewhere the engine cannot poll (an event loop) subscribes a wake that
/// [`InterruptHandle::interrupt`] runs after raising the flag. Clones share the flag and the
/// subscriptions.
#[derive(Clone, Default)]
pub struct InterruptHandle {
    flag: Arc<AtomicBool>,
    subscribers: Arc<Mutex<Subscribers>>,
}

/// A wake registered with [`InterruptHandle::subscribe`]; dropping it unsubscribes.
#[must_use = "dropping the subscription unsubscribes the wake"]
pub struct InterruptSubscription {
    subscribers: Weak<Mutex<Subscribers>>,
    id: u64,
}

impl Drop for InterruptSubscription {
    fn drop(&mut self) {
        if let Some(subscribers) = self.subscribers.upgrade() {
            lock(&subscribers).wakes.retain(|(id, _)| *id != self.id);
        }
    }
}

fn lock(subscribers: &Mutex<Subscribers>) -> MutexGuard<'_, Subscribers> {
    subscribers.lock().unwrap_or_else(PoisonError::into_inner)
}

impl InterruptHandle {
    pub fn new() -> InterruptHandle {
        InterruptHandle::default()
    }

    /// A handle over a flag the embedder owns.
    pub fn from_flag(flag: Arc<AtomicBool>) -> InterruptHandle {
        InterruptHandle {
            flag,
            subscribers: Arc::default(),
        }
    }

    /// Run `wake` on every [`interrupt`](InterruptHandle::interrupt), for the life of the handle
    /// and its clones.
    pub fn with_wake(self, wake: impl Fn() + Send + Sync + 'static) -> InterruptHandle {
        std::mem::forget(self.subscribe(Arc::new(wake)));
        self
    }

    /// Run `wake` on every [`interrupt`](InterruptHandle::interrupt) until the returned
    /// subscription is dropped. The wake runs on the interrupting thread and must not block.
    pub fn subscribe(&self, wake: Arc<dyn Fn() + Send + Sync>) -> InterruptSubscription {
        let mut subscribers = lock(&self.subscribers);
        let id = subscribers.next;
        subscribers.next += 1;
        subscribers.wakes.push((id, wake));
        InterruptSubscription {
            subscribers: Arc::downgrade(&self.subscribers),
            id,
        }
    }

    pub fn interrupt(&self) {
        self.flag.store(true, SeqCst);
        let wakes: Vec<Wake> = lock(&self.subscribers)
            .wakes
            .iter()
            .map(|(_, wake)| Arc::clone(wake))
            .collect();
        for wake in wakes {
            wake();
        }
    }

    /// Synonym of [`interrupt`](InterruptHandle::interrupt) for hosts that end realms.
    pub fn terminate(&self) {
        self.interrupt();
    }

    pub fn is_interrupted(&self) -> bool {
        self.flag.load(Relaxed)
    }

    pub fn clear(&self) {
        self.flag.store(false, SeqCst);
    }

    pub fn flag(&self) -> &Arc<AtomicBool> {
        &self.flag
    }
}

impl fmt::Debug for InterruptHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InterruptHandle")
            .field("interrupted", &self.is_interrupted())
            .finish()
    }
}

/// A cooperative time-slice request: when `flag` is raised at a safe point the poll lowers it and
/// runs `hook` on the current native stack, then carries on.
#[derive(Clone)]
struct YieldHook {
    flag: Arc<AtomicBool>,
    hook: Arc<dyn Fn() + Send + Sync>,
}

impl fmt::Debug for YieldHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("YieldHook")
            .field("pending", &self.flag.load(Relaxed))
            .finish()
    }
}

/// The flags a host raises to stop a run, polled from safe points: `interrupt` ends the run for
/// good, `deadline` is a script timeout the host converts to a catchable error and later lowers.
/// An optional yield hook time-slices the run without ending it.
#[derive(Clone, Debug, Default)]
pub struct StopFlags {
    interrupt: Option<Arc<AtomicBool>>,
    deadline: Option<Arc<AtomicBool>>,
    yield_hook: Option<YieldHook>,
}

impl StopFlags {
    pub const fn new() -> StopFlags {
        StopFlags {
            interrupt: None,
            deadline: None,
            yield_hook: None,
        }
    }

    pub fn from_handle(handle: &InterruptHandle) -> StopFlags {
        StopFlags {
            interrupt: Some(handle.flag.clone()),
            deadline: None,
            yield_hook: None,
        }
    }

    pub fn set_interrupt(&mut self, flag: Arc<AtomicBool>) {
        self.interrupt = Some(flag);
    }

    pub fn has_interrupt(&self) -> bool {
        self.interrupt.is_some()
    }

    pub fn interrupt_flag(&self) -> Option<&Arc<AtomicBool>> {
        self.interrupt.as_ref()
    }

    /// The deadline flag, created on first use.
    pub fn deadline_flag(&mut self) -> Arc<AtomicBool> {
        self.deadline
            .get_or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    }

    /// Install the cooperative yield: see [`StopFlags::poll`].
    pub fn set_yield_hook(&mut self, flag: Arc<AtomicBool>, hook: Arc<dyn Fn() + Send + Sync>) {
        self.yield_hook = Some(YieldHook { flag, hook });
    }

    pub fn clear_yield_hook(&mut self) {
        self.yield_hook = None;
    }

    /// Whichever flag is raised, the interrupt first. A raised yield flag is lowered and its hook
    /// run (on the caller's stack) only when no hard stop is pending; the hard flags are read again
    /// after the hook, since it may have raised one.
    #[inline]
    pub fn poll(&self) -> Abort {
        let hard = self.poll_hard();
        if hard != Abort::None {
            return hard;
        }
        match &self.yield_hook {
            Some(y) if y.flag.load(Relaxed) => self.run_yield(y),
            _ => Abort::None,
        }
    }

    #[inline]
    fn poll_hard(&self) -> Abort {
        if self.interrupt.as_ref().is_some_and(|f| f.load(Relaxed)) {
            return Abort::Interrupt;
        }
        if self.deadline.as_ref().is_some_and(|f| f.load(Relaxed)) {
            return Abort::Deadline;
        }
        Abort::None
    }

    #[cold]
    #[inline(never)]
    fn run_yield(&self, y: &YieldHook) -> Abort {
        if y.flag.swap(false, SeqCst) {
            let hook = y.hook.clone();
            hook();
        }
        self.poll_hard()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn handle_raises_clears_and_wakes() {
        let woken = Arc::new(AtomicUsize::new(0));
        let seen = woken.clone();
        let handle = InterruptHandle::new().with_wake(move || {
            seen.fetch_add(1, Relaxed);
        });
        let copy = handle.clone();
        assert!(!handle.is_interrupted());
        copy.interrupt();
        assert!(handle.is_interrupted());
        assert_eq!(woken.load(Relaxed), 1);
        handle.clear();
        assert!(!copy.is_interrupted());
    }

    #[test]
    fn subscriptions_fan_out_and_unsubscribe_on_drop() {
        let handle = InterruptHandle::new();
        let (a, b) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (sa, sb) = (a.clone(), b.clone());
        let first = handle.subscribe(Arc::new(move || {
            sa.fetch_add(1, Relaxed);
        }));
        let _second = handle.clone().subscribe(Arc::new(move || {
            sb.fetch_add(1, Relaxed);
        }));
        handle.interrupt();
        assert_eq!((a.load(Relaxed), b.load(Relaxed)), (1, 1));
        drop(first);
        handle.interrupt();
        assert_eq!((a.load(Relaxed), b.load(Relaxed)), (1, 2));
    }

    #[test]
    fn yield_runs_the_hook_once_and_the_interrupt_wins() {
        let handle = InterruptHandle::new();
        let mut stop = StopFlags::from_handle(&handle);
        let flag = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let (c, h) = (calls.clone(), handle.clone());
        stop.set_yield_hook(
            flag.clone(),
            Arc::new(move || {
                if c.fetch_add(1, Relaxed) == 1 {
                    h.interrupt();
                }
            }),
        );
        assert_eq!(stop.poll(), Abort::None);
        assert_eq!(calls.load(Relaxed), 0);
        flag.store(true, Relaxed);
        assert_eq!(stop.poll(), Abort::None);
        assert!(!flag.load(Relaxed));
        flag.store(true, Relaxed);
        assert_eq!(stop.poll(), Abort::Interrupt);
        assert_eq!(calls.load(Relaxed), 2);
        flag.store(true, Relaxed);
        assert_eq!(stop.poll(), Abort::Interrupt);
        assert_eq!(calls.load(Relaxed), 2);
    }

    #[test]
    fn poll_reports_the_interrupt_before_the_deadline() {
        let handle = InterruptHandle::new();
        let mut stop = StopFlags::from_handle(&handle);
        assert_eq!(stop.poll(), Abort::None);
        stop.deadline_flag().store(true, Relaxed);
        assert_eq!(stop.poll(), Abort::Deadline);
        handle.interrupt();
        assert_eq!(stop.poll(), Abort::Interrupt);
    }
}
