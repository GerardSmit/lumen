//! Host-independent HTML document core. Parsing, style, layout, and paint derive from this tree.
#![no_std]

extern crate alloc;

pub mod css;
mod entities;
pub mod html;
pub mod layout;
pub mod paint;
pub mod selector;
pub mod session;
pub mod observe;

use alloc::{rc::Rc, string::String, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

static NEXT_DOCUMENT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct NodeId {
    document: u64,
    index: u32,
    generation: u32,
}

impl NodeId {
    pub const fn index(self) -> usize {
        self.index as usize
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Namespace {
    Html,
    Svg,
    MathMl,
    Other(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NodeKind {
    Document,
    DocumentFragment,
    DocumentType(String),
    Element {
        namespace: Namespace,
        name: String,
        attributes: Vec<(String, String)>,
    },
    Text(String),
    Comment(String),
    ProcessingInstruction {
        target: String,
        data: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Dirty(u8);

impl Dirty {
    pub const NONE: Self = Self(0);
    pub const PAINT: Self = Self(1);
    pub const LAYOUT: Self = Self(1 | 2);
    pub const STYLE: Self = Self(1 | 2 | 4);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidNode,
    Hierarchy,
    UnsupportedDoctype,
    LimitExceeded,
    WrongKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MutationKind {
    Tree {
        added: Option<NodeId>,
        removed: Option<NodeId>,
        styles_changed: bool,
    },
    Attribute(String),
    CharacterData,
    FullRebuild,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mutation {
    pub version: u64,
    pub target: NodeId,
    pub kind: MutationKind,
}

const MAX_JOURNAL: usize = 1024;
const NO_LINK: u32 = u32::MAX;

#[derive(Debug)]
struct Node {
    generation: u32,
    alive: bool,
    kind: NodeKind,
    parent: u32,
    first_child: u32,
    last_child: u32,
    prev_sibling: u32,
    next_sibling: u32,
    template_content: u32,
    dirty: Dirty,
}

enum InsertingNodes {
    One([NodeId; 1]),
    Fragment(Vec<NodeId>),
}

impl InsertingNodes {
    fn as_slice(&self) -> &[NodeId] {
        match self {
            Self::One(node) => node,
            Self::Fragment(nodes) => nodes,
        }
    }
}

/// Removed nodes retain their identity until explicitly destroyed.
pub struct Document {
    id: u64,
    nodes: Vec<Node>,
    free: Vec<u32>,
    live_nodes: usize,
    version: u64,
    max_nodes: usize,
    journal: Vec<Mutation>,
    mutation_sink: Option<Rc<dyn Fn(&Document, &observe::ObservedMutation)>>,
}

impl Document {
    pub fn new(max_nodes: usize) -> Self {
        Self {
            id: NEXT_DOCUMENT_ID.fetch_add(1, Ordering::Relaxed),
            nodes: alloc::vec![Node {
                generation: 1,
                alive: true,
                kind: NodeKind::Document,
                parent: NO_LINK,
                first_child: NO_LINK,
                last_child: NO_LINK,
                prev_sibling: NO_LINK,
                next_sibling: NO_LINK,
                template_content: NO_LINK,
                dirty: Dirty::STYLE,
            }],
            free: Vec::new(),
            live_nodes: 1,
            version: 0,
            max_nodes: max_nodes.max(1),
            journal: Vec::new(),
            mutation_sink: None,
        }
    }

    pub const fn root(&self) -> NodeId {
        NodeId {
            document: self.id,
            index: 0,
            generation: 1,
        }
    }

    pub const fn version(&self) -> u64 {
        self.version
    }

    pub fn node_count(&self) -> usize {
        self.live_nodes
    }

    pub fn set_mutation_sink(&mut self, sink: Option<Rc<dyn Fn(&Document, &observe::ObservedMutation)>>) { self.mutation_sink = sink; }

    fn notify(&self, mutation: observe::ObservedMutation) {
        if let Some(sink) = &self.mutation_sink { sink(self, &mutation); }
    }

    pub fn drain_mutations(&mut self) -> Vec<Mutation> {
        core::mem::take(&mut self.journal)
    }

    pub fn mutations(&self) -> &[Mutation] {
        &self.journal
    }

    pub fn clear_mutations(&mut self) {
        self.journal.clear();
    }

    pub fn create(&mut self, kind: NodeKind) -> Result<NodeId, Error> {
        let template = matches!(&kind, NodeKind::Element { namespace: Namespace::Html, name, .. } if name == "template");
        if template && self.max_nodes.saturating_sub(self.live_nodes) < 2 {
            return Err(Error::LimitExceeded);
        }
        if self.live_nodes >= self.max_nodes {
            return Err(Error::LimitExceeded);
        }
        if matches!(kind, NodeKind::Document) {
            return Err(Error::Hierarchy);
        }
        if let Some(index) = self.free.pop() {
            let generation = self.nodes[index as usize].generation;
            self.nodes[index as usize] = Node {
                generation,
                alive: true,
                kind,
                parent: NO_LINK,
                first_child: NO_LINK,
                last_child: NO_LINK,
                prev_sibling: NO_LINK,
                next_sibling: NO_LINK,
                template_content: NO_LINK,
                dirty: Dirty::STYLE,
            };
            self.live_nodes += 1;
            let id = NodeId {
                document: self.id,
                index,
                generation,
            };
            if template { self.create_template_content(id)?; }
            return Ok(id);
        }
        if self.nodes.len() >= u32::MAX as usize {
            return Err(Error::LimitExceeded);
        }
        let id = NodeId {
            document: self.id,
            index: self.nodes.len() as u32,
            generation: 1,
        };
        self.nodes.push(Node {
            generation: 1,
            alive: true,
            kind,
            parent: NO_LINK,
            first_child: NO_LINK,
            last_child: NO_LINK,
            prev_sibling: NO_LINK,
            next_sibling: NO_LINK,
            template_content: NO_LINK,
            dirty: Dirty::STYLE,
        });
        self.live_nodes += 1;
        if template { self.create_template_content(id)?; }
        Ok(id)
    }

    fn create_template_content(&mut self, host: NodeId) -> Result<(), Error> {
        let content = self.create(NodeKind::DocumentFragment)?;
        self.node_mut(host).template_content = content.index;
        self.node_mut(content).template_content = host.index;
        Ok(())
    }

    /// HTML template contents are detached from the ordinary child tree.
    pub fn template_content(&self, id: NodeId) -> Result<Option<NodeId>, Error> {
        if !matches!(self.kind(id)?, NodeKind::Element { namespace: Namespace::Html, name, .. } if name == "template") { return Ok(None); }
        let index = self.nodes[id.index()].template_content;
        Ok((index != NO_LINK).then(|| NodeId { document: self.id, index, generation: self.nodes[index as usize].generation }))
    }

    fn host_including_parent(&self, id: NodeId) -> Result<Option<NodeId>, Error> {
        if matches!(self.kind(id)?, NodeKind::DocumentFragment) {
            let index = self.nodes[id.index()].template_content;
            if index != NO_LINK { return Ok(Some(NodeId { document: self.id, index, generation: self.nodes[index as usize].generation })); }
        }
        self.parent(id)
    }

    /// Reclaim a detached subtree; its old IDs become invalid.
    pub fn destroy_subtree(&mut self, root: NodeId) -> Result<(), Error> {
        if root == self.root() || self.parent(root)?.is_some() {
            return Err(Error::Hierarchy);
        }
        let mut pending = alloc::vec![root];
        while let Some(id) = pending.pop() {
            if let Some(content) = self.template_content(id)? { pending.push(content); }
            let mut child = self.first_child(id)?;
            while let Some(next) = child {
                pending.push(next);
                child = self.next_sibling(next)?;
            }
            let node = self.node_mut(id);
            node.kind = NodeKind::DocumentFragment;
            node.alive = false;
            let reusable = node.generation < u32::MAX;
            if reusable {
                node.generation += 1;
            }
            node.parent = NO_LINK;
            node.first_child = NO_LINK;
            node.last_child = NO_LINK;
            node.prev_sibling = NO_LINK;
            node.next_sibling = NO_LINK;
            node.template_content = NO_LINK;
            node.dirty = Dirty::NONE;
            if reusable {
                self.free.push(id.index);
            }
            self.live_nodes -= 1;
        }
        Ok(())
    }

    /// Clone a detached subtree. Only attaching the result invalidates the document.
    pub fn clone_subtree(&mut self, source: NodeId) -> Result<NodeId, Error> {
        if matches!(self.kind(source)?, NodeKind::Document) {
            return Err(Error::Hierarchy);
        }
        let mut pending = alloc::vec![source];
        let mut count = 0usize;
        while let Some(id) = pending.pop() {
            if let Some(content) = self.template_content(id)? { pending.push(content); }
            count = count.checked_add(1).ok_or(Error::LimitExceeded)?;
            let mut child = self.first_child(id)?;
            while let Some(next) = child {
                pending.push(next);
                child = self.next_sibling(next)?;
            }
        }
        if self
            .live_nodes
            .checked_add(count)
            .is_none_or(|total| total > self.max_nodes)
            || self
                .nodes
                .len()
                .checked_add(count.saturating_sub(self.free.len()))
                .is_none_or(|total| total > u32::MAX as usize)
        {
            return Err(Error::LimitExceeded);
        }
        let cloned = self.create(self.kind(source)?.clone())?;
        let mut pending = alloc::vec![(source, cloned)];
        while let Some((original, copy)) = pending.pop() {
            if let Some(content) = self.template_content(original)? {
                pending.push((content, self.template_content(copy)?.unwrap()));
            }
            let mut child = self.first_child(original)?;
            while let Some(next) = child {
                let child_copy = self.create(self.kind(next)?.clone())?;
                self.attach_detached(copy, child_copy);
                pending.push((next, child_copy));
                child = self.next_sibling(next)?;
            }
        }
        Ok(cloned)
    }

    fn attach_detached(&mut self, parent: NodeId, child: NodeId) {
        self.insert_detached_before(parent, child, None);
    }

    fn insert_detached_before(&mut self, parent: NodeId, child: NodeId, before: Option<NodeId>) {
        let previous = before.map_or(self.nodes[parent.index as usize].last_child, |id| {
            self.nodes[id.index()].prev_sibling
        });
        self.node_mut(child).parent = parent.index;
        self.node_mut(child).prev_sibling = previous;
        self.node_mut(child).next_sibling = Self::link_index(before);
        if previous == NO_LINK {
            self.node_mut(parent).first_child = child.index;
        } else {
            self.nodes[previous as usize].next_sibling = child.index;
        }
        if let Some(before) = before {
            self.node_mut(before).prev_sibling = child.index;
        } else {
            self.node_mut(parent).last_child = child.index;
        }
    }

    fn prepend_detached(&mut self, parent: NodeId, child: NodeId) {
        let first = self.link_id(self.nodes[parent.index as usize].first_child);
        self.insert_detached_before(parent, child, first);
    }

    fn node(&self, id: NodeId) -> Result<&Node, Error> {
        if id.document != self.id {
            return Err(Error::InvalidNode);
        }
        self.nodes
            .get(id.index as usize)
            .filter(|node| node.alive && node.generation == id.generation)
            .ok_or(Error::InvalidNode)
    }

    fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id.index as usize]
    }

    fn link_id(&self, index: u32) -> Option<NodeId> {
        if index == NO_LINK {
            None
        } else {
            Some(NodeId {
                document: self.id,
                index,
                generation: self.nodes[index as usize].generation,
            })
        }
    }

    fn link_index(id: Option<NodeId>) -> u32 {
        id.map_or(NO_LINK, |id| id.index)
    }

    pub fn kind(&self, id: NodeId) -> Result<&NodeKind, Error> {
        Ok(&self.node(id)?.kind)
    }

    pub fn parent(&self, id: NodeId) -> Result<Option<NodeId>, Error> {
        Ok(self.link_id(self.node(id)?.parent))
    }

    pub fn first_child(&self, id: NodeId) -> Result<Option<NodeId>, Error> {
        Ok(self.link_id(self.node(id)?.first_child))
    }

    pub fn last_child(&self, id: NodeId) -> Result<Option<NodeId>, Error> {
        Ok(self.link_id(self.node(id)?.last_child))
    }

    pub fn previous_sibling(&self, id: NodeId) -> Result<Option<NodeId>, Error> {
        Ok(self.link_id(self.node(id)?.prev_sibling))
    }

    pub fn next_sibling(&self, id: NodeId) -> Result<Option<NodeId>, Error> {
        Ok(self.link_id(self.node(id)?.next_sibling))
    }

    pub fn dirty(&self, id: NodeId) -> Result<Dirty, Error> {
        Ok(self.node(id)?.dirty)
    }

    pub fn clear_dirty(&mut self, id: NodeId) -> Result<(), Error> {
        self.node(id)?;
        self.node_mut(id).dirty = Dirty::NONE;
        Ok(())
    }

    fn mark_dirty(&mut self, mut id: NodeId, level: Dirty, kind: MutationKind) {
        let target = id;
        loop {
            let parent_index = {
                let node = self.node_mut(id);
                node.dirty.0 |= level.0;
                node.parent
            };
            if let Some(parent) = self.link_id(parent_index) {
                id = parent;
            } else {
                break;
            }
        }
        self.version = self.version.wrapping_add(1);
        if matches!(
            self.journal.first(),
            Some(Mutation {
                kind: MutationKind::FullRebuild,
                ..
            })
        ) {
            self.journal[0].version = self.version;
        } else if self.journal.len() == MAX_JOURNAL {
            self.journal.clear();
            self.journal.push(Mutation {
                version: self.version,
                target: self.root(),
                kind: MutationKind::FullRebuild,
            });
        } else {
            self.journal.push(Mutation {
                version: self.version,
                target,
                kind,
            });
        }
    }

    pub fn append(&mut self, parent: NodeId, child: NodeId) -> Result<(), Error> {
        self.insert_before(parent, child, None)
    }

    pub fn append_many(&mut self, parent: NodeId, nodes: &[NodeId]) -> Result<(), Error> {
        self.insert_many_before(parent, nodes, None)
    }

    pub fn insert_many_before(
        &mut self,
        parent: NodeId,
        nodes: &[NodeId],
        mut before: Option<NodeId>,
    ) -> Result<(), Error> {
        let inserting = self.prepare_many(parent, nodes)?;
        if let Some(reference) = before {
            if self.parent(reference)? != Some(parent) {
                return Err(Error::Hierarchy);
            }
            while let Some(reference) = before {
                if !inserting.contains(&reference) {
                    break;
                }
                before = self.next_sibling(reference)?;
            }
        }
        self.validate_insertion(parent, &inserting, before, None)?;
        for candidate in inserting {
            self.insert_validated(parent, candidate, before)?;
        }
        Ok(())
    }

    fn prepare_many(&self, parent: NodeId, nodes: &[NodeId]) -> Result<Vec<NodeId>, Error> {
        if !matches!(
            self.kind(parent)?,
            NodeKind::Document | NodeKind::DocumentFragment | NodeKind::Element { .. }
        ) {
            return Err(Error::Hierarchy);
        }
        let mut inserting = Vec::new();
        for &node in nodes {
            if node == parent {
                return Err(Error::Hierarchy);
            }
            for &candidate in self.inserting_nodes(node)?.as_slice() {
                let mut ancestor = Some(parent);
                while let Some(id) = ancestor {
                    if candidate == id {
                        return Err(Error::Hierarchy);
                    }
            ancestor = self.host_including_parent(id)?;
                }
                if let Some(index) = inserting.iter().position(|&id| id == candidate) {
                    inserting.remove(index);
                }
                inserting.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
                inserting.push(candidate);
            }
        }
        Ok(inserting)
    }

    pub fn insert_before(
        &mut self,
        parent: NodeId,
        child: NodeId,
        before: Option<NodeId>,
    ) -> Result<(), Error> {
        let parent_kind = self.kind(parent)?;
        if !matches!(
            parent_kind,
            NodeKind::Document | NodeKind::DocumentFragment | NodeKind::Element { .. }
        ) {
            return Err(Error::Hierarchy);
        }
        let inserting = self.inserting_nodes(child)?;
        let inserting = inserting.as_slice();
        if let Some(before) = before {
            if self.parent(before)? != Some(parent) {
                return Err(Error::Hierarchy);
            }
            if before == child {
                return Ok(());
            }
        }
        for &candidate in inserting {
            let mut ancestor = Some(parent);
            while let Some(id) = ancestor {
                if id == candidate {
                    return Err(Error::Hierarchy);
                }
            ancestor = self.host_including_parent(id)?;
            }
        }
        self.validate_insertion(parent, &inserting, before, None)?;
        for &candidate in inserting {
            self.insert_validated(parent, candidate, before)?;
        }
        Ok(())
    }

    fn inserting_nodes(&self, child: NodeId) -> Result<InsertingNodes, Error> {
        match self.kind(child)? {
            NodeKind::Document => Err(Error::Hierarchy),
            NodeKind::DocumentFragment => {
                let mut children = Vec::new();
                let mut current = self.first_child(child)?;
                while let Some(id) = current {
                    children.push(id);
                    current = self.next_sibling(id)?;
                }
                Ok(InsertingNodes::Fragment(children))
            }
            _ => Ok(InsertingNodes::One([child])),
        }
    }

    fn insert_validated(
        &mut self,
        parent: NodeId,
        child: NodeId,
        before: Option<NodeId>,
    ) -> Result<(), Error> {
        if self.parent(child)?.is_some() {
            self.remove(child)?;
        }
        let prev = if let Some(before) = before {
            self.previous_sibling(before)?
        } else {
            self.last_child(parent)?
        };
        self.node_mut(child).parent = parent.index;
        self.node_mut(child).prev_sibling = Self::link_index(prev);
        self.node_mut(child).next_sibling = Self::link_index(before);
        self.node_mut(child).dirty.0 |= Dirty::STYLE.0;
        if let Some(prev) = prev {
            self.node_mut(prev).next_sibling = child.index;
        } else {
            self.node_mut(parent).first_child = child.index;
        }
        if let Some(before) = before {
            self.node_mut(before).prev_sibling = child.index;
        } else {
            self.node_mut(parent).last_child = child.index;
        }
        self.mark_dirty(
            parent,
            Dirty::STYLE,
            MutationKind::Tree {
                added: Some(child),
                removed: None,
                styles_changed: false,
            },
        );
        if self.mutation_sink.is_some() { self.notify(observe::ObservedMutation { target: parent, kind: observe::ObservedKind::ChildList { added: Some(child), removed: None, previous_sibling: prev, next_sibling: before } }); }
        Ok(())
    }

    fn validate_insertion(
        &self,
        parent: NodeId,
        inserting: &[NodeId],
        before: Option<NodeId>,
        replaced: Option<NodeId>,
    ) -> Result<(), Error> {
        if !matches!(self.kind(parent)?, NodeKind::Document) {
            return if inserting
                .iter()
                .any(|&id| matches!(self.kind(id), Ok(NodeKind::DocumentType(_))))
            {
                Err(Error::Hierarchy)
            } else {
                Ok(())
            };
        }
        let mut children = Vec::new();
        let mut current = self.first_child(parent)?;
        while let Some(id) = current {
            if !inserting.contains(&id) && Some(id) != replaced {
                children.push(id);
            }
            current = self.next_sibling(id)?;
        }
        let index = if let Some(before) = before {
            children
                .iter()
                .position(|&id| id == before)
                .ok_or(Error::Hierarchy)?
        } else {
            children.len()
        };
        children.splice(index..index, inserting.iter().copied());
        self.validate_document_children(&children)
    }

    fn validate_document_children(&self, children: &[NodeId]) -> Result<(), Error> {
        let mut element_index = None;
        let mut doctype_index = None;
        for (index, &id) in children.iter().enumerate() {
            match self.kind(id)? {
                NodeKind::Element { .. } => {
                    if element_index.replace(index).is_some() {
                        return Err(Error::Hierarchy);
                    }
                }
                NodeKind::DocumentType(name) => {
                    if !name.eq_ignore_ascii_case("html") {
                        return Err(Error::UnsupportedDoctype);
                    }
                    if doctype_index.replace(index).is_some() {
                        return Err(Error::Hierarchy);
                    }
                }
                NodeKind::Comment(_) | NodeKind::ProcessingInstruction { .. } => {}
                _ => return Err(Error::Hierarchy),
            }
        }
        if matches!((doctype_index, element_index), (Some(doctype), Some(element)) if doctype > element)
        {
            return Err(Error::Hierarchy);
        }
        Ok(())
    }

    pub fn remove(&mut self, child: NodeId) -> Result<(), Error> {
        let siblings = if self.mutation_sink.is_some() { (self.previous_sibling(child)?, self.next_sibling(child)?) } else { (None, None) };
        if let Some(parent) = self.detach(child)? {
            self.mark_dirty(
                parent,
                Dirty::STYLE,
                MutationKind::Tree {
                    added: None,
                    removed: Some(child),
                    styles_changed: false,
                },
            );
            if self.mutation_sink.is_some() { self.notify(observe::ObservedMutation { target: parent, kind: observe::ObservedKind::ChildList { added: None, removed: Some(child), previous_sibling: siblings.0, next_sibling: siblings.1 } }); }
        }
        Ok(())
    }

    fn detach(&mut self, child: NodeId) -> Result<Option<NodeId>, Error> {
        let node = self.node(child)?;
        let Some(parent) = self.link_id(node.parent) else {
            return Ok(None);
        };
        let (prev, next) = (
            self.link_id(node.prev_sibling),
            self.link_id(node.next_sibling),
        );
        if let Some(prev) = prev {
            self.node_mut(prev).next_sibling = Self::link_index(next);
        } else {
            self.node_mut(parent).first_child = Self::link_index(next);
        }
        if let Some(next) = next {
            self.node_mut(next).prev_sibling = Self::link_index(prev);
        } else {
            self.node_mut(parent).last_child = Self::link_index(prev);
        }
        let node = self.node_mut(child);
        node.parent = NO_LINK;
        node.prev_sibling = NO_LINK;
        node.next_sibling = NO_LINK;
        Ok(Some(parent))
    }

    pub fn replace(&mut self, old: NodeId, new: NodeId) -> Result<(), Error> {
        let parent = self.parent(old)?.ok_or(Error::Hierarchy)?;
        if old == new {
            return Ok(());
        }
        let inserting = self.inserting_nodes(new)?;
        let inserting = inserting.as_slice();
        for &candidate in inserting {
            let mut ancestor = Some(parent);
            while let Some(id) = ancestor {
                if id == candidate {
                    return Err(Error::Hierarchy);
                }
                ancestor = self.parent(id)?;
            }
        }
        let mut before = self.next_sibling(old)?;
        while before.is_some_and(|id| inserting.contains(&id)) {
            before = self.next_sibling(before.unwrap())?;
        }
        self.validate_insertion(parent, &inserting, before, Some(old))?;
        self.remove(old)?;
        for &candidate in inserting {
            self.insert_validated(parent, candidate, before)?;
        }
        Ok(())
    }

    /// Replace an element or fragment's children as one update.
    pub fn replace_children(&mut self, parent: NodeId, replacement: NodeId) -> Result<(), Error> {
        self.replace_children_many(parent, &[replacement])
    }

    pub fn replace_children_many(&mut self, parent: NodeId, nodes: &[NodeId]) -> Result<(), Error> {
        let inserting = self.prepare_many(parent, nodes)?;
        if matches!(self.kind(parent)?, NodeKind::Document) {
            self.validate_document_children(&inserting)?;
        } else {
            self.validate_insertion(parent, &inserting, None, None)?;
        }
        let mut styles_changed = inserting
            .iter()
            .any(|&id| session::subtree_has_style(self, id));
        let mut old_child = self.first_child(parent)?;
        while let Some(id) = old_child {
            styles_changed |= session::subtree_has_style(self, id);
            old_child = self.next_sibling(id)?;
        }
        for &node in nodes {
            if matches!(self.kind(node)?, NodeKind::DocumentFragment) {
                self.detach_all_children(node);
            }
        }
        for &candidate in &inserting {
            if self
                .parent(candidate)?
                .is_some_and(|old_parent| old_parent != parent)
            {
                self.remove(candidate)?;
            }
        }
        self.detach_all_children(parent);
        for &candidate in &inserting {
            self.attach_detached(parent, candidate);
        }
        self.mark_dirty(
            parent,
            Dirty::STYLE,
            MutationKind::Tree {
                added: None,
                removed: None,
                styles_changed,
            },
        );
        Ok(())
    }

    fn detach_all_children(&mut self, parent: NodeId) {
        let mut child = self.nodes[parent.index as usize].first_child;
        self.node_mut(parent).first_child = NO_LINK;
        self.node_mut(parent).last_child = NO_LINK;
        while child != NO_LINK {
            let next = self.nodes[child as usize].next_sibling;
            let node = &mut self.nodes[child as usize];
            node.parent = NO_LINK;
            node.prev_sibling = NO_LINK;
            node.next_sibling = NO_LINK;
            child = next;
        }
    }

    pub fn set_attribute(&mut self, id: NodeId, name: &str, value: &str) -> Result<(), Error> {
        let old_value = if self.mutation_sink.is_some() { match self.kind(id)? { NodeKind::Element { attributes, .. } => attributes.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone()), _ => None } } else { None };
        let NodeKind::Element { attributes, .. } = &mut self.node_mut_checked(id)?.kind else {
            return Err(Error::WrongKind);
        };
        if let Some((_, old)) = attributes.iter_mut().find(|(key, _)| key == name) {
            if old == value {
                return Ok(());
            }
            *old = String::from(value);
        } else {
            attributes.push((String::from(name), String::from(value)));
        }
        self.mark_dirty(
            id,
            Dirty::STYLE,
            MutationKind::Attribute(String::from(name)),
        );
        if self.mutation_sink.is_some() { self.notify(observe::ObservedMutation { target: id, kind: observe::ObservedKind::Attribute { name: String::from(name), old_value } }); }
        Ok(())
    }

    pub fn remove_attribute(&mut self, id: NodeId, name: &str) -> Result<(), Error> {
        let NodeKind::Element { attributes, .. } = &mut self.node_mut_checked(id)?.kind else {
            return Err(Error::WrongKind);
        };
        if let Some(index) = attributes.iter().position(|(key, _)| key == name) {
            let (_, old_value) = attributes.remove(index);
            self.mark_dirty(
                id,
                Dirty::STYLE,
                MutationKind::Attribute(String::from(name)),
            );
            if self.mutation_sink.is_some() { self.notify(observe::ObservedMutation { target: id, kind: observe::ObservedKind::Attribute { name: String::from(name), old_value: Some(old_value) } }); }
        }
        Ok(())
    }

    pub fn replace_data(&mut self, id: NodeId, data: &str) -> Result<(), Error> {
        let observing = self.mutation_sink.is_some();
        let kind = &mut self.node_mut_checked(id)?.kind;
        let text = match kind {
            NodeKind::Text(text) | NodeKind::Comment(text) => text,
            NodeKind::ProcessingInstruction { data, .. } => data,
            _ => return Err(Error::WrongKind),
        };
        if text != data {
            let old_value = observing.then(|| text.clone());
            *text = String::from(data);
            self.mark_dirty(id, Dirty::STYLE, MutationKind::CharacterData);
            if let Some(old_value) = old_value { self.notify(observe::ObservedMutation { target: id, kind: observe::ObservedKind::CharacterData { old_value } }); }
        }
        Ok(())
    }

    fn node_mut_checked(&mut self, id: NodeId) -> Result<&mut Node, Error> {
        self.node(id)?;
        Ok(self.node_mut(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(name: &str) -> NodeKind {
        NodeKind::Element {
            namespace: Namespace::Html,
            name: String::from(name),
            attributes: Vec::new(),
        }
    }

    #[test]
    fn mutation_preserves_identity_and_invalidates_ancestors() {
        let mut doc = Document::new(5);
        let root = doc.root();
        let host = doc.create(element("host")).unwrap();
        let a = doc.create(element("a")).unwrap();
        let b = doc.create(element("b")).unwrap();
        let text = doc.create(NodeKind::Text(String::from("before"))).unwrap();
        doc.append(root, host).unwrap();
        doc.append(host, a).unwrap();
        doc.append(a, text).unwrap();
        doc.clear_dirty(root).unwrap();
        doc.clear_dirty(a).unwrap();
        let version = doc.version();
        doc.replace_data(text, "after").unwrap();
        assert!(doc.version() > version);
        assert_eq!(
            doc.drain_mutations().last().unwrap().kind,
            MutationKind::CharacterData
        );
        assert!(doc.dirty(a).unwrap().contains(Dirty::LAYOUT));
        assert!(doc.dirty(root).unwrap().contains(Dirty::LAYOUT));
        doc.insert_before(host, b, Some(a)).unwrap();
        assert_eq!(doc.first_child(host).unwrap(), Some(b));
        assert_eq!(doc.next_sibling(b).unwrap(), Some(a));
        doc.remove(b).unwrap();
        assert_eq!(doc.parent(b).unwrap(), None);
        doc.append(a, b).unwrap();
        assert_eq!(doc.parent(b).unwrap(), Some(a));
        assert_eq!(doc.append(b, a), Err(Error::Hierarchy));
        let other = Document::new(2);
        assert_eq!(doc.append(a, other.root()), Err(Error::InvalidNode));
    }

    #[test]
    fn variadic_tree_mutations_validate_before_moves_and_keep_last_duplicate() {
        let mut doc = Document::new(12);
        let root = doc.root();
        let parent = doc.create(element("parent")).unwrap();
        let a = doc.create(element("a")).unwrap();
        let b = doc.create(element("b")).unwrap();
        doc.append(root, parent).unwrap();
        doc.append(parent, a).unwrap();
        let version = doc.version();
        assert_eq!(doc.append_many(a, &[b, parent]), Err(Error::Hierarchy));
        assert_eq!(doc.parent(b).unwrap(), None);
        assert_eq!(doc.parent(parent).unwrap(), Some(root));
        assert_eq!(doc.version(), version);
        assert_eq!(
            doc.replace_children_many(a, &[b, parent]),
            Err(Error::Hierarchy)
        );
        assert_eq!(doc.version(), version);
        doc.append_many(parent, &[a, b, a]).unwrap();
        assert_eq!(doc.first_child(parent).unwrap(), Some(b));
        assert_eq!(doc.next_sibling(b).unwrap(), Some(a));
        let fragment = doc.create(NodeKind::DocumentFragment).unwrap();
        let c = doc.create(element("c")).unwrap();
        doc.append(fragment, c).unwrap();
        doc.replace_children_many(parent, &[fragment, a]).unwrap();
        assert_eq!(doc.first_child(fragment).unwrap(), None);
        assert_eq!(doc.first_child(parent).unwrap(), Some(c));
        assert_eq!(doc.next_sibling(c).unwrap(), Some(a));
        assert_eq!(doc.parent(b).unwrap(), None);
        doc.replace_children_many(parent, &[]).unwrap();
        assert_eq!(doc.first_child(parent).unwrap(), None);
    }

    #[test]
    fn journal_overflow_requests_full_rebuild() {
        let mut doc = Document::new(2);
        let text = doc.create(NodeKind::Text(String::new())).unwrap();
        for n in 0..=MAX_JOURNAL {
            let value = alloc::format!("{n}");
            doc.replace_data(text, &value).unwrap();
        }
        assert_eq!(
            doc.drain_mutations(),
            alloc::vec![Mutation {
                version: doc.version(),
                target: doc.root(),
                kind: MutationKind::FullRebuild,
            }]
        );
    }

    #[test]
    fn document_hierarchy_is_validated_before_mutation() {
        let mut doc = Document::new(8);
        let root = doc.root();
        let html = doc.create(element("html")).unwrap();
        let other = doc.create(element("other")).unwrap();
        let doctype = doc
            .create(NodeKind::DocumentType(String::from("html")))
            .unwrap();
        let text = doc.create(NodeKind::Text(String::from("body"))).unwrap();
        doc.append(root, html).unwrap();
        let version = doc.version();
        assert_eq!(doc.append(root, other), Err(Error::Hierarchy));
        assert_eq!(doc.append(root, text), Err(Error::Hierarchy));
        assert_eq!(doc.append(root, doctype), Err(Error::Hierarchy));
        assert_eq!(doc.version(), version);
        doc.insert_before(root, doctype, Some(html)).unwrap();
        doc.replace(html, other).unwrap();
        assert_eq!(doc.parent(html).unwrap(), None);
        assert_eq!(doc.next_sibling(doctype).unwrap(), Some(other));
    }

    #[test]
    fn subtree_clone_is_detached_and_keeps_child_order() {
        let mut doc = Document::new(10);
        let host = doc.create(element("div")).unwrap();
        let first = doc.create(NodeKind::Text("a".into())).unwrap();
        let second = doc.create(NodeKind::Text("b".into())).unwrap();
        doc.append(host, first).unwrap();
        doc.append(host, second).unwrap();
        doc.drain_mutations();
        let copy = doc.clone_subtree(host).unwrap();
        assert_eq!(doc.parent(copy).unwrap(), None);
        assert!(doc.drain_mutations().is_empty());
        let a = doc.first_child(copy).unwrap().unwrap();
        let b = doc.next_sibling(a).unwrap().unwrap();
        assert_eq!(doc.kind(a).unwrap(), doc.kind(first).unwrap());
        assert_eq!(doc.kind(b).unwrap(), doc.kind(second).unwrap());
        assert_ne!(a, first);
        doc.append(doc.root(), copy).unwrap();
        assert_eq!(doc.drain_mutations().len(), 1);
    }

    #[test]
    fn arena_node_stays_compact() {
        assert!(core::mem::size_of::<Node>() <= 104);
    }

    #[test]
    fn replace_children_moves_fragment_in_one_mutation() {
        let mut doc = Document::new(10);
        let parent = doc.create(element("div")).unwrap();
        let old = doc.create(element("old")).unwrap();
        let fragment = doc.create(NodeKind::DocumentFragment).unwrap();
        let a = doc.create(element("a")).unwrap();
        let b = doc.create(element("b")).unwrap();
        doc.append(parent, old).unwrap();
        doc.append(fragment, a).unwrap();
        doc.append(fragment, b).unwrap();
        doc.drain_mutations();
        doc.replace_children(parent, fragment).unwrap();
        assert_eq!(doc.first_child(parent).unwrap(), Some(a));
        assert_eq!(doc.next_sibling(a).unwrap(), Some(b));
        assert_eq!(doc.parent(old).unwrap(), None);
        assert_eq!(doc.first_child(fragment).unwrap(), None);
        assert_eq!(doc.drain_mutations().len(), 1);
    }

    #[test]
    fn detached_subtree_slots_reuse_with_new_generations() {
        let mut doc = Document::new(3);
        let old = doc.create(element("old")).unwrap();
        let text = doc.create(NodeKind::Text("data".into())).unwrap();
        doc.append(old, text).unwrap();
        assert_eq!(doc.create(element("overflow")), Err(Error::LimitExceeded));
        doc.destroy_subtree(old).unwrap();
        assert_eq!(doc.node_count(), 1);
        assert_eq!(doc.kind(old), Err(Error::InvalidNode));
        assert_eq!(doc.kind(text), Err(Error::InvalidNode));
        let fresh = doc.create(element("fresh")).unwrap();
        assert_eq!(fresh.index(), text.index());
        assert_ne!(fresh, text);
        assert_eq!(doc.destroy_subtree(doc.root()), Err(Error::Hierarchy));
    }

    #[test]
    fn repeated_replacement_keeps_arena_bounded() {
        let mut doc = Document::new(4);
        let host = doc.create(element("host")).unwrap();
        doc.append(doc.root(), host).unwrap();
        for _ in 0..1000 {
            let item = doc.create(element("item")).unwrap();
            doc.append(host, item).unwrap();
            doc.remove(item).unwrap();
            doc.destroy_subtree(item).unwrap();
        }
        assert_eq!(doc.node_count(), 2);
        assert_eq!(doc.nodes.len(), 3);
    }
}
