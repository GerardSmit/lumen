//! Shared HTML directionality algorithms used by selectors and form
//! submission. Text classification uses the pinned Unicode bidi data in
//! `lumen-common`; all tree walks are bounded and allocation-free.

use crate::{Document, NodeId, NodeKind};
use lumen_common::bidi;

pub use lumen_common::bidi::Directionality as Direction;

// Ordinary ancestry and text scans are iterative. Recursion is needed only
// when slot directionality follows assigned nodes into another slot.
const MAX_SLOT_DIRECTIONALITY_DEPTH: usize = 128;

struct DirectionalityBudget {
    visits_left: usize,
    slot_depth: usize,
    exhausted: bool,
}

impl DirectionalityBudget {
    fn new(document: &Document) -> Self {
        Self {
            visits_left: document.node_count().saturating_mul(8).saturating_add(64),
            slot_depth: 0,
            exhausted: false,
        }
    }

    fn visit(&mut self) -> bool {
        if self.visits_left == 0 {
            self.exhausted = true;
            return false;
        }
        self.visits_left -= 1;
        true
    }

    fn enter_slot(&mut self) -> bool {
        if self.slot_depth >= MAX_SLOT_DIRECTIONALITY_DEPTH {
            self.exhausted = true;
            return false;
        }
        self.slot_depth += 1;
        true
    }

    fn leave_slot(&mut self) {
        self.slot_depth = self.slot_depth.saturating_sub(1);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirAttributeState {
    Ltr,
    Rtl,
    Auto,
}

impl DirAttributeState {
    pub const fn keyword(self) -> &'static str {
        match self {
            Self::Ltr => "ltr",
            Self::Rtl => "rtl",
            Self::Auto => "auto",
        }
    }
}

/// Classify the first strong character using Unicode bidi classes L, R, and AL.
pub fn first_strong_direction(text: &str) -> Option<Direction> {
    bidi::first_strong_direction(text)
}

/// Whether a control's value participates in HTML's auto-directionality
/// algorithm and may submit a `dirname` entry.
pub fn is_auto_directionality_form_associated(document: &Document, node: NodeId) -> bool {
    match crate::forms::html_element_local_name(document, node) {
        Some("textarea") => true,
        Some("input") => matches!(
            crate::forms::input_type_state(document, node),
            "hidden"
                | "text"
                | "search"
                | "tel"
                | "url"
                | "email"
                | "password"
                | "submit"
                | "reset"
                | "button"
        ),
        _ => false,
    }
}

/// Read the null-namespace `dir` attribute state of an HTML element.
/// Invalid or absent values have the Undefined state and return `None`.
pub fn dir_attribute_state(document: &Document, node: NodeId) -> Option<DirAttributeState> {
    crate::forms::html_element_local_name(document, node)?;
    let value = document
        .get_attribute_ns_ref(node, None, "dir")
        .ok()
        .flatten()?;
    if value.eq_ignore_ascii_case("ltr") {
        Some(DirAttributeState::Ltr)
    } else if value.eq_ignore_ascii_case("rtl") {
        Some(DirAttributeState::Rtl)
    } else if value.eq_ignore_ascii_case("auto") {
        Some(DirAttributeState::Auto)
    } else {
        None
    }
}

/// Compute the directionality used by CSS matching, consulting the sparse
/// live-value snapshot installed by a host when one is available.
pub fn resolved_element_directionality(document: &Document, node: NodeId) -> Direction {
    let mut budget = DirectionalityBudget::new(document);
    resolved_element_directionality_with_budget(document, node, &mut budget)
}

fn resolved_element_directionality_with_budget(
    document: &Document,
    node: NodeId,
    budget: &mut DirectionalityBudget,
) -> Direction {
    let live_value_direction = document
        .form_selector_state(node)
        .and_then(|state| state.auto_value_directionality);
    element_directionality_with_budget(document, node, live_value_direction, budget)
}

/// Compute element directionality with a borrowed control-value direction.
/// The value is used only for an auto-directionality input or textarea; other
/// states follow their explicit attribute or DOM parent/shadow-host direction.
pub fn element_directionality_with_value_direction(
    document: &Document,
    node: NodeId,
    live_value_direction: Option<Direction>,
) -> Direction {
    let mut budget = DirectionalityBudget::new(document);
    element_directionality_with_budget(document, node, live_value_direction, &mut budget)
}

fn element_directionality_with_budget(
    document: &Document,
    node: NodeId,
    live_value_direction: Option<Direction>,
    budget: &mut DirectionalityBudget,
) -> Direction {
    let limit = document.node_count();
    let mut current = Some(node);
    for _ in 0..limit {
        if !budget.visit() {
            return Direction::Ltr;
        }
        let Some(element) = current else {
            return Direction::Ltr;
        };
        let html_local = crate::forms::html_element_local_name(document, element);
        if let Some(local) = html_local {
            match dir_attribute_state(document, element) {
                Some(DirAttributeState::Ltr) => return Direction::Ltr,
                Some(DirAttributeState::Rtl) => return Direction::Rtl,
                Some(DirAttributeState::Auto) => {
                    return auto_directionality(
                        document,
                        element,
                        (element == node).then_some(live_value_direction).flatten(),
                        budget,
                    )
                    .unwrap_or(Direction::Ltr);
                }
                None if local == "bdi" => {
                    return auto_directionality(
                        document,
                        element,
                        (element == node).then_some(live_value_direction).flatten(),
                        budget,
                    )
                    .unwrap_or(Direction::Ltr);
                }
                None if local == "input"
                    && crate::forms::input_type_state(document, element) == "tel" =>
                {
                    return Direction::Ltr;
                }
                None => {}
            }
        }
        current = parent_directionality_element(document, element, budget);
    }
    Direction::Ltr
}

/// For an empty auto-directionality value the HTML algorithm returns null;
/// dirname serializes the element's resulting direction, whose fallback is
/// LTR. This helper is also used for live `dir=auto` input matching.
pub fn control_value_direction(value: &str) -> Direction {
    first_strong_direction(value).unwrap_or(Direction::Ltr)
}

/// Whether a live form value can change the computed CSS `direction` even
/// when the stylesheet has no state-dependent selector. This structural scan
/// is allocation-free and bounded by the document's node count; a session can
/// cache its result against the DOM version.
pub(crate) fn has_auto_directionality_controls(document: &Document) -> bool {
    let root = document.root();
    let limit = document.node_count();
    let mut current = document.first_child(root).ok().flatten();
    let mut visited = 0usize;
    while let Some(node) = current {
        if visited >= limit {
            return false;
        }
        visited += 1;
        if is_auto_directionality_form_associated(document, node)
            && dir_attribute_state(document, node) == Some(DirAttributeState::Auto)
        {
            return true;
        }
        current = crate::selector::next_shadow_including_descendant(document, root, node)
            .ok()
            .flatten();
    }
    false
}

fn parent_directionality_element(
    document: &Document,
    node: NodeId,
    budget: &mut DirectionalityBudget,
) -> Option<NodeId> {
    // Parent directionality follows the DOM parent chain, not the flattened
    // slot-assignment parent. A shadow root is the one special boundary: its
    // host supplies the parent directionality.
    let mut current = document.parent(node).ok().flatten();
    let limit = document.node_count();
    for _ in 0..limit {
        if !budget.visit() {
            return None;
        }
        let parent = current?;
        if matches!(document.kind(parent), Ok(NodeKind::Element { .. })) {
            return Some(parent);
        }
        current = document
            .shadow_host(parent)
            .ok()
            .flatten()
            .or_else(|| document.parent(parent).ok().flatten());
    }
    None
}

fn auto_directionality(
    document: &Document,
    element: NodeId,
    supplied_value_direction: Option<Direction>,
    budget: &mut DirectionalityBudget,
) -> Option<Direction> {
    if is_auto_directionality_form_associated(document, element) {
        if let Some(direction) = supplied_value_direction.or_else(|| {
            document
                .form_selector_state(element)
                .and_then(|state| state.auto_value_directionality)
        }) {
            return Some(direction);
        }
        if crate::forms::html_element_local_name(document, element) == Some("input") {
            let value = document
                .get_attribute_ns_ref(element, None, "value")
                .ok()
                .flatten()
                .unwrap_or("");
            return first_strong_direction(value);
        }
    }
    contained_text_auto_directionality(document, element, false, budget)
}

fn contained_text_auto_directionality(
    document: &Document,
    root: NodeId,
    can_exclude_root: bool,
    budget: &mut DirectionalityBudget,
) -> Option<Direction> {
    if can_exclude_root && excludes_contained_text_root(document, root) {
        return None;
    }
    if crate::forms::html_element_local_name(document, root) == Some("slot") {
        if let Some(assigned_direction) = assigned_slot_directionality(document, root, budget) {
            return assigned_direction;
        }
    }

    let limit = document.node_count();
    let mut current = document.first_child(root).ok().flatten();
    let mut visited = 0usize;
    while let Some(node) = current {
        if visited >= limit || !budget.visit() {
            return None;
        }
        visited += 1;
        match document.kind(node).ok()? {
            NodeKind::Text(text) | NodeKind::CData(text) => {
                if let Some(direction) = first_strong_direction(text) {
                    return Some(direction);
                }
            }
            NodeKind::Element { .. } => {
                let local = crate::forms::html_element_local_name(document, node);
                if local.is_some_and(|name| matches!(name, "bdi" | "script" | "style" | "textarea"))
                    || dir_attribute_state(document, node).is_some()
                {
                    current = next_after_subtree(document, root, node, budget);
                    continue;
                }
                if local == Some("slot") {
                    if let Some(host) = shadow_slot_host(document, node) {
                        // A descendant active slot contributes its shadow
                        // host's directionality here. Assigned slottables are
                        // consulted only when the slot itself is being
                        // resolved with `dir=auto`.
                        if !budget.enter_slot() {
                            return None;
                        }
                        let direction =
                            resolved_element_directionality_with_budget(document, host, budget);
                        budget.leave_slot();
                        return Some(direction);
                    }
                }
            }
            _ => {}
        }
        current = crate::selector::next_descendant(document, root, node)
            .ok()
            .flatten();
    }
    None
}

/// HTML's slot branch checks assigned slottables before fallback descendants.
/// Walk the host's live child list and consult the canonical assignment query,
/// avoiding an allocated `assigned_nodes()` snapshot in selector matching.
fn assigned_slot_directionality(
    document: &Document,
    slot: NodeId,
    budget: &mut DirectionalityBudget,
) -> Option<Option<Direction>> {
    let host = shadow_slot_host(document, slot)?;
    if !budget.enter_slot() {
        return Some(None);
    }
    let result = assigned_slot_directionality_inner(document, slot, host, budget);
    budget.leave_slot();
    if budget.exhausted {
        Some(None)
    } else {
        result
    }
}

fn assigned_slot_directionality_inner(
    document: &Document,
    slot: NodeId,
    host: NodeId,
    budget: &mut DirectionalityBudget,
) -> Option<Option<Direction>> {
    let mut current = document.first_child(host).ok().flatten();
    let limit = document.node_count();
    let mut visited = 0usize;
    let mut assigned = false;
    while let Some(node) = current {
        if visited >= limit || !budget.visit() {
            return Some(None);
        }
        visited += 1;
        if document.assigned_slot(node).ok().flatten() == Some(slot)
            && matches!(
                document.kind(node),
                Ok(NodeKind::Text(_) | NodeKind::CData(_) | NodeKind::Element { .. })
            )
        {
            assigned = true;
            match document.kind(node).ok()? {
                NodeKind::Text(text) | NodeKind::CData(text) => {
                    if let Some(direction) = first_strong_direction(text) {
                        return Some(Some(direction));
                    }
                }
                NodeKind::Element { .. } => {
                    if let Some(direction) =
                        contained_text_auto_directionality(document, node, true, budget)
                    {
                        return Some(Some(direction));
                    }
                }
                _ => {}
            }
        }
        current = document.next_sibling(node).ok().flatten();
    }
    assigned.then_some(None)
}

fn excludes_contained_text_root(document: &Document, root: NodeId) -> bool {
    let local = crate::forms::html_element_local_name(document, root);
    local.is_some_and(|name| matches!(name, "bdi" | "script" | "style" | "textarea"))
        || dir_attribute_state(document, root).is_some()
}

fn shadow_slot_host(document: &Document, slot: NodeId) -> Option<NodeId> {
    if !document.is_slot(slot).ok()? {
        return None;
    }
    let root = document.root_node(slot, false).ok()?;
    document.shadow_host(root).ok().flatten()
}

fn next_after_subtree(
    document: &Document,
    root: NodeId,
    mut node: NodeId,
    budget: &mut DirectionalityBudget,
) -> Option<NodeId> {
    let limit = document.node_count();
    for _ in 0..limit {
        if !budget.visit() {
            return None;
        }
        if let Some(sibling) = document.next_sibling(node).ok().flatten() {
            return Some(sibling);
        }
        let parent = document.parent(node).ok().flatten()?;
        if parent == root {
            return None;
        }
        node = parent;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html_element(name: &str) -> NodeKind {
        NodeKind::Element {
            namespace: crate::Namespace::Html,
            name: crate::Name::new(name),
            attributes: alloc::vec::Vec::new(),
        }
    }

    #[test]
    fn html_directionality_handles_inheritance_auto_bdi_tel_and_foreign_elements() {
        let document = crate::html::parse(
            concat!(
                "<body dir='RTL'>",
                "<div id='inherited'><span id='child'></span></div>",
                "<div id='explicit' dir='ltr'></div>",
                "<bdi id='bdi'> 123 مرحبا</bdi>",
                "<bdi id='empty-bdi'></bdi>",
                "<div id='auto' dir='auto'><span dir='ltr'>Latin</span> مرحبا</div>",
                "<div id='auto-empty' dir='auto'> 123 !</div>",
                "<input id='tel' type='tel'>",
                "<input id='auto-input' dir='auto' value='עברית' type='text'>",
                "<svg id='foreign'></svg>",
                "</body>"
            ),
            64,
        )
        .unwrap();
        let direction = |id: &str| {
            let node = crate::selector::get_element_by_id(&document, document.root(), id)
                .unwrap()
                .unwrap();
            resolved_element_directionality(&document, node)
        };

        assert_eq!(direction("inherited"), Direction::Rtl);
        assert_eq!(direction("child"), Direction::Rtl);
        assert_eq!(direction("explicit"), Direction::Ltr);
        assert_eq!(direction("bdi"), Direction::Rtl);
        assert_eq!(direction("empty-bdi"), Direction::Ltr);
        assert_eq!(direction("auto"), Direction::Rtl);
        assert_eq!(direction("auto-empty"), Direction::Ltr);
        assert_eq!(direction("tel"), Direction::Ltr);
        assert_eq!(direction("auto-input"), Direction::Rtl);
        assert_eq!(direction("foreign"), Direction::Rtl);
    }

    #[test]
    fn assigned_slot_directionality_uses_slottables_without_a_snapshot() {
        let mut document = crate::html::parse(
            concat!(
                "<div id='host' dir='ltr'> 123 ",
                "<span id='excluded' slot='rtl' dir='ltr'>مرحبا</span>",
                "<span id='neutral' slot='rtl'>123 !</span>",
                "<span id='empty' slot='rtl'></span>",
                "<span id='assigned' slot='rtl'>עברית</span>",
                "</div>"
            ),
            32,
        )
        .unwrap();
        let host = crate::selector::get_element_by_id(&document, document.root(), "host")
            .unwrap()
            .unwrap();
        let assigned = crate::selector::get_element_by_id(&document, document.root(), "assigned")
            .unwrap()
            .unwrap();
        let excluded = crate::selector::get_element_by_id(&document, document.root(), "excluded")
            .unwrap()
            .unwrap();
        let neutral = crate::selector::get_element_by_id(&document, document.root(), "neutral")
            .unwrap()
            .unwrap();
        let empty = crate::selector::get_element_by_id(&document, document.root(), "empty")
            .unwrap()
            .unwrap();
        let shadow = document
            .attach_shadow(host, crate::ShadowMode::Open)
            .unwrap();
        let auto = document.create(html_element("div")).unwrap();
        document.set_attribute(auto, "dir", "auto").unwrap();
        let slot = document.create(html_element("slot")).unwrap();
        let auto_slot = document.create(html_element("slot")).unwrap();
        document.set_attribute(auto_slot, "name", "rtl").unwrap();
        document.set_attribute(auto_slot, "dir", "auto").unwrap();
        document.append(shadow, auto).unwrap();
        document.append(auto, slot).unwrap();
        document.append(shadow, auto_slot).unwrap();

        // A second shadow boundary exercises the same slot path with a nested
        // host, without flattening either tree into an allocated node list.
        let nested_host = document.create(html_element("div")).unwrap();
        let nested_assigned = document.create(html_element("span")).unwrap();
        let nested_text = document
            .create(NodeKind::Text(alloc::string::String::from("مرحبا")))
            .unwrap();
        document
            .set_attribute(nested_assigned, "slot", "inner")
            .unwrap();
        document.append(nested_assigned, nested_text).unwrap();
        document.append(nested_host, nested_assigned).unwrap();
        document.append(shadow, nested_host).unwrap();
        let nested_shadow = document
            .attach_shadow(nested_host, crate::ShadowMode::Open)
            .unwrap();
        let nested_slot = document.create(html_element("slot")).unwrap();
        document
            .set_attribute(nested_slot, "name", "inner")
            .unwrap();
        document.set_attribute(nested_slot, "dir", "auto").unwrap();
        document.append(nested_shadow, nested_slot).unwrap();

        assert_eq!(
            resolved_element_directionality(&document, auto),
            Direction::Ltr
        );
        // Slot assignment scans host child order. The explicit-direction root
        // is excluded, and neutral/empty roots do not prevent the later strong
        // assigned value from determining the slot's direction.
        assert_eq!(
            resolved_element_directionality(&document, auto_slot),
            Direction::Rtl
        );
        assert_eq!(
            resolved_element_directionality(&document, assigned),
            Direction::Ltr
        );
        assert_eq!(document.assigned_slot(assigned), Ok(Some(auto_slot)));
        assert_eq!(document.assigned_slot(excluded), Ok(Some(auto_slot)));
        assert_eq!(document.assigned_slot(neutral), Ok(Some(auto_slot)));
        assert_eq!(document.assigned_slot(empty), Ok(Some(auto_slot)));
        assert_eq!(
            resolved_element_directionality(&document, nested_slot),
            Direction::Rtl
        );
    }

    #[test]
    fn deep_directionality_walk_stays_iterative_and_slot_depth_is_bounded() {
        let mut document = crate::html::parse("<div id='root' dir='auto'></div>", 1024).unwrap();
        let root = crate::selector::get_element_by_id(&document, document.root(), "root")
            .unwrap()
            .unwrap();
        let mut parent = root;
        for _ in 0..512 {
            let child = document.create(html_element("div")).unwrap();
            document.append(parent, child).unwrap();
            parent = child;
        }
        let text = document
            .create(NodeKind::Text(alloc::string::String::from("עברית")))
            .unwrap();
        document.append(parent, text).unwrap();
        assert_eq!(
            resolved_element_directionality(&document, root),
            Direction::Rtl
        );

        let mut budget = DirectionalityBudget::new(&document);
        budget.slot_depth = MAX_SLOT_DIRECTIONALITY_DEPTH;
        assert!(!budget.enter_slot());
        assert!(budget.exhausted);
    }
}
