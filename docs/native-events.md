# Native events

One native implementation of `Event`, `EventTarget`, `CustomEvent`, `ErrorEvent`,
`AbortSignal`, `AbortController` and `DOMException` serves every Lumen runtime: the plain and
Node runtime, the browser runtime, HTML windows, workers and the Bitnest kernel. It replaces the
JS units `lumen-web/src/js/events.js`, `custom-event.js` and `error-events.js`, and the HTML
window no longer swaps the realm's classes for a second `DomEventTarget` / `DomEvent`.

## Ownership

The core lives in `lumen_host::events` (`crates/lumen-host/src/events/`). `lumen-host` sits below
every op crate and does not depend on `lumen-html`, so the core knows nothing about DOM trees.

| Module | Contents |
| --- | --- |
| `events::target` | `TargetData` (the listener list), listener registration, the dispatch loop, listener invocation, `Callback`, the `TargetHooks` trait |
| `events::event` | `EventState` and the operations host subclasses share (`EventInit`, legacy initialization) |
| `events::abort` | `SignalState`, abort algorithms, `AbortSignal.timeout` / `any`, `clone_transferable` / `is_transferable_signal` (used by `structuredClone`) |
| `events::web` (re-exported as `events::bindings`) | the `lumen_bind` module published as lazy globals: `Event`, `EventTarget`, `CustomEvent`, `ErrorEvent`, `AbortSignal`, `AbortController`, `DOMException` |
| `events::node` (re-exported as `events::internals`) | the `__eventTargetInternals` constant: `NodeEventTarget`, the Node symbols, `defineEventHandler`, `hasListeners`, `listeners`, brand predicates |
| `events::RealmPolicy` | the per-realm exception reporter and error style (see below) |
| `lumen_host::realm_services` | `RealmServices<T>`, the per-realm typed service store (moved here from `lumen-html-js`) |

`lumen-html-js` keeps the names `DomEventTarget` / `DomEvent` as re-exports of the shared types,
so its subclasses (`Node`, `Window`, `UIEvent`, `PermissionStatus`, ...) keep
`extends = DomEventTarget` / `extends = DomEvent` and need no second implementation. The
HTML-only operations (`DomEventTarget::node`, `window`, `independent`, `rebind_node`,
`handler_value`, `set_content_handler`, click bookkeeping) live in the extension trait
`HtmlTargetExt` in `lumen-html-js/src/events.rs`.

## HTML hooks

A target's `TargetData` may carry one `Rc<dyn TargetHooks>`. A target without hooks (every
target created by `new EventTarget()`, Node targets, workers, the kernel) has a single-entry
path. `lumen-html-js` attaches `HtmlTarget` (realm, node id, click-in-progress flag) to the
targets it creates for nodes, the window and platform objects. The hooks provide:

- `event_path`: the full propagation path. HTML computes DOM parents, shadow-root retargeting,
  closed shadow scopes (opaque `u128` keys from `NodeId::key`), the adjusted `relatedTarget`,
  the window entry after the document, and whether the target is cleared after dispatch.
- `compile_deferred`: lazy compilation of a content-attribute event handler. The core stores it
  as `Callback::Deferred` (an opaque `Rc<dyn Any>` source plus the compiled function).
- `is_global_scope`: whether the current target is a `Window`, which selects the special
  `window.onerror` argument list for a native `ErrorEvent`.

Targets with hooks are rooted with `retain_instance` while they have listeners and node wrappers
stay traced by `DomNode`; targets without hooks become traced identity owners instead (see GC
and retention). `EventTarget::update_retention` implements both.

A realm registers a `RealmPolicy { report, dom_errors }` (a `RealmServices` entry). HTML
registers `DomRealm::report_exception` as `report` and sets `dom_errors`, so a dispatch of an
uninitialized or already dispatching event throws an `InvalidStateError` `DOMException`; without
a policy the core throws Node's `ERR_EVENT_RECURSION`. Without a reporter the core rethrows a
listener's exception on a later turn the way Node does (`process.nextTick`, else `setTimeout`,
else a microtask), and also reports a rejected promise returned by a listener.

There is no `parentNode`-based path for plain `EventTarget`s: only targets with `event_path`
hooks have a path longer than themselves.

## Classes and Node extensions

`Event` holds `Rc<EventState>`; subclasses (`CustomEvent`, `ErrorEvent`, HTML's `UIEvent`
family) embed it as `base`. `CustomEvent.detail` and HTML singleton members use native private
value slots, so the values are traced from the wrapper.

Node's extensions are native options and hooks of the same target, not a second class:

- `Symbol.for("nodejs.internal.kWeakHandler")`: the listener is stored as a `WeakValue`; a
  hidden private slot on the owner object keeps it alive while the owner lives.
- `Symbol.for("nodejs.internal.kResistStopPropagation")`: the listener still runs after
  `stopImmediatePropagation()`.
- `kTrustEvent`: a per-realm unique symbol exposed only through `__eventTargetInternals`, read
  from the `Event` options bag. A registry symbol would let any script forge `isTrusted`.
- `kNewListener` / `kRemoveListener`: per-realm unique symbols. After adding or removing a
  listener the core calls `this[kNewListener](size, type, listener, once, capture, passive,
  weak)` / `this[kRemoveListener](size, type, listener, capture)` when the target defines them
  (MessagePort uses this). Without an override the default `kNewListener` emits Node's
  `MaxListenersExceededWarning`, reading `this[Symbol.for("events.maxEventTargetListeners")]`
  (default `EventEmitter.defaultMaxListeners`, else 10) and the warned flag.
- `Symbol.for("lumen.kEvents")`: a getter on `EventTarget.prototype` that returns a snapshot
  `Map` of type to listener callbacks (`signal[kEvents].size` in Node's tests).
- `NodeEventTarget` (`on`, `once`, `off`, `emit`, `eventNames`, `listenerCount`,
  `removeAllListeners`, max listeners) is a native subclass reachable through
  `require('internal/event_target')`. Node-style listeners are a listener flag; `emit(type,
  arg)` passes `arg` to them and builds a `CustomEvent` with `detail` only when a DOM-style
  listener runs.
- `defineEventHandler(target, name, type = name)` defines Node-style `on<name>` accessors with
  native closures: the handler keeps the position of its first registration, also across
  `null`. HTML IDL handlers use the HTML rule (`null` removes the listener).
- The `signal` option registers a native abort step on the signal (a weak target reference and
  the listener's removal flag), not a JS closure.

## DOMException

`DOMException` is a native class with the class hint `hint(js(error))`: its prototype inherits
`Error.prototype`, its constructor inherits `Error`, and each instance (also one created through a
JS subclass's `super()`) gets `[[ErrorData]]` with a captured stack, so `instanceof Error`,
`Error.prototype.toString`, `stack` and `Object.prototype.toString` (`[object Error]`) behave as
for a built-in error. `name`, `message` and `code` are prototype getters; `cause` is an own data
property when the options bag has one. The 25 legacy codes (`INDEX_SIZE_ERR` ...) are
`#[constant]`s, enumerable and read-only on the constructor and the prototype.

## Spec-observable shape

- All classes use `hint(js(webidl))`: operations and attributes are enumerable prototype
  properties, `@@toStringTag` is the interface name, and lengths come from the signatures
  (`Event.length === 1`, `addEventListener.length === 2`, `CustomEvent.length === 1`).
- `Event.NONE` ... `BUBBLING_PHASE` are class constants.
- `isTrusted` is `[LegacyUnforgeable]`: the getter hint `hint(js(unforgeable))` defines an own,
  non-configurable accessor on each instance. Its getter is the same function object as the
  prototype accessor, which Node's `test-abortcontroller.js` also reads.
- Brand failures throw Node's `ERR_INVALID_THIS` (`hint(js(invalid_this))`). Missing arguments
  throw `ERR_MISSING_ARGS`, wrong argument types `ERR_INVALID_ARG_TYPE`, using Node's wording.
  These are `TypeError`s, so Web IDL checks hold as well.
- `AbortSignal` has no constructor (`ERR_ILLEGAL_CONSTRUCTOR`); `AbortController` inherits
  `Object.prototype`; prototype chains match Web IDL (`AbortSignal.prototype` inherits
  `EventTarget.prototype`; `CustomEvent.prototype` and `ErrorEvent.prototype` inherit
  `Event.prototype`).
- Calling `addEventListener` and friends with an `undefined` / `null` receiver uses the global
  object only when the global is itself an `EventTarget` (an HTML window); otherwise it is a
  brand failure.

## GC and retention

- Listener callbacks are Rust-held `Value`s. For a target without hooks the first
  `addEventListener` makes the wrapper a traced native identity owner
  (`Ctx::ensure_native_identity_owner::<EventTarget>`, which leaves an existing owner such as
  `DomNode` in place), so callbacks are owner edges and an unreachable target with listeners
  collects, as it did when the listeners lived in a JS `Map`.
- `AbortSignal` is an identity owner from creation and also traces its source and dependent
  signals (`AbortSignal.any`). A signal from `AbortSignal.timeout` with a strong `abort`
  listener is rooted in an `OpState` registry until it aborts or loses the listener; its timer
  holds only a weak reference, so a timeout signal without listeners, or with only weak ones,
  collects.
- A dispatched `Event` becomes an identity owner tracing its target, current target, related
  target and path.
- HTML targets keep their current retention (see `retention` above).

## Performance

Dispatch reads a native listener vector. The JS glue built a closure and a copied array per
dispatch, an options object per `addEventListener`, and kept per-event state in a symbol-keyed
object. The native core:

- snapshots only the listeners that match the type and phase, into a `SmallVec`-sized `Vec`
  reused per invocation, and skips targets whose list is empty without allocating;
- shares one `Rc<Cell<bool>>` removal flag per listener between the list and the snapshot;
- creates `emit`'s `CustomEvent` only when a DOM-style listener runs;
- reads options with intrinsic gets, without building an options object;
- keeps event state in one `Rc<EventState>` with `Cell` fields.

## Migration

- `events.js`, `custom-event.js` and `error-events.js` are deleted; `lumen-web`'s `build.rs` drops
  their units and its extension publishes `lumen_host::events` as lazy globals.
- `xhr.js` is gone: `XMLHttpRequest` is native ([native-network.md](native-network.md));
  `encoding.js` and `serialize.js` are native too: `structuredClone` copies a transferable
  `AbortSignal` with `events::clone_transferable` ([native-clone.md](native-clone.md)). `MessageEvent`,
  `MessagePort` and the other channel classes are native subclasses
  ([native-messaging.md](native-messaging.md)).
- `lumen-node`: `internal/event_target` maps to the native internals; `events.js` reads listeners
  through `__eventTargetInternals.listeners`; `worker_threads.js` builds `MessagePort` on the
  native `NodeEventTarget` with `kNewListener` / `kRemoveListener` hooks and `emit`;
  `perf_hooks.js` no longer calls `initEventTarget` (any object becomes a target by having the
  native data).
- `lumen-runtime` worker scopes are native (`lumen_host::workers`); `worker_scope.js` and its
  `kTrustEvent` reads are gone.
- The Bitnest kernel installs `lumen_host::events::bindings` with `define_lazy_globals` before
  its byte-type glue, instead of concatenating `events.js` and `error-events.js`.
- `fetch.js` is gone: `Headers`, `Request`, `Response` and `fetch` are native ([native-network.md](native-network.md)).
  `fetch` registers an owned abort step on the signal it was given (`add_owned_step`, a weak
  `AbortStep::Owned`, pruned on each registration), and `Request.signal` is a signal that
  `follow_signal`s the one passed in.
- `wasm.js` is gone: the `WebAssembly` namespace is native ([native-wasm.md](native-wasm.md)).
- `server.js` is gone: `Lumen.serve`, `Lumen.upgradeWebSocket` and `Lumen.version` are native
  ([native-network.md](native-network.md)), with the raw `__http_server` and `__ws` namespaces
  removed; `lumen-web`'s `build.rs` drops the unit.
- `urlpattern.js` is gone: `URLPattern` is native ([native-urlpattern.md](native-urlpattern.md)),
  published lazily by `lumen-web`; `build.rs` has no lazy glue units left.
- `preamble.js` and `navigator.js` are gone: `lumen-web` has no JS glue (`build.rs`, the lazy unit
  machinery `__lazyWeb` and the glue IIFE are deleted, so no realm decodes or runs a web glue
  blob at boot). `Navigator` and the `navigator` global are native (`lumen_host::navigator`,
  published lazily): `userAgent` is a prototype getter, no longer an own data property. The DOM
  realm's `Navigator` (`DomNavigator`, `lumen-html-js`) `extends` it. The extension drops the raw
  `__http` namespace after registering the transport and no longer creates `__url`.
- Worker and SharedWorker page classes and the worker global scopes are native
  (`lumen_host::workers`, [native-workers.md](native-workers.md)), as are the service-worker classes;
  `messaging.js` and `platform.js` moved to [native-messaging.md](native-messaging.md). `lumen-web` has no
  `src/js` directory left.
