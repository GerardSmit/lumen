use alloc::string::String;
use crate::NodeId;

#[derive(Clone)]
pub enum ObservedKind {
    Attribute { name: String, old_value: Option<String> },
    CharacterData { old_value: String },
    ChildList { added: Option<NodeId>, removed: Option<NodeId>, previous_sibling: Option<NodeId>, next_sibling: Option<NodeId> },
}

#[derive(Clone)]
pub struct ObservedMutation { pub target: NodeId, pub kind: ObservedKind }
