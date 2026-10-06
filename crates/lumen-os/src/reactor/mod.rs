//! The I/O readiness reactor: one-shot registrations of descriptors (or host sources) with a
//! [`Wake`] callback, driven by a [`Poller`] that one loop thread turns.
//!
//! Raw syscalls only, no crate: `epoll` on Linux and Android, `kqueue` on macOS, iOS and FreeBSD,
//! `poll(2)` on other Unix. Windows and wasm32 have no backend yet; [`Poller::new`] reports
//! `Unsupported` there. Registrations are one-shot: after a wake the source is disarmed until
//! [`Registration::rearm`], which the consumer calls once it has seen `WouldBlock`. A slot index
//! plus a generation in every kernel token lets a late event for a dropped registration be
//! recognised and ignored.
//!
//! [`LoopWaker`] is the coalesced cross-thread wake of a poller: only the first wake after the
//! loop re-armed costs a syscall.

use crate::sched::SchedError;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

#[cfg(all(
    unix,
    any(
        test,
        not(any(
            target_os = "linux",
            target_os = "android",
            all(any(target_os = "macos", target_os = "ios", target_os = "freebsd"), target_pointer_width = "64")
        ))
    )
))]
mod pollfd;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod epoll;
#[cfg(all(
    any(target_os = "macos", target_os = "ios", target_os = "freebsd"),
    target_pointer_width = "64"
))]
mod kqueue;
mod hosted;

pub use hosted::{HostHooks, HostedReactor};

/// Which readiness a registration waits for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Interest(u8);

impl Interest {
    pub const READ: Interest = Interest(1);
    pub const WRITE: Interest = Interest(2);
    pub const BOTH: Interest = Interest(3);

    pub const fn bits(self) -> u8 {
        self.0
    }
    pub const fn from_bits(bits: u8) -> Interest {
        Interest(bits & 3)
    }
    pub const fn contains(self, other: Interest) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for Interest {
    type Output = Interest;
    fn bitor(self, rhs: Interest) -> Interest {
        Interest(self.0 | rhs.0)
    }
}

/// What a registration was woken for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ready(u8);

impl Ready {
    pub const NONE: Ready = Ready(0);
    pub const READ: Ready = Ready(1);
    pub const WRITE: Ready = Ready(2);
    pub const ERROR: Ready = Ready(4);
    pub const HUP: Ready = Ready(8);

    pub const fn bits(self) -> u8 {
        self.0
    }
    pub const fn from_bits(bits: u8) -> Ready {
        Ready(bits & 15)
    }
    pub const fn contains(self, other: Ready) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for Ready {
    type Output = Ready;
    fn bitor(self, rhs: Ready) -> Ready {
        Ready(self.0 | rhs.0)
    }
}

/// What a registration watches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// A descriptor, borrowed: the caller keeps it open until the [`Registration`] is dropped.
    #[cfg(unix)]
    Fd(std::os::fd::RawFd),
    /// A `SOCKET`.
    #[cfg(windows)]
    Socket(usize),
    /// An opaque token of the embedding host (Bitnest sockets and pipes).
    Host(u64),
}

/// Called when a registration became ready. On a [`Poller`] it runs inline on the loop thread, so
/// it must be short and must not block.
pub trait Wake: Send + Sync {
    fn wake(&self);
}

impl<F: Fn() + Send + Sync> Wake for F {
    fn wake(&self) {
        self()
    }
}

pub trait Reactor: Send + Sync {
    /// Registers `src` one-shot: after `wake` runs it is disarmed until [`Registration::rearm`].
    fn register(
        &self,
        src: Source,
        interest: Interest,
        wake: Arc<dyn Wake>,
    ) -> Result<Registration, SchedError>;
}

/// Backend side of a registration; dropping it deregisters.
pub(crate) trait RegistrationOps: Send + Sync {
    fn rearm(&self, interest: Interest) -> Result<(), SchedError>;
}

#[derive(Default)]
pub(crate) struct IoState {
    ready: AtomicU8,
    closed: AtomicBool,
}

impl IoState {
    pub(crate) fn set(&self, ready: Ready) {
        self.ready.fetch_or(ready.0, Ordering::AcqRel);
    }

    pub(crate) fn mark_closed(&self) {
        self.closed.store(true, Ordering::Release);
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// A live registration. Dropping it deregisters the source; do that before closing the descriptor.
pub struct Registration {
    state: Arc<IoState>,
    ops: Box<dyn RegistrationOps>,
}

impl Registration {
    pub(crate) fn new(state: Arc<IoState>, ops: Box<dyn RegistrationOps>) -> Registration {
        Registration { state, ops }
    }

    /// The readiness recorded since the last call.
    pub fn take_ready(&self) -> Ready {
        Ready(self.state.ready.swap(0, Ordering::AcqRel))
    }

    /// Arms the source again for `interest`.
    pub fn rearm(&self, interest: Interest) -> Result<(), SchedError> {
        self.ops.rearm(interest)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.state.mark_closed();
    }
}

/// The kernel side of a [`Poller`].
pub(crate) trait Backend: Send + Sync {
    fn add(&self, src: Source, interest: Interest, token: u64) -> Result<(), SchedError>;
    fn rearm(&self, src: Source, interest: Interest, token: u64) -> Result<(), SchedError>;
    fn remove(&self, src: Source, token: u64);
    /// Called after `fired` woke a registration armed for `armed`, so a backend that disarms per
    /// filter can disarm the rest.
    fn fired(&self, _src: Source, _armed: Interest, _fired: Ready) {}
    /// One cross-thread wake syscall.
    fn trigger(&self);
    /// Blocks until events, a wake or `timeout`. Appends `(token, ready)` to `out`; returns
    /// whether a wake was consumed. `EINTR` is not an error.
    fn wait(&self, timeout: Option<Duration>, out: &mut Vec<(u64, Ready)>) -> Result<bool, SchedError>;
}

#[cfg(unix)]
pub(crate) const WAKE_TOKEN: u64 = u64::MAX;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct Entry {
    src: Source,
    interest: Interest,
    wake: Arc<dyn Wake>,
    state: Arc<IoState>,
}

struct Slot {
    generation: u32,
    entry: Option<Entry>,
}

#[derive(Default)]
struct Slab {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

fn token_of(index: u32, generation: u32) -> u64 {
    (u64::from(generation) << 32) | u64::from(index)
}

fn split(token: u64) -> (usize, u32) {
    ((token & 0xffff_ffff) as usize, (token >> 32) as u32)
}

impl Slab {
    fn insert(&mut self, entry: Entry) -> u64 {
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.entry = Some(entry);
            return token_of(index, slot.generation);
        }
        let index = self.slots.len() as u32;
        self.slots.push(Slot { generation: 1, entry: Some(entry) });
        token_of(index, 1)
    }

    fn entry_mut(&mut self, token: u64) -> Option<&mut Entry> {
        let (index, generation) = split(token);
        let slot = self.slots.get_mut(index)?;
        if slot.generation != generation {
            return None;
        }
        slot.entry.as_mut()
    }

    fn remove(&mut self, token: u64) -> Option<Entry> {
        let (index, generation) = split(token);
        let slot = self.slots.get_mut(index)?;
        if slot.generation != generation {
            return None;
        }
        let entry = slot.entry.take()?;
        slot.generation = slot.generation.wrapping_add(1).max(1);
        self.free.push(index as u32);
        Some(entry)
    }
}

struct Core {
    backend: Box<dyn Backend>,
    slab: Mutex<Slab>,
    events: Mutex<Vec<(u64, Ready)>>,
    armed: AtomicBool,
    #[cfg(test)]
    triggers: std::sync::atomic::AtomicUsize,
}

impl Core {
    fn trigger(&self) {
        #[cfg(test)]
        self.triggers.fetch_add(1, Ordering::SeqCst);
        self.backend.trigger();
    }

    fn register(
        self: &Arc<Self>,
        src: Source,
        interest: Interest,
        wake: Arc<dyn Wake>,
    ) -> Result<Registration, SchedError> {
        if interest.is_empty() {
            return Err(SchedError::Exhausted("empty interest".into()));
        }
        let state = Arc::new(IoState::default());
        let token = lock(&self.slab).insert(Entry { src, interest, wake, state: state.clone() });
        if let Err(e) = self.backend.add(src, interest, token) {
            lock(&self.slab).remove(token);
            return Err(e);
        }
        Ok(Registration::new(state, Box::new(PollerOps { core: self.clone(), src, token })))
    }

    fn dispatch(&self, token: u64, ready: Ready) -> bool {
        let (src, armed, wake, state) = {
            let mut slab = lock(&self.slab);
            let Some(entry) = slab.entry_mut(token) else { return false };
            (entry.src, entry.interest, entry.wake.clone(), entry.state.clone())
        };
        if state.is_closed() {
            return false;
        }
        state.set(ready);
        self.backend.fired(src, armed, ready);
        wake.wake();
        true
    }

    fn turn(&self, timeout: Option<Duration>) -> Result<usize, SchedError> {
        let mut events = std::mem::take(&mut *lock(&self.events));
        events.clear();
        let woken = match self.backend.wait(timeout, &mut events) {
            Ok(woken) => woken,
            Err(e) => {
                *lock(&self.events) = events;
                return Err(e);
            }
        };
        if woken {
            self.armed.store(false, Ordering::Release);
        }
        events.sort_unstable_by_key(|e| e.0);
        let mut dispatched = 0;
        let mut i = 0;
        while i < events.len() {
            let token = events[i].0;
            let mut ready = events[i].1;
            i += 1;
            while i < events.len() && events[i].0 == token {
                ready = ready | events[i].1;
                i += 1;
            }
            dispatched += usize::from(self.dispatch(token, ready));
        }
        events.clear();
        *lock(&self.events) = events;
        Ok(dispatched)
    }
}

struct PollerOps {
    core: Arc<Core>,
    src: Source,
    token: u64,
}

impl RegistrationOps for PollerOps {
    fn rearm(&self, interest: Interest) -> Result<(), SchedError> {
        if let Some(entry) = lock(&self.core.slab).entry_mut(self.token) {
            entry.interest = interest;
        }
        self.core.backend.rearm(self.src, interest, self.token)
    }
}

impl Drop for PollerOps {
    fn drop(&mut self) {
        self.core.backend.remove(self.src, self.token);
        lock(&self.core.slab).remove(self.token);
    }
}

/// A poller owned by one loop thread, which calls [`Poller::turn`]. Registrations may be made and
/// dropped from any thread; wakes run inline on the thread that turns.
pub struct Poller {
    core: Arc<Core>,
}

impl Poller {
    /// A poller on this platform's native backend; `Unsupported` where there is none.
    pub fn new() -> Result<Poller, SchedError> {
        Ok(Self::with_backend(native_backend()?))
    }

    pub(crate) fn with_backend(backend: Box<dyn Backend>) -> Poller {
        Poller {
            core: Arc::new(Core {
                backend,
                slab: Mutex::default(),
                events: Mutex::default(),
                armed: AtomicBool::new(false),
                #[cfg(test)]
                triggers: std::sync::atomic::AtomicUsize::new(0),
            }),
        }
    }

    pub fn waker(&self) -> LoopWaker {
        LoopWaker { core: self.core.clone() }
    }

    /// Waits for readiness, a [`LoopWaker::wake`] or `timeout` (`None`: no limit), then runs the
    /// wakes of the registrations that became ready. Returns how many ran. Spurious early returns
    /// are possible.
    pub fn turn(&self, timeout: Option<Duration>) -> Result<usize, SchedError> {
        self.core.turn(timeout)
    }

    #[cfg(test)]
    pub(crate) fn trigger_count(&self) -> usize {
        self.core.triggers.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn dispatch_for_test(&self, token: u64, ready: Ready) -> bool {
        self.core.dispatch(token, ready)
    }
}

impl Reactor for Poller {
    fn register(
        &self,
        src: Source,
        interest: Interest,
        wake: Arc<dyn Wake>,
    ) -> Result<Registration, SchedError> {
        self.core.register(src, interest, wake)
    }
}

/// Wakes a [`Poller`] from any thread. Clones share one flag: while it is set, further wakes cost
/// nothing, and it is cleared when the loop consumes the wake. A burst of wakes is one syscall.
#[derive(Clone)]
pub struct LoopWaker {
    core: Arc<Core>,
}

impl LoopWaker {
    pub fn wake(&self) {
        if !self.core.armed.swap(true, Ordering::AcqRel) {
            self.core.trigger();
        }
    }
}

#[allow(unreachable_code)]
fn native_backend() -> Result<Box<dyn Backend>, SchedError> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    return Ok(Box::new(epoll::EpollBackend::new()?));
    #[cfg(all(
        any(target_os = "macos", target_os = "ios", target_os = "freebsd"),
        target_pointer_width = "64"
    ))]
    return Ok(Box::new(kqueue::KqueueBackend::new()?));
    #[cfg(all(
        unix,
        not(any(target_os = "linux", target_os = "android")),
        not(all(
            any(target_os = "macos", target_os = "ios", target_os = "freebsd"),
            target_pointer_width = "64"
        ))
    ))]
    return Ok(Box::new(pollfd::PollBackend::new()?));
    Err(SchedError::Unsupported("readiness reactor"))
}

#[cfg(all(test, unix))]
mod tests;
