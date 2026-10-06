//! Shared dialog and popover state transitions that use the document's bounded top-layer list.
//!
//! These shared state primitives back dialog and popover bindings. DOM event/task ordering,
//! focus restoration, modal inertness, and presentation painting remain adapter/rendering work.

use crate::{Document, Error, Namespace, NodeId, NodeKind};
use alloc::vec::Vec;

const MAX_ANCESTOR_STEPS: usize = 65_536;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DialogMode {
    NonModal,
    Modal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DialogModalState {
    NotDialog,
    NonModal,
    Modal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PopoverMode {
    Auto,
    Hint,
    Manual,
}

impl PopoverMode {
    pub const fn keyword(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Hint => "hint",
            Self::Manual => "manual",
        }
    }
}

/// Typed metadata stored beside each existing top-layer element. Keeping it in
/// the bounded stack distinguishes modal dialogs and popovers without a
/// parallel per-node registry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Generic,
    ModalDialog,
    Popover(PopoverMode),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectorPseudo {
    Open,
    Modal,
    PopoverOpen,
}

impl SelectorPseudo {
    pub const ALL: [Self; 3] = [Self::Open, Self::Modal, Self::PopoverOpen];

    pub const fn bit(self) -> u8 {
        match self {
            Self::Open => 1,
            Self::Modal => 2,
            Self::PopoverOpen => 4,
        }
    }

    pub fn matches(self, document: &Document, node: NodeId) -> bool {
        match self {
            Self::Open => matches_open(document, node),
            Self::Modal => matches_modal(document, node),
            Self::PopoverOpen => matches_popover_open(document, node),
        }
    }
}

pub(crate) struct Entry {
    pub(crate) node: NodeId,
    pub(crate) kind: Kind,
    pub(crate) restore_focus: Option<NodeId>,
    /// Transient guard while a popover's synchronous beforetoggle closing
    /// event runs. It lives on the existing bounded top-layer entry rather
    /// than in a second per-node registry.
    pub(crate) popover_transitioning: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PopoverVisibility {
    NotPopover,
    Hidden,
    Showing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateError {
    InvalidState,
    NotSupported,
    UnsupportedState,
    ResourceLimit,
    Dom(Error),
}

impl StateError {
    /// DOMException mapping for the binding adapter. `UnsupportedState` is intentionally distinct
    /// from a standards error: the existing stack has no storage for a requested advanced state.
    pub const fn exception_name(self) -> &'static str {
        match self {
            Self::InvalidState => "InvalidStateError",
            Self::NotSupported => "NotSupportedError",
            Self::UnsupportedState => "NotSupportedError",
            Self::ResourceLimit => "QuotaExceededError",
            Self::Dom(Error::LimitExceeded) => "QuotaExceededError",
            Self::Dom(Error::WrongKind | Error::UnsupportedDoctype) => "NotSupportedError",
            Self::Dom(Error::InvalidNode | Error::NotFound) => "NotFoundError",
            Self::Dom(Error::IndexSize) => "IndexSizeError",
            Self::Dom(Error::InUseAttribute) => "InUseAttributeError",
            Self::Dom(Error::Hierarchy) => "HierarchyRequestError",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PopoverTransition {
    pub changed: bool,
    pub closed_auto_popovers: usize,
    /// The exact peer entries removed by this transition, in top-layer order.
    /// Adapters use this bounded result to dispatch the corresponding events.
    pub closed_auto_popover_nodes: Vec<NodeId>,
    pub is_open: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DialogCloseTransition {
    pub changed: bool,
    pub was_modal: bool,
    pub restore_focus: Option<NodeId>,
}

/// The Auto popovers to hide for a matching trusted pointerup. `endpoint` is
/// the topmost open Auto popover containing the pointerup target, when there is
/// one. The empty list is meaningful: a matched outside click closes the full
/// Auto stack, while a mismatch also produces an empty list.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PointerUpLightDismissPlan {
    pub endpoint: Option<NodeId>,
    pub popovers_to_hide: Vec<NodeId>,
}

/// Parse the `popover` content attribute. Absence is the No Popover state, an empty value is
/// Auto, and an invalid value defaults to Manual.
pub fn popover_mode(document: &Document, node: NodeId) -> Option<PopoverMode> {
    let NodeKind::Element {
        namespace: Namespace::Html,
        ..
    } = document.kind(node).ok()?
    else {
        return None;
    };
    let value = document
        .get_attribute_ns_ref(node, None, "popover")
        .ok()??;
    Some(if value.is_empty() {
        PopoverMode::Auto
    } else if value.eq_ignore_ascii_case("manual") {
        PopoverMode::Manual
    } else if value.eq_ignore_ascii_case("hint") {
        PopoverMode::Hint
    } else if value.eq_ignore_ascii_case("auto") {
        PopoverMode::Auto
    } else {
        PopoverMode::Manual
    })
}

/// Canonical getter value for the limited-enumerated `HTMLElement.popover` reflection.
/// The No Popover state and a missing attribute have no keyword and return the empty string.
pub fn reflected_popover_value(document: &Document, node: NodeId) -> Option<&'static str> {
    popover_mode(document, node).map(PopoverMode::keyword)
}

pub fn dialog_is_open(document: &Document, node: NodeId) -> bool {
    is_html_element_named(document, node, "dialog")
        && document
            .get_attribute_ns_ref(node, None, "open")
            .ok()
            .flatten()
            .is_some()
}

/// Selector-facing `:open` subset. Select picker state is deliberately absent until a real
/// picker implementation can publish it.
pub fn matches_open(document: &Document, node: NodeId) -> bool {
    ((is_html_element_named(document, node, "dialog")
        || is_html_element_named(document, node, "details"))
        && document
            .get_attribute_ns_ref(node, None, "open")
            .ok()
            .flatten()
            .is_some())
        || matches_popover_open(document, node)
}

/// Current modal classification from the typed top-layer entry.
pub fn dialog_modal_state(document: &Document, node: NodeId) -> DialogModalState {
    if !is_html_element_named(document, node, "dialog") {
        return DialogModalState::NotDialog;
    }
    match document.top_layer_kind(node) {
        Some(Kind::ModalDialog) => DialogModalState::Modal,
        Some(Kind::Generic | Kind::Popover(_)) | None => DialogModalState::NonModal,
    }
}

pub fn matches_modal(document: &Document, node: NodeId) -> bool {
    dialog_modal_state(document, node) == DialogModalState::Modal
}

/// A popover is showing only when its typed top-layer entry is a popover state.
pub fn popover_visibility(document: &Document, node: NodeId) -> PopoverVisibility {
    if popover_mode(document, node).is_none() {
        return PopoverVisibility::NotPopover;
    }
    match document.top_layer_kind(node) {
        Some(Kind::Popover(_)) => PopoverVisibility::Showing,
        Some(Kind::ModalDialog | Kind::Generic) | None => PopoverVisibility::Hidden,
    }
}

pub fn matches_popover_open(document: &Document, node: NodeId) -> bool {
    popover_visibility(document, node) == PopoverVisibility::Showing
}

/// Apply the state portion of `dialog.show()` or `dialog.showModal()` after the caller's
/// `beforetoggle` phase has succeeded. Event dispatch and focus handling stay in the JS adapter.
pub fn show_dialog(
    document: &mut Document,
    node: NodeId,
    mode: DialogMode,
) -> Result<bool, StateError> {
    show_dialog_with_focus(document, node, mode, None)
}

/// Dialog state transition with a focus return target for modal entries.
pub fn show_dialog_with_focus(
    document: &mut Document,
    node: NodeId,
    mode: DialogMode,
    restore_focus: Option<NodeId>,
) -> Result<bool, StateError> {
    if !validate_show_dialog(document, node, mode)? {
        return Ok(false);
    }
    document
        .set_attribute_ns(node, None, "open", "")
        .map_err(StateError::Dom)?;
    if mode == DialogMode::Modal {
        if let Err(error) =
            document.push_top_layer_element_with_focus(node, Kind::ModalDialog, restore_focus)
        {
            let _ = document.remove_attribute_ns(node, None, "open");
            return Err(if error == Error::LimitExceeded {
                StateError::ResourceLimit
            } else {
                StateError::Dom(error)
            });
        }
    }
    Ok(true)
}

pub fn validate_show_dialog(
    document: &Document,
    node: NodeId,
    mode: DialogMode,
) -> Result<bool, StateError> {
    require_html_element(document, node, "dialog")?;
    let open = dialog_is_open(document, node);
    let modal_state = dialog_modal_state(document, node);

    if open {
        return match mode {
            DialogMode::NonModal if modal_state == DialogModalState::NonModal => Ok(false),
            DialogMode::Modal if modal_state == DialogModalState::Modal => Ok(false),
            _ => Err(StateError::InvalidState),
        };
    }

    if mode == DialogMode::Modal {
        if !is_connected(document, node)?
            || matches!(
                popover_visibility(document, node),
                PopoverVisibility::Showing
            )
        {
            return Err(StateError::InvalidState);
        }
    }
    Ok(true)
}

/// Apply the state portion of `dialog.close()` after `beforetoggle` has been dispatched. This
/// does not store or return `returnValue`, queue a `close` event, or restore focus.
pub fn close_dialog(document: &mut Document, node: NodeId) -> Result<bool, StateError> {
    close_dialog_with_state(document, node).map(|transition| transition.changed)
}

pub fn close_dialog_with_state(
    document: &mut Document,
    node: NodeId,
) -> Result<DialogCloseTransition, StateError> {
    require_html_element(document, node, "dialog")?;
    if !dialog_is_open(document, node) {
        return Ok(DialogCloseTransition {
            changed: false,
            was_modal: false,
            restore_focus: None,
        });
    }
    let state = dialog_modal_state(document, node);
    let mut restore_focus = None;
    match state {
        DialogModalState::Modal => {
            document
                .remove_attribute_ns(node, None, "open")
                .map_err(StateError::Dom)?;
            restore_focus = document
                .remove_top_layer_element_with_state(node)
                .and_then(|(_, focus)| focus);
        }
        DialogModalState::NonModal => {
            document
                .remove_attribute_ns(node, None, "open")
                .map_err(StateError::Dom)?;
        }
        DialogModalState::NotDialog => unreachable!(),
    }
    Ok(DialogCloseTransition {
        changed: true,
        was_modal: state == DialogModalState::Modal,
        restore_focus,
    })
}

/// Apply the state portion of `HTMLElement.showPopover()`. Auto popovers close other showing
/// Auto popovers outside the target's flat-tree ancestor chain. Hint stacks, source relationships,
/// focus restoration, and toggle events are left to the binding layer. Trusted-pointer
/// light-dismiss uses the separate pointerdown/pointerup state below.
pub fn show_popover(
    document: &mut Document,
    node: NodeId,
) -> Result<PopoverTransition, StateError> {
    show_popover_with_focus(document, node, None)
}

pub fn show_popover_with_focus(
    document: &mut Document,
    node: NodeId,
    restore_focus: Option<NodeId>,
) -> Result<PopoverTransition, StateError> {
    let mode = validate_show_popover(document, node)?;

    // Reserve and publish the new top-layer entry first. If allocation fails, no previously open
    // popover has been closed. There are no callbacks during this core commit, so moving the new
    // item above the old entries before removing Auto peers is unobservable.
    document
        .push_top_layer_element_with_focus(node, Kind::Popover(mode), restore_focus)
        .map_err(|error| {
            if error == Error::LimitExceeded {
                StateError::ResourceLimit
            } else {
                StateError::Dom(error)
            }
        })?;

    let closed_auto_popover_nodes = if mode == PopoverMode::Auto {
        close_unrelated_auto_popovers(document, node)?
    } else {
        Vec::new()
    };
    Ok(PopoverTransition {
        changed: true,
        closed_auto_popovers: closed_auto_popover_nodes.len(),
        closed_auto_popover_nodes,
        is_open: true,
    })
}

pub fn validate_show_popover(document: &Document, node: NodeId) -> Result<PopoverMode, StateError> {
    let mode = popover_mode(document, node).ok_or(StateError::NotSupported)?;
    if !is_connected(document, node)? {
        return Err(StateError::InvalidState);
    }
    if document.any_popover_transitioning() {
        return Err(StateError::InvalidState);
    }
    match popover_visibility(document, node) {
        PopoverVisibility::Showing => return Err(StateError::InvalidState),
        PopoverVisibility::NotPopover => return Err(StateError::NotSupported),
        PopoverVisibility::Hidden => {}
    }
    if mode == PopoverMode::Hint {
        return Err(StateError::UnsupportedState);
    }
    if dialog_modal_state(document, node) == DialogModalState::Modal {
        return Err(StateError::InvalidState);
    }
    Ok(mode)
}

pub fn hide_popover(document: &mut Document, node: NodeId) -> Result<bool, StateError> {
    hide_popover_with_state(document, node).map(|(changed, _)| changed)
}

pub fn validate_hide_popover(document: &Document, node: NodeId) -> Result<bool, StateError> {
    if popover_mode(document, node).is_none() {
        return Err(StateError::NotSupported);
    }
    if document.popover_transitioning(node) {
        return Err(StateError::InvalidState);
    }
    Ok(popover_visibility(document, node) == PopoverVisibility::Showing)
}

pub fn hide_popover_with_state(
    document: &mut Document,
    node: NodeId,
) -> Result<(bool, Option<NodeId>), StateError> {
    match validate_hide_popover(document, node)? {
        false => Ok((false, None)),
        true => Ok(document
            .remove_top_layer_element_with_state(node)
            .map_or((false, None), |(_, focus)| (true, focus))),
    }
}

pub fn toggle_popover(
    document: &mut Document,
    node: NodeId,
) -> Result<PopoverTransition, StateError> {
    match validate_toggle_popover(document, node)? {
        true => {
            let (changed, _) = hide_popover_with_state(document, node)?;
            Ok(PopoverTransition {
                changed,
                closed_auto_popovers: 0,
                closed_auto_popover_nodes: Vec::new(),
                is_open: false,
            })
        }
        false => show_popover(document, node),
    }
}

/// Validate `togglePopover()` without changing state, returning whether the
/// popover is currently showing. Unlike `showPopover()`, an already showing
/// popover is a valid no-op; connectivity and pending close transitions still
/// apply to both force values.
pub fn validate_toggle_popover(document: &Document, node: NodeId) -> Result<bool, StateError> {
    let mode = popover_mode(document, node).ok_or(StateError::NotSupported)?;
    if !is_connected(document, node)? || document.any_popover_transitioning() {
        return Err(StateError::InvalidState);
    }
    let showing = popover_visibility(document, node) == PopoverVisibility::Showing;
    if !showing && mode == PopoverMode::Hint {
        return Err(StateError::UnsupportedState);
    }
    Ok(showing)
}

pub fn auto_popovers_to_close(
    document: &Document,
    target: NodeId,
) -> Result<Vec<NodeId>, StateError> {
    let mut nodes = Vec::new();
    let count = document.top_layer_entries().count();
    nodes
        .try_reserve(count)
        .map_err(|_| StateError::ResourceLimit)?;
    for (node, kind, _) in document.top_layer_entries() {
        if should_close_unrelated_auto_popover_on_show(document, node, target, kind)? {
            nodes.push(node);
        }
    }
    nodes.reverse();
    Ok(nodes)
}

fn should_close_unrelated_auto_popover_on_show(
    document: &Document,
    popover: NodeId,
    target: NodeId,
    kind: Kind,
) -> Result<bool, StateError> {
    if popover == target
        || kind != Kind::Popover(PopoverMode::Auto)
        || popover_mode(document, popover) != Some(PopoverMode::Auto)
    {
        return Ok(false);
    }
    Ok(!is_flat_ancestor(document, popover, target)?)
}

/// Record the pointerdown endpoint used by the HTML light-dismiss algorithm.
/// The current implementation supports the Auto stack; Hint popovers and
/// invoker-associated target popovers are not represented by this core yet.
/// Like the standard algorithm, this leaves the previous value untouched when
/// there is no supported open Auto popover.
pub fn record_popover_pointerdown_target(
    document: &mut Document,
    target: NodeId,
) -> Result<(), StateError> {
    if !has_open_auto_popover(document) {
        return Ok(());
    }
    let endpoint = nearest_inclusive_open_auto_popover(document, target)?;
    document.set_popover_pointerdown_target(endpoint);
    Ok(())
}

/// Clear a pending pointerdown endpoint when the host abandons a pointer
/// sequence without dispatching a pointerup event.
pub fn clear_popover_pointerdown_target(document: &mut Document) {
    document.set_popover_pointerdown_target(None);
}

/// Finish one pointerup's light-dismiss comparison. A `None` result means the
/// standard's no-open-Auto-popover early return applied, so the recorded
/// pointerdown target is left untouched. Otherwise, the stored target is
/// cleared before returning a plan, including on mismatch.
pub fn popovers_to_hide_on_pointerup(
    document: &mut Document,
    target: NodeId,
) -> Result<Option<PointerUpLightDismissPlan>, StateError> {
    if !has_open_auto_popover(document) {
        return Ok(None);
    }

    let endpoint = nearest_inclusive_open_auto_popover(document, target)?;
    let same_target = endpoint == document.popover_pointerdown_target();
    document.set_popover_pointerdown_target(None);
    let popovers_to_hide = if same_target {
        auto_popovers_to_hide_until(document, endpoint)?
    } else {
        Vec::new()
    };
    Ok(Some(PointerUpLightDismissPlan {
        endpoint,
        popovers_to_hide,
    }))
}

/// Whether `popover` is still an Auto-stack entry that lies above `endpoint`.
/// Adapters call this after synchronous `beforetoggle` callbacks before each
/// removal, so stale snapshots cannot close a popover that is now an ancestor
/// of the endpoint or has already been hidden.
pub fn should_hide_auto_popover_until(
    document: &Document,
    popover: NodeId,
    endpoint: Option<NodeId>,
) -> Result<bool, StateError> {
    if document.top_layer_kind(popover) != Some(Kind::Popover(PopoverMode::Auto)) {
        return Ok(false);
    }
    let mut popover_index = None;
    let mut endpoint_index = None;
    let mut index = 0usize;
    for (node, kind, _) in document.top_layer_entries() {
        if kind != Kind::Popover(PopoverMode::Auto) {
            continue;
        }
        if node == popover {
            popover_index = Some(index);
        }
        if Some(node) == endpoint {
            endpoint_index = Some(index);
        }
        index += 1;
    }
    let Some(popover_index) = popover_index else {
        return Ok(false);
    };
    Ok(endpoint_index.is_none_or(|endpoint_index| popover_index > endpoint_index))
}

fn has_open_auto_popover(document: &Document) -> bool {
    document
        .top_layer_entries()
        .any(|(_, kind, _)| kind == Kind::Popover(PopoverMode::Auto))
}

fn nearest_inclusive_open_auto_popover(
    document: &Document,
    node: NodeId,
) -> Result<Option<NodeId>, StateError> {
    let mut current = node;
    for _ in 0..MAX_ANCESTOR_STEPS {
        if document.top_layer_kind(current) == Some(Kind::Popover(PopoverMode::Auto)) {
            return Ok(Some(current));
        }
        let Some(parent) = document.composed_parent(current).map_err(StateError::Dom)? else {
            return Ok(None);
        };
        current = parent;
    }
    Err(StateError::ResourceLimit)
}

fn auto_popovers_to_hide_until(
    document: &Document,
    endpoint: Option<NodeId>,
) -> Result<Vec<NodeId>, StateError> {
    let endpoint_index = endpoint.and_then(|endpoint| {
        document
            .top_layer_entries()
            .filter(|(_, kind, _)| *kind == Kind::Popover(PopoverMode::Auto))
            .position(|(node, _, _)| node == endpoint)
    });
    // The standard keeps the endpoint itself: a stack position is one-based,
    // and the hide slice begins at endpoint-index + 1. If the endpoint is not
    // in the current Auto list, the index defaults to zero and the whole list
    // is hidden.
    let first_to_hide = endpoint_index.map_or(0, |index| index + 1);
    let mut nodes = Vec::new();
    let count = document
        .top_layer_entries()
        .filter(|(_, kind, _)| *kind == Kind::Popover(PopoverMode::Auto))
        .count();
    nodes
        .try_reserve(count.saturating_sub(first_to_hide))
        .map_err(|_| StateError::ResourceLimit)?;
    for (index, (node, _, _)) in document
        .top_layer_entries()
        .filter(|(_, kind, _)| *kind == Kind::Popover(PopoverMode::Auto))
        .enumerate()
    {
        if index >= first_to_hide {
            nodes.push(node);
        }
    }
    nodes.reverse();
    Ok(nodes)
}

/// Return every currently showing popover that must be removed before a modal
/// dialog enters the top layer. The adapter dispatches script-visible close
/// events before committing these state changes.
pub fn all_popovers_to_close(
    document: &Document,
    except: NodeId,
) -> Result<Vec<NodeId>, StateError> {
    let mut nodes = Vec::new();
    let count = document.top_layer_entries().count();
    nodes
        .try_reserve(count)
        .map_err(|_| StateError::ResourceLimit)?;
    for (node, kind, _) in document.top_layer_entries() {
        if node != except && matches!(kind, Kind::Popover(_)) {
            nodes.push(node);
        }
    }
    nodes.reverse();
    Ok(nodes)
}

fn close_unrelated_auto_popovers(
    document: &mut Document,
    target: NodeId,
) -> Result<Vec<NodeId>, StateError> {
    let nodes = auto_popovers_to_close(document, target)?;
    for node in &nodes {
        document.remove_top_layer_element(*node);
    }
    Ok(nodes)
}

fn is_html_element_named(document: &Document, node: NodeId, expected: &str) -> bool {
    matches!(
        document.kind(node),
        Ok(NodeKind::Element {
            namespace: Namespace::Html,
            name,
            ..
        }) if name.as_str() == expected
    )
}

fn require_html_element(
    document: &Document,
    node: NodeId,
    expected: &str,
) -> Result<(), StateError> {
    let kind = document.kind(node).map_err(StateError::Dom)?;
    if matches!(kind, NodeKind::Element { namespace: Namespace::Html, name, .. } if name.as_str() == expected)
    {
        Ok(())
    } else {
        Err(StateError::NotSupported)
    }
}

fn is_connected(document: &Document, node: NodeId) -> Result<bool, StateError> {
    if !matches!(document.kind(node), Ok(NodeKind::Element { .. })) {
        return Ok(false);
    }
    let root = document.root();
    let mut current = node;
    for _ in 0..MAX_ANCESTOR_STEPS {
        if current == root {
            return Ok(true);
        }
        let Some(parent) = document
            .shadow_including_parent(current)
            .map_err(StateError::Dom)?
        else {
            return Ok(false);
        };
        current = parent;
    }
    Err(StateError::ResourceLimit)
}

fn is_flat_ancestor(
    document: &Document,
    ancestor: NodeId,
    node: NodeId,
) -> Result<bool, StateError> {
    let mut current = node;
    for _ in 0..MAX_ANCESTOR_STEPS {
        let Some(parent) = document.composed_parent(current).map_err(StateError::Dom)? else {
            return Ok(false);
        };
        if parent == ancestor {
            return Ok(true);
        }
        current = parent;
    }
    Err(StateError::ResourceLimit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn element(name: &str, attrs: &[(&str, &str)]) -> NodeKind {
        NodeKind::Element {
            namespace: Namespace::Html,
            name: name.into(),
            attributes: attrs
                .iter()
                .map(|(name, value)| ((*name).into(), (*value).into()))
                .collect::<Vec<_>>(),
        }
    }

    fn append(document: &mut Document, parent: NodeId, child: NodeId) {
        document.append(parent, child).unwrap();
    }

    #[test]
    fn dialog_open_and_modal_state_are_separate_from_generic_popovers() {
        let mut document = Document::new(16);
        let host = document.create(element("main", &[])).unwrap();
        let plain = document.create(element("dialog", &[])).unwrap();
        let modal = document.create(element("dialog", &[])).unwrap();
        let document_root = document.root();
        append(&mut document, document_root, host);
        append(&mut document, host, plain);
        append(&mut document, host, modal);

        assert!(show_dialog(&mut document, plain, DialogMode::NonModal).unwrap());
        assert!(matches_open(&document, plain));
        assert_eq!(
            dialog_modal_state(&document, plain),
            DialogModalState::NonModal
        );
        assert!(!show_dialog(&mut document, plain, DialogMode::NonModal).unwrap());
        assert!(close_dialog(&mut document, plain).unwrap());
        assert!(!matches_open(&document, plain));

        assert!(show_dialog(&mut document, modal, DialogMode::Modal).unwrap());
        assert!(matches_open(&document, modal));
        assert!(matches_modal(&document, modal));
        assert!(document.is_top_layer_element(modal));
        assert!(close_dialog(&mut document, modal).unwrap());
        assert!(!document.is_top_layer_element(modal));
    }

    #[test]
    fn selector_states_follow_open_attribute_and_typed_top_layer_membership() {
        let mut document = Document::new(16);
        let host = document.create(element("main", &[])).unwrap();
        let dialog = document.create(element("dialog", &[])).unwrap();
        let popover = document
            .create(element("div", &[("popover", "auto")]))
            .unwrap();
        let root = document.root();
        append(&mut document, root, host);
        append(&mut document, host, dialog);
        append(&mut document, host, popover);
        let open = crate::css::parse_selector(":open", 0).unwrap();
        let modal = crate::css::parse_selector(":modal", 0).unwrap();
        let popover_open = crate::css::parse_selector(":popover-open", 0).unwrap();

        assert!(!open.matches_node_in_scope(&document, dialog, None));
        show_dialog(&mut document, dialog, DialogMode::NonModal).unwrap();
        assert!(open.matches_node_in_scope(&document, dialog, None));
        assert!(!modal.matches_node_in_scope(&document, dialog, None));
        close_dialog(&mut document, dialog).unwrap();
        show_dialog_with_focus(&mut document, dialog, DialogMode::Modal, Some(host)).unwrap();
        assert!(modal.matches_node_in_scope(&document, dialog, None));
        assert_eq!(document.top_layer_restore_focus(dialog), Some(host));

        show_popover(&mut document, popover).unwrap();
        assert!(open.matches_node_in_scope(&document, popover, None));
        assert!(popover_open.matches_node_in_scope(&document, popover, None));
        assert!(!modal.matches_node_in_scope(&document, popover, None));
        hide_popover(&mut document, popover).unwrap();
        assert!(!popover_open.matches_node_in_scope(&document, popover, None));
    }

    #[test]
    fn auto_popovers_close_unrelated_peers_and_preserve_flat_ancestors() {
        let mut document = Document::new(24);
        let root = document.create(element("main", &[])).unwrap();
        let parent = document
            .create(element("div", &[("popover", "auto")]))
            .unwrap();
        let child = document
            .create(element("div", &[("popover", "auto")]))
            .unwrap();
        let sibling = document
            .create(element("div", &[("popover", "auto")]))
            .unwrap();
        let manual = document
            .create(element("div", &[("popover", "manual")]))
            .unwrap();
        let document_root = document.root();
        append(&mut document, document_root, root);
        append(&mut document, root, parent);
        append(&mut document, parent, child);
        append(&mut document, root, sibling);
        append(&mut document, root, manual);

        show_popover(&mut document, parent).unwrap();
        show_popover(&mut document, sibling).unwrap();
        assert!(!matches_popover_open(&document, parent));
        assert!(matches_popover_open(&document, sibling));

        show_popover(&mut document, parent).unwrap();
        show_popover(&mut document, manual).unwrap();
        let result = show_popover(&mut document, child).unwrap();
        assert_eq!(result.closed_auto_popovers, 0);
        assert!(matches_popover_open(&document, parent));
        assert!(matches_popover_open(&document, child));
        assert!(matches_popover_open(&document, manual));

        assert!(hide_popover(&mut document, child).unwrap());
        assert!(!matches_popover_open(&document, child));
    }

    #[test]
    fn auto_popover_light_dismiss_requires_matching_pointerdown_and_pointerup() {
        let mut document = Document::new(24);
        let root = document.create(element("main", &[])).unwrap();
        let outer = document
            .create(element("div", &[("popover", "auto")]))
            .unwrap();
        let inner = document
            .create(element("div", &[("popover", "auto")]))
            .unwrap();
        let outside = document.create(element("button", &[])).unwrap();
        let document_root = document.root();
        append(&mut document, document_root, root);
        append(&mut document, root, outer);
        append(&mut document, outer, inner);
        append(&mut document, root, outside);
        show_popover(&mut document, outer).unwrap();
        show_popover(&mut document, inner).unwrap();

        record_popover_pointerdown_target(&mut document, inner).unwrap();
        let drag_out = popovers_to_hide_on_pointerup(&mut document, outside)
            .unwrap()
            .unwrap();
        assert_eq!(drag_out.endpoint, None);
        assert!(drag_out.popovers_to_hide.is_empty());
        assert_eq!(document.popover_pointerdown_target(), None);
        assert!(matches_popover_open(&document, outer));
        assert!(matches_popover_open(&document, inner));

        // A click on the outer popover closes only the later nested Auto item.
        record_popover_pointerdown_target(&mut document, outer).unwrap();
        let inside = popovers_to_hide_on_pointerup(&mut document, outer)
            .unwrap()
            .unwrap();
        assert_eq!(inside.endpoint, Some(outer));
        assert_eq!(inside.popovers_to_hide, [inner]);
        for popover in inside.popovers_to_hide {
            hide_popover(&mut document, popover).unwrap();
        }
        assert!(matches_popover_open(&document, outer));
        assert!(!matches_popover_open(&document, inner));

        // A matching outside click has a null endpoint and closes the stack.
        record_popover_pointerdown_target(&mut document, outside).unwrap();
        let outside_click = popovers_to_hide_on_pointerup(&mut document, outside)
            .unwrap()
            .unwrap();
        assert_eq!(outside_click.endpoint, None);
        assert_eq!(outside_click.popovers_to_hide, [outer]);
    }

    #[test]
    fn no_popover_and_invalid_receivers_have_distinct_state_errors() {
        let mut document = Document::new(8);
        let root = document.create(element("main", &[])).unwrap();
        let div = document.create(element("div", &[])).unwrap();
        let document_root = document.root();
        append(&mut document, document_root, root);
        append(&mut document, root, div);
        assert_eq!(
            show_popover(&mut document, div),
            Err(StateError::NotSupported)
        );
        assert_eq!(
            show_dialog(&mut document, div, DialogMode::Modal),
            Err(StateError::NotSupported)
        );
    }

    #[test]
    fn closing_beforetoggle_blocks_reentrant_popover_show_until_transition_finishes() {
        let mut document = Document::new(16);
        let root = document.create(element("main", &[])).unwrap();
        let outer = document
            .create(element("div", &[("popover", "auto")]))
            .unwrap();
        let nested = document
            .create(element("div", &[("popover", "auto")]))
            .unwrap();
        let hint = document
            .create(element("div", &[("popover", "hint")]))
            .unwrap();
        let document_root = document.root();
        append(&mut document, document_root, root);
        append(&mut document, root, outer);
        append(&mut document, outer, nested);
        append(&mut document, root, hint);

        show_popover(&mut document, outer).unwrap();
        assert!(document.begin_popover_transition(outer));
        assert_eq!(
            validate_show_popover(&document, nested),
            Err(StateError::InvalidState)
        );
        // A closing popover's beforetoggle guard takes precedence over a
        // separate unsupported popover mode reached by reentrant script.
        assert_eq!(
            validate_show_popover(&document, hint),
            Err(StateError::InvalidState)
        );
        assert_eq!(
            hide_popover_with_state(&mut document, outer),
            Err(StateError::InvalidState)
        );

        document.end_popover_transition(outer);
        show_popover(&mut document, nested).unwrap();
        assert!(matches_popover_open(&document, nested));
    }

    #[test]
    fn toggle_validation_checks_receiver_before_forced_noop() {
        let mut document = Document::new(8);
        let root = document.create(element("main", &[])).unwrap();
        let popover = document
            .create(element("div", &[("popover", "manual")]))
            .unwrap();
        let disconnected = document
            .create(element("div", &[("popover", "manual")]))
            .unwrap();
        let ordinary = document.create(element("div", &[])).unwrap();
        let document_root = document.root();
        append(&mut document, document_root, root);
        append(&mut document, root, popover);
        append(&mut document, root, ordinary);

        assert_eq!(
            validate_toggle_popover(&document, ordinary),
            Err(StateError::NotSupported)
        );
        assert_eq!(
            validate_toggle_popover(&document, disconnected),
            Err(StateError::InvalidState)
        );
        assert_eq!(validate_toggle_popover(&document, popover), Ok(false));
        show_popover(&mut document, popover).unwrap();
        assert_eq!(validate_toggle_popover(&document, popover), Ok(true));
    }

    #[test]
    fn popover_enumerated_defaults_and_reflection_are_distinct() {
        let mut document = Document::new(16);
        let root = document.create(element("main", &[])).unwrap();
        let absent = document.create(element("div", &[])).unwrap();
        let empty = document.create(element("div", &[("popover", "")])).unwrap();
        let invalid = document
            .create(element("div", &[("popover", "unknown")]))
            .unwrap();
        let hint = document
            .create(element("div", &[("popover", "HINT")]))
            .unwrap();
        let document_root = document.root();
        append(&mut document, document_root, root);
        for node in [absent, empty, invalid, hint] {
            append(&mut document, root, node);
        }

        assert_eq!(popover_mode(&document, absent), None);
        assert_eq!(reflected_popover_value(&document, absent), None);
        assert_eq!(popover_mode(&document, empty), Some(PopoverMode::Auto));
        assert_eq!(reflected_popover_value(&document, empty), Some("auto"));
        assert_eq!(popover_mode(&document, invalid), Some(PopoverMode::Manual));
        assert_eq!(reflected_popover_value(&document, invalid), Some("manual"));
        assert_eq!(popover_mode(&document, hint), Some(PopoverMode::Hint));
        assert_eq!(reflected_popover_value(&document, hint), Some("hint"));
    }
}
