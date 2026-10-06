//! Workers — realm-per-thread with structured messaging. Backs the Web `Worker`, `SharedWorker`
//! and `node:worker_threads` (the node glue's Worker class drives the dedicated-worker ops in
//! "node mode"). Shared workers are process-registered by URL, origin, type, and name; each
//! parent runtime retains a separate native MessagePort endpoint.
//!
//! A `Worker` is a dedicated OS thread running its OWN [`Runtime`] (a fresh realm: its own global,
//! intrinsics, event loop, and thread pool). Because engine `Value`s are `!Send`, messages cross
//! the thread boundary as structured-clone wire bytes over the native port endpoints of
//! `lumen_host::ports`; the receiver deserializes in its own realm.
//!
//! ## Web workers
//! The classes live in `lumen_host::workers`: `Worker` / `SharedWorker` in the parent, the worker
//! global scope in the worker. This module is the [`WorkerBackend`] they run over (thread, flags,
//! shared-worker registry) and the [`WorkerScopeHost`] a worker realm hands to
//! `lumen_host::workers::install_scope`. A dedicated worker's messages travel on the implicit
//! port pair; errors and the exit travel on the page object's `Control`, which this module feeds
//! straight from the worker thread. No thread blocks on a receive: every wake is a completion of
//! the receiving realm's loop.
//!
//! ## Node mode (`spawn` options `{ node: true, ... }`)
//! - The worker realm gets the `node:worker_threads` bootstrap (parentPort, workerData, threadId,
//!   patched process.argv/env/exit) instead of the DedicatedWorkerGlobalScope surface.
//! - `online` is announced before the entry runs; `exit` carries a real exit code (0 natural,
//!   `process.exit(code)`'s code, 1 on terminate/uncaught — Node's codes).
//! - The worker→main inbox is *unref'd* until `parentPort` gains a `'message'` listener, so a
//!   worker with nothing pending exits naturally with code 0 (Node's port-ref semantics).
//! - A non-`.mjs` file entry runs through the node CJS loader as the realm's main module
//!   (`require.main === module`, `__filename`/`__dirname` in scope), like Node workers.
//!
//! ## What's intentionally v1
//! - **Cooperative terminate**: `terminate()` (and the worker's `close()`/`process.exit()`) set a
//!   shared stop flag the worker loop polls; a worker stuck in a long *synchronous* JS task
//!   finishes it first (no preemption — the engine has no interrupt point). Pending timers are
//!   dropped on stop. Likewise `process.exit()` in a worker stops at the next loop poll rather
//!   than instantly.
//! - Native capability envelopes share `SharedArrayBuffer` backing and transfer MessagePort
//!   ownership; fixed-length ordinary ArrayBuffers use byte transport and true sender detachment.
//!   Other transferable types/resizable buffers remain explicitly unsupported.
//! - Reuses the full [`Runtime`] per worker (own 4-thread pool). Heavy but correct.

use lumen_bind::{NativeError, NativeResult};
use lumen_host::OpError;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use lumen::embed::JsFunction;
use lumen_host::workers::{
    self, Control, DedicatedSpec, ScopeInstall, ScopeKind, SharedSpec, WorkerBackend, WorkerEvent,
    WorkerScopeHost,
};
use lumen_host::{CompletionSender, Ctx, Extension, TaskId, TaskRegistry, Value};

use crate::Runtime;
use lumen_host::clone_transfer::{self, CloneMessage};
use lumen_host::ports::PortTransfer;

/// Node-visible thread ids: the main thread is 0, workers count up from 1 (process-wide, so ids
/// stay unique even for workers spawned from workers).
static NEXT_THREAD_ID: AtomicU64 = AtomicU64::new(1);

// ---- what a worker reports to its parent --------------------------------------------------------

/// What a worker thread tells whoever started it.
enum ToMain {
    Online,
    OutOfMemory,
    Error(String),
    Exited(i32),
}

/// Where a worker thread's reports go: a node worker's blocking inbox, a page `Worker`'s control,
/// or the clients of a shared worker.
#[derive(Clone)]
enum Parent {
    Node(Sender<ToMain>),
    Web(Control),
    Shared(Weak<SharedWorkerHost>),
}

impl Parent {
    fn send(&self, event: ToMain) {
        match self {
            Parent::Node(tx) => {
                let _ = tx.send(event);
            }
            Parent::Web(control) => match event {
                ToMain::Error(message) => {
                    control.send(WorkerEvent::Error(message));
                }
                ToMain::Exited(code) => {
                    control.send(WorkerEvent::Exit(code));
                }
                ToMain::Online | ToMain::OutOfMemory => {}
            },
            Parent::Shared(host) => {
                if let Some(host) = host.upgrade() {
                    host.route(event);
                }
            }
        }
    }
}

// ---- main side --------------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct WorkerRegistry {
    next: u64,
    workers: HashMap<u64, WorkerEntry>,
    /// The page end of every shared-worker connection this realm holds, closed with the realm.
    shared_clients: HashMap<u64, PortTransfer>,
}

struct WorkerEntry {
    stop: Arc<AtomicBool>,
    /// Set by `terminate()` only: the worker realm's engine interrupt, so running JS (a busy
    /// loop, a microtask or nextTick storm) is stopped at its next safe point, not just the loop.
    kill: Arc<AtomicBool>,
    node: Option<NodeLink>,
}

/// What a node worker's parent keeps: the sender whose drop wakes the worker's loop on terminate,
/// and the `worker_threads` event callback with its loop-task bookkeeping.
struct NodeLink {
    to_worker: Option<Sender<()>>,
    dispatch: Value,
    /// Whether this worker's main-side inbox keeps the main loop alive (`worker.unref()` clears).
    keep_alive: bool,
    /// The currently armed inbox task, so `setRef` can re-mark it in flight.
    inbox_task: Option<TaskId>,
}

type SharedWorkerKey = lumen_common::worker::SharedWorkerKey;

struct SharedClient {
    worker_port: PortTransfer,
    control: Control,
}

struct SharedWorkerHost {
    stop: Arc<AtomicBool>,
    kill: Arc<AtomicBool>,
    /// The worker realm's control: `Connect` for each client, and a wake to stop.
    scope: Control,
    clients: Mutex<HashMap<u64, SharedClient>>,
}

static SHARED_WORKERS: OnceLock<Mutex<HashMap<SharedWorkerKey, Arc<SharedWorkerHost>>>> =
    OnceLock::new();
static NEXT_SHARED_CLIENT: AtomicU64 = AtomicU64::new(1);

fn shared_workers() -> &'static Mutex<HashMap<SharedWorkerKey, Arc<SharedWorkerHost>>> {
    SHARED_WORKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn registry(ctx: &mut Ctx) -> &mut WorkerRegistry {
    ctx.host_mut::<WorkerRegistry>()
        .expect("worker registry installed")
}

impl SharedWorkerHost {
    fn broadcast(&self, message: &str) {
        let controls = self
            .clients
            .lock()
            .unwrap()
            .values()
            .map(|client| client.control.clone())
            .collect::<Vec<_>>();
        for control in controls {
            control.send(WorkerEvent::Error(message.to_owned()));
        }
    }

    /// A report from the worker thread, for every client connected now.
    fn route(self: &Arc<Self>, event: ToMain) {
        match event {
            ToMain::Error(message) => self.broadcast(&message),
            ToMain::OutOfMemory => self.broadcast("shared worker ran out of memory"),
            ToMain::Exited(code) => {
                self.stop.store(true, Ordering::SeqCst);
                self.kill.store(true, Ordering::SeqCst);
                let clients = self
                    .clients
                    .lock()
                    .unwrap()
                    .drain()
                    .map(|(_, client)| client)
                    .collect::<Vec<_>>();
                for client in clients {
                    if code != 0 {
                        client.control.send(WorkerEvent::Error(format!(
                            "shared worker exited with code {code}"
                        )));
                    }
                    client.control.send(WorkerEvent::Close);
                    client.control.close();
                    client.worker_port.close();
                }
                unpublish_shared_worker(self);
            }
            ToMain::Online => {}
        }
    }
}

/// A node worker's `resourceLimits`, as far as they bound its realm.
#[derive(Clone, Copy, Default)]
struct WorkerLimits {
    /// Old-generation ceiling in MiB.
    max_old_mb: Option<f64>,
    stack_mb: Option<f64>,
}

/// Bytes the engine accounts to one live object when it converts a heap ceiling to an object
/// count (see `process.memoryUsage().heapUsed`).
const OBJECT_BYTES: f64 = 128.0;

/// Depth of recursion a worker gets per MiB of `stackSizeMb`; Node's default of 4 MiB maps to
/// the engine's own ceiling.
const DEPTH_PER_STACK_MB: f64 = 1750.0;

/// What an idle worker realm costs outside its object heap (measured as the resident growth per
/// worker): a cap at or below it leaves no room to run anything, as with V8's.
const WORKER_BASE_MB: f64 = 4.0;

impl WorkerLimits {
    fn fits_a_realm(&self) -> bool {
        self.max_old_mb.map_or(true, |mb| mb > WORKER_BASE_MB)
    }

    fn live_object_limit(&self) -> Option<i64> {
        self.max_old_mb
            .map(|mb| ((mb - WORKER_BASE_MB).max(0.0) * 1024.0 * 1024.0 / OBJECT_BYTES) as i64)
    }

    fn max_depth(&self) -> Option<u32> {
        self.stack_mb
            .filter(|mb| *mb > 0.0)
            .map(|mb| (mb * DEPTH_PER_STACK_MB) as u32)
    }
}

/// What the spawned thread needs to build and run the worker realm.
struct WorkerSpec {
    /// The entry: a path, or the source itself when `is_eval`.
    entry: String,
    is_module: bool,
    is_node: bool,
    is_shared: bool,
    is_remote: bool,
    is_eval: bool,
    /// Initial URL used to construct the web worker's immutable `WorkerLocation`.
    location_href: Option<String>,
    name: String,
    /// Structured-clone bytes of `{ workerData, argv, env, envData, entry }` (node mode).
    init: Option<CloneMessage>,
    thread_id: u64,
    /// The parent's embedding, when the parent realm runs inside a host process: a worker must
    /// not fall back to the host's own cwd, stdio and process.
    embedding: Option<crate::WorkerEmbedding>,
    /// Immutable snapshot of the parent's per-runtime HTTP routes and trust roots.
    fetch_config: lumen_web::FetchConfig,
    origin: Option<String>,
    shared_key: Option<SharedWorkerKey>,
    shared_env: Option<crate::process_env::RealmEnvironment>,
    /// Node mode: the worker's ends of the public (`parentPort`) and internal channels.
    ports: Option<(PortTransfer, PortTransfer)>,
    limits: WorkerLimits,
    parent: Parent,
    /// A dedicated web worker's end of the implicit port pair.
    inside: Option<PortTransfer>,
    /// A shared worker's control, on which its clients connect.
    scope_control: Option<Control>,
    /// Node mode: the receiver whose disconnect wakes the loop when the parent terminates.
    node_inbox: Option<Receiver<()>>,
}

/// Normalize a web Worker constructor URL and enforce the same-origin rule before starting its
/// thread. The dedicated worker's strict resource loader enforces this origin on every redirect
/// hop and the worker thread checks the final response URL again before it runs any script.
fn prepare_web_worker_entry(
    raw_url: &str,
    owner_origin: &str,
) -> NativeResult<(String, bool, Option<String>, Option<String>)> {
    let mut url = lumen_common::url::parse(raw_url, None)
        .map_err(|_| NativeError::named("SyntaxError", "invalid Worker script URL"))?;
    url.fragment = None;
    match url.scheme.as_str() {
        "http" | "https" => {
            if !url.username.is_empty() || !url.password.is_empty() {
                return Err(NativeError::named(
                    "SecurityError",
                    "Worker URL cannot contain credentials",
                ));
            }
            let worker_origin = url.origin();
            let owner = lumen_common::url::parse(owner_origin, None)
                .ok()
                .map(|url| url.origin());
            if owner.as_deref() != Some(worker_origin.as_str()) {
                return Err(NativeError::named(
                    "SecurityError",
                    "Worker script must be same-origin",
                ));
            }
            let href = url.href();
            Ok((href.clone(), true, Some(worker_origin), Some(href)))
        }
        "file" => {
            if !owner_origin.is_empty() && owner_origin != "null" {
                return Err(NativeError::named(
                    "SecurityError",
                    "file Worker must have an opaque owner origin",
                ));
            }
            if url.host.as_deref().is_some_and(|host| !host.is_empty()) {
                return Err(NativeError::named(
                    "SecurityError",
                    "file Worker URL cannot name a remote host",
                ));
            }
            let entry = percent_decode_path(&url.path)
                .ok_or_else(|| NativeError::named("SyntaxError", "invalid escape in Worker URL"))?;
            let href = url.href();
            Ok((entry, false, None, Some(href)))
        }
        _ => Err(NativeError::named(
            "NotSupportedError",
            format!(
                "unsupported Worker URL scheme '{}'; expected HTTP(S) or file",
                url.scheme
            ),
        )),
    }
}

fn percent_decode_path(path: &str) -> Option<String> {
    String::from_utf8(lumen_common::codec::percent_decode_strict(path.as_bytes())?).ok()
}

fn start_thread(
    name: String,
    spec: WorkerSpec,
    stop: Arc<AtomicBool>,
    kill: Arc<AtomicBool>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(name)
        // Same reasoning as the CLI's main thread: the engine recurses natively, and a debug
        // build's frames overflow the 2 MiB default long before the depth guard trips.
        .stack_size(lumen::THREAD_STACK_SIZE)
        .spawn(move || {
            lumen::set_thread_stack_size(lumen::THREAD_STACK_SIZE);
            run_worker(spec, stop, kill);
        })
        .map(|_| ())
}

/// The backend `lumen_host::workers` runs over in this runtime: one OS thread and one `Runtime`
/// per worker.
struct ThreadBackend;

impl WorkerBackend for ThreadBackend {
    fn spawn_dedicated(&self, ctx: &mut Ctx, spec: DedicatedSpec) -> NativeResult<u64> {
        let (entry, is_remote, origin, location_href) = match &spec.owner_origin {
            Some(owner) => prepare_web_worker_entry(&spec.url, owner)?,
            None => (spec.url.clone(), false, None, None),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let kill = Arc::new(AtomicBool::new(false));
        let thread_id = NEXT_THREAD_ID.fetch_add(1, Ordering::SeqCst);
        let worker = WorkerSpec {
            entry,
            is_module: spec.module,
            is_node: false,
            is_shared: false,
            is_remote,
            is_eval: false,
            location_href,
            name: spec.name,
            init: None,
            thread_id,
            embedding: ctx.op_state().get::<crate::WorkerEmbedding>().cloned(),
            fetch_config: ctx
                .op_state()
                .get::<lumen_web::FetchConfig>()
                .cloned()
                .unwrap_or_default(),
            origin,
            shared_key: None,
            shared_env: None,
            ports: None,
            limits: WorkerLimits::default(),
            parent: Parent::Web(spec.control),
            inside: Some(spec.inside),
            scope_control: None,
            node_inbox: None,
        };
        let id = {
            let reg = registry(ctx);
            let id = reg.next;
            reg.next += 1;
            reg.workers.insert(
                id,
                WorkerEntry {
                    stop: Arc::clone(&stop),
                    kill: Arc::clone(&kill),
                    node: None,
                },
            );
            id
        };
        if let Err(error) = start_thread(format!("lumen-worker-{thread_id}"), worker, stop, kill) {
            registry(ctx).workers.remove(&id);
            return Err(NativeError::runtime(format!(
                "could not start worker: {error}"
            )));
        }
        Ok(id)
    }

    fn terminate(&self, ctx: &mut Ctx, id: u64) {
        if let Some(entry) = registry(ctx).workers.get(&id) {
            entry.stop.store(true, Ordering::SeqCst);
            entry.kill.store(true, Ordering::SeqCst);
        }
    }

    fn exited(&self, ctx: &mut Ctx, id: u64) {
        registry(ctx).workers.remove(&id);
    }

    fn connect_shared(&self, ctx: &mut Ctx, spec: SharedSpec) -> NativeResult<u64> {
        let SharedSpec {
            key,
            entry,
            remote,
            page_side,
            worker_side,
            control,
        } = spec;
        let host = get_or_start_shared_worker(ctx, key.clone(), entry, key.is_module, remote)?;
        let client_id = NEXT_SHARED_CLIENT.fetch_add(1, Ordering::SeqCst);
        host.clients.lock().unwrap().insert(
            client_id,
            SharedClient {
                worker_port: worker_side.clone(),
                control,
            },
        );
        let weak_host = Arc::downgrade(&host);
        page_side.on_close(Arc::new(move || {
            remove_shared_client(&weak_host, client_id);
        }));
        if !host.scope.send(WorkerEvent::Connect(worker_side)) {
            page_side.close();
            return Err(NativeError::named(
                "NetworkError",
                "shared worker has stopped",
            ));
        }
        registry(ctx).shared_clients.insert(client_id, page_side);
        Ok(client_id)
    }

    fn disconnect_shared(&self, ctx: &mut Ctx, id: u64) {
        registry(ctx).shared_clients.remove(&id);
    }
}

fn get_or_start_shared_worker(
    ctx: &mut Ctx,
    key: SharedWorkerKey,
    entry: String,
    is_module: bool,
    is_remote: bool,
) -> NativeResult<Arc<SharedWorkerHost>> {
    let mut workers = shared_workers().lock().unwrap();
    if let Some(host) = workers
        .get(&key)
        .filter(|host| !host.stop.load(Ordering::SeqCst))
    {
        return Ok(Arc::clone(host));
    }

    let stop = Arc::new(AtomicBool::new(false));
    let kill = Arc::new(AtomicBool::new(false));
    let scope = Control::new();
    let host = Arc::new(SharedWorkerHost {
        stop: Arc::clone(&stop),
        kill: Arc::clone(&kill),
        scope: scope.clone(),
        clients: Mutex::new(HashMap::new()),
    });
    workers.insert(key.clone(), Arc::clone(&host));

    let location_href = is_remote.then(|| entry.clone());
    let thread_id = NEXT_THREAD_ID.fetch_add(1, Ordering::SeqCst);
    let spec = WorkerSpec {
        entry,
        is_module,
        is_node: false,
        is_shared: true,
        is_remote,
        is_eval: false,
        location_href,
        name: key.name.clone(),
        init: None,
        thread_id,
        embedding: ctx.op_state().get::<crate::WorkerEmbedding>().cloned(),
        fetch_config: ctx
            .op_state()
            .get::<lumen_web::FetchConfig>()
            .cloned()
            .unwrap_or_default(),
        origin: Some(key.origin.clone()),
        shared_key: Some(key.clone()),
        shared_env: None,
        ports: None,
        limits: WorkerLimits::default(),
        parent: Parent::Shared(Arc::downgrade(&host)),
        inside: None,
        scope_control: Some(scope),
        node_inbox: None,
    };
    if let Err(error) = start_thread(
        format!("lumen-shared-worker-{thread_id}"),
        spec,
        stop,
        kill,
    ) {
        workers.remove(&key);
        return Err(NativeError::runtime(format!(
            "could not start shared worker: {error}"
        )));
    }
    Ok(host)
}

fn remove_shared_client(host: &Weak<SharedWorkerHost>, id: u64) {
    let Some(host) = host.upgrade() else { return };
    let (client, empty) = {
        let mut clients = host.clients.lock().unwrap();
        let client = clients.remove(&id);
        (client, clients.is_empty())
    };
    if let Some(client) = client {
        client.control.close();
    }
    if !empty {
        return;
    }
    host.stop.store(true, Ordering::SeqCst);
    host.kill.store(true, Ordering::SeqCst);
    host.scope.send(WorkerEvent::Close);
    unpublish_shared_worker(&host);
}

fn unpublish_shared_worker(host: &Arc<SharedWorkerHost>) {
    shared_workers()
        .lock()
        .unwrap()
        .retain(|_, current| !Arc::ptr_eq(current, host));
}

fn publish_shared_worker_final_url(spec: &WorkerSpec, final_url: &str) {
    let (Some(key), Parent::Shared(host)) = (spec.shared_key.as_ref(), &spec.parent) else {
        return;
    };
    let Some(host) = host.upgrade() else {
        return;
    };
    if host.stop.load(Ordering::SeqCst) {
        return;
    }
    let Some(final_origin) = lumen_common::url::parse(final_url, None)
        .ok()
        .map(|url| url.origin())
    else {
        return;
    };
    if final_origin != key.origin {
        return;
    }
    let mut final_key = key.clone();
    final_key.url = final_url.split('#').next().unwrap_or(final_url).to_owned();
    let mut workers = shared_workers().lock().unwrap();
    match workers.get(&final_key) {
        Some(existing)
            if !Arc::ptr_eq(existing, &host) && !existing.stop.load(Ordering::SeqCst) => {}
        _ => {
            workers.insert(final_key, host);
        }
    }
}

/// Stop every worker this realm started: its loop and any JS it is running. Used when the realm
/// itself is being interrupted or dropped, so no worker outlives it.
pub(crate) fn terminate_all(ctx: &mut Ctx) {
    let ports = if let Some(reg) = ctx.host_mut::<WorkerRegistry>() {
        for w in reg.workers.values_mut() {
            w.stop.store(true, Ordering::SeqCst);
            w.kill.store(true, Ordering::SeqCst);
            if let Some(node) = &mut w.node {
                node.to_worker = None;
            }
        }
        reg.shared_clients
            .drain()
            .map(|(_, port)| port)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    for port in ports {
        port.close();
    }
}

/// The worker→main inbox of a node worker, carried by value through each completion so the next
/// receive re-arms.
struct MainInbox {
    id: u64,
    rx: Receiver<ToMain>,
}

struct MainInboxResult {
    id: u64,
    event: ToMain,
    inbox: Option<MainInbox>,
}

fn arm_main_inbox(ctx: &mut Ctx, inbox: MainInbox) {
    let id = inbox.id;
    let (dispatch, keep_alive) = match registry(ctx)
        .workers
        .get(&id)
        .and_then(|w| w.node.as_ref())
    {
        Some(node) => (node.dispatch.clone(), node.keep_alive),
        None => return, // already gone
    };
    let task = lumen_host::register_task(ctx, dispatch, None, decode_main_inbox);
    let reg = ctx
        .host_mut::<TaskRegistry>()
        .expect("task registry installed");
    if !keep_alive {
        reg.set_unref(task);
    }
    if let Some(node) = registry(ctx)
        .workers
        .get_mut(&id)
        .and_then(|w| w.node.as_mut())
    {
        node.inbox_task = Some(task);
    }
    let sender = ctx
        .op_state()
        .get::<CompletionSender>()
        .expect("completion sender installed")
        .clone();
    sender.run_blocking(task, move || {
        let event = inbox.rx.recv().unwrap_or(ToMain::Exited(1));
        let done = matches!(event, ToMain::Exited(_));
        Box::new(MainInboxResult {
            id,
            event,
            inbox: (!done).then_some(inbox),
        })
    });
}

fn decode_main_inbox(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    let MainInboxResult { id, event, inbox } = *payload
        .downcast::<MainInboxResult>()
        .expect("main inbox payload");
    if let Some(inbox) = inbox {
        arm_main_inbox(ctx, inbox);
    }
    match event {
        ToMain::Online => Ok(vec![Value::from_string("online".into())]),
        ToMain::OutOfMemory => Ok(vec![Value::from_string("oom".into())]),
        ToMain::Error(msg) => Ok(vec![
            Value::from_string("error".into()),
            Value::from_string(msg),
        ]),
        ToMain::Exited(code) => {
            registry(ctx).workers.remove(&id);
            Ok(vec![
                Value::from_string("exit".into()),
                Value::Num(code as f64),
            ])
        }
    }
}

// ---- worker side ------------------------------------------------------------------------------

/// A node worker's state in its own realm: the stop flag `process.exit()` and the parent's
/// `terminate()` set, the exit code, and the realm interrupt.
struct WorkerSelf {
    stop: Arc<AtomicBool>,
    exit_code: Option<i32>,
    /// The realm's engine interrupt (absent for an embedded parent's workers, which share the
    /// embedder's): `process.exit()` raises it so the calling JS stops at once, as in Node.
    kill: Option<Arc<AtomicBool>>,
}

// Declared before Runtime so every normal return drops the runtime and its host
// roots before collecting. Publish exit only after its realm has been reclaimed.
struct WorkerExit {
    parent: Parent,
    code: i32,
    out_of_memory: bool,
}
impl Drop for WorkerExit {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            lumen::collect_disposed_realms();
        }
        if self.out_of_memory {
            self.parent.send(ToMain::OutOfMemory);
        }
        self.parent.send(ToMain::Exited(self.code));
    }
}

/// What a web worker's global scope asks of this runtime.
struct ScopeHost {
    kind: ScopeKind,
    location: String,
    name: String,
    module: bool,
    stop: Arc<AtomicBool>,
    parent: Parent,
}

impl WorkerScopeHost for ScopeHost {
    fn kind(&self) -> ScopeKind {
        self.kind
    }

    fn location(&self) -> String {
        self.location.clone()
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn module(&self) -> bool {
        self.module
    }

    fn close(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Synchronously fetch one classic `importScripts()` resource with the worker's captured route
    /// and trust configuration. Cross-origin imports are intentionally allowed; the HTML algorithm
    /// requires a successful response and a JavaScript MIME type but does not apply the initial
    /// Worker constructor's same-origin restriction here.
    fn load_classic_script(&self, ctx: &mut Ctx, raw_url: &str) -> NativeResult<(String, String)> {
        let network = |message: String| NativeError::named("NetworkError", message);
        let mut url = lumen_common::url::parse(raw_url, None).map_err(|error| network(error.to_string()))?;
        if !matches!(url.scheme.as_str(), "http" | "https") {
            return Err(network(format!(
                "unsupported importScripts URL scheme '{}'; expected HTTP(S)",
                url.scheme
            )));
        }
        if !url.username.is_empty() || !url.password.is_empty() {
            return Err(network(
                "importScripts URL cannot contain credentials".into(),
            ));
        }
        // Fragments are not part of Fetch's request or response URL.
        url.fragment = None;
        let config = ctx
            .op_state()
            .get::<lumen_web::FetchConfig>()
            .cloned()
            .unwrap_or_default();
        let resource = lumen_web::load_module_resource_with_config(&url.href(), &config)
            .map_err(|error| network(error.to_string()))?;
        if !lumen_web::is_javascript_module_mime(resource.content_type.as_deref()) {
            return Err(network(format!(
                "importScripts resource has a non-JavaScript MIME type: {}",
                resource.content_type.as_deref().unwrap_or("(missing)")
            )));
        }
        let source = String::from_utf8_lossy(&resource.bytes).into_owned();
        Ok((resource.url, source))
    }

    fn report_error(&self, message: String) {
        self.parent.send(ToMain::Error(message));
    }
}

/// The worker thread's whole lifecycle: build a realm, run the entry, then pump the loop until
/// stopped (or, node mode, idle), bridging messages both ways.
fn run_worker(mut spec: WorkerSpec, stop: Arc<AtomicBool>, kill: Arc<AtomicBool>) {
    // An embedded parent's workers share its interrupt (the embedder ends them together);
    // otherwise `terminate()` interrupts this realm's JS directly.
    let parent = spec.parent.clone();
    let mut exit = WorkerExit {
        parent: parent.clone(),
        code: 1,
        out_of_memory: false,
    };
    let embedded = spec.embedding.is_some();
    if !embedded && !spec.limits.fits_a_realm() {
        exit.out_of_memory = true;
        return;
    }
    let mut rt = if spec.is_node {
        Runtime::new_worker(spec.embedding.clone())
    } else {
        Runtime::new_browser_worker(spec.embedding.clone())
    };
    // The extension installs a default FetchConfig in every Runtime. Replace it before any
    // worker bootstrap or module entry can issue requests, using the parent's captured snapshot.
    rt.engine().ctx().op_state().put(spec.fetch_config.clone());
    if let Some(environment) = spec.shared_env.take() {
        crate::process_env::bind(rt.engine().ctx(), environment);
    }
    if !embedded {
        rt.set_worker_interrupt(Arc::clone(&kill));
    }
    if !embedded {
        if let Some(limit) = spec.limits.live_object_limit() {
            rt.engine().set_live_object_limit(limit);
        }
        if spec.limits.max_old_mb.is_some() {
            lumen::Engine::scope_heap_bytes_to_thread();
        }
        if let (Some(mb), Some(used)) = (spec.limits.max_old_mb, lumen::Engine::heap_bytes()) {
            rt.engine().set_heap_limit_fatal(true);
            rt.engine()
                .set_heap_limit(used + (mb * 1024.0 * 1024.0) as usize);
        }
    }
    if let Some(depth) = spec.limits.max_depth() {
        rt.engine().set_max_depth(depth);
    }
    if crate::atomics_wait_trace_enabled() {
        crate::install_atomics_wait_trace(rt.engine(), spec.thread_id);
    }
    if spec.is_node {
        lumen_host::install(rt.engine(), &[worker_scope_extension()]);
    } else {
        if lumen_html_js::install_css_typed_om(rt.engine().ctx()).is_err() {
            parent.send(ToMain::Error(
                "worker CSS Typed OM installation failed".to_string(),
            ));
            return;
        }
        if lumen_html_js::install_worker_canvas(rt.engine().ctx()).is_err() {
            parent.send(ToMain::Error("worker canvas installation failed".to_string()));
            return;
        }
    }
    if spec.is_node {
        rt.engine().ctx().op_state().put(WorkerSelf {
            stop: Arc::clone(&stop),
            exit_code: None,
            kill: (!embedded).then(|| Arc::clone(&kill)),
        });
    }

    // Fetch a web worker's HTTP(S) entry before bootstrapping its realm so WorkerLocation reflects
    // the final response URL, including redirects. The same-origin loader checks every redirect
    // hop before issuing its request; imported classic scripts and module dependencies keep their
    // own resource-loading policies after the global scope exists.
    let remote_entry = if spec.is_remote {
        let resource = match lumen_web::load_module_resource_same_origin_with_config(
            &spec.entry,
            &spec.fetch_config,
        ) {
            Ok(resource) => resource,
            Err(error) => {
                parent.send(ToMain::Error(format!("cannot fetch worker script: {error}")));
                return;
            }
        };
        let response_origin = lumen_common::url::parse(&resource.url, None)
            .ok()
            .map(|url| url.origin());
        if spec.origin.as_deref() != response_origin.as_deref() {
            parent.send(ToMain::Error(format!(
                "worker redirect left its origin: {}",
                resource.url
            )));
            return;
        }
        if !lumen_web::is_javascript_module_mime(resource.content_type.as_deref()) {
            parent.send(ToMain::Error(format!(
                "worker script has a non-JavaScript MIME type: {}",
                resource.content_type.as_deref().unwrap_or("(missing)")
            )));
            return;
        }
        publish_shared_worker_final_url(&spec, &resource.url);
        Some(resource)
    } else {
        None
    };

    // The per-realm bootstrap: a DedicatedWorkerGlobalScope or SharedWorkerGlobalScope for web
    // workers, and worker_threads wiring (parentPort/workerData/threadId/process patches) for
    // node workers.
    let booted = if spec.is_node {
        boot_node_worker(&mut rt, &mut spec)
    } else {
        let location = remote_entry
            .as_ref()
            .map(|resource| resource.url.clone())
            .or_else(|| spec.location_href.clone())
            .unwrap_or_else(|| spec.entry.clone());
        let host = Rc::new(ScopeHost {
            kind: if spec.is_shared {
                ScopeKind::Shared
            } else {
                ScopeKind::Dedicated
            },
            location,
            name: spec.name.clone(),
            module: spec.is_module,
            stop: Arc::clone(&stop),
            parent: parent.clone(),
        });
        workers::install_scope(
            rt.engine().ctx(),
            ScopeInstall {
                host,
                inside: spec.inside.take(),
                control: spec.scope_control.take(),
            },
        )
        .map_err(|_| "worker scope bootstrap failed".to_string())
    };
    if let Err(message) = booted {
        if rt.engine().heap_limit_hit() {
            exit.out_of_memory = true;
        } else {
            parent.send(ToMain::Error(message));
        }
        return;
    }

    if !spec.is_node {
        rt.enable_worker_rejection_events();
    }

    parent.send(ToMain::Online);

    if spec.is_node {
        let inbox = spec.node_inbox.take();
        run_node_worker(&mut rt, &spec, inbox, &stop, &kill);
        exit.out_of_memory = rt.engine().heap_limit_hit();
        exit.code = if exit.out_of_memory {
            1
        } else {
            worker_exit_code(&mut rt)
                .or(rt.fatal_exit_code())
                .unwrap_or(
                    if stop.load(Ordering::SeqCst) || kill.load(Ordering::SeqCst) {
                        1
                    } else {
                        0
                    },
                )
        };
        return;
    }

    let is_module = spec.is_module;
    let entry_result = if spec.is_eval {
        let cwd = match &spec.embedding {
            Some(embedding) => Ok(embedding.cwd.clone()),
            None => std::env::current_dir(),
        };
        let base = cwd
            .map(|d| d.join("[worker eval]").to_string_lossy().into_owned())
            .unwrap_or_else(|_| "[worker eval]".to_string());
        rt.eval_worker_entry(&spec.entry, &base, false)
    } else if let Some(resource) = remote_entry {
        let text = String::from_utf8_lossy(&resource.bytes);
        let source = text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned();
        rt.eval_worker_entry(&crate::import_source_text(source), &resource.url, is_module)
    } else {
        match std::fs::read_to_string(&spec.entry) {
            Ok(source) => {
                rt.eval_worker_entry(&crate::import_source_text(source), &spec.entry, is_module)
            }
            Err(e) => Err(format!("cannot load worker script {}: {e}", spec.entry)),
        }
    };
    if let Err(e) = entry_result {
        if kill.load(Ordering::SeqCst) {
            exit.code = 1;
            return;
        }
        parent.send(ToMain::Error(e));
        if spec.is_shared {
            // A failed shared-worker entry never reaches a usable SharedWorkerGlobalScope. Send
            // the error first, then close every client through the normal exit path.
            stop.store(true, Ordering::SeqCst);
        }
    }

    rt.run_worker_loop(&stop);

    exit.code = if spec.is_shared || !stop.load(Ordering::SeqCst) {
        0
    } else {
        1
    };
}

/// Hand the node worker's runtime pieces (the `__wself` exit op, the thread id, the init payload and
/// the channel ports) to the `worker_threads` glue's `__lumenInitWorkerThread`, and keep the hooks
/// it returns on the global for [`run_node_worker`].
fn boot_node_worker(rt: &mut Runtime, spec: &mut WorkerSpec) -> Result<(), String> {
    let ctx = rt.engine().ctx();
    let global = ctx.global_this();
    let wself = ctx.get_member(&global, "__wself").unwrap_or(Value::Undefined);
    let _ = ctx.delete_member(&global, "__wself");
    let ports = match spec.ports.take() {
        Some((public, internal)) => {
            let public = lumen_host::ports::adopt(ctx, public);
            let internal = lumen_host::ports::adopt(ctx, internal);
            ctx.make_array(vec![Value::Num(public as f64), Value::Num(internal as f64)])
        }
        None => Value::Undefined,
    };
    let init = match spec.init.take() {
        Some(message) => {
            let bytes = clone_transfer::install_message(ctx, message);
            ctx.make_uint8array(&bytes)
                .map_err(|_| "worker init payload failed".to_string())?
        }
        None => Value::Undefined,
    };
    let hook = ctx
        .get_member(&global, "__lumenInitWorkerThread")
        .unwrap_or(Value::Undefined);
    if !hook.is_callable() {
        return Err("node worker glue is not installed".into());
    }
    let hooks = rt
        .engine()
        .call_function(
            &hook,
            Value::Undefined,
            &[wself, Value::Num(spec.thread_id as f64), init, ports],
        )
        .map_err(|_| "worker scope bootstrap failed".to_string())?;
    let ctx = rt.engine().ctx();
    let descriptor = ctx.new_object_with_proto(&Value::Null);
    for (key, value) in [
        ("value", hooks),
        ("writable", Value::Bool(false)),
        ("enumerable", Value::Bool(false)),
        ("configurable", Value::Bool(true)),
    ] {
        let _ = ctx.member_set(&descriptor, key, value);
    }
    ctx.define_property_value(&global, Value::str("__lumenWorkerHooks"), &descriptor)
        .map_err(|_| "worker scope bootstrap failed".to_string())
}

/// A node worker: the entry runs as the realm's main program (a throw is an uncaught exception
/// the worker's own `process` handlers see), then the loop, then Node's end-of-thread protocol
/// (`'beforeExit'` while listeners add work, then `'exit'`) when it ran out of work by itself.
fn run_node_worker(
    rt: &mut Runtime,
    spec: &WorkerSpec,
    inbox: Option<Receiver<()>>,
    stop: &AtomicBool,
    kill: &AtomicBool,
) {
    lumen_host::perf::mark(lumen_host::perf::Milestone::LoopStart);
    let hooks = {
        let ctx = rt.engine().ctx();
        let global = ctx.global_this();
        ctx.get_member(&global, "__lumenWorkerHooks")
            .unwrap_or(Value::Undefined)
    };
    let hook = |rt: &mut Runtime, name: &str| -> Value {
        rt.engine()
            .ctx()
            .get_member(&hooks, name)
            .unwrap_or(Value::Undefined)
    };
    // Node worker entries follow the same extension/package scope policy as imports.
    let is_module = spec.is_module
        || (!spec.is_eval && crate::esm::file_is_esm(std::path::Path::new(&spec.entry)));
    let thrown = if spec.is_eval {
        let run = hook(rt, "runEval");
        let result = rt.engine().call_function(
            &run,
            Value::Undefined,
            &[Value::from_string(spec.entry.clone())],
        );
        rt.checkpoint();
        result.err()
    } else if is_module {
        rt.install_module_loader(&spec.entry, false);
        let run = hook(rt, "runModule");
        let result = rt.engine().call_function(
            &run,
            Value::Undefined,
            &[Value::from_string(spec.entry.clone())],
        );
        rt.checkpoint();
        result.err()
    } else {
        match rt.start_main_raw(&spec.entry, None) {
            Ok(()) => None,
            Err(crate::StartError::Thrown(error)) => Some(error),
            Err(crate::StartError::NotInstalled(message)) => {
                Some(rt.engine().ctx().make_error("Error", message))
            }
        }
    };
    if let Some(error) = thrown {
        if !kill.load(Ordering::SeqCst) && !rt.engine().is_terminated() {
            rt.report_uncaught(&error);
        }
    }
    if kill.load(Ordering::SeqCst) || rt.halted() {
        return;
    }
    if let Some(inbox) = inbox {
        arm_terminate_wake(rt.engine().ctx(), inbox);
    }
    rt.run_worker_loop(stop);
    let before_exit = hook(rt, "beforeExit");
    for _ in 0..1000 {
        if stop.load(Ordering::SeqCst) || kill.load(Ordering::SeqCst) || rt.halted() {
            return;
        }
        rt.fire(&before_exit, &[]);
        if stop.load(Ordering::SeqCst) || kill.load(Ordering::SeqCst) || rt.halted() || rt.idle() {
            break;
        }
        rt.run_worker_loop(stop);
    }
    if stop.load(Ordering::SeqCst) || kill.load(Ordering::SeqCst) || rt.halted() {
        return;
    }
    let exit = hook(rt, "exit");
    rt.fire(&exit, &[]);
}

fn worker_exit_code(rt: &mut Runtime) -> Option<i32> {
    rt.engine()
        .ctx()
        .op_state()
        .get::<WorkerSelf>()
        .and_then(|w| w.exit_code)
}

/// An unref'd task that completes when the parent drops its sender (terminate), so a node
/// worker blocked in its loop notices the stop flag at once instead of at the next poll.
fn arm_terminate_wake(ctx: &mut Ctx, rx: Receiver<()>) {
    let nothing = ctx.new_native_fn(
        "",
        0,
        Rc::new(|_: &mut Ctx, _this: Value, _: &[Value]| Ok(Value::Undefined)),
    );
    let task = lumen_host::register_task(ctx, nothing, None, decode_terminate_wake);
    ctx.host_mut::<TaskRegistry>()
        .expect("task registry installed")
        .set_unref(task);
    let sender = ctx
        .op_state()
        .get::<CompletionSender>()
        .expect("completion sender installed")
        .clone();
    sender.run_blocking(task, move || {
        let _ = rx.recv();
        Box::new(())
    });
}

fn decode_terminate_wake(
    _ctx: &mut Ctx,
    _payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    Ok(Vec::new())
}

#[lumen_bind::module(name = "__wself")]
mod worker_scope_ops {
    use super::*;

    /// `__wself.exit(code)` — node `process.exit(code)` inside a worker: record the code and stop the
    /// loop. With the realm's own interrupt available the calling JS unwinds now (every later safe
    /// point throws again); an embedded parent's worker stops cooperatively at the next loop poll.
    #[op(name = "exit", coerce)]
    fn op_wself_exit(ctx: &mut Ctx, code: Option<f64>) -> Result<(), OpError> {
        let code = code.unwrap_or(0.0) as i32;
        let mut kill = None;
        if let Some(w) = ctx.host_mut::<WorkerSelf>() {
            w.exit_code.get_or_insert(code);
            w.stop.store(true, Ordering::SeqCst);
            kill = w.kill.clone();
        }
        match kill {
            Some(kill) => {
                kill.store(true, Ordering::SeqCst);
                ctx.terminate_for_host();
                Err(NativeError::runtime(format!("process.exit({code})")).into())
            }
            None => Ok(()),
        }
    }
}

fn worker_scope_extension() -> Extension {
    Extension {
        name: "worker-scope",
        modules: &[lumen_host::namespace::<worker_scope_ops::Module>],
        state_init: None,
        js_init: None,
        js_init_snapshot: None,
        lazy_globals: &[],
    }
}

#[lumen_bind::module(name = "__lumenWorkerOps")]
mod worker_ops {
    use super::*;

    /// `__lumenWorkerOps.spawn(path, isModule, dispatch, opts)` → `{ id, threadId, port, internal }`.
    /// Spawns a node worker thread and arms the worker→main inbox. `dispatch(kind, ...)` receives
    /// `("online")`, `("oom")`, `("error", string)`, or `("exit", code)`. `opts`:
    /// `{ eval: bool, init: Uint8Array, shareEnv: bool, maxOldMb: number, stackMb: number }`.
    #[op(name = "spawn", coerce)]
    fn op_worker_spawn(
        ctx: &mut Ctx,
        raw_entry: String,
        is_module: Option<bool>,
        dispatch: JsFunction,
        opts: Option<Value>,
    ) -> Result<Value, Value> {
        let is_module = is_module.unwrap_or(false);
        let dispatch = dispatch.into_value();
        let opts = opts.unwrap_or(Value::Undefined);
        let has_opts = opts.as_obj().is_some();
        let opt_bool = |ctx: &mut Ctx, name: &str| -> bool {
            has_opts && matches!(ctx.get_member(&opts, name), Ok(Value::Bool(true)))
        };
        let is_eval = opt_bool(ctx, "eval");
        let init = if has_opts {
            ctx.get_member(&opts, "init")
                .ok()
                .and_then(|v| ctx.typed_array_bytes(&v))
        } else {
            None
        };

        let limits = if has_opts {
            let number = |ctx: &mut Ctx, name: &str| {
                ctx.get_member(&opts, name)
                    .ok()
                    .and_then(|v| v.as_num_opt())
                    .filter(|n| n.is_finite())
            };
            WorkerLimits {
                max_old_mb: number(ctx, "maxOldMb"),
                stack_mb: number(ctx, "stackMb"),
            }
        } else {
            WorkerLimits::default()
        };

        let shared_env = if opt_bool(ctx, "shareEnv") {
            Some(crate::process_env::backing(ctx))
        } else {
            None
        };
        let init = init.map(|bytes| clone_transfer::take_message(ctx, bytes));
        let (to_worker_tx, to_worker_rx) = channel::<()>();
        let (to_main_tx, to_main_rx) = channel::<ToMain>();
        let stop = Arc::new(AtomicBool::new(false));
        let kill = Arc::new(AtomicBool::new(false));
        let thread_id = NEXT_THREAD_ID.fetch_add(1, Ordering::SeqCst);

        let id = {
            let reg = registry(ctx);
            let id = reg.next;
            reg.next += 1;
            reg.workers.insert(
                id,
                WorkerEntry {
                    stop: Arc::clone(&stop),
                    kill: Arc::clone(&kill),
                    node: Some(NodeLink {
                        to_worker: Some(to_worker_tx),
                        dispatch: dispatch.clone(),
                        keep_alive: true,
                        inbox_task: None,
                    }),
                },
            );
            id
        };

        let embedding = ctx.op_state().get::<crate::WorkerEmbedding>().cloned();
        let fetch_config = ctx
            .op_state()
            .get::<lumen_web::FetchConfig>()
            .cloned()
            .unwrap_or_default();
        let (public_main, public_worker) = lumen_host::ports::new_pair();
        let (internal_main, internal_worker) = lumen_host::ports::new_pair();
        let parent_ports = (
            lumen_host::ports::adopt(ctx, public_main),
            lumen_host::ports::adopt(ctx, internal_main),
        );
        let spec = WorkerSpec {
            entry: raw_entry,
            is_module,
            is_node: true,
            is_shared: false,
            is_remote: false,
            is_eval,
            location_href: None,
            name: String::new(),
            init,
            thread_id,
            embedding,
            fetch_config,
            origin: None,
            shared_key: None,
            shared_env,
            ports: Some((public_worker, internal_worker)),
            limits,
            parent: Parent::Node(to_main_tx),
            inside: None,
            scope_control: None,
            node_inbox: Some(to_worker_rx),
        };
        start_thread(
            format!("lumen-worker-{thread_id}"),
            spec,
            stop,
            kill,
        )
        .expect("spawn worker thread");

        arm_main_inbox(ctx, MainInbox { id, rx: to_main_rx });
        let o = Value::Obj(ctx.new_object());
        let _ = ctx.set_member(&o, "id", Value::Num(id as f64));
        let _ = ctx.set_member(&o, "threadId", Value::Num(thread_id as f64));
        let _ = ctx.set_member(&o, "port", Value::Num(parent_ports.0 as f64));
        let _ = ctx.set_member(&o, "internal", Value::Num(parent_ports.1 as f64));
        Ok(o)
    }

    /// `__lumenWorkerOps.terminate(id)` — set the shared stop flag and drop the worker's inbox
    /// sender (so a blocked receive unblocks). The worker loop exits at its next poll and posts
    /// `Exited`, which is still delivered (the registry entry lives until then) so `'exit'` fires
    /// with the code.
    #[op(name = "terminate", coerce)]
    fn op_worker_terminate(ctx: &mut Ctx, id: f64) {
        if let Some(w) = registry(ctx).workers.get_mut(&(id as u64)) {
            w.stop.store(true, Ordering::SeqCst);
            w.kill.store(true, Ordering::SeqCst);
            if let Some(node) = &mut w.node {
                node.to_worker = None;
            }
        }
    }

    /// `__lumenWorkerOps.setRef(id, keep)` — `worker.ref()/unref()`: whether this worker's inbox
    /// keeps the main loop alive. Applies to the in-flight inbox task and every re-arm after it.
    #[op(name = "setRef", coerce)]
    fn op_worker_set_ref(ctx: &mut Ctx, id: f64, keep: Option<bool>) {
        let keep = keep.unwrap_or(false);
        let task = match registry(ctx)
            .workers
            .get_mut(&(id as u64))
            .and_then(|w| w.node.as_mut())
        {
            Some(node) => {
                node.keep_alive = keep;
                node.inbox_task
            }
            None => return,
        };
        if let (Some(task), Some(reg)) = (task, ctx.host_mut::<TaskRegistry>()) {
            if keep {
                reg.set_ref(task);
            } else {
                reg.set_unref(task);
            }
        }
    }
}

fn install_worker_classes(ctx: &mut Ctx) -> Result<(), Value> {
    workers::set_backend(ctx, Rc::new(ThreadBackend));
    workers::install_page_classes(ctx)
}

/// `Worker`, `SharedWorker` and the `__lumenWorkerOps` handle `node:worker_threads` reads.
pub(crate) fn extension() -> Extension {
    Extension {
        name: "worker",
        modules: &[
            lumen_host::namespace::<worker_ops::Module>,
            install_worker_classes,
        ],
        state_init: Some(|state| state.put(WorkerRegistry::default())),
        js_init: None,
        js_init_snapshot: None,
        lazy_globals: &[],
    }
}
