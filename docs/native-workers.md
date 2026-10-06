# Native workers

`Worker`, `SharedWorker`, the service-worker classes and the worker global scopes are native
`lumen_bind` classes in `lumen_host::workers`. `lumen-runtime` (one OS thread and one `Runtime` per
worker) and the Bitnest kernel (every realm in one address space, driven by the shell) run the same
classes over different backends. There is no JavaScript worker glue left in `lumen-web`,
`lumen-runtime` or the kernel; the only worker JavaScript is `node:worker_threads` in `lumen-node`.

Related: [native-messaging.md](native-messaging.md) (ports, `MessageChannel`, message events),
[native-clone.md](native-clone.md) (the wire format every message uses),
[native-events.md](native-events.md).

## Layout

| Module (`lumen_host::workers`) | Contents |
| --- | --- |
| `control` | `Control`: a `Send` mailbox of `WorkerEvent`s for one realm-side object. Wakes the owning realm through its `CompletionSender` (`ports::Mailbox<T>`, shared with port endpoints). |
| `backend` | `WorkerBackend` (dedicated spawn, terminate, shared connect/disconnect), `WorkerScopeHost` (kind, location, name, `close`, `importScripts` loader, error reporting). `set_backend` stores the backend in `OpState`. |
| `page` | `Worker`, `SharedWorker`. |
| `scope` | `WorkerGlobalScope`, `DedicatedWorkerGlobalScope`, `SharedWorkerGlobalScope`, `WorkerLocation`, `install_scope`. The global is an instance of its scope class (`attach_instance`), so `self instanceof EventTarget` needs no `hasInstance` override. |
| `registry`, `service`, `service_scope` | Service-worker data types and `ServiceWorkerRegistry` / `ServiceScopeHost`; `ServiceWorker`, `ServiceWorkerRegistration`, `ServiceWorkerContainer` (`navigator.serviceWorker`, an own accessor installed only where a registry exists and the context is secure); `ServiceWorkerGlobalScope`, `ExtendableEvent`, `ExtendableMessageEvent`, `FetchEvent`, `Clients`, `Client`, `WindowClient`. |

A dedicated worker uses the HTML implicit port: the `Worker` holds the outside end of a
`ports::new_pair()`, the scope the inside end, and delivery goes through
`messaging::listen_native` (a native receiver that dispatches the `MessageEvent` on the `Worker` or
the global instead of a `MessagePort`). Transfer lists work in both directions. Errors, `Online`,
`Exit` and shared-worker `Connect` travel on the `Control`. Before an `error` or `exit` is
dispatched the page flushes the port's queued messages, so a message posted before an error is not
overtaken.

Service-worker messages ride the `Control` as `ServiceMessage` / `ClientMessage` events (they need a
per-message source), as `CloneMessage`s in the structured-clone wire format. Registry events are
pushed (`Registration` snapshot before the `UpdateFound` / `StateChange` it explains,
`ControllerChange`, `JobSettled`), not diffed.

## Owner loop

A host without an event loop (the kernel) installs `lumen_host::owner_loop`: a `TaskRegistry`, a
`CompletionSender::with_notify` whose notify wakes the host's scheduler, and the receiver plus a
pending counter in `OpState`.

- `notify` runs only when the pending counter goes 0 to 1; port and control wakes are coalesced, so
  a burst costs one notify. A realm that received nothing is never pumped and nothing is armed
  while idle (no timer, no thread, no poll).
- `owner_loop::pump(engine, budget)` runs up to `budget` completions with a microtask checkpoint
  after each and returns thrown values. `has_ready` is one atomic load. A callback that posts
  during the pump does not re-notify, so the host checks `has_ready` after `pump`
  (`next_timer_delay_ms` returns `Some(0)` while work is ready).
- `owner_loop::settle` is shared with `Runtime::dispatch` in `lumen-runtime`.
- `CompletionSender::run_blocking` spawns OS threads and is not used by anything the kernel
  installs; do not call it from an op a kernel realm can reach.

## `lumen-runtime`

`src/worker.rs` is the `WorkerBackend` and the `WorkerScopeHost`. Dedicated and shared workers run
on a thread per worker; no thread blocks on a receive for web workers. Shared workers are
registered process-wide by URL, origin, type and name; a client is a connection whose page end is
open, and the worker lives until every page end closes or is collected. A page `Worker` with a
`message`, `messageerror` or `error` listener is pinned until it exits or is terminated; one
without listeners is collectable (collecting never terminates the thread).

Node `worker_threads` shares the thread and realm machinery (`__lumenWorkerOps.spawn`) but not the
scope: the realm gets the `worker_threads` bootstrap, and `exit` carries a real exit code. Both
directions of the node worker's lifecycle are `Control`s: the worker reports `online`, `oom`,
`error` and `exit` on a main-side `Control` (an unref-able loop task, one event per turn), and the
parent wakes a blocked worker loop on `terminate()` by closing a worker-side `Control`. Neither
needs a blocking thread.

The worker loop still waits with a 50 ms `recv_timeout`. It is a fallback, not the wake path:
`terminate()`, `terminate_all` for node workers, port close, and shared-worker control events all
wake the loop at once. It stays because (1) an embedded parent's workers share the embedder's
`AtomicBool` interrupt, which nothing can wake, (2) `terminate_all` sets the stop flag of web
workers without a wake handle, and (3) `idle_collect` maintenance runs on the poll tick.

## Bitnest kernel

`crates/kernel/runtime-services` implements the backends over registries that keep identity and
policy only; messages are native ports (one address space, so `PortTransfer` and `CloneMessage`
move between realms with no copy of the transport).

- Every realm installs the owner loop (waker first), `ports`, `clone_transfer` and `messaging`
  extensions and limits of 1 MiB per message and 128 queued messages
  (`QuotaExceededError` / `DataCloneError`). `MessageChannel` and `BroadcastChannel` exist in every
  kernel realm.
- `SharedWorkerHost` keeps worker identity, script requests and each client's `Control` and
  pending worker-side port; `Connect` is sent once the worker is running. `importScripts` throws
  `NotSupportedError`; the kernel has no dedicated `Worker`.
- The service-worker host is a passive `Rc<RefCell<Registry>>` state machine. Registration ids are
  stable per (origin, scope); worker ids change per version. Effects collect in an outbox flushed
  after the borrow is released. `register()` settles at `installing`. Limits: 128 queued client
  messages per scope, 32 queued fetch events (the fetch then fails with a `TypeError`). Worker
  responses are validated in `http.rs` (status, status text, header tokens, 64 KiB of headers,
  16 MiB of body).
- Managers (`kernel/runtime/src/services/{shared_worker,service_worker}.rs`) enter a worker realm
  only when its wake flag was set or its cached timer deadline is due, so an idle worker costs one
  atomic swap per shell turn; worker timers feed the shell's idle sleep.

## Open items

- Module service-worker scripts are rejected by the kernel manager; `importScripts` is unsupported
  in kernel service and shared workers.
- The 50 ms fallback poll in `Runtime::worker_loop` (see above).
- Not run: the `lumen-host`, `lumen-runtime` and `bitnest-runtime-services` tests written for the
  native workers (only compiled), and no QEMU run of the kernel workers.
- `node:worker_threads` keeps its own `MessagePort` / `MessageChannel` / `BroadcastChannel`
  classes in `lumen-node/src/js/worker_threads.js` (Node's `EventEmitter`-style surface:
  `ref`/`unref`, `receiveMessageOnPort`, `markAsUntransferable`, ...). They are a different class
  surface from the web ones, so they are not duplicates, but they sit over the same native
  endpoints.
