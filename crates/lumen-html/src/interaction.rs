//! Sparse browser interaction state used by shared selector matching.
//!
//! The host publishes only the current focus, hover and active anchors. The
//! renderer and DOM query APIs then share one matcher without storing state on
//! every element or copying the host's event/listener data.

use crate::{Document, NodeId};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InteractionState {
    pub focused: Option<NodeId>,
    pub focus_visible: Option<NodeId>,
    pub hover: Option<NodeId>,
    /// The pointing-device target and an associated activation control, when
    /// present (for example, a label's labeled control).
    pub active: [Option<NodeId>; 2],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum InteractionPseudo {
    Focus,
    FocusWithin,
    FocusVisible,
    Hover,
    Active,
    Target,
}

impl InteractionPseudo {
    pub(crate) const ALL: [Self; 6] = [
        Self::Focus,
        Self::FocusWithin,
        Self::FocusVisible,
        Self::Hover,
        Self::Active,
        Self::Target,
    ];

    const fn bit(self) -> u8 {
        1u8 << self as u8
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct InteractionSet(u8);

impl InteractionSet {
    pub(crate) const EMPTY: Self = Self(0);

    pub(crate) fn insert(&mut self, pseudo: InteractionPseudo) {
        self.0 |= pseudo.bit();
    }

    pub(crate) const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub(crate) const fn contains(self, pseudo: InteractionPseudo) -> bool {
        self.0 & pseudo.bit() != 0
    }

    pub(crate) const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

pub(crate) fn matches(document: &Document, node: NodeId, pseudo: InteractionPseudo) -> bool {
    let state = document.interaction_state();
    match pseudo {
        InteractionPseudo::Target => document.target_element() == Some(node),
        InteractionPseudo::Focus => state.focused == Some(node),
        InteractionPseudo::FocusVisible => state.focus_visible == Some(node),
        InteractionPseudo::FocusWithin => state
            .focused
            .is_some_and(|anchor| propagated_to(document, anchor, node)),
        InteractionPseudo::Hover => state
            .hover
            .is_some_and(|anchor| propagated_to(document, anchor, node)),
        InteractionPseudo::Active => state
            .active
            .into_iter()
            .flatten()
            .any(|anchor| propagated_to(document, anchor, node)),
    }
}

/// Interaction propagation follows the flat tree and includes a top-layer
/// element, but does not continue through its ordinary-tree ancestors.
fn propagated_to(document: &Document, mut current: NodeId, candidate: NodeId) -> bool {
    loop {
        if current == candidate {
            return true;
        }
        if document.is_top_layer_element(current) {
            return false;
        }
        let Ok(Some(parent)) = document.composed_parent(current) else {
            return false;
        };
        current = parent;
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use super::*;
    use crate::{Namespace, NodeKind};

    fn element(name: &str) -> NodeKind {
        NodeKind::Element {
            namespace: Namespace::Html,
            name: name.into(),
            attributes: Vec::new(),
        }
    }

    #[test]
    fn focus_hover_and_active_follow_the_flat_tree_and_stop_at_top_layer() {
        let mut document = Document::new(16);
        let root = document.create(element("main")).unwrap();
        let outside = document.create(element("section")).unwrap();
        let top = document.create(element("dialog")).unwrap();
        let child = document.create(element("button")).unwrap();
        document.append(document.root(), root).unwrap();
        document.append(root, outside).unwrap();
        document.append(root, top).unwrap();
        document.append(top, child).unwrap();
        document.push_top_layer_element(top).unwrap();
        document.set_interaction_state(InteractionState {
            focused: Some(child),
            focus_visible: Some(child),
            hover: Some(child),
            active: [Some(child), None],
        });

        assert!(matches(&document, child, InteractionPseudo::Focus));
        assert!(matches(&document, child, InteractionPseudo::FocusVisible));
        assert!(matches(&document, top, InteractionPseudo::FocusWithin));
        assert!(matches(&document, top, InteractionPseudo::Hover));
        assert!(matches(&document, top, InteractionPseudo::Active));
        assert!(!matches(&document, root, InteractionPseudo::FocusWithin));
        assert!(!matches(&document, outside, InteractionPseudo::Hover));
        assert!(!matches(&document, root, InteractionPseudo::Active));
    }

    #[test]
    fn detached_interaction_targets_are_cleared_and_generation_changes() {
        let mut document = Document::new(8);
        let parent = document.create(element("div")).unwrap();
        let child = document.create(element("button")).unwrap();
        document.append(document.root(), parent).unwrap();
        document.append(parent, child).unwrap();
        document.set_interaction_state(InteractionState {
            hover: Some(child),
            active: [Some(child), None],
            ..InteractionState::default()
        });
        let generation = document.interaction_generation();

        document.remove(child).unwrap();

        assert!(document.interaction_generation() > generation);
        assert!(!matches(&document, parent, InteractionPseudo::Hover));
        assert!(!matches(&document, parent, InteractionPseudo::Active));
    }
}
