//! Host-testable HTML form constraint and entry-list algorithms.
//!
//! These helpers intentionally consume the shared DOM tree. The JavaScript
//! adapter can expose them from the element classes without maintaining a
//! second control tree.
use crate::{Document, Error, Name, NodeId, NodeKind};
use alloc::{borrow::ToOwned, string::String, sync::Arc, vec, vec::Vec};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ValidityState {
    pub value_missing: bool,
    pub type_mismatch: bool,
    pub too_long: bool,
    pub too_short: bool,
    pub pattern_mismatch: bool,
    pub range_overflow: bool,
    pub range_underflow: bool,
    pub step_mismatch: bool,
    pub bad_input: bool,
    pub custom_error: bool,
}

impl ValidityState {
    pub fn valid(self) -> bool {
        !(self.value_missing
            || self.type_mismatch
            || self.too_long
            || self.too_short
            || self.pattern_mismatch
            || self.range_overflow
            || self.range_underflow
            || self.step_mismatch
            || self.bad_input
            || self.custom_error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormEntry {
    pub name: String,
    pub value: FormEntryValue,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FormEntryValue {
    Text(String),
    File(FormFile),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormFile {
    pub name: String,
    pub media_type: String,
    pub last_modified: i64,
    pub bytes: Arc<[u8]>,
}

/// Immutable input to the embedder's form-navigation service. `action` keeps
/// its markup/submitter value here because URL resolution needs the owning
/// browser document URL, which the shared DOM deliberately does not invent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormSubmission {
    pub action: String,
    pub method: String,
    pub enctype: String,
    pub target: String,
    pub entries: Vec<FormEntry>,
}

pub fn submission_metadata(
    document: &Document,
    form: NodeId,
    submitter: Option<NodeId>,
    entries: Vec<FormEntry>,
) -> Option<FormSubmission> {
    let (_, form_attributes) = element(document, form)?;
    let submitter_attributes =
        submitter.and_then(|node| element(document, node).map(|(_, attrs)| attrs));
    let overridden = |name: &str| {
        submitter_attributes
            .and_then(|attrs| attribute(attrs, name))
            .filter(|value| !value.is_empty())
    };
    let attr = |name: &str| attribute(form_attributes, name).unwrap_or("");
    let method = overridden("formmethod").unwrap_or_else(|| attr("method"));
    let method = match method.trim().to_ascii_lowercase().as_str() {
        "post" => "post",
        "dialog" => "dialog",
        _ => "get",
    };
    let enctype = overridden("formenctype").unwrap_or_else(|| attr("enctype"));
    let enctype = match enctype.trim().to_ascii_lowercase().as_str() {
        "text/plain" => "text/plain",
        "multipart/form-data" => "multipart/form-data",
        _ => "application/x-www-form-urlencoded",
    };
    Some(FormSubmission {
        action: overridden("formaction")
            .unwrap_or_else(|| attr("action"))
            .to_owned(),
        method: method.to_owned(),
        enctype: enctype.to_owned(),
        target: overridden("formtarget")
            .unwrap_or_else(|| attr("target"))
            .to_owned(),
        entries,
    })
}

pub fn validation_bypassed(document: &Document, form: NodeId, submitter: Option<NodeId>) -> bool {
    element(document, form).is_some_and(|(_, attrs)| attribute(attrs, "novalidate").is_some())
        || submitter
            .and_then(|node| element(document, node))
            .is_some_and(|(_, attrs)| attribute(attrs, "formnovalidate").is_some())
}

impl FormEntry {
    pub fn text(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: FormEntryValue::Text(value.into()),
        }
    }

    pub fn file(name: impl Into<String>, value: FormFile) -> Self {
        Self {
            name: name.into(),
            value: FormEntryValue::File(value),
        }
    }
}

/// Find a control's form owner, honoring explicit `form=id` before ancestor
/// association. A missing referenced form leaves the control unassociated.
pub fn form_owner(document: &Document, control: NodeId) -> Option<NodeId> {
    if !element(document, control).is_some_and(|(tag, _)| {
        matches!(
            tag,
            "input" | "textarea" | "select" | "button" | "fieldset" | "output"
        )
    }) {
        return None;
    }
    let (_, attributes) = element(document, control)?;
    if let Some(id) = attribute(attributes, "form") {
        if id.is_empty() {
            return None;
        }
        return descendants(document, document.root())
            .into_iter()
            .find(|node| {
                element(document, *node)
                    .is_some_and(|(tag, attrs)| tag == "form" && attribute(attrs, "id") == Some(id))
            });
    }
    let mut parent = document.parent(control).ok().flatten();
    while let Some(node) = parent {
        if element(document, node).is_some_and(|(name, _)| name == "form") {
            return Some(node);
        }
        parent = document.parent(node).ok().flatten();
    }
    None
}

/// Form-associated controls in document tree order, including external
/// controls with an explicit `form` attribute.
pub fn form_controls(document: &Document, form: NodeId) -> Vec<NodeId> {
    descendants(document, document.root())
        .into_iter()
        .skip(1)
        .filter(|node| {
            form_owner(document, *node) == Some(form)
                && element(document, *node).is_some_and(|(name, _)| {
                    matches!(
                        name,
                        "input" | "textarea" | "select" | "button" | "fieldset" | "output"
                    )
                })
        })
        .collect()
}

fn attribute<'a>(attributes: &'a [(Name, String)], name: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|(key, _)| key.as_str() == name)
        .map(|(_, value)| value.as_str())
}

fn element<'a>(document: &'a Document, node: NodeId) -> Option<(&'a str, &'a [(Name, String)])> {
    match document.kind(node).ok()? {
        NodeKind::Element {
            name, attributes, ..
        } => Some((name.as_str(), attributes)),
        _ => None,
    }
}

fn text_content(document: &Document, root: NodeId) -> String {
    let mut text = String::new();
    if let Ok(NodeKind::Text(value)) = document.kind(root) {
        text.push_str(value);
    } else {
        let _ = document.append_descendant_text(root, &mut text);
    }
    text
}

fn descendants(document: &Document, root: NodeId) -> Vec<NodeId> {
    let mut result = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        result.push(node);
        let mut children = Vec::new();
        let mut current = document.first_child(node).ok().flatten();
        while let Some(child) = current {
            children.push(child);
            current = document.next_sibling(child).ok().flatten();
        }
        pending.extend(children.into_iter().rev());
    }
    result
}

fn control_value(
    document: &Document,
    node: NodeId,
    name: &str,
    attributes: &[(Name, String)],
) -> String {
    if let Some(value) = attribute(attributes, "value") {
        value.to_owned()
    } else if name == "textarea" {
        text_content(document, node)
    } else {
        String::new()
    }
}

/// Reset default value for an input, textarea, or other value-bearing control.
pub fn default_value(document: &Document, node: NodeId) -> Option<String> {
    let (name, attributes) = element(document, node)?;
    if name == "input"
        && attribute(attributes, "type").is_some_and(|value| value.eq_ignore_ascii_case("file"))
    {
        return Some(String::new());
    }
    if name == "select" {
        return Some(select_value(document, node));
    }
    // A textarea's default value is its text content. The reflected `value`
    // attribute is not part of HTMLTextAreaElement's markup model; the JS
    // adapter uses it only as a shared live-value slot while the control is
    // dirty.
    if name == "textarea" {
        return Some(text_content(document, node));
    }
    Some(control_value(document, node, name, attributes))
}

/// Option descendants in tree order for a select control.
pub fn select_options(document: &Document, select: NodeId) -> Vec<NodeId> {
    descendants(document, select)
        .into_iter()
        .skip(1)
        .filter(|node| element(document, *node).is_some_and(|(tag, _)| tag == "option"))
        .collect()
}

pub fn option_value(document: &Document, option: NodeId) -> Option<String> {
    let (_, attributes) = element(document, option)?;
    Some(
        attribute(attributes, "value")
            .map(str::to_owned)
            .unwrap_or_else(|| text_content(document, option).trim().to_owned()),
    )
}

fn option_disabled(document: &Document, option: NodeId) -> bool {
    if is_disabled(document, option) {
        return true;
    }
    let mut ancestor = document.parent(option).ok().flatten();
    while let Some(node) = ancestor {
        if let Some((tag, attributes)) = element(document, node) {
            if tag == "optgroup" && attribute(attributes, "disabled").is_some() {
                return true;
            }
            if tag == "select" {
                break;
            }
        }
        ancestor = document.parent(node).ok().flatten();
    }
    false
}

fn selected_option_ids(document: &Document, select: NodeId) -> Vec<NodeId> {
    selected_option_ids_with(document, select, &[])
}

/// Resolve selectedness with adapter-owned dirty-state overrides. An override
/// can explicitly clear every option without triggering the initial
/// first-option fallback used by markup defaults.
pub fn selected_option_ids_with(
    document: &Document,
    select: NodeId,
    selectedness: &[(NodeId, bool)],
) -> Vec<NodeId> {
    let options = select_options(document, select);
    let has_override = options
        .iter()
        .any(|option| selectedness.iter().any(|(id, _)| id == option));
    let explicit = options
        .iter()
        .copied()
        .filter(|option| {
            selectedness
                .iter()
                .find(|(id, _)| id == option)
                .map(|(_, selected)| *selected)
                .unwrap_or_else(|| {
                    element(document, *option)
                        .is_some_and(|(_, attrs)| attribute(attrs, "selected").is_some())
                })
        })
        .collect::<Vec<_>>();
    if !explicit.is_empty()
        || has_override
        || element(document, select)
            .is_some_and(|(_, attrs)| attribute(attrs, "multiple").is_some())
    {
        explicit
    } else {
        options
            .into_iter()
            .find(|option| !option_disabled(document, *option))
            .into_iter()
            .collect()
    }
}

/// Current select value; a multiple select exposes its first selected value.
pub fn select_value(document: &Document, select: NodeId) -> String {
    select_value_with(document, select, &[])
}

pub fn select_value_with(
    document: &Document,
    select: NodeId,
    selectedness: &[(NodeId, bool)],
) -> String {
    selected_option_ids_with(document, select, selectedness)
        .first()
        .and_then(|option| option_value(document, *option))
        .unwrap_or_default()
}

pub fn selected_index(document: &Document, select: NodeId) -> isize {
    selected_index_with(document, select, &[])
}

pub fn selected_index_with(
    document: &Document,
    select: NodeId,
    selectedness: &[(NodeId, bool)],
) -> isize {
    let options = select_options(document, select);
    selected_option_ids_with(document, select, selectedness)
        .first()
        .and_then(|selected| options.iter().position(|option| option == selected))
        .map_or(-1, |index| index as isize)
}

/// Set option selectedness through the shared DOM representation. Callers
/// capture reset defaults before invoking this mutation.
pub fn set_option_selected(
    document: &mut Document,
    option: NodeId,
    selected: bool,
) -> Result<(), Error> {
    if selected {
        document.set_attribute(option, "selected", "")
    } else {
        document.remove_attribute(option, "selected")
    }
}

pub fn set_select_value(document: &mut Document, select: NodeId, value: &str) -> Result<(), Error> {
    let options = select_options(document, select);
    let multiple = element(document, select)
        .is_some_and(|(_, attributes)| attribute(attributes, "multiple").is_some());
    let mut matched = false;
    for option in options {
        let selected =
            option_value(document, option).as_deref() == Some(value) && (multiple || !matched);
        set_option_selected(document, option, selected)?;
        matched |= selected;
    }
    Ok(())
}

pub fn set_selected_index(
    document: &mut Document,
    select: NodeId,
    index: isize,
) -> Result<(), Error> {
    let options = select_options(document, select);
    for (position, option) in options.into_iter().enumerate() {
        set_option_selected(document, option, index >= -1 && position as isize == index)?;
    }
    Ok(())
}

/// Controls plus option nodes whose selectedness is reset with their select.
pub fn form_reset_controls(document: &Document, form: NodeId) -> Vec<NodeId> {
    let controls = form_controls(document, form);
    let mut reset = controls.clone();
    for control in controls {
        if element(document, control).is_some_and(|(tag, _)| tag == "select") {
            reset.extend(select_options(document, control));
        }
    }
    reset
}

fn radio_group_required(document: &Document, node: NodeId, group: &str) -> bool {
    descendants(document, document.root())
        .into_iter()
        .any(|candidate| {
            let Some((name, attributes)) = element(document, candidate) else {
                return false;
            };
            name == "input"
                && attribute(attributes, "type")
                    .unwrap_or("text")
                    .eq_ignore_ascii_case("radio")
                && attribute(attributes, "name") == Some(group)
                && attribute(attributes, "required").is_some()
                && !is_disabled(document, candidate)
                && form_owner(document, candidate) == form_owner(document, node)
                && (candidate == node || document.kind(candidate).is_ok())
        })
}

pub fn is_disabled(document: &Document, node: NodeId) -> bool {
    let Some((name, attributes)) = element(document, node) else {
        return true;
    };
    if attribute(attributes, "disabled").is_some() {
        return true;
    }
    // A disabled fieldset disables descendants except those in its first legend.
    let mut child = node;
    let mut parent = document.parent(child).ok().flatten();
    while let Some(ancestor) = parent {
        if let Some(("fieldset", attrs)) = element(document, ancestor) {
            if attribute(attrs, "disabled").is_some() {
                let first_legend =
                    descendants(document, ancestor)
                        .into_iter()
                        .skip(1)
                        .find(|candidate| {
                            element(document, *candidate).is_some_and(|(tag, _)| tag == "legend")
                        });
                let inside_first_legend = first_legend.is_some_and(|legend| {
                    let mut current = Some(child);
                    while let Some(id) = current {
                        if id == legend {
                            return true;
                        }
                        current = document.parent(id).ok().flatten();
                    }
                    false
                });
                if !inside_first_legend {
                    return true;
                }
            }
        }
        child = ancestor;
        parent = document.parent(ancestor).ok().flatten();
    }
    let _ = name;
    false
}

pub fn will_validate(document: &Document, node: NodeId) -> bool {
    let Some((name, attributes)) = element(document, node) else {
        return false;
    };
    if is_disabled(document, node) || attribute(attributes, "readonly").is_some() {
        return false;
    }
    match name {
        "input" => !matches!(
            attribute(attributes, "type")
                .unwrap_or("text")
                .to_ascii_lowercase()
                .as_str(),
            "hidden" | "button" | "reset" | "submit" | "image"
        ),
        "select" | "textarea" => true,
        "button" => matches!(attribute(attributes, "type").unwrap_or("submit"), "submit"),
        _ => false,
    }
}

pub fn validity(document: &Document, node: NodeId, custom_message: &str) -> ValidityState {
    validity_with_value(document, node, None, custom_message)
}

pub fn validity_with_value(
    document: &Document,
    node: NodeId,
    value_override: Option<&str>,
    custom_message: &str,
) -> ValidityState {
    validity_with_value_and_selectedness(document, node, value_override, custom_message, &[])
}

pub fn validity_with_value_and_selectedness(
    document: &Document,
    node: NodeId,
    value_override: Option<&str>,
    custom_message: &str,
    selectedness: &[(NodeId, bool)],
) -> ValidityState {
    let mut state = ValidityState {
        custom_error: !custom_message.is_empty(),
        ..ValidityState::default()
    };
    let Some((name, attributes)) = element(document, node) else {
        return state;
    };
    if !will_validate(document, node) {
        return state;
    }
    let value = value_override.map(ToOwned::to_owned).unwrap_or_else(|| {
        if name == "select" {
            select_value_with(document, node, selectedness)
        } else {
            control_value(document, node, name, attributes)
        }
    });
    let type_name = attribute(attributes, "type")
        .unwrap_or("text")
        .to_ascii_lowercase();
    let required = attribute(attributes, "required").is_some();
    if required {
        state.value_missing = match (name, type_name.as_str()) {
            ("input", "checkbox") => attribute(attributes, "checked").is_none(),
            ("input", "radio") => {
                let group = attribute(attributes, "name").unwrap_or("");
                !group.is_empty()
                    && radio_group_required(document, node, group)
                    && !descendants(document, document.root())
                        .into_iter()
                        .any(|candidate| {
                            element(document, candidate).is_some_and(|(tag, attrs)| {
                                tag == "input"
                                    && attribute(attrs, "type")
                                        .unwrap_or("text")
                                        .eq_ignore_ascii_case("radio")
                                    && attribute(attrs, "name") == Some(group)
                                    && attribute(attrs, "checked").is_some()
                            })
                        })
            }
            ("select", _) => {
                let selected = selected_option_ids_with(document, node, selectedness);
                selected.is_empty()
                    || (!matches!(element(document, node), Some((_, attrs)) if attribute(attrs, "multiple").is_some())
                        && selected.first().is_some_and(|option| {
                            select_options(document, node).first() == Some(option)
                                && option_value(document, *option)
                                    .is_some_and(|value| value.is_empty())
                        }))
            }
            _ => value.is_empty(),
        };
    }
    if !value.is_empty() {
        state.type_mismatch = match type_name.as_str() {
            "email" => {
                let multiple = attribute(attributes, "multiple").is_some();
                let valid = |address: &str| {
                    let Some((local, domain)) = address.rsplit_once('@') else {
                        return false;
                    };
                    !local.is_empty()
                        && !domain.is_empty()
                        && domain.contains('.')
                        && !address.chars().any(char::is_whitespace)
                };
                if multiple {
                    value.split(',').any(|address| !valid(address.trim()))
                } else {
                    !valid(&value)
                }
            }
            "url" => {
                !(value.starts_with("http://")
                    || value.starts_with("https://")
                    || value.starts_with("ftp://"))
                    || value.chars().any(char::is_whitespace)
            }
            _ => false,
        };
        if matches!(type_name.as_str(), "number" | "range") {
            let parsed = value
                .parse::<f64>()
                .ok()
                .filter(|number| number.is_finite());
            if parsed.is_none() {
                state.bad_input = true;
            } else if let Some(number) = parsed {
                state.range_underflow = attribute(attributes, "min")
                    .and_then(|raw| raw.parse::<f64>().ok())
                    .is_some_and(|min| number < min);
                state.range_overflow = attribute(attributes, "max")
                    .and_then(|raw| raw.parse::<f64>().ok())
                    .is_some_and(|max| number > max);
                if let Some(step) = attribute(attributes, "step")
                    .filter(|step| *step != "any")
                    .and_then(|raw| raw.parse::<f64>().ok())
                    .filter(|step| *step > 0.0)
                {
                    let base = attribute(attributes, "min")
                        .and_then(|raw| raw.parse::<f64>().ok())
                        .unwrap_or(0.0);
                    let quotient = (number - base) / step;
                    state.step_mismatch = (quotient - quotient.round()).abs() > 1e-9;
                }
            }
        }
    }
    state
}

/// Construct the successful-control entry list for a form, optionally
/// including the submitter. The returned entries preserve tree order and
/// duplicate names, as FormData does.
pub fn form_entries(
    document: &Document,
    form: NodeId,
    submitter: Option<NodeId>,
) -> Vec<FormEntry> {
    form_entries_with_values(document, form, submitter, &[])
}

pub fn form_entries_with_values(
    document: &Document,
    form: NodeId,
    submitter: Option<NodeId>,
    values: &[(NodeId, String)],
) -> Vec<FormEntry> {
    form_entries_with_values_and_selectedness(document, form, submitter, values, &[])
}

pub fn form_entries_with_values_and_selectedness(
    document: &Document,
    form: NodeId,
    submitter: Option<NodeId>,
    values: &[(NodeId, String)],
    selectedness: &[(NodeId, bool)],
) -> Vec<FormEntry> {
    form_entries_with_values_selectedness_and_files(
        document,
        form,
        submitter,
        values,
        selectedness,
        &[],
    )
}

pub fn form_entries_with_values_selectedness_and_files(
    document: &Document,
    form: NodeId,
    submitter: Option<NodeId>,
    values: &[(NodeId, String)],
    selectedness: &[(NodeId, bool)],
    files: &[(NodeId, Vec<FormFile>)],
) -> Vec<FormEntry> {
    let mut entries = Vec::new();
    for node in form_controls(document, form) {
        let Some((tag, attributes)) = element(document, node) else {
            continue;
        };
        if !matches!(tag, "input" | "textarea" | "select" | "button") || is_disabled(document, node)
        {
            continue;
        }
        let Some(name) = attribute(attributes, "name").filter(|name| !name.is_empty()) else {
            continue;
        };
        let input_type = attribute(attributes, "type")
            .unwrap_or(if tag == "button" { "submit" } else { "text" })
            .to_ascii_lowercase();
        if matches!(input_type.as_str(), "button" | "reset" | "image")
            || (matches!(input_type.as_str(), "submit") && submitter != Some(node))
        {
            continue;
        }
        if input_type == "file" {
            if let Some((_, selected_files)) = files.iter().find(|(id, _)| *id == node) {
                entries.extend(
                    selected_files
                        .iter()
                        .cloned()
                        .map(|file| FormEntry::file(name, file)),
                );
            } else {
                entries.push(FormEntry::file(
                    name,
                    FormFile {
                        name: String::new(),
                        media_type: String::new(),
                        last_modified: 0,
                        bytes: Arc::from([]),
                    },
                ));
            }
            continue;
        }
        if matches!(input_type.as_str(), "checkbox" | "radio")
            && attribute(attributes, "checked").is_none()
        {
            continue;
        }
        if tag == "select" {
            let selected = selected_option_ids_with(document, node, selectedness);
            for option in selected {
                let Some((_, option_attributes)) = element(document, option) else {
                    continue;
                };
                if option_disabled(document, option)
                    || attribute(option_attributes, "disabled").is_some()
                {
                    continue;
                }
                let value = option_value(document, option).unwrap_or_default();
                entries.push(FormEntry::text(name, value));
            }
            continue;
        }
        let value = if tag == "textarea" {
            values
                .iter()
                .find(|(id, _)| *id == node)
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| control_value(document, node, tag, attributes))
        } else if matches!(input_type.as_str(), "checkbox" | "radio") {
            attribute(attributes, "value").unwrap_or("on").to_owned()
        } else {
            values
                .iter()
                .find(|(id, _)| *id == node)
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| control_value(document, node, tag, attributes))
        };
        entries.push(FormEntry::text(name, value));
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    #[test]
    fn constraint_validation_handles_required_types_and_numeric_ranges() {
        let document = html::parse("<form><input id='required' required><input id='email' type='email' value='bad'><input id='number' type='number' min='2' max='8' step='2' value='5'></form>", 64).unwrap();
        let input = |id: &str| {
            crate::selector::query_selector(&document, document.root(), &alloc::format!("#{id}"))
                .unwrap()
                .unwrap()
        };
        assert!(validity(&document, input("required"), "").value_missing);
        assert!(validity(&document, input("email"), "").type_mismatch);
        let numeric = validity(&document, input("number"), "");
        assert!(numeric.step_mismatch);
        assert!(numeric.valid() == false);
    }

    #[test]
    fn successful_entries_preserve_order_and_skip_unsuccessful_controls() {
        let document = html::parse("<form id='f'><input name='a' value='1'><input name='off' disabled value='x'><input type='checkbox' name='c'><input type='checkbox' name='c' checked><select name='s'><option value='first'>First</option><option selected>Second</option></select><button name='go' value='yes'>Go</button></form>", 64).unwrap();
        let form = crate::selector::query_selector(&document, document.root(), "#f")
            .unwrap()
            .unwrap();
        let button = crate::selector::query_selector(&document, document.root(), "button")
            .unwrap()
            .unwrap();
        let entries = form_entries(&document, form, Some(button));
        assert_eq!(
            entries,
            vec![
                FormEntry::text("a", "1"),
                FormEntry::text("c", "on"),
                FormEntry::text("s", "Second"),
                FormEntry::text("go", "yes"),
            ]
        );
    }

    #[test]
    fn file_controls_preserve_file_metadata_and_bytes_in_entry_list() {
        let document = html::parse(
            "<form id='f'><input type='file' name='upload'><input name='note' value='ok'></form>",
            64,
        )
        .unwrap();
        let form = crate::selector::query_selector(&document, document.root(), "#f")
            .unwrap()
            .unwrap();
        let input = crate::selector::query_selector(&document, document.root(), "input[type=file]")
            .unwrap()
            .unwrap();
        let selected = FormFile {
            name: "report.txt".into(),
            media_type: "text/plain".into(),
            last_modified: 1234,
            bytes: Arc::from(&b"hello"[..]),
        };
        let entries = form_entries_with_values_selectedness_and_files(
            &document,
            form,
            None,
            &[],
            &[],
            &[(input, vec![selected.clone()])],
        );
        assert_eq!(entries[0], FormEntry::file("upload", selected));
        assert_eq!(entries[1], FormEntry::text("note", "ok"));
    }

    #[test]
    fn select_selectedness_value_and_reset_defaults_share_dom_tree() {
        let mut document = html::parse(
            "<form id='f'><select id='s' name='choice'><option value='a'>A</option><option value='b' selected>B</option><option value='c'>C</option></select><select id='m' name='many' multiple><option value='x' selected>X</option><option value='y' selected>Y</option></select><textarea id='t' name='note'>original</textarea></form>",
            64,
        ).unwrap();
        let id = |document: &Document, selector: &str| {
            crate::selector::query_selector(document, document.root(), selector)
                .unwrap()
                .unwrap()
        };
        let form = id(&document, "#f");
        let select = id(&document, "#s");
        let multi = id(&document, "#m");
        let textarea = id(&document, "#t");
        assert_eq!(select_value(&document, select), "b");
        assert_eq!(selected_index(&document, select), 1);
        assert_eq!(
            default_value(&document, textarea).as_deref(),
            Some("original")
        );
        let default_selected = select_options(&document, select).into_iter()
            .chain(select_options(&document, multi))
            .map(|option| (option, matches!(document.kind(option), Ok(NodeKind::Element { attributes, .. }) if attribute(attributes, "selected").is_some())))
            .collect::<Vec<_>>();

        set_select_value(&mut document, select, "c").unwrap();
        set_selected_index(&mut document, multi, 1).unwrap();
        assert_eq!(select_value(&document, select), "c");
        assert_eq!(selected_index(&document, multi), 1);
        assert_eq!(
            form_entries(&document, form, None),
            vec![
                FormEntry::text("choice", "c"),
                FormEntry::text("many", "y"),
                FormEntry::text("note", "original"),
            ]
        );

        for (option, selected) in default_selected {
            set_option_selected(&mut document, option, selected).unwrap();
        }
        assert_eq!(select_value(&document, select), "b");
        assert_eq!(select_value(&document, multi), "x");

        let options = select_options(&document, select);
        let cleared = options
            .iter()
            .map(|option| (*option, false))
            .collect::<Vec<_>>();
        assert_eq!(select_value_with(&document, select, &cleared), "");
        assert_eq!(selected_index_with(&document, select, &cleared), -1);
        let entries =
            form_entries_with_values_and_selectedness(&document, form, None, &[], &cleared);
        assert!(!entries.iter().any(|entry| entry.name == "choice"));
    }
}
