//! JavaScript form adapters over the host-testable shared HTML algorithms.
use super::*;
use lumen_html::forms as core_forms;
use std::collections::{HashMap, HashSet};

pub use lumen_html::forms::{
    FormEntry, FormEntryValue, FormFile, ValidityState, default_value as default_control_value,
    form_controls, form_entries, form_entries_with_values, validity, validity_with_value,
};

pub struct FormSubmissionRequest {
    pub metadata: core_forms::FormSubmission,
    pub form_data: Value,
}

#[derive(Clone)]
pub struct FilePickerRequest {
    pub accept: Vec<String>,
    pub multiple: bool,
    pub capture: Option<String>,
    target: PickerTarget,
}

#[derive(Clone)]
struct PickerTarget {
    realm: Rc<DomRealm>,
    node: NodeId,
}

#[derive(Clone, Default)]
struct Defaults {
    value: Option<String>,
    value_attribute_present: bool,
    checked: bool,
    selected: bool,
}

/// State held by each `DomRealm`. Current control values remain in the shared
/// DOM attributes, which are also consumed by editing and rendering.
#[derive(Clone, Default)]
pub struct FormState {
    defaults: HashMap<NodeId, Defaults>,
    dirty: HashSet<NodeId>,
    custom_messages: HashMap<NodeId, String>,
    selectedness: HashMap<NodeId, bool>,
    files: HashMap<NodeId, Vec<FormFile>>,
    file_lists: HashMap<NodeId, Rc<RefCell<FileListData>>>,
}

struct FileListData {
    values: Vec<Value>,
    wrapper: Option<WeakValue>,
}

/// Drop adapter state after the shared DOM has reclaimed detached nodes.
/// Call from `DomRealm::reap_detached` after `destroy_subtree` completes.
pub fn reap(state: &mut FormState, document: &lumen_html::Document) {
    state
        .defaults
        .retain(|node, _| document.kind(*node).is_ok());
    state.dirty.retain(|node| document.kind(*node).is_ok());
    state
        .custom_messages
        .retain(|node, _| document.kind(*node).is_ok());
    state
        .selectedness
        .retain(|node, _| document.kind(*node).is_ok());
    state.files.retain(|node, _| document.kind(*node).is_ok());
    state
        .file_lists
        .retain(|node, _| document.kind(*node).is_ok());
}

/// Move per-control IDL state along with nodes transferred to another document.
pub fn adopt_nodes_into(
    source: &mut FormState,
    target: &mut FormState,
    mapping: &[(NodeId, NodeId)],
) {
    for &(old, new) in mapping {
        if let Some(value) = source.defaults.remove(&old) {
            target.defaults.insert(new, value);
        }
        if source.dirty.remove(&old) {
            target.dirty.insert(new);
        }
        if let Some(value) = source.custom_messages.remove(&old) {
            target.custom_messages.insert(new, value);
        }
        if let Some(value) = source.selectedness.remove(&old) {
            target.selectedness.insert(new, value);
        }
        if let Some(value) = source.files.remove(&old) {
            target.files.insert(new, value);
        }
        if let Some(value) = source.file_lists.remove(&old) {
            target.file_lists.insert(new, value);
        }
    }
}

pub fn install(ctx: &mut Ctx) {
    ctx.class_constructor::<DomValidityState>();
    ctx.class_constructor::<DomFileList>();
}

/// Capture markup defaults before a DOM-backed value or checked state is
/// mutated. Call this from the corresponding IDL setter.
pub fn capture_defaults(state: &mut FormState, document: &lumen_html::Document, node: NodeId) {
    if state.defaults.contains_key(&node) {
        return;
    }
    let Some(NodeKind::Element { attributes, .. }) = document.kind(node).ok() else {
        return;
    };
    let value = core_forms::default_value(document, node);
    let value_attribute_present = attributes.iter().any(|(name, _)| name == "value");
    let checked = attributes.iter().any(|(name, _)| name == "checked");
    let selected = attributes.iter().any(|(name, _)| name == "selected");
    state.defaults.insert(
        node,
        Defaults {
            value,
            value_attribute_present,
            checked,
            selected,
        },
    );
}

/// Record an IDL `value` write while preserving the original reset default.
pub fn set_control_value(
    realm: &DomRealm,
    state: &mut FormState,
    node: NodeId,
    value: &str,
) -> OpResult<()> {
    capture_defaults(state, realm.session.borrow().document(), node);
    let name = match realm.session.borrow().document().kind(node).ok() {
        Some(NodeKind::Element { name, .. }) => name.clone(),
        _ => return Err(OpError::new("TypeError", "value requires a form control")),
    };
    let file_input = name == "input"
        && matches!(realm.session.borrow().document().kind(node),
        Ok(NodeKind::Element { attributes, .. }) if attributes.iter().find(|(key, _)| key.as_str() == "type").is_some_and(|(_, value)| value.eq_ignore_ascii_case("file")));
    if file_input {
        if !value.is_empty() {
            return Err(OpError::new(
                "InvalidStateError",
                "a file input value can only be cleared",
            ));
        }
        state.files.remove(&node);
        if let Some(slot) = state.file_lists.get(&node) {
            slot.borrow_mut().values.clear();
        }
        state.dirty.insert(node);
        realm.invalidate_editing_for_value_change(node);
        return Ok(());
    }
    if name == "select" {
        let options = core_forms::select_options(realm.session.borrow().document(), node);
        let multiple = matches!(realm.session.borrow().document().kind(node), Ok(NodeKind::Element { attributes, .. }) if attributes.iter().any(|(name, _)| name == "multiple"));
        let mut matched = false;
        for option in options {
            capture_defaults(state, realm.session.borrow().document(), option);
            let selected = core_forms::option_value(realm.session.borrow().document(), option)
                .as_deref()
                == Some(value)
                && (multiple || !matched);
            state.selectedness.insert(option, selected);
            matched |= selected;
        }
    } else {
        realm
            .session
            .borrow_mut()
            .document_mut()
            .set_attribute(node, "value", value)
            .map_err(dom_error)?;
        if name == "input" || name == "textarea" {
            // Reentrant IDL assignments abort pending host edits even if the
            // assigned string equals the current value.
            realm.invalidate_editing_for_value_change(node);
        }
    }
    state.dirty.insert(node);
    Ok(())
}

/// Publish file-picker output for a file input. The host owns the picker and
/// supplies immutable file metadata and bytes; JavaScript receives actual File
/// objects created by this adapter rather than path strings.
pub fn set_input_files(
    ctx: &mut Ctx,
    realm: &DomRealm,
    state: &mut FormState,
    node: NodeId,
    files: Vec<FormFile>,
) -> OpResult<()> {
    let is_file = matches!(realm.session.borrow().document().kind(node),
        Ok(NodeKind::Element { name, attributes, .. }) if name == "input" && attributes.iter().find(|(key, _)| key.as_str() == "type").is_some_and(|(_, value)| value.eq_ignore_ascii_case("file")));
    if !is_file {
        return Err(OpError::new(
            "TypeError",
            "files requires an input type=file",
        ));
    }
    let values = files
        .iter()
        .map(|file| file_object(ctx, file))
        .collect::<OpResult<Vec<_>>>()?;
    let slot = state
        .file_lists
        .entry(node)
        .or_insert_with(|| {
            Rc::new(RefCell::new(FileListData {
                values: Vec::new(),
                wrapper: None,
            }))
        })
        .clone();
    slot.borrow_mut().values = values;
    state.files.insert(node, files);
    state.dirty.insert(node);
    Ok(())
}

/// Build a typed picker request from a file input's current attributes.
pub fn file_picker_request(realm: &Rc<DomRealm>, node: NodeId) -> OpResult<FilePickerRequest> {
    let document = realm.session.borrow();
    let NodeKind::Element {
        name, attributes, ..
    } = document.document().kind(node).map_err(dom_error)?
    else {
        return Err(OpError::new(
            "TypeError",
            "showPicker requires an input element",
        ));
    };
    if name != "input"
        || !attributes
            .iter()
            .find(|(key, _)| key.as_str() == "type")
            .is_some_and(|(_, value)| value.eq_ignore_ascii_case("file"))
    {
        return Err(OpError::new(
            "NotSupportedError",
            "only file input pickers are supported",
        ));
    }
    if core_forms::is_disabled(document.document(), node) {
        return Err(OpError::new(
            "InvalidStateError",
            "disabled file input cannot show a picker",
        ));
    }
    let get = |key: &str| {
        attributes
            .iter()
            .find(|(name, _)| name.as_str() == key)
            .map(|(_, value)| value.clone())
    };
    let accept = get("accept")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    Ok(FilePickerRequest {
        accept,
        multiple: get("multiple").is_some(),
        capture: get("capture"),
        target: PickerTarget {
            realm: realm.clone(),
            node,
        },
    })
}

/// Commit picker results, then dispatch the standards-ordered `input` and
/// `change` notifications. The native host calls this only after a user picks
/// files; canceled pickers leave the existing selection unchanged.
pub fn complete_file_picker(
    ctx: &mut Ctx,
    request: &FilePickerRequest,
    files: Vec<FormFile>,
) -> OpResult<()> {
    let (realm, node) = request
        .target
        .realm
        .resolve_adopted_node(request.target.node);
    let changed = realm.forms.borrow().files.get(&node) != Some(&files);
    set_input_files(ctx, &realm, &mut realm.forms.borrow_mut(), node, files)?;
    if changed {
        realm.dispatch(ctx, node, "input", true, false, &[])?;
        realm.dispatch(ctx, node, "change", true, false, &[])?;
    } else {
        realm.dispatch(ctx, node, "cancel", true, false, &[])?;
    }
    Ok(())
}

pub fn cancel_file_picker(ctx: &mut Ctx, request: &FilePickerRequest) -> OpResult<()> {
    let (realm, node) = request
        .target
        .realm
        .resolve_adopted_node(request.target.node);
    realm.dispatch(ctx, node, "cancel", true, false, &[])?;
    Ok(())
}

/// Stable live FileList object for an input control.
pub fn files_value(
    ctx: &mut Ctx,
    realm: &DomRealm,
    node: NodeId,
    state: &RefCell<FormState>,
) -> OpResult<Value> {
    let is_file = matches!(realm.session.borrow().document().kind(node),
        Ok(NodeKind::Element { name, attributes, .. }) if name == "input" && attributes.iter().find(|(key, _)| key.as_str() == "type").is_some_and(|(_, value)| value.eq_ignore_ascii_case("file")));
    if !is_file {
        return Ok(Value::Null);
    }
    let slot = {
        let mut state = state.borrow_mut();
        state
            .file_lists
            .entry(node)
            .or_insert_with(|| {
                Rc::new(RefCell::new(FileListData {
                    values: Vec::new(),
                    wrapper: None,
                }))
            })
            .clone()
    };
    if let Some(value) = slot.borrow().wrapper.as_ref().and_then(WeakValue::upgrade) {
        return Ok(value);
    }
    let value = ctx.new_instance(DomFileList { data: slot.clone() });
    slot.borrow_mut().wrapper = ctx.weak_value(&value);
    Ok(value)
}

pub fn set_textarea_default_value(
    realm: &DomRealm,
    state: &mut FormState,
    node: NodeId,
    value: &str,
) -> OpResult<()> {
    let text = realm
        .session
        .borrow_mut()
        .document_mut()
        .create(NodeKind::Text(value.to_owned()))
        .map_err(dom_error)?;
    realm
        .session
        .borrow_mut()
        .document_mut()
        .replace_children(node, text)
        .map_err(dom_error)?;
    if !state.dirty.contains(&node) {
        realm
            .session
            .borrow_mut()
            .document_mut()
            .remove_attribute(node, "value")
            .map_err(dom_error)?;
        realm.invalidate_editing_for_value_change(node);
    }
    Ok(())
}

pub fn set_checked(
    realm: &DomRealm,
    state: &mut FormState,
    node: NodeId,
    checked: bool,
) -> OpResult<()> {
    capture_defaults(state, realm.session.borrow().document(), node);
    if checked {
        realm
            .session
            .borrow_mut()
            .document_mut()
            .set_attribute(node, "checked", "")
            .map_err(dom_error)?;
    } else {
        realm
            .session
            .borrow_mut()
            .document_mut()
            .remove_attribute(node, "checked")
            .map_err(dom_error)?;
    }
    state.dirty.insert(node);
    Ok(())
}

pub fn control_value(realm: &DomRealm, node: NodeId) -> OpResult<String> {
    let snapshot = realm.forms.borrow().clone();
    control_value_with_state(realm, &snapshot, node)
}

pub fn selected_index(realm: &DomRealm, node: NodeId) -> OpResult<isize> {
    let snapshot = realm.forms.borrow().clone();
    selected_index_with_state(realm, &snapshot, node)
}

pub fn control_value_with_state(
    realm: &DomRealm,
    state: &FormState,
    node: NodeId,
) -> OpResult<String> {
    let session = realm.session.borrow();
    let document = session.document();
    if matches!(document.kind(node), Ok(NodeKind::Element { name, attributes, .. }) if name == "input" && attributes.iter().find(|(key, _)| key.as_str() == "type").is_some_and(|(_, value)| value.eq_ignore_ascii_case("file")))
    {
        return Ok(state
            .files
            .get(&node)
            .and_then(|files| files.first())
            .map_or_else(String::new, |file| format!("C:\\fakepath\\{}", file.name)));
    }
    if matches!(document.kind(node), Ok(NodeKind::Element { name, .. }) if name == "select") {
        let selectedness = core_forms::select_options(document, node)
            .into_iter()
            .filter_map(|option| {
                state
                    .selectedness
                    .get(&option)
                    .map(|selected| (option, *selected))
            })
            .collect::<Vec<_>>();
        return Ok(core_forms::select_value_with(document, node, &selectedness));
    }
    default_control_value(document, node)
        .ok_or_else(|| OpError::new("TypeError", "value requires a form control"))
}

pub fn selected_index_with_state(
    realm: &DomRealm,
    state: &FormState,
    node: NodeId,
) -> OpResult<isize> {
    if !matches!(realm.session.borrow().document().kind(node), Ok(NodeKind::Element { name, .. }) if name == "select")
    {
        return Err(OpError::new(
            "TypeError",
            "selectedIndex requires a select element",
        ));
    }
    let session = realm.session.borrow();
    let document = session.document();
    let selectedness = core_forms::select_options(document, node)
        .iter()
        .filter_map(|option| {
            state
                .selectedness
                .get(option)
                .map(|selected| (*option, *selected))
        })
        .collect::<Vec<_>>();
    Ok(core_forms::selected_index_with(
        document,
        node,
        &selectedness,
    ))
}

pub fn set_select_selected_index(
    realm: &DomRealm,
    state: &mut FormState,
    node: NodeId,
    index: isize,
) -> OpResult<()> {
    let options = core_forms::select_options(realm.session.borrow().document(), node);
    for &option in &options {
        capture_defaults(state, realm.session.borrow().document(), option);
    }
    for (position, option) in options.iter().enumerate() {
        state
            .selectedness
            .insert(*option, index >= -1 && position as isize == index);
    }
    state.dirty.insert(node);
    Ok(())
}

pub fn option_selected_with_state(realm: &DomRealm, state: &FormState, node: NodeId) -> bool {
    state.selectedness.get(&node).copied().unwrap_or_else(|| matches!(realm.session.borrow().document().kind(node), Ok(NodeKind::Element { attributes, .. }) if attributes.iter().any(|(name, _)| name == "selected")))
}

pub fn set_option_selected(
    realm: &DomRealm,
    state: &mut FormState,
    node: NodeId,
    selected: bool,
) -> OpResult<()> {
    capture_defaults(state, realm.session.borrow().document(), node);
    let select = {
        let session = realm.session.borrow();
        let document = session.document();
        let mut parent = document.parent(node).ok().flatten();
        let mut select = None;
        while let Some(current) = parent {
            match document.kind(current) {
                Ok(NodeKind::Element {
                    name, attributes, ..
                }) if name == "select" => {
                    select = Some((
                        current,
                        attributes.iter().any(|(name, _)| name == "multiple"),
                    ));
                    break;
                }
                _ => parent = document.parent(current).ok().flatten(),
            }
        }
        select
    };
    if let Some((select, false)) = select {
        if selected {
            let options = core_forms::select_options(realm.session.borrow().document(), select);
            for option in options {
                capture_defaults(state, realm.session.borrow().document(), option);
                state.selectedness.insert(option, option == node);
            }
        } else {
            state.selectedness.insert(node, false);
        }
    } else {
        state.selectedness.insert(node, selected);
    }
    state.dirty.insert(node);
    Ok(())
}

pub fn option_selected(realm: &DomRealm, node: NodeId) -> bool {
    let snapshot = realm.forms.borrow().clone();
    option_selected_with_state(realm, &snapshot, node)
}

pub fn form_owner_value(ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId) -> Value {
    core_forms::form_owner(realm.session.borrow().document(), node)
        .map_or(Value::Null, |form| realm.wrap(ctx, form))
}

/// Build a FormData object using the browser's existing FormData class.
pub fn form_data(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    form: NodeId,
    submitter: Option<NodeId>,
) -> OpResult<Value> {
    // Native host code must resolve constructors from this realm's backing global. In
    // a browsing context, globalThis is the published WindowProxy and may be cross-origin
    // guarded while the constructor is being looked up.
    let global = ctx.global_object();
    let constructor = ctx
        .get_member(&global, "FormData")
        .map_err(|_| OpError::new("TypeError", "FormData constructor is unavailable"))?;
    let data = ctx
        .construct_value(constructor, &[])
        .map_err(OpError::thrown)?;
    populate_form_data(ctx, realm, form, submitter, data.clone())?;
    Ok(data)
}

fn file_object(ctx: &mut Ctx, file: &FormFile) -> OpResult<Value> {
    let global = ctx.global_object();
    let array_ctor = ctx
        .get_member(&global, "Uint8Array")
        .map_err(|_| OpError::new("TypeError", "Uint8Array is unavailable"))?;
    let bytes = ctx
        .construct_value(array_ctor, &[Value::Num(file.bytes.len() as f64)])
        .map_err(OpError::thrown)?;
    for (index, byte) in file.bytes.iter().copied().enumerate() {
        ctx.set_member(&bytes, &index.to_string(), Value::Num(byte as f64))
            .map_err(|_| OpError::new("Error", "file byte assignment failed"))?;
    }
    let options = Value::Obj(ctx.new_object());
    ctx.set_member(&options, "type", Value::Str(file.media_type.clone().into()))
        .map_err(|_| OpError::new("Error", "file type assignment failed"))?;
    ctx.set_member(
        &options,
        "lastModified",
        Value::Num(file.last_modified as f64),
    )
    .map_err(|_| OpError::new("Error", "file timestamp assignment failed"))?;
    let constructor = ctx
        .get_member(&global, "File")
        .map_err(|_| OpError::new("TypeError", "File constructor is unavailable"))?;
    let array_ctor = ctx
        .get_member(&global, "Array")
        .map_err(|_| OpError::new("Error", "Array constructor is unavailable"))?;
    let parts = ctx
        .construct_value(array_ctor, &[Value::Num(1.0)])
        .map_err(OpError::thrown)?;
    ctx.set_member(&parts, "0", bytes)
        .map_err(|_| OpError::new("Error", "file parts assignment failed"))?;
    ctx.construct_value(
        constructor,
        &[parts, Value::Str(file.name.clone().into()), options],
    )
    .map_err(OpError::thrown)
}

/// Populate an existing FormData object from a form and dispatch the
/// non-cancelable `formdata` event with that same object.
pub fn populate_form_data(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    form: NodeId,
    submitter: Option<NodeId>,
    data: Value,
) -> OpResult<()> {
    if let Some(submitter) = submitter {
        let session = realm.session.borrow();
        let document = session.document();
        let is_submitter = lumen_html::forms::form_controls(document, form).contains(&submitter)
            && matches!(document.kind(submitter), Ok(NodeKind::Element { name, attributes, .. }) if (name == "button" && attributes.iter().find(|(key, _)| key.as_str() == "type").map_or("submit", |(_, value)| value).eq_ignore_ascii_case("submit")) || (name == "input" && attributes.iter().find(|(key, _)| key.as_str() == "type").is_some_and(|(_, value)| value.eq_ignore_ascii_case("submit") || value.eq_ignore_ascii_case("image"))));
        if !is_submitter {
            return Err(OpError::new(
                "TypeError",
                "submitter is not a submit button associated with this form",
            ));
        }
    }
    let append = ctx
        .get_member(&data, "append")
        .map_err(|_| OpError::new("TypeError", "FormData.append is unavailable"))?;
    let snapshot = realm.forms.borrow().clone();
    let selectedness = snapshot
        .selectedness
        .iter()
        .map(|(node, selected)| (*node, *selected))
        .collect::<Vec<_>>();
    let files = snapshot
        .files
        .iter()
        .map(|(node, files)| (*node, files.clone()))
        .collect::<Vec<_>>();
    let entries = core_forms::form_entries_with_values_selectedness_and_files(
        realm.session.borrow().document(),
        form,
        submitter,
        &[],
        &selectedness,
        &files,
    );
    for entry in entries {
        let value = match entry.value {
            FormEntryValue::Text(value) => Value::Str(value.into()),
            FormEntryValue::File(file) => file_object(ctx, &file)?,
        };
        ctx.invoke(
            append.clone(),
            data.clone(),
            &[Value::Str(entry.name.into()), value],
        )
        .map_err(OpError::thrown)?;
    }
    realm.dispatch(ctx, form, "formdata", false, false, &[("formData", data)])?;
    Ok(())
}

pub fn set_custom_validity(state: &mut FormState, node: NodeId, message: &str) {
    if message.is_empty() {
        state.custom_messages.remove(&node);
    } else {
        state.custom_messages.insert(node, message.to_owned());
    }
}

#[lumen_bind::class(name = "FileList", hint(js(webidl)))]
pub struct DomFileList {
    data: Rc<RefCell<FileListData>>,
}

#[lumen_bind::methods]
impl DomFileList {
    #[proto(len)]
    fn length(&self) -> usize {
        self.data.borrow().values.len()
    }
    #[proto(getitem)]
    fn indexed(&self, index: usize) -> Value {
        self.data
            .borrow()
            .values
            .get(index)
            .cloned()
            .unwrap_or(Value::Undefined)
    }
    fn item(&self, index: usize) -> Value {
        self.data
            .borrow()
            .values
            .get(index)
            .cloned()
            .unwrap_or(Value::Null)
    }
    #[proto(iter)]
    fn values(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let global = ctx.global_object();
        let array_ctor = ctx
            .get_member(&global, "Array")
            .map_err(|_| OpError::new("Error", "Array constructor is unavailable"))?;
        let values = self.data.borrow().values.clone();
        let array = ctx
            .construct_value(array_ctor, &[Value::Num(values.len() as f64)])
            .map_err(OpError::thrown)?;
        for (index, value) in values.into_iter().enumerate() {
            ctx.set_member(&array, &index.to_string(), value)
                .map_err(|_| OpError::new("Error", "FileList iterator setup failed"))?;
        }
        let method = ctx
            .get_member(&array, "values")
            .map_err(|_| OpError::new("Error", "Array iterator is unavailable"))?;
        ctx.invoke(method, array, &[]).map_err(OpError::thrown)
    }
}

#[lumen_bind::class(name = "ValidityState", hint(js(webidl)))]
pub struct DomValidityState {
    state: ValidityState,
}

#[lumen_bind::methods]
impl DomValidityState {
    #[getter]
    fn value_missing(&self) -> bool {
        self.state.value_missing
    }
    #[getter]
    fn type_mismatch(&self) -> bool {
        self.state.type_mismatch
    }
    #[getter]
    fn too_long(&self) -> bool {
        self.state.too_long
    }
    #[getter]
    fn too_short(&self) -> bool {
        self.state.too_short
    }
    #[getter]
    fn pattern_mismatch(&self) -> bool {
        self.state.pattern_mismatch
    }
    #[getter]
    fn range_overflow(&self) -> bool {
        self.state.range_overflow
    }
    #[getter]
    fn range_underflow(&self) -> bool {
        self.state.range_underflow
    }
    #[getter]
    fn step_mismatch(&self) -> bool {
        self.state.step_mismatch
    }
    #[getter]
    fn bad_input(&self) -> bool {
        self.state.bad_input
    }
    #[getter]
    fn custom_error(&self) -> bool {
        self.state.custom_error
    }
    #[getter]
    fn valid(&self) -> bool {
        self.state.valid()
    }
}

fn current_validity(realm: &DomRealm, node: NodeId, state: &FormState) -> OpResult<ValidityState> {
    let session = realm.session.borrow();
    let selectedness = state
        .selectedness
        .iter()
        .map(|(node, selected)| (*node, *selected))
        .collect::<Vec<_>>();
    Ok(core_forms::validity_with_value_and_selectedness(
        session.document(),
        node,
        None,
        state.custom_messages.get(&node).map_or("", String::as_str),
        &selectedness,
    ))
}

pub fn validity_object(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    state: &RefCell<FormState>,
) -> OpResult<Value> {
    let snapshot = state.borrow().clone();
    Ok(ctx.new_instance(DomValidityState {
        state: current_validity(realm, node, &snapshot)?,
    }))
}

pub fn will_validate(realm: &DomRealm, node: NodeId) -> bool {
    core_forms::will_validate(realm.session.borrow().document(), node)
}

pub fn validation_message(
    realm: &DomRealm,
    node: NodeId,
    state: &RefCell<FormState>,
) -> OpResult<String> {
    if !core_forms::will_validate(realm.session.borrow().document(), node) {
        return Ok(String::new());
    }
    let snapshot = state.borrow().clone();
    if let Some(message) = snapshot.custom_messages.get(&node) {
        return Ok(message.clone());
    }
    let validity = current_validity(realm, node, &snapshot)?;
    let message = if validity.value_missing {
        "Please fill out this field."
    } else if validity.type_mismatch {
        "Please enter a value with a valid type."
    } else if validity.range_underflow {
        "Value is below the allowed minimum."
    } else if validity.range_overflow {
        "Value is above the allowed maximum."
    } else if validity.step_mismatch {
        "Please enter a valid value."
    } else if validity.bad_input {
        "Please enter a number."
    } else {
        ""
    };
    Ok(message.into())
}

pub fn check_validity(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    state: &RefCell<FormState>,
) -> OpResult<bool> {
    if !core_forms::will_validate(realm.session.borrow().document(), node) {
        return Ok(true);
    }
    let snapshot = state.borrow().clone();
    let valid = current_validity(realm, node, &snapshot)?.valid();
    if !valid {
        realm.dispatch(ctx, node, "invalid", false, true, &[])?;
    }
    Ok(valid)
}

/// Reports a validity failure through `invalid` and moves focus to the first
/// invalid control. The host has no native validation bubble yet.
pub fn report_validity(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    state: &RefCell<FormState>,
) -> OpResult<bool> {
    let valid = check_validity(ctx, realm, node, state)?;
    if !valid {
        realm.focus(ctx, Some(node))?;
    }
    Ok(valid)
}

/// Validates the form's subtree and dispatches `submit`; the browser host owns
/// the eventual navigation or network request.
pub fn request_submit(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    form: NodeId,
    submitter: Option<NodeId>,
    state: &RefCell<FormState>,
) -> OpResult<Option<Vec<FormEntry>>> {
    let snapshot = state.borrow().clone();
    let invalid = {
        let session = realm.session.borrow();
        if core_forms::validation_bypassed(session.document(), form, submitter) {
            Vec::new()
        } else {
            let selectedness = snapshot
                .selectedness
                .iter()
                .map(|(node, selected)| (*node, *selected))
                .collect::<Vec<_>>();
            core_forms::form_controls(session.document(), form)
                .into_iter()
                .filter(|node| core_forms::will_validate(session.document(), *node))
                .filter(|node| {
                    !core_forms::validity_with_value_and_selectedness(
                        session.document(),
                        *node,
                        None,
                        snapshot
                            .custom_messages
                            .get(node)
                            .map_or("", String::as_str),
                        &selectedness,
                    )
                    .valid()
                })
                .collect::<Vec<_>>()
        }
    };
    for node in &invalid {
        realm.dispatch(ctx, *node, "invalid", false, true, &[])?;
    }
    if !invalid.is_empty() {
        return Ok(None);
    }
    let properties = submitter
        .map(|node| vec![("submitter", realm.wrap(ctx, node))])
        .unwrap_or_default();
    if !realm.dispatch(ctx, form, "submit", true, true, &properties)? {
        return Ok(None);
    }
    let session = realm.session.borrow();
    let selectedness = snapshot
        .selectedness
        .iter()
        .map(|(node, selected)| (*node, *selected))
        .collect::<Vec<_>>();
    let files = snapshot
        .files
        .iter()
        .map(|(node, files)| (*node, files.clone()))
        .collect::<Vec<_>>();
    Ok(Some(
        core_forms::form_entries_with_values_selectedness_and_files(
            session.document(),
            form,
            submitter,
            &[],
            &selectedness,
            &files,
        ),
    ))
}

/// Validate and dispatch submit, then return a complete immutable payload for
/// the browser-owned navigation service. Relative actions use the effective
/// document base URL through Lumen's shared URL parser.
pub fn request_submission(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    form: NodeId,
    submitter: Option<NodeId>,
    state: &RefCell<FormState>,
) -> OpResult<Option<FormSubmissionRequest>> {
    let Some(_entries) = request_submit(ctx, realm, form, submitter, state)? else {
        return Ok(None);
    };
    // Constructing the entry list dispatches a non-cancelable `formdata`
    // event. Snapshot the same object after listeners have run, preserving
    // listener additions/removals and actual File bytes for navigation.
    let form_data = form_data(ctx, realm, form, submitter)?;
    let entries = snapshot_form_data(ctx, &form_data)?;
    let session = realm.session.borrow();
    let Some(mut metadata) =
        core_forms::submission_metadata(session.document(), form, submitter, entries)
    else {
        return Ok(None);
    };
    drop(session);
    let has_document_url = realm.document_url().is_some();
    let base = realm.base_url();
    if metadata.method == "dialog" {
        metadata.action.clear();
    } else if metadata.action.is_empty() && !has_document_url {
        return Err(OpError::new(
            "InvalidStateError",
            "the browser has not supplied this document's URL",
        ));
    } else {
        let resolved = lumen_common::url::parse(&metadata.action, Some(&base))
            .map_err(|_| OpError::new("SyntaxError", "form action URL could not be resolved"))?;
        metadata.action = resolved.href();
    }
    Ok(Some(FormSubmissionRequest {
        metadata,
        form_data,
    }))
}

fn snapshot_form_data(ctx: &mut Ctx, data: &Value) -> OpResult<Vec<FormEntry>> {
    let global = ctx.global_object();
    let snapshot = ctx
        .get_member(&global, "__lumenSnapshotFormData")
        .map_err(|_| OpError::new("TypeError", "FormData snapshot bridge is unavailable"))?;
    let values = ctx
        .invoke(snapshot, global, &[data.clone()])
        .map_err(OpError::thrown)?;
    let Value::Num(length) = ctx
        .get_member(&values, "length")
        .map_err(|_| OpError::new("TypeError", "FormData snapshot length is unavailable"))?
    else {
        return Err(OpError::new(
            "TypeError",
            "FormData snapshot is not an array",
        ));
    };
    let mut entries = Vec::with_capacity((length.max(0.0) as usize).min(100_000));
    for index in 0..(length.max(0.0) as usize).min(100_000) {
        let entry = ctx
            .get_member(&values, &index.to_string())
            .map_err(|_| OpError::new("TypeError", "FormData entry is unavailable"))?;
        let member = |ctx: &mut Ctx, key: &str| -> OpResult<Value> {
            ctx.get_member(&entry, key)
                .map_err(|_| OpError::new("TypeError", "FormData entry field is unavailable"))
        };
        let name_value = member(ctx, "name")?;
        let name = ctx
            .coerce_string(&name_value)
            .map_err(OpError::thrown)?
            .to_string();
        let kind_value = member(ctx, "kind")?;
        let kind = ctx
            .coerce_string(&kind_value)
            .map_err(OpError::thrown)?
            .to_string();
        let value = if kind == "file" {
            let file_name_value = member(ctx, "fileName")?;
            let file_name = ctx
                .coerce_string(&file_name_value)
                .map_err(OpError::thrown)?
                .to_string();
            let media_type_value = member(ctx, "type")?;
            let media_type = ctx
                .coerce_string(&media_type_value)
                .map_err(OpError::thrown)?
                .to_string();
            let modified_value = member(ctx, "lastModified")?;
            let last_modified = ctx
                .coerce_number(&modified_value)
                .map_err(OpError::thrown)? as i64;
            let bytes = member(ctx, "bytes")?;
            let Value::Num(byte_length) = ctx
                .get_member(&bytes, "length")
                .map_err(|_| OpError::new("TypeError", "File byte length is unavailable"))?
            else {
                return Err(OpError::new("TypeError", "File bytes are not array-like"));
            };
            let mut data =
                Vec::with_capacity((byte_length.max(0.0) as usize).min(64 * 1024 * 1024));
            for byte_index in 0..(byte_length.max(0.0) as usize).min(64 * 1024 * 1024) {
                let byte = ctx
                    .get_member(&bytes, &byte_index.to_string())
                    .map_err(|_| OpError::new("TypeError", "File byte is unavailable"))?;
                data.push(ctx.coerce_number(&byte).map_err(OpError::thrown)? as u8);
            }
            FormEntryValue::File(FormFile {
                name: file_name,
                media_type,
                last_modified,
                bytes: data.into(),
            })
        } else {
            let text_value = member(ctx, "value")?;
            FormEntryValue::Text(
                ctx.coerce_string(&text_value)
                    .map_err(OpError::thrown)?
                    .to_string(),
            )
        };
        entries.push(FormEntry { name, value });
    }
    Ok(entries)
}

/// Dispatches the cancelable reset event, then restores captured IDL defaults.
/// The custom validity message is intentionally retained by reset.
pub fn reset_form(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    form: NodeId,
    state: &RefCell<FormState>,
) -> OpResult<bool> {
    if !realm.dispatch(ctx, form, "reset", true, true, &[])? {
        return Ok(false);
    }
    let controls = core_forms::form_reset_controls(realm.session.borrow().document(), form);
    let mut state = state.borrow_mut();
    let mut session = realm.session.borrow_mut();
    let document = session.document_mut();
    for node in controls {
        let Some(defaults) = state.defaults.get(&node).cloned() else {
            state.dirty.remove(&node);
            state.selectedness.remove(&node);
            continue;
        };
        let name = match document.kind(node).map_err(dom_error)? {
            NodeKind::Element { name, .. } => name.as_str().to_owned(),
            _ => continue,
        };
        if name == "input" {
            if defaults.value_attribute_present {
                let value = defaults.value.unwrap_or_default();
                document
                    .set_attribute(node, "value", &value)
                    .map_err(dom_error)?;
            } else {
                document
                    .remove_attribute(node, "value")
                    .map_err(dom_error)?;
            }
        } else if name == "textarea" {
            // Reset reveals the markup text again; current value is stored in
            // the synthetic shared-DOM attribute used by the value adapter.
            document
                .remove_attribute(node, "value")
                .map_err(dom_error)?;
        } else if name == "option" {
            if defaults.selected {
                document
                    .set_attribute(node, "selected", "")
                    .map_err(dom_error)?;
            } else {
                document
                    .remove_attribute(node, "selected")
                    .map_err(dom_error)?;
            }
        }
        if name == "input" {
            if defaults.checked {
                document
                    .set_attribute(node, "checked", "")
                    .map_err(dom_error)?;
            } else {
                document
                    .remove_attribute(node, "checked")
                    .map_err(dom_error)?;
            }
        }
        state.dirty.remove(&node);
        state.selectedness.remove(&node);
        if name == "input"
            && matches!(document.kind(node), Ok(NodeKind::Element { attributes, .. }) if attributes.iter().find(|(key, _)| key.as_str() == "type").is_some_and(|(_, value)| value.eq_ignore_ascii_case("file")))
        {
            state.files.remove(&node);
            if let Some(slot) = state.file_lists.get(&node) {
                slot.borrow_mut().values.clear();
            }
        }
        state.defaults.remove(&node);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    fn eval(engine: &mut Engine, source: &str) -> Value {
        match engine.eval_value(source).expect("script parses") {
            Ok(value) => value,
            Err(error) => {
                let message = engine
                    .ctx()
                    .get_member(&error, "message")
                    .ok()
                    .and_then(|value| {
                        if let Value::Str(message) = value {
                            Some(message.to_string())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| "script threw an unknown error".into());
                panic!("{message}");
            }
        }
    }

    #[test]
    fn public_constraint_validation_is_live_and_safe_under_reentrant_events() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<form><input required></form>", 64).unwrap();
        let result = eval(
            &mut engine,
            "(() => { const input = document.querySelector('input'); let invalid = 0; input.addEventListener('invalid', () => { invalid++; input.setCustomValidity('changed in listener'); }); const failed = !input.checkValidity() && invalid === 1 && input.validity.customError && !input.validity.valid && input.validationMessage === 'changed in listener'; input.setCustomValidity(''); input.value = 'ready'; return failed && input.checkValidity() && input.willValidate; })()",
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn public_report_focus_reset_and_canceled_submit_hooks_work() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<form><input required value='seed'><button type='submit'>Go</button></form>",
            96,
        )
        .unwrap();
        let result = eval(
            &mut engine,
            "(() => { const form = document.querySelector('form'); const input = document.querySelector('input'); input.value = ''; let invalid = 0; input.addEventListener('invalid', () => invalid++); const reported = !input.reportValidity() && document.activeElement === input && invalid === 1; if (!reported) throw new Error('reportValidity did not focus and dispatch invalid'); input.value = 'edited'; form.onreset = e => e.preventDefault(); form.reset(); const canceled = input.value === 'edited'; if (!canceled) throw new Error('canceled reset changed value'); form.onreset = null; form.reset(); const restored = input.value === 'seed'; if (!restored) throw new Error('reset did not restore default value: ' + input.value); input.value = ''; let submitted = 0; form.onsubmit = e => { submitted++; e.preventDefault(); }; form.requestSubmit(); const blocked = submitted === 0; if (!blocked) throw new Error('invalid form submitted'); input.value = 'ok'; form.requestSubmit(); if (submitted !== 1) throw new Error('submit cancellation handler did not run'); return true; })()",
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn select_dirty_selectedness_and_textarea_default_reset_are_live() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<form><select name='choice'><option value='a'>A</option><option value='b' selected>B</option></select><textarea name='note'>original</textarea></form>",
            96,
        ).unwrap();
        let result = eval(
            &mut engine,
            "(() => { const form = document.querySelector('form'); const select = document.querySelector('select'); const options = document.querySelectorAll('option'); const area = document.querySelector('textarea'); if (!(select instanceof HTMLSelectElement) || select.value !== 'b' || select.selectedIndex !== 1) throw new Error('select initial state'); select.value = 'missing'; if (select.value !== '' || select.selectedIndex !== -1 || options[1].selected) throw new Error('unmatched select value did not clear selection'); options[0].selected = true; if (select.value !== 'a' || select.selectedIndex !== 0 || options[1].selected) throw new Error('option selectedness did not update single select'); area.value = 'edited'; area.defaultValue = 'replacement'; if (area.value !== 'edited' || area.defaultValue !== 'replacement') throw new Error('textarea dirty default semantics'); form.reset(); return select.value === 'b' && select.selectedIndex === 1 && area.value === 'replacement' && area.defaultValue === 'replacement'; })()",
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn formdata_constructor_preserves_identity_and_dispatches_formdata() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<form><input name='a' value='one'><select name='s'><option value='x'>X</option><option value='y' selected>Y</option></select><textarea name='t'>seed</textarea></form>", 96).unwrap();
        eval(
            &mut engine,
            "globalThis.constructingFormData = null; globalThis.FormData = class FormData { constructor(form, submitter) { this.items = []; if (form !== undefined) { globalThis.constructingFormData = this; __lumenPopulateFormData(this, form, submitter); } } append(name, value) { this.items.push([String(name), String(value)]); } getAll(name) { return this.items.filter(x => x[0] === name).map(x => x[1]); } };",
        );
        let result = eval(
            &mut engine,
            "(() => { const form = document.querySelector('form'); const select = document.querySelector('select'); const area = document.querySelector('textarea'); select.selectedIndex = -1; area.value = 'live'; let same = false; form.addEventListener('formdata', event => { same = event.formData === globalThis.constructingFormData; event.formData.append('event', 'ran'); }); const data = new FormData(form); return same && data.getAll('a')[0] === 'one' && data.getAll('s').length === 0 && data.getAll('t')[0] === 'live' && data.getAll('event')[0] === 'ran'; })()",
        );
        assert!(matches!(result, Value::Bool(true)));
    }
}
