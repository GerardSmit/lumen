//! Shadow trees keep ordinary DOM links intact; composition is derived on demand.
use crate::{Document, Error, MutationKind, Namespace, NodeId, NodeKind, Dirty};
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShadowMode { Open, Closed }

pub(super) struct ShadowTree { pub host: NodeId, pub root: NodeId, pub mode: ShadowMode }

impl Document {
    pub fn attach_shadow(&mut self, host: NodeId, mode: ShadowMode) -> Result<NodeId, Error> {
        let NodeKind::Element { namespace: Namespace::Html, name, .. } = self.kind(host)? else { return Err(Error::WrongKind); };
        if !name.contains('-') && !matches!(name.as_str(), "article"|"aside"|"blockquote"|"body"|"div"|"footer"|"h1"|"h2"|"h3"|"h4"|"h5"|"h6"|"header"|"main"|"nav"|"p"|"section"|"span") { return Err(Error::WrongKind); }
        if self.shadow_root(host)?.is_some() { return Err(Error::Hierarchy); }
        self.shadow_trees.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
        let root = self.create(NodeKind::DocumentFragment)?;
        self.shadow_trees.push(ShadowTree { host, root, mode });
        self.mark_dirty(host, Dirty::STYLE, MutationKind::FullRebuild);
        Ok(root)
    }

    pub fn shadow_root(&self, host: NodeId) -> Result<Option<NodeId>, Error> {
        self.kind(host)?;
        Ok(self.shadow_trees.iter().find(|tree| tree.host == host).map(|tree| tree.root))
    }
    pub fn shadow_host(&self, root: NodeId) -> Result<Option<NodeId>, Error> {
        self.kind(root)?;
        Ok(self.shadow_trees.iter().find(|tree| tree.root == root).map(|tree| tree.host))
    }
    pub fn shadow_mode(&self, root: NodeId) -> Result<Option<ShadowMode>, Error> {
        self.kind(root)?;
        Ok(self.shadow_trees.iter().find(|tree| tree.root == root).map(|tree| tree.mode))
    }
    pub fn shadow_roots(&self) -> impl Iterator<Item = (NodeId, NodeId, ShadowMode)> + '_ {
        self.shadow_trees.iter().map(|tree| (tree.host, tree.root, tree.mode))
    }
    pub fn composed_parent(&self, node: NodeId) -> Result<Option<NodeId>, Error> {
        if let Some(slot) = self.assigned_slot(node)? { return Ok(Some(slot)); }
        match self.parent(node)? { Some(parent) => Ok(self.shadow_host(parent)?.or(Some(parent))), None => self.shadow_host(node) }
    }
    pub fn root_node(&self, mut node: NodeId, composed: bool) -> Result<NodeId, Error> {
        loop {
            if let Some(parent) = self.parent(node)? { node = parent; }
            else if composed { if let Some(host) = self.shadow_host(node)? { node = host; } else { return Ok(node); } }
            else { return Ok(node); }
        }
    }
    pub fn shadow_including_parent(&self, node: NodeId) -> Result<Option<NodeId>, Error> {
        Ok(self.parent(node)?.or(self.shadow_host(node)?))
    }
    fn children_vec(&self, node: NodeId) -> Result<Vec<NodeId>, Error> {
        let mut result = Vec::new();
        let mut child = self.first_child(node)?;
        while let Some(id) = child { result.try_reserve(1).map_err(|_| Error::LimitExceeded)?; result.push(id); child = self.next_sibling(id)?; }
        Ok(result)
    }
    fn slot_name(&self, node: NodeId, attribute: &str) -> Result<&str, Error> {
        Ok(match self.kind(node)? { NodeKind::Element { attributes, .. } => attributes.iter().find(|(name, _)| name == attribute).map_or("", |(_, value)| value.as_str()), _ => "" })
    }
    pub fn is_slot(&self, node: NodeId) -> Result<bool, Error> {
        Ok(matches!(self.kind(node)?, NodeKind::Element { namespace: Namespace::Html, name, .. } if name == "slot"))
    }
    pub fn assigned_slot(&self, node: NodeId) -> Result<Option<NodeId>, Error> {
        if !matches!(self.kind(node)?, NodeKind::Element { .. }|NodeKind::Text(_)) { return Ok(None); }
        let Some(parent) = self.parent(node)? else { return Ok(None); };
        let Some(root) = self.shadow_root(parent)? else { return Ok(None); };
        let name = self.slot_name(node, "slot")?;
        let mut current = self.first_child(root)?;
        while let Some(id) = current {
            if self.is_slot(id)? && self.slot_name(id, "name")? == name { return Ok(Some(id)); }
            current = crate::selector::next_descendant(self, root, id)?;
        }
        Ok(None)
    }
    pub fn assigned_nodes(&self, slot: NodeId, flatten: bool) -> Result<Vec<NodeId>, Error> {
        if !self.is_slot(slot)? { return Err(Error::WrongKind); }
        let root = self.root_node(slot, false)?;
        let Some(host) = self.shadow_host(root)? else { return Ok(Vec::new()); };
        let mut nodes = Vec::new();
        for node in self.children_vec(host)? {
            if self.assigned_slot(node)? == Some(slot) { nodes.try_reserve(1).map_err(|_| Error::LimitExceeded)?; nodes.push(node); }
        }
        if !flatten { return Ok(nodes); }
        if nodes.is_empty() { nodes = self.children_vec(slot)?; }
        let mut pending = nodes; pending.reverse();
        let mut result = Vec::new();
        let mut visited = 0;
        while let Some(node) = pending.pop() {
            visited += 1;
            if visited > self.node_count() { return Err(Error::LimitExceeded); }
            if self.is_slot(node)? && self.shadow_host(self.root_node(node, false)?)?.is_some() {
                let mut children = self.assigned_nodes(node, false)?;
                if children.is_empty() { children = self.children_vec(node)?; }
                pending.try_reserve(children.len()).map_err(|_| Error::LimitExceeded)?;
                pending.extend(children.into_iter().rev());
            } else if matches!(self.kind(node)?, NodeKind::Element { .. }|NodeKind::Text(_)) { result.try_reserve(1).map_err(|_| Error::LimitExceeded)?; result.push(node); }
        }
        Ok(result)
    }
    pub fn composed_children(&self, node: NodeId) -> Result<Vec<NodeId>, Error> {
        if let Some(root) = self.shadow_root(node)? { return self.children_vec(root); }
        if self.is_slot(node)? && self.shadow_host(self.root_node(node, false)?)?.is_some() {
            let assigned = self.assigned_nodes(node, false)?;
            if !assigned.is_empty() { return Ok(assigned); }
        }
        self.children_vec(node)
    }
    pub fn event_parent(&self, node: NodeId, composed: bool, origin_root: NodeId) -> Result<Option<NodeId>, Error> {
        if let Some(slot) = self.assigned_slot(node)? { return Ok(Some(slot)); }
        if let Some(host) = self.shadow_host(node)? { return Ok((composed || node != origin_root).then_some(host)); }
        self.parent(node)
    }
    pub fn retarget(&self, mut target: NodeId, against: Option<NodeId>) -> Result<NodeId, Error> {
        loop {
            let root = self.root_node(target, false)?;
            let Some(host) = self.shadow_host(root)? else { return Ok(target); };
            let mut current = against;
            while let Some(node) = current { if node == root { return Ok(target); } current = self.shadow_including_parent(node)?; }
            target = host;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{html, selector};
    fn find(document: &Document, root: NodeId, query: &str) -> NodeId { selector::query_selector(document, root, query).unwrap().unwrap() }

    #[test]
    fn roots_are_bounded_detached_and_invalidate_through_the_host() {
        let mut document = html::parse("<div></div>", 64).unwrap();
        let host = find(&document, document.root(), "div");
        let root = document.attach_shadow(host, ShadowMode::Closed).unwrap();
        let text = document.create(NodeKind::Text("shadow".into())).unwrap();
        document.append(root, text).unwrap();
        assert_eq!(document.parent(root), Ok(None));
        assert_eq!(document.root_node(text, false), Ok(root));
        assert_eq!(document.root_node(text, true), Ok(document.root()));
        assert_eq!(document.shadow_mode(root), Ok(Some(ShadowMode::Closed)));
        assert_eq!(document.composed_children(host), Ok(alloc::vec![text]));
        assert_eq!(document.append(root, host), Err(Error::Hierarchy));
        assert_eq!(document.attach_shadow(host, ShadowMode::Open), Err(Error::Hierarchy));
        document.clear_dirty(host).unwrap();
        document.replace_data(text, "updated").unwrap();
        assert!(document.dirty(host).unwrap().contains(Dirty::LAYOUT));
        assert_eq!(document.destroy_subtree(root), Err(Error::Hierarchy));
        document.remove(host).unwrap();
        document.destroy_subtree(host).unwrap();
        assert_eq!(document.kind(root), Err(Error::InvalidNode));
        assert_eq!(document.kind(text), Err(Error::InvalidNode));
        assert_eq!(document.shadow_roots().count(), 0);
        let mut bounded = Document::new(2);
        let host = bounded.create(NodeKind::Element { namespace: Namespace::Html, name: "div".into(), attributes: Vec::new() }).unwrap();
        assert_eq!(bounded.attach_shadow(host, ShadowMode::Open), Err(Error::LimitExceeded));
        assert_eq!(bounded.shadow_root(host), Ok(None));
        assert_eq!(bounded.node_count(), 2);
    }

    #[test]
    fn slots_assign_first_match_text_and_fallback_and_reassign() {
        let mut document = html::parse("<div><b slot=named>A</b>text<i slot=missing>B</i></div>", 64).unwrap();
        let host = find(&document, document.root(), "div");
        let named = find(&document, host, "b");
        let unmatched = find(&document, host, "i");
        let text = document.next_sibling(named).unwrap().unwrap();
        let root = document.attach_shadow(host, ShadowMode::Open).unwrap();
        let fragment = html::parse_fragment(&mut document, "<slot name=named></slot><slot></slot><slot name=named><slot><em>fallback</em></slot></slot>").unwrap();
        document.append(root, fragment).unwrap();
        let first = document.first_child(root).unwrap().unwrap();
        let default = document.next_sibling(first).unwrap().unwrap();
        let duplicate = document.next_sibling(default).unwrap().unwrap();
        assert_eq!(document.assigned_slot(named), Ok(Some(first)));
        assert_eq!(document.assigned_slot(text), Ok(Some(default)));
        assert_eq!(document.assigned_slot(unmatched), Ok(None));
        assert_eq!(document.assigned_nodes(first, false), Ok(alloc::vec![named]));
        assert!(document.assigned_nodes(duplicate, false).unwrap().is_empty());
        let fallback = find(&document, duplicate, "em");
        assert_eq!(document.assigned_nodes(duplicate, true), Ok(alloc::vec![fallback]));
        document.set_attribute(named, "slot", "").unwrap();
        assert!(document.assigned_nodes(first, false).unwrap().is_empty());
        assert_eq!(document.assigned_nodes(default, false), Ok(alloc::vec![named, text]));
        assert_eq!(document.parent(named), Ok(Some(host)));
        assert_eq!(document.composed_parent(named), Ok(Some(default)));
    }

    #[test]
    fn event_parent_and_retarget_observe_shadow_boundaries() {
        let mut document = html::parse("<div><b></b></div>", 32).unwrap();
        let host = find(&document, document.root(), "div");
        let light = find(&document, host, "b");
        let root = document.attach_shadow(host, ShadowMode::Open).unwrap();
        let slot = document.create(NodeKind::Element { namespace: Namespace::Html, name: "slot".into(), attributes: Vec::new() }).unwrap();
        document.append(root, slot).unwrap();
        assert_eq!(document.event_parent(light, false, document.root()), Ok(Some(slot)));
        assert_eq!(document.event_parent(root, false, document.root()), Ok(Some(host)));
        assert_eq!(document.event_parent(root, false, root), Ok(None));
        assert_eq!(document.event_parent(root, true, root), Ok(Some(host)));
        assert_eq!(document.retarget(slot, Some(host)), Ok(host));
        assert_eq!(document.retarget(slot, Some(slot)), Ok(slot));
        assert_eq!(document.retarget(light, Some(slot)), Ok(light));
    }
}
