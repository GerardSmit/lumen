//! lumen-runtime — the event loop that turns the lumen engine into a runtime.
//!
//! One [`Runtime`] = one engine (one realm) + the installed extensions (timers, console,
//! process) + a threadpool for blocking work. The engine is `!Send`, so the thread that
//! creates the runtime owns it; everything asynchronous funnels back to that thread as
//! either a queued JS callback or an mpsc [`TaskCompletion`].
//!
//! A loop turn, in order (see [`Runtime::run_to_completion`]):
//! 1. drain **microtasks** (promise reactions),
//! 2. run queued **callbacks** (`process.nextTick`, `setImmediate`),
//! 3. fire due **timers**,
//! 4. dispatch ready **completions** from the threadpool,
//! then block on the completion channel until the next timer deadline (or indefinitely if
//! only completions remain). The loop exits when nothing is pending anywhere. A readiness
//! reactor (epoll/kqueue) would need raw syscalls and stays out unless explicitly authorized;
//! threadpool + completions is libuv's own fs strategy and covers everything we host today.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use lumen_host::{
    install, CallbackQueue, CompletionSender, Engine, TaskCompletion, TaskDecoder, TaskId,
    TaskRegistry, ThreadPool, Value,
};

mod console;
mod esm;
mod jsx;
mod process;
mod worker;

pub use console::{describe_error, render_value, ConsoleOut};
pub use lumen_host::{Completion, Ctx, RealmProcess, Spawner};

/// A realm that runs inside a host process instead of owning one (an editor running extensions on
/// a thread). Everything a Node program treats as process-wide comes from here, so the program
/// cannot reach the host's: `process.exit` ends the realm, `process.chdir` moves the realm's cwd,
/// `process.env` starts as `env`, stdio are the embedder's streams, and subprocesses start through
/// `spawner`. The engine polls `interrupt` at every call and loop turn (see
/// [`Engine::set_interrupt`](lumen_host::Engine::set_interrupt)).
pub struct Embedding {
    /// `process.argv`: `[argv0, script, ...args]`. `argv0` is also `execPath`.
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub stdin: Box<dyn Read + Send>,
    pub stdout: SharedWriter,
    pub stderr: SharedWriter,
    pub interrupt: Arc<AtomicBool>,
    /// Lower live-object ceiling for this realm; crossing it ends the realm with
    /// [`RealmExit::HeapLimit`].
    pub live_object_limit: Option<i64>,
    pub spawner: Option<Arc<dyn Spawner>>,
}

/// A writer several realms (a realm and its workers) can share: `console` and `process.stdout`
/// write whole chunks under the lock.
#[derive(Clone)]
pub struct SharedWriter(pub(crate) Arc<Mutex<Box<dyn Write + Send>>>);

impl SharedWriter {
    pub fn new(inner: impl Write + Send + 'static) -> SharedWriter {
        SharedWriter(Arc::new(Mutex::new(Box::new(inner))))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Box<dyn Write + Send>> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.lock().write(buf)
    }
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.lock().write_all(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.lock().flush()
    }
}

/// How an embedded realm ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealmExit {
    /// The program finished or called `process.exit(code)`; an uncaught error is `Exited(1)`.
    Exited(i32),
    /// The embedder set the interrupt.
    Terminated,
    /// The realm crossed its live-object ceiling.
    HeapLimit,
}

/// Stops an embedded realm from any thread: sets its interrupt and wakes its loop if it is
/// blocked waiting for I/O or a timer.
#[derive(Clone)]
pub struct Terminator {
    interrupt: Arc<AtomicBool>,
    wake: mpsc::Sender<TaskCompletion>,
}

impl Terminator {
    pub fn terminate(&self) {
        self.interrupt.store(true, Ordering::SeqCst);
        // No task has this id, so the loop wakes, finds nothing to settle, and sees the flag.
        let _ = self.wake.send(TaskCompletion {
            task: TaskId::MAX,
            result: Box::new(()),
        });
    }
}

/// What a worker inherits from an embedded parent realm.
#[derive(Clone)]
pub(crate) struct WorkerEmbedding {
    pub(crate) cwd: PathBuf,
    env: Vec<(String, String)>,
    stdout: SharedWriter,
    stderr: SharedWriter,
    interrupt: Arc<AtomicBool>,
    spawner: Option<Arc<dyn Spawner>>,
}

/// Workers for blocking work. libuv's default; revisit when async fs lands and has numbers.
const POOL_SIZE: usize = 4;

/// First scalar of the engine's lone-surrogate smuggle range (see `lumen::jstr`).
const SMUGGLE_BASE: u32 = 0x10F800;

/// Bring source text read from disk into the engine's string encoding. The engine stores a lone
/// surrogate as a plane-16 private-use scalar (U+10F800..=U+10FFFF) and a *real* character in that
/// range as the corresponding smuggled surrogate pair; text decoded from UTF-8 can hold such a real
/// character (e.g. a literal U+10FFFF in a string), which the parser would otherwise read as a
/// lone surrogate. Everything else passes through untouched.
pub fn import_source_text(s: String) -> String {
    // Every scalar >= U+10F800 encodes with a leading F4 8F byte pair; most text has none.
    if !s.as_bytes().contains(&0xF4) {
        return s;
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        let v = c as u32;
        if v >= SMUGGLE_BASE {
            let w = v - 0x10000;
            let (hi, lo) = (0xD800 + (w >> 10), 0xDC00 + (w & 0x3FF));
            out.push(char::from_u32(SMUGGLE_BASE + hi - 0xD800).expect("smuggle scalar"));
            out.push(char::from_u32(SMUGGLE_BASE + lo - 0xD800).expect("smuggle scalar"));
        } else {
            out.push(c);
        }
    }
    out
}

pub struct Runtime {
    engine: Engine,
    pool: ThreadPool,
    completions: mpsc::Receiver<TaskCompletion>,
    /// The error-reporting shims (see `Runtime::new`): `(error) -> suppressed` for the global
    /// `onerror` convention and `(promise, reason) -> suppressed` for `onunhandledrejection`.
    fire_error: Value,
    fire_rejection: Value,
    /// Set once an exception or rejection went unhandled: Node's fatal path. The loop stops,
    /// `finish_process` emits `'exit'` with this code (no `'beforeExit'`) and returns it.
    fatal_exit: Option<i32>,
    /// When the loop last collected because it was about to block, and the heap-object count it
    /// saw then (see `idle_collect`).
    idle_gc: (Instant, i64),
    idle_gc_followup: bool,
    /// Embedded realms only: the stop request (see [`Terminator`]).
    interrupt: Option<Arc<AtomicBool>>,
    /// Wakes the loop when it is blocked on completions.
    wake: mpsc::Sender<TaskCompletion>,
}

/// Why the entry script did not start cleanly.
enum StartError {
    /// The node glue is missing from this engine.
    NotInstalled(String),
    /// The script threw; the value is the thrown error.
    Thrown(Value),
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime {
    /// An engine with the runtime globals installed: timers, streaming `console`, minimal
    /// `process`, `queueMicrotask`.
    pub fn new() -> Runtime {
        Self::build(None)
    }

    /// A runtime for a realm inside a host process (see [`Embedding`]). Runs on the calling
    /// thread, which must have a large stack (the CLI uses 256 MiB; the engine recurses natively).
    pub fn new_embedded(embedding: Embedding) -> Runtime {
        Self::build(Some(embedding))
    }

    /// A worker's runtime: embedded like its parent when the parent is, else process-backed.
    pub(crate) fn new_worker(parent: Option<WorkerEmbedding>) -> Runtime {
        Self::build(parent.map(|p| Embedding {
            argv: Vec::new(),
            env: p.env,
            cwd: p.cwd,
            stdin: Box::new(std::io::empty()),
            stdout: p.stdout,
            stderr: p.stderr,
            interrupt: p.interrupt,
            live_object_limit: None,
            spawner: p.spawner,
        }))
    }

    fn build(embedding: Option<Embedding>) -> Runtime {
        let (tx, rx) = mpsc::channel();
        let pool = ThreadPool::new(POOL_SIZE, tx.clone());
        let mut engine = Engine::new();
        // Substrate first: fs's js_init runs during install and its ops need these.
        engine.ctx().op_state().put(pool.handle());
        // Dedicated-thread completions for unbounded-blocking work (child stdio) that must not
        // occupy a shared pool worker.
        engine.ctx().op_state().put(CompletionSender::new(tx.clone()));
        engine.ctx().op_state().put(TaskRegistry::default());
        // `#[op(async)]` / `Ctx::spawn_blocking` / `Ctx::completer` settle through the loop.
        engine.ctx().set_async_host(lumen_host::LoopAsyncHost {
            spawn: pool.handle(),
            completions: CompletionSender::new(tx.clone()),
        });
        // Before install: the extensions' js_init already asks for the cwd.
        let mut embedded_io = None;
        let mut interrupt = None;
        if let Some(mut e) = embedding {
            // `process.cwd()` never carries Windows' verbatim `\\?\` prefix; JS path code does
            // not expect it.
            e.cwd = lumen_host::strip_verbatim(e.cwd);
            engine.set_interrupt(Arc::clone(&e.interrupt));
            if let Some(limit) = e.live_object_limit {
                engine.set_live_object_limit(limit);
            }
            engine.ctx().op_state().put(RealmProcess {
                cwd: e.cwd.clone(),
                exit_code: None,
                interrupt: Arc::clone(&e.interrupt),
                stdin: Arc::new(Mutex::new(e.stdin)),
                stdout: Arc::clone(&e.stdout.0),
                stderr: Arc::clone(&e.stderr.0),
                spawner: e.spawner.clone(),
            });
            engine.ctx().op_state().put(WorkerEmbedding {
                cwd: e.cwd,
                env: e.env.clone(),
                stdout: e.stdout.clone(),
                stderr: e.stderr.clone(),
                interrupt: Arc::clone(&e.interrupt),
                spawner: e.spawner,
            });
            interrupt = Some(e.interrupt);
            embedded_io = Some((e.argv, e.env, e.stdout, e.stderr));
        }
        install(
            &mut engine,
            &[
                lumen_timers::extension(),
                console::extension(),
                process::extension(),
                lumen_fs::extension(),
                lumen_web::extension(),
                // Last: node's glue wraps the fs global, Buffer uses TextEncoder (web), and
                // require() calls process.cwd().
                lumen_node::extension(),
                worker::extension(),
            ],
        );
        let data = embedded_io
            .as_ref()
            .map(|(argv, env, _, _)| (argv.as_slice(), env.as_slice()));
        process::install_data_props(&mut engine, data);
        if let Some((_, _, stdout, stderr)) = embedded_io {
            engine.ctx().op_state().put(ConsoleOut {
                out: Box::new(stdout),
                err: Box::new(stderr),
            });
        }
        // queueMicrotask, via the promise queue the engine already has. Close enough to spec
        // for v0 (a thrown callback error becomes an unhandled rejection, not a reported
        // exception); a native microtask hook can replace it if that gap ever matters.
        engine
            .eval(
                "globalThis.queueMicrotask = (cb) => {
                    if (typeof cb !== 'function')
                        throw new TypeError('queueMicrotask expects a function');
                    Promise.resolve().then(cb);
                };",
                false,
            )
            .expect("shim parses");
        // The HTML error-reporting globals (WinterTC Minimum Common API §5.2): `onerror` /
        // `onunhandledrejection` global event-handler properties and `reportError`. The fire
        // helpers return whether the default report is suppressed (`onerror` returning `true`;
        // `unhandledrejection`'s `event.preventDefault()`); the loop's uncaught/rejection
        // reporting consults them through handles grabbed (and then unglobaled) below. A throw
        // inside a handler never re-enters it — the original error still default-reports.
        engine
            .eval(
                r#"
                globalThis.onerror = null;
                globalThis.onunhandledrejection = null;
                // Node's process-level hooks come first: a registered 'uncaughtException' /
                // 'unhandledRejection' listener owns the error, exactly as it does in Node.
                const processHas = (event) => {
                    const p = globalThis.process;
                    return !!(p && typeof p.listenerCount === 'function' && p.listenerCount(event) > 0);
                };
                globalThis.__lumen_fire_error = function (error, origin = 'uncaughtException') {
                    if (processHas('uncaughtException')) {
                        try {
                            globalThis.process.emit('uncaughtException', error, origin);
                            return true;
                        } catch {
                            return false;
                        }
                    }
                    const h = globalThis.onerror;
                    if (typeof h !== 'function') return false;
                    let message = '';
                    try {
                        message =
                            error instanceof Error
                                ? `Uncaught ${error.name}: ${error.message}`
                                : `Uncaught ${String(error)}`;
                    } catch {}
                    try {
                        return h.call(globalThis, message, '', 0, 0, error) === true;
                    } catch {
                        return false;
                    }
                };
                globalThis.__lumen_fire_rejection = function (promise, reason) {
                    if (processHas('unhandledRejection')) {
                        try {
                            globalThis.process.emit('unhandledRejection', reason, promise);
                            return true;
                        } catch {
                            return false;
                        }
                    }
                    const h = globalThis.onunhandledrejection;
                    if (typeof h !== 'function') return false;
                    let prevented = false;
                    const event = {
                        type: 'unhandledrejection',
                        promise,
                        reason,
                        cancelable: true,
                        preventDefault() { prevented = true; },
                        get defaultPrevented() { return prevented; },
                    };
                    try {
                        h.call(globalThis, event);
                    } catch {}
                    return prevented;
                };
                {
                    const fire = globalThis.__lumen_fire_error;
                    const report = console.__reportUncaught;
                    delete console.__reportUncaught;
                    globalThis.reportError = function reportError(e) {
                        if (arguments.length === 0)
                            throw new TypeError('reportError requires at least 1 argument');
                        if (!fire(e)) report(e);
                    };
                }
                "#,
                false,
            )
            .expect("error-reporting shim parses");
        let global = engine.global_this();
        let fire_error = engine
            .ctx()
            .get_member(&global, "__lumen_fire_error")
            .unwrap_or_else(|_| panic!("error-reporting shim installed"));
        let fire_rejection = engine
            .ctx()
            .get_member(&global, "__lumen_fire_rejection")
            .unwrap_or_else(|_| panic!("error-reporting shim installed"));
        engine
            .eval(
                "delete globalThis.__lumen_fire_error; delete globalThis.__lumen_fire_rejection;",
                false,
            )
            .expect("shim cleanup");
        Runtime {
            engine,
            pool,
            interrupt,
            wake: tx,
            completions: rx,
            fire_error,
            fire_rejection,
            fatal_exit: None,
            idle_gc: (Instant::now(), 0),
            idle_gc_followup: false,
        }
    }

    /// The engine, for embedder access beyond script evaluation (defining globals, etc.).
    pub fn engine(&mut self) -> &mut Engine {
        &mut self.engine
    }

    /// Run `path` as a CommonJS program entry (`require.main === module`, with `__dirname`/
    /// `__filename`/`require` in scope), then loop to quiescence. `Err` is the rendered
    /// uncaught error. This is what the CLI uses for `lumen-cli file.js`.
    pub fn run_main(&mut self, path: &str) -> Result<(), String> {
        self.run_main_from(path, None)
    }

    /// [`Self::run_main`] with `source` as the main module's text instead of `path`'s contents.
    /// `path` is still its `__filename`: relative `require`s and `__dirname` resolve against it,
    /// whether or not anything exists there.
    fn run_main_from(&mut self, path: &str, source: Option<&str>) -> Result<(), String> {
        match self.start_main_raw(path, source) {
            Ok(()) => {}
            Err(StartError::NotInstalled(message)) => return Err(message),
            // A throw out of the entry script is an uncaught exception like any other: a
            // `process.on('uncaughtException')` listener may own it, otherwise it is fatal.
            Err(StartError::Thrown(error)) => self.report_uncaught(&error),
        }
        self.run_to_completion();
        Ok(())
    }

    /// Start `path` as the CJS main module WITHOUT pumping the macrotask loop (only the top-level
    /// microtask checkpoint runs). Worker threads use this: the caller arms its message inbox
    /// first and then drives the loop itself. `Err` is the rendered uncaught error.
    pub(crate) fn start_main(&mut self, path: &str) -> Result<(), String> {
        self.start_main_raw(path, None).map_err(|e| match e {
            StartError::NotInstalled(message) => message,
            StartError::Thrown(error) => describe_error(self.engine.ctx(), &error),
        })
    }

    fn start_main_raw(&mut self, path: &str, source: Option<&str>) -> Result<(), StartError> {
        // A CJS script can still dynamic-`import()`: give the engine the ESM loader and resolve
        // bare relative specifiers against the entry file.
        let loader = esm::make_loader(self.builtin_modules());
        self.engine.set_module_loader_attrs(loader);
        match lumen_host::canonicalize(path) {
            Ok(abs) => self.engine.set_import_base(&abs.to_string_lossy()),
            Err(_) if source.is_some() => self.engine.set_import_base(path),
            Err(_) => {}
        }
        let global = self.engine.global_this();
        let entry = if source.is_some() { "__runMainSource" } else { "__runMain" };
        let run_main = self
            .engine
            .ctx()
            .get_member(&global, entry)
            .map_err(|_| StartError::NotInstalled("node runtime not installed".to_string()))?;
        let mut args = vec![Value::from_string(path.to_string())];
        if let Some(source) = source {
            args.push(Value::from_string(source.to_string()));
        }
        let result = self.engine.call_function(&run_main, Value::Undefined, &args);
        self.engine.run_microtasks();
        result.map(|_| ()).map_err(StartError::Thrown)
    }

    /// Run `path` as an ES module: its `import` graph resolves against disk + `node_modules`
    /// (and the `node:` builtins), then the loop runs to quiescence so top-level `await`,
    /// timers, and I/O settle. `Err` is the rendered uncaught error.
    pub fn run_module(&mut self, path: &str) -> Result<(), String> {
        let source =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
        let source = import_source_text(source);
        // A `.jsx` entry is lowered to plain JS before the engine parses it.
        let source = if path.ends_with(".jsx") {
            jsx::transform(&source).map_err(|e| format!("JSX transform failed for {path}: {e}"))?
        } else {
            source
        };
        let key = lumen_host::canonicalize(path)
            .unwrap_or_else(|_| std::path::PathBuf::from(path))
            .to_string_lossy()
            .into_owned();
        let loader = esm::make_loader(self.builtin_modules());
        let result = self.engine.eval_module_attrs(&source, &key, loader);
        match result {
            Ok(Completion::Value(_)) => {
                self.run_to_completion();
                Ok(())
            }
            // The module evaluation reports a rendered throw, not the value, so process-level
            // listeners cannot own it; it is fatal outright, as in Node: the loop does not run,
            // and the caller prints the error.
            Ok(Completion::Throw { name, message }) => {
                self.fatal_exit = Some(1);
                Err(if name.is_empty() {
                    message
                } else {
                    format!("{name}: {message}")
                })
            }
            Err(e) => Err(format!("SyntaxError: {} (line {})", e.message, e.line)),
        }
    }

    /// Run an ahead-of-time compiled program (a `lumen_aot::include_js!` blob): its scripts,
    /// then its entry module, then the loop to quiescence — [`Self::run_module`] with no JS
    /// source on disk. Imports between the blob's units (and bare packages it bundled with
    /// `node_modules = true`) resolve inside the blob; `require` loads its CommonJS units from
    /// it; `node:*` builtins come from the runtime, and anything else falls back to the usual
    /// disk resolution (bare packages from the current directory's `node_modules`). `Err` is the
    /// rendered uncaught error.
    pub fn run_precompiled(&mut self, blob: &lumen::Precompiled) -> Result<(), String> {
        let loader = esm::make_loader(self.builtin_modules());
        self.engine.set_module_loader_attrs(loader);
        match self.engine.load_precompiled(blob) {
            Ok(Completion::Value(_)) => {
                self.run_to_completion();
                Ok(())
            }
            Ok(Completion::Throw { name, message }) => {
                self.fatal_exit = Some(1);
                Err(if name.is_empty() {
                    message
                } else {
                    format!("{name}: {message}")
                })
            }
            Err(e) => Err(e.message),
        }
    }

    /// Evaluate a Worker entry `source` (a module when `is_module`, else a classic script) WITHOUT
    /// running the loop — the caller arms the message inbox first, then pumps the loop itself so
    /// the worker stays alive for messages. `base` seeds relative-import resolution. `Err` is the
    /// rendered load/parse/top-level error.
    pub fn eval_worker_entry(
        &mut self,
        source: &str,
        base: &str,
        is_module: bool,
    ) -> Result<(), String> {
        let loader = esm::make_loader(self.builtin_modules());
        self.engine.set_module_loader_attrs(loader);
        self.engine.set_import_base(base);
        let result = if is_module {
            let loader = esm::make_loader(self.builtin_modules());
            self.engine.eval_module_attrs(source, base, loader)
        } else {
            self.engine.eval(source, false)
        };
        // Drain the microtask checkpoint from top-level code, but not the macrotask loop.
        self.engine.run_microtasks();
        match result {
            Ok(Completion::Value(_)) => Ok(()),
            Ok(Completion::Throw { name, message }) => Err(if name.is_empty() {
                message
            } else {
                format!("{name}: {message}")
            }),
            Err(e) => Err(format!("SyntaxError: {} (line {})", e.message, e.line)),
        }
    }

    /// Like [`run_to_completion`](Runtime::run_to_completion), but for a Worker: it does NOT exit
    /// when idle (the armed message inbox keeps it alive), and it returns promptly when `stop` is
    /// set — a cooperative terminate that also drops any still-pending timers. The blocking wait
    /// polls `stop` on a short interval so a `terminate()` from another thread is noticed even
    /// with no message or timer due.
    pub fn run_worker_loop(&mut self, stop: &std::sync::atomic::AtomicBool) {
        let poll = Duration::from_millis(50);
        loop {
            if stop.load(Ordering::SeqCst) || self.interrupted() {
                return;
            }
            self.engine.run_microtasks();
            self.report_unhandled_rejections();
            loop {
                let mut progressed = false;
                for (cb, args) in self.take_queued_callbacks() {
                    progressed = true;
                    self.fire(&cb, &args);
                }
                let now = Instant::now();
                while let Some((cb, args)) = self.take_next_due_timer(now) {
                    progressed = true;
                    self.fire(&cb, &args);
                }
                while let Ok(done) = self.completions.try_recv() {
                    progressed = true;
                    self.dispatch(done);
                }
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                if !progressed {
                    break;
                }
            }
            if stop.load(Ordering::SeqCst) || self.idle() {
                return;
            }
            let wait = match self.next_timer_deadline() {
                Some(deadline) => {
                    let now = Instant::now();
                    if deadline <= now {
                        continue;
                    }
                    (deadline - now).min(poll)
                }
                None => poll,
            };
            if let Ok(done) = self.completions.recv_timeout(wait) {
                self.dispatch(done);
            }
        }
    }

    /// Pull the JS-precomputed synthetic ESM source for each `node:` builtin out of the engine
    /// (the loader can't enumerate a builtin's exports from Rust; see the node module glue).
    fn builtin_modules(&mut self) -> esm::BuiltinModules {
        let global = self.engine.global_this();
        let ctx = self.engine.ctx();
        let mut map = std::collections::HashMap::new();
        let names = ctx
            .get_member(&global, "__builtinNames")
            .ok()
            .and_then(|v| ctx.coerce_string(&v).ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let sources = ctx.get_member(&global, "__esmBuiltinSources").ok();
        if let Some(sources) = sources {
            for name in names.split(',').filter(|s| !s.is_empty()) {
                let key = format!("node:{name}");
                if let Ok(src) = ctx.get_member(&sources, &key) {
                    if let Ok(src) = ctx.coerce_string(&src) {
                        map.insert(key, src.to_string());
                    }
                }
            }
        }
        esm::BuiltinModules(map)
    }

    /// Evaluate a script, then run the event loop until quiescent — timers fired, spawned
    /// work completed, promise queue empty.
    pub fn eval(&mut self, src: &str) -> Result<Completion, lumen_host::ParseError> {
        let result = self.engine.eval(src, false);
        self.run_to_completion();
        result
    }

    /// Spawn blocking work on the pool; when it finishes, `decode` turns its payload into
    /// arguments and `callback` runs on the loop thread. This is the pattern async fs ops
    /// will use from inside native fns (via the `SpawnHandle`/`TaskRegistry` in `OpState`).
    pub fn spawn_blocking(
        &mut self,
        work: impl FnOnce() -> Box<dyn std::any::Any + Send> + Send + 'static,
        callback: Value,
        decode: TaskDecoder,
    ) {
        let registry = self
            .engine
            .ctx()
            .host_mut::<TaskRegistry>()
            .expect("installed in new()");
        let id = registry.register(callback, None, decode);
        self.pool.spawn_blocking(id, work);
    }

    /// Run the loop until nothing is pending: no microtasks, no queued callbacks, no live
    /// timers, no in-flight tasks.
    pub fn run_to_completion(&mut self) {
        loop {
            // Run everything already runnable. Each JS entry is followed by a microtask
            // checkpoint, matching the "after every macrotask" model.
            self.engine.run_microtasks();
            self.report_unhandled_rejections();
            loop {
                let mut progressed = false;
                for (cb, args) in self.take_queued_callbacks() {
                    if self.halted() {
                        return;
                    }
                    progressed = true;
                    self.fire(&cb, &args);
                }
                let now = Instant::now();
                while let Some((cb, args)) = self.take_next_due_timer(now) {
                    if self.halted() {
                        return;
                    }
                    progressed = true;
                    self.fire(&cb, &args);
                }
                while !self.halted() {
                    let Ok(done) = self.completions.try_recv() else {
                        break;
                    };
                    progressed = true;
                    self.dispatch(done);
                }
                if !progressed || self.halted() {
                    break;
                }
            }

            if self.halted() || self.idle() {
                return;
            }

            self.idle_collect();
            // Blocked: only a timer deadline or a task completion can make progress now.
            match self.next_timer_deadline() {
                Some(deadline) => {
                    let now = Instant::now();
                    if deadline > now {
                        if let Ok(done) = self.completions.recv_timeout(deadline - now) {
                            self.dispatch(done);
                        }
                    }
                }
                None => match self.completions.recv() {
                    Ok(done) => self.dispatch(done),
                    // The pool is gone (unreachable while `self.pool` lives); nothing can
                    // ever complete, so pending tasks are abandoned rather than spun on.
                    Err(_) => return,
                },
            }
        }
    }

    /// Collect while the loop is about to block. The engine's own trigger fires on allocation
    /// volume, so a program that loads a lot and then waits (a server, an editor host) never
    /// collects again and keeps every function body it ran once; the collector releases those
    /// bodies only across two collections it actually runs. Throttled so a loop that blocks
    /// between every request does not collect on every request.
    fn idle_collect(&mut self) {
        const MIN_INTERVAL: Duration = Duration::from_secs(1);
        const MIN_NEW_OBJECTS: i64 = 10_000;
        let (last, live_then) = self.idle_gc;
        if last.elapsed() < MIN_INTERVAL {
            return;
        }
        let ctx = self.engine.ctx();
        let live = ctx.live_object_count();
        // A collection releases the bodies that were cold across the *previous* one, so the
        // collection after a busy stretch is followed by one more even when nothing allocated.
        let busy = (live - live_then).abs() >= MIN_NEW_OBJECTS;
        if !busy && !self.idle_gc_followup {
            self.idle_gc.0 = Instant::now();
            return;
        }
        ctx.collect_garbage_for_host();
        ctx.release_unused_memory_for_host();
        self.idle_gc_followup = busy;
        self.idle_gc = (Instant::now(), ctx.live_object_count());
    }

    /// Set `process.argv` to `[argv0, ...script_argv]` and `process.execArgv` to the runtime
    /// flags, the way Node splits its command line. The runtime's default is the raw process
    /// arguments, which is right for an embedder but not for a CLI that takes flags.
    pub fn set_process_args(&mut self, argv0: &str, exec_argv: &[String], script_argv: &[String]) {
        let global = self.engine.global_this();
        let ctx = self.engine.ctx();
        let Ok(process) = ctx.get_member(&global, "process") else {
            return;
        };
        // Node reports argv[0] as the resolved execPath (argv0 keeps what was typed).
        let exec_path = match ctx.get_member(&process, "execPath") {
            Ok(Value::Str(s)) if !s.to_string().is_empty() => s.to_string(),
            _ => argv0.to_string(),
        };
        let argv: Vec<Value> = std::iter::once(exec_path)
            .chain(script_argv.iter().cloned())
            .map(Value::from_string)
            .collect();
        let argv = ctx.make_array(argv);
        let _ = ctx.set_member(&process, "argv", argv);
        let exec: Vec<Value> = exec_argv.iter().cloned().map(Value::from_string).collect();
        let exec = ctx.make_array(exec);
        let _ = ctx.set_member(&process, "execArgv", exec);
    }

    /// Node's `--expose-gc`: define `globalThis.gc`, which runs lumen's cycle collector.
    pub fn expose_gc(&mut self) {
        // `Bun.gc` is the JS-visible entry to the same collector; V8's `gc()` returns nothing.
        let _ = self.engine.eval(
            "globalThis.gc = function gc() { globalThis.Bun.gc(true); };",
            false,
        );
    }

    /// Node's end-of-program protocol, for a CLI that has run the loop to quiescence: emit
    /// `process` `'beforeExit'` (and keep running while a listener schedules more work), then
    /// `'exit'`, and return the code the process should exit with — `process.exitCode`, or 0.
    pub fn finish_process(&mut self) -> i32 {
        let global = self.engine.global_this();
        let Ok(process) = self.engine.ctx().get_member(&global, "process") else {
            return 0;
        };
        let Ok(emit) = self.engine.ctx().get_member(&process, "emit") else {
            return 0;
        };
        let code = |rt: &mut Runtime| -> i32 {
            match rt.engine.ctx().get_member(&process, "exitCode") {
                Ok(Value::Num(n)) => n as i32,
                Ok(Value::Str(s)) => s.to_string().parse().unwrap_or(0),
                _ => 0,
            }
        };
        // An uncaught exception ends the process without 'beforeExit': only 'exit' runs, with
        // the fatal code.
        if let Some(fatal) = self.fatal_exit {
            let _ = self.engine.call_function(
                &emit,
                process.clone(),
                &[Value::from_string("exit".to_string()), Value::Num(fatal as f64)],
            );
            self.engine.run_microtasks();
            return fatal;
        }
        // A 'beforeExit' listener may schedule more work; Node re-enters the loop until one
        // round adds nothing.
        for _ in 0..1000 {
            let c = code(self);
            let _ = self.engine.call_function(
                &emit,
                process.clone(),
                &[Value::from_string("beforeExit".to_string()), Value::Num(c as f64)],
            );
            self.engine.run_microtasks();
            if self.idle() {
                break;
            }
            self.run_to_completion();
        }
        let c = code(self);
        let _ = self.engine.call_function(
            &emit,
            process.clone(),
            &[Value::from_string("exit".to_string()), Value::Num(c as f64)],
        );
        self.engine.run_microtasks();
        code(self)
    }

    fn idle(&mut self) -> bool {
        if self.engine.has_pending_jobs() {
            return false;
        }
        let state = self.engine.ctx().op_state();
        let callbacks_queued = state
            .get::<CallbackQueue>()
            .is_some_and(|q| !q.queue.is_empty());
        let timers_pending = state
            .get::<lumen_timers::Timers>()
            .is_some_and(|t| t.has_pending());
        let tasks_pending = state
            .get::<TaskRegistry>()
            .is_some_and(|r| r.has_ref_pending());
        !callbacks_queued && !timers_pending && !tasks_pending
    }

    fn take_queued_callbacks(&mut self) -> Vec<(Value, Vec<Value>)> {
        match self.engine.ctx().host_mut::<CallbackQueue>() {
            Some(q) => std::mem::take(&mut q.queue).into(),
            None => Vec::new(),
        }
    }

    /// One due timer at a time (see `Timers::take_next_due`): each callback runs before the next
    /// is taken, so it can clear or refresh a timer due in the same turn. `now` is fixed for the
    /// turn, so an interval cannot keep itself due forever.
    fn take_next_due_timer(&mut self, now: Instant) -> Option<(Value, Vec<Value>)> {
        self.engine
            .ctx()
            .host_mut::<lumen_timers::Timers>()?
            .take_next_due(now)
    }

    fn next_timer_deadline(&mut self) -> Option<Instant> {
        self.engine
            .ctx()
            .host_mut::<lumen_timers::Timers>()?
            .next_deadline()
    }

    /// Settle one completed task: decode the payload, then run the success callback — or the
    /// failure one (a promise's reject) when the decoder says the work failed.
    fn dispatch(&mut self, done: TaskCompletion) {
        let entry = self
            .engine
            .ctx()
            .host_mut::<TaskRegistry>()
            .and_then(|r| r.take(done.task));
        let Some(entry) = entry else {
            return; // cancelled while in flight
        };
        // A host completion is a fresh turn of the loop: it runs in no async context. Ops that
        // settle a promise get the right context back through the reaction (captured at `then`
        // time), which is how `await __op()` inside an `AsyncLocalStorage.run` keeps its store.
        let outer = self.engine.ctx().set_async_context(Value::Undefined);
        match (entry.decode)(self.engine.ctx(), done.result) {
            Ok(args) => self.fire(&entry.on_ok, &args),
            Err(e) => match &entry.on_err {
                Some(reject) => self.fire(reject, std::slice::from_ref(&e)),
                None => self.report_uncaught(&e),
            },
        }
        self.engine.ctx().set_async_context(outer);
        self.engine.run_microtasks();
        self.report_unhandled_rejections();
    }

    /// One JS callback entry: call, report an uncaught throw, then the microtask checkpoint.
    fn fire(&mut self, callback: &Value, args: &[Value]) {
        if let Err(e) = self.engine.call_function(callback, Value::Undefined, args) {
            self.report_uncaught(&e);
        }
        self.engine.run_microtasks();
        self.report_unhandled_rejections();
    }

    /// An exception escaped the entry script or a loop-fired callback. A
    /// `process.on('uncaughtException')` listener (or the HTML `onerror` returning `true`) owns
    /// it and the loop continues; otherwise, as in Node, the error is printed and the process is
    /// done: the loop stops and the exit code is 1.
    fn report_uncaught(&mut self, error: &Value) {
        self.report_fatal(error, "Uncaught", "uncaughtException");
    }

    fn report_fatal(&mut self, error: &Value, prefix: &str, origin: &str) {
        // A terminating realm's unwinding error is the termination, not a program failure.
        if self.interrupted() {
            return;
        }
        let fire = self.fire_error.clone();
        if let Ok(Value::Bool(true)) = self.engine.call_function(
            &fire,
            Value::Undefined,
            &[error.clone(), Value::str(origin)],
        ) {
            return;
        }
        let text = console::describe_error(self.engine.ctx(), error);
        console::write_err_line(self.engine.ctx(), format!("{prefix} {text}"));
        self.fatal_exit.get_or_insert(1);
    }

    /// Report promises rejected without a handler. Called after each microtask checkpoint; a
    /// rejection handled in the same checkpoint won't appear. Node's default
    /// (`--unhandled-rejections=throw`): a `process.on('unhandledRejection')` listener owns it,
    /// otherwise it is raised as an uncaught exception — which an `'uncaughtException'` listener
    /// may still catch, and which is fatal if nothing does.
    fn report_unhandled_rejections(&mut self) {
        for (promise, reason) in self.engine.take_unhandled_rejections_full() {
            let fire = self.fire_rejection.clone();
            if let Ok(Value::Bool(true)) =
                self.engine
                    .call_function(&fire, Value::Undefined, &[promise, reason.clone()])
            {
                continue;
            }
            self.report_fatal(&reason, "Uncaught (in promise)", "unhandledRejection");
        }
    }

    /// The exit code an unhandled exception or rejection decided, if one did.
    pub fn fatal_exit_code(&self) -> Option<i32> {
        self.fatal_exit
    }

    fn interrupted(&self) -> bool {
        self.interrupt
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
    }

    /// The loop stops: a fatal error, or the embedder / `process.exit` asked the realm to end.
    fn halted(&self) -> bool {
        self.fatal_exit.is_some() || self.interrupted()
    }

    /// A handle that stops this realm from another thread. `None` unless embedded.
    pub fn terminator(&self) -> Option<Terminator> {
        Some(Terminator {
            interrupt: Arc::clone(self.interrupt.as_ref()?),
            wake: self.wake.clone(),
        })
    }

    /// Run an embedded realm's entry script to the end: `.mjs` as an ES module, anything else as
    /// the CommonJS main module (the Node programs this hosts are `.cjs`). Returns how it ended.
    pub fn run_embedded_main(&mut self, path: &str) -> RealmExit {
        let result = if path.ends_with(".mjs") {
            self.run_module(path)
        } else {
            self.run_main(path)
        };
        self.finish_embedded(result)
    }

    /// [`Self::run_embedded_main`] for a CommonJS program the embedder holds in memory (a bundle
    /// compiled into its binary, say). `path` is the program's `__filename`; nothing is read
    /// from it, and it need not exist.
    pub fn run_embedded_source(&mut self, path: &str, source: &str) -> RealmExit {
        let result = self.run_main_from(path, Some(source));
        self.finish_embedded(result)
    }

    fn finish_embedded(&mut self, result: Result<(), String>) -> RealmExit {
        if let Err(message) = result {
            if !self.interrupted() {
                console::write_err_line(self.engine.ctx(), format!("Uncaught {message}"));
                self.fatal_exit.get_or_insert(1);
            }
        }
        self.realm_exit()
    }

    fn realm_exit(&mut self) -> RealmExit {
        if self.engine.heap_limit_hit() {
            return RealmExit::HeapLimit;
        }
        let requested = self
            .engine
            .ctx()
            .op_state()
            .get::<RealmProcess>()
            .and_then(|realm| realm.exit_code);
        if let Some(code) = requested {
            return RealmExit::Exited(code);
        }
        if self.interrupted() {
            return RealmExit::Terminated;
        }
        RealmExit::Exited(self.finish_process())
    }
}

#[cfg(test)]
mod crypto_asym_tests;
#[cfg(test)]
mod tests;
