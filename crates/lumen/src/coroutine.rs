//! Stackful coroutines for generators (and async functions), built on OS threads.
//!
//! lumen is a tree-walking interpreter, so suspending a generator mid-body means parking its native
//! call stack. Each live coroutine runs on an OS thread; control is handed back and forth with a
//! pair of channels in strict ping-pong — exactly one of {driver, coroutine thread} runs at any
//! instant, the other parked in `recv`. The shared [`Interp`] is therefore never touched
//! concurrently, which is why shuttling a `*mut Interp` across the thread boundary (see
//! [`InterpPtr`]) is sound in practice.
//!
//! Worker threads are **pooled**: spawning a fresh thread per async call / generator was ~30µs
//! (measured), dominating async cost, so finished workers return to an idle pool and are handed the
//! next coroutine instead. At most eight idle helpers stay warm, and an offered helper retires
//! after one second of silence. A helper checked out for a new job is fenced against retirement;
//! active coroutines retain the same ~6µs per-suspend channel handoff.
//!
//! The running coroutine's channels live in a thread-local [`YIELDER`], so a `yield` buried deep in
//! eval finds the right channel and nested coroutines (each on their own worker) need no extra
//! bookkeeping — every thread reads its own thread-local.
//!
//! Address stability: a coroutine never outlives the `Engine` that owns the interpreter, and that
//! `Engine` is not moved between the `eval` calls that create and drive the coroutine, so the
//! captured pointer stays valid for the coroutine's whole life.

use std::cell::RefCell;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;

use crate::interpreter::Interp;
use crate::value::Value;

/// Driver → generator: resume the body.
pub enum Resume {
    /// `next(v)` — the `yield` expression evaluates to `v`.
    Next(Value),
    /// `return(v)` — inject a return completion at the suspended `yield`.
    Return(Value),
    /// `throw(e)` — inject a throw at the suspended `yield`.
    Throw(Value),
}

// `Resume`/`Suspend` carry `Value`s (which hold non-`Send` `Rc`s). Transferring them across the
// channel is sound because of the strict ping-pong: a value is produced on one side only after the
// other side has parked, so it is never touched on two threads at once.
unsafe impl Send for Resume {}
unsafe impl Send for Suspend {}

/// Generator → driver: the body parked or finished.
pub enum Suspend {
    /// `yield v` — parked, produced `v` (a generator value).
    Yield(Value),
    /// `await v` — parked waiting for `v` to settle (async functions/generators).
    Await(Value),
    /// The body ran to completion / `return v`.
    Done(Value),
    /// The body threw `e` and it escaped.
    Throw(Value),
}

/// A `*mut Interp` carried to the generator thread. Sound only under the strict ping-pong handoff:
/// when the generator thread dereferences it the driver is parked (not touching the interpreter),
/// and vice versa, so the two `&mut` reborrows are never *used* concurrently.
pub struct InterpPtr(pub *mut Interp);
unsafe impl Send for InterpPtr {}

/// The generator body, boxed. It captures `Rc`s (the function + its scope) so it is not really
/// `Send`; the strict handoff makes moving it to the worker thread sound.
pub struct SendBody(pub Box<dyn FnOnce(&mut Interp) -> Suspend>);
unsafe impl Send for SendBody {}

// Internal handoffs are consumed by ThreadCoro::resume, never exposed as JS suspension.
enum WorkerMessage {
    Suspend(Suspend),
    DriverCall(Box<dyn FnOnce(&mut Interp) -> Result<Value, Value>>),
}
// Same exclusive ping-pong ownership as Suspend; no captured value is used concurrently.
unsafe impl Send for WorkerMessage {}

/// The generator-thread side of the channels, kept in the worker thread's TLS.
struct Yielder {
    suspend_tx: Sender<WorkerMessage>,
    resume_rx: Receiver<Resume>,
}

thread_local! {
    static YIELDER: RefCell<Option<Yielder>> = const { RefCell::new(None) };
    /// Set on the coroutine thread when the body is an *async* generator, so `yield` knows to
    /// `Await` its operand (AsyncGeneratorYield) before suspending.
    static ASYNC_GEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the current thread is executing a generator body (so `yield` is legal here).
pub fn in_coroutine() -> bool {
    YIELDER.with(|y| y.borrow().is_some())
}

/// Mark the running coroutine thread as an async generator body.
pub fn set_async_gen(v: bool) {
    ASYNC_GEN.with(|c| c.set(v));
}

/// Whether the running coroutine is an async generator (its `yield` awaits the operand).
pub fn in_async_gen() -> bool {
    ASYNC_GEN.with(|c| c.get())
}

/// The driver side of one coroutine, stored on the generator object in `Interp.generators`. Either
/// an OS-thread-backed coroutine (generators, and async bodies the bytecode compiler declined) or a
/// bytecode [`VmCoro`](crate::bytecode::VmCoro) (async bodies that compile) — both drive the same
/// way, so `drive_async`/`drive_generator` are agnostic.
pub enum Coroutine {
    Thread(ThreadCoro),
    Vm(crate::bytecode::VmCoro),
    #[cfg(feature = "aot-native")]
    Native(crate::native_aot::coroutines::NativeCoro),
}

impl Coroutine {
    /// Run the body to its next suspension. The body runs under its own stack-trace frame
    /// (see [`Coroutine::set_frame`]), since a resume happens outside the call that created it.
    #[inline]
    pub(crate) fn resume(&mut self, i: &mut Interp, signal: Resume) -> Suspend {
        let started = self.started();
        let frame = match self {
            Coroutine::Thread(c) => &c.frame,
            Coroutine::Vm(c) => &c.frame,
            #[cfg(feature = "aot-native")]
            Coroutine::Native(c) => &c.frame,
        };
        // An await job can be drained while a different document is entered.
        // Every module step, including resumed steps, uses its captured settings.
        let saved_realm = if let crate::interpreter::stack_trace::ResumeFrame::Script(module) = frame {
            if module.settings != crate::value::Gc::as_ptr(&i.global) as usize {
                let Some(origin) = i.realms.get(&module.settings).map(crate::interpreter::RealmState::snapshot_clone) else {
                    return Suspend::Throw(crate::interpreter::abrupt_value(i.throw("TypeError","module settings no longer exist")));
                };
                let saved = i.snapshot_realm();
                i.restore_realm(&origin);
                Some(saved)
            } else { None }
        } else { None };
        let pushed = i.enter_resume_frame(frame, started);
        let s = match self {
            Coroutine::Thread(c) => c.resume(i, signal),
            Coroutine::Vm(c) => c.resume(i, signal),
            #[cfg(feature = "aot-native")]
            Coroutine::Native(c) => c.resume(i, signal),
        };
        i.leave_resume_frame(pushed);
        if let Some(saved) = saved_realm { i.restore_realm(&saved); }
        s
    }
    /// The frame (function or module body) the body's resumes run under.
    pub(crate) fn set_frame(&mut self, frame: crate::interpreter::stack_trace::ResumeFrame) {
        match self {
            Coroutine::Thread(c) => c.frame = frame,
            Coroutine::Vm(c) => c.frame = frame,
            #[cfg(feature = "aot-native")]
            Coroutine::Native(c) => c.frame = frame,
        }
    }
    /// Whether the body has finished (further resumes are no-ops).
    #[inline]
    pub fn done(&self) -> bool {
        match self {
            Coroutine::Thread(c) => c.done,
            Coroutine::Vm(c) => c.done,
            #[cfg(feature = "aot-native")]
            Coroutine::Native(c) => c.done,
        }
    }
    /// Whether the first resume has happened (distinguishes suspendedStart from a suspended yield).
    #[inline]
    pub fn started(&self) -> bool {
        match self {
            Coroutine::Thread(c) => c.started,
            Coroutine::Vm(c) => c.started,
            #[cfg(feature = "aot-native")]
            Coroutine::Native(c) => c.started,
        }
    }
}

/// An OS-thread-backed coroutine (a pooled worker runs the body; see [`spawn_coroutine`]).
pub struct ThreadCoro {
    resume_tx: Sender<Resume>,
    suspend_rx: Receiver<WorkerMessage>,
    /// Set once the body has finished (Done/Throw); further resumes are no-ops.
    pub done: bool,
    /// Set on the first resume — distinguishes "suspendedStart" from a suspended yield.
    pub started: bool,
    views: std::sync::Arc<crate::lstr::ViewRegistry>,
    /// Frame-ownership tag: `FnFrame`s pushed while this coroutine's body runs carry this id
    /// (via `Interp::cur_coro`), so a worker-thread panic can evict exactly the dead body's
    /// frames — they may be interleaved with the driver's own frames across suspensions, so a
    /// watermark truncate cannot find them.
    pub(crate) id: u32,
    /// The stack-trace frame each resume runs under (see [`Coroutine::set_frame`]).
    pub(crate) frame: crate::interpreter::stack_trace::ResumeFrame,
}

impl Drop for ThreadCoro {
    /// A body that never ran is still owned by its worker, which drops it once it sees the resume
    /// channel close. Wait for that: the body holds `Rc`s into the driver's heap, and releasing
    /// them on the worker while the driver keeps running (a collection sweeping dead generators,
    /// say) races the refcounts and the object registry. A started body needs no wait — a
    /// finished one released everything before its last handoff, and a suspended one parks for
    /// good without touching the heap again (see `park`). Its native-stack count remains
    /// retained by that parked job: borrowed string-view bytes still cannot safely move.
    fn drop(&mut self) {
        if !self.started {
            drop(std::mem::replace(&mut self.resume_tx, channel().0));
            let _ = self.suspend_rx.recv();
        }
    }
}

/// Allocates [`ThreadCoro::id`]s; 0 is reserved for "not in a coroutine body" (the main driver).
static CORO_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

impl ThreadCoro {
    /// Hand control to the generator and block until it next parks or finishes. Saves/restores the
    /// interpreter's scalar execution context (`strict`, recursion `depth`, tail-call eligibility
    /// `tco_ok`) across the handoff so the driver and the body don't clobber each other's. `tco_ok`
    /// matters because a coroutine body executes outside `Interp::call`'s tail-call trampoline: if a
    /// leaked `tco_ok == true` reached an `async`/generator body, its `return f(...)` would be parked
    /// as a pending tail call that nothing ever runs, and the body would resolve to `undefined`.
    pub(crate) fn resume(&mut self, i: &mut Interp, signal: Resume) -> Suspend {
        if self.done {
            return Suspend::Done(Value::Undefined);
        }
        if !self.started {
            self.views.start_native_stack();
            self.started = true;
        }
        let (saved_strict, saved_depth, saved_tco) = (i.strict, i.depth, i.tco_ok);
        // The running-native chain links Rust stack records: a body that died mid-native must
        // not leave its (unwound) ones linked.
        let saved_natives = i.native_top;
        let saved_coro = std::mem::replace(&mut i.cur_coro, self.id);
        // The body mutates `*i` from its worker thread while we block (see `park_message`).
        std::hint::black_box(&mut *i as *mut Interp);
        // Only the realm's own thread times the body: a nested driver runs inside an outer
        // body's slice, which is already counted. Driver calls run on this thread, whose own
        // clock counts them, so they are taken out.
        let timed = (saved_coro == 0).then(std::time::Instant::now);
        let mut on_driver = std::time::Duration::ZERO;
        let _ = self.resume_tx.send(signal);
        let s = loop {
            match self.suspend_rx.recv() {
                Ok(WorkerMessage::DriverCall(call)) => {
                    i.depth_limit = 0;
                    let call_started = timed.map(|_| std::time::Instant::now());
                    // A nested coroutine may have another coroutine as its driver. Forward
                    // until the realm's owning native thread executes the call.
                    let reply = match driver_call(i, call) {
                        Ok(value) => Resume::Next(value),
                        Err(error) => Resume::Throw(error),
                    };
                    if let Some(started) = call_started {
                        on_driver += started.elapsed();
                    }
                    if self.resume_tx.send(reply).is_err() {
                        break Err(std::sync::mpsc::RecvError);
                    }
                }
                Ok(WorkerMessage::Suspend(s)) => break Ok(s),
                Err(error) => break Err(error),
            }
        };
        if let Some(started) = timed {
            crate::value::note_coroutine_time(started.elapsed().saturating_sub(on_driver));
        }
        i.cur_coro = saved_coro;
        i.strict = saved_strict;
        i.depth = saved_depth;
        // Back on this thread's stack: re-derive the JIT's recursion ceiling from it.
        i.depth_limit = 0;
        i.tco_ok = saved_tco;
        i.native_top = saved_natives;
        match s {
            Ok(s) => {
                if matches!(s, Suspend::Done(_) | Suspend::Throw(_)) {
                    self.done = true;
                    self.views.finish_native_stack();
                }
                s
            }
            // The worker died (panicked) — treat as a finished generator. Its unwind skipped the
            // straight-line `fn_frames.pop()`s of any calls the body had in flight — across ALL
            // its resumes, whose frames may be interleaved with the driver's — while dropping
            // their callee handles, so those frames' `fn_ptr`s no longer point at live objects
            // (see `FnFrame::fn_ptr`). Evict exactly this body's frames by ownership tag before
            // anything (an error's `capture_stack`, `f.caller` reflection) reconstructs a handle
            // through one.
            Err(_) => {
                i.fn_frames.retain(|f| f.coro != self.id);
                crate::bytecode::jit::evict_coro_frames(i, self.id);
                self.done = true;
                self.views.finish_native_stack();
                Suspend::Done(Value::Undefined)
            }
        }
    }
}

/// Park the running coroutine, hand `msg` (a `Yield` or `Await`) to the driver, and block until
/// resumed. Restores the body's scalar context (which the driver mutated while it ran).
fn park_message(i: &mut Interp, msg: WorkerMessage) -> Resume {
    let (gen_strict, gen_depth, gen_tco) = (i.strict, i.depth, i.tco_ok);
    // The resumer runs arbitrary code in between (it may itself be another generator body).
    let gen_agb = i.in_async_gen_body;
    // The driver mutates `*i` through its own pointer while this thread blocks: escape `i` so
    // the compiler cannot treat the restores below as dead stores of unchanged values (`&mut`
    // is `noalias`, and the channel calls never see `i`).
    std::hint::black_box(&mut *i as *mut Interp);
    let resumed = YIELDER.with(|y| {
        let b = y.borrow();
        let yl = b.as_ref().expect("suspend outside a coroutine");
        let _ = yl.suspend_tx.send(msg);
        // A suspended activation must stay intact, but its unused allocation cache
        // need not stay resident during a long yield/await/driver-call handoff.
        receive_after_quiet_trim(&yl.resume_rx, || false)
    });
    match resumed {
        Ok(r) => {
            i.strict = gen_strict;
            i.depth = gen_depth;
            i.depth_limit = 0;
            i.tco_ok = gen_tco;
            i.in_async_gen_body = gen_agb;
            r
        }
        // `recv` errors only when the driver's `resume_tx` was dropped — i.e. the `Coroutine` (and
        // usually the owning `Engine`) is being torn down. Resuming the body here would run JS
        // (`finally` blocks, iterator-close) that dereferences the shared `*mut Interp` from this
        // thread while the main thread tears it down — a data race that surfaced as random
        // `RefCell already borrowed` panics / SIGSEGV. Instead, never touch the interpreter again:
        // park forever, holding only the captured `Rc`s. The detached thread is reaped at process
        // exit (the generator never outlives its `Engine`).
        Err(_) => {
            #[cfg(not(target_arch = "wasm32"))]
            crate::fastalloc::trim();
            loop {
                std::thread::park();
            }
        }
    }
}

/// Run a thread-affine host call on the realm's driver while the coroutine is parked.
pub(crate) fn driver_call(
    i: &mut Interp,
    call: Box<dyn FnOnce(&mut Interp) -> Result<Value, Value>>,
) -> Result<Value, Value> {
    if !in_coroutine() {
        return call(i);
    }
    match park_message(i, WorkerMessage::DriverCall(call)) {
        Resume::Next(value) => Ok(value),
        Resume::Throw(error) => Err(error),
        Resume::Return(_) => unreachable!("driver host call received generator return"),
    }
}

/// `yield value` — park producing a generator value.
pub fn coroutine_yield(i: &mut Interp, value: Value) -> Resume {
    park_message(i, WorkerMessage::Suspend(Suspend::Yield(value)))
}

/// `await value` — park waiting for `value` to settle.
pub fn coroutine_await(i: &mut Interp, value: Value) -> Resume {
    park_message(i, WorkerMessage::Suspend(Suspend::Await(value)))
}

/// Thrown as a JS `Error` when a coroutine cannot start (wasm32 has no OS threads, so
/// `std::thread::Builder::spawn` reports `Unsupported` there).
pub const UNSUPPORTED_MSG: &str =
    "generators and async functions require OS threads, which this WebAssembly build does not have";

/// One unit of work for a pooled worker: the interpreter pointer, the body to run, and this
/// coroutine's channel ends. All fields are `Send` (via the `unsafe impl`s above / channel `Send`),
/// so `Job` is `Send`; moving the captured `Rc`s to the worker is sound for the same ping-pong
/// reason `spawn` was — the driver stops touching them the instant it sends.
struct Job {
    ptr: InterpPtr,
    /// The driver's heap state: the body allocates into and tombstones out of the same registry
    /// as the interpreter that spawned it (see `value::GcState`).
    gc: std::sync::Arc<crate::value::GcState>,
    templates: std::sync::Arc<crate::parser::TemplateSites>,
    views: std::sync::Arc<crate::lstr::ViewRegistry>,
    /// The driver's `Symbol.for` registry (see `interpreter::sym_for_enter`).
    syms: DriverTable,
    body: SendBody,
    resume_rx: Receiver<Resume>,
    suspend_tx: Sender<WorkerMessage>,
}

/// A driver's thread-local table (the `Symbol.for` registry), used by the worker only while the
/// driver is parked.
struct DriverTable(*const ());
unsafe impl Send for DriverTable {}

/// Idle worker threads waiting for their next coroutine. Guarded by a plain `Mutex`: under the
/// strict ping-pong exactly one thread touches the interpreter at a time, so contention is
/// near-zero (a worker only pushes itself back *after* handing its final value to the driver).
const MAX_IDLE_WORKERS: usize = 8;

struct IdlePool<T> {
    entries: Vec<(std::sync::Arc<()>, Sender<T>)>,
}

impl<T> IdlePool<T> {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn take(&mut self) -> Option<Sender<T>> {
        self.entries.pop().map(|(_, sender)| sender)
    }

    fn offer(&mut self, identity: std::sync::Arc<()>, sender: Sender<T>) -> bool {
        if self.entries.len() >= MAX_IDLE_WORKERS {
            return false;
        }
        self.entries.push((identity, sender));
        true
    }

    // Removing under the same lock as take fences retirement against checkout. Absence
    // means the driver already reserved this worker and may still be constructing its job.
    fn retire(&mut self, identity: &std::sync::Arc<()>) -> bool {
        match self
            .entries
            .iter()
            .position(|(entry, _)| std::sync::Arc::ptr_eq(entry, identity))
        {
            Some(index) => {
                self.entries.swap_remove(index);
                true
            }
            None => false,
        }
    }
}

static IDLE: Mutex<IdlePool<Job>> = Mutex::new(IdlePool::new());

/// Grab an idle worker, or start a new one. `Err` when the platform cannot spawn threads (wasm32).
fn get_worker() -> std::io::Result<Sender<Job>> {
    if let Some(tx) = IDLE.lock().unwrap().take() {
        return Ok(tx);
    }
    let (job_tx, job_rx) = channel::<Job>();
    let self_tx = job_tx.clone();
    // Generous stack: execution recurses up to MAX_EVAL_DEPTH units (see its sizing).
    crate::spawn_engine_thread("lumen-coroutine", 0, move || worker_loop(job_rx, self_tx))
        .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
    Ok(job_tx)
}

/// A pooled worker: run one coroutine to completion, return to the idle pool, repeat. A worker that
/// parks forever (its `Engine` was torn down while it was suspended; see `park`) simply never comes
/// back — the same leak-at-teardown as the pre-pool one-thread-per-coroutine design.
fn worker_loop(job_rx: Receiver<Job>, self_tx: Sender<Job>) {
    let identity = std::sync::Arc::new(());
    while let Ok(job) = receive_after_quiet_trim(&job_rx, || IDLE.lock().unwrap().retire(&identity))
    {
        run_job(job);
        if !IDLE
            .lock()
            .unwrap()
            .offer(identity.clone(), self_tx.clone())
        {
            return;
        }
    }
}

// Release unused thread-local allocation blocks after one quiet second. A suspended
// activation always uses retire=false and stays intact. Completed helpers may retire
// only an unreserved pool offer after restoring the driver's TLS.
fn receive_after_quiet_trim<T>(
    rx: &Receiver<T>,
    retire: impl FnOnce() -> bool,
) -> Result<T, std::sync::mpsc::RecvError> {
    match rx.recv_timeout(std::time::Duration::from_secs(1)) {
        Ok(job) => Ok(job),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(std::sync::mpsc::RecvError),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            #[cfg(not(target_arch = "wasm32"))]
            crate::fastalloc::trim();
            if retire() {
                Err(std::sync::mpsc::RecvError)
            } else {
                rx.recv()
            }
        }
    }
}

#[cfg(test)]
mod idle_tests {
    use super::*;

    #[test]
    fn idle_pool_never_offers_more_than_its_warm_limit() {
        let mut pool = IdlePool::new();
        for _ in 0..MAX_IDLE_WORKERS {
            let (tx, _) = channel::<()>();
            assert!(pool.offer(std::sync::Arc::new(()), tx));
        }
        let (tx, _) = channel::<()>();
        assert!(!pool.offer(std::sync::Arc::new(()), tx));
        assert_eq!(pool.entries.len(), MAX_IDLE_WORKERS);
    }

    #[test]
    fn quiet_pool_worker_exits_and_removes_its_exact_offer() {
        let pool = std::sync::Arc::new(Mutex::new(IdlePool::new()));
        let identity = std::sync::Arc::new(());
        let (tx, rx) = channel::<()>();
        assert!(pool.lock().unwrap().offer(identity.clone(), tx.clone()));
        let other = std::sync::Arc::new(());
        assert!(!pool.lock().unwrap().retire(&other));
        let owner = pool.clone();
        let worker = std::thread::spawn(move || {
            receive_after_quiet_trim(&rx, || owner.lock().unwrap().retire(&identity)).is_err()
        });
        assert!(worker.join().unwrap());
        assert!(pool.lock().unwrap().take().is_none());
        assert!(tx.send(()).is_err());
    }

    #[test]
    fn checked_out_worker_survives_timeout_before_job_dispatch() {
        let pool = std::sync::Arc::new(Mutex::new(IdlePool::new()));
        let identity = std::sync::Arc::new(());
        let (tx, rx) = channel::<u8>();
        assert!(pool.lock().unwrap().offer(identity.clone(), tx));
        let reservation = pool.lock().unwrap().take().unwrap();
        let (timeout_tx, timeout_rx) = channel();
        let worker = std::thread::spawn(move || {
            receive_after_quiet_trim(&rx, || {
                let retired = pool.lock().unwrap().retire(&identity);
                timeout_tx.send(retired).unwrap();
                retired
            })
            .unwrap()
        });
        // Dispatch deliberately follows the worker's timeout; an absent offer is reserved,
        // not eligible for exit. No speculative resend or second worker is needed.
        assert!(!timeout_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap());
        reservation.send(42).unwrap();
        assert_eq!(worker.join().unwrap(), 42);
    }

    #[test]
    fn suspended_activation_resumes_after_the_quiet_trim_deadline() {
        let mut engine = crate::Engine::new();
        let first = engine
            .eval(
                r#"
            let suspended = (function* (value) {
                "use strict";
                const text = "a captured string";
                const next = yield value + arguments.length;
                return value + next + (text === "a captured string" ? 1 : 0);
            })(40);
            suspended.next().value;
        "#,
                false,
            )
            .unwrap();
        assert!(matches!(first, crate::Completion::Value(value) if value == "41"));
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let resumed = engine
            .eval("$262.gc(); suspended.next(8).value;", false)
            .unwrap();
        assert!(matches!(resumed, crate::Completion::Value(value) if value == "49"));
    }

    #[test]
    fn silent_coroutine_pool_wait_releases_native_cache_before_reuse() {
        let (job_tx, job_rx) = channel::<()>();
        let (ready_tx, ready_rx) = channel();
        let worker = std::thread::spawn(move || {
            use std::alloc::{GlobalAlloc, Layout};
            let allocator = crate::fastalloc::ClassAlloc;
            let layout = Layout::from_size_align(112, 16).unwrap();
            let blocks: Vec<_> = (0..20_000)
                .map(|_| {
                    // The library leaves global allocator selection to its embedder. Exercise
                    // its cache explicitly with matching allocation/deallocation layouts.
                    let block = unsafe { allocator.alloc(layout) };
                    assert!(!block.is_null());
                    block
                })
                .collect();
            for block in blocks {
                unsafe {
                    allocator.dealloc(block, layout);
                }
            }
            let cached = crate::fastalloc::cached_bytes_for_test();
            ready_tx.send(cached).unwrap();
            receive_after_quiet_trim(&job_rx, || false).unwrap();
            crate::fastalloc::cached_bytes_for_test()
        });
        assert!(ready_rx.recv().unwrap() >= 2_000_000);
        std::thread::sleep(std::time::Duration::from_millis(1500));
        job_tx.send(()).unwrap();
        assert!(worker.join().unwrap() < 64 * 1024);
    }
}

struct RestoreDriverTables(*const ());

impl Drop for RestoreDriverTables {
    fn drop(&mut self) {
        crate::interpreter::sym_for_enter(self.0 as *const _);
    }
}

struct RestoreGcState(Option<std::sync::Arc<crate::value::GcState>>);

impl Drop for RestoreGcState {
    fn drop(&mut self) {
        if let Some(own) = self.0.take() {
            crate::value::enter_gc_state(own);
        }
    }
}

struct RestoreViewRegistry(Option<std::sync::Arc<crate::lstr::ViewRegistry>>);
impl Drop for RestoreViewRegistry {
    fn drop(&mut self) {
        if let Some(own) = self.0.take() {
            crate::lstr::enter_view_registry(own);
        }
    }
}

struct RestoreTemplateSites(Option<std::sync::Arc<crate::parser::TemplateSites>>);
impl Drop for RestoreTemplateSites {
    fn drop(&mut self) {
        if let Some(own) = self.0.take() {
            crate::parser::enter_template_sites(own);
        }
    }
}

/// Set up this thread's coroutine TLS, run the body from its first drive to completion, and hand
/// the outcome to the driver. Resets the per-thread coroutine state a reused worker would otherwise
/// inherit from its previous job.
fn run_job(job: Job) {
    let Job {
        ptr,
        gc,
        templates,
        views,
        syms,
        body,
        resume_rx,
        suspend_tx,
    } = job;
    // The body's `Symbol.for` lookups must see the driver's table, not this thread's.
    let _restore_syms =
        RestoreDriverTables(crate::interpreter::sym_for_enter(syms.0 as *const _) as *const ());
    let SendBody(body) = body;
    // Everything this job drops — the captured closure on an undriven job, the values a body
    // leaves behind — belongs to the driver's registry, so route it there until the job is done.
    let own_templates = crate::parser::enter_template_sites(templates);
    let _restore_templates = RestoreTemplateSites(Some(own_templates));
    let own_views = crate::lstr::enter_view_registry(views);
    let _restore_views = RestoreViewRegistry(Some(own_views));
    let own_gc = crate::value::enter_gc_state(gc);
    let _restore = RestoreGcState(Some(own_gc));
    // A plain async body never sets ASYNC_GEN, so it must not inherit a previous async-generator
    // job's `true`.
    ASYNC_GEN.with(|c| c.set(false));
    YIELDER.with(|y| {
        *y.borrow_mut() = Some(Yielder {
            suspend_tx: suspend_tx.clone(),
            resume_rx,
        })
    });
    // Park until the first next()/return()/throw(); the body doesn't run before then.
    let first = YIELDER.with(|y| y.borrow().as_ref().unwrap().resume_rx.recv());
    let outcome = match first {
        Err(_) => {
            // Dropped before its first drive (see `ThreadCoro`'s `Drop`): the driver is parked
            // waiting for this job to release the body's `Rc`s, so drop them now, while nothing
            // else runs, then let the driver go.
            YIELDER.with(|y| *y.borrow_mut() = None);
            drop(body);
            let _ = suspend_tx.send(WorkerMessage::Suspend(Suspend::Done(Value::Undefined)));
            return;
        }
        // The first `next(v)`'s argument is unobservable; release it before the body runs, not
        // after the final handoff, when the driver is running again.
        Ok(Resume::Next(v)) => {
            drop(v);
            let interp = unsafe { &mut *ptr.0 };
            body(interp)
        }
        // `return()`/`throw()` before the first `next()`: the body never runs. Release its
        // captures here, into the driver's registry and while the driver is parked — left to the
        // end of this function they would drop after the handoff (racing the driver) and after
        // `_restore` (tombstoning the worker's registry, leaving the driver's slots dangling).
        Ok(Resume::Return(v)) => {
            drop(body);
            Suspend::Done(v)
        }
        Ok(Resume::Throw(e)) => {
            drop(body);
            Suspend::Throw(e)
        }
    };
    let _ = suspend_tx.send(WorkerMessage::Suspend(outcome));
    // Clear the TLS so the next job starts clean and `in_coroutine()` reads false between jobs.
    YIELDER.with(|y| *y.borrow_mut() = None);
}

/// Spawn a coroutine over `body` on a pooled worker, parked until its first [`Coroutine::resume`].
/// `Err` when the platform cannot spawn threads (wasm32).
pub fn spawn_coroutine(interp: *mut Interp, body: SendBody) -> std::io::Result<Coroutine> {
    let (resume_tx, resume_rx) = channel::<Resume>();
    let (suspend_tx, suspend_rx) = channel::<WorkerMessage>();
    let worker = get_worker()?;
    let job = Job {
        ptr: InterpPtr(interp),
        gc: crate::value::gc_state_handle(),
        templates: crate::parser::template_sites_handle(),
        views: crate::lstr::view_registry_handle(),
        syms: DriverTable(crate::interpreter::sym_for_handle() as *const ()),
        body,
        resume_rx,
        suspend_tx,
    };
    // The worker is idle in `job_rx.recv()`; hand it this coroutine. A send failure means the worker
    // vanished — surface it like a failed spawn rather than wedging.
    worker
        .send(job)
        .map_err(|_| std::io::Error::other("coroutine worker unavailable"))?;
    Ok(Coroutine::Thread(ThreadCoro {
        id: CORO_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        resume_tx,
        suspend_rx,
        done: false,
        started: false,
        views: crate::lstr::view_registry_handle(),
        frame: Default::default(),
    }))
}
