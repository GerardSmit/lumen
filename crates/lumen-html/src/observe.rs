use crate::NodeId;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone)]
pub enum ObservedKind {
    Attribute {
        name: String,
        namespace_uri: Option<String>,
        old_value: Option<String>,
    },
    CharacterData {
        old_value: String,
        offset: usize,
        removed: usize,
        inserted: usize,
    },
    /// Internal live-range relocation after insertion and before truncation.
    TextSplit { new_node: NodeId, offset: usize, parent: NodeId, index: usize },
    /// Internal live-range relocation before removing a normalized Text node.
    TextMerge { destination: NodeId, offset: usize, parent: NodeId, index: usize },
    ChildList {
        added: Option<NodeId>,
        removed: Option<NodeId>,
        previous_sibling: Option<NodeId>,
        next_sibling: Option<NodeId>,
    },
    ChildListMany {
        added: Vec<NodeId>,
        removed: Vec<NodeId>,
    },
    /// A compound replacement's observer-only record; internal mutation
    /// consumers already received each removal and insertion step.
    ChildListReplacement {
        added: Vec<NodeId>,
        removed: Vec<NodeId>,
        previous_sibling: Option<NodeId>,
        next_sibling: Option<NodeId>,
    },
    /// Internal slot-distribution update; this does not create a
    /// MutationObserver record.
    SlotAssignment,
}

impl ObservedKind {
    pub fn added_nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        let (one, many): (Option<NodeId>, &[NodeId]) = match self {
            Self::ChildList { added, .. } => (*added, &[]),
            Self::ChildListMany { added, .. } | Self::ChildListReplacement { added, .. } => (None, added),
            _ => (None, &[]),
        };
        one.into_iter().chain(many.iter().copied())
    }
    pub fn removed_nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        let (one, many): (Option<NodeId>, &[NodeId]) = match self {
            Self::ChildList { removed, .. } => (*removed, &[]),
            Self::ChildListMany { removed, .. } | Self::ChildListReplacement { removed, .. } => (None, removed),
            Self::SlotAssignment => (None, &[]),
            _ => (None, &[]),
        };
        one.into_iter().chain(many.iter().copied())
    }
}

#[derive(Clone)]
pub struct ObservedMutation {
    pub target: NodeId,
    pub kind: ObservedKind,
}
