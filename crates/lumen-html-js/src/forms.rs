//! JavaScript form adapters over the host-testable shared HTML algorithms.
use super::*;
use lumen::embed::JsHost;
use lumen_bind::{FromArg, Host, Slot};
use lumen_html::forms as core_forms;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};

/// The Web IDL element union accepted by `HTMLSelectElement.add` and
/// `HTMLOptionsCollection.add`. Retain the actual wrapper until the shared DOM
/// insertion algorithm runs so cross-document adoption preserves identity.
pub(crate) struct SelectAddElement(Value);

impl<'a> FromArg<'a, JsHost> for SelectAddElement {
    fn from_arg(
        cx: &'a lumen::embed::ArgCx<'_>,
        value: &'a Value,
        _at: Slot,
    ) -> Result<Self, Value> {
        let allowed = <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
            // Keep the native projection scoped to this check. `class_ref`
            // retains argument projections in ArgCx, which would prevent the
            // later DOM insertion/adoption from mutating the wrapper.
            let (realm, id) = ctx
                .with_instance::<super::DomHtmlElement, _>(value, |element| {
                    let node = &element.base.base;
                    (node.realm.clone(), node.id)
                })
                .map_err(|error| error.to_value(ctx))?;
            let (realm, id) = realm.resolve_adopted_node(id);
            let session = realm.session.borrow();
            Ok(matches!(
                html_local_name(session.document(), id),
                Some("option" | "optgroup")
            ))
        })?;
        if !allowed {
            return Err(<JsHost as Host>::with_ctx(cx, |ctx| {
                ctx.make_error(
                    "TypeError",
                    "select.add requires an HTMLOptionElement or HTMLOptGroupElement",
                )
            }));
        }
        Ok(Self(value.clone()))
    }
}

/// The nullable `(HTMLElement or long)` Web IDL union used for the optional
/// `before` argument. HTMLElement brand matching precedes numeric conversion;
/// other values use the shared Web IDL `long` conversion exactly once.
pub(crate) enum SelectAddBefore {
    Append,
    Element(Value),
    Index(i32),
}

impl<'a> FromArg<'a, JsHost> for SelectAddBefore {
    fn from_arg(
        cx: &'a lumen::embed::ArgCx<'_>,
        value: &'a Value,
        at: Slot,
    ) -> Result<Self, Value> {
        if <JsHost as Host>::is_none(value) {
            return Ok(Self::Append);
        }
        let is_element = if matches!(value, Value::Obj(_)) {
            <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
                // Brand-check without retaining a projected reference while
                // the rest of the union conversion runs.
                ctx.with_instance::<super::DomHtmlElement, _>(value, |_| ())
                    .is_ok()
            })
        } else {
            false
        };
        if is_element {
            return Ok(Self::Element(value.clone()));
        }
        <i32 as FromArg<'a, JsHost>>::from_arg(cx, value, at).map(Self::Index)
    }
}

pub use lumen_html::forms::{
    default_value as default_control_value, form_controls, form_entries, form_entries_with_values,
    validity, validity_with_value, FormEntry, FormEntryValue, FormFile, ValidityState,
};

pub struct FormSubmissionRequest {
    /// The form whose successful controls produced this submission snapshot.
    /// Hosts use the realm plus this id to replace only that form's pending
    /// navigation when multiple submit actions occur in one task turn.
    pub form: NodeId,
    pub metadata: core_forms::FormSubmission,
    pub form_data: Value,
    pub navigation_metadata: super::browsing_context::NavigationMetadata,
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
    selected: bool,
}

#[derive(Clone, Copy)]
struct Checkedness {
    value: bool,
    dirty: bool,
}

#[derive(Clone)]
struct PendingInputTypeChange {
    previous_type: &'static str,
    previous_mode: core_forms::InputValueMode,
    previous_value: Option<String>,
    previous_value_dirty: bool,
    previous_selection_supported: bool,
}

#[derive(Clone, Copy)]
struct SingleSelectOptionCache {
    select: NodeId,
    document_version: u64,
    selectedness_generation: u64,
    selected: Option<NodeId>,
}

const USER_EDITED: u8 = 1 << 0;
const USER_VALIDITY_INTERACTED: u8 = 1 << 1;

/// Rare FACE state; actual JavaScript values are traced by its one attached
/// ElementInternals wrapper, while default native controls keep no entry.
#[derive(Default)]
pub(crate) struct CustomFormState {
    pub completed:bool,
    pub states:Option<Rc<RefCell<lumen::embed::DomStringSet>>>,
    pub submission:Option<Value>,
    pub restore:Option<Value>,
    pub validity:ValidityState,
    pub message:String,
    pub anchor:Option<Value>,
}
impl CustomFormState {
    pub fn trace_values(&self,visit:&mut dyn FnMut(&Value)) {
        for value in [&self.submission,&self.restore,&self.anchor].into_iter().flatten(){visit(value);}
    }
}

#[derive(Clone)]
struct NumberEdit { text: String, bad_input: bool }

/// State held by each `DomRealm`. Only dirty live values are stored here;
/// defaults remain in the shared DOM attributes or textarea text children.
#[derive(Clone, Default)]
pub struct FormState {
    pub(crate) custom_elements:HashMap<NodeId,Rc<RefCell<CustomFormState>>>,
    defaults: HashMap<NodeId, Defaults>,
    dirty: HashSet<NodeId>,
    checkedness: HashMap<NodeId, Checkedness>,
    indeterminate: HashSet<NodeId>,
    live_dirty: HashSet<NodeId>,
    /// Sparse per-control user provenance. The two bits share one map entry:
    /// user edits affect tooShort/tooLong, while significant interaction also
    /// drives :user-valid/:user-invalid and survives script value assignment.
    user_state: HashMap<NodeId, u8>,
    live_values: HashMap<NodeId, String>,
    /// Actual user-visible number text, including partial exponents. API and
    /// submission values stay sanitized in live_values. Entries are user-edit only.
    number_edits: HashMap<NodeId, NumberEdit>,
    pending_input_type_change: Option<(NodeId, PendingInputTypeChange)>,
    custom_messages: HashMap<NodeId, String>,
    selectedness: HashMap<NodeId, bool>,
    output_default_overrides: HashMap<NodeId, String>,
    files: HashMap<NodeId, Vec<FormFile>>,
    file_lists: HashMap<NodeId, Rc<RefCell<FileListData>>>,
    /// Sparse HTMLFormElement past-names maps. Entries are created only when
    /// a single named getter result is observed, as required by the legacy
    /// named-property algorithm.
    past_form_names: HashMap<NodeId, Vec<(String, NodeId)>>,
    validity_generation: u64,
    /// One constant-size cache for the last single-select queried by option
    /// getters and CSS matching. Document and selectedness revisions make
    /// tree/attribute and IDL changes invalidate it without a per-option map.
    single_select_option_cache: Cell<Option<SingleSelectOptionCache>>,
}

impl FormState {
pub(crate) fn custom_form_state(&mut self,node:NodeId)->OpResult<Rc<RefCell<CustomFormState>>> {
    if let Some(state)=self.custom_elements.get(&node){return Ok(state.clone());}
    if self.custom_elements.len()>=65_536{return Err(OpError::new("QuotaExceededError","custom form state limit"));}
    self.custom_elements.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","custom form state allocation"))?;
    let state=Rc::new(RefCell::new(CustomFormState::default()));
    self.custom_elements.insert(node,state.clone());Ok(state)
}
pub(crate) fn is_custom_form_control(&self,node:NodeId)->bool {
    self.custom_elements.get(&node).is_some_and(|state|state.borrow().completed)
}
pub(crate) fn complete_custom_form_control(&mut self,node:NodeId)->OpResult<()> {
    self.custom_form_state(node)?.borrow_mut().completed=true;
    self.bump_validity_generation();Ok(())
}
pub(crate) fn changed_custom_form_state(&mut self){self.bump_validity_generation();}

    fn was_user_edited(&self, node: NodeId) -> bool {
        self.user_state
            .get(&node)
            .is_some_and(|flags| flags & USER_EDITED != 0)
    }

    pub(crate) fn has_user_validity_interaction(&self, node: NodeId) -> bool {
        self.user_state
            .get(&node)
            .is_some_and(|flags| flags & USER_VALIDITY_INTERACTED != 0)
    }

    fn mark_user_edited_and_interacted(&mut self, node: NodeId) -> bool {
        let flags = self.user_state.entry(node).or_default();
        let previous = *flags;
        *flags |= USER_EDITED | USER_VALIDITY_INTERACTED;
        *flags != previous
    }

    fn clear_user_edited(&mut self, node: NodeId) {
        let empty = if let Some(flags) = self.user_state.get_mut(&node) {
            *flags &= !USER_EDITED;
            *flags == 0
        } else {
            false
        };
        if empty {
            self.user_state.remove(&node);
        }
    }

    fn mark_user_validity_interacted(&mut self, node: NodeId) -> bool {
        let flags = self.user_state.entry(node).or_default();
        let changed = *flags & USER_VALIDITY_INTERACTED == 0;
        *flags |= USER_VALIDITY_INTERACTED;
        changed
    }

    fn clear_user_state(&mut self, node: NodeId) {
        self.user_state.remove(&node);
    }

    pub(crate) fn single_select_option(
        &self,
        document: &lumen_html::Document,
        select: NodeId,
    ) -> Option<Option<NodeId>> {
        cached_single_select_option_by(
            document,
            select,
            &self.single_select_option_cache,
            self.validity_generation,
            |option| self.selectedness.get(&option).copied(),
        )
    }

    pub(crate) fn past_form_names(&self, form: NodeId) -> &[(String, NodeId)] {
        self.past_form_names.get(&form).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn past_form_name(&self, form: NodeId, name: &str) -> Option<NodeId> {
        self.past_form_names
            .get(&form)?
            .iter()
            .find_map(|(past_name, node)| (past_name == name).then_some(*node))
    }

    pub(crate) fn remember_form_name(&mut self, form: NodeId, name: &str, node: NodeId) {
        let names = self.past_form_names.entry(form).or_default();
        if let Some((_, remembered_node)) =
            names.iter_mut().find(|(past_name, _)| past_name == name)
        {
            *remembered_node = node;
            return;
        }
        names.push((name.to_owned(), node));
    }

    pub(crate) fn forget_form_names_not_owned_by(
        &mut self,
        form: NodeId,
        mut is_still_owned: impl FnMut(NodeId) -> bool,
    ) {
        let Some(names) = self.past_form_names.get_mut(&form) else {
            return;
        };
        names.retain(|(_, node)| is_still_owned(*node));
        if names.is_empty() {
            self.past_form_names.remove(&form);
        }
    }
}

fn cached_single_select_option_by(
    document: &lumen_html::Document,
    select: NodeId,
    cache: &Cell<Option<SingleSelectOptionCache>>,
    selectedness_generation: u64,
    selectedness: impl FnMut(NodeId) -> Option<bool>,
) -> Option<Option<NodeId>> {
    if html_local_name(document, select) != Some("select")
        || has_null_attribute(document, select, "multiple")
    {
        return None;
    }
    let document_version = document.version();
    if let Some(entry) = cache.get().filter(|entry| {
        entry.select == select
            && entry.document_version == document_version
            && entry.selectedness_generation == selectedness_generation
    }) {
        return Some(entry.selected);
    }

    let mut selected = None;
    core_forms::for_each_selected_option_by(document, select, selectedness, |option, _, _| {
        selected = Some(option);
        false
    })
    .ok()?;
    cache.set(Some(SingleSelectOptionCache {
        select,
        document_version,
        selectedness_generation,
        selected,
    }));
    Some(selected)
}

impl FormState {
    fn bump_validity_generation(&mut self) {
        self.validity_generation = self.validity_generation.wrapping_add(1).max(1);
    }

    pub fn validity_generation(&self) -> u64 {
        self.validity_generation
    }
}

impl core_forms::ValidityStateView for FormState {
    fn user_bad_input(&self, node: NodeId) -> bool { self.number_edits.get(&node).is_some_and(|edit| edit.bad_input) }
    fn custom_element_validity(&self,node:NodeId)->Option<ValidityState>{self.custom_elements.get(&node).map(|state|state.borrow().validity)}
    fn value_override(&self, node: NodeId) -> Option<&str> {
        self.live_values.get(&node).map(String::as_str)
    }

    fn custom_message(&self, node: NodeId) -> Option<&str> {
        self.custom_messages.get(&node).map(String::as_str)
    }

    fn user_edited(&self, node: NodeId) -> bool {
        self.was_user_edited(node)
    }

    fn user_validity_interacted(&self, node: NodeId) -> bool {
        self.has_user_validity_interaction(node)
    }

    fn selectedness(&self, option: NodeId) -> Option<bool> {
        self.selectedness.get(&option).copied()
    }

    fn checkedness(&self, control: NodeId) -> Option<bool> {
        self.checkedness.get(&control).map(|checked| checked.value)
    }

    fn has_selected_files(&self, input: NodeId) -> bool {
        self.files
            .get(&input)
            .is_some_and(|files| !files.is_empty())
    }
}

impl core_forms::FormEntryStateView for FormState {
    fn value_for_control(&self, node: NodeId) -> Option<&str> {
        self.live_values.get(&node).map(String::as_str)
    }

    fn selected_for_option(&self, option: NodeId) -> Option<bool> {
        self.selectedness.get(&option).copied()
    }

    fn checked_for_control(&self, control: NodeId) -> Option<bool> {
        self.checkedness.get(&control).map(|checked| checked.value)
    }

    fn files_for_control(&self, control: NodeId) -> Option<&[FormFile]> {
        self.files.get(&control).map(Vec::as_slice)
    }
}

pub fn presentation_value(state: &FormState, node: NodeId) -> Option<&str> {
    state
        .number_edits
        .get(&node)
        .map(|edit| edit.text.as_str())
        .or_else(|| live_value(state, node))
}

pub fn live_value(state: &FormState, node: NodeId) -> Option<&str> {
    state.live_values.get(&node).map(String::as_str)
}

pub fn selectedness_override(state: &FormState, option: NodeId) -> Option<bool> {
    state.selectedness.get(&option).copied()
}

struct FileListData {
    values: Vec<Value>,
    wrapper: Option<WeakValue>,
}

/// Drop adapter state after the shared DOM has reclaimed detached nodes.
/// Call from `DomRealm::reap_detached` after `destroy_subtree` completes.
pub fn reap(state: &mut FormState, document: &lumen_html::Document) {
    state.custom_elements.retain(|node,_|document.kind(*node).is_ok());
    state
        .defaults
        .retain(|node, _| document.kind(*node).is_ok());
    state.dirty.retain(|node| document.kind(*node).is_ok());
    state
        .checkedness
        .retain(|node, _| document.kind(*node).is_ok());
    state
        .indeterminate
        .retain(|node| document.kind(*node).is_ok());
    state.live_dirty.retain(|node| document.kind(*node).is_ok());
    state.user_state.retain(|node, _| document.kind(*node).is_ok());
    state.number_edits.retain(|node, _| document.kind(*node).is_ok());
    state
        .live_values
        .retain(|node, _| document.kind(*node).is_ok());
    if state
        .pending_input_type_change
        .as_ref()
        .is_some_and(|(node, _)| document.kind(*node).is_err())
    {
        state.pending_input_type_change = None;
    }
    state
        .custom_messages
        .retain(|node, _| document.kind(*node).is_ok());
    state
        .selectedness
        .retain(|node, _| document.kind(*node).is_ok());
    state
        .output_default_overrides
        .retain(|node, _| document.kind(*node).is_ok());
    state.files.retain(|node, _| document.kind(*node).is_ok());
    state
        .file_lists
        .retain(|node, _| document.kind(*node).is_ok());
    state.past_form_names.retain(|form, entries| {
        document.kind(*form).is_ok() && {
            entries.retain(|(_, node)| document.kind(*node).is_ok());
            !entries.is_empty()
        }
    });
}

/// Move per-control IDL state along with nodes transferred to another document.
pub fn adopt_nodes_into(
    source: &mut FormState,
    target: &mut FormState,
    mapping: &[(NodeId, NodeId)],
) {
    for &(old, new) in mapping {
        if let Some(value)=source.custom_elements.remove(&old){target.custom_elements.insert(new,value);}
        if let Some(value) = source.defaults.remove(&old) {
            target.defaults.insert(new, value);
        }
        if source.dirty.remove(&old) {
            target.dirty.insert(new);
        }
        if let Some(value) = source.checkedness.remove(&old) {
            target.checkedness.insert(new, value);
        }
        if source.indeterminate.remove(&old) {
            target.indeterminate.insert(new);
        }
        if source.live_dirty.remove(&old) {
            target.live_dirty.insert(new);
        }
        if let Some(value) = source.user_state.remove(&old) {
            target.user_state.insert(new, value);
        }
        if let Some(value) = source.live_values.remove(&old) {
            target.live_values.insert(new, value);
        }
        if let Some(edit) = source.number_edits.remove(&old) { target.number_edits.insert(new, edit); }
        if let Some(value) = source.custom_messages.remove(&old) {
            target.custom_messages.insert(new, value);
        }
        if let Some(value) = source.selectedness.remove(&old) {
            target.selectedness.insert(new, value);
        }
        if let Some(value) = source.output_default_overrides.remove(&old) {
            target.output_default_overrides.insert(new, value);
        }
        if let Some(value) = source.files.remove(&old) {
            target.files.insert(new, value);
        }
        if let Some(value) = source.file_lists.remove(&old) {
            target.file_lists.insert(new, value);
        }
        if let Some(mut entries) = source.past_form_names.remove(&old) {
            for (_, node) in &mut entries {
                *node = mapping
                    .iter()
                    .find_map(|(candidate, replacement)| {
                        (*candidate == *node).then_some(*replacement)
                    })
                    .unwrap_or(*node);
            }
            target.past_form_names.insert(new, entries);
        }
    }
    if !mapping.is_empty() {
        source.bump_validity_generation();
        target.bump_validity_generation();
    }
}

/// Copy live values and their dirty state when DOM cloning creates new
/// controls. The clone's reset default comes from its copied attributes or
/// textarea text children.
pub fn clone_live_values_into(
    source: &FormState,
    target: &mut FormState,
    document: &lumen_html::Document,
    mapping: &[(NodeId, NodeId)],
) {
    for &(old, new) in mapping {
        let mut copies_reset_default = false;
        if let Some(value) = source.live_values.get(&old) {
            target.live_values.insert(new, value.clone());
            if source.dirty.contains(&old) {
                target.dirty.insert(new);
            }
            if source.live_dirty.contains(&old) {
                target.live_dirty.insert(new);
            }
            copies_reset_default = true;
        }
        if let Some(checked) = source.checkedness.get(&old) {
            target.checkedness.insert(new, *checked);
            if source.dirty.contains(&old) {
                target.dirty.insert(new);
            }
            copies_reset_default = true;
        }
        if source.indeterminate.contains(&old) {
            target.indeterminate.insert(new);
        }
        if copies_reset_default {
            capture_defaults(target, document, new);
        }
    }
}

/// Snapshot only the states named by HTML's input/textarea cloning steps.
/// The source and destination can share a realm, so release the read borrow
/// before applying this small snapshot. User validity, validity messages,
/// selection and picker state belong to the new control's initial state.
pub(crate) fn clone_state_snapshot(source: &FormState, mapping: &[(NodeId, NodeId)]) -> FormState {
    let mut snapshot = FormState::default();
    for &(old, _) in mapping {
        if let Some(value) = source.live_values.get(&old) { snapshot.live_values.insert(old, value.clone()); }
        if source.live_dirty.contains(&old) { snapshot.live_dirty.insert(old); }
        if source.dirty.contains(&old) { snapshot.dirty.insert(old); }
        if let Some(value) = source.checkedness.get(&old) { snapshot.checkedness.insert(old, *value); }
        if source.indeterminate.contains(&old) { snapshot.indeterminate.insert(old); }
    }
    snapshot
}

pub fn install(ctx: &mut Ctx) -> OpResult<()> {
    ctx.class_constructor::<DomValidityState>();
    ctx.class_constructor::<DomFileList>();
    super::option_factory::install(ctx)
}

fn null_attribute<'a>(
    document: &'a lumen_html::Document,
    node: NodeId,
    name: &str,
) -> Option<&'a str> {
    document
        .get_attribute_ns_ref(node, None, name)
        .ok()
        .flatten()
}

fn has_null_attribute(document: &lumen_html::Document, node: NodeId, name: &str) -> bool {
    null_attribute(document, node, name).is_some()
}

fn html_local_name<'a>(document: &'a lumen_html::Document, node: NodeId) -> Option<&'a str> {
    core_forms::html_element_local_name(document, node)
}

fn is_input_state_attribute(name: &str) -> bool {
    ["type", "min", "max", "step", "multiple", "value", "checked"]
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

/// Capture markup defaults before a DOM-backed value or checked state is
/// mutated. Call this from the corresponding IDL setter.
pub fn capture_defaults(state: &mut FormState, document: &lumen_html::Document, node: NodeId) {
    if state.defaults.contains_key(&node) {
        return;
    }
    if !matches!(document.kind(node), Ok(NodeKind::Element { .. })) {
        return;
    }
    let selected = has_null_attribute(document, node, "selected");
    state.defaults.insert(node, Defaults { selected });
}

/// Apply an IDL `value` write using the active control's value mode.
pub fn set_control_value(
    realm: &DomRealm,
    state: &mut FormState,
    node: NodeId,
    value: &str,
) -> OpResult<()> {
    set_control_value_inner(realm, state, node, value, true)
}

/// Commit a host text edit without invalidating the edit transaction that is
/// currently producing it. Script-driven IDL writes use `set_control_value`.
pub fn set_control_value_from_user(
    realm: &DomRealm,
    state: &mut FormState,
    node: NodeId,
    value: &str,
) -> OpResult<()> {
    let number = core_forms::html_element_local_name(realm.session.borrow().document(), node)
        == Some("input")
        && core_forms::input_type_state(realm.session.borrow().document(), node) == "number";
    if number {
        if value.len() > super::editing_history::MAX_CONTROL_BYTES {
            return Err(OpError::new(
                "RangeError",
                "number editing text exceeds the editing limit",
            ));
        }
        let api = core_forms::number_user_value(value);
        let bad_input = api.is_none();
        let edit = if value == api.as_deref().unwrap_or("") {
            None
        } else {
            let mut text = String::new();
            text.try_reserve_exact(value.len())
                .map_err(|_| OpError::new("RangeError", "number editing allocation failed"))?;
            text.push_str(value);
            state.number_edits.try_reserve(1).map_err(|_| {
                OpError::new("RangeError", "number editing state allocation failed")
            })?;
            Some(NumberEdit { text, bad_input })
        };
        set_control_value_inner(realm, state, node, api.as_deref().unwrap_or(""), false)?;
        if let Some(edit) = edit {
            state.number_edits.insert(node, edit);
        } else {
            state.number_edits.remove(&node);
        }
        return Ok(());
    }
    set_control_value_inner(realm, state, node, value, false)
}


fn set_control_value_inner(
    realm: &DomRealm,
    state: &mut FormState,
    node: NodeId,
    value: &str,
    invalidate_editing: bool,
) -> OpResult<()> {
    capture_defaults(state, realm.session.borrow().document(), node);
    let (is_input, is_select, is_textarea, input_mode) = {
        let session = realm.session.borrow();
        let document = session.document();
        let Some(name) = html_local_name(document, node) else {
            return Err(OpError::new("TypeError", "value requires a form control"));
        };
        let is_input = name == "input";
        let is_select = name == "select";
        let is_textarea = name == "textarea";
        if !(is_input || is_select || is_textarea) {
            return Err(OpError::new("TypeError", "value requires a form control"));
        }
        (
            is_input,
            is_select,
            is_textarea,
            core_forms::input_value_mode(document, node),
        )
    };
    let file_input = input_mode == Some(core_forms::InputValueMode::Filename);
    if file_input {
        if !value.is_empty() {
            return Err(OpError::new(
                "InvalidStateError",
                "a file input value can only be cleared",
            ));
        }
        state.clear_user_edited(node);
        state.files.remove(&node);
        if let Some(slot) = state.file_lists.get(&node) {
            slot.borrow_mut().values.clear();
        }
        state.dirty.insert(node);
        state.bump_validity_generation();
        if invalidate_editing {
            realm.invalidate_editing_for_value_change(node);
        }
        return Ok(());
    }
    // Any successful IDL assignment replaces the user-edit provenance,
    // including an assignment equal to the currently displayed value.
    state.clear_user_edited(node);
    if invalidate_editing { state.number_edits.remove(&node); }
    if is_select {
        let session = realm.session.borrow();
        let document = session.document();
        let multiple = has_null_attribute(document, node, "multiple");
        let mut matched = false;
        core_forms::for_each_select_option(document, node, |option, _| {
            capture_defaults(state, document, option);
            let selected = core_forms::option_value(document, option).as_deref() == Some(value)
                && (multiple || !matched);
            state.selectedness.insert(option, selected);
            matched |= selected;
            true
        })
        .map_err(dom_error)?;
    } else if is_textarea {
        state.live_values.insert(node, value.to_owned());
        state.live_dirty.insert(node);
        state.dirty.insert(node);
        state.bump_validity_generation();
        if invalidate_editing {
            realm.invalidate_editing_for_value_change(node);
            // The value setter moves the text entry cursor to the end.
            let length = lumen_common::smuggle::utf16_unit_len(value);
            realm
                .selections
                .borrow_mut()
                .insert(node, (length, length, "none".into()));
        }
        return Ok(());
    } else {
        let value = if let Some(mode) = input_mode {
            let session = realm.session.borrow();
            let document = session.document();
            match mode {
                core_forms::InputValueMode::Value => {
                    let kind = null_attribute(document, node, "type")
                        .unwrap_or("text")
                        .to_ascii_lowercase();
                    let sanitized = core_forms::sanitize_input_value_with_attributes(
                        &kind,
                        value,
                        null_attribute(document, node, "min"),
                        null_attribute(document, node, "max"),
                        null_attribute(document, node, "step"),
                        null_attribute(document, node, "value"),
                        has_null_attribute(document, node, "multiple"),
                    );
                    state.live_values.insert(node, sanitized);
                    state.live_dirty.insert(node);
                    if invalidate_editing {
                        realm.invalidate_editing_for_value_change(node);
                    }
                    state.dirty.insert(node);
                    state.bump_validity_generation();
                    return Ok(());
                }
                core_forms::InputValueMode::Filename => unreachable!("handled above"),
                core_forms::InputValueMode::Default | core_forms::InputValueMode::DefaultOn => {
                    state.live_values.remove(&node);
                    state.live_dirty.remove(&node);
                    value
                }
            }
        } else {
            value
        };
        realm
            .session
            .borrow_mut()
            .document_mut()
            .set_attribute_ns(node, None, "value", value)
            .map_err(dom_error)?;
        if invalidate_editing && (is_input || is_textarea) {
            // Reentrant IDL assignments abort pending host edits even if the
            // assigned string equals the current value.
            realm.invalidate_editing_for_value_change(node);
        }
    }
    state.dirty.insert(node);
    state.bump_validity_generation();
    Ok(())
}

/// Mark a successful host-originated edit before its `input` event runs.
pub fn mark_user_edited(state: &mut FormState, node: NodeId) {
    state.mark_user_edited_and_interacted(node);
    state.live_dirty.insert(node);
    state.dirty.insert(node);
    state.bump_validity_generation();
}

fn mark_form_user_validity_interacted(
    realm: &DomRealm,
    form: NodeId,
    state: &RefCell<FormState>,
) -> OpResult<()> {
    // Eligibility calls the shared document resolver, which borrows the same
    // form state. Finish that read phase before mutating interaction state.
    let eligible = {
        let session = realm.session.borrow();
        let document = session.document();
        let mut eligible = Vec::new();
        let mut allocation_failed = false;
        core_forms::for_each_form_control(document, form, |node| {
            if core_forms::will_validate(document, node) {
                if eligible.try_reserve(1).is_err() { allocation_failed = true; return false; }
                eligible.push(node);
            }
            true
        }).map_err(dom_error)?;
        if allocation_failed { return Err(OpError::new("QuotaExceededError", "validation control snapshot allocation failed")); }
        eligible
    };
    let mut state = state.borrow_mut();
    let mut changed = false;
    for node in eligible { changed |= state.mark_user_validity_interacted(node); }
    if changed { state.bump_validity_generation(); }
    Ok(())
}

/// Preserve reset bookkeeping before a relevant input content attribute is
/// changed. Dirty live values already live in `live_values`, so changing a
/// default or constraint never has to copy the current string.
pub fn prepare_input_attribute_change(
    realm: &DomRealm,
    node: NodeId,
    changed_attribute: &str,
) -> OpResult<()> {
    if !is_input_state_attribute(changed_attribute) {
        return Ok(());
    }
    let mut state = realm.forms.borrow_mut();
    let session = realm.session.borrow();
    let document = session.document();
    let Some(previous_mode) = core_forms::input_value_mode(document, node) else {
        return Ok(());
    };
    capture_defaults(&mut state, document, node);
    let is_range = null_attribute(document, node, "type")
        .is_some_and(|value| value.eq_ignore_ascii_case("range"));
    if is_range && !state.live_values.contains_key(&node) {
        let current = core_forms::default_value(document, node).unwrap_or_default();
        state.live_values.insert(node, current);
    }
    if changed_attribute.eq_ignore_ascii_case("type") {
        let previous_selection_supported = core_forms::supports_text_selection(document, node);
        let previous_value = if previous_mode == core_forms::InputValueMode::Value
            && !state.live_values.contains_key(&node)
        {
            Some(core_forms::default_value(document, node).unwrap_or_default())
        } else {
            None
        };
        let previous_value_dirty = state.live_dirty.contains(&node);
        state.pending_input_type_change = Some((
            node,
            PendingInputTypeChange {
                previous_type: core_forms::input_type_state(document, node),
                previous_mode,
                previous_value,
                previous_value_dirty,
                previous_selection_supported,
            },
        ));
    }
    Ok(())
}

/// Re-sanitize a cached input value after its type or constraint changes.
/// Clean non-range values are derived from the current content attribute.
pub fn resanitize_input_after_attribute_change(
    realm: &DomRealm,
    node: NodeId,
    changed_attribute: &str,
) -> OpResult<()> {
    if changed_attribute.eq_ignore_ascii_case("selected") {
        option_selected_attribute_changed(realm, node)?;
    }
    let select_to_normalize = {
        let session = realm.session.borrow();
        let document = session.document();
        if (changed_attribute.eq_ignore_ascii_case("multiple")
            || changed_attribute.eq_ignore_ascii_case("size"))
            && html_local_name(document, node) == Some("select")
        {
            Some(node)
        } else if changed_attribute.eq_ignore_ascii_case("disabled")
            && matches!(html_local_name(document, node), Some("option" | "optgroup"))
        {
            core_forms::select_ancestor(document, node).map_err(dom_error)?
        } else {
            None
        }
    };
    if let Some(select) = select_to_normalize {
        let mut state = realm.forms.borrow_mut();
        let session = realm.session.borrow();
        let document = session.document();
        // Removing `multiple` preserves the first selected option before
        // applying the ordinary single-select reset algorithm. Other reset
        // triggers retain the last selected option.
        let preferred = if changed_attribute.eq_ignore_ascii_case("multiple")
            && !has_null_attribute(document, node, "multiple")
        {
            let mut first_selected = None;
            core_forms::for_each_select_option(document, select, |option, _| {
                let selected = state
                    .selectedness
                    .get(&option)
                    .copied()
                    .unwrap_or_else(|| has_null_attribute(document, option, "selected"));
                if selected && first_selected.is_none() {
                    first_selected = Some(option);
                }
                true
            })
            .map_err(dom_error)?;
            first_selected
        } else {
            None
        };
        if normalize_select_selection_with_state(document, &mut state, select, preferred)? {
            state.bump_validity_generation();
        }
    }
    if !is_input_state_attribute(changed_attribute) {
        return Ok(());
    }
    if changed_attribute.eq_ignore_ascii_case("checked") {
        let mut state = realm.forms.borrow_mut();
        if state
            .checkedness
            .get(&node)
            .is_some_and(|checkedness| checkedness.dirty)
        {
            return Ok(());
        }
        state.checkedness.remove(&node);
        let peers = {
            let session = realm.session.borrow();
            if has_null_attribute(session.document(), node, "checked") {
                core_forms::radio_group_members(session.document(), node)
            } else {
                Vec::new()
            }
        };
        for peer in peers {
            let dirty = state
                .checkedness
                .get(&peer)
                .is_some_and(|checkedness| checkedness.dirty);
            state.checkedness.insert(
                peer,
                Checkedness {
                    value: false,
                    dirty,
                },
            );
        }
        return Ok(());
    }
    let mut state = realm.forms.borrow_mut();
    let is_type_change = changed_attribute.eq_ignore_ascii_case("type");
    let pending_type_change = if is_type_change {
        match state.pending_input_type_change.take() {
            Some((pending_node, pending)) if pending_node == node => Some(pending),
            other => {
                state.pending_input_type_change = other;
                None
            }
        }
    } else {
        None
    };
    let mode = {
        let session = realm.session.borrow();
        core_forms::input_value_mode(session.document(), node)
    };

    if pending_type_change.as_ref().is_some_and(|pending| {
        pending.previous_type != core_forms::input_type_state(realm.session.borrow().document(), node)
    }) { state.number_edits.remove(&node); }

    let became_selection_supported = pending_type_change.as_ref().is_some_and(|pending| {
        !pending.previous_selection_supported
            && core_forms::supports_text_selection(&realm.session.borrow().document(), node)
    });
    if became_selection_supported {
        realm
            .selections
            .borrow_mut()
            .insert(node, (0, 0, "none".into()));
    }

    if let Some(pending) = pending_type_change {
        match (pending.previous_mode, mode) {
            (
                core_forms::InputValueMode::Value,
                Some(core_forms::InputValueMode::Default | core_forms::InputValueMode::DefaultOn),
            ) => {
                let current = state
                    .live_values
                    .get(&node)
                    .map(String::as_str)
                    .or(pending.previous_value.as_deref());
                if let Some(current) = current.filter(|value| !value.is_empty()) {
                    realm
                        .session
                        .borrow_mut()
                        .document_mut()
                        .set_attribute_ns(node, None, "value", current)
                        .map_err(dom_error)?;
                }
                state.live_values.remove(&node);
                state.live_dirty.remove(&node);
                return Ok(());
            }
            (previous, Some(core_forms::InputValueMode::Value))
                if previous == core_forms::InputValueMode::Value =>
            {
                if !state.live_values.contains_key(&node) {
                    if let Some(value) = pending.previous_value {
                        state.live_values.insert(node, value);
                        if pending.previous_value_dirty {
                            state.live_dirty.insert(node);
                        } else {
                            state.live_dirty.remove(&node);
                        }
                    }
                }
            }
            (previous, Some(core_forms::InputValueMode::Value))
                if previous != core_forms::InputValueMode::Value =>
            {
                // The default content attribute initializes a newly entered
                // value mode, and the dirty value flag starts false.
                state.live_values.remove(&node);
                state.live_dirty.remove(&node);
                return Ok(());
            }
            (previous, Some(core_forms::InputValueMode::Filename))
                if previous != core_forms::InputValueMode::Filename =>
            {
                state.live_values.remove(&node);
                state.live_dirty.remove(&node);
                state.files.remove(&node);
                if let Some(slot) = state.file_lists.get(&node) {
                    slot.borrow_mut().values.clear();
                }
                return Ok(());
            }
            _ => {}
        }
    }

    if !matches!(mode, Some(core_forms::InputValueMode::Value)) {
        if is_type_change {
            state.live_values.remove(&node);
            state.live_dirty.remove(&node);
        }
        return Ok(());
    }
    if !state.live_values.contains_key(&node) {
        return Ok(());
    }
    let (kind, min, max, step, value_attribute, multiple) = {
        let session = realm.session.borrow();
        let document = session.document();
        let get = |key: &str| null_attribute(document, node, key);
        (
            get("type").unwrap_or("text").to_ascii_lowercase(),
            get("min").map(str::to_owned),
            get("max").map(str::to_owned),
            get("step").map(str::to_owned),
            get("value").map(str::to_owned),
            get("multiple").is_some(),
        )
    };
    let use_attribute_value =
        changed_attribute.eq_ignore_ascii_case("value") && !state.live_dirty.contains(&node);
    let current = state
        .live_values
        .get(&node)
        .map(String::as_str)
        .unwrap_or("");
    let sanitized = core_forms::sanitize_input_value_with_attributes(
        &kind,
        if use_attribute_value {
            value_attribute.as_deref().unwrap_or("")
        } else {
            current
        },
        min.as_deref(),
        max.as_deref(),
        step.as_deref(),
        value_attribute.as_deref(),
        multiple,
    );
    state.live_values.insert(node, sanitized);
    Ok(())
}

/// Set the dirty value flag without changing the current value. `setRangeText`
/// performs this step before it validates the requested range, so even an
/// IndexSizeError leaves the control's default/live relationship updated.
pub fn mark_value_dirty(realm: &DomRealm, state: &mut FormState, node: NodeId) -> OpResult<()> {
    let (is_textarea, is_input, mode) = {
        let session = realm.session.borrow();
        let document = session.document();
        let tag = html_local_name(document, node);
        (
            tag == Some("textarea"),
            tag == Some("input"),
            core_forms::input_value_mode(document, node),
        )
    };
    if !is_textarea && !(is_input && mode == Some(core_forms::InputValueMode::Value)) {
        return Err(OpError::new(
            "TypeError",
            "dirty value state requires a text control",
        ));
    }
    let current = control_value_with_state(realm, state, node)?;
    {
        let session = realm.session.borrow();
        capture_defaults(state, session.document(), node);
    }
    state.live_values.entry(node).or_insert(current);
    state.live_dirty.insert(node);
    state.dirty.insert(node);
    state.bump_validity_generation();
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
    let is_file = core_forms::input_value_mode(realm.session.borrow().document(), node)
        == Some(core_forms::InputValueMode::Filename);
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
    state.bump_validity_generation();
    Ok(())
}

/// Build a typed picker request from a file input's current attributes.
pub fn file_picker_request(realm: &Rc<DomRealm>, node: NodeId) -> OpResult<FilePickerRequest> {
    let document = realm.session.borrow();
    let NodeKind::Element { name, .. } = document.document().kind(node).map_err(dom_error)? else {
        return Err(OpError::new(
            "TypeError",
            "showPicker requires an input element",
        ));
    };
    if name != "input"
        || !null_attribute(document.document(), node, "type")
            .is_some_and(|value| value.eq_ignore_ascii_case("file"))
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
    let get = |key: &str| null_attribute(document.document(), node, key).map(str::to_owned);
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
    let is_file = core_forms::input_value_mode(realm.session.borrow().document(), node)
        == Some(core_forms::InputValueMode::Filename);
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
    if !state.live_dirty.contains(&node) {
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
    let peers = if checked {
        let session = realm.session.borrow();
        core_forms::radio_group_members(session.document(), node)
    } else {
        Vec::new()
    };
    state.checkedness.insert(
        node,
        Checkedness {
            value: checked,
            dirty: true,
        },
    );
    for peer in peers {
        let dirty = state
            .checkedness
            .get(&peer)
            .is_some_and(|checkedness| checkedness.dirty);
        state.checkedness.insert(
            peer,
            Checkedness {
                value: false,
                dirty,
            },
        );
    }
    state.dirty.insert(node);
    state.bump_validity_generation();
    Ok(())
}

pub fn checked(realm: &DomRealm, node: NodeId) -> OpResult<bool> {
    if let Some(checked) = realm.forms.borrow().checkedness.get(&node) {
        return Ok(checked.value);
    }
    let session = realm.session.borrow();
    Ok(has_null_attribute(session.document(), node, "checked"))
}

pub fn indeterminate(state: &FormState, node: NodeId) -> bool {
    state.indeterminate.contains(&node)
}

pub fn set_indeterminate(state: &mut FormState, node: NodeId, value: bool) {
    if value {
        state.indeterminate.insert(node);
    } else {
        state.indeterminate.remove(&node);
    }
}

pub fn control_value(realm: &DomRealm, node: NodeId) -> OpResult<String> {
    control_value_with_state(realm, &realm.forms.borrow(), node)
}

pub fn selected_index(realm: &DomRealm, node: NodeId) -> OpResult<isize> {
    selected_index_with_state(realm, &realm.forms.borrow(), node)
}

pub fn control_value_with_state(
    realm: &DomRealm,
    state: &FormState,
    node: NodeId,
) -> OpResult<String> {
    let session = realm.session.borrow();
    let document = session.document();
    if core_forms::input_value_mode(document, node) == Some(core_forms::InputValueMode::Filename) {
        return Ok(state
            .files
            .get(&node)
            .and_then(|files| files.first())
            .map_or_else(String::new, |file| format!("C:\\fakepath\\{}", file.name)));
    }
    if html_local_name(document, node) == Some("select") {
        return Ok(core_forms::select_value_by(document, node, |option| {
            state.selectedness.get(&option).copied()
        }));
    }
    let uses_live_value = html_local_name(document, node) == Some("textarea")
        || core_forms::input_value_mode(document, node) == Some(core_forms::InputValueMode::Value);
    if uses_live_value {
        if let Some(value) = state.live_values.get(&node) {
            return Ok(value.clone());
        }
    }
    default_control_value(document, node)
        .ok_or_else(|| OpError::new("TypeError", "value requires a form control"))
}

fn output_text_content(realm: &DomRealm, node: NodeId) -> OpResult<String> {
    let session = realm.session.borrow();
    let mut text = String::new();
    session
        .document()
        .append_descendant_text(node, &mut text)
        .map_err(dom_error)?;
    Ok(text)
}

pub fn input_value_as_number(realm: &DomRealm, node: NodeId) -> OpResult<f64> {
    let state = realm.forms.borrow();
    let session = realm.session.borrow();
    let document = session.document();
    if !core_forms::input_numeric_type_supported(document, node) {
        return Ok(f64::NAN);
    }
    if let Some(value) = state.live_values.get(&node) {
        return Ok(core_forms::input_value_as_number(document, node, value));
    }
    let value = default_control_value(document, node)
        .ok_or_else(|| OpError::type_error("numeric value requires an input"))?;
    Ok(core_forms::input_value_as_number(document, node, &value))
}

fn numeric_input_result(
    ctx: &mut Ctx,
    result: Result<String, core_forms::InputNumberError>,
) -> OpResult<String> {
    match result {
        Ok(value) => Ok(value),
        Err(core_forms::InputNumberError::Unrepresentable) => Ok(String::new()),
        Err(core_forms::InputNumberError::UnsupportedType) => Err(error_reporting::dom_exception(
            ctx,
            "InvalidStateError",
            "numeric operations do not apply to this input type",
        )),
        Err(core_forms::InputNumberError::StepAny) => Err(error_reporting::dom_exception(
            ctx,
            "InvalidStateError",
            "this input has no allowed value step",
        )),
    }
}

pub fn set_input_value_as_number(
    ctx: &mut Ctx,
    realm: &DomRealm,
    node: NodeId,
    value: f64,
) -> OpResult<()> {
    if value.is_infinite() {
        return Err(OpError::type_error("valueAsNumber cannot be infinite"));
    }
    let result = {
        let session = realm.session.borrow();
        core_forms::input_number_value_string(session.document(), node, value)
    };
    let value = numeric_input_result(ctx, result)?;
    set_control_value(realm, &mut realm.forms.borrow_mut(), node, &value)
}

pub fn step_input_value(
    ctx: &mut Ctx,
    realm: &DomRealm,
    node: NodeId,
    count: i64,
    direction: core_forms::InputStepDirection,
) -> OpResult<()> {
    let supported = {
        let session = realm.session.borrow();
        core_forms::input_numeric_type_supported(session.document(), node)
    };
    if !supported {
        return numeric_input_result(ctx, Err(core_forms::InputNumberError::UnsupportedType))
            .map(|_| ());
    }
    let value = control_value(realm, node)?;
    let result = {
        let session = realm.session.borrow();
        core_forms::input_step_value(session.document(), node, &value, count, direction)
    };
    match result {
        Ok(None) => Ok(()),
        result => {
            let value = numeric_input_result(ctx, result.map(|value| value.unwrap_or_default()))?;
            set_control_value(realm, &mut realm.forms.borrow_mut(), node, &value)
        }
    }
}

pub fn output_value(realm: &DomRealm, node: NodeId) -> OpResult<String> {
    output_text_content(realm, node)
}

pub fn output_default_value(realm: &DomRealm, state: &FormState, node: NodeId) -> OpResult<String> {
    if let Some(value) = state.output_default_overrides.get(&node) {
        Ok(value.clone())
    } else {
        output_text_content(realm, node)
    }
}

pub fn set_output_value(realm: &Rc<DomRealm>, node: NodeId, value: &str) -> OpResult<()> {
    let default_value = output_default_value(realm, &realm.forms.borrow(), node)?;
    realm
        .forms
        .borrow_mut()
        .output_default_overrides
        .insert(node, default_value);
    super::DomNode::replace_text_content_without_checkpoint(realm, node, value)
}

pub fn set_output_default_value(realm: &Rc<DomRealm>, node: NodeId, value: &str) -> OpResult<()> {
    if realm
        .forms
        .borrow()
        .output_default_overrides
        .contains_key(&node)
    {
        realm
            .forms
            .borrow_mut()
            .output_default_overrides
            .insert(node, value.to_owned());
        Ok(())
    } else {
        super::DomNode::replace_text_content_without_checkpoint(realm, node, value)
    }
}

pub fn selected_index_with_state(
    realm: &DomRealm,
    state: &FormState,
    node: NodeId,
) -> OpResult<isize> {
    if html_local_name(realm.session.borrow().document(), node) != Some("select") {
        return Err(OpError::new(
            "TypeError",
            "selectedIndex requires a select element",
        ));
    }
    let session = realm.session.borrow();
    let document = session.document();
    core_forms::selected_option_index_by(document, node, |option| {
        state.selectedness.get(&option).copied()
    })
    .map_err(dom_error)
}

pub fn set_select_selected_index(
    realm: &DomRealm,
    state: &mut FormState,
    node: NodeId,
    index: isize,
) -> OpResult<()> {
    let session = realm.session.borrow();
    let document = session.document();
    core_forms::for_each_select_option(document, node, |option, position| {
        capture_defaults(state, document, option);
        state
            .selectedness
            .insert(option, index >= -1 && position as isize == index);
        true
    })
    .map_err(dom_error)?;
    state.dirty.insert(node);
    state.bump_validity_generation();
    Ok(())
}

/// Implement the shared select/options-collection `add` algorithm. Numeric
/// references use the canonical streaming options list; element references
/// insert under that node's actual parent, which also handles options inside
/// optgroups. The DOM insertion helper owns hierarchy checks, adoption and
/// wrapper preservation.
pub(crate) fn add_select_element(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    select: NodeId,
    element: SelectAddElement,
    before: SelectAddBefore,
) -> OpResult<()> {
    let (realm, select) = realm.resolve_adopted_node(select);
    let (source, element_id) =
        ctx.with_instance::<DomNode, _>(&element.0, |node| (node.realm.clone(), node.id))?;
    let (source, element_id) = source.resolve_adopted_node(element_id);
    let element_is_valid = {
        let session = source.session.borrow();
        matches!(
            html_local_name(session.document(), element_id),
            Some("option" | "optgroup")
        )
    };
    if !element_is_valid {
        return Err(OpError::type_error(
            "select.add requires an HTMLOptionElement or HTMLOptGroupElement",
        ));
    }

    // This first pre-insertion validation enforces the algorithm's ancestor
    // check before resolving `before`. The actual insertion is validated again
    // against its selected parent by insert_dom_node.
    if Rc::ptr_eq(&realm, &source) {
        let session = realm.session.borrow();
        session
            .document()
            .validate_insert_from(session.document(), select, element_id, None)
            .map_err(dom_error)?;
    } else {
        let target = realm.session.borrow();
        let donor = source.session.borrow();
        target
            .document()
            .validate_insert_from(donor.document(), select, element_id, None)
            .map_err(dom_error)?;
    }

    let reference = match before {
        SelectAddBefore::Append => None,
        SelectAddBefore::Index(index) if index >= 0 => {
            let session = realm.session.borrow();
            core_forms::select_option_at(session.document(), select, index as usize)
                .map_err(dom_error)?
        }
        SelectAddBefore::Index(_) => None,
        SelectAddBefore::Element(value) => {
            let (before_realm, before_id) =
                ctx.with_instance::<super::DomHtmlElement, _>(&value, |before| {
                    let node = &before.base.base;
                    (node.realm.clone(), node.id)
                })?;
            let (before_realm, before_id) = before_realm.resolve_adopted_node(before_id);
            if !Rc::ptr_eq(&before_realm, &realm) {
                return Err(OpError::new(
                    "NotFoundError",
                    "before is not a descendant of the select element",
                ));
            }
            let session = realm.session.borrow();
            let document = session.document();
            let mut ancestor = document.parent(before_id).map_err(dom_error)?;
            let mut descendant = false;
            while let Some(node) = ancestor {
                if node == select {
                    descendant = true;
                    break;
                }
                ancestor = document.parent(node).map_err(dom_error)?;
            }
            if !descendant {
                return Err(OpError::new(
                    "NotFoundError",
                    "before is not a descendant of the select element",
                ));
            }
            Some(before_id)
        }
    };

    if reference == Some(element_id) && Rc::ptr_eq(&realm, &source) {
        return Ok(());
    }
    let (parent, before_value) = if let Some(reference) = reference {
        let parent = realm
            .session
            .borrow()
            .document()
            .parent(reference)
            .map_err(dom_error)?
            .ok_or_else(|| {
                OpError::new(
                    "NotFoundError",
                    "before is not a descendant of the select element",
                )
            })?;
        (parent, realm.wrap(ctx, reference))
    } else {
        (select, Value::Null)
    };

    super::insert_dom_node(ctx, &realm, parent, element.0, before_value)?;
    Ok(())
}

pub(crate) fn set_select_options_length(
    realm: &Rc<DomRealm>,
    select: NodeId,
    length: u32,
) -> OpResult<()> {
    let (realm, select) = realm.resolve_adopted_node(select);
    let changed = {
        let mut session = realm.session.borrow_mut();
        let document = session.document_mut();
        let current = core_forms::select_option_count(document, select).map_err(dom_error)?;
        if current == length as usize {
            false
        } else {
            core_forms::resize_select_options(document, select, length as usize)
                .map_err(dom_error)?;
            true
        }
    };
    if changed {
        select_option_list_changed(&realm, select)?;
    }
    Ok(())
}

pub(crate) fn set_select_option_at(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    select: NodeId,
    index: usize,
    option: Option<Value>,
) -> OpResult<()> {
    let (realm, select) = realm.resolve_adopted_node(select);
    let current_length = {
        let session = realm.session.borrow();
        core_forms::select_option_count(session.document(), select).map_err(dom_error)?
    };

    let Some(option) = option else {
        if index >= current_length {
            return Ok(());
        }
        core_forms::remove_select_option_at(
            realm.session.borrow_mut().document_mut(),
            select,
            index,
        )
        .map_err(dom_error)?;
        select_option_list_changed(&realm, select)?;
        return Ok(());
    };

    let (source, option_id) =
        ctx.with_instance::<super::DomOptionElement, _>(&option, |option| {
            let node = &option.base.base.base;
            (node.realm.clone(), node.id)
        })?;
    let (source, option_id) = source.resolve_adopted_node(option_id);

    if index > current_length {
        set_select_options_length(
            &realm,
            select,
            index.try_into().map_err(|_| {
                OpError::range_error("HTMLOptionsCollection index exceeds the supported length")
            })?,
        )?;
    }

    let old_option = {
        let session = realm.session.borrow();
        core_forms::select_option_at(session.document(), select, index).map_err(dom_error)?
    };
    if let Some(old_option) = old_option {
        if Rc::ptr_eq(&realm, &source) && old_option == option_id {
            return Ok(());
        }
        let parent = {
            let session = realm.session.borrow();
            let document = session.document();
            document
                .parent(old_option)
                .map_err(dom_error)?
                .ok_or_else(|| OpError::new("NotFoundError", "option is detached"))?
        };
        let before = realm.wrap(ctx, old_option);
        super::insert_dom_node(ctx, &realm, parent, option, before)?;
        realm
            .session
            .borrow_mut()
            .document_mut()
            .remove(old_option)
            .map_err(dom_error)?;
        select_option_list_changed(&realm, select)?;
    } else {
        super::insert_dom_node(ctx, &realm, select, option, Value::Null)?;
    }
    Ok(())
}

pub(crate) fn select_option_named_item(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    select: NodeId,
    name: &str,
) -> OpResult<Value> {
    let (realm, select) = realm.resolve_adopted_node(select);
    let option = {
        let session = realm.session.borrow();
        core_forms::select_option_named_item(session.document(), select, name).map_err(dom_error)?
    };
    Ok(option.map_or(Value::Null, |option| realm.wrap(ctx, option)))
}

pub fn option_selected_with_state(realm: &DomRealm, state: &FormState, node: NodeId) -> bool {
    let session = realm.session.borrow();
    let document = session.document();
    if let Ok(Some(select)) = core_forms::option_select(document, node) {
        if let Some(selected) = state.single_select_option(document, select) {
            return selected == Some(node);
        }
    }
    core_forms::option_is_selected_by(document, node, |option| {
        state.selectedness.get(&option).copied()
    })
    .unwrap_or(false)
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
        core_forms::option_select(document, node)
            .map_err(dom_error)?
            .map(|select| (select, has_null_attribute(document, select, "multiple")))
    };
    if let Some((select, false)) = select {
        if selected {
            let session = realm.session.borrow();
            let document = session.document();
            core_forms::for_each_select_option(document, select, |option, _| {
                capture_defaults(state, document, option);
                state.selectedness.insert(option, option == node);
                true
            })
            .map_err(dom_error)?;
        } else {
            state.selectedness.insert(node, false);
        }
    } else {
        state.selectedness.insert(node, selected);
    }
    state.dirty.insert(node);
    state.bump_validity_generation();
    if let Some((select, _)) = select {
        let session = realm.session.borrow();
        normalize_select_selection_with_state(session.document(), state, select, None)?;
    }
    Ok(())
}

fn normalize_select_selection_with_state(
    document: &lumen_html::Document,
    state: &mut FormState,
    select: NodeId,
    preferred_option: Option<NodeId>,
) -> OpResult<bool> {
    if html_local_name(document, select) != Some("select") {
        return Ok(false);
    }

    let multiple = has_null_attribute(document, select, "multiple");
    let has_fallback = !multiple && core_forms::select_display_size(document, select) == Some(1);
    let mut last_selected = None;
    let mut preferred_selected = false;
    let mut selected_count = 0usize;
    let mut first_enabled = None;
    core_forms::for_each_select_option(document, select, |option, _| {
        let selected = state
            .selectedness
            .get(&option)
            .copied()
            .unwrap_or_else(|| has_null_attribute(document, option, "selected"));
        if selected {
            last_selected = Some(option);
            selected_count += 1;
            if preferred_option == Some(option) {
                preferred_selected = true;
            }
        }
        if first_enabled.is_none() && !core_forms::option_disabled(document, option) {
            first_enabled = Some(option);
        }
        true
    })
    .map_err(dom_error)?;

    let keep_selected = preferred_selected
        .then_some(preferred_option)
        .flatten()
        .or(last_selected);
    let mut changed = false;
    if !multiple && selected_count > 1 {
        core_forms::for_each_select_option(document, select, |option, _| {
            let selected = state
                .selectedness
                .get(&option)
                .copied()
                .unwrap_or_else(|| has_null_attribute(document, option, "selected"));
            if selected && Some(option) != keep_selected {
                capture_defaults(state, document, option);
                if state.selectedness.insert(option, false) != Some(false) {
                    changed = true;
                }
            }
            true
        })
        .map_err(dom_error)?;
    }

    if keep_selected.is_none() && has_fallback {
        if let Some(option) = first_enabled {
            capture_defaults(state, document, option);
            if state.selectedness.insert(option, true) != Some(true) {
                changed = true;
            }
        }
    }
    Ok(changed)
}

/// Re-run a select's selectedness normalization after a relevant option-tree
/// or selection-state mutation. The shared walker keeps this constant-space.
pub fn select_option_list_changed(realm: &Rc<DomRealm>, select: NodeId) -> OpResult<()> {
    select_option_list_changed_with_preferred(realm, select, None)
}

pub(crate) fn select_option_list_changed_with_preferred(
    realm: &Rc<DomRealm>,
    select: NodeId,
    preferred_option: Option<NodeId>,
) -> OpResult<()> {
    let (realm, select) = realm.resolve_adopted_node(select);
    let mut state = realm.forms.borrow_mut();
    let session = realm.session.borrow();
    if normalize_select_selection_with_state(
        session.document(),
        &mut state,
        select,
        preferred_option,
    )? {
        state.bump_validity_generation();
    }
    Ok(())
}

/// Return the last selected option across a bounded set of insertion roots.
/// This is used after a DocumentFragment has been drained, when the fragment
/// itself no longer contains the options whose selectedness was snapshotted
/// before insertion.
pub(crate) fn selected_option_in_roots(
    realm: &DomRealm,
    roots: &[NodeId],
) -> OpResult<Option<NodeId>> {
    let state = realm.forms.borrow();
    let session = realm.session.borrow();
    let document = session.document();
    let mut selected = None;
    for &root in roots {
        core_forms::for_each_option_in_subtree(document, root, |option, _| {
            let is_selected = state
                .selectedness
                .get(&option)
                .copied()
                .unwrap_or_else(|| has_null_attribute(document, option, "selected"));
            if is_selected {
                selected = Some(option);
            }
            true
        })
        .map_err(dom_error)?;
    }
    Ok(selected)
}

/// Preserve the actual selectedness of options whose select-list owner may
/// change during a tree mutation. Most option values remain represented by
/// their `selected` attribute; only existing overrides, selected attributes
/// suppressed by a single-select winner, and selected options without a
/// selected attribute need a sparse entry in FormState.
///
/// `source_select` is the select-list owner before the mutation, if any. A
/// single-select winner is resolved once, then reused for every moved option
/// so a large optgroup does not trigger one full select scan per option. The
/// returned booleans distinguish options in that source list from unowned
/// options that might become list members after insertion.
pub(crate) fn capture_option_subtree_selectedness(
    realm: &DomRealm,
    root: NodeId,
    source_select: Option<NodeId>,
) -> OpResult<(bool, bool, Option<NodeId>)> {
    let mut state = realm.forms.borrow_mut();
    let session = realm.session.borrow();
    let document = session.document();
    let single_select_winner = source_select
        .and_then(|select| state.single_select_option(document, select));
    let mut contains_source_options = false;
    let mut contains_unowned_options = false;
    let mut selected_option = None;
    let mut walk_error = None;
    core_forms::for_each_option_in_subtree(document, root, |option, _| {
        let owner = match core_forms::option_select(document, option) {
            Ok(owner) => owner,
            Err(error) => {
                walk_error = Some(error);
                return false;
            }
        };
        // Options inside nested selects keep their owner when an ancestor
        // subtree moves. Unowned options are also snapshotted: changing an
        // optgroup boundary can make them members of the destination list.
        let source_member = owner.is_some() && owner == source_select;
        if source_member {
            contains_source_options = true;
        } else if owner.is_none() {
            contains_unowned_options = true;
        } else {
            return true;
        }
        let had_override = state.selectedness.get(&option).copied();
        let has_selected_attribute = has_null_attribute(document, option, "selected");
        let selected = match (source_member, single_select_winner) {
            (true, Some(winner)) => winner == Some(option),
            _ => had_override.unwrap_or(has_selected_attribute),
        };
        if had_override.is_some()
            || (has_selected_attribute && !selected)
            || (selected && !has_selected_attribute)
        {
            state.selectedness.insert(option, selected);
        }
        if selected {
            selected_option = Some(option);
        }
        true
    })
    .map_err(dom_error)?;
    if let Some(error) = walk_error {
        return Err(dom_error(error));
    }
    Ok((
        contains_source_options,
        contains_unowned_options,
        selected_option,
    ))
}

pub(crate) fn option_subtree_has_select_owner(
    realm: &DomRealm,
    root: NodeId,
    select: NodeId,
) -> OpResult<bool> {
    let session = realm.session.borrow();
    let document = session.document();
    let mut found = false;
    let mut walk_error = None;
    core_forms::for_each_option_in_subtree(document, root, |option, _| {
        match core_forms::option_select(document, option) {
            Ok(Some(owner)) if owner == select => {
                found = true;
                false
            }
            Ok(_) => true,
            Err(error) => {
                walk_error = Some(error);
                false
            }
        }
    })
    .map_err(dom_error)?;
    if let Some(error) = walk_error {
        return Err(dom_error(error));
    }
    Ok(found)
}

/// Initialize an option's selectedness without setting its dirty flag.
///
/// The `Option()` legacy factory can produce a selectedness value that differs
/// from the `selected` content attribute. Store only overrides needed to
/// preserve that state; an ordinary false option with no selected attribute
/// remains eligible for a single-select's first-option fallback.
pub fn initialize_option_selectedness(
    realm: &DomRealm,
    node: NodeId,
    selected: bool,
) -> OpResult<()> {
    let mut state = realm.forms.borrow_mut();
    let session = realm.session.borrow();
    let document = session.document();
    if html_local_name(document, node) != Some("option") {
        return Err(OpError::new(
            "TypeError",
            "selectedness initialization requires an HTML option",
        ));
    }
    capture_defaults(&mut state, document, node);
    if selected || has_null_attribute(document, node, "selected") {
        state.selectedness.insert(node, selected);
    } else {
        state.selectedness.remove(&node);
    }
    Ok(())
}

/// Apply the selected-attribute reaction for a clean HTML option. A property
/// write marks the option dirty and therefore remains authoritative when the
/// content attribute changes later.
pub fn option_selected_attribute_changed(realm: &DomRealm, node: NodeId) -> OpResult<()> {
    let mut state = realm.forms.borrow_mut();
    if state.dirty.contains(&node) {
        return Ok(());
    }
    let session = realm.session.borrow();
    let document = session.document();
    if html_local_name(document, node) != Some("option") {
        return Ok(());
    }
    capture_defaults(&mut state, document, node);
    let selected = has_null_attribute(document, node, "selected");
    let select = select_ancestor_for_option(document, node);
    if selected {
        state.selectedness.insert(node, true);
        if let Some((select, _)) = select.filter(|(_, multiple)| !*multiple) {
            core_forms::for_each_select_option(document, select, |option, _| {
                if option != node {
                    capture_defaults(&mut state, document, option);
                    state.selectedness.insert(option, false);
                }
                true
            })
            .map_err(dom_error)?;
        }
    } else {
        state.selectedness.remove(&node);
    }
    if let Some((select, _)) = select {
        normalize_select_selection_with_state(document, &mut state, select, None)?;
    }
    state.bump_validity_generation();
    Ok(())
}

fn select_ancestor_for_option(
    document: &lumen_html::Document,
    option: NodeId,
) -> Option<(NodeId, bool)> {
    core_forms::option_select(document, option)
        .ok()
        .flatten()
        .map(|node| (node, has_null_attribute(document, node, "multiple")))
}

pub fn option_selected(realm: &DomRealm, node: NodeId) -> bool {
    option_selected_with_state(realm, &realm.forms.borrow(), node)
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
    form_data_with_encoding(ctx, realm, form, submitter, "UTF-8")
}

fn form_data_with_encoding(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    form: NodeId,
    submitter: Option<NodeId>,
    encoding: &str,
) -> OpResult<Value> {
    let data = lumen_host::blob::new_form_data(ctx);
    populate_form_data_with_encoding(ctx, realm, form, submitter, data.clone(), encoding)?;
    Ok(data)
}

fn file_object(ctx: &mut Ctx, file: &FormFile) -> OpResult<Value> {
    Ok(lumen_host::blob::new_file(
        ctx,
        file.bytes.to_vec(),
        &file.name,
        &file.media_type,
        file.last_modified as f64,
    ))
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
    populate_form_data_with_encoding(ctx, realm, form, submitter, data, "UTF-8")
}

fn populate_form_data_with_encoding(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    form: NodeId,
    submitter: Option<NodeId>,
    data: Value,
    encoding: &str,
) -> OpResult<()> {
    if let Some(submitter) = submitter {
        let session = realm.session.borrow();
        let document = session.document();
        let is_submitter = core_forms::form_owner(document, submitter) == Some(form)
            && core_forms::is_submit_button(document, submitter);
        if !is_submitter {
            return Err(OpError::new(
                "TypeError",
                "submitter is not a submit button associated with this form",
            ));
        }
    }
    // Freeze only the live entry-list inputs before calling author-controlled
    // FormData.append methods. The full FormState also contains unrelated
    // validity, selection, and wrapper state and must not be cloned here.
let (entries,custom_entries)={
    let state=realm.forms.borrow();let session=realm.session.borrow();let document=session.document();
    let mut entries=Vec::new();let mut custom_entries=Vec::new();
    core_forms::for_each_form_control(document,form,|node| {
        if document.is_form_associated_custom_element(node) {
            if !core_forms::is_disabled(document,node) {
                if let Some(value)=state.custom_elements.get(&node).and_then(|state|state.borrow().submission.clone()) {
                    let name=document.get_attribute_ns_ref(node,None,"name").ok().flatten().unwrap_or("").to_owned();
                    custom_entries.push((entries.len(),name,value));
                }
            }
        }else{core_forms::append_form_entries_for_control(document,node,submitter,&*state,encoding,&mut entries);}
        true
    }).map_err(dom_error)?;(entries,custom_entries)
};
let mut custom_entries=custom_entries.into_iter().peekable();
for (index,entry) in entries.into_iter().enumerate() {
    append_custom_form_entries(ctx,&data,index,&mut custom_entries)?;

        match entry.value {
            FormEntryValue::Text(value) => {
                lumen_host::blob::append_text(ctx, &data, &entry.name, &value)?
            }
            FormEntryValue::File(file) => lumen_host::blob::append_file(
                ctx,
                &data,
                &entry.name,
                lumen_host::blob::FormFile {
                    name: file.name,
                    media_type: file.media_type,
                    last_modified: file.last_modified as f64,
                    bytes: file.bytes.to_vec().into(),
                },
            )?,
        }
    }
    append_custom_form_entries(ctx,&data,usize::MAX,&mut custom_entries)?;
    realm.dispatch_user_agent(ctx, form, "formdata", false, false, &[("formData", data)])?;
    Ok(())
}

fn append_custom_form_entries(ctx:&mut Ctx,data:&Value,before:usize,entries:&mut std::iter::Peekable<std::vec::IntoIter<(usize,String,Value)>>)->OpResult<()> {
    while entries.peek().is_some_and(|entry|entry.0<=before){
        let (_,name,value)=entries.next().expect("peeked custom entry");
        if lumen_host::blob::is_form_data(ctx,&value){lumen_host::blob::append_form_data(ctx,data,&value)?;}
        else if !name.is_empty(){
            if let Value::Str(text)=value {lumen_host::blob::append_text(ctx,data,&name,text.as_str())?;}
            else{lumen_host::blob::append_file_value(ctx,data,&name,value)?;}
        }
    }Ok(())
}

pub fn set_custom_validity(state: &mut FormState, node: NodeId, message: &str) {
    let previous = state.custom_messages.get(&node).map(String::as_str);
    if previous == Some(message) || (previous.is_none() && message.is_empty()) {
        return;
    }
    if message.is_empty() {
        state.custom_messages.remove(&node);
    } else {
        state.custom_messages.insert(node, message.to_owned());
    }
    state.bump_validity_generation();
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
    owner: Value,
}

impl lumen::embed::NativeIdentityOwner for DomValidityState {
    const TRACES_NATIVE_VALUES:bool=true;
    fn trace_native_identities(&self,_:u64,_:&mut dyn FnMut(&Value)){}
    fn trace_native_values(&self,visit:&mut dyn FnMut(&Value)){visit(&self.owner);}
}
impl DomValidityState {
    fn current(&self, ctx: &mut Ctx) -> OpResult<ValidityState> {
        ctx.with_instance::<DomNode, _>(&self.owner, |node| {
            let state = node.realm.forms.borrow();
            current_validity(&node.realm, node.id, &state)
        })?
    }
}

#[lumen_bind::methods]
impl DomValidityState {
    #[getter]
    fn value_missing(&self, ctx: &mut Ctx) -> OpResult<bool> {
        Ok(self.current(ctx)?.value_missing)
    }
    #[getter]
    fn type_mismatch(&self, ctx: &mut Ctx) -> OpResult<bool> {
        Ok(self.current(ctx)?.type_mismatch)
    }
    #[getter]
    fn too_long(&self, ctx: &mut Ctx) -> OpResult<bool> {
        Ok(self.current(ctx)?.too_long)
    }
    #[getter]
    fn too_short(&self, ctx: &mut Ctx) -> OpResult<bool> {
        Ok(self.current(ctx)?.too_short)
    }
    #[getter]
    fn pattern_mismatch(&self, ctx: &mut Ctx) -> OpResult<bool> {
        Ok(self.current(ctx)?.pattern_mismatch)
    }
    #[getter]
    fn range_overflow(&self, ctx: &mut Ctx) -> OpResult<bool> {
        Ok(self.current(ctx)?.range_overflow)
    }
    #[getter]
    fn range_underflow(&self, ctx: &mut Ctx) -> OpResult<bool> {
        Ok(self.current(ctx)?.range_underflow)
    }
    #[getter]
    fn step_mismatch(&self, ctx: &mut Ctx) -> OpResult<bool> {
        Ok(self.current(ctx)?.step_mismatch)
    }
    #[getter]
    fn bad_input(&self, ctx: &mut Ctx) -> OpResult<bool> {
        Ok(self.current(ctx)?.bad_input)
    }
    #[getter]
    fn custom_error(&self, ctx: &mut Ctx) -> OpResult<bool> {
        Ok(self.current(ctx)?.custom_error)
    }
    #[getter]
    fn valid(&self, ctx: &mut Ctx) -> OpResult<bool> {
        Ok(self.current(ctx)?.valid())
    }
}

fn current_validity(realm: &DomRealm, node: NodeId, state: &FormState) -> OpResult<ValidityState> {
    let session = realm.session.borrow();
    Ok(core_forms::validity_with_view(
        session.document(),
        node,
        state,
    ))
}

pub fn validity_object(ctx: &mut Ctx, owner: Value) -> OpResult<Value> {
    let value=ctx.new_instance(DomValidityState { owner });
    ctx.set_native_identity_owner::<DomValidityState>(&value)?;Ok(value)
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
    let state = state.borrow();
    if let Some(custom)=state.custom_elements.get(&node){return Ok(custom.borrow().message.clone());}
    if let Some(message) = state.custom_messages.get(&node) {
        return Ok(message.clone());
    }
    let validity = current_validity(realm, node, &state)?;
    let message = if validity.value_missing {
        "Please fill out this field."
    } else if validity.type_mismatch {
        "Please enter a value with a valid type."
    } else if validity.too_long {
        "Please shorten this value."
    } else if validity.too_short {
        "Please lengthen this value."
    } else if validity.pattern_mismatch {
        "Please match the requested format."
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
    Ok(validate_control(ctx, realm, node, state)?.0)
}

fn validate_control(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    state: &RefCell<FormState>,
) -> OpResult<(bool, bool)> {
    if !core_forms::will_validate(realm.session.borrow().document(), node) {
        return Ok((true, false));
    }
    let valid = {
        let state = state.borrow();
        current_validity(realm, node, &state)?.valid()
    };
    if valid {
        return Ok((true, false));
    }
    let not_canceled = realm.dispatch(ctx, node, "invalid", false, true, &[])?;
    Ok((false, not_canceled))
}

/// Snapshot invalid associated controls before any `invalid` listener runs.
/// The shared validity view borrows live adapter state only while collecting
/// NodeIds, so script can safely mutate controls during event dispatch without
/// changing this validation round.
fn invalid_form_controls(
    realm: &Rc<DomRealm>,
    form: NodeId,
    state: &RefCell<FormState>,
) -> Vec<NodeId> {
    let state = state.borrow();
    let session = realm.session.borrow();
    let document = session.document();
    core_forms::form_controls(document, form)
        .into_iter()
        .filter(|node| core_forms::will_validate(document, *node))
        .filter(|node| !core_forms::validity_with_view(document, *node, &*state).valid())
        .collect()
}

/// Reports a validity failure through `invalid` and focuses the target only
/// when the event was not canceled. The host has no native validation bubble.
pub fn report_validity(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    state: &RefCell<FormState>,
) -> OpResult<bool> {
    let (valid, not_canceled) = validate_control(ctx, realm, node, state)?;
    if !valid && not_canceled {
        realm.focus(ctx, Some(node))?;
    }
    Ok(valid)
}

pub(crate) fn report_custom_validity(ctx:&mut Ctx,realm:&Rc<DomRealm>,node:NodeId,custom:&Rc<RefCell<CustomFormState>>)->OpResult<bool>{
    let (valid,not_canceled)=validate_control(ctx,realm,node,&realm.forms)?;
    if !valid && not_canceled {
        let anchor=custom.borrow().anchor.clone();
        let target=if let Some(anchor)=anchor {ctx.with_instance::<DomNode,_>(&anchor,|node|node.realm.resolve_adopted_node(node.id)).ok()}else{None};
        if let Some((owner,id))=target {owner.focus(ctx,Some(id))?;}else{realm.focus(ctx,Some(node))?;}
    }Ok(valid)
}

/// Statically validate the form's associated controls and fire one invalid
/// event for each invalid control in document order.
pub fn check_form_validity(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    form: NodeId,
    state: &RefCell<FormState>,
) -> OpResult<bool> {
    let invalid = invalid_form_controls(realm, form, state);
    for control in invalid.iter().copied() {
        realm.dispatch(ctx, control, "invalid", false, true, &[])?;
    }
    Ok(invalid.is_empty())
}

/// Report all invalid associated controls, then focus the first invalid one
/// whose `invalid` event was not canceled.
pub fn report_form_validity(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    form: NodeId,
    state: &RefCell<FormState>,
) -> OpResult<bool> {
    let invalid = invalid_form_controls(realm, form, state);
    let mut focus_candidates = Vec::new();
    for control in invalid.iter().copied() {
        let not_canceled = realm.dispatch(ctx, control, "invalid", false, true, &[])?;
        if not_canceled {
            focus_candidates.push(control);
        }
    }
    for control in focus_candidates {
        if realm.focus_rendered(control)? {
            realm.focus(ctx, Some(control))?;
            break;
        }
    }
    Ok(invalid.is_empty())
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
    if !prepare_submission(ctx, realm, form, submitter, state, SubmissionKind::Request)? {
        return Ok(None);
    }
    Ok(Some(form_entries_from_state(
        realm,
        form,
        submitter,
        &state.borrow(),
    )))
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
    if !prepare_submission(ctx, realm, form, submitter, state, SubmissionKind::Request)? {
        return Ok(None);
    }
    // Do not call `request_submit` here: its compatibility result is a native
    // entry-list vector. Navigation must instead snapshot the actual FormData
    // after `formdata` listeners mutate it, so building that vector first
    // would be unused work.
    finish_submission(ctx, realm, form, submitter)
}

/// Run legacy `HTMLFormElement.submit()`: skip constraint validation and the
/// `submit` event, while retaining FormData construction and its `formdata`
/// event before the host receives a navigation request.
pub fn legacy_submission(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    form: NodeId,
    state: &RefCell<FormState>,
) -> OpResult<Option<FormSubmissionRequest>> {
    if !prepare_submission(ctx, realm, form, None, state, SubmissionKind::Legacy)? {
        return Ok(None);
    }
    finish_submission(ctx, realm, form, None)
}

#[derive(Clone, Copy)]
enum SubmissionKind {
    Request,
    Legacy,
}

/// Apply the stages shared by requestSubmit and legacy submit. Legacy submit
/// intentionally omits validation and the cancelable submit event.
fn prepare_submission(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    form: NodeId,
    submitter: Option<NodeId>,
    state: &RefCell<FormState>,
    kind: SubmissionKind,
) -> OpResult<bool> {
    if matches!(kind, SubmissionKind::Legacy) {
        return Ok(true);
    }
    let validation_bypassed = {
        let session = realm.session.borrow();
        core_forms::validation_bypassed(session.document(), form, submitter)
    };
    let invalid = if validation_bypassed {
        Vec::new()
    } else {
        invalid_form_controls(realm, form, state)
    };
    if !validation_bypassed {
        mark_form_user_validity_interacted(realm, form, state)?;
    }
    for node in &invalid {
        realm.dispatch(ctx, *node, "invalid", false, true, &[])?;
    }
    if !invalid.is_empty() {
        return Ok(false);
    }
    let properties = submitter
        .map(|node| vec![("submitter", realm.wrap(ctx, node))])
        .unwrap_or_default();
    realm.dispatch_user_agent(ctx, form, "submit", true, true, &properties)
}

fn finish_submission(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    form: NodeId,
    submitter: Option<NodeId>,
) -> OpResult<Option<FormSubmissionRequest>> {
    // Pick encoding before constructing the entry list: a formdata listener
    // may mutate accept-charset without changing this submission's encoding.
    let encoding = realm.with_session(|session| {
        core_forms::pick_form_encoding(
            null_attribute(session.document(), form, "accept-charset"),
            realm.document_encoding(),
        )
    });
    // Constructing the entry list dispatches a non-cancelable `formdata`
    // event. Snapshot the same object after listeners have run, preserving
    // listener additions/removals and actual File bytes for navigation.
    let form_data = form_data_with_encoding(ctx, realm, form, submitter, encoding)?;
    let entries = snapshot_form_data(ctx, &form_data)?;
    let session = realm.session.borrow();
    let Some(mut metadata) =
        core_forms::submission_metadata(session.document(), form, submitter, entries)
    else {
        return Ok(None);
    };
    metadata.encoding = encoding.to_owned();
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
        form,
        metadata,
        form_data,
        navigation_metadata: {
            let mut metadata = super::browsing_context::NavigationMetadata::from_document(realm);
            metadata.source_element = Some(form);
            metadata
        },
    }))
}

/// Build the entry list from the realm's borrowed live-control state. The core
/// builder queries only successful controls belonging to this form and clones
/// data only for the returned entry-list snapshot.
fn form_entries_from_state(
    realm: &Rc<DomRealm>,
    form: NodeId,
    submitter: Option<NodeId>,
    state: &FormState,
) -> Vec<FormEntry> {
    core_forms::form_entries_with_state(realm.session.borrow().document(), form, submitter, state)
}

fn snapshot_form_data(ctx: &mut Ctx, data: &Value) -> OpResult<Vec<FormEntry>> {
    Ok(lumen_host::blob::form_data_entries(ctx, data)?
        .into_iter()
        .map(|entry| FormEntry {
            name: entry.name,
            value: match entry.value {
                lumen_host::blob::FormValue::Text(text) => FormEntryValue::Text(text),
                lumen_host::blob::FormValue::File(file) => FormEntryValue::File(FormFile {
                    name: file.name,
                    media_type: file.media_type,
                    last_modified: file.last_modified as i64,
                    bytes: file.bytes.to_vec().into(),
                }),
            },
        })
        .collect())
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
    custom_elements::enqueue_form_reset(realm,form)?;
    let controls = core_forms::form_reset_controls(realm.session.borrow().document(), form);
    let has_controls = !controls.is_empty();
    let selects = {
        let session = realm.session.borrow();
        controls
            .iter()
            .copied()
            .filter(|node| html_local_name(session.document(), *node) == Some("select"))
            .collect::<Vec<_>>()
    };
    let mut output_resets = Vec::new();
    let mut state = state.borrow_mut();
    let mut session = realm.session.borrow_mut();
    let document = session.document_mut();
    for node in controls {
        state.number_edits.remove(&node);
        let Some(name) = html_local_name(document, node) else {
            continue;
        };
        if name == "output" {
            let value = if let Some(default_value) = state.output_default_overrides.remove(&node) {
                default_value
            } else {
                let mut value = String::new();
                document
                    .append_descendant_text(node, &mut value)
                    .map_err(dom_error)?;
                value
            };
            output_resets.push((node, value));
            state.dirty.remove(&node);
            state.checkedness.remove(&node);
            state.clear_user_state(node);
            state.live_dirty.remove(&node);
            state.live_values.remove(&node);
            state.selectedness.remove(&node);
            state.defaults.remove(&node);
            continue;
        }
        let Some(defaults) = state.defaults.get(&node).cloned() else {
            state.dirty.remove(&node);
            state.checkedness.remove(&node);
            state.clear_user_state(node);
            state.live_dirty.remove(&node);
            state.live_values.remove(&node);
            state.selectedness.remove(&node);
            continue;
        };
        let is_option = name == "option";
        if is_option {
            if defaults.selected {
                document
                    .set_attribute_ns(node, None, "selected", "")
                    .map_err(dom_error)?;
            } else {
                document
                    .remove_attribute_ns(node, None, "selected")
                    .map_err(dom_error)?;
            }
        }
        state.dirty.remove(&node);
        state.checkedness.remove(&node);
        state.clear_user_state(node);
        state.live_dirty.remove(&node);
        state.live_values.remove(&node);
        state.selectedness.remove(&node);
        if core_forms::input_value_mode(document, node)
            == Some(core_forms::InputValueMode::Filename)
        {
            state.files.remove(&node);
            if let Some(slot) = state.file_lists.get(&node) {
                slot.borrow_mut().values.clear();
            }
        }
        state.defaults.remove(&node);
    }
    for select in selects {
        if core_forms::select_display_size(document, select) != Some(1) {
            continue;
        }
        let mut has_default_selected = false;
        let mut first_enabled = None;
        core_forms::for_each_select_option(document, select, |option, _| {
            has_default_selected |= null_attribute(document, option, "selected").is_some();
            if first_enabled.is_none() && !core_forms::option_disabled(document, option) {
                first_enabled = Some(option);
            }
            true
        })
        .map_err(dom_error)?;
        if !has_default_selected {
            if let Some(option) = first_enabled {
                state.selectedness.insert(option, true);
            }
        }
    }
    if has_controls {
        state.bump_validity_generation();
    }
    drop(session);
    drop(state);
    for (output, value) in output_resets {
        super::DomNode::replace_text_content_without_checkpoint(realm, output, &value)?;
    }
    Ok(true)
}

#[derive(Clone, Copy)]
enum ClickActivation {
    None,
    Checkable { radio: bool },
    Reset,
    Submit,
}

fn click_activation(document: &lumen_html::Document, node: NodeId) -> ClickActivation {
    let Some(name) = html_local_name(document, node) else {
        return ClickActivation::None;
    };
    let control_type = null_attribute(document, node, "type").unwrap_or(if name == "button" {
        "submit"
    } else {
        "text"
    });
    match name {
        "input" if control_type.eq_ignore_ascii_case("checkbox") => {
            ClickActivation::Checkable { radio: false }
        }
        "input" if control_type.eq_ignore_ascii_case("radio") => {
            ClickActivation::Checkable { radio: true }
        }
        "input" if control_type.eq_ignore_ascii_case("reset") => ClickActivation::Reset,
        "button" if control_type.eq_ignore_ascii_case("reset") => ClickActivation::Reset,
        "input"
            if control_type.eq_ignore_ascii_case("submit")
                || control_type.eq_ignore_ascii_case("image") =>
        {
            ClickActivation::Submit
        }
        // Invalid or missing button types use the submit state; input's missing and invalid type
        // values use the text state above.
        "button" if !control_type.eq_ignore_ascii_case("button") => ClickActivation::Submit,
        _ => ClickActivation::None,
    }
}

/// Implement HTMLElement.click() with an untrusted synthetic pointer event and
/// the shared activation behavior. Unlike a pointer interaction, this does not
/// focus the element or dispatch pointer/mouse compatibility events.
pub fn click_element(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    target: &DomEventTarget,
    node: NodeId,
) -> OpResult<()> {
    let view = realm
        .window_wrapper
        .borrow()
        .as_ref()
        .and_then(WeakValue::upgrade)
        .unwrap_or(Value::Null);
    let event = super::ui_events::synthetic_click_event(ctx, view)?;
    click_with_event(ctx, realm, target, node, event, false, false, true)
}

pub(crate) fn activate_keyboard_default_button(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    button: NodeId,
) -> OpResult<()> {
    let view = realm
        .window_wrapper
        .borrow()
        .as_ref()
        .and_then(WeakValue::upgrade)
        .unwrap_or(Value::Null);
    let event = super::ui_events::synthetic_click_event(ctx, view)?;
    let target = DomEventTarget::node(realm, button);
    click_with_event(ctx, realm, &target, button, event, true, false, true)
}

/// Dispatch a bounded trusted primary-mouse interaction against a target that
/// the host has already hit-tested. The bridge is responsible for scrolling,
/// viewport/interception checks, and replacing stale targets before calling.
pub fn trusted_pointer_click(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    client_x: f64,
    client_y: f64,
) -> OpResult<()> {
    if !client_x.is_finite() || !client_y.is_finite() {
        return Err(OpError::type_error("pointer coordinates must be finite"));
    }
    let is_element = {
        let session = realm.session.borrow();
        matches!(
            session.document().kind(node).map_err(dom_error)?,
            lumen_html::NodeKind::Element { .. }
        )
    };
    if !is_element {
        return Err(OpError::type_error("pointer target must be an Element"));
    }
    if !connected_pointer_target(realm, node) {
        return Err(OpError::new(
            "InvalidStateError",
            "pointer target is no longer connected to its document",
        ));
    }
    let mut state = TrustedPointerState::default();
    let modifiers = super::ui_events::UserAgentModifiers::default();
    trusted_pointer_move(ctx, realm, &mut state, node, client_x, client_y, modifiers)?;
    trusted_pointer_down(ctx, realm, &mut state, node, modifiers)?;
    trusted_pointer_up(ctx, realm, &mut state, node, modifiers)
}

/// Persistent pointer state used by the bounded WebDriver Actions profile. It stores native
/// document identities and scalar input state, never JavaScript Values.
#[derive(Clone, Copy)]
pub(crate) struct TrustedPointerState {
    pub x: f64,
    pub y: f64,
    pub hover_target: Option<NodeId>,
    pub down_target: Option<NodeId>,
    pub buttons: u16,
    /// Whether pointerdown permitted compatibility mouse events. Canceling pointerdown suppresses
    /// mousedown/mouseup, while click dispatch remains governed by target continuity.
    pub compatibility_mouse_allowed: bool,
    /// Whether the down target remained connected and may receive click on a matching release.
    pub down_allowed: bool,
    pub properties: super::ui_events::UserAgentPointerProperties,
    pub pressure: f64,
    pub click_detail: i32,
}

impl Default for TrustedPointerState {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            hover_target: None,
            down_target: None,
            buttons: 0,
            compatibility_mouse_allowed: false,
            down_allowed: false,
            properties: super::ui_events::UserAgentPointerProperties::default(),
            pressure: 0.0,
            click_detail: 1,
        }
    }
}

pub(crate) fn trusted_pointer_move(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    state: &mut TrustedPointerState,
    target: NodeId,
    x: f64,
    y: f64,
    modifiers: super::ui_events::UserAgentModifiers,
) -> OpResult<()> {
    if !x.is_finite() || !y.is_finite() {
        return Err(OpError::type_error("pointer coordinates must be finite"));
    }
    if !connected_pointer_target(realm, target) {
        return Err(OpError::new(
            "InvalidStateError",
            "pointer target is no longer connected to its document",
        ));
    }
    let touch = state.properties.pointer_type == "touch";
    if touch && state.buttons == 0 {
        // Touch pointers have no hover state. Their position is retained so the first contact
        // starts at the requested point, without manufacturing pre-contact pointermove events.
        state.x = x;
        state.y = y;
        state.hover_target = None;
        realm.publish_hover_target(None);
        return Ok(());
    }
    realm.note_pointer_modality();
    let old = state.hover_target;
    let dx = x - state.x;
    let dy = y - state.y;
    if old != Some(target) {
        // The hit-test result is observable from pointer transition listeners,
        // including listeners for the outgoing target.
        realm.publish_hover_target((!touch).then_some(Some(target)).flatten());
        if state.buttons != 0 {
            let active = realm.active_targets_for_pointer(target);
            realm.publish_active_targets(active[0], active[1]);
        }
        if let Some(previous) = old.filter(|node| connected_pointer_target(realm, *node)) {
            let related = realm.wrap(ctx, target);
            dispatch_pointer_state_event(
                ctx,
                realm,
                previous,
                "pointerout",
                x,
                y,
                -1,
                state.buttons,
                0,
                0.0,
                true,
                true,
                related.clone(),
                modifiers,
                dx,
                dy,
                state.properties,
            )?;
            dispatch_state_leave_sequence(
                ctx,
                realm,
                previous,
                Some(target),
                "pointerleave",
                x,
                y,
                state.buttons,
                true,
                related.clone(),
                modifiers,
                dx,
                dy,
                state.properties,
            )?;
            if !touch {
                dispatch_mouse_state_event(
                    ctx,
                    realm,
                    previous,
                    "mouseout",
                    x,
                    y,
                    -1,
                    state.buttons,
                    0,
                    true,
                    true,
                    related.clone(),
                    modifiers,
                    dx,
                    dy,
                )?;
                dispatch_state_leave_sequence(
                    ctx,
                    realm,
                    previous,
                    Some(target),
                    "mouseleave",
                    x,
                    y,
                    state.buttons,
                    false,
                    related,
                    modifiers,
                    dx,
                    dy,
                    state.properties,
                )?;
            }
        }
        if connected_pointer_target(realm, target) {
            let related = old
                .filter(|node| connected_pointer_target(realm, *node))
                .map_or(Value::Null, |node| realm.wrap(ctx, node));
            dispatch_pointer_state_event(
                ctx,
                realm,
                target,
                "pointerover",
                x,
                y,
                -1,
                state.buttons,
                0,
                0.0,
                true,
                true,
                related.clone(),
                modifiers,
                dx,
                dy,
                state.properties,
            )?;
            dispatch_state_enter_sequence(
                ctx,
                realm,
                target,
                old,
                "pointerenter",
                x,
                y,
                state.buttons,
                true,
                related.clone(),
                modifiers,
                dx,
                dy,
                state.properties,
            )?;
            if !touch {
                dispatch_mouse_state_event(
                    ctx,
                    realm,
                    target,
                    "mouseover",
                    x,
                    y,
                    -1,
                    state.buttons,
                    0,
                    true,
                    true,
                    related.clone(),
                    modifiers,
                    dx,
                    dy,
                )?;
                dispatch_state_enter_sequence(
                    ctx,
                    realm,
                    target,
                    old,
                    "mouseenter",
                    x,
                    y,
                    state.buttons,
                    false,
                    related,
                    modifiers,
                    dx,
                    dy,
                    state.properties,
                )?;
            }
        }
    }
    if connected_pointer_target(realm, target) {
        dispatch_pointer_state_event(
            ctx,
            realm,
            target,
            "pointermove",
            x,
            y,
            -1,
            state.buttons,
            0,
            if state.buttons == 0 {
                0.0
            } else if state.pressure > 0.0 {
                state.pressure
            } else {
                0.5
            },
            true,
            true,
            Value::Null,
            modifiers,
            dx,
            dy,
            state.properties,
        )?;
        if !touch && connected_pointer_target(realm, target) {
            dispatch_mouse_state_event(
                ctx,
                realm,
                target,
                "mousemove",
                x,
                y,
                -1,
                state.buttons,
                0,
                true,
                true,
                Value::Null,
                modifiers,
                dx,
                dy,
            )?;
        }
    }
    state.x = x;
    state.y = y;
    state.hover_target = connected_pointer_target(realm, target).then_some(target);
    realm.publish_hover_target((!touch).then_some(state.hover_target).flatten());
    if state.buttons != 0 && state.hover_target.is_none() {
        realm.publish_active_targets(None, None);
    }
    Ok(())
}

pub(crate) fn trusted_pointer_down(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    state: &mut TrustedPointerState,
    target: NodeId,
    modifiers: super::ui_events::UserAgentModifiers,
) -> OpResult<()> {
    if state.buttons & 1 != 0 {
        return Err(OpError::new(
            "InvalidStateError",
            "primary pointer button is already down",
        ));
    }
    if !connected_pointer_target(realm, target) {
        return Err(OpError::new(
            "InvalidStateError",
            "pointer target is not connected",
        ));
    }
    realm.note_pointer_modality();
    state.buttons |= 1;
    state.down_target = None;
    state.compatibility_mouse_allowed = false;
    state.down_allowed = false;
    let active = realm.active_targets_for_pointer(target);
    realm.publish_active_targets(active[0], active[1]);
    realm.mark_user_activation();
    let touch = state.properties.pointer_type == "touch";
    if touch && state.hover_target.is_none() {
        dispatch_pointer_state_event(
            ctx,
            realm,
            target,
            "pointerover",
            state.x,
            state.y,
            -1,
            state.buttons,
            0,
            0.0,
            true,
            true,
            Value::Null,
            modifiers,
            0.0,
            0.0,
            state.properties,
        )?;
        dispatch_state_enter_sequence(
            ctx,
            realm,
            target,
            None,
            "pointerenter",
            state.x,
            state.y,
            state.buttons,
            true,
            Value::Null,
            modifiers,
            0.0,
            0.0,
            state.properties,
        )?;
        state.hover_target = Some(target);
        realm.publish_hover_target(None);
        if !connected_pointer_target(realm, target) {
            state.buttons &= !1;
            state.hover_target = None;
            realm.publish_active_targets(None, None);
            return Ok(());
        }
    }
    super::dialog_popover::record_popover_pointerdown_target(ctx, realm, target)?;
    let pointer_allowed = dispatch_pointer_state_event(
        ctx,
        realm,
        target,
        "pointerdown",
        state.x,
        state.y,
        0,
        state.buttons,
        1,
        if state.pressure > 0.0 {
            state.pressure
        } else {
            0.5
        },
        true,
        true,
        Value::Null,
        modifiers,
        0.0,
        0.0,
        state.properties,
    )?;
    if !connected_pointer_target(realm, target) {
        state.down_target = None;
        state.down_allowed = false;
        realm.publish_active_targets(None, None);
        return Ok(());
    }
    let mouse_allowed = if pointer_allowed && !touch {
        realm.mark_user_activation();
        dispatch_mouse_state_event(
            ctx,
            realm,
            target,
            "mousedown",
            state.x,
            state.y,
            0,
            state.buttons,
            1,
            true,
            true,
            Value::Null,
            modifiers,
            0.0,
            0.0,
        )?
    } else {
        false
    };
    if !connected_pointer_target(realm, target) {
        state.down_target = None;
        state.down_allowed = false;
        state.compatibility_mouse_allowed = false;
        realm.publish_active_targets(None, None);
        return Ok(());
    }
    if mouse_allowed && connected_pointer_target(realm, target) {
        realm.focus_from_pointer(ctx, Some(target))?;
    }
    state.down_target = Some(target);
    state.compatibility_mouse_allowed = pointer_allowed && !touch;
    // PointerEvent.preventDefault() suppresses compatibility mouse events, but it does not
    // suppress the later click. Keep this separate from target continuity.
    state.down_allowed = true;
    Ok(())
}

pub(crate) fn trusted_pointer_up(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    state: &mut TrustedPointerState,
    target: NodeId,
    modifiers: super::ui_events::UserAgentModifiers,
) -> OpResult<()> {
    trusted_pointer_up_with_click_detail(ctx, realm, state, target, modifiers, state.click_detail)
        .map(|_| ())
}

pub(crate) fn trusted_pointer_up_with_click_detail(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    state: &mut TrustedPointerState,
    target: NodeId,
    modifiers: super::ui_events::UserAgentModifiers,
    click_detail: i32,
) -> OpResult<bool> {
    if state.buttons & 1 == 0 {
        return Err(OpError::new(
            "InvalidStateError",
            "primary pointer button is not down",
        ));
    }
    if !connected_pointer_target(realm, target) {
        // A down listener may synchronously adopt/remove its target. The already-started native
        // interaction ends without dispatching a stale release to the old document.
        super::dialog_popover::clear_popover_pointerdown_target(realm);
        state.buttons &= !1;
        state.down_target = None;
        state.compatibility_mouse_allowed = false;
        state.down_allowed = false;
        realm.publish_active_targets(None, None);
        if state.properties.pointer_type == "touch" {
            state.hover_target = None;
            realm.publish_hover_target(None);
        }
        return Ok(false);
    }
    let was_allowed = state.down_allowed;
    let compatibility_mouse_allowed = state.compatibility_mouse_allowed;
    let down_target = state.down_target;
    state.buttons &= !1;
    realm.publish_active_targets(None, None);
    state.down_target = None;
    state.compatibility_mouse_allowed = false;
    state.down_allowed = false;
    if let Err(error) = super::dialog_popover::dismiss_popovers_for_pointer_up(ctx, realm, target) {
        super::dialog_popover::clear_popover_pointerdown_target(realm);
        return Err(error);
    }
    if !connected_pointer_target(realm, target) {
        // Closing popovers can run author callbacks before pointerup. If one adopts/removes the
        // release target, abandon the stale release and clear any document endpoint left by the
        // no-open-popover early return.
        super::dialog_popover::clear_popover_pointerdown_target(realm);
        if state.properties.pointer_type == "touch" {
            state.hover_target = None;
            realm.publish_hover_target(None);
        }
        return Ok(false);
    }
    dispatch_pointer_state_event(
        ctx,
        realm,
        target,
        "pointerup",
        state.x,
        state.y,
        0,
        state.buttons,
        1,
        0.0,
        true,
        true,
        Value::Null,
        modifiers,
        0.0,
        0.0,
        state.properties,
    )?;
    if compatibility_mouse_allowed && connected_pointer_target(realm, target) {
        dispatch_mouse_state_event(
            ctx,
            realm,
            target,
            "mouseup",
            state.x,
            state.y,
            0,
            state.buttons,
            1,
            true,
            true,
            Value::Null,
            modifiers,
            0.0,
            0.0,
        )?;
    }
    let touch = state.properties.pointer_type == "touch";
    let mut clicked = false;
    if was_allowed && down_target == Some(target) && connected_pointer_target(realm, target) {
        if !core_forms::is_disabled(realm.session.borrow().document(), target) {
            let click = super::ui_events::user_agent_pointer_event_with_properties(
                ctx,
                "click",
                pointer_view(realm),
                state.x,
                state.y,
                0,
                0,
                if touch { 0 } else { click_detail },
                0.0,
                true,
                true,
                Value::Null,
                modifiers,
                0.0,
                0.0,
                state.properties,
            )?;
            click_with_event(
                ctx,
                realm,
                &DomEventTarget::node(realm, target),
                target,
                click,
                true,
                true,
                false,
            )?;
            clicked = true;
        }
    }
    if touch {
        dispatch_touch_pointer_out(ctx, realm, state, modifiers)?;
    }
    Ok(clicked)
}

fn dispatch_touch_pointer_out(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    state: &mut TrustedPointerState,
    modifiers: super::ui_events::UserAgentModifiers,
) -> OpResult<()> {
    let previous = state.hover_target;
    state.hover_target = None;
    realm.publish_hover_target(None);
    if let Some(previous) = previous.filter(|node| connected_pointer_target(realm, *node)) {
        dispatch_pointer_state_event(
            ctx,
            realm,
            previous,
            "pointerout",
            state.x,
            state.y,
            -1,
            0,
            0,
            0.0,
            true,
            true,
            Value::Null,
            modifiers,
            0.0,
            0.0,
            state.properties,
        )?;
        dispatch_state_leave_sequence(
            ctx,
            realm,
            previous,
            None,
            "pointerleave",
            state.x,
            state.y,
            0,
            true,
            Value::Null,
            modifiers,
            0.0,
            0.0,
            state.properties,
        )?;
    }
    Ok(())
}

fn pointer_view(realm: &DomRealm) -> Value {
    realm
        .window_wrapper
        .borrow()
        .as_ref()
        .and_then(WeakValue::upgrade)
        .unwrap_or(Value::Null)
}

fn connected_pointer_target(realm: &Rc<DomRealm>, node: NodeId) -> bool {
    let session = realm.session.borrow();
    let document = session.document();
    matches!(
        document.kind(node),
        Ok(lumen_html::NodeKind::Element { .. })
    ) && super::script_loading::is_connected(document, node)
}

#[allow(clippy::too_many_arguments)]
fn dispatch_pointer_state_event(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    kind: &str,
    x: f64,
    y: f64,
    button: i16,
    buttons: u16,
    detail: i32,
    pressure: f64,
    bubbles: bool,
    cancelable: bool,
    related: Value,
    modifiers: super::ui_events::UserAgentModifiers,
    movement_x: f64,
    movement_y: f64,
    properties: super::ui_events::UserAgentPointerProperties,
) -> OpResult<bool> {
    let event = super::ui_events::user_agent_pointer_event_with_properties(
        ctx,
        kind,
        pointer_view(realm),
        x,
        y,
        button,
        buttons,
        detail,
        pressure,
        bubbles,
        cancelable,
        related,
        modifiers,
        movement_x,
        movement_y,
        properties,
    )?;
    dispatch_pointer_sequence_event(ctx, realm, node, event)
}

#[allow(clippy::too_many_arguments)]
fn dispatch_mouse_state_event(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    kind: &str,
    x: f64,
    y: f64,
    button: i16,
    buttons: u16,
    detail: i32,
    bubbles: bool,
    cancelable: bool,
    related: Value,
    modifiers: super::ui_events::UserAgentModifiers,
    movement_x: f64,
    movement_y: f64,
) -> OpResult<bool> {
    let event = super::ui_events::user_agent_mouse_event_with_state(
        ctx,
        kind,
        pointer_view(realm),
        x,
        y,
        button,
        buttons,
        detail,
        bubbles,
        cancelable,
        related,
        modifiers,
        movement_x,
        movement_y,
    )?;
    dispatch_pointer_sequence_event(ctx, realm, node, event)
}

fn pointer_ancestor_path(realm: &Rc<DomRealm>, target: NodeId) -> OpResult<Vec<NodeId>> {
    let session = realm.session_handle();
    let session = session.borrow();
    let document = session.document();
    let mut ancestors = Vec::new();
    let mut current = Some(target);
    while let Some(node) = current {
        if ancestors.len() >= 1024 {
            return Err(OpError::new(
                "RangeError",
                "pointer event path limit exceeded",
            ));
        }
        if matches!(
            document.kind(node),
            Ok(lumen_html::NodeKind::Element { .. })
        ) {
            ancestors.push(node);
        }
        if node == document.root() {
            break;
        }
        current = document.shadow_including_parent(node).map_err(dom_error)?;
    }
    Ok(ancestors)
}

fn first_common_path_node(first: &[NodeId], second: &[NodeId]) -> Option<NodeId> {
    first.iter().copied().find(|node| second.contains(node))
}

#[allow(clippy::too_many_arguments)]
fn dispatch_state_enter_sequence(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    target: NodeId,
    previous: Option<NodeId>,
    kind: &str,
    x: f64,
    y: f64,
    buttons: u16,
    pointer: bool,
    related: Value,
    modifiers: super::ui_events::UserAgentModifiers,
    movement_x: f64,
    movement_y: f64,
    properties: super::ui_events::UserAgentPointerProperties,
) -> OpResult<()> {
    let target_path = pointer_ancestor_path(realm, target)?;
    let previous_path = previous
        .filter(|node| connected_pointer_target(realm, *node))
        .map(|node| pointer_ancestor_path(realm, node))
        .transpose()?
        .unwrap_or_default();
    let common = first_common_path_node(&target_path, &previous_path);
    let mut entry = target_path
        .into_iter()
        .take_while(|node| Some(*node) != common)
        .collect::<Vec<_>>();
    entry.reverse();
    for node in entry {
        if !connected_pointer_target(realm, target) || !connected_pointer_target(realm, node) {
            break;
        }
        if pointer {
            dispatch_pointer_state_event(
                ctx,
                realm,
                node,
                kind,
                x,
                y,
                -1,
                buttons,
                0,
                if buttons == 0 { 0.0 } else { 0.5 },
                false,
                false,
                related.clone(),
                modifiers,
                movement_x,
                movement_y,
                properties,
            )?;
        } else {
            dispatch_mouse_state_event(
                ctx,
                realm,
                node,
                kind,
                x,
                y,
                -1,
                buttons,
                0,
                false,
                false,
                related.clone(),
                modifiers,
                movement_x,
                movement_y,
            )?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn dispatch_state_leave_sequence(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    target: NodeId,
    next: Option<NodeId>,
    kind: &str,
    x: f64,
    y: f64,
    buttons: u16,
    pointer: bool,
    related: Value,
    modifiers: super::ui_events::UserAgentModifiers,
    movement_x: f64,
    movement_y: f64,
    properties: super::ui_events::UserAgentPointerProperties,
) -> OpResult<()> {
    let target_path = pointer_ancestor_path(realm, target)?;
    let next_path = next
        .filter(|node| connected_pointer_target(realm, *node))
        .map(|node| pointer_ancestor_path(realm, node))
        .transpose()?
        .unwrap_or_default();
    let common = first_common_path_node(&target_path, &next_path);
    for node in target_path
        .into_iter()
        .take_while(|node| Some(*node) != common)
    {
        if !connected_pointer_target(realm, node) {
            break;
        }
        if pointer {
            dispatch_pointer_state_event(
                ctx,
                realm,
                node,
                kind,
                x,
                y,
                -1,
                buttons,
                0,
                if buttons == 0 { 0.0 } else { 0.5 },
                false,
                false,
                related.clone(),
                modifiers,
                movement_x,
                movement_y,
                properties,
            )?;
        } else {
            dispatch_mouse_state_event(
                ctx,
                realm,
                node,
                kind,
                x,
                y,
                -1,
                buttons,
                0,
                false,
                false,
                related.clone(),
                modifiers,
                movement_x,
                movement_y,
            )?;
        }
    }
    Ok(())
}

fn dispatch_pointer_sequence_event(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    event_value: Value,
) -> OpResult<bool> {
    let receiver = realm.wrap(ctx, node);
    let event = lumen::embed::JsObject::from_value(event_value)
        .ok_or_else(|| OpError::type_error("user-agent input event is not an object"))?;
    super::events::dispatch_user_agent_event(ctx, lumen_bind::This(receiver), event)
}

struct ControlActivation {
    element: Value,
    event: Value,
    previous_checked: bool,
    previous_indeterminate: bool,
    previous_radio: Option<Value>,
    prepared: bool,
}

pub(crate) fn activation_behavior(ctx: &mut Ctx, receiver: &Value, event: &Value) -> OpResult<Option<lumen_host::events::PreparedActivation>> {
    let Some((realm, node)) = ctx.with_instance::<DomNode, _>(receiver, |node| (node.realm.clone(), node.id)).ok() else { return Ok(None); };
    let name = {
        let session = realm.session.borrow();
        html_local_name(session.document(), node).map(str::to_owned)
    };
    if !matches!(name.as_deref(), Some("input" | "button" | "summary" | "label")) { return Ok(None); }
    if ctx.with_instance::<super::ui_events::DomMouseEvent, _>(event, |_| ()).is_err()
        || !ctx.with_instance::<DomEvent, _>(event, |event| event.kind()=="click").unwrap_or(false) { return Ok(None); }
    Ok(Some(Box::new(ControlActivation {
        element: receiver.clone(), event: event.clone(), previous_checked:false,
        previous_indeterminate:false, previous_radio:None, prepared:false,
    })))
}

impl lumen_host::events::ActivationBehavior for ControlActivation {
    fn pre_activate(&mut self, ctx: &mut Ctx) -> OpResult<()> {
        let (realm, node) = ctx.with_instance::<DomNode, _>(&self.element, |node| (node.realm.clone(), node.id))?;
        let action = click_activation(realm.session.borrow().document(), node);
        self.previous_checked = checked(&realm, node)?;
        self.previous_indeterminate = indeterminate(&realm.forms.borrow(), node);
        if let ClickActivation::Checkable { radio:true } = action {
            let mut members = core_forms::radio_group_members(realm.session.borrow().document(), node);
            members.push(node);
            for member in members {
                if checked(&realm, member)? {
                    self.previous_radio = Some(realm.wrap(ctx, member));
                    break;
                }
            }
        }
        self.prepared = true;
        if let ClickActivation::Checkable { radio } = action {
            set_checked(&realm, &mut realm.forms.borrow_mut(), node, radio || !self.previous_checked)?;
            if !radio { set_indeterminate(&mut realm.forms.borrow_mut(), node, false); }
        }
        Ok(())
    }

    fn finish(self: Box<Self>, ctx: &mut Ctx, accepted: bool) -> OpResult<()> {
        let (realm, node) = ctx.with_instance::<DomNode, _>(&self.element, |node| (node.realm.clone(), node.id))?;
        let (name, action, form, connected, disabled) = {
            let session = realm.session.borrow();
            let document = session.document();
            if document.kind(node).is_err() { return Ok(()); }
            (html_local_name(document,node).unwrap_or("").to_owned(),
                click_activation(document,node), core_forms::form_owner(document,node),
                document.is_connected_element(node), core_forms::is_disabled(document,node))
        };
        if !accepted {
            if !self.prepared { return Ok(()); }
            // Cancellation is defined by the input's current type and current
            // radio group, not the type/group that existed before listeners.
            match action {
                ClickActivation::Checkable { radio:false } => {
                    set_checked(&realm, &mut realm.forms.borrow_mut(),node,self.previous_checked)?;
                    set_indeterminate(&mut realm.forms.borrow_mut(),node,self.previous_indeterminate);
                }
                ClickActivation::Checkable { radio:true } => {
                    let previous = self.previous_radio.as_ref().and_then(|value|
                        ctx.with_instance::<DomNode,_>(value,|node|(node.realm.clone(),node.id)).ok());
                    let previous = previous.filter(|(owner,previous)| Rc::ptr_eq(owner,&realm) &&
                        (*previous==node || core_forms::radio_group_members(realm.session.borrow().document(),node).contains(previous)));
                    if let Some((_,previous))=previous {
                        set_checked(&realm,&mut realm.forms.borrow_mut(),previous,true)?;
                    } else { set_checked(&realm,&mut realm.forms.borrow_mut(),node,false)?; }
                }
                _=>{}
            }
            return Ok(());
        }
        if name=="summary" {
            realm.session.borrow_mut().document_mut().activate_summary(node).map_err(dom_error)?;
            return Ok(());
        }
        if name=="label" {
            let event_target=ctx.with_instance::<DomEvent,_>(&self.event,|event|event.target_for_retarget())?;
            let event_target=ctx.with_instance::<DomNode,_>(&event_target,|target|(target.realm.clone(),target.id)).ok();
            let control = {
                let session=realm.session.borrow();
                let document=session.document();
                let control=lumen_html::labels::label_control(document,node).map_err(dom_error)?;
                let mut blocked=false;
                if let Some((owner,target))=&event_target {
                    if Rc::ptr_eq(owner,&realm) {
                        let mut current=Some(*target);
                        while let Some(candidate)=current {
                            if candidate==node { break; }
                            if Some(candidate)==control || document.is_interactive_content(candidate) { blocked=true;break; }
                            current=document.composed_parent(candidate).map_err(dom_error)?;
                        }
                    }
                }
                control.filter(|control|!blocked && !core_forms::is_disabled(document,*control))
            };
            if let Some(control)=control {
                // The platform's label policy focuses the shared associated
                // control and sends a real synthetic click, including FACE.
                if realm.focusable_node(control)? { realm.focus(ctx,Some(control))?; }
                click_element(ctx,&realm,&DomEventTarget::node(&realm,control),control)?;
            }
            return Ok(());
        }
        let event_target = ctx.with_instance::<DomEvent, _>(&self.event, |event| event.target_for_retarget())?;
        if name == "button" {
            return super::invokers::button_activation(ctx, self.element.clone(), event_target);
        }
        match action {
            ClickActivation::Checkable { .. } if connected => {
                realm.dispatch_user_agent(ctx,node,"input",true,false,&[])?;
                realm.dispatch_user_agent(ctx,node,"change",true,false,&[])?;
            }
            ClickActivation::Submit | ClickActivation::Reset if !disabled => {
                if !realm.browsing_context().is_some_and(|context| browsing_context::is_active_document(&context,&realm)) { return Ok(()); }
                if let Some(form)=form {
                    match action {
                        ClickActivation::Submit=>{ realm.submit_form(ctx,form,Some(node))?; }
                        ClickActivation::Reset=>{ reset_form(ctx,&realm,form,&realm.forms)?; }
                        _=>{}
                    }
                }
            }
            _=>{}
        }
        if name == "input" { super::invokers::input_popover_activation(ctx, self.element.clone(), event_target)?; }
        Ok(())
    }
}

fn click_with_event(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    target: &DomEventTarget,
    node: NodeId,
    event_value: Value,
    trusted: bool,
    allow_non_html: bool,
    set_click_in_progress: bool,
) -> OpResult<()> {
    {
        let session=realm.session.borrow();
        let document=session.document();
        let name=html_local_name(document,node);
        if name.is_none() && !allow_non_html { return Err(OpError::type_error("click requires an HTML element")); }
        if matches!(name,Some("input"|"button"|"select"|"textarea")) && core_forms::is_disabled(document,node) { return Ok(()); }
    }
    if set_click_in_progress && !target.try_begin_click() { return Ok(()); }
    struct ClickScope<'a> { target:&'a DomEventTarget, enabled:bool }
    impl Drop for ClickScope<'_> { fn drop(&mut self) { if self.enabled { self.target.end_click(); } } }
    let _scope=ClickScope {target,enabled:set_click_in_progress};
    let receiver=realm.wrap(ctx,node);
    let event=lumen::embed::JsObject::from_value(event_value)
        .ok_or_else(||OpError::type_error("click event constructor returned a non-object"))?;
    if trusted { events::dispatch_user_agent_event(ctx,lumen_bind::This(receiver),event)?; }
    else { events::dispatch_event(ctx,lumen_bind::This(receiver),event)?; }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    #[test]
    fn specification_window_shared_activation_pre_cancel_post_and_path_arbitration() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<!doctype html><form id=form><input id=box type=checkbox><input id=old type=radio name=g checked><input id=radio type=radio name=g><button id=submit><span id=leaf></span></button></form><details id=details><summary id=summary><input id=nested type=checkbox></summary></details><label id=label for=box><span id=labelleaf></span><button id=interactive type=button></button></label>", 256).unwrap();
        realm.set_document_url("https://activation.test/page");
        let value = engine.eval_value(r#"(() => {
            const check=(ok,message)=>{if(!ok)throw new Error(message);};
            const box=document.getElementById('box'),radio=document.getElementById('radio'),old=document.getElementById('old');
            let input=0,change=0;
            box.addEventListener('input',event=>{check(event.isTrusted && event.composed,'UA input metadata');input++;});
            box.addEventListener('change',event=>{check(event.isTrusted && !event.composed,'UA change metadata');change++;});
            box.indeterminate=true;
            box.onclick=event=>{check(box.checked && !box.indeterminate,'preactivation precedes listeners');event.preventDefault();};
            const canceled=new MouseEvent('click',{bubbles:true,cancelable:true});
            check(!box.dispatchEvent(canceled) && !box.checked && box.indeterminate,'author dispatch cancellation restores state');
            check(input===0 && change===0,'canceled activation has no post events');
            box.onclick=null;box.dispatchEvent(new MouseEvent('click',{bubbles:true,cancelable:true}));
            check(box.checked && input===1 && change===1,'accepted author MouseEvent performs native activation');
            radio.onclick=event=>{check(radio.checked && !old.checked,'radio group preactivation');event.preventDefault();};
            radio.click();check(old.checked && !radio.checked,'canceled radio restores actual prior group member');
            radio.onclick=event=>{radio.name='different';event.preventDefault();};
            radio.click();check(!radio.checked,'changed group cannot restore old radio into a different group');
            box.onclick=event=>{box.type='text';event.preventDefault();};
            box.click();check(!box.checked,'canceled activation examines the current input type');
            box.type='checkbox';box.onclick=null;box.disabled=true;box.click();
            check(!box.checked,'HTMLElement click bails for disabled form control');
            box.dispatchEvent(new MouseEvent('click',{bubbles:true,cancelable:true}));
            check(box.checked,'author dispatch still performs checkbox preactivation');
            box.disabled=false;
            const details=document.getElementById('details'),nested=document.getElementById('nested');
            nested.dispatchEvent(new MouseEvent('click',{bubbles:true,cancelable:true}));
            check(nested.checked && !details.open,'nearest input activation suppresses summary activation');
            document.getElementById('summary').dispatchEvent(new MouseEvent('click',{bubbles:true,cancelable:true}));
            check(details.open,'summary action runs through the common dispatcher');
            let submissions=0;document.getElementById('form').onsubmit=event=>{submissions++;event.preventDefault();};
            document.getElementById('leaf').dispatchEvent(new MouseEvent('click',{bubbles:true,cancelable:true}));
            check(submissions===1,'ancestor submit button wins the actual event path');
            document.getElementById('interactive').dispatchEvent(new MouseEvent('click',{bubbles:true,cancelable:true}));
            check(box.checked,'interactive descendant prevents label forwarding');
            document.getElementById('labelleaf').dispatchEvent(new MouseEvent('click',{bubbles:true,cancelable:true}));
            check(!box.checked,'label forwards through core control association and common activation');
            return true;
        })()"#).unwrap().unwrap_or_else(|exception| {
            let message = engine.ctx().member_get(&exception, "stack").ok()
                .and_then(|value| engine.ctx().coerce_string(&value).ok()).map(|value|value.to_string()).unwrap_or_default();
            panic!("shared activation guard: {message}");
        });
        assert!(matches!(value, Value::Bool(true)));
    }
    use lumen_runtime::Runtime;

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
    fn details_summary_click_respects_first_child_interactive_guards_and_cancellation() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), r#"<details id='d'><p>before</p><summary id='first'>
            <span id='plain'>plain</span><button id='button' type='button'>button</button>
            <a id='link' href='#test'>link</a><input id='input'></summary>
            <summary id='second'>second</summary></details>"#, 128).unwrap();
        assert!(matches!(eval(&mut engine, r#"
            globalThis.summaryEvents=[];
            const details=document.getElementById('d'), first=document.getElementById('first');
            details.addEventListener('toggle',e=>summaryEvents.push(
                e instanceof ToggleEvent&&e.isTrusted&&e.oldState==='closed'&&e.newState==='open'));
            document.getElementById('second').click();
            document.getElementById('button').click();
            document.getElementById('link').click();
            document.getElementById('input').click();
            if(details.open) throw new Error('interactive or nonprimary summary toggled');
            const cancel=e=>e.preventDefault();
            first.addEventListener('click',cancel);
            document.getElementById('plain').click();
            if(details.open) throw new Error('canceled summary toggled');
            first.removeEventListener('click',cancel);
            document.getElementById('plain').click();
            details.open&&summaryEvents.length===0
        "#), Value::Bool(true)));
        assert!(crate::scheduling::run_tasks(&mut engine, 16).is_empty());
        assert!(matches!(eval(&mut engine, "summaryEvents.length===1&&summaryEvents[0]"), Value::Bool(true)));
    }

    #[test]
    fn details_summary_trusted_pointer_and_listener_reselection_share_native_state() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(),
            "<details id='d'><summary id='s'><span id='p'>plain</span></summary></details><details id='other'></details>",
            128).unwrap();
        let node = {
            let session = realm.session.borrow();
            lumen_html::selector::query_selector(session.document(), session.document().root(), "#p").unwrap().unwrap()
        };
        assert!(matches!(eval(&mut engine, r#"
            globalThis.pointerSummaryClicks=[];globalThis.pointerSummaryToggles=[];
            document.getElementById('s').addEventListener('click',e=>pointerSummaryClicks.push(e.isTrusted));
            document.getElementById('s').addEventListener('click',e=>e.preventDefault(),{once:true});
            for(const id of ['d','other']) document.getElementById(id).addEventListener('toggle',e=>pointerSummaryToggles.push(e.isTrusted));
            true
        "#), Value::Bool(true)));
        trusted_pointer_click(engine.ctx(), &realm, node, 1.0, 1.0).unwrap();
        assert!(matches!(eval(&mut engine, "!document.getElementById('d').open&&pointerSummaryClicks.length===1&&pointerSummaryClicks[0]"), Value::Bool(true)));
        assert!(crate::scheduling::run_tasks(&mut engine, 16).is_empty());
        assert!(matches!(eval(&mut engine, "pointerSummaryToggles.length===0"), Value::Bool(true)));
        trusted_pointer_click(engine.ctx(), &realm, node, 1.0, 1.0).unwrap();
        assert!(matches!(eval(&mut engine, "document.getElementById('d').open&&pointerSummaryClicks.length===2&&pointerSummaryClicks[1]"), Value::Bool(true)));
        assert!(crate::scheduling::run_tasks(&mut engine, 16).is_empty());
        assert!(matches!(eval(&mut engine, r#"
            const summary=document.getElementById('s');
            summary.addEventListener('click',()=>document.getElementById('other').appendChild(summary),{once:true});
            summary.click();
            document.getElementById('d').open&&document.getElementById('other').open&&pointerSummaryClicks[2]===false
        "#), Value::Bool(true)));
        assert!(crate::scheduling::run_tasks(&mut engine, 16).is_empty());
        assert!(matches!(eval(&mut engine, "pointerSummaryToggles.length===2&&pointerSummaryToggles.every(Boolean)"), Value::Bool(true)));
    }

    #[test]
    fn single_select_selected_option_cache_is_constant_space_and_versioned() {
        let mut document = lumen_html::html::parse(
            "<select id='s'><option>First</option><option selected>Second</option><option>Third</option></select>",
            32,
        )
        .unwrap();
        let select = lumen_html::selector::query_selector(&document, document.root(), "#s")
            .unwrap()
            .unwrap();
        let option_count = core_forms::select_option_count(&document, select).unwrap();
        let last = core_forms::select_option_at(&document, select, 1)
            .unwrap()
            .unwrap();
        let cache = Cell::new(None);
        let reads = Cell::new(0usize);

        for _ in 0..option_count {
            assert_eq!(
                cached_single_select_option_by(&document, select, &cache, 0, |_| {
                    reads.set(reads.get() + 1);
                    None
                }),
                Some(Some(last))
            );
        }
        assert_eq!(reads.get(), option_count);

        // A new selectedness generation and a DOM mutation each force exactly
        // one fresh streaming resolution, while the cache retains no option
        // array between reads.
        assert_eq!(
            cached_single_select_option_by(&document, select, &cache, 1, |_| {
                reads.set(reads.get() + 1);
                None
            }),
            Some(Some(last))
        );
        assert_eq!(reads.get(), option_count * 2);

        let option = document
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: "option".into(),
                attributes: Vec::new(),
            })
            .unwrap();
        document.append(select, option).unwrap();
        assert_eq!(
            cached_single_select_option_by(&document, select, &cache, 1, |_| {
                reads.set(reads.get() + 1);
                None
            }),
            Some(Some(last))
        );
        assert_eq!(reads.get(), option_count * 2 + option_count + 1);
    }

    #[test]
    fn disabled_control_click_suppression_keeps_explicit_dispatch_and_legend_exception() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<fieldset disabled><legend><select id=exempt></select></legend><select id=inherited></select><textarea id=area></textarea></fieldset><select id=direct disabled></select><div id=ordinary disabled></div>",
            64,
        )
        .unwrap();
        let result = eval(
            &mut engine,
            r#"(() => {
                const ids = ['exempt', 'inherited', 'area', 'direct', 'ordinary'];
                const counts = {};
                for (const id of ids) {
                    counts[id] = 0;
                    const element = document.getElementById(id);
                    element.addEventListener('click', () => counts[id]++);
                    element.click();
                }
                if (counts.exempt !== 1 || counts.ordinary !== 1 ||
                    counts.inherited !== 0 || counts.area !== 0 || counts.direct !== 0)
                    return false;
                const area = document.getElementById('area');
                area.dispatchEvent(new Event('click', {bubbles: true}));
                if (counts.area !== 1) return false;
                document.querySelector('fieldset').disabled = false;
                area.click();
                return counts.area === 2;
            })()"#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    fn query_node(realm: &Rc<DomRealm>, selector_text: &str) -> NodeId {
        realm.with_session(|session| {
            selector::query_selector(session.document(), session.document().root(), selector_text)
                .expect("valid selector")
                .expect("fixture node exists")
        })
    }

    fn assert_true(engine: &mut Engine, source: &str, label: &str) {
        if !matches!(eval(engine, source), Value::Bool(true)) {
            panic!("{label}");
        }
    }

    #[test]
    fn trusted_pointer_descendant_focuses_ancestor_without_retargeting_events() {
        let mut runtime=Runtime::new();
        let engine=runtime.engine();
        let realm=crate::install(engine.ctx(),"<button id=before>before</button><table id=table tabindex=0><tr><td id=cell>cell</td></tr></table>",64).unwrap();
        assert_true(engine,r#"
            globalThis.before=document.getElementById('before');
            globalThis.table=document.getElementById('table');
            globalThis.cell=document.getElementById('cell');
            globalThis.trace=[];
            table.addEventListener('focus',e=>trace.push('focus:'+e.target.id));
            table.addEventListener('click',e=>trace.push('click:'+e.target.id));
            before.focus();cell.focus();document.activeElement===before
        "#,"programmatic focus on a nonfocusable descendant does not climb");
        let cell=query_node(&realm,"#cell");
        trusted_pointer_click(engine.ctx(),&realm,cell,10.0,10.0).unwrap();
        assert_true(engine,"document.activeElement===table && trace.join(',')==='focus:table,click:cell' && !table.matches(':focus-visible')","pointer focus climbs while click retains the hit descendant");
        assert_true(engine,"before.focus();trace=[];cell.addEventListener('mousedown',e=>e.preventDefault(),{once:true});true","canceling fixture installs");
        trusted_pointer_click(engine.ctx(),&realm,cell,10.0,10.0).unwrap();
        assert_true(engine,"document.activeElement===before && trace.join(',')==='click:cell'","canceling mousedown prevents ancestor focus but preserves click");
    }

    #[test]
    fn trusted_pointer_click_dispatches_trusted_pointer_mouse_and_focus_sequence() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = crate::install(
            engine.ctx(),
            "<main><button id=target>go</button></main>",
            64,
        )
        .unwrap();
        assert_true(
            engine,
            r#"(() => {
              const target = document.querySelector('#target');
              globalThis.inputTrace = [];
              globalThis.inputEvents = [];
              globalThis.activeAtPointerdown = false;
              const names = ['pointerover', 'pointerenter', 'mouseover', 'mouseenter',
                'pointermove', 'mousemove', 'pointerdown', 'mousedown', 'focus', 'focusin',
                'pointerup', 'mouseup', 'click'];
              for (const name of names) target.addEventListener(name, event => {
                inputTrace.push(name);
                inputEvents.push(event);
                if (name === 'pointerdown') activeAtPointerdown = navigator.userActivation.isActive;
              });
              return true;
            })()"#,
            "trusted pointer fixture installs",
        );
        let node = query_node(&realm, "#target");
        trusted_pointer_click(engine.ctx(), &realm, node, 17.0, 23.0)
            .expect("trusted pointer dispatch succeeds");
        assert_true(
            engine,
            r#"(() => {
              const expected = 'pointerover,pointerenter,mouseover,mouseenter,pointermove,mousemove,' +
                'pointerdown,mousedown,focus,focusin,pointerup,mouseup,click';
              const byType = type => inputEvents.find(event => event.type === type);
              return inputTrace.join(',') === expected &&
                inputEvents.every(event => event.isTrusted) &&
                inputEvents.filter(event => event.type.startsWith('pointer') ||
                  event.type.startsWith('mouse') || event.type === 'click')
                  .every(event => event.view === window) &&
                inputEvents.filter(event => event.type.startsWith('pointer') ||
                  ['mouseover', 'mouseenter', 'mousemove', 'mousedown', 'mouseup'].includes(event.type))
                  .every(event => event.clientX === 17 && event.clientY === 23) &&
                byType('pointerdown') instanceof PointerEvent && byType('pointerdown').button === 0 &&
                byType('pointerdown').buttons === 1 && byType('pointerup').buttons === 0 &&
                byType('click') instanceof PointerEvent && byType('click').detail === 1 &&
                byType('click').button === 0 && byType('click').buttons === 0 &&
                activeAtPointerdown && document.activeElement === document.querySelector('#target');
            })()"#,
            "trusted pointer event sequence, fields, activation, and focus are correct",
        );
    }

    #[test]
    fn trusted_pointer_publishes_hover_and_active_before_listeners_run() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = crate::install(
            engine.ctx(),
            "<button id=first>one</button><button id=second>two</button>",
            64,
        )
        .unwrap();
        assert_true(
            engine,
            r#"(() => {
              const first = document.getElementById('first');
              const second = document.getElementById('second');
              globalThis.hoverWasPublished = false;
              globalThis.transitionWasPublished = false;
              globalThis.activeWasPublished = false;
              globalThis.activeWasCleared = false;
              first.addEventListener('pointerover', () => {
                hoverWasPublished = first.matches(':hover');
              });
              first.addEventListener('pointerout', () => {
                transitionWasPublished = second.matches(':hover') && !first.matches(':hover');
              });
              second.addEventListener('pointerdown', () => {
                activeWasPublished = second.matches(':active');
              });
              second.addEventListener('pointerup', () => {
                activeWasCleared = !second.matches(':active');
              });
              return true;
            })()"#,
            "pointer interaction-state fixture installs",
        );
        let first = query_node(&realm, "#first");
        let second = query_node(&realm, "#second");
        let mut state = TrustedPointerState::default();
        let modifiers = super::super::ui_events::UserAgentModifiers::default();
        trusted_pointer_move(engine.ctx(), &realm, &mut state, first, 5.0, 5.0, modifiers)
            .expect("initial hover transition succeeds");
        trusted_pointer_move(
            engine.ctx(),
            &realm,
            &mut state,
            second,
            20.0,
            5.0,
            modifiers,
        )
        .expect("second hover transition succeeds");
        trusted_pointer_down(engine.ctx(), &realm, &mut state, second, modifiers)
            .expect("trusted pointer down succeeds");
        trusted_pointer_up(engine.ctx(), &realm, &mut state, second, modifiers)
            .expect("trusted pointer up succeeds");
        assert_true(
            engine,
            "hoverWasPublished && transitionWasPublished && activeWasPublished && activeWasCleared",
            "hover and active selectors reflect input before their event listeners",
        );
    }

    #[test]
    fn trusted_pointer_popover_light_dismiss_waits_for_matching_pointer_endpoints() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = crate::install(
            engine.ctx(),
            "<div id=popover popover=auto><button id=inside>inside</button></div><button id=outside>outside</button>",
            64,
        )
        .unwrap();
        assert_true(
            engine,
            r#"(() => {
              const popover = document.getElementById('popover');
              globalThis.pointerupOpenStates = [];
              document.getElementById('outside').addEventListener('pointerup', () =>
                pointerupOpenStates.push(popover.matches(':popover-open')));
              popover.showPopover();
              return popover.matches(':popover-open');
            })()"#,
            "Auto popover is open before trusted pointer input",
        );

        let inside = query_node(&realm, "#inside");
        let outside = query_node(&realm, "#outside");
        let modifiers = super::super::ui_events::UserAgentModifiers::default();
        let mut state = TrustedPointerState::default();
        trusted_pointer_move(
            engine.ctx(),
            &realm,
            &mut state,
            inside,
            5.0,
            5.0,
            modifiers,
        )
        .expect("pointer enters open Auto popover");
        trusted_pointer_down(engine.ctx(), &realm, &mut state, inside, modifiers)
            .expect("pointerdown inside popover records its endpoint");
        trusted_pointer_move(
            engine.ctx(),
            &realm,
            &mut state,
            outside,
            30.0,
            5.0,
            modifiers,
        )
        .expect("held pointer crosses the popover boundary");
        trusted_pointer_up(engine.ctx(), &realm, &mut state, outside, modifiers)
            .expect("mismatched pointerup does not light-dismiss");
        assert_true(
            engine,
            "document.getElementById('popover').matches(':popover-open') && pointerupOpenStates.length === 1 && pointerupOpenStates[0]",
            "dragging from inside to outside leaves the Auto popover open",
        );

        trusted_pointer_down(engine.ctx(), &realm, &mut state, outside, modifiers)
            .expect("matching outside pointerdown records a null endpoint");
        trusted_pointer_up(engine.ctx(), &realm, &mut state, outside, modifiers)
            .expect("matching outside pointerup light-dismisses before dispatch");
        assert_true(
            engine,
            "!document.getElementById('popover').matches(':popover-open') && pointerupOpenStates.length === 2 && !pointerupOpenStates[1]",
            "matching outside endpoints close before pointerup listeners run",
        );
    }

    #[test]
    fn canceled_trusted_pointerdown_suppresses_compatibility_mouse_events_but_keeps_click() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = crate::install(
            engine.ctx(),
            "<button id=target>go</button><button id=mouse-canceled>go</button>",
            64,
        )
        .unwrap();
        assert_true(
            engine,
            r#"(() => {
              const target = document.querySelector('#target');
              globalThis.canceledTrace = [];
              globalThis.activationWasSet = false;
              globalThis.mouseCanceledTrace = [];
              target.addEventListener('pointerdown', event => {
                activationWasSet = navigator.userActivation.isActive;
                event.preventDefault();
              });
              for (const name of ['mousedown', 'mouseup', 'pointerup', 'click'])
                target.addEventListener(name, event => canceledTrace.push([name, event.isTrusted]));
              const mouseCanceled = document.querySelector('#mouse-canceled');
              mouseCanceled.addEventListener('mousedown', event => event.preventDefault());
              for (const name of ['mousedown', 'mouseup', 'pointerup', 'click'])
                mouseCanceled.addEventListener(name, event => mouseCanceledTrace.push(name));
              return true;
            })()"#,
            "canceled pointerdown fixture installs",
        );
        let node = query_node(&realm, "#target");
        trusted_pointer_click(engine.ctx(), &realm, node, 2.0, 3.0)
            .expect("canceled pointerdown dispatch succeeds");
        assert_true(
            engine,
            r#"(() => {
              const names = canceledTrace.map(entry => entry[0]);
              return activationWasSet && !names.includes('mousedown') && !names.includes('mouseup') &&
                names.join(',') === 'pointerup,click' && canceledTrace.every(entry => entry[1]) &&
                document.activeElement === document.body;
            })()"#,
            "pointerdown cancellation suppresses mouse compatibility and focus, not click",
        );
        let node = query_node(&realm, "#mouse-canceled");
        trusted_pointer_click(engine.ctx(), &realm, node, 2.0, 3.0)
            .expect("canceled mousedown dispatch succeeds");
        assert_true(
            engine,
            "mouseCanceledTrace.join(',') === 'mousedown,pointerup,mouseup,click' && document.activeElement === document.body",
            "canceled mousedown suppresses focus but keeps pointerup, mouseup, and click",
        );
    }

    #[test]
    fn trusted_checkable_click_rolls_back_canceled_activation_and_untrusted_click_stays_distinct() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = crate::install(
            engine.ctx(),
            "<input id=check type=checkbox><input id=radio type=radio checked><input id=uncanceled type=checkbox><button id=plain>go</button>",
            96,
        )
        .unwrap();
        assert_true(
            engine,
            r#"(() => {
              const check = document.querySelector('#check');
              const radio = document.querySelector('#radio');
              const plain = document.querySelector('#plain');
              globalThis.checkDuringClick = false;
              globalThis.checkInputEvents = 0;
              globalThis.trustedClick = false;
              globalThis.checkActivationEvents = [];
              globalThis.plainClickTrust = [];
              check.addEventListener('click', event => {
                checkDuringClick = check.checked;
                event.preventDefault();
              });
              for (const name of ['input', 'change'])
                check.addEventListener(name, () => checkInputEvents++);
              radio.addEventListener('click', event => event.preventDefault());
              check.addEventListener('click', event => { trustedClick = event.isTrusted; });
              const uncanceled = document.querySelector('#uncanceled');
              for (const name of ['input', 'change'])
                uncanceled.addEventListener(name, event => checkActivationEvents.push([name, event.isTrusted]));
              plain.addEventListener('click', event => plainClickTrust.push(event.isTrusted));
              plain.addEventListener('pointerdown', () => plain.click());
              return true;
            })()"#,
            "checkable and reentrant click fixtures install",
        );

        for selector_text in ["#check", "#radio", "#plain"] {
            let node = query_node(&realm, selector_text);
            trusted_pointer_click(engine.ctx(), &realm, node, 4.0, 5.0)
                .expect("trusted click dispatch succeeds");
            if selector_text == "#check" {
                assert_true(
                    engine,
                    "checkDuringClick && !document.querySelector('#check').checked && checkInputEvents === 0 && trustedClick",
                    "canceled trusted checkbox activation rolls back without input/change",
                );
            } else if selector_text == "#radio" {
                assert_true(
                    engine,
                    "document.querySelector('#radio').checked",
                    "canceling an already checked radio click preserves its checkedness",
                );
            }
        }
        let node = query_node(&realm, "#uncanceled");
        trusted_pointer_click(engine.ctx(), &realm, node, 4.0, 5.0)
            .expect("uncanceled checkable activation succeeds");
        assert_true(
            engine,
            "document.querySelector('#uncanceled').checked && checkActivationEvents.map(entry=>entry.join(':')).join(',') === 'input:true,change:true'",
            "checkbox activation emits trusted input then change events",
        );
        assert_true(
            engine,
            "plainClickTrust.join(',') === 'false,true'",
            "trusted pointerdown may run a separate untrusted click without setting its guard",
        );

        let mut synthetic_runtime = Runtime::new();
        let synthetic_engine = synthetic_runtime.engine();
        crate::install(synthetic_engine.ctx(), "<button id=target>go</button>", 64).unwrap();
        assert_true(
            synthetic_engine,
            r#"(() => {
              const target = document.querySelector('#target');
              let pointerdowns = 0;
              let mousedowns = 0;
              let clicks = 0;
              target.addEventListener('pointerdown', () => pointerdowns++);
              target.addEventListener('mousedown', () => mousedowns++);
              target.addEventListener('click', event => {
                if (event.isTrusted || !(event instanceof PointerEvent)) return;
                clicks++;
                if (clicks === 1) target.click();
              });
              target.click();
              return clicks === 1 && pointerdowns === 0 && mousedowns === 0 &&
                document.activeElement === document.body;
            })()"#,
            "HTMLElement.click remains untrusted, non-focusing, and reentrancy guarded",
        );
    }

    #[test]
    fn trusted_pointer_click_stops_dispatch_after_listener_adopts_target() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = crate::install(engine.ctx(), "<button id=target>go</button>", 64).unwrap();
        assert_true(
            engine,
            r#"(() => {
              const target = document.querySelector('#target');
              globalThis.otherDocument = document.implementation.createHTMLDocument('other');
              globalThis.afterAdoption = [];
              target.addEventListener('pointerdown', () => otherDocument.body.appendChild(target));
              for (const name of ['pointerup', 'mouseup', 'click'])
                target.addEventListener(name, () => afterAdoption.push(name));
              return true;
            })()"#,
            "adoption fixture installs",
        );
        let node = query_node(&realm, "#target");
        trusted_pointer_click(engine.ctx(), &realm, node, 7.0, 8.0)
            .expect("listener adoption ends the stale pointer sequence");
        assert_true(
            engine,
            "document.querySelector('#target') === null && otherDocument.querySelector('#target') !== null && afterAdoption.length === 0",
            "no later input events target a node adopted during pointerdown",
        );
    }

    #[test]
    fn numeric_input_bindings_preserve_defaults_reset_and_event_provenance() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<form><input id='n' type='number' value='1.5' step='0.1'><input id='r' type='range'><input id='d' type='date'></form>",
            64,
        )
        .unwrap();
        let result = eval(
            &mut engine,
            r#"(() => {
            const n = document.querySelector('#n');
            const r = document.querySelector('#r');
            const d = document.querySelector('#d');
            const clean = document.createElement('input');
            clean.type = 'number';
            clean.defaultValue = '1';
            clean.stepUp(0);
            clean.defaultValue = '9';
            if (clean.value !== '1') return false;
            const overflow = document.createElement('input');
            overflow.type = 'number';
            overflow.min = '0';
            overflow.max = '10';
            overflow.step = '1e308';
            overflow.defaultValue = '0';
            overflow.stepUp(2);
            overflow.defaultValue = '5';
            if (overflow.value !== '0') return false;
            let events = 0;
            n.addEventListener('input', () => events++);
            n.addEventListener('change', () => events++);
            if (n.valueAsNumber !== 1.5 || !Number.isNaN(d.valueAsNumber)) return false;
            n.valueAsNumber = '2.5';
            n.stepUp();
            if (n.value !== '2.6' || n.defaultValue !== '1.5' || events !== 0) return false;
            n.stepDown('-2');
            if (n.value !== '2.6') return false;
            n.stepUp('2');
            if (n.value !== '2.8') return false;
            n.stepUp(4294967297);
            if (n.value !== '2.9') return false;
            n.valueAsNumber = NaN;
            r.valueAsNumber = NaN;
            d.valueAsNumber = 0;
            if (n.value !== '' || r.value !== '50' || d.value !== '1970-01-01') return false;
            document.querySelector('form').reset();
            return n.value === '1.5' && d.value === '' && events === 0;
        })()"#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn numeric_input_bindings_use_branded_errors_and_current_type_after_coercion() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        crate::install(engine.ctx(), "<input type='number' value='4'>", 32).unwrap();
        let result = eval(
            engine,
            r#"(() => {
            const n = document.querySelector('input');
            function invalid(operation) {
                try { operation(); } catch (e) {
                    return e instanceof DOMException && e.name === 'InvalidStateError';
                }
                return false;
            }
            n.step = 'any';
            if (!invalid(() => n.stepUp())) return false;
            n.type = 'text';
            if (!Number.isNaN(n.valueAsNumber) || !invalid(() => n.valueAsNumber = NaN)) return false;
            try { n.valueAsNumber = Infinity; return false; }
            catch (e) { if (!(e instanceof TypeError)) return false; }
            n.type = 'number';
            n.step = '1';
            if (!invalid(() => n.stepDown({valueOf() { n.type = 'text'; return 1; }}))) return false;
            n.type = 'number';
            return invalid(() => n.valueAsNumber = {valueOf() { n.type = 'text'; return 2; }});
        })()"#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn numeric_input_coercion_can_adopt_the_receiver_before_value_mutation() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<input type='number' value='1' step='0.5'>",
            32,
        )
        .unwrap();
        let result = eval(
            &mut engine,
            r#"(() => {
            const n = document.querySelector('input');
            const other = document.implementation.createHTMLDocument('destination');
            n.valueAsNumber = {valueOf() { other.adoptNode(n); return 2; }};
            if (n.ownerDocument !== other || n.value !== '2' || n.valueAsNumber !== 2) return false;
            n.stepUp({valueOf() { document.adoptNode(n); return 2; }});
            return n.ownerDocument === document && n.value === '3' && n.valueAsNumber === 3;
        })()"#,
        );
        assert!(matches!(result, Value::Bool(true)));
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
    fn validity_flags_are_independent_of_candidate_status() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<form><datalist><textarea id='inside' required></textarea></datalist><input id='disabled' required disabled><input id='readonly' required readonly><input id='checkbox' type='checkbox' required readonly><input id='range' type='range' required readonly><input id='button' type='button' required></form>",
            64,
        )
        .unwrap();
        let result = eval(
            &mut engine,
            r#"(()=>{
                const area=document.querySelector('#inside');
                let invalid=0;
                area.addEventListener('invalid',()=>invalid++);
                const disabled=document.querySelector('#disabled');
                const readonly=document.querySelector('#readonly');
                const checkbox=document.querySelector('#checkbox');
                const range=document.querySelector('#range');
                const button=document.querySelector('#button');
                // readonly bars every input from validation independently of
                // whether its type is editable (HTML readonly constraint rule;
                // pinned WPT form-validation-willValidate.html covers this).
                const flags = [
                    ['area.willValidate',area.willValidate,false],
                    ['area.valueMissing',area.validity.valueMissing,true],
                    ['area.valid',area.validity.valid,false],
                    ['area.checkValidity',area.checkValidity(),true],
                    ['area.reportValidity',area.reportValidity(),true],
                    ['area.invalid events',invalid,0],
                    ['disabled.willValidate',disabled.willValidate,false],
                    ['disabled.valueMissing',disabled.validity.valueMissing,false],
                    ['readonly.willValidate',readonly.willValidate,false],
                    ['readonly.valueMissing',readonly.validity.valueMissing,false],
                    ['checkbox.willValidate',checkbox.willValidate,false],
                    ['checkbox.valueMissing',checkbox.validity.valueMissing,true],
                    ['range.willValidate',range.willValidate,false],
                    ['range.valueMissing',range.validity.valueMissing,false],
                    ['button.willValidate',button.willValidate,false],
                    ['button.valueMissing',button.validity.valueMissing,false]
                ];
                for(const [name,actual,expected] of flags)
                    if(actual!==expected) throw new Error(name+': actual '+actual+', expected '+expected);
                checkbox.removeAttribute('readonly');
                range.removeAttribute('readonly');
                if(!checkbox.willValidate || !range.willValidate ||
                    !checkbox.validity.valueMissing || range.validity.valueMissing)
                    throw new Error('readonly removal must restore candidacy while preserving independent flags');
                return true;
            })()"#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn file_value_missing_tracks_the_selected_file_list() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = crate::install(
            engine.ctx(),
            "<form><input id='upload' type='file' required></form>",
            32,
        )
        .unwrap();
        let input = {
            let session = realm.session.borrow();
            let document = session.document();
            lumen_html::selector::get_element_by_id(document, document.root(), "upload")
                .unwrap()
                .unwrap()
        };
        assert!(matches!(
            eval(
                engine,
                "document.querySelector('#upload').validity.valueMissing"
            ),
            Value::Bool(true)
        ));
        set_input_files(
            engine.ctx(),
            &realm,
            &mut realm.forms.borrow_mut(),
            input,
            vec![FormFile {
                name: "selected.txt".into(),
                media_type: "text/plain".into(),
                last_modified: 0,
                bytes: std::sync::Arc::from([]),
            }],
        )
        .unwrap();
        assert!(matches!(
            eval(
                engine,
                "!document.querySelector('#upload').validity.valueMissing"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn retained_validity_state_is_live_identical_and_keeps_adopted_controls_alive() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<input required>", 64).unwrap();
        let result = eval(
            &mut engine,
            r#"(() => {
            const input = document.querySelector('input');
            const validity = input.validity;
            if (!(validity instanceof ValidityState) || validity !== input.validity || !validity.valueMissing)
                throw new Error('validity must be a live SameObject wrapper');
            input.value = 'filled';
            if (!validity.valid || validity.valueMissing)
                throw new Error('retained validity did not observe the live value');
            input.setCustomValidity('blocked');
            if (!validity.customError || validity.valid)
                throw new Error('retained validity did not observe custom validity');
            const target = document.implementation.createHTMLDocument('');
            target.body.appendChild(input);
            if (input.validity !== validity || !validity.customError)
                throw new Error('validity identity or state changed across adoption');
            input.setCustomValidity('');
            input.value = '';
            input.remove();
            globalThis.retainedValidity = validity;
            return validity.valueMissing && !validity.customError;
        })()"#,
        );
        assert!(matches!(result, Value::Bool(true)));
        engine.collect_garbage();
        realm.with_session(|_| ());
        let result = eval(
            &mut engine,
            "retainedValidity.valueMissing && !retainedValidity.customError && !retainedValidity.valid",
        );
        assert!(
            matches!(result, Value::Bool(true)),
            "validity must retain its detached control owner"
        );
    }

    #[test]
    fn public_date_time_and_default_step_validation_is_exposed() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<form><input type='date' value='2024-02-29' step='2'><input type='time' min='23:00' max='02:00' value='12:00'><input type='number' min='0' value='0.5'><input type='datetime-local'></form>", 64).unwrap();
        let result = eval(
            &mut engine,
            "(() => { const [date,time,number,local]=document.querySelectorAll('input'); const initial=date.validity.valid && time.validity.rangeUnderflow && time.validity.rangeOverflow && number.validity.stepMismatch && !number.checkValidity(); date.value='2024-02-30'; number.value='not a number'; local.value='2024-01-01 12:00:00.120'; return initial && date.value==='' && number.value==='' && !number.validity.badInput && local.value==='2024-01-01T12:00:00.12'; })()",
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn public_live_values_sanitize_and_email_url_types_validate() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<form><input id='default' type='range'><input id='range' type='range' min='10' max='20' step='3' value='15'><input id='upper' type='range' min='0' max='10' step='6' value='10'><input id='email' type='email'><input id='url' type='url'></form>", 64).unwrap();
        let result = eval(
            &mut engine,
            "(() => { const d=document.querySelector('#default'),r=document.querySelector('#range'),upper=document.querySelector('#upper'),e=document.querySelector('#email'),u=document.querySelector('#url'); if(d.value!=='50'||r.value!=='16'||upper.value!=='6')return false; r.value='2'; if(r.value!=='10')return false; r.value='bad'; if(r.value!=='16')return false; e.value=' a@localhost\\n'; u.value=' https://example.test/a\\n'; return e.value==='a@localhost'&&!e.validity.typeMismatch&&u.value==='https://example.test/a'&&!u.validity.typeMismatch; })()",
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn text_color_and_type_changed_values_use_html_sanitizers() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<form><input id='text' type='text'><input id='color' type='text'></form>",
            32,
        )
        .unwrap();
        let result = eval(
            &mut engine,
            r#"(()=>{
                const text=document.querySelector('#text'), color=document.querySelector('#color');
                text.value='left\r\nright\0';
                if(text.value!=='leftright\0') throw new Error('text value kept CR/LF');
                text.type='search'; text.value='a\rb\nc';
                if(text.value!=='abc') throw new Error('search value kept CR/LF');
                color.value='not-a-simple-color'; color.type='color';
                if(color.value!=='#000000') throw new Error('color type change did not sanitize');
                color.value='#AB09fF';
                return color.value==='#ab09ff';
            })()"#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn range_live_value_survives_default_and_constraint_attribute_changes() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<form><input id='r' type='range' min='20' max='30' step='2' value='10'><input id='t' type='text' value='35'><textarea id='a' type='date' maxlength='1'></textarea></form>",
            64,
        )
        .unwrap();
        let result = eval(
            &mut engine,
            "(() => { const r=document.querySelector('#r'),t=document.querySelector('#t'),a=document.querySelector('#a'),form=document.querySelector('form'); if(r.value!=='20')return false; r.min='0'; if(r.value!=='20')return false; r.min='20'; r.value='24'; r.defaultValue='22'; if(r.value!=='24')return false; form.reset(); if(r.value!=='22')return false; t.value='37'; t.type='range'; if(t.value!=='37')return false; a.value='ab'; return !a.validity.tooLong; })()",
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn range_live_value_follows_clone_import_form_data_and_reset_lifetime() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<form><input id='r' type='range' name='v' min='0' max='10' step='2' value='4'></form>",
            64,
        )
        .unwrap();
        assert!(lumen_host::lazy_globals::<lumen_host::blob::bindings::Module>(engine.ctx()).is_ok());
        let result = eval(
            &mut engine,
            "(() => { const form=document.querySelector('form'),r=document.querySelector('#r'); r.value='8'; const copy=r.cloneNode(), imported=document.importNode(r); form.append(copy,imported); if(copy.value!=='8'||imported.value!=='8')return false; const values=new FormData(form).getAll('v').join(','); if(values!=='8,8,8')return false; form.reset(); return r.value==='4'&&copy.value==='4'&&imported.value==='4'; })()",
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn value_mode_controls_separate_live_values_from_defaults_across_clone_and_reset() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<form><input id='text' name='text' type='text' value='seed'><input id='check' name='check' type='checkbox'><input id='transfer' type='text' value='check-seed'><input id='empty' type='text' value='keep'><input id='to-file' type='text' value='file-seed'><input id='range' name='range' type='range' min='0' max='10' value='4'><textarea id='area' name='area'>initial</textarea></form>",
            96,
        )
        .unwrap();
        assert!(lumen_host::lazy_globals::<lumen_host::blob::bindings::Module>(engine.ctx()).is_ok());
        let result = eval(
            &mut engine,
            r#"(()=>{
                const form=document.querySelector('form');
                const text=document.querySelector('#text');
                const check=document.querySelector('#check');
                const transfer=document.querySelector('#transfer');
                const empty=document.querySelector('#empty');
                const toFile=document.querySelector('#to-file');
                const range=document.querySelector('#range');
                const area=document.querySelector('#area');
                text.value='live';
                if(text.value!=='live'||text.defaultValue!=='seed'||text.getAttribute('value')!=='seed') throw new Error('input live value changed its default');
                text.defaultValue='replacement';
                if(text.value!=='live'||text.getAttribute('value')!=='replacement') throw new Error('default mutation replaced dirty value');
                const copy=text.cloneNode(); form.append(copy);
                if(copy.value!=='live'||copy.defaultValue!=='replacement') throw new Error('clone did not copy live/default state');
                check.value='enabled';
                if(check.value!=='enabled'||check.getAttribute('value')!=='enabled') throw new Error('default-on state did not reflect value attribute');
                check.checked=true;
                range.value='8';
                if(range.value!=='8'||range.defaultValue!=='4'||range.getAttribute('value')!=='4') throw new Error('range live value changed its default');
                area.setAttribute('value','unrelated');
                area.value='edited';
                if(area.value!=='edited'||area.defaultValue!=='initial'||area.textContent!=='initial') throw new Error('textarea live value changed default text');
                area.defaultValue='new text';
                if(area.value!=='edited'||area.defaultValue!=='new text'||area.getAttribute('value')!=='unrelated') throw new Error('textarea default setter replaced live value');
                const data=new FormData(form);
                const submitted='text='+data.getAll('text').join(',')+';check='+data.getAll('check').join(',')+';range='+data.getAll('range').join(',')+';area='+data.getAll('area').join(',');
                if(data.getAll('text').join(',')!=='live,live'||data.getAll('check')[0]!=='enabled'||data.getAll('range')[0]!=='8'||data.getAll('area')[0]!=='edited') throw new Error('form entry list did not use live values: '+submitted);
                text.type='number';
                if(text.value!=='') throw new Error('type change did not sanitize live value');
                text.value='42'; text.type='text';
                if(text.value!=='42') throw new Error('value-mode type transition lost dirty value');
                text.type='button';
                if(text.value!=='42'||text.defaultValue!=='42'||text.getAttribute('value')!=='42') throw new Error('value-to-default transition did not transfer the live value');
                text.type='text';
                if(text.value!=='42') throw new Error('default-to-value transition did not initialize from the content attribute');
                text.setAttribute('value','73');
                if(text.value!=='73') throw new Error('default-to-value transition did not clear the dirty value flag');
                transfer.value='check-live'; transfer.type='checkbox';
                if(transfer.value!=='check-live'||transfer.getAttribute('value')!=='check-live') throw new Error('value-to-default-on transition did not transfer the live value');
                transfer.type='text';
                transfer.setAttribute('value','check-next');
                if(transfer.value!=='check-next') throw new Error('default-on to value transition did not initialize cleanly from the content attribute');
                empty.value=''; empty.type='button';
                if(empty.value!=='keep'||empty.getAttribute('value')!=='keep') throw new Error('empty live value replaced a nonempty default during type change');
                empty.type='text';
                if(empty.value!=='keep') throw new Error('empty default-mode transition did not reinitialize from the content attribute');
                toFile.value='file-live'; toFile.type='file';
                if(toFile.value!==''||toFile.files.length!==0) throw new Error('entering filename mode retained a live value or file list');
                form.reset();
                return text.value==='73'&&copy.value==='replacement'&&transfer.value==='check-next'&&range.value==='4'&&area.value==='new text'&&area.defaultValue==='new text'&&check.value==='enabled';
            })()"#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn dirty_checkedness_and_indeterminate_clone_and_detached_form_reset() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<div></div>", 64).unwrap();
        let result = eval(
            &mut engine,
            r#"(()=>{
                const form=document.createElement('form');
                const text=document.createElement('input');
                text.defaultValue='text default'; text.value='text live';
                form.appendChild(text);

                const checkbox=document.createElement('input');
                checkbox.type='checkbox'; checkbox.defaultChecked=true;
                if(!checkbox.defaultChecked || !checkbox.hasAttribute('checked'))
                    throw new Error('defaultChecked did not reflect the checked content attribute');
                checkbox.checked=false; checkbox.indeterminate=true;
                form.appendChild(checkbox);

                const dirtyCopy=checkbox.cloneNode();
                dirtyCopy.setAttribute('checked','checked');
                const indeterminateCopy=checkbox.cloneNode();
                const checked=document.createElement('input');
                checked.checked=true;
                const checkedCopy=checked.cloneNode();

                const radios=document.createElement('div');
                const firstRadio=document.createElement('input');
                const secondRadio=document.createElement('input');
                const cleanRadio=document.createElement('input');
                for(const radio of [firstRadio,secondRadio,cleanRadio]) {
                    radio.type='radio'; radio.name='choice'; radios.appendChild(radio);
                }
                if(firstRadio.name!=='choice' || firstRadio.getAttribute('name')!=='choice')
                    throw new Error('input.name did not reflect the null-namespace attribute');
                firstRadio.checked=true; secondRadio.checked=true;
                firstRadio.setAttribute('checked','checked');
                cleanRadio.setAttribute('checked','checked');

                if(dirtyCopy.checked!==false) throw new Error('clone lost dirty checkedness');
                if(indeterminateCopy.indeterminate!==true) throw new Error('clone lost indeterminateness');
                if(checkedCopy.checked!==true) throw new Error('clone lost checkedness');
                if(firstRadio.checked!==false || secondRadio.checked!==false || !cleanRadio.checked)
                    throw new Error('radio checkedness group or dirty state was lost');
                form.reset();
                if(text.value!=='text default') throw new Error('detached form reset did not restore the text default');
                if(checkbox.checked!==true) throw new Error('detached form reset did not restore defaultChecked');
                if(checkbox.indeterminate!==true) throw new Error('reset unexpectedly cleared indeterminate');
                return true;
            })()"#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn prefixed_html_input_uses_shared_value_mode_without_accepting_foreign_input() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<div></div>", 16).unwrap();
        let (html_input, foreign_input) = {
            let mut session = realm.session.borrow_mut();
            let document = session.document_mut();
            let html_input = document
                .create(NodeKind::Element {
                    namespace: Namespace::Html,
                    name: lumen_html::Name::new("p:input"),
                    attributes: Vec::new(),
                })
                .unwrap();
            let foreign_input = document
                .create(NodeKind::Element {
                    namespace: Namespace::Other(std::rc::Rc::from("urn:foreign")),
                    name: lumen_html::Name::new("input"),
                    attributes: Vec::new(),
                })
                .unwrap();
            (html_input, foreign_input)
        };
        let mut state = realm.forms.borrow_mut();
        set_control_value(&realm, &mut state, html_input, "live").unwrap();
        assert!(set_control_value(&realm, &mut state, foreign_input, "wrong").is_err());
        drop(state);
        assert_eq!(control_value(&realm, html_input).unwrap(), "live");
    }

    #[test]
    fn public_pattern_validation_updates_invalid_events_and_submission() {
        let mut engine = Engine::new();
        let _realm = crate::install(
            engine.ctx(),
            "<form><input pattern='[0-9]+' value='letters'><button>Send</button></form>",
            64,
        )
        .unwrap();
        let result = eval(
            &mut engine,
            r#"(()=>{
            const form=document.querySelector('form'),input=document.querySelector('input');
            let invalid=0,submitted=0;
            input.oninvalid=()=>invalid++;
            form.onsubmit=e=>{submitted++;e.preventDefault()};
            if(!input.validity.patternMismatch||input.validity.valid||!input.validationMessage)throw new Error('initial pattern state');
            form.requestSubmit();
            if(invalid!==1||submitted!==0)throw new Error('invalid event/submission '+invalid+':'+submitted);
            input.value='123';
            if(input.validity.patternMismatch||!input.checkValidity()||input.validationMessage!=='')throw new Error('recovery '+input.value+':'+input.validity.patternMismatch+':'+input.validationMessage);
            form.requestSubmit();
            if(submitted!==1)throw new Error('valid submit');
            input.setAttribute('pattern','[');
            input.value='unrestricted';
            if(!input.checkValidity())throw new Error('invalid pattern');
            input.setAttribute('pattern','');
            if(!input.validity.patternMismatch)throw new Error('empty pattern');
            input.value='';
            if(!input.checkValidity())throw new Error('empty value');
            input.value='x';
            input.setAttribute('oninvalid','globalThis.markupInvalid=(globalThis.markupInvalid||0)+1;return false');
            if(input.checkValidity()||globalThis.markupInvalid!==1)throw new Error('markup invalid handler');
            input.removeAttribute('oninvalid');
            return input.oninvalid===null;
        })()"#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn pattern_reflection_and_validity_selectors_follow_live_values() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<form><input id='pattern' pattern='[0-9]+' value='letters'><input id='required' required></form>",
            64,
        )
        .unwrap();
        let result = eval(
            &mut engine,
            r#"(()=>{
                const form=document.querySelector('form');
                const pattern=document.querySelector('#pattern');
                const required=document.querySelector('#required');
                if(pattern.pattern!=='[0-9]+'||!pattern.matches(':invalid')||!form.matches(':invalid'))
                    throw new Error('initial reflected pattern/invalid state');
                pattern.pattern='[a-z]+';
                if(pattern.getAttribute('pattern')!=='[a-z]+'||!pattern.matches(':valid'))
                    throw new Error('pattern IDL setter did not update matching');
                pattern.pattern='('; // Invalid pattern syntax imposes no constraint.
                if(!pattern.matches(':valid')) throw new Error('invalid pattern syntax was enforced');
                required.value='filled';
                if(!required.matches(':is(:valid)')||!form.matches(':valid'))
                    throw new Error('live value did not update :valid aggregation');
                required.setCustomValidity('blocked');
                if(!required.matches(':invalid')||!form.matches(':invalid'))
                    throw new Error('custom validity did not update :invalid aggregation');
                required.setCustomValidity('');
                return required.matches(':valid')&&form.matches(':valid');
            })()"#,
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
    fn form_report_validity_dispatches_all_invalid_events_and_focuses_first_uncanceled() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<form><input id='first' required><input id='second' required></form>",
            64,
        )
        .unwrap();
        let result = eval(
            &mut engine,
            r#"(()=>{
                const form=document.querySelector('form');
                const first=document.querySelector('#first');
                const second=document.querySelector('#second');
                const seen=[];
                first.addEventListener('invalid', event=>{seen.push('first');event.preventDefault()});
                second.addEventListener('invalid', ()=>seen.push('second'));
                const checked=!form.checkValidity()&&seen.join(',')==='first,second'&&document.activeElement!==first&&document.activeElement!==second;
                seen.length=0;
                const reported=!form.reportValidity()&&seen.join(',')==='first,second'&&document.activeElement===second;
                return checked&&reported;
            })()"#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn form_check_validity_snapshots_invalid_controls_before_dispatch() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<form><input id='first' required><input id='second' required></form>",
            64,
        )
        .unwrap();
        let result = eval(
            &mut engine,
            r#"(()=>{
                const form=document.querySelector('form');
                const first=document.querySelector('#first');
                const second=document.querySelector('#second');
                const seen=[];
                first.addEventListener('invalid',()=>{seen.push('first');second.value='filled'});
                second.addEventListener('invalid',()=>seen.push('second'));
                return !form.checkValidity() && seen.join(',')==='first,second' && second.validity.valid;
            })()"#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn form_report_validity_skips_focus_after_candidate_disconnects() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<form><input id='canceled' required><input id='removed' required></form>",
            64,
        )
        .unwrap();
        let result = eval(
            &mut engine,
            r#"(()=>{
                const form=document.querySelector('form');
                const canceled=document.querySelector('#canceled');
                const removed=document.querySelector('#removed');
                canceled.addEventListener('invalid',event=>event.preventDefault());
                removed.addEventListener('invalid',()=>removed.remove());
                return !form.reportValidity() && removed.parentNode===null &&
                    document.activeElement!==canceled && document.activeElement!==removed;
            })()"#,
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
    fn select_add_uses_shared_nullable_before_and_tree_insertion_rules() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        crate::install(
            engine.ctx(),
            "<select id='select'><option id='first' value='a'>A</option><optgroup id='group'><option id='last' value='z'>Z</option></optgroup></select>",
            96,
        )
        .unwrap();
        let result = eval(
            engine,
            r#"(() => {
              const failures = [];
              const check = (name, condition) => { if (!condition) failures.push(name); };
              const select = document.querySelector('#select');
              const group = document.querySelector('#group');
              const first = document.querySelector('#first');
              const last = document.querySelector('#last');

              const numeric = new Option('numeric', 'n');
              select.add(numeric, 1);
              check('index uses canonical option list and optgroup parent',
                select.options[1] === numeric && numeric.parentNode === group);

              const beforeGroup = new Option('before group', 'g');
              select.add(beforeGroup, group);
              check('HTMLElement reference inserts before its parent child',
                beforeGroup.parentNode === select && beforeGroup.nextSibling === group);

              const beforeOption = new Option('before option', 'p');
              select.options.add(beforeOption, last);
              check('collection add shares nested reference placement',
                beforeOption.parentNode === group && beforeOption.nextSibling === last);

              const appendedNull = new Option('null append', 'null');
              select.add(appendedNull, null);
              const appendedDefault = new Option('default append', 'default');
              select.add(appendedDefault);
              check('null and omitted before append',
                appendedNull.parentNode === select && appendedDefault.parentNode === select &&
                select.options[select.options.length - 1] === appendedDefault);

              const sameLength = select.options.length;
              select.add(numeric, numeric);
              check('same node before is a no-op', select.options.length === sameLength);

              const insertedAtZero = new Option('index zero', 'zero');
              select.options.add(insertedAtZero, 0);
              let numericConversions = 0;
              const numericLike = {
                valueOf() { numericConversions++; return 1; },
                toString() { throw new Error('ToNumber must use valueOf once'); }
              };
              const fromNumericObject = new Option('numeric object', 'numeric-object');
              select.add(fromNumericObject, numericLike);
              check('numeric before union uses ToNumber exactly once',
                numericConversions === 1 && select.options[1] === fromNumericObject);
              const thrown = {};
              let abruptIdentity = false;
              try {
                select.add(new Option('abrupt', 'abrupt'), {
                  valueOf() { throw thrown; }
                });
              } catch (error) { abruptIdentity = error === thrown; }
              check('numeric before conversion preserves abrupt identity', abruptIdentity);

              const newGroup = document.createElement('optgroup');
              const groupedOption = document.createElement('option');
              newGroup.append(groupedOption);
              select.add(newGroup, null);
              check('collection and select accept option groups',
                select.options[0] === insertedAtZero && newGroup.parentNode === select &&
                groupedOption.parentNode === newGroup);

              const foreignDocument = document.implementation.createHTMLDocument('foreign');
              const foreignOption = foreignDocument.createElement('option');
              foreignOption.textContent = 'adopted';
              select.add(foreignOption, null);
              check('cross-document insertion keeps wrapper identity',
                foreignOption === select.options[select.options.length - 1] &&
                foreignOption.ownerDocument === document && foreignOption.parentNode === select);

              const lengthBeforeErrors = select.options.length;
              let notFound = false;
              try { select.add(new Option('bad reference', 'bad'), document.createElement('div')); }
              catch (error) { notFound = error instanceof DOMException &&
                error.name === 'NotFoundError' && error.code === 8; }
              let hierarchy = false;
              const ancestor = document.createElement('option');
              const nestedSelect = document.createElement('select');
              ancestor.append(nestedSelect);
              try { nestedSelect.add(ancestor); }
              catch (error) { hierarchy = error instanceof DOMException &&
                error.name === 'HierarchyRequestError' && error.code === 3; }
              let elementType = false;
              try { select.add(document.createElement('div')); }
              catch (error) { elementType = error instanceof TypeError; }
              check('NotFound, hierarchy and element-union errors',
                notFound && hierarchy && elementType && select.options.length === lengthBeforeErrors);
              return failures.join(',');
            })()"#,
        );
        match result {
            Value::Str(failures) if failures.is_empty() => {}
            Value::Str(failures) => panic!("select.add contract failed: {failures}"),
            _ => panic!("select.add contract did not return diagnostics"),
        }
    }

    #[test]
    fn formdata_constructor_preserves_identity_and_dispatches_formdata() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<form><input name='a' value='one'><select name='s'><option value='x'>X</option><option value='y' selected>Y</option></select><textarea name='t'>seed</textarea></form>", 96).unwrap();
        assert!(lumen_host::lazy_globals::<lumen_host::blob::bindings::Module>(engine.ctx()).is_ok());
        let result = eval(
            &mut engine,
            "(() => { const form = document.querySelector('form'); const select = document.querySelector('select'); const area = document.querySelector('textarea'); select.selectedIndex = -1; area.value = 'live'; let seen = null; form.addEventListener('formdata', event => { seen = event.formData; event.formData.append('event', 'ran'); }); const data = new FormData(form); const same = seen === data; return same && data.getAll('a')[0] === 'one' && data.getAll('s').length === 0 && data.getAll('t')[0] === 'live' && data.getAll('event')[0] === 'ran'; })()",
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn formdata_and_request_submit_use_normalized_button_type_state() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<form><button id='missing' name='missing' value='m'>Missing</button><button id='invalid' type='menu' name='invalid' value='i'>Invalid</button><button id='casefold' type='SuBmIt' name='casefold' value='c'>Casefold</button><button id='reset' type='RESET' name='reset' value='r'>Reset</button><textarea type='checkbox' name='area'>area-value</textarea><select type='file' name='choice'><option value='selected' selected>Selected</option></select></form>",
            64,
        )
        .unwrap();
        assert!(lumen_host::lazy_globals::<lumen_host::blob::bindings::Module>(engine.ctx()).is_ok());
        let result = eval(
            &mut engine,
            r#"(()=>{
                const form=document.querySelector('form');
                const missing=document.querySelector('#missing');
                const invalid=document.querySelector('#invalid');
                const casefold=document.querySelector('#casefold');
                const reset=document.querySelector('#reset');
                const contains=(submitter,name,value)=>[...new FormData(form,submitter)].some(entry=>entry[0]===name&&entry[1]===value);
                let observed=null;
                form.addEventListener('submit',event=>{observed=event.submitter;event.preventDefault()});
                form.requestSubmit(invalid);
                const invalidDefault=observed===invalid && contains(invalid,'invalid','i');
                form.requestSubmit(casefold);
                const caseInsensitive=observed===casefold && contains(casefold,'casefold','c');
                form.requestSubmit(missing);
                const missingDefault=observed===missing && contains(missing,'missing','m');
                const resetRejected=(()=>{try{form.requestSubmit(reset);return false}catch(error){return error instanceof TypeError}})();
                const nonInputTypeIgnored=contains(missing,'area','area-value')&&contains(missing,'choice','selected');
                return invalidDefault&&caseInsensitive&&missingDefault&&resetRejected&&nonInputTypeIgnored;
            })()"#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn formdata_empty_file_entry_has_octet_stream_type() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        crate::install(
            engine.ctx(),
            "<form><input type='file' name='upload'></form>",
            32,
        )
        .unwrap();
        let result = eval(
            engine,
            "(()=>{const file=new FormData(document.querySelector('form')).get('upload');return file!==null&&file.name===''&&file.type==='application/octet-stream'&&file.size===0})()",
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn specification_clone_control_state_excludes_user_interaction_and_selection() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = crate::install(engine.ctx(), "<input id='edited' minlength='5'><textarea id='area'>default</textarea><input id='checked' type='checkbox'>", 128).unwrap();
        let input = {
            let session = realm.session.borrow();
            lumen_html::selector::query_selector(session.document(), session.document().root(), "#edited").unwrap().unwrap()
        };
        {
            let mut state = realm.forms.borrow_mut();
            set_control_value_from_user(&realm, &mut state, input, "x").unwrap();
            mark_user_edited(&mut state, input);
        }
        assert!(matches!(eval(engine, r#"
          const check=(v,m)=>{if(!v)throw new Error(m)}, input=document.querySelector('#edited'), area=document.querySelector('#area'), checked=document.querySelector('#checked');
          area.value='live';area.setSelectionRange(1,3);checked.checked=true;checked.indeterminate=true;input.setCustomValidity('source-only');
          const copy=input.cloneNode(), areaCopy=document.importNode(area,true), checkedCopy=checked.cloneNode();
          check(input.validity.tooShort,'original actual user edit provenance');
          check(copy.value==='x','cloned live value');
          check(!copy.validity.tooShort,'clone initial user edit provenance');
          check(!copy.matches(':user-invalid'),'clone initial user validity');
          check(!copy.validity.customError && copy.validationMessage==='','custom validity is not a cloning state');
          copy.maxLength=0;check(copy.getAttribute('maxlength')==='0' && !copy.validity.tooLong,'input maxlength reflection without user editing clone');
          check(areaCopy.value==='live' && areaCopy.selectionStart===0 && areaCopy.selectionEnd===0,'textarea raw value with initial selection');
          check(checkedCopy.checked && checkedCopy.indeterminate,'checkedness and indeterminate copy');
          copy.setAttribute('value','new-default');areaCopy.textContent='new-default';check(copy.value==='x' && areaCopy.value==='live','dirty flags copied');
          checkedCopy.defaultChecked=false;check(checkedCopy.checked,'dirty checkedness copied independently from value');
          const pristine=document.createElement('input'), pristineCopy=document.importNode(pristine,false);pristineCopy.defaultValue='changed';pristineCopy.defaultChecked=true;check(pristineCopy.value==='changed' && pristineCopy.checked,'initial dirty flags remain false');
          const select=document.createElement('select');select.innerHTML='<option selected>first</option><option>second</option>';select.selectedIndex=1;const selectCopy=select.cloneNode(true);check(select.selectedIndex===1 && selectCopy.selectedIndex===0,'option selectedness initializes from cloned attributes rather than dirty selectedness');true
        "#), Value::Bool(true)));
    }

    #[test]
    fn form_submission_encoding_is_captured_before_formdata_and_hidden_charset_uses_it() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = crate::install(engine.ctx(),
            "<form action='submit' accept-charset='Shift_JIS'><input type='hidden' name='_charset_' value='wrong'><input name='text' value='あ'></form>", 64).unwrap();
        realm.set_document_url("https://forms.test/page.html");
        assert!(realm.set_document_encoding("windows-1252"));
        let requests = Rc::new(RefCell::new(Vec::<FormSubmissionRequest>::new()));
        let captured = requests.clone();
        realm.set_form_submission_host(Rc::new(move |request| {
            captured.borrow_mut().push(request);
            Ok(())
        }));
        let result = eval(engine, r#"(() => {
            if (document.characterSet !== 'windows-1252' || document.charset !== document.characterSet || document.inputEncoding !== document.characterSet) return false;
            const form = document.querySelector('form');
            form.addEventListener('formdata', event => {
                form.acceptCharset = 'UTF-8';
                event.formData.append('observed', event.formData.get('_charset_'));
            });
            form.submit();
            return true;
        })()"#);
        assert!(matches!(result, Value::Bool(true)));
        let requests = requests.borrow();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].metadata.encoding, "Shift_JIS");
        assert!(requests[0].metadata.entries.iter().any(|entry| entry.name == "_charset_" && entry.value == FormEntryValue::Text("Shift_JIS".into())));
        assert!(requests[0].metadata.entries.iter().any(|entry| entry.name == "observed" && entry.value == FormEntryValue::Text("Shift_JIS".into())));
    }

    #[test]
    fn legacy_submit_bypasses_validation_and_submit_event_but_keeps_formdata_mutations() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = crate::install(
            engine.ctx(),
            "<form id='target' action='submit' method='post'><input name='field' required value='seed'></form><form><input name='unrelated' value='wrong-form'></form><input form='target' name='external' value='outside-control'>",
            96,
        )
        .unwrap();
        realm.set_document_url("https://forms.test/base/page.html");

        let requests = Rc::new(RefCell::new(Vec::<FormSubmissionRequest>::new()));
        let captured = requests.clone();
        realm.set_form_submission_host(Rc::new(move |request| {
            captured.borrow_mut().push(request);
            Ok(())
        }));

        let result = eval(
            engine,
            r#"(()=>{
                const form=document.querySelector('form');
                const field=document.querySelector('input');
                let invalid=0, submits=0, formdatas=0;
                field.addEventListener('invalid',()=>invalid++);
                form.addEventListener('submit',()=>submits++);
                form.addEventListener('formdata',event=>{
                    formdatas++;
                    event.formData.append('observed',String(formdatas));
                });

                field.value='';
                const requestBlocked=form.requestSubmit()===undefined && invalid===1 &&
                    submits===0 && formdatas===0;
                field.value='ready';
                form.requestSubmit();
                const requestSent=invalid===1 && submits===1 && formdatas===1;
                field.value='';
                const legacyReturn=form.submit();
                return requestBlocked && requestSent && legacyReturn===undefined &&
                    invalid===1 && submits===1 && formdatas===2;
            })()"#,
        );
        assert!(matches!(result, Value::Bool(true)));

        let requests = requests.borrow();
        assert_eq!(
            requests.len(),
            2,
            "invalid requestSubmit must not reach the host"
        );
        assert_eq!(
            requests[0].metadata.action,
            "https://forms.test/base/submit"
        );
        assert_eq!(
            requests[1].metadata.action,
            "https://forms.test/base/submit"
        );
        for (index, expected) in ["1", "2"].into_iter().enumerate() {
            assert!(requests[index]
                .metadata
                .entries
                .iter()
                .any(|entry| entry.name == "external"
                    && entry.value == FormEntryValue::Text("outside-control".to_owned())));
            assert!(!requests[index]
                .metadata
                .entries
                .iter()
                .any(|entry| entry.name == "unrelated"));
            let entry = requests[index]
                .metadata
                .entries
                .iter()
                .find(|entry| entry.name == "observed")
                .expect("formdata listener entry reaches the host request");
            assert_eq!(
                entry.value,
                FormEntryValue::Text(expected.to_owned()),
                "the host snapshot must include the post-event FormData value"
            );
        }
    }

    #[test]
    fn readonly_candidates_and_live_user_form_pseudos_use_native_state() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        crate::install(
            engine.ctx(),
            "<form><input id='required' required placeholder='hint'><input id='color' type='color' readonly><input id='date' type='date' placeholder='ignored'><button id='button'></button><select id='select'><option selected>one</option></select><textarea id='area' readonly></textarea></form>",
            96,
        )
        .unwrap();
        let result = eval(
            engine,
            r#"(() => {
                const failures = [];
                const check = (ok, name) => { if (!ok) failures.push(name); };
                const form = document.querySelector('form');
                const required = document.getElementById('required');
                const color = document.getElementById('color');
                const date = document.getElementById('date');
                const button = document.getElementById('button');
                const select = document.getElementById('select');
                const area = document.getElementById('area');

                color.setCustomValidity('barred');
                button.setCustomValidity('button custom');
                select.setCustomValidity('select custom');
                check(color.readOnly && !color.willValidate && color.validity.customError &&
                    color.validationMessage === '', 'readonly-candidate-flags-and-message');
                check(!('readOnly' in button) && !('readOnly' in select) &&
                    button.validationMessage === 'button custom' &&
                    select.validationMessage === 'select custom', 'readonly-interface-scope');
                check(area.readOnly, 'textarea-readonly-remains-reflected');
                check(!date.matches(':placeholder-shown'), 'unsupported-placeholder-input-state');
                button.setCustomValidity('');
                select.setCustomValidity('');

                const initially = required.matches(':placeholder-shown') &&
                    !required.matches(':user-valid') && !required.matches(':user-invalid');
                required.value = 'script';
                const scriptEditDoesNotCountAsUser = !required.matches(':user-valid') &&
                    !required.matches(':user-invalid') && !required.matches(':placeholder-shown');
                required.value = '';
                form.requestSubmit();
                const afterAttempt = required.matches(':user-invalid') &&
                    !required.matches(':user-valid');
                required.value = 'now valid';
                const afterCorrection = required.matches(':user-valid') &&
                    !required.matches(':user-invalid');
                form.reset();
                const afterReset = !required.matches(':user-valid') &&
                    !required.matches(':user-invalid') && required.matches(':placeholder-shown');
                check(initially && scriptEditDoesNotCountAsUser && afterAttempt &&
                    afterCorrection && afterReset, 'interaction-submit-correction-reset');
                check(!form.matches(':user-valid') && !form.matches(':user-invalid'),
                    'user-validity-is-control-only');
                return failures.join('|');
            })()"#,
        );
        let Value::Str(failures) = result else {
            panic!("readonly/user pseudo contract must return diagnostics");
        };
        assert!(failures.is_empty(), "form pseudo failures: {failures}");
    }
}
