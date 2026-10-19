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

/// Brand-check a File without inspecting author properties.
pub fn is_file(ctx:&mut Ctx,value:&Value)->bool {
    matches!(value,Value::Obj(_)) && ctx.with_instance::<File,_>(value,|_|()).is_ok()
}

/// Clone the actual entry list, preserving File object identities.
pub fn clone_form_data(ctx:&mut Ctx,data:&Value)->OpResult<Value> {
    let entries=entries_of(ctx,data)?;
    let result=new_form_data(ctx);
    for entry in entries.borrow().iter().cloned(){push_entry(ctx,&result,entry)?;}
    Ok(result)
}

/// Append actual entry values without calling author-controlled methods.
pub fn append_form_data(ctx:&mut Ctx,target:&Value,source:&Value)->OpResult<()> {
    let source_entries=entries_of(ctx,source)?;let target_entries=entries_of(ctx,target)?;
    if Rc::ptr_eq(&source_entries,&target_entries){
        let entries=source_entries.borrow().clone();
        for entry in entries{push_entry(ctx,target,entry)?;}
    }else{
        for entry in source_entries.borrow().iter().cloned(){push_entry(ctx,target,entry)?;}
    }Ok(())
}

pub fn append_file_value(ctx:&mut Ctx,data:&Value,name:&str,file:Value)->OpResult<()> {
    if !is_file(ctx,&file) {return Err(OpError::type_error("form entry requires File"));}
    push_entry(ctx,data,Entry{name:name.to_owned(),value:EntryValue::File(file)})
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
                FormValue::File(snapshot_file(ctx,&file)?)
            }
        };
        out.push(FormEntry {
            name: entry.name,
            value,
        });
    }
    Ok(out)
}

/// Attachment-free persisted state for a form-associated custom element.
/// Files retain shared immutable bytes and metadata, never a script or document root.
#[derive(Clone)]
pub enum StoredFormValue {
    Value(FormValue),
    Entries(Vec<FormEntry>),
}
fn snapshot_file(ctx:&mut Ctx,value:&Value)->OpResult<FormFile> {
    let (name,last_modified,media_type,source)=ctx.with_instance::<File,_>(value,|file|(file.name.clone(),file.last_modified,file.base.kind.clone(),file.base.source.clone()))?;
    Ok(FormFile{name,last_modified,media_type,bytes:source.bytes(ctx)?})
}
impl StoredFormValue {
    pub fn retained_bytes(&self)->usize {
        fn value_bytes(value:&FormValue)->usize {match value{FormValue::Text(value)=>value.len(),FormValue::File(file)=>file.name.len().saturating_add(file.media_type.len()).saturating_add(file.bytes.len())}}
        match self {Self::Value(value)=>value_bytes(value),Self::Entries(entries)=>entries.iter().fold(0usize,|size,entry|size.saturating_add(entry.name.len()).saturating_add(value_bytes(&entry.value)))}
    }
}
pub fn snapshot_form_value(ctx:&mut Ctx,value:&Value)->OpResult<StoredFormValue> {
    if is_form_data(ctx,value){return form_data_entries(ctx,value).map(StoredFormValue::Entries);}
    if is_file(ctx,value){return snapshot_file(ctx,value).map(|value|StoredFormValue::Value(FormValue::File(value)));}
    let Value::Str(value)=value else{return Err(OpError::type_error("invalid custom form state"));};
    Ok(StoredFormValue::Value(FormValue::Text(value.as_str().to_owned())))
}
pub fn restore_form_value(ctx:&mut Ctx,value:&StoredFormValue)->OpResult<Value> {
    match value {
        StoredFormValue::Value(FormValue::Text(value))=>Ok(Value::from_string(value.clone())),
        StoredFormValue::Value(FormValue::File(file))=>Ok(new_file(ctx,file.bytes.clone(),&file.name,&file.media_type,file.last_modified)),
        StoredFormValue::Entries(entries)=>{
            let data=new_form_data(ctx);
            for entry in entries{match &entry.value{FormValue::Text(value)=>append_text(ctx,&data,&entry.name,value)?,FormValue::File(file)=>append_file(ctx,&data,&entry.name,file.clone())?}}
            Ok(data)
        }
    }
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
