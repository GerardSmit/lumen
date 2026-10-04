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
    },
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
    /// Internal slot-distribution update; this does not create a
    /// MutationObserver record.
    SlotAssignment,
}

impl ObservedKind {
    pub fn removed_nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        let (one, many): (Option<NodeId>, &[NodeId]) = match self {
            Self::ChildList { removed, .. } => (*removed, &[]),
            Self::ChildListMany { removed, .. } => (None, removed),
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
