//! Host-independent HTML document core. Parsing, style, layout, and paint derive from this tree.
#![no_std]

extern crate alloc;

pub mod animation;
pub mod css;
pub mod debug;
pub mod details;
pub mod directionality;
pub mod font_display;
pub mod forms;
pub mod top_layer;
pub mod interaction;
pub mod labels;
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
pub use shadow::{ShadowMode, ShadowOptions, SlotAssignmentMode};

use alloc::{boxed::Box, rc::Rc, string::String, vec, vec::Vec};
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
    /// A materialized DOM Attr node. Element attribute storage stays compact until an
    /// attribute node is requested; attached Attr nodes are indexed by the owning Element's
    /// sparse `attribute_nodes` sidecar and are not part of the ordinary child tree.
    Attribute {
        namespace_uri: Option<Box<Rc<str>>>,
        qualified_name: Name,
        value: String,
        // Only materialized attached attributes pay for this uncommon payload.
        owner_element: Option<Box<NodeId>>,
    },
    Element {
        namespace: Namespace,
        name: Name,
        attributes: Vec<(Name, String)>,
    },
    Text(String),
    CData(String),
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
    InUseAttribute,
    NotFound,
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
    /// Lazily materialized Attr nodes, aligned with each Element's compact attribute vector.
    /// No sidecar is allocated for parser-created attributes until the host requests a Node.
    // Only materialized Attr identities are stored here. Keep each owner's entries sparse so
    // exposing one Attr on an Element with a large attribute list does not allocate a slot for
    // every parsed attribute.
    attribute_nodes: Vec<(NodeId, Vec<(usize, NodeId)>)>,
    doctype_identifiers: Vec<(NodeId, String, String)>,
    document_mode: DocumentMode,
    is_html_document: bool,
    scripting_enabled: bool,
    allow_declarative_shadow_roots: bool,
    free: Vec<u32>,
    live_nodes: usize,
    version: u64,
    max_nodes: usize,
    journal: Vec<Mutation>,
    mutation_sink: Option<Rc<dyn Fn(&Document, &observe::ObservedMutation)>>,
    details_sink: Option<Rc<dyn Fn(&Document, details::DetailsTransition)>>,
    shadow_mutation_sink: Option<Rc<dyn Fn(&Document, &observe::ObservedMutation)>>,
    validity_resolver: Option<ValidityResolver>,
    form_selector_state_resolver:
        Option<Rc<dyn Fn(&Document, NodeId) -> Option<forms::FormSelectorState>>>,
    interaction_state: interaction::InteractionState,
    interaction_generation: u64,
    top_layer_elements: Vec<top_layer::Entry>,
    /// The topmost Auto popover hit by the preceding trusted pointerdown, or
    /// null when that pointerdown was outside the open Auto stack.
    popover_pointerdown_target: Option<NodeId>,
    dialog_return_values: Vec<(NodeId, String)>,
    shadow_trees: Vec<shadow::ShadowTree>,
    manual_assignments: Vec<(NodeId, Vec<NodeId>)>,
    selector_cache: core::cell::RefCell<Vec<(String, Rc<Vec<css::Selector>>)>>,
    id_index: core::cell::RefCell<IdIndex>,
}

/// First element per `id` in the connected light tree, valid for one document version.
/// Built on the second lookup at a version so mutate-then-lookup loops keep the cheap walk.
#[derive(Default)]
struct IdIndex {
    walked_version: Option<u64>,
    built_version: Option<u64>,
    first: alloc::collections::BTreeMap<String, NodeId>,
}

/// Host-owned live form state is read through a weak adapter without copying
/// values into the DOM or exposing a language runtime to the shared engine.
struct ValidityResolver {
    read: Rc<dyn Fn(&Document, NodeId) -> Option<forms::ValidityState>>,
    generation: Rc<dyn Fn() -> u64>,
}

impl Document {
    pub fn new(max_nodes: usize) -> Self {
        Self {
            id: NEXT_DOCUMENT_ID.fetch_add(1, Ordering::Relaxed),
            nodes: alloc::vec![Node::new(NodeKind::Document, 1)],
            attribute_namespaces: Vec::new(),
            attribute_nodes: Vec::new(),
            doctype_identifiers: Vec::new(),
            document_mode: DocumentMode::NoQuirks,
            is_html_document: false,
            scripting_enabled: false,
            allow_declarative_shadow_roots: false,
            free: Vec::new(),
            live_nodes: 1,
            version: 0,
            max_nodes: max_nodes.max(1),
            journal: Vec::new(),
            mutation_sink: None,
            details_sink: None,
            shadow_mutation_sink: None,
            validity_resolver: None,
            form_selector_state_resolver: None,
            interaction_state: interaction::InteractionState::default(),
            interaction_generation: 0,
            top_layer_elements: Vec::new(),
            popover_pointerdown_target: None,
            dialog_return_values: Vec::new(),
            shadow_trees: Vec::new(),
            manual_assignments: Vec::new(),
            selector_cache: core::cell::RefCell::new(Vec::new()),
            id_index: core::cell::RefCell::new(IdIndex::default()),
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

    /// Looks up the first connected light-tree element with `wanted` as its `id`.
    /// Returns `None` when the index is not warm yet and the caller must walk the tree.
    pub(crate) fn indexed_element_by_id(&self, wanted: &str) -> Option<Option<NodeId>> {
        let mut index = self.id_index.borrow_mut();
        if index.built_version != Some(self.version) {
            if index.walked_version != Some(self.version) {
                index.walked_version = Some(self.version);
                return None;
            }
            let mut first = alloc::collections::BTreeMap::new();
            let root = self.root();
            let mut current = self.first_child(root).ok()?;
            while let Some(id) = current {
                if matches!(self.kind(id), Ok(NodeKind::Element { .. })) {
                    if let Ok(Some(value)) = self.get_attribute_ns_ref(id, None, "id") {
                        if !value.is_empty() && !first.contains_key(value) {
                            first.insert(String::from(value), id);
                        }
                    }
                }
                current = selector::next_descendant(self, root, id).ok()?;
            }
            index.first = first;
            index.built_version = Some(self.version);
        }
        Some(index.first.get(wanted).copied())
    }

    pub fn set_validity_resolver(
        &mut self,
        read: Rc<dyn Fn(&Document, NodeId) -> Option<forms::ValidityState>>,
        generation: Rc<dyn Fn() -> u64>,
    ) {
        self.validity_resolver = Some(ValidityResolver { read, generation });
        self.mark_dirty(self.root(), Dirty::STYLE, MutationKind::FullRebuild);
    }

    /// Live validity when the document has a host state source. A missing or
    /// expired source lets callers use the shared attribute-backed algorithms.
    pub fn validity_state(&self, node: NodeId) -> Option<forms::ValidityState> {
        self.validity_resolver
            .as_ref()
            .and_then(|source| (source.read)(self, node))
    }

    pub fn validity_generation(&self) -> u64 {
        self.validity_resolver
            .as_ref()
            .map_or(0, |source| (source.generation)())
    }

    /// Install a sparse host-state reader for checkedness and selectedness.
    /// Its mutations share the generation published by `set_validity_resolver`
    /// so retained CSS styles observe live control state without DOM copies.
    pub fn set_form_selector_state_resolver(
        &mut self,
        read: Rc<dyn Fn(&Document, NodeId) -> Option<forms::FormSelectorState>>,
    ) {
        self.form_selector_state_resolver = Some(read);
        self.mark_dirty(self.root(), Dirty::STYLE, MutationKind::FullRebuild);
    }

    /// Read host-owned selector state for one candidate node, if available.
    pub fn form_selector_state(&self, node: NodeId) -> Option<forms::FormSelectorState> {
        self.form_selector_state_resolver
            .as_ref()
            .and_then(|read| read(self, node))
    }

    /// Replace the sparse host interaction snapshot used by CSS selector
    /// matching. Detached or stale anchors are discarded at this boundary.
    pub fn set_interaction_state(&mut self, mut state: interaction::InteractionState) {
        let valid = |node: Option<NodeId>| {
            node.filter(|node| self.is_connected_element(*node))
        };
        state.focused = valid(state.focused);
        state.focus_visible = valid(state.focus_visible);
        state.hover = valid(state.hover);
        state.active = [valid(state.active[0]), valid(state.active[1])];
        if self.interaction_state == state {
            return;
        }
        self.interaction_state = state;
        self.interaction_generation = self.interaction_generation.wrapping_add(1);
    }

    pub const fn interaction_state(&self) -> interaction::InteractionState {
        self.interaction_state
    }

    pub const fn interaction_generation(&self) -> u64 {
        self.interaction_generation
    }

    /// Add an element to the document's top layer. This shared membership
    /// primitive is intentionally independent of dialog/popover APIs.
    pub fn push_top_layer_element(&mut self, node: NodeId) -> Result<(), Error> {
        self.push_top_layer_entry(node, None)
    }

    /// Promote an element to the top layer with a typed platform state. The
    /// entry shares the same bounded stack as fullscreen and other generic
    /// top-layer consumers, so CSS can distinguish modal dialogs from popovers
    /// without a second per-node registry.
    pub fn push_top_layer_element_with_kind(
        &mut self,
        node: NodeId,
        kind: top_layer::Kind,
    ) -> Result<(), Error> {
        self.push_top_layer_entry(node, Some((kind, None)))
    }

    pub fn push_top_layer_element_with_focus(
        &mut self,
        node: NodeId,
        kind: top_layer::Kind,
        restore_focus: Option<NodeId>,
    ) -> Result<(), Error> {
        self.push_top_layer_entry(node, Some((kind, restore_focus)))
    }

    /// Iterate the bounded top-layer stack in paint order without making a snapshot.
    pub fn top_layer_entries(
        &self,
    ) -> impl Iterator<Item = (NodeId, top_layer::Kind, Option<NodeId>)> + '_ {
        self.top_layer_elements
            .iter()
            .map(|entry| (entry.node, entry.kind, entry.restore_focus))
    }

    pub(crate) const fn popover_pointerdown_target(&self) -> Option<NodeId> {
        self.popover_pointerdown_target
    }

    pub(crate) fn set_popover_pointerdown_target(&mut self, target: Option<NodeId>) {
        self.popover_pointerdown_target = target;
    }

    fn push_top_layer_entry(
        &mut self,
        node: NodeId,
        requested_state: Option<(top_layer::Kind, Option<NodeId>)>,
    ) -> Result<(), Error> {
        if !matches!(self.kind(node)?, NodeKind::Element { .. }) {
            return Err(Error::WrongKind);
        }
        if !self.is_connected_element(node) {
            return Err(Error::InvalidNode);
        }
        if let Some(index) = self
            .top_layer_elements
            .iter()
            .position(|entry| entry.node == node)
        {
            if index + 1 == self.top_layer_elements.len()
                && requested_state.is_none_or(|(kind, focus)| {
                    self.top_layer_elements[index].kind == kind
                        && self.top_layer_elements[index].restore_focus == focus
                })
            {
                return Ok(());
            }
            let mut entry = self.top_layer_elements.remove(index);
            if let Some((kind, restore_focus)) = requested_state {
                entry.kind = kind;
                entry.restore_focus = restore_focus;
            }
            self.top_layer_elements.push(entry);
        } else {
            self.top_layer_elements
                .try_reserve(1)
                .map_err(|_| Error::LimitExceeded)?;
            self.top_layer_elements.push(top_layer::Entry {
                node,
                kind: requested_state.map_or(top_layer::Kind::Generic, |(kind, _)| kind),
                restore_focus: requested_state.and_then(|(_, focus)| focus),
                popover_transitioning: false,
            });
        }
        self.interaction_generation = self.interaction_generation.wrapping_add(1);
        Ok(())
    }

    pub fn remove_top_layer_element(&mut self, node: NodeId) -> bool {
        self.remove_top_layer_element_with_state(node).is_some()
    }

    pub fn remove_top_layer_element_with_state(
        &mut self,
        node: NodeId,
    ) -> Option<(top_layer::Kind, Option<NodeId>)> {
        let Some(index) = self
            .top_layer_elements
            .iter()
            .position(|entry| entry.node == node)
        else {
            return None;
        };
        let entry = self.top_layer_elements.remove(index);
        self.interaction_generation = self.interaction_generation.wrapping_add(1);
        Some((entry.kind, entry.restore_focus))
    }

    pub(crate) fn is_top_layer_element(&self, node: NodeId) -> bool {
        self.top_layer_elements
            .iter()
            .any(|entry| entry.node == node)
    }

    pub fn top_layer_kind(&self, node: NodeId) -> Option<top_layer::Kind> {
        self.top_layer_elements
            .iter()
            .find(|entry| entry.node == node)
            .map(|entry| entry.kind)
    }

    pub fn top_layer_restore_focus(&self, node: NodeId) -> Option<NodeId> {
        self.top_layer_elements
            .iter()
            .find(|entry| entry.node == node)
            .and_then(|entry| entry.restore_focus)
    }

    pub(crate) fn popover_transitioning(&self, node: NodeId) -> bool {
        self.top_layer_elements
            .iter()
            .find(|entry| entry.node == node)
            .is_some_and(|entry| {
                matches!(entry.kind, top_layer::Kind::Popover(_))
                    && entry.popover_transitioning
            })
    }

    pub(crate) fn any_popover_transitioning(&self) -> bool {
        self.top_layer_elements.iter().any(|entry| {
            matches!(entry.kind, top_layer::Kind::Popover(_)) && entry.popover_transitioning
        })
    }

    /// Acquire the per-popover transition guard for the DOM binding adapter.
    pub fn begin_popover_transition(&mut self, node: NodeId) -> bool {
        let Some(entry) = self
            .top_layer_elements
            .iter_mut()
            .find(|entry| entry.node == node && matches!(entry.kind, top_layer::Kind::Popover(_)))
        else {
            return false;
        };
        if entry.popover_transitioning {
            return false;
        }
        entry.popover_transitioning = true;
        true
    }

    /// Release a transition guard after the binding finishes dispatching events.
    pub fn end_popover_transition(&mut self, node: NodeId) {
        if let Some(entry) = self
            .top_layer_elements
            .iter_mut()
            .find(|entry| entry.node == node && matches!(entry.kind, top_layer::Kind::Popover(_)))
        {
            entry.popover_transitioning = false;
        }
    }

    pub fn dialog_return_value(&self, node: NodeId) -> Result<&str, Error> {
        if !matches!(self.kind(node)?, NodeKind::Element { namespace: Namespace::Html, name, .. } if name.as_str() == "dialog") {
            return Err(Error::WrongKind);
        }
        Ok(self
            .dialog_return_values
            .iter()
            .find(|(owner, _)| *owner == node)
            .map_or("", |(_, value)| value.as_str()))
    }

    pub fn set_dialog_return_value(&mut self, node: NodeId, value: &str) -> Result<(), Error> {
        if !matches!(self.kind(node)?, NodeKind::Element { namespace: Namespace::Html, name, .. } if name.as_str() == "dialog") {
            return Err(Error::WrongKind);
        }
        let previous_length = self
            .dialog_return_values
            .iter()
            .find(|(owner, _)| *owner == node)
            .map_or(0, |(_, stored)| stored.len());
        let other_bytes = self
            .dialog_return_values
            .iter()
            .try_fold(0usize, |total, (_, stored)| {
                lumen_common::limits::size::sum(
                    total,
                    stored.len(),
                    html::MAX_HTML_BYTES,
                )
                .ok()
            })
            .ok_or(Error::LimitExceeded)?
            .checked_sub(previous_length)
            .ok_or(Error::LimitExceeded)?;
        lumen_common::limits::size::sum(other_bytes, value.len(), html::MAX_HTML_BYTES)
            .map_err(|_| Error::LimitExceeded)?;
        if value.is_empty() {
            self.dialog_return_values.retain(|(owner, _)| *owner != node);
            return Ok(());
        }
        if let Some((_, stored)) = self
            .dialog_return_values
            .iter_mut()
            .find(|(owner, _)| *owner == node)
        {
            stored
                .try_reserve(value.len().saturating_sub(stored.len()))
                .map_err(|_| Error::LimitExceeded)?;
            stored.clear();
            stored.push_str(value);
            return Ok(());
        }
        self.dialog_return_values
            .try_reserve(1)
            .map_err(|_| Error::LimitExceeded)?;
        if self.dialog_return_values.len() >= self.max_nodes {
            return Err(Error::LimitExceeded);
        }
        let mut stored = String::new();
        stored
            .try_reserve(value.len())
            .map_err(|_| Error::LimitExceeded)?;
        stored.push_str(value);
        self.dialog_return_values.push((node, stored));
        Ok(())
    }

    fn is_connected_element(&self, node: NodeId) -> bool {
        if !matches!(self.kind(node), Ok(NodeKind::Element { .. })) {
            return false;
        }
        let mut current = node;
        loop {
            if current == self.root() {
                return true;
            }
            let Ok(Some(parent)) = self.shadow_including_parent(current) else {
                return false;
            };
            current = parent;
        }
    }

    fn is_shadow_including_descendant_of(&self, root: NodeId, mut node: NodeId) -> bool {
        loop {
            if node == root {
                return true;
            }
            let Ok(Some(parent)) = self.shadow_including_parent(node) else {
                return false;
            };
            node = parent;
        }
    }

    fn clear_interaction_subtree(&mut self, root: NodeId) {
        let mut state = self.interaction_state;
        let old_state = self.interaction_state;
        let mut clear = |node: &mut Option<NodeId>| {
            if node.is_some_and(|node| self.is_shadow_including_descendant_of(root, node)) {
                *node = None;
            }
        };
        clear(&mut state.focused);
        clear(&mut state.focus_visible);
        clear(&mut state.hover);
        clear(&mut state.active[0]);
        clear(&mut state.active[1]);
        drop(clear);
        let mut removed_top_layer = false;
        let mut index = 0;
        while index < self.top_layer_elements.len() {
            let node = self.top_layer_elements[index].node;
            if self.is_shadow_including_descendant_of(root, node) {
                self.top_layer_elements.remove(index);
                removed_top_layer = true;
            } else {
                index += 1;
            }
        }
        if old_state != state || removed_top_layer {
            self.interaction_state = state;
            self.interaction_generation = self.interaction_generation.wrapping_add(1);
        }
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

    /// Whether HTML parsing and rendering use the document's scripting-enabled mode.
    pub const fn scripting_enabled(&self) -> bool {
        self.scripting_enabled
    }

    /// Whether document parsing may attach declarative shadow roots.
    pub const fn allow_declarative_shadow_roots(&self) -> bool {
        self.allow_declarative_shadow_roots
    }

    /// Set while the parser constructs a document, before it is exposed to a host.
    pub fn set_scripting_enabled(&mut self, enabled: bool) {
        self.scripting_enabled = enabled;
    }

    /// Set the parser permission used for declarative shadow roots.
    pub fn set_allow_declarative_shadow_roots(&mut self, allowed: bool) {
        self.allow_declarative_shadow_roots = allowed;
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

    /// Remaining arena capacity under this document's configured node budget.
    /// Hosts use this for bounded multi-node mutations that must preflight
    /// growth before creating any part of the replacement subtree.
    pub fn remaining_node_capacity(&self) -> usize {
        self.max_nodes.saturating_sub(self.live_nodes)
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
        let id = self.create_without_details_steps(kind)?;
        self.details_created(id)?;
        Ok(id)
    }

    fn create_without_details_steps(&mut self, kind: NodeKind) -> Result<NodeId, Error> {
        let template = matches!(&kind, NodeKind::Element { namespace: Namespace::Html, name, .. } if svg::local_name(name) == "template");
        if template && self.max_nodes.saturating_sub(self.live_nodes) < 2 {
            return Err(Error::LimitExceeded);
        }
        if self.live_nodes >= self.max_nodes {
            return Err(Error::LimitExceeded);
        }
        if matches!(kind, NodeKind::Document) {
            return Err(Error::Hierarchy);
        }
        if matches!(
            kind,
            NodeKind::Attribute {
                owner_element: Some(_),
                ..
            }
        ) {
            return Err(Error::InUseAttribute);
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
        if !matches!(self.kind(id)?, NodeKind::Element { namespace: Namespace::Html, name, .. } if svg::local_name(name) == "template")
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

    pub(crate) fn host_including_parent(&self, id: NodeId) -> Result<Option<NodeId>, Error> {
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
        if matches!(
            self.kind(root)?,
            NodeKind::Attribute {
                owner_element: Some(_),
                ..
            }
        ) {
            return Err(Error::InUseAttribute);
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
            if let Some((_, attributes)) =
                self.attribute_nodes.iter().find(|(owner, _)| *owner == id)
            {
                pending.extend(attributes.iter().map(|(_, attribute)| *attribute));
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
            self.attribute_nodes.retain(|(owner, _)| *owner != id);
            self.doctype_identifiers
                .retain(|(owner, _, _)| *owner != id);
            self.live_nodes -= 1;
        }
        let document_id = self.id;
        let nodes = &self.nodes;
        self.dialog_return_values.retain(|(owner, _)| {
            owner.document == document_id
                && nodes.get(owner.index()).is_some_and(|node| {
                    node.alive && node.generation == owner.generation
                })
        });
        Ok(())
    }

    /// Copy one node and its attributes into a fresh detached node.
    pub fn clone_shallow(&mut self, source: NodeId) -> Result<NodeId, Error> {
        let kind = match self.kind(source)? {
            NodeKind::Attribute {
                namespace_uri,
                qualified_name,
                value,
                ..
            } => NodeKind::Attribute {
                namespace_uri: namespace_uri.clone(),
                qualified_name: qualified_name.clone(),
                value: value.clone(),
                owner_element: None,
            },
            kind => kind.clone(),
        };
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
        let clone = self.create_without_details_steps(kind)?;
        if !namespaces.is_empty() {
            self.attribute_namespaces.push((clone, namespaces));
        }
        if let Some((public_id, system_id)) = doctype_identifiers {
            self.doctype_identifiers.push((clone, public_id, system_id));
        }
        self.details_created(clone)?;
        Ok(clone)
    }

    /// Append descendant text in ordinary tree order without a traversal stack.
    /// Template contents and shadow trees are separate trees and are not included.
    pub fn append_descendant_text(&self, root: NodeId, out: &mut String) -> Result<(), Error> {
        let mut current = self.first_child(root)?;
        while let Some(node) = current {
            if let NodeKind::Text(text) | NodeKind::CData(text) = self.kind(node)? {
                out.push_str(text);
            }
            current = selector::next_descendant(self, root, node)?;
        }
        Ok(())
    }

    /// The title source in tree order. SVG documents use only direct SVG title
    /// children of their SVG root; other documents use the first HTML title.
    pub fn title_element(&self) -> Result<Option<NodeId>, Error> {
        if let Some(root) = selector::document_element(self) {
            if matches!(self.kind(root)?, NodeKind::Element { namespace: Namespace::Svg, name, .. }
                if svg::local_name(name) == "svg")
            {
                let mut child = self.first_child(root)?;
                while let Some(node) = child {
                    if matches!(self.kind(node)?, NodeKind::Element { namespace: Namespace::Svg, name, .. }
                        if svg::local_name(name) == "title")
                    {
                        return Ok(Some(node));
                    }
                    child = self.next_sibling(node)?;
                }
                return Ok(None);
            }
        }
        let mut cursor = self.first_child(self.root())?;
        while let Some(node) = cursor {
            if matches!(self.kind(node)?, NodeKind::Element { namespace: Namespace::Html, name, .. }
                if svg::local_name(name) == "title")
            {
                return Ok(Some(node));
            }
            cursor = selector::next_descendant(self, self.root(), node)?;
        }
        Ok(None)
    }

    /// Child text content with ASCII whitespace stripped and collapsed. This
    /// streams the direct text children and allocates only the resulting title.
    pub fn title(&self) -> Result<String, Error> {
        let Some(title) = self.title_element()? else {
            return Ok(String::new());
        };
        let mut cursor = self.first_child(title)?;
        let mut error = None;
        let parts = core::iter::from_fn(|| loop {
            let node = cursor?;
            cursor = match self.next_sibling(node) {
                Ok(next) => next,
                Err(problem) => {
                    error = Some(problem);
                    return None;
                }
            };
            match self.kind(node) {
                Ok(NodeKind::Text(text) | NodeKind::CData(text)) => return Some(text.as_str()),
                Ok(_) => {}
                Err(problem) => {
                    error = Some(problem);
                    cursor = None;
                    return None;
                }
            }
        });
        let result = lumen_common::scan::strip_and_collapse_ascii_whitespace(parts);
        match error {
            Some(problem) => Err(problem),
            None => Ok(result),
        }
    }

    /// Find or create the title element for the Document title setter.
    /// A document without an appropriate root/head leaves the assignment inert.
    pub fn ensure_title_element(&mut self) -> Result<Option<NodeId>, Error> {
        let Some(root) = selector::document_element(self) else {
            return Ok(None);
        };
        let (namespace, is_svg, is_html_root) = match self.kind(root)? {
            NodeKind::Element {
                namespace: Namespace::Svg,
                name,
                ..
            } if svg::local_name(name) == "svg" => (Namespace::Svg, true, false),
            NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            } => (Namespace::Html, false, svg::local_name(name) == "html"),
            _ => return Ok(None),
        };
        if let Some(title) = self.title_element()? {
            return Ok(Some(title));
        }
        let parent = if is_svg {
            root
        } else {
            if !is_html_root {
                return Ok(None);
            }
            let mut child = self.first_child(root)?;
            let mut head = None;
            while let Some(node) = child {
                if matches!(self.kind(node)?, NodeKind::Element { namespace: Namespace::Html, name, .. }
                    if svg::local_name(name) == "head")
                {
                    head = Some(node);
                    break;
                }
                child = self.next_sibling(node)?;
            }
            let Some(head) = head else { return Ok(None) };
            head
        };
        let title = self.create(NodeKind::Element {
            namespace,
            name: "title".into(),
            attributes: Vec::new(),
        })?;
        let inserted = if is_svg {
            self.insert_before(parent, title, self.first_child(parent)?)
        } else {
            self.append(parent, title)
        };
        if let Err(error) = inserted {
            self.destroy_subtree(title)?;
            return Err(error);
        }
        Ok(Some(title))
    }

    /// Clone a detached subtree. Only attaching the result invalidates the document.
    pub fn clone_subtree(&mut self, source: NodeId) -> Result<NodeId, Error> {
        self.clone_node(source, true)
    }

    /// Clone this Document into a new, independent document arena.
    ///
    /// Document nodes cannot be cloned into the same arena because each arena
    /// owns exactly one root. This operation preserves the document's parsing
    /// metadata and, when `deep` is true, copies its child tree in order. The
    /// partially built document is unpublished, so a node-limit or hierarchy
    /// error leaves the source untouched and drops the incomplete copy.
    pub fn clone_document(&self, deep: bool) -> Result<Document, Error> {
        let mut clone = Document::new(self.max_nodes);
        clone.document_mode = self.document_mode;
        clone.is_html_document = self.is_html_document;
        clone.scripting_enabled = self.scripting_enabled;
        clone.allow_declarative_shadow_roots = self.allow_declarative_shadow_roots;
        if !deep {
            return Ok(clone);
        }

        let mut child = self.first_child(self.root())?;
        while let Some(original) = child {
            let copy = clone.clone_subtree_from(self, original, true)?;
            clone.append(clone.root(), copy)?;
            child = self.next_sibling(original)?;
        }
        Ok(clone)
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
        source.clear_interaction_subtree(root);
        if matches!(
            source.kind(root)?,
            NodeKind::Attribute {
                owner_element: Some(_),
                ..
            }
        ) {
            return Err(Error::InUseAttribute);
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
        fn enqueue(
            pending: &mut Vec<NodeId>,
            discovered: usize,
            live_nodes: usize,
            node: NodeId,
        ) -> Result<(), Error> {
            let queued = discovered
                .checked_add(pending.len())
                .and_then(|count| count.checked_add(1))
                .ok_or(Error::LimitExceeded)?;
            if queued > live_nodes {
                return Err(Error::LimitExceeded);
            }
            pending.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
            pending.push(node);
            Ok(())
        }

        let mut pending = Vec::new();
        enqueue(&mut pending, 0, source.live_nodes, root)?;
        let mut originals = Vec::new();
        while let Some(id) = pending.pop() {
            source.node(id)?;
            originals.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
            originals.push(id);
            if let Some((_, attributes)) = source
                .attribute_nodes
                .iter()
                .find(|(owner, _)| *owner == id)
            {
                for (_, attribute) in attributes {
                    enqueue(&mut pending, originals.len(), source.live_nodes, *attribute)?;
                }
            }
            if let Some(content) = source.template_content(id)? {
                enqueue(&mut pending, originals.len(), source.live_nodes, content)?;
            }
            if let Some(shadow) = source.shadow_root(id)? {
                enqueue(&mut pending, originals.len(), source.live_nodes, shadow)?;
            }
            let mut child = source.first_child(id)?;
            while let Some(next) = child {
                enqueue(&mut pending, originals.len(), source.live_nodes, next)?;
                child = source.next_sibling(next)?;
            }
        }

        // Keep the returned map in host-including traversal order, while using this compact sorted
        // index only for membership and link remapping. A document's arena can be much larger than
        // a subtree after removals, so neither scratch structure scales with the arena high-water
        // mark. The source-node bound above prevents malformed cyclic links from growing the work
        // queue beyond the live document.
        let mut lookup = lumen_common::limits::size::vec_with_capacity::<(u32, usize)>(
            originals.len(),
            source.live_nodes,
        )
        .map_err(|_| Error::LimitExceeded)?;
        lookup.extend(
            originals
                .iter()
                .enumerate()
                .map(|(position, node)| (node.index, position)),
        );
        lookup.sort_unstable_by_key(|(index, _)| *index);
        if lookup.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(Error::Hierarchy);
        }
        let lookup_position = |index: u32| {
            lookup
                .binary_search_by_key(&index, |(candidate, _)| *candidate)
                .ok()
                .map(|entry| lookup[entry].1)
        };
        let contains_node = |node: NodeId| {
            lookup_position(node.index).is_some_and(|position| originals[position] == node)
        };
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
        self.attribute_nodes
            .try_reserve(source.attribute_nodes.len())
            .map_err(|_| Error::LimitExceeded)?;
        self.doctype_identifiers
            .try_reserve(source.doctype_identifiers.len())
            .map_err(|_| Error::LimitExceeded)?;
        let moved_shadows = source
            .shadow_trees
            .iter()
            .filter(|tree| contains_node(tree.host))
            .count();
        let mut remaining_shadows = Vec::new();
        remaining_shadows
            .try_reserve(source.shadow_trees.len().saturating_sub(moved_shadows))
            .map_err(|_| Error::LimitExceeded)?;
        let mut remaining_manual_assignments = Vec::new();
        remaining_manual_assignments
            .try_reserve(source.manual_assignments.len())
            .map_err(|_| Error::LimitExceeded)?;
        self.shadow_trees
            .try_reserve(moved_shadows)
            .map_err(|_| Error::LimitExceeded)?;
        self.manual_assignments
            .try_reserve(source.manual_assignments.len())
            .map_err(|_| Error::LimitExceeded)?;
        let mut remaining_namespaces = Vec::new();
        remaining_namespaces
            .try_reserve(source.attribute_namespaces.len())
            .map_err(|_| Error::LimitExceeded)?;
        let mut remaining_attribute_nodes = Vec::new();
        remaining_attribute_nodes
            .try_reserve(source.attribute_nodes.len())
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
        let moved_dialog_returns = source
            .dialog_return_values
            .iter()
            .filter(|(owner, _)| contains_node(*owner))
            .count();
        if self
            .dialog_return_values
            .len()
            .checked_add(moved_dialog_returns)
            .is_none_or(|count| count > self.max_nodes)
        {
            return Err(Error::LimitExceeded);
        }
        let target_dialog_bytes = self
            .dialog_return_values
            .iter()
            .try_fold(0usize, |total, (_, value)| {
                lumen_common::limits::size::sum(total, value.len(), html::MAX_HTML_BYTES).ok()
            })
            .ok_or(Error::LimitExceeded)?;
        let _adopted_dialog_bytes = source
            .dialog_return_values
            .iter()
            .filter(|(owner, _)| contains_node(*owner))
            .try_fold(target_dialog_bytes, |total, (_, value)| {
                lumen_common::limits::size::sum(total, value.len(), html::MAX_HTML_BYTES).ok()
            })
            .ok_or(Error::LimitExceeded)?;
        self.dialog_return_values
            .try_reserve(moved_dialog_returns)
            .map_err(|_| Error::LimitExceeded)?;
        let mut remaining_dialog_returns = Vec::new();
        remaining_dialog_returns
            .try_reserve(source.dialog_return_values.len() - moved_dialog_returns)
            .map_err(|_| Error::LimitExceeded)?;
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
            mapping.push((
                *original,
                NodeId {
                    document: self.id,
                    index,
                    generation,
                },
            ));
        }
        let mapped_node = |node: NodeId| {
            lookup_position(node.index)
                .filter(|position| mapping[*position].0 == node)
                .map(|position| mapping[position].1)
        };
        drop(originals);

        for (owner, value) in core::mem::take(&mut source.dialog_return_values) {
            if let Some(adopted_owner) = mapped_node(owner) {
                self.dialog_return_values.push((adopted_owner, value));
            } else {
                remaining_dialog_returns.push((owner, value));
            }
        }
        source.dialog_return_values = remaining_dialog_returns;

        let map_index = |index: u32| {
            if index == NO_LINK {
                NO_LINK
            } else {
                let position = lookup_position(index)
                    .expect("adoption subtree must contain every linked node");
                mapping[position].1.index
            }
        };
        for &(source_id, target_id) in &mapping {
            let old_generation = source.nodes[source_id.index()].generation;
            let mut moved = core::mem::replace(
                &mut source.nodes[source_id.index()],
                Node::new(NodeKind::DocumentFragment, old_generation),
            );
            moved.generation = target_id.generation;
            moved.parent = map_index(moved.parent);
            moved.first_child = map_index(moved.first_child);
            moved.last_child = map_index(moved.last_child);
            moved.prev_sibling = map_index(moved.prev_sibling);
            moved.next_sibling = map_index(moved.next_sibling);
            moved.template_content = if detached_template_host.is_some() && source_id == root {
                NO_LINK
            } else {
                map_index(moved.template_content)
            };
            if let NodeKind::Attribute {
                owner_element: Some(owner),
                ..
            } = &mut moved.kind
            {
                **owner = mapped_node(**owner).expect("attribute owner is in adoption subtree");
            }
            self.nodes[target_id.index as usize] = moved;

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
            if let Some(target) = mapped_node(owner) {
                self.attribute_namespaces.push((target, namespaces));
            } else {
                remaining_namespaces.push((owner, namespaces));
            }
        }
        source.attribute_namespaces = remaining_namespaces;
        let mut source_attribute_nodes = core::mem::take(&mut source.attribute_nodes);
        for (owner, mut nodes) in source_attribute_nodes.drain(..) {
            if let Some(target_owner) = mapped_node(owner) {
                for (_, attribute) in &mut nodes {
                    *attribute =
                        mapped_node(*attribute).expect("materialized Attr is in adoption subtree");
                }
                self.attribute_nodes.push((target_owner, nodes));
            } else {
                remaining_attribute_nodes.push((owner, nodes));
            }
        }
        source.attribute_nodes = remaining_attribute_nodes;
        for (owner, public_id, system_id) in core::mem::take(&mut source.doctype_identifiers) {
            if let Some(target) = mapped_node(owner) {
                self.doctype_identifiers
                    .push((target, public_id, system_id));
            } else {
                remaining_doctypes.push((owner, public_id, system_id));
            }
        }
        source.doctype_identifiers = remaining_doctypes;
        for tree in core::mem::take(&mut source.shadow_trees) {
            if mapped_node(tree.host).is_some() {
                self.shadow_trees.push(shadow::ShadowTree {
                    host: mapped_node(tree.host).expect("shadow host is in adoption subtree"),
                    root: mapped_node(tree.root).expect("shadow root is in adoption subtree"),
                    mode: tree.mode,
                    options: tree.options,
                });
            } else {
                remaining_shadows.push(tree);
            }
        }
        source.shadow_trees = remaining_shadows;
        for (slot, mut nodes) in core::mem::take(&mut source.manual_assignments) {
            if let Some(new_slot) = mapped_node(slot) {
                nodes.retain_mut(|node| {
                    let Some(mapped) = mapped_node(*node) else {
                        return false;
                    };
                    *node = mapped;
                    true
                });
                self.manual_assignments.push((new_slot, nodes));
            } else {
                nodes.retain(|node| mapped_node(*node).is_none());
                if !nodes.is_empty() {
                    remaining_manual_assignments.push((slot, nodes));
                }
            }
        }
        source.manual_assignments = remaining_manual_assignments;

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

        let adopted_root = mapped_node(root).expect("adoption root is in mapping");
        self.mark_dirty(adopted_root, Dirty::STYLE, MutationKind::FullRebuild);
        source.mark_dirty(source.root(), Dirty::STYLE, MutationKind::FullRebuild);
        Ok((adopted_root, mapping))
    }

    fn clone_shallow_from(&mut self, source: &Document, node: NodeId) -> Result<NodeId, Error> {
        let kind = match source.kind(node)? {
            NodeKind::Attribute {
                namespace_uri,
                qualified_name,
                value,
                ..
            } => NodeKind::Attribute {
                namespace_uri: namespace_uri.clone(),
                qualified_name: qualified_name.clone(),
                value: value.clone(),
                owner_element: None,
            },
            kind => kind.clone(),
        };
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
        let clone = self.create_without_details_steps(kind)?;
        if !namespaces.is_empty() {
            self.attribute_namespaces.push((clone, namespaces));
        }
        if let Some((public_id, system_id)) = doctype_identifiers {
            self.doctype_identifiers.push((clone, public_id, system_id));
        }
        self.details_created(clone)?;
        Ok(clone)
    }

    fn attach_detached(&mut self, parent: NodeId, child: NodeId) {
        self.insert_detached_before(parent, child, None);
    }

    fn insert_detached_before(&mut self, parent: NodeId, child: NodeId, before: Option<NodeId>) {
        // Parser insertions into a live document do not advance `version`, so the id index
        // cannot be trusted across them.
        let index = self.id_index.get_mut();
        index.built_version = None;
        index.walked_version = None;
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
        self.details_inserted(child)
            .expect("validated detached insertion has a valid ordinary tree");
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

    /// Number of attributes on an Element, preserving its native attribute-list order.
    pub fn attribute_count(&self, id: NodeId) -> Result<usize, Error> {
        let NodeKind::Element { attributes, .. } = &self.node(id)?.kind else {
            return Err(Error::WrongKind);
        };
        Ok(attributes.len())
    }

    /// Return the ordered supported named-property names for an Element's NamedNodeMap.
    ///
    /// This reads the compact attribute list directly and never materializes Attr nodes. Names
    /// are deduplicated in first-attribute order because namespace-distinct attributes can share
    /// a qualified name. HTML elements in HTML documents expose only ASCII-lowercase names;
    /// foreign elements and XML documents preserve the qualified names as written.
    pub fn supported_attribute_names(&self, id: NodeId) -> Result<Vec<String>, Error> {
        let NodeKind::Element {
            namespace,
            attributes,
            ..
        } = &self.node(id)?.kind
        else {
            return Err(Error::WrongKind);
        };
        let lowercase_only = self.is_html_document && *namespace == Namespace::Html;
        let mut names = Vec::new();
        const LINEAR_DEDUP_LIMIT: usize = 16;
        if attributes.len() <= LINEAR_DEDUP_LIMIT {
            names
                .try_reserve(attributes.len())
                .map_err(|_| Error::LimitExceeded)?;
            for (attribute_name, _) in attributes {
                let name = attribute_name.as_str();
                if lowercase_only && name.bytes().any(|byte| byte.is_ascii_uppercase()) {
                    continue;
                }
                if !names.iter().any(|known| known == name) {
                    names.push(String::from(name));
                }
            }
            return Ok(names);
        }

        // For a large map, sort borrowed attribute positions rather than allocating copied
        // strings for a temporary set. Ties include the original position so compaction keeps
        // the first occurrence; sorting the compacted positions restores attribute-list order.
        let mut positions = Vec::new();
        positions
            .try_reserve(attributes.len())
            .map_err(|_| Error::LimitExceeded)?;
        for (index, (attribute_name, _)) in attributes.iter().enumerate() {
            let name = attribute_name.as_str();
            if !lowercase_only || !name.bytes().any(|byte| byte.is_ascii_uppercase()) {
                positions.push(index);
            }
        }
        positions.sort_unstable_by(|left, right| {
            attributes[*left]
                .0
                .cmp(&attributes[*right].0)
                .then_with(|| left.cmp(right))
        });
        let mut unique = 0;
        for read in 0..positions.len() {
            if read == 0 || attributes[positions[read]].0 != attributes[positions[read - 1]].0 {
                positions[unique] = positions[read];
                unique += 1;
            }
        }
        positions.truncate(unique);
        positions.sort_unstable();
        names
            .try_reserve(positions.len())
            .map_err(|_| Error::LimitExceeded)?;
        for index in positions {
            names.push(String::from(attributes[index].0.as_str()));
        }
        Ok(names)
    }

    /// Check one supported NamedNodeMap property name without allocating or materializing Attr.
    pub fn supports_attribute_name(&self, id: NodeId, name: &str) -> Result<bool, Error> {
        let NodeKind::Element {
            namespace,
            attributes,
            ..
        } = &self.node(id)?.kind
        else {
            return Err(Error::WrongKind);
        };
        if self.is_html_document
            && *namespace == Namespace::Html
            && name.bytes().any(|byte| byte.is_ascii_uppercase())
        {
            return Ok(false);
        }
        Ok(attributes
            .iter()
            .any(|(attribute_name, _)| attribute_name.as_str() == name))
    }

    /// Return the lazily materialized Attr node at `index`. Parser-created and otherwise
    /// unobserved attributes do not allocate Node arena entries.
    pub fn attribute_node_at(&mut self, id: NodeId, index: usize) -> Result<Option<NodeId>, Error> {
        let (qualified_name, value, namespace_uri) = {
            let NodeKind::Element { attributes, .. } = &self.node(id)?.kind else {
                return Err(Error::WrongKind);
            };
            let Some((name, value)) = attributes.get(index) else {
                return Ok(None);
            };
            (
                name.clone(),
                value.clone(),
                self.attribute_namespace_uri_at(id, index).map(Rc::from),
            )
        };
        if let Some(existing) = self
            .attribute_nodes
            .iter()
            .find(|(owner, _)| *owner == id)
            .and_then(|(_, nodes)| {
                nodes
                    .binary_search_by_key(&index, |(known_index, _)| *known_index)
                    .ok()
                    .map(|position| nodes[position].1)
            })
        {
            return Ok(Some(existing));
        }

        let sidecar_index = self
            .attribute_nodes
            .iter()
            .position(|(owner, _)| *owner == id);
        let mut new_sidecar = None;
        if let Some(sidecar_index) = sidecar_index {
            self.attribute_nodes[sidecar_index]
                .1
                .try_reserve(1)
                .map_err(|_| Error::LimitExceeded)?;
        } else {
            self.attribute_nodes
                .try_reserve(1)
                .map_err(|_| Error::LimitExceeded)?;
            let mut nodes = Vec::new();
            nodes.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
            new_sidecar = Some(nodes);
        }
        let attribute = self.create(NodeKind::Attribute {
            namespace_uri: namespace_uri.map(Box::new),
            qualified_name,
            value,
            owner_element: None,
        })?;
        if let Some(sidecar_index) = sidecar_index {
            let nodes = &mut self.attribute_nodes[sidecar_index].1;
            let position = nodes.partition_point(|(known_index, _)| *known_index < index);
            nodes.insert(position, (index, attribute));
        } else {
            let mut nodes = new_sidecar.expect("preallocated Attr sidecar");
            nodes.push((index, attribute));
            self.attribute_nodes.push((id, nodes));
        }
        if let NodeKind::Attribute { owner_element, .. } = &mut self.node_mut(attribute).kind {
            *owner_element = Some(Box::new(id));
        }
        Ok(Some(attribute))
    }

    /// Return the materialized Attr node for a qualified name, allocating it only on a hit.
    pub fn attribute_node_by_name(
        &mut self,
        id: NodeId,
        qualified_name: &str,
    ) -> Result<Option<NodeId>, Error> {
        let index = {
            let NodeKind::Element { attributes, .. } = &self.node(id)?.kind else {
                return Err(Error::WrongKind);
            };
            attributes
                .iter()
                .position(|(name, _)| name.as_str() == qualified_name)
        };
        match index {
            Some(index) => self.attribute_node_at(id, index),
            None => Ok(None),
        }
    }

    /// Return the materialized Attr node for a namespace/local-name pair.
    pub fn attribute_node_by_ns(
        &mut self,
        id: NodeId,
        namespace_uri: Option<&str>,
        local_name: &str,
    ) -> Result<Option<NodeId>, Error> {
        match self.attribute_index_ns(id, namespace_uri, local_name)? {
            Some(index) => self.attribute_node_at(id, index),
            None => Ok(None),
        }
    }

    /// Return a live snapshot-free view of already materialized attribute identities.
    pub fn materialized_attribute_nodes(&self, id: NodeId) -> Option<&[(usize, NodeId)]> {
        self.attribute_nodes
            .iter()
            .find(|(owner, _)| *owner == id)
            .map(|(_, nodes)| nodes.as_slice())
    }

    /// Create a detached Attr node. Name and namespace validation belongs to the DOM binding.
    pub fn create_attribute(
        &mut self,
        namespace_uri: Option<&str>,
        qualified_name: &str,
        value: &str,
    ) -> Result<NodeId, Error> {
        self.create(NodeKind::Attribute {
            namespace_uri: namespace_uri
                .filter(|uri| !uri.is_empty())
                .map(|uri| Box::new(Rc::from(uri))),
            qualified_name: Name::new(qualified_name),
            value: String::from(value),
            owner_element: None,
        })
    }

    /// Set an existing detached Attr node on an Element and preserve its identity.
    /// `namespace_aware` selects expanded-name replacement (`setAttributeNodeNS`) rather than
    /// qualified-name replacement (`setAttributeNode`).
    pub fn set_attribute_node(
        &mut self,
        element: NodeId,
        attribute: NodeId,
        namespace_aware: bool,
    ) -> Result<Option<NodeId>, Error> {
        let details_before = self.details_open_state(element);
        let (namespace_uri, qualified_name, value, owner) = match self.kind(attribute)? {
            NodeKind::Attribute {
                namespace_uri,
                qualified_name,
                value,
                owner_element,
            } => (
                namespace_uri.as_ref().map(|uri| (**uri).clone()),
                qualified_name.clone(),
                value.clone(),
                owner_element.as_deref().copied(),
            ),
            _ => return Err(Error::WrongKind),
        };
        if owner == Some(element) {
            // setAttributeNode(attr) on the Attr already present in this element returns that
            // Attr. Do not attempt to detach/reinsert it: the ordered attribute list is already
            // correct and its identity must stay stable.
            return Ok(Some(attribute));
        }
        if owner.is_some() {
            return Err(Error::InUseAttribute);
        }
        let index = if namespace_aware {
            self.attribute_index_ns(
                element,
                namespace_uri.as_deref(),
                if namespace_uri.is_some() {
                    split_qualified_name(qualified_name.as_str()).1
                } else {
                    qualified_name.as_str()
                },
            )?
        } else {
            let NodeKind::Element { attributes, .. } = &self.node(element)?.kind else {
                return Err(Error::WrongKind);
            };
            attributes
                .iter()
                .position(|(name, _)| name == qualified_name.as_str())
        };
        let name_changed = (qualified_name == "name" && namespace_uri.is_none())
            || index.is_some_and(|index| {
                matches!(&self.node(element).expect("validated element").kind, NodeKind::Element { attributes, .. } if attributes[index].0 == "name")
                    && self.attribute_namespace_uri_at(element, index).is_none()
            });
        let append_index = if index.is_none() {
            Some(self.attribute_count(element)?)
        } else {
            None
        };
        self.ensure_attribute_node_sidecar(element)?;
        // The replaced value is observable as an Attr even when script had never requested one
        // before. Materialize only this replaced entry; ordinary Element attributes stay lazy.
        let old_attribute = match index {
            Some(index) => self.attribute_node_at(element, index)?,
            None => None,
        };
        if index.is_none() {
            let NodeKind::Element { .. } = &self.node(element)?.kind else {
                return Err(Error::WrongKind);
            };
            if let NodeKind::Element { attributes, .. } = &mut self.node_mut_checked(element)?.kind
            {
                attributes
                    .len()
                    .checked_add(1)
                    .ok_or(Error::LimitExceeded)?;
                attributes
                    .try_reserve(1)
                    .map_err(|_| Error::LimitExceeded)?;
            }
            self.attribute_nodes
                .iter_mut()
                .find(|(owner, _)| *owner == element)
                .expect("ensure inserted Attr sidecar")
                .1
                .try_reserve(1)
                .map_err(|_| Error::LimitExceeded)?;
        }
        let observing = self.observing_mutations();
        let old_value = if let Some(index) = index {
            let NodeKind::Element { attributes, .. } = &mut self.node_mut_checked(element)?.kind
            else {
                return Err(Error::WrongKind);
            };
            let old_value = core::mem::replace(&mut attributes[index].1, value.clone());
            attributes[index].0 = qualified_name.clone();
            observing.then_some(old_value)
        } else {
            let NodeKind::Element { attributes, .. } = &mut self.node_mut_checked(element)?.kind
            else {
                return Err(Error::WrongKind);
            };
            attributes.push((qualified_name.clone(), value.clone()));
            self.attribute_nodes
                .iter_mut()
                .find(|(owner, _)| *owner == element)
                .expect("ensure inserted Attr sidecar")
                .1
                .push((
                    append_index.expect("new Attr appends an attribute"),
                    attribute,
                ));
            None
        };
        if let Some(index) = index {
            self.set_attribute_namespace_at(element, index, namespace_uri.as_deref());
            let nodes = &mut self
                .attribute_nodes
                .iter_mut()
                .find(|(owner, _)| *owner == element)
                .expect("ensure inserted Attr sidecar")
                .1;
            let position = nodes.partition_point(|(known_index, _)| *known_index < index);
            if position < nodes.len() && nodes[position].0 == index {
                nodes[position].1 = attribute;
            } else {
                nodes.insert(position, (index, attribute));
            }
            self.sync_materialized_attribute(
                element,
                index,
                &qualified_name,
                &value,
                namespace_uri.as_deref(),
            );
        } else {
            let index = self.attribute_count(element)? - 1;
            self.set_attribute_namespace_at(element, index, namespace_uri.as_deref());
        }
        if let Some(old_attribute) = old_attribute {
            if let NodeKind::Attribute { owner_element, .. } =
                &mut self.node_mut(old_attribute).kind
            {
                *owner_element = None;
            }
        }
        if let NodeKind::Attribute { owner_element, .. } = &mut self.node_mut(attribute).kind {
            *owner_element = Some(Box::new(element));
        }
        self.mark_dirty(
            element,
            Dirty::STYLE,
            MutationKind::Attribute(qualified_name.clone()),
        );
        if observing {
            self.notify(observe::ObservedMutation {
                target: element,
                kind: observe::ObservedKind::Attribute {
                    name: String::from(qualified_name.as_str()),
                    namespace_uri: namespace_uri.as_ref().map(|uri| String::from(uri.as_ref())),
                    old_value,
                },
            });
        }
        self.details_attribute_changed(element, details_before, name_changed)?;
        Ok(old_attribute)
    }

    /// Detach the Attr node from its current Element while preserving the Attr identity.
    pub fn remove_attribute_node(
        &mut self,
        element: NodeId,
        attribute: NodeId,
    ) -> Result<NodeId, Error> {
        let owner = match self.kind(attribute)? {
            NodeKind::Attribute { owner_element, .. } => owner_element.as_deref().copied(),
            _ => return Err(Error::WrongKind),
        };
        if owner != Some(element) {
            return Err(Error::NotFound);
        }
        let index = self
            .attribute_nodes
            .iter()
            .find(|(candidate, _)| *candidate == element)
            .and_then(|(_, nodes)| {
                nodes
                    .iter()
                    .find_map(|(index, node)| (*node == attribute).then_some(*index))
            })
            .ok_or(Error::NotFound)?;
        self.remove_attribute_at(element, index)?;
        Ok(attribute)
    }

    /// Set `Attr.value`, updating the owning Element's attribute list and mutation stream.
    pub fn set_attribute_node_value(
        &mut self,
        attribute: NodeId,
        value: &str,
    ) -> Result<(), Error> {
        let (owner, unchanged) = match self.kind(attribute)? {
            NodeKind::Attribute {
                owner_element,
                value: current,
                ..
            } => (owner_element.as_deref().copied(), current.as_str() == value),
            _ => return Err(Error::WrongKind),
        };
        let Some(owner) = owner else {
            if unchanged {
                return Ok(());
            }
            if let NodeKind::Attribute { value: current, .. } = &mut self.node_mut(attribute).kind {
                current.clear();
                current.push_str(value);
            }
            return Ok(());
        };
        let observing = self.observing_mutations();
        if unchanged {
            if observing {
                let index = self
                    .attribute_nodes
                    .iter()
                    .find(|(element, _)| *element == owner)
                    .and_then(|(_, nodes)| {
                        nodes
                            .iter()
                            .find_map(|(index, node)| (*node == attribute).then_some(*index))
                    })
                    .ok_or(Error::WrongKind)?;
                let namespace_uri = self
                    .attribute_namespace_uri_at(owner, index)
                    .map(String::from);
                let (qualified_name, old_value) = match self.kind(attribute)? {
                    NodeKind::Attribute {
                        qualified_name,
                        value,
                        ..
                    } => (String::from(qualified_name.as_str()), value.clone()),
                    _ => return Err(Error::WrongKind),
                };
                self.notify(observe::ObservedMutation {
                    target: owner,
                    kind: observe::ObservedKind::Attribute {
                        name: qualified_name,
                        namespace_uri,
                        old_value: Some(old_value),
                    },
                });
            }
            return Ok(());
        }
        let details_before = self.details_open_state(owner);
        let qualified_name = match self.kind(attribute)? {
            NodeKind::Attribute { qualified_name, .. } => qualified_name.clone(),
            _ => return Err(Error::WrongKind),
        };
        let index = self
            .attribute_nodes
            .iter()
            .find(|(element, _)| *element == owner)
            .and_then(|(_, nodes)| {
                nodes
                    .iter()
                    .find_map(|(index, node)| (*node == attribute).then_some(*index))
            })
            .ok_or(Error::WrongKind)?;
        let namespace_uri = self
            .attribute_namespace_uri_at(owner, index)
            .map(String::from);
        let details_name_changed = qualified_name == "name" && namespace_uri.is_none();
        let old_value = {
            let NodeKind::Element { attributes, .. } = &mut self.node_mut_checked(owner)?.kind
            else {
                return Err(Error::WrongKind);
            };
            let target = &mut attributes[index].1;
            if observing {
                Some(core::mem::replace(target, String::from(value)))
            } else {
                target.clear();
                target.push_str(value);
                None
            }
        };
        if let NodeKind::Attribute { value: current, .. } = &mut self.node_mut(attribute).kind {
            current.clear();
            current.push_str(value);
        }
        self.mark_dirty(
            owner,
            Dirty::STYLE,
            MutationKind::Attribute(qualified_name.clone()),
        );
        if let Some(old_value) = old_value {
            self.notify(observe::ObservedMutation {
                target: owner,
                kind: observe::ObservedKind::Attribute {
                    name: String::from(qualified_name.as_str()),
                    namespace_uri,
                    old_value: Some(old_value),
                },
            });
        }
        self.details_attribute_changed(owner, details_before, details_name_changed)?;
        Ok(())
    }

    fn ensure_attribute_node_sidecar(&mut self, id: NodeId) -> Result<(), Error> {
        if self.attribute_nodes.iter().any(|(owner, _)| *owner == id) {
            return Ok(());
        }
        self.attribute_nodes
            .try_reserve(1)
            .map_err(|_| Error::LimitExceeded)?;
        self.attribute_nodes.push((id, Vec::new()));
        Ok(())
    }

    fn sync_materialized_attribute(
        &mut self,
        element: NodeId,
        index: usize,
        qualified_name: &Name,
        value: &str,
        namespace_uri: Option<&str>,
    ) {
        let Some(attribute) = self
            .attribute_nodes
            .iter()
            .find(|(owner, _)| *owner == element)
            .and_then(|(_, nodes)| {
                nodes
                    .binary_search_by_key(&index, |(known_index, _)| *known_index)
                    .ok()
                    .map(|position| nodes[position].1)
            })
        else {
            return;
        };
        if let NodeKind::Attribute {
            namespace_uri: current_namespace,
            qualified_name: current_name,
            value: current_value,
            owner_element,
        } = &mut self.node_mut(attribute).kind
        {
            let namespace: Option<Rc<str>> =
                namespace_uri.filter(|uri| !uri.is_empty()).map(Rc::from);
            if let (Some(current), Some(namespace)) =
                (current_namespace.as_deref_mut(), namespace.as_ref())
            {
                *current = namespace.clone();
            } else {
                *current_namespace = namespace.map(Box::new);
            }
            *current_name = qualified_name.clone();
            current_value.clear();
            current_value.push_str(value);
            if let Some(owner) = owner_element.as_deref_mut() {
                *owner = element;
            } else {
                *owner_element = Some(Box::new(element));
            }
        }
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

    fn attribute_namespace_rc_at(&self, id: NodeId, index: usize) -> Option<Rc<str>> {
        self.attribute_namespaces
            .iter()
            .find(|(owner, _)| *owner == id)
            .and_then(|(_, metadata)| {
                metadata
                    .iter()
                    .find_map(|(known, uri)| (*known == index).then(|| uri.clone()))
            })
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
        let inserting = self.prepare_insert_from(self, parent, child, before)?;
        if before == Some(child) {
            return Ok(());
        }
        for &candidate in inserting.as_slice() {
            self.insert_validated(parent, candidate, before)?;
        }
        Ok(())
    }

    /// Validate pre-insertion before a host adopts a foreign subtree.
    /// Neither document is mutated, including when validation fails.
    pub fn validate_insert_from(
        &self,
        source: &Document,
        parent: NodeId,
        child: NodeId,
        before: Option<NodeId>,
    ) -> Result<(), Error> {
        self.prepare_insert_from(source, parent, child, before)
            .map(|_| ())
    }

    fn prepare_insert_from(
        &self,
        source: &Document,
        parent: NodeId,
        child: NodeId,
        before: Option<NodeId>,
    ) -> Result<InsertingNodes, Error> {
        let parent_kind = self.kind(parent)?;
        if !matches!(
            parent_kind,
            NodeKind::Document | NodeKind::DocumentFragment | NodeKind::Element { .. }
        ) {
            return Err(Error::Hierarchy);
        }
        source.kind(child)?;
        if child == parent || source.can_be_ancestor(child) {
            let mut ancestor = Some(parent);
            while let Some(id) = ancestor {
                if id == child {
                    return Err(Error::Hierarchy);
                }
                ancestor = self.host_including_parent(id)?;
            }
        }
        if let Some(before) = before {
            if before.document != self.id || self.parent(before)? != Some(parent) {
                return Err(Error::NotFound);
            }
        }
        let inserting = source.inserting_nodes(child)?;
        if before == Some(child) {
            return Ok(inserting);
        }
        self.validate_insertion_from(source, parent, inserting.as_slice(), before, None)?;
        Ok(inserting)
    }

    fn inserting_nodes(&self, child: NodeId) -> Result<InsertingNodes, Error> {
        match self.kind(child)? {
            NodeKind::Document => Err(Error::Hierarchy),
            NodeKind::Attribute { .. } => Err(Error::Hierarchy),
            NodeKind::DocumentFragment => {
                let mut children = Vec::new();
                let mut current = self.first_child(child)?;
                while let Some(id) = current {
                    children.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
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
        self.details_inserted(child)?;
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
        self.clear_interaction_subtree(child);
        let node = self.node(child)?;
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
        let details_before = self.details_open_state(id);
        let index = {
            let NodeKind::Element { attributes, .. } = &self.node(id)?.kind else {
                return Err(Error::WrongKind);
            };
            attributes.iter().position(|(key, _)| key == name)
        };
        let namespace_uri = index.and_then(|index| self.attribute_namespace_rc_at(id, index));
        let observing = self.observing_mutations();
        let (key, old_value, changed) = if let Some(index) = index {
            let (key, old_value, changed) = {
                let NodeKind::Element { attributes, .. } = &mut self.node_mut_checked(id)?.kind
                else {
                    return Err(Error::WrongKind);
                };
                let (key, old) = &mut attributes[index];
                let changed = old.as_str() != value;
                if !changed {
                    if !observing {
                        return Ok(());
                    }
                }
                let old_value = if observing {
                    Some(core::mem::replace(old, String::from(value)))
                } else {
                    old.clear();
                    old.push_str(value);
                    None
                };
                (key.clone(), old_value, changed)
            };
            self.sync_materialized_attribute(id, index, &key, value, namespace_uri.as_deref());
            (key, old_value, changed)
        } else {
            let key = Name::new(name);
            let NodeKind::Element { attributes, .. } = &mut self.node_mut_checked(id)?.kind else {
                return Err(Error::WrongKind);
            };
            attributes
                .try_reserve(1)
                .map_err(|_| Error::LimitExceeded)?;
            attributes.push((key.clone(), String::from(value)));
            (key, None, true)
        };
        if changed {
            self.mark_dirty(id, Dirty::STYLE, MutationKind::Attribute(key));
        }
        if observing {
            self.notify(observe::ObservedMutation {
                target: id,
                kind: observe::ObservedKind::Attribute {
                    name: String::from(name),
                    namespace_uri: namespace_uri
                        .as_ref()
                        .map(|namespace_uri| String::from(namespace_uri.as_ref())),
                    old_value,
                },
            });
        }
        self.details_attribute_changed(id, details_before, name == "name" && namespace_uri.is_none())?;
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
        let details_before = self.details_open_state(id);
        let name_changed = matches!(&self.node(id)?.kind, NodeKind::Element { attributes, .. } if attributes[index].0 == "name")
            && self.attribute_namespace_uri_at(id, index).is_none();
        let namespace_uri = if self.observing_mutations() {
            self.attribute_namespace_uri_at(id, index).map(String::from)
        } else {
            None
        };
        let NodeKind::Element { attributes, .. } = &mut self.node_mut_checked(id)?.kind else {
            return Err(Error::WrongKind);
        };
        let (key, old_value) = attributes.remove(index);
        let materialized = self
            .attribute_nodes
            .iter()
            .find(|(owner, _)| *owner == id)
            .and_then(|(_, nodes)| {
                nodes
                    .binary_search_by_key(&index, |(known_index, _)| *known_index)
                    .ok()
                    .map(|position| nodes[position].1)
            });
        let mut remove_sidecar = false;
        if let Some((_, nodes)) = self
            .attribute_nodes
            .iter_mut()
            .find(|(owner, _)| *owner == id)
        {
            if let Ok(position) =
                nodes.binary_search_by_key(&index, |(known_index, _)| *known_index)
            {
                nodes.remove(position);
            }
            for (known_index, _) in nodes.iter_mut() {
                if *known_index > index {
                    *known_index -= 1;
                }
            }
            remove_sidecar = nodes.is_empty();
        }
        if let Some(attribute) = materialized {
            if let NodeKind::Attribute { owner_element, .. } = &mut self.node_mut(attribute).kind {
                *owner_element = None;
            }
        }
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
        if let Some(sidecar_index) = remove_sidecar
            .then(|| {
                self.attribute_nodes
                    .iter()
                    .position(|(owner, _)| *owner == id)
            })
            .flatten()
        {
            self.attribute_nodes.remove(sidecar_index);
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
        self.details_attribute_changed(id, details_before, name_changed)?;
        Ok(())
    }

    /// Attach metadata to an existing parser-created attribute without a mutation.
    pub fn set_attribute_namespace_metadata(
        &mut self,
        id: NodeId,
        qualified_name: &str,
        namespace_uri: Option<&str>,
    ) -> Result<(), Error> {
        let details_before = self.details_open_state(id);
        let NodeKind::Element { attributes, .. } = &self.node(id)?.kind else {
            return Err(Error::WrongKind);
        };
        let index = attributes
            .iter()
            .position(|(name, _)| name == qualified_name)
            .ok_or(Error::WrongKind)?;
        self.set_attribute_namespace_at(id, index, namespace_uri);
        let (name, value) = match &self.node(id)?.kind {
            NodeKind::Element { attributes, .. } => {
                (attributes[index].0.clone(), attributes[index].1.clone())
            }
            _ => return Err(Error::WrongKind),
        };
        self.sync_materialized_attribute(id, index, &name, &value, namespace_uri);
        self.details_attribute_changed(id, details_before, qualified_name == "name")?;
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
        let details_before = self.details_open_state(id);
        let namespace_uri = namespace_uri.filter(|uri| !uri.is_empty());
        let local = if namespace_uri.is_some() {
            split_qualified_name(qualified_name).1
        } else {
            qualified_name
        };
        let existing = self.attribute_index_ns(id, namespace_uri, local)?;
        let observing = self.observing_mutations();
        let key = Name::new(qualified_name);
        if existing.is_none() {
            if let NodeKind::Element { attributes, .. } = &mut self.node_mut_checked(id)?.kind {
                attributes
                    .try_reserve(1)
                    .map_err(|_| Error::LimitExceeded)?;
            } else {
                return Err(Error::WrongKind);
            }
        }
        let NodeKind::Element { attributes, .. } = &mut self.node_mut_checked(id)?.kind else {
            return Err(Error::WrongKind);
        };
        let mut changed = true;
        let (index, mutation_name, old_value) = if let Some(index) = existing {
            let (old_name, old) = &mut attributes[index];
            let mutation_name = old_name.clone();
            if old == value {
                if !observing {
                    return Ok(());
                }
                changed = false;
            }
            let old_value = if observing {
                Some(core::mem::replace(old, String::from(value)))
            } else {
                old.clear();
                old.push_str(value);
                None
            };
            (index, mutation_name, old_value)
        } else {
            let index = attributes.len();
            attributes.push((key.clone(), String::from(value)));
            (index, key.clone(), None)
        };
        if changed {
            self.set_attribute_namespace_at(id, index, namespace_uri);
            self.sync_materialized_attribute(id, index, &mutation_name, value, namespace_uri);
            self.mark_dirty(
                id,
                Dirty::STYLE,
                MutationKind::Attribute(mutation_name.clone()),
            );
        }
        if observing {
            self.notify(observe::ObservedMutation {
                target: id,
                kind: observe::ObservedKind::Attribute {
                    name: String::from(mutation_name.as_str()),
                    namespace_uri: namespace_uri.map(String::from),
                    old_value,
                },
            });
        }
        self.details_attribute_changed(id, details_before, qualified_name == "name" && namespace_uri.is_none())?;
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
        Ok(self
            .get_attribute_ns_ref(id, namespace_uri, local_name)?
            .map(String::from))
    }

    /// Borrow an attribute value by expanded name without allocating.
    pub fn get_attribute_ns_ref(
        &self,
        id: NodeId,
        namespace_uri: Option<&str>,
        local_name: &str,
    ) -> Result<Option<&str>, Error> {
        let index = self.attribute_index_ns(id, namespace_uri, local_name)?;
        let NodeKind::Element { attributes, .. } = &self.node(id)?.kind else {
            return Err(Error::WrongKind);
        };
        Ok(index.map(|index| attributes[index].1.as_str()))
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
            NodeKind::Text(text) | NodeKind::CData(text) | NodeKind::Comment(text) => text,
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
            NodeKind::Text(data) | NodeKind::CData(data) | NodeKind::Comment(data) => Ok(data),
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
    fn cdata_sections_share_character_data_and_text_content_algorithms() {
        let mut doc = Document::new(8);
        let root = doc.root();
        let parent = doc.create(element("root")).unwrap();
        doc.append(root, parent).unwrap();
        let cdata = doc.create(NodeKind::CData(String::from("before"))).unwrap();
        assert_eq!(doc.append(root, cdata), Err(Error::Hierarchy));
        doc.append(parent, cdata).unwrap();

        assert_eq!(doc.character_data_length(cdata), Ok(6));
        assert_eq!(doc.substring_data(cdata, 1, 3).unwrap(), "efo");
        doc.replace_data_range(cdata, 1, 3, "DATA").unwrap();
        assert_eq!(
            doc.kind(cdata).unwrap(),
            &NodeKind::CData(String::from("bDATAre"))
        );

        let mut content = String::new();
        doc.append_descendant_text(parent, &mut content).unwrap();
        assert_eq!(content, "bDATAre");
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
    fn attr_nodes_are_lazy_stable_live_and_detachable() {
        let mut doc = Document::new(16);
        let element = doc.create(element("button")).unwrap();
        doc.set_attribute(element, "data-state", "before").unwrap();
        assert!(doc.materialized_attribute_nodes(element).is_none());

        let first = doc.attribute_node_at(element, 0).unwrap().unwrap();
        assert_eq!(doc.attribute_node_at(element, 0).unwrap(), Some(first));
        assert!(matches!(
            doc.kind(first).unwrap(),
            NodeKind::Attribute {
                qualified_name,
                value,
                owner_element: Some(owner),
                ..
            } if qualified_name == "data-state" && value == "before" && **owner == element
        ));
        assert_eq!(doc.parent(first).unwrap(), None);
        assert_eq!(doc.append(element, first), Err(Error::Hierarchy));

        doc.set_attribute(element, "data-state", "after").unwrap();
        assert_eq!(doc.attribute_node_at(element, 0).unwrap(), Some(first));
        assert!(
            matches!(doc.kind(first).unwrap(), NodeKind::Attribute { value, .. } if value == "after")
        );
        doc.set_attribute_node_value(first, "from Attr").unwrap();
        assert_eq!(
            doc.get_attribute_ns(element, None, "data-state")
                .unwrap()
                .as_deref(),
            Some("from Attr")
        );

        let replacement = doc
            .create_attribute(None, "data-state", "replacement")
            .unwrap();
        assert_eq!(
            doc.set_attribute_node(element, replacement, false),
            Ok(Some(first))
        );
        assert!(
            matches!(doc.kind(first).unwrap(), NodeKind::Attribute { owner_element: None, value, .. } if value == "from Attr")
        );
        assert_eq!(
            doc.attribute_node_at(element, 0).unwrap(),
            Some(replacement)
        );
        doc.remove_attribute_node(element, replacement).unwrap();
        assert!(matches!(
            doc.kind(replacement).unwrap(),
            NodeKind::Attribute {
                owner_element: None,
                ..
            }
        ));
        assert_eq!(doc.set_attribute_node(element, first, false), Ok(None));
        assert_eq!(doc.attribute_node_at(element, 0).unwrap(), Some(first));

        doc.remove_attribute(element, "data-state").unwrap();
        doc.set_attribute(element, "data-state", "new identity")
            .unwrap();
        let fresh = doc.attribute_node_at(element, 0).unwrap().unwrap();
        assert_ne!(fresh, first);
    }

    #[test]
    fn named_attribute_names_are_ordered_deduplicated_and_lazy() {
        let mut doc = Document::new(32);
        doc.set_html_document(true);
        let html_element = doc.create(element("div")).unwrap();
        doc.set_attribute_ns(html_element, Some("urn:first"), "g:h", "one")
            .unwrap();
        doc.set_attribute_ns(html_element, Some("urn:second"), "g:h", "two")
            .unwrap();
        doc.set_attribute(html_element, "j", "three").unwrap();
        doc.set_attribute_ns(html_element, Some("urn:third"), "A:B", "hidden")
            .unwrap();

        assert_eq!(
            doc.supported_attribute_names(html_element).unwrap(),
            ["g:h", "j"]
        );
        assert!(doc.supports_attribute_name(html_element, "g:h").unwrap());
        assert!(!doc.supports_attribute_name(html_element, "A:B").unwrap());
        assert!(doc.materialized_attribute_nodes(html_element).is_none());

        let foreign_element = doc
            .create(NodeKind::Element {
                namespace: Namespace::Svg,
                name: Name::new("rect"),
                attributes: Vec::new(),
            })
            .unwrap();
        doc.set_attribute_ns(foreign_element, Some("urn:first"), "A:B", "one")
            .unwrap();
        doc.set_attribute(foreign_element, "j", "two").unwrap();
        assert_eq!(
            doc.supported_attribute_names(foreign_element).unwrap(),
            ["A:B", "j"]
        );
        assert!(doc.materialized_attribute_nodes(foreign_element).is_none());
    }

    #[test]
    fn large_named_attribute_list_deduplicates_in_first_order_without_materializing_attrs() {
        let mut doc = Document::new(8);
        doc.set_html_document(true);
        let element = doc.create(element("div")).unwrap();
        for index in 0..512 {
            let name = alloc::format!("data-{index:04}");
            doc.set_attribute_ns(element, Some("urn:first"), &name, "first")
                .unwrap();
            doc.set_attribute_ns(element, Some("urn:second"), &name, "second")
                .unwrap();
        }

        let names = doc.supported_attribute_names(element).unwrap();
        assert_eq!(names.len(), 512);
        for (index, name) in names.iter().enumerate() {
            assert_eq!(name, &alloc::format!("data-{index:04}"));
        }
        assert!(doc.materialized_attribute_nodes(element).is_none());
    }

    #[test]
    fn removing_a_sparse_attr_node_uses_its_element_attribute_index() {
        let mut doc = Document::new(16);
        let node = doc.create(element("button")).unwrap();
        doc.set_attribute(node, "first", "keep").unwrap();
        doc.set_attribute(node, "middle", "keep").unwrap();
        doc.set_attribute(node, "last", "remove").unwrap();
        let last = doc.attribute_node_at(node, 2).unwrap().unwrap();
        doc.remove_attribute_node(node, last).unwrap();
        assert_eq!(
            doc.get_attribute_ns(node, None, "first")
                .unwrap()
                .as_deref(),
            Some("keep")
        );
        assert_eq!(
            doc.get_attribute_ns(node, None, "middle")
                .unwrap()
                .as_deref(),
            Some("keep")
        );
        assert_eq!(doc.get_attribute_ns(node, None, "last").unwrap(), None);
        assert!(matches!(
            doc.kind(last).unwrap(),
            NodeKind::Attribute {
                owner_element: None,
                ..
            }
        ));
    }

    #[test]
    fn attr_nodes_preserve_namespace_identity_and_name_replacement_rules() {
        let mut doc = Document::new(16);
        let element = doc.create(element("svg")).unwrap();
        doc.set_attribute_ns(element, Some("urn:custom"), "p:href", "custom")
            .unwrap();
        doc.set_attribute_ns(element, None, "p:href", "plain")
            .unwrap();
        assert_eq!(doc.attribute_count(element).unwrap(), 2);
        let plain = doc
            .attribute_node_by_ns(element, None, "p:href")
            .unwrap()
            .unwrap();
        let namespaced = doc
            .attribute_node_by_ns(element, Some("urn:custom"), "href")
            .unwrap()
            .unwrap();
        assert_ne!(plain, namespaced);
        assert!(
            matches!(doc.kind(plain).unwrap(), NodeKind::Attribute { namespace_uri: None, value, .. } if value == "plain")
        );
        assert!(
            matches!(doc.kind(namespaced).unwrap(), NodeKind::Attribute { namespace_uri: Some(uri), qualified_name, value, .. } if uri.as_ref().as_ref() == "urn:custom" && qualified_name == "p:href" && value == "custom")
        );

        let detached = doc
            .create_attribute(Some("urn:custom"), "q:href", "replacement")
            .unwrap();
        assert_eq!(
            doc.set_attribute_node(element, detached, true),
            Ok(Some(namespaced))
        );
        assert!(matches!(
            doc.kind(namespaced).unwrap(),
            NodeKind::Attribute {
                owner_element: None,
                ..
            }
        ));
        assert_eq!(
            doc.attribute_node_by_ns(element, Some("urn:custom"), "href")
                .unwrap(),
            Some(detached)
        );
        assert_eq!(
            doc.attribute_node_by_ns(element, None, "p:href").unwrap(),
            Some(plain)
        );
    }

    #[test]
    fn replacing_unrequested_attr_materializes_old_node_and_same_node_returns_itself() {
        let mut doc = Document::new(12);
        let element = doc.create(element("div")).unwrap();
        doc.set_attribute(element, "title", "old").unwrap();
        assert!(doc.materialized_attribute_nodes(element).is_none());

        let replacement = doc.create_attribute(None, "title", "new").unwrap();
        let old = doc
            .set_attribute_node(element, replacement, false)
            .unwrap()
            .expect("replaced attributes are observable Attr nodes");
        assert!(
            matches!(doc.kind(old).unwrap(), NodeKind::Attribute { owner_element: None, value, .. } if value == "old")
        );
        assert_eq!(
            doc.attribute_node_at(element, 0).unwrap(),
            Some(replacement)
        );

        doc.set_attribute(element, "class", "other").unwrap();
        assert_eq!(
            doc.set_attribute_node(element, replacement, false),
            Ok(Some(replacement))
        );
        assert_eq!(doc.attribute_count(element), Ok(2));
    }

    #[test]
    fn attr_node_adoption_and_cloning_keep_owner_and_identity_rules() {
        let mut source = Document::new(16);
        let element = source.create(element("div")).unwrap();
        source.set_attribute(element, "id", "x").unwrap();
        let attribute = source.attribute_node_at(element, 0).unwrap().unwrap();

        let mut target = Document::new(16);
        let (adopted_element, mapping) = target.adopt_subtree_from(&mut source, element).unwrap();
        let adopted_attribute = mapping
            .iter()
            .find_map(|(old, new)| (*old == attribute).then_some(*new))
            .expect("materialized Attr is included in adoption mapping");
        assert!(
            matches!(target.kind(adopted_attribute).unwrap(), NodeKind::Attribute { owner_element: Some(owner), .. } if **owner == adopted_element)
        );
        target
            .set_attribute_node_value(adopted_attribute, "adopted")
            .unwrap();
        assert_eq!(
            target
                .get_attribute_ns(adopted_element, None, "id")
                .unwrap()
                .as_deref(),
            Some("adopted")
        );

        let cloned_element = target.clone_node(adopted_element, false).unwrap();
        let cloned_attribute = target
            .attribute_node_at(cloned_element, 0)
            .unwrap()
            .unwrap();
        assert_ne!(cloned_attribute, adopted_attribute);
        assert!(
            matches!(target.kind(cloned_attribute).unwrap(), NodeKind::Attribute { owner_element: Some(owner), .. } if **owner == cloned_element)
        );

        let detached = target
            .create_attribute(None, "title", "standalone")
            .unwrap();
        let (adopted_detached, detached_map) =
            source.adopt_subtree_from(&mut target, detached).unwrap();
        assert_eq!(adopted_detached, detached_map[0].1);
        assert!(
            matches!(source.kind(adopted_detached).unwrap(), NodeKind::Attribute { owner_element: None, value, .. } if value == "standalone")
        );
    }

    #[test]
    fn materializing_attr_nodes_obeys_the_document_node_limit() {
        let mut doc = Document::new(2);
        let element = doc.create(element("div")).unwrap();
        doc.set_attribute(element, "id", "bounded").unwrap();
        assert_eq!(doc.attribute_node_at(element, 0), Err(Error::LimitExceeded));
        assert_eq!(doc.attribute_count(element), Ok(1));
    }

    #[test]
    fn materialized_attr_sidecar_stays_sparse_and_tracks_attribute_order() {
        let mut doc = Document::new(8);
        let element = doc.create(element("div")).unwrap();
        for index in 0..1024 {
            doc.set_attribute(element, &alloc::format!("data-{index}"), "v")
                .unwrap();
        }

        let high = doc.attribute_node_at(element, 900).unwrap().unwrap();
        let low = doc.attribute_node_at(element, 500).unwrap().unwrap();
        let sidecar = doc.materialized_attribute_nodes(element).unwrap();
        assert_eq!(sidecar.len(), 2);
        assert_eq!(sidecar[0], (500, low));
        assert_eq!(sidecar[1], (900, high));

        doc.remove_attribute(element, "data-10").unwrap();
        let sidecar = doc.materialized_attribute_nodes(element).unwrap();
        assert_eq!(sidecar, &[(499, low), (899, high)]);

        doc.set_attribute(element, "data-tail", "v").unwrap();
        let tail = doc.attribute_node_at(element, 1023).unwrap().unwrap();
        let sidecar = doc.materialized_attribute_nodes(element).unwrap();
        assert_eq!(sidecar.len(), 3);
        assert_eq!(sidecar[2], (1023, tail));
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
    fn attached_attr_same_value_notifies_without_style_invalidation() {
        let mut doc = Document::new(8);
        let node = doc.create(element("div")).unwrap();
        doc.set_attribute(node, "data-state", "same").unwrap();
        let attr = doc.attribute_node_at(node, 0).unwrap().unwrap();
        let records = Rc::new(core::cell::RefCell::new(Vec::new()));
        let captured = records.clone();
        doc.set_mutation_sink(Some(Rc::new(move |_, mutation| {
            let observe::ObservedKind::Attribute {
                name,
                namespace_uri,
                old_value,
            } = &mutation.kind
            else {
                panic!("unexpected mutation")
            };
            captured
                .borrow_mut()
                .push((name.clone(), namespace_uri.clone(), old_value.clone()));
        })));
        doc.clear_mutations();
        let version = doc.version();

        doc.set_attribute_node_value(attr, "same").unwrap();

        assert_eq!(
            records.borrow().as_slice(),
            &[(String::from("data-state"), None, Some(String::from("same")),)]
        );
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
    fn insertion_validation_preserves_foreign_tree_on_document_hierarchy_failure() {
        let mut target = Document::new(16);
        let html = target.create(element("html")).unwrap();
        target.append(target.root(), html).unwrap();
        let mut source = Document::new(16);
        let donor = source.create(element("section")).unwrap();
        source.append(source.root(), donor).unwrap();
        let child = source.create(element("span")).unwrap();
        source.append(donor, child).unwrap();
        let versions = (target.version(), source.version());
        assert_eq!(
            target.validate_insert_from(&source, target.root(), donor, None),
            Err(Error::Hierarchy)
        );
        assert_eq!(
            target.validate_insert_from(&source, html, source.root(), None),
            Err(Error::Hierarchy)
        );
        assert_eq!((target.version(), source.version()), versions);
        assert_eq!(source.parent(donor).unwrap(), Some(source.root()));
        assert_eq!(source.parent(child).unwrap(), Some(donor));
        target
            .validate_insert_from(&source, html, donor, None)
            .unwrap();
        assert_eq!((target.version(), source.version()), versions);
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
    fn document_title_uses_namespace_tree_order_and_direct_text_without_temporary_text() {
        let mut document =
            html::parse("<!doctype html><title></title><title>later</title>", 48).unwrap();
        let first = document.title_element().unwrap().unwrap();
        let a = document.create(NodeKind::Text(" \tA".into())).unwrap();
        let comment = document
            .create(NodeKind::Comment("ignored".into()))
            .unwrap();
        let nested = document
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: "span".into(),
                attributes: Vec::new(),
            })
            .unwrap();
        let nested_text = document
            .create(NodeKind::Text("not child text".into()))
            .unwrap();
        document.append(nested, nested_text).unwrap();
        let b = document
            .create(NodeKind::Text("B\n\u{00a0}\u{000b}C\r ".into()))
            .unwrap();
        for child in [a, comment, nested, b] {
            document.append(first, child).unwrap();
        }
        assert_eq!(document.title().unwrap(), "AB \u{00a0}\u{000b}C");
        assert_eq!(document.ensure_title_element().unwrap(), Some(first));

        let mut empty = html::parse("<!doctype html><head></head><body></body>", 24).unwrap();
        assert_eq!(empty.title().unwrap(), "");
        let created = empty.ensure_title_element().unwrap().unwrap();
        assert!(
            matches!(empty.kind(empty.parent(created).unwrap().unwrap()),
            Ok(NodeKind::Element { namespace: Namespace::Html, name, .. }) if name == "head")
        );
        assert_eq!(empty.ensure_title_element().unwrap(), Some(created));

        let mut svg = xml::parse("<svg xmlns='http://www.w3.org/2000/svg'><g><title>nested</title></g><title>direct<![CDATA[ \n text]]></title></svg>", 32).unwrap();
        assert_eq!(svg.title().unwrap(), "direct text");
        let old_title = svg.title_element().unwrap().unwrap();
        svg.remove(old_title).unwrap();
        assert_eq!(svg.title().unwrap(), "");
        let replacement = svg.ensure_title_element().unwrap().unwrap();
        let root = selector::document_element(&svg).unwrap();
        assert_eq!(svg.first_child(root).unwrap(), Some(replacement));
        assert!(matches!(svg.kind(replacement), Ok(NodeKind::Element {
            namespace: Namespace::Svg, name, ..
        }) if name == "title"));

        let mut unrelated = xml::parse("<root><title>other namespace</title></root>", 16).unwrap();
        let version = unrelated.version();
        assert_eq!(unrelated.title().unwrap(), "");
        assert_eq!(unrelated.ensure_title_element().unwrap(), None);
        assert_eq!(unrelated.version(), version);
    }

    #[test]
    fn document_clone_copies_metadata_children_and_doctype_identifiers() {
        let mut source = Document::new(24);
        source.set_html_document(true);
        source.set_document_mode(DocumentMode::LimitedQuirks);
        let root = source.root();
        let doctype = source
            .create(NodeKind::DocumentType(String::from("html")))
            .unwrap();
        source
            .set_doctype_identifiers(doctype, "public-id", "system-id")
            .unwrap();
        let html = source.create(element("html")).unwrap();
        source
            .set_attribute_ns(html, Some("urn:clone"), "p:state", "kept")
            .unwrap();
        let text = source.create(NodeKind::Text(String::from("body"))).unwrap();
        source.append(root, doctype).unwrap();
        source.append(root, html).unwrap();
        source.append(html, text).unwrap();
        let source_attr = source
            .attribute_node_by_ns(html, Some("urn:clone"), "state")
            .unwrap()
            .unwrap();

        let shallow = source.clone_document(false).unwrap();
        assert_ne!(source.root(), shallow.root());
        assert!(shallow.is_html_document());
        assert_eq!(shallow.document_mode(), DocumentMode::LimitedQuirks);
        assert_eq!(shallow.first_child(shallow.root()), Ok(None));

        let mut clone = source.clone_document(true).unwrap();
        assert_ne!(source.root(), clone.root());
        assert!(clone.is_html_document());
        assert_eq!(clone.document_mode(), DocumentMode::LimitedQuirks);
        let clone_doctype = clone.first_child(clone.root()).unwrap().unwrap();
        assert_eq!(
            clone.kind(clone_doctype),
            Ok(&NodeKind::DocumentType("html".into()))
        );
        assert_eq!(clone.doctype_public_id(clone_doctype), Ok("public-id"));
        assert_eq!(clone.doctype_system_id(clone_doctype), Ok("system-id"));
        let clone_html = clone.next_sibling(clone_doctype).unwrap().unwrap();
        assert_eq!(
            clone.get_attribute_ns(clone_html, Some("urn:clone"), "state"),
            Ok(Some("kept".into()))
        );
        let clone_attr = clone
            .attribute_node_by_ns(clone_html, Some("urn:clone"), "state")
            .unwrap()
            .unwrap();
        assert_ne!(source_attr, clone_attr);
        let clone_text = clone.first_child(clone_html).unwrap().unwrap();
        assert_eq!(clone.kind(clone_text), Ok(&NodeKind::Text("body".into())));
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
            slot_assignment: crate::shadow::SlotAssignmentMode::Named,
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
    fn cross_document_adoption_from_sparse_arena_preserves_live_nodes_and_limit_atomicity() {
        let mut source = Document::new(1024);
        let survivor = source.create(element("aside")).unwrap();
        source.append(source.root(), survivor).unwrap();
        let mut arena_donor = Document::new(512);
        let mut temporaries = Vec::new();
        for index in 0..256 {
            temporaries.push(
                source
                    .create(NodeKind::Text(alloc::format!("temporary-{index}")))
                    .unwrap(),
            );
        }
        for temporary in temporaries {
            arena_donor
                .adopt_subtree_from(&mut source, temporary)
                .unwrap();
        }

        let host = source.create(element("section")).unwrap();
        source.set_attribute(host, "id", "adopted").unwrap();
        let child = source.create(element("span")).unwrap();
        source.append(host, child).unwrap();
        let attribute = source.attribute_node_at(host, 0).unwrap().unwrap();
        source.append(survivor, host).unwrap();

        let mut too_small = Document::new(3);
        let source_version = source.version();
        let source_count = source.node_count();
        assert_eq!(
            too_small.adopt_subtree_from(&mut source, host),
            Err(Error::LimitExceeded)
        );
        assert_eq!(source.version(), source_version);
        assert_eq!(source.node_count(), source_count);
        assert_eq!(source.parent(host).unwrap(), Some(survivor));
        assert_eq!(source.parent(child).unwrap(), Some(host));
        assert_eq!(source.parent(survivor).unwrap(), Some(source.root()));
        assert_eq!(too_small.node_count(), 1);

        let mut destination = Document::new(8);
        let (adopted, mapping) = destination.adopt_subtree_from(&mut source, host).unwrap();
        let migrated = |old| {
            mapping
                .iter()
                .find_map(|(before, after)| (*before == old).then_some(*after))
                .expect("every live node in the adopted subtree has a new identity")
        };
        let adopted_child = migrated(child);
        let adopted_attribute = migrated(attribute);
        assert_eq!(mapping[0].0, host);
        assert_eq!(mapping[1].0, child);
        assert_eq!(mapping[2].0, attribute);
        assert_eq!(adopted, migrated(host));
        assert_eq!(mapping.len(), 3);
        assert_eq!(destination.parent(adopted).unwrap(), None);
        assert_eq!(destination.parent(adopted_child).unwrap(), Some(adopted));
        assert_eq!(
            destination.attribute_node_at(adopted, 0).unwrap(),
            Some(adopted_attribute)
        );
        assert_eq!(source.parent(survivor).unwrap(), Some(source.root()));
        assert_eq!(source.first_child(source.root()).unwrap(), Some(survivor));
        assert_eq!(source.first_child(survivor).unwrap(), None);
        assert!(source.kind(host).is_err());
    }

    #[test]
    fn cross_document_adoption_preserves_manual_slot_assignments() {
        let mut source = Document::new(16);
        let host = source.create(element("div")).unwrap();
        source.append(source.root(), host).unwrap();
        let light_child = source.create(element("span")).unwrap();
        source.append(host, light_child).unwrap();
        let shadow = source
            .attach_shadow_with_options(
                host,
                ShadowOptions {
                    mode: ShadowMode::Open,
                    slot_assignment: SlotAssignmentMode::Manual,
                    delegates_focus: false,
                    clonable: false,
                    serializable: false,
                    declarative: false,
                },
            )
            .unwrap();
        let slot = source.create(element("slot")).unwrap();
        source.append(shadow, slot).unwrap();
        assert_eq!(source.assign_slot(slot, &[light_child]), Ok(true));

        let mut target = Document::new(16);
        let (adopted_host, mapping) = target.adopt_subtree_from(&mut source, host).unwrap();
        let migrated = |old| {
            mapping
                .iter()
                .find_map(|(before, after)| (*before == old).then_some(*after))
                .expect("host-including descendants are part of the adoption map")
        };
        let adopted_light_child = migrated(light_child);
        let adopted_shadow = migrated(shadow);
        let adopted_slot = migrated(slot);

        assert_eq!(target.shadow_root(adopted_host), Ok(Some(adopted_shadow)));
        assert_eq!(target.first_child(adopted_shadow), Ok(Some(adopted_slot)));
        assert_eq!(
            target.assigned_nodes(adopted_slot, false),
            Ok(alloc::vec![adopted_light_child])
        );
        assert_eq!(
            target.assigned_slot(adopted_light_child),
            Ok(Some(adopted_slot))
        );
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
