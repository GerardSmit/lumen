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
//! only completions remain). The loop exits when nothing is pending anywhere. A hand-rolled
//! readiness reactor on raw syscalls (epoll, kqueue, poll; no crate, no mio) now exists in
//! `lumen_os::reactor`; this loop does not use it yet, and threadpool + completions (libuv's own fs
//! strategy) still covers everything hosted here today.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use lumen_host::time::Instant;

use lumen_host::{
    install, owner_loop, CallbackQueue, CompletionSender, CompletionTx, Engine, Extension, HostRealmInstaller,
    TaskCompletion, TaskDecoder, TaskId, TaskRegistry, ThreadPool, Value,
};

mod child_realm;
mod console;
mod esm;
#[cfg(all(feature = "parallel", not(target_os = "none")))]
mod parallel;
mod process;
mod process_env;
pub mod tsconfig;
#[cfg(not(target_arch = "wasm32"))]
mod worker;
#[cfg(target_arch = "wasm32")]
#[path = "worker_browser.rs"]
mod worker;

#[cfg(not(target_arch = "wasm32"))]
pub use esm::fetch_network_module_resource;
pub use esm::{
    resolve_network_module_request, PrefetchedModuleResource, PrefetchedModuleResources,
};

/// js/error_shim.js, precompiled by build.rs.
const ERROR_SHIM_AOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/error_shim.aot"));

/// Run a script precompiled by build.rs, in the tree-walker like extension glue (its setup path
/// runs once, so compiling it would be wasted).
pub(crate) fn run_aot(
    engine: &mut Engine,
    blob: &'static [u8],
) -> Result<lumen::Completion, lumen::ParseError> {
    let tier = engine.tier();
    engine.set_tier(lumen_host::Tier::Interp);
    let result = lumen_host::load_glue(engine, blob);
    engine.set_tier(tier);
    result
}

fn install_queue_microtask(ctx: &mut Ctx) {
    let Value::Obj(global) = ctx.global_object() else {
        return;
    };
    ctx.def_method(
        &global,
        "queueMicrotask",
        1,
        |interp, _this, args| match args.first() {
            Some(callback) if callback.is_callable() => {
                interp.queue_microtask(callback.clone());
                Ok(Value::Undefined)
            }
            _ => Err(interp.make_error("TypeError", "queueMicrotask expects a function")),
        },
    );
}

pub use console::{describe_error, render_value, ConsoleOut};
pub use lumen_host::{Completion, Ctx, RealmProcess, Spawner};

/// The package's own module type, excluding identically named nested metadata fields.
pub fn package_type_from_json(source: &str) -> Option<String> {
    match tsconfig::parse_jsonc(source).ok()?.get("type")? {
        tsconfig::Json::Str(value) => Some(value.clone()),
        _ => None,
    }
}

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
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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

/// Stops a realm from any thread: sets its interrupt, which running JS and the native waits
/// that poll it (`Atomics.wait`, `spawnSync`, a synchronous stdin read) notice, and wakes the
/// event loop if it is blocked waiting for I/O, a timer or a worker. The loop then unwinds with
/// the same termination an interrupt raised inside JS gives, stops the realm's workers and
/// closes its sockets and listeners. Interrupting is permanent for the realm.
pub use lumen::limits::InterruptHandle;
use lumen::limits::InterruptSubscription;

/// The wake-up that carries a signal for the realm's own `process.on` listeners: its payload is
/// the signal number.
const SIGNAL_TASK: TaskId = TaskId::MAX - 1;

/// A completion that carries nothing: it only ends a blocking wait so the embedder re-checks
/// its own channels.
const WAKE_TASK: TaskId = TaskId::MAX - 2;

/// Wakes a runtime blocked in [`Runtime::wait_for_completion`] from any thread, for work whose
/// result travels over a channel the embedder owns.
#[derive(Clone)]
pub struct RuntimeWaker {
    wake: CompletionTx,
}

impl RuntimeWaker {
    pub fn wake(&self) {
        let _ = self.wake.send(TaskCompletion {
            task: WAKE_TASK,
            result: Box::new(()),
        });
    }
}

/// An embedded realm's stop request (its [`InterruptHandle`]), which can also deliver a signal
/// to the realm's own `process.on` listeners.
#[derive(Clone)]
pub struct Terminator {
    handle: InterruptHandle,
    wake: CompletionTx,
}

impl Terminator {
    /// Queue `signal` for the realm's loop, which emits it to the program's listeners.
    pub(crate) fn deliver(&self, signal: i32) {
        let _ = self.wake.send(TaskCompletion {
            task: SIGNAL_TASK,
            result: Box::new(signal),
        });
    }

    pub fn terminate(&self) {
        self.handle.interrupt();
    }

    pub fn is_interrupted(&self) -> bool {
        self.handle.is_interrupted()
    }

    pub fn handle(&self) -> &InterruptHandle {
        &self.handle
    }
}

/// What a worker inherits from an embedded parent realm.
#[derive(Clone)]
pub(crate) struct WorkerEmbedding {
    exec_path: String,
    pub(crate) cwd: PathBuf,
    env: Vec<(String, String)>,
    stdout: SharedWriter,
    stderr: SharedWriter,
    interrupt: InterruptHandle,
    spawner: Option<Arc<dyn Spawner>>,
    owned_fds: Vec<i32>,
    tree: Arc<child_realm::RealmTree>,
}

/// Bring source text read from disk into the engine's string encoding. The engine stores a lone
/// surrogate as a plane-16 private-use scalar (U+10F800..=U+10FFFF) and a *real* character in that
/// range as the corresponding smuggled surrogate pair; text decoded from UTF-8 can hold such a real
/// character (e.g. a literal U+10FFFF in a string), which the parser would otherwise read as a
/// lone surrogate. Everything else passes through untouched.
pub fn import_source_text(s: String) -> String {
    lumen_common::smuggle::utf16_text_owned(s)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RootProviders {
    Node,
    Browser,
}

fn browser_extensions() -> Vec<Extension> {
    vec![
        lumen_timers::extension(),
        console::extension(),
        lumen_web::extension(),
        lumen_webstreams::extension(),
        lumen_host::clone_transfer::extension(),
        lumen_host::ports::extension(),
    ]
}

pub struct Runtime {
    engine: Engine,
    root_providers: RootProviders,
    pool: ThreadPool,
    completions: mpsc::Receiver<TaskCompletion>,
    /// The error-reporting shims (see `Runtime::new`): `(error) -> suppressed` for the global
    /// `onerror` convention and `(promise, reason) -> suppressed` for `onunhandledrejection`.
    fire_error: Value,
    fire_rejection: Value,
    fire_handled: Value,
    /// Browser embedders collect these and dispatch real Window tasks. Node keeps its fatal
    /// rejection policy unless this is explicitly enabled.
    browser_rejections: lumen_html_js::BrowserRejectionPolicy,
    /// Worker globals admit rejection notifications as tasks of their own loop.
    worker_rejection_tasks: Rc<RefCell<VecDeque<lumen_html_js::BrowserRejectionTask>>>,
    /// Set once an exception or rejection went unhandled: Node's fatal path. The loop stops,
    /// `finish_process` emits `'exit'` with this code (no `'beforeExit'`) and returns it.
    fatal_exit: Option<i32>,
    #[cfg(feature = "aot-native")]
    native_builtins_installed: bool,
    /// When the loop last collected because it was about to block, and the heap-object count it
    /// saw then (see `idle_collect`).
    idle_gc: (Instant, i64),
    idle_gc_followup: bool,
    /// Embedded realms only: the stop request (see [`Terminator`]).
    interrupt: Option<InterruptHandle>,
    /// Wakes this loop when `interrupt` is raised through its handle.
    interrupt_wake: Option<InterruptSubscription>,
    /// Blocking waits the worker loop returned from; tests assert an idle worker has none.
    #[cfg(test)]
    pub(crate) loop_wakeups: Arc<AtomicU64>,
    /// Embedded realms only: the child realms this one started (see `child_realm`).
    child_realms: Option<Arc<child_realm::ChildRealms>>,
    /// A signal's default action stopped the realm (see `deliver_signal`).
    signal_exit: Option<i32>,
    /// A tick threw and its error was handled: the queue behind it drains before the next callback.
    tick_recovery: bool,
    /// Wakes the loop when it is blocked on completions.
    wake: CompletionTx,
    #[cfg(not(target_arch = "wasm32"))]
    deadline: Option<lumen_os::sched::Deadline>,
}

/// Where the loop stands after [`Runtime::run_until_idle`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoopStatus {
    /// Milliseconds until the earliest pending timer is due (0 when already due).
    pub next_timer_ms: Option<f64>,
    /// A registered task is still waiting for its completion.
    pub pending_tasks: bool,
    /// Nothing is pending anywhere: the program has finished.
    pub idle: bool,
    /// `process.exit` or a termination request ended the realm.
    pub halted: bool,
}

pub use lumen_html_js::{BrowserRejectionBatch, BrowserRejectionDelivery, BrowserRejectionEvent};

/// Why the entry script did not start cleanly.
enum StartError {
    /// The node glue is missing from this engine.
    NotInstalled(String),
    /// The script threw; the value is the thrown error.
    Thrown(Value),
}

impl Drop for Runtime {
    fn drop(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        self.deadline.take();
        // Nothing reads a child's output once its launcher is going away: wake any child blocked
        // on a full pipe before waiting for it.
        lumen_node::close_child_pipes(self.engine.ctx());
        if let Some(children) = &self.child_realms {
            children.shutdown();
        }
        lumen_node::shutdown_native_addons(self.engine.ctx());
        self.release_blocked_io();
        self.engine.ctx().set_native_rejection_handled_hook(None);
        // Engine is the first field and performs the last collection when it
        // drops. Release our independent JS roots first: leaving them alive
        // during that collection can preserve cycles after their later drop.
        self.fire_error = Value::Undefined;
        self.fire_rejection = Value::Undefined;
        self.fire_handled = Value::Undefined;
        // Release payloads outside the RefCell borrows; native owners captured
        // by host sinks can themselves run teardown while being destroyed.
        self.browser_rejections.release(&mut self.engine);
        let worker_tasks = std::mem::take(&mut *self.worker_rejection_tasks.borrow_mut());
        drop(worker_tasks);
    }
}

impl Runtime {
    /// Freeze the host namespace identity before a target query or realm creation.
    pub fn freeze_native_catalog() {
        #[cfg(feature = "aot-native")]
        {
            static CATALOG: std::sync::Once = std::sync::Once::new();
            CATALOG.call_once(|| {
                let hash = lumen_common::aot::builtin_catalog::hash(
                    lumen_common::aot::builtin_catalog::Features {
                        node: true,
                        parallel: cfg!(feature = "parallel"),
                        http2: cfg!(feature = "http2"),
                        cluster: cfg!(feature = "cluster"),
                        dgram: cfg!(feature = "dgram"),
                        wasi: cfg!(feature = "wasi"),
                        bitnest_process: false,
                    },
                );
                lumen::target::set_builtin_modules_hash(hash)
                    .unwrap_or_else(|error| panic!("native runtime catalog: {error}"));
            });
        }
    }
    /// Install one standalone app's read-only filesystem assets at `/lumen-assets`.
    pub fn install_embedded_assets(&mut self, blob: &[u8]) -> Result<(), String> {
        lumen_os::vfs::install_assets(blob).map_err(String::from)
    }
    /// End the process with `code` without tearing the realm down (freeing every object and
    /// collecting its cycles is wasted work when the OS reclaims the address space). A clean
    /// exit still runs what the drop would: addon cleanup hooks and closing SQLite databases.
    /// Standard output and error are flushed by `process::exit`.
    pub fn exit(&mut self, code: i32) -> ! {
        if code == 0 {
            lumen_node::shutdown_native_resources(self.engine.ctx());
        }
        std::process::exit(code)
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

/// A wake for a loop blocked on completions, which no task id matches, so the loop finds nothing
/// to settle and sees the interrupt flag.
fn loop_wake(tx: &CompletionTx) -> Arc<dyn Fn() + Send + Sync> {
    let tx = tx.clone();
    Arc::new(move || {
        let _ = tx.send(TaskCompletion {
            task: TaskId::MAX,
            result: Box::new(()),
        });
    })
}

impl Runtime {
    /// An engine with the runtime globals installed: timers, streaming `console`, minimal
    /// `process`, `queueMicrotask`.
    pub fn new() -> Runtime {
        let mut rt = Self::build(None, None, RootProviders::Node);
        process_env::own_time_zone(rt.engine().ctx());
        rt
    }

    /// Browser globals with the same native async services as child browser realms.
    /// Node/process providers are excluded. Window rejection task admission remains
    /// configured by the browser embedder through its existing document sink API.
    pub fn new_browser() -> Runtime {
        let mut rt = Self::build(None, None, RootProviders::Browser);
        process_env::own_time_zone(rt.engine().ctx());
        rt
    }

    /// A runtime for a realm inside a host process (see [`Embedding`]). Runs on the calling
    /// thread, which must have a large stack (the CLI uses 256 MiB; the engine recurses natively).
    pub fn new_embedded(embedding: Embedding) -> Runtime {
        Self::build(Some(embedding), None, RootProviders::Node)
    }

    /// A child realm's runtime: embedded, and counted in its launcher's realm tree.
    pub(crate) fn new_child(embedding: Embedding, tree: Arc<child_realm::RealmTree>) -> Runtime {
        Self::build(Some(embedding), Some(tree), RootProviders::Node)
    }

    /// A worker's runtime: embedded like its parent when the parent is, else process-backed.
    pub(crate) fn new_worker(parent: Option<WorkerEmbedding>) -> Runtime {
        Self::new_worker_with_providers(parent, RootProviders::Node)
    }

    /// A web worker's provider profile; the worker launcher selects this only
    /// for a genuine web worker, retaining Node workers' existing construction.
    pub(crate) fn new_browser_worker(parent: Option<WorkerEmbedding>) -> Runtime {
        Self::new_worker_with_providers(parent, RootProviders::Browser)
    }

    fn new_worker_with_providers(parent: Option<WorkerEmbedding>, providers: RootProviders) -> Runtime {
        let owned_fds = parent
            .as_ref()
            .map(|p| p.owned_fds.clone())
            .unwrap_or_default();
        let tree = parent.as_ref().map(|p| Arc::clone(&p.tree));
        let parent_interrupt = parent.as_ref().map(|p| p.interrupt.clone());
        let mut runtime = Self::build(
            parent.map(|p| Embedding {
                argv: vec![p.exec_path],
                env: p.env,
                cwd: p.cwd,
                stdin: Box::new(std::io::empty()),
                stdout: p.stdout,
                stderr: p.stderr,
                interrupt: Arc::clone(p.interrupt.flag()),
                live_object_limit: None,
                spawner: p.spawner,
            }),
            tree,
            providers,
        );
        if let Some(handle) = parent_interrupt {
            runtime.share_interrupt(handle);
        }
        // A worker shares its realm's descriptors: closing one there must not close the host's.
        if let Some(realm) = runtime.engine.ctx().host_mut::<RealmProcess>() {
            realm.owned_fds = owned_fds;
        }
        runtime
    }

    fn build(
        embedding: Option<Embedding>,
        tree: Option<Arc<child_realm::RealmTree>>,
        providers: RootProviders,
    ) -> Runtime {
        Self::freeze_native_catalog();
        let boot = lumen_host::startup_timing().then(Instant::now);
        lumen_host::perf::start_clock();
        lumen_host::perf::mark(lumen_host::perf::Milestone::NodeStart);
        let (tx, rx) = mpsc::channel();
        let tx = CompletionTx::new(tx);
        let pool = ThreadPool::new(tx.clone());
        lumen::set_tail_calls(false);
        let mut engine = Engine::new();
        engine.set_jsx_options_loader(|filename, defaults| {
            tsconfig::load_jsx_for(std::path::Path::new(filename), defaults)
        });
        lumen_host::perf::mark(lumen_host::perf::Milestone::V8Start);
        // Substrate first: fs's js_init runs during install and its ops need these.
        engine.ctx().op_state().put(pool.handle());
        // Dedicated-thread completions for unbounded-blocking work (child stdio) that must not
        // occupy a shared pool worker.
        engine
            .ctx()
            .op_state()
            .put(CompletionSender::new(tx.clone()));
        engine.ctx().op_state().put(TaskRegistry::default());
        // `#[op(async)]` / `Ctx::spawn_blocking` / `Ctx::completer` settle through the loop.
        engine.ctx().set_async_host(lumen_host::LoopAsyncHost {
            spawn: pool.handle(),
            completions: CompletionSender::new(tx.clone()),
        });
        // Before install: the extensions' js_init already asks for the cwd.
        let mut embedded_io = None;
        let mut interrupt = None;
        let mut child_realms = None;
        if let Some(mut e) = embedding {
            // `process.cwd()` never carries Windows' verbatim `\\?\` prefix; JS path code does
            // not expect it.
            e.cwd = lumen_host::strip_verbatim(e.cwd);
            engine.set_interrupt(Arc::clone(&e.interrupt));
            if let Some(limit) = e.live_object_limit {
                engine.set_live_object_limit(limit);
            }
            let exec_path = e
                .argv
                .first()
                .cloned()
                .unwrap_or_else(|| "lumen".to_string());
            let tree = tree.unwrap_or_default();
            let launcher = Arc::new(child_realm::ChildRealms::new(
                e.spawner.clone(),
                e.live_object_limit,
                Arc::clone(&tree),
            ));
            child_realms = Some(Arc::clone(&launcher));
            engine.ctx().op_state().put(RealmProcess {
                exec_path: exec_path.clone(),
                launcher: Some(launcher),
                owned_fds: Vec::new(),
                signal_handlers: Arc::new(AtomicU64::new(0)),
                cwd: e.cwd.clone(),
                exit_code: None,
                interrupt: Arc::clone(&e.interrupt),
                stdin: Arc::new(Mutex::new(e.stdin)),
                stdout: Arc::clone(&e.stdout.0),
                stderr: Arc::clone(&e.stderr.0),
                spawner: e.spawner.clone(),
            });
            let handle = InterruptHandle::from_flag(Arc::clone(&e.interrupt));
            engine.ctx().op_state().put(WorkerEmbedding {
                exec_path,
                cwd: e.cwd,
                env: e.env.clone(),
                stdout: e.stdout.clone(),
                stderr: e.stderr.clone(),
                interrupt: handle.clone(),
                spawner: e.spawner,
                owned_fds: Vec::new(),
                tree,
            });
            interrupt = Some(handle);
            embedded_io = Some((e.argv, e.env, e.stdout, e.stderr));
        }
        // queueMicrotask, on the engine's job queue. A thrown callback error becomes an
        // unhandled rejection, not a reported exception.
        install_queue_microtask(engine.ctx());
        if providers == RootProviders::Node {
            install(
                &mut engine,
                &[
                    lumen_timers::extension(),
                    console::extension(),
                    process::extension(),
                    process_env::extension(),
                    lumen_web::extension(),
                    // Last: Buffer uses TextEncoder (web), and require() calls process.cwd().
                    lumen_node::extension(),
                    lumen_host::clone_transfer::extension(),
                    lumen_host::ports::extension(),
                    worker::extension(),
                ],
            );
        } else {
            install(&mut engine, &browser_extensions());
            // Preserve the existing genuine root Worker/SharedWorker service.
            install(&mut engine, &[worker::extension()]);
        }
        let browser_extensions: Rc<[Extension]> = browser_extensions().into();
        let browser_installer: HostRealmInstaller = Rc::new(move |ctx, realm| {
            lumen_host::install_realm_in_ctx(ctx, realm, &browser_extensions)?;
            ctx.with_host_realm(realm, install_queue_microtask)
                .map_err(|error| error.to_string())
        });
        lumen_host::register_host_realm_installer(engine.ctx(), browser_installer);
        lumen_host::perf::mark(lumen_host::perf::Milestone::Environment);
        let data = embedded_io
            .as_ref()
            .map(|(argv, env, _, _)| (argv.as_slice(), env.as_slice()));
        if providers == RootProviders::Node {
            process::install_data_props(&mut engine, data);
        }
        if let Some((_, _, stdout, stderr)) = embedded_io {
            engine.ctx().op_state().put(ConsoleOut {
                out: Box::new(stdout),
                err: Box::new(stderr),
            });
        }
        // The HTML error-reporting globals (WinterTC Minimum Common API §5.2): `onerror` /
        // `onunhandledrejection` global event-handler properties and `reportError`. The fire
        // helpers return whether the default report is suppressed (`onerror` returning `true`;
        // `unhandledrejection`'s `event.preventDefault()`); the loop's uncaught/rejection
        // reporting consults them through handles grabbed (and then unglobaled) below. A throw
        // inside a handler never re-enters it — the original error still default-reports.
        run_aot(&mut engine, ERROR_SHIM_AOT).expect("error-reporting shim loads");
        let global = engine.global_this();
        let fire_error = engine
            .ctx()
            .get_member(&global, "__lumen_fire_error")
            .unwrap_or_else(|_| panic!("error-reporting shim installed"));
        let fire_handled = engine
            .ctx()
            .get_member(&global, "__lumen_fire_handled")
            .unwrap_or(Value::Undefined);
        engine.track_late_handled_rejections();
        engine.report_task_errors();
        let fire_rejection = engine
            .ctx()
            .get_member(&global, "__lumen_fire_rejection")
            .unwrap_or_else(|_| panic!("error-reporting shim installed"));
        let reflect = engine
            .ctx()
            .get_member(&global, "Reflect")
            .unwrap_or_else(|_| panic!("Reflect installed"));
        let delete = engine
            .ctx()
            .get_member(&reflect, "deleteProperty")
            .unwrap_or_else(|_| panic!("Reflect.deleteProperty installed"));
        for name in [
            "__lumen_fire_error",
            "__lumen_fire_rejection",
            "__lumen_fire_handled",
        ] {
            engine
                .call_function(
                    &delete,
                    reflect.clone(),
                    &[global.clone(), Value::str(name)],
                )
                .unwrap_or_else(|_| panic!("error shim globals configurable"));
        }
        if let Some(t0) = boot {
            eprintln!("[startup] runtime built    {:?}", t0.elapsed());
        }
        lumen::memstats::phase("runtime built");
        lumen_host::perf::mark(lumen_host::perf::Milestone::BootstrapComplete);
        #[cfg(all(feature = "parallel", not(target_os = "none")))]
        parallel::install(&mut engine);
        let interrupt_wake = interrupt
            .as_ref()
            .map(|handle| handle.subscribe(loop_wake(&tx)));
        Runtime {
            engine,
            root_providers: providers,
            pool,
            interrupt,
            interrupt_wake,
            #[cfg(test)]
            loop_wakeups: Arc::default(),
            child_realms,
            signal_exit: None,
            tick_recovery: false,
            wake: tx,
            completions: rx,
            fire_error,
            fire_rejection,
            fire_handled,
            browser_rejections: lumen_html_js::BrowserRejectionPolicy::default(),
            worker_rejection_tasks: Rc::new(RefCell::new(VecDeque::new())),
            fatal_exit: None,
            #[cfg(feature = "aot-native")]
            native_builtins_installed: false,
            idle_gc: (Instant::now(), 0),
            idle_gc_followup: false,
            #[cfg(not(target_arch = "wasm32"))]
            deadline: None,
        }
    }

    /// The engine, for embedder access beyond script evaluation (defining globals, etc.).
    pub fn engine(&mut self) -> &mut Engine {
        &mut self.engine
    }

    /// Install this runtime's browser-side host providers in another realm owned by its engine.
    /// Native extension state and queues remain shared, while each realm gets its own globals,
    /// namespaces, and wrapper functions. Node and process providers are intentionally excluded.
    pub fn install_browser_realm(
        &mut self,
        realm: &lumen::embed::RealmHandle,
    ) -> Result<(), String> {
        lumen_host::install_registered_host_realm(self.engine.ctx(), realm)
    }

    /// Cancel timers owned by a browser realm that is being navigated or discarded.
    ///
    /// The timer heap remains shared by the runtime; this removes only entries created while
    /// `realm` was the active timer-global realm and drops their callback values immediately.
    /// It deliberately does not cancel other async tasks or retained JavaScript callbacks.
    pub fn cancel_timers_for_realm(&mut self, realm: &lumen::embed::RealmHandle) -> usize {
        self.engine
            .ctx()
            .host_mut::<lumen_timers::Timers>()
            .map(|timers| timers.cancel_realm(realm))
            .unwrap_or(0)
    }

    /// Cancel pending async settlements admitted by a browser realm being navigated or
    /// discarded. This releases their JavaScript callbacks immediately; underlying native I/O
    /// may still finish, but its completion will be ignored by the shared task registry.
    pub fn cancel_tasks_for_realm(&mut self, realm: &lumen::embed::RealmHandle) -> usize {
        self.engine
            .ctx()
            .host_mut::<TaskRegistry>()
            .map(|tasks| tasks.cancel_realm(realm))
            .unwrap_or(0)
    }

    /// Collect browser-style rejection notifications instead of applying Node's fatal rejection
    /// policy. Notifications are returned by [`Self::take_browser_rejection_events`] after a
    /// normal checkpoint so the embedder can enqueue actual `unhandledrejection` and
    /// `rejectionhandled` events on its user-agent task queue.
    pub fn enable_browser_rejection_events(&mut self) {
        self.browser_rejections.enable(&mut self.engine);
    }

    /// Register native user-agent task admission for one live browser document.
    /// The sink should retain its document weakly; no Runtime/Engine is captured.
    pub fn set_browser_rejection_sink(
        &mut self, owner: &lumen::embed::RealmHandle,
        sink: impl Fn(&mut Ctx, Vec<BrowserRejectionEvent>, BrowserRejectionDelivery) -> Vec<Value> + 'static,
    ) {
        self.browser_rejections.set_sink(&mut self.engine, owner, Rc::new(sink));
    }

    /// Register a Window document as the admission target of its creating realm.
    pub fn register_browser_rejection_document(
        &mut self, realm: &Rc<lumen_html_js::DomRealm>,
    ) -> Result<(), String> {
        lumen_html_js::register_document_rejection_sink(&mut self.browser_rejections, &mut self.engine, realm)
    }

    /// Admit the document realm's pending rejection notifications as Window tasks.
    pub fn queue_browser_rejection_document_events(
        &mut self, realm: &Rc<lumen_html_js::DomRealm>,
    ) -> Result<(), String> {
        lumen_html_js::queue_document_rejection_events(&mut self.browser_rejections, &mut self.engine, realm)
    }

    /// Deliver rejection events to a dedicated or shared worker global as tasks of its own loop.
    pub(crate) fn enable_worker_rejection_events(&mut self) {
        let owner = self.engine.ctx().current_host_realm();
        let tasks = self.worker_rejection_tasks.clone();
        self.set_browser_rejection_sink(&owner, move |_, events, _| {
            tasks.borrow_mut().extend(lumen_html_js::group_rejection_tasks(events));
            Vec::new()
        });
    }

    fn take_worker_rejection_tasks(&mut self) -> Vec<lumen_html_js::BrowserRejectionTask> {
        std::mem::take(&mut *self.worker_rejection_tasks.borrow_mut()).into()
    }

    fn run_worker_rejection_task(&mut self, task: lumen_html_js::BrowserRejectionTask) {
        let delivery = self.browser_rejections.delivery();
        let result: Result<(), lumen::embed::OpError> = lumen_html_js::run_rejection_task(self.engine.ctx(), &delivery, task, |ctx, kind, promise, reason| {
            lumen_host::workers::dispatch_rejection(ctx, kind, promise, reason)
        });
        if let Err(error) = result {
            let error = error.to_value(self.engine.ctx());
            self.report_uncaught(&error);
        }
        self.checkpoint();
        self.report_unhandled_rejections();
    }

    /// Clone the shared delivery handle for tasks that execute after this runtime borrow ends.
    pub fn browser_rejection_delivery(&self) -> BrowserRejectionDelivery {
        self.browser_rejections.delivery()
    }

    /// Take browser rejection notifications accumulated at the latest checkpoints.
    pub fn take_browser_rejection_events(&mut self) -> Vec<BrowserRejectionEvent> {
        self.browser_rejections.take_events()
    }

    /// Take only notifications belonging to one document realm, leaving other realms' events
    /// queued for their own host task pumps.
    pub fn take_browser_rejection_events_for_realm(
        &mut self,
        realm: &lumen::embed::RealmHandle,
    ) -> Vec<BrowserRejectionEvent> {
        self.browser_rejections.take_events_for_realm(&mut self.engine, realm)
    }

    /// Cancel pending rejection notifications and tracking for a document realm being retired.
    /// A weak global marker also suppresses promises that reject after this call.
    pub fn cancel_browser_rejection_events_for_realm(
        &mut self,
        realm: &lumen::embed::RealmHandle,
    ) -> usize {
        self.browser_rejections.cancel_for_realm(&mut self.engine, realm)
    }

    /// Evaluate a module using this runtime's configured loader while leaving pending top-level
    /// await work on the normal event loop. The returned handle is polled through
    /// `Engine::module_evaluation_status`.
    pub fn eval_module_pending(
        &mut self,
        source: &str,
        record_key: &str,
        module_url: &str,
    ) -> Result<lumen::ModuleEvaluationHandle, lumen::ParseError> {
        let (loader, cache) = self.make_cached_module_loader();
        let result = self
            .engine
            .eval_module_attrs_pending(source, record_key, module_url, loader);
        // The static graph is synchronously registered before the evaluation handle is returned.
        cache.forget_sources();
        result
    }

    /// Evaluate a browser-prepared module graph. The graph cache is consulted by the ordinary ESM
    /// resolver, which still applies redirects, same-origin, MIME, and import-attribute rules.
    /// Entries absent from the snapshot use this runtime's normal configured native loader.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn eval_module_pending_with_prefetched_resources(
        &mut self,
        source: &str,
        record_key: &str,
        module_url: &str,
        prefetched: PrefetchedModuleResources,
    ) -> Result<lumen::ModuleEvaluationHandle, lumen::ParseError> {
        self.eval_module_pending_with_prefetched_resources_and_base(
            source, record_key, module_url, module_url, prefetched,
        )
    }

    /// Variant of [`Self::eval_module_pending_with_prefetched_resources`] for inline HTML modules
    /// whose import-resolution base differs from their observable module URL.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn eval_module_pending_with_prefetched_resources_and_base(
        &mut self,
        source: &str,
        record_key: &str,
        module_url: &str,
        resolution_url: &str,
        prefetched: PrefetchedModuleResources,
    ) -> Result<lumen::ModuleEvaluationHandle, lumen::ParseError> {
        let builtins = self.builtin_modules();
        let fetch_config = self
            .engine
            .ctx()
            .op_state()
            .get::<lumen_web::FetchConfig>()
            .cloned()
            .unwrap_or_default();
        let (loader, cache) = esm::make_cached_loader_with_fetch_config_and_prefetched(
            builtins,
            fetch_config,
            prefetched,
        );
        let result = self.engine.eval_module_attrs_pending_with_base(
            source,
            record_key,
            module_url,
            resolution_url,
            loader,
        );
        cache.forget_sources();
        result
    }

    /// Schedule blocking resource work on the process scheduler's shared blocking pool. The job
    /// must return its result through a channel owned by the embedder; it never touches the
    /// runtime's JavaScript engine from the worker thread.
    pub fn spawn_blocking_detached(&self, job: impl FnOnce() + Send + 'static) {
        self.pool.handle().spawn_detached(Box::new(job));
    }

    /// Node's `--trace-atomics-wait`: every `Atomics.wait` of this realm and of the workers it
    /// spawns reports its progress on stderr.
    pub fn trace_atomics_wait(&mut self) {
        TRACE_ATOMICS_WAIT.store(true, std::sync::atomic::Ordering::SeqCst);
        install_atomics_wait_trace(&mut self.engine, 0);
    }

    /// Run `path` as a CommonJS program entry (`require.main === module`, with `__dirname`/
    /// `__filename`/`require` in scope), then loop to quiescence. `Err` is the rendered
    /// uncaught error. This is what the CLI uses for `lumen-cli file.js`.
    pub fn run_main(&mut self, path: &str) -> Result<(), String> {
        let _mem = lumen::memstats::enter(lumen::memstats::Cat::Runtime);
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
        self.run_entry_loop();
        Ok(())
    }

    /// Give the engine the ESM loader (so dynamic `import()` works) and resolve bare relative
    /// specifiers against `path`.
    pub fn install_module_loader(&mut self, path: &str, has_source: bool) {
        self.install_module_loader_with(path, has_source, |_| None);
    }

    fn make_cached_module_loader(
        &mut self,
    ) -> (
        impl Fn(&str, &str, Option<&str>) -> Option<(String, String)>,
        std::rc::Rc<esm::LoaderCache>,
    ) {
        let builtins = self.builtin_modules();
        #[cfg(not(target_arch = "wasm32"))]
        {
            let fetch_config = self
                .engine
                .ctx()
                .op_state()
                .get::<lumen_web::FetchConfig>()
                .cloned()
                .unwrap_or_default();
            esm::make_cached_loader_with_fetch_config(builtins, fetch_config)
        }
        #[cfg(target_arch = "wasm32")]
        {
            esm::make_cached_loader(builtins)
        }
    }

    fn make_module_loader(
        &mut self,
    ) -> impl Fn(&str, &str, Option<&str>) -> Option<(String, String)> {
        self.make_cached_module_loader().0
    }

    /// Extend the runtime's module graph with host-provided source modules.
    pub fn install_module_loader_with(
        &mut self,
        path: &str,
        has_source: bool,
        source: impl Fn(&str) -> Option<&'static str> + 'static,
    ) {
        let loader = self.make_module_loader();
        self.engine
            .set_module_loader_attrs(move |specifier, referrer, attributes| {
                source(specifier)
                    .filter(|_| attributes.is_none())
                    .map(|text| (format!("host:{specifier}"), text.to_owned()))
                    .or_else(|| loader(specifier, referrer, attributes))
            });
        match lumen_host::canonicalize(path) {
            Ok(abs) => self.engine.set_import_base(&abs.to_string_lossy()),
            Err(_) if has_source => self.engine.set_import_base(path),
            Err(_) => {}
        }
    }

    fn start_main_raw(&mut self, path: &str, source: Option<&str>) -> Result<(), StartError> {
        self.install_module_loader(path, source.is_some());
        let global = self.engine.global_this();
        let entry = if source.is_some() {
            "__runMainSource"
        } else {
            "__runMain"
        };
        let run_main = self
            .engine
            .ctx()
            .get_member(&global, entry)
            .map_err(|_| StartError::NotInstalled("node runtime not installed".to_string()))?;
        let mut args = vec![Value::from_string(path.to_string())];
        if let Some(source) = source {
            args.push(Value::from_string(source.to_string()));
        }
        let result = self
            .engine
            .call_function(&run_main, Value::Undefined, &args);
        self.checkpoint();
        result.map(|_| ()).map_err(StartError::Thrown)
    }

    /// Run `path` as an ES module: its `import` graph resolves against disk + `node_modules`
    /// (and the `node:` builtins), then the loop runs to quiescence so top-level `await`,
    /// timers, and I/O settle. `Err` is the rendered uncaught error.
    pub fn run_module(&mut self, path: &str) -> Result<(), String> {
        let _mem = lumen::memstats::enter(lumen::memstats::Cat::Runtime);
        let source = lumen_host::sysfs::read_to_string(path)
            .map_err(|e| format!("cannot read {path}: {e}"))?;
        let source = import_source_text(source);
        let key = lumen_host::canonicalize(path)
            .unwrap_or_else(|_| std::path::PathBuf::from(path))
            .to_string_lossy()
            .into_owned();
        self.run_module_text(&source, &key)
    }

    /// Run `source` as an ES module identified by `key` (relative imports resolve against it),
    /// then the loop to quiescence. `--input-type=module` eval and piped stdin use this.
    pub fn run_module_source(&mut self, source: &str, key: &str) -> Result<(), String> {
        let _mem = lumen::memstats::enter(lumen::memstats::Cat::Runtime);
        self.run_module_text(&import_source_text(source.to_string()), key)
    }

    fn run_module_text(&mut self, source: &str, key: &str) -> Result<(), String> {
        let (loader, cache) = self.make_cached_module_loader();
        let t_eval = lumen_host::startup_timing().then(Instant::now);
        let result = self.engine.eval_module_attrs(source, key, loader);
        // The static graph has loaded: its sources live in the engine now.
        cache.forget_sources();
        if let Some(t0) = t_eval {
            report_load_stats("entry module evaluated", t0);
        }
        lumen::memstats::phase("entry module evaluated");
        match result {
            Ok(Completion::Value(_)) => {
                self.run_entry_loop();
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
    #[cfg(feature = "compiler")]
    pub fn run_precompiled(&mut self, blob: &lumen::Precompiled) -> Result<(), String> {
        let _mem = lumen::memstats::enter(lumen::memstats::Cat::Runtime);
        let loader = self.make_module_loader();
        self.engine.set_module_loader_attrs(loader);
        let t_load = lumen_host::startup_timing().then(Instant::now);
        let loaded = self.engine.load_precompiled(blob);
        if let Some(t0) = t_load {
            report_load_stats("precompiled program evaluated", t0);
        }
        match loaded {
            Ok(Completion::Value(_)) => {
                self.run_entry_loop();
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

    #[cfg(not(feature = "compiler"))]
    pub fn run_precompiled(&mut self, _: &lumen::Precompiled) -> Result<(), String> {
        Err("the Aot runtime requires a native payload".into())
    }

    #[cfg(feature = "compiler")]
    pub fn run_precompiled_owned(&mut self, bytes: std::sync::Arc<[u8]>) -> Result<(), String> {
        self.run_precompiled(&lumen::Precompiled::from_bytes(bytes))
    }

    #[cfg(not(feature = "compiler"))]
    pub fn run_precompiled_owned(&mut self, _: std::sync::Arc<[u8]>) -> Result<(), String> {
        Err("the Aot runtime requires a native payload".into())
    }

    #[cfg(feature = "aot-native")]
    pub fn run_native_owned(&mut self, bytes: std::sync::Arc<[u8]>) -> Result<(), String> {
        let _mem = lumen::memstats::enter(lumen::memstats::Cat::Runtime);
        self.install_native_builtins()?;
        let entry = self
            .engine
            .load_native_value_owned(bytes, None, &[], true)
            .map_err(|error| {
                self.fatal_exit = Some(1);
                error
            })?;
        self.drive_native_entry(entry)
    }

    /// Execute a linker-produced payload whose code/GOT live for the process.
    ///
    /// # Safety
    /// The ranges must be those exported by this payload's object and remain valid
    /// while any native callable exists. External users must not mutate the GOT.
    #[cfg(feature = "aot-native")]
    pub unsafe fn run_native_linked(
        &mut self,
        bytes: std::sync::Arc<[u8]>,
        code: *const u8,
        code_len: usize,
        got: *mut usize,
        got_len: usize,
    ) -> Result<(), String> {
        static LINKED_ENTRY: Mutex<()> = Mutex::new(());
        let _entry = LINKED_ENTRY
            .lock()
            .map_err(|_| "linked native entry lock poisoned".to_string())?;
        self.install_native_builtins()?;
        let value = unsafe {
            self.engine
                .load_native_linked_value_owned(bytes, code, code_len, got, got_len)
        }
        .map_err(|error| {
            self.fatal_exit = Some(1);
            error
        })?;
        self.drive_native_entry(value)
    }

    #[cfg(feature = "aot-native")]
    pub fn install_native_builtins(&mut self) -> Result<(), String> {
        if self.native_builtins_installed {
            return Ok(());
        }
        let global = self.engine.global_this();
        let getter = self
            .engine
            .ctx()
            .get_member(&global, "__esmBuiltin")
            .map_err(|_| "native Node builtin lookup is unavailable")?;
        let modules = self.builtin_modules();
        let mut modules: Vec<_> = modules.0.into_iter().collect();
        modules.sort_by(|a, b| a.0.cmp(&b.0));
        for (specifier, exports) in modules {
            let name = specifier.strip_prefix("node:").unwrap();
            if matches!(name, "http2") && !cfg!(feature = "http2")
                || matches!(name, "cluster") && !cfg!(feature = "cluster")
                || matches!(name, "dgram") && !cfg!(feature = "dgram")
                || matches!(name, "wasi") && !cfg!(feature = "wasi")
            {
                continue;
            }
            let default = self
                .engine
                .call_function(&getter, global.clone(), &[Value::from_string(name.into())])
                .map_err(|error| describe_error(self.engine.ctx(), &error))?;
            let namespace = self.engine.ctx().new_object_with_proto(&Value::Null);
            self.engine
                .ctx()
                .set_member(&namespace, "default", default.clone())
                .map_err(|_| "cannot initialize native builtin namespace")?;
            for key in exports.split_whitespace() {
                let value =
                    self.engine.ctx().get_member(&default, key).map_err(|_| {
                        format!("cannot read native builtin export {specifier}:{key}")
                    })?;
                self.engine
                    .ctx()
                    .set_member(&namespace, key, value)
                    .map_err(|_| "cannot initialize native builtin namespace")?;
            }
            let object = self
                .engine
                .ctx()
                .get_member(&global, "Object")
                .map_err(|_| "Object intrinsic is unavailable")?;
            let freeze = self
                .engine
                .ctx()
                .get_member(&object, "freeze")
                .map_err(|_| "Object.freeze intrinsic is unavailable")?;
            self.engine
                .call_function(&freeze, object, &[namespace.clone()])
                .map_err(|error| describe_error(self.engine.ctx(), &error))?;
            self.engine
                .register_native_module(name.to_owned(), namespace.clone())
                .map_err(str::to_owned)?;
            self.engine
                .register_native_module(specifier, namespace)
                .map_err(str::to_owned)?;
        }
        self.native_builtins_installed = true;
        Ok(())
    }

    #[cfg(feature = "aot-native")]
    fn drive_native_entry(&mut self, entry: Value) -> Result<(), String> {
        self.engine.ctx().observe_promise_for_host(&entry);
        self.run_entry_loop();
        match self.engine.ctx().promise_state_for_host(&entry) {
            Some((2, error)) => {
                self.fatal_exit = Some(1);
                Err(describe_error(self.engine.ctx(), &error))
            }
            Some((0, _)) => {
                // A pending entry with no live timers/tasks cannot make further progress.
                self.fatal_exit = Some(13);
                Err("native top-level await did not settle".into())
            }
            _ if self.fatal_exit.is_some() => Err(format!(
                "native entry exited with status {}",
                self.fatal_exit.unwrap()
            )),
            _ => Ok(()),
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
        let loader = self.make_module_loader();
        self.engine.set_module_loader_attrs(loader);
        self.engine.set_import_base(base);
        let result = if is_module {
            let loader = self.make_module_loader();
            self.engine.eval_module_attrs(source, base, loader)
        } else {
            self.engine.eval(source, false)
        };
        // Drain the checkpoint from top-level code, but not the macrotask loop.
        self.checkpoint();
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
    /// set — a cooperative terminate that also drops any still-pending timers. The loop blocks
    /// with no timeout when nothing is armed, so whoever sets `stop` (or raises the realm
    /// interrupt) must also wake it, through [`Runtime::waker`] or the interrupt handle.
    pub fn run_worker_loop(&mut self, stop: &std::sync::atomic::AtomicBool) {
        self.worker_loop(stop);
        lumen_host::perf::mark_loop_exit();
        if self.interrupted() {
            self.release_blocked_io();
        }
    }

    fn worker_loop(&mut self, stop: &std::sync::atomic::AtomicBool) {
        loop {
            if stop.load(Ordering::SeqCst) || self.interrupted() {
                return;
            }
            self.checkpoint();
            self.report_unhandled_rejections();
            loop {
                let mut progressed = false;
                for (cb, args) in self.take_queued_callbacks() {
                    progressed = true;
                    self.fire(&cb, &args);
                }
                for task in self.take_worker_rejection_tasks() {
                    progressed = true;
                    self.run_worker_rejection_task(task);
                }
                let now = Instant::now();
                while let Some((cb, args, owner)) = self.take_next_due_timer(now) {
                    progressed = true;
                    self.fire_timer(&cb, &args, &owner);
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
            if self.ticks_pending() {
                continue;
            }
            let deadline = self.wait_deadline();
            let blocked = Instant::now();
            let received = match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if deadline <= now {
                        continue;
                    }
                    self.completions.recv_timeout(deadline - now)
                }
                None => self
                    .completions
                    .recv()
                    .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
            };
            lumen_host::perf::add_idle(blocked.elapsed());
            match received {
                Ok(done) => self.dispatch(done),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            #[cfg(test)]
            self.loop_wakeups.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Each `node:` builtin's list of named exports, from the table lumen-node generates out of
    /// `esm_exports.js` (the loader can't enumerate a builtin's exports from Rust, and reading
    /// them from the engine would load the builtins). The loader builds a builtin's ESM source
    /// from its list on first import.
    fn builtin_modules(&mut self) -> esm::BuiltinModules {
        if self.root_providers == RootProviders::Browser {
            return esm::BuiltinModules(std::collections::HashMap::new());
        }
        let global = self.engine.global_this();
        let ctx = self.engine.ctx();
        let mut map = std::collections::HashMap::new();
        let names = ctx
            .get_member(&global, "__builtinNames")
            .ok()
            .and_then(|v| ctx.coerce_string(&v).ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        for name in names.split(',').filter(|s| !s.is_empty()) {
            let exports = lumen_node::ESM_EXPORTS
                .iter()
                .find_map(|(n, list)| (*n == name).then_some(*list))
                .unwrap_or("");
            map.insert(format!("node:{name}"), exports);
        }
        esm::BuiltinModules(map)
    }

    /// Evaluate a script, then run the event loop until quiescent — timers fired, spawned
    /// work completed, promise queue empty.
    pub fn eval(&mut self, src: &str) -> Result<Completion, lumen_host::ParseError> {
        let _mem = lumen::memstats::enter(lumen::memstats::Cat::Runtime);
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
        let id = lumen_host::register_task(self.engine.ctx(), callback, None, decode);
        self.pool.spawn_blocking(id, work);
    }

    /// A sender that pushes a completion into this runtime's loop from outside it. An embedder
    /// with no threads (the browser) settles tasks it registered with
    /// [`lumen_host::register_task`] this way, then calls [`Self::run_until_idle`].
    pub fn completion_sender(&self) -> CompletionSender {
        CompletionSender::new(self.wake.clone())
    }

    /// Unpark `notify` whenever a completion, a wake or an interrupt arrives, for a host that
    /// parks this loop's thread itself between [`Self::run_until_idle`] turns instead of blocking
    /// in [`Self::wait_for_completion`].
    pub fn set_completion_notify(&self, notify: Arc<dyn lumen_os::sched::Unpark>) {
        self.wake.set_notify(notify);
    }

    /// A handle that ends [`Self::wait_for_completion`] when an embedder-owned channel has
    /// something to read.
    pub fn waker(&self) -> RuntimeWaker {
        RuntimeWaker {
            wake: self.wake.clone(),
        }
    }

    /// Block until a completion or wake arrives, or `timeout` passes. The completion is
    /// dispatched (its JS callback is queued for the next [`Self::run_until_idle`]); returns
    /// whether one arrived.
    pub fn wait_for_completion(&mut self, timeout: Duration) -> bool {
        if cfg!(target_arch = "wasm32") || self.halted() || timeout.is_zero() {
            return false;
        }
        let blocked = Instant::now();
        let received = self.completions.recv_timeout(timeout);
        lumen_host::perf::add_idle(blocked.elapsed());
        match received {
            Ok(done) => {
                self.dispatch(done);
                true
            }
            Err(_) => false,
        }
    }

    /// Run every turn that is ready now (microtasks, queued callbacks, due timers, delivered
    /// completions) and return without blocking, so a host that owns the thread (a browser tab)
    /// can hand control back and resume when [`LoopStatus::next_timer_ms`] elapses or a
    /// completion is pushed.
    pub fn run_until_idle(&mut self) -> LoopStatus {
        loop {
            self.checkpoint();
            self.report_unhandled_rejections();
            let mut progressed = false;
            for (cb, args) in self.take_queued_callbacks() {
                if self.halted() {
                    break;
                }
                progressed = true;
                self.fire(&cb, &args);
            }
            let now = Instant::now();
            while let Some((cb, args, owner)) = self.take_next_due_timer(now) {
                if self.halted() {
                    break;
                }
                progressed = true;
                self.fire_timer(&cb, &args, &owner);
            }
            while !self.halted() {
                let Ok(done) = self.completions.try_recv() else {
                    break;
                };
                progressed = true;
                self.dispatch(done);
            }
            if self.halted() || !(progressed || self.ticks_pending()) {
                break;
            }
        }
        let now = Instant::now();
        let next_timer_ms = self
            .next_timer_deadline()
            .map(|d| d.saturating_duration_since(now).as_secs_f64() * 1000.0);
        let pending_tasks = self
            .engine
            .ctx()
            .op_state()
            .get::<TaskRegistry>()
            .is_some_and(|r| r.has_ref_pending());
        LoopStatus {
            next_timer_ms,
            pending_tasks,
            idle: self.idle(),
            halted: self.halted(),
        }
    }

    /// Run the loop until nothing is pending: no microtasks, no queued callbacks, no live
    /// timers, no in-flight tasks.
    pub fn run_to_completion(&mut self) {
        self.run_loop();
        if self.interrupted() {
            self.release_blocked_io();
        }
    }

    /// [`Self::run_to_completion`] for the program's own loop: the point `nodeTiming` reports as
    /// the loop's start and exit.
    fn run_entry_loop(&mut self) {
        lumen_host::perf::mark(lumen_host::perf::Milestone::LoopStart);
        self.run_to_completion();
        lumen_host::perf::mark_loop_exit();
    }

    /// Stop the realm's workers and close its sockets and listeners, so the threads blocked on
    /// them end instead of waiting for a peer that may never act.
    fn release_blocked_io(&mut self) {
        let ctx = self.engine.ctx();
        worker::terminate_all(ctx);
        lumen_node::close_native_io(ctx);
        lumen_web::close_servers(ctx);
    }

    fn run_loop(&mut self) {
        loop {
            // Run everything already runnable. Each JS entry is followed by a microtask
            // checkpoint, matching the "after every macrotask" model.
            self.checkpoint();
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
                if self.tick_recovery && !self.halted() {
                    self.tick_recovery = false;
                    progressed = true;
                    self.checkpoint();
                    self.report_unhandled_rejections();
                }
                let now = Instant::now();
                while let Some((cb, args, owner)) = self.take_next_due_timer(now) {
                    if self.halted() {
                        return;
                    }
                    progressed = true;
                    self.fire_timer(&cb, &args, &owner);
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
            // Ticks left behind by a throwing tick run at the next checkpoint, not after a wait.
            if self.ticks_pending() {
                continue;
            }

            // With no thread to block, the embedder resumes the loop when a completion arrives
            // or the next timer is due (see `run_until_idle`).
            #[cfg(target_arch = "wasm32")]
            return;
            #[cfg(not(target_arch = "wasm32"))]
            let maintenance = self.idle_collect();
            // A pending idle pass must not wait forever behind an otherwise silent task.
            #[cfg(not(target_arch = "wasm32"))]
            {
                let deadline = self.wait_deadline_with(maintenance);
                match deadline {
                    Some(deadline) => {
                        let now = Instant::now();
                        if deadline > now {
                            let blocked = Instant::now();
                            let received = self.completions.recv_timeout(deadline - now);
                            lumen_host::perf::add_idle(blocked.elapsed());
                            if let Ok(done) = received {
                                self.dispatch(done);
                            }
                        }
                    }
                    None => match {
                        let blocked = Instant::now();
                        let received = self.completions.recv();
                        lumen_host::perf::add_idle(blocked.elapsed());
                        received
                    } {
                        Ok(done) => self.dispatch(done),
                        // The pool is gone (unreachable while `self.pool` lives); nothing can
                        // ever complete, so pending tasks are abandoned rather than spun on.
                        Err(_) => return,
                    },
                }
            }
        }
    }

    /// When a blocking wait must end by itself: the next timer or the idle maintenance pass,
    /// whichever is first. `None` blocks until something is delivered.
    fn wait_deadline(&mut self) -> Option<Instant> {
        let maintenance = self.idle_collect();
        self.wait_deadline_with(maintenance)
    }

    fn wait_deadline_with(&mut self, maintenance: Option<Instant>) -> Option<Instant> {
        match (self.next_timer_deadline(), maintenance) {
            (Some(timer), Some(gc)) => Some(timer.min(gc)),
            (timer, gc) => timer.or(gc),
        }
    }

    /// Collect while the loop is about to block. The engine's own trigger fires on allocation
    /// volume, so a program that loads a lot and then waits (a server, an editor host) never
    /// collects again and keeps every function body it ran once; the collector releases those
    /// bodies only across two collections it actually runs. Throttled so a loop that blocks
    /// between every request does not collect on every request.
    /// Returns the next maintenance deadline, if an initial or follow-up pass is pending.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    fn idle_collect(&mut self) -> Option<Instant> {
        const MIN_INTERVAL: Duration = Duration::from_secs(1);
        const MIN_NEW_OBJECTS: i64 = 10_000;
        let (last, live_then) = self.idle_gc;
        let ctx = self.engine.ctx();
        let live = ctx.live_object_count();
        // A collection releases the bodies that were cold across the *previous* one, so the
        // collection after a busy stretch is followed by one more even when nothing allocated.
        let busy = (live - live_then).abs() >= MIN_NEW_OBJECTS;
        if !busy && !self.idle_gc_followup {
            self.idle_gc.0 = Instant::now();
            return None;
        }
        if last.elapsed() < MIN_INTERVAL {
            return Some(last + MIN_INTERVAL);
        }
        ctx.collect_garbage_for_host();
        ctx.release_unused_memory_for_host();
        self.idle_gc_followup = busy;
        self.idle_gc = (Instant::now(), ctx.live_object_count());
        self.idle_gc_followup
            .then_some(self.idle_gc.0 + MIN_INTERVAL)
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

    /// Hand the parsed command-line options (a JSON object keyed by canonical option name) to the
    /// JS side, which answers `getOptionValue` from them.
    pub fn set_cli_options(&mut self, json: &str) {
        let global = self.engine.global_this();
        if let Ok(function) = self
            .engine
            .ctx()
            .get_member(&global, "__lumenSetCliOptions")
        {
            let _ = self.engine.call_function(
                &function,
                Value::Undefined,
                &[Value::from_string(json.into())],
            );
        }
    }

    /// Node's `--expose-gc`: define `globalThis.gc`, which runs lumen's cycle collector.
    pub fn expose_gc(&mut self) {
        self.engine.define_global("gc", 0, |ctx, _, _| {
            ctx.collect_garbage_for_host();
            Ok(Value::Undefined)
        });
    }

    /// V8's `--expose-externalize-string`: `externalizeString` (a no-op here, as lumen strings
    /// have no external representation) and `isOneByteString` (every code unit fits Latin-1).
    pub fn expose_externalize_string(&mut self) {
        self.engine
            .define_global("externalizeString", 1, |ctx, _, args| {
                if !matches!(args.first(), Some(Value::Str(_))) {
                    return Err(ctx.make_error("TypeError", "First parameter is not a string"));
                }
                Ok(Value::Undefined)
            });
        self.engine
            .define_global("isOneByteString", 1, |ctx, _, args| {
                let Some(Value::Str(value)) = args.first() else {
                    return Err(ctx.make_error("TypeError", "First parameter is not a string"));
                };
                Ok(Value::Bool(
                    value.chars().all(|character| character as u32 <= 0xff),
                ))
            });
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
        let exit_code = |rt: &mut Runtime| -> Option<i32> {
            match rt.engine.ctx().get_member(&process, "exitCode") {
                Ok(Value::Num(n)) => Some(n as i32),
                Ok(Value::Str(s)) => s.to_string().trim().parse().ok(),
                _ => None,
            }
        };
        let code = |rt: &mut Runtime| exit_code(rt).unwrap_or(0);
        let exiting = |rt: &mut Runtime| {
            matches!(
                rt.engine.ctx().get_member(&process, "_exiting"),
                Ok(Value::Bool(true))
            )
        };
        let emit_exit = |rt: &mut Runtime, c: i32| {
            let _ = rt
                .engine
                .ctx()
                .set_member(&process, "_exiting", Value::Bool(true));
            if let Err(e) = rt.engine.call_function(
                &emit,
                process.clone(),
                &[Value::from_string("exit".to_string()), Value::Num(c as f64)],
            ) {
                rt.report_uncaught(&e);
            }
            rt.checkpoint();
        };
        // A 'beforeExit' listener may schedule more work; Node re-enters the loop until one
        // round adds nothing.
        let mut rounds = 0;
        loop {
            // An uncaught exception ends the process without (further) 'beforeExit'. A generic
            // one (code 1) runs 'exit' (unless the fatal-exception handler already did) and
            // exits with `process.exitCode`, which those listeners may change; Node's other
            // fatal codes exit as they are.
            if let Some(fatal) = self.fatal_exit {
                if fatal != 1 {
                    return fatal;
                }
                if !exiting(self) {
                    let _ = self
                        .engine
                        .ctx()
                        .set_member(&process, "exitCode", Value::Num(1.0));
                    emit_exit(self, 1);
                }
                return exit_code(self).unwrap_or(1);
            }
            if rounds == 1000 {
                break;
            }
            rounds += 1;
            let c = code(self);
            if let Err(e) = self.engine.call_function(
                &emit,
                process.clone(),
                &[
                    Value::from_string("beforeExit".to_string()),
                    Value::Num(c as f64),
                ],
            ) {
                self.report_uncaught(&e);
            }
            self.checkpoint();
            if self.fatal_exit.is_some() {
                continue;
            }
            if self.idle() {
                break;
            }
            self.run_entry_loop();
        }
        if !exiting(self) {
            let c = code(self);
            emit_exit(self, c);
        }
        if self.fatal_exit.is_some() {
            return exit_code(self).unwrap_or(1);
        }
        code(self)
    }

    fn ticks_pending(&mut self) -> bool {
        self.engine
            .ctx()
            .op_state()
            .get::<process::TickQueue>()
            .is_some_and(|q| !q.queue.is_empty())
    }

    fn idle(&mut self) -> bool {
        if self.engine.has_pending_jobs() {
            return false;
        }
        let state = self.engine.ctx().op_state();
        let callbacks_queued = state
            .get::<CallbackQueue>()
            .is_some_and(|q| !q.queue.is_empty())
            || state
                .get::<process::TickQueue>()
                .is_some_and(|q| !q.queue.is_empty());
        let timers_pending = state
            .get::<lumen_timers::Timers>()
            .is_some_and(|t| t.has_pending());
        let tasks_pending = state
            .get::<TaskRegistry>()
            .is_some_and(|r| r.has_ref_pending());
        !callbacks_queued
            && !timers_pending
            && !tasks_pending
            && self.worker_rejection_tasks.borrow().is_empty()
    }

    fn take_queued_callbacks(&mut self) -> VecDeque<(Value, Vec<Value>)> {
        match self.engine.ctx().host_mut::<CallbackQueue>() {
            Some(q) if !q.queue.is_empty() => std::mem::take(&mut q.queue),
            _ => VecDeque::new(),
        }
    }

    /// One due timer at a time (see `Timers::take_next_due`): each callback runs before the next
    /// is taken, so it can clear or refresh a timer due in the same turn. `now` is fixed for the
    /// turn, so an interval cannot keep itself due forever.
    fn take_next_due_timer(
        &mut self,
        now: Instant,
    ) -> Option<(Value, Vec<Value>, lumen::embed::RealmHandle)> {
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
        if done.task == SIGNAL_TASK {
            if let Ok(signal) = done.result.downcast::<i32>() {
                self.deliver_signal(*signal);
            }
            return;
        }
        if done.task == WAKE_TASK {
            return;
        }
        // Native resources retain their admitting scope even for raw callbacks (sockets,
        // subprocesses, workers). Promise reactions additionally keep their own scope.
        let Some(settled) = owner_loop::settle(self.engine.ctx(), done) else {
            return; // cancelled while in flight
        };
        match &settled.outcome {
            owner_loop::Outcome::Call { callback, args } => self.fire(callback, args),
            owner_loop::Outcome::Uncaught(error) => self.report_uncaught(error),
        }
        settled.finish(self.engine.ctx());
        self.checkpoint();
        self.report_unhandled_rejections();
    }

    /// Emit `signal` to the program's `process.on` listeners; with none (the program dropped
    /// them since the sender looked), the signal takes its default action.
    fn deliver_signal(&mut self, signal: i32) {
        let global = self.engine.global_this();
        let deliver = self
            .engine
            .ctx()
            .get_member(&global, "__lumen_deliver_signal")
            .unwrap_or(Value::Undefined);
        let handled = match self.engine.call_function(
            &deliver,
            Value::Undefined,
            &[Value::Num(signal as f64)],
        ) {
            Ok(Value::Bool(handled)) => handled,
            Ok(_) => false,
            Err(error) => {
                self.report_uncaught(&error);
                true
            }
        };
        if !handled && child_realm::terminates_by_default(signal) {
            self.signal_exit.get_or_insert(signal);
            if let Some(handle) = &self.interrupt {
                handle.flag().store(true, Ordering::SeqCst);
            }
        }
        self.checkpoint();
        self.report_unhandled_rejections();
    }

    /// The signal that stopped this realm by its default action, if one did.
    pub(crate) fn signal_exit(&self) -> Option<i32> {
        self.signal_exit
    }

    /// The live signal-listener mask (see [`RealmProcess::signal_handlers`]).
    pub(crate) fn signal_handlers(&mut self) -> Option<Arc<AtomicU64>> {
        self.engine
            .ctx()
            .op_state()
            .get::<RealmProcess>()
            .map(|realm| Arc::clone(&realm.signal_handlers))
    }

    /// Node's tick checkpoint (`processTicksAndRejections`): run the nextTick queue (including
    /// ticks queued by ticks), then the promise microtasks, and repeat until both are empty.
    fn checkpoint(&mut self) {
        loop {
            loop {
                if self.halted() {
                    return;
                }
                let next = self
                    .engine
                    .ctx()
                    .host_mut::<process::TickQueue>()
                    .and_then(|q| q.queue.pop_front());
                let Some((callback, args)) = next else {
                    break;
                };
                if let Err(e) = self
                    .engine
                    .call_function(&callback, Value::Undefined, &args)
                {
                    // As in Node, a throwing tick unwinds the drain: once the error is handled,
                    // the rest of the queue and the pending microtasks wait for the next
                    // checkpoint, so a tick that keeps rescheduling a throw cannot starve the
                    // loop, and queued ticks still run before the promise jobs.
                    self.report_uncaught(&e);
                    self.tick_recovery = true;
                    return;
                }
            }
            self.run_microtasks();
            let more = self
                .engine
                .ctx()
                .op_state()
                .get::<process::TickQueue>()
                .is_some_and(|q| !q.queue.is_empty());
            if !more {
                return;
            }
        }
    }

    /// Drain the microtask queue; a throwing `queueMicrotask` callback is an uncaught exception,
    /// and the queue carries on once a listener handled it.
    fn run_microtasks(&mut self) {
        loop {
            self.engine.run_microtasks();
            let errors = self.engine.take_task_errors();
            if errors.is_empty() {
                return;
            }
            for e in errors {
                if self.halted() {
                    return;
                }
                self.report_uncaught(&e);
            }
        }
    }

    /// One JS callback entry: call, report an uncaught throw, then the microtask checkpoint.
    fn fire(&mut self, callback: &Value, args: &[Value]) {
        self.fire_with_this(callback, Value::Undefined, args);
    }

    /// Fire a timer with the registering realm's global-this value. The callback's lexical
    /// realm is still selected by the engine from the callback object itself.
    fn fire_timer(&mut self, callback: &Value, args: &[Value], owner: &lumen::embed::RealmHandle) {
        let this = self
            .engine
            .ctx()
            .with_host_realm(owner, |ctx| ctx.global_this())
            .expect("live timer owner realm remains registered until cancellation");
        self.fire_with_this(callback, this, args);
    }

    fn fire_with_this(&mut self, callback: &Value, this: Value, args: &[Value]) {
        if std::mem::take(&mut self.tick_recovery) {
            self.checkpoint();
        }
        if let Err(e) = self.engine.call_function(callback, this, args) {
            self.report_uncaught(&e);
        }
        self.checkpoint();
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
        // The hook answers `true` (handled), `false` (fatal), or the exit code of a fatal error
        // it could not hand to the process (Node's 6: `process._fatalException` is not a
        // function). A throw out of the handlers is itself fatal with Node's code 7, and that
        // error is the one reported.
        let (code, thrown) = match self.engine.call_function(
            &fire,
            Value::Undefined,
            &[error.clone(), Value::str(origin)],
        ) {
            Ok(Value::Bool(true)) => return,
            Ok(Value::Num(n)) => (n as i32, None),
            Ok(_) => (1, None),
            Err(e) => (7, Some(e)),
        };
        if self.interrupted() {
            return;
        }
        let error = thrown.as_ref().unwrap_or(error);
        self.print_fatal(error, prefix);
        self.fatal_exit.get_or_insert(code);
    }

    fn print_fatal(&mut self, error: &Value, prefix: &str) {
        // A rejected TypeScript source prints as Node prints it (its `stack` and `code`).
        let ctx = self.engine.ctx();
        let member = |ctx: &mut lumen_host::Ctx, key: &str| match ctx.get_member(error, key) {
            Ok(Value::Str(s)) => Some(s.to_string()),
            _ => None,
        };
        let ts_text = member(ctx, "code")
            .filter(|c| c.ends_with("_TYPESCRIPT_SYNTAX"))
            .and_then(|code| {
                member(ctx, "stack").map(|s| lumen::typescript::node_uncaught_text(&s, &code))
            });
        let line = match ts_text {
            Some(t) => t,
            None => {
                let global = self.engine.global_this();
                // A Node runtime reports a fatal error as Node does (its own text, no prefix).
                if let Ok(report) = self.engine.ctx().get_member(&global, "__lumenFatalReport") {
                    if report.as_obj().is_some() {
                        if let Ok(Value::Str(s)) =
                            self.engine
                                .call_function(&report, Value::Undefined, &[error.clone()])
                        {
                            console::write_err_line(self.engine.ctx(), s.to_string());
                            return;
                        }
                    }
                }
                let describe = self
                    .engine
                    .ctx()
                    .get_member(&global, "__lumenDescribeError");
                let detailed = match describe {
                    Ok(f) if f.as_obj().is_some() => {
                        match self
                            .engine
                            .call_function(&f, Value::Undefined, &[error.clone()])
                        {
                            Ok(Value::Str(s)) => Some(s.to_string()),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                match detailed {
                    Some(text) => format!("{prefix} {text}"),
                    None => format!(
                        "{prefix} {}",
                        console::describe_error(self.engine.ctx(), error)
                    ),
                }
            }
        };
        console::write_err_line(self.engine.ctx(), line);
    }

    /// Report promises rejected without a handler. Called after each microtask checkpoint; a
    /// rejection handled in the same checkpoint won't appear. Node's default
    /// (`--unhandled-rejections=throw`): a `process.on('unhandledRejection')` listener owns it,
    /// otherwise it is raised as an uncaught exception — which an `'uncaughtException'` listener
    /// may still catch, and which is fatal if nothing does.
    fn report_unhandled_rejections(&mut self) {
        if self.browser_rejections.is_enabled() {
            for error in self.browser_rejections.checkpoint(&mut self.engine) {
                self.report_uncaught(&error);
            }
            return;
        }
        // Rejections reported earlier that have a handler now (Node's 'rejectionHandled').
        let handled = self.engine.take_late_handled_rejections();
        if !handled.is_empty() {
            let fire = self.fire_handled.clone();
            for promise in handled {
                if self.halted() {
                    return;
                }
                if let Err(e) = self
                    .engine
                    .call_function(&fire, Value::Undefined, &[promise])
                {
                    self.report_uncaught(&e);
                }
            }
        }
        for (promise, reason) in self.engine.take_unhandled_rejections_full() {
            if self.halted() {
                return;
            }
            let fire = self.fire_rejection.clone();
            // The policy returns true when it dealt with the rejection, or `[error]` to raise
            // `error` (the reason, or Node's UnhandledPromiseRejection for a non-error).
            let raised =
                match self
                    .engine
                    .call_function(&fire, Value::Undefined, &[promise, reason.clone()])
                {
                    Ok(Value::Bool(true)) => continue,
                    Ok(list @ Value::Obj(_)) => {
                        self.engine.ctx().get_member(&list, "0").unwrap_or(reason)
                    }
                    // A throwing 'unhandledRejection' listener is an uncaught exception itself.
                    Err(e) => {
                        self.report_uncaught(&e);
                        continue;
                    }
                    _ => reason,
                };
            self.report_fatal(&raised, "Uncaught (in promise)", "unhandledRejection");
        }
    }

    /// The exit code an unhandled exception or rejection decided, if one did.
    pub fn fatal_exit_code(&self) -> Option<i32> {
        self.fatal_exit
    }

    fn interrupted(&self) -> bool {
        self.interrupt
            .as_ref()
            .is_some_and(|handle| handle.flag().load(Ordering::SeqCst))
    }

    /// The loop stops: a fatal error, or the embedder / `process.exit` asked the realm to end.
    fn halted(&self) -> bool {
        self.fatal_exit.is_some() || self.interrupted() || self.engine.is_terminated()
    }

    /// A worker realm's `terminate()` flag: the engine polls it at every call and loop turn, and
    /// the loop treats it like an embedder's interrupt (a terminated realm reports nothing).
    pub(crate) fn set_worker_interrupt(&mut self, flag: Arc<AtomicBool>) {
        self.engine.set_interrupt(Arc::clone(&flag));
        self.share_interrupt(InterruptHandle::from_flag(flag));
    }

    /// Stop on `handle` (an embedded parent's, shared with its other workers) and wake this loop
    /// when it is raised.
    fn share_interrupt(&mut self, handle: InterruptHandle) {
        self.interrupt_wake = Some(handle.subscribe(loop_wake(&self.wake)));
        self.interrupt = Some(handle);
    }

    /// A handle that stops this runtime from any thread, however it is blocked (see
    /// [`InterruptHandle`]). The first call installs the engine interrupt, after which crossing
    /// the live-object ceiling or the heap limit also terminates the realm instead of throwing a
    /// catchable `RangeError`.
    pub fn interrupt_handle(&mut self) -> InterruptHandle {
        if self.interrupt.is_none() {
            let handle = InterruptHandle::new();
            self.engine.set_interrupt(Arc::clone(handle.flag()));
            self.share_interrupt(handle);
        }
        self.interrupt.clone().expect("interrupt installed")
    }

    /// Interrupt this runtime once `limit` has passed, as [`InterruptHandle::interrupt`] would.
    /// A later call replaces the earlier deadline; dropping the runtime cancels it.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn set_deadline(&mut self, limit: Duration) {
        let handle = self.interrupt_handle();
        self.deadline = Some(lumen_os::sched::Deadline::start(
            "lumen-deadline",
            limit,
            move || handle.interrupt(),
        ));
    }

    /// Run `f` so that SIGINT stops the JS it runs (as `vm`'s `breakOnSigint` does) instead of
    /// killing the process; the flag says whether it was the signal that stopped it. The realm
    /// stays usable afterwards.
    pub fn with_sigint_break<R>(&mut self, f: impl FnOnce(&mut Runtime) -> R) -> (R, bool) {
        let raised = self.engine.ctx().script_timeout_flag();
        let guard = lumen_node::SigintBreak::new(Arc::clone(&raised));
        let result = f(self);
        let fired = guard.fired();
        drop(guard);
        if fired {
            raised.store(false, Ordering::SeqCst);
        }
        (result, fired)
    }

    /// Whether the realm has been interrupted.
    pub fn is_interrupted(&self) -> bool {
        self.interrupted()
    }

    /// A handle that stops this realm from another thread. `None` unless embedded.
    pub fn terminator(&self) -> Option<Terminator> {
        Some(Terminator {
            handle: self.interrupt.clone()?,
            wake: self.wake.clone(),
        })
    }

    /// Run an embedded realm's entry script to the end: `.mjs` as an ES module, anything else as
    /// the CommonJS main module (the Node programs this hosts are `.cjs`). Returns how it ended.
    pub fn run_embedded_main(&mut self, path: &str) -> RealmExit {
        self.prepare_main();
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
        self.prepare_main();
        let result = self.run_main_from(path, Some(source));
        self.finish_embedded(result)
    }

    /// Node's prepareMainThreadExecution, as the CLI runs it: opens the IPC channel a parent
    /// passed in `NODE_CHANNEL_FD` and sets up a cluster worker.
    fn prepare_main(&mut self) {
        let global = self.engine.global_this();
        if let Ok(function) = self.engine.ctx().get_member(&global, "__lumenPrepareMain") {
            if function.is_callable() {
                let _ = self.engine.call_function(&function, Value::Undefined, &[]);
            }
        }
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

/// `LUMEN_STARTUP_TIMING` report: wall time since `t0`, plus the module-load counters.
fn report_load_stats(what: &str, t0: Instant) {
    let (pms, pn, pb) = lumen::load_stats::get(&lumen::load_stats::PARSE);
    let (fms, fn_, _) = lumen::load_stats::get(&lumen::load_stats::FETCH);
    eprintln!(
        "[startup] {what} {:?} (parse/decode {pms:.1} ms, {pn} modules, {pb} bytes; fetch {fms:.1} ms, {fn_} loads)",
        t0.elapsed()
    );
}

static TRACE_ATOMICS_WAIT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Print each `Atomics.wait` step of `engine` the way Node's `--trace-atomics-wait` does; the
/// thread id is Node's (0 for the main thread, a worker's `threadId` otherwise).
pub(crate) fn install_atomics_wait_trace(engine: &mut Engine, thread_id: u64) {
    use lumen::AtomicsWaitPhase::*;
    engine.set_atomics_wait_hook(Some(Box::new(move |event| {
        let call = format!(
            "[Thread {thread_id}] Atomics.wait({:#x} + {:x}, {}, {})",
            event.buffer,
            event.byte_index,
            event.value,
            if event.timeout_ms.is_infinite() {
                "inf".to_string()
            } else {
                format!("{}", event.timeout_ms)
            }
        );
        let outcome = match event.phase {
            Started => "started",
            NotEqual => "did not wait because the values mismatched",
            Woken => "was woken up by another thread",
            TimedOut => "timed out",
        };
        eprintln!("(node:{}) {call} {outcome}", std::process::id());
    })));
}

pub(crate) fn atomics_wait_trace_enabled() -> bool {
    TRACE_ATOMICS_WAIT.load(std::sync::atomic::Ordering::SeqCst)
}

fn js_source_string(s: &str) -> String {
    lumen_common::json::quote(s, &lumen_common::json::Quote::JS_SOURCE)
}
