//! Shadow trees keep ordinary DOM links intact; composition is derived on demand.
use crate::{Dirty, Document, Error, MutationKind, Namespace, NodeId, NodeKind};
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShadowMode {
    Open,
    Closed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SlotAssignmentMode {
    #[default]
    Named,
    Manual,
}

/// Native shadow-root state retained independently of author attributes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShadowOptions {
    pub mode: ShadowMode,
    pub slot_assignment: SlotAssignmentMode,
    pub delegates_focus: bool,
    pub clonable: bool,
    pub serializable: bool,
    pub declarative: bool,
}

impl ShadowOptions {
    pub const fn new(mode: ShadowMode) -> Self {
        Self {
            mode,
            slot_assignment: SlotAssignmentMode::Named,
            delegates_focus: false,
            clonable: false,
            serializable: false,
            declarative: false,
        }
    }
}

pub(super) struct ShadowTree {
    pub host: NodeId,
    pub root: NodeId,
    pub mode: ShadowMode,
    pub options: ShadowOptions,
}

enum ComposedSource {
    Raw { current: Option<NodeId> },
    List(alloc::vec::IntoIter<NodeId>),
}

/// Composed child walk: shadow tree children for a host, assigned (or
/// fallback) nodes for a slot, otherwise the ordinary child sequence.
pub struct ComposedChildren<'a> {
    document: &'a Document,
    source: ComposedSource,
}

impl ComposedChildren<'_> {
    pub fn next(&mut self) -> Result<Option<NodeId>, Error> {
        match &mut self.source {
            ComposedSource::Raw { current } => {
                let node = *current;
                if let Some(id) = node {
                    *current = self.document.next_sibling(id)?;
                }
                Ok(node)
            }
            ComposedSource::List(list) => Ok(list.next()),
        }
    }
}

impl Document {
    pub fn attach_shadow(&mut self, host: NodeId, mode: ShadowMode) -> Result<NodeId, Error> {
        self.attach_shadow_with_options(host, ShadowOptions::new(mode))
    }

    pub fn attach_shadow_with_options(
        &mut self,
        host: NodeId,
        options: ShadowOptions,
    ) -> Result<NodeId, Error> {
        let NodeKind::Element {
            namespace: Namespace::Html,
            name,
            ..
        } = self.kind(host)?
        else {
            return Err(Error::WrongKind);
        };
        if !name.contains('-')
            && !matches!(
                name.as_str(),
                "article"
                    | "aside"
                    | "blockquote"
                    | "body"
                    | "div"
                    | "footer"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "header"
                    | "main"
                    | "nav"
                    | "p"
                    | "section"
                    | "span"
            )
        {
            return Err(Error::WrongKind);
        }
        if let Some(index) = self.shadow_trees.iter().position(|tree| tree.host == host) {
            let existing = &self.shadow_trees[index];
            if !existing.options.declarative || existing.mode != options.mode {
                return Err(Error::Hierarchy);
            }
            let root = existing.root;
            // Declarative roots can be claimed once by attachShadow. Preserve the root's
            // identity and original options; removal uses the ordinary mutation/range hooks.
            while let Some(child) = self.first_child(root)? {
                self.remove(child)?;
            }
            self.shadow_trees[index].options.declarative = false;
            return Ok(root);
        }
        self.shadow_trees
            .try_reserve(1)
            .map_err(|_| Error::LimitExceeded)?;
        let root = self.create(NodeKind::DocumentFragment)?;
        self.shadow_trees.push(ShadowTree {
            host,
            root,
            mode: options.mode,
            options,
        });
        self.mark_dirty(host, Dirty::STYLE, MutationKind::FullRebuild);
        Ok(root)
    }

    pub fn shadow_root(&self, host: NodeId) -> Result<Option<NodeId>, Error> {
        self.kind(host)?;
        Ok(self
            .shadow_trees
            .iter()
            .find(|tree| tree.host == host)
            .map(|tree| tree.root))
    }
    pub fn shadow_host(&self, root: NodeId) -> Result<Option<NodeId>, Error> {
        self.kind(root)?;
        Ok(self
            .shadow_trees
            .iter()
            .find(|tree| tree.root == root)
            .map(|tree| tree.host))
    }
    pub fn shadow_mode(&self, root: NodeId) -> Result<Option<ShadowMode>, Error> {
        self.kind(root)?;
        Ok(self
            .shadow_trees
            .iter()
            .find(|tree| tree.root == root)
            .map(|tree| tree.mode))
    }

    pub fn shadow_options(&self, root: NodeId) -> Result<Option<ShadowOptions>, Error> {
        self.kind(root)?;
        Ok(self
            .shadow_trees
            .iter()
            .find(|tree| tree.root == root)
            .map(|tree| tree.options))
    }
    pub fn shadow_roots(&self) -> impl Iterator<Item = (NodeId, NodeId, ShadowMode)> + '_ {
        self.shadow_trees
            .iter()
            .map(|tree| (tree.host, tree.root, tree.mode))
    }
    pub fn composed_parent(&self, node: NodeId) -> Result<Option<NodeId>, Error> {
        if let Some(slot) = self.assigned_slot(node)? {
            return Ok(Some(slot));
        }
        match self.parent(node)? {
            Some(parent) => Ok(self.shadow_host(parent)?.or(Some(parent))),
            None => self.shadow_host(node),
        }
    }
    pub fn root_node(&self, mut node: NodeId, composed: bool) -> Result<NodeId, Error> {
        loop {
            if let Some(parent) = self.parent(node)? {
                node = parent;
            } else if composed {
                if let Some(host) = self.shadow_host(node)? {
                    node = host;
                } else {
                    return Ok(node);
                }
            } else {
                return Ok(node);
            }
        }
    }
    pub fn shadow_including_parent(&self, node: NodeId) -> Result<Option<NodeId>, Error> {
        Ok(self.parent(node)?.or(self.shadow_host(node)?))
    }
    fn children_vec(&self, node: NodeId) -> Result<Vec<NodeId>, Error> {
        let mut result = Vec::new();
        let mut child = self.first_child(node)?;
        while let Some(id) = child {
            result.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
            result.push(id);
            child = self.next_sibling(id)?;
        }
        Ok(result)
    }
    fn slot_name(&self, node: NodeId, attribute: &str) -> Result<&str, Error> {
        Ok(match self.kind(node)? {
            NodeKind::Element { .. } => self
                .get_attribute_ns_ref(node, None, attribute)?
                .unwrap_or(""),
            _ => "",
        })
    }
    pub fn is_slot(&self, node: NodeId) -> Result<bool, Error> {
        Ok(
            matches!(self.kind(node)?, NodeKind::Element { namespace: Namespace::Html, name, .. } if name == "slot"),
        )
    }
    pub fn assigned_slot(&self, node: NodeId) -> Result<Option<NodeId>, Error> {
        if !matches!(
            self.kind(node)?,
            NodeKind::Element { .. } | NodeKind::Text(_) | NodeKind::CData(_)
        ) {
            return Ok(None);
        }
        let Some(parent) = self.parent(node)? else {
            return Ok(None);
        };
        let Some(root) = self.shadow_root(parent)? else {
            return Ok(None);
        };
        if let Some(tree) = self.shadow_trees.iter().find(|tree| tree.root == root) {
            if tree.options.slot_assignment == SlotAssignmentMode::Manual {
                for slot in self.slots_in_tree(root)? {
                    if self
                        .manual_assignments
                        .iter()
                        .find(|(assigned_slot, _)| *assigned_slot == slot)
                        .is_some_and(|(_, nodes)| nodes.contains(&node))
                    {
                        return Ok(Some(slot));
                    }
                }
                return Ok(None);
            }
        }
        let name = self.slot_name(node, "slot")?;
        let mut current = self.first_child(root)?;
        while let Some(id) = current {
            if self.is_slot(id)? && self.slot_name(id, "name")? == name {
                return Ok(Some(id));
            }
            current = crate::selector::next_descendant(self, root, id)?;
        }
        Ok(None)
    }
    pub fn assigned_nodes(&self, slot: NodeId, flatten: bool) -> Result<Vec<NodeId>, Error> {
        if !self.is_slot(slot)? {
            return Err(Error::WrongKind);
        }
        let root = self.root_node(slot, false)?;
        let Some(host) = self.shadow_host(root)? else {
            return Ok(Vec::new());
        };
        if let Some(tree) = self.shadow_trees.iter().find(|tree| tree.root == root) {
            if tree.options.slot_assignment == SlotAssignmentMode::Manual {
                let mut nodes = self
                    .manual_assignments
                    .iter()
                    .find(|(assigned_slot, _)| *assigned_slot == slot)
                    .map(|(_, nodes)| nodes.clone())
                    .unwrap_or_default();
                nodes.retain(|node| {
                    self.parent(*node).ok().flatten() == Some(host)
                        && matches!(
                            self.kind(*node),
                            Ok(NodeKind::Element { .. } | NodeKind::Text(_) | NodeKind::CData(_))
                        )
                });
                if !flatten || !nodes.is_empty() {
                    return Ok(nodes);
                }
                nodes = self.children_vec(slot)?;
                return self.flatten_assigned_nodes(nodes);
            }
        }
        let mut nodes = Vec::new();
        for node in self.children_vec(host)? {
            if self.assigned_slot(node)? == Some(slot) {
                nodes.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
                nodes.push(node);
            }
        }
        if !flatten {
            return Ok(nodes);
        }
        if nodes.is_empty() {
            nodes = self.children_vec(slot)?;
        }
        self.flatten_assigned_nodes(nodes)
    }

    fn flatten_assigned_nodes(&self, nodes: Vec<NodeId>) -> Result<Vec<NodeId>, Error> {
        let mut pending = nodes;
        pending.reverse();
        let mut result = Vec::new();
        let mut visited = 0;
        while let Some(node) = pending.pop() {
            visited += 1;
            if visited > self.node_count() {
                return Err(Error::LimitExceeded);
            }
            if self.is_slot(node)? && self.shadow_host(self.root_node(node, false)?)?.is_some() {
                let mut children = self.assigned_nodes(node, false)?;
                if children.is_empty() {
                    children = self.children_vec(node)?;
                }
                pending
                    .try_reserve(children.len())
                    .map_err(|_| Error::LimitExceeded)?;
                pending.extend(children.into_iter().rev());
            } else if matches!(
                self.kind(node)?,
                NodeKind::Element { .. } | NodeKind::Text(_) | NodeKind::CData(_)
            ) {
                result.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
                result.push(node);
            }
        }
        Ok(result)
    }

    fn slots_in_tree(&self, root: NodeId) -> Result<Vec<NodeId>, Error> {
        let mut slots = Vec::new();
        let mut current = self.first_child(root)?;
        while let Some(node) = current {
            if self.is_slot(node)? {
                slots.push(node);
            }
            current = crate::selector::next_descendant(self, root, node)?;
        }
        Ok(slots)
    }

    /// Set a slot's manual slottables. Assignment data is retained even while
    /// the slot is detached or belongs to a named-assignment shadow root.
    pub fn assign_slot(&mut self, slot: NodeId, nodes: &[NodeId]) -> Result<bool, Error> {
        if !self.is_slot(slot)? {
            return Err(Error::WrongKind);
        }
        let mut unique = Vec::new();
        unique
            .try_reserve(nodes.len())
            .map_err(|_| Error::LimitExceeded)?;
        for &node in nodes {
            if !matches!(
                self.kind(node)?,
                NodeKind::Element { .. } | NodeKind::Text(_) | NodeKind::CData(_)
            ) {
                return Err(Error::WrongKind);
            }
            if !unique.contains(&node) {
                unique.push(node);
            }
        }
        let existing = self
            .manual_assignments
            .iter()
            .position(|(id, _)| *id == slot);
        if existing.is_some_and(|index| self.manual_assignments[index].1 == unique) {
            return Ok(true);
        }
        if existing.is_none() && !unique.is_empty() {
            self.manual_assignments
                .try_reserve(1)
                .map_err(|_| Error::LimitExceeded)?;
        }
        let prior_slots = self
            .manual_assignments
            .iter()
            .filter(|(id, assigned)| {
                *id != slot && assigned.iter().any(|node| unique.contains(node))
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        let mut affected = prior_slots.clone();
        if !affected.contains(&slot) {
            affected.push(slot);
        }
        let before = affected
            .iter()
            .map(|changed_slot| {
                (
                    *changed_slot,
                    self.assigned_nodes(*changed_slot, false)
                        .unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>();
        for (id, assigned) in &mut self.manual_assignments {
            if *id != slot {
                assigned.retain(|node| !unique.contains(node));
            }
        }
        if let Some(index) = existing {
            self.manual_assignments[index].1 = unique;
        } else if !unique.is_empty() {
            self.manual_assignments.push((slot, unique));
        }
        let mut notify = false;
        let mut changed = Vec::new();
        for changed_slot in affected {
            let previous = before
                .iter()
                .find(|(id, _)| *id == changed_slot)
                .map(|(_, nodes)| nodes.as_slice())
                .unwrap_or(&[]);
            let current = self.assigned_nodes(changed_slot, false).unwrap_or_default();
            if previous == current.as_slice() {
                continue;
            }
            let root = self.root_node(changed_slot, false).ok();
            let host = root.and_then(|root| {
                self.shadow_trees
                    .iter()
                    .find(|tree| tree.root == root)
                    .filter(|tree| tree.options.slot_assignment == SlotAssignmentMode::Manual)
                    .map(|tree| tree.host)
            });
            if let Some(host) = host {
                self.mark_dirty(host, Dirty::STYLE, MutationKind::FullRebuild);
                notify = true;
                changed.push(changed_slot);
            }
        }
        if notify {
            for changed_slot in changed {
                self.notify(crate::observe::ObservedMutation {
                    target: changed_slot,
                    kind: crate::observe::ObservedKind::SlotAssignment,
                });
            }
        }
        Ok(true)
    }
    pub fn composed_children(&self, node: NodeId) -> Result<Vec<NodeId>, Error> {
        if let Some(root) = self.shadow_root(node)? {
            return self.children_vec(root);
        }
        if self.is_slot(node)? && self.shadow_host(self.root_node(node, false)?)?.is_some() {
            let assigned = self.assigned_nodes(node, false)?;
            if !assigned.is_empty() {
                return Ok(assigned);
            }
        }
        self.children_vec(node)
    }
    /// Whether a node's children compose: hosts with an attached shadow tree
    /// and slots inside shadow trees. Cheap gate for the composed walk.
    pub fn composes(&self, node: NodeId) -> Result<bool, Error> {
        if self.shadow_trees.iter().any(|tree| tree.host == node) {
            return Ok(true);
        }
        Ok(self.is_slot(node)? && self.shadow_host(self.root_node(node, false)?)?.is_some())
    }
    /// Iterator over the composed children, falling back to the ordinary
    /// child walk (no allocation) when the node does not compose.
    pub fn composed_children_iter(&self, node: NodeId) -> Result<ComposedChildren<'_>, Error> {
        if self.composes(node)? {
            Ok(ComposedChildren {
                document: self,
                source: ComposedSource::List(self.composed_children(node)?.into_iter()),
            })
        } else {
            Ok(ComposedChildren {
                document: self,
                source: ComposedSource::Raw {
                    current: self.first_child(node)?,
                },
            })
        }
    }
    pub fn event_parent(
        &self,
        node: NodeId,
        composed: bool,
        origin_root: NodeId,
    ) -> Result<Option<NodeId>, Error> {
        if let Some(slot) = self.assigned_slot(node)? {
            return Ok(Some(slot));
        }
        if let Some(host) = self.shadow_host(node)? {
            return Ok((composed || node != origin_root).then_some(host));
        }
        self.parent(node)
    }
    pub fn retarget(&self, mut target: NodeId, against: Option<NodeId>) -> Result<NodeId, Error> {
        loop {
            let root = self.root_node(target, false)?;
            let Some(host) = self.shadow_host(root)? else {
                return Ok(target);
            };
            let mut current = against;
            while let Some(node) = current {
                if node == root {
                    return Ok(target);
                }
                current = self.shadow_including_parent(node)?;
            }
            target = host;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{html, selector};
    fn find(document: &Document, root: NodeId, query: &str) -> NodeId {
        selector::query_selector(document, root, query)
            .unwrap()
            .unwrap()
    }

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
        assert_eq!(
            document.attach_shadow(host, ShadowMode::Open),
            Err(Error::Hierarchy)
        );
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
        let host = bounded
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: "div".into(),
                attributes: Vec::new(),
            })
            .unwrap();
        assert_eq!(
            bounded.attach_shadow(host, ShadowMode::Open),
            Err(Error::LimitExceeded)
        );
        assert_eq!(bounded.shadow_root(host), Ok(None));
        assert_eq!(bounded.node_count(), 2);
    }

    #[test]
    fn slots_assign_first_match_text_and_fallback_and_reassign() {
        let mut document = html::parse(
            "<div><b slot=named>A</b>text<i slot=missing>B</i></div>",
            64,
        )
        .unwrap();
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
        assert_eq!(
            document.assigned_nodes(first, false),
            Ok(alloc::vec![named])
        );
        assert!(document
            .assigned_nodes(duplicate, false)
            .unwrap()
            .is_empty());
        let fallback = find(&document, duplicate, "em");
        assert_eq!(
            document.assigned_nodes(duplicate, true),
            Ok(alloc::vec![fallback])
        );
        document.set_attribute(named, "slot", "").unwrap();
        assert!(document.assigned_nodes(first, false).unwrap().is_empty());
        assert_eq!(
            document.assigned_nodes(default, false),
            Ok(alloc::vec![named, text])
        );
        assert_eq!(document.parent(named), Ok(Some(host)));
        assert_eq!(document.composed_parent(named), Ok(Some(default)));
    }

    #[test]
    fn slot_assignment_ignores_namespaced_slot_and_name_attributes() {
        let mut document = html::parse("<div><b>child</b></div>", 64).unwrap();
        let host = find(&document, document.root(), "div");
        let child = find(&document, host, "b");
        let root = document.attach_shadow(host, ShadowMode::Open).unwrap();
        let fragment =
            html::parse_fragment(&mut document, "<slot name=target></slot><slot></slot>").unwrap();
        document.append(root, fragment).unwrap();
        let named = document.first_child(root).unwrap().unwrap();
        let default = document.next_sibling(named).unwrap().unwrap();
        let ns = Some("https://example.test/attributes");
        document
            .set_attribute_ns(child, ns, "slot", "target")
            .unwrap();
        assert_eq!(document.assigned_slot(child), Ok(Some(default)));
        document
            .set_attribute_ns(child, None, "slot", "target")
            .unwrap();
        assert_eq!(document.assigned_slot(child), Ok(Some(named)));
        document.remove_attribute_ns(named, None, "name").unwrap();
        document
            .set_attribute_ns(named, ns, "name", "target")
            .unwrap();
        assert_eq!(document.assigned_slot(child), Ok(None));
    }

    #[test]
    fn manual_slots_assign_ordered_slottables_and_ignore_slot_attributes() {
        let mut document = html::parse("<div><b slot=ignored>A</b>text<i>B</i></div>", 64).unwrap();
        let host = find(&document, document.root(), "div");
        let first = find(&document, host, "b");
        let text = document.next_sibling(first).unwrap().unwrap();
        let second = document.next_sibling(text).unwrap().unwrap();
        let root = document
            .attach_shadow_with_options(
                host,
                ShadowOptions {
                    slot_assignment: SlotAssignmentMode::Manual,
                    ..ShadowOptions::new(ShadowMode::Open)
                },
            )
            .unwrap();
        let fragment = html::parse_fragment(&mut document, "<slot></slot><slot></slot>").unwrap();
        document.append(root, fragment).unwrap();
        let slot = document.first_child(root).unwrap().unwrap();
        let next_slot = document.next_sibling(slot).unwrap().unwrap();
        assert!(document.assigned_nodes(slot, false).unwrap().is_empty());
        assert_eq!(
            document.assign_slot(slot, &[second, first, second]),
            Ok(true)
        );
        assert_eq!(
            document.assigned_nodes(slot, false),
            Ok(alloc::vec![second, first])
        );
        assert_eq!(document.assigned_slot(first), Ok(Some(slot)));
        assert_eq!(document.assigned_slot(second), Ok(Some(slot)));
        assert_eq!(document.assigned_slot(text), Ok(None));
        assert_eq!(
            document.composed_children(slot),
            Ok(alloc::vec![second, first])
        );
        assert_eq!(document.assign_slot(next_slot, &[first]), Ok(true));
        assert_eq!(
            document.assigned_nodes(slot, false),
            Ok(alloc::vec![second])
        );
        assert_eq!(document.assigned_slot(first), Ok(Some(next_slot)));
        assert_eq!(document.assign_slot(slot, &[]), Ok(true));
        assert_eq!(document.assigned_slot(first), Ok(Some(next_slot)));
        assert_eq!(document.assigned_nodes(slot, true), Ok(alloc::vec![]));
        let detached = document
            .create(NodeKind::Text(alloc::string::String::from("detached")))
            .unwrap();
        assert_eq!(document.assign_slot(slot, &[detached]), Ok(true));
        assert_eq!(document.assigned_nodes(slot, false), Ok(alloc::vec![]));
        document.append(host, detached).unwrap();
        assert_eq!(
            document.assigned_nodes(slot, false),
            Ok(alloc::vec![detached])
        );
    }

    #[test]
    fn cdata_nodes_participate_in_named_and_manual_slot_assignment() {
        for mode in [SlotAssignmentMode::Named, SlotAssignmentMode::Manual] {
            let mut document = html::parse("<div></div>", 64).unwrap();
            let host = find(&document, document.root(), "div");
            let cdata = document.create(NodeKind::CData("content".into())).unwrap();
            document.append(host, cdata).unwrap();
            let root = document
                .attach_shadow_with_options(
                    host,
                    ShadowOptions {
                        slot_assignment: mode,
                        ..ShadowOptions::new(ShadowMode::Open)
                    },
                )
                .unwrap();
            let fragment = html::parse_fragment(&mut document, "<slot></slot>").unwrap();
            document.append(root, fragment).unwrap();
            let slot = document.first_child(root).unwrap().unwrap();
            if mode == SlotAssignmentMode::Manual {
                assert_eq!(document.assigned_slot(cdata), Ok(None));
                assert_eq!(document.assign_slot(slot, &[cdata]), Ok(true));
            }
            assert_eq!(document.assigned_slot(cdata), Ok(Some(slot)));
            assert_eq!(document.assigned_nodes(slot, true), Ok(alloc::vec![cdata]));
            document.remove(cdata).unwrap();
            assert_eq!(document.assigned_slot(cdata), Ok(None));
            assert!(document.assigned_nodes(slot, true).unwrap().is_empty());
        }
    }

    #[test]
    fn declarative_shadow_root_is_claimed_once_without_replacing_identity() {
        for mode in [ShadowMode::Open, ShadowMode::Closed] {
            let mut document = html::parse("<div></div>", 32).unwrap();
            let host = find(&document, document.root(), "div");
            let options = ShadowOptions {
                mode,
                slot_assignment: SlotAssignmentMode::Named,
                delegates_focus: true,
                clonable: true,
                serializable: true,
                declarative: true,
                ..ShadowOptions::new(mode)
            };
            let root = document.attach_shadow_with_options(host, options).unwrap();
            let content =
                html::parse_fragment(&mut document, "<b>retained child</b><i></i>").unwrap();
            document.append(root, content).unwrap();
            let first = document.first_child(root).unwrap().unwrap();
            let second = document.next_sibling(first).unwrap().unwrap();
            let other_mode = if mode == ShadowMode::Open {
                ShadowMode::Closed
            } else {
                ShadowMode::Open
            };
            assert_eq!(
                document.attach_shadow(host, other_mode),
                Err(Error::Hierarchy)
            );
            assert_eq!(document.first_child(root), Ok(Some(first)));
            let live_nodes = document.node_count();
            assert_eq!(document.attach_shadow(host, mode), Ok(root));
            assert_eq!(document.shadow_root(host), Ok(Some(root)));
            assert_eq!(document.first_child(root), Ok(None));
            assert_eq!(document.parent(first), Ok(None));
            assert_eq!(document.parent(second), Ok(None));
            assert_eq!(document.node_count(), live_nodes);
            assert_eq!(
                document.shadow_options(root),
                Ok(Some(ShadowOptions {
                    declarative: false,
                    ..options
                }))
            );
            assert_eq!(document.attach_shadow(host, mode), Err(Error::Hierarchy));
            assert!(
                document.kind(first).is_ok(),
                "retained child remains a live detached node"
            );
        }
    }

    #[test]
    fn event_parent_and_retarget_observe_shadow_boundaries() {
        let mut document = html::parse("<div><b></b></div>", 32).unwrap();
        let host = find(&document, document.root(), "div");
        let light = find(&document, host, "b");
        let root = document.attach_shadow(host, ShadowMode::Open).unwrap();
        let slot = document
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: "slot".into(),
                attributes: Vec::new(),
            })
            .unwrap();
        document.append(root, slot).unwrap();
        assert_eq!(
            document.event_parent(light, false, document.root()),
            Ok(Some(slot))
        );
        assert_eq!(
            document.event_parent(root, false, document.root()),
            Ok(Some(host))
        );
        assert_eq!(document.event_parent(root, false, root), Ok(None));
        assert_eq!(document.event_parent(root, true, root), Ok(Some(host)));
        assert_eq!(document.retarget(slot, Some(host)), Ok(host));
        assert_eq!(document.retarget(slot, Some(slot)), Ok(slot));
        assert_eq!(document.retarget(light, Some(slot)), Ok(light));
    }
}
