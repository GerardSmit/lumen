# Native network classes

`XMLHttpRequest`, `XMLHttpRequestEventTarget`, `XMLHttpRequestUpload`, `ProgressEvent`,
`WebSocket`, `EventSource`, `Headers`, `Request`, `Response` and `fetch` are native `lumen_bind`
classes. They replace `lumen-web`'s `xhr.js`, `websocket.js`, `eventsource.js` and `fetch.js`.

## Ownership

| Crate / module | Contents |
| --- | --- |
| `lumen_host::net::transport` | `Transport`: the realm's HTTP transport objects as Rust sees them; `Failure`, `ResponseBody`, `read_chunk`, `SyncRequest` |
| `lumen_host::net::flow` | `start`, `RequestSpec`, `Response`, `RequestControl`: one request through the CORS policy |
| `lumen_host::net::body` | `extract_body` (`BodyInit` to bytes and default `Content-Type`), charset and media type helpers |
| `lumen_host::net::headers` | `HeadersData` (header list, guards, forbidden names, HeadersInit parsing) |
| `lumen_host::net::fetch_body` | `Body`, `Source`, `NetBody`, `Drain`: body mixin state, clone/tee, reading a body to the end |
| `lumen_host::net::fetch` | `Headers`, `HeadersIterator`, `Request`, `Response`, `fetch` (`net::fetch_bindings::Module`) |
| `lumen_host::net::xhr` | `ProgressEvent`, `XMLHttpRequestEventTarget`, `XMLHttpRequestUpload`, `XMLHttpRequest` (`net::bindings::Module`) |
| `lumen_host::timers` | `set_timeout`: a native callback scheduled through the realm's `setTimeout` |
| `lumen-web` `websocket_class`, `eventsource_class`, `net_class` | `WebSocket`, `EventSource` and what they share (constructor that starts the connection, pins, event helpers) |

`net::bindings::Module` is installed with `lazy_globals`. `lumen-web`'s extension does so and
registers its transport; the Bitnest kernel does the same in `install_html_byte_types`.

## Transport

A host exposes its HTTP transport as `__http`-shaped operations:

- `request(method, url, headerPairs, bodyU8 | undefined, resolve, reject, redirect, options)`
  returns a handle with `abort()` and the upload counters `uploadLoaded`, `uploadTotal`,
  `uploadComplete`. `options` is `{mode, credentials, redirect, uploadProgress, forcePreflight}`.
  `resolve` receives `{status, statusText, url, headers, type?, redirected?, body? | bodyReader?}`
  where `bodyReader` has `read()` (a promise of a `Uint8Array`, `null` at the end) and `cancel()`.
- `requestSync(method, url, headerPairs, bodyU8 | undefined, {mode, credentials, redirect,
  timeout, forcePreflight, origin})` returns `{status, statusText, url, headers, body}`.
- optionally `policyHandledByHost` and `browserOrigin`.

`Transport::install(ctx, request, sync, policy)` stores the three objects for the current realm
(`RealmServices`). `lumen-web` passes `__http` and `__http_policy`; the kernel passes
`__bitnestHttp` for all three. Rust calls them with native closures as `resolve` and `reject`, so
no script callback is held.

## Request flow

`flow::start` picks one of two plans:

- **Browser policy.** The realm is a browsing context (the transport reports a `browserOrigin`,
  or the global has a `document` or `location`) and the transport does not apply policy itself
  (`policyHandledByHost`). A `lumen_common::cors::FetchPolicy` is driven over the transport:
  preflight when `needs_preflight`, redirect hops requested with `redirect = "manual"`,
  `response_head` checks, then `filter_response`. `fetch` and XHR share this loop.
- **Direct.** One transport request that carries `mode`, `credentials` and `redirect`. The kernel takes
  this plan: its transport applies the policy (`policyHandledByHost`).

A response is a `Response` with an unread `ResponseBody` (`None`, `Bytes` or a `Reader`). An
opaque or opaque-redirect result has status 0 and no body. `RequestControl::abort` aborts the
in-flight transport request and silences the completion callback; `upload_progress` reads the
counters of the first request that carried the body (not the preflight). `done` never runs
inside `start`, even when the transport settles synchronously.

## XMLHttpRequest

State lives in a `Core` shared with the callbacks of the in-flight request: a plain `State`, the
`upload` object, the cached `response_object` and a `pin`.

- **Generations.** `open`, `abort` and every terminal path bump a generation counter. Callbacks
  and dispatch sequences re-check it after every event, because a listener may call `abort()`,
  `open()` or `send()` again.
- **Events.** `send()` fires `loadstart` (and, with a body and upload listeners, the upload
  `loadstart`) synchronously. The response then produces `readystatechange` 2, 3 per chunk with
  `progress`, 4, `load`, `loadend`. `abort`, `error` and `timeout` go through one `fail` that
  fires `readystatechange` 4 and then the upload and request terminal events. Events are
  trusted. `ProgressEvent` init members follow WebIDL `unsigned long long`.
- **Upload progress.** While the upload is incomplete, a 50 ms `set_timeout` samples the
  transport counters. When response headers arrive the counters are sampled once more and the
  upload is completed (`progress`, `load`, `loadend`) before `readystatechange` 2.
- **Timeout.** Armed from `send()`; `timeout = n` while sent re-arms from the start time.
- **Responses.** Bytes are collected in one buffer. `responseText` decodes with the charset of
  the MIME override, else of `Content-Type` (`lumen_common::encoding`), `json` parses UTF-8
  through the realm's JSON, `arraybuffer` and `blob` are cached and release the buffer,
  `document` calls the global `DOMParser` (HTML or XML, a `parsererror` root is `null`) and is
  cached. Response headers are lowercased and combined; `Set-Cookie` is hidden.
- **Sync.** `open(.., false)` plus `send()` calls `Transport::request_sync` (the transport
  applies CORS and credentials policy; `origin` is passed when the realm is a browsing context).
  Failures throw `TimeoutError`, `AbortError`, `NotSupportedError` or `NetworkError`
  `DOMException`s and fire no events.
- **Request bodies.** `extract_body` handles `Blob`/`File` (its type), `BufferSource`,
  `FormData` (multipart, with its boundary), `URLSearchParams` and strings (`text/plain;charset=UTF-8`);
  the default type applies only when the author set none. `GET` and `HEAD` drop the body.

### GC

An in-flight request holds its object in `Core::pin`; the transport and timer closures capture
the `Core`, never the object. Every terminal path releases the pin, so a finished or
never-sent object is collectable. The instance is a native identity owner that traces its
listeners, the upload object and the cached response object.

## WebSocket and EventSource

The transports in `websocket.rs` and `sse.rs` stay in Rust and report to a dispatch function the
class creates once its wrapper exists. That function holds the instance weakly. A connection
that is not closed and has a listener or handler is pinned in a host-state map, re-evaluated
whenever listeners change and after every transport event; an unreferenced, closed or
listener-less connection is collectable, and a completion for a collected instance closes the
transport. `EventSource` keeps the pin while waiting to reconnect, reconnects with
`Last-Event-ID`, strips a leading BOM, ignores an `id` containing U+0000 and joins a CR/LF pair
split across chunks. `WebSocket` follows the HTML Standard's state machine, including
`close()` while connecting (no `open`, then `error` and `close` with 1006).

## Behavior changes

- `XMLHttpRequestEventTarget` and `XMLHttpRequestUpload` have illegal constructors.
- XHR events are trusted; the upload `loadstart` fires inside `send()`.
- A `ReadableStream` request body is no longer special-cased (it is stringified like any other
  object).
- An invalid header name throws `SyntaxError` (was `TypeError`); an unparsable URL throws a
  `SyntaxError` `DOMException`.
- An opaque or status-0 response is a network error (`error`, not `load`).
- `fetch.js` no longer has the XHR-only upload and forced-preflight hooks.

## fetch

`Headers`, `Request` and `Response` follow the WebIDL shape (enumerable members, `@@toStringTag`,
lengths, `Headers` iterator).

- **Header list.** Names are stored lowercase; iteration is sorted and combined with `, `, except
  `set-cookie`, which stays one entry per value (`getSetCookie`). Guards: none, `request`,
  `request-no-cors`, `response`, `immutable`. Request guards apply only when the transport reports
  a `browserOrigin` (a context whose policy Lumen applies); forbidden names are dropped
  silently. The `response` guard (browsing context only; otherwise constructed responses are unguarded) drops `set-cookie`. `immutable` throws `TypeError`.
- **Bodies.** `Source` is `Null`, shared `Bytes`, a transport `Net` body or a `Stream`. Strings,
  buffers, `Blob`, `FormData` and `URLSearchParams` go through `extract_body`. A `ReadableStream`
  is created lazily (`new ReadableStream({type: 'bytes'})` from the global) so `new Response('x')`
  never loads the streams glue. `json/text/arrayBuffer/blob/formData/bytes` share one `consume`
  path: used and locked checks, then a cancellable `Drain`. `clone()` shares bytes or tees the
  stream through the `Symbol.for('lumen.cloneBody')` hook.
- **fetch.** `run_fetch` builds the `RequestSpec` and calls `flow::start`. A `FetchState`
  (deferred, request control, drain, weak body, abort step) lives while the fetch is in flight
  and while the response body is open; the response body is a `NetBody` that holds it. Abort
  rejects the promise, cancels the transport and errors open bodies (`AbortError` or the signal
  reason). A native owned abort step is registered on the signal given to `fetch` (no signal, no
  step).
- **Signal.** `Request.signal` is created on first access and follows the signal passed in
  (`follow_signal`).
- **GC.** The `Request`/`Response` objects are native identity owners that trace their headers,
  body stream and signal; in-flight state is released at every terminal path and holds the
  controller weakly.

### fetch behavior changes

- Methods are normalised only for `DELETE GET HEAD OPTIONS POST PUT`; `patch` stays lowercase.
- Forbidden request headers are dropped at `Headers`/`Request` construction in a browsing
  context, not at send time. The `response` guard drops `set-cookie` in a browsing context only.
- `Headers.getSetCookie()` exists; `new Headers(null)` and non-object inits throw `TypeError`.
- `request.signal` is not the object passed as `init.signal`.
- Added attributes: `cache`, `referrer`, `referrerPolicy`, `integrity`, `keepalive`,
  `destination`, `duplex` and the navigation flags.
- `new Request(request)` transfers the body (the input becomes used) instead of piping it.
- Constructed responses have type `default`; status outside 200..599 throws `RangeError`;
  `Response.error`, `redirect` and `json` exist; `GET`/`HEAD` with a body is a `TypeError`.
- `fetch()` without arguments throws synchronously. Transport failures reject with `TypeError`;
  abort and timeout reject with `DOMException`.
- `body::to_text` converts to USVString, which also affects XHR string bodies.

## Lumen.serve

`Lumen.serve`, `Lumen.upgradeWebSocket` and `Lumen.version` are one `lumen_bind` module named
`Lumen` (`lumen-web/src/server.rs`; `js/server.js` and the raw `__http_server` and `__ws`
namespaces are gone). The extension installs it eagerly on the realm's `Lumen` object (created
when absent, so the parallel glue still finds it); `serve` and `upgradeWebSocket` are enumerable
operations and `version` is a read-only, enumerable, configurable data property. On `wasm32` the
same module exists and the operations throw `unsupported`.

- **Requests.** The accept decoder builds the `Request` natively (`net::server_request`: method
  checks of the `Request` constructor, URL without credentials, the wire's headers under no guard,
  a bytes body for non-`GET`/`HEAD` requests with a body) and calls the handler directly with
  `(request, info)`. The connection id lives in a native private slot of the `Request`, not in a
  script-visible symbol property.
- **Responses.** The handler's result is awaited with `Ctx::then_value`, then read natively:
  `net::served_response` (status, status text, headers as iteration yields them) and
  `net::read_served_body` (the body drain of `fetch_body`; a missing, used, locked or failing body
  reads as empty). A value that is not a `Response`, or a status outside 100..=599, is a handler
  error. `onError` (also awaited) produces the response, else the error is logged with
  `console.error` and the answer is a 500 with `Internal Server Error` (`text/plain;charset=UTF-8`);
  if `onError` throws or returns a non-`Response`, the answer is that 500.
- **WebSocket upgrade.** `upgradeWebSocket` adopts the socket with `websocket::adopt_connection`
  (the same registry and read loop as the `WebSocket` client, unmasked frames) and returns the
  handle `{remoteAddress, onmessage, onclose, send(data) -> bool, close(code?, reason?)}`. The
  transport reports to a native dispatch function that reads `onmessage` / `onclose` from the
  handle when an event arrives; `onclose` fires once for a close frame, a protocol failure
  (`1006`-style) or a dead socket. The handle is held by that dispatch function for as long as the
  socket is registered.
- **GC.** A server entry holds the handler and `onError` until the listener stops (`shutdown`,
  `AbortSignal` or realm teardown). Requests and responses are ordinary native identity owners and
  are collectable once the answer has been written; no per-request state outlives the write.

### Server behavior changes

- Constructed `Response` headers follow the `response` guard only in a browsing context; outside
  one (Node, Bun, `Lumen.serve` handlers) they are unguarded, so a `Set-Cookie` header on a
  `Response` returned from a handler is kept and written as one header line per value. Responses
  filtered from the network (`basic`/`cors`) still hide it.
- `new Response("", { status: 204 })` (an empty body with a null body status) is accepted outside a
  browsing context and has no body, as in Bun; a non-empty body still throws `TypeError`, and a
  browsing context throws for any body.
- Answers to `HEAD` keep the headers (and `Content-Length`, when computed) of the `GET` answer and
  write no body. `1xx`, `204` and `304` answers write no `Content-Length` and no body.
- Upgraded server sockets poll their reads (100 ms timeout, like the client end) so that
  `send`/`close` from the loop thread never wait on a blocked read.
- The handler receives a request whose header list is the wire's (invalid pairs are skipped
  instead of failing the request); a request that is rejected by the `Request` rules (a forbidden
  method such as `TRACE`, a URL with credentials) goes through `onError`, as before.
- `await`ing a handler result uses the engine's promise machinery, so a patched
  `Promise.prototype.then` no longer sees it.
- A request object used after its connection was answered or upgraded throws a `TypeError`
  (`upgrade: unknown or already-answered connection`); it used to report an already-upgraded
  connection as such.
- `Lumen.serve` throws `RangeError` for a `port` outside 0..65535.

## Still JavaScript

None in `lumen-web`: it has no JS glue left ([native-events.md](native-events.md)), and the service-worker and
shared-worker classes are native ([native-workers.md](native-workers.md)).
