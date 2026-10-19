# Native Blob, File and FormData

`Blob`, `File` and `FormData` are `lumen_bind` classes in `crates/lumen-host/src/blob`. They
replace `lumen-web/src/js/blob.js`. Install them with
`lazy_globals::<lumen_host::blob::bindings::Module>` and, for the glue that needs them,
`lazy_globals::<lumen_host::blob::internals::Module>`.

## Bytes

A blob holds a `Source`: `Memory(Bytes)` or `File(FileSource)`. `Bytes` is an `Rc<Vec<u8>>` plus a
range, so `slice()`, `new Blob([blob])` with one part, structured clone and object URLs share one
allocation. `ByteStore::shared_readonly` (lumen-common) exposes the same allocation to JavaScript
as a read-only `ArrayBuffer`; `fetch` request bodies use it.

A file-backed source (`fs.openAsBlob`) keeps a JavaScript `read(from, to)` callback and never
copies the file. A slice of it only narrows the window it passes to that callback. The callback
throws `NotReadableError` once the file changed. File-backed blobs are not cloneable
(`ERR_INVALID_STATE`).

## Classes

- `Blob`: `size`, `type`, `slice(start, end, contentType)` with `[Clamp]` bounds, `text()`,
  `arrayBuffer()`, `bytes()` (promises) and `stream()`. Constructor parts are converted element by
  element through the engine's iterator protocol (`ArrayBuffer`, views, `Blob`, other values as
  `USVString`), `endings: "native"` rewrites line endings of string parts, and `type` follows the
  File API normalisation.
- `File extends Blob`: `name`, `lastModified`.
- `FormData`: `append`/`set` (a `Blob` value becomes a `File`, named `blob` or by the filename
  argument; a `File` without a filename keeps its identity), `get`, `getAll`, `has`, `delete`,
  `entries`/`keys`/`values`/`forEach`/`@@iterator`. A `FormData` that holds files traces them
  through `NativeIdentityOwner`.

Brand checks use the native instance entry, not private fields, so they work across realms and
cannot be spoofed by `Symbol.hasInstance` or prototype swaps.

## DOM bridge

`new FormData(form, submitter)` needs a DOM, which lumen-host does not know. The HTML embedder
registers a `FormBridge` per realm (`FormBridge::install(ctx, populate)`, a `RealmServices` entry).
`lumen-html-js` registers one that validates the arguments, appends the entry list with
`append_text`/`append_file` and dispatches `formdata`. Without a bridge `new FormData(form)`
throws a `TypeError`.

Rust embedders read and build form data through `new_form_data`, `append_text`, `append_file`,
`form_data_entries`, `encode_form_data` and `decode_multipart`, so author code that replaces
`FormData.prototype.append` cannot intercept them. The multipart codec itself is
`lumen_common::multipart`.

## Object URLs

`URL.createObjectURL` registers the blob's `Source` (not the wrapper object) in an interpreter
scoped registry (`OpState`); `revokeObjectURL` removes it. The registry is unbounded unless the
host calls `set_object_url_limits` (lumen-html-js does, with the browser limits). Identifiers
come from `set_token_provider` when the host supplies one (the kernel's transport), else from the
operating system. `resolve_object_url` (Node's `resolveObjectURL`) returns a new `Blob` over the
registered source; `object_url_resource` gives browser resource loaders the bytes and media type.

## `__lumenBlobInternals`

A hidden, non-enumerable global for the remaining JavaScript glue: `isBlob`, `isFileBacked`,
`isFormData`, `bytes`, `fileBlob`,
`encodeFormData`, `decodeMultipart`, `resolveObjectURL`.
