//! The process scheduler: dedicated threads, a shared pool for bounded blocking work, parking and
//! one-shot timers, behind one trait. Hosts install a backend with [`install`]
//! (Bitnest does, from its runtime services); otherwise [`current`] returns the platform default:
//! [`OsScheduler`] on operating systems, [`NoThreads`] on wasm32 and [`Unavailable`] on bare metal.

use std::borrow::Cow;
use std::num::NonZeroUsize;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

mod os;
mod park;
mod timer;

pub use os::OsScheduler;
pub use park::OsPark;
pub use timer::Deadline;

pub type Job = Box<dyn FnOnce() + Send + 'static>;
pub type ThreadMain = Box<dyn FnOnce(ThreadStart) + Send + 'static>;

/// Which kind of core a thread prefers. The engine's `CpuHint` is this type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CpuHint {
    #[default]
    Any,
    Efficiency,
    Performance,
}

impl CpuHint {
    pub fn name(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Efficiency => "efficiency",
            Self::Performance => "performance",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    /// Runs an engine: needs the engine stack size; the thread calls `set_thread_stack_size`
    /// with [`ThreadStart::stack_bytes`].
    Engine,
    /// Blocks for an unbounded time (child wait, synchronous read); never takes a pool slot.
    Blocking,
    /// Long-lived helper (timer driver, signal fan-out).
    Service,
}

#[derive(Clone, Debug)]
pub struct ThreadSpec {
    pub name: Cow<'static, str>,
    /// Requested stack; the scheduler may round it.
    pub stack_bytes: usize,
    pub cpu: CpuHint,
    pub purpose: Purpose,
}

impl ThreadSpec {
    pub fn new(name: impl Into<Cow<'static, str>>, purpose: Purpose) -> Self {
        Self { name: name.into(), stack_bytes: 0, cpu: CpuHint::Any, purpose }
    }
}

/// What a new thread was actually given. A `stack_bytes` of 0 means the platform default.
#[derive(Clone, Copy, Debug)]
pub struct ThreadStart {
    pub stack_bytes: usize,
    pub core: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchedError {
    Unsupported(&'static str),
    Exhausted(String),
    Os(crate::FsError),
    Shutdown,
}

impl std::fmt::Display for SchedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(what) => write!(f, "{what} is not supported here"),
            Self::Exhausted(why) => f.write_str(why),
            Self::Os(e) => write!(f, "{}", e.code()),
            Self::Shutdown => f.write_str("scheduler is shut down"),
        }
    }
}
impl std::error::Error for SchedError {}

/// Wakes a parked thread. Token semantics: any number of unparks before the next park end that
/// park once.
pub trait Unpark: Send + Sync {
    fn unpark(&self);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Woke {
    Unparked,
    TimedOut,
}

/// A park slot. `park(None)` waits without arming any timer.
pub trait Park: Unpark {
    fn park(&self, deadline: Option<Instant>) -> Woke;
    fn unparker(self: Arc<Self>) -> Arc<dyn Unpark>;
}

pub trait JoinThread: Send {
    fn join(self: Box<Self>) -> Result<(), SchedError>;
    fn is_finished(&self) -> bool;
}

pub struct ThreadHandle(pub Box<dyn JoinThread>);

impl ThreadHandle {
    pub fn join(self) -> Result<(), SchedError> {
        self.0.join()
    }
    pub fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
}

pub trait TimerCancel: Send + Sync {
    /// True when the timer had not fired yet and now never will.
    fn cancel(&self) -> bool;
}

/// A one-shot timer. Dropping it cancels; [`Timer::detach`] lets it fire unattended.
pub struct Timer(Option<Arc<dyn TimerCancel>>);

impl Timer {
    pub fn new(cancel: Arc<dyn TimerCancel>) -> Self {
        Self(Some(cancel))
    }
    pub fn cancel(&self) -> bool {
        self.0.as_ref().is_some_and(|c| c.cancel())
    }
    pub fn detach(mut self) {
        self.0 = None;
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if let Some(c) = self.0.take() {
            c.cancel();
        }
    }
}

pub trait Scheduler: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn available_parallelism(&self) -> NonZeroUsize;
    /// A dedicated thread of execution.
    fn spawn_thread(&self, spec: ThreadSpec, main: ThreadMain) -> Result<ThreadHandle, SchedError>;
    /// Bounded blocking work on the shared pool. `Err(job)` hands the job back when there is no
    /// pool; the caller runs it inline.
    fn spawn_blocking(&self, job: Job) -> Result<(), Job>;
    /// A park slot for the calling context.
    fn parker(&self) -> Result<Arc<dyn Park>, SchedError>;
    /// Runs `fire` once after `delay`, off the caller's thread. Nothing is armed until the first
    /// call. `fire` runs on the scheduler's timer driver, so it must be short and must not block;
    /// hand longer work to `spawn_blocking` or `spawn_thread`.
    fn after(&self, delay: Duration, fire: Job) -> Result<Timer, SchedError>;
}

/// The standard library's view of the CPU count, 1 when it cannot tell. The single source for
/// every scheduler and for `sysinfo::cpu_count`.
fn host_parallelism() -> NonZeroUsize {
    std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN)
}

/// Single-threaded targets: nothing can be spawned, parked or timed; callers run work inline.
pub struct NoThreads;

impl Scheduler for NoThreads {
    fn name(&self) -> &'static str {
        "none"
    }
    fn available_parallelism(&self) -> NonZeroUsize {
        NonZeroUsize::MIN
    }
    fn spawn_thread(&self, _: ThreadSpec, _: ThreadMain) -> Result<ThreadHandle, SchedError> {
        Err(SchedError::Unsupported("threads"))
    }
    fn spawn_blocking(&self, job: Job) -> Result<(), Job> {
        Err(job)
    }
    fn parker(&self) -> Result<Arc<dyn Park>, SchedError> {
        Err(SchedError::Unsupported("parking"))
    }
    fn after(&self, _: Duration, _: Job) -> Result<Timer, SchedError> {
        Err(SchedError::Unsupported("timers"))
    }
}

/// The default before a bare-metal host installs its own: every call fails at once, none spin.
pub struct Unavailable;

impl Scheduler for Unavailable {
    fn name(&self) -> &'static str {
        "unavailable"
    }
    fn available_parallelism(&self) -> NonZeroUsize {
        host_parallelism()
    }
    fn spawn_thread(&self, _: ThreadSpec, _: ThreadMain) -> Result<ThreadHandle, SchedError> {
        Err(SchedError::Unsupported("scheduler not installed"))
    }
    fn spawn_blocking(&self, job: Job) -> Result<(), Job> {
        Err(job)
    }
    fn parker(&self) -> Result<Arc<dyn Park>, SchedError> {
        Err(SchedError::Unsupported("scheduler not installed"))
    }
    fn after(&self, _: Duration, _: Job) -> Result<Timer, SchedError> {
        Err(SchedError::Unsupported("scheduler not installed"))
    }
}

static INSTALLED: OnceLock<&'static dyn Scheduler> = OnceLock::new();

/// Installs the process scheduler. Once per process, before the first [`current`] that should see
/// it.
pub fn install(scheduler: &'static dyn Scheduler) -> Result<(), &'static str> {
    INSTALLED.set(scheduler).map_err(|_| "a scheduler is already installed")
}

fn platform_default() -> &'static dyn Scheduler {
    #[cfg(target_arch = "wasm32")]
    {
        static DEFAULT: NoThreads = NoThreads;
        &DEFAULT
    }
    #[cfg(target_os = "none")]
    {
        static DEFAULT: Unavailable = Unavailable;
        &DEFAULT
    }
    #[cfg(not(any(target_arch = "wasm32", target_os = "none")))]
    {
        static DEFAULT: OsScheduler = OsScheduler::new();
        &DEFAULT
    }
}

/// The installed scheduler, else the platform default.
pub fn current() -> &'static dyn Scheduler {
    match INSTALLED.get() {
        Some(s) => *s,
        None => platform_default(),
    }
}

/// Sleeps the calling thread for `duration` by parking on the scheduler, falling back to
/// `std::thread::sleep` where it has no parker.
pub fn sleep(duration: Duration) {
    let Ok(parker) = current().parker() else {
        std::thread::sleep(duration);
        return;
    };
    let Some(deadline) = Instant::now().checked_add(duration) else {
        loop {
            parker.park(None);
        }
    };
    while Instant::now() < deadline {
        parker.park(Some(deadline));
    }
}

#[cfg(test)]
mod tests;
