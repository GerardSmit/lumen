//! Workers — realm-per-thread with structured messaging. Backs the Web `Worker`, `SharedWorker`
//! and `node:worker_threads` (the node glue's Worker class drives the dedicated-worker ops in
//! "node mode"). Shared workers are process-registered by URL, origin, type, and name; each
//! parent runtime retains a separate dispatcher and native MessagePort endpoint.
//!
//! A `Worker` is a dedicated OS thread running its OWN [`Runtime`] (a fresh realm: its own global,
//! intrinsics, event loop, and thread pool). Because engine `Value`s are `!Send`, messages cross
//! the thread boundary as bytes: the sender serializes with the JS structured-clone wire format
//! (`__serializeForClone`, see `lumen-web/src/js/serialize.js`) and the receiver deserializes in
//! its own realm. Two `mpsc` channels carry those bytes; each side arms a blocking "inbox" task
//! (the WebSocket-reader re-arm pattern) that delivers each message to JS and re-arms the next.
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

use lumen_bind::NativeError;
use lumen_host::OpError;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use lumen::embed::JsFunction;
use lumen_host::{CompletionSender, Ctx, Extension, TaskId, TaskRegistry, Value};

use crate::clone_transfer::{self, CloneMessage};
use crate::Runtime;

#[lumen_bind::module(name = "__lumenSharedWorker")]
pub(crate) mod shared_worker_bindings {
    use lumen::embed::{Ctx, OpError, OpResult, Value};

    #[op]
    pub fn connect(
        ctx: &mut Ctx,
        url: String,
        origin: String,
        is_module: bool,
        name: String,
        dispatch: Value,
    ) -> OpResult<Value> {
        let args = [
            Value::from_string(url),
            Value::from_string(origin),
            Value::Bool(is_module),
            Value::from_string(name),
            dispatch,
        ];
        super::op_shared_worker_connect(ctx, Value::Undefined, &args).map_err(OpError::thrown)
    }

    #[op]
    pub fn disconnect(ctx: &mut Ctx, id: u64) -> OpResult<Value> {
        super::op_shared_worker_disconnect(ctx, Value::Undefined, &[Value::Num(id as f64)])
            .map_err(OpError::thrown)
    }
}

#[lumen_bind::module(name = "__lumenWorkerScript")]
pub(crate) mod worker_script_bindings {
    use lumen::embed::{Ctx, HostRealmEvalError, OpError, OpResult, Value};

    /// Execute one fetched worker import as a global classic Script. Unlike indirect eval, this
    /// preserves the worker global's lexical environment and records the fetched URL for stacks.
    #[op(rename(js = "executeClassicScript"))]
    pub fn execute_classic_script(
        ctx: &mut Ctx,
        source: String,
        source_url: String,
    ) -> OpResult<Value> {
        let realm = ctx.current_host_realm();
        match ctx.eval_value_in_host_realm_named(&realm, &source, false, Some(&source_url)) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(exception)) => Err(OpError::thrown(exception)),
            Err(HostRealmEvalError::Parse(error)) => {
                let dynamic_code_unavailable =
                    error.message == "dynamic code is unavailable in native execution";
                let exception = if dynamic_code_unavailable {
                    ctx.make_error("EvalError", error.message)
                } else {
                    ctx.make_error(
                        "SyntaxError",
                        format!("{}:{}: {}", source_url, error.line, error.message),
                    )
                };
                Err(OpError::thrown(exception))
            }
            Err(HostRealmEvalError::Scope(_)) => Err(OpError::thrown(ctx.make_error(
                "Error",
                format!("could not enter worker script realm for {source_url}"),
            ))),
        }
    }
}

/// Node-visible thread ids: the main thread is 0, workers count up from 1 (process-wide, so ids
/// stay unique even for workers spawned from workers).
static NEXT_THREAD_ID: AtomicU64 = AtomicU64::new(1);

// ---- cross-thread messages (all Send) ---------------------------------------------------------

enum ToWorker {
    Data(CloneMessage),
    Connect(crate::ports::PortTransfer),
}

enum ToMain {
    Online,
    OutOfMemory,
    Message(CloneMessage),
    Error(String),
    Exited(i32),
}

// ---- main side --------------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct WorkerRegistry {
    next: u64,
    workers: HashMap<u64, WorkerEntry>,
    shared_clients: HashMap<u64, SharedClientLocal>,
}

struct WorkerEntry {
    /// `None` once terminated (dropping the sender unblocks the worker's inbox receive).
    to_worker: Option<Sender<ToWorker>>,
    dispatch: Value,
    stop: Arc<AtomicBool>,
    /// Set by `terminate()` only: the worker realm's engine interrupt, so running JS (a busy
    /// loop, a microtask or nextTick storm) is stopped at its next safe point, not just the loop.
    kill: Arc<AtomicBool>,
    /// Whether this worker's main-side inbox keeps the main loop alive (`worker.unref()` clears).
    keep_alive: bool,
    /// The currently armed inbox task, so `setRef` can re-mark it in flight.
    inbox_task: Option<TaskId>,
}

struct SharedClientLocal {
    dispatch: Value,
    inbox_task: Option<TaskId>,
    port: crate::ports::PortTransfer,
}

type SharedWorkerKey = lumen_common::worker::SharedWorkerKey;

struct SharedClient {
    worker_port: crate::ports::PortTransfer,
    events: Sender<SharedEvent>,
}

struct SharedWorkerHost {
    to_worker: Sender<ToWorker>,
    stop: Arc<AtomicBool>,
    kill: Arc<AtomicBool>,
    clients: Mutex<HashMap<u64, SharedClient>>,
}

#[derive(Clone)]
enum SharedEvent {
    Error(String),
    Closed,
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

/// The worker→main inbox, carried by value through each completion so the next receive re-arms.
struct MainInbox {
    id: u64,
    rx: Receiver<ToMain>,
}

struct MainInboxResult {
    id: u64,
    event: ToMain,
    inbox: Option<MainInbox>,
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
    shared_host: Option<Weak<SharedWorkerHost>>,
    shared_env: Option<crate::process_env::RealmEnvironment>,
    /// Node mode: the worker's ends of the public (`parentPort`) and internal channels.
    ports: Option<(crate::ports::PortTransfer, crate::ports::PortTransfer)>,
    limits: WorkerLimits,
}

/// Normalize a web Worker constructor URL and enforce the same-origin rule before starting its
/// thread. The dedicated worker's strict resource loader enforces this origin on every redirect
/// hop and the worker thread checks the final response URL again before it runs any script.
fn prepare_web_worker_entry(
    ctx: &mut Ctx,
    raw_url: &str,
    owner_origin: &str,
) -> Result<(String, bool, Option<String>, Option<String>), Value> {
    let mut url = lumen_common::url::parse(raw_url, None)
        .map_err(|_| ctx.make_error("SyntaxError", "invalid Worker script URL"))?;
    url.fragment = None;
    match url.scheme.as_str() {
        "http" | "https" => {
            if !url.username.is_empty() || !url.password.is_empty() {
                return Err(
                    ctx.make_error("SecurityError", "Worker URL cannot contain credentials")
                );
            }
            let worker_origin = shared_url_origin(&url);
            let owner = lumen_common::url::parse(owner_origin, None)
                .ok()
                .map(|url| shared_url_origin(&url));
            if owner.as_deref() != Some(worker_origin.as_str()) {
                return Err(ctx.make_error("SecurityError", "Worker script must be same-origin"));
            }
            let href = url.href();
            Ok((href.clone(), true, Some(worker_origin), Some(href)))
        }
        "file" => {
            if !owner_origin.is_empty() && owner_origin != "null" {
                return Err(ctx.make_error(
                    "SecurityError",
                    "file Worker must have an opaque owner origin",
                ));
            }
            if url.host.as_deref().is_some_and(|host| !host.is_empty()) {
                return Err(
                    ctx.make_error("SecurityError", "file Worker URL cannot name a remote host")
                );
            }
            let entry = percent_decode_path(&url.path)
                .ok_or_else(|| ctx.make_error("SyntaxError", "invalid escape in Worker URL"))?;
            let href = url.href();
            Ok((entry, false, None, Some(href)))
        }
        _ => Err(ctx.make_error(
            "NotSupportedError",
            format!(
                "unsupported Worker URL scheme '{}'; expected HTTP(S) or file",
                url.scheme
            ),
        )),
    }
}

/// `__worker.sharedConnect(url, origin, isModule, name, dispatch)` returns a native client port
/// and registers its event callback in this parent realm. The global registry spans Runtime
/// instances; each client still owns its own dispatcher and message-port endpoint.
fn op_shared_worker_connect(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let raw_url = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    let requested_origin = ctx
        .coerce_string(args.get(1).unwrap_or(&Value::Undefined))?
        .to_string();
    let is_module = matches!(args.get(2), Some(Value::Bool(true)));
    let name = ctx
        .coerce_string(args.get(3).unwrap_or(&Value::Undefined))?
        .to_string();
    let dispatch = match args.get(4) {
        Some(value) if value.is_callable() => value.clone(),
        _ => return Err(ctx.make_error("TypeError", "SharedWorker dispatcher must be callable")),
    };
    let (key, entry, is_remote) =
        resolve_shared_worker(ctx, &raw_url, &requested_origin, is_module, name)?;
    let host = get_or_start_shared_worker(ctx, key.clone(), entry, is_module, is_remote)?;

    let (client_port, worker_port) = crate::ports::new_pair();
    let client_id = NEXT_SHARED_CLIENT.fetch_add(1, Ordering::SeqCst);
    let (events_tx, events_rx) = channel();
    {
        let mut clients = host.clients.lock().unwrap();
        clients.insert(
            client_id,
            SharedClient {
                worker_port: worker_port.clone(),
                events: events_tx,
            },
        );
    }
    let weak_host = Arc::downgrade(&host);
    client_port.on_close(Arc::new(move || {
        remove_shared_client(&weak_host, client_id);
    }));
    if host.to_worker.send(ToWorker::Connect(worker_port)).is_err() {
        client_port.close();
        return Err(ctx.make_error("NetworkError", "shared worker has stopped"));
    }

    let local_port = client_port.clone();
    let port_id = crate::ports::adopt(ctx, client_port);
    let local_id = {
        let registry = registry(ctx);
        let id = registry.next;
        registry.next = registry.next.wrapping_add(1).max(1);
        registry.shared_clients.insert(
            id,
            SharedClientLocal {
                dispatch,
                inbox_task: None,
                port: local_port,
            },
        );
        id
    };
    arm_shared_inbox(
        ctx,
        SharedInbox {
            id: local_id,
            rx: events_rx,
        },
    );
    let result = Value::Obj(ctx.new_object());
    let _ = ctx.set_member(&result, "id", Value::Num(local_id as f64));
    let _ = ctx.set_member(&result, "port", Value::Num(port_id as f64));
    Ok(result)
}

fn op_shared_worker_disconnect(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let id = args.first().and_then(Value::as_num_opt).unwrap_or(-1.0) as u64;
    let task = registry(ctx)
        .shared_clients
        .remove(&id)
        .and_then(|client| client.inbox_task);
    if let (Some(task), Some(tasks)) = (task, ctx.host_mut::<TaskRegistry>()) {
        tasks.cancel(task);
    }
    Ok(Value::Undefined)
}

fn resolve_shared_worker(
    ctx: &mut Ctx,
    raw_url: &str,
    requested_origin: &str,
    is_module: bool,
    name: String,
) -> Result<(SharedWorkerKey, String, bool), Value> {
    let cwd = ctx
        .op_state()
        .get::<lumen_host::RealmProcess>()
        .map(|process| process.cwd.clone())
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| {
            ctx.make_error("NotSupportedError", "shared worker has no base directory")
        })?;
    let mut base = format!("file://{}", cwd.to_string_lossy());
    if !base.ends_with('/') {
        base.push('/');
    }
    let parsed = lumen_common::url::parse(raw_url, Some(&base))
        .map_err(|_| ctx.make_error("SyntaxError", "invalid shared worker URL"))?;
    if !matches!(parsed.scheme.as_str(), "file" | "http" | "https") {
        return Err(ctx.make_error(
            "NotSupportedError",
            "shared worker scripts require file, http, or https URLs",
        ));
    }
    let script_origin = shared_url_origin(&parsed);
    let caller_origin = if requested_origin.is_empty() || requested_origin == "null" {
        "null".to_owned()
    } else {
        let caller = lumen_common::url::parse(&requested_origin, None)
            .map_err(|_| ctx.make_error("SecurityError", "invalid SharedWorker origin"))?;
        shared_url_origin(&caller)
    };
    if caller_origin != script_origin {
        return Err(ctx.make_error("SecurityError", "SharedWorker script must be same-origin"));
    }
    let (canonical_url, entry, is_remote) = if parsed.scheme == "file" {
        let entry = percent_decode_path(&parsed.path)
            .ok_or_else(|| ctx.make_error("SyntaxError", "invalid escape in shared worker URL"))?;
        let canonical_path =
            std::fs::canonicalize(&entry).unwrap_or_else(|_| std::path::PathBuf::from(&entry));
        let mut canonical_url = format!("file://{}", canonical_path.to_string_lossy());
        if let Some(query) = parsed.query.as_deref() {
            canonical_url.push('?');
            canonical_url.push_str(query);
        }
        (
            canonical_url,
            canonical_path.to_string_lossy().into_owned(),
            false,
        )
    } else {
        let mut parsed = parsed;
        // Fragments do not participate in fetching or the shared-worker identity.
        parsed.fragment = None;
        let canonical_url = parsed.href();
        (canonical_url.clone(), canonical_url, true)
    };
    Ok((
        SharedWorkerKey {
            url: canonical_url,
            origin: script_origin,
            is_module,
            credentials: lumen_common::cors::Credentials::SameOrigin,
            name,
        },
        entry,
        is_remote,
    ))
}

fn shared_url_origin(url: &lumen_common::url::Url) -> String {
    if url.scheme == "file" || url.opaque {
        return "null".to_owned();
    }
    let Some(host) = url.host.as_deref() else {
        return "null".to_owned();
    };
    let default_port = match url.scheme.as_str() {
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        "ftp" => Some(21),
        _ => None,
    };
    let mut origin = format!("{}://{host}", url.scheme);
    if let Some(port) = url.port.filter(|port| Some(*port) != default_port) {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    origin
}

fn percent_decode_path(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hi = *bytes.get(index + 1)?;
            let lo = *bytes.get(index + 2)?;
            output.push((hex_digit(hi)? << 4) | hex_digit(lo)?);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).ok()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn get_or_start_shared_worker(
    ctx: &mut Ctx,
    key: SharedWorkerKey,
    entry: String,
    is_module: bool,
    is_remote: bool,
) -> Result<Arc<SharedWorkerHost>, Value> {
    let mut workers = shared_workers().lock().unwrap();
    if let Some(host) = workers
        .get(&key)
        .filter(|host| !host.stop.load(Ordering::SeqCst))
    {
        return Ok(Arc::clone(host));
    }

    let (to_worker, worker_rx) = channel::<ToWorker>();
    let (to_main, main_rx) = channel::<ToMain>();
    let stop = Arc::new(AtomicBool::new(false));
    let kill = Arc::new(AtomicBool::new(false));
    let host = Arc::new(SharedWorkerHost {
        to_worker: to_worker.clone(),
        stop: Arc::clone(&stop),
        kill: Arc::clone(&kill),
        clients: Mutex::new(HashMap::new()),
    });
    workers.insert(key.clone(), Arc::clone(&host));

    let location_href = is_remote.then(|| entry.clone());
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
        thread_id: NEXT_THREAD_ID.fetch_add(1, Ordering::SeqCst),
        embedding: ctx.op_state().get::<crate::WorkerEmbedding>().cloned(),
        fetch_config: ctx
            .op_state()
            .get::<lumen_web::FetchConfig>()
            .cloned()
            .unwrap_or_default(),
        origin: Some(key.origin.clone()),
        shared_key: Some(key.clone()),
        shared_host: Some(Arc::downgrade(&host)),
        shared_env: None,
        ports: None,
        limits: WorkerLimits::default(),
    };
    let worker_stop = Arc::clone(&stop);
    let worker_kill = Arc::clone(&kill);
    if let Err(error) = std::thread::Builder::new()
        .name(format!("lumen-shared-worker-{}", spec.thread_id))
        .stack_size(lumen::THREAD_STACK_SIZE)
        .spawn(move || {
            lumen::set_thread_stack_size(lumen::THREAD_STACK_SIZE);
            run_worker(spec, worker_rx, to_main, worker_stop, worker_kill);
        })
    {
        workers.remove(&key);
        return Err(ctx.make_error("Error", format!("could not start shared worker: {error}")));
    }
    let weak_host = Arc::downgrade(&host);
    if let Err(error) = std::thread::Builder::new()
        .name("lumen-shared-worker-events".into())
        .spawn(move || route_shared_events(weak_host, main_rx))
    {
        // The execution thread is already live. Stop and unpublish it if the event router cannot
        // be created, so a failed constructor never leaves an unreachable shared realm behind.
        host.stop.store(true, Ordering::SeqCst);
        host.kill.store(true, Ordering::SeqCst);
        workers.remove(&key);
        return Err(ctx.make_error("Error", format!("could not monitor shared worker: {error}")));
    }
    Ok(host)
}

fn route_shared_events(host: Weak<SharedWorkerHost>, events: Receiver<ToMain>) {
    while let Ok(event) = events.recv() {
        let Some(host) = host.upgrade() else { return };
        match event {
            ToMain::Error(message) => {
                let senders = host
                    .clients
                    .lock()
                    .unwrap()
                    .values()
                    .map(|client| client.events.clone())
                    .collect::<Vec<_>>();
                for sender in senders {
                    let _ = sender.send(SharedEvent::Error(message.clone()));
                }
            }
            ToMain::OutOfMemory => {
                let senders = host
                    .clients
                    .lock()
                    .unwrap()
                    .values()
                    .map(|client| client.events.clone())
                    .collect::<Vec<_>>();
                for sender in senders {
                    let _ =
                        sender.send(SharedEvent::Error("shared worker ran out of memory".into()));
                }
            }
            ToMain::Exited(code) => {
                host.stop.store(true, Ordering::SeqCst);
                host.kill.store(true, Ordering::SeqCst);
                let clients = host
                    .clients
                    .lock()
                    .unwrap()
                    .drain()
                    .map(|(_, client)| client)
                    .collect::<Vec<_>>();
                for client in clients {
                    if code != 0 {
                        let _ = client.events.send(SharedEvent::Error(format!(
                            "shared worker exited with code {code}"
                        )));
                    }
                    let _ = client.events.send(SharedEvent::Closed);
                    client.worker_port.close();
                }
                unpublish_shared_worker(&host);
                return;
            }
            ToMain::Online | ToMain::Message(_) => {}
        }
    }
}

fn remove_shared_client(host: &Weak<SharedWorkerHost>, id: u64) {
    let Some(host) = host.upgrade() else { return };
    let empty = {
        let mut clients = host.clients.lock().unwrap();
        clients.remove(&id);
        clients.is_empty()
    };
    if !empty {
        return;
    }
    host.stop.store(true, Ordering::SeqCst);
    host.kill.store(true, Ordering::SeqCst);
    unpublish_shared_worker(&host);
}

fn unpublish_shared_worker(host: &Arc<SharedWorkerHost>) {
    shared_workers()
        .lock()
        .unwrap()
        .retain(|_, current| !Arc::ptr_eq(current, host));
}

fn publish_shared_worker_final_url(spec: &WorkerSpec, final_url: &str) {
    let (Some(key), Some(host)) = (
        spec.shared_key.as_ref(),
        spec.shared_host.as_ref().and_then(Weak::upgrade),
    ) else {
        return;
    };
    if host.stop.load(Ordering::SeqCst) {
        return;
    }
    let Some(final_origin) = lumen_common::url::parse(final_url, None)
        .ok()
        .map(|url| shared_url_origin(&url))
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

struct SharedInbox {
    id: u64,
    rx: Receiver<SharedEvent>,
}

struct SharedInboxResult {
    id: u64,
    event: SharedEvent,
    inbox: Option<SharedInbox>,
}

fn arm_shared_inbox(ctx: &mut Ctx, inbox: SharedInbox) {
    let id = inbox.id;
    let dispatch = match registry(ctx).shared_clients.get(&id) {
        Some(client) => client.dispatch.clone(),
        None => return,
    };
    let task = lumen_host::register_task(ctx, dispatch, None, decode_shared_inbox);
    if let Some(client) = registry(ctx).shared_clients.get_mut(&id) {
        client.inbox_task = Some(task);
    }
    let sender = ctx
        .op_state()
        .get::<CompletionSender>()
        .expect("completion sender installed")
        .clone();
    sender.run_blocking(task, move || {
        let received = inbox.rx.recv();
        let done = matches!(received, Ok(SharedEvent::Closed) | Err(_));
        let event = received.unwrap_or(SharedEvent::Closed);
        Box::new(SharedInboxResult {
            id,
            event,
            inbox: (!done).then_some(inbox),
        })
    });
}

fn decode_shared_inbox(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    let SharedInboxResult { id, event, inbox } = *payload
        .downcast::<SharedInboxResult>()
        .expect("shared inbox payload");
    if let Some(inbox) = inbox {
        arm_shared_inbox(ctx, inbox);
    } else {
        registry(ctx).shared_clients.remove(&id);
    }
    Ok(match event {
        SharedEvent::Error(message) => vec![
            Value::from_string("error".into()),
            Value::from_string(message),
        ],
        SharedEvent::Closed => vec![Value::from_string("close".into())],
    })
}

/// Stop every worker this realm started: its loop, any JS it is running, and its inbox thread.
/// Used when the realm itself is being interrupted or dropped, so no worker outlives it.
pub(crate) fn terminate_all(ctx: &mut Ctx) {
    let (ports, tasks) = if let Some(reg) = ctx.host_mut::<WorkerRegistry>() {
        for w in reg.workers.values_mut() {
            w.stop.store(true, Ordering::SeqCst);
            w.kill.store(true, Ordering::SeqCst);
            w.to_worker = None;
        }
        let ports = reg
            .shared_clients
            .values()
            .map(|client| client.port.clone())
            .collect::<Vec<_>>();
        let tasks = reg
            .shared_clients
            .values()
            .filter_map(|client| client.inbox_task)
            .collect::<Vec<_>>();
        reg.shared_clients.clear();
        (ports, tasks)
    } else {
        (Vec::new(), Vec::new())
    };
    for port in ports {
        port.close();
    }
    if let Some(registry) = ctx.host_mut::<TaskRegistry>() {
        for task in tasks {
            registry.cancel(task);
        }
    }
}

fn arm_main_inbox(ctx: &mut Ctx, inbox: MainInbox) {
    let id = inbox.id;
    let (dispatch, keep_alive) = match registry(ctx).workers.get(&id) {
        Some(w) => (w.dispatch.clone(), w.keep_alive),
        None => return, // already gone
    };
    let task = lumen_host::register_task(ctx, dispatch, None, decode_main_inbox);
    let reg = ctx
        .host_mut::<TaskRegistry>()
        .expect("task registry installed");
    if !keep_alive {
        reg.set_unref(task);
    }
    if let Some(w) = registry(ctx).workers.get_mut(&id) {
        w.inbox_task = Some(task);
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
        ToMain::Message(message) => {
            let bytes = clone_transfer::install_message(ctx, message);
            let arr = ctx.make_uint8array(&bytes)?;
            Ok(vec![Value::from_string("message".into()), arr])
        }
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

/// Per-worker state living in the worker Runtime's `OpState`: how to reach the main thread, the
/// shared stop flag `self.close()` / `process.exit()` / the main's `terminate()` set, the exit
/// code, and the inbox ref bookkeeping (Node's parentPort ref semantics).
struct WorkerSelf {
    to_main: Sender<ToMain>,
    stop: Arc<AtomicBool>,
    exit_code: Option<i32>,
    /// Whether the armed inbox keeps the worker loop alive. Web workers: always (a worker waits
    /// for messages until closed). Node workers: only while parentPort has a 'message' listener.
    keep_alive: bool,
    is_shared: bool,
    is_module: bool,
    inbox_task: Option<TaskId>,
    /// The realm's engine interrupt (absent for an embedded parent's workers, which share the
    /// embedder's): `process.exit()` raises it so the calling JS stops at once, as in Node.
    kill: Option<Arc<AtomicBool>>,
}

// Declared before Runtime so every normal return drops the runtime and its host
// roots before collecting. Publish exit only after its realm has been reclaimed.
struct WorkerExit {
    parent: Sender<ToMain>,
    code: i32,
    out_of_memory: bool,
}
impl Drop for WorkerExit {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            lumen::collect_disposed_realms();
        }
        if self.out_of_memory {
            let _ = self.parent.send(ToMain::OutOfMemory);
        }
        let _ = self.parent.send(ToMain::Exited(self.code));
    }
}

/// The worker thread's whole lifecycle: build a realm, run the entry, then pump the loop until
/// stopped (or, node mode, idle), bridging messages both ways.
fn run_worker(
    mut spec: WorkerSpec,
    to_worker_rx: Receiver<ToWorker>,
    to_main_tx: Sender<ToMain>,
    stop: Arc<AtomicBool>,
    kill: Arc<AtomicBool>,
) {
    // An embedded parent's workers share its interrupt (the embedder ends them together);
    // otherwise `terminate()` interrupts this realm's JS directly.
    let mut exit = WorkerExit {
        parent: to_main_tx.clone(),
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
    lumen_host::install(rt.engine(), &[worker_scope_extension(!spec.is_node)]);
    if !spec.is_node && lumen_html_js::install_css_typed_om(rt.engine().ctx()).is_err() {
        let _ = to_main_tx.send(ToMain::Error(
            "worker CSS Typed OM installation failed".to_string(),
        ));
        return;
    }
    if !spec.is_node && lumen_html_js::install_worker_canvas(rt.engine().ctx()).is_err() {
        let _ = to_main_tx.send(ToMain::Error(
            "worker canvas installation failed".to_string(),
        ));
        return;
    }
    rt.engine().ctx().op_state().put(WorkerSelf {
        to_main: to_main_tx.clone(),
        stop: Arc::clone(&stop),
        exit_code: None,
        keep_alive: !spec.is_node,
        is_shared: spec.is_shared,
        is_module: spec.is_module,
        inbox_task: None,
        kill: (!embedded).then(|| Arc::clone(&kill)),
    });

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
                let _ = to_main_tx.send(ToMain::Error(format!(
                    "cannot fetch worker script: {error}"
                )));
                return;
            }
        };
        let response_origin = lumen_common::url::parse(&resource.url, None)
            .ok()
            .map(|url| shared_url_origin(&url));
        if spec.origin.as_deref() != response_origin.as_deref() {
            let _ = to_main_tx.send(ToMain::Error(format!(
                "worker redirect left its origin: {}",
                resource.url
            )));
            return;
        }
        if !lumen_web::is_javascript_module_mime(resource.content_type.as_deref()) {
            let _ = to_main_tx.send(ToMain::Error(format!(
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
    // node workers. Evaluated only in worker realms, so it lives here rather than in shared glue.
    let boot = if spec.is_node {
        let ctx = rt.engine().ctx();
        let global = ctx.global_this();
        let _ = ctx.set_member(
            &global,
            "__lumenWorkerThreadId",
            Value::Num(spec.thread_id as f64),
        );
        if let Some((public, internal)) = spec.ports.take() {
            let public = crate::ports::adopt(ctx, public);
            let internal = crate::ports::adopt(ctx, internal);
            let ids = ctx.make_array(vec![Value::Num(public as f64), Value::Num(internal as f64)]);
            let _ = ctx.set_member(&global, "__lumenWorkerPorts", ids);
        }
        if let Some(message) = spec.init.take() {
            let bytes = clone_transfer::install_message(ctx, message);
            match ctx.make_uint8array(&bytes) {
                Ok(arr) => {
                    let _ = ctx.set_member(&global, "__lumenWorkerInit", arr);
                }
                Err(_) => {
                    let _ = to_main_tx.send(ToMain::Error("worker init payload failed".into()));
                    return;
                }
            }
        }
        NODE_WORKER_SCOPE_JS
    } else {
        let ctx = rt.engine().ctx();
        let global = ctx.global_this();
        let href = remote_entry
            .as_ref()
            .map(|resource| resource.url.as_str())
            .or(spec.location_href.as_deref())
            .unwrap_or(&spec.entry);
        let _ = ctx.set_member(
            &global,
            "__lumenWorkerLocationHref",
            Value::from_string(href.to_owned()),
        );
        let _ = ctx.set_member(
            &global,
            "__lumenWorkerType",
            Value::from_string(if spec.is_module { "module" } else { "classic" }.into()),
        );
        let _ = ctx.set_member(
            &global,
            "__lumenWorkerName",
            Value::from_string(spec.name.clone()),
        );
        let _ = ctx.set_member(&global, "__lumenWorkerShared", Value::Bool(spec.is_shared));
        WORKER_SCOPE_JS
    };
    if rt.engine().eval(boot, false).is_err() {
        if rt.engine().heap_limit_hit() {
            exit.out_of_memory = true;
        } else {
            let _ = to_main_tx.send(ToMain::Error("worker scope bootstrap failed".into()));
        }
        return;
    }
    if spec.is_shared && rt.engine().eval(SHARED_WORKER_SCOPE_JS, false).is_err() {
        if rt.engine().heap_limit_hit() {
            exit.out_of_memory = true;
        } else {
            let _ = to_main_tx.send(ToMain::Error("shared worker scope bootstrap failed".into()));
        }
        return;
    }

    if !spec.is_node {
        rt.enable_worker_rejection_events();
    }

    let _ = to_main_tx.send(ToMain::Online);

    if spec.is_node {
        run_node_worker(&mut rt, &spec, to_worker_rx, &stop, &kill);
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
            exit.code = worker_exit_code(&mut rt).unwrap_or(1);
            return;
        }
        let _ = to_main_tx.send(ToMain::Error(e));
        if spec.is_shared {
            // A failed shared-worker entry never reaches a usable SharedWorkerGlobalScope. Send
            // the error first, then close every client through the normal exit path.
            stop.store(true, Ordering::SeqCst);
        }
    }

    arm_worker_inbox(rt.engine().ctx(), WorkerInbox { rx: to_worker_rx });
    rt.run_worker_loop(&stop);

    exit.code =
        worker_exit_code(&mut rt).unwrap_or(if spec.is_shared || !stop.load(Ordering::SeqCst) {
            0
        } else {
            1
        });
}

/// A node worker: the entry runs as the realm's main program (a throw is an uncaught exception
/// the worker's own `process` handlers see), then the loop, then Node's end-of-thread protocol
/// (`'beforeExit'` while listeners add work, then `'exit'`) when it ran out of work by itself.
fn run_node_worker(
    rt: &mut Runtime,
    spec: &WorkerSpec,
    to_worker_rx: Receiver<ToWorker>,
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
    arm_worker_inbox(rt.engine().ctx(), WorkerInbox { rx: to_worker_rx });
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

struct WorkerInbox {
    rx: Receiver<ToWorker>,
}

enum WorkerInboxEvent {
    Message(CloneMessage),
    Connect(crate::ports::PortTransfer),
}

struct WorkerInboxResult {
    event: Option<WorkerInboxEvent>,
    inbox: Option<WorkerInbox>,
}

fn arm_worker_inbox(ctx: &mut Ctx, inbox: WorkerInbox) {
    // The JS dispatcher is a stable global installed by the scope bootstrap.
    let global = ctx.global_this();
    let shared = ctx
        .op_state()
        .get::<WorkerSelf>()
        .is_some_and(|worker| worker.is_shared);
    let dispatch_name = if shared {
        "__sharedWorkerDispatchEvent"
    } else {
        "__workerDispatchMessage"
    };
    let dispatch = match ctx.get_member(&global, dispatch_name) {
        Ok(v) if v.is_callable() => v,
        _ => return,
    };
    let keep_alive = ctx
        .op_state()
        .get::<WorkerSelf>()
        .map(|w| w.keep_alive)
        .unwrap_or(true);
    let task = lumen_host::register_task(ctx, dispatch, None, decode_worker_inbox);
    let reg = ctx
        .host_mut::<TaskRegistry>()
        .expect("task registry installed");
    if !keep_alive {
        reg.set_unref(task);
    }
    if let Some(w) = ctx.host_mut::<WorkerSelf>() {
        w.inbox_task = Some(task);
    }
    let sender = ctx
        .op_state()
        .get::<CompletionSender>()
        .expect("completion sender installed")
        .clone();
    sender.run_blocking(task, move || {
        let (event, keep) = match inbox.rx.recv() {
            Ok(ToWorker::Data(message)) => (Some(WorkerInboxEvent::Message(message)), true),
            Ok(ToWorker::Connect(port)) => (Some(WorkerInboxEvent::Connect(port)), true),
            Err(_) => (None, false), // main dropped the sender (terminate)
        };
        Box::new(WorkerInboxResult {
            event,
            inbox: keep.then_some(inbox),
        })
    });
}

fn decode_worker_inbox(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    let WorkerInboxResult { event, inbox } = *payload
        .downcast::<WorkerInboxResult>()
        .expect("worker inbox payload");
    if let Some(inbox) = inbox {
        arm_worker_inbox(ctx, inbox);
    }
    match event {
        Some(WorkerInboxEvent::Message(message)) => {
            let bytes = clone_transfer::install_message(ctx, message);
            let arr = ctx.make_uint8array(&bytes)?;
            Ok(vec![arr])
        }
        Some(WorkerInboxEvent::Connect(port)) => {
            let id = crate::ports::adopt(ctx, port);
            Ok(vec![
                Value::from_string("connect".into()),
                Value::Num(id as f64),
            ])
        }
        // Channel closed (terminate): fire nothing; the loop exits via the stop flag.
        None => Ok(vec![Value::Bool(false)]),
    }
}

#[lumen_bind::module(name = "__wself")]
mod worker_scope_ops {
    use super::*;

    /// `__wself.post(u8array)` — send an already-serialized message to the main thread.
    #[op(name = "post")]
    fn op_wself_post(ctx: &mut Ctx, bytes: &[u8]) {
        let message = clone_transfer::take_message(ctx, bytes.to_vec());
        if let Some(w) = ctx.host_mut::<WorkerSelf>() {
            let _ = w.to_main.send(ToMain::Message(message));
        }
    }

    /// `__wself.close()` — request the worker loop to stop.
    #[op(name = "close")]
    fn op_wself_close(ctx: &mut Ctx) {
        if let Some(w) = ctx.host_mut::<WorkerSelf>() {
            w.stop.store(true, Ordering::SeqCst);
        }
    }

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

    /// `__wself.setRef(keep)` — whether the armed inbox keeps this worker alive (parentPort ref
    /// semantics: on while a 'message' listener exists, off otherwise).
    #[op(name = "setRef", coerce)]
    fn op_wself_set_ref(ctx: &mut Ctx, keep: Option<bool>) {
        let keep = keep.unwrap_or(false);
        let task = match ctx.host_mut::<WorkerSelf>() {
            Some(w) => {
                w.keep_alive = keep;
                w.inbox_task
            }
            None => None,
        };
        if let (Some(task), Some(reg)) = (task, ctx.host_mut::<TaskRegistry>()) {
            if keep {
                reg.set_ref(task);
            } else {
                reg.set_unref(task);
            }
        }
    }

    /// `__wself.report(message)` — forward a worker-side uncaught error to the main thread (wired to
    /// the worker's global `onerror` in the bootstrap).
    #[op(name = "report", coerce)]
    fn op_wself_report(ctx: &mut Ctx, message: String) {
        if let Some(w) = ctx.host_mut::<WorkerSelf>() {
            let _ = w.to_main.send(ToMain::Error(message));
        }
    }

    /// Synchronously fetch one classic `importScripts()` resource with the worker's captured route
    /// and trust configuration. Cross-origin imports are intentionally allowed; the HTML algorithm
    /// requires a successful response and a JavaScript MIME type but does not apply the initial
    /// Worker constructor's same-origin restriction here.
    #[op(name = "loadClassicScript", coerce)]
    fn op_wself_load_classic_script(ctx: &mut Ctx, raw_url: String) -> Result<Value, Value> {
        if ctx
            .op_state()
            .get::<WorkerSelf>()
            .is_some_and(|worker| worker.is_module)
        {
            return Err(ctx.make_error(
                "TypeError",
                "importScripts is unavailable in module workers",
            ));
        }
        let mut url = lumen_common::url::parse(&raw_url, None)
            .map_err(|error| ctx.make_error("NetworkError", error))?;
        if !matches!(url.scheme.as_str(), "http" | "https") {
            return Err(ctx.make_error(
                "NetworkError",
                format!(
                    "unsupported importScripts URL scheme '{}'; expected HTTP(S)",
                    url.scheme
                ),
            ));
        }
        if !url.username.is_empty() || !url.password.is_empty() {
            return Err(ctx.make_error(
                "NetworkError",
                "importScripts URL cannot contain credentials",
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
            .map_err(|error| ctx.make_error("NetworkError", error))?;
        if !lumen_web::is_javascript_module_mime(resource.content_type.as_deref()) {
            return Err(ctx.make_error(
                "NetworkError",
                format!(
                    "importScripts resource has a non-JavaScript MIME type: {}",
                    resource.content_type.as_deref().unwrap_or("(missing)")
                ),
            ));
        }
        let source = String::from_utf8_lossy(&resource.bytes).into_owned();
        let result = Value::Obj(ctx.new_object());
        let _ = ctx.set_member(&result, "source", Value::from_string(source));
        let _ = ctx.set_member(&result, "url", Value::from_string(resource.url));
        Ok(result)
    }
}

fn worker_scope_extension(include_browser_script_op: bool) -> Extension {
    Extension {
        name: "worker-scope",
        modules: if include_browser_script_op {
            &[
                lumen_host::namespace::<worker_script_bindings::Module>,
                lumen_host::namespace::<worker_scope_ops::Module>,
            ]
        } else {
            &[lumen_host::namespace::<worker_scope_ops::Module>]
        },
        state_init: None,
        js_init: None,
        js_init_snapshot: None,
    }
}

#[lumen_bind::module(name = "__worker")]
mod worker_ops {
    use super::*;

    /// `__worker.spawn(path, isModule, dispatch, opts?)` → `{ id, threadId }`. Spawns the worker
    /// thread and arms the worker→main inbox. `dispatch(kind, ...)` receives `("online")`,
    /// `("message", u8array)`, `("error", string)`, or `("exit", code)`. `opts` (node mode):
    /// `{ node: true, eval: bool, init: Uint8Array }`.
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
        let is_node = opt_bool(ctx, "node");
        let is_web = opt_bool(ctx, "web");
        let is_eval = opt_bool(ctx, "eval");
        let name = if has_opts {
            ctx.get_member(&opts, "name")
                .ok()
                .map(|value| ctx.coerce_string(&value).map(|value| value.to_string()))
                .transpose()?
                .unwrap_or_default()
        } else {
            String::new()
        };
        let (entry, is_remote, origin, location_href) = if is_web {
            if is_eval || is_node {
                return Err(
                    ctx.make_error("TypeError", "web Worker cannot use eval or node options")
                );
            }
            let owner_origin = ctx
                .get_member(&opts, "ownerOrigin")
                .ok()
                .and_then(|value| ctx.coerce_string(&value).ok())
                .map(|value| value.to_string())
                .unwrap_or_default();
            prepare_web_worker_entry(ctx, &raw_entry, &owner_origin)?
        } else {
            (raw_entry, false, None, None)
        };
        let init = if has_opts {
            ctx.get_member(&opts, "init")
                .ok()
                .and_then(|v| ctx.typed_array_bytes(&v))
        } else {
            None
        };

        let limits = if is_node && has_opts {
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

        let shared_env = if is_node && opt_bool(ctx, "shareEnv") {
            Some(crate::process_env::backing(ctx))
        } else {
            None
        };
        let init = init.map(|bytes| clone_transfer::take_message(ctx, bytes));
        let (to_worker_tx, to_worker_rx) = channel::<ToWorker>();
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
                    to_worker: Some(to_worker_tx),
                    dispatch: dispatch.clone(),
                    stop: Arc::clone(&stop),
                    kill: Arc::clone(&kill),
                    keep_alive: true,
                    inbox_task: None,
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
        let mut parent_ports = None;
        let ports = is_node.then(|| {
            let (public_main, public_worker) = crate::ports::new_pair();
            let (internal_main, internal_worker) = crate::ports::new_pair();
            parent_ports = Some((
                crate::ports::adopt(ctx, public_main),
                crate::ports::adopt(ctx, internal_main),
            ));
            (public_worker, internal_worker)
        });
        let spec = WorkerSpec {
            entry,
            is_module,
            is_node,
            is_shared: false,
            is_remote,
            is_eval,
            location_href,
            name,
            init,
            thread_id,
            embedding,
            fetch_config,
            origin,
            shared_key: None,
            shared_host: None,
            shared_env,
            ports,
            limits,
        };
        let worker_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name(format!("lumen-worker-{thread_id}"))
            // Same reasoning as the CLI's main thread: the engine recurses natively, and a debug
            // build's frames overflow the 2 MiB default long before the depth guard trips.
            .stack_size(lumen::THREAD_STACK_SIZE)
            .spawn(move || {
                lumen::set_thread_stack_size(lumen::THREAD_STACK_SIZE);
                run_worker(spec, to_worker_rx, to_main_tx, worker_stop, kill)
            })
            .expect("spawn worker thread");

        arm_main_inbox(ctx, MainInbox { id, rx: to_main_rx });
        let o = Value::Obj(ctx.new_object());
        let _ = ctx.set_member(&o, "id", Value::Num(id as f64));
        let _ = ctx.set_member(&o, "threadId", Value::Num(thread_id as f64));
        if let Some((public, internal)) = parent_ports {
            let _ = ctx.set_member(&o, "port", Value::Num(public as f64));
            let _ = ctx.set_member(&o, "internal", Value::Num(internal as f64));
        }
        Ok(o)
    }

    /// `__worker.post(id, u8array)` — enqueue an already-serialized message for the worker.
    #[op(name = "post", coerce)]
    fn op_worker_post(ctx: &mut Ctx, id: f64, bytes: &[u8]) {
        let message = clone_transfer::take_message(ctx, bytes.to_vec());
        if let Some(w) = registry(ctx).workers.get(&(id as u64)) {
            if let Some(tx) = &w.to_worker {
                let _ = tx.send(ToWorker::Data(message));
            }
        }
    }

    /// `__worker.terminate(id)` — set the shared stop flag and drop the worker's inbox sender (so a
    /// blocked receive unblocks). The worker loop exits at its next poll and posts `Exited`, which is
    /// still delivered (the registry entry lives until then) so `'exit'` fires with the code.
    #[op(name = "terminate", coerce)]
    fn op_worker_terminate(ctx: &mut Ctx, id: f64) {
        if let Some(w) = registry(ctx).workers.get_mut(&(id as u64)) {
            w.stop.store(true, Ordering::SeqCst);
            w.kill.store(true, Ordering::SeqCst);
            w.to_worker = None; // drops the sender, unblocking the worker's inbox receive
        }
    }

    /// `__worker.setRef(id, keep)` — `worker.ref()/unref()`: whether this worker's inbox keeps the
    /// main loop alive. Applies to the in-flight inbox task and every re-arm after it.
    #[op(name = "setRef", coerce)]
    fn op_worker_set_ref(ctx: &mut Ctx, id: f64, keep: Option<bool>) {
        let keep = keep.unwrap_or(false);
        let task = match registry(ctx).workers.get_mut(&(id as u64)) {
            Some(w) => {
                w.keep_alive = keep;
                w.inbox_task
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

/// The main-thread `Worker` class + `__worker` op capture — the runtime extension's js_init.
pub(crate) fn extension() -> Extension {
    Extension {
        name: "worker",
        modules: &[
            lumen_host::namespace::<worker_ops::Module>,
            lumen_host::namespace::<shared_worker_bindings::Module>,
        ],
        state_init: Some(|state| state.put(WorkerRegistry::default())),
        js_init: None,
        js_init_snapshot: Some(WORKER_AOT),
    }
}

/// js/worker.js, precompiled by build.rs: the main-side web `Worker` class. Captures `__worker` (removing it from global scope, but
/// stashing a hidden handle for the node glue's worker_threads Worker, whose js_init ran earlier
/// and grabs the ops lazily on first use) and drives the message bridge through the
/// structured-clone wire format.
const WORKER_AOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/worker.aot"));

/// The DedicatedWorkerGlobalScope surface, evaluated inside each *web* worker realm (see
/// `run_worker`). Evaluated from source: loading a precompiled blob in every worker costs more
/// memory per worker than parsing these few lines.
const WORKER_SCOPE_JS: &str = include_str!("js/worker_scope.js");

/// The `SharedWorkerGlobalScope` surface evaluated inside one shared worker realm.
const SHARED_WORKER_SCOPE_JS: &str = include_str!("js/shared_worker_scope.js");

/// The node worker bootstrap, evaluated (from source, like the web scope) inside each *node*
/// worker realm: hands the `__wself` ops, the thread id, and the init payload to the hook the node glue installed
/// (`__lumenInitWorkerThread`, see lumen-node/src/js/worker_threads.js), which wires parentPort/
/// workerData/process and returns the inbox dispatcher.
const NODE_WORKER_SCOPE_JS: &str = include_str!("js/node_worker_scope.js");
