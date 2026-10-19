use alloc::vec::Vec;
use core::ops::Deref;
use crate::{Document, Error, NodeId, ShadowOptions};

pub trait DocumentGraph {
    type Read<'a>: Deref<Target = Document> where Self: 'a;
    fn read(&self, node: NodeId) -> Result<Self::Read<'_>, Error>;
    fn node_limit(&self) -> usize;
}

impl DocumentGraph for Document {
    type Read<'a> = &'a Document;
    fn read(&self, node: NodeId) -> Result<&Document, Error> { self.kind(node)?; Ok(self) }
    fn node_limit(&self) -> usize { self.node_count() }
}

pub fn host_including_ancestor<G: DocumentGraph>(graph: &G, ancestor: NodeId, mut node: NodeId) -> Result<bool, Error> {
    for _ in 0..=graph.node_limit() {
        if ancestor == node { return Ok(true); }
        let Some(parent) = graph.read(node)?.host_including_parent(node)? else { return Ok(false); };
        node = parent;
    }
    Err(Error::Hierarchy)
}

pub fn identity_root<G: DocumentGraph>(graph: &G, mut node: NodeId) -> Result<NodeId, Error> {
    for _ in 0..=graph.node_limit() {
        let Some(parent) = graph.read(node)?.host_including_parent(node)? else { return Ok(node); };
        node = parent;
    }
    Err(Error::Hierarchy)
}

pub fn identity_nodes<G: DocumentGraph>(graph: &G, root: NodeId) -> Result<Vec<NodeId>, Error> {
    identity_nodes_bounded(graph, root, usize::MAX)
}

/// Shared identity traversal with operation-local allocation admission. Both
/// frontier and output capacity are charged before growth, including Vec's
/// minimum allocation. Callers can reject a wide tree without first retaining
/// an unbounded temporary list.
pub fn identity_nodes_bounded<G: DocumentGraph>(graph: &G, root: NodeId, byte_limit: usize) -> Result<Vec<NodeId>, Error> {
    fn reserve(nodes: &mut Vec<NodeId>, additional: usize, other_capacity: usize, limit: usize) -> Result<(), Error> {
        let required=nodes.len().checked_add(additional).ok_or(Error::LimitExceeded)?;
        if required<=nodes.capacity() {return Ok(());}
        let capacity=required.max(nodes.capacity().checked_mul(2).ok_or(Error::LimitExceeded)?).max(4);
        let bytes=capacity.checked_add(other_capacity).and_then(|count|count.checked_mul(core::mem::size_of::<NodeId>())).ok_or(Error::LimitExceeded)?;
        if bytes>limit {return Err(Error::LimitExceeded);}
        nodes.try_reserve_exact(capacity-nodes.len()).map_err(|_|Error::LimitExceeded)?;
        // An allocator may grant excess capacity; retain the same admission
        // invariant before making any mutation outside this temporary walk.
        if nodes.capacity().checked_add(other_capacity).and_then(|count|count.checked_mul(core::mem::size_of::<NodeId>())).is_none_or(|bytes|bytes>limit) {return Err(Error::LimitExceeded);}
        Ok(())
    }
    let mut pending=Vec::new();
    let mut result=Vec::new();
    reserve(&mut pending,1,0,byte_limit)?;
    pending.push(root);
    while let Some(node) = pending.pop() {
        if result.len() >= graph.node_limit() { return Err(Error::Hierarchy); }
        reserve(&mut result,1,pending.capacity(),byte_limit)?;
        result.push(node);
        let document = graph.read(node)?;
        if let Some(attributes) = document.materialized_attribute_nodes(node) {
            reserve(&mut pending,attributes.len(),result.capacity(),byte_limit)?;
            pending.extend(attributes.iter().map(|(_, attribute)| *attribute));
        }
        if let Some(root) = document.shadow_root(node)? {
            reserve(&mut pending,1,result.capacity(),byte_limit)?;pending.push(root);
        }
        if let Some(content) = document.template_content(node)? {
            reserve(&mut pending,1,result.capacity(),byte_limit)?;pending.push(content);
        }
        let mut child = document.first_child(node)?;
        while let Some(id) = child {
            if pending.len()>=graph.node_limit() {return Err(Error::Hierarchy);}
            reserve(&mut pending,1,result.capacity(),byte_limit)?;pending.push(id);
            child = document.next_sibling(id)?;
        }
    }
    Ok(result)
}

#[derive(Clone, Copy)]
pub enum ClonePlacement { Root, Child(usize), Template(usize), Shadow(usize, ShadowOptions) }
pub struct CloneStep { pub source: NodeId, pub placement: ClonePlacement }
pub struct ClonePlan { pub steps: Vec<CloneStep>, pub has_template: bool }

pub fn clone_plan<G: DocumentGraph>(graph: &G, root: NodeId, deep: bool) -> Result<ClonePlan, Error> {
    let mut plan = ClonePlan { steps: Vec::new(), has_template: false };
    let mut pending = alloc::vec![(root, deep, ClonePlacement::Root)];
    while let Some((source, children, placement)) = pending.pop() {
        if plan.steps.len() >= graph.node_limit() { return Err(Error::Hierarchy); }
        let index = plan.steps.len();
        plan.steps.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
        plan.steps.push(CloneStep { source, placement });
        let document = graph.read(source)?;
        if children {
            let mut child = document.last_child(source)?;
            while let Some(node) = child { pending.push((node, true, ClonePlacement::Child(index))); child = document.previous_sibling(node)?; }
        }
        if let Some(shadow) = document.shadow_root(source)? {
            let options = document.shadow_options(shadow)?.ok_or(Error::WrongKind)?;
            if options.clonable { pending.push((shadow, true, ClonePlacement::Shadow(index, options))); }
        }
        if let Some(content) = document.template_content(source)? {
            plan.has_template = true;
            pending.push((content, children, ClonePlacement::Template(index)));
        }
    }
    Ok(plan)
}
