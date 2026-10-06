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
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::mpsc;

pub use lumen::bytecode::Tier;
#[cfg(feature = "compiler")]
use lumen::embed::HostRealmEvalError;
pub use lumen::embed::{
    Ctx, NativeClosure, NativeFn, OpError, OpState, ResourceId, ResourceTable, Value,
};
use lumen::embed::{JsHost, RealmHandle};
pub use lumen::{well_formed_utf8, Completion, Engine, ParseError};

/// Compression codecs (zlib, Brotli, Zstandard), shared by web CompressionStream, node:zlib and
/// the Bun APIs.
pub use lumen_common::compress as codec;

pub mod encoding;
/// The native `Event` / `EventTarget` / `AbortSignal` / `DOMException` core (`docs/native-events.md`).
pub mod events;
/// Native structured-clone attachments (`__cloneTransfer`).
pub mod clone_transfer;
/// `MessageEvent`, `CloseEvent`, `PromiseRejectionEvent`, `MessagePort`, `MessageChannel` and
/// `BroadcastChannel` (`docs/native-messaging.md`).
pub mod messaging;
/// The native `Navigator` interface and the `navigator` global.
pub mod navigator;
/// Event delivery for hosts without their own event loop (the Bitnest kernel): `install`,
/// `has_ready`, `pump` and `settle`.
pub mod owner_loop;
/// The process clock behind `performance` and the event loop's milestone and idle counters.
pub mod perf;
/// The native `Performance` interface and the `performance` and `self` globals.
pub mod performance;
/// Transferable `MessagePort` endpoints.
pub mod ports;
/// `structuredClone` and the wire format behind `postMessage` (`docs/native-clone.md`).
pub mod structured_clone;
/// Typed host services scoped to one realm.
pub mod random;
pub mod realm_services;
pub mod blob;
pub mod sysfs;
/// Network requests for native web classes: the shared request pipeline and `XMLHttpRequest`
/// (`docs/native-network.md`).
pub mod net;
/// Native callbacks scheduled through the realm's `setTimeout`.
pub mod timers;
/// Monotonic and wall clocks that also work on `wasm32-unknown-unknown` (`performance.now()` /
/// `Date.now()`), where `std::time::Instant::now()` panics.
pub mod time;
pub mod url;
pub mod url_pattern;
pub mod webidl;
#[cfg(feature = "webcrypto")]
pub mod webcrypto;

/// Browser-embedding hooks: the suspending synchronous host call (Worker + `Atomics.wait`, or
/// JSPI) and the completion queue the embedder pushes settled Promises into.
pub mod browser;

/// Checkpoint hook of a glue built with `LUMEN_GLUE_MEM_MARKS=1`.
#[lumen_bind::module(name = "memMark")]
mod mem_mark {
    #[op(coerce, name = "__lumenMemMark")]
    pub fn mem_mark(label: String) {
        lumen::memstats::phase(&format!("    glue {label}"));
    }
}

/// Installs a `lumen_bind` module into a realm (see [`Extension::modules`]).
pub type ModuleInit = fn(&mut Ctx) -> Result<(), Value>;

/// [`ModuleInit`]: everything `#[lumen_bind::module]` `M` declares, as globals.
pub fn globals<M: lumen_bind::Module<JsHost>>(ctx: &mut Ctx) -> Result<(), Value> {
    let g = ctx.global_object();
    ctx.install_module::<M>(&g)
}

/// [`ModuleInit`]: everything `M` declares as lazy globals: each name is an accessor on the
/// global object that builds the real class or function on first access and replaces itself
/// with a data property (see `Ctx::install_module_lazy`). A realm that already defines a name
/// keeps it.
pub fn lazy_globals<M: lumen_bind::Module<JsHost>>(ctx: &mut Ctx) -> Result<(), Value> {
    ctx.install_module_lazy::<M>()
}

/// [`ModuleInit`]: `globalThis.<module name>` holding everything `M` declares.
pub fn namespace<M: lumen_bind::Module<JsHost>>(ctx: &mut Ctx) -> Result<(), Value> {
    let ns = ctx.namespace_object(M::DESC.name_for("js"));
    ctx.install_module::<M>(&ns)
}

/// A bundle of native ops + host-state init, exported one per op crate. Composing a runtime
/// is `install(&mut engine, &[timers::extension(), fs::extension(), ...])`.
pub struct Extension {
    pub name: &'static str,
    /// `lumen_bind` modules to install ([`globals`] / [`namespace`]).
    pub modules: &'static [ModuleInit],
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
    /// Globals the glue defines and nothing else (no side effects other pieces rely on). When
    /// non-empty, `install` / `install_realm_in_ctx` do not run the glue at boot: each name is
    /// published as a lazy global instead, and the glue (`js_init` / `js_init_snapshot`, the
    /// same sources as the eager path) runs once, in the realm that owns the global, on the
    /// first access to any listed name. Reflection reports ordinary data properties because the
    /// engine materializes a lazy global before `Object.getOwnPropertyDescriptor`,
    /// `Object.keys` and friends see it, and the accessors are replaced by the real properties
    /// the glue defines. The glue must therefore:
    ///
    /// - define every listed name on `globalThis` (it may read other globals, which may be lazy
    ///   themselves);
    /// - not read a listed name before defining it (the accessors are already gone, so it reads
    ///   `undefined`);
    /// - keep a data property a script put on a listed name beforehand (assigning to a lazy
    ///   name replaces only that accessor with the assigned value and does not run the glue).
    ///
    /// A name the realm already defines is skipped. `in` does not trigger the glue. A glue
    /// that throws surfaces the error at the first access, and the names are then gone.
    /// Empty (the default) keeps the eager behavior.
    pub lazy_globals: &'static [&'static str],
}

/// Records which extensions initialized their shared OpState during ordinary installation.
/// Host-realm installation checks this marker and never invokes `state_init` a second time.
#[derive(Default)]
struct InitializedExtensionStates(HashSet<&'static str>);

/// Exact static source/snapshot pairs shared by host realms in one engine. This retains Rust
/// source text only, never JS values or realm handles.
#[derive(Default)]
struct SharedExtensionSources(HashMap<ExtensionSourceKey, Rc<str>>);

/// A Ctx-only host initializer that installs providers into one actual registered realm.
/// Initial document creation can use this without borrowing the owning `Engine` or `Runtime`.
pub type HostRealmInstaller = Rc<dyn Fn(&mut Ctx, &RealmHandle) -> Result<(), String>>;

#[derive(Default)]
struct HostRealmInstallers(Vec<HostRealmInstaller>);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ExtensionSourceKey {
    source_address: usize,
    source_len: usize,
    snapshot_address: Option<usize>,
    snapshot_len: usize,
}

const MAX_SHARED_EXTENSION_SOURCES: usize = 64;

fn shared_extension_source(ctx: &mut Ctx, extension: &Extension, source: &'static str) -> Rc<str> {
    let (snapshot_address, snapshot_len) = extension
        .js_init_snapshot
        .map(|snapshot| (Some(snapshot.as_ptr() as usize), snapshot.len()))
        .unwrap_or((None, 0));
    let key = ExtensionSourceKey {
        source_address: source.as_ptr() as usize,
        source_len: source.len(),
        snapshot_address,
        snapshot_len,
    };
    let state = ctx.op_state();
    if !state.has::<SharedExtensionSources>() {
        state.put(SharedExtensionSources::default());
    }
    let cache = state
        .get_mut::<SharedExtensionSources>()
        .expect("source cache was just installed");
    if let Some(source) = cache.0.get(&key) {
        return source.clone();
    }
    let shared = Rc::<str>::from(source);
    if cache.0.len() < MAX_SHARED_EXTENSION_SOURCES {
        cache.0.insert(key, shared.clone());
    }
    shared
}

impl Extension {
    /// An empty extension named `name`; fill in the fields that apply.
    pub const fn new(name: &'static str) -> Extension {
        Extension {
            name,
            modules: &[],
            state_init: None,
            js_init: None,
            js_init_snapshot: None,
            lazy_globals: &[],
        }
    }

    /// The glue-only view of this extension that a lazy global group runs on first access.
    fn glue(&self) -> Extension {
        Extension {
            name: self.name,
            modules: &[],
            state_init: None,
            js_init: self.js_init,
            js_init_snapshot: self.js_init_snapshot,
            lazy_globals: &[],
        }
    }

    fn defers_glue(&self) -> bool {
        !self.lazy_globals.is_empty() && (self.js_init.is_some() || self.js_init_snapshot.is_some())
    }
}

/// Publish `extension.lazy_globals` in the active realm; the glue runs on first access.
fn defer_glue(ctx: &mut Ctx, extension: &Extension) {
    let glue = extension.glue();
    ctx.install_lazy_global_group(
        extension.lazy_globals,
        Rc::new(move |ctx, global| {
            let realm = ctx
                .host_realm_for_global(global)
                .ok_or_else(|| ctx.make_error("Error", "lazy extension realm is unavailable"))?;
            install_realm_glue(ctx, &realm, &glue).map_err(|message| ctx.make_error("Error", message))
        }),
    );
}

/// Install extensions into an engine: state first (an op may fire during install), then ops,
/// then JS glue.
pub fn install(engine: &mut Engine, extensions: &[Extension]) {
    let timing = startup_timing();
    lumen::memstats::phase("engine created");
    if lumen::memstats::enabled() {
        // Per-file checkpoints of a glue built with LUMEN_GLUE_MEM_MARKS=1 (see lumen-node's
        // build.rs).
        if globals::<mem_mark::Module>(engine.ctx()).is_err() {
            panic!("installing the glue memory marks threw");
        }
    }
    for ext in extensions {
        let _mem = lumen::memstats::enter(lumen::memstats::Cat::Glue);
        let t0 = timing.then(crate::time::Instant::now);
        if let Some(init) = ext.state_init {
            init(engine.ctx().op_state());
            let state = engine.ctx().op_state();
            if !state.has::<InitializedExtensionStates>() {
                state.put(InitializedExtensionStates::default());
            }
            state
                .get_mut::<InitializedExtensionStates>()
                .expect("initialized extension state marker was just installed")
                .0
                .insert(ext.name);
        }
        for m in ext.modules {
            if m(engine.ctx()).is_err() {
                panic!("extension {}: installing a module threw", ext.name);
            }
        }
        if ext.defers_glue() {
            defer_glue(engine.ctx(), ext);
            continue;
        }
        let aot = ext.js_init_snapshot.filter(|b| b.starts_with(b"LUMENAOT"));
        if let Some(blob) = aot {
            let tier = engine.tier();
            engine.set_tier(lumen::bytecode::Tier::Interp);
            let loaded = load_glue(engine, blob);
            let completion = match (loaded, ext.js_init) {
                #[cfg(feature = "compiler")]
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
            #[cfg(not(feature = "compiler"))]
            panic!(
                "extension '{}' requires native initialization glue",
                ext.name
            );
            #[cfg(feature = "compiler")]
            {
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
        }
        if let Some(t0) = t0 {
            eprintln!("[startup] install {:<8} {:?}", ext.name, t0.elapsed());
        }
        if lumen::memstats::enabled() {
            lumen::memstats::phase(&format!("install {}", ext.name));
        }
    }
}

/// Install already-initialized extensions into another registered realm of the same engine.
/// Native state remains shared through OpState; only modules, globals, namespaces, and glue are
/// installed in the target realm. In particular, this function never calls `state_init`.
pub fn install_realm(
    engine: &mut Engine,
    realm: &RealmHandle,
    extensions: &[Extension],
) -> Result<(), String> {
    install_realm_in_ctx(engine.ctx(), realm, extensions)
}

/// Install already-initialized extensions from a native host callback with only the shared
/// interpreter context. The realm is validated by `with_host_realm`; no engine/runtime alias or
/// agent checkpoint is needed.
pub fn install_realm_in_ctx(
    ctx: &mut Ctx,
    realm: &RealmHandle,
    extensions: &[Extension],
) -> Result<(), String> {
    if extensions
        .iter()
        .any(|extension| extension.state_init.is_some())
    {
        let state = ctx.op_state();
        let initialized = state
            .get::<InitializedExtensionStates>()
            .ok_or_else(|| "base extension state has not been initialized".to_string())?;
        for extension in extensions {
            if extension.state_init.is_some() && !initialized.0.contains(extension.name) {
                return Err(format!(
                    "extension '{}' has not initialized shared host state",
                    extension.name
                ));
            }
        }
    }

    for extension in extensions {
        let installed = ctx
            .with_host_realm(realm, |ctx| {
                for module in extension.modules {
                    module(ctx).map_err(|_| {
                        format!("extension '{}' module installation threw", extension.name)
                    })?;
                }
                Ok::<(), String>(())
            })
            .map_err(|error| error.to_string())?;
        installed?;

        if extension.defers_glue() {
            ctx.with_host_realm(realm, |ctx| defer_glue(ctx, extension))
                .map_err(|error| error.to_string())?;
            continue;
        }
        install_realm_glue(ctx, realm, extension)?;
    }
    Ok(())
}

/// Register one browser/host realm initializer in shared OpState. The closure is cloned out of
/// OpState before invocation, so it may safely call other native providers that consult OpState.
pub fn register_host_realm_installer(ctx: &mut Ctx, installer: HostRealmInstaller) {
    if !ctx.op_state().has::<HostRealmInstallers>() {
        ctx.op_state().put(HostRealmInstallers::default());
    }
    ctx.op_state()
        .get_mut::<HostRealmInstallers>()
        .expect("host realm installer list was just installed")
        .0
        .push(installer);
}

/// Run the registered host-realm initializers synchronously in `realm`. No JavaScript job or
/// timer checkpoint is performed; the Runtime remains the sole owner of event-loop ordering.
pub fn install_registered_host_realm(ctx: &mut Ctx, realm: &RealmHandle) -> Result<(), String> {
    let installers = ctx
        .op_state()
        .get::<HostRealmInstallers>()
        .map(|installers| installers.0.clone())
        .unwrap_or_default();
    for installer in installers {
        installer(ctx, realm)?;
    }
    Ok(())
}

fn install_realm_glue(
    ctx: &mut Ctx,
    realm: &RealmHandle,
    extension: &Extension,
) -> Result<(), String> {
    let snapshot = extension.js_init_snapshot;
    let shared_source = extension
        .js_init
        .map(|source| shared_extension_source(ctx, extension, source));
    let native_version = lumen_common::aot::NATIVE_FORMAT_VERSION.to_le_bytes();
    let native = snapshot.filter(|bytes| bytes.get(8..12) == Some(native_version.as_slice()));

    if let Some(blob) = native {
        #[cfg(feature = "aot-native")]
        {
            let loaded = ctx.with_host_bootstrap_tier(|ctx| {
                ctx.load_native_glue_value_in_host_realm(realm, blob)
            });
            let loaded = loaded.map_err(|error| error.to_string())?;
            match loaded {
                Ok(_) => return Ok(()),
                Err(error) => {
                    return Err(format!(
                        "extension '{}' native glue failed: {error}",
                        extension.name
                    ));
                }
            }
        }
        #[cfg(not(feature = "aot-native"))]
        {
            #[cfg(feature = "compiler")]
            if let Some(source) = shared_source.as_deref() {
                return run_realm_source(ctx, realm, extension.name, source);
            }
            return Err(format!(
                "extension '{}' native glue is unavailable",
                extension.name
            ));
        }
    }

    let Some(source) = shared_source.as_deref() else {
        if snapshot.is_some() {
            return Err(format!(
                "extension '{}' has portable snapshot glue but no source for host-realm decoding",
                extension.name
            ));
        }
        return Ok(());
    };

    #[cfg(feature = "compiler")]
    {
        if std::env::var_os("LUMEN_NO_SNAPSHOT").is_none() {
            if let Some(snapshot) = snapshot.filter(|bytes| bytes.starts_with(b"LUMENAOT")) {
                let result = ctx.with_host_bootstrap_tier(|ctx| {
                    ctx.eval_snapshot_shared_source_in_host_realm(
                        realm,
                        snapshot,
                        shared_source
                            .as_ref()
                            .expect("shared source was checked above")
                            .clone(),
                        false,
                    )
                });
                match result {
                    Ok(Ok(_)) => return Ok(()),
                    Ok(Err(thrown)) => {
                        return Err(describe_realm_throw(ctx, extension.name, thrown));
                    }
                    Err(HostRealmEvalError::Scope(error)) => return Err(error.to_string()),
                    // A stale/corrupt snapshot follows the existing extension contract and
                    // falls back to the source used to build it.
                    Err(HostRealmEvalError::Parse(_)) => {}
                }
            }
        }
        run_realm_source(ctx, realm, extension.name, source)
    }
    #[cfg(not(feature = "compiler"))]
    {
        let _ = (ctx, realm, source);
        Err(format!(
            "extension '{}' requires native initialization glue",
            extension.name
        ))
    }
}

#[cfg(feature = "compiler")]
fn run_realm_source(
    ctx: &mut Ctx,
    realm: &RealmHandle,
    extension: &str,
    source: &str,
) -> Result<(), String> {
    let result =
        ctx.with_host_bootstrap_tier(|ctx| ctx.eval_value_in_host_realm(realm, source, false));
    match result {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(thrown)) => Err(describe_realm_throw(ctx, extension, thrown)),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(not(feature = "compiler"))]
fn run_realm_source(
    _ctx: &mut Ctx,
    _realm: &RealmHandle,
    extension: &str,
    _source: &str,
) -> Result<(), String> {
    Err(format!(
        "extension '{}' requires native initialization glue",
        extension
    ))
}

fn describe_realm_throw(ctx: &mut Ctx, extension: &str, thrown: Value) -> String {
    let name = host_error_property(ctx, &thrown, "name").or_else(|| {
        let constructor = ctx.member_get(&thrown, "constructor").ok()?;
        host_error_property(ctx, &constructor, "name")
    });
    let message = if thrown.as_obj().is_some() {
        host_error_property(ctx, &thrown, "message").unwrap_or_default()
    } else {
        ctx.coerce_string(&thrown)
            .map(|value| value.to_string())
            .unwrap_or_default()
    };
    format!(
        "extension '{extension}' js_init threw {}: {message}",
        name.unwrap_or_default()
    )
}

fn host_error_property(ctx: &mut Ctx, object: &Value, name: &str) -> Option<String> {
    let value = ctx.member_get(object, name).ok()?;
    if matches!(value, Value::Undefined | Value::Null) {
        return None;
    }
    ctx.coerce_string(&value)
        .ok()
        .map(|value| value.to_string())
}

/// Load build-produced extension glue in its recorded execution format.
pub fn load_glue(engine: &mut Engine, blob: &'static [u8]) -> Result<Completion, ParseError> {
    if blob.get(8..12) == Some(&lumen_common::aot::NATIVE_FORMAT_VERSION.to_le_bytes()[..]) {
        #[cfg(feature = "aot-native")]
        return engine
            .load_native_glue_value(blob)
            .map(|_| Completion::Value(String::new()))
            .map_err(|message| ParseError {
                message,
                line: 1,
                at_eof: false,
            });
        #[cfg(not(feature = "aot-native"))]
        return Err(ParseError {
            message: "native extension glue is unavailable in this runtime".into(),
            line: 1,
            at_eof: false,
        });
    }
    #[cfg(feature = "compiler")]
    {
        engine.load_precompiled(&lumen::Precompiled::from_static(blob))
    }
    #[cfg(not(feature = "compiler"))]
    {
        Err(ParseError {
            message: "Aot initialization requires native glue".into(),
            line: 1,
            at_eof: false,
        })
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
    shared: std::sync::Arc<PoolShared>,
}

/// State shared by the pool and its spawn handles. Worker threads are created by the first
/// submission, so a runtime that never runs blocking work never starts any.
struct PoolShared {
    size: usize,
    completions: mpsc::Sender<TaskCompletion>,
    /// Tasks submitted and not yet finished (queued or running).
    pending: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    state: std::sync::Mutex<PoolState>,
}

enum PoolState {
    Idle,
    Running {
        work_tx: mpsc::Sender<Task>,
        workers: Vec<std::thread::JoinHandle<()>>,
    },
    Closed,
}

/// Spawn a host thread with the engine's thread stack size: work run here may call back into an
/// engine (or run one), whose recursion is sized for it.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_thread<F: FnOnce() + Send + 'static>(f: F) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .stack_size(lumen::THREAD_STACK_SIZE)
        .spawn(move || {
            lumen::set_thread_stack_size(lumen::THREAD_STACK_SIZE);
            f()
        })
        .expect("spawn host thread")
}

impl ThreadPool {
    /// Up to `size` worker threads, started on the first submitted job, sending
    /// [`TaskCompletion`]s to `completions` (the loop thread holds the receiving end).
    ///
    /// On `wasm32` there are no threads: the pool has no workers and runs each job inline on the
    /// calling (loop) thread, delivering its completion on the same channel.
    pub fn new(size: usize, completions: mpsc::Sender<TaskCompletion>) -> ThreadPool {
        ThreadPool {
            shared: std::sync::Arc::new(PoolShared {
                size,
                completions,
                pending: Default::default(),
                state: std::sync::Mutex::new(PoolState::Idle),
            }),
        }
    }

    /// Run `work` on a pool thread; its return value comes back to the loop as a
    /// [`TaskCompletion`] tagged with `id`.
    pub fn spawn_blocking(
        &self,
        id: TaskId,
        work: impl FnOnce() -> Box<dyn Any + Send> + Send + 'static,
    ) {
        self.handle().spawn_blocking(id, work);
    }

    /// A cloneable spawn handle. The runtime puts one in [`OpState`], which is how a native fn
    /// (holding only `&mut Ctx`) reaches the pool.
    pub fn handle(&self) -> SpawnHandle {
        SpawnHandle {
            shared: std::sync::Arc::clone(&self.shared),
        }
    }

    /// Whether any worker thread has been started.
    pub fn started(&self) -> bool {
        matches!(
            *self.shared.state.lock().expect("pool state poisoned"),
            PoolState::Running { .. }
        )
    }
}

impl PoolShared {
    #[cfg(not(target_arch = "wasm32"))]
    fn start(&self) -> (mpsc::Sender<Task>, Vec<std::thread::JoinHandle<()>>) {
        let (work_tx, work_rx) = mpsc::channel::<Task>();
        // std's mpsc receiver is single-consumer: share it across workers behind a mutex.
        let work_rx = std::sync::Arc::new(std::sync::Mutex::new(work_rx));
        let workers = (0..self.size.max(1))
            .map(|_| {
                let work_rx = std::sync::Arc::clone(&work_rx);
                let completions = self.completions.clone();
                let pending = std::sync::Arc::clone(&self.pending);
                spawn_thread(move || loop {
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
                    pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                })
            })
            .collect();
        (work_tx, workers)
    }

    /// Queue `task`, starting the workers if this is the first submission. `Err` hands the task
    /// back when the pool is closed or has no threads (`wasm32`).
    fn submit(&self, task: Task) -> Result<(), Task> {
        #[cfg(target_arch = "wasm32")]
        {
            return Err(task);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut state = self.state.lock().expect("pool state poisoned");
            if matches!(*state, PoolState::Idle) {
                let (work_tx, workers) = self.start();
                *state = PoolState::Running { work_tx, workers };
            }
            match &*state {
                PoolState::Running { work_tx, .. } => {
                    self.pending
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    work_tx.send(task).expect("worker threads gone");
                    Ok(())
                }
                _ => Err(task),
            }
        }
    }
}

/// [`ThreadPool::spawn_blocking`] as an [`OpState`]-storable handle, so op crates can spawn
/// blocking work from inside a native fn.
#[derive(Clone)]
pub struct SpawnHandle {
    shared: std::sync::Arc<PoolShared>,
}

impl SpawnHandle {
    pub fn spawn_blocking(
        &self,
        id: TaskId,
        work: impl FnOnce() -> Box<dyn Any + Send> + Send + 'static,
    ) {
        let task = Task::Tracked {
            id,
            work: Box::new(work),
        };
        if let Err(Task::Tracked { id, work }) = self.shared.submit(task) {
            if cfg!(target_arch = "wasm32") {
                let result = work();
                let _ = self
                    .shared
                    .completions
                    .send(TaskCompletion { task: id, result });
            }
        }
    }

    /// Run `job` on a pool thread; it reports back by itself (e.g. through a
    /// [`lumen::embed::Completer`]), so no completion is sent for it.
    pub fn spawn_detached(&self, job: Box<dyn FnOnce() + Send>) {
        if let Err(Task::Detached(job)) = self.shared.submit(Task::Detached(job)) {
            if cfg!(target_arch = "wasm32") {
                job();
            }
        }
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
    wake: Option<std::sync::Arc<owner_loop::Wake>>,
}

impl CompletionSender {
    pub fn new(tx: mpsc::Sender<TaskCompletion>) -> CompletionSender {
        CompletionSender { tx, wake: None }
    }

    /// A sender for a loop that is driven by its host's turns ([`owner_loop`]): every completion
    /// is counted, and `notify` runs on the transition from nothing ready to something ready, so a
    /// burst of sends wakes the host once. `notify` runs on the sending thread and must not block.
    pub fn with_notify(
        tx: mpsc::Sender<TaskCompletion>,
        notify: std::sync::Arc<dyn Fn() + Send + Sync>,
    ) -> CompletionSender {
        CompletionSender {
            tx,
            wake: Some(std::sync::Arc::new(owner_loop::Wake::new(notify))),
        }
    }

    pub(crate) fn wake_handle(&self) -> Option<std::sync::Arc<owner_loop::Wake>> {
        self.wake.clone()
    }

    /// Run `work` on a new dedicated thread; its result comes back to the loop as a
    /// [`TaskCompletion`] tagged with `id` (settled through the [`TaskRegistry`], like pool work).
    ///
    /// Not available on an [`owner_loop`] sender: the owner loop never spawns threads.
    pub fn run_blocking(
        &self,
        id: TaskId,
        work: impl FnOnce() -> Box<dyn Any + Send> + Send + 'static,
    ) {
        debug_assert!(
            self.wake.is_none(),
            "run_blocking is not supported on an owner-loop sender"
        );
        let tx = self.tx.clone();
        let run = move || {
            let result = work();
            let _ = tx.send(TaskCompletion { task: id, result });
        };
        #[cfg(target_arch = "wasm32")]
        run();
        #[cfg(not(target_arch = "wasm32"))]
        spawn_thread(run);
    }

    /// Deliver a completion from any thread (an I/O callback, a readiness thread).
    pub fn send(&self, id: TaskId, result: Box<dyn Any + Send>) {
        match &self.wake {
            None => {
                let _ = self.tx.send(TaskCompletion { task: id, result });
            }
            Some(wake) => {
                // Count before queueing so the receiver never sees a message the counter does not
                // cover; notify after queueing so the woken host finds it.
                let first = wake.arrive();
                if self.tx.send(TaskCompletion { task: id, result }).is_ok() && first {
                    wake.notify();
                }
            }
        }
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
        let id = crate::register_task(ctx, resolve, Some(reject), decode_settle);
        let completions = self.completions.clone();
        lumen::embed::Completer::new(move |settle| completions.send(id, Box::new(settle)))
    }

    fn spawn(&self, job: Box<dyn FnOnce() + Send>, dedicated: bool) {
        #[cfg(not(target_arch = "wasm32"))]
        if dedicated {
            spawn_thread(job);
            return;
        }
        #[cfg(target_arch = "wasm32")]
        let _ = dedicated;
        self.spawn.spawn_detached(job);
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
    unref_count: usize,
}

/// How to settle one in-flight task: success callback, optional failure callback (a promise's
/// reject — when absent, a decode error is reported as an uncaught exception), and the
/// payload decoder.
pub struct TaskEntry {
    pub on_ok: Value,
    pub on_err: Option<Value>,
    pub decode: TaskDecoder,
    /// Immutable async-context frame captured when the native operation was admitted.
    pub context: Value,
    /// Realm that admitted a task through [`register_task`]. Direct registry users may leave
    /// this unset when the work belongs to a host agent rather than one browsing context.
    pub owner: Option<RealmHandle>,
    /// An `unref`'d task still settles when it completes, but does not by itself keep the event
    /// loop alive (Node's `child.unref()` — e.g. esbuild's persistent service child).
    pub unref: bool,
    /// A stream entry survives its completions (a WebSocket's events); it is removed with
    /// [`TaskRegistry::cancel`].
    pub persistent: bool,
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
                context: Value::Undefined,
                owner: None,
                unref: false,
                persistent: false,
            },
        );
        id
    }
    /// Reserve an id whose callback fires for every completion until [`cancel`](Self::cancel)led.
    pub fn register_stream(&mut self, on_event: Value, decode: TaskDecoder) -> TaskId {
        let id = self.register(on_event, None, decode);
        if let Some(e) = self.map.get_mut(&id) {
            e.persistent = true;
        }
        id
    }
    /// Claim a completed task's settlement entry (a missing id means it was cancelled). A
    /// stream entry is copied, not removed.
    pub fn take(&mut self, id: TaskId) -> Option<TaskEntry> {
        if let Some(e) = self.map.get(&id) {
            if e.persistent {
                return Some(TaskEntry {
                    on_ok: e.on_ok.clone(),
                    on_err: e.on_err.clone(),
                    decode: e.decode,
                    context: e.context.clone(),
                    owner: e.owner.clone(),
                    unref: e.unref,
                    persistent: true,
                });
            }
        }
        let entry = self.map.remove(&id);
        if entry.as_ref().is_some_and(|e| e.unref) {
            self.unref_count -= 1;
        }
        self.release_idle_capacity();
        entry
    }
    fn release_idle_capacity(&mut self) {
        if self.map.is_empty() {
            if self.map.capacity() > 64 {
                self.map = std::collections::HashMap::new();
            }
        } else if self.map.capacity() > 256 && self.map.len() * 4 < self.map.capacity() {
            self.map.shrink_to(self.map.len().max(64) * 2);
        }
    }
    /// Drop a pending task or stream; late completions for it are ignored.
    pub fn cancel(&mut self, id: TaskId) {
        if self.map.remove(&id).is_some_and(|e| e.unref) {
            self.unref_count -= 1;
        }
        self.release_idle_capacity();
    }
    /// Drop settlement entries admitted by `realm`. Blocking work may still complete later; its
    /// completion is ignored because the corresponding task ID is no longer registered.
    pub fn cancel_realm(&mut self, realm: &RealmHandle) -> usize {
        let before = self.map.len();
        let mut unref_removed = 0;
        self.map.retain(|_, entry| {
            let keep = entry
                .owner
                .as_ref()
                .map_or(true, |owner| !owner.same_realm(realm));
            if !keep && entry.unref {
                unref_removed += 1;
            }
            keep
        });
        self.unref_count -= unref_removed;
        self.release_idle_capacity();
        before - self.map.len()
    }
    /// Number of pending settlement entries admitted by `realm`.
    pub fn pending_for_realm(&self, realm: &RealmHandle) -> usize {
        self.map
            .values()
            .filter(|entry| {
                entry
                    .owner
                    .as_ref()
                    .is_some_and(|owner| owner.same_realm(realm))
            })
            .count()
    }
    /// Mark a pending task as `unref`'d (see [`TaskEntry::unref`]).
    pub fn set_unref(&mut self, id: TaskId) {
        if let Some(e) = self.map.get_mut(&id) {
            if !e.unref {
                self.unref_count += 1;
            }
            e.unref = true;
        }
    }
    /// Re-`ref` a pending task so it keeps the loop alive again (Node's `handle.ref()`).
    pub fn set_ref(&mut self, id: TaskId) {
        if let Some(e) = self.map.get_mut(&id) {
            if e.unref {
                self.unref_count -= 1;
            }
            e.unref = false;
        }
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    /// Whether any *ref*'d (loop-keeping) task is pending. Unref'd tasks are ignored — they
    /// settle if they complete but must not hold the process open.
    pub fn has_ref_pending(&self) -> bool {
        self.map.len() > self.unref_count
    }
}

/// Register native I/O with its admitting async context. Raw callbacks, decoders and promise
/// settlement all run inside this frame; reactions retain their own captured context as usual.
pub fn register_task(
    ctx: &mut Ctx,
    on_ok: Value,
    on_err: Option<Value>,
    decode: TaskDecoder,
) -> TaskId {
    let owner = ctx.current_host_realm();
    let context = ctx.async_context();
    let registry = ctx
        .host_mut::<TaskRegistry>()
        .expect("runtime task registry");
    let id = registry.register(on_ok, on_err, decode);
    let entry = registry.map.get_mut(&id).unwrap();
    entry.context = context;
    entry.owner = Some(owner);
    id
}

/// How long dropping a pool waits for tasks already running before it stops waiting.
const POOL_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

impl Drop for ThreadPool {
    fn drop(&mut self) {
        // Closing the work channel ends each worker's recv loop. Normally the workers are joined
        // so none outlives the runtime that owns the completion receiver; a task stuck in a
        // blocking call (a read that never returns) must not hold the drop hostage, so after a
        // grace period the workers are detached and exit when that call does.
        let state = std::mem::replace(
            &mut *self.shared.state.lock().expect("pool state poisoned"),
            PoolState::Closed,
        );
        let PoolState::Running { work_tx, workers } = state else {
            return;
        };
        drop(work_tx);
        let pending = &self.shared.pending;
        let give_up = std::time::Instant::now() + POOL_DRAIN_GRACE;
        while pending.load(std::sync::atomic::Ordering::SeqCst) > 0
            && std::time::Instant::now() < give_up
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let stuck = pending.load(std::sync::atomic::Ordering::SeqCst) > 0;
        for w in workers {
            if !stuck || w.is_finished() {
                let _ = w.join();
            }
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
    /// `process.execPath`. `child_process` starts a program with exactly this path as a child
    /// realm (see [`RealmLauncher`]) instead of an OS process: nothing on disk runs a realm.
    pub exec_path: String,
    pub launcher: Option<std::sync::Arc<dyn RealmLauncher>>,
    /// Descriptors the host owns and closes when the realm ends (a child realm's IPC channel):
    /// the program closing one only shuts the connection down.
    pub owned_fds: Vec<i32>,
    /// Bit `n` is set while the program has a `process.on('SIG…')` listener for signal number
    /// `n`; read by the launching realm's threads to choose between delivering a signal to the
    /// program and its default action.
    pub signal_handlers: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// Stand-in pids of child realms start here: far above any OS pid, so signalling one by number
/// never reaches a real process.
pub const REALM_PID_BASE: u32 = 1 << 30;

/// What a child realm runs with: the same things an OS process gets from `spawn`.
pub struct ChildRealmRequest {
    /// `process.argv`: `[execPath, script, ...args]`.
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: std::path::PathBuf,
    pub stdin: Box<dyn std::io::Read + Send>,
    pub stdout: Box<dyn std::io::Write + Send>,
    pub stderr: Box<dyn std::io::Write + Send>,
    /// The realm's stop request, shared with whoever feeds its stdin so a blocked read can give
    /// up when the realm is stopped.
    pub interrupt: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Descriptors in `resources` the program may name (see [`RealmProcess::owned_fds`]).
    pub owned_fds: Vec<i32>,
    /// Kept alive for as long as the realm runs and dropped after it ends (the realm's end of an
    /// IPC channel, say).
    pub resources: Vec<Box<dyn Send>>,
    /// Optional packet transport supplied by an embedder without OS descriptors.
    pub ipc: Option<std::sync::Arc<dyn RealmIpc>>,
}

/// An embedder's bounded, duplex child-realm packet channel.
pub trait RealmIpc: Send + Sync {
    fn send(&self, bytes: Vec<u8>) -> std::io::Result<()>;
    fn receive(&self) -> std::io::Result<Option<Vec<u8>>>;
    fn disconnect(&self);
    fn connected(&self) -> bool;
}

/// How a child realm ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildRealmExit {
    Exited(i32),
    /// Stopped by [`ChildRealm::terminate`] with this signal number.
    Signalled(i32),
}

/// A running child realm.
pub trait ChildRealm: Send + Sync {
    /// The stand-in pid (at least [`REALM_PID_BASE`]).
    fn pid(&self) -> u32;
    /// Stop the realm at once; it then ends as [`ChildRealmExit::Signalled`] with `signal`. No
    /// effect once it has ended.
    fn terminate(&self, signal: i32);
    /// Deliver `signal` as an OS would: `SIGKILL` stops the realm at once; any other signal
    /// reaches the program's `process.on` listeners when it has some, else takes its default
    /// action (terminate for `SIGHUP`, `SIGINT`, `SIGQUIT` and `SIGTERM`, ignore the rest).
    fn signal(&self, signal: i32);
    /// `None` while the realm runs.
    fn exit(&self) -> Option<ChildRealmExit>;
}

/// Starts child realms on threads of their own, for `child_process` when the program spawns its
/// own `process.execPath`. An embedder's realm tree owns what it launches: ending the parent
/// stops and joins its children.
pub trait RealmLauncher: Send + Sync {
    /// Fails with `WouldBlock` when the realm tree already runs as many realms as it may.
    fn launch(&self, request: ChildRealmRequest)
        -> std::io::Result<std::sync::Arc<dyn ChildRealm>>;
    /// Deliver `signal` to the live child realm with stand-in pid `pid`; `false` when there is
    /// none.
    fn signal_pid(&self, pid: u32, signal: i32) -> bool;
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
        None => {
            #[cfg(unix)]
            wait_stdin_readable(ctx)?;
            std::io::stdin().read(buf)
        }
    }
}

/// Block until fd 0 has data (or hangs up), watching the realm's interrupt: a `read` on a pipe
/// nobody writes to cannot be cancelled, so it is only entered once it will not block.
#[cfg(unix)]
fn wait_stdin_readable(ctx: &mut Ctx) -> std::io::Result<()> {
    use lumen_os::poll::{poll, PollFd, POLLIN};
    if ctx.interrupt_for_host().is_none() {
        return Ok(());
    }
    loop {
        if ctx.poll_interrupt_for_host().is_err() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "realm terminated",
            ));
        }
        // An error (EINTR, EBADF) falls through to the read, which reports it.
        if poll(&mut [PollFd::new(0, POLLIN)], 20) != Ok(0) {
            return Ok(());
        }
    }
}

/// Write to fd 1 or fd 2: the realm's writers when embedded, the process's otherwise.
pub fn write_std_fd(ctx: &mut Ctx, fd: u32, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let realm = ctx.op_state().get::<RealmProcess>().map(|realm| {
        std::sync::Arc::clone(if fd == 2 {
            &realm.stderr
        } else {
            &realm.stdout
        })
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
#[cfg(not(target_arch = "wasm32"))]
pub fn canonicalize(path: impl AsRef<std::path::Path>) -> std::io::Result<std::path::PathBuf> {
    std::fs::canonicalize(path).map(strip_verbatim)
}

#[cfg(target_arch = "wasm32")]
pub fn canonicalize(path: impl AsRef<std::path::Path>) -> std::io::Result<std::path::PathBuf> {
    use lumen_os::vfs::FileSystem;
    lumen_os::vfs::mem()
        .realpath(&path.as_ref().to_string_lossy())
        .map(std::path::PathBuf::from)
        .map_err(std::io::Error::from)
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
