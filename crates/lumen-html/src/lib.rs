//! Host-independent HTML document core. Parsing, style, layout, and paint derive from this tree.
#![no_std]

extern crate alloc;

pub mod animation;
pub mod css;
pub mod debug;
pub mod font_display;
pub mod forms;
pub mod html;
pub mod layout;
mod name;
mod named_colors;
pub mod observe;
pub mod paint;
pub mod selector;
pub mod session;
pub mod shadow;
pub mod svg;
pub mod xml;
pub use name::Name;
pub use shadow::ShadowMode;

use alloc::{rc::Rc, string::String, vec, vec::Vec};
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
    Other(Rc<str>),
}

/// The HTML parsing mode selected by the document's doctype.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentMode {
    NoQuirks,
    LimitedQuirks,
    Quirks,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NodeKind {
    Document,
    DocumentFragment,
    DocumentType(String),
    Element {
        namespace: Namespace,
        name: Name,
        attributes: Vec<(Name, String)>,
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
    IndexSize,
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
    Attribute(Name),
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

impl Node {
    fn new(kind: NodeKind, generation: u32) -> Self {
        Self {
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
        }
    }
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
    attribute_namespaces: Vec<(NodeId, Vec<(usize, Rc<str>)>)>,
    doctype_identifiers: Vec<(NodeId, String, String)>,
    document_mode: DocumentMode,
    is_html_document: bool,
    free: Vec<u32>,
    live_nodes: usize,
    version: u64,
    max_nodes: usize,
    journal: Vec<Mutation>,
    mutation_sink: Option<Rc<dyn Fn(&Document, &observe::ObservedMutation)>>,
    shadow_mutation_sink: Option<Rc<dyn Fn(&Document, &observe::ObservedMutation)>>,
    shadow_trees: Vec<shadow::ShadowTree>,
}

impl Document {
    pub fn new(max_nodes: usize) -> Self {
        Self {
            id: NEXT_DOCUMENT_ID.fetch_add(1, Ordering::Relaxed),
            nodes: alloc::vec![Node::new(NodeKind::Document, 1)],
            attribute_namespaces: Vec::new(),
            doctype_identifiers: Vec::new(),
            document_mode: DocumentMode::NoQuirks,
            is_html_document: false,
            free: Vec::new(),
            live_nodes: 1,
            version: 0,
            max_nodes: max_nodes.max(1),
            journal: Vec::new(),
            mutation_sink: None,
            shadow_mutation_sink: None,
            shadow_trees: Vec::new(),
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

    /// Return the document's current HTML parsing mode.
    pub const fn document_mode(&self) -> DocumentMode {
        self.document_mode
    }

    /// The legacy DOM compatibility string derived from the parsing mode.
    pub const fn compat_mode(&self) -> &'static str {
        match self.document_mode {
            DocumentMode::Quirks => "BackCompat",
            DocumentMode::NoQuirks | DocumentMode::LimitedQuirks => "CSS1Compat",
        }
    }

    /// Set the parsing mode while a parser is constructing the document.
    pub fn is_html_document(&self) -> bool {
        self.is_html_document
    }

    /// Set document kind at construction, independently of quirks mode.
    pub fn set_html_document(&mut self, is_html: bool) {
        self.is_html_document = is_html;
    }

    pub fn set_document_mode(&mut self, mode: DocumentMode) {
        self.document_mode = mode;
    }

    /// Return the document's doctype child, if one is present.
    pub fn doctype(&self) -> Result<Option<NodeId>, Error> {
        let mut child = self.first_child(self.root())?;
        while let Some(id) = child {
            if matches!(self.kind(id)?, NodeKind::DocumentType(_)) {
                return Ok(Some(id));
            }
            child = self.next_sibling(id)?;
        }
        Ok(None)
    }

    /// Return the public identifier for a doctype, or the DOM-specified empty string.
    pub fn doctype_public_id(&self, id: NodeId) -> Result<&str, Error> {
        if !matches!(self.kind(id)?, NodeKind::DocumentType(_)) {
            return Err(Error::WrongKind);
        }
        Ok(self
            .doctype_identifiers
            .iter()
            .find(|(owner, _, _)| *owner == id)
            .map_or("", |(_, public_id, _)| public_id.as_str()))
    }

    /// Return the system identifier for a doctype, or the DOM-specified empty string.
    pub fn doctype_system_id(&self, id: NodeId) -> Result<&str, Error> {
        if !matches!(self.kind(id)?, NodeKind::DocumentType(_)) {
            return Err(Error::WrongKind);
        }
        Ok(self
            .doctype_identifiers
            .iter()
            .find(|(owner, _, _)| *owner == id)
            .map_or("", |(_, _, system_id)| system_id.as_str()))
    }

    /// Preserve the identifiers emitted by an HTML or XML doctype token.
    pub fn set_doctype_identifiers(
        &mut self,
        id: NodeId,
        public_id: &str,
        system_id: &str,
    ) -> Result<(), Error> {
        if !matches!(self.kind(id)?, NodeKind::DocumentType(_)) {
            return Err(Error::WrongKind);
        }
        self.doctype_identifiers
            .retain(|(owner, _, _)| *owner != id);
        if !public_id.is_empty() || !system_id.is_empty() {
            self.doctype_identifiers
                .push((id, String::from(public_id), String::from(system_id)));
        }
        Ok(())
    }

    pub fn node_count(&self) -> usize {
        self.live_nodes
    }

    pub fn set_mutation_sink(
        &mut self,
        sink: Option<Rc<dyn Fn(&Document, &observe::ObservedMutation)>>,
    ) {
        self.mutation_sink = sink;
    }

    fn notify(&self, mutation: observe::ObservedMutation) {
        if !self.shadow_trees.is_empty() {
            if let Some(sink) = &self.shadow_mutation_sink {
                sink(self, &mutation);
            }
        }
        if let Some(sink) = &self.mutation_sink {
            sink(self, &mutation);
        }
    }

    /// Receives mutations affecting slot assignment without enabling observer
    /// record construction on documents with no shadow trees.
    pub fn set_shadow_mutation_sink(
        &mut self,
        sink: Option<Rc<dyn Fn(&Document, &observe::ObservedMutation)>>,
    ) {
        self.shadow_mutation_sink = sink;
    }

    fn observing_mutations(&self) -> bool {
        self.mutation_sink.is_some()
            || (!self.shadow_trees.is_empty() && self.shadow_mutation_sink.is_some())
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
            self.nodes[index as usize] = Node::new(kind, generation);
            self.live_nodes += 1;
            let id = NodeId {
                document: self.id,
                index,
                generation,
            };
            if template {
                self.create_template_content(id)?;
            }
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
        self.nodes.push(Node::new(kind, 1));
        self.live_nodes += 1;
        if template {
            self.create_template_content(id)?;
        }
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
        if !matches!(self.kind(id)?, NodeKind::Element { namespace: Namespace::Html, name, .. } if name == "template")
        {
            return Ok(None);
        }
        let index = self.nodes[id.index()].template_content;
        Ok((index != NO_LINK).then(|| NodeId {
            document: self.id,
            index,
            generation: self.nodes[index as usize].generation,
        }))
    }

    fn host_including_parent(&self, id: NodeId) -> Result<Option<NodeId>, Error> {
        if let Some(host) = self.shadow_host(id)? {
            return Ok(Some(host));
        }
        if matches!(self.kind(id)?, NodeKind::DocumentFragment) {
            let index = self.nodes[id.index()].template_content;
            if index != NO_LINK {
                return Ok(Some(NodeId {
                    document: self.id,
                    index,
                    generation: self.nodes[index as usize].generation,
                }));
            }
        }
        self.parent(id)
    }

    /// Reclaim a detached subtree; its old IDs become invalid.
    pub fn destroy_subtree(&mut self, root: NodeId) -> Result<(), Error> {
        if root == self.root() || self.parent(root)?.is_some() || self.shadow_host(root)?.is_some()
        {
            return Err(Error::Hierarchy);
        }
        let mut pending = alloc::vec![root];
        while let Some(id) = pending.pop() {
            if let Some(index) = self.shadow_trees.iter().position(|tree| tree.host == id) {
                pending.push(self.shadow_trees.remove(index).root);
            }
            if let Some(content) = self.template_content(id)? {
                pending.push(content);
            }
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
            self.attribute_namespaces.retain(|(owner, _)| *owner != id);
            self.doctype_identifiers
                .retain(|(owner, _, _)| *owner != id);
            self.live_nodes -= 1;
        }
        Ok(())
    }

    /// Copy one node and its attributes into a fresh detached node.
    pub fn clone_shallow(&mut self, source: NodeId) -> Result<NodeId, Error> {
        let kind = self.kind(source)?.clone();
        let namespaces = self
            .attribute_namespaces
            .iter()
            .find(|(owner, _)| *owner == source)
            .map(|(_, namespaces)| namespaces.clone())
            .unwrap_or_default();
        let doctype_identifiers = self
            .doctype_identifiers
            .iter()
            .find(|(owner, _, _)| *owner == source)
            .map(|(_, public_id, system_id)| (public_id.clone(), system_id.clone()));
        let clone = self.create(kind)?;
        if !namespaces.is_empty() {
            self.attribute_namespaces.push((clone, namespaces));
        }
        if let Some((public_id, system_id)) = doctype_identifiers {
            self.doctype_identifiers.push((clone, public_id, system_id));
        }
        Ok(clone)
    }

    /// Append descendant text in ordinary tree order without a traversal stack.
    /// Template contents and shadow trees are separate trees and are not included.
    pub fn append_descendant_text(&self, root: NodeId, out: &mut String) -> Result<(), Error> {
        let mut current = self.first_child(root)?;
        while let Some(node) = current {
            if let NodeKind::Text(text) = self.kind(node)? {
                out.push_str(text);
            }
            current = selector::next_descendant(self, root, node)?;
        }
        Ok(())
    }

    /// Clone a detached subtree. Only attaching the result invalidates the document.
    pub fn clone_subtree(&mut self, source: NodeId) -> Result<NodeId, Error> {
        self.clone_node(source, true)
    }

    /// Clone a node and its clonable shadow trees. `deep` controls ordinary and template
    /// children; a clonable shadow root's complete contents are copied even for shallow clones.
    pub fn clone_node(&mut self, source: NodeId, deep: bool) -> Result<NodeId, Error> {
        if matches!(self.kind(source)?, NodeKind::Document) || self.shadow_host(source)?.is_some() {
            return Err(Error::Hierarchy);
        }
        let count = self.clone_node_count(source, deep)?;
        self.ensure_clone_capacity(count)?;
        let cloned = self.clone_shallow(source)?;
        let mut pending = alloc::vec![(source, cloned, deep)];
        while let Some((original, copy, children)) = pending.pop() {
            if let Some(content) = self.template_content(original)? {
                pending.push((content, self.template_content(copy)?.unwrap(), children));
            }
            if let Some(root) = self.shadow_root(original)? {
                let options = self.shadow_options(root)?.ok_or(Error::WrongKind)?;
                if options.clonable {
                    let root_copy = self.attach_shadow_with_options(copy, options)?;
                    pending.push((root, root_copy, true));
                }
            }
            if children {
                let mut child = self.first_child(original)?;
                while let Some(next) = child {
                    let child_copy = self.clone_shallow(next)?;
                    self.attach_detached(copy, child_copy);
                    pending.push((next, child_copy, true));
                    child = self.next_sibling(next)?;
                }
            }
        }
        Ok(cloned)
    }

    fn clone_node_count(&self, source: NodeId, deep: bool) -> Result<usize, Error> {
        let mut pending = alloc::vec![(source, deep)];
        let mut count = 0usize;
        while let Some((id, children)) = pending.pop() {
            if let Some(content) = self.template_content(id)? {
                pending.push((content, children));
            }
            count = count.checked_add(1).ok_or(Error::LimitExceeded)?;
            if let Some(root) = self.shadow_root(id)? {
                if self
                    .shadow_options(root)?
                    .is_some_and(|options| options.clonable)
                {
                    pending.push((root, true));
                }
            }
            if children {
                let mut child = self.first_child(id)?;
                while let Some(next) = child {
                    pending.push((next, true));
                    child = self.next_sibling(next)?;
                }
            }
        }
        Ok(count)
    }

    fn ensure_clone_capacity(&self, count: usize) -> Result<(), Error> {
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
        Ok(())
    }

    /// Import a node from another document by cloning its data into this document.
    ///
    /// Unlike adoption, this deliberately leaves the source node, wrappers, event listeners,
    /// and ranges untouched. Clonable shadow roots are imported with their full contents;
    /// ordinary children and template contents are cloned when `deep` is true.
    pub fn clone_subtree_from(
        &mut self,
        source: &Document,
        root: NodeId,
        deep: bool,
    ) -> Result<NodeId, Error> {
        if matches!(source.kind(root)?, NodeKind::Document) || source.shadow_host(root)?.is_some() {
            return Err(Error::Hierarchy);
        }

        self.ensure_clone_capacity(source.clone_node_count(root, deep)?)?;

        let cloned = self.clone_shallow_from(source, root)?;
        let mut pending = alloc::vec![(root, cloned, deep)];
        while let Some((original, copy, children)) = pending.pop() {
            if let (Some(source_content), Some(target_content)) = (
                source.template_content(original)?,
                self.template_content(copy)?,
            ) {
                pending.push((source_content, target_content, children));
            }
            if let Some(root) = source.shadow_root(original)? {
                let options = source.shadow_options(root)?.ok_or(Error::WrongKind)?;
                if options.clonable {
                    let root_copy = self.attach_shadow_with_options(copy, options)?;
                    pending.push((root, root_copy, true));
                }
            }
            if children {
                let mut child = source.first_child(original)?;
                while let Some(next) = child {
                    let child_copy = self.clone_shallow_from(source, next)?;
                    self.attach_detached(copy, child_copy);
                    pending.push((next, child_copy, true));
                    child = source.next_sibling(next)?;
                }
            }
        }
        Ok(cloned)
    }

    /// Move a node and its complete host-including subtree from another document.
    ///
    /// The returned map pairs every source identity with its new identity in this document. The
    /// node payloads themselves are moved, not cloned; DOM host wrappers use the map to preserve
    /// JavaScript object identity while rebinding their document and event-target ownership.
    pub fn adopt_subtree_from(
        &mut self,
        source: &mut Document,
        root: NodeId,
    ) -> Result<(NodeId, Vec<(NodeId, NodeId)>), Error> {
        if core::ptr::eq(self, source)
            || matches!(source.kind(root)?, NodeKind::Document)
            || source.shadow_host(root)?.is_some()
        {
            return Err(Error::Hierarchy);
        }
        let detached_template_host = if matches!(source.kind(root)?, NodeKind::DocumentFragment) {
            let host_index = source.nodes[root.index()].template_content;
            (host_index != NO_LINK)
                .then(|| source.link_id(host_index))
                .flatten()
        } else {
            None
        };

        // Build the complete move set before mutating either document. Shadow trees and template
        // contents are document-owned parts of the adopted node just like ordinary descendants.
        let mut pending = alloc::vec![root];
        let mut seen = alloc::vec![false; source.nodes.len()];
        let mut originals = Vec::new();
        while let Some(id) = pending.pop() {
            source.node(id)?;
            if seen[id.index()] {
                continue;
            }
            seen[id.index()] = true;
            originals.push(id);
            if let Some(content) = source.template_content(id)? {
                pending.push(content);
            }
            if let Some(shadow) = source.shadow_root(id)? {
                pending.push(shadow);
            }
            let mut child = source.first_child(id)?;
            while let Some(next) = child {
                pending.push(next);
                child = source.next_sibling(next)?;
            }
        }
        let count = originals.len();
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

        // Reserve every vector that can grow below. Once source.remove succeeds, no ordinary
        // capacity failure can leave a half-adopted subtree behind.
        self.nodes
            .try_reserve(count.saturating_sub(self.free.len()))
            .map_err(|_| Error::LimitExceeded)?;
        self.attribute_namespaces
            .try_reserve(originals.len())
            .map_err(|_| Error::LimitExceeded)?;
        self.doctype_identifiers
            .try_reserve(source.doctype_identifiers.len())
            .map_err(|_| Error::LimitExceeded)?;
        let moved_shadows = source
            .shadow_trees
            .iter()
            .filter(|tree| seen[tree.host.index()])
            .count();
        let mut remaining_shadows = Vec::new();
        remaining_shadows
            .try_reserve(source.shadow_trees.len().saturating_sub(moved_shadows))
            .map_err(|_| Error::LimitExceeded)?;
        self.shadow_trees
            .try_reserve(moved_shadows)
            .map_err(|_| Error::LimitExceeded)?;
        let mut remaining_namespaces = Vec::new();
        remaining_namespaces
            .try_reserve(source.attribute_namespaces.len())
            .map_err(|_| Error::LimitExceeded)?;
        let mut remaining_doctypes = Vec::new();
        remaining_doctypes
            .try_reserve(source.doctype_identifiers.len())
            .map_err(|_| Error::LimitExceeded)?;
        source
            .free
            .try_reserve(count)
            .map_err(|_| Error::LimitExceeded)?;
        source
            .journal
            .try_reserve(1)
            .map_err(|_| Error::LimitExceeded)?;
        self.journal
            .try_reserve(1)
            .map_err(|_| Error::LimitExceeded)?;
        let mut remap = Vec::new();
        remap
            .try_reserve(source.nodes.len())
            .map_err(|_| Error::LimitExceeded)?;
        remap.resize(source.nodes.len(), None);
        let mut mapping = Vec::new();
        mapping
            .try_reserve(originals.len())
            .map_err(|_| Error::LimitExceeded)?;

        // Removing first preserves source mutation/range semantics. The move below is infallible
        // after preflight and uses the source's existing slots, replacing them with invalid ones.
        source.remove(root)?;

        for original in &originals {
            let (index, generation) = if let Some(index) = self.free.pop() {
                (index, self.nodes[index as usize].generation)
            } else {
                let index = self.nodes.len() as u32;
                self.nodes.push(Node::new(NodeKind::DocumentFragment, 1));
                (index, 1)
            };
            remap[original.index()] = Some(NodeId {
                document: self.id,
                index,
                generation,
            });
        }

        let map_index = |index: u32| {
            if index == NO_LINK {
                NO_LINK
            } else {
                remap[index as usize]
                    .expect("adoption subtree must contain every linked node")
                    .index
            }
        };
        for original in &originals {
            let target_id = remap[original.index()].expect("adoption mapping was preallocated");
            let old_generation = source.nodes[original.index()].generation;
            let mut moved = core::mem::replace(
                &mut source.nodes[original.index()],
                Node::new(NodeKind::DocumentFragment, old_generation),
            );
            moved.generation = target_id.generation;
            moved.parent = map_index(moved.parent);
            moved.first_child = map_index(moved.first_child);
            moved.last_child = map_index(moved.last_child);
            moved.prev_sibling = map_index(moved.prev_sibling);
            moved.next_sibling = map_index(moved.next_sibling);
            moved.template_content = if detached_template_host.is_some() && *original == root {
                NO_LINK
            } else {
                map_index(moved.template_content)
            };
            self.nodes[target_id.index as usize] = moved;

            let source_id = *original;
            let source_node = &mut source.nodes[source_id.index()];
            source_node.alive = false;
            if source_node.generation < u32::MAX {
                source_node.generation += 1;
                source.free.push(source_id.index as u32);
            }
            source.live_nodes -= 1;
            self.live_nodes += 1;
        }

        for (owner, namespaces) in core::mem::take(&mut source.attribute_namespaces) {
            if let Some(target) = remap.get(owner.index()).copied().flatten() {
                self.attribute_namespaces.push((target, namespaces));
            } else {
                remaining_namespaces.push((owner, namespaces));
            }
        }
        source.attribute_namespaces = remaining_namespaces;
        for (owner, public_id, system_id) in core::mem::take(&mut source.doctype_identifiers) {
            if let Some(target) = remap.get(owner.index()).copied().flatten() {
                self.doctype_identifiers
                    .push((target, public_id, system_id));
            } else {
                remaining_doctypes.push((owner, public_id, system_id));
            }
        }
        source.doctype_identifiers = remaining_doctypes;
        for tree in core::mem::take(&mut source.shadow_trees) {
            if seen[tree.host.index()] {
                self.shadow_trees.push(shadow::ShadowTree {
                    host: remap[tree.host.index()].expect("shadow host is in adoption subtree"),
                    root: remap[tree.root.index()].expect("shadow root is in adoption subtree"),
                    mode: tree.mode,
                    options: tree.options,
                    manual_assignments: tree
                        .manual_assignments
                        .iter()
                        .filter_map(|(slot, nodes)| {
                            let slot = remap[slot.index()]?;
                            let nodes = nodes.iter().filter_map(|node| remap[node.index()]).collect();
                            Some((slot, nodes))
                        })
                        .collect(),
                });
            } else {
                remaining_shadows.push(tree);
            }
        }
        source.shadow_trees = remaining_shadows;

        if let Some(host) = detached_template_host {
            // A template always owns a content fragment. If that fragment itself is adopted,
            // keep the source template valid by giving it a fresh empty content node.
            let replacement = source
                .create(NodeKind::DocumentFragment)
                .expect("the detached adopted slot provides capacity for template content");
            source.nodes[host.index()].template_content = replacement.index as u32;
            source.nodes[replacement.index()].template_content = host.index as u32;
            source.mark_dirty(host, Dirty::STYLE, MutationKind::FullRebuild);
        }

        mapping.extend(
            originals
                .iter()
                .map(|old| (*old, remap[old.index()].expect("adoption mapping exists"))),
        );
        let adopted_root = remap[root.index()].expect("adoption root is in mapping");
        self.mark_dirty(adopted_root, Dirty::STYLE, MutationKind::FullRebuild);
        source.mark_dirty(source.root(), Dirty::STYLE, MutationKind::FullRebuild);
        Ok((adopted_root, mapping))
    }

    fn clone_shallow_from(&mut self, source: &Document, node: NodeId) -> Result<NodeId, Error> {
        let kind = source.kind(node)?.clone();
        let namespaces = source
            .attribute_namespaces
            .iter()
            .find(|(owner, _)| *owner == node)
            .map(|(_, namespaces)| namespaces.clone())
            .unwrap_or_default();
        let doctype_identifiers = source
            .doctype_identifiers
            .iter()
            .find(|(owner, _, _)| *owner == node)
            .map(|(_, public_id, system_id)| (public_id.clone(), system_id.clone()));
        let clone = self.create(kind)?;
        if !namespaces.is_empty() {
            self.attribute_namespaces.push((clone, namespaces));
        }
        if let Some((public_id, system_id)) = doctype_identifiers {
            self.doctype_identifiers.push((clone, public_id, system_id));
        }
        Ok(clone)
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

    /// Borrow namespace metadata by attribute position, without allocating.
    /// Positions distinguish equal qualified names in different namespaces.
    pub fn attribute_namespace_uri_at(&self, id: NodeId, index: usize) -> Option<&str> {
        self.attribute_namespaces
            .iter()
            .find(|(owner, _)| *owner == id)
            .and_then(|(_, namespaces)| namespaces.iter().find(|(known, _)| *known == index))
            .map(|(_, uri)| uri.as_ref())
    }

    fn attribute_index_ns(
        &self,
        id: NodeId,
        namespace: Option<&str>,
        local: &str,
    ) -> Result<Option<usize>, Error> {
        let NodeKind::Element { attributes, .. } = &self.node(id)?.kind else {
            return Err(Error::WrongKind);
        };
        let namespace = namespace.filter(|uri| !uri.is_empty());
        Ok(attributes
            .iter()
            .enumerate()
            .position(|(index, (name, _))| {
                let uri = self.attribute_namespace_uri_at(id, index);
                let actual_local = if uri.is_some() {
                    split_qualified_name(name.as_str()).1
                } else {
                    name.as_str()
                };
                uri == namespace && actual_local == local
            }))
    }

    fn set_attribute_namespace_at(&mut self, id: NodeId, index: usize, uri: Option<&str>) {
        let uri = uri.filter(|uri| !uri.is_empty());
        if let Some((_, metadata)) = self
            .attribute_namespaces
            .iter_mut()
            .find(|(owner, _)| *owner == id)
        {
            metadata.retain(|(known, _)| *known != index);
            if let Some(uri) = uri {
                metadata.push((index, Rc::from(uri)));
            }
        } else if let Some(uri) = uri {
            self.attribute_namespaces
                .push((id, alloc::vec![(index, Rc::from(uri))]));
        }
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

    /// False for a valid node that no host-including ancestor chain can pass through.
    fn can_be_ancestor(&self, id: NodeId) -> bool {
        let node = &self.nodes[id.index()];
        node.first_child != NO_LINK
            || node.template_content != NO_LINK
            || self.shadow_trees.iter().any(|tree| tree.host == id)
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
            if let Some(parent) = self.link_id(parent_index).or_else(|| {
                self.shadow_trees
                    .iter()
                    .find(|tree| tree.root == id)
                    .map(|tree| tree.host)
            }) {
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
                if candidate == parent || self.can_be_ancestor(candidate) {
                    let mut ancestor = Some(parent);
                    while let Some(id) = ancestor {
                        if candidate == id {
                            return Err(Error::Hierarchy);
                        }
                        ancestor = self.host_including_parent(id)?;
                    }
                }
                if nodes.len() > 1 {
                    if let Some(index) = inserting.iter().position(|&id| id == candidate) {
                        inserting.remove(index);
                    }
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
            if candidate != parent && !self.can_be_ancestor(candidate) {
                continue;
            }
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
        if self.observing_mutations() {
            self.notify(observe::ObservedMutation {
                target: parent,
                kind: observe::ObservedKind::ChildList {
                    added: Some(child),
                    removed: None,
                    previous_sibling: prev,
                    next_sibling: before,
                },
            });
        }
        Ok(())
    }

    fn validate_insertion(
        &self,
        parent: NodeId,
        inserting: &[NodeId],
        before: Option<NodeId>,
        replaced: Option<NodeId>,
    ) -> Result<(), Error> {
        self.validate_insertion_from(self, parent, inserting, before, replaced)
    }

    fn validate_insertion_from(
        &self,
        source: &Document,
        parent: NodeId,
        inserting: &[NodeId],
        before: Option<NodeId>,
        replaced: Option<NodeId>,
    ) -> Result<(), Error> {
        if !matches!(self.kind(parent)?, NodeKind::Document) {
            return if inserting
                .iter()
                .any(|&id| matches!(source.kind(id), Ok(NodeKind::DocumentType(_))))
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
        self.validate_document_children_from(source, &children)
    }

    fn validate_document_children(&self, children: &[NodeId]) -> Result<(), Error> {
        self.validate_document_children_from(self, children)
    }

    fn validate_document_children_from(
        &self,
        source: &Document,
        children: &[NodeId],
    ) -> Result<(), Error> {
        let mut element_index = None;
        let mut doctype_index = None;
        for (index, &id) in children.iter().enumerate() {
            match if id.document == self.id {
                self.kind(id)?
            } else {
                source.kind(id)?
            } {
                NodeKind::Element { .. } => {
                    if element_index.replace(index).is_some() {
                        return Err(Error::Hierarchy);
                    }
                }
                NodeKind::DocumentType(_) => {
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
        let siblings = if self.observing_mutations() {
            (self.previous_sibling(child)?, self.next_sibling(child)?)
        } else {
            (None, None)
        };
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
            if self.observing_mutations() {
                self.notify(observe::ObservedMutation {
                    target: parent,
                    kind: observe::ObservedKind::ChildList {
                        added: None,
                        removed: Some(child),
                        previous_sibling: siblings.0,
                        next_sibling: siblings.1,
                    },
                });
            }
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

    /// Validate replacement before a host adopts a node from another document.
    /// This does not detach nodes or modify either document.
    pub fn validate_replace_from(
        &self,
        source: &Document,
        old: NodeId,
        new: NodeId,
    ) -> Result<(), Error> {
        self.prepare_replacement(source, old, new).map(|_| ())
    }

    fn prepare_replacement(
        &self,
        source: &Document,
        old: NodeId,
        new: NodeId,
    ) -> Result<(NodeId, InsertingNodes, Option<NodeId>), Error> {
        let parent = self.parent(old)?.ok_or(Error::Hierarchy)?;
        let inserting = source.inserting_nodes(new)?;
        let candidates = inserting.as_slice();
        for &candidate in candidates {
            let mut ancestor = Some(parent);
            while let Some(id) = ancestor {
                if id == candidate {
                    return Err(Error::Hierarchy);
                }
                ancestor = self.host_including_parent(id)?;
            }
        }
        let mut before = self.next_sibling(old)?;
        while before.is_some_and(|id| candidates.contains(&id)) {
            before = self.next_sibling(before.unwrap())?;
        }
        self.validate_insertion_from(source, parent, candidates, before, Some(old))?;
        Ok((parent, inserting, before))
    }

    pub fn replace(&mut self, old: NodeId, new: NodeId) -> Result<(), Error> {
        self.node(old)?;
        if old == new {
            self.parent(old)?.ok_or(Error::Hierarchy)?;
            return Ok(());
        }
        let (parent, inserting, before) = self.prepare_replacement(self, old, new)?;
        self.remove(old)?;
        for &candidate in inserting.as_slice() {
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
        let children_changed = self.first_child(parent)?.is_some() || !inserting.is_empty();
        if matches!(self.kind(parent)?, NodeKind::Document) {
            self.validate_document_children(&inserting)?;
        } else {
            self.validate_insertion(parent, &inserting, None, None)?;
        }
        let mut styles_changed = inserting
            .iter()
            .any(|&id| session::subtree_has_style(self, id));
        let mut old_child = self.first_child(parent)?;
        let observing = self.observing_mutations();
        let mut removed = Vec::new();
        while let Some(id) = old_child {
            if observing {
                removed.push(id);
            }
            styles_changed |= session::subtree_has_style(self, id);
            old_child = self.next_sibling(id)?;
        }
        for &node in nodes {
            if matches!(self.kind(node)?, NodeKind::DocumentFragment) {
                let mut fragment_removed = Vec::new();
                if observing {
                    let mut child = self.first_child(node)?;
                    while let Some(id) = child {
                        fragment_removed.push(id);
                        child = self.next_sibling(id)?;
                    }
                }
                self.detach_all_children(node);
                if !fragment_removed.is_empty() {
                    self.notify(observe::ObservedMutation {
                        target: node,
                        kind: observe::ObservedKind::ChildListMany {
                            added: Vec::new(),
                            removed: fragment_removed,
                        },
                    });
                }
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
        // Publish one replacement record with complete identity snapshots.
        if children_changed && observing {
            self.notify(observe::ObservedMutation {
                target: parent,
                kind: observe::ObservedKind::ChildListMany {
                    added: inserting,
                    removed,
                },
            });
        }
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
        let observing = self.observing_mutations();
        let node = self.node_mut_checked(id)?;
        let NodeKind::Element { attributes, .. } = &mut node.kind else {
            return Err(Error::WrongKind);
        };
        let mut old_value = None;
        let mut changed = true;
        let key = if let Some((key, old)) = attributes.iter_mut().find(|(key, _)| key == name) {
            if old.as_str() == value {
                if !observing {
                    return Ok(());
                }
                changed = false;
            }
            if observing {
                old_value = Some(core::mem::replace(old, String::from(value)));
            } else {
                old.clear();
                old.push_str(value);
            }
            key.clone()
        } else {
            let key = Name::new(name);
            attributes.push((key.clone(), String::from(value)));
            key
        };
        let namespace_uri = if observing {
            self.attribute_namespace_uri(id, name)?
        } else {
            None
        };
        if changed {
            self.mark_dirty(id, Dirty::STYLE, MutationKind::Attribute(key));
        }
        if observing {
            self.notify(observe::ObservedMutation {
                target: id,
                kind: observe::ObservedKind::Attribute {
                    name: String::from(name),
                    namespace_uri,
                    old_value,
                },
            });
        }
        Ok(())
    }

    pub fn remove_attribute(&mut self, id: NodeId, name: &str) -> Result<(), Error> {
        let NodeKind::Element { attributes, .. } = &self.node(id)?.kind else {
            return Err(Error::WrongKind);
        };
        if let Some(index) = attributes.iter().position(|(key, _)| key == name) {
            self.remove_attribute_at(id, index)?;
        }
        Ok(())
    }

    fn remove_attribute_at(&mut self, id: NodeId, index: usize) -> Result<(), Error> {
        let namespace_uri = if self.observing_mutations() {
            self.attribute_namespace_uri_at(id, index).map(String::from)
        } else {
            None
        };
        let NodeKind::Element { attributes, .. } = &mut self.node_mut_checked(id)?.kind else {
            return Err(Error::WrongKind);
        };
        let (key, old_value) = attributes.remove(index);
        if let Some((_, metadata)) = self
            .attribute_namespaces
            .iter_mut()
            .find(|(owner, _)| *owner == id)
        {
            metadata.retain(|(known, _)| *known != index);
            for (known, _) in metadata {
                if *known > index {
                    *known -= 1;
                }
            }
        }
        let name = self
            .observing_mutations()
            .then(|| String::from(key.as_str()));
        self.mark_dirty(id, Dirty::STYLE, MutationKind::Attribute(key));
        if let Some(name) = name {
            self.notify(observe::ObservedMutation {
                target: id,
                kind: observe::ObservedKind::Attribute {
                    name,
                    namespace_uri,
                    old_value: Some(old_value),
                },
            });
        }
        Ok(())
    }

    /// Attach metadata to an existing parser-created attribute without a mutation.
    pub fn set_attribute_namespace_metadata(
        &mut self,
        id: NodeId,
        qualified_name: &str,
        namespace_uri: Option<&str>,
    ) -> Result<(), Error> {
        let NodeKind::Element { attributes, .. } = &self.node(id)?.kind else {
            return Err(Error::WrongKind);
        };
        let index = attributes
            .iter()
            .position(|(name, _)| name == qualified_name)
            .ok_or(Error::WrongKind)?;
        self.set_attribute_namespace_at(id, index, namespace_uri);
        Ok(())
    }

    /// Set by expanded name; equal qualified names in distinct namespaces coexist.
    /// Install metadata before synchronous mutation observers see the new value.
    pub fn set_attribute_ns(
        &mut self,
        id: NodeId,
        namespace_uri: Option<&str>,
        qualified_name: &str,
        value: &str,
    ) -> Result<(), Error> {
        let namespace_uri = namespace_uri.filter(|uri| !uri.is_empty());
        let local = if namespace_uri.is_some() {
            split_qualified_name(qualified_name).1
        } else {
            qualified_name
        };
        let existing = self.attribute_index_ns(id, namespace_uri, local)?;
        let observing = self.observing_mutations();
        let key = Name::new(qualified_name);
        let NodeKind::Element { attributes, .. } = &mut self.node_mut_checked(id)?.kind else {
            return Err(Error::WrongKind);
        };
        let mut changed = true;
        let (index, old_value) = if let Some(index) = existing {
            let (old_name, old) = &mut attributes[index];
            if old_name == qualified_name && old == value {
                if !observing {
                    return Ok(());
                }
                changed = false;
            }
            *old_name = key.clone();
            let old_value = if observing {
                Some(core::mem::replace(old, String::from(value)))
            } else {
                old.clear();
                old.push_str(value);
                None
            };
            (index, old_value)
        } else {
            let index = attributes.len();
            attributes.push((key.clone(), String::from(value)));
            (index, None)
        };
        if changed {
            self.set_attribute_namespace_at(id, index, namespace_uri);
            self.mark_dirty(id, Dirty::STYLE, MutationKind::Attribute(key));
        }
        if observing {
            self.notify(observe::ObservedMutation {
                target: id,
                kind: observe::ObservedKind::Attribute {
                    name: String::from(qualified_name),
                    namespace_uri: namespace_uri.map(String::from),
                    old_value,
                },
            });
        }
        Ok(())
    }

    pub fn remove_attribute_ns(
        &mut self,
        id: NodeId,
        namespace_uri: Option<&str>,
        local_name: &str,
    ) -> Result<(), Error> {
        if let Some(index) = self.attribute_index_ns(id, namespace_uri, local_name)? {
            self.remove_attribute_at(id, index)?;
        }
        Ok(())
    }

    /// Look up by namespace URI and local name, independently of the prefix.
    pub fn get_attribute_ns(
        &self,
        id: NodeId,
        namespace_uri: Option<&str>,
        local_name: &str,
    ) -> Result<Option<String>, Error> {
        let index = self.attribute_index_ns(id, namespace_uri, local_name)?;
        let NodeKind::Element { attributes, .. } = &self.node(id)?.kind else {
            return Err(Error::WrongKind);
        };
        Ok(index.map(|index| attributes[index].1.clone()))
    }

    /// Namespace of the first attribute with this qualified name (DOM name lookup).
    /// Attribute-list consumers should use `attribute_namespace_uri_at` instead.
    pub fn attribute_namespace_uri(
        &self,
        id: NodeId,
        qualified_name: &str,
    ) -> Result<Option<String>, Error> {
        let NodeKind::Element { attributes, .. } = &self.node(id)?.kind else {
            return Err(Error::WrongKind);
        };
        Ok(attributes
            .iter()
            .position(|(name, _)| name == qualified_name)
            .and_then(|index| self.attribute_namespace_uri_at(id, index))
            .map(String::from))
    }

    pub fn replace_data(&mut self, id: NodeId, data: &str) -> Result<(), Error> {
        let observing = self.observing_mutations();
        let kind = &mut self.node_mut_checked(id)?.kind;
        let text = match kind {
            NodeKind::Text(text) | NodeKind::Comment(text) => text,
            NodeKind::ProcessingInstruction { data, .. } => data,
            _ => return Err(Error::WrongKind),
        };
        if text != data {
            let old_value = observing.then(|| text.clone());
            text.clear();
            text.push_str(data);
            self.mark_dirty(id, Dirty::STYLE, MutationKind::CharacterData);
            if let Some(old_value) = old_value {
                self.notify(observe::ObservedMutation {
                    target: id,
                    kind: observe::ObservedKind::CharacterData { old_value },
                });
            }
        }
        Ok(())
    }

    fn character_data_value(&self, id: NodeId) -> Result<&str, Error> {
        match self.kind(id)? {
            NodeKind::Text(data) | NodeKind::Comment(data) => Ok(data),
            NodeKind::ProcessingInstruction { data, .. } => Ok(data),
            _ => Err(Error::WrongKind),
        }
    }

    /// Return CharacterData length in UTF-16 code units.
    pub fn character_data_length(&self, id: NodeId) -> Result<usize, Error> {
        Ok(lumen_common::smuggle::utf16_unit_len(
            self.character_data_value(id)?,
        ))
    }

    /// Return the requested CharacterData slice using DOM UTF-16 offsets.
    pub fn substring_data(&self, id: NodeId, offset: usize, count: usize) -> Result<String, Error> {
        let units = lumen_common::smuggle::utf16_units(self.character_data_value(id)?);
        if offset > units.len() {
            return Err(Error::IndexSize);
        }
        let end = offset.saturating_add(count).min(units.len());
        Ok(lumen_common::smuggle::utf16_from_units(&units[offset..end]))
    }

    /// Append CharacterData using UTF-16-aware concatenation.
    pub fn append_data(&mut self, id: NodeId, data: &str) -> Result<(), Error> {
        let offset = self.character_data_length(id)?;
        self.replace_data_range(id, offset, 0, data)
    }

    /// Insert CharacterData at a UTF-16 code-unit offset.
    pub fn insert_data(&mut self, id: NodeId, offset: usize, data: &str) -> Result<(), Error> {
        self.replace_data_range(id, offset, 0, data)
    }

    /// Delete up to `count` UTF-16 code units starting at `offset`.
    pub fn delete_data(&mut self, id: NodeId, offset: usize, count: usize) -> Result<(), Error> {
        self.replace_data_range(id, offset, count, "")
    }

    /// Replace up to `count` UTF-16 code units starting at `offset`.
    pub fn replace_data_range(
        &mut self,
        id: NodeId,
        offset: usize,
        count: usize,
        data: &str,
    ) -> Result<(), Error> {
        let mut units = lumen_common::smuggle::utf16_units(self.character_data_value(id)?);
        if offset > units.len() {
            return Err(Error::IndexSize);
        }
        let end = offset.saturating_add(count).min(units.len());
        units.splice(offset..end, lumen_common::smuggle::utf16_units(data));
        let replacement = lumen_common::smuggle::utf16_from_units(&units);
        self.replace_data(id, &replacement)
    }

    fn node_mut_checked(&mut self, id: NodeId) -> Result<&mut Node, Error> {
        self.node(id)?;
        Ok(self.node_mut(id))
    }
}

fn split_qualified_name(name: &str) -> (&str, &str) {
    name.split_once(':').unwrap_or(("", name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shadow::ShadowOptions;

    fn element(name: &str) -> NodeKind {
        NodeKind::Element {
            namespace: Namespace::Html,
            name: Name::new(name),
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
    fn character_data_methods_use_utf16_offsets_and_preserve_surrogate_units() {
        let mut doc = Document::new(8);
        let text = doc.create(NodeKind::Text(String::from("A"))).unwrap();
        let high = String::from(lumen_common::smuggle::smuggle(0xD83D));
        let low = String::from(lumen_common::smuggle::smuggle(0xDCA9));

        assert_eq!(doc.character_data_length(text), Ok(1));
        doc.append_data(text, &high).unwrap();
        doc.append_data(text, &low).unwrap();
        assert_eq!(doc.substring_data(text, 1, 2).unwrap(), "💩");
        assert_eq!(doc.character_data_length(text), Ok(3));

        doc.insert_data(text, 3, "!").unwrap();
        assert_eq!(doc.substring_data(text, 1, 3).unwrap(), "💩!");
        doc.delete_data(text, 1, 1).unwrap();
        assert_eq!(doc.substring_data(text, 1, 1).unwrap(), low);
        assert_eq!(doc.character_data_length(text), Ok(3));
        doc.replace_data_range(text, 1, 1, "💩").unwrap();
        assert_eq!(doc.substring_data(text, 0, usize::MAX).unwrap(), "A💩!");

        assert_eq!(doc.substring_data(text, 5, 0), Err(Error::IndexSize));
        assert_eq!(doc.insert_data(text, 5, "x"), Err(Error::IndexSize));
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
    fn attribute_namespaces_preserve_expanded_identity_and_order() {
        for namespaced_first in [false, true] {
            let mut doc = Document::new(16);
            let node = doc.create(element("base")).unwrap();
            if namespaced_first {
                doc.set_attribute_ns(node, Some("urn:custom"), "href", "custom")
                    .unwrap();
                doc.set_attribute_ns(node, None, "href", "plain").unwrap();
            } else {
                doc.set_attribute(node, "href", "plain").unwrap();
                doc.set_attribute_ns(node, Some("urn:custom"), "href", "custom")
                    .unwrap();
            }
            assert_eq!(
                doc.get_attribute_ns(node, None, "href").unwrap().as_deref(),
                Some("plain")
            );
            assert_eq!(
                doc.get_attribute_ns(node, Some("urn:custom"), "href")
                    .unwrap()
                    .as_deref(),
                Some("custom")
            );
            doc.set_attribute_ns(node, Some("urn:custom"), "p:href", "changed")
                .unwrap();
            assert_eq!(
                doc.get_attribute_ns(node, None, "href").unwrap().as_deref(),
                Some("plain")
            );
            let cloned = doc.clone_node(node, false).unwrap();
            let mut other = Document::new(16);
            let imported = other.clone_subtree_from(&doc, node, false).unwrap();
            for (document, copy) in [(&doc, cloned), (&other, imported)] {
                assert_eq!(
                    document
                        .get_attribute_ns(copy, None, "href")
                        .unwrap()
                        .as_deref(),
                    Some("plain")
                );
                assert_eq!(
                    document
                        .get_attribute_ns(copy, Some("urn:custom"), "href")
                        .unwrap()
                        .as_deref(),
                    Some("changed")
                );
            }
            doc.remove_attribute_ns(node, None, "href").unwrap();
            assert_eq!(doc.attribute_namespace_uri_at(node, 0), Some("urn:custom"));
            assert_eq!(doc.get_attribute_ns(node, None, "href").unwrap(), None);
            doc.remove_attribute_ns(node, Some("urn:custom"), "href")
                .unwrap();
            assert_eq!(doc.attribute_namespace_uri_at(node, 0), None);
        }
    }

    #[test]
    fn attribute_namespaces_same_value_notifies_without_style_invalidation() {
        let mut doc = Document::new(8);
        let node = doc.create(element("img")).unwrap();
        doc.set_attribute(node, "src", "a.png").unwrap();
        let count = Rc::new(core::cell::Cell::new(0));
        let captured = count.clone();
        doc.set_mutation_sink(Some(Rc::new(move |_, mutation| {
            let observe::ObservedKind::Attribute {
                namespace_uri,
                old_value,
                ..
            } = &mutation.kind
            else {
                panic!("unexpected mutation")
            };
            assert!(namespace_uri.is_none());
            assert_eq!(old_value.as_deref(), Some("a.png"));
            captured.set(captured.get() + 1);
        })));
        doc.drain_mutations();
        let version = doc.version();
        doc.set_attribute(node, "src", "a.png").unwrap();
        doc.set_attribute_ns(node, None, "src", "a.png").unwrap();
        assert_eq!(count.get(), 2);
        assert_eq!(doc.version(), version);
        assert!(doc.drain_mutations().is_empty());
    }

    #[test]
    fn attribute_namespaces_are_atomic_and_preserved_in_observer_records() {
        let mut doc = Document::new(8);
        let node = doc.create(element("base")).unwrap();
        let records = Rc::new(core::cell::RefCell::new(Vec::new()));
        let captured = records.clone();
        doc.set_mutation_sink(Some(Rc::new(move |document, mutation| {
            if let observe::ObservedKind::Attribute {
                namespace_uri,
                old_value,
                ..
            } = &mutation.kind
            {
                captured.borrow_mut().push((
                    namespace_uri.clone(),
                    old_value.clone(),
                    document.get_attribute_ns(node, None, "href").unwrap(),
                    document
                        .get_attribute_ns(node, Some("urn:a"), "href")
                        .unwrap(),
                ));
            }
        })));
        doc.set_attribute_ns(node, Some("urn:a"), "href", "custom")
            .unwrap();
        doc.set_attribute_ns(node, None, "href", "plain").unwrap();
        doc.set_attribute(node, "href", "updated").unwrap();
        doc.remove_attribute_ns(node, Some("urn:a"), "href")
            .unwrap();
        assert_eq!(
            &*records.borrow(),
            &[
                (Some("urn:a".into()), None, None, Some("custom".into())),
                (None, None, Some("plain".into()), Some("custom".into())),
                (
                    Some("urn:a".into()),
                    Some("custom".into()),
                    Some("plain".into()),
                    Some("updated".into())
                ),
                (
                    Some("urn:a".into()),
                    Some("updated".into()),
                    Some("plain".into()),
                    None
                ),
            ]
        );
    }

    #[test]
    fn attribute_namespaces_keep_null_namespace_colons_and_qualified_lookup() {
        let mut doc = Document::new(8);
        let node = doc.create(element("div")).unwrap();
        doc.set_attribute(node, "p:href", "literal").unwrap();
        doc.set_attribute_ns(node, Some("urn:a"), "p:href", "expanded")
            .unwrap();
        assert_eq!(
            doc.get_attribute_ns(node, None, "p:href")
                .unwrap()
                .as_deref(),
            Some("literal")
        );
        assert_eq!(doc.get_attribute_ns(node, None, "href").unwrap(), None);
        doc.remove_attribute(node, "p:href").unwrap();
        assert_eq!(
            doc.attribute_namespace_uri(node, "p:href")
                .unwrap()
                .as_deref(),
            Some("urn:a")
        );
        let mut other = Document::new(8);
        let (adopted, _) = other.adopt_subtree_from(&mut doc, node).unwrap();
        assert_eq!(
            other
                .get_attribute_ns(adopted, Some("urn:a"), "href")
                .unwrap()
                .as_deref(),
            Some("expanded")
        );
    }

    #[test]
    fn replacement_validation_checks_foreign_fragments_before_adoption() {
        let mut target = Document::new(16);
        let old = target.create(element("html")).unwrap();
        target.append(target.root(), old).unwrap();
        let mut source = Document::new(16);
        let fragment = source.create(NodeKind::DocumentFragment).unwrap();
        let first = source.create(element("html")).unwrap();
        let second = source.create(element("other")).unwrap();
        source.append(fragment, first).unwrap();
        source.append(fragment, second).unwrap();
        let versions = (target.version(), source.version());
        assert_eq!(
            target.validate_replace_from(&source, old, fragment),
            Err(Error::Hierarchy)
        );
        assert_eq!((target.version(), source.version()), versions);
        assert_eq!(target.first_child(target.root()).unwrap(), Some(old));
        assert_eq!(source.first_child(fragment).unwrap(), Some(first));
        assert_eq!(source.next_sibling(first).unwrap(), Some(second));
        source.remove(second).unwrap();
        target
            .validate_replace_from(&source, old, fragment)
            .unwrap();
        let (adopted, mapping) = target.adopt_subtree_from(&mut source, fragment).unwrap();
        let adopted_first = mapping
            .iter()
            .find(|(original, _)| *original == first)
            .unwrap()
            .1;
        target.replace(old, adopted).unwrap();
        assert_eq!(
            target.first_child(target.root()).unwrap(),
            Some(adopted_first)
        );
        assert_eq!(target.parent(old).unwrap(), None);
        assert_eq!(target.first_child(adopted).unwrap(), None);
    }

    #[test]
    fn replacement_validation_rejects_shadow_host_ancestor_cycles() {
        let mut doc = Document::new(16);
        let host = doc.create(element("div")).unwrap();
        doc.append(doc.root(), host).unwrap();
        let shadow = doc.attach_shadow(host, shadow::ShadowMode::Open).unwrap();
        let old = doc.create(element("span")).unwrap();
        doc.append(shadow, old).unwrap();
        let version = doc.version();
        assert_eq!(doc.replace(old, host), Err(Error::Hierarchy));
        assert_eq!(doc.version(), version);
        assert_eq!(doc.parent(host).unwrap(), Some(doc.root()));
        assert_eq!(doc.parent(old).unwrap(), Some(shadow));
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
    fn descendant_text_is_bounded_and_keeps_shadow_and_template_trees_separate() {
        let mut doc = Document::new(1100);
        let host = doc.create(element("div")).unwrap();
        let sibling = doc.create(NodeKind::Text("outside".into())).unwrap();
        doc.append(doc.root(), host).unwrap();
        // Keep the sibling under the host's ancestor rather than the document element.
        let parent = doc.create(element("span")).unwrap();
        doc.append(host, parent).unwrap();
        doc.append(host, sibling).unwrap();
        let mut deepest = parent;
        for _ in 0..512 {
            let child = doc.create(element("span")).unwrap();
            doc.append(deepest, child).unwrap();
            deepest = child;
        }
        let text = doc.create(NodeKind::Text("inside".into())).unwrap();
        doc.append(deepest, text).unwrap();
        let template = doc.create(element("template")).unwrap();
        doc.append(parent, template).unwrap();
        let content = doc.template_content(template).unwrap().unwrap();
        let inert = doc.create(NodeKind::Text("inert".into())).unwrap();
        doc.append(content, inert).unwrap();
        let root = doc.attach_shadow(parent, ShadowMode::Closed).unwrap();
        let shadow = doc.create(NodeKind::Text("shadow".into())).unwrap();
        doc.append(root, shadow).unwrap();
        let mut out = String::from("prefix:");
        doc.append_descendant_text(parent, &mut out).unwrap();
        assert_eq!(out, "prefix:inside");
        out.clear();
        doc.append_descendant_text(root, &mut out).unwrap();
        assert_eq!(out, "shadow");
        out.clear();
        doc.append_descendant_text(content, &mut out).unwrap();
        assert_eq!(out, "inert");
    }

    #[test]
    fn clonable_shadow_trees_copy_full_contents_even_for_shallow_clones_and_imports() {
        let mut source = Document::new(100);
        let host = source.create(element("div")).unwrap();
        let light = source.create(NodeKind::Text("light".into())).unwrap();
        source.append(host, light).unwrap();
        let options = ShadowOptions {
            mode: ShadowMode::Closed,
            delegates_focus: true,
            clonable: true,
            serializable: true,
            declarative: true,
            ..ShadowOptions::new(ShadowMode::Closed)
        };
        let root = source.attach_shadow_with_options(host, options).unwrap();
        let nested = source.create(element("span")).unwrap();
        source.append(root, nested).unwrap();
        let nested_root = source.attach_shadow_with_options(nested, options).unwrap();
        let template = source.create(element("template")).unwrap();
        source.append(nested_root, template).unwrap();
        let content = source.template_content(template).unwrap().unwrap();
        let text = source
            .create(NodeKind::Text("inert content".into()))
            .unwrap();
        source.append(content, text).unwrap();
        let shallow = source.clone_node(host, false).unwrap();
        assert_eq!(source.first_child(shallow), Ok(None));
        let copy_root = source.shadow_root(shallow).unwrap().unwrap();
        assert_ne!(copy_root, root);
        assert_eq!(source.shadow_options(copy_root), Ok(Some(options)));
        let copy_nested = source.first_child(copy_root).unwrap().unwrap();
        let copy_nested_root = source.shadow_root(copy_nested).unwrap().unwrap();
        let copy_template = source.first_child(copy_nested_root).unwrap().unwrap();
        let copy_content = source.template_content(copy_template).unwrap().unwrap();
        let copy_text = source.first_child(copy_content).unwrap().unwrap();
        assert_ne!(copy_text, text);
        assert_eq!(source.kind(copy_text), source.kind(text));

        let mut destination = Document::new(100);
        for deep in [false, true] {
            let imported = destination.clone_subtree_from(&source, host, deep).unwrap();
            assert_eq!(destination.first_child(imported).unwrap().is_some(), deep);
            let imported_root = destination.shadow_root(imported).unwrap().unwrap();
            assert_eq!(destination.shadow_options(imported_root), Ok(Some(options)));
            let imported_nested = destination.first_child(imported_root).unwrap().unwrap();
            let imported_nested_root = destination.shadow_root(imported_nested).unwrap().unwrap();
            let imported_template = destination
                .first_child(imported_nested_root)
                .unwrap()
                .unwrap();
            let imported_content = destination
                .template_content(imported_template)
                .unwrap()
                .unwrap();
            let imported_text = destination.first_child(imported_content).unwrap().unwrap();
            assert_eq!(destination.kind(imported_text), source.kind(text));
        }
    }

    #[test]
    fn clonable_shadow_copy_limits_are_checked_before_allocating_any_nodes() {
        let mut source = Document::new(7);
        let host = source.create(element("div")).unwrap();
        let options = ShadowOptions {
            clonable: true,
            ..ShadowOptions::new(ShadowMode::Open)
        };
        let root = source.attach_shadow_with_options(host, options).unwrap();
        let template = source.create(element("template")).unwrap();
        source.append(root, template).unwrap();
        let content = source.template_content(template).unwrap().unwrap();
        let text = source.create(NodeKind::Text("full".into())).unwrap();
        source.append(content, text).unwrap();
        let count = source.node_count();
        assert_eq!(source.clone_node(host, false), Err(Error::LimitExceeded));
        assert_eq!(source.node_count(), count);
        assert_eq!(source.first_child(root), Ok(Some(template)));
        let mut destination = Document::new(5);
        assert_eq!(
            destination.clone_subtree_from(&source, host, false),
            Err(Error::LimitExceeded)
        );
        assert_eq!(destination.node_count(), 1);
        assert_eq!(destination.first_child(destination.root()), Ok(None));
    }

    #[test]
    fn cross_document_import_clones_deep_template_content_and_attribute_namespaces() {
        let mut source = Document::new(16);
        let host = source.create(element("section")).unwrap();
        let template = source.create(element("template")).unwrap();
        source.append(host, template).unwrap();
        source
            .set_attribute_ns(host, Some("urn:example"), "ex:kind", "source")
            .unwrap();
        let source_content = source.template_content(template).unwrap().unwrap();
        let source_text = source.create(NodeKind::Text("inside".into())).unwrap();
        source.append(source_content, source_text).unwrap();

        let mut destination = Document::new(16);
        let imported = destination.clone_subtree_from(&source, host, true).unwrap();
        assert_ne!(imported, host);
        assert_eq!(
            destination.kind(imported).unwrap(),
            source.kind(host).unwrap()
        );
        assert_eq!(
            destination
                .attribute_namespace_uri(imported, "ex:kind")
                .unwrap(),
            Some("urn:example".into())
        );
        let imported_template = destination.first_child(imported).unwrap().unwrap();
        let imported_content = destination
            .template_content(imported_template)
            .unwrap()
            .unwrap();
        let imported_text = destination.first_child(imported_content).unwrap().unwrap();
        assert_eq!(
            destination.kind(imported_text).unwrap(),
            &NodeKind::Text("inside".into())
        );
        assert_eq!(source.first_child(source.root()).unwrap(), None);
        assert_eq!(
            source.first_child(source_content).unwrap(),
            Some(source_text)
        );

        let shallow = destination
            .clone_subtree_from(&source, template, false)
            .unwrap();
        let shallow_content = destination.template_content(shallow).unwrap().unwrap();
        assert_eq!(destination.first_child(shallow).unwrap(), None);
        assert_eq!(destination.first_child(shallow_content).unwrap(), None);
    }

    #[test]
    fn cross_document_adoption_moves_node_slots_templates_shadow_trees_and_namespaces() {
        let mut source = Document::new(24);
        let host = source.create(element("section")).unwrap();
        let template = source.create(element("template")).unwrap();
        source.append(host, template).unwrap();
        source
            .set_attribute_ns(host, Some("urn:example"), "ex:kind", "source")
            .unwrap();
        let source_content = source.template_content(template).unwrap().unwrap();
        let source_text = source.create(NodeKind::Text("inside".into())).unwrap();
        source.append(source_content, source_text).unwrap();
        let shadow = source.attach_shadow(host, ShadowMode::Open).unwrap();
        let shadow_child = source.create(element("b")).unwrap();
        source.append(shadow, shadow_child).unwrap();
        source.append(source.root(), host).unwrap();

        let mut destination = Document::new(24);
        let external_parent = destination.create(element("main")).unwrap();
        destination
            .append(destination.root(), external_parent)
            .unwrap();
        let (adopted, mapping) = destination.adopt_subtree_from(&mut source, host).unwrap();
        let migrated = |old: NodeId| mapping.iter().find(|(before, _)| *before == old).unwrap().1;
        assert!(source.parent(host).is_err());
        assert!(source.kind(host).is_err());
        assert_eq!(
            destination.kind(adopted).unwrap(),
            &NodeKind::Element {
                namespace: Namespace::Html,
                name: "section".into(),
                attributes: alloc::vec![("ex:kind".into(), "source".into())],
            }
        );
        assert_eq!(
            destination
                .attribute_namespace_uri(adopted, "ex:kind")
                .unwrap(),
            Some("urn:example".into())
        );
        let adopted_template = destination.first_child(adopted).unwrap().unwrap();
        let adopted_content = destination
            .template_content(adopted_template)
            .unwrap()
            .unwrap();
        let adopted_text = destination.first_child(adopted_content).unwrap().unwrap();
        assert_eq!(adopted_text, migrated(source_text));
        assert_eq!(
            destination.kind(adopted_text).unwrap(),
            &NodeKind::Text("inside".into())
        );
        let adopted_shadow = destination.shadow_root(adopted).unwrap().unwrap();
        assert_eq!(adopted_shadow, migrated(shadow));
        assert_eq!(
            destination.first_child(adopted_shadow).unwrap(),
            Some(migrated(shadow_child))
        );
        destination.append(external_parent, adopted).unwrap();
        assert_eq!(destination.parent(adopted).unwrap(), Some(external_parent));
    }

    #[test]
    fn adopting_template_contents_leaves_the_source_host_with_empty_contents() {
        let mut source = Document::new(8);
        let template = source.create(element("template")).unwrap();
        let content = source.template_content(template).unwrap().unwrap();
        let text = source.create(NodeKind::Text("moved".into())).unwrap();
        source.append(content, text).unwrap();
        let mut destination = Document::new(8);
        let (adopted, mapping) = destination
            .adopt_subtree_from(&mut source, content)
            .unwrap();
        assert_eq!(
            destination.kind(adopted).unwrap(),
            &NodeKind::DocumentFragment
        );
        assert_eq!(
            destination
                .kind(mapping.iter().find(|(old, _)| *old == text).unwrap().1)
                .unwrap(),
            &NodeKind::Text("moved".into())
        );
        let replacement = source.template_content(template).unwrap().unwrap();
        assert_ne!(replacement, content);
        assert_eq!(source.first_child(replacement).unwrap(), None);
    }

    #[test]
    fn shallow_clone_copies_attributes_without_children() {
        let mut doc = Document::new(8);
        let host = doc.create(element("div")).unwrap();
        let child = doc.create(element("span")).unwrap();
        doc.append(host, child).unwrap();
        doc.set_attribute(host, "class", "a").unwrap();
        doc.set_attribute(host, "data-custom", "b").unwrap();
        doc.set_attribute(host, "class", "c").unwrap();
        let copy = doc.clone_shallow(host).unwrap();
        assert_eq!(doc.kind(copy).unwrap(), doc.kind(host).unwrap());
        assert_eq!(doc.first_child(copy).unwrap(), None);
        assert_eq!(doc.parent(copy).unwrap(), None);
        assert_eq!(
            doc.mutations().last().unwrap().kind,
            MutationKind::Attribute(Name::new("class"))
        );
        doc.remove_attribute(host, "data-custom").unwrap();
        assert_eq!(
            doc.mutations().last().unwrap().kind,
            MutationKind::Attribute(Name::new("data-custom"))
        );
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
