//! The Web-exposed `Blob`, `File` and `FormData` classes.

use super::{
    blob_of, normalize_type, require_dictionary, uint8_array_from_vec, Bytes, Source,
};
use crate::webidl::{coded, invalid_arg_type, invalid_this, usv_string};
use lumen::embed::{Ctx, OpError, OpResult, Value};
use std::{cell::Cell, rc::Rc};

/// How much of a file-backed blob one stream chunk reads.
const FILE_CHUNK: usize = 64 * 1024;

/// One element of the `blobParts` sequence after WebIDL conversion.
enum Piece {
    Text(String),
    Blob(Source),
    Buffer(Vec<u8>),
}

fn read_piece(ctx: &mut Ctx, element: Value) -> OpResult<Piece> {
    if matches!(element, Value::Obj(_)) {
        if let Some(view) = blob_of(ctx, &element) {
            return Ok(Piece::Blob(view.source));
        }
        if let Some(bytes) = ctx.with_buffer_source_bytes(&element, <[u8]>::to_vec) {
            return Ok(Piece::Buffer(bytes));
        }
    }
    Ok(Piece::Text(
        usv_string(ctx, &element).map_err(OpError::thrown)?,
    ))
}

/// `sequence<BlobPart>`: converted element by element while the iterator runs.
fn read_pieces(ctx: &mut Ctx, parts: &Value) -> OpResult<Vec<Piece>> {
    match parts {
        Value::Undefined => Ok(Vec::new()),
        Value::Obj(_) => ctx.convert_iterable(parts, usize::MAX, read_piece),
        _ => Err(invalid_arg_type(ctx, "sources", "a sequence", parts)),
    }
}

/// Text as UTF-8; with `native` every CR, LF and CRLF becomes the platform line ending.
fn encode_text(text: String, native: bool) -> Vec<u8> {
    if !native {
        return text.into_bytes();
    }
    let newline = if cfg!(windows) { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str(newline);
            }
            '\n' => out.push_str(newline),
            other => out.push(other),
        }
    }
    out.into_bytes()
}

/// The bytes the pieces stand for; a lone blob part is shared, not copied.
fn assemble(ctx: &mut Ctx, pieces: Vec<Piece>, native_endings: bool) -> OpResult<Source> {
    if pieces.len() == 1 {
        return Ok(match pieces.into_iter().next().expect("one piece") {
            Piece::Blob(source) => source,
            Piece::Buffer(bytes) => Source::Memory(Bytes::new(bytes)),
            Piece::Text(text) => Source::Memory(Bytes::new(encode_text(text, native_endings))),
        });
    }
    let known: usize = pieces
        .iter()
        .map(|piece| match piece {
            Piece::Text(text) => text.len(),
            Piece::Blob(source) => source.len(),
            Piece::Buffer(bytes) => bytes.len(),
        })
        .sum();
    let mut out = Vec::with_capacity(known);
    for piece in pieces {
        match piece {
            Piece::Blob(source) => out.extend_from_slice(&source.bytes(ctx)?),
            Piece::Buffer(bytes) => out.extend_from_slice(&bytes),
            Piece::Text(text) => out.extend_from_slice(&encode_text(text, native_endings)),
        }
    }
    Ok(Source::Memory(Bytes::new(out)))
}

/// `BlobPropertyBag` / `FilePropertyBag`, read in dictionary (lexicographic) member order.
struct Options {
    kind: String,
    native_endings: bool,
    last_modified: Option<f64>,
}

fn to_long_long(value: f64) -> f64 {
    if value.is_nan() {
        0.0
    } else {
        value.trunc()
    }
}

fn read_options(ctx: &mut Ctx, options: &Value, file: bool) -> OpResult<Options> {
    require_dictionary(ctx, "options", options)?;
    let mut read = Options {
        kind: String::new(),
        native_endings: false,
        last_modified: None,
    };
    if !matches!(options, Value::Obj(_)) {
        return Ok(read);
    }
    let endings = ctx.member_get(options, "endings").map_err(OpError::thrown)?;
    if !matches!(endings, Value::Undefined) {
        let endings = ctx.coerce_string(&endings).map_err(OpError::thrown)?;
        match &*endings {
            "transparent" => {}
            "native" => read.native_endings = true,
            other => {
                return Err(coded(
                    OpError::type_error(format!(
                        "The property 'options.endings' must be one of: 'transparent', 'native'. Received '{other}'"
                    )),
                    "ERR_INVALID_ARG_VALUE",
                ))
            }
        }
    }
    if file {
        let modified = ctx
            .member_get(options, "lastModified")
            .map_err(OpError::thrown)?;
        if !matches!(modified, Value::Undefined) {
            read.last_modified = Some(to_long_long(
                ctx.coerce_number(&modified).map_err(OpError::thrown)?,
            ));
        }
    }
    let kind = ctx.member_get(options, "type").map_err(OpError::thrown)?;
    if !matches!(kind, Value::Undefined) {
        read.kind = normalize_type(&ctx.coerce_string(&kind).map_err(OpError::thrown)?);
    }
    Ok(read)
}

/// `[Clamp] long long` slice bound against `size`; `None` when the argument is `undefined`.
fn slice_bound(ctx: &mut Ctx, value: &Value, size: usize) -> OpResult<Option<usize>> {
    if matches!(value, Value::Undefined) {
        return Ok(None);
    }
    let number = to_long_long(ctx.coerce_number(value).map_err(OpError::thrown)?);
    let size = size as f64;
    Ok(Some(if number < 0.0 {
        (size + number).max(0.0) as usize
    } else {
        number.min(size) as usize
    }))
}

fn controller_call(
    ctx: &mut Ctx,
    controller: &Value,
    method: &str,
    args: &[Value],
) -> Result<Value, Value> {
    let function = ctx.member_get(controller, method)?;
    ctx.invoke(function, controller.clone(), args)
}

/// A `ReadableStream` (the realm's own constructor) over `source`: one chunk for bytes in
/// memory, `FILE_CHUNK`-sized pulls for a file.
fn stream_of(ctx: &mut Ctx, source: Source) -> OpResult<Value> {
    let global = ctx.global_object();
    let constructor = ctx
        .member_get(&global, "ReadableStream")
        .map_err(OpError::thrown)?;
    if !constructor.is_callable() {
        return Err(OpError::type_error("ReadableStream is not available"));
    }
    let underlying = match source.clone() {
        Source::Memory(bytes) => {
            let start = ctx.new_native_fn(
                "start",
                1,
                Rc::new(move |ctx: &mut Ctx, _: Value, args: &[Value]| {
                    let controller = args.first().cloned().unwrap_or(Value::Undefined);
                    if !bytes.is_empty() {
                        let chunk = uint8_array_from_vec(ctx, bytes.to_vec())
                            .map_err(|error| error.to_value(ctx))?;
                        controller_call(ctx, &controller, "enqueue", &[chunk])?;
                    }
                    controller_call(ctx, &controller, "close", &[])
                }),
            );
            ctx.plain_object(&[("start", start)])
        }
        Source::File(_) => {
            let position = Cell::new(0usize);
            let pull = ctx.new_native_fn(
                "pull",
                1,
                Rc::new(move |ctx: &mut Ctx, _: Value, args: &[Value]| {
                    let controller = args.first().cloned().unwrap_or(Value::Undefined);
                    let at = position.get();
                    if at >= source.len() {
                        return controller_call(ctx, &controller, "close", &[]);
                    }
                    let end = (at + FILE_CHUNK).min(source.len());
                    let chunk = source
                        .read(ctx, at, end)
                        .and_then(|bytes| uint8_array_from_vec(ctx, bytes.to_vec()));
                    match chunk {
                        Ok(chunk) => {
                            position.set(end);
                            controller_call(ctx, &controller, "enqueue", &[chunk])
                        }
                        Err(error) => {
                            let reason = error.to_value(ctx);
                            controller_call(ctx, &controller, "error", &[reason])
                        }
                    }
                }),
            );
            ctx.plain_object(&[("pull", pull)])
        }
    };
    ctx.construct_value(constructor, &[underlying])
        .map_err(OpError::thrown)
}

/// UTF-8 decode with BOM removal and replacement characters, as `Blob.text()` specifies.
fn decode_text(bytes: &[u8]) -> OpResult<Value> {
    use lumen_common::encoding::{DecodeError, TextDecoder, MAX_DECODED_BYTES};
    if bytes.len() > MAX_DECODED_BYTES {
        return Err(OpError::range_error(
            "decoded text exceeds the host resource limit",
        ));
    }
    let mut decoder = TextDecoder::new("utf-8", false, false)
        .map_err(|_: DecodeError| OpError::type_error("UTF-8 decoding is unavailable"))?;
    let text = decoder
        .decode(bytes, false)
        .map_err(|_| OpError::range_error("decoded text exceeds the host resource limit"))?;
    Ok(Value::from_string(lumen_common::smuggle::utf16_text_owned(text)))
}

#[lumen_bind::module(name = "webBlob")]
pub mod bindings {
    use super::super::form::{
        entries_of, file_entry, populate_from_form, push_entry, Entries, FormBridge,
    };
    use super::*;
    use lumen::embed::{JsHost, Promise};
    use lumen_bind::{CtorRet, Host, This};

    #[class(name = "Blob", hint(js(webidl, invalid_this)))]
    pub struct Blob {
        pub(crate) source: Source,
        pub(crate) kind: String,
    }

    #[class(name = "File", extends = Blob, hint(js(webidl, invalid_this)))]
    pub struct File {
        pub(crate) base: Blob,
        pub(crate) name: String,
        pub(crate) last_modified: f64,
    }

    /// A `FormData` value: text, or the `File` object itself (so `get` keeps its identity).
    #[derive(Clone)]
    pub(crate) enum EntryValue {
        Text(String),
        File(Value),
    }

    #[derive(Clone)]
    pub(crate) struct Entry {
        pub(crate) name: String,
        pub(crate) value: EntryValue,
    }

    #[class(name = "FormData", hint(js(webidl, invalid_this)))]
    pub struct FormData {
        pub(crate) entries: Entries,
    }

    #[derive(Clone, Copy)]
    enum IterKind {
        Keys,
        Values,
        Entries,
    }

    #[class(name = "FormData Iterator", skip(js), hint(js(webidl, iterator)))]
    pub struct FormDataIterator {
        entries: Entries,
        kind: IterKind,
        index: usize,
    }

    impl Blob {
        pub(crate) fn from_source(source: Source, kind: String) -> Blob {
            Blob { source, kind }
        }
    }

    impl File {
        pub(crate) fn new_native(
            source: Source,
            kind: String,
            name: String,
            last_modified: f64,
        ) -> File {
            File {
                base: Blob { source, kind },
                name,
                last_modified,
            }
        }
    }

    impl FormData {
        pub(crate) fn new_native() -> FormData {
            FormData {
                entries: Rc::new(std::cell::RefCell::new(Vec::new())),
            }
        }
    }

    fn blob_of_this(ctx: &mut Ctx, this: &Value) -> OpResult<Source> {
        ctx.with_instance::<Blob, _>(this, |blob| blob.source.clone())
            .map_err(|_| invalid_this("Blob"))
    }

    #[methods]
    impl Blob {
        #[constructor]
        fn new(
            ctx: &mut Ctx,
            #[default(Value::Undefined)] parts: Value,
            #[default(Value::Undefined)] options: Value,
        ) -> OpResult<Self> {
            let pieces = read_pieces(ctx, &parts)?;
            let options = read_options(ctx, &options, false)?;
            Ok(Blob {
                source: assemble(ctx, pieces, options.native_endings)?,
                kind: options.kind,
            })
        }

        #[getter]
        fn size(&self) -> f64 {
            self.source.len() as f64
        }

        #[getter(name = "type")]
        fn content_type(&self) -> String {
            self.kind.clone()
        }

        fn slice(
            &self,
            ctx: &mut Ctx,
            #[default(Value::Undefined)] start: Value,
            #[default(Value::Undefined)] end: Value,
            #[default(Value::Undefined)] content_type: Value,
        ) -> OpResult<Blob> {
            let size = self.source.len();
            let from = slice_bound(ctx, &start, size)?.unwrap_or(0);
            let to = slice_bound(ctx, &end, size)?.unwrap_or(size).max(from);
            let kind = match content_type {
                Value::Undefined => String::new(),
                value => normalize_type(&ctx.coerce_string(&value).map_err(OpError::thrown)?),
            };
            Ok(Blob {
                source: self.source.slice(from, to),
                kind,
            })
        }

        fn text(this: This<Value>, ctx: &mut Ctx) -> Promise<Value> {
            Promise::ready(
                blob_of_this(ctx, &this)
                    .and_then(|source| source.bytes(ctx))
                    .and_then(|bytes| decode_text(&bytes)),
            )
        }

        fn array_buffer(this: This<Value>, ctx: &mut Ctx) -> Promise<Value> {
            Promise::ready(
                blob_of_this(ctx, &this)
                    .and_then(|source| source.bytes(ctx))
                    .map(|bytes| ctx.make_array_buffer_from(bytes.to_vec())),
            )
        }

        fn bytes(this: This<Value>, ctx: &mut Ctx) -> Promise<Value> {
            Promise::ready(
                blob_of_this(ctx, &this)
                    .and_then(|source| source.bytes(ctx))
                    .and_then(|bytes| uint8_array_from_vec(ctx, bytes.to_vec())),
            )
        }

        fn stream(&self, ctx: &mut Ctx) -> OpResult<Value> {
            stream_of(ctx, self.source.clone())
        }

        #[method(hint(js(symbol_for = "nodejs.util.inspect.custom")))]
        fn inspect_custom(
            &self,
            ctx: &mut Ctx,
            this: This<Value>,
            depth: Value,
            options: Value,
            inspect: Value,
        ) -> OpResult<Value> {
            crate::events::inspect_object(
                ctx,
                &this.0,
                vec![
                    ("size", Value::Num(self.source.len() as f64)),
                    ("type", Value::from_string(self.kind.clone())),
                ],
                &depth,
                &options,
                &inspect,
                true,
            )
        }
    }

    #[methods]
    impl File {
        #[constructor(hint(js(
            missing_message = "The \"fileBits\" and \"fileName\" arguments must be specified",
            missing_code = "ERR_MISSING_ARGS"
        )))]
        fn new(
            ctx: &mut Ctx,
            bits: Value,
            name: crate::webidl::Usv,
            #[default(Value::Undefined)] options: Value,
        ) -> OpResult<Self> {
            let pieces = read_pieces(ctx, &bits)?;
            let options = read_options(ctx, &options, true)?;
            let source = assemble(ctx, pieces, options.native_endings)?;
            Ok(File::new_native(
                source,
                options.kind,
                name.0,
                options
                    .last_modified
                    .unwrap_or_else(|| crate::time::unix_ms().floor()),
            ))
        }

        #[getter]
        fn name(&self) -> String {
            self.name.clone()
        }

        #[getter]
        fn last_modified(&self) -> f64 {
            self.last_modified
        }

        #[method(hint(js(symbol_for = "nodejs.util.inspect.custom")))]
        fn inspect_custom(
            &self,
            ctx: &mut Ctx,
            this: This<Value>,
            depth: Value,
            options: Value,
            inspect: Value,
        ) -> OpResult<Value> {
            crate::events::inspect_object(
                ctx,
                &this.0,
                vec![
                    ("size", Value::Num(self.base.source.len() as f64)),
                    ("type", Value::from_string(self.base.kind.clone())),
                    ("name", Value::from_string(self.name.clone())),
                    ("lastModified", Value::Num(self.last_modified)),
                ],
                &depth,
                &options,
                &inspect,
                true,
            )
        }
    }

    /// What `new FormData(form, submitter)` returns: the object, then the DOM fills it.
    pub struct FormDataCtor {
        data: FormData,
        populate: Option<(Value, Value)>,
    }

    impl CtorRet<JsHost, FormData> for FormDataCtor {
        fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
            let Self { data, populate } = self;
            let instance = <JsHost as Host>::construct(cx, data)?;
            if let Some((form_element, submitter)) = populate {
                <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
                    populate_from_form(ctx, &instance, &form_element, &submitter)
                        .map_err(|error| error.to_value(ctx))
                })?;
            }
            Ok(instance)
        }
    }

    impl lumen::embed::NativeIdentityOwner for FormData {
        const TRACES_NATIVE_VALUES: bool = true;

        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            if let Ok(entries) = self.entries.try_borrow() {
                for entry in entries.iter() {
                    if let EntryValue::File(file) = &entry.value {
                        visit(file);
                    }
                }
            }
        }
    }

    fn js_value(value: &EntryValue) -> Value {
        match value {
            EntryValue::Text(text) => Value::from_string(text.clone()),
            EntryValue::File(file) => file.clone(),
        }
    }

    /// Creates an entry value: a text value, or a `File` for a blob (renamed with `filename`).
    fn entry_value(
        ctx: &mut Ctx,
        method: &str,
        value: &Value,
        filename: Option<&Value>,
    ) -> OpResult<EntryValue> {
        if blob_of(ctx, value).is_some() {
            return Ok(EntryValue::File(file_entry(ctx, value, filename)?));
        }
        if filename.is_some() {
            return Err(OpError::type_error(format!(
                "Failed to execute '{method}' on 'FormData': parameter 2 is not of type 'Blob'."
            )));
        }
        Ok(EntryValue::Text(
            usv_string(ctx, value).map_err(OpError::thrown)?,
        ))
    }

    fn filename_arg(filename: &Value) -> Option<&Value> {
        (!matches!(filename, Value::Undefined)).then_some(filename)
    }

    #[methods]
    impl FormData {
        #[constructor]
        fn new(
            ctx: &mut Ctx,
            #[default(Value::Undefined)] form: Value,
            #[default(Value::Undefined)] submitter: Value,
        ) -> OpResult<FormDataCtor> {
            let populate = match form {
                Value::Undefined => None,
                form => {
                    if FormBridge::current(ctx).is_none() {
                        return Err(OpError::type_error("FormData(form) requires a DOM form"));
                    }
                    Some((form, submitter))
                }
            };
            Ok(FormDataCtor {
                data: FormData::new_native(),
                populate,
            })
        }

        #[method(hint(js(missing_message = "The \"name\" and \"value\" arguments must be specified", missing_code = "ERR_MISSING_ARGS")))]
        fn append(
            this: This<Value>,
            ctx: &mut Ctx,
            name: crate::webidl::Usv,
            value: Value,
            #[default(Value::Undefined)] filename: Value,
        ) -> OpResult<()> {
            let value = entry_value(ctx, "append", &value, filename_arg(&filename))?;
            push_entry(ctx, &this, Entry { name: name.0, value })
        }

        #[method(hint(js(missing_message = "The \"name\" argument must be specified", missing_code = "ERR_MISSING_ARGS")))]
        fn delete(this: This<Value>, ctx: &mut Ctx, name: crate::webidl::Usv) -> OpResult<()> {
            let entries = entries_of(ctx, &this)?;
            entries.borrow_mut().retain(|entry| entry.name != name.0);
            Ok(())
        }

        #[method(hint(js(missing_message = "The \"name\" argument must be specified", missing_code = "ERR_MISSING_ARGS")))]
        fn get(this: This<Value>, ctx: &mut Ctx, name: crate::webidl::Usv) -> OpResult<Value> {
            let entries = entries_of(ctx, &this)?;
            let entries = entries.borrow();
            Ok(entries
                .iter()
                .find(|entry| entry.name == name.0)
                .map_or(Value::Null, |entry| js_value(&entry.value)))
        }

        #[method(hint(js(missing_message = "The \"name\" argument must be specified", missing_code = "ERR_MISSING_ARGS")))]
        fn get_all(this: This<Value>, ctx: &mut Ctx, name: crate::webidl::Usv) -> OpResult<Value> {
            let entries = entries_of(ctx, &this)?;
            let values: Vec<Value> = entries
                .borrow()
                .iter()
                .filter(|entry| entry.name == name.0)
                .map(|entry| js_value(&entry.value))
                .collect();
            Ok(ctx.make_array(values))
        }

        #[method(hint(js(missing_message = "The \"name\" argument must be specified", missing_code = "ERR_MISSING_ARGS")))]
        fn has(this: This<Value>, ctx: &mut Ctx, name: crate::webidl::Usv) -> OpResult<bool> {
            let entries = entries_of(ctx, &this)?;
            let found = entries.borrow().iter().any(|entry| entry.name == name.0);
            Ok(found)
        }

        #[method(hint(js(missing_message = "The \"name\" and \"value\" arguments must be specified", missing_code = "ERR_MISSING_ARGS")))]
        fn set(
            this: This<Value>,
            ctx: &mut Ctx,
            name: crate::webidl::Usv,
            value: Value,
            #[default(Value::Undefined)] filename: Value,
        ) -> OpResult<()> {
            let entries = entries_of(ctx, &this)?;
            let value = entry_value(ctx, "set", &value, filename_arg(&filename))?;
            if matches!(value, EntryValue::File(_)) {
                ctx.ensure_native_identity_owner::<FormData>(&this)?;
            }
            let mut entries = entries.borrow_mut();
            let mut replacement = Some(value);
            entries.retain_mut(|entry| {
                if entry.name != name.0 {
                    return true;
                }
                match replacement.take() {
                    Some(value) => {
                        entry.value = value;
                        true
                    }
                    None => false,
                }
            });
            if let Some(value) = replacement {
                entries.push(Entry {
                    name: name.0,
                    value,
                });
            }
            Ok(())
        }

        #[method(hint(js(also_iterator)))]
        fn entries(this: This<Value>, ctx: &mut Ctx) -> OpResult<FormDataIterator> {
            iterator(ctx, &this, IterKind::Entries)
        }

        fn keys(this: This<Value>, ctx: &mut Ctx) -> OpResult<FormDataIterator> {
            iterator(ctx, &this, IterKind::Keys)
        }

        fn values(this: This<Value>, ctx: &mut Ctx) -> OpResult<FormDataIterator> {
            iterator(ctx, &this, IterKind::Values)
        }

        #[method(hint(js(
            missing_message = "The \"callback\" argument must be of type function. Received undefined",
            missing_code = "ERR_INVALID_ARG_TYPE"
        )))]
        fn for_each(
            this: This<Value>,
            ctx: &mut Ctx,
            callback: Value,
            #[default(Value::Undefined)] this_arg: Value,
        ) -> OpResult<()> {
            let entries = entries_of(ctx, &this)?;
            if !callback.is_callable() {
                return Err(invalid_arg_type(
                    ctx,
                    "callback",
                    "of type function",
                    &callback,
                ));
            }
            let mut index = 0;
            loop {
                let entry = entries.borrow().get(index).cloned();
                let Some(entry) = entry else {
                    return Ok(());
                };
                ctx.invoke(
                    callback.clone(),
                    this_arg.clone(),
                    &[js_value(&entry.value), Value::from_string(entry.name.clone()), this.0.clone()],
                )
                .map_err(OpError::thrown)?;
                index += 1;
            }
        }
    }

    fn iterator(ctx: &mut Ctx, this: &Value, kind: IterKind) -> OpResult<FormDataIterator> {
        Ok(FormDataIterator {
            entries: entries_of(ctx, this)?,
            kind,
            index: 0,
        })
    }

    #[methods]
    impl FormDataIterator {
        #[proto(next)]
        fn next(this: This<Value>, ctx: &mut Ctx) -> OpResult<Option<Value>> {
            let state = ctx
                .instance_data::<FormDataIterator>(&this)
                .ok_or_else(|| invalid_this("FormDataIterator"))?;
            let mut state = state.borrow_mut();
            let entry = state.entries.borrow().get(state.index).cloned();
            let Some(entry) = entry else {
                return Ok(None);
            };
            state.index += 1;
            Ok(Some(match state.kind {
                IterKind::Keys => Value::from_string(entry.name.clone()),
                IterKind::Values => js_value(&entry.value),
                IterKind::Entries => {
                    ctx.make_array(vec![Value::from_string(entry.name.clone()), js_value(&entry.value)])
                }
            }))
        }
    }
}
