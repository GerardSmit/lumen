//! `Blob`, `File` and `FormData` as native classes, plus the object-URL registry behind
//! `URL.createObjectURL`. Shared by every runtime; design in `docs/native-blob.md`.
//!
//! [`bindings::Module`] publishes the three classes (install it with [`crate::lazy_globals`]);
//! [`internals::Module`] publishes the hidden `__lumenBlobInternals` object the web and Node glue
//! read (structured clone, `fetch` bodies, `fs.openAsBlob`, `resolveObjectURL`).
//!
//! A blob is immutable: its bytes live in one shared allocation ([`Bytes`]) and `slice()` only
//! narrows the window. A blob over a file ([`Source::File`]) reads ranges through a JavaScript
//! callback instead and never copies the file.
//!
//! Rust embedders (the HTML window, the kernel's form navigation) build and read these objects
//! through the functions of this module rather than the global constructors, so a script that
//! replaces `Blob` or `FormData.prototype.append` cannot intercept them.

use crate::webidl::{coded, invalid_arg_type};
use lumen::embed::{Ctx, OpError, OpResult, Value};
use lumen_common::buffer::ByteStore;
use std::{ops::Deref, rc::Rc};

mod bridge;
mod form;
mod urls;
mod web;

pub use form::{
    append_file, append_text, decode_multipart, encode_form_data, form_data_entries,
    StoredFormValue, snapshot_form_value, restore_form_value, is_form_data, is_file, clone_form_data, append_form_data, append_file_value, new_form_data, EncodedForm, FormBridge, FormEntry, FormFile, FormValue,
};
pub use bridge::internals;
pub use urls::{
    create_object_url, object_url_resource, resolve_object_url, revoke_object_url,
    set_object_url_limits, set_token_provider, ObjectUrlLimits, ObjectUrlResource,
    set_object_url_environment_provider, revoke_object_urls_for_environment,
    ObjectUrlEnvironment, ObjectUrlEnvironmentProvider, object_url_environment,
    ensure_random_token_provider, object_url_resource_for_environment,
};
pub use web::bindings;
pub use web::bindings::{Blob, File, FormData};

/// A window onto shared, immutable bytes.
#[derive(Clone)]
pub struct Bytes {
    store: Rc<Vec<u8>>,
    start: usize,
    end: usize,
}

impl Bytes {
    pub fn new(bytes: Vec<u8>) -> Bytes {
        let end = bytes.len();
        Bytes {
            store: Rc::new(bytes),
            start: 0,
            end,
        }
    }

    /// The bytes `from..to` of this window, sharing the allocation. Out-of-range ends clamp.
    pub fn slice(&self, from: usize, to: usize) -> Bytes {
        let to = to.min(self.len());
        let from = from.min(to);
        Bytes {
            store: self.store.clone(),
            start: self.start + from,
            end: self.start + to,
        }
    }

    /// A read-only `ArrayBuffer` store over these bytes, without copying them.
    pub(crate) fn readonly_store(&self) -> ByteStore {
        ByteStore::shared_readonly(self.store.clone(), self.start..self.end)
    }
}

impl Default for Bytes {
    fn default() -> Bytes {
        Bytes::new(Vec::new())
    }
}

impl Deref for Bytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.store[self.start..self.end]
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(bytes: Vec<u8>) -> Bytes {
        Bytes::new(bytes)
    }
}

/// A byte range of a file that JavaScript reads on demand: `read(from, to)` returns a
/// `Uint8Array` of that range of the whole file, and throws once the file changed.
#[derive(Clone)]
pub struct FileSource {
    read: Value,
    offset: usize,
    len: usize,
}

impl FileSource {
    fn read(&self, ctx: &mut Ctx, from: usize, to: usize) -> OpResult<Bytes> {
        let to = to.min(self.len);
        let from = from.min(to);
        let chunk = ctx
            .invoke(
                self.read.clone(),
                Value::Undefined,
                &[
                    Value::Num((self.offset + from) as f64),
                    Value::Num((self.offset + to) as f64),
                ],
            )
            .map_err(OpError::thrown)?;
        ctx.typed_array_bytes(&chunk)
            .map(Bytes::new)
            .ok_or_else(|| OpError::type_error("the blob reader must return a Uint8Array"))
    }
}

/// Where a blob's bytes are.
#[derive(Clone)]
pub enum Source {
    Memory(Bytes),
    File(FileSource),
}

impl Source {
    pub fn len(&self) -> usize {
        match self {
            Source::Memory(bytes) => bytes.len(),
            Source::File(file) => file.len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_file_backed(&self) -> bool {
        matches!(self, Source::File(_))
    }

    /// The bytes `from..to`, sharing the allocation. Out-of-range ends clamp.
    pub fn slice(&self, from: usize, to: usize) -> Source {
        match self {
            Source::Memory(bytes) => Source::Memory(bytes.slice(from, to)),
            Source::File(file) => {
                let to = to.min(file.len);
                let from = from.min(to);
                Source::File(FileSource {
                    read: file.read.clone(),
                    offset: file.offset + from,
                    len: to - from,
                })
            }
        }
    }

    /// The bytes `from..to`; reads a file-backed source.
    pub fn read(&self, ctx: &mut Ctx, from: usize, to: usize) -> OpResult<Bytes> {
        match self {
            Source::Memory(bytes) => Ok(bytes.slice(from, to)),
            Source::File(file) => file.read(ctx, from, to),
        }
    }

    /// All bytes; reads a file-backed source.
    pub fn bytes(&self, ctx: &mut Ctx) -> OpResult<Bytes> {
        self.read(ctx, 0, self.len())
    }

    /// A source over a file the host reads through `read`.
    pub fn file(read: Value, len: usize) -> Source {
        Source::File(FileSource {
            read,
            offset: 0,
            len,
        })
    }
}

/// The blob's media type per the File API: empty when it holds a byte outside U+0020..U+007E,
/// else lowercased.
pub fn normalize_type(kind: &str) -> String {
    if kind.bytes().any(|byte| !(0x20..=0x7e).contains(&byte)) {
        String::new()
    } else {
        kind.to_ascii_lowercase()
    }
}

/// A blob's content and media type, for Rust code that needs more than the JavaScript API.
pub struct BlobView {
    pub source: Source,
    pub content_type: String,
}

/// The content of `value` when it is a genuine `Blob` or `File` (also of a subclass).
pub fn blob_of(ctx: &mut Ctx, value: &Value) -> Option<BlobView> {
    ctx.with_instance::<Blob, _>(value, |blob| BlobView {
        source: blob.source.clone(),
        content_type: blob.kind.clone(),
    })
    .ok()
}

/// A new `Blob` over `bytes`.
pub fn new_blob(ctx: &mut Ctx, bytes: impl Into<Bytes>, content_type: &str) -> Value {
    ctx.new_instance(Blob::from_source(
        Source::Memory(bytes.into()),
        normalize_type(content_type),
    ))
}

/// A new `File` over `bytes`; `last_modified` is milliseconds since the epoch.
pub fn new_file(
    ctx: &mut Ctx,
    bytes: impl Into<Bytes>,
    name: &str,
    content_type: &str,
    last_modified: f64,
) -> Value {
    ctx.new_instance(File::new_native(
        Source::Memory(bytes.into()),
        normalize_type(content_type),
        name.to_owned(),
        last_modified,
    ))
}

/// What structured clone keeps of a `Blob` or `File`.
pub(crate) struct BlobSnapshot {
    pub file: bool,
    pub content_type: String,
    pub name: String,
    pub last_modified: f64,
    pub bytes: Bytes,
}

/// A genuine `Blob` or `File`'s content for structured clone: `None` for any other value, an
/// error for a file-backed blob.
pub(crate) fn snapshot_blob(ctx: &mut Ctx, value: &Value) -> Option<OpResult<BlobSnapshot>> {
    let view = blob_of(ctx, value)?;
    if view.source.is_file_backed() {
        return Some(Err(invalid_state("File-backed Blobs are not cloneable")));
    }
    let parts = ctx
        .with_instance::<File, _>(value, |file| (file.name.clone(), file.last_modified))
        .ok();
    let bytes = match view.source.bytes(ctx) {
        Ok(bytes) => bytes,
        Err(error) => return Some(Err(error)),
    };
    let (file, name, last_modified) = match parts {
        Some((name, last_modified)) => (true, name, last_modified),
        None => (false, String::new(), 0.0),
    };
    Some(Ok(BlobSnapshot {
        file,
        content_type: view.content_type,
        name,
        last_modified,
        bytes,
    }))
}

/// The `Blob` or `File` a [`BlobSnapshot`] describes.
pub(crate) fn restore_blob(ctx: &mut Ctx, snapshot: BlobSnapshot) -> Value {
    let source = Source::Memory(snapshot.bytes);
    let content_type = normalize_type(&snapshot.content_type);
    if snapshot.file {
        ctx.new_instance(File::new_native(
            source,
            content_type,
            snapshot.name,
            snapshot.last_modified,
        ))
    } else {
        ctx.new_instance(Blob::from_source(source, content_type))
    }
}

/// A `Uint8Array` over `buffer`, constructed through the realm's `Uint8Array`.
pub(crate) fn uint8_array(ctx: &mut Ctx, buffer: Value) -> OpResult<Value> {
    let global = ctx.global_object();
    let constructor = ctx
        .member_get(&global, "Uint8Array")
        .map_err(OpError::thrown)?;
    ctx.construct_value(constructor, &[buffer])
        .map_err(OpError::thrown)
}

/// A mutable `Uint8Array` holding `bytes`, adopted without copying.
pub(crate) fn uint8_array_from_vec(ctx: &mut Ctx, bytes: Vec<u8>) -> OpResult<Value> {
    Ok(<lumen::embed::JsHost as lumen_bind::Host>::from_bytes(ctx, bytes))
}

/// A read-only `Uint8Array` sharing `bytes`.
pub(crate) fn uint8_array_view(ctx: &mut Ctx, bytes: &Bytes) -> OpResult<Value> {
    let buffer = ctx.make_array_buffer_from_store(Rc::new(bytes.readonly_store()));
    uint8_array(ctx, buffer)
}

/// Node's `ERR_INVALID_STATE`.
pub(crate) fn invalid_state(message: &str) -> OpError {
    coded(
        OpError::error(format!("Invalid state: {message}")),
        "ERR_INVALID_STATE",
    )
}

/// A dictionary argument: absent, `null` or an object.
pub(crate) fn require_dictionary(ctx: &mut Ctx, name: &str, value: &Value) -> OpResult<()> {
    match value {
        Value::Undefined | Value::Null | Value::Obj(_) => Ok(()),
        _ => Err(invalid_arg_type(ctx, name, "of type object", value)),
    }
}
