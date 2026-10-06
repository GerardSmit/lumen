# Native messaging

`MessageEvent`, `CloseEvent`, `PromiseRejectionEvent`, `MessagePort`, `MessageChannel`,
`BroadcastChannel` and `Performance` (with the `performance` and `self` globals) are native
`lumen_bind` classes in `lumen_host`. They replace `lumen-web`'s `messaging.js` and `platform.js`.
The kernel keeps its own transport for shared workers and installs only the event classes and
`Performance`.

## Ownership

| Module | Contents |
| --- | --- |
| `messaging::events` | `MessageEvent`, `CloseEvent`, `PromiseRejectionEvent`: subclasses of the native `Event` |
| `messaging::channel` | `MessagePort`, `MessageChannel`, `BroadcastChannel`, the structured-clone bridge, the `__lumenSharedPorts` namespace |
| `ports` | endpoint handles (moved from `lumen-runtime`): `new_pair`, `adopt`, `poll`, `post`, `listen`, `broadcast`, reaper |
| `clone_transfer` | `__cloneTransfer`: shared-memory and port attachments of a message (moved from `lumen-runtime`) |
| `performance` | `Performance`, `install_globals` |

Install `messaging::event_bindings::Module` and `messaging::channel_bindings::Module` with
`lazy_globals`, `messaging::shared::Module` with `namespace`, and call
`messaging::install_port_clone` and `performance::install_globals` as module initializers.
`lumen-web`'s extension does all of this; ports additionally need `lumen_host::ports::extension()`
and `clone_transfer::extension()` (the runtime lists them) and an event loop. `new MessageChannel()`
without the ports extension throws a `TypeError`.

## Events

The three classes embed `Event` as `base`. Values they hold (`data`, `source`, `ports`, `promise`,
`reason`) are traced by making the instance a native identity owner. `ports` is a frozen array
created once (`[SameObject]`). `MessageEvent::create` and `PromiseRejectionEvent::for_user_agent`
build trusted events for the user agent. There is one `PromiseRejectionEvent`: `lumen-html-js`
fires the same class (`dispatch_promise_rejection`) and registers it in the window.

## Ports

A web port is an `EventTarget` owning a `Link` (the endpoint handle id, the started flag, the
close hook). Messages are structured-clone wire bytes:

- `postMessage(message, transfer | options)` parses the transfer list, then calls
  `__serializeForClone(message, list, true, bridge)` synchronously and queues the bytes on the
  endpoint (`ports::post`). Posting on a closed port does nothing.
- The receiving realm runs a loop task (`ports::listen`) that polls one message per wake,
  calls `__deserializeClone(bytes, bridge)` and dispatches a trusted `MessageEvent` (or
  `messageerror` carrying the error). Ports created by the deserialization are collected by
  `ports::begin_received` / `end_received` and become `event.ports`.
- The peer closing is reported as a `close` event after the queued messages.
- Assigning `onmessage` (a function) starts the port; `addEventListener("message")` does not.

`bridge` is the explicit port parameter of the serializer: `serializeForClone(value, transfer,
transport, ports)` and `deserializeClone(bytes, ports)` default to `globalThis.__lumenPortClone`.
The web bridge (built natively, kept in a native slot of the realm's global) answers for web
ports and hands every other port to the installed global bridge, which Node's `worker_threads`
replaces. Nothing mutates a global any more.

### Lifetime

- The loop task holds its port weakly.
- A port that has started, has a `message` listener and can still receive (`ports::can_receive`:
  open, and the peer endpoint exists) is pinned: its handle holds the wrapper, so the port lives
  without a script reference. `TargetData::observe_changes` re-evaluates the pin whenever the
  listener list changes, and every wake re-evaluates it.
- When the peer's wrapper is collected the reaper releases its handle and wakes this endpoint,
  which drops its pin. A closed or unreferenced port is collectable.
- A collected wrapper reports its id to `DeadPorts` from `Drop`; the realm's reaper task
  releases the handle on the realm thread.
- The loop task is unref'd: an open port never keeps the loop alive.

## BroadcastChannel

A channel joins the process-wide group `origin + "\0" + name` (`location.origin` when the realm
has one). `ports::post` fans the bytes out to every other open endpoint of the group on any
thread; each receiver deserializes its own copy and gets a task. A channel starts receiving at
construction and is pinned while it has a `message` listener and is open. `postMessage` on a
closed channel throws `InvalidStateError`.

## Performance

`Performance` extends `EventTarget`, has an illegal constructor and reads `lumen_host::perf`
(`now()` on the shared 100-microsecond grid, `timeOrigin`, `toJSON`). `performance` and `self`
are writable, enumerable, configurable data properties defined only when the realm has not
defined them; `Performance` is a lazy global. `lumen-node`'s `perf_hooks` adds its timeline
methods to `Performance.prototype` as before.

## Behavior changes

- Events a port delivers are trusted (`isTrusted`), and `event.ports` holds the received ports.
- A realm without an event loop or the ports extension cannot create a `MessageChannel`; the
  in-process JS fallback is gone.
- A collected web port does not close its peer.
- The interface objects are non-enumerable globals (Web IDL), no longer plain assignments.
- `MessageEvent`'s `source` is checked against `MessagePort`, `Window` and `ServiceWorker`.
