# Native structured clone

`structuredClone` and the byte format behind `postMessage`, `MessagePort`, `BroadcastChannel`,
workers and `node:worker_threads` live in `lumen_host::structured_clone`. They replace
`lumen-web`'s `encoding.js` (the in-realm copier) and `serialize.js` (the wire serializer), which
are deleted.

## Ownership

| Item | Contents |
| --- | --- |
| `structured_clone::bindings` | the `structuredClone` global (install with `lazy_globals`) |
| `structured_clone::internals` | `__serializeForClone(value, transfer, transport, bridge)` and `__deserializeClone(bytes, bridge)`, for the worker glue and `worker_threads` (install with `lazy_globals`; they are globals, not a namespace) |
| `structured_clone::serialize` / `deserialize` | the same halves for Rust embedders; `messaging::channel` calls them directly |
| `structured_clone::structured_clone` | the local copy with a validated transfer list |
| `clone_transfer` | attachments (`SharedArrayBuffer` memory, port endpoints) of the message being built or read; its `__cloneTransfer` namespace keeps only the capability checks the tests drive |
| `blob::{snapshot_blob, restore_blob}` | the bytes and parts of a `Blob` or `File` on the wire |
| `events::{clone_transferable, is_transferable_signal}` | the copy of a transferable `AbortSignal` |

`lumen-web`'s extension installs both modules. The Bitnest kernel installs only `bindings`: the
frame state of `clone_transfer` creates itself on first use, so a realm without the messaging
extension can still clone `SharedArrayBuffer`s locally.

## One path

`structuredClone(value, { transfer })` is serialize, take the attachments, install them as the
incoming message, deserialize. A local clone therefore behaves like a message that crosses a
channel: the same errors, the same transfer validation, the same sharing of `SharedArrayBuffer`
memory and the same port transfer. Only transferable `AbortSignal`s skip the wire: they are copied
(`events::clone_transferable`) before serialization and the copy is substituted on reading
(`T_LOCAL`). Blob and `ArrayBuffer` contents are copied once into the wire bytes.

## Serializer

Objects are classified with `Ctx::clone_brand` (internal slots), never by `Symbol.toStringTag`,
`constructor` or prototype methods. The walk follows HTML's StructuredSerializeInternal:

- Ordinary objects and arrays: take `Object.keys`-order own enumerable string keys first, then for
  each key `HasOwnProperty` and `[[Get]]`. A getter that deletes a later key or mutates the graph
  sees the spec's result; a deleted key is skipped. Arrays also keep holes, `length` and extra named
  properties.
- Error objects keep one of the seven standard names (anything else is `Error`), an own data
  `message`, `stack` and an own data `cause`. A native error class (`DOMException`) contributes its
  accessor `message`.
- Repeated and cyclic references of every object kind, `Date` and `RegExp` included, are
  back-references. `RegExp.lastIndex` is not kept.
- `Map` and `Set` entries are copied before walking, so a getter that mutates the collection does
  not change what is serialized.
- Typed arrays, `DataView`s and their buffers keep their windows. A view that is out of bounds or
  over a detached buffer, and a detached `ArrayBuffer`, are `DataCloneError`s. A resizable
  `ArrayBuffer` is copied at its current length and arrives fixed-length.
- Functions, symbols, `Symbol` objects, proxies, `Promise`, `WeakMap`, `WeakSet`, `WeakRef` and
  instances of native classes (`Event`, `URL`, `Headers`, ...) are `DataCloneError`s; strings keep
  lone surrogates.
- `Blob` and `File` travel as bytes plus type, name and `lastModified`; file-backed blobs are not
  cloneable (`ERR_INVALID_STATE`).
- Host objects: an object with `[Symbol.for("lumen.transferable.clone")]()` returning
  `{ data, deserializeInfo }` is rebuilt on reading from `globalThis.__lumenCloneResolve(info)`: a
  class with `[Symbol.for("lumen.transferable.deserialize")]` is constructed with no arguments and
  handed `data`; any other function is called with `data`. `lumen-node` installs the resolver.

Transfer lists (`structuredClone` options, `postMessage`): Node's pooled buffers
(`nodejs.untransferable`) are dropped from the list and cloned; duplicates, untransferable
objects, detached or non-`ArrayBuffer` items are `DataCloneError`s. Ports and buffers are
validated again after serialization, since getters may have transferred them, and only then
detached. A failed serialization drops its frame and detaches nothing.

## Ports

Port questions go through a bridge object (`isPort`, `isUntransferable`, `isUncloneable`,
`validate`, `export`, `detach`, `import`): the web bridge `messaging` builds natively, or Node's
`__lumenPortClone`. `serialize` takes it explicitly and otherwise reads the global. An unlisted
port in a message is a `DataCloneError` locally and, for `postMessage`, Node's
`ERR_MISSING_TRANSFERABLE_IN_TRANSFER_LIST` `TypeError`.

## Wire format

A tag byte then a payload; integers are little-endian, strings are `u32` length and UTF-8 (lone
surrogates use the engine's own encoding, so the two ends must be the same engine). Constants and
layouts are in `structured_clone/wire.rs`. A message that starts with `T_PORTS` lists the
transferred port indexes first, so a port the value does not reference still reaches
`MessageEvent.ports`. Readers reject unknown tags, truncation and bad indexes with
`DataCloneError`. The layouts of arrays, plain objects and errors differ from the JavaScript
serializer's; both ends are this module, so nothing else reads them.

## Behaviour changes from the JavaScript glue

- Native-class instances and proxies now fail instead of becoming `{}`.
- `Date` and `RegExp` have identity (a shared `Date` clones to one `Date`).
- Arrays keep holes and named properties; objects with a `__proto__` key stay own data.
- Errors keep `cause`; `DOMException` keeps its message.
- Lone surrogates survive the wire (they became U+FFFD through `TextEncoder`).
- Float16Array travels; `Blob`/`File` copy their bytes instead of sharing them when cloned
  locally.
- `structuredClone` option errors carry Node's "Received ..." suffix.
