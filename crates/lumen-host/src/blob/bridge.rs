//! The hidden `__lumenBlobInternals` object: what the web and Node glue (structured clone,
//! `fetch` bodies, `fs.openAsBlob`, `resolveObjectURL`) needs of blobs and form data.

#[lumen_bind::module(name = "blobInternals")]
pub mod internals {
    use super::super::form::{self, encode_form_data};
    use super::super::web::bindings::{Blob, File};
    use super::super::{
        blob_of, invalid_state, normalize_type, resolve_object_url, uint8_array_view,
        Bytes, Source,
    };
    use crate::webidl::invalid_arg_type;
    use lumen::embed::{Ctx, OpError, OpResult, Value};
    use lumen_bind::This;

    /// `globalThis.__lumenBlobInternals`.
    #[class(name = "BlobInternals", skip(js))]
    pub struct BlobInternals;

    #[constant(name = "__lumenBlobInternals")]
    const INTERNALS: BlobInternals = BlobInternals;

    fn require_blob(ctx: &mut Ctx, value: &Value) -> OpResult<super::super::BlobView> {
        blob_of(ctx, value).ok_or_else(|| invalid_arg_type(ctx, "blob", "an instance of Blob", value))
    }

    fn not_cloneable() -> OpError {
        invalid_state("File-backed Blobs are not cloneable")
    }

    fn file_parts(ctx: &mut Ctx, value: &Value) -> Option<(String, f64)> {
        ctx.with_instance::<File, _>(value, |file| (file.name.clone(), file.last_modified))
            .ok()
    }

    #[methods]
    impl BlobInternals {
        #[method(name = "isBlob")]
        fn is_blob(ctx: &mut Ctx, _this: This<Value>, value: Value) -> bool {
            blob_of(ctx, &value).is_some()
        }

        #[method(name = "isFileBacked")]
        fn is_file_backed(ctx: &mut Ctx, _this: This<Value>, value: Value) -> bool {
            blob_of(ctx, &value).is_some_and(|view| view.source.is_file_backed())
        }

        #[method(name = "isFormData")]
        fn is_form_data(ctx: &mut Ctx, _this: This<Value>, value: Value) -> bool {
            form::is_form_data(ctx, &value)
        }

        /// A read-only `Uint8Array` sharing the blob's bytes; reads a file-backed blob whole.
        fn bytes(ctx: &mut Ctx, _this: This<Value>, blob: Value) -> OpResult<Value> {
            let view = require_blob(ctx, &blob)?;
            let bytes = view.source.bytes(ctx)?;
            uint8_array_view(ctx, &bytes)
        }

        /// A new `Blob` or `File` over the same bytes, for `structuredClone`.
        #[method(name = "clone")]
        fn clone_blob(ctx: &mut Ctx, _this: This<Value>, blob: Value) -> OpResult<Value> {
            let view = require_blob(ctx, &blob)?;
            if view.source.is_file_backed() {
                return Err(not_cloneable());
            }
            Ok(match file_parts(ctx, &blob) {
                Some((name, last_modified)) => ctx.new_instance(File::new_native(
                    view.source,
                    view.content_type,
                    name,
                    last_modified,
                )),
                None => ctx.new_instance(Blob::from_source(view.source, view.content_type)),
            })
        }

        /// `[kind, type, name, lastModified, bytes]` for the wire format of `postMessage`.
        fn snapshot(ctx: &mut Ctx, _this: This<Value>, blob: Value) -> OpResult<Value> {
            let view = require_blob(ctx, &blob)?;
            if view.source.is_file_backed() {
                return Err(not_cloneable());
            }
            let bytes = view.source.bytes(ctx)?;
            let (kind, name, last_modified) = match file_parts(ctx, &blob) {
                Some((name, last_modified)) => ("File", name, last_modified),
                None => ("Blob", String::new(), 0.0),
            };
            let bytes = uint8_array_view(ctx, &bytes)?;
            Ok(ctx.make_array(vec![
                Value::str(kind),
                Value::from_string(view.content_type),
                Value::from_string(name),
                Value::Num(last_modified),
                bytes,
            ]))
        }

        /// The inverse of `snapshot`.
        fn restore(
            ctx: &mut Ctx,
            _this: This<Value>,
            kind: String,
            content_type: String,
            name: String,
            last_modified: f64,
            bytes: Value,
        ) -> OpResult<Value> {
            let bytes = ctx
                .with_buffer_source_bytes(&bytes, <[u8]>::to_vec)
                .ok_or_else(|| invalid_arg_type(ctx, "bytes", "an instance of Uint8Array", &bytes))?;
            let source = Source::Memory(Bytes::new(bytes));
            let content_type = normalize_type(&content_type);
            Ok(if kind == "File" {
                ctx.new_instance(File::new_native(source, content_type, name, last_modified))
            } else {
                ctx.new_instance(Blob::from_source(source, content_type))
            })
        }

        /// A `Blob` whose bytes stay on disk: `read(from, to)` returns that range of the file as a
        /// `Uint8Array` and throws once the file no longer matches what was opened.
        #[method(name = "fileBlob")]
        fn file_blob(
            ctx: &mut Ctx,
            _this: This<Value>,
            size: f64,
            content_type: String,
            read: Value,
        ) -> OpResult<Value> {
            if !read.is_callable() {
                return Err(invalid_arg_type(ctx, "read", "of type function", &read));
            }
            Ok(ctx.new_instance(Blob::from_source(
                Source::file(read, size.max(0.0) as usize),
                normalize_type(&content_type),
            )))
        }

        /// `{ bytes, contentType }` of the `multipart/form-data` encoding of a `FormData`.
        #[method(name = "encodeFormData")]
        fn encode_form_data(ctx: &mut Ctx, _this: This<Value>, data: Value) -> OpResult<Value> {
            let encoded = encode_form_data(ctx, &data)?;
            let bytes = super::super::uint8_array_from_vec(ctx, encoded.body)?;
            Ok(ctx.plain_object(&[
                ("bytes", bytes),
                ("contentType", Value::from_string(encoded.content_type)),
            ]))
        }

        /// A `FormData` of a `multipart/form-data` body.
        #[method(name = "decodeMultipart")]
        fn decode_multipart(
            ctx: &mut Ctx,
            _this: This<Value>,
            bytes: Value,
            boundary: String,
        ) -> OpResult<Value> {
            let bytes = ctx
                .with_buffer_source_bytes(&bytes, <[u8]>::to_vec)
                .ok_or_else(|| invalid_arg_type(ctx, "bytes", "an instance of Uint8Array", &bytes))?;
            form::decode_multipart(ctx, &Bytes::new(bytes), &boundary)
        }

        /// Node's `resolveObjectURL` for the id of a `blob:nodedata:<id>` URL.
        #[method(name = "resolveObjectURL")]
        fn resolve_object_url(ctx: &mut Ctx, _this: This<Value>, id: String) -> Value {
            resolve_object_url(ctx, &id).unwrap_or(Value::Undefined)
        }
    }
}
