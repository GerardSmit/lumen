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
  silently. The `response` guard drops `set-cookie`. `immutable` throws `TypeError`.
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
  context, not at send time. The `response` guard drops `set-cookie`.
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

## Still JavaScript

`server.js` (it
uses `__ws.upgrade/send/close`), `service_worker.js` and `shared_worker.js`.
