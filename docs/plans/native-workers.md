# Plan: native workers

Status: steps 1-4 implemented (owner loop, port limits, messaging extension, native receivers,
native `Worker`/`SharedWorker` and worker global scopes in `lumen-runtime`); steps 5-8 are design. Goal: delete the last JavaScript worker glue and run one
native implementation of `Worker`, `SharedWorker`, the service-worker classes and the worker
global scopes in both `lumen-runtime` and the Bitnest kernel.

## What exists today (checked against the source)

| Piece | Where | Notes |
| --- | --- | --- |
| `service_worker.js` | `crates/lumen-web/src/js/` | `ServiceWorker`, `ServiceWorkerRegistration`, `ServiceWorkerContainer`, `navigator.serviceWorker`, `__bitnestPumpServiceWorkerContainer`; worker-side `__bitnestWorker*` hooks. Not built by `lumen-web` (no `build.rs` glue left); only Bitnest's `crates/kernel/runtime-services/build.rs` concatenates it into `html-byte-types.aot` behind `__bitnestInstallFetch`. Registry state is read by re-parsing `registrations()` JSON every pump and diffing installer ids. |
| `shared_worker.js` | same | Second `MessagePort` (+ `SharedWorkerPort`), a JS reviver for `__lumenHostAttachmentIndex`, `SharedWorker`, `MessageChannel` over `__sharedWorker.message_channel`, connect/close dispatch. Duplicates `lumen_host::messaging::channel` and the port half of `structured_clone`/`clone_transfer`. |
| Kernel transport | `runtime-services/src/shared_worker.rs` | Own endpoint registry (`Port { open, started, queues, reserved }`, 128 messages/port, 1 MiB/message), `PortObjectRegistry`, transfer-list parsing, `lumen::parallel::Parcel` instead of the structured-clone wire, `pump_bindings` called from `JsRuntime::pump_html_browser_services`. |
| Kernel SW | `runtime-services/src/service_worker.rs`, `js/service_worker_scope.js` | Host registry (`ServiceWorkerHost`, `Rc<RefCell<Registry>>`), Parcel messages, JSON fetch events; a hand-written listener map and plain-object events on the worker global. |
| Kernel managers | `kernel/runtime/src/services/{shared_worker,service_worker}.rs` | Fetch scripts, create `RealmHost` fibers, call `dispatch_shared_worker_connect` / `dispatch_shared_worker_messages` / `dispatch_service_worker_*`, `pump_timers(32)` per worker. |
| `lumen-runtime` page | `src/js/worker.js` (AOT via `build.rs` `SCRIPTS`), `src/worker.rs` | `Worker`, `SharedWorker` (JS classes over `__worker` / `__lumenSharedWorker` ops). `Worker.postMessage` ignores the transfer list. `__lumenWorkerOps` is the hidden handle `lumen-node`'s `worker_threads.js` reads. |
| `lumen-runtime` scope | `src/js/worker_scope.js`, `shared_worker_scope.js`, `node_worker_scope.js` | Scope interfaces built in JS, `Symbol.hasInstance` overrides on `EventTarget` and the scope classes, `setPrototypeOf(globalThis, ...)`, `initEventTarget(globalThis)`, `__workerDispatchMessage`, `__sharedWorkerDispatchEvent`, `__workerDispatchRejection` (read by `Runtime::run_worker_rejection_task`). Self `postMessage` ignores the transfer list. |
| Service workers in `lumen-runtime` | none | `grep ServiceWorker` finds only the kernel, `lumen-web/src/js/service_worker.js`, `messaging/events.rs` (the `source` check) and `perf_hooks.js`. |
| Native ports | `lumen-host/src/ports.rs`, `messaging/channel.rs` | `Endpoint` is a concrete `Arc` pair over `lumen_os::channel::Queue` (unbounded); `PortTransfer` is `Send` and can be adopted by any realm of the process. Delivery wakes a `TaskRegistry` stream task through the realm's `CompletionSender`; `ports::listen` fails with "MessagePort requires an event loop" when `OpState` has no `CompletionSender`. There is no port backend trait; the only seam is `CompletionSender` + `TaskRegistry`. |
| Kernel loop | `runtime-services/src/lib.rs` | No `TaskRegistry`/`CompletionSender`. Wakes come from `Ctx::set_async_waker` (fallback `AsyncInbox`, drained by `poll_async` in `pump_engine_timers`) and the shell's `notify`; `next_timer_delay_ms` returns `Some(0)` when ready work exists. Idle realms are not polled. |

Kernel realms share one address space (the shell's pinned owner runs every `RealmHost` fiber), so
`PortTransfer`, `CloneMessage` and `SharedBufferHandle` move between kernel engines exactly as
they move between `lumen-runtime` threads. That makes the kernel's own endpoint registry
unnecessary: no port backend trait is needed, only a wake path.

## Decisions

### 1. One class surface: `lumen_host::workers`

New module tree in `lumen-host` (it sits below `lumen-runtime`, `lumen-web` and the kernel and
already owns `EventTarget`, ports, clone and `Navigator`):

| Module | Contents |
| --- | --- |
| `workers::control` | `Control`: a `Send` mailbox of `WorkerEvent`s for one realm-side object, woken through the realm's `CompletionSender` (same coalescing as `ports::Endpoint::wake`; the shared part is extracted into `ports::Mailbox<T>` so ports and controls have one waker). |
| `workers::backend` | `WorkerBackend`, `ServiceWorkerRegistry`, `WorkerScopeHost` traits; `set_backend` / `set_scope_host` (a `RealmServices` entry per realm). |
| `workers::page` (`page_bindings`) | `Worker`, `SharedWorker`, `ServiceWorker`, `ServiceWorkerRegistration`, `ServiceWorkerContainer`. |
| `workers::scope` (`scope_bindings`) | `WorkerGlobalScope`, `DedicatedWorkerGlobalScope`, `SharedWorkerGlobalScope`, `WorkerLocation`; `install_scope`. |
| `workers::service_scope` | `ServiceWorkerGlobalScope`, `ExtendableEvent`, `ExtendableMessageEvent`, `FetchEvent`, `Clients`, `Client`, `WindowClient`. |
| `navigator` | `Navigator.serviceWorker` getter (see 4). |

The backend owns registry, transport of control events and script loading. Scheduling is not a
backend method: every host supplies the same primitive, a `CompletionSender` + `TaskRegistry` in
`OpState` (the `lumen-runtime` loop already does; the kernel gets one from `OwnerLoop`, section 3).

```rust
pub trait WorkerBackend: 'static {
    fn spawn_dedicated(&self, ctx: &mut Ctx, spec: DedicatedSpec) -> NativeResult<u64>; // default: NotSupportedError
    fn terminate(&self, id: u64);
    fn set_ref(&self, id: u64, keep: bool);
    fn connect_shared(&self, ctx: &mut Ctx, key: SharedWorkerKey, worker_side: PortTransfer, control: Control) -> NativeResult<u64>;
    fn disconnect_shared(&self, id: u64);
    fn service_workers(&self) -> Option<Rc<dyn ServiceWorkerRegistry>> { None }
}
pub struct DedicatedSpec { pub url: String, pub module: bool, pub name: String,
    pub credentials: Credentials, pub owner_origin: Option<String>,
    pub inside: PortTransfer, pub control: Control }

pub trait ServiceWorkerRegistry: 'static {
    fn register(&self, client: &ClientInfo, script: &str, scope: Option<&str>, module: bool, via_cache: UpdateViaCache) -> NativeResult<JobId>;
    fn update(&self, client: &ClientInfo, registration: u64) -> NativeResult<JobId>;
    fn unregister(&self, client: &ClientInfo, registration: u64) -> bool;
    fn registrations(&self, origin: &str) -> Vec<RegistrationRecord>;
    fn controller(&self, client: &ClientInfo) -> Option<WorkerRecord>;
    fn post_to_worker(&self, client: &ClientInfo, worker: u64, message: CloneMessage) -> NativeResult<()>;
    fn subscribe(&self, client: &ClientInfo, control: Control); // pushes WorkerEvent::{StateChange, UpdateFound, ControllerChange, Message, JobSettled}
}

pub trait WorkerScopeHost: 'static {
    fn kind(&self) -> ScopeKind;           // Dedicated | Shared | Service
    fn location(&self) -> String;          // final response URL
    fn name(&self) -> String;
    fn close(&self);
    fn load_classic_script(&self, ctx: &mut Ctx, url: &str) -> NativeResult<(String, String)>; // importScripts: (final url, source)
    fn report_error(&self, message: String);
    fn service(&self) -> Option<Rc<dyn ServiceScopeHost>> { None } // skipWaiting, clients, fetch responses
}
```

`WorkerEvent` is a `Send` enum: `Message(CloneMessage)` (service-worker container only),
`Error(String)`, `Online`, `Exit(i32)`, `Close`, `Connect(PortTransfer)`,
`StateChange { worker, state }`, `UpdateFound { registration }`, `ControllerChange`,
`JobSettled { job, result: Result<RegistrationRecord, (String /*DOMException name*/, String)> }`,
`Fetch(FetchRequest)`, `Lifecycle(Install | Activate)`.

Which JS moves into this module (all of it is deleted):

- `lumen-runtime/src/js/worker.js` → `workers::page::{Worker, SharedWorker}`. `__lumenWorkerOps`
  stays as the `__worker` namespace: `lumen-node`'s `worker_threads.js` keeps calling
  `spawn(path, isModule, dispatch, opts)` for node mode; the native `Worker` calls the same Rust
  function through `WorkerBackend::spawn_dedicated`. `worker_browser.rs` becomes a backend whose
  `spawn_dedicated` / `connect_shared` return the current `unsupported` errors.
- `worker_scope.js`, `shared_worker_scope.js` → `workers::scope`. The global gets its native state
  with `Ctx::attach_instance(&global, DedicatedWorkerGlobalScope { .. })` (what
  `lumen-html-js` does for `Window`), so the prototype chain and every `EventTarget` brand check
  are native and the `Symbol.hasInstance` overrides, `setPrototypeOf` and `initEventTarget(globalThis)`
  go away. `__workerDispatchRejection` becomes a Rust function the runtime calls directly.
- `node_worker_scope.js` (16 lines): replaced by Rust that calls `__lumenInitWorkerThread` with the
  same arguments and stores `__lumenWorkerHooks`; node mode keeps its own glue in `lumen-node`.
- `lumen-web/src/js/service_worker.js`, the kernel's `js/service_worker_scope.js` →
  `workers::page` service classes and `workers::service_scope`.
- `lumen-web/src/js/shared_worker.js` → nothing new: `MessagePort`/`MessageChannel` are the
  existing native classes, `SharedWorker` is `workers::page::SharedWorker`.

Dedicated workers get the HTML "implicit port": the `Worker` holds the outside endpoint of a
`ports::new_pair()`, the scope holds the inside one. `postMessage` on either side is
`messaging::channel::post_link` (transfer lists now honoured); delivery uses a native receiver
(section 2) that dispatches the `MessageEvent` on the `Worker` / the global instead of a
`MessagePort`. Errors, `online` and `exit` travel on the `Control`. Node mode keeps its
`ToMain`/`ToWorker` bytes path for this plan (its ref/exit-code semantics are tested by
`tests/worker_threads.rs`); moving it onto the pair is a follow-up.

### 2. Native `MessagePort` over a kernel-routed endpoint

No port backend trait. The kernel adopts `lumen_host::ports` endpoints directly:

- Install `ports::extension()`, `clone_transfer::extension()` and the messaging classes in every
  kernel realm. Add `messaging::extension()` (the list `lumen-web` builds today at
  `lumen-web/src/lib.rs:746`: `channel_bindings` lazily, `shared` namespace,
  `install_port_clone`) so `lumen-web` and the kernel install one list.
- `SharedWorkerHost::connect` creates `ports::new_pair()`; the page adopts one end
  (`messaging::channel::new_port`), the other waits in the registry's `pending_connects` as a
  `PortTransfer` and is adopted by the worker realm on its next turn (`Connect` event →
  `MessageEvent("connect", { ports: [port] })`). Transfer between kernel realms is the normal
  `CloneAttachment::Port` path; `structured_clone` handles port export/import and
  `event.ports`. The kernel's JS `MessagePort`, reviver, `PortObjectRegistry`,
  `split_transfer_list`, `build_parcel`, `dispatch_port_parcel`, `MessageReservation` and the
  `Port` queues are deleted.
- Kernel limits move into `ports`: `ports::set_limits(ctx, PortLimits { max_message_bytes, max_queued })`
  stored in the realm's `Ports` state. `post` checks the wire length and `peer.queue.len()`
  (`lumen_os::channel::Queue::len` exists) and returns a new `Posted::Full`, mapped to
  `QuotaExceededError`; an oversized message is a `DataCloneError`. The `lumen-runtime` default is
  no limit, so its behaviour is unchanged. The serializer gains an optional byte ceiling read from
  the same state, so an oversized graph fails while it is written instead of after.
- Native receiver for implicit ports: `messaging::channel` gets
  `listen_native(ctx, id, target: WeakValue, kind: Receiver, on_close) -> NativeReceiver` where `Receiver` selects the event
  built in `deliver` (`MessageEvent` on a port, on a `Worker`, on a worker global;
  `ExtendableMessageEvent` on a service-worker global). `start_link`, `on_wake`, pinning and the
  reaper are shared; only the dispatch target and event class differ. `Link::on_close` keeps
  serving `SharedWorker`'s disconnect hook.
- Shared-worker client accounting follows `lumen-runtime`: a client is a connection whose page
  end is open (an `on_close` hook on the page `PortTransfer` calls `disconnect_shared`). The
  kernel's "port transferred into the worker no longer counts as a client" rule is dropped.

### 3. Event delivery without an event loop: `OwnerLoop`

Chosen: a bounded pump of the existing completion machinery, driven by the shell's polling.
Rejected: (a) per-feature JS pumps (`__bitnestPump*`): one per feature, diff-polls registry JSON
every turn; (b) `lumen_html_js::scheduling::queue_task`: `lumen-host` cannot depend on
`lumen-html-js`, and ports already speak `CompletionSender`.

```rust
// lumen_host::owner_loop
impl CompletionSender {
    pub fn with_notify(tx: mpsc::Sender<TaskCompletion>, notify: Arc<dyn Fn() + Send + Sync>) -> Self;
}
pub fn install(ctx: &mut Ctx, notify: Arc<dyn Fn() + Send + Sync>);   // TaskRegistry (if absent), CompletionSender::with_notify, receiver + pending counter in OpState
pub fn has_ready(ctx: &mut Ctx) -> bool;                               // pending counter > 0; no syscall, no timer
pub fn pump(engine: &mut Engine, budget: usize) -> Vec<Value>;         // up to `budget` completions, microtask checkpoint after each; returns thrown values
pub fn settle(ctx: &mut Ctx, done: TaskCompletion) -> Option<Settled>; // TaskRegistry::take + async context + decode; shared with Runtime::dispatch
```

- `send` pushes on the `mpsc` channel, increments the counter and calls `notify` (the kernel
  passes the shell's existing waker, `INPUT_EVENT.notify` / the worker manager's notify). Port
  wakes are already coalesced per endpoint (`wake_pending`), so a burst wakes the shell once.
- Idle cost is zero: nothing is armed; a realm with no posted message, connect or registry event
  receives no notify and is not pumped. `JsRuntime::next_timer_delay_ms` returns `Some(0)` while
  `has_ready`, so remaining work (budget exhausted) gets another owner turn.
- Kernel call sites: `pump_html_browser_services` (page) and `pump_timers` (worker realms, which
  `SharedWorkerManager::pump` already calls each turn) call `owner_loop::pump(engine, 32)`;
  errors join the existing timer error strings.
- `Runtime::dispatch` in `lumen-runtime` calls `owner_loop::settle` and keeps its signal/wake
  sentinels and `fire`/reporting. `run_blocking` on an owner-loop sender is not used by any code
  the kernel installs (only `lumen-web` and `lumen-runtime` call it); document it as unsupported
  there.

### 4. Class sets and install points

| Realm | Classes / globals | Install |
| --- | --- | --- |
| Every realm with ports | `MessagePort`, `MessageChannel`, `BroadcastChannel`, `MessageEvent` | `messaging::extension()` (new; `lumen-web` and kernel) |
| Window / page, and `lumen-runtime` main realms | `Worker`, `SharedWorker`, `ServiceWorker`, `ServiceWorkerRegistration`, `ServiceWorkerContainer` | `lazy_globals::<workers::page_bindings::Module>` from `lumen-runtime`'s `worker::extension()` (replaces `js_init_snapshot: WORKER_AOT`) and from the kernel's `install_html`; backend via `workers::set_backend` |
| Dedicated worker | `WorkerGlobalScope`, `DedicatedWorkerGlobalScope`, `WorkerLocation`, `Worker` | `workers::install_scope(ctx, host)`: `attach_instance` on the global, interface objects as non-enumerable lazy globals, `self`/`location`/`name`/`postMessage`/`close`/`importScripts` from the prototypes |
| Shared worker | `WorkerGlobalScope`, `SharedWorkerGlobalScope`, `WorkerLocation`, `onconnect` | same, `ScopeKind::Shared` |
| Service worker | `WorkerGlobalScope`, `ServiceWorkerGlobalScope`, `ExtendableEvent`, `ExtendableMessageEvent`, `FetchEvent`, `Clients`, `Client`, `WindowClient`, `registration`, `clients`, `skipWaiting` | same, `ScopeKind::Service` |

- `navigator.serviceWorker`: a getter on the native `Navigator` (`navigator.rs`), so
  `DomNavigator` inherits it. It returns the realm's `[SameObject]` container (a `RealmServices`
  entry created on first read) when the realm's backend has `service_workers()`, else
  `undefined`. Behaviour change: `"serviceWorker" in navigator` is `true` in every realm (the JS
  defined the property only where the kernel ops existed).
- Container state is native: identity maps (`ServiceWorker` per worker id, registration per
  scope), `ready` deferreds, register/update deferreds keyed by `JobId`. The registry pushes
  events to the subscribed `Control`; there is no `registrations()` JSON diffing.
- `FetchEvent.request` is built like `net::server_request` (new `net::request_from_parts` that
  keeps `mode`, `credentials`, `redirect`), `respondWith` resolves through
  `net::served_response` + `net::read_served_body` (the `Lumen.serve` path), and
  `ExtendableEvent.waitUntil` uses `Ctx::then_value`. Event handlers use `node_handler_get/set`
  as `MessagePort` and `WebSocket` do.

### 5. Kernel layering

- Generic, host-testable: everything above lives in Lumen (`lumen-host`), tested with Lumen's
  tests. No Bitnest code knows about JS classes any more.
- `crates/kernel/runtime-services` (kernel services crate, keeps its host tests): implements
  `WorkerBackend`, `ServiceWorkerRegistry`, `WorkerScopeHost` over the existing
  `SharedWorkerHost` / `ServiceWorkerHost` registries; installs `owner_loop`, ports, messaging and
  `workers` in `install_html` / `install_*_worker_script`; sets `PortLimits` (1 MiB, 128).
  Keeps: worker identity (`SharedWorkerKey` → id, state, errors, script requests), SW registration
  and lifecycle state machine, fetch interception (`http.rs` `attach_service_worker_host`),
  same-origin/credential policy for `connect`/`register`. Loses: port registry, Parcel messaging,
  `__bitnestSharedWorker` / `__bitnestServiceWorker` namespaces, `pump_bindings`,
  `__bitnestInstallFetch` and the `html-byte-types` glue in `build.rs`, `js/service_worker_scope.js`.
- `crates/kernel/runtime/src/services/{shared_worker,service_worker}.rs` (kernel-runtime; no tests
  here): keep script fetching, `RealmHost` creation, time slicing and the per-turn manager loop.
  `dispatch_shared_worker_connect/messages` become `accept_shared_connections(host, id)` (adopt
  pending `PortTransfer`s) plus the owner-loop pump inside `pump_timers`;
  `dispatch_service_worker_event/message` become control events on the SW realm.
- No board identity enters any of this; nothing moves into kernel-core.

## Steps

Each step builds and passes its tests on its own. Lumen steps: `cargo test -p lumen-host`,
`cargo test -p lumen-runtime --test worker_threads`, debug profile, with a timeout. Bitnest steps:
`cargo xtask build --board virt --verify-gui`, then `cargo xtask build --board a7z`.

1. **Owner loop** (lumen) — done. Files: `lumen-host/src/lib.rs` (`CompletionSender::with_notify`,
   `settle`), new `lumen-host/src/owner_loop.rs`, `lumen-runtime/src/lib.rs` (`dispatch` uses
   `settle`). Tests (`lumen-host/src/tests.rs`): `MessageChannel` round trip in a bare `Engine`
   with `owner_loop::install`; `notify` runs once for a burst; `has_ready` false when idle and
   after draining; `pump` respects its budget and leaves the rest ready; dead-port reaping works
   without a runtime. Behaviour: none for existing hosts.
2. **Port limits, messaging extension, native receivers** (lumen) — done. Files: `ports.rs`
   (`PortLimits`, `Posted::Full`, `Mailbox<T>` extracted from `Endpoint`), `structured_clone`
   (byte ceiling), `messaging/channel.rs` (`listen_native`, `Receiver`), `messaging/mod.rs`
   (`install` and `extension()`), `lumen-web/src/lib.rs` (uses it). Tests: full queue →
   `QuotaExceededError`, oversized → `DataCloneError`, native receiver gets `data` + `ports`,
   close fires on the receiver. Behaviour: none without limits.
3. **Native `Worker` and `SharedWorker` page classes** (lumen) — done. Files: new
   `lumen-host/src/workers/{mod,control,backend,page}.rs`; `lumen-runtime/src/worker.rs`
   (backend impl; `WorkerEntry.dispatch` becomes a `Control`, the per-inbox relay thread for web
   workers is replaced by the backend pushing into `Control` from `route`), `worker_browser.rs`,
   `build.rs` (drop `"worker"`), delete `src/js/worker.js`; `lumen/src/snapshot.rs`
   (`roundtrip_real_web_glue` no longer lists `worker.js`). Transport for dedicated messages is
   unchanged in this step (bytes over `ToWorker`/`ToMain`). Tests: the `worker_threads.rs` suite;
   new: `Worker.length`, `onmessage` accessors on the prototype, brand checks, `SharedWorker.port`
   is a native `MessagePort`, `error`/`close` events trusted. Behaviour: classes are Web IDL
   shaped; events trusted.
4. **Native worker global scopes + implicit port** (lumen) — done. Files:
   `lumen-host/src/workers/scope.rs`; `lumen-runtime/src/worker.rs` (`run_worker` calls
   `install_scope`; dedicated messages move to the implicit pair: `ToWorker::Data`,
   `ToMain::Message` and the inbox `Message` arms go for web workers; inside port ref'd for web
   workers so the worker loop stays alive), `lumen-runtime/src/lib.rs`
   (`run_worker_rejection_task` calls Rust), delete `worker_scope.js`, `shared_worker_scope.js`,
   `node_worker_scope.js`; `snapshot.rs` test drops `worker_scope.js`. Tests: existing HTTP worker
   tests (location after redirect, `importScripts`, shared scope + connect); new: transfer of a
   `MessagePort` and an `ArrayBuffer` in both directions of a dedicated worker, `self instanceof
   EventTarget` without `hasInstance` overrides, `Object.getPrototypeOf(self) ===
   DedicatedWorkerGlobalScope.prototype`. Behaviour: transfer lists honoured on `Worker` and
   worker `postMessage`; a message posted before `terminate()` is dropped by the closed port
   (was filtered in JS).
5. **Service-worker classes** (lumen). Files: `lumen-host/src/workers/{page,service_scope}.rs`,
   `navigator.rs`, `net/fetch.rs` (`request_from_parts`). Tests in `lumen-host` with an
   in-memory `ServiceWorkerRegistry` and owner loop: `register` settles on `JobSettled`, `ready`
   resolves on push without polling, `statechange`/`updatefound`/`controllerchange` order,
   `ServiceWorker` identity per worker, `FetchEvent.respondWith` produces status/headers/body,
   `waitUntil` extends, `navigator.serviceWorker === navigator.serviceWorker`, `undefined` without
   a registry. `lumen-web/src/js/*.js` stay (Bitnest still reads them).
6. **Kernel: owner loop, ports, SharedWorker** (Bitnest, bumps `external/lumen`). Files:
   `runtime-services/src/lib.rs` (install owner loop, `ports`/`clone_transfer`/`messaging`
   extensions, `workers` page + scope install, pump calls, `next_timer_delay_ms`),
   `runtime-services/src/shared_worker.rs` (registry keeps identity; backend impl over
   `PortTransfer`; port code deleted), `runtime-services/build.rs` (drop `shared_worker.js`),
   `kernel/runtime/src/services/shared_worker.rs` (`accept_shared_connections`). Tests: the
   SharedWorker tests in `runtime-services` (rewritten against native ports): connect, two clients
   share one worker, `MessageChannel` port transfer page → worker → page, last client close stops
   the worker, queue full → `QuotaExceededError`, idle realm gets no notify. Behaviour:
   `MessageChannel` and `BroadcastChannel` exist in every kernel realm; trusted events; wire is the
   structured-clone format instead of `Parcel`.
7. **Kernel: ServiceWorker** (Bitnest). Files: `runtime-services/src/service_worker.rs`
   (registry implements `ServiceWorkerRegistry` with subscriber `Control`s, `CloneMessage`
   queues; bindings module and `pump_bindings` deleted), `runtime-services/src/lib.rs`
   (`install_service_worker_script` uses `install_scope(Service)`; `dispatch_service_worker_*`
   become control sends; `__bitnestInstallFetch` and the byte-types glue go; `queueMicrotask`
   installed natively), `runtime-services/build.rs` (the `html-byte-types` block is removed),
   delete `runtime-services/js/service_worker_scope.js`,
   `kernel/runtime/src/services/service_worker.rs`. Tests: the existing host tests in
   `service_worker.rs` (claim, skipWaiting, update byte compare) plus page-level ones: `ready`,
   controller `postMessage` with a transferred port, fetch interception through `FetchEvent`.
8. **Cleanup** (lumen, then Bitnest bumps the submodule). Delete
   `lumen-web/src/js/service_worker.js`, `shared_worker.js`; update `docs/native-messaging.md`
   (the kernel no longer has its own transport), `docs/native-events.md` ("Left in JS"),
   `docs/native-network.md` ("Still JavaScript"); add `docs/native-workers.md` from this plan.

## Risks

- **Ordering across two task sources.** Messages arrive on a port task, errors/lifecycle on the
  `Control`. A worker that posts and then throws may report `error` before `message`; HTML allows
  it, `lumen-runtime` tests may assume the old single-channel order. Node mode is left on the old
  path for this reason.
- **Kernel memory bounds.** `Parcel` bounded objects (65 536) as well as bytes; the native wire
  is bounded by bytes only (step 2). Queue bound is checked by the sender against the peer queue,
  so a transferred port keeps the limits of the realm that posts on it.
- **Owner-loop threads.** `CompletionSender::run_blocking` spawns OS threads; nothing the kernel
  installs calls it today, but a future op could. Make it a debug assertion on owner-loop senders.
- **`std::sync::mpsc` and `Mutex` in the kernel PAL.** Already used (`webgpu.rs` uses `mpsc`;
  ports use `Mutex`/`Condvar` through `lumen_os::channel`), but every port wake now crosses them.
- **Shared-worker lifetime change.** Transferring a client's connection port into the worker no
  longer ends that client; the worker lives until every page end closes or is collected.
- **`navigator.serviceWorker` exposure** changes feature detection (`in` is always true).
- **Submodule ordering.** Bitnest reads `lumen-web/src/js/*.js` at build time; the files are
  deleted only after Bitnest stops reading them (step 8), never in the same Lumen commit as an
  API Bitnest still needs.
- **Rejection events in workers.** `run_worker_rejection_task` moves from a JS global to Rust;
  `PromiseRejectionEvent` is already native, so only the call site changes.

## Implementation notes for steps 1 and 2

What differs from the sketch above, and why (checked against the source while implementing):

- **`settle` returns a `Settled`.** `settle(ctx, done) -> Option<Settled>` decodes inside the task's
  async context and returns `Outcome::Call { callback, args }` (success, or a rejection routed to
  `on_err`) or `Outcome::Uncaught(value)`; the caller runs it and calls `Settled::finish(ctx)` to
  restore the outer async context. `Runtime::dispatch` keeps its `SIGNAL_TASK`/`WAKE_TASK`
  sentinels and its `fire`/`report_uncaught`; only the lookup/decode moved.
- **The counter lives in `CompletionSender`, not only in `OpState`.** The sender is `Clone + Send`,
  so the pending counter and `notify` are one `Arc<owner_loop::Wake>` shared by every clone and by
  the realm's receiver (`OwnerLoop` in `OpState`). `install` also sets `CompletionSender` in
  `OpState`, so `ports::listen` needs nothing else.
- **`Mailbox<T>`** (in `ports`) is the extracted waker: a `Queue<T>` plus a coalescing wake
  (`wake`, `bind`, `unbind`). `Endpoint` embeds `Mailbox<CloneMessage>`; `workers::control` will
  embed `Mailbox<WorkerEvent>`.
- **`messaging::install` + `extension()`.** `Extension::modules` is a `&'static [ModuleInit]`, which
  cannot be concatenated from another crate's list, so the shared list is one `ModuleInit`
  (`messaging::install`) that `lumen-web` names in its own list; `messaging::extension()` wraps it
  for hosts that list extensions (the kernel).
- **`Receiver::Custom`.** `Receiver` has `Port`, `Worker`, `WorkerGlobal` (all `MessageEvent`;
  only `Port` fires `close` on the peer's close) and `Custom(fn(&mut Ctx, data, ports) ->
  OpResult<Value>)` for `ExtendableMessageEvent` in step 5. `listen_native` returns a
  `NativeReceiver` the owning class stores; dropping it releases the endpoint through the reaper.
  It has no wrapper to pin, so the owner calls `NativeReceiver::pin` and traces the close hook with
  `NativeReceiver::trace`.
- **Limits are checked twice.** `post_link` asks `ports::is_full` before serializing, so a refused
  post throws `QuotaExceededError` without detaching transferred buffers or ports; `ports::post`
  checks again (another thread may have filled the queue meanwhile) and reports `Posted::Full`,
  which `postMessage` also maps to `QuotaExceededError` (after the detach, in that race only).
  A broadcast member whose queue is full is skipped. The `__lumenPorts.post` op maps `Full` to the
  same error for Node's `worker_threads`.
- **The serializer ceiling** is in the wire `Sink` (`limit`, `overflow`): a write that would pass it
  appends nothing and sets `overflow`, and the writer fails with `DataCloneError` before the next
  value, so a huge buffer is never copied into the wire. Only transported messages
  (`serialize(.., transport = true)`, i.e. `postMessage`) use the realm's limit; local
  `structuredClone` is unbounded.

### Performance

- **Idle is free.** Nothing is armed: no timer, no poll, no thread. `has_ready` is one atomic load
  behind one `OpState` lookup. A realm that received nothing is not pumped.
- **One notify per burst.** `notify` runs only when the pending counter goes 0 to 1. A completion
  stays counted while it runs, so what a callback posts during `pump` (the next message of the same
  port) does not notify again; the host must check `has_ready` after `pump` returns, which the
  kernel's `next_timer_delay_ms` does. Trade-off: leftover work after a budget-limited pump is
  found by the host's own next poll, not by a notify.
- **Wakes allocate nothing in steady state.** A port wake used to box a `PortTransfer` per wake;
  the payload is now a small token recycled through the mailbox (`Waker::spare`), and the reaper
  wake is coalesced with a flag (it used to send one completion per collected port).
- **No script buffer between wire and event.** The native receiver path polls with `poll_raw`
  (`Vec<u8>` straight from the queue) and deserializes from it; the old path built a `Uint8Array`
  and copied it back out. `postMessage` hands the serializer's `Vec<u8>` to `post_owned`, which
  queues it without the `to_vec` copy `post(&[u8])` still does for script callers. Broadcast
  fan-out still clones the bytes once per receiver.
- **One extra wake is avoided per burst.** After delivering a message the receiver re-wakes itself
  only when the queue still holds something or is closed (`ports::rewake`), where it used to wake
  unconditionally (one empty turn per burst).
- **Limit checks.** Without limits a post pays one `Ports` lookup (`limits`) and one compare per
  written value; with `max_queued` set it also locks the peer's queue once per post. The
  `lumen-runtime` default stays unbounded.
- **Remaining costs.** A cross-thread send takes the mailbox's waker mutex and the channel's
  mutex; each message is still its own loop task (one `Weak::upgrade` pair and a native call), which
  is what gives microtask checkpoints between messages.

## Implementation notes for steps 3 and 4

Steps 3 and 4 landed together: the dedicated transport could not stay on `ToWorker`/`ToMain` once
the scopes were native, so the implicit port pair replaced it directly. Not compiled-and-run here:
only compile checks were done (`lumen`, `lumen-host`, `lumen-web`, `lumen-runtime`, `lumen-node`,
`lumen-html-js` tests, `lumen-web` and `lumen-runtime` on wasm32); the new tests have not been run.

What differs from the sketch above:

- **Files.** `lumen-host/src/workers/{mod,control,backend,page,scope}.rs`. `lumen-runtime/src/worker.rs`
  is rewritten; `worker_browser.rs` keeps only the unsupported `__lumenWorkerOps` stub plus the lazy
  page classes (constructing throws `NotSupportedError` through the default backend). Deleted:
  `worker.js`, `worker_scope.js`, `shared_worker_scope.js`, `node_worker_scope.js`; `build.rs` no
  longer lists `worker`; the `roundtrip_real_web_glue` snapshot test reads `error_shim.js` and
  `env_proxy.js` instead.
- **`WorkerBackend`** differs from the sketch: `terminate`, `exited` and `disconnect_shared` take
  `ctx`; `connect_shared` takes a `SharedSpec { key, entry, remote, page_side, worker_side, control }`
  (the backend, not the page class, owns the registry key and client accounting); there is no
  `set_ref` (the page class pins itself) and no `service_workers` yet. The backend is stored in
  `OpState` with `workers::set_backend`. `WorkerScopeHost` supplies `kind`, `location`, `name`,
  `module`, `close`, `load_classic_script` (the runtime's HTTP loader, so `importScripts` stays
  outside `lumen-host`) and `report_error`.
- **`Control`** is an `Arc<Mailbox<WorkerEvent>>` (`Online`, `Error`, `Exit`, `Close`, `Connect`).
  The page object listens with a stream task; the worker thread pushes straight into it, so the
  per-worker relay thread is gone. Dedicated terminate closes the page receiver, which wakes the
  worker through the peer-endpoint wake; shared workers wake through their scope `Control`.
- **Event order.** Before an `error` or `exit` is dispatched the page flushes the port's queued
  messages, so a message posted before an error is not overtaken. The shared page side drains
  every control event in one turn so the worker's `error` then `close` beat the port-close hook.
- **Node `worker_threads` is unchanged in shape** but its bootstrap moved into Rust
  (`boot_node_worker` calls `__lumenInitWorkerThread` with `__wself`, the thread id, the init bytes
  and the `[public, internal]` ports); `__worker` was renamed `__lumenWorkerOps` and its `spawn`
  is node-only. `__wself` keeps only `exit`; `post`, `close`, `setRef`, `report` and
  `loadClassicScript` and the `ToWorker`/`ToMain::Message` channels are gone. Node still has its
  two blocking inbox threads (main-side `arm_main_inbox`, worker-side terminate wake): follow-up.

Behaviour changes:

- `SharedWorker` with an invalid URL throws a `DOMException` `SyntaxError` (was `TypeError`).
- Constructor options are validated strictly (`TypeError`, including enum errors for `type` and
  `credentials`).
- `SharedWorker` has no `onclose` property; the `close` event still fires through
  `addEventListener`.
- Page-side `error` events are trusted, cancelable `ErrorEvent`s; `onmessage`/`onerror` handlers
  are ordinary listeners in registration order (the handler used to run first).
- A `SharedWorker` with no document location resolves against the realm cwd, else `file:///`.
- Transfer lists are honoured by `Worker.postMessage` and the worker's `postMessage`.
- A message posted before `terminate()` is dropped by the closed port.
- A page `Worker` with a `message`/`messageerror`/`error` listener is pinned until it exits or is
  terminated; one without listeners is collectable, and collecting never terminates the thread.
- The worker global is an instance of `DedicatedWorkerGlobalScope`/`SharedWorkerGlobalScope`
  (`attach_instance`), so `self instanceof EventTarget` holds with no `hasInstance` override;
  `self` is an own accessor and `location` a `WorkerLocation`.

### Performance and startup

- No blocking inbox thread per web worker and no shared-worker router thread; both directions are
  wire bytes to a native receiver to an event, with no JS glue and no `Uint8Array` copy.
- Idle is free: nothing is armed beyond ref'd stream tasks that hold the loop only while the worker
  can still receive. Wakes are coalesced (one per burst); terminate wakes the worker immediately.
  The 50 ms stop poll in `worker_loop` stays only as a fallback.
- Startup: web workers no longer parse or run `worker_scope.js`/`shared_worker_scope.js` or the AOT
  blob, and the page realm no longer evaluates `worker.js`; the classes are lazy globals, so a realm
  that never touches `Worker` pays nothing. Node workers still run the `worker_threads` glue as
  before; startup is unchanged there.

Tests: `lumen-host/src/tests.rs` (mock backend: WebIDL shape, brand checks, `SharedWorker.port`,
trusted events, GC of an idle `Worker`) and `lumen-runtime/src/tests.rs` (dedicated transfer of an
`ArrayBuffer` and a `MessagePort` both ways, scope prototype chain and globals, posting before
`terminate()`). Existing worker tests were not changed.
