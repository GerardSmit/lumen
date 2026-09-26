//! lumen-host — the substrate shared by every op crate and the runtime.
//!
//! An *op crate* (timers, fs, ...) exports one [`Extension`]: a named bundle of native
//! functions plus host-state initialization. A runtime is assembled by [`install`]ing a list
//! of extensions into an [`Engine`]. Rust state never lives in the native fns themselves
//! (they are bare `fn` pointers): it lives in [`OpState`], reached through the `&mut Ctx`
//! argument every native fn receives.
//!
//! Two scheduling primitives serve every async op (this mirrors libuv's own fs strategy —
//! regular files are not pollable, so async fs is threadpool + completion, not readiness):
//! - [`ThreadPool::spawn_blocking`]: run blocking work off-thread; its result comes back to
//!   the loop thread as a [`TaskCompletion`] over `mpsc`.
//! - [`CallbackQueue`]: loop-thread-local queue of JS callbacks to fire on the next turn
//!   (JS values are `!Send`, so they never cross threads; off-thread work refers to its
//!   callback by [`TaskId`]).

use std::any::Any;
use std::collections::VecDeque;
use std::sync::mpsc;

pub use lumen::bytecode::Tier;
pub use lumen::embed::{Ctx, NativeClosure, NativeFn, OpState, ResourceId, ResourceTable, Value};
pub use lumen::{well_formed_utf8, Completion, Engine, ParseError};

/// DEFLATE/zlib/gzip codec (std-only), shared by web CompressionStream and node:zlib.
pub mod deflate;

/// Brotli (RFC 7932) codec (std-only), for node:zlib brotli* APIs.
pub mod brotli;

/// Zstandard (RFC 8878) codec (std-only), for node:zlib zstd* and Bun.zstd* APIs.
pub mod zstd;

/// One native op: a named native function with its JS arity.
#[derive(Clone, Copy)]
pub struct OpDecl {
    pub name: &'static str,
    pub len: usize,
    pub f: NativeFn,
}

/// Declarative op-declaration table: `ops!["add" (2) => add_impl, ...]`. Uniform by design so
/// op registration stays a table, never hand-written glue (deno's `#[op2]` lesson).
#[macro_export]
macro_rules! ops {
    ($($name:literal ($len:expr) => $f:expr),* $(,)?) => {
        &[$($crate::OpDecl { name: $name, len: $len, f: $f }),*]
    };
}

/// A bundle of native ops + host-state init, exported one per op crate. Composing a runtime
/// is `install(&mut engine, &[timers::extension(), fs::extension(), ...])`.
pub struct Extension {
    pub name: &'static str,
    /// Installed as `globalThis.<name>` functions (e.g. `setTimeout`).
    pub globals: &'static [OpDecl],
    /// Installed as `globalThis.<ns>.<name>` namespace methods (e.g. a `__lumen_fs` ops
    /// object that a JS shim wraps into the public API).
    pub namespaces: &'static [(&'static str, &'static [OpDecl])],
    /// Installs this extension's state (timer heap, fd table, ...) into [`OpState`].
    pub state_init: Option<fn(&mut OpState)>,
    /// JS glue evaluated after this extension's ops are installed — the promise-returning
    /// public API is JS wrapping raw callback ops (e.g. `fs.promises` over `__fs_async`).
    /// A parse/throw here is a bug in the extension: `install` panics with its name.
    pub js_init: Option<&'static str>,
    /// A build-time snapshot of `js_init`'s parsed AST (see `Engine::eval_snapshot`). When
    /// present, `install` decodes it instead of re-lexing/parsing `js_init` every boot — the
    /// dominant cold-start cost. A decode failure (version skew) falls back to `js_init`, so it
    /// is a pure optimization. `js_init` must still be set: it is the fallback source, and the
    /// text the snapshot's functions read their source ranges and unparsed bodies from.
    ///
    /// It may instead be an ahead-of-time blob (`lumen::precompiled::precompile_glue`, starting
    /// `LUMENAOT`): the glue's AST, bytecode and (compressed) function text, loaded with
    /// `Engine::load_precompiled` — then `js_init` may be `None` (it is only a fallback).
    pub js_init_snapshot: Option<&'static [u8]>,
}

impl Extension {
    /// An empty extension named `name`; fill in the fields that apply.
    pub const fn new(name: &'static str) -> Extension {
        Extension {
            name,
            globals: &[],
            namespaces: &[],
            state_init: None,
            js_init: None,
            js_init_snapshot: None,
        }
    }
}

/// Install extensions into an engine: state first (an op may fire during install), then ops,
/// then JS glue.
pub fn install(engine: &mut Engine, extensions: &[Extension]) {
    let timing = startup_timing();
    lumen::memstats::phase("engine created");
    if lumen::memstats::enabled() {
        // Per-file checkpoints of a glue built with LUMEN_GLUE_MEM_MARKS=1 (see lumen-node's
        // build.rs).
        engine.define_global("__lumenMemMark", 1, |ctx, _this, args| {
            let label = match args.first() {
                Some(v) => ctx.coerce_string(v).map(|s| s.to_string()).unwrap_or_default(),
                None => String::new(),
            };
            lumen::memstats::phase(&format!("    glue {label}"));
            Ok(lumen::embed::Value::Undefined)
        });
    }
    for ext in extensions {
        let _mem = lumen::memstats::enter(lumen::memstats::Cat::Glue);
        let t0 = timing.then(std::time::Instant::now);
        if let Some(init) = ext.state_init {
            init(engine.ctx().op_state());
        }
        for op in ext.globals {
            engine.define_global(op.name, op.len, op.f);
        }
        for (ns, ops) in ext.namespaces {
            let table: Vec<(&str, usize, NativeFn)> =
                ops.iter().map(|o| (o.name, o.len, o.f)).collect();
            engine.define_namespace(ns, &table);
        }
        let aot = ext
            .js_init_snapshot
            .filter(|b| b.starts_with(b"LUMENAOT"));
        if let Some(blob) = aot {
            let tier = engine.tier();
            engine.set_tier(lumen::bytecode::Tier::Interp);
            let loaded = engine.load_precompiled(&lumen::Precompiled::from_static(blob));
            let completion = match (loaded, ext.js_init) {
                (Err(_), Some(src)) => engine.eval(src, false),
                (r, _) => r,
            };
            engine.set_tier(tier);
            match completion {
                Ok(Completion::Value(_)) => {}
                Ok(Completion::Throw { name, message }) => {
                    panic!("extension '{}' js_init threw {name}: {message}", ext.name)
                }
                Err(e) => panic!("extension '{}' js_init: {}", ext.name, e.message),
            }
        } else if let Some(src) = ext.js_init {
            // The glue's setup path runs once, in the tree-walker: compiling it would parse the
            // body of every function it defines (the capture scan needs them) for code that
            // never runs again. Functions it defines tier up on their own calls as usual.
            let tier = engine.tier();
            engine.set_tier(lumen::bytecode::Tier::Interp);
            // Prefer the precompiled snapshot (skips lex+parse); on a decode failure fall back to
            // parsing the source, so the snapshot can never change behavior — only speed.
            let completion = ext
                .js_init_snapshot
                .filter(|_| std::env::var_os("LUMEN_NO_SNAPSHOT").is_none())
                .and_then(|bytes| engine.eval_snapshot(bytes, src, false).ok())
                .map(Ok)
                .unwrap_or_else(|| engine.eval(src, false));
            engine.set_tier(tier);
            match completion {
                Ok(Completion::Value(_)) => {}
                Ok(Completion::Throw { name, message }) => {
                    panic!("extension '{}' js_init threw {name}: {message}", ext.name)
                }
                Err(e) => panic!(
                    "extension '{}' js_init: SyntaxError: {}",
                    ext.name, e.message
                ),
            }
        }
        if let Some(t0) = t0 {
            eprintln!("[startup] install {:<8} {:?}", ext.name, t0.elapsed());
        }
        if lumen::memstats::enabled() {
            lumen::memstats::phase(&format!("install {}", ext.name));
        }
    }
}

/// Whether `LUMEN_STARTUP_TIMING` is set: startup phases then report their wall time on stderr.
pub fn startup_timing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LUMEN_STARTUP_TIMING").is_some())
}

/// Identifies an in-flight async task. The op that spawns work registers `TaskId -> JS
/// callback/promise` in its own [`OpState`] slot; the completion carries the id back so the
/// loop thread can look the JS value up (JS values themselves are `!Send`).
pub type TaskId = u64;

/// What off-thread work sends back to the loop thread. `result` is whatever `Send` payload
/// the spawning op chose; that op downcasts it when the loop hands the completion over.
pub struct TaskCompletion {
    pub task: TaskId,
    pub result: Box<dyn Any + Send>,
}

/// JS callbacks queued (on the loop thread) to run on the next loop turn — the
/// `enqueue_callback` primitive. Lives in [`OpState`]; the runtime drains it each turn.
#[derive(Default)]
pub struct CallbackQueue {
    pub queue: VecDeque<(Value, Vec<Value>)>,
}

impl CallbackQueue {
    /// Queue `callback(args...)` for the next loop turn.
    pub fn enqueue(state: &mut OpState, callback: Value, args: Vec<Value>) {
        if !state.has::<CallbackQueue>() {
            state.put(CallbackQueue::default());
        }
        state
            .get_mut::<CallbackQueue>()
            .expect("just installed")
            .queue
            .push_back((callback, args));
    }
}

enum Task {
    /// Work whose result goes back to the loop as a [`TaskCompletion`] tagged `id`.
    Tracked {
        id: TaskId,
        work: Box<dyn FnOnce() -> Box<dyn Any + Send> + Send>,
    },
    /// Work that reports back by itself (an async op's job holds a `Completer`).
    Detached(Box<dyn FnOnce() + Send>),
}

/// A fixed pool of std worker threads running blocking work (`std::fs`, blocking
/// `std::net`); completions come back over the `mpsc` channel given at construction. This is
/// the whole async-I/O story until (if ever) a hand-rolled readiness reactor on raw platform
/// syscalls is explicitly authorized — never via a crate.
pub struct ThreadPool {
    work_tx: Option<mpsc::Sender<Task>>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl ThreadPool {
    /// `size` worker threads sending [`TaskCompletion`]s to `completions` (the loop thread
    /// holds the receiving end).
    pub fn new(size: usize, completions: mpsc::Sender<TaskCompletion>) -> ThreadPool {
        let (work_tx, work_rx) = mpsc::channel::<Task>();
        // std's mpsc receiver is single-consumer: share it across workers behind a mutex.
        let work_rx = std::sync::Arc::new(std::sync::Mutex::new(work_rx));
        let workers = (0..size.max(1))
            .map(|_| {
                let work_rx = std::sync::Arc::clone(&work_rx);
                let completions = completions.clone();
                std::thread::spawn(move || loop {
                    let task = match work_rx.lock().expect("worker queue poisoned").recv() {
                        Ok(t) => t,
                        Err(_) => return, // pool dropped: no more work
                    };
                    match task {
                        Task::Tracked { id, work } => {
                            let result = work();
                            // The loop shutting down first is fine; the result just has nowhere
                            // to go.
                            let _ = completions.send(TaskCompletion { task: id, result });
                        }
                        Task::Detached(job) => job(),
                    }
                })
            })
            .collect();
        ThreadPool {
            work_tx: Some(work_tx),
            workers,
        }
    }

    /// Run `work` on a pool thread; its return value comes back to the loop as a
    /// [`TaskCompletion`] tagged with `id`.
    pub fn spawn_blocking(
        &self,
        id: TaskId,
        work: impl FnOnce() -> Box<dyn Any + Send> + Send + 'static,
    ) {
        self.work_tx
            .as_ref()
            .expect("pool shut down")
            .send(Task::Tracked {
                id,
                work: Box::new(work),
            })
            .expect("worker threads gone");
    }

    /// A cloneable spawn handle. The runtime puts one in [`OpState`], which is how a native fn
    /// (holding only `&mut Ctx`) reaches the pool.
    pub fn handle(&self) -> SpawnHandle {
        SpawnHandle {
            work_tx: self.work_tx.clone().expect("pool shut down"),
        }
    }
}

/// [`ThreadPool::spawn_blocking`] as an [`OpState`]-storable handle, so op crates can spawn
/// blocking work from inside a native fn.
#[derive(Clone)]
pub struct SpawnHandle {
    work_tx: mpsc::Sender<Task>,
}

impl SpawnHandle {
    pub fn spawn_blocking(
        &self,
        id: TaskId,
        work: impl FnOnce() -> Box<dyn Any + Send> + Send + 'static,
    ) {
        self.work_tx
            .send(Task::Tracked {
                id,
                work: Box::new(work),
            })
            .expect("worker threads gone");
    }

    /// Run `job` on a pool thread; it reports back by itself (e.g. through a
    /// [`lumen::embed::Completer`]), so no completion is sent for it.
    pub fn spawn_detached(&self, job: Box<dyn FnOnce() + Send>) {
        self.work_tx
            .send(Task::Detached(job))
            .expect("worker threads gone");
    }
}

/// Sends [`TaskCompletion`]s straight to the loop from a *dedicated* thread, bypassing the fixed
/// [`ThreadPool`]. For work that blocks for an unbounded time — a subprocess's stdout read, waiting
/// on a child to exit — where occupying a shared pool worker for the whole duration would starve
/// everything else. The runtime stores one in [`OpState`]. `run_blocking` spawns a fresh thread per
/// call; blocked threads cost only memory, not a pool slot.
#[derive(Clone)]
pub struct CompletionSender {
    tx: mpsc::Sender<TaskCompletion>,
}

impl CompletionSender {
    pub fn new(tx: mpsc::Sender<TaskCompletion>) -> CompletionSender {
        CompletionSender { tx }
    }
    /// Run `work` on a new dedicated thread; its result comes back to the loop as a
    /// [`TaskCompletion`] tagged with `id` (settled through the [`TaskRegistry`], like pool work).
    pub fn run_blocking(
        &self,
        id: TaskId,
        work: impl FnOnce() -> Box<dyn Any + Send> + Send + 'static,
    ) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = work();
            let _ = tx.send(TaskCompletion { task: id, result });
        });
    }
    /// Deliver a completion from any thread (an I/O callback, a readiness thread).
    pub fn send(&self, id: TaskId, result: Box<dyn Any + Send>) {
        let _ = self.tx.send(TaskCompletion { task: id, result });
    }
}

/// The event loop's [`AsyncHost`](lumen::embed::AsyncHost): what `#[op(async)]`,
/// [`Ctx::spawn_blocking`] and [`Ctx::completer`] run on inside a lumen runtime. A pending
/// promise is a [`TaskRegistry`] entry (so it keeps the loop alive) whose resolve/reject pair
/// the loop calls when the completion arrives; the completion's payload is the
/// [`Settle`](lumen::embed::Settle) closure, run on the loop thread to build the JS value.
pub struct LoopAsyncHost {
    pub spawn: SpawnHandle,
    pub completions: CompletionSender,
}

impl lumen::embed::AsyncHost for LoopAsyncHost {
    fn pending(&self, ctx: &mut Ctx, deferred: lumen::embed::Deferred) -> lumen::embed::Completer {
        let (resolve, reject) = deferred.resolving_functions(ctx);
        let id = ctx
            .host_mut::<TaskRegistry>()
            .expect("the runtime installs the task registry with its async host")
            .register(resolve, Some(reject), decode_settle);
        let completions = self.completions.clone();
        lumen::embed::Completer::new(move |settle| completions.send(id, Box::new(settle)))
    }

    fn spawn(&self, job: Box<dyn FnOnce() + Send>, dedicated: bool) {
        if dedicated {
            std::thread::spawn(job);
        } else {
            self.spawn.spawn_detached(job);
        }
    }
}

/// [`TaskDecoder`] for [`LoopAsyncHost`] completions: run the `Settle` on the loop thread.
fn decode_settle(ctx: &mut Ctx, payload: Box<dyn Any + Send>) -> Result<Vec<Value>, Value> {
    let settle = payload
        .downcast::<lumen::embed::Settle>()
        .expect("async completion carries a Settle");
    settle(ctx).map(|v| vec![v])
}

/// Turns a completed task's `Send` payload back into JS callback arguments, on the loop
/// thread. `Err` is a JS value to report as an uncaught exception (later: a rejection).
pub type TaskDecoder = fn(&mut Ctx, Box<dyn Any + Send>) -> Result<Vec<Value>, Value>;

/// In-flight async tasks: `TaskId -> (JS callback, payload decoder)`. Lives in [`OpState`];
/// the op that spawns work registers here, the loop settles from [`TaskCompletion`]s. The
/// event loop stays alive while this is non-empty.
#[derive(Default)]
pub struct TaskRegistry {
    next: TaskId,
    map: std::collections::HashMap<TaskId, TaskEntry>,
}

/// How to settle one in-flight task: success callback, optional failure callback (a promise's
/// reject — when absent, a decode error is reported as an uncaught exception), and the
/// payload decoder.
pub struct TaskEntry {
    pub on_ok: Value,
    pub on_err: Option<Value>,
    pub decode: TaskDecoder,
    /// An `unref`'d task still settles when it completes, but does not by itself keep the event
    /// loop alive (Node's `child.unref()` — e.g. esbuild's persistent service child).
    pub unref: bool,
}

impl TaskRegistry {
    /// Reserve an id for work about to be spawned, remembering how to settle it.
    pub fn register(&mut self, on_ok: Value, on_err: Option<Value>, decode: TaskDecoder) -> TaskId {
        let id = self.next;
        self.next += 1;
        self.map.insert(
            id,
            TaskEntry {
                on_ok,
                on_err,
                decode,
                unref: false,
            },
        );
        id
    }
    /// Claim a completed task's settlement entry (a missing id means it was cancelled).
    pub fn take(&mut self, id: TaskId) -> Option<TaskEntry> {
        self.map.remove(&id)
    }
    /// Mark a pending task as `unref`'d (see [`TaskEntry::unref`]).
    pub fn set_unref(&mut self, id: TaskId) {
        if let Some(e) = self.map.get_mut(&id) {
            e.unref = true;
        }
    }
    /// Re-`ref` a pending task so it keeps the loop alive again (Node's `handle.ref()`).
    pub fn set_ref(&mut self, id: TaskId) {
        if let Some(e) = self.map.get_mut(&id) {
            e.unref = false;
        }
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    /// Whether any *ref*'d (loop-keeping) task is pending. Unref'd tasks are ignored — they
    /// settle if they complete but must not hold the process open.
    pub fn has_ref_pending(&self) -> bool {
        self.map.values().any(|e| !e.unref)
    }
}

impl Drop for ThreadPool {
    fn drop(&mut self) {
        // Closing the work channel ends each worker's recv loop; join so no worker outlives
        // the runtime that owns the completion receiver.
        self.work_tx.take();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests;

/// Starts a subprocess for `child_process`. The default is `command.spawn()`; an embedder that runs
/// realms inside its own process supplies one that puts the child where it belongs (a job object
/// or cgroup per realm) — the realm has no process of its own for descendants to inherit.
pub trait Spawner: Send + Sync {
    fn spawn(&self, command: &mut std::process::Command) -> std::io::Result<std::process::Child>;
}

/// Present in [`OpState`] when the runtime runs inside a host process instead of owning one (see
/// `lumen_runtime::Embedding`). Everything a Node program believes is process-wide and a realm
/// must not change for its host lives here instead: the working directory, the exit request,
/// stdin, and how subprocesses start. Ops that would otherwise reach the OS consult it first.
pub struct RealmProcess {
    /// `process.cwd()`; `process.chdir` moves only this.
    pub cwd: std::path::PathBuf,
    /// Set by `process.exit` / `process.abort`: the realm is terminating with this code.
    pub exit_code: Option<i32>,
    /// The realm's stop request; `process.exit` sets it so the engine unwinds at its next safe
    /// point.
    pub interrupt: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// `process.stdin`'s source. Shared because a blocking read runs on its own thread.
    pub stdin: std::sync::Arc<std::sync::Mutex<Box<dyn std::io::Read + Send>>>,
    /// What fd 1 and fd 2 write to: the same writers `console` and `process.stdout` use.
    pub stdout: std::sync::Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>,
    pub stderr: std::sync::Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>,
    pub spawner: Option<std::sync::Arc<dyn Spawner>>,
}

impl RealmProcess {
    /// Resolve `path` against the realm's working directory (absolute paths pass through).
    pub fn resolve(&self, path: &str) -> std::path::PathBuf {
        let p = std::path::Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cwd.join(p)
        }
    }
}

/// Read from fd 0: the realm's stdin when embedded, the process's otherwise.
pub fn read_stdin_fd(ctx: &mut Ctx, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::io::Read;
    let realm = ctx
        .op_state()
        .get::<RealmProcess>()
        .map(|realm| std::sync::Arc::clone(&realm.stdin));
    match realm {
        Some(stdin) => stdin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .read(buf),
        None => std::io::stdin().read(buf),
    }
}

/// Write to fd 1 or fd 2: the realm's writers when embedded, the process's otherwise.
pub fn write_std_fd(ctx: &mut Ctx, fd: u32, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let realm = ctx.op_state().get::<RealmProcess>().map(|realm| {
        std::sync::Arc::clone(if fd == 2 { &realm.stderr } else { &realm.stdout })
    });
    match realm {
        Some(writer) => {
            let mut writer = writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            writer.write_all(bytes)?;
            writer.flush()
        }
        None if fd == 2 => {
            let mut stderr = std::io::stderr();
            stderr.write_all(bytes)?;
            stderr.flush()
        }
        None => {
            let mut stdout = std::io::stdout();
            stdout.write_all(bytes)?;
            stdout.flush()
        }
    }
}

/// `std::fs::canonicalize` as Node's `realpath` reports it. On Windows std returns verbatim paths
/// (`\\?\C:\dir`, `\\?\UNC\server\share`), which Node never shows a program: they leak into
/// `__filename`, `require.cache` keys and paths handed to Node APIs that reject them.
pub fn canonicalize(path: impl AsRef<std::path::Path>) -> std::io::Result<std::path::PathBuf> {
    std::fs::canonicalize(path).map(strip_verbatim)
}

/// Drop Windows' verbatim prefix: `\\?\C:\x` -> `C:\x`, `\\?\UNC\srv\share` -> `\\srv\share`.
pub fn strip_verbatim(path: std::path::PathBuf) -> std::path::PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return std::path::PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) => std::path::PathBuf::from(rest),
        None => path,
    }
}

/// Spawn `command` through the realm's [`Spawner`] when one is installed, else directly.
pub fn spawn_command(
    ctx: &mut Ctx,
    command: &mut std::process::Command,
) -> std::io::Result<std::process::Child> {
    #[cfg(windows)]
    std_handles_not_inheritable();
    let spawner = ctx
        .op_state()
        .get::<RealmProcess>()
        .and_then(|realm| realm.spawner.clone());
    match spawner {
        Some(spawner) => spawner.spawn(command),
        None => command.spawn(),
    }
}

/// Windows `CreateProcess` (as std calls it) hands every *inheritable* handle to the child. Our
/// own stdin/stdout/stderr usually arrived inheritable, so without this a child spawned with
/// `stdio: "ignore"` (say a detached daemon) would still hold our stdout pipe open, and a parent
/// reading that pipe would never see EOF (libuv avoids the leak the same way). Children that
/// should inherit them still do: `Stdio::inherit` hands over an inheritable duplicate.
#[cfg(windows)]
fn std_handles_not_inheritable() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        #[link(name = "kernel32", kind = "raw-dylib")]
        extern "system" {
            fn GetStdHandle(which: u32) -> *mut core::ffi::c_void;
            fn SetHandleInformation(handle: *mut core::ffi::c_void, mask: u32, flags: u32) -> i32;
        }
        const HANDLE_FLAG_INHERIT: u32 = 1;
        // STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE: (DWORD)-10, -11, -12.
        for which in [-10i32, -11, -12] {
            // SAFETY: GetStdHandle returns null / INVALID_HANDLE_VALUE or a live handle, and
            // SetHandleInformation only fails (harmlessly) on the former.
            unsafe {
                let handle = GetStdHandle(which as u32);
                if !handle.is_null() && handle as isize != -1 {
                    SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
    });
}

/// Fill `buf` from the operating system's CSPRNG: `ProcessPrng` on Windows (what std itself
/// uses), `/dev/urandom` elsewhere.
pub fn fill_random(buf: &mut [u8]) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        #[link(name = "bcryptprimitives", kind = "raw-dylib")]
        extern "system" {
            fn ProcessPrng(data: *mut u8, len: usize) -> i32;
        }
        // Documented to always succeed (it returns TRUE); checked anyway.
        if unsafe { ProcessPrng(buf.as_mut_ptr(), buf.len()) } == 0 {
            return Err(std::io::Error::other("ProcessPrng failed"));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        use std::io::Read;
        static URANDOM: std::sync::OnceLock<std::fs::File> = std::sync::OnceLock::new();
        let file = match URANDOM.get() {
            Some(file) => file,
            None => {
                let opened = std::fs::File::open("/dev/urandom")?;
                URANDOM.get_or_init(|| opened)
            }
        };
        (&*file).read_exact(buf)
    }
}
