//! Host-testable HTML form constraint and entry-list algorithms.
//!
//! These helpers intentionally consume the shared DOM tree. The JavaScript
//! adapter can expose them from the element classes without maintaining a
//! second control tree.
use crate::{Document, Error, Name, Namespace, NodeId, NodeKind};
use alloc::{
    borrow::ToOwned,
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};

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

/// Adapter-owned state that affects CSS selector matching but is not stored
/// in content attributes. The document resolver keeps this sparse: callers
/// ask only about the candidate node currently being matched.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FormSelectorState {
    pub checkedness: Option<bool>,
    pub selectedness: Option<bool>,
    /// Resolved selected option for a single-select containing the candidate.
    /// `Some(None)` means the select currently has no selected option.
    pub single_select_option: Option<Option<NodeId>>,
    /// Whether the UA has recorded a significant user interaction with this
    /// control for :user-valid/:user-invalid matching.
    pub user_validity_interacted: Option<bool>,
    /// Whether this control is currently presenting its placeholder, resolved
    /// from sparse host value state without copying the value into the DOM.
    pub placeholder_shown: Option<bool>,
    /// Direction derived from a host-owned live value for `dir=auto` controls.
    /// `Some(Ltr)` also records an empty/directionless live value, preventing
    /// fallback to a stale content attribute value.
    pub auto_value_directionality: Option<crate::directionality::Direction>,
}

/// The form-control pseudo-classes whose matching depends on HTML form state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum FormStatePseudo {
    Disabled,
    Enabled,
    Checked,
    Required,
    Optional,
    ReadOnly,
    ReadWrite,
    Default,
    UserValid,
    UserInvalid,
    PlaceholderShown,
}

impl FormStatePseudo {
    pub(crate) const ALL: [Self; 11] = [
        Self::Disabled,
        Self::Enabled,
        Self::Checked,
        Self::Required,
        Self::Optional,
        Self::ReadOnly,
        Self::ReadWrite,
        Self::Default,
        Self::UserValid,
        Self::UserInvalid,
        Self::PlaceholderShown,
    ];

    pub(crate) const fn bit(self) -> u16 {
        1u16 << self as u8
    }
}

/// Compact set of the form-state tests attached to one selector compound.
/// Repeated pseudos share a bit while their individual specificity is still
/// accounted for by the selector parser.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct FormStateSet(u16);

impl FormStateSet {
    pub(crate) const EMPTY: Self = Self(0);

    pub(crate) fn insert(&mut self, pseudo: FormStatePseudo) {
        self.0 |= pseudo.bit();
    }

    pub(crate) const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub(crate) const fn contains(self, pseudo: FormStatePseudo) -> bool {
        self.0 & pseudo.bit() != 0
    }

    pub(crate) const fn is_empty(self) -> bool {
        self.0 == 0
    }
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

/// Borrowed live form state supplied by an embedder while matching selectors
/// or computing validity. The shared algorithms consult only the nodes they
/// inspect and do not require copying an adapter's control-state tables.
pub trait ValidityStateView {
    fn value_override(&self, _node: NodeId) -> Option<&str> {
        None
    }

    fn custom_message(&self, _node: NodeId) -> Option<&str> {
        None
    }

    fn user_edited(&self, _node: NodeId) -> bool {
        false
    }

    fn user_validity_interacted(&self, _node: NodeId) -> bool {
        false
    }

    fn selectedness(&self, _option: NodeId) -> Option<bool> {
        None
    }

    fn checkedness(&self, _control: NodeId) -> Option<bool> {
        None
    }

    fn has_selected_files(&self, _input: NodeId) -> bool {
        false
    }
}

/// The shared DOM's attribute-backed defaults, used by callers without an
/// embedder-owned live-control state.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoValidityOverrides;

impl ValidityStateView for NoValidityOverrides {}

struct SliceValidityStateView<'a> {
    node: NodeId,
    value: Option<&'a str>,
    custom_message: &'a str,
    user_edited: bool,
    selectedness: &'a [(NodeId, bool)],
    checkedness: &'a [(NodeId, bool)],
}

impl ValidityStateView for SliceValidityStateView<'_> {
    fn value_override(&self, node: NodeId) -> Option<&str> {
        (node == self.node).then_some(self.value).flatten()
    }

    fn custom_message(&self, node: NodeId) -> Option<&str> {
        (node == self.node).then_some(self.custom_message)
    }

    fn user_edited(&self, node: NodeId) -> bool {
        node == self.node && self.user_edited
    }

    fn selectedness(&self, option: NodeId) -> Option<bool> {
        self.selectedness
            .iter()
            .find(|(node, _)| *node == option)
            .map(|(_, selected)| *selected)
    }

    fn checkedness(&self, control: NodeId) -> Option<bool> {
        self.checkedness
            .iter()
            .find(|(node, _)| *node == control)
            .map(|(_, checked)| *checked)
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

/// Encode a form entry snapshot as UTF-8 application/x-www-form-urlencoded.
/// Names, text values and file names normalize newlines to CRLF. File contents
/// are never copied. Measure first so an oversized body fails before allocation
/// and an accepted body needs only one output allocation.
pub fn encode_form_urlencoded(entries: &[FormEntry], max_bytes: usize) -> Result<String, Error> {
    let mut length = 0usize;
    for (index, entry) in entries.iter().enumerate() {
        length = lumen_common::limits::size::sum(length, 1 + usize::from(index != 0), max_bytes)
            .map_err(|_| Error::LimitExceeded)?;
        let value = match &entry.value {
            FormEntryValue::Text(value) => value.as_str(),
            FormEntryValue::File(file) => file.name.as_str(),
        };
        for input in [entry.name.as_str(), value] {
            for byte in NormalizedFormBytes::new(input) {
                length = lumen_common::limits::size::sum(
                    length,
                    if form_urlencoded_literal(byte) || byte == b' ' {
                        1
                    } else {
                        3
                    },
                    max_bytes,
                )
                .map_err(|_| Error::LimitExceeded)?;
            }
        }
    }
    let mut output = String::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| Error::LimitExceeded)?;
    for (index, entry) in entries.iter().enumerate() {
        if index != 0 {
            output.push('&');
        }
        let value = match &entry.value {
            FormEntryValue::Text(value) => value.as_str(),
            FormEntryValue::File(file) => file.name.as_str(),
        };
        for (part, input) in [entry.name.as_str(), value].into_iter().enumerate() {
            if part != 0 {
                output.push('=');
            }
            for byte in NormalizedFormBytes::new(input) {
                if byte == b' ' {
                    output.push('+');
                } else if form_urlencoded_literal(byte) {
                    output.push(byte as char);
                } else {
                    lumen_common::codec::push_percent_escape(&mut output, byte);
                }
            }
        }
    }
    Ok(output)
}

fn form_urlencoded_literal(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'*' | b'-' | b'.' | b'_')
}

/// Encode a form snapshot with the selected WHATWG output encoding.
/// Retain the allocation-free UTF-8 measurement fast path; legacy encodings
/// stream through fixed stack scratch and allocate only the final query.
pub fn encode_form_urlencoded_with_encoding(
    entries: &[FormEntry],
    encoding: &str,
    max_bytes: usize,
) -> Result<String, Error> {
    let encoding =
        lumen_common::encoding::canonical_output_label(encoding).map_err(|_| Error::WrongKind)?;
    if encoding == "UTF-8" {
        return encode_form_urlencoded(entries, max_bytes);
    }
    let mut length = 0;
    visit_form_encoded_bytes(entries, encoding, max_bytes, &mut |byte, input| {
        length = lumen_common::limits::size::sum(
            length,
            if !input || form_urlencoded_literal(byte) || byte == b' ' {
                1
            } else {
                3
            },
            max_bytes,
        )
        .map_err(|_| Error::LimitExceeded)?;
        Ok(())
    })?;
    // The structural separators are emitted separately from encoded input.
    let mut output = String::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| Error::LimitExceeded)?;
    visit_form_encoded_bytes(entries, encoding, max_bytes, &mut |byte, input| {
        if !input {
            output.push(byte as char);
        } else if byte == b' ' {
            output.push('+');
        } else if form_urlencoded_literal(byte) {
            output.push(byte as char);
        } else {
            lumen_common::codec::push_percent_escape(&mut output, byte);
        }
        Ok(())
    })?;
    Ok(output)
}

fn visit_form_encoded_bytes(
    entries: &[FormEntry],
    encoding: &str,
    max_bytes: usize,
    emit: &mut impl FnMut(u8, bool) -> Result<(), Error>,
) -> Result<(), Error> {
    use lumen_common::encoding::{DecodeError, OutputEncoder};
    for (index, entry) in entries.iter().enumerate() {
        if index != 0 {
            emit(b'&', false)?;
        }
        let value = match &entry.value {
            FormEntryValue::Text(value) => value.as_str(),
            FormEntryValue::File(file) => file.name.as_str(),
        };
        for (part, input) in [entry.name.as_str(), value].into_iter().enumerate() {
            if part != 0 {
                emit(b'=', false)?;
            }
            let mut encoder =
                OutputEncoder::new(encoding, max_bytes).map_err(|_| Error::WrongKind)?;
            let mut input = NormalizedFormBytes::new(input).peekable();
            let mut scratch = [0u8; 4096];
            loop {
                let mut length = 0;
                while length < scratch.len() - 4 {
                    let Some(byte) = input.next() else {
                        break;
                    };
                    scratch[length] = byte;
                    length += 1;
                }
                // Normalization changes only ASCII. Complete the final UTF-8
                // scalar so every chunk is valid borrowed input to the codec.
                while input.peek().is_some_and(|byte| byte & 0xc0 == 0x80) {
                    scratch[length] = input.next().ok_or(Error::WrongKind)?;
                    length += 1;
                }
                let last = input.peek().is_none();
                let chunk =
                    core::str::from_utf8(&scratch[..length]).map_err(|_| Error::WrongKind)?;
                encoder
                    .write(chunk, last, &mut |bytes| {
                        for &byte in bytes {
                            emit(byte, true).map_err(|_| DecodeError::ResourceLimit)?;
                        }
                        Ok(())
                    })
                    .map_err(|_| Error::LimitExceeded)?;
                if last {
                    break;
                }
            }
        }
    }
    Ok(())
}

struct NormalizedFormBytes<'a> {
    bytes: &'a [u8],
    position: usize,
    pending_lf: bool,
}

impl<'a> NormalizedFormBytes<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            bytes: input.as_bytes(),
            position: 0,
            pending_lf: false,
        }
    }
}

impl Iterator for NormalizedFormBytes<'_> {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        if self.pending_lf {
            self.pending_lf = false;
            return Some(b'\n');
        }
        let byte = *self.bytes.get(self.position)?;
        self.position += 1;
        if matches!(byte, b'\r' | b'\n') {
            if byte == b'\r' && self.bytes.get(self.position) == Some(&b'\n') {
                self.position += 1;
            }
            self.pending_lf = true;
            Some(b'\r')
        } else {
            Some(byte)
        }
    }
}

/// Borrowed live control state used while building one form's entry list.
/// Implementations are queried only for controls/options reached by that
/// form's tree-order traversal, so adapters can keep their sparse state maps
/// in place instead of copying every control in the document.
pub trait FormEntryStateView {
    fn value_for_control(&self, _node: NodeId) -> Option<&str> {
        None
    }

    fn selected_for_option(&self, _option: NodeId) -> Option<bool> {
        None
    }

    fn checked_for_control(&self, _control: NodeId) -> Option<bool> {
        None
    }

    fn files_for_control(&self, _control: NodeId) -> Option<&[FormFile]> {
        None
    }
}

struct SliceFormEntryStateView<'a> {
    values: &'a [(NodeId, String)],
    selectedness: &'a [(NodeId, bool)],
    checkedness: &'a [(NodeId, bool)],
    files: &'a [(NodeId, Vec<FormFile>)],
}

impl FormEntryStateView for SliceFormEntryStateView<'_> {
    fn value_for_control(&self, node: NodeId) -> Option<&str> {
        self.values
            .iter()
            .find(|(id, _)| *id == node)
            .map(|(_, value)| value.as_str())
    }

    fn selected_for_option(&self, option: NodeId) -> Option<bool> {
        self.selectedness
            .iter()
            .find(|(id, _)| *id == option)
            .map(|(_, selected)| *selected)
    }

    fn checked_for_control(&self, control: NodeId) -> Option<bool> {
        self.checkedness
            .iter()
            .find(|(id, _)| *id == control)
            .map(|(_, checked)| *checked)
    }

    fn files_for_control(&self, control: NodeId) -> Option<&[FormFile]> {
        self.files
            .iter()
            .find(|(id, _)| *id == control)
            .map(|(_, files)| files.as_slice())
    }
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
    pub encoding: String,
    pub entries: Vec<FormEntry>,
}

/// Normalize the HTML form method enumerated value used by form and submitter
/// reflection as well as form submission. Unknown values use the `get` state.
pub fn normalized_form_method(value: &str) -> &'static str {
    let value = value.trim();
    if value.eq_ignore_ascii_case("post") {
        "post"
    } else if value.eq_ignore_ascii_case("dialog") {
        "dialog"
    } else {
        "get"
    }
}

/// Normalize the HTML form encoding enumerated value used by form and
/// submitter reflection as well as form submission.
pub fn normalized_form_enctype(value: &str) -> &'static str {
    let value = value.trim();
    if value.eq_ignore_ascii_case("text/plain") {
        "text/plain"
    } else if value.eq_ignore_ascii_case("multipart/form-data") {
        "multipart/form-data"
    } else {
        "application/x-www-form-urlencoded"
    }
}

pub fn submission_metadata(
    document: &Document,
    form: NodeId,
    submitter: Option<NodeId>,
    entries: Vec<FormEntry>,
) -> Option<FormSubmission> {
    submission_metadata_with_encoding(document, form, submitter, entries, "UTF-8")
}

/// Pick the first recognized accept-charset label, or use the document's
/// encoding when the attribute is absent. Output-incompatible encodings use UTF-8.
pub fn pick_form_encoding(accept_charset: Option<&str>, document_encoding: &str) -> &'static str {
    let label = if let Some(labels) = accept_charset {
        labels
            .split_ascii_whitespace()
            .find_map(|label| lumen_common::encoding::canonical_document_label(label).ok())
            .unwrap_or("UTF-8")
    } else {
        document_encoding
    };
    lumen_common::encoding::canonical_output_label(label).unwrap_or("UTF-8")
}

pub fn submission_metadata_with_encoding(
    document: &Document,
    form: NodeId,
    submitter: Option<NodeId>,
    entries: Vec<FormEntry>,
    document_encoding: &str,
) -> Option<FormSubmission> {
    element(document, form)?;
    let submitter = submitter.filter(|node| {
        is_submit_button(document, *node) && form_owner(document, *node) == Some(form)
    });
    let overridden = |name: &str| {
        submitter
            .and_then(|node| attribute(document, node, name))
            .filter(|value| !value.is_empty())
    };
    let attr = |name: &str| attribute(document, form, name).unwrap_or("");
    let method = normalized_form_method(overridden("formmethod").unwrap_or_else(|| attr("method")));
    let enctype =
        normalized_form_enctype(overridden("formenctype").unwrap_or_else(|| attr("enctype")));
    Some(FormSubmission {
        action: overridden("formaction")
            .unwrap_or_else(|| attr("action"))
            .to_owned(),
        method: method.to_owned(),
        enctype: enctype.to_owned(),
        target: overridden("formtarget")
            .unwrap_or_else(|| attr("target"))
            .to_owned(),
        encoding: pick_form_encoding(
            attribute(document, form, "accept-charset"),
            document_encoding,
        )
        .to_owned(),
        entries,
    })
}

pub fn validation_bypassed(document: &Document, form: NodeId, submitter: Option<NodeId>) -> bool {
    element(document, form).is_some() && attribute(document, form, "novalidate").is_some()
        || submitter.is_some_and(|node| {
            is_submit_button(document, node)
                && form_owner(document, node) == Some(form)
                && attribute(document, node, "formnovalidate").is_some()
        })
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

/// Find a form-associated element's owner, honoring explicit `form=id`
/// before ancestor association for controls. A missing referenced form leaves
/// the control unassociated; `img` elements use only nearest-ancestor
/// association for the legacy form named-property algorithm.
pub fn form_owner(document: &Document, control: NodeId) -> Option<NodeId> {
    let tag = element(document, control).map(|(tag, _)| tag)?;
    // `img` is not a listed form element, but the legacy form named-property
    // algorithm associates descendant images with their nearest ancestor
    // form. Its `form` content attribute is ignored.
    if tag == "img" {
        return ancestor_form_owner(document, control);
    }
    if !matches!(
        tag,
        "input" | "textarea" | "select" | "button" | "fieldset" | "output" | "object"
    ) {
        return None;
    }
    if let Some(id) = attribute(document, control, "form") {
        if id.is_empty() {
            return None;
        }
        let root = tree_root(document, control);
        let first = if matches!(document.kind(root), Ok(NodeKind::Element { .. }))
            && attribute(document, root, "id") == Some(id)
        {
            Some(root)
        } else {
            crate::selector::get_element_by_id(document, root, id)
                .ok()
                .flatten()
        }?;
        return element(document, first)
            .is_some_and(|(tag, _)| tag == "form")
            .then_some(first);
    }
    ancestor_form_owner(document, control)
}

fn ancestor_form_owner(document: &Document, node: NodeId) -> Option<NodeId> {
    let mut parent = document.parent(node).ok().flatten();
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
    let mut controls = Vec::new();
    let _ = for_each_form_control(document, form, |node| {
        controls.push(node);
        true
    });
    controls
}

/// Visit form-associated controls in tree order without materializing a
/// temporary collection. A `false` callback result stops the traversal.
pub fn for_each_form_control(
    document: &Document,
    form: NodeId,
    mut visit: impl FnMut(NodeId) -> bool,
) -> Result<(), Error> {
    for_each_associated_form_element(document, form, false, |node, _| visit(node))
}

/// Visit the current `HTMLFormControlsCollection` members in tree order.
/// The traversal uses constant auxiliary storage and includes controls whose
/// explicit `form` attribute points to this form from elsewhere in its tree.
/// Image submit buttons are excluded, as required by the listed-elements set.
pub fn for_each_form_element(
    document: &Document,
    form: NodeId,
    visit: impl FnMut(NodeId, usize) -> bool,
) -> Result<(), Error> {
    for_each_associated_form_element(document, form, true, visit)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormNamedElementKind {
    Listed,
    Image,
    Other,
}

/// Visit HTML elements in the form's tree root in tree order, classifying
/// current named-property candidates. `Other` is useful to place a past-name
/// entry at its element's correct relative position after it ceases to be a
/// listed element (for example, when an input changes to type=image).
pub fn for_each_form_named_property_element(
    document: &Document,
    form: NodeId,
    mut visit: impl FnMut(NodeId, FormNamedElementKind) -> bool,
) -> Result<(), Error> {
    let root = tree_root(document, form);
    let mut current = document.first_child(root)?;
    while let Some(node) = current {
        let kind = element(document, node).map_or(FormNamedElementKind::Other, |(tag, _)| {
            if form_owner(document, node) != Some(form) {
                FormNamedElementKind::Other
            } else if tag == "img" {
                FormNamedElementKind::Image
            } else if is_listed_form_element(document, node, tag) {
                FormNamedElementKind::Listed
            } else {
                FormNamedElementKind::Other
            }
        });
        if !visit(node, kind) {
            return Ok(());
        }
        current = crate::selector::next_descendant(document, root, node)?;
    }
    Ok(())
}

/// Visit the current legacy form named-property candidates in tree order.
/// The boolean is true for an `img`; these are considered only after listed
/// elements by the value lookup algorithm, but their names are interleaved by
/// tree order when producing supported property names. Image elements remain
/// excluded from the indexed/listed-elements collections.
pub fn for_each_form_named_element(
    document: &Document,
    form: NodeId,
    mut visit: impl FnMut(NodeId, bool) -> bool,
) -> Result<(), Error> {
    for_each_form_named_property_element(document, form, |node, kind| match kind {
        FormNamedElementKind::Listed => visit(node, false),
        FormNamedElementKind::Image => visit(node, true),
        FormNamedElementKind::Other => true,
    })
}

/// Visit a form's legacy descendant-image candidates in tree order without
/// traversing unrelated parts of the document tree.
pub fn for_each_form_named_image(
    document: &Document,
    form: NodeId,
    mut visit: impl FnMut(NodeId) -> bool,
) -> Result<(), Error> {
    let mut current = crate::selector::next_descendant(document, form, form)?;
    while let Some(node) = current {
        if element(document, node).is_some_and(|(tag, _)| tag == "img")
            && form_owner(document, node) == Some(form)
            && !visit(node)
        {
            return Ok(());
        }
        current = crate::selector::next_descendant(document, form, node)?;
    }
    Ok(())
}

/// Count the live form-elements collection without materializing its members.
pub fn form_element_count(document: &Document, form: NodeId) -> Result<usize, Error> {
    let mut count = 0;
    for_each_form_element(document, form, |_, _| {
        count += 1;
        true
    })?;
    Ok(count)
}

/// Return one live form-elements collection member by tree-order index.
pub fn form_element_at(
    document: &Document,
    form: NodeId,
    wanted_index: usize,
) -> Result<Option<NodeId>, Error> {
    let mut found = None;
    for_each_form_element(document, form, |node, index| {
        if index == wanted_index {
            found = Some(node);
            false
        } else {
            true
        }
    })?;
    Ok(found)
}

/// Visit listed form-associated descendants of a fieldset in tree order.
/// Unlike `HTMLFormElement.elements`, this collection is rooted at the
/// fieldset and does not follow `form=` ownership outside its subtree. Inputs
/// of type image remain included, as the fieldset collection is defined by
/// the listed-element set rather than the form's indexed-elements filter.
pub fn for_each_fieldset_element(
    document: &Document,
    fieldset: NodeId,
    mut visit: impl FnMut(NodeId, usize) -> bool,
) -> Result<(), Error> {
    let mut index = 0;
    let mut current = crate::selector::next_descendant(document, fieldset, fieldset)?;
    while let Some(node) = current {
        let is_member = element(document, node)
            .is_some_and(|(name, _)| is_fieldset_listed_element(document, node, name));
        if is_member {
            if !visit(node, index) {
                return Ok(());
            }
            index += 1;
        }
        current = crate::selector::next_descendant(document, fieldset, node)?;
    }
    Ok(())
}

pub fn fieldset_element_count(document: &Document, fieldset: NodeId) -> Result<usize, Error> {
    let mut count = 0;
    for_each_fieldset_element(document, fieldset, |_, _| {
        count += 1;
        true
    })?;
    Ok(count)
}

pub fn fieldset_element_at(
    document: &Document,
    fieldset: NodeId,
    wanted_index: usize,
) -> Result<Option<NodeId>, Error> {
    let mut found = None;
    for_each_fieldset_element(document, fieldset, |node, index| {
        if index == wanted_index {
            found = Some(node);
            false
        } else {
            true
        }
    })?;
    Ok(found)
}

fn for_each_associated_form_element(
    document: &Document,
    form: NodeId,
    listed_only: bool,
    mut visit: impl FnMut(NodeId, usize) -> bool,
) -> Result<(), Error> {
    let root = tree_root(document, form);
    let mut current = document.first_child(root)?;
    let mut index = 0;
    while let Some(node) = current {
        let is_member = form_owner(document, node) == Some(form)
            && element(document, node).is_some_and(|(name, _)| {
                is_form_associated_control(name, listed_only, document, node)
            });
        if is_member {
            if !visit(node, index) {
                return Ok(());
            }
            index += 1;
        }
        current = crate::selector::next_descendant(document, root, node)?;
    }
    Ok(())
}

fn is_form_associated_control(
    name: &str,
    listed_only: bool,
    document: &Document,
    node: NodeId,
) -> bool {
    if listed_only {
        is_listed_form_element(document, node, name)
    } else {
        matches!(
            name,
            "input" | "textarea" | "select" | "button" | "fieldset" | "output"
        )
    }
}

fn is_listed_form_element(document: &Document, node: NodeId, name: &str) -> bool {
    is_listed_form_element_with_image_policy(document, node, name, false)
}

fn is_fieldset_listed_element(document: &Document, node: NodeId, name: &str) -> bool {
    is_listed_form_element_with_image_policy(document, node, name, true)
}

fn is_listed_form_element_with_image_policy(
    document: &Document,
    node: NodeId,
    name: &str,
    include_image_input: bool,
) -> bool {
    matches!(
        name,
        "button" | "fieldset" | "object" | "output" | "select" | "textarea"
    ) || name == "input"
        && (include_image_input
            || !attribute(document, node, "type")
                .unwrap_or("text")
                .eq_ignore_ascii_case("image"))
}

/// Other radios in the control's radio button group, in tree order.
pub fn radio_group_members(document: &Document, node: NodeId) -> Vec<NodeId> {
    if !element(document, node).is_some_and(|(tag, _)| tag == "input")
        || !attribute(document, node, "type")
            .unwrap_or("text")
            .eq_ignore_ascii_case("radio")
    {
        return Vec::new();
    }
    let Some(name) = attribute(document, node, "name").filter(|name| !name.is_empty()) else {
        return Vec::new();
    };
    let owner = form_owner(document, node);
    let root = tree_root(document, node);
    let mut members = Vec::new();
    let mut current = Some(root);
    while let Some(candidate) = current {
        if candidate != node
            && element(document, candidate).is_some_and(|(tag, _)| {
                tag == "input"
                    && attribute(document, candidate, "type")
                        .unwrap_or("text")
                        .eq_ignore_ascii_case("radio")
                    && attribute(document, candidate, "name") == Some(name)
                    && form_owner(document, candidate) == owner
            })
        {
            members.push(candidate);
        }
        current = crate::selector::next_descendant(document, root, candidate)
            .ok()
            .flatten();
    }
    members
}

/// Return the root of the control's current tree. Form association also works
/// for a detached subtree, such as a script-created form before insertion.
fn tree_root(document: &Document, node: NodeId) -> NodeId {
    let mut root = node;
    while let Ok(Some(parent)) = document.parent(root) {
        root = parent;
    }
    root
}

fn attribute<'a>(document: &'a Document, node: NodeId, name: &str) -> Option<&'a str> {
    document
        .get_attribute_ns_ref(node, None, name)
        .ok()
        .flatten()
}

pub fn parse_nonnegative_integer(value: &str) -> Option<usize> {
    let value = value
        .trim_start_matches(|character| matches!(character, '\t' | '\n' | '\u{000C}' | '\r' | ' '));
    let digits = value.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0).then(|| value[..digits].parse().ok()).flatten()
}

fn element<'a>(document: &'a Document, node: NodeId) -> Option<(&'a str, &'a [(Name, String)])> {
    match document.kind(node).ok()? {
        NodeKind::Element {
            namespace: Namespace::Html,
            name,
            attributes,
            ..
        } => Some((crate::svg::local_name(name), attributes)),
        _ => None,
    }
}

fn text_content(document: &Document, root: NodeId) -> String {
    let mut text = String::new();
    if let Ok(NodeKind::Text(value) | NodeKind::CData(value)) = document.kind(root) {
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

/// The input states whose `value` IDL property is backed by a separate live
/// value, rather than by the `value` content attribute.
///
/// Keep this classification shared by the host-neutral form algorithms and
/// the JavaScript adapter. Unknown type keywords use the Text state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputValueMode {
    /// The IDL value directly reflects the content attribute.
    Default,
    /// The IDL value reflects the content attribute, defaulting to `"on"`.
    DefaultOn,
    /// The IDL value has a distinct dirty/live value.
    Value,
    /// The file input's IDL value is its filename mode, not the content value.
    Filename,
}

/// Normalized state of an HTML button's `type` attribute. Missing or invalid
/// values use the Submit state, as does an explicit ASCII-case-insensitive
/// `submit` keyword.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ButtonTypeState {
    Submit,
    Reset,
    Button,
}

/// Return the normalized button type state for an HTML button element.
pub fn button_type_state(document: &Document, node: NodeId) -> Option<ButtonTypeState> {
    if html_element_local_name(document, node)? != "button" {
        return None;
    }
    let raw = attribute(document, node, "type").unwrap_or("submit");
    Some(if raw.eq_ignore_ascii_case("reset") {
        ButtonTypeState::Reset
    } else if raw.eq_ignore_ascii_case("button") {
        ButtonTypeState::Button
    } else {
        ButtonTypeState::Submit
    })
}

/// Whether an HTML input or button is in a submit-button state.
pub fn is_submit_button(document: &Document, node: NodeId) -> bool {
    match html_element_local_name(document, node) {
        Some("button") => button_type_state(document, node) == Some(ButtonTypeState::Submit),
        Some("input") => matches!(input_type_state(document, node), "submit" | "image"),
        _ => false,
    }
}

/// The default action associated with implicit form submission from a text
/// control. The adapter is responsible for dispatching a click at a default
/// button (and therefore honoring disabled state, cancellation, and activation
/// behavior), or invoking submission when the form has no default button.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImplicitSubmission {
    ClickDefaultButton { form: NodeId, button: NodeId },
    SubmitForm(NodeId),
}

/// Determine the HTML implicit-submission action for an input that blocks it.
/// This walks the associated controls once with constant auxiliary storage.
/// Disabled text controls still count toward the no-submit-button blocker
/// rule; the default submit button is selected in form-control tree order.
pub fn implicit_submission(document: &Document, input: NodeId) -> Option<ImplicitSubmission> {
    if html_element_local_name(document, input) != Some("input")
        || !matches!(
            input_type_state(document, input),
            "text"
                | "search"
                | "tel"
                | "url"
                | "email"
                | "password"
                | "date"
                | "month"
                | "week"
                | "time"
                | "datetime-local"
                | "number"
        )
    {
        return None;
    }
    let form = form_owner(document, input)?;
    let mut default_button = None;
    let mut blocking_inputs = 0usize;
    let _ = for_each_form_control(document, form, |control| {
        if is_submit_button(document, control) {
            default_button = Some(control);
            return false;
        }
        if html_element_local_name(document, control) == Some("input")
            && matches!(
                input_type_state(document, control),
                "text"
                    | "search"
                    | "tel"
                    | "url"
                    | "email"
                    | "password"
                    | "date"
                    | "month"
                    | "week"
                    | "time"
                    | "datetime-local"
                    | "number"
            )
        {
            blocking_inputs = blocking_inputs.saturating_add(1);
        }
        true
    });
    if let Some(button) = default_button {
        Some(ImplicitSubmission::ClickDefaultButton { form, button })
    } else if blocking_inputs <= 1 {
        Some(ImplicitSubmission::SubmitForm(form))
    } else {
        None
    }
}

/// Return the local name of an HTML-namespace element.
///
/// Form algorithms only apply to HTML elements. Using the shared qualified-name
/// helper also handles elements created in the HTML namespace with a prefix,
/// without accidentally treating foreign-namespace elements as controls.
pub fn html_element_local_name(document: &Document, node: NodeId) -> Option<&str> {
    match document.kind(node).ok()? {
        NodeKind::Element {
            namespace: Namespace::Html,
            name,
            ..
        } => Some(crate::svg::local_name(name)),
        _ => None,
    }
}

pub fn input_value_mode(document: &Document, node: NodeId) -> Option<InputValueMode> {
    if html_element_local_name(document, node)? != "input" {
        return None;
    }
    let kind = attribute(document, node, "type").unwrap_or("text");
    Some(
        if ["hidden", "submit", "image", "reset", "button"]
            .iter()
            .any(|candidate| kind.eq_ignore_ascii_case(candidate))
        {
            InputValueMode::Default
        } else if ["checkbox", "radio"]
            .iter()
            .any(|candidate| kind.eq_ignore_ascii_case(candidate))
        {
            InputValueMode::DefaultOn
        } else if kind.eq_ignore_ascii_case("file") {
            InputValueMode::Filename
        } else {
            InputValueMode::Value
        },
    )
}

/// Whether the text-selection APIs apply to this control.
///
/// Textareas always support selections. Inputs support them only in the Text,
/// Search, Telephone, URL, and Password states; an unrecognized `type` value
/// is the Text state, as required by HTML.
pub fn supports_text_selection(document: &Document, node: NodeId) -> bool {
    match html_element_local_name(document, node) {
        Some("textarea") => true,
        Some("input") => {
            let kind = attribute(document, node, "type").unwrap_or("text");
            ![
                "hidden",
                "email",
                "datetime-local",
                "date",
                "month",
                "week",
                "time",
                "number",
                "range",
                "color",
                "checkbox",
                "radio",
                "file",
                "submit",
                "image",
                "reset",
                "button",
            ]
            .iter()
            .any(|unsupported| kind.eq_ignore_ascii_case(unsupported))
        }
        _ => false,
    }
}

fn control_value(document: &Document, node: NodeId, name: &str) -> String {
    if name == "input" {
        let kind = attribute(document, node, "type")
            .unwrap_or("text")
            .to_ascii_lowercase();
        let value = attribute(document, node, "value");
        return match input_value_mode(document, node).unwrap_or(InputValueMode::Value) {
            InputValueMode::Default => value.unwrap_or("").to_owned(),
            InputValueMode::DefaultOn => value.unwrap_or("on").to_owned(),
            InputValueMode::Filename => String::new(),
            InputValueMode::Value => sanitize_input_value_with_attributes(
                &kind,
                value.unwrap_or(""),
                attribute(document, node, "min"),
                attribute(document, node, "max"),
                attribute(document, node, "step"),
                value,
                attribute(document, node, "multiple").is_some(),
            ),
        };
    }
    if name == "textarea" {
        text_content(document, node)
    } else if let Some(value) = attribute(document, node, "value") {
        value.to_owned()
    } else {
        String::new()
    }
}

/// Apply input value sanitization for the numeric and date/time states used by
/// both the DOM value adapter and shared form algorithms.
pub fn sanitize_input_value(kind: &str, value: &str) -> String {
    sanitize_input_value_with_constraints(kind, value, None, None, None)
}

/// Type-aware value sanitization using the input's current numeric constraints.
/// This is shared by the DOM value adapter and default/current-value reads so
/// range defaults, clamping, and step rounding stay consistent.
pub fn sanitize_input_value_with_constraints(
    kind: &str,
    value: &str,
    min: Option<&str>,
    max: Option<&str>,
    step: Option<&str>,
) -> String {
    sanitize_input_value_with_attributes(kind, value, min, max, step, None, false)
}

pub fn sanitize_input_value_with_attributes(
    kind: &str,
    value: &str,
    min: Option<&str>,
    max: Option<&str>,
    step: Option<&str>,
    value_attribute: Option<&str>,
    multiple: bool,
) -> String {
    if kind == "range" {
        return range_sanitized_value(value, min, max, step, value_attribute).to_string();
    }
    if matches!(kind, "text" | "search" | "tel" | "password") {
        return strip_newlines(value);
    }
    if kind == "color" {
        return if is_simple_color(value) {
            value.to_ascii_lowercase()
        } else {
            "#000000".to_owned()
        };
    }
    if kind == "url" {
        return strip_newlines(value)
            .trim_matches(is_ascii_whitespace)
            .to_owned();
    }
    if kind == "email" {
        let value = strip_newlines(value);
        if multiple {
            return value
                .split(',')
                .map(|part| part.trim_matches(is_ascii_whitespace))
                .collect::<Vec<_>>()
                .join(",");
        }
        return value.trim_matches(is_ascii_whitespace).to_owned();
    }
    if matches!(
        kind,
        "number" | "date" | "month" | "week" | "time" | "datetime-local"
    ) && !value.is_empty()
        && input_value_number(value, kind).is_none()
    {
        String::new()
    } else if kind == "datetime-local" && !value.is_empty() {
        normalize_local_datetime(value).unwrap_or_default()
    } else {
        value.to_owned()
    }
}

fn is_simple_color(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 7 && bytes[0] == b'#' && bytes[1..].iter().all(u8::is_ascii_hexdigit)
}

fn is_ascii_whitespace(character: char) -> bool {
    matches!(character, '\t' | '\n' | '\u{000C}' | '\r' | ' ')
}

fn strip_newlines(value: &str) -> String {
    characters_without_newlines(value).collect()
}

/// Input value and placeholder sanitization share HTML's CR/LF stripping rule.
/// An iterator lets bounded rendering callers reserve only the output bytes.
pub(crate) fn characters_without_newlines(value: &str) -> impl Iterator<Item = char> + '_ {
    value
        .chars()
        .filter(|character| !matches!(character, '\n' | '\r'))
}

fn range_bounds(min: Option<&str>, max: Option<&str>) -> (f64, f64) {
    let minimum = min.and_then(parse_html_float).unwrap_or(0.0);
    let maximum = max.and_then(parse_html_float).unwrap_or(100.0);
    (minimum, maximum.max(minimum))
}

fn range_step(step: Option<&str>) -> Option<f64> {
    match step {
        Some(raw) if raw.eq_ignore_ascii_case("any") => None,
        Some(raw) => Some(
            parse_html_float(raw)
                .filter(|value| *value > 0.0)
                .unwrap_or(1.0),
        ),
        None => Some(1.0),
    }
}

fn range_step_base(min: Option<&str>, value_attribute: Option<&str>) -> f64 {
    min.and_then(parse_html_float)
        .or_else(|| value_attribute.and_then(parse_html_float))
        .unwrap_or(0.0)
}

fn range_default(
    min: Option<&str>,
    max: Option<&str>,
    step: Option<&str>,
    value_attribute: Option<&str>,
) -> f64 {
    let (minimum, maximum) = range_bounds(min, max);
    let midpoint = minimum / 2.0 + maximum / 2.0;
    range_round_and_clamp(
        midpoint,
        minimum,
        maximum,
        range_step(step),
        range_step_base(min, value_attribute),
    )
}

fn range_sanitized_value(
    value: &str,
    min: Option<&str>,
    max: Option<&str>,
    step: Option<&str>,
    value_attribute: Option<&str>,
) -> f64 {
    let (minimum, maximum) = range_bounds(min, max);
    let Some(value) = parse_html_float(value) else {
        return range_default(min, max, step, value_attribute);
    };
    range_round_and_clamp(
        value,
        minimum,
        maximum,
        range_step(step),
        range_step_base(min, value_attribute),
    )
}

fn step_quotient(value: f64, base: f64, step: f64) -> f64 {
    let difference = value - base;
    if difference.is_finite() {
        difference / step
    } else {
        value / step - base / step
    }
}

fn range_round_and_clamp(
    value: f64,
    minimum: f64,
    maximum: f64,
    step: Option<f64>,
    base: f64,
) -> f64 {
    let clamped = value.clamp(minimum, maximum);
    let Some(step) = step else {
        return clamped;
    };
    let lower = step_quotient(minimum, base, step).ceil();
    let upper = step_quotient(maximum, base, step).floor();
    if lower > upper {
        // Bounds can be narrower than one step and contain no grid point.
        return minimum;
    }
    let quotient = step_quotient(clamped, base, step);
    if !quotient.is_finite() {
        return clamped;
    }
    // HTML's nearest-step rule resolves exact ties toward positive infinity.
    let index = (quotient + 0.5).floor().clamp(lower, upper);
    index.mul_add(step, base).clamp(minimum, maximum)
}

fn normalize_local_datetime(value: &str) -> Option<String> {
    let (date, time) = value.split_once('T').or_else(|| value.split_once(' '))?;
    let mut parts = time.split(':');
    let hour = parts.next()?;
    let minute = parts.next()?;
    let seconds = parts.next();
    if parts.next().is_some() {
        return None;
    }
    let mut result = alloc::format!("{date}T{hour}:{minute}");
    if let Some(seconds) = seconds {
        let (whole, fraction) = seconds
            .split_once('.')
            .map_or((seconds, None), |(s, f)| (s, Some(f)));
        let whole_seconds: u8 = whole.parse().ok()?;
        let fraction = fraction.unwrap_or("");
        let fraction_value = if fraction.is_empty() {
            0
        } else {
            fraction.parse::<u16>().ok()?
        };
        if whole_seconds != 0 || fraction_value != 0 {
            result.push(':');
            result.push_str(whole);
            let fraction = fraction.trim_end_matches('0');
            if !fraction.is_empty() {
                result.push('.');
                result.push_str(fraction);
            }
        }
    }
    Some(result)
}

/// Reset default value for an input, textarea, or other value-bearing control.
pub fn default_value(document: &Document, node: NodeId) -> Option<String> {
    let (name, _) = element(document, node)?;
    if name == "input"
        && attribute(document, node, "type").is_some_and(|value| value.eq_ignore_ascii_case("file"))
    {
        return Some(String::new());
    }
    if name == "select" {
        return Some(select_value(document, node));
    }
    // A textarea's default value is its text content. A `value` attribute is
    // not part of HTMLTextAreaElement's value model.
    if name == "textarea" {
        return Some(text_content(document, node));
    }
    // An output's reset default is its descendant text content. Its live value
    // is also text content, but the DOM adapter tracks the optional
    // defaultValue override required by HTMLInputElement's value model.
    if name == "output" {
        return Some(text_content(document, node));
    }
    Some(control_value(document, node, name))
}

fn is_html_element_named(document: &Document, node: NodeId, wanted: &str) -> Result<bool, Error> {
    Ok(matches!(
        document.kind(node)?,
        NodeKind::Element {
            namespace: Namespace::Html,
            name,
            ..
        } if name.as_str() == wanted
    ))
}

fn option_walk_prunes_subtree(
    document: &Document,
    select: NodeId,
    node: NodeId,
) -> Result<bool, Error> {
    if is_html_element_named(document, node, "select")?
        || is_html_element_named(document, node, "hr")?
        || is_html_element_named(document, node, "option")?
        || is_html_element_named(document, node, "datalist")?
    {
        return Ok(true);
    }
    if !is_html_element_named(document, node, "optgroup")? {
        return Ok(false);
    }

    let mut ancestor = document.parent(node)?;
    while let Some(id) = ancestor {
        if id == select {
            return Ok(false);
        }
        if is_html_element_named(document, id, "optgroup")? {
            return Ok(true);
        }
        ancestor = document.parent(id)?;
    }
    Ok(false)
}

/// Visit a select's options in tree order without allocating a descendant or
/// option vector. Return `false` from the visitor to stop early.
pub fn for_each_select_option(
    document: &Document,
    select: NodeId,
    mut visit: impl FnMut(NodeId, usize) -> bool,
) -> Result<(), Error> {
    document.kind(select)?;
    if !is_html_element_named(document, select, "select")? {
        return Ok(());
    }

    let mut index = 0usize;
    let mut node = document.first_child(select)?;
    while let Some(id) = node {
        if is_html_element_named(document, id, "option")? {
            if !visit(id, index) {
                return Ok(());
            }
            index += 1;
        }
        node = if option_walk_prunes_subtree(document, select, id)? {
            crate::selector::next_after_subtree(document, select, id)?
        } else {
            crate::selector::next_descendant(document, select, id)?
        };
    }
    Ok(())
}

/// Find the nearest HTML select containing `node`, including `node` itself.
/// This is used by the DOM adapter after option-related tree mutations; it
/// follows ordinary parent links and therefore does not cross shadow roots.
pub fn select_ancestor(document: &Document, node: NodeId) -> Result<Option<NodeId>, Error> {
    let mut current = Some(node);
    while let Some(candidate) = current {
        if is_html_element_named(document, candidate, "select")? {
            return Ok(Some(candidate));
        }
        current = document.parent(candidate)?;
    }
    Ok(None)
}

/// Return whether a node and its ordinary descendants contain an HTML option.
/// The streaming walk avoids allocating a subtree snapshot for DOM insert and
/// remove hooks that may need to reset the containing select.
pub fn subtree_contains_select_option(document: &Document, root: NodeId) -> Result<bool, Error> {
    let mut found = false;
    for_each_option_in_subtree(document, root, |_, _| {
        found = true;
        false
    })?;
    Ok(found)
}

/// Visit HTML options in an ordinary subtree in tree order, including the
/// root when it is itself an option. This supports bounded mutation reactions
/// without materializing descendants.
pub fn for_each_option_in_subtree(
    document: &Document,
    root: NodeId,
    mut visit: impl FnMut(NodeId, usize) -> bool,
) -> Result<(), Error> {
    document.kind(root)?;
    let mut index = 0;
    if is_html_element_named(document, root, "option")? {
        if !visit(root, index) {
            return Ok(());
        }
        index += 1;
    }
    let mut current = crate::selector::next_descendant(document, root, root)?;
    while let Some(node) = current {
        if is_html_element_named(document, node, "option")? {
            if !visit(node, index) {
                return Ok(());
            }
            index += 1;
        }
        current = crate::selector::next_descendant(document, root, node)?;
    }
    Ok(())
}

/// Count a select's options in tree order with constant auxiliary storage.
pub fn select_option_count(document: &Document, select: NodeId) -> Result<usize, Error> {
    let mut count = 0usize;
    for_each_select_option(document, select, |_, _| {
        count += 1;
        true
    })?;
    Ok(count)
}

/// Find the first option whose null-namespace `id` or `name` matches a
/// collection's named lookup argument. The canonical option walk preserves
/// tree order without materializing the collection.
pub fn select_option_named_item(
    document: &Document,
    select: NodeId,
    name: &str,
) -> Result<Option<NodeId>, Error> {
    if name.is_empty() {
        return Ok(None);
    }
    let mut found = None;
    let mut error = None;
    for_each_select_option(document, select, |option, _| {
        let id_matches = match document.get_attribute_ns_ref(option, None, "id") {
            Ok(value) => value == Some(name),
            Err(value) => {
                error = Some(value);
                return false;
            }
        };
        let name_matches = match document.get_attribute_ns_ref(option, None, "name") {
            Ok(value) => value == Some(name),
            Err(value) => {
                error = Some(value);
                return false;
            }
        };
        if id_matches || name_matches {
            found = Some(option);
            return false;
        }
        true
    })?;
    if let Some(error) = error {
        return Err(error);
    }
    Ok(found)
}

/// Set the select's option-list length. Growth is preflighted against the
/// document's node budget before allocating any option; shrinking removes the
/// suffix in one tree-order pass, including options nested in optgroups.
pub fn resize_select_options(
    document: &mut Document,
    select: NodeId,
    length: usize,
) -> Result<(), Error> {
    if !is_html_element_named(document, select, "select")? {
        return Err(Error::WrongKind);
    }
    let current = select_option_count(document, select)?;
    if length == current {
        return Ok(());
    }
    if length > current {
        let added = length - current;
        if added > document.remaining_node_capacity() {
            return Err(Error::LimitExceeded);
        }
        for _ in current..length {
            let option = document.create(NodeKind::Element {
                namespace: Namespace::Html,
                name: "option".into(),
                attributes: Vec::new(),
            })?;
            document.append(select, option)?;
        }
        return Ok(());
    }

    let mut preserved = 0usize;
    let mut node = document.first_child(select)?;
    while let Some(id) = node {
        if is_html_element_named(document, id, "option")? {
            let next = crate::selector::next_after_subtree(document, select, id)?;
            if preserved >= length {
                document.remove(id)?;
            } else {
                preserved += 1;
            }
            node = next;
        } else {
            node = if option_walk_prunes_subtree(document, select, id)? {
                crate::selector::next_after_subtree(document, select, id)?
            } else {
                crate::selector::next_descendant(document, select, id)?
            };
        }
    }
    Ok(())
}

/// Remove one indexed option if present. Returns the detached option node so
/// host wrappers and selectedness state can retain its identity.
pub fn remove_select_option_at(
    document: &mut Document,
    select: NodeId,
    index: usize,
) -> Result<Option<NodeId>, Error> {
    let Some(option) = select_option_at(document, select, index)? else {
        return Ok(None);
    };
    document.remove(option)?;
    Ok(Some(option))
}

/// Return the select's display size as defined by HTML's size parsing rules.
/// An absent or invalid `size` uses the default for the multiple state.
pub fn select_display_size(document: &Document, select: NodeId) -> Option<usize> {
    let (name, _) = element(document, select)?;
    if name != "select" {
        return None;
    }
    if let Some(size) = attribute(document, select, "size").and_then(parse_nonnegative_integer) {
        return Some(size);
    }
    Some(if attribute(document, select, "multiple").is_some() {
        4
    } else {
        1
    })
}

/// Return one option by its current tree-order index without materializing the
/// rest of the collection.
pub fn select_option_at(
    document: &Document,
    select: NodeId,
    wanted_index: usize,
) -> Result<Option<NodeId>, Error> {
    let mut found = None;
    for_each_select_option(document, select, |node, index| {
        if index == wanted_index {
            found = Some(node);
            false
        } else {
            true
        }
    })?;
    Ok(found)
}

/// Visit every HTML option descendant of a datalist in tree order. Unlike a
/// select's option list, datalist options include options inside fallback
/// descendants such as a nested select.
pub fn for_each_datalist_option(
    document: &Document,
    datalist: NodeId,
    mut visit: impl FnMut(NodeId, usize) -> bool,
) -> Result<(), Error> {
    document.kind(datalist)?;
    if !is_html_element_named(document, datalist, "datalist")? {
        return Ok(());
    }
    let mut index = 0usize;
    let mut node = document.first_child(datalist)?;
    while let Some(id) = node {
        if is_html_element_named(document, id, "option")? {
            if !visit(id, index) {
                return Ok(());
            }
            index += 1;
        }
        node = crate::selector::next_descendant(document, datalist, id)?;
    }
    Ok(())
}

pub fn datalist_option_count(document: &Document, datalist: NodeId) -> Result<usize, Error> {
    let mut count = 0usize;
    for_each_datalist_option(document, datalist, |_, _| {
        count += 1;
        true
    })?;
    Ok(count)
}

pub fn datalist_option_at(
    document: &Document,
    datalist: NodeId,
    wanted_index: usize,
) -> Result<Option<NodeId>, Error> {
    let mut found = None;
    for_each_datalist_option(document, datalist, |option, index| {
        if index == wanted_index {
            found = Some(option);
            false
        } else {
            true
        }
    })?;
    Ok(found)
}

/// Resolve an input's `list` reference in its current tree, including a
/// detached root. The first matching ID wins; it must be an HTML datalist.
pub fn input_list(document: &Document, input: NodeId) -> Result<Option<NodeId>, Error> {
    if !is_html_element_named(document, input, "input")? {
        return Ok(None);
    }
    let Some(wanted) = attribute(document, input, "list") else {
        return Ok(None);
    };
    if wanted.is_empty() {
        return Ok(None);
    }
    let root = tree_root(document, input);
    let first = if matches!(document.kind(root)?, NodeKind::Element { .. })
        && attribute(document, root, "id") == Some(wanted)
    {
        Some(root)
    } else {
        crate::selector::get_element_by_id(document, root, wanted)?
    };
    let Some(first) = first else {
        return Ok(None);
    };
    Ok(is_html_element_named(document, first, "datalist")?.then_some(first))
}

/// Collect a select's options in tree order for callers whose result is a
/// materialized list. Live collection length/index reads should use the
/// streaming helpers above instead.
pub fn select_options(document: &Document, select: NodeId) -> Vec<NodeId> {
    let mut options = Vec::new();
    let _ = for_each_select_option(document, select, |node, _| {
        options.push(node);
        true
    });
    options
}

/// Option text in tree order, excluding HTML/SVG scripts and collapsing only
/// ASCII whitespace. Streams borrowed text chunks without a traversal buffer.
pub fn option_text(document: &Document, option: NodeId) -> Result<String, Error> {
    let mut cursor = document.first_child(option)?;
    let mut error = None;
    let parts = core::iter::from_fn(|| loop {
        let node = cursor?;
        let kind = match document.kind(node) {
            Ok(kind) => kind,
            Err(problem) => {
                error = Some(problem);
                cursor = None;
                return None;
            }
        };
        let skip = matches!(kind, NodeKind::Element { namespace: Namespace::Html | Namespace::Svg, name, .. }
            if crate::svg::local_name(name) == "script");
        cursor = match if skip {
            crate::selector::next_after_subtree(document, option, node)
        } else {
            crate::selector::next_descendant(document, option, node)
        } {
            Ok(next) => next,
            Err(problem) => {
                error = Some(problem);
                return None;
            }
        };
        if let NodeKind::Text(text) | NodeKind::CData(text) = kind {
            return Some(text.as_str());
        }
    });
    let text = lumen_common::scan::strip_and_collapse_ascii_whitespace(parts);
    match error {
        Some(problem) => Err(problem),
        None => Ok(text),
    }
}

/// Resolve an option's select through the same ordinary-tree boundaries used
/// by the select option list. No ancestor vector or cached ownership is needed.
pub fn option_select(document: &Document, option: NodeId) -> Result<Option<NodeId>, Error> {
    let mut ancestor = document.parent(option)?;
    let mut optgroup_seen = false;
    while let Some(node) = ancestor {
        match element(document, node).map(|(name, _)| name) {
            Some("datalist" | "hr" | "option") => return Ok(None),
            Some("optgroup") if optgroup_seen => return Ok(None),
            Some("optgroup") => optgroup_seen = true,
            Some("select") => return Ok(Some(node)),
            _ => {}
        }
        ancestor = document.parent(node)?;
    }
    Ok(None)
}

pub fn option_index(document: &Document, option: NodeId) -> Result<usize, Error> {
    let Some(select) = option_select(document, option)? else {
        return Ok(0);
    };
    let mut found = 0;
    for_each_select_option(document, select, |node, index| {
        if node == option {
            found = index;
            false
        } else {
            true
        }
    })?;
    Ok(found)
}

pub fn option_value(document: &Document, option: NodeId) -> Option<String> {
    element(document, option)?;
    match attribute(document, option, "value") {
        Some(value) => Some(value.to_owned()),
        None => option_text(document, option).ok(),
    }
}

/// Return the option's own disabledness used by select selection algorithms.
/// CSS `:disabled` separately also accounts for a disabled nearest `select`.
pub fn option_disabled(document: &Document, option: NodeId) -> bool {
    if html_element_local_name(document, option) != Some("option") {
        return false;
    }
    if attribute(document, option, "disabled").is_some() {
        return true;
    }
    let mut ancestor = document.parent(option).ok().flatten();
    while let Some(node) = ancestor {
        if let Some(tag) = html_element_local_name(document, node) {
            if matches!(tag, "select" | "hr" | "datalist" | "option") {
                return false;
            }
            if tag == "optgroup" {
                return attribute(document, node, "disabled").is_some();
            }
        }
        ancestor = document.parent(node).ok().flatten();
    }
    false
}

/// Return whether `:enabled`/`:disabled` apply to this element and, if so,
/// whether it is actually disabled. This keeps the HTML selector tag set
/// separate from the more general disabled-ancestor helper used by controls.
fn selector_disabled_state(document: &Document, node: NodeId) -> Option<bool> {
    match html_element_local_name(document, node)? {
        "button" | "input" | "select" | "textarea" => Some(is_disabled(document, node)),
        "fieldset" => Some(is_disabled(document, node)),
        "optgroup" => {
            let own_disabled = attribute(document, node, "disabled").is_some();
            let mut ancestor = document.parent(node).ok().flatten();
            while let Some(parent) = ancestor {
                if html_element_local_name(document, parent) == Some("select") {
                    return Some(own_disabled || attribute(document, parent, "disabled").is_some());
                }
                ancestor = document.parent(parent).ok().flatten();
            }
            Some(own_disabled)
        }
        "option" => {
            if option_disabled(document, node) {
                return Some(true);
            }
            let mut ancestor = document.parent(node).ok().flatten();
            while let Some(parent) = ancestor {
                if html_element_local_name(document, parent) == Some("select") {
                    return Some(attribute(document, parent, "disabled").is_some());
                }
                ancestor = document.parent(parent).ok().flatten();
            }
            Some(false)
        }
        _ => None,
    }
}

fn required_state(document: &Document, node: NodeId) -> Option<bool> {
    match html_element_local_name(document, node)? {
        "input" if input_required_applies(input_type_state(document, node)) => {
            Some(attribute(document, node, "required").is_some())
        }
        "select" | "textarea" => Some(attribute(document, node, "required").is_some()),
        _ => None,
    }
}

fn contenteditable_value(value: &str) -> Option<bool> {
    if value.is_empty()
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("plaintext-only")
    {
        Some(true)
    } else if value.eq_ignore_ascii_case("false") {
        Some(false)
    } else {
        None
    }
}

fn content_is_editable(document: &Document, node: NodeId) -> bool {
    let mut current = Some(node);
    for _ in 0..512 {
        let Some(candidate) = current else {
            return false;
        };
        if let Some(tag) = html_element_local_name(document, candidate) {
            if matches!(tag, "input" | "textarea") {
                return false;
            }
            if let Some(value) = attribute(document, candidate, "contenteditable") {
                if let Some(editable) = contenteditable_value(value) {
                    return editable;
                }
            }
        }
        current = document.parent(candidate).ok().flatten();
    }
    false
}

fn selector_read_write(document: &Document, node: NodeId) -> bool {
    match html_element_local_name(document, node) {
        Some("input") => {
            input_readonly_applies(input_type_state(document, node))
                && attribute(document, node, "readonly").is_none()
                && !is_disabled(document, node)
        }
        Some("textarea") => {
            attribute(document, node, "readonly").is_none() && !is_disabled(document, node)
        }
        Some(_) => content_is_editable(document, node),
        None => false,
    }
}

fn effective_checkedness(document: &Document, node: NodeId, view: &dyn ValidityStateView) -> bool {
    view.checkedness(node)
        .or_else(|| {
            document
                .form_selector_state(node)
                .and_then(|state| state.checkedness)
        })
        .unwrap_or_else(|| attribute(document, node, "checked").is_some())
}

fn effective_option_selectedness(
    document: &Document,
    option: NodeId,
    view: &dyn ValidityStateView,
) -> bool {
    if html_element_local_name(document, option) != Some("option") {
        return false;
    }
    let selectedness = |candidate| {
        view.selectedness(candidate).or_else(|| {
            document
                .form_selector_state(candidate)
                .and_then(|state| state.selectedness)
        })
    };
    if let Ok(Some(select)) = option_select(document, option) {
        if attribute(document, select, "multiple").is_some() {
            // Multiple selects do not normalize sibling selectedness.
            return selectedness(option)
                .unwrap_or_else(|| attribute(document, option, "selected").is_some());
        }
        if let Some(selected) = document
            .form_selector_state(select)
            .and_then(|state| state.single_select_option)
        {
            return selected == Some(option);
        }
    }
    if let Some(selected) = selectedness(option) {
        return selected;
    }
    // Share the last-selected single-select rule and first-enabled fallback
    // with the DOM IDL, values, form submission, and CSS :checked matching.
    option_is_selected_by(document, option, selectedness).unwrap_or(false)
}

fn is_default_submit_button(document: &Document, node: NodeId) -> bool {
    if !is_submit_button(document, node) {
        return false;
    }
    let Some(form) = form_owner(document, node) else {
        return false;
    };
    let mut default = None;
    let _ = for_each_form_control(document, form, |candidate| {
        if is_submit_button(document, candidate) {
            default = Some(candidate);
            false
        } else {
            true
        }
    });
    default == Some(node)
}

/// Match one of the stateful HTML form pseudo-classes using the same live
/// value/checkedness view as constraint validation and option selection.
pub fn matches_form_state_pseudo(
    document: &Document,
    node: NodeId,
    pseudo: FormStatePseudo,
    view: &dyn ValidityStateView,
) -> bool {
    let tag = html_element_local_name(document, node);
    match pseudo {
        FormStatePseudo::Disabled => selector_disabled_state(document, node) == Some(true),
        FormStatePseudo::Enabled => selector_disabled_state(document, node) == Some(false),
        FormStatePseudo::Checked => match tag {
            Some("input") => {
                matches!(input_type_state(document, node), "checkbox" | "radio")
                    && effective_checkedness(document, node, view)
            }
            Some("option") => effective_option_selectedness(document, node, view),
            _ => false,
        },
        FormStatePseudo::Required => required_state(document, node) == Some(true),
        FormStatePseudo::Optional => required_state(document, node) == Some(false),
        FormStatePseudo::ReadOnly => !selector_read_write(document, node),
        FormStatePseudo::ReadWrite => selector_read_write(document, node),
        FormStatePseudo::Default => match tag {
            Some("button") => is_default_submit_button(document, node),
            Some("input") => {
                is_default_submit_button(document, node)
                    || (matches!(input_type_state(document, node), "checkbox" | "radio")
                        && attribute(document, node, "checked").is_some())
            }
            Some("option") => attribute(document, node, "selected").is_some(),
            _ => false,
        },
        FormStatePseudo::UserValid | FormStatePseudo::UserInvalid => {
            if !matches!(tag, Some("input" | "select" | "textarea"))
                || !will_validate(document, node)
            {
                return false;
            }
            let interacted = view.user_validity_interacted(node)
                || document
                    .form_selector_state(node)
                    .and_then(|state| state.user_validity_interacted)
                    .unwrap_or(false);
            if !interacted {
                return false;
            }
            let valid = document
                .validity_state(node)
                .unwrap_or_else(|| validity_with_view(document, node, view))
                .valid();
            match pseudo {
                FormStatePseudo::UserValid => valid,
                FormStatePseudo::UserInvalid => !valid,
                _ => false,
            }
        }
        FormStatePseudo::PlaceholderShown => {
            let resolved = document
                .form_selector_state(node)
                .and_then(|state| state.placeholder_shown);
            resolved.unwrap_or_else(|| placeholder_shown(document, node, view.value_override(node)))
        }
    }
}

/// Whether the given HTML input or textarea is currently showing its
/// placeholder. `value_override` is borrowed host state for a dirty control.
pub fn placeholder_shown(
    document: &Document,
    node: NodeId,
    value_override: Option<&str>,
) -> bool {
    let Some((name, _)) = element(document, node) else {
        return false;
    };
    if attribute(document, node, "placeholder").is_none_or(str::is_empty) {
        return false;
    }
    match name {
        "textarea" => {}
        "input" => {
            let kind = input_type_state(document, node);
            if !matches!(
                kind,
                "text" | "search" | "url" | "tel" | "email" | "password" | "number"
            ) {
                return false;
            }
        }
        _ => return false,
    }
    let value_empty = value_override.map_or_else(
        || {
            let value = control_value(document, node, name);
            value.is_empty()
        },
        str::is_empty,
    );
    value_empty
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
    selected_option_ids_by(document, select, |option| {
        selectedness
            .iter()
            .find(|(id, _)| *id == option)
            .map(|(_, selected)| *selected)
    })
}

fn selected_option_ids_by(
    document: &Document,
    select: NodeId,
    selectedness: impl FnMut(NodeId) -> Option<bool>,
) -> Vec<NodeId> {
    let mut selected = Vec::new();
    let _ = for_each_selected_option_by(document, select, selectedness, |option, _, _| {
        selected.push(option);
        true
    });
    selected
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
    select_value_by(document, select, |option| {
        selectedness
            .iter()
            .find(|(id, _)| *id == option)
            .map(|(_, selected)| *selected)
    })
}

/// Read the select value using caller-owned selectedness without allocating an
/// intermediate list of option IDs. `None` means use the markup selectedness.
pub fn select_value_by(
    document: &Document,
    select: NodeId,
    selectedness: impl FnMut(NodeId) -> Option<bool>,
) -> String {
    let mut selected_value = None;
    let _ = for_each_selected_option_by(document, select, selectedness, |option, _, _| {
        selected_value = option_value(document, option);
        false
    });
    selected_value.unwrap_or_default()
}

pub fn selected_index(document: &Document, select: NodeId) -> isize {
    selected_index_with(document, select, &[])
}

pub fn selected_index_with(
    document: &Document,
    select: NodeId,
    selectedness: &[(NodeId, bool)],
) -> isize {
    selected_option_index_by(document, select, |option| {
        selectedness
            .iter()
            .find(|(id, _)| *id == option)
            .map(|(_, selected)| *selected)
    })
    .unwrap_or(-1)
}

/// Resolve the selected index with adapter-owned overrides and the same clean
/// single-select fallback used by selected-options and value resolution.
/// An explicit clean-state override, including `false`, suppresses fallback.
pub fn selected_option_index_by(
    document: &Document,
    select: NodeId,
    selectedness: impl FnMut(NodeId) -> Option<bool>,
) -> Result<isize, Error> {
    let mut selected_index = -1;
    for_each_selected_option_by(document, select, selectedness, |_, option_index, _| {
        selected_index = option_index as isize;
        false
    })?;
    Ok(selected_index)
}

/// Visit the effective selected options in tree order without materializing
/// the option list. Selection overrides are adapter-owned; an explicit false
/// override suppresses the clean single-select fallback.
pub fn for_each_selected_option_by(
    document: &Document,
    select: NodeId,
    mut selectedness: impl FnMut(NodeId) -> Option<bool>,
    mut visit: impl FnMut(NodeId, usize, usize) -> bool,
) -> Result<(), Error> {
    let multiple =
        element(document, select).is_some() && attribute(document, select, "multiple").is_some();
    if !multiple {
        let mut has_override = false;
        let mut last_selected = None;
        let mut first_enabled = None;
        for_each_select_option(document, select, |option, option_index| {
            let override_value = selectedness(option);
            has_override |= override_value.is_some();
            if override_value.unwrap_or_else(|| attribute(document, option, "selected").is_some()) {
                // In a single-select, later selected options supersede earlier
                // ones. Retain only the last candidate so parser-created
                // markup follows the same rule without allocating a list.
                last_selected = Some((option, option_index));
            }
            if first_enabled.is_none() && !option_disabled(document, option) {
                first_enabled = Some((option, option_index));
            }
            true
        })?;
        if let Some((option, option_index)) = last_selected {
            visit(option, option_index, 0);
        } else if !has_override && select_display_size(document, select) == Some(1) {
            if let Some((option, option_index)) = first_enabled {
                visit(option, option_index, 0);
            }
        }
        return Ok(());
    }

    let mut selected_position = 0usize;
    for_each_select_option(document, select, |option, option_index| {
        let override_value = selectedness(option);
        let selected =
            override_value.unwrap_or_else(|| attribute(document, option, "selected").is_some());
        if selected {
            let keep_going = visit(option, option_index, selected_position);
            selected_position += 1;
            if !keep_going {
                return false;
            }
        }
        true
    })?;
    Ok(())
}

/// Resolve whether one option is in its select's effective selected set.
/// This shares the single-select last-selected and default fallback rules with
/// selectedOptions, value, form submission, and selectedIndex.
pub fn option_is_selected_by(
    document: &Document,
    option: NodeId,
    mut selectedness: impl FnMut(NodeId) -> Option<bool>,
) -> Result<bool, Error> {
    let Some(select) = option_select(document, option)? else {
        return Ok(selectedness(option)
            .unwrap_or_else(|| attribute(document, option, "selected").is_some()));
    };
    if attribute(document, select, "multiple").is_some() {
        return Ok(selectedness(option)
            .unwrap_or_else(|| attribute(document, option, "selected").is_some()));
    }
    let mut is_selected = false;
    for_each_selected_option_by(
        document,
        select,
        |candidate| selectedness(candidate),
        |candidate, _, _| {
            is_selected |= candidate == option;
            true
        },
    )?;
    Ok(is_selected)
}

/// Find the index of the first option accepted by `is_selected` without
/// materializing the list. A negative result means no option matched.
pub fn select_option_index_by(
    document: &Document,
    select: NodeId,
    mut is_selected: impl FnMut(NodeId) -> bool,
) -> Result<isize, Error> {
    let mut selected_index = -1;
    for_each_select_option(document, select, |option, index| {
        if is_selected(option) {
            selected_index = index as isize;
            false
        } else {
            true
        }
    })?;
    Ok(selected_index)
}

/// Set option selectedness through the shared DOM representation. Callers
/// capture reset defaults before invoking this mutation.
pub fn set_option_selected(
    document: &mut Document,
    option: NodeId,
    selected: bool,
) -> Result<(), Error> {
    if selected {
        document.set_attribute_ns(option, None, "selected", "")
    } else {
        document.remove_attribute_ns(option, None, "selected")
    }
}

pub fn set_select_value(document: &mut Document, select: NodeId, value: &str) -> Result<(), Error> {
    let options = select_options(document, select);
    let multiple =
        element(document, select).is_some() && attribute(document, select, "multiple").is_some();
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
            let _ = for_each_select_option(document, control, |option, _| {
                reset.push(option);
                true
            });
        }
    }
    reset
}

fn radio_group_required(document: &Document, node: NodeId, group: &str) -> bool {
    let root = tree_root(document, node);
    let mut current = Some(root);
    while let Some(candidate) = current {
        if element(document, candidate).is_some_and(|(name, _)| {
            name == "input"
                && attribute(document, candidate, "type")
                    .unwrap_or("text")
                    .eq_ignore_ascii_case("radio")
                && attribute(document, candidate, "name") == Some(group)
                && attribute(document, candidate, "required").is_some()
                && form_owner(document, candidate) == form_owner(document, node)
        }) {
            return true;
        }
        current = if candidate == root {
            document.first_child(root).ok().flatten()
        } else {
            crate::selector::next_descendant(document, root, candidate)
                .ok()
                .flatten()
        };
    }
    false
}

pub fn is_disabled(document: &Document, node: NodeId) -> bool {
    let Some((name, _attributes)) = element(document, node) else {
        return true;
    };
    if attribute(document, node, "disabled").is_some() {
        return true;
    }
    // A disabled fieldset disables descendants except those in its first legend.
    let mut child = node;
    let mut parent = document.parent(child).ok().flatten();
    while let Some(ancestor) = parent {
        if let Some(("fieldset", _attrs)) = element(document, ancestor) {
            if attribute(document, ancestor, "disabled").is_some() {
                let mut first_legend = None;
                let mut candidate = document.first_child(ancestor).ok().flatten();
                while let Some(id) = candidate {
                    if element(document, id).is_some_and(|(tag, _)| tag == "legend") {
                        first_legend = Some(id);
                        break;
                    }
                    candidate = document.next_sibling(id).ok().flatten();
                }
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
    let Some((name, _attributes)) = element(document, node) else {
        return false;
    };
    if !matches!(name, "input" | "select" | "textarea" | "button")
        || is_disabled(document, node)
        || has_datalist_ancestor(document, node)
    {
        return false;
    }
    // HTML bars every input with a specified readonly content attribute from
    // constraint validation, even when readonly does not make that input type
    // user-editable. Textarea has the same candidate rule.
    if matches!(name, "input" | "textarea")
        && attribute(document, node, "readonly").is_some()
    {
        return false;
    }
    match name {
        "input" => {
            let kind = input_type_state(document, node);
            !["hidden", "button", "reset"]
                .iter()
                .any(|barred| kind.eq_ignore_ascii_case(barred))
        }
        "select" | "textarea" => true,
        "button" => button_type_state(document, node) == Some(ButtonTypeState::Submit),
        _ => false,
    }
}

fn has_datalist_ancestor(document: &Document, node: NodeId) -> bool {
    let mut ancestor = document.parent(node).ok().flatten();
    while let Some(id) = ancestor {
        if element(document, id).is_some_and(|(name, _)| name == "datalist") {
            return true;
        }
        ancestor = document.parent(id).ok().flatten();
    }
    false
}

/// Return the normalized input type state without allocating. Unknown
/// keywords use the Text state, as required by the input type algorithm.
pub(crate) fn input_type_state(document: &Document, node: NodeId) -> &'static str {
    const STATES: &[&str] = &[
        "hidden",
        "text",
        "search",
        "tel",
        "url",
        "email",
        "password",
        "date",
        "month",
        "week",
        "time",
        "datetime-local",
        "number",
        "range",
        "color",
        "checkbox",
        "radio",
        "file",
        "submit",
        "image",
        "reset",
        "button",
    ];
    let raw = attribute(document, node, "type").unwrap_or("text");
    STATES
        .iter()
        .copied()
        .find(|state| raw.eq_ignore_ascii_case(state))
        .unwrap_or("text")
}

/// The direction used by `HTMLInputElement.stepUp()` and `stepDown()`.
///
/// The method direction is kept separate from its signed `n` argument because
/// an off-step value is first realigned in the method's direction even when
/// `n` is zero or negative.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputStepDirection {
    Up,
    Down,
}

/// Why an input numeric conversion or stepping operation cannot be performed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputNumberError {
    /// The element is not an input in a state with numeric value operations.
    UnsupportedType,
    /// The input's `step` attribute is `any`, so stepping is disabled.
    StepAny,
    /// The number has no representable value in the input type's string domain.
    Unrepresentable,
}

fn numeric_input_kind(document: &Document, node: NodeId) -> Option<&'static str> {
    if html_element_local_name(document, node)? != "input" {
        return None;
    }
    let kind = input_type_state(document, node);
    matches!(
        kind,
        "number" | "range" | "date" | "month" | "week" | "time" | "datetime-local"
    )
    .then_some(kind)
}

/// Whether `valueAsNumber`, `stepUp()`, and `stepDown()` apply to this element.
pub fn input_numeric_type_supported(document: &Document, node: NodeId) -> bool {
    numeric_input_kind(document, node).is_some()
}

/// Convert an input's current string to its HTML numeric value, returning NaN
/// for an unsupported state, an empty value, or a malformed value.
pub fn input_value_as_number(document: &Document, node: NodeId, value: &str) -> f64 {
    let Some(kind) = numeric_input_kind(document, node) else {
        return f64::NAN;
    };
    if kind == "range" && value.is_empty() {
        return range_sanitized_value(
            value,
            attribute(document, node, "min"),
            attribute(document, node, "max"),
            attribute(document, node, "step"),
            attribute(document, node, "value"),
        );
    }
    input_value_number(value, kind).unwrap_or(f64::NAN)
}

/// Whether this input's `valueAsDate` accessor has a date/time state. Unlike
/// `valueAsNumber`, `datetime-local`, `number`, and `range` are not supported.
pub fn input_date_type_supported(document: &Document, node: NodeId) -> bool {
    matches!(
        numeric_input_kind(document, node),
        Some("date" | "month" | "week" | "time")
    )
}

/// Convert an input's current value into the UTC millisecond time value used
/// by its `valueAsDate` Date object. Empty and malformed values return `None`.
pub fn input_value_as_date_ms(document: &Document, node: NodeId, value: &str) -> Option<f64> {
    let kind = numeric_input_kind(document, node)?;
    let numeric = input_value_number(value, kind)?;
    match kind {
        "date" | "week" | "time" => Some(numeric),
        "month" => {
            if numeric.fract() != 0.0 || numeric.abs() > (MAX_INPUT_YEAR as f64) * 12.0 {
                return None;
            }
            let month_index = numeric as i64;
            let year = 1970i64.checked_add(month_index.div_euclid(12))?;
            if !(1..=MAX_INPUT_YEAR).contains(&year) {
                return None;
            }
            let month = month_index.rem_euclid(12) + 1;
            Some(lumen_common::civil::days_from_civil(year, month, 1) as f64 * MILLISECONDS_PER_DAY)
        }
        _ => None,
    }
}

/// Serialize a Date's UTC millisecond time value for an input state supported
/// by `valueAsDate`. Month values use the Date's UTC year and month, while
/// date/week/time reuse the existing numeric serializer and calendar rules.
pub fn input_date_ms_value_string(
    document: &Document,
    node: NodeId,
    milliseconds: f64,
) -> Result<String, InputNumberError> {
    let kind = numeric_input_kind(document, node).ok_or(InputNumberError::UnsupportedType)?;
    if !matches!(kind, "date" | "month" | "week" | "time") {
        return Err(InputNumberError::UnsupportedType);
    }
    if milliseconds.is_nan() {
        return Ok(String::new());
    }
    if !milliseconds.is_finite() {
        return Err(InputNumberError::Unrepresentable);
    }
    if kind == "month" {
        let day =
            bounded_day_for_milliseconds(milliseconds).ok_or(InputNumberError::Unrepresentable)?;
        let (year, month, _) = lumen_common::civil::civil_from_days(day);
        let month_index = year
            .checked_sub(1970)
            .and_then(|year| year.checked_mul(12))
            .and_then(|year| year.checked_add(month as i64 - 1))
            .ok_or(InputNumberError::Unrepresentable)?;
        return input_number_value_string(document, node, month_index as f64);
    }
    input_number_value_string(document, node, milliseconds)
}

/// Convert an unrestricted number to the value string for a supported input.
/// NaN maps to the empty string; finite values outside a date/time string's
/// representable domain return `Unrepresentable` so callers can apply the
/// value sanitization algorithm without turning it into an InvalidStateError.
pub fn input_number_value_string(
    document: &Document,
    node: NodeId,
    value: f64,
) -> Result<String, InputNumberError> {
    let kind = numeric_input_kind(document, node).ok_or(InputNumberError::UnsupportedType)?;
    if value.is_nan() {
        return Ok(String::new());
    }
    if !value.is_finite() {
        return Err(InputNumberError::Unrepresentable);
    }
    match kind {
        "number" | "range" => Ok(format_html_number(value)),
        "date" => format_date_number(value).ok_or(InputNumberError::Unrepresentable),
        "month" => format_month_number(value).ok_or(InputNumberError::Unrepresentable),
        "week" => format_week_number(value).ok_or(InputNumberError::Unrepresentable),
        "time" => format_time_number(value).ok_or(InputNumberError::Unrepresentable),
        "datetime-local" => {
            format_datetime_local_number(value).ok_or(InputNumberError::Unrepresentable)
        }
        _ => Err(InputNumberError::UnsupportedType),
    }
}

const MILLISECONDS_PER_DAY: f64 = 86_400_000.0;
const MILLISECONDS_PER_WEEK: f64 = 604_800_000.0;
const MAX_INPUT_YEAR: i64 = 1_000_000_000_000_000;

fn format_html_number(value: f64) -> String {
    if value == 0.0 {
        return String::from("0");
    }
    let negative = value.is_sign_negative();
    let digits = lumen_common::float::shortest(value.abs());
    let source = digits.as_str();
    let decimal = digits.decpt;
    let mut result = String::new();
    if negative {
        result.push('-');
    }

    // This is the decimal/exponential cutover used by the HTML floating-point
    // string representation (the same cutover as Number::toString).
    if decimal > 0 && decimal <= 21 {
        if decimal as usize >= source.len() {
            result.push_str(source);
            for _ in source.len()..decimal as usize {
                result.push('0');
            }
        } else {
            let split = decimal as usize;
            result.push_str(&source[..split]);
            result.push('.');
            result.push_str(&source[split..]);
        }
    } else if decimal <= 0 && decimal > -6 {
        result.push_str("0.");
        for _ in 0..decimal.unsigned_abs() {
            result.push('0');
        }
        result.push_str(source);
    } else {
        result.push(source.as_bytes()[0] as char);
        if source.len() > 1 {
            result.push('.');
            result.push_str(&source[1..]);
        }
        result.push('e');
        let exponent = decimal - 1;
        if exponent >= 0 {
            result.push('+');
        }
        result.push_str(&exponent.to_string());
    }
    result
}

fn day_bounds() -> (i64, i64) {
    use lumen_common::civil::days_from_civil;
    (
        days_from_civil(1, 1, 1),
        days_from_civil(MAX_INPUT_YEAR, 12, 31),
    )
}

fn date_string_from_day(day: i64) -> Option<String> {
    use lumen_common::civil::civil_from_days;
    let (min_day, max_day) = day_bounds();
    if !(min_day..=max_day).contains(&day) {
        return None;
    }
    let (year, month, day) = civil_from_days(day);
    if !(1..=MAX_INPUT_YEAR).contains(&year) {
        return None;
    }
    Some(alloc::format!("{year:04}-{month:02}-{day:02}"))
}

fn bounded_day_for_milliseconds(value: f64) -> Option<i64> {
    if !value.is_finite() {
        return None;
    }
    let day = (value / MILLISECONDS_PER_DAY).floor();
    let (minimum, maximum) = day_bounds();
    // Week 0001-W01 starts three days before 0001-01-01. The small margin
    // keeps the conversion bounded before the ISO week-year check below.
    if day < (minimum - 7) as f64 || day > (maximum + 7) as f64 {
        return None;
    }
    Some(day as i64)
}

fn format_date_number(value: f64) -> Option<String> {
    date_string_from_day(bounded_day_for_milliseconds(value)?)
}

fn format_month_number(value: f64) -> Option<String> {
    if !value.is_finite() || value.fract() != 0.0 {
        return None;
    }
    let months = value as i64;
    // Keep the conversion in the same supported year range as the input
    // grammar, before doing arithmetic on an extreme floating-point value.
    let year = 1970i64.checked_add(months.div_euclid(12))?;
    if !(1..=MAX_INPUT_YEAR).contains(&year) {
        return None;
    }
    let month = months.rem_euclid(12) + 1;
    Some(alloc::format!("{year:04}-{month:02}"))
}

fn format_week_number(value: f64) -> Option<String> {
    use lumen_common::civil::{civil_from_days, iso_week};
    let day = bounded_day_for_milliseconds(value)?;
    let (year, month, day_of_month) = civil_from_days(day);
    let (week, week_year) = iso_week(year, month as i64, day_of_month as i64);
    if !(1..=MAX_INPUT_YEAR).contains(&week_year) || !(1..=53).contains(&week) {
        return None;
    }
    Some(alloc::format!("{week_year:04}-W{week:02}"))
}

fn format_time_number(value: f64) -> Option<String> {
    format_time_of_day(value.rem_euclid(MILLISECONDS_PER_DAY))
}

fn format_time_of_day(milliseconds: f64) -> Option<String> {
    if !milliseconds.is_finite() || milliseconds.fract() != 0.0 {
        return None;
    }
    let milliseconds = milliseconds as u32;
    if milliseconds >= 86_400_000 {
        return None;
    }
    let hours = milliseconds / 3_600_000;
    let minutes = milliseconds / 60_000 % 60;
    let seconds = milliseconds / 1_000 % 60;
    let fraction = milliseconds % 1_000;
    let mut result = alloc::format!("{hours:02}:{minutes:02}");
    if seconds != 0 || fraction != 0 {
        result.push_str(&alloc::format!(":{seconds:02}"));
        if fraction != 0 {
            let mut digits = alloc::format!("{fraction:03}");
            while digits.ends_with('0') {
                digits.pop();
            }
            result.push('.');
            result.push_str(&digits);
        }
    }
    Some(result)
}

fn format_datetime_local_number(value: f64) -> Option<String> {
    if !value.is_finite() || value.fract() != 0.0 {
        return None;
    }
    let day = bounded_day_for_milliseconds(value)?;
    let date = date_string_from_day(day)?;
    let milliseconds = value.rem_euclid(MILLISECONDS_PER_DAY);
    Some(alloc::format!(
        "{date}T{}",
        format_time_of_day(milliseconds)?
    ))
}

#[derive(Clone, Copy)]
struct InputStepSettings {
    minimum: Option<f64>,
    maximum: Option<f64>,
    step: Option<f64>,
    step_is_any: bool,
    base: f64,
}

fn input_step_settings(document: &Document, node: NodeId, kind: &str) -> InputStepSettings {
    let step_scale = match kind {
        "date" => MILLISECONDS_PER_DAY,
        "week" => MILLISECONDS_PER_WEEK,
        "time" | "datetime-local" => 1000.0,
        _ => 1.0,
    };
    let default_step = match kind {
        "time" | "datetime-local" => 60.0,
        _ => 1.0,
    };
    let minimum = if kind == "range" {
        Some(
            range_bounds(
                attribute(document, node, "min"),
                attribute(document, node, "max"),
            )
            .0,
        )
    } else {
        attribute(document, node, "min").and_then(|raw| input_value_number(raw, kind))
    };
    let maximum = if kind == "range" {
        Some(
            range_bounds(
                attribute(document, node, "min"),
                attribute(document, node, "max"),
            )
            .1,
        )
    } else {
        attribute(document, node, "max").and_then(|raw| input_value_number(raw, kind))
    };
    let raw_step = attribute(document, node, "step");
    let step_is_any = raw_step.is_some_and(|raw| raw.eq_ignore_ascii_case("any"));
    let step = if step_is_any {
        None
    } else {
        Some(
            raw_step
                .and_then(parse_html_float)
                .filter(|value| *value > 0.0)
                .unwrap_or(default_step)
                * step_scale,
        )
    };
    let base = minimum
        .filter(|_| kind != "range" || attribute(document, node, "min").is_some())
        .or_else(|| {
            attribute(document, node, "value").and_then(|raw| input_value_number(raw, kind))
        })
        .unwrap_or(if kind == "week" { -259_200_000.0 } else { 0.0 });
    InputStepSettings {
        minimum,
        maximum,
        step,
        step_is_any,
        base,
    }
}

fn step_quotient_is_integral(quotient: f64) -> bool {
    quotient.is_finite() && (quotient - quotient.round()).abs() <= 1e-7
}

fn aligned_step_index(quotient: f64, round_up: bool) -> Option<f64> {
    if !quotient.is_finite() {
        return None;
    }
    let nearest = quotient.round();
    if (quotient - nearest).abs() <= 1e-7 {
        return Some(nearest);
    }
    Some(if round_up {
        quotient.ceil()
    } else {
        quotient.floor()
    })
}

fn decimal_precision(value: &str) -> Option<u32> {
    if parse_html_float(value).is_none() {
        return None;
    }
    let unsigned = value.strip_prefix('-').unwrap_or(value);
    let (mantissa, exponent) = unsigned
        .split_once('e')
        .or_else(|| unsigned.split_once('E'))
        .map_or((unsigned, 0i32), |(mantissa, exponent)| {
            (mantissa, exponent.parse::<i32>().unwrap_or(0))
        });
    let fractional = mantissa.split_once('.').map_or(0, |(_, part)| part.len()) as i32;
    Some((fractional - exponent).max(0).min(308) as u32)
}

fn stabilize_numeric_step_result(
    document: &Document,
    node: NodeId,
    kind: &str,
    input_value: &str,
    result: f64,
) -> f64 {
    if !matches!(kind, "number" | "range") {
        return result;
    }
    let mut precision = decimal_precision(input_value).unwrap_or(0);
    for raw in [
        attribute(document, node, "value"),
        attribute(document, node, "min"),
        attribute(document, node, "max"),
        attribute(document, node, "step"),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(candidate) = decimal_precision(raw) {
            precision = precision.max(candidate);
        }
    }
    let factor = 10.0f64.powi(precision as i32);
    let scaled = result * factor;
    if !factor.is_finite() || !scaled.is_finite() || scaled.abs() >= (1u64 << 53) as f64 {
        result
    } else {
        scaled.round() / factor
    }
}

/// Apply the HTML step algorithm to a numeric input's current string.
/// `Ok(None)` means bounds made the operation a no-op; `Some("")` is reserved
/// for a step result that the input type cannot serialize.
pub fn input_step_value(
    document: &Document,
    node: NodeId,
    value: &str,
    steps: i64,
    direction: InputStepDirection,
) -> Result<Option<String>, InputNumberError> {
    let kind = numeric_input_kind(document, node).ok_or(InputNumberError::UnsupportedType)?;
    let settings = input_step_settings(document, node, kind);
    let step = settings.step.ok_or(InputNumberError::StepAny)?;
    let Some(step) = (step.is_finite() && step > 0.0).then_some(step) else {
        return Ok(None);
    };

    if settings
        .minimum
        .zip(settings.maximum)
        .is_some_and(|(minimum, maximum)| minimum > maximum)
    {
        return Ok(None);
    }
    if let (Some(minimum), Some(maximum)) = (settings.minimum, settings.maximum) {
        let lower = step_quotient(minimum, settings.base, step);
        let upper = step_quotient(maximum, settings.base, step);
        let Some(first) = aligned_step_index(lower, true) else {
            return Ok(None);
        };
        let Some(last) = aligned_step_index(upper, false) else {
            return Ok(None);
        };
        if first > last {
            return Ok(None);
        }
    }

    let parsed_before = input_value_number(value, kind);
    let before = parsed_before.unwrap_or(0.0);
    let quotient = step_quotient(before, settings.base, step);
    if !quotient.is_finite() {
        return Ok(None);
    }
    let mut result = if !step_quotient_is_integral(quotient) {
        let index = match direction {
            InputStepDirection::Up => quotient.floor() + 1.0,
            InputStepDirection::Down => quotient.ceil() - 1.0,
        };
        settings.base + index * step
    } else {
        let signed_steps = match direction {
            InputStepDirection::Up => steps as f64,
            InputStepDirection::Down => -(steps as f64),
        };
        before + step * signed_steps
    };
    if settings.minimum.is_some_and(|minimum| result < minimum) {
        let minimum = settings.minimum.unwrap_or(result);
        let quotient = step_quotient(minimum, settings.base, step);
        let Some(index) = aligned_step_index(quotient, true) else {
            return Ok(None);
        };
        result = settings.base + index * step;
    }
    if settings.maximum.is_some_and(|maximum| result > maximum) {
        let maximum = settings.maximum.unwrap_or(result);
        let quotient = step_quotient(maximum, settings.base, step);
        let Some(index) = aligned_step_index(quotient, false) else {
            return Ok(None);
        };
        result = settings.base + index * step;
    }
    if !result.is_finite() {
        return Ok(None);
    }
    if matches!(direction, InputStepDirection::Up) && result < before
        || matches!(direction, InputStepDirection::Down) && result > before
    {
        return Ok(None);
    }
    result = stabilize_numeric_step_result(document, node, kind, value, result);
    let serialized =
        input_number_value_string(document, node, result).unwrap_or_else(|_| String::new());
    Ok(Some(serialized))
}

fn input_readonly_applies(kind: &str) -> bool {
    matches!(
        kind,
        "text"
            | "search"
            | "tel"
            | "url"
            | "email"
            | "password"
            | "date"
            | "month"
            | "week"
            | "time"
            | "datetime-local"
            | "number"
    )
}

fn input_required_applies(kind: &str) -> bool {
    // The required attribute does not apply in the Hidden, Range, Color, or
    // button-like states. Checkbox, radio, and file use separate algorithms.
    !matches!(
        kind,
        "hidden" | "range" | "color" | "submit" | "image" | "reset" | "button"
    )
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
    validity_with_value_and_selectedness_and_user_edit(
        document,
        node,
        value_override,
        custom_message,
        selectedness,
        false,
    )
}

/// As above, with the host user-edit flag required by HTML's length
/// constraints. Script-set values must not trigger tooLong or tooShort.
pub fn validity_with_value_and_selectedness_and_user_edit(
    document: &Document,
    node: NodeId,
    value_override: Option<&str>,
    custom_message: &str,
    selectedness: &[(NodeId, bool)],
    user_edited: bool,
) -> ValidityState {
    validity_with_value_and_selectedness_and_user_edit_and_checkedness(
        document,
        node,
        value_override,
        custom_message,
        selectedness,
        user_edited,
        &[],
    )
}

/// As above, using current dirty checkedness for checkbox/radio validity.
pub fn validity_with_value_and_selectedness_and_user_edit_and_checkedness(
    document: &Document,
    node: NodeId,
    value_override: Option<&str>,
    custom_message: &str,
    selectedness: &[(NodeId, bool)],
    user_edited: bool,
    checkedness: &[(NodeId, bool)],
) -> ValidityState {
    let view = SliceValidityStateView {
        node,
        value: value_override,
        custom_message,
        user_edited,
        selectedness,
        checkedness,
    };
    validity_with_view(document, node, &view)
}

/// Compute a control's validity against borrowed live form state.
///
/// The same view is used by selector matching and rendering so changing a
/// dirty value, selected option, checked radio, or custom validity message
/// cannot make `:valid` disagree with `ValidityState.valid`.
pub fn validity_with_view(
    document: &Document,
    node: NodeId,
    view: &dyn ValidityStateView,
) -> ValidityState {
    let mut state = ValidityState {
        custom_error: view
            .custom_message(node)
            .is_some_and(|message| !message.is_empty()),
        ..ValidityState::default()
    };
    let Some((name, _attributes)) = element(document, node) else {
        return state;
    };
    let type_name = input_type_state(document, node);
    let value_buffer;
    let value = if let Some(value) = view.value_override(node) {
        value
    } else if name == "select" {
        value_buffer = select_value_by(document, node, |option| view.selectedness(option));
        &value_buffer
    } else {
        value_buffer = control_value(document, node, name);
        &value_buffer
    };
    let length_applicable = name == "textarea"
        || (name == "input"
            && matches!(
                type_name,
                "text" | "search" | "url" | "tel" | "email" | "password"
            ));
    if length_applicable && view.user_edited(node) && !value.is_empty() {
        let length = value.encode_utf16().count();
        state.too_long = attribute(document, node, "maxlength")
            .and_then(parse_nonnegative_integer)
            .is_some_and(|maximum| length > maximum);
        state.too_short = attribute(document, node, "minlength")
            .and_then(parse_nonnegative_integer)
            .is_some_and(|minimum| length < minimum);
    }
    let required = attribute(document, node, "required").is_some();
    state.value_missing = match name {
        "input" => match input_value_mode(document, node) {
            Some(InputValueMode::DefaultOn) if type_name == "checkbox" && required => {
                !checked_by(document, node, |control| view.checkedness(control))
            }
            Some(InputValueMode::DefaultOn) if type_name == "radio" => {
                let group = attribute(document, node, "name").unwrap_or("");
                let group_required = if group.is_empty() {
                    required
                } else {
                    radio_group_required(document, node, group)
                };
                group_required
                    && !tree_contains_checked_radio_by(document, node, group, |control| {
                        view.checkedness(control)
                    })
            }
            Some(InputValueMode::Filename) if required => !view.has_selected_files(node),
            Some(InputValueMode::Value)
                if required
                    && input_required_applies(type_name)
                    && !is_disabled(document, node)
                    && !(input_readonly_applies(type_name)
                        && attribute(document, node, "readonly").is_some()) =>
            {
                value.is_empty()
            }
            _ => false,
        },
        "textarea"
            if required
                && !is_disabled(document, node)
                && attribute(document, node, "readonly").is_none() =>
        {
            value.is_empty()
        }
        "select" if required => select_value_missing(document, node, view),
        _ => false,
    };
    if !value.is_empty() {
        if name == "input"
            && matches!(
                type_name,
                "text" | "search" | "tel" | "url" | "email" | "password"
            )
        {
            if let Some(pattern) = attribute(document, node, "pattern") {
                state.pattern_mismatch = pattern_mismatch(
                    pattern,
                    &value,
                    type_name == "email" && attribute(document, node, "multiple").is_some(),
                );
            }
        }
        state.type_mismatch = if name == "input" {
            match type_name {
                "email" => {
                    let multiple = attribute(document, node, "multiple").is_some();
                    if multiple {
                        value.split(',').any(|address| {
                            !valid_email_address(address.trim_matches(is_ascii_whitespace))
                        })
                    } else {
                        !valid_email_address(&value)
                    }
                }
                "url" => !valid_absolute_url(&value),
                _ => false,
            }
        } else {
            false
        };
        if name == "input"
            && matches!(
                type_name,
                "number" | "range" | "date" | "month" | "week" | "time" | "datetime-local"
            )
        {
            let parsed = input_value_number(&value, type_name);
            if parsed.is_none() {
                // The JS value setter sanitizes these types. A raw invalid
                // markup value is not a numeric value and has no numeric constraints.
                state.bad_input = matches!(type_name, "number" | "range");
            } else if let Some(number) = parsed {
                let constraints = input_step_settings(document, node, type_name);
                let (min, max) = (constraints.minimum, constraints.maximum);
                if type_name == "time" && min.zip(max).is_some_and(|(lo, hi)| lo > hi) {
                    state.range_underflow = number > max.unwrap() && number < min.unwrap();
                    state.range_overflow = state.range_underflow;
                } else {
                    state.range_underflow = min.is_some_and(|min| number < min);
                    state.range_overflow = max.is_some_and(|max| number > max);
                }
                if let Some(step) = constraints.step {
                    let quotient = step_quotient(number, constraints.base, step);
                    state.step_mismatch = if step.is_infinite() {
                        number != constraints.base
                    } else {
                        quotient.is_finite() && !step_quotient_is_integral(quotient)
                    };
                }
            }
        }
    }
    state
}

fn checked_by(
    document: &Document,
    node: NodeId,
    mut checkedness: impl FnMut(NodeId) -> Option<bool>,
) -> bool {
    checkedness(node).unwrap_or_else(|| attribute(document, node, "checked").is_some())
}

fn tree_contains_checked_radio_by(
    document: &Document,
    node: NodeId,
    group: &str,
    mut checkedness: impl FnMut(NodeId) -> Option<bool>,
) -> bool {
    let root = tree_root(document, node);
    let mut current = Some(root);
    while let Some(candidate) = current {
        if element(document, candidate).is_some_and(|(tag, _)| {
            tag == "input"
                && attribute(document, candidate, "type")
                    .unwrap_or("text")
                    .eq_ignore_ascii_case("radio")
                && attribute(document, candidate, "name") == Some(group)
                && checkedness(candidate)
                    .unwrap_or_else(|| attribute(document, candidate, "checked").is_some())
        }) {
            return true;
        }
        current = if candidate == root {
            document.first_child(root).ok().flatten()
        } else {
            crate::selector::next_descendant(document, root, candidate)
                .ok()
                .flatten()
        };
    }
    false
}

fn select_value_missing(document: &Document, select: NodeId, view: &dyn ValidityStateView) -> bool {
    let multiple = attribute(document, select, "multiple").is_some();
    let mut first_option = None;
    let mut first_enabled = None;
    let mut selected = None;
    let mut has_override = false;
    let _ = for_each_select_option(document, select, |option, _| {
        first_option.get_or_insert(option);
        if first_enabled.is_none() && !option_disabled(document, option) {
            first_enabled = Some(option);
        }
        let override_value = view.selectedness(option);
        has_override |= override_value.is_some();
        if selected.is_none()
            && override_value.unwrap_or_else(|| attribute(document, option, "selected").is_some())
        {
            selected = Some(option);
        }
        true
    });
    let selected = selected.or_else(|| {
        (!multiple && !has_override)
            .then_some(first_enabled)
            .flatten()
    });
    let Some(selected) = selected else {
        return true;
    };
    !multiple
        && first_option == Some(selected)
        && option_value(document, selected).is_some_and(|value| value.is_empty())
}

/// Convert supported HTML input values to the type's numeric domain (HTML's
/// valueAsNumber units: milliseconds for date/time-like states).
fn input_value_number(value: &str, kind: &str) -> Option<f64> {
    use lumen_common::civil::{days_from_civil, days_in_month, iso_week};
    fn year(s: &str) -> Option<i64> {
        (s.len() >= 4 && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
            .filter(|y: &i64| *y > 0)
            .filter(|y| *y <= 1_000_000_000_000_000)
    }
    fn date(s: &str) -> Option<i64> {
        let (y, md) = s.split_once('-')?;
        let (m, d) = md.split_once('-')?;
        if m.len() != 2
            || d.len() != 2
            || !m.bytes().all(|b| b.is_ascii_digit())
            || !d.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let y = year(y)?;
        let m: u8 = m.parse().ok()?;
        let d: u8 = d.parse().ok()?;
        (m >= 1 && m <= 12 && d >= 1 && d <= days_in_month(y, m))
            .then(|| days_from_civil(y, m as i64, d as i64))
    }
    fn month(s: &str) -> Option<i64> {
        let (y, m) = s.split_once('-')?;
        if m.len() != 2 || !m.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let y = year(y)?;
        let m: u8 = m.parse().ok()?;
        (m >= 1 && m <= 12).then_some((y - 1970) * 12 + m as i64 - 1)
    }
    fn time(s: &str) -> Option<f64> {
        let mut parts = s.split(':');
        let h_raw = parts.next()?;
        let m_raw = parts.next()?;
        let seconds = parts.next();
        if parts.next().is_some()
            || h_raw.len() != 2
            || m_raw.len() != 2
            || !h_raw.bytes().all(|b| b.is_ascii_digit())
            || !m_raw.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let h: u32 = h_raw.parse().ok()?;
        let m: u32 = m_raw.parse().ok()?;
        let (sec, frac) = if let Some(raw) = seconds {
            let (s, f) = raw.split_once('.').map_or((raw, ""), |(s, f)| (s, f));
            if s.len() != 2
                || !s.bytes().all(|b| b.is_ascii_digit())
                || (raw.contains('.')
                    && (f.is_empty() || f.len() > 3 || !f.bytes().all(|b| b.is_ascii_digit())))
            {
                return None;
            }
            let scale = if f.is_empty() {
                0.0
            } else {
                alloc::format!("0.{f}").parse::<f64>().ok()? * 1000.0
            };
            (s.parse::<u32>().ok()?, scale)
        } else {
            (0, 0.0)
        };
        (h < 24 && m < 60 && sec < 60).then_some(((h * 3600 + m * 60 + sec) * 1000) as f64 + frac)
    }
    let n = match kind {
        "number" | "range" => parse_html_float(value),
        "date" => date(value).map(|d| d as f64 * 86_400_000.0),
        "month" => month(value).map(|m| m as f64),
        "week" => {
            let (y, w) = value.split_once("-W")?;
            if w.len() != 2 || !w.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let y = year(y)?;
            let w: u8 = w.parse().ok()?;
            if !(1..=53).contains(&w) || iso_week(y, 12, 28).0 < w as i64 {
                None
            } else {
                Some(
                    (days_from_civil(y, 1, 4) - ((days_from_civil(y, 1, 4) + 3).rem_euclid(7))
                        + (w as i64 - 1) * 7) as f64
                        * 86_400_000.0,
                )
            }
        }
        "time" => time(value),
        "datetime-local" => {
            let (d, t) = value.split_once('T').or_else(|| value.split_once(' '))?;
            date(d)
                .zip(time(t))
                .map(|(d, t)| d as f64 * 86_400_000.0 + t)
        }
        _ => None,
    };
    n.filter(|n| n.is_finite())
}

fn parse_html_float(value: &str) -> Option<f64> {
    let bytes = value.as_bytes();
    let mut i = usize::from(bytes.first() == Some(&b'-'));
    let integer_start = i;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    let has_integer = i > integer_start;
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        let fraction_start = i;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == fraction_start {
            return None;
        }
    } else if !has_integer {
        return None;
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exponent_start = i;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == exponent_start {
            return None;
        }
    }
    (i == bytes.len())
        .then(|| value.parse::<f64>().ok())
        .flatten()
        .filter(|n| n.is_finite())
}

/// HTML patterns use the existing ECMAScript Unicode-set regular expression
/// core and must match the complete value (or every address in an email list).
/// Invalid patterns impose no constraint, as required by HTML.
fn pattern_mismatch(pattern: &str, value: &str, multiple_email: bool) -> bool {
    use lumen_common::regex::{js, ExecOptions};
    let flags = js::Flags::parse("v").expect("constant ECMAScript flags");
    let Ok(regex) = js::compile(pattern.chars().collect(), &flags) else {
        return false;
    };
    let mismatch =
        |value: &str| !matches!(regex.exec_str(value, ExecOptions::full(0)), Ok(Some(_)));
    if multiple_email {
        value.split(',').any(|address| {
            mismatch(address.trim_matches(|character| {
                matches!(character, '\t' | '\n' | '\u{000C}' | '\r' | ' ')
            }))
        })
    } else {
        mismatch(value)
    }
}

fn valid_email_address(value: &str) -> bool {
    let Some((local, domain)) = value.rsplit_once('@') else {
        return false;
    };
    let local_char = |character: u8| {
        character.is_ascii_alphanumeric()
            || matches!(
                character,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'/'
                    | b'='
                    | b'?'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'{'
                    | b'|'
                    | b'}'
                    | b'~'
                    | b'.'
            )
    };
    if local.is_empty() || !local.bytes().all(local_char) || domain.is_empty() {
        return false;
    }
    domain.split('.').all(|label| {
        let bytes = label.as_bytes();
        !bytes.is_empty()
            && bytes.len() <= 63
            && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
            && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
    })
}

fn valid_absolute_url(value: &str) -> bool {
    lumen_common::url::parse_url(value, None).is_some_and(|url| !url.scheme.is_empty())
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
    form_entries_with_values_selectedness_checkedness_and_files(
        document,
        form,
        submitter,
        values,
        selectedness,
        &[],
        files,
    )
}

/// Construct the successful-control list using live dirty checkedness in
/// addition to live values and selectedness.
pub fn form_entries_with_values_selectedness_checkedness_and_files(
    document: &Document,
    form: NodeId,
    submitter: Option<NodeId>,
    values: &[(NodeId, String)],
    selectedness: &[(NodeId, bool)],
    checkedness: &[(NodeId, bool)],
    files: &[(NodeId, Vec<FormFile>)],
) -> Vec<FormEntry> {
    form_entries_with_state(
        document,
        form,
        submitter,
        &SliceFormEntryStateView {
            values,
            selectedness,
            checkedness,
            files,
        },
    )
}

/// Construct the successful-control list using a borrowed live-state view.
/// The shared walker visits controls directly in tree order; only entries in
/// this form are queried and cloned into the required returned snapshot.
pub fn form_entries_with_state(
    document: &Document,
    form: NodeId,
    submitter: Option<NodeId>,
    state: &impl FormEntryStateView,
) -> Vec<FormEntry> {
    form_entries_with_state_and_encoding(document, form, submitter, state, "UTF-8")
}

pub fn form_entries_with_state_and_encoding(
    document: &Document,
    form: NodeId,
    submitter: Option<NodeId>,
    state: &impl FormEntryStateView,
    encoding: &str,
) -> Vec<FormEntry> {
    let mut entries = Vec::new();
    let _ = for_each_form_control(document, form, |node| {
        let Some((tag, _attributes)) = element(document, node) else {
            return true;
        };
        if !matches!(tag, "input" | "textarea" | "select" | "button")
            || is_disabled(document, node)
            || has_datalist_ancestor(document, node)
        {
            return true;
        }
        let Some(name) = attribute(document, node, "name").filter(|name| !name.is_empty()) else {
            return true;
        };
        let input_type = match tag {
            "input" => input_type_state(document, node),
            "button" => match button_type_state(document, node) {
                Some(ButtonTypeState::Submit) => "submit",
                Some(ButtonTypeState::Reset) => "reset",
                Some(ButtonTypeState::Button) => "button",
                None => return true,
            },
            _ => "text",
        };
        if tag == "button" {
            if input_type != "submit" || submitter != Some(node) {
                return true;
            }
        } else if tag == "input"
            && (matches!(input_type, "button" | "reset" | "image")
                || input_type == "submit" && submitter != Some(node))
        {
            return true;
        }
        if tag == "input" && input_type == "file" {
            if let Some(selected_files) = state
                .files_for_control(node)
                .filter(|files| !files.is_empty())
            {
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
                        media_type: "application/octet-stream".to_owned(),
                        last_modified: 0,
                        bytes: Arc::from([]),
                    },
                ));
            }
            return true;
        }
        let is_checkbox = tag == "input" && input_type == "checkbox";
        let is_radio = tag == "input" && input_type == "radio";
        if (is_checkbox || is_radio)
            && !checked_by(document, node, |control| state.checked_for_control(control))
        {
            return true;
        }
        if tag == "select" {
            let selected =
                selected_option_ids_by(document, node, |option| state.selected_for_option(option));
            for option in selected {
                let Some((_, _option_attributes)) = element(document, option) else {
                    continue;
                };
                if option_disabled(document, option)
                    || attribute(document, option, "disabled").is_some()
                {
                    continue;
                }
                let value = option_value(document, option).unwrap_or_default();
                entries.push(FormEntry::text(name, value));
            }
            return true;
        }
        let value = if tag == "input" && input_type == "hidden" && name.eq_ignore_ascii_case("_charset_") {
            encoding.to_owned()
        } else if tag == "textarea" {
            state
                .value_for_control(node)
                .map(str::to_owned)
                .unwrap_or_else(|| control_value(document, node, tag))
        } else if is_checkbox || is_radio {
            attribute(document, node, "value")
                .unwrap_or("on")
                .to_owned()
        } else {
            state
                .value_for_control(node)
                .map(str::to_owned)
                .unwrap_or_else(|| control_value(document, node, tag))
        };
        let dirname = attribute(document, node, "dirname")
            .filter(|dirname| !dirname.is_empty())
            .filter(|_| {
                crate::directionality::is_auto_directionality_form_associated(document, node)
            });
        let dirname_value =
            dirname.map(
                |_| match crate::directionality::element_directionality_with_value_direction(
                    document,
                    node,
                    Some(crate::directionality::control_value_direction(&value)),
                ) {
                    crate::directionality::Direction::Ltr => "ltr",
                    crate::directionality::Direction::Rtl => "rtl",
                },
            );
        entries.push(FormEntry::text(name, value));
        if let (Some(dirname), Some(direction)) = (dirname, dirname_value) {
            entries.push(FormEntry::text(dirname, direction));
        }
        true
    });
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    #[test]
    fn implicit_submission_uses_default_button_or_single_blocking_input() {
        let document = html::parse(
            concat!(
                "<form id='f'><input id='first' type='search'>",
                "<input id='second' type='text' disabled>",
                "<button id='default'>send</button><button type='submit' form='f' id='external'>later</button>",
                "</form><input id='outside' type='text' form='f'>"
            ),
            64,
        )
        .unwrap();
        let by_id = |id: &str| {
            crate::selector::query_selector(&document, document.root(), &alloc::format!("#{id}"))
                .unwrap()
                .unwrap()
        };
        let form = by_id("f");
        let first = by_id("first");
        assert_eq!(
            implicit_submission(&document, first),
            Some(ImplicitSubmission::ClickDefaultButton {
                form,
                button: by_id("default"),
            })
        );
        assert_eq!(
            implicit_submission(&document, by_id("outside")),
            Some(ImplicitSubmission::ClickDefaultButton {
                form,
                button: by_id("default"),
            })
        );

        let mut no_button = html::parse(
            "<form id='single'><input id='only' type='email'></form><form id='many'><input id='a'><input id='b'></form>",
            32,
        )
        .unwrap();
        let query = |id: &str| {
            crate::selector::query_selector(&no_button, no_button.root(), &alloc::format!("#{id}"))
                .unwrap()
                .unwrap()
        };
        assert_eq!(
            implicit_submission(&no_button, query("only")),
            Some(ImplicitSubmission::SubmitForm(query("single")))
        );
        assert_eq!(implicit_submission(&no_button, query("a")), None);
        assert_eq!(implicit_submission(&no_button, query("b")), None);
        let unowned = no_button
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: Name::new("input"),
                attributes: Vec::new(),
            })
            .unwrap();
        assert_eq!(implicit_submission(&no_button, unowned), None);
    }

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
    fn selector_form_states_follow_applicability_and_control_algorithms() {
        let mut document = html::parse(
            concat!(
                "<form id='form'>",
                "<fieldset id='disabled-set' disabled><legend><input id='legend-input'></legend>",
                "<input id='fieldset-input'></fieldset>",
                "<input id='check' type='checkbox' checked required>",
                "<input id='plain' type='text' checked>",
                "<input id='hidden' type='hidden' required>",
                "<input id='readonly' type='text' readonly>",
                "<textarea id='area' required></textarea>",
                "<select id='disabled-select' disabled><option id='select-option'>one</option></select>",
                "<select id='group-select'><optgroup disabled><option id='group-option'>two</option></optgroup>",
                "<option id='default-option' selected>three</option></select>",
                "<input id='default-input' type='image' alt='send'>",
                "<button id='later-button'>send</button>",
                "<input id='later-submit' type='submit'>",
                "<input id='default-check' type='radio' checked>",
                "</form><form><button id='default-button'>send</button><input type='submit'></form>",
                "<div id='not-a-control' disabled required></div>",
                "<div id='editable' contenteditable='true'><span id='editable-child'></span></div>"
            ),
            128,
        )
        .unwrap();
        let id = |document: &Document, name: &str| {
            crate::selector::get_element_by_id(document, document.root(), name)
                .unwrap()
                .unwrap()
        };
        let plain = id(&document, "plain");
        document
            .set_attribute_ns(plain, Some("urn:example"), "disabled", "")
            .unwrap();
        let matches = |document: &Document, name, pseudo| {
            matches_form_state_pseudo(document, id(document, name), pseudo, &NoValidityOverrides)
        };

        assert!(matches(&document, "legend-input", FormStatePseudo::Enabled));
        assert!(matches(
            &document,
            "fieldset-input",
            FormStatePseudo::Disabled
        ));
        assert!(matches(
            &document,
            "disabled-set",
            FormStatePseudo::Disabled
        ));
        assert!(!matches(
            &document,
            "not-a-control",
            FormStatePseudo::Disabled
        ));
        assert!(!matches(
            &document,
            "not-a-control",
            FormStatePseudo::Enabled
        ));
        assert!(matches(&document, "check", FormStatePseudo::Checked));
        assert!(!matches(&document, "plain", FormStatePseudo::Checked));
        assert!(matches(
            &document,
            "default-option",
            FormStatePseudo::Checked
        ));
        assert!(matches(
            &document,
            "default-option",
            FormStatePseudo::Default
        ));
        assert!(matches(
            &document,
            "select-option",
            FormStatePseudo::Checked
        ));
        assert!(matches(
            &document,
            "select-option",
            FormStatePseudo::Disabled
        ));
        assert!(!option_disabled(&document, id(&document, "select-option")));
        assert!(matches(
            &document,
            "group-option",
            FormStatePseudo::Disabled
        ));
        assert!(matches(&document, "check", FormStatePseudo::Required));
        assert!(!matches(&document, "hidden", FormStatePseudo::Required));
        assert!(!matches(&document, "hidden", FormStatePseudo::Optional));
        assert!(matches(&document, "area", FormStatePseudo::Required));
        assert!(matches(
            &document,
            "legend-input",
            FormStatePseudo::Optional
        ));
        assert!(matches(&document, "check", FormStatePseudo::ReadOnly));
        assert!(matches(&document, "plain", FormStatePseudo::ReadWrite));
        assert!(matches(&document, "readonly", FormStatePseudo::ReadOnly));
        assert!(matches(
            &document,
            "fieldset-input",
            FormStatePseudo::ReadOnly
        ));
        assert!(matches(
            &document,
            "editable-child",
            FormStatePseudo::ReadWrite
        ));
        assert!(matches(
            &document,
            "not-a-control",
            FormStatePseudo::ReadOnly
        ));
        assert!(matches(
            &document,
            "default-input",
            FormStatePseudo::Default
        ));
        assert!(!matches(
            &document,
            "later-button",
            FormStatePseudo::Default
        ));
        assert!(matches(
            &document,
            "default-button",
            FormStatePseudo::Default
        ));
        assert!(!matches(
            &document,
            "later-submit",
            FormStatePseudo::Default
        ));
        assert!(matches(
            &document,
            "default-check",
            FormStatePseudo::Default
        ));

        // A namespaced lookalike is not the null-namespace disabled attribute.
        assert!(matches(&document, "plain", FormStatePseudo::Enabled));
    }

    #[test]
    fn user_validity_and_placeholder_pseudos_follow_live_control_state() {
        struct LiveState {
            interacted: NodeId,
            value_node: NodeId,
            value: &'static str,
        }
        impl ValidityStateView for LiveState {
            fn user_validity_interacted(&self, node: NodeId) -> bool {
                node == self.interacted
            }

            fn value_override(&self, node: NodeId) -> Option<&str> {
                (node == self.value_node).then_some(self.value)
            }
        }

        let document = html::parse(
            "<form id='f'><input id='invalid' required placeholder='hint'><input id='valid'><textarea id='area' placeholder='area'></textarea><input id='check' type='checkbox' placeholder='ignored'><input id='date' type='date' placeholder='ignored'></form>",
            64,
        )
        .unwrap();
        let id = |name: &str| {
            crate::selector::get_element_by_id(&document, document.root(), name)
                .unwrap()
                .unwrap()
        };
        let invalid = id("invalid");
        let valid = id("valid");
        let form = id("f");
        let matches = |node, selector, view: &dyn ValidityStateView| {
            crate::selector::matches_with_validity(&document, node, selector, view).unwrap()
        };

        assert!(!matches(
            invalid,
            "input:user-invalid",
            &NoValidityOverrides
        ));
        let interacted_invalid = LiveState {
            interacted: invalid,
            value_node: valid,
            value: "",
        };
        assert!(matches(
            invalid,
            "input:user-invalid",
            &interacted_invalid
        ));
        assert!(!matches(invalid, "input:user-valid", &interacted_invalid));

        let interacted_valid = LiveState {
            interacted: valid,
            value_node: invalid,
            value: "",
        };
        assert!(matches(valid, "input:user-valid", &interacted_valid));
        assert!(!matches(valid, "input:user-invalid", &interacted_valid));
        assert!(!matches(form, ":user-invalid", &interacted_invalid));
        assert!(!matches(form, ":user-valid", &interacted_valid));

        assert!(matches(
            invalid,
            "input:placeholder-shown",
            &NoValidityOverrides
        ));
        assert!(matches(
            id("area"),
            "textarea:placeholder-shown",
            &NoValidityOverrides
        ));
        assert!(!matches(
            id("check"),
            "input:placeholder-shown",
            &NoValidityOverrides
        ));
        assert!(!matches(
            id("date"),
            "input:placeholder-shown",
            &NoValidityOverrides
        ));
        let filled = LiveState {
            interacted: valid,
            value_node: invalid,
            value: "typed",
        };
        assert!(!matches(
            invalid,
            "input:placeholder-shown",
            &filled
        ));
        assert!(crate::css::parse_selector_list("input:user-valid(x)", 0).is_err());
    }

    #[test]
    fn validation_candidates_respect_datalist_and_control_specific_states() {
        let document = html::parse(
            "<form><button id='implicit' readonly>Send</button><button id='submit' type='SUBMIT' readonly>Send</button><button id='reset' type='RESET'>Reset</button><button id='button' type='BUTTON'>Button</button><button id='invalid' type='menu'>Menu</button><input id='input-submit' type='submit'><input id='input-image' type='image'><textarea id='readonly-area' readonly></textarea><datalist><input id='inside-input'><button id='inside-button'>Send</button><select id='inside-select'></select><textarea id='inside-area'></textarea></datalist></form>",
            96,
        )
        .unwrap();
        let id = |name: &str| {
            crate::selector::get_element_by_id(&document, document.root(), name)
                .unwrap()
                .unwrap()
        };
        for name in [
            "implicit",
            "submit",
            "invalid",
            "input-submit",
            "input-image",
        ] {
            assert!(will_validate(&document, id(name)), "{name} should validate");
        }
        for name in [
            "reset",
            "button",
            "readonly-area",
            "inside-input",
            "inside-button",
            "inside-select",
            "inside-area",
        ] {
            assert!(!will_validate(&document, id(name)), "{name} is barred");
        }
    }

    #[test]
    fn barred_controls_keep_only_applicable_constraint_flags() {
        let document = html::parse(
            "<form><datalist><textarea id='inside' required></textarea></datalist><textarea id='outside' required></textarea><input id='disabled-text' required disabled><input id='readonly-text' required readonly><input id='readonly-date' type='date' required readonly><input id='readonly-checkbox' type='checkbox' required readonly><input id='disabled-checkbox' type='checkbox' required disabled><input id='readonly-range' type='range' required readonly><input id='readonly-color' type='color' required readonly><input id='readonly-file' type='file' required readonly><input id='required-file' type='file' required><input id='required-button' type='button' required></form>",
            64,
        )
        .unwrap();
        let id = |name: &str| {
            crate::selector::get_element_by_id(&document, document.root(), name)
                .unwrap()
                .unwrap()
        };

        let inside = id("inside");
        assert!(!will_validate(&document, inside));
        let inside_validity = validity(&document, inside, "");
        assert!(inside_validity.value_missing);
        assert!(!inside_validity.valid());

        let outside = id("outside");
        assert!(will_validate(&document, outside));
        assert!(validity(&document, outside, "").value_missing);

        let disabled_text = id("disabled-text");
        assert!(!will_validate(&document, disabled_text));
        assert!(!validity(&document, disabled_text, "").value_missing);

        let readonly_text = id("readonly-text");
        assert!(!will_validate(&document, readonly_text));
        assert!(!validity(&document, readonly_text, "").value_missing);

        let readonly_date = id("readonly-date");
        assert!(!will_validate(&document, readonly_date));
        assert!(!validity(&document, readonly_date, "").value_missing);

        // The attribute does not make these controls readonly, but its
        // presence still bars input elements from constraint validation.
        let readonly_checkbox = id("readonly-checkbox");
        assert!(!will_validate(&document, readonly_checkbox));
        assert!(validity(&document, readonly_checkbox, "").value_missing);

        let disabled_checkbox = id("disabled-checkbox");
        assert!(!will_validate(&document, disabled_checkbox));
        assert!(validity(&document, disabled_checkbox, "").value_missing);

        let readonly_range = id("readonly-range");
        assert!(!will_validate(&document, readonly_range));
        assert!(!validity(&document, readonly_range, "").value_missing);

        for name in ["readonly-color", "readonly-file"] {
            assert!(!will_validate(&document, id(name)), "{name} is barred");
        }

        let required_file = id("required-file");
        assert!(will_validate(&document, required_file));
        assert!(validity(&document, required_file, "").value_missing);

        let required_button = id("required-button");
        assert!(!will_validate(&document, required_button));
        assert!(!validity(&document, required_button, "").value_missing);
    }

    #[test]
    fn file_validity_uses_the_live_selected_file_list() {
        struct HasFiles;
        impl ValidityStateView for HasFiles {
            fn has_selected_files(&self, _input: NodeId) -> bool {
                true
            }
        }

        let document = html::parse("<input type='file' required>", 8).unwrap();
        let input = crate::selector::query_selector(&document, document.root(), "input")
            .unwrap()
            .unwrap();
        assert!(validity(&document, input, "").value_missing);
        assert!(!validity_with_view(&document, input, &HasFiles).value_missing);
    }

    #[test]
    fn radio_group_validity_applies_to_every_member_and_keeps_empty_names_independent() {
        let document = html::parse(
            "<form><input id='required' type='radio' name='group' required><input id='peer' type='radio' name='group'><input id='disabled-required' type='radio' name='disabled-group' required disabled><input id='enabled-peer' type='radio' name='disabled-group'><input id='unnamed-required' type='radio' required><input id='unnamed-checked' type='radio' checked></form>",
            64,
        )
        .unwrap();
        let id = |name: &str| {
            crate::selector::get_element_by_id(&document, document.root(), name)
                .unwrap()
                .unwrap()
        };
        assert!(validity(&document, id("required"), "").value_missing);
        assert!(validity(&document, id("peer"), "").value_missing);
        assert!(validity(&document, id("enabled-peer"), "").value_missing);
        assert!(validity(&document, id("unnamed-required"), "").value_missing);
        assert!(!validity(&document, id("unnamed-checked"), "").value_missing);
    }

    #[test]
    fn date_time_constraints_parse_values_and_apply_default_steps_and_bases() {
        let document = html::parse(
            "<form>
            <input id='date' type='date' value='2024-02-29' step='2'>
            <input id='date_bad' type='date' value='2023-02-29'>
            <input id='month' type='month' value='1970-03'>
            <input id='week' type='week' value='1970-W02'>
            <input id='time' type='time' value='12:30'>
            <input id='time_wrap' type='time' min='23:00' max='02:00' value='12:00'>
            <input id='local' type='datetime-local' value='2024-01-01T12:30'>
            <input id='number_default' type='number' min='0' value='0.5'>
            <input id='number_base' type='number' value='0.5' step='2'>
        </form>",
            64,
        )
        .unwrap();
        let check = |id: &str| {
            let node = crate::selector::query_selector(
                &document,
                document.root(),
                &alloc::format!("#{id}"),
            )
            .unwrap()
            .unwrap();
            validity(&document, node, "")
        };
        assert!(!check("date").step_mismatch);
        assert!(!check("date_bad").step_mismatch);
        assert!(!check("month").step_mismatch);
        assert!(!check("week").step_mismatch);
        assert!(!check("time").step_mismatch);
        let wrapped = check("time_wrap");
        assert!(wrapped.range_underflow && wrapped.range_overflow);
        assert!(!check("local").step_mismatch);
        assert!(check("number_default").step_mismatch);
        assert!(!check("number_base").step_mismatch);
    }

    #[test]
    fn numeric_and_date_value_grammars_use_html_sanitization_and_step_defaults() {
        assert_eq!(parse_html_float(".5"), Some(0.5));
        assert_eq!(parse_html_float("-.5"), Some(-0.5));
        assert_eq!(parse_html_float("1."), None);
        assert_eq!(
            input_value_number("12:00:00.123", "time"),
            Some(43_200_123.0)
        );
        assert_eq!(input_value_number("12:00:00.1234", "time"), None);
        assert_eq!(input_value_number("12:00:00.", "time"), None);
        assert!(input_value_number("999999999999999999999999-01-01", "date").is_none());
        assert_eq!(sanitize_input_value("date", "2024-02-30"), "");
        assert_eq!(sanitize_input_value("date", "2024-00-01"), "");
        assert_eq!(sanitize_input_value("date", "2024-13-01"), "");
        assert_eq!(sanitize_input_value("number", "not a number"), "");
        assert_eq!(
            sanitize_input_value("datetime-local", "2024-01-01 12:00:00"),
            "2024-01-01T12:00"
        );
        assert_eq!(
            sanitize_input_value("datetime-local", "2024-01-01T12:00:00.120"),
            "2024-01-01T12:00:00.12"
        );

        let document = html::parse("<form><input id='week' type='week' min='1970-W01' step='2' value='1970-W02'><input id='bad_step' type='number' min='0' step='0' value='1.5'><input id='decimal_step' type='number' min='0' step='.5' value='.5'><input id='plus_step' type='number' min='0' step='+3' value='2'></form>", 64).unwrap();
        let check = |id: &str| {
            let node = crate::selector::query_selector(
                &document,
                document.root(),
                &alloc::format!("#{id}"),
            )
            .unwrap()
            .unwrap();
            validity(&document, node, "")
        };
        assert!(check("week").step_mismatch);
        assert!(check("bad_step").step_mismatch);
        assert!(!check("decimal_step").step_mismatch);
        assert!(!check("plus_step").step_mismatch);
    }

    #[test]
    fn numeric_input_value_as_number_and_serialization_cover_calendar_domains() {
        let document = html::parse(
            concat!(
                "<input id='number' type='number'>",
                "<input id='range' type='range' min='0' max='100'>",
                "<input id='date' type='date'>",
                "<input id='month' type='month'>",
                "<input id='week' type='week'>",
                "<input id='time' type='time'>",
                "<input id='local' type='datetime-local'>",
                "<input id='text' type='text'>"
            ),
            32,
        )
        .unwrap();
        let id = |name: &str| {
            crate::selector::get_element_by_id(&document, document.root(), name)
                .unwrap()
                .unwrap()
        };
        let number = id("number");
        let range = id("range");
        let date = id("date");
        let month = id("month");
        let week = id("week");
        let time = id("time");
        let local = id("local");
        let text = id("text");

        assert!(input_value_as_number(&document, number, "").is_nan());
        assert!(input_value_as_number(&document, text, "12").is_nan());
        assert_eq!(input_value_as_number(&document, number, "1.25"), 1.25);
        assert_eq!(
            input_value_as_number(&document, date, "1969-12-31"),
            -86_400_000.0
        );
        assert_eq!(input_value_as_number(&document, month, "1969-12"), -1.0);
        assert_eq!(
            input_value_as_number(&document, week, "2019-W50"),
            1_575_849_600_000.0
        );
        assert_eq!(
            input_value_as_number(&document, time, "12:00:00.123"),
            43_200_123.0
        );
        assert_eq!(input_value_as_number(&document, range, ""), 50.0);

        assert_eq!(
            input_number_value_string(&document, number, 123.456),
            Ok("123.456".into())
        );
        assert_eq!(
            input_number_value_string(&document, date, 0.0),
            Ok("1970-01-01".into())
        );
        assert_eq!(
            input_number_value_string(&document, month, 0.0),
            Ok("1970-01".into())
        );
        assert_eq!(
            input_number_value_string(&document, month, -1.0),
            Ok("1969-12".into())
        );
        assert_eq!(
            input_number_value_string(&document, week, 0.0),
            Ok("1970-W01".into())
        );
        assert_eq!(
            input_number_value_string(&document, time, 43_200_123.0),
            Ok("12:00:00.123".into())
        );
        assert_eq!(
            input_number_value_string(&document, time, -3_600_000.0),
            Ok("23:00".into())
        );
        assert_eq!(
            input_number_value_string(&document, local, -86_400_000.0),
            Ok("1969-12-31T00:00".into())
        );
        assert_eq!(
            input_number_value_string(&document, number, f64::NAN),
            Ok(String::new())
        );
        assert_eq!(
            input_number_value_string(&document, month, 0.5),
            Err(InputNumberError::Unrepresentable)
        );
        assert_eq!(
            input_number_value_string(&document, date, -62_167_219_200_000.0),
            Err(InputNumberError::Unrepresentable)
        );
        assert_eq!(
            input_number_value_string(&document, text, 1.0),
            Err(InputNumberError::UnsupportedType)
        );
    }

    #[test]
    fn numeric_step_up_down_realign_clamp_and_preserve_decimal_steps() {
        let document = html::parse(
            concat!(
                "<input id='even' type='number' min='0' step='2'>",
                "<input id='odd' type='number' min='0' step='3'>",
                "<input id='decimal' type='number' step='0.3'>",
                "<input id='clamp' type='number' step='3' max='7'>",
                "<input id='positive' type='number' min='7'>",
                "<input id='negative' type='number' min='-7'>",
                "<input id='empty' type='number'>",
                "<input id='reverse' type='number' min='10' max='5'>",
                "<input id='any' type='number' step='any'>",
                "<input id='overflow' type='number' min='0' max='10' step='1e308'>"
            ),
            32,
        )
        .unwrap();
        let id = |name: &str| {
            crate::selector::get_element_by_id(&document, document.root(), name)
                .unwrap()
                .unwrap()
        };

        let even = id("even");
        assert_eq!(
            input_step_value(&document, id("overflow"), "0", 2, InputStepDirection::Up),
            Ok(Some("0".into()))
        );
        assert_eq!(
            input_step_value(&document, even, "5", 1, InputStepDirection::Up),
            Ok(Some("6".into()))
        );
        assert_eq!(
            input_step_value(&document, even, "6", 1, InputStepDirection::Up),
            Ok(Some("8".into()))
        );

        let odd = id("odd");
        assert_eq!(
            input_step_value(&document, odd, "8", 1, InputStepDirection::Down),
            Ok(Some("6".into()))
        );
        let decimal = id("decimal");
        assert_eq!(
            input_step_value(&document, decimal, "1.2", 1, InputStepDirection::Down),
            Ok(Some("0.9".into()))
        );
        assert_eq!(
            input_step_value(&document, decimal, "0.9", 0, InputStepDirection::Up),
            Ok(Some("0.9".into()))
        );

        let clamp = id("clamp");
        assert_eq!(
            input_step_value(&document, clamp, "0", 3, InputStepDirection::Up),
            Ok(Some("6".into()))
        );
        assert_eq!(
            input_step_value(&document, id("positive"), "", 1, InputStepDirection::Down),
            Ok(None)
        );
        assert_eq!(
            input_step_value(&document, id("negative"), "", 1, InputStepDirection::Down),
            Ok(Some("-1".into()))
        );
        assert_eq!(
            input_step_value(&document, id("empty"), "", 1, InputStepDirection::Down),
            Ok(Some("-1".into()))
        );
        assert_eq!(
            input_step_value(&document, id("reverse"), "0", 1, InputStepDirection::Up),
            Ok(None)
        );
        assert_eq!(
            input_step_value(&document, id("any"), "0", 1, InputStepDirection::Up),
            Err(InputNumberError::StepAny)
        );
        assert_eq!(
            input_step_value(&document, id("even"), "0", 0, InputStepDirection::Up),
            Ok(Some("0".into()))
        );
        assert_eq!(
            input_step_value(&document, id("even"), "0", -1, InputStepDirection::Up),
            Ok(Some("0".into()))
        );
        assert_eq!(
            input_step_value(&document, id("empty"), "0", 1, InputStepDirection::Down),
            Ok(Some("-1".into()))
        );
        assert_eq!(
            input_step_value(&document, id("even"), "0", 1, InputStepDirection::Down),
            Ok(Some("0".into()))
        );
        assert_eq!(
            input_step_value(&document, id("negative"), "-7", 1, InputStepDirection::Down),
            Ok(Some("-7".into()))
        );
        assert_eq!(
            input_step_value(&document, id("positive"), "7", 1, InputStepDirection::Down),
            Ok(Some("7".into()))
        );
        assert_eq!(
            input_step_value(&document, id("odd"), "5", 1, InputStepDirection::Up),
            Ok(Some("6".into()))
        );
    }

    #[test]
    fn range_values_use_default_bounds_clamping_and_step_rounding() {
        assert_eq!(sanitize_input_value("range", ""), "50");
        assert_eq!(
            sanitize_input_value_with_constraints(
                "range",
                "bad",
                Some("10"),
                Some("20"),
                Some("3")
            ),
            "16"
        );
        assert_eq!(
            sanitize_input_value_with_constraints("range", "2", Some("10"), Some("20"), Some("3")),
            "10"
        );
        assert_eq!(
            sanitize_input_value_with_constraints("range", "2", Some("0"), Some("10"), Some("4")),
            "4"
        );
        assert_eq!(
            sanitize_input_value_with_constraints("range", "100", Some("2"), Some("8"), Some("2")),
            "8"
        );
        assert_eq!(
            sanitize_input_value_with_constraints("range", "4", Some("10"), Some("5"), None),
            "10"
        );
        assert_eq!(
            sanitize_input_value_with_constraints(
                "range",
                "3.25",
                Some("0"),
                Some("10"),
                Some("any")
            ),
            "3.25"
        );
        assert_eq!(
            sanitize_input_value_with_constraints("range", "3.5", Some("0"), Some("10"), Some("1")),
            "4"
        );
        assert_eq!(
            sanitize_input_value_with_constraints("range", "10", Some("0"), Some("10"), Some("6")),
            "6"
        );
        assert_eq!(
            sanitize_input_value_with_attributes(
                "range",
                "10",
                None,
                None,
                Some("6"),
                Some("9"),
                false
            ),
            "9"
        );
        assert_eq!(
            sanitize_input_value_with_constraints("range", "10", None, None, Some("6")),
            "12"
        );
        assert_eq!(
            sanitize_input_value_with_constraints(
                "range",
                "0",
                Some("-1e308"),
                Some("1e308"),
                Some("1e308")
            ),
            "0"
        );
    }

    #[test]
    fn input_value_modes_share_default_and_filename_behavior() {
        let document = html::parse(
            "<input id='text' value='seed'><input id='check' type='checkbox'><input id='radio' type='radio' value='yes'><input id='hidden' type='hidden' value='secret'><input id='file' type='file'><input id='unknown' type='obsolete' value='legacy'><output id='output'>seed<b> nested</b><!-- ignored --></output>",
            64,
        )
        .unwrap();
        let id = |selector| {
            crate::selector::query_selector(&document, document.root(), selector)
                .unwrap()
                .unwrap()
        };
        let text = id("#text");
        let check = id("#check");
        let radio = id("#radio");
        let hidden = id("#hidden");
        let file = id("#file");
        let unknown = id("#unknown");
        let output = id("#output");
        assert_eq!(
            input_value_mode(&document, text),
            Some(InputValueMode::Value)
        );
        assert_eq!(
            input_value_mode(&document, check),
            Some(InputValueMode::DefaultOn)
        );
        assert_eq!(
            input_value_mode(&document, radio),
            Some(InputValueMode::DefaultOn)
        );
        assert_eq!(
            input_value_mode(&document, hidden),
            Some(InputValueMode::Default)
        );
        assert_eq!(
            input_value_mode(&document, file),
            Some(InputValueMode::Filename)
        );
        assert_eq!(
            input_value_mode(&document, unknown),
            Some(InputValueMode::Value)
        );
        assert_eq!(default_value(&document, check).as_deref(), Some("on"));
        assert_eq!(default_value(&document, radio).as_deref(), Some("yes"));
        assert_eq!(default_value(&document, hidden).as_deref(), Some("secret"));
        assert_eq!(default_value(&document, file).as_deref(), Some(""));
        assert_eq!(default_value(&document, unknown).as_deref(), Some("legacy"));
        assert_eq!(
            default_value(&document, output).as_deref(),
            Some("seed nested")
        );

        let mut xml_document = Document::new(8);
        let prefixed_html_input = xml_document
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: Name::new("p:input"),
                attributes: Vec::new(),
            })
            .unwrap();
        let foreign_input = xml_document
            .create(NodeKind::Element {
                namespace: Namespace::Other(alloc::rc::Rc::from("urn:foreign")),
                name: Name::new("input"),
                attributes: Vec::new(),
            })
            .unwrap();
        assert_eq!(
            input_value_mode(&xml_document, prefixed_html_input),
            Some(InputValueMode::Value)
        );
        assert_eq!(input_value_mode(&xml_document, foreign_input), None);
    }

    #[test]
    fn week_inputs_do_not_expose_text_selection() {
        let document = html::parse(
            "<input id='week' type='week'><input id='month' type='month'><input id='text' type='text'><input id='unknown' type='obsolete'>",
            32,
        )
        .unwrap();
        let input = |id: &str| {
            crate::selector::query_selector(&document, document.root(), &alloc::format!("#{id}"))
                .unwrap()
                .unwrap()
        };
        assert!(!supports_text_selection(&document, input("week")));
        assert!(!supports_text_selection(&document, input("month")));
        assert!(supports_text_selection(&document, input("text")));
        assert!(supports_text_selection(&document, input("unknown")));
    }

    #[test]
    fn disabled_fieldset_exempts_only_its_first_direct_legend() {
        let document = crate::html::parse(
            "<fieldset disabled><div><legend><input id='nested'></legend></div><legend><span><input id='exempt'></span></legend><legend><input id='later'></legend><input id='plain'><fieldset><legend><input id='inner'></legend></fieldset></fieldset>",
            48,
        ).unwrap();
        for (id, disabled) in [
            ("nested", true),
            ("exempt", false),
            ("later", true),
            ("plain", true),
            ("inner", true),
        ] {
            let node = crate::selector::get_element_by_id(&document, document.root(), id)
                .unwrap()
                .unwrap();
            assert_eq!(
                is_disabled(&document, node),
                disabled,
                "fieldset disabledness for {id}"
            );
        }
    }

    #[test]
    fn text_and_color_input_values_use_html_sanitization() {
        for kind in ["text", "search", "tel", "password"] {
            assert_eq!(
                sanitize_input_value(kind, "left\r\nright\0"),
                "leftright\0",
                "{kind} values strip CR and LF while preserving other code points"
            );
        }
        assert_eq!(sanitize_input_value("color", "not-a-color"), "#000000");
        assert_eq!(sanitize_input_value("color", "#Ab09fF"), "#ab09ff");
    }

    #[test]
    fn detached_form_reset_controls_follow_their_own_tree() {
        let mut document = html::parse(
            "<form id='detached'><input form='detached'><textarea></textarea></form>",
            32,
        )
        .unwrap();
        let form = crate::selector::query_selector(&document, document.root(), "form")
            .unwrap()
            .unwrap();
        let input = crate::selector::query_selector(&document, form, "input")
            .unwrap()
            .unwrap();
        let textarea = crate::selector::query_selector(&document, form, "textarea")
            .unwrap()
            .unwrap();
        document.remove(form).unwrap();

        assert_eq!(form_owner(&document, input), Some(form));
        assert_eq!(form_reset_controls(&document, form), vec![input, textarea]);

        let duplicate_ids = html::parse(
            "<div><input form='same'><span id='same'></span><form id='same'></form></div>",
            32,
        )
        .unwrap();
        let input = crate::selector::query_selector(&duplicate_ids, duplicate_ids.root(), "input")
            .unwrap()
            .unwrap();
        assert_eq!(form_owner(&duplicate_ids, input), None);
    }

    #[test]
    fn form_elements_are_live_tree_order_and_exclude_image_submitters() {
        let document = html::parse(
            "<div><form id='f'><input id='first'><input type='image' id='image'><object id='object'></object></form><input id='external' form='f'></div>",
            64,
        )
        .unwrap();
        let form = crate::selector::query_selector(&document, document.root(), "form")
            .unwrap()
            .unwrap();
        let expected = ["first", "object", "external"].map(|id| {
            crate::selector::query_selector(&document, document.root(), &alloc::format!("#{id}"))
                .unwrap()
                .unwrap()
        });
        assert_eq!(form_element_count(&document, form).unwrap(), expected.len());
        assert_eq!(
            (0..expected.len())
                .map(|index| form_element_at(&document, form, index).unwrap().unwrap())
                .collect::<Vec<_>>(),
            expected.to_vec()
        );
        assert_eq!(
            form_element_at(&document, form, expected.len()).unwrap(),
            None
        );
        assert!(form_owner(&document, expected[1]).is_some());
    }

    #[test]
    fn legacy_form_named_candidates_include_only_ancestor_owned_images() {
        let document = html::parse(
            "<form id='f'><img id='inside' form='other'><input id='image-button' type='image'><input id='listed'></form><form id='other'></form><img id='outside' form='f'>",
            64,
        )
        .unwrap();
        let get = |id| {
            crate::selector::get_element_by_id(&document, document.root(), id)
                .unwrap()
                .unwrap()
        };
        let form = get("f");
        let inside = get("inside");
        let image_button = get("image-button");
        let listed = get("listed");
        let outside = get("outside");

        assert_eq!(form_owner(&document, inside), Some(form));
        assert_eq!(form_owner(&document, outside), None);
        assert_eq!(form_element_count(&document, form).unwrap(), 1);
        assert_eq!(form_element_at(&document, form, 0).unwrap(), Some(listed));

        let mut candidates = Vec::new();
        for_each_form_named_element(&document, form, |node, is_image| {
            candidates.push((node, is_image));
            true
        })
        .unwrap();
        assert_eq!(candidates, vec![(inside, true), (listed, false)]);
        assert!(!candidates.iter().any(|(node, _)| *node == image_button));
        assert!(!candidates.iter().any(|(node, _)| *node == outside));
    }

    #[test]
    fn email_and_url_type_mismatch_use_html_syntax_and_absolute_urls() {
        let document = html::parse("<form>
            <input id='local' type='email' value='a@localhost'>
            <input id='bad_label' type='email' value='a@-example.test'>
            <input id='empty_label' type='email' value='a@example..test'>
            <input id='long_label' type='email' value='a@abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijkl.test'>
            <input id='mail' type='url' value='mailto:a@example.test'>
            <input id='relative' type='url' value='/relative/path'>
        </form>", 64).unwrap();
        let mismatch = |id: &str| {
            let node = crate::selector::query_selector(
                &document,
                document.root(),
                &alloc::format!("#{id}"),
            )
            .unwrap()
            .unwrap();
            validity(&document, node, "").type_mismatch
        };
        assert!(!mismatch("local"));
        assert!(mismatch("bad_label"));
        assert!(mismatch("empty_label"));
        assert!(mismatch("long_label"));
        assert!(!mismatch("mail"));
        assert!(mismatch("relative"));
        assert_eq!(
            sanitize_input_value("url", " \nhttps://example.test/path\r "),
            "https://example.test/path"
        );
        assert_eq!(
            sanitize_input_value("email", " a@example.test\n"),
            "a@example.test"
        );
        assert_eq!(
            sanitize_input_value_with_attributes(
                "email",
                " a@example.test , b@localhost ",
                None,
                None,
                None,
                None,
                true
            ),
            "a@example.test,b@localhost"
        );
    }

    #[test]
    fn textarea_ignores_input_type_specific_validity_constraints() {
        let document = html::parse("<form><textarea id='date' type='date' min='2024-01-01'>not-a-date</textarea><textarea id='number' type='number'>not-a-number</textarea><textarea id='email' type='email'>not-an-email</textarea></form>", 64).unwrap();
        for id in ["date", "number", "email"] {
            let node = crate::selector::query_selector(
                &document,
                document.root(),
                &alloc::format!("#{id}"),
            )
            .unwrap()
            .unwrap();
            let state = validity(&document, node, "");
            assert!(state.valid());
            assert!(
                !state.bad_input
                    && !state.type_mismatch
                    && !state.range_underflow
                    && !state.range_overflow
                    && !state.step_mismatch
            );
        }
    }

    #[test]
    fn pattern_constraint_uses_unicode_sets_full_matches_and_email_lists() {
        let document = html::parse(r#"<form>
            <input id='set' pattern='[\p{Letter}&amp;&amp;[^A-Z]]+' value='é'>
            <input id='partial' pattern='[0-9]+' value='x12'>
            <input id='invalid' pattern='[' value='anything'>
            <input id='empty-pattern' pattern='' value='x'>
            <input id='empty-value' pattern='[0-9]+' value=''>
            <input id='list' type='email' multiple pattern='.+@example\.org' value='a@example.org, b@example.org'>
            <input id='wrong-list' type='email' multiple pattern='.+@example\.org' value='a@example.org,b@other.org'>
            <input id='number' type='number' pattern='[A-Z]+' value='12'>
            <input id='unknown' type='obsolete' pattern='[0-9]+' value='letters'>
            <textarea id='area' pattern='[0-9]+'>letters</textarea>
        </form>"#, 64).unwrap();
        let check = |id: &str| {
            let node = crate::selector::query_selector(
                &document,
                document.root(),
                &alloc::format!("#{id}"),
            )
            .unwrap()
            .unwrap();
            validity(&document, node, "").pattern_mismatch
        };
        assert!(!check("set"));
        assert!(check("partial"));
        assert!(!check("invalid"));
        assert!(check("empty-pattern"));
        assert!(!check("empty-value"));
        assert!(!check("list"));
        assert!(check("wrong-list"));
        assert!(!check("number"));
        assert!(check("unknown"));
        assert!(!check("area"));
    }

    #[test]
    fn form_urlencoded_legacy_encodings_preserve_separators_references_and_exact_budget() {
        let entries = vec![FormEntry::text("n&", "あ\n😀"), FormEntry::text("n&", "い")];
        let expected = "n%26=%82%A0%0D%0A%26%23128512%3B&n%26=%82%A2";
        assert_eq!(encode_form_urlencoded_with_encoding(&entries, "Shift_JIS", expected.len()).unwrap(), expected);
        assert_eq!(encode_form_urlencoded_with_encoding(&entries, "Shift_JIS", expected.len()-1), Err(Error::LimitExceeded));
        assert_eq!(encode_form_urlencoded_with_encoding(&[], "Shift_JIS", 0).unwrap(), "");
        assert_eq!(encode_form_urlencoded_with_encoding(&[FormEntry::text("n", "あ")], "ISO-2022-JP", 64).unwrap(), "n=%1B%24B%24%22%1B%28B");
        let boundary = alloc::format!("{}😀あ\r\n", "a".repeat(4091));
        let entries = vec![FormEntry::text("n", boundary)];
        let expected = alloc::format!("n={}%26%23128512%3B%82%A0%0D%0A", "a".repeat(4091));
        assert_eq!(encode_form_urlencoded_with_encoding(&entries, "Shift_JIS", expected.len()).unwrap(), expected);
    }

    #[test]
    fn form_encoding_selection_uses_first_valid_label_document_fallback_and_output_mapping() {
        assert_eq!(pick_form_encoding(Some("unknown Shift_JIS UTF-8"), "windows-1252"), "Shift_JIS");
        assert_eq!(pick_form_encoding(Some("unknown"), "windows-1252"), "UTF-8");
        assert_eq!(pick_form_encoding(Some(""), "windows-1252"), "UTF-8");
        assert_eq!(pick_form_encoding(None, "windows-1252"), "windows-1252");
        assert_eq!(pick_form_encoding(Some("utf-16be"), "windows-1252"), "UTF-8");
        assert_eq!(pick_form_encoding(Some("replacement"), "windows-1252"), "UTF-8");
    }

    #[test]
    fn form_urlencoded_preserves_order_utf8_newlines_and_file_names_with_exact_bound() {
        let entries = vec![
            FormEntry::text("a b", "*.-_~+&=é"),
            FormEntry::text("a b", "a\r\nb\rc\nd"),
            FormEntry::text("line\nname", ""),
            FormEntry::file(
                "upload",
                FormFile {
                    name: String::from("a\nb.txt"),
                    media_type: String::from("application/octet-stream"),
                    last_modified: 0,
                    bytes: Arc::from(&b"contents must not appear in body"[..]),
                },
            ),
        ];
        let expected = "a+b=*.-_%7E%2B%26%3D%C3%A9&a+b=a%0D%0Ab%0D%0Ac%0D%0Ad&line%0D%0Aname=&upload=a%0D%0Ab.txt";
        assert_eq!(
            encode_form_urlencoded(&entries, expected.len()).unwrap(),
            expected
        );
        assert_eq!(
            encode_form_urlencoded(&entries, expected.len() - 1),
            Err(Error::LimitExceeded)
        );
        assert_eq!(encode_form_urlencoded(&[], 0).unwrap(), "");
        assert_eq!(
            encode_form_urlencoded(&[FormEntry::text("", "")], 1).unwrap(),
            "="
        );
        assert_eq!(
            encode_form_urlencoded(&[FormEntry::text("", "")], 0),
            Err(Error::LimitExceeded)
        );
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
    fn borrowed_entry_view_reads_only_target_form_controls_and_preserves_file_bytes_arc() {
        use core::cell::RefCell;

        struct RecordingState<'a> {
            value_node: NodeId,
            file_node: NodeId,
            external_node: NodeId,
            file: &'a FormFile,
            queries: RefCell<Vec<(u8, NodeId)>>,
        }

        impl FormEntryStateView for RecordingState<'_> {
            fn value_for_control(&self, node: NodeId) -> Option<&str> {
                self.queries.borrow_mut().push((b'v', node));
                (node == self.value_node || node == self.external_node).then_some("live")
            }

            fn selected_for_option(&self, node: NodeId) -> Option<bool> {
                self.queries.borrow_mut().push((b's', node));
                None
            }

            fn checked_for_control(&self, node: NodeId) -> Option<bool> {
                self.queries.borrow_mut().push((b'c', node));
                None
            }

            fn files_for_control(&self, node: NodeId) -> Option<&[FormFile]> {
                self.queries.borrow_mut().push((b'f', node));
                (node == self.file_node).then_some(core::slice::from_ref(self.file))
            }
        }

        let document = html::parse(
            concat!(
                "<form id='target'><input id='value' name='first'>",
                "<input id='file' type='file' name='upload'></form>",
                "<form><input id='unrelated' type='checkbox' name='other' checked></form>",
                "<input id='external' form='target' name='last'>"
            ),
            96,
        )
        .unwrap();
        let id = |name: &str| {
            crate::selector::get_element_by_id(&document, document.root(), name)
                .unwrap()
                .unwrap()
        };
        let form = id("target");
        let value_node = id("value");
        let file_node = id("file");
        let external_node = id("external");
        let unrelated_node = id("unrelated");
        let file = FormFile {
            name: "selected.bin".into(),
            media_type: "application/octet-stream".into(),
            last_modified: 7,
            bytes: Arc::from(&b"payload"[..]),
        };
        let state = RecordingState {
            value_node,
            file_node,
            external_node,
            file: &file,
            queries: RefCell::new(Vec::new()),
        };

        let entries = form_entries_with_state(&document, form, None, &state);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "upload", "last"]
        );
        assert_eq!(entries[0], FormEntry::text("first", "live"));
        assert_eq!(entries[2], FormEntry::text("last", "live"));
        let FormEntryValue::File(returned_file) = &entries[1].value else {
            panic!("file control contributes its selected File");
        };
        assert!(Arc::ptr_eq(&returned_file.bytes, &file.bytes));
        assert_eq!(
            state.queries.borrow().as_slice(),
            &[(b'v', value_node), (b'f', file_node), (b'v', external_node)]
        );
        assert!(!state
            .queries
            .borrow()
            .iter()
            .any(|(_, node)| *node == unrelated_node));
    }

    #[test]
    fn entry_list_uses_each_controls_own_type_state_and_skips_datalist_descendants() {
        let document = html::parse(
            concat!(
                "<form id='f'>",
                "<textarea type='checkbox' name='area'>area-value</textarea>",
                "<select type='file' name='choice'><option value='selected' selected>Selected</option></select>",
                "<input type='menu' name='unknown-input' value='text-state'>",
                "<input type='checkbox' name='unchecked' value='omit'>",
                "<button id='missing' name='missing' value='missing'>Missing</button>",
                "<button id='invalid' type='menu' formnovalidate name='invalid' value='invalid'>Invalid</button>",
                "<button id='casefold' type='SuBmIt' name='casefold' value='casefold'>Casefold</button>",
                "<button id='reset' type='RESET' formnovalidate name='reset' value='omit'>Reset</button>",
                "<button id='button' type='button' name='button' value='omit'>Button</button>",
                "<datalist><input id='inside-input' required name='inside-input' value='omit'>",
                "<textarea id='inside-area' required name='inside-area'>omit</textarea>",
                "<select id='inside-select' required name='inside-select'><option selected value='omit'>Omit</option></select>",
                "<input id='inside-check' type='checkbox' checked name='inside-check' value='omit'></datalist>",
                "</form>"
            ),
            96,
        )
        .unwrap();
        let form = crate::selector::query_selector(&document, document.root(), "form")
            .unwrap()
            .unwrap();
        let by_id = |id: &str| {
            crate::selector::get_element_by_id(&document, document.root(), id)
                .unwrap()
                .unwrap()
        };
        fn names(entries: &[FormEntry]) -> Vec<&str> {
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>()
        }

        assert_eq!(
            names(&form_entries(&document, form, None)),
            vec!["area", "choice", "unknown-input"]
        );
        assert_eq!(
            names(&form_entries(&document, form, Some(by_id("missing")))),
            vec!["area", "choice", "unknown-input", "missing"]
        );
        assert_eq!(
            names(&form_entries(&document, form, Some(by_id("invalid")))),
            vec!["area", "choice", "unknown-input", "invalid"]
        );
        assert_eq!(
            names(&form_entries(&document, form, Some(by_id("casefold")))),
            vec!["area", "choice", "unknown-input", "casefold"]
        );
        assert_eq!(
            button_type_state(&document, by_id("missing")),
            Some(ButtonTypeState::Submit)
        );
        assert_eq!(
            button_type_state(&document, by_id("invalid")),
            Some(ButtonTypeState::Submit)
        );
        assert_eq!(
            button_type_state(&document, by_id("casefold")),
            Some(ButtonTypeState::Submit)
        );
        assert!(validation_bypassed(&document, form, Some(by_id("invalid"))));
        assert!(!validation_bypassed(&document, form, Some(by_id("reset"))));
        for id in ["missing", "invalid", "casefold"] {
            assert!(
                will_validate(&document, by_id(id)),
                "{id} defaults to submit"
            );
        }
        for id in ["reset", "button"] {
            assert!(!will_validate(&document, by_id(id)), "{id} is barred");
        }
        for id in [
            "inside-input",
            "inside-area",
            "inside-select",
            "inside-check",
        ] {
            assert!(
                !will_validate(&document, by_id(id)),
                "{id} is in a datalist"
            );
        }
    }

    #[test]
    fn file_control_with_empty_live_file_list_contributes_empty_file_entry() {
        let document =
            html::parse("<form id='f'><input type='file' name='upload'></form>", 32).unwrap();
        let form = crate::selector::query_selector(&document, document.root(), "form")
            .unwrap()
            .unwrap();
        let input = crate::selector::query_selector(&document, document.root(), "input")
            .unwrap()
            .unwrap();
        let entries = form_entries_with_values_selectedness_checkedness_and_files(
            &document,
            form,
            None,
            &[],
            &[],
            &[],
            &[(input, Vec::new())],
        );
        assert_eq!(
            entries,
            vec![FormEntry::file(
                "upload",
                FormFile {
                    name: String::new(),
                    media_type: "application/octet-stream".to_owned(),
                    last_modified: 0,
                    bytes: Arc::from([]),
                }
            )]
        );
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
        let default_selected = select_options(&document, select)
            .into_iter()
            .chain(select_options(&document, multi))
            .map(|option| (option, attribute(&document, option, "selected").is_some()))
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
        assert_eq!(
            select_value_by(&document, select, |option| {
                cleared
                    .iter()
                    .find(|(id, _)| *id == option)
                    .map(|(_, selected)| *selected)
            }),
            ""
        );
        assert_eq!(
            select_value_by(&document, select, |_| None),
            select_value(&document, select)
        );
        assert_eq!(selected_index_with(&document, select, &cleared), -1);
        let entries =
            form_entries_with_values_and_selectedness(&document, form, None, &[], &cleared);
        assert!(!entries.iter().any(|entry| entry.name == "choice"));
    }

    #[test]
    fn single_select_uses_last_parser_selected_option_without_rewriting_attributes() {
        let document = html::parse(
            "<select id='s'><option id='first' selected>First</option><option id='last' selected>Last</option></select>",
            32,
        )
        .unwrap();
        let select = crate::selector::get_element_by_id(&document, document.root(), "s")
            .unwrap()
            .unwrap();
        let first = crate::selector::get_element_by_id(&document, document.root(), "first")
            .unwrap()
            .unwrap();
        let last = crate::selector::get_element_by_id(&document, document.root(), "last")
            .unwrap()
            .unwrap();
        let mut selected = Vec::new();
        for_each_selected_option_by(
            &document,
            select,
            |_| None,
            |option, _, _| {
                selected.push(option);
                true
            },
        )
        .unwrap();

        assert_eq!(selected, vec![last]);
        assert_eq!(select_value(&document, select), "Last");
        assert_eq!(selected_index(&document, select), 1);
        assert!(!option_is_selected_by(&document, first, |_| None).unwrap());
        assert!(option_is_selected_by(&document, last, |_| None).unwrap());
        assert!(attribute(&document, first, "selected").is_some());
        assert!(attribute(&document, last, "selected").is_some());

        let multiple = html::parse(
            "<select id='m' multiple><option id='m-first' selected>First</option><option id='m-last' selected>Last</option></select>",
            32,
        )
        .unwrap();
        let multiple_select = crate::selector::get_element_by_id(&multiple, multiple.root(), "m")
            .unwrap()
            .unwrap();
        let multiple_first =
            crate::selector::get_element_by_id(&multiple, multiple.root(), "m-first")
                .unwrap()
                .unwrap();
        let multiple_last =
            crate::selector::get_element_by_id(&multiple, multiple.root(), "m-last")
                .unwrap()
                .unwrap();
        let mut multiple_selected = Vec::new();
        for_each_selected_option_by(
            &multiple,
            multiple_select,
            |_| None,
            |option, _, _| {
                multiple_selected.push(option);
                true
            },
        )
        .unwrap();
        assert_eq!(multiple_selected, vec![multiple_first, multiple_last]);
        assert!(option_is_selected_by(&multiple, multiple_first, |_| None).unwrap());
        assert!(option_is_selected_by(&multiple, multiple_last, |_| None).unwrap());

        let display_list = html::parse(
            "<select id='list' size='2'><option>First</option><option>Second</option></select><select id='drop'><option>First</option><option>Second</option></select>",
            32,
        )
        .unwrap();
        let display_select =
            crate::selector::get_element_by_id(&display_list, display_list.root(), "list")
                .unwrap()
                .unwrap();
        let drop_select =
            crate::selector::get_element_by_id(&display_list, display_list.root(), "drop")
                .unwrap()
                .unwrap();
        assert_eq!(selected_index(&display_list, display_select), -1);
        assert_eq!(selected_index(&display_list, drop_select), 0);
    }

    #[test]
    fn select_display_size_uses_parsed_size_and_multiple_default() {
        let document = html::parse(
            "<select id='default'></select><select id='multiple' multiple></select><select id='explicit' size='2tail'></select><select id='invalid' size='bad' multiple></select>",
            32,
        )
        .unwrap();
        let id = |selector: &str| {
            crate::selector::query_selector(&document, document.root(), selector)
                .unwrap()
                .unwrap()
        };
        assert_eq!(select_display_size(&document, id("#default")), Some(1));
        assert_eq!(select_display_size(&document, id("#multiple")), Some(4));
        assert_eq!(select_display_size(&document, id("#explicit")), Some(2));
        assert_eq!(select_display_size(&document, id("#invalid")), Some(4));
    }

    #[test]
    fn select_option_walk_prunes_html_subtrees_and_respects_namespaces() {
        let mut document = Document::new(64);
        let html = |name: &str| NodeKind::Element {
            namespace: Namespace::Html,
            name: Name::new(name),
            attributes: Vec::new(),
        };
        let select = document.create(html("select")).unwrap();
        let root = document.root();
        document.append(root, select).unwrap();
        let add = |document: &mut Document, parent, name: &str| {
            let child = document.create(html(name)).unwrap();
            document.append(parent, child).unwrap();
            child
        };

        let first = add(&mut document, select, "option");
        let wrapper = add(&mut document, select, "div");
        let wrapped = add(&mut document, wrapper, "option");

        let datalist = add(&mut document, wrapper, "datalist");
        add(&mut document, datalist, "option");
        let rule = add(&mut document, wrapper, "hr");
        add(&mut document, rule, "option");

        let parent_option = add(&mut document, wrapper, "option");
        add(&mut document, parent_option, "option");
        let nested_select = add(&mut document, wrapper, "select");
        add(&mut document, nested_select, "option");

        let group = add(&mut document, wrapper, "optgroup");
        let group_first = add(&mut document, group, "option");
        let nested_group = add(&mut document, group, "optgroup");
        let nested_group_option = add(&mut document, nested_group, "option");
        let group_last = add(&mut document, group, "option");
        document
            .set_attribute_ns(group, None, "disabled", "")
            .unwrap();

        let foreign_option = document
            .create(NodeKind::Element {
                namespace: Namespace::Svg,
                name: Name::new("option"),
                attributes: Vec::new(),
            })
            .unwrap();
        document.append(wrapper, foreign_option).unwrap();
        add(&mut document, wrapper, "Option");
        let last = add(&mut document, select, "option");

        let expected = [first, wrapped, parent_option, group_first, group_last, last];
        let mut streamed = Vec::new();
        for_each_select_option(&document, select, |node, index| {
            assert_eq!(index, streamed.len());
            streamed.push(node);
            true
        })
        .unwrap();
        assert_eq!(streamed, expected);
        assert_eq!(select_options(&document, select), expected);
        assert!(option_disabled(&document, group_first));
        assert!(!option_disabled(&document, nested_group_option));
        assert!(option_disabled(&document, group_last));
        assert_eq!(select_option_count(&document, select), Ok(expected.len()));
        for (index, node) in expected.iter().copied().enumerate() {
            assert_eq!(select_option_at(&document, select, index), Ok(Some(node)));
        }
        assert_eq!(
            select_option_at(&document, select, expected.len()),
            Ok(None)
        );
    }

    #[test]
    fn select_option_walk_handles_deep_trees_and_stops_at_requested_index() {
        let mut document = Document::new(1200);
        let select = document
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: Name::new("select"),
                attributes: Vec::new(),
            })
            .unwrap();
        let root = document.root();
        document.append(root, select).unwrap();
        let mut parent = select;
        for _ in 0..900 {
            let wrapper = document
                .create(NodeKind::Element {
                    namespace: Namespace::Html,
                    name: Name::new("div"),
                    attributes: Vec::new(),
                })
                .unwrap();
            document.append(parent, wrapper).unwrap();
            parent = wrapper;
        }
        let deep_option = document
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: Name::new("option"),
                attributes: Vec::new(),
            })
            .unwrap();
        document.append(parent, deep_option).unwrap();
        let next_option = document
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: Name::new("option"),
                attributes: Vec::new(),
            })
            .unwrap();
        document.append(select, next_option).unwrap();

        assert_eq!(select_option_count(&document, select), Ok(2));
        assert_eq!(
            select_option_at(&document, select, 0),
            Ok(Some(deep_option))
        );
        assert_eq!(
            select_option_at(&document, select, 1),
            Ok(Some(next_option))
        );
        assert_eq!(select_option_at(&document, select, 2), Ok(None));
    }

    #[test]
    fn form_state_ignores_namespaced_attribute_lookalikes() {
        let mut document = html::parse(
            "<form id='f'><input id='check'><input id='enabled'><select id='s'><option id='a'>A</option><option id='b'>B</option></select></form>",
            64,
        )
        .unwrap();
        let id = |document: &Document, selector: &str| {
            crate::selector::query_selector(document, document.root(), selector)
                .unwrap()
                .unwrap()
        };
        let form = id(&document, "#f");
        let check = id(&document, "#check");
        let enabled = id(&document, "#enabled");
        let select = id(&document, "#s");
        let first = id(&document, "#a");
        let second = id(&document, "#b");

        for (element, name, value) in [
            (check, "type", "file"),
            (check, "value", "wrong"),
            (check, "name", "wrong"),
            (check, "checked", ""),
            (enabled, "disabled", ""),
            (enabled, "name", "wrong"),
            (enabled, "value", "wrong"),
            (select, "multiple", ""),
            (second, "selected", ""),
            (second, "value", "wrong"),
        ] {
            document
                .set_attribute_ns(element, Some("urn:custom"), name, value)
                .unwrap();
        }
        for (element, name, value) in [
            (check, "type", "checkbox"),
            (check, "value", "yes"),
            (check, "name", "check"),
            (enabled, "name", "active"),
            (enabled, "value", "works"),
        ] {
            document
                .set_attribute_ns(element, None, name, value)
                .unwrap();
        }

        assert_eq!(default_value(&document, check).as_deref(), Some("yes"));
        assert!(!is_disabled(&document, enabled));
        assert_eq!(option_value(&document, second).as_deref(), Some("B"));
        assert_eq!(selected_option_ids(&document, select), vec![first]);
        assert_eq!(select_value(&document, select), "A");
        set_option_selected(&mut document, second, true).unwrap();
        assert_eq!(selected_option_ids(&document, select), vec![second]);
        assert_eq!(
            form_entries(&document, form, None),
            vec![FormEntry::text("active", "works")]
        );
    }

    #[test]
    fn dirname_entries_follow_control_values_direction_and_order() {
        let document = html::parse(
            concat!(
                "<form id='f' dir='rtl'>",
                "<input id='first' name='first' dirname='firstDir' dir='auto' value='English'>",
                "<input id='live' name='live' dirname='liveDir' dir='auto' value='English'>",
                "<input id='inherited' name='inherited' dirname='inheritedDir' value='text'>",
                "<input id='explicit' name='explicit' dirname='explicitDir' dir='ltr' value='abc'>",
                "<input name='disabled' dirname='disabledDir' disabled value='skip'>",
                "</form>"
            ),
            64,
        )
        .unwrap();
        let get = |selector: &str| {
            crate::selector::query_selector(&document, document.root(), selector)
                .unwrap()
                .unwrap()
        };
        let form = get("#f");
        let live = get("#live");
        let values = [(live, "مرحبا".to_owned())];
        let state = SliceFormEntryStateView {
            values: &values,
            selectedness: &[],
            checkedness: &[],
            files: &[],
        };

        assert_eq!(
            form_entries_with_state(&document, form, None, &state),
            vec![
                FormEntry::text("first", "English"),
                FormEntry::text("firstDir", "ltr"),
                FormEntry::text("live", "مرحبا"),
                FormEntry::text("liveDir", "rtl"),
                FormEntry::text("inherited", "text"),
                FormEntry::text("inheritedDir", "rtl"),
                FormEntry::text("explicit", "abc"),
                FormEntry::text("explicitDir", "ltr"),
            ]
        );
    }
}
