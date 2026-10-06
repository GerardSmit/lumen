//! `FormData` as seen from Rust: the DOM bridge that fills one from a form, the snapshot the
//! form navigation reads, and the multipart codec glue.

use super::urls::random_token;
use super::web::bindings::{Entry, EntryValue, File, FormData};
use super::{blob_of, new_file, normalize_type, Bytes};
use crate::realm_services::RealmServices;
use crate::webidl::usv_string;
use lumen::embed::{Ctx, OpError, OpResult, Value};
use lumen_common::multipart;
use std::rc::Rc;

/// A file entry of a form submission.
#[derive(Clone)]
pub struct FormFile {
    pub name: String,
    pub media_type: String,
    pub last_modified: f64,
    pub bytes: Bytes,
}

#[derive(Clone)]
pub enum FormValue {
    Text(String),
    File(FormFile),
}

#[derive(Clone)]
pub struct FormEntry {
    pub name: String,
    pub value: FormValue,
}

/// A `multipart/form-data` body and the `Content-Type` that carries its boundary.
pub struct EncodedForm {
    pub body: Vec<u8>,
    pub content_type: String,
}

type Populate = dyn Fn(&mut Ctx, &Value, &Value, &Value) -> OpResult<()>;

/// What `new FormData(form, submitter)` needs from the DOM. `lumen-host` knows no DOM, so the
/// HTML embedder registers this per realm; without it `new FormData(form)` throws.
pub struct FormBridge {
    populate: Rc<Populate>,
}

impl FormBridge {
    /// Registers `populate(ctx, formData, form, submitter)` for the active realm. It validates
    /// `form`/`submitter`, appends the entry list through [`append_text`]/[`append_file`] and
    /// dispatches the `formdata` event.
    pub fn install(
        ctx: &mut Ctx,
        populate: impl Fn(&mut Ctx, &Value, &Value, &Value) -> OpResult<()> + 'static,
    ) {
        RealmServices::replace_current(
            ctx,
            FormBridge {
                populate: Rc::new(populate),
            },
        );
    }

    pub(crate) fn current(ctx: &mut Ctx) -> Option<Rc<FormBridge>> {
        RealmServices::<FormBridge>::current(ctx)
    }
}

pub(crate) fn populate_from_form(
    ctx: &mut Ctx,
    data: &Value,
    form: &Value,
    submitter: &Value,
) -> OpResult<()> {
    let bridge = FormBridge::current(ctx)
        .ok_or_else(|| OpError::type_error("FormData(form) requires a DOM form"))?;
    (bridge.populate)(ctx, data, form, submitter)
}

pub(crate) type Entries = Rc<std::cell::RefCell<Vec<Entry>>>;

pub(crate) fn entries_of(ctx: &mut Ctx, data: &Value) -> OpResult<Entries> {
    ctx.with_instance::<FormData, _>(data, |data| data.entries.clone())
        .map_err(|_| crate::webidl::invalid_this("FormData"))
}

/// Stores `entry`; a `File` value makes the `FormData` trace its entries.
pub(crate) fn push_entry(ctx: &mut Ctx, data: &Value, entry: Entry) -> OpResult<()> {
    let entries = entries_of(ctx, data)?;
    if matches!(entry.value, EntryValue::File(_)) {
        ctx.ensure_native_identity_owner::<FormData>(data)?;
    }
    entries.borrow_mut().push(entry);
    Ok(())
}

/// The `File` an entry holds for `value`: `value` itself when it already is a `File` and no
/// `filename` is given, otherwise a new `File` over the same bytes.
pub(crate) fn file_entry(
    ctx: &mut Ctx,
    value: &Value,
    filename: Option<&Value>,
) -> OpResult<Value> {
    let view = blob_of(ctx, value).expect("callers pass a blob");
    let existing = ctx
        .with_instance::<File, _>(value, |file| (file.name.clone(), file.last_modified))
        .ok();
    let name = match filename {
        Some(filename) => usv_string(ctx, filename).map_err(OpError::thrown)?,
        None => match &existing {
            Some(_) => return Ok(value.clone()),
            None => "blob".to_owned(),
        },
    };
    let last_modified = existing.map_or_else(
        || crate::time::unix_ms().floor(),
        |(_, last_modified)| last_modified,
    );
    Ok(ctx.new_instance(File::new_native(
        view.source,
        view.content_type,
        name,
        last_modified,
    )))
}

pub fn new_form_data(ctx: &mut Ctx) -> Value {
    ctx.new_instance(FormData::new_native())
}

pub fn is_form_data(ctx: &mut Ctx, value: &Value) -> bool {
    matches!(value, Value::Obj(_)) && ctx.with_instance::<FormData, _>(value, |_| ()).is_ok()
}

pub fn append_text(ctx: &mut Ctx, data: &Value, name: &str, value: &str) -> OpResult<()> {
    push_entry(
        ctx,
        data,
        Entry {
            name: name.to_owned(),
            value: EntryValue::Text(value.to_owned()),
        },
    )
}

pub fn append_file(ctx: &mut Ctx, data: &Value, name: &str, file: FormFile) -> OpResult<()> {
    let file = new_file(
        ctx,
        file.bytes,
        &file.name,
        &file.media_type,
        file.last_modified,
    );
    push_entry(
        ctx,
        data,
        Entry {
            name: name.to_owned(),
            value: EntryValue::File(file),
        },
    )
}

/// The entry list of `data`, with file contents read.
pub fn form_data_entries(ctx: &mut Ctx, data: &Value) -> OpResult<Vec<FormEntry>> {
    let entries = entries_of(ctx, data)?;
    let snapshot: Vec<Entry> = entries.borrow().clone();
    let mut out = Vec::with_capacity(snapshot.len());
    for entry in snapshot {
        let value = match entry.value {
            EntryValue::Text(text) => FormValue::Text(text),
            EntryValue::File(file) => {
                let (name, last_modified, kind, source) = ctx.with_instance::<File, _>(
                    &file,
                    |file| {
                        (
                            file.name.clone(),
                            file.last_modified,
                            file.base.kind.clone(),
                            file.base.source.clone(),
                        )
                    },
                )?;
                FormValue::File(FormFile {
                    name,
                    media_type: kind,
                    last_modified,
                    bytes: source.bytes(ctx)?,
                })
            }
        };
        out.push(FormEntry {
            name: entry.name,
            value,
        });
    }
    Ok(out)
}

/// Serializes `entries` as `multipart/form-data` with a fresh boundary.
pub fn encode_entries(ctx: &mut Ctx, entries: &[FormEntry]) -> OpResult<EncodedForm> {
    let boundary = format!("----lumenFormBoundary{}", random_token(ctx)?);
    let body = multipart::encode(
        entries.iter().map(|entry| multipart::Entry {
            name: &entry.name,
            value: match &entry.value {
                FormValue::Text(text) => multipart::Value::Text(text),
                FormValue::File(file) => multipart::Value::File {
                    filename: &file.name,
                    content_type: if file.media_type.is_empty() {
                        "application/octet-stream"
                    } else {
                        &file.media_type
                    },
                    bytes: &file.bytes,
                },
            },
        }),
        &boundary,
    );
    Ok(EncodedForm {
        body,
        content_type: format!("multipart/form-data; boundary={boundary}"),
    })
}

/// `data` as a `multipart/form-data` body.
pub fn encode_form_data(ctx: &mut Ctx, data: &Value) -> OpResult<EncodedForm> {
    let entries = form_data_entries(ctx, data)?;
    encode_entries(ctx, &entries)
}

/// A `FormData` of the parts of a `multipart/form-data` body; file parts share `bytes`.
pub fn decode_multipart(ctx: &mut Ctx, bytes: &Bytes, boundary: &str) -> OpResult<Value> {
    let data = new_form_data(ctx);
    for part in multipart::decode(bytes, boundary) {
        let body = bytes.slice(part.body.start, part.body.end);
        match part.filename {
            Some(filename) => {
                let file = new_file(
                    ctx,
                    body,
                    &filename,
                    &normalize_type(&part.content_type),
                    crate::time::unix_ms().floor(),
                );
                push_entry(
                    ctx,
                    &data,
                    Entry {
                        name: part.name,
                        value: EntryValue::File(file),
                    },
                )?;
            }
            None => append_text(
                ctx,
                &data,
                &part.name,
                &String::from_utf8_lossy(&body),
            )?,
        }
    }
    Ok(data)
}
