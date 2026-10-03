//! Python threads. Objects are `Rc`, so only one OS thread may touch them at a time: the
//! interpreter has a global interpreter lock ([`Gil`]), and every Python thread is an OS thread
//! that runs only while it holds it.
//!
//! What a thread owns while it runs (its frames, the exception being handled, the contextvars
//! context, ...) lives in the [`Interp`] itself; a thread that gives the GIL up *parks* that
//! state (`Threads::parked`) and takes it back when it has the GIL again. The same goes for the
//! thread-local caches that hold interpreter objects (`TlsBundle`), which must follow the
//! interpreter rather than the OS thread.
//!
//! The GIL changes hands
//! - at a *switch request*: a thread that has waited a whole switch interval
//!   (`sys.setswitchinterval`) raises `Threads::request`, which the running thread notices at
//!   its next backward jump or call ([`Interp::preempt`]);
//! - around every blocking operation ([`Interp::unlocked`]): lock waits, sleeps, descriptor
//!   waits and `waitpid` run without the GIL (and, being sliced, notice signals on the main
//!   thread and the embedder's interrupt).
//!
//! Signal handlers run on the main thread only (`signalm::check`); spawned threads block all
//! asynchronous signals.

use crate::object::*;
use crate::vm::{dict_get_str, Frame, Interp};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Default length of a thread's stack: address space only, committed as it is used.
const DEFAULT_STACK: usize = 64 << 20;

/// `sys.getswitchinterval()` at startup, in microseconds.
const DEFAULT_SWITCH_US: u64 = 5000;

/// Longest a blocking wait goes without looking at signals, the interrupt and the GIL queue.
const SLICE: Duration = Duration::from_millis(20);

struct GilState {
    locked: bool,
    waiting: usize,
    /// Counts acquisitions, so a waiter can tell that the lock moved while it slept.
    switches: u64,
}

/// The global interpreter lock: a mutex with a switch-request flag, after CPython's.
///
/// # The invariant that makes `Rc` objects safe across threads
///
/// Python objects are `Rc`s, which are neither `Send` nor `Sync`. They are shared by OS threads
/// anyway, which is sound only because *every touch of an `Rc` (clone, drop, borrow, read of
/// its contents) happens on the thread that holds the GIL*:
///
/// 1. A thread runs interpreter code only between [`Gil::acquire`] and [`Gil::release`]; the
///    lock's mutex orders those sections, so one thread's writes are visible to the next holder.
/// 2. Everything that holds objects belongs to the interpreter, not to an OS thread. State kept
///    in `Interp` is moved by `park` / `unpark` ([`ThreadState`]); state kept in `thread_local!`
///    caches (weak registry, object ids, the cycle collector's lists, watchers, regex and
///    struct caches) is moved the same way as [`TlsBundle`]. A thread-local that holds objects
///    and is not in the bundle is a bug: its destructor would run on a thread with no GIL, and
///    another thread's `Drop` would not find the object's bookkeeping.
/// 3. Code that runs without the GIL does so only through [`Interp::unlocked`], whose closure
///    and result are `Send`, so they cannot carry an `Rc`. Helper threads (the sentinels,
///    waiters, the signal handler) are `Arc` / atomic state only.
/// 4. The only `Rc`-carrying value that crosses a thread boundary is the start-up [`Boot`]
///    (the callable and its arguments). The parent builds it while it holds the GIL and keeps no
///    copy; the child touches it only after its own `acquire` (the [`Unsend`] wrapper marks the
///    one `unsafe impl Send`).
/// 5. Signal handlers only record the signal (`lumen_os::signal`); the Python handler runs on
///    the main thread, with the GIL, at a poll site.
pub struct Gil {
    state: Mutex<GilState>,
    free: Condvar,
    taken: Condvar,
    request: Arc<AtomicBool>,
    interval_us: AtomicU64,
    interp: AtomicPtr<Interp>,
    /// Python threads started and not yet finished (the main thread is not counted).
    running: AtomicUsize,
}

impl Gil {
    fn new(request: Arc<AtomicBool>, interval_us: u64) -> Gil {
        Gil {
            state: Mutex::new(GilState { locked: true, waiting: 0, switches: 0 }),
            free: Condvar::new(),
            taken: Condvar::new(),
            request,
            interval_us: AtomicU64::new(interval_us),
            interp: AtomicPtr::new(std::ptr::null_mut()),
            running: AtomicUsize::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, GilState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Blocks until this thread holds the lock; after a whole switch interval of waiting it asks
    /// the holder to give it up.
    pub fn acquire(&self) {
        let mut st = self.lock();
        st.waiting += 1;
        let mut seen = st.switches;
        while st.locked {
            let interval = Duration::from_micros(self.interval_us.load(Ordering::Relaxed));
            let (guard, res) = self.free.wait_timeout(st, interval).unwrap_or_else(PoisonError::into_inner);
            st = guard;
            if res.timed_out() && st.locked && st.switches == seen {
                self.request.store(true, Ordering::Relaxed);
            }
            seen = st.switches;
        }
        st.waiting -= 1;
        st.locked = true;
        st.switches += 1;
        self.request.store(false, Ordering::Relaxed);
        drop(st);
        self.taken.notify_all();
    }

    pub fn release(&self) {
        let mut st = self.lock();
        st.locked = false;
        drop(st);
        self.free.notify_one();
    }

    /// Hands the lock to a waiting thread, if there is one, and takes it back afterwards.
    fn yield_now(&self) {
        let mut st = self.lock();
        self.request.store(false, Ordering::Relaxed);
        if st.waiting == 0 {
            return;
        }
        let seen = st.switches;
        st.locked = false;
        self.free.notify_one();
        while st.switches == seen && st.waiting > 0 {
            st = self.taken.wait_timeout(st, Duration::from_millis(1)).unwrap_or_else(PoisonError::into_inner).0;
        }
        drop(st);
        self.acquire();
    }

    /// Records the interpreter the lock guards. The pointer is stored where the waiting threads
    /// can reach it, so the compiler cannot assume that blocking calls leave the interpreter
    /// untouched.
    fn publish(&self, it: *mut Interp) {
        self.interp.store(it, Ordering::SeqCst);
    }

    fn interp(&self) -> *mut Interp {
        self.interp.load(Ordering::SeqCst)
    }

    /// Python threads started and not yet finished.
    pub fn running(&self) -> usize {
        self.running.load(Ordering::SeqCst)
    }
}

/// What a thread keeps in the interpreter while it runs and parks while it does not.
pub struct ThreadState {
    frames: Vec<Frame>,
    handled: Option<Obj>,
    yielded: Option<Frame>,
    no_tb: bool,
    repr_stack: Vec<usize>,
    ret_val: Value,
    context: Option<Value>,
}

/// The thread-local caches that hold interpreter objects.
struct TlsBundle {
    ids: IdState,
    weak: crate::weak::WeakState,
    sre: Box<dyn std::any::Any>,
    structs: Box<dyn std::any::Any>,
    gc: crate::gc::HeapState,
    watch: crate::watch::WatchState,
}

impl TlsBundle {
    fn take() -> TlsBundle {
        TlsBundle {
            ids: id_state_take(),
            weak: crate::weak::state_take(),
            sre: crate::builtins::sre::tls_take(),
            structs: crate::builtins::structm::_struct::tls_take(),
            gc: crate::gc::state_take(),
            watch: crate::watch::state_take(),
        }
    }

    fn put(self) {
        id_state_put(self.ids);
        crate::weak::state_put(self.weak);
        crate::builtins::sre::tls_put(self.sre);
        crate::builtins::structm::_struct::tls_put(self.structs);
        crate::gc::state_put(self.gc);
        crate::watch::state_put(self.watch);
    }
}

/// The interpreter's threads.
pub struct Threads {
    /// Created when the first thread starts.
    pub gil: Option<Arc<Gil>>,
    /// Raised by a thread that has waited a whole switch interval for the GIL.
    request: Arc<AtomicBool>,
    parked: HashMap<u64, ThreadState>,
    tls: Option<Box<TlsBundle>>,
    /// The thread that created the interpreter: it runs signal handlers.
    pub main_ident: u64,
    pub stack_size: usize,
    pub switch_interval_us: u64,
    /// The lock `_thread._set_sentinel` made for each thread, released when the thread ends.
    pub sentinels: HashMap<u64, Value>,
}

impl Default for Threads {
    fn default() -> Threads {
        Threads {
            gil: None,
            request: Arc::new(AtomicBool::new(false)),
            parked: HashMap::new(),
            tls: None,
            main_ident: lumen_os::thread::ident(),
            stack_size: 0,
            switch_interval_us: DEFAULT_SWITCH_US,
            sentinels: HashMap::new(),
        }
    }
}

struct Unsend<T>(T);

// SAFETY: the value is only touched by the thread that receives it after it has taken the GIL;
// the sender holds the GIL while it hands the value over and keeps no copy.
unsafe impl<T> Send for Unsend<T> {}

impl<T> Unsend<T> {
    fn into_inner(self) -> T {
        self.0
    }
}

struct Boot {
    interp: *mut Interp,
    gil: Arc<Gil>,
    func: Value,
    args: Vec<Value>,
    kwargs: Vec<(Obj, Value)>,
}

fn thread_main(boot: Boot, started: std::sync::mpsc::Sender<u64>) {
    let ident = lumen_os::thread::ident();
    let _ = started.send(ident);
    drop(started);
    lumen_os::signal::block_on_this_thread();
    let Boot { interp, gil, func, args, kwargs } = boot;
    gil.acquire();
    // SAFETY: the interpreter outlives its non-daemon threads, and daemon threads never get past
    // `acquire` once it is gone (the main thread keeps the GIL until the process ends).
    let it = unsafe { &mut *interp };
    it.unpark();
    it.run_thread(ident, func, args, kwargs);
    it.park_exit();
    gil.running.fetch_sub(1, Ordering::SeqCst);
    gil.release();
}

/// A non-blocking descriptor never needs waiting for: the operation itself reports `EAGAIN`.
fn nonblocking(fd: i32) -> bool {
    matches!(lumen_os::fdctl::get_blocking(fd), Ok(false))
}

impl Interp {
    pub fn is_main_thread(&self) -> bool {
        lumen_os::thread::ident() == self.threads.main_ident
    }

    /// Whether any Python thread other than the main one is alive.
    pub fn other_threads(&self) -> bool {
        self.threads.gil.as_ref().is_some_and(|g| g.running() > 0)
    }

    pub fn set_switch_interval(&mut self, secs: f64) {
        let us = ((secs * 1e6) as u64).max(1);
        self.threads.switch_interval_us = us;
        if let Some(g) = &self.threads.gil {
            g.interval_us.store(us, Ordering::Relaxed);
        }
    }

    fn take_thread_state(&mut self) -> ThreadState {
        ThreadState {
            frames: std::mem::take(&mut self.frames),
            handled: self.handled.take(),
            yielded: self.yielded.take(),
            no_tb: std::mem::replace(&mut self.no_tb, false),
            repr_stack: std::mem::take(&mut self.repr_stack),
            ret_val: std::mem::replace(&mut self.ret_val, Value::None),
            context: self.context.take(),
        }
    }

    fn put_thread_state(&mut self, s: ThreadState) {
        self.frames = s.frames;
        self.handled = s.handled;
        self.yielded = s.yielded;
        self.no_tb = s.no_tb;
        self.repr_stack = s.repr_stack;
        self.ret_val = s.ret_val;
        self.context = s.context;
    }

    /// Stores what the running thread owns where the next GIL holder can find it.
    fn park(&mut self) {
        let state = self.take_thread_state();
        self.threads.parked.insert(lumen_os::thread::ident(), state);
        self.threads.tls = Some(Box::new(TlsBundle::take()));
    }

    /// The thread has the GIL: it takes back its own state and the interpreter's caches.
    fn unpark(&mut self) {
        if let Some(bundle) = self.threads.tls.take() {
            bundle.put();
        }
        if let Some(state) = self.threads.parked.remove(&lumen_os::thread::ident()) {
            self.put_thread_state(state);
        }
    }

    /// A finished thread leaves only the interpreter's caches behind.
    fn park_exit(&mut self) {
        self.threads.tls = Some(Box::new(TlsBundle::take()));
    }

    /// Runs `f`, which must not touch Python objects, without the GIL. The `Send` bounds make the
    /// compiler enforce that: an `Rc` (every Python object) cannot be captured or returned.
    pub fn unlocked<T: Send>(&mut self, f: impl FnOnce() -> T + Send) -> T {
        let Some(gil) = self.threads.gil.clone() else { return f() };
        let me: *mut Interp = self;
        gil.publish(me);
        self.park();
        gil.release();
        let out = f();
        gil.acquire();
        // SAFETY: this thread holds the GIL again, so no other thread uses the interpreter; the
        // pointer comes from the lock, which is what other threads' changes are published
        // through.
        unsafe { (*gil.interp()).unpark() };
        out
    }

    /// Lets a waiting thread run now (`time.sleep(0)`).
    pub fn yield_gil(&mut self) {
        let Some(gil) = self.threads.gil.clone() else { return };
        let me: *mut Interp = self;
        gil.publish(me);
        self.park();
        gil.yield_now();
        // SAFETY: as in `unlocked`.
        unsafe { (*gil.interp()).unpark() };
    }

    /// Gives the GIL up if another thread has asked for it; cheap otherwise.
    #[inline(always)]
    pub fn preempt(&mut self) {
        if self.threads.request.load(Ordering::Relaxed) {
            self.yield_gil();
        }
    }

    /// Sleeps one slice of a longer wait, letting other threads run.
    pub fn sleep_slice(&mut self, secs: f64) {
        if self.threads.gil.is_none() {
            self.platform.borrow_mut().sleep(secs);
            return;
        }
        let d = Duration::try_from_secs_f64(secs).unwrap_or_default();
        self.unlocked(|| std::thread::sleep(d));
    }

    /// Waits until `fd` is ready for `events` (or fails: the operation reports why), letting
    /// other threads run, signals and the interrupt being serviced between slices. A no-op while
    /// no other thread exists, so single-threaded programs keep their plain blocking calls.
    pub fn wait_fd(&mut self, fd: i32, events: i16) -> R<()> {
        if (self.threads.gil.is_none() && !self.catching_signals) || nonblocking(fd) {
            return Ok(());
        }
        loop {
            if self.poll_fd_slice(fd, events) {
                return Ok(());
            }
            self.poll()?;
        }
    }

    fn poll_fd_slice(&mut self, fd: i32, events: i16) -> bool {
        let ms = SLICE.as_millis() as i32;
        let r = self.unlocked(|| {
            let mut fds = [lumen_os::poll::PollFd::new(fd, events)];
            lumen_os::poll::poll(&mut fds, ms)
        });
        match r {
            Ok(n) => n > 0,
            Err(e) => e.errno() != 4,
        }
    }

    /// `waitpid`, which waits for the child in slices while other threads exist.
    pub fn waitpid_blocking(&mut self, pid: i32, options: i32) -> R<crate::platform::PResult<(i32, i32)>> {
        const WNOHANG: i32 = 1;
        if self.threads.gil.is_none() || options & WNOHANG != 0 {
            return Ok(self.platform.borrow_mut().waitpid(pid, options));
        }
        let mut delay = Duration::from_millis(1);
        loop {
            let r = self.platform.borrow_mut().waitpid(pid, options | WNOHANG);
            match r {
                Ok((0, _)) => {}
                r => return Ok(r),
            }
            self.poll()?;
            self.unlocked(|| std::thread::sleep(delay));
            delay = (delay * 2).min(SLICE);
        }
    }

    /// Starts a thread that calls `func(*args, **kwargs)`; returns its identifier.
    pub fn start_thread(&mut self, func: Value, args: Vec<Value>, kwargs: Vec<(Obj, Value)>) -> R<u64> {
        if !self.platform.borrow().supports_threads() {
            return Err(self.runtime_error("can't start new thread"));
        }
        let gil = match self.threads.gil.clone() {
            Some(g) => g,
            None => {
                let g = Arc::new(Gil::new(self.threads.request.clone(), self.threads.switch_interval_us));
                self.threads.gil = Some(g.clone());
                g
            }
        };
        let interp: *mut Interp = self;
        gil.publish(interp);
        let (tx, rx) = std::sync::mpsc::channel::<u64>();
        let boot = Unsend(Boot { interp, gil: gil.clone(), func, args, kwargs });
        let size = if self.threads.stack_size == 0 { DEFAULT_STACK } else { self.threads.stack_size };
        gil.running.fetch_add(1, Ordering::SeqCst);
        let spawned = std::thread::Builder::new().stack_size(size).spawn(move || thread_main(boot.into_inner(), tx));
        if spawned.is_err() {
            gil.running.fetch_sub(1, Ordering::SeqCst);
            return Err(self.runtime_error("can't start new thread"));
        }
        match rx.recv() {
            Ok(ident) => Ok(ident),
            Err(_) => Err(self.runtime_error("can't start new thread")),
        }
    }

    fn run_thread(&mut self, ident: u64, func: Value, args: Vec<Value>, kwargs: Vec<(Obj, Value)>) {
        if let Err(e) = self.call(&func, args, kwargs) {
            if !self.exc_is(&e, "SystemExit") {
                self.write_unraisable(&e, Some("Exception ignored in thread started by"), Some(&func));
            }
        }
        drop(func);
        self.handled = None;
        self.yielded = None;
        self.context = None;
        self.ret_val = Value::None;
        self.frames.clear();
        self.repr_stack.clear();
        let sentinel = self.threads.sentinels.remove(&ident);
        crate::builtins::threadm::_thread::thread_finished(self, ident, sentinel);
    }

    /// `(identifier, innermost frame)` of every thread.
    pub fn current_frames(&mut self) -> Vec<(u64, Value)> {
        let mut out = Vec::new();
        if !self.frames.is_empty() {
            let top = self.frame_object(self.frames.len() - 1);
            out.push((lumen_os::thread::ident(), top));
        }
        for (ident, state) in &self.threads.parked {
            if !state.frames.is_empty() {
                out.push((*ident, self.frame_chain_snapshot(&state.frames)));
            }
        }
        out
    }

    /// `(identifier, exception being handled)` of every thread.
    pub fn current_exceptions(&mut self) -> Vec<(u64, Value)> {
        let mut out = vec![(lumen_os::thread::ident(), self.handled.clone().map_or(Value::None, Value::Obj))];
        for (ident, state) in &self.threads.parked {
            out.push((*ident, state.handled.clone().map_or(Value::None, Value::Obj)));
        }
        out
    }

    /// Writes `text` to `sys.stderr` (the process's stderr when that is unusable).
    pub fn print_to_sys_stderr(&mut self, text: &str) {
        let file = self.sys_module.clone().and_then(|m| {
            let d = self.module_dict(&m);
            dict_get_str(&d, "stderr")
        });
        if let Some(file) = file.filter(|f| !f.is_none()) {
            if self.call_method(&file, "write", vec![Value::str(text)]).is_ok() {
                let _ = self.call_method(&file, "flush", Vec::new());
                return;
            }
        }
        self.write_stderr(text);
    }

    /// Waits for the non-daemon threads (`threading._shutdown`) before the interpreter exits.
    pub fn wait_for_thread_shutdown(&mut self) {
        let Some(threading) = dict_get_str(&self.modules, "threading") else { return };
        let Ok(shutdown) = self.get_attr_str(&threading, "_shutdown") else { return };
        if let Err(e) = self.call(&shutdown, Vec::new(), Vec::new()) {
            if self.exc_is(&e, "SystemExit") {
                return;
            }
            let repr = self.repr_of(&shutdown).unwrap_or_default();
            let body = self.format_exception(&e);
            self.print_to_sys_stderr(&format!("Exception ignored in: {repr}\n{body}"));
        }
    }
}
