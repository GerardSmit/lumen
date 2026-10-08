//! Host-independent HTML document core. Parsing, style, layout, and paint derive from this tree.
#![no_std]

extern crate alloc;

pub mod animation;
pub mod css;
pub mod debug;
pub mod details;
pub mod directionality;
pub mod element_metadata;
pub mod equality;
pub mod font_display;
pub mod forms;
pub(crate) mod control_appearance;
pub mod tables;
pub mod invokers;
pub mod focus;
pub mod graph;
pub mod headings;
pub mod top_layer;
pub mod interaction;
pub mod labels;
pub mod language;
pub mod html;
pub mod parser_documents;
pub mod layout;
mod name;
mod named_colors;
pub mod observe;
pub mod object;
pub mod responsive_images;
pub mod paint;
pub mod render_capture;
pub mod ranges;
pub mod highlights;
pub mod rendered_text;
pub mod selector;
pub mod session;
pub mod stylesheet_loading;
pub mod shadow;
pub mod svg;
pub mod svg_dom;
pub mod xml;
pub mod xml_stylesheet;
pub use name::Name;
pub use shadow::{ShadowMode, ShadowOptions, SlotAssignmentMode};

use alloc::{boxed::Box, rc::Rc, string::String, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

static NEXT_DOCUMENT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct NodeId {
    document: u64,
    index: u32,
    generation: u32,
}

impl NodeId {
    pub const fn document_id(self) -> u64 { self.document }
    pub const fn index(self) -> usize {
        self.index as usize
    }

    /// An opaque key unique to this node of this document, for hosts that need to record node
    /// identity without depending on `NodeId`.
    pub const fn key(self) -> u128 {
        ((self.document as u128) << 64) | ((self.index as u128) << 32) | self.generation as u128
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
    style_attribute_blocked: bool,
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
            style_attribute_blocked: false,
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
    pub(crate) registered_custom_properties: Vec<css::registered_properties::RegisteredCustomProperty>,
    pub(crate) highlights: Vec<highlights::Highlight>,
    id: u64,
    nodes: Vec<Node>,
    attribute_namespaces: Vec<(NodeId, Vec<(usize, Rc<str>)>)>,
    /// Lazily materialized Attr nodes, aligned with each Element's compact attribute vector.
    /// No sidecar is allocated for parser-created attributes until the host requests a Node.
    // Only materialized Attr identities are stored here. Keep each owner's entries sparse so
    // exposing one Attr on an Element with a large attribute list does not allocate a slot for
    // every parsed attribute.
    attribute_nodes: Vec<(NodeId, Vec<(usize, NodeId)>)>,
    /// Rare createElement names containing a literal colon have no namespace prefix.
    /// Sorted arena positions distinguish them from createElementNS qualified names.
    literal_colon_names: Vec<NodeId>,
    /// Immutable HTML custom-element birth values. Ordinary elements have no
    /// entry; clones share strings and adoption moves entries with their IDs.
    custom_element_is_values: Vec<(NodeId, Rc<str>)>,
    cryptographic_nonces: Vec<(NodeId, Rc<str>)>,
    cryptographic_nonce_bytes: usize,
    hide_connected_nonces: bool,
    /// One lazily created inert HTML document shares this arena. Only detached
    /// roots born in it need records; connected descendants inherit ownership.
    template_owner_document: Option<NodeId>,
    inert_document_roots: Vec<NodeId>,
    /// Only enabled logical Document owners need an editability record.
    design_mode_documents: Vec<NodeId>,
    /// Parsed declarations exist only for CSSOM-mutated elements. Shared pending
    /// shorthand values cannot always round-trip through the style attribute.
    inline_cssom_styles: Vec<(NodeId, Rc<css::DeclarationBlock>)>,
    /// One ordinary attribute parse shared by read-only CSSOM operations.
    inline_cssom_read_cache: Option<(NodeId, Rc<css::DeclarationBlock>)>,
    doctype_identifiers: Vec<(NodeId, String, String)>,
    document_mode: DocumentMode,
    is_html_document: bool,
    language_defaults: Option<Box<language::DefaultLanguages>>,
    scripting_enabled: bool,
    allow_declarative_shadow_roots: bool,
    free: Vec<u32>,
    live_nodes: usize,
    version: u64,
    max_nodes: usize,
    allocation_budget: Option<Rc<dyn Fn(&Document, usize, usize) -> Result<(), Error>>>,
    template_ancestor_resolver: Option<Rc<dyn Fn(&Document, NodeId, NodeId) -> Result<bool, Error>>>,
    allocation_budget_group: Option<Rc<()>>,
    node_count_tracking: Option<Rc<core::cell::Cell<usize>>>,
    journal: Vec<Mutation>,
    mutation_sink: Option<Rc<dyn Fn(&Document, &observe::ObservedMutation)>>,
    mutation_observer_sink: Option<Rc<dyn Fn(&Document, &observe::ObservedMutation)>>,
    details_sink: Option<Rc<dyn Fn(&Document, details::DetailsTransition)>>,
    shadow_mutation_sink: Option<Rc<dyn Fn(&Document, &observe::ObservedMutation)>>,
    validity_resolver: Option<ValidityResolver>,
    custom_state_resolver: Option<Rc<dyn Fn(NodeId, &str) -> bool>>,
    form_selector_state_resolver:
        Option<Rc<dyn Fn(&Document, NodeId) -> Option<forms::FormSelectorState>>>,
    /// An embedding registry can reject shadow attachment from its compact
    /// custom-element definition policy; the callback contains no script values.
    shadow_host_policy: Option<Rc<dyn Fn(&Document, NodeId) -> bool>>,
    inline_stylesheet_policy: Option<Rc<dyn Fn(&Document, NodeId, &str) -> bool>>,
    inline_style_attribute_policy: Option<Rc<dyn Fn(&Document, NodeId, &str) -> Result<bool, Error>>>,
    parser_element_completion_sink: Option<Rc<dyn Fn(&Document, NodeId)>>,
    parser_style_block_sink: Option<Rc<dyn Fn(&Document, NodeId) -> Result<(), Error>>>,
    parser_custom_element_predicate: Option<Rc<dyn Fn(&Document, NodeId, &str, Option<&str>) -> bool>>,
    parser_element_birth_sink: Option<Rc<dyn Fn(&Document, NodeId, NodeId, bool) -> Result<(), Error>>>,
    interaction_state: interaction::InteractionState,
    target_element: Option<NodeId>,
    parser_form_owners: core::cell::RefCell<Vec<(NodeId, NodeId)>>,
    target_element_resolver: Option<Rc<dyn Fn(&Document) -> Option<NodeId>>>,
    interaction_generation: u64,
    top_layer_elements: Vec<top_layer::Entry>,
    /// The topmost Auto popover hit by the preceding trusted pointerdown, or
    /// null when that pointerdown was outside the open Auto stack.
    popover_pointerdown_target: Option<NodeId>,
    dialog_return_values: Vec<(NodeId, String)>,
    // Sparse records sorted by host identity; lookups need no second index.
    shadow_trees: Vec<shadow::ShadowTree>,
    // Reverse links are rare and sorted by full root identity, so shadow-including
    // parent walks avoid rescanning every attached tree at each shadow boundary.
    shadow_hosts_by_root: Vec<(NodeId, NodeId)>,
    foreign_template_links: Vec<(NodeId, NodeId)>,
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
    value: Option<Rc<dyn Fn(NodeId, &mut dyn FnMut(Option<&str>))>>,
}

impl Document {
    pub fn new(max_nodes: usize) -> Self {
        Self {
            registered_custom_properties: Vec::new(),
            highlights: Vec::new(),
            id: NEXT_DOCUMENT_ID.fetch_add(1, Ordering::Relaxed),
            nodes: alloc::vec![Node::new(NodeKind::Document, 1)],
            attribute_namespaces: Vec::new(),
            attribute_nodes: Vec::new(),
            literal_colon_names: Vec::new(),
            custom_element_is_values: Vec::new(),
            cryptographic_nonces: Vec::new(),
            cryptographic_nonce_bytes: 0,
            hide_connected_nonces: false,
            template_owner_document: None,
            inert_document_roots: Vec::new(),
            design_mode_documents: Vec::new(),
            inline_cssom_styles: Vec::new(),
            inline_cssom_read_cache: None,
            doctype_identifiers: Vec::new(),
            document_mode: DocumentMode::NoQuirks,
            is_html_document: false,
            language_defaults: None,
            scripting_enabled: false,
            allow_declarative_shadow_roots: false,
            free: Vec::new(),
            live_nodes: 1,
            version: 0,
            max_nodes: max_nodes.max(1),
            allocation_budget: None,
            template_ancestor_resolver: None,
            allocation_budget_group: None,
            node_count_tracking: None,
            journal: Vec::new(),
            mutation_sink: None,
            mutation_observer_sink: None,
            details_sink: None,
            shadow_mutation_sink: None,
            validity_resolver: None,
            form_selector_state_resolver: None,
            custom_state_resolver: None,
            shadow_host_policy: None,
            inline_stylesheet_policy: None,
            inline_style_attribute_policy: None,
            parser_element_completion_sink: None,
            parser_style_block_sink: None,
            parser_custom_element_predicate: None,
            parser_element_birth_sink: None,
            interaction_state: interaction::InteractionState::default(),
            target_element: None,
            parser_form_owners: core::cell::RefCell::new(Vec::new()),
            target_element_resolver: None,
            interaction_generation: 0,
            top_layer_elements: Vec::new(),
            popover_pointerdown_target: None,
            dialog_return_values: Vec::new(),
            shadow_trees: Vec::new(),
            shadow_hosts_by_root: Vec::new(),
            foreign_template_links: Vec::new(),
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
        self.validity_resolver = Some(ValidityResolver { read, generation, value: None });
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

    /// Borrow the existing host live value only for the duration of a read.
    /// The callback stores no strings or node state in the shared document.
    pub fn set_form_value_resolver(&mut self, read: Rc<dyn Fn(NodeId, &mut dyn FnMut(Option<&str>))>) {
        if let Some(resolver) = self.validity_resolver.as_mut() {
            resolver.value = Some(read);
            self.mark_dirty(self.root(), Dirty::LAYOUT, MutationKind::FullRebuild);
        }
    }

    pub fn has_form_value_resolver(&self) -> bool {
        self.validity_resolver.as_ref().is_some_and(|resolver| resolver.value.is_some())
    }

    pub fn with_form_value<R>(&self, node: NodeId, read: impl FnOnce(Option<&str>) -> R) -> R {
        let mut read = Some(read);
        let mut result = None;
        if let Some(source) = self.validity_resolver.as_ref().and_then(|resolver| resolver.value.as_ref()) {
            source(node, &mut |value| {
                if let Some(read) = read.take() { result = Some(read(value)); }
            });
        }
        match result {
            Some(result) => result,
            None => read.expect("unconsumed form value reader")(None),
        }
    }

    /// Install a sparse host-state reader for checkedness and selectedness.
    /// Its mutations share the generation published by `set_validity_resolver`
    /// so retained CSS styles observe live control state without DOM copies.
/// Host custom states are sparse native data; the document stores only a weak resolver.
pub fn set_custom_state_resolver(&mut self, read: Rc<dyn Fn(NodeId, &str) -> bool>) {
    self.custom_state_resolver = Some(read);
}
pub fn has_custom_state(&self, node: NodeId, name: &str) -> bool {
    self.custom_state_resolver.as_ref().is_some_and(|read| read(node, name))
}

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
/// Classification shares the host's sparse form state, never a node field.
pub fn is_form_associated_custom_element(&self,node:NodeId)->bool {
    if !matches!(self.kind(node),Ok(NodeKind::Element{namespace:Namespace::Html,name,..}) if name.contains('-')) {return false;}
    self.form_selector_state(node).is_some_and(|state|state.form_associated_custom_element)
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

    /// HTML's document target is retained independently of transient focus or
    /// pointer state. DOM removal does not itself select a different fragment.
    pub fn target_element(&self) -> Option<NodeId> {
        self.target_element_resolver.as_ref().map_or(self.target_element,|resolve|resolve(self))
    }

    /// Embeddings with stable cross-document node identities can resolve the
    /// remembered target without transferring another document's target state.
    /// The resolver must not run script or re-enter this document's session.
    pub fn set_target_element_resolver(&mut self, resolve: Option<Rc<dyn Fn(&Document) -> Option<NodeId>>>) {
        self.target_element_resolver = resolve;
        self.interaction_generation = self.interaction_generation.wrapping_add(1);
    }

    pub fn set_target_element(&mut self, target: Option<NodeId>) -> Result<(), Error> {
        if let Some(node) = target {
            if !matches!(self.kind(node)?, NodeKind::Element { .. }) { return Err(Error::InvalidNode); }
        }
        let changed = self.target_element_resolver.is_some() || self.target_element != target;
        self.target_element_resolver = None;
        if changed {
            self.target_element = target;
            self.interaction_generation = self.interaction_generation.wrapping_add(1);
        }
        Ok(())
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
                popover_trigger: None,
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

    pub fn popover_trigger(&self, node: NodeId) -> Option<NodeId> {
        self.top_layer_elements.iter().find(|entry| entry.node == node)
            .filter(|entry| matches!(entry.kind, top_layer::Kind::Popover(_)))
            .and_then(|entry| entry.popover_trigger)
            .filter(|trigger| self.is_connected_element(*trigger))
    }

    pub fn set_popover_trigger(&mut self, node: NodeId, trigger: Option<NodeId>) {
        let trigger = trigger.filter(|trigger| self.is_connected_element(*trigger));
        if let Some(entry) = self.top_layer_elements.iter_mut().find(|entry| entry.node == node) {
            entry.popover_trigger = trigger;
        }
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

    pub fn is_connected_element(&self, node: NodeId) -> bool {
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
        let clear = |node: &mut Option<NodeId>| {
            if node.is_some_and(|node| self.is_shadow_including_descendant_of(root, node)) {
                *node = None;
            }
        };
        clear(&mut state.focused);
        clear(&mut state.focus_visible);
        clear(&mut state.hover);
        clear(&mut state.active[0]);
        clear(&mut state.active[1]);
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

    pub fn design_mode_enabled(&self, owner: NodeId) -> bool {
        matches!(self.kind(owner), Ok(NodeKind::Document)) && self.design_mode_documents.contains(&owner)
    }

    /// Returns whether an actual off/on transition occurred. The internal style
    /// journal changes without producing an authored DOM mutation record.
    pub fn set_design_mode_enabled(&mut self, owner: NodeId, enabled: bool) -> Result<bool, Error> {
        if !matches!(self.kind(owner)?, NodeKind::Document) { return Err(Error::WrongKind); }
        let position=self.design_mode_documents.iter().position(|candidate|*candidate==owner);
        if enabled == position.is_some() { return Ok(false); }
        if enabled {
            self.design_mode_documents.try_reserve(1).map_err(|_|Error::LimitExceeded)?;
            self.design_mode_documents.push(owner);
        } else if let Some(position)=position { self.design_mode_documents.swap_remove(position); }
        self.mark_dirty(owner, Dirty::STYLE, MutationKind::FullRebuild);
        Ok(true)
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
        self.doctype_at(self.root())
    }

    pub fn doctype_at(&self, owner: NodeId) -> Result<Option<NodeId>, Error> {
        let mut child = self.first_child(owner)?;
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

    /// Admission for a document participating in an embedder's aggregate arena
    /// budget. Runs before allocation and must not execute script or mutate DOM.
    pub fn set_allocation_budget(&mut self, budget: Option<Rc<dyn Fn(&Document, usize, usize) -> Result<(), Error>>>) {
        self.allocation_budget = budget;
    }

    pub fn set_allocation_budget_group(&mut self, group: Rc<()>) { self.allocation_budget_group = Some(group); }

    pub fn set_node_count_tracking(&mut self, tracking: Rc<core::cell::Cell<usize>>) {
        tracking.set(self.live_nodes);
        self.node_count_tracking = Some(tracking);
    }

    fn update_tracked_node_count(&self) {
        if let Some(tracking) = &self.node_count_tracking { tracking.set(self.live_nodes); }
    }

    pub fn set_mutation_sink(
        &mut self,
        sink: Option<Rc<dyn Fn(&Document, &observe::ObservedMutation)>>,
    ) {
        self.mutation_sink = sink;
    }

    pub(crate) fn associate_parser_form(&self, control: NodeId, form: NodeId) -> Result<(), Error> {
        self.kind(form)?;
        self.associate_parser_form_validated(control,form)
    }

    // The parser's borrowed owner graph validates a foreign form before using
    // this seam. A constructor reaction may adopt the detached control while
    // its intended parent/form pointer remain in the original document.
    pub(crate) fn associate_parser_form_validated(&self,control:NodeId,form:NodeId)->Result<(),Error> {
        self.kind(control)?;
        let mut associations = self.parser_form_owners.borrow_mut();
        if let Some(entry) = associations.iter_mut().find(|(node, _)| *node == control) { entry.1 = form; }
        else {
            associations.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
            associations.push((control, form));
        }
        Ok(())
    }

    pub(crate) fn parser_form_owner(&self, control: NodeId) -> Option<NodeId> {
        self.parser_form_owners.borrow().iter().find(|(node, _)| *node == control).map(|(_, form)| *form)
    }

    fn notify(&self, mutation: observe::ObservedMutation) {
        self.notify_internal(&mutation);
        self.notify_observer(&mutation);
    }

    fn notify_internal(&self, mutation: &observe::ObservedMutation) {
        if let observe::ObservedKind::Attribute { name, namespace_uri: None, .. } = &mutation.kind {
            if name == "form" { self.parser_form_owners.borrow_mut().retain(|(control, _)| *control != mutation.target); }
        }
        if mutation.kind.removed_nodes().next().is_some() {
            self.parser_form_owners.borrow_mut().retain(|(control, form)|
                self.root_node(*control, false).ok().zip(self.root_node(*form, false).ok())
                    .is_some_and(|(control_root, form_root)| control_root == form_root));
        }
        if !self.shadow_trees.is_empty() {
            if let Some(sink) = &self.shadow_mutation_sink {
                sink(self, mutation);
            }
        }
        if let Some(sink) = &self.mutation_sink {
            sink(self, mutation);
        }
    }

    fn notify_observer(&self, mutation: &observe::ObservedMutation) {
        if let Some(sink) = &self.mutation_observer_sink { sink(self, mutation); }
    }

    /// Observer records can be suppressed independently of internal removal,
    /// range, insertion, and lifecycle steps by compound DOM algorithms.
    pub fn set_mutation_observer_sink(&mut self, sink: Option<Rc<dyn Fn(&Document, &observe::ObservedMutation)>>) {
        self.mutation_observer_sink = sink;
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
            || self.mutation_observer_sink.is_some()
            || (!self.shadow_trees.is_empty() && self.shadow_mutation_sink.is_some())
            || !self.parser_form_owners.borrow().is_empty()
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
        let is_value = Self::authored_is_value(&kind);
        self.create_with_is_rc(kind, is_value)
    }

    pub fn set_parser_custom_element_predicate(&mut self, predicate: Option<Rc<dyn Fn(&Document, NodeId, &str, Option<&str>) -> bool>>) {
        self.parser_custom_element_predicate = predicate;
    }

    /// Record a parser-created element's registry before connection or script.
    pub fn set_inline_stylesheet_policy(&mut self, policy: Option<Rc<dyn Fn(&Document, NodeId, &str) -> bool>>) {
        self.inline_stylesheet_policy = policy;
    }

    pub fn inline_stylesheet_allowed(&self, node: NodeId, text: &str) -> bool {
        self.inline_stylesheet_policy.as_ref().is_none_or(|policy| policy(self, node, text))
    }
    pub fn has_inline_stylesheet_policy(&self)->bool {self.inline_stylesheet_policy.is_some()}

    /// Evaluate authored style attributes when they change, rather than
    /// rechecking later policies on every layout or retaining their text twice.
    pub fn set_inline_style_attribute_policy(&mut self, policy: Option<Rc<dyn Fn(&Document, NodeId, &str) -> Result<bool, Error>>>) {
        self.inline_style_attribute_policy = policy;
    }

    pub fn inline_style_attribute_allowed(&self, node: NodeId) -> bool {
        self.node(node).is_ok_and(|node| !node.style_attribute_blocked)
    }

    fn initialize_inline_style_attribute(&mut self, node: NodeId) -> Result<(), Error> {
        let allowed = match (&self.inline_style_attribute_policy, self.get_attribute_ns_ref(node, None, "style").ok().flatten()) {
            (Some(policy), Some(text)) => policy(self, node, text)?,
            _ => true,
        };
        self.node_mut(node).style_attribute_blocked = !allowed;
        Ok(())
    }

    /// Observe actual tree-builder retirement without running author code.
    /// The parser may still hold its document borrow while this sink runs.
    pub fn set_parser_element_completion_sink(&mut self, sink: Option<Rc<dyn Fn(&Document, NodeId)>>) {
        self.parser_element_completion_sink = sink;
    }

    pub(crate) fn record_parser_element_completion(&self, node: NodeId) {
        if let Some(sink) = &self.parser_element_completion_sink { sink(self, node); }
    }

    pub fn set_parser_style_block_sink(&mut self, sink: Option<Rc<dyn Fn(&Document, NodeId) -> Result<(), Error>>>) {
        self.parser_style_block_sink = sink;
    }

    pub fn record_parser_style_block_update(&self, node: NodeId) -> Result<(), Error> {
        if let Some(sink) = &self.parser_style_block_sink { sink(self, node)?; }
        Ok(())
    }

    pub fn set_parser_element_birth_sink(&mut self, sink: Option<Rc<dyn Fn(&Document, NodeId, NodeId, bool) -> Result<(), Error>>>) {
        self.parser_element_birth_sink = sink;
    }

    pub(crate) fn record_parser_element_birth(&self, element: NodeId, intended_parent: NodeId, document_parser: bool) -> Result<(), Error> {
        if let Some(sink) = &self.parser_element_birth_sink { sink(self, element, intended_parent, document_parser)?; }
        Ok(())
    }

    pub(crate) fn parser_custom_element_defined(&self, context: NodeId, local_name: &str, is_value: Option<&str>) -> bool {
        self.parser_custom_element_predicate.as_ref().is_some_and(|predicate| predicate(self, context, local_name, is_value))
    }

    pub(crate) fn initialize_parser_is_value(&mut self, node: NodeId, is_value: Option<&str>) -> Result<(), Error> {
        self.kind(node)?;
        if let Some(value) = is_value { self.insert_custom_element_is(node, Rc::from(value)); }
        Ok(())
    }

    fn authored_is_value(kind: &NodeKind) -> Option<Rc<str>> {
        match kind {
            NodeKind::Element { namespace: Namespace::Html, attributes, .. } =>
                attributes.iter().find(|(name, _)| name == "is").map(|(_, value)| Rc::from(value.as_str())),
            _ => None,
        }
    }

    /// Set a creation dictionary's immutable `is` value without inventing a
    /// content attribute. Non-HTML nodes cannot have custom-element state.
    pub fn create_with_is_value(&mut self, kind: NodeKind, is_value: Option<&str>) -> Result<NodeId, Error> {
        let is_value = if matches!(kind, NodeKind::Element { namespace: Namespace::Html, .. }) {
            is_value.map(Rc::from)
        } else { None };
        self.create_with_is_rc(kind, is_value)
    }

    fn create_with_is_rc(&mut self, kind: NodeKind, is_value: Option<Rc<str>>) -> Result<NodeId, Error> {
        let id = self.create_with_is_rc_without_details(kind, is_value)?;
        self.details_created(id)?;
        Ok(id)
    }

    fn create_with_is_rc_without_details(&mut self, kind: NodeKind, is_value: Option<Rc<str>>) -> Result<NodeId, Error> {
        if is_value.is_some() { self.custom_element_is_values.try_reserve(1).map_err(|_| Error::LimitExceeded)?; }
        let id = self.create_without_details_steps(kind)?;
        if let Some(value) = is_value { self.insert_custom_element_is(id, value); }
        Ok(id)
    }

    fn includes_nonce(&self,node:NodeId)->Result<bool,Error> {
        Ok(matches!(self.kind(node)?,NodeKind::Element {namespace:Namespace::Html|Namespace::Svg|Namespace::MathMl,..}))
    }
    fn nonce_rc(&self,node:NodeId)->Option<&Rc<str>> {
        self.cryptographic_nonces.binary_search_by_key(&node.index,|(id,_)|id.index).ok()
            .and_then(|index|{let(id,value)=&self.cryptographic_nonces[index];(*id==node).then_some(value)})
    }
    pub fn cryptographic_nonce(&self,node:NodeId)->Result<&str,Error> {
        if !self.includes_nonce(node)? {return Err(Error::WrongKind);}
        Ok(self.nonce_rc(node).map_or("",|value|value.as_ref()))
    }
    pub fn set_cryptographic_nonce(&mut self,node:NodeId,value:&str)->Result<(),Error> {
        if !self.includes_nonce(node)? {return Err(Error::WrongKind);}
        self.reserve_nonce(value.len(),self.nonce_rc(node).map_or(0,|value|value.len()))?;
        self.store_nonce(node,(!value.is_empty()).then(||Rc::from(value)));Ok(())
    }
    fn reserve_nonce(&mut self,new:usize,old:usize)->Result<(),Error> {
        let bytes=self.cryptographic_nonce_bytes.checked_sub(old).and_then(|bytes|bytes.checked_add(new)).ok_or(Error::LimitExceeded)?;
        let additional=usize::from(new!=0 && old==0);
        let required=self.cryptographic_nonces.len().checked_add(additional).ok_or(Error::LimitExceeded)?;
        let capacity=self.cryptographic_nonces.capacity();
        let slots=if required>capacity {required.max(capacity.checked_mul(2).ok_or(Error::LimitExceeded)?).max(4)} else {capacity};
        if slots.checked_mul(core::mem::size_of::<(NodeId,Rc<str>)>()).and_then(|storage|storage.checked_add(bytes)).is_none_or(|bytes|bytes>html::MAX_HTML_BYTES) {return Err(Error::LimitExceeded);}
        if additional!=0 {self.cryptographic_nonces.try_reserve(additional).map_err(|_|Error::LimitExceeded)?;}
        Ok(())
    }
    fn store_nonce(&mut self,node:NodeId,value:Option<Rc<str>>) {
        let value=value.filter(|value|!value.is_empty());
        match self.cryptographic_nonces.binary_search_by_key(&node.index,|(id,_)|id.index) {
            Ok(index)=>{
                self.cryptographic_nonce_bytes-=self.cryptographic_nonces[index].1.len();
                if let Some(value)=value {self.cryptographic_nonce_bytes+=value.len();self.cryptographic_nonces[index].1=value;}
                else {self.cryptographic_nonces.remove(index);}
            }
            Err(index)=>if let Some(value)=value {self.cryptographic_nonce_bytes+=value.len();self.cryptographic_nonces.insert(index,(node,value));},
        }
    }
    fn nonce_attribute_changed(&mut self,node:NodeId,namespace:Option<&str>,name:&str,value:Option<&str>)->Result<(),Error> {
        if namespace.is_none() && name=="nonce" && self.includes_nonce(node)? {self.set_cryptographic_nonce(node,value.unwrap_or(""))?;}
        Ok(())
    }
    /// Header-delivered CSP plus a real browsing context enables nonce hiding.
    pub fn set_connected_nonce_hiding(&mut self,enabled:bool)->Result<(),Error> {
        if self.hide_connected_nonces==enabled {return Ok(());}
        self.hide_connected_nonces=enabled;
        // Embedders may construct the initial tree before installing response
        // metadata. Complete those connection steps before any author turn.
        if enabled {self.hide_inserted_nonces(self.root())?;}
        Ok(())
    }
    fn hide_inserted_nonces(&mut self,root:NodeId)->Result<(),Error> {
        if !self.hide_connected_nonces || self.root_node(root,true)?!=self.root() {return Ok(());}
        let mut current=Some(root);
        while let Some(node)=current {
            if self.includes_nonce(node)? && self.get_attribute_ns_ref(node,None,"nonce")?.is_some_and(|value|!value.is_empty()) {
                // The slot is restored by the connection algorithm. Keep its
                // Rc in place while applying the ordinary attribute mutation;
                // no author callback runs during these core mutation steps.
                self.set_attribute_ns_with_style(node,None,"nonce","",None,true)?;
            }
            current=selector::next_shadow_including_descendant(self,root,node)?;
        }
        Ok(())
    }

    fn insert_custom_element_is(&mut self, id: NodeId, value: Rc<str>) {
        let index = self.custom_element_is_values.binary_search_by_key(&id.index, |(owner, _)| owner.index)
            .unwrap_or_else(|index| index);
        self.custom_element_is_values.insert(index, (id, value));
    }

    fn custom_element_is_rc(&self, id: NodeId) -> Option<&Rc<str>> {
        self.custom_element_is_values.binary_search_by_key(&id.index, |(owner, _)| owner.index).ok()
            .and_then(|index| { let (owner, value) = &self.custom_element_is_values[index]; (*owner == id).then_some(value) })
    }

    pub fn custom_element_is_value(&self, id: NodeId) -> Result<Option<&str>, Error> {
        self.kind(id)?;
        Ok(self.custom_element_is_rc(id).map(|value| value.as_ref()))
    }

    fn create_without_details_steps(&mut self, kind: NodeKind) -> Result<NodeId, Error> {
        self.create_node_without_details(kind, false, false)
    }

    pub fn element_creation_allocation_count(&self, namespace: &Namespace, name: &str, literal_colon: bool) -> usize {
        let local = if literal_colon { name } else { name.rsplit(':').next().unwrap_or(name) };
        if *namespace == Namespace::Html && local == "template" { 2 + usize::from(self.template_owner_document.is_none()) } else { 1 }
    }

    fn create_node_without_details(&mut self, kind: NodeKind, internal_document: bool, literal_colon: bool) -> Result<NodeId, Error> {
        let nonce=match &kind {
            NodeKind::Element {namespace:Namespace::Html|Namespace::Svg|Namespace::MathMl,attributes,..}=>attributes.iter().find(|(name,_)|name=="nonce").map(|(_,value)|Rc::<str>::from(value.as_str())).filter(|value|!value.is_empty()),
            _=>None,
        };
        if let Some(value)=&nonce {self.reserve_nonce(value.len(),0)?;}
        let template = matches!(&kind, NodeKind::Element { namespace: Namespace::Html, name, .. }
            if (if literal_colon { name.as_str() } else { svg::local_name(name) }) == "template");
        let required = match &kind {
            NodeKind::Element { namespace, name, .. } => self.element_creation_allocation_count(namespace, name.as_str(), literal_colon),
            _ => 1,
        };
        if self.max_nodes.saturating_sub(self.live_nodes) < required {
            return Err(Error::LimitExceeded);
        }
        if let Some(budget) = &self.allocation_budget { budget(self, required, required)?; }
        if self.nodes.len().checked_add(required.saturating_sub(self.free.len()))
            .is_none_or(|total| total > u32::MAX as usize) {
            return Err(Error::LimitExceeded);
        }
        if self.live_nodes >= self.max_nodes {
            return Err(Error::LimitExceeded);
        }
        if matches!(kind, NodeKind::Document) && !internal_document {
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
        self.nodes.try_reserve(required.saturating_sub(self.free.len())).map_err(|_| Error::LimitExceeded)?;
        if template { self.ensure_template_owner_document()?; }
        if let Some(index) = self.free.pop() {
            let generation = self.nodes[index as usize].generation;
            self.nodes[index as usize] = Node::new(kind, generation);
            self.live_nodes += 1;
            self.update_tracked_node_count();
            let id = NodeId {
                document: self.id,
                index,
                generation,
            };
            if template {self.create_template_content(id)?;}
            if let Some(value)=nonce {self.store_nonce(id,Some(value));}
            if let Err(error) = self.initialize_inline_style_attribute(id) {
                self.destroy_subtree(id)?;
                return Err(error);
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
        self.update_tracked_node_count();
        if template {self.create_template_content(id)?;}
        if let Some(value)=nonce {self.store_nonce(id,Some(value));}
        if let Err(error) = self.initialize_inline_style_attribute(id) {
            self.destroy_subtree(id)?;
            return Err(error);
        }
        Ok(id)
    }

    fn create_template_content(&mut self, host: NodeId) -> Result<(), Error> {
        self.ensure_template_owner_document()?;
        let content = self.create(NodeKind::DocumentFragment)?;
        self.node_mut(host).template_content = content.index;
        self.node_mut(content).template_content = host.index;
        self.set_node_document(content, self.template_owner_document.unwrap_or(self.root()))?;
        Ok(())
    }

    pub fn ensure_template_owner_document(&mut self) -> Result<NodeId, Error> {
        if let Some(owner) = self.template_owner_document { return Ok(owner); }
        let owner = self.create_node_without_details(NodeKind::Document, true, false)?;
        self.template_owner_document = Some(owner);
        Ok(owner)
    }

    pub fn template_owner_document(&self) -> Option<NodeId> { self.template_owner_document }

    pub fn is_template_owner_document(&self, node: NodeId) -> bool {
        self.template_owner_document == Some(node)
    }

    /// Logical node document, independent of this arena's allocation identity.
    /// Template contents use one shared inert document, including after removal.
    pub fn node_document(&self, mut node: NodeId) -> Result<NodeId, Error> {
        loop {
            let kind = self.kind(node)?;
            if matches!(kind, NodeKind::Document) { return Ok(node); }
            if self.inert_document_roots.binary_search_by_key(&node.index, |id| id.index)
                .is_ok_and(|index| self.inert_document_roots[index] == node) {
                return self.template_owner_document.ok_or(Error::InvalidNode);
            }
            if let NodeKind::Attribute { owner_element: Some(owner), .. } = kind { node = **owner; continue; }
            if let Some(parent) = self.shadow_including_parent(node)? { node = parent; }
            else { return Ok(self.root()); }
        }
    }

    pub fn set_node_document(&mut self, node: NodeId, owner: NodeId) -> Result<(), Error> {
        self.kind(node)?;
        if owner != self.root() && !self.is_template_owner_document(owner) { return Err(Error::InvalidNode); }
        let index = self.inert_document_roots.binary_search_by_key(&node.index, |id| id.index);
        if self.is_template_owner_document(owner) {
            if let Err(index) = index {
                self.inert_document_roots.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
                self.inert_document_roots.insert(index, node);
            }
        } else if let Ok(index) = index { self.inert_document_roots.remove(index); }
        Ok(())
    }

    pub fn reserve_node_document(&mut self, owner: NodeId) -> Result<(), Error> {
        if owner != self.root() && !self.is_template_owner_document(owner) { return Err(Error::InvalidNode); }
        if self.is_template_owner_document(owner) {
            self.inert_document_roots.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
        }
        Ok(())
    }

    fn reserve_inert_detach(&mut self, nodes: &[NodeId]) -> Result<(), Error> {
        if self.template_owner_document.is_none() { return Ok(()); }
        let mut count = 0usize;
        for &node in nodes {
            if self.parent(node)?.is_some() && self.is_template_owner_document(self.node_document(node)?) { count += 1; }
        }
        self.inert_document_roots.try_reserve(count).map_err(|_| Error::LimitExceeded)
    }

    /// HTML template contents are detached from the ordinary child tree.
    pub fn template_content(&self, id: NodeId) -> Result<Option<NodeId>, Error> {
        if !self.is_html_template(id)?
        {
            return Ok(None);
        }
        if let Some((_, content)) = self.foreign_template_links.iter().find(|(node, _)| *node == id) { return Ok(Some(*content)); }
        let index = self.nodes[id.index()].template_content;
        Ok((index != NO_LINK).then(|| NodeId {
            document: self.id,
            index,
            generation: self.nodes[index as usize].generation,
        }))
    }

    pub fn is_html_template(&self, id: NodeId) -> Result<bool, Error> {
        Ok(matches!(self.kind(id)?, NodeKind::Element { namespace: Namespace::Html, .. })
            && self.element_name_parts(id)?.1 == "template")
    }

    pub fn template_host(&self, id: NodeId) -> Result<Option<NodeId>, Error> {
        if !matches!(self.kind(id)?, NodeKind::DocumentFragment) { return Ok(None); }
        if let Some((_, host)) = self.foreign_template_links.iter().find(|(node, _)| *node == id) { return Ok(Some(*host)); }
        Ok(self.link_id(self.nodes[id.index()].template_content))
    }

    pub fn foreign_template_links(&self) -> &[(NodeId, NodeId)] { &self.foreign_template_links }

    pub fn set_template_link(&mut self, node: NodeId, counterpart: NodeId) -> Result<(), Error> {
        self.kind(node)?;
        if counterpart.document == self.id {
            self.kind(counterpart)?;
            self.foreign_template_links.retain(|(local, _)| *local != node);
            self.node_mut(node).template_content = counterpart.index;
        } else {
            if let Some((_, target)) = self.foreign_template_links.iter_mut().find(|(local, _)| *local == node) { *target = counterpart; }
            else {
                self.foreign_template_links.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
                self.foreign_template_links.push((node, counterpart));
            }
            self.node_mut(node).template_content = NO_LINK;
        }
        Ok(())
    }

    pub fn remap_foreign_template_links(&mut self, mapping: &[(NodeId, NodeId)]) -> Result<(), Error> {
        if self.foreign_template_links.is_empty() { return Ok(()); }
        let mut sorted = Vec::new();
        sorted.try_reserve(mapping.len()).map_err(|_| Error::LimitExceeded)?;
        sorted.extend_from_slice(mapping);
        sorted.sort_unstable_by_key(|(old, _)| old.key());
        let mut changed = Vec::new();
        for &(local, remote) in &self.foreign_template_links {
            if let Ok(index) = sorted.binary_search_by_key(&remote.key(), |(old, _)| old.key()) {
                changed.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
                changed.push((local, sorted[index].1));
            }
        }
        for (local, remote) in changed { self.set_template_link(local, remote)?; }
        Ok(())
    }

    pub(crate) fn host_including_parent(&self, id: NodeId) -> Result<Option<NodeId>, Error> {
        if let Some(host) = self.shadow_host(id)? {
            return Ok(Some(host));
        }
        if let Some(host) = self.template_host(id)? { return Ok(Some(host)); }
        self.parent(id)
    }

    pub fn set_template_ancestor_resolver(&mut self, resolver: Rc<dyn Fn(&Document, NodeId, NodeId) -> Result<bool, Error>>) {
        self.template_ancestor_resolver = Some(resolver);
    }

    pub fn is_host_including_inclusive_ancestor(&self, ancestor: NodeId, mut node: NodeId) -> Result<bool, Error> {
        if !self.foreign_template_links.is_empty() {
            if let Some(resolve) = &self.template_ancestor_resolver { return resolve(self, ancestor, node); }
        }
        for _ in 0..=self.node_count() {
            if node == ancestor { return Ok(true); }
            let Some(parent) = self.host_including_parent(node)? else { return Ok(false); };
            node = parent;
        }
        Err(Error::Hierarchy)
    }

    /// Reclaim a detached subtree; its old IDs become invalid.
    pub fn destroy_subtree(&mut self, root: NodeId) -> Result<(), Error> {
        if root == self.root() || self.is_template_owner_document(root) || self.parent(root)?.is_some() || self.shadow_host(root)?.is_some()
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
            if let Ok(index) = self.shadow_trees.binary_search_by_key(&id.key(), |tree| tree.host.key()) {
                let tree = self.shadow_trees.remove(index);
                self.remove_shadow_host_index(tree.root);
                pending.push(tree.root);
            }
            if let Some(content) = self.template_content(id)? {
                if content.document == self.id { pending.push(content); }
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
            self.parser_form_owners.get_mut().retain(|(control, form)| *control != id && *form != id);
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
            self.foreign_template_links.retain(|(owner, _)| *owner != id);
            self.inert_document_roots.retain(|owner| *owner != id);
            self.design_mode_documents.retain(|owner| *owner != id);
            if let Ok(index) = self.custom_element_is_values.binary_search_by_key(&id.index, |(owner, _)| owner.index) {
                self.custom_element_is_values.remove(index);
            }
            self.store_nonce(id,None);
            self.attribute_nodes.retain(|(owner, _)| *owner != id);
            self.doctype_identifiers
                .retain(|(owner, _, _)| *owner != id);
            self.live_nodes -= 1;
            self.update_tracked_node_count();
        }
        if self.cryptographic_nonces.is_empty() {self.cryptographic_nonces=Vec::new();}
        self.compact_shadow_host_index();
        let document_id = self.id;
        let nodes = &self.nodes;
        if self.inline_cssom_read_cache.as_ref().is_some_and(|(id, _)| !nodes.get(id.index()).is_some_and(|node| node.alive && node.generation == id.generation)) {
            self.inline_cssom_read_cache = None;
        }
        self.inline_cssom_styles.retain(|(id, _)| nodes.get(id.index()).is_some_and(|node|
            node.alive && node.generation == id.generation));
        if self.inline_cssom_styles.is_empty() {
            self.inline_cssom_styles = Vec::new();
        } else if self.inline_cssom_styles.capacity() >= 1024 &&
            self.inline_cssom_styles.len() < self.inline_cssom_styles.capacity() / 8 {
            self.inline_cssom_styles.shrink_to(self.inline_cssom_styles.len().max(256));
        }
        self.literal_colon_names.retain(|id| nodes.get(id.index()).is_some_and(|node|
            node.alive && node.generation == id.generation));
        if self.literal_colon_names.is_empty() {
            self.literal_colon_names = Vec::new();
        } else if self.literal_colon_names.capacity() >= 1024 &&
            self.literal_colon_names.len() < self.literal_colon_names.capacity() / 8 {
            self.literal_colon_names.shrink_to(self.literal_colon_names.len().max(256));
        }
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
        let owner = self.node_document(source)?;
        if self.is_template_owner_document(owner) {
            self.inert_document_roots.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
        }
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
        let literal_colon = self.literal_colon_name(source);
        if literal_colon { self.literal_colon_names.try_reserve(1).map_err(|_| Error::LimitExceeded)?; }
        let is_value = self.custom_element_is_rc(source).cloned();
        if is_value.is_some() { self.custom_element_is_values.try_reserve(1).map_err(|_| Error::LimitExceeded)?; }
        let nonce=self.nonce_rc(source).cloned();
        if let Some(value)=&nonce {self.reserve_nonce(value.len(),0)?;}
        let clone = self.create_node_without_details(kind, false, literal_colon)?;
        self.set_node_document(clone, owner)?;
        if let Some(value) = is_value { self.insert_custom_element_is(clone, value); }
        if literal_colon { self.insert_literal_colon_name(clone); }
        if !namespaces.is_empty() {
            self.attribute_namespaces.push((clone, namespaces));
        }
        if let Some((public_id, system_id)) = doctype_identifiers {
            self.doctype_identifiers.push((clone, public_id, system_id));
        }
        if self.includes_nonce(clone)? {self.store_nonce(clone,nonce);}
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
        self.title_element_at(self.root())
    }

    pub fn document_element_at(&self, owner: NodeId) -> Result<Option<NodeId>, Error> {
        let mut child = self.first_child(owner)?;
        while let Some(node) = child {
            if matches!(self.kind(node)?, NodeKind::Element { .. }) { return Ok(Some(node)); }
            child = self.next_sibling(node)?;
        }
        Ok(None)
    }

    pub fn title_element_at(&self, owner: NodeId) -> Result<Option<NodeId>, Error> {
        if let Some(root) = self.document_element_at(owner)? {
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
        let mut cursor = self.first_child(owner)?;
        while let Some(node) = cursor {
            if matches!(self.kind(node)?, NodeKind::Element { namespace: Namespace::Html, name, .. }
                if svg::local_name(name) == "title")
            {
                return Ok(Some(node));
            }
            cursor = selector::next_descendant(self, owner, node)?;
        }
        Ok(None)
    }

    /// Child text content with ASCII whitespace stripped and collapsed. This
    /// streams the direct text children and allocates only the resulting title.
    pub fn title(&self) -> Result<String, Error> {
        self.title_at(self.root())
    }

    pub fn title_at(&self, owner: NodeId) -> Result<String, Error> {
        let Some(title) = self.title_element_at(owner)? else {
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
        self.ensure_title_element_at(self.root())
    }

    pub fn ensure_title_element_at(&mut self, owner: NodeId) -> Result<Option<NodeId>, Error> {
        let Some(root) = self.document_element_at(owner)? else {
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
        if let Some(title) = self.title_element_at(owner)? {
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
        self.clone_node_document(self.root(), deep)
    }

    pub fn clone_node_document(&self, owner: NodeId, deep: bool) -> Result<Document, Error> {
        if !matches!(self.kind(owner)?, NodeKind::Document) { return Err(Error::WrongKind); }
        let mut clone = Document::new(self.max_nodes);
        let inert = self.is_template_owner_document(owner);
        clone.document_mode = if inert { DocumentMode::NoQuirks } else { self.document_mode };
        clone.is_html_document = self.is_html_document;
        clone.scripting_enabled = !inert && self.scripting_enabled;
        clone.allow_declarative_shadow_roots = self.allow_declarative_shadow_roots;
        if !deep {
            return Ok(clone);
        }

        let mut child = self.first_child(owner)?;
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
        self.clone_plan_count(source, deep).map(|(count, _)| count)
    }

    /// Allocation preflight shared by native clone/import and the core copier.
    pub fn clone_allocation_count_from(&self, source: &Document, root: NodeId, deep: bool) -> Result<usize, Error> {
        let (count, has_template) = source.clone_plan_count(root, deep)?;
        count.checked_add(usize::from(has_template && self.template_owner_document.is_none())).ok_or(Error::LimitExceeded)
    }

    /// HTML rendered text fragment: CRLF is one break; every CR or LF creates
    /// a br and only nonempty runs create Text. No parser or string codec is
    /// involved, so authored DOM strings retain the normal CharacterData path.
    pub fn rendered_text_fragment_allocation_count(input: &str, ensure_text: bool) -> usize {
        let mut count = 1usize;
        let mut text = false;
        let mut previous_cr = false;
        for character in input.chars() {
            if character == '\n' && previous_cr { previous_cr = false; continue; }
            if matches!(character, '\r' | '\n') {
                count = count.saturating_add(usize::from(text) + 1);
                text = false;
            } else { text = true; }
            previous_cr = character == '\r';
        }
        count.saturating_add(usize::from(text || (ensure_text && input.is_empty())))
    }

    pub fn rendered_text_fragment(&mut self, owner: NodeId, input: &str, ensure_text: bool) -> Result<NodeId, Error> {
        self.ensure_clone_capacity(Self::rendered_text_fragment_allocation_count(input, ensure_text))?;
        self.reserve_node_document(owner)?;
        let fragment = self.create(NodeKind::DocumentFragment)?;
        self.set_node_document(fragment, owner)?;
        let mut start = 0usize;
        let mut position = 0usize;
        while position < input.len() {
            let byte = input.as_bytes()[position];
            if matches!(byte, b'\r' | b'\n') {
                if start != position {
                    let text = self.create(NodeKind::Text(input[start..position].into()))?;
                    self.attach_detached(fragment, text);
                }
                let br = self.create(NodeKind::Element { namespace: Namespace::Html, name: "br".into(), attributes: Vec::new() })?;
                self.attach_detached(fragment, br);
                position += if byte == b'\r' && input.as_bytes().get(position + 1) == Some(&b'\n') { 2 } else { 1 };
                start = position;
            } else { position += 1; }
        }
        if start != input.len() || (ensure_text && input.is_empty()) {
            let text = self.create(NodeKind::Text(input[start..].into()))?;
            self.attach_detached(fragment, text);
        }
        Ok(fragment)
    }

    /// HTML outerText merges data and then removes the sibling. Unlike DOM
    /// normalize, this does not relocate live ranges from the removed Text.
    pub fn merge_with_next_text(&mut self, node: NodeId) -> Result<Option<NodeId>, Error> {
        if !matches!(self.kind(node)?, NodeKind::Text(_) | NodeKind::CData(_)) { return Ok(None); }
        let Some(next) = self.next_sibling(node)? else { return Ok(None); };
        let data = match self.kind(next)? { NodeKind::Text(data) | NodeKind::CData(data) => data, _ => return Ok(None) };
        let data = data.clone();
        self.append_data(node, &data)?;
        self.remove(next)?;
        Ok(Some(next))
    }

    fn clone_plan_count(&self, source: NodeId, deep: bool) -> Result<(usize, bool), Error> {
        let mut pending = alloc::vec![(source, deep)];
        let mut count = 0usize;
        let mut has_template = false;
        while let Some((id, children)) = pending.pop() {
            if let Some(content) = self.template_content(id)? {
                has_template = true;
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
        Ok((count, has_template))
    }

    pub fn ensure_clone_capacity(&self, count: usize) -> Result<(), Error> {
        if let Some(budget) = &self.allocation_budget { budget(self, count, count)?; }
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

    pub fn clone_graph_plan<G: graph::DocumentGraph>(
        &mut self, source: &G, plan: &graph::ClonePlan, owner: NodeId, document_root: bool,
    ) -> Result<(NodeId, Vec<(NodeId, NodeId)>), Error> {
        let count = self.graph_clone_allocation_count(plan, document_root)?;
        self.ensure_clone_capacity(count)?;
        let mut pairs = Vec::new();
        pairs.try_reserve(plan.steps.len()).map_err(|_| Error::LimitExceeded)?;
        let mut copies = Vec::new();
        copies.try_reserve(plan.steps.len()).map_err(|_| Error::LimitExceeded)?;
        for step in &plan.steps {
            use graph::ClonePlacement;
            let copy = match step.placement {
                ClonePlacement::Template(host) => self.template_content(copies[host])?.ok_or(Error::WrongKind)?,
                ClonePlacement::Shadow(host, options) => self.attach_shadow_with_options(copies[host], options)?,
                ClonePlacement::Root if document_root => self.root(),
                ClonePlacement::Root | ClonePlacement::Child(_) => {
                    let copy = if step.source.document == self.id { self.clone_shallow(step.source)? }
                    else { let document = source.read(step.source)?; self.clone_shallow_from(&document, step.source)? };
                    let node_owner = match step.placement {
                        ClonePlacement::Child(parent) => self.node_document(copies[parent])?,
                        _ => owner,
                    };
                    self.set_node_document(copy, node_owner)?;
                    if let ClonePlacement::Child(parent) = step.placement { self.attach_detached(copies[parent], copy); }
                    copy
                }
            };
            copies.push(copy);
            pairs.push((step.source, copy));
        }
        Ok((*copies.first().ok_or(Error::InvalidNode)?, pairs))
    }

    pub fn graph_clone_allocation_count(&self, plan: &graph::ClonePlan, document_root: bool) -> Result<usize, Error> {
        plan.steps.len().saturating_sub(usize::from(document_root))
            .checked_add(usize::from(plan.has_template && self.template_owner_document.is_none())).ok_or(Error::LimitExceeded)
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

        let (count, has_template) = source.clone_plan_count(root, deep)?;
        self.ensure_clone_capacity(count.checked_add(usize::from(has_template && self.template_owner_document.is_none())).ok_or(Error::LimitExceeded)?)?;

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
    fn collect_adoption_nodes(&self, root: NodeId) -> Result<Vec<NodeId>, Error> {
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
        enqueue(&mut pending, 0, self.live_nodes, root)?;
        let mut originals = Vec::new();
        while let Some(id) = pending.pop() {
            self.node(id)?;
            originals.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
            originals.push(id);
            if let Some((_, attributes)) = self
                .attribute_nodes
                .iter()
                .find(|(owner, _)| *owner == id)
            {
                for (_, attribute) in attributes {
                    enqueue(&mut pending, originals.len(), self.live_nodes, *attribute)?;
                }
            }
            if let Some(content) = self.template_content(id)? {
                if content.document == self.id { enqueue(&mut pending, originals.len(), self.live_nodes, content)?; }
            }
            if let Some(shadow) = self.shadow_root(id)? {
                enqueue(&mut pending, originals.len(), self.live_nodes, shadow)?;
            }
            let mut child = self.first_child(id)?;
            while let Some(next) = child {
                enqueue(&mut pending, originals.len(), self.live_nodes, next)?;
                child = self.next_sibling(next)?;
            }
        }

        Ok(originals)
    }

    pub fn adoption_allocation_count_from(&self, source: &Document, root: NodeId) -> Result<usize, Error> {
        let nodes = source.collect_adoption_nodes(root)?;
        let template_owner = self.template_owner_document.is_none()
            && nodes.iter().any(|id| source.template_content(*id).ok().flatten().is_some());
        nodes.len().checked_add(usize::from(template_owner)).ok_or(Error::LimitExceeded)
    }

    pub fn adoption_node_documents(&self, root: NodeId) -> Result<Vec<(NodeId, NodeId)>, Error> {
        self.collect_adoption_nodes(root)?.into_iter().map(|node| self.node_document(node).map(|owner| (node, owner))).collect()
    }

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
        let detached_template_host = source.template_host(root)?;

        let originals = source.collect_adoption_nodes(root)?;
        let templates = originals.iter().filter(|node| source.template_content(**node).ok().flatten().is_some()).count();
        self.inert_document_roots.try_reserve(templates).map_err(|_| Error::LimitExceeded)?;
        let foreign_links = source.foreign_template_links.iter().filter(|(local, _)| originals.contains(local)).copied().collect::<Vec<_>>();
        self.foreign_template_links.try_reserve(foreign_links.len() + usize::from(detached_template_host.is_some())).map_err(|_| Error::LimitExceeded)?;
        source.foreign_template_links.try_reserve(usize::from(detached_template_host.is_some())).map_err(|_| Error::LimitExceeded)?;

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
        let needs_template_owner = self.template_owner_document.is_none()
            && originals.iter().any(|id| source.template_content(*id).ok().flatten().is_some());
        let count = originals.len();
        let allocation_count = count.checked_add(usize::from(needs_template_owner)).ok_or(Error::LimitExceeded)?;
        if let Some(budget) = &self.allocation_budget {
            let shared = self.allocation_budget_group.as_ref().zip(source.allocation_budget_group.as_ref())
                .is_some_and(|(destination, source)| Rc::ptr_eq(destination, source));
            budget(self, allocation_count, if shared { usize::from(needs_template_owner) } else { allocation_count })?;
        }
        if self
            .live_nodes
            .checked_add(allocation_count)
            .is_none_or(|total| total > self.max_nodes)
            || self
                .nodes
                .len()
                .checked_add(allocation_count.saturating_sub(self.free.len()))
                .is_none_or(|total| total > u32::MAX as usize)
        {
            return Err(Error::LimitExceeded);
        }

        // Reserve every vector that can grow below. Once source.remove succeeds, no ordinary
        // capacity failure can leave a half-adopted subtree behind.
        self.parser_form_owners.get_mut().try_reserve(source.parser_form_owners.borrow().len()).map_err(|_| Error::LimitExceeded)?;
        self.nodes
            .try_reserve(allocation_count.saturating_sub(self.free.len()))
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
        let moved_literal_names = source.literal_colon_names.iter().filter(|id| contains_node(**id)).count();
        self.literal_colon_names.try_reserve(moved_literal_names).map_err(|_| Error::LimitExceeded)?;
        let mut remaining_literal_names = Vec::new();
        remaining_literal_names.try_reserve(source.literal_colon_names.len() - moved_literal_names)
            .map_err(|_| Error::LimitExceeded)?;
        let moved_is_values = source.custom_element_is_values.iter().filter(|(id, _)| contains_node(*id)).count();
        self.custom_element_is_values.try_reserve(moved_is_values).map_err(|_| Error::LimitExceeded)?;
        let mut remaining_is_values = Vec::new();
        if moved_is_values != 0 {
            remaining_is_values.try_reserve(source.custom_element_is_values.len() - moved_is_values)
                .map_err(|_| Error::LimitExceeded)?;
        }
        let moved_styles = source.inline_cssom_styles.iter().filter(|(id, _)| contains_node(*id)).count();
        self.inline_cssom_styles.try_reserve(moved_styles).map_err(|_| Error::LimitExceeded)?;
        let mut remaining_styles = Vec::new();
        remaining_styles.try_reserve(source.inline_cssom_styles.len() - moved_styles)
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
        self.shadow_hosts_by_root
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
        let moved_nonces=source.cryptographic_nonces.iter().filter(|(node,_)|contains_node(*node)).count();
        let moved_nonce_bytes=source.cryptographic_nonces.iter().filter(|(node,_)|contains_node(*node)).try_fold(0usize,|bytes,(_,value)|bytes.checked_add(value.len())).ok_or(Error::LimitExceeded)?;
        self.reserve_nonce(moved_nonce_bytes,0)?;
        let required=self.cryptographic_nonces.len().checked_add(moved_nonces).ok_or(Error::LimitExceeded)?;
        let capacity=self.cryptographic_nonces.capacity();
        let slots=if required>capacity {required.max(capacity.checked_mul(2).ok_or(Error::LimitExceeded)?).max(4)} else {capacity};
        let slots=slots.checked_mul(core::mem::size_of::<(NodeId,Rc<str>)>()).and_then(|bytes|bytes.checked_add(self.cryptographic_nonce_bytes)).and_then(|bytes|bytes.checked_add(moved_nonce_bytes)).ok_or(Error::LimitExceeded)?;
        if slots>html::MAX_HTML_BYTES {return Err(Error::LimitExceeded);}
        self.cryptographic_nonces.try_reserve(moved_nonces).map_err(|_|Error::LimitExceeded)?;
        let mut remaining_nonces=Vec::new();remaining_nonces.try_reserve(source.cryptographic_nonces.len()-moved_nonces).map_err(|_|Error::LimitExceeded)?;
        let mut mapping = Vec::new();
        mapping
            .try_reserve(originals.len())
            .map_err(|_| Error::LimitExceeded)?;
        source.reserve_inert_detach(&[root])?;
        if needs_template_owner { self.ensure_template_owner_document()?; }

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
        for (node,value) in core::mem::take(&mut source.cryptographic_nonces) {
            if let Some(adopted)=mapped_node(node) {source.cryptographic_nonce_bytes-=value.len();self.store_nonce(adopted,Some(value));}
            else {remaining_nonces.push((node,value));}
        }
        source.cryptographic_nonces=remaining_nonces;

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
            source.update_tracked_node_count();
            self.update_tracked_node_count();
        }

        for (owner, namespaces) in core::mem::take(&mut source.attribute_namespaces) {
            if let Some(target) = mapped_node(owner) {
                self.attribute_namespaces.push((target, namespaces));
            } else {
                remaining_namespaces.push((owner, namespaces));
            }
        }
        source.attribute_namespaces = remaining_namespaces;
        source.inert_document_roots.retain(|id| mapped_node(*id).is_none());
        for owner in core::mem::take(&mut source.literal_colon_names) {
            if let Some(target) = mapped_node(owner) {
                self.literal_colon_names.push(target);
            } else {
                remaining_literal_names.push(owner);
            }
        }
        self.literal_colon_names.sort_unstable_by_key(|id| id.index);
        source.literal_colon_names = remaining_literal_names;
        if moved_is_values != 0 {
            for (owner, value) in core::mem::take(&mut source.custom_element_is_values) {
                if let Some(target) = mapped_node(owner) {
                    self.custom_element_is_values.push((target, value));
                } else {
                    remaining_is_values.push((owner, value));
                }
            }
            self.custom_element_is_values.sort_unstable_by_key(|(id, _)| id.index);
            source.custom_element_is_values = remaining_is_values;
        }
        if source.inline_cssom_read_cache.as_ref().is_some_and(|(owner, _)| mapped_node(*owner).is_some()) {
            source.inline_cssom_read_cache = None;
        }
        for (owner, block) in core::mem::take(&mut source.inline_cssom_styles) {
            if let Some(target) = mapped_node(owner) {
                self.inline_cssom_styles.push((target, block));
            } else {
                remaining_styles.push((owner, block));
            }
        }
        self.inline_cssom_styles.sort_unstable_by_key(|(id, _)| id.index);
        source.inline_cssom_styles = remaining_styles;
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
            if let Some(host) = mapped_node(tree.host) {
                let root = mapped_node(tree.root).expect("shadow root is in adoption subtree");
                self.shadow_trees.push(shadow::ShadowTree {
                    host,
                    root,
                    mode: tree.mode,
                    options: tree.options,
                });
                self.shadow_hosts_by_root.push((root, host));
            } else {
                remaining_shadows.push(tree);
            }
        }
        if moved_shadows != 0 {
            self.shadow_trees.sort_unstable_by_key(|tree| tree.host.key());
            self.shadow_hosts_by_root.sort_unstable_by_key(|(root, _)| root.key());
            source
                .shadow_hosts_by_root
                .retain(|(_, host)| mapped_node(*host).is_none());
            source.compact_shadow_host_index();
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
        source.parser_form_owners.get_mut().retain(|(control, form)| {
            if let Some(new_control) = mapped_node(*control) {
                let new_form=mapped_node(*form).or_else(|| (form.document==self.id && self.kind(*form).is_ok()).then_some(*form));
                if let Some(new_form) = new_form { self.parser_form_owners.get_mut().push((new_control, new_form)); }
                false
            } else { true }
        });

        if let Some(host) = detached_template_host {
            let content = mapped_node(root).expect("mapped content");
            let host = mapped_node(host).unwrap_or(host);
            self.set_template_link(content, host)?;
            if host.document == source.id { source.set_template_link(host, content)?; }
        }

        for (local, remote) in foreign_links {
            source.foreign_template_links.retain(|(node, _)| *node != local);
            self.set_template_link(mapped_node(local).expect("mapped template edge"), mapped_node(remote).unwrap_or(remote))?;
        }

        if templates != 0 {
            let owner = self.template_owner_document.ok_or(Error::InvalidNode)?;
            for &(_, node) in &mapping {
                if let Some(content) = self.template_content(node)? {
                    if content.document == self.id { self.set_node_document(content, owner)?; }
                }
            }
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
        let literal_colon = source.literal_colon_name(node);
        if literal_colon { self.literal_colon_names.try_reserve(1).map_err(|_| Error::LimitExceeded)?; }
        let is_value = source.custom_element_is_rc(node).cloned();
        if is_value.is_some() { self.custom_element_is_values.try_reserve(1).map_err(|_| Error::LimitExceeded)?; }
        let nonce=source.nonce_rc(node).cloned();
        if let Some(value)=&nonce {self.reserve_nonce(value.len(),0)?;}
        let clone = self.create_node_without_details(kind, false, literal_colon)?;
        if let Some(value) = is_value { self.insert_custom_element_is(clone, value); }
        if literal_colon { self.insert_literal_colon_name(clone); }
        if !namespaces.is_empty() {
            self.attribute_namespaces.push((clone, namespaces));
        }
        if let Some((public_id, system_id)) = doctype_identifiers {
            self.doctype_identifiers.push((clone, public_id, system_id));
        }
        if self.includes_nonce(clone)? {self.store_nonce(clone,nonce);}
        self.details_created(clone)?;
        Ok(clone)
    }

    fn attach_detached(&mut self, parent: NodeId, child: NodeId) {
        self.insert_detached_before(parent, child, None);
    }

    fn clear_detached_node_document(&mut self, child: NodeId) {
        if let Ok(index) = self.inert_document_roots.binary_search_by_key(&child.index, |id| id.index) {
            if self.inert_document_roots[index] == child { self.inert_document_roots.remove(index); }
        }
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
        // Linked roots inherit the destination document. Template-content
        // fragments remain inert through their independent host link.
        self.clear_detached_node_document(child);
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

    fn node(&self, id: NodeId) -> Result<&Node, Error> {
        if id.document != self.id {
            return Err(Error::InvalidNode);
        }
        self.nodes
            .get(id.index as usize)
            .filter(|node| node.alive && node.generation == id.generation)
            .ok_or(Error::InvalidNode)
    }

    fn literal_colon_name(&self, id: NodeId) -> bool {
        self.literal_colon_names.binary_search_by_key(&id.index, |known| known.index)
            .is_ok_and(|index| self.literal_colon_names[index] == id)
    }

    fn insert_literal_colon_name(&mut self, id: NodeId) {
        let index = self.literal_colon_names.binary_search_by_key(&id.index, |known| known.index)
            .unwrap_or_else(|index| index);
        self.literal_colon_names.insert(index, id);
    }

    /// Create an Element with a literal local name and no prefix, as document.createElement
    /// does. Only colon-containing names need metadata beyond the compact element record.
    pub fn create_unprefixed_element(&mut self, namespace: Namespace, name: Name,
        attributes: Vec<(Name, String)>) -> Result<NodeId, Error> {
        let kind = NodeKind::Element { namespace, name, attributes };
        let is_value = Self::authored_is_value(&kind);
        self.create_unprefixed_with_is_rc(kind, is_value)
    }

    pub fn create_unprefixed_element_with_is_value(&mut self, namespace: Namespace, name: Name,
        attributes: Vec<(Name, String)>, is_value: Option<&str>) -> Result<NodeId, Error> {
        let is_value = if namespace == Namespace::Html { is_value.map(Rc::from) } else { None };
        self.create_unprefixed_with_is_rc(NodeKind::Element { namespace, name, attributes }, is_value)
    }

    fn create_unprefixed_with_is_rc(&mut self, kind: NodeKind, is_value: Option<Rc<str>>) -> Result<NodeId, Error> {
        let NodeKind::Element { name, .. } = &kind else { return Err(Error::WrongKind); };
        let literal_colon = name.contains(':');
        if literal_colon {
            self.literal_colon_names.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
        }
        if is_value.is_some() { self.custom_element_is_values.try_reserve(1).map_err(|_| Error::LimitExceeded)?; }
        let id = self.create_node_without_details(kind, false, literal_colon)?;
        if let Some(value) = is_value { self.insert_custom_element_is(id, value); }
        if literal_colon { self.insert_literal_colon_name(id); }
        self.details_created(id)?;
        Ok(id)
    }

    /// Borrow the DOM prefix/local-name pair without allocating or splitting a literal colon.
    pub fn element_name_parts(&self, id: NodeId) -> Result<(Option<&str>, &str), Error> {
        let NodeKind::Element { name, .. } = self.kind(id)? else { return Err(Error::WrongKind); };
        if name.contains(':') && !self.literal_colon_name(id) {
            if let Some((prefix, local)) = name.split_once(':') { return Ok((Some(prefix), local)); }
        }
        Ok((None, name.as_str()))
    }

    /// Apply the namespace-prefix step of DOM element creation to the actual
    /// successful custom-constructor result, before authored token attributes.
    /// This internal construction state change is not an attribute mutation.
    pub fn set_created_element_prefix(&mut self,id:NodeId,prefix:Option<&str>)->Result<(),Error> {
        let (old_prefix,local)=self.element_name_parts(id)?;
        if old_prefix==prefix {return Ok(());}
        let prefix_length=prefix.map_or(Some(0),|prefix|prefix.len().checked_add(1)).ok_or(Error::LimitExceeded)?;
        let length=local.len().checked_add(prefix_length).ok_or(Error::LimitExceeded)?;
        let mut qualified=String::new();qualified.try_reserve_exact(length).map_err(|_|Error::LimitExceeded)?;
        if let Some(prefix)=prefix {qualified.push_str(prefix);qualified.push(':');}
        qualified.push_str(local);
        let name=Name::new(&qualified);
        let NodeKind::Element {name:stored,..}=&mut self.node_mut_checked(id)?.kind else {return Err(Error::WrongKind);};
        *stored=name;
        if let Ok(index)=self.literal_colon_names.binary_search_by_key(&id.index,|known|known.index) {
            self.literal_colon_names.remove(index);
        }
        Ok(())
    }

    /// Borrow namespace metadata by attribute position, without allocating.
    /// Positions distinguish equal qualified names in different namespaces.
    pub fn attribute_namespace_uri_at(&self, id: NodeId, index: usize) -> Option<&str> {
        self.attribute_namespace_metadata(id)
            .iter().find(|(known, _)| *known == index)
            .map(|(_, uri)| uri.as_ref())
    }

    pub(crate) fn attribute_namespace_metadata(&self, id: NodeId) -> &[(usize, Rc<str>)] {
        self.attribute_namespaces.iter().find(|(owner, _)| *owner == id)
            .map_or(&[], |(_, namespaces)| namespaces.as_slice())
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
        let style_changed = (qualified_name == "style" && namespace_uri.is_none())
            || index.is_some_and(|index| {
                matches!(&self.node(element).expect("validated element").kind, NodeKind::Element { attributes, .. } if attributes[index].0 == "style")
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
        let attribute_owner = self.node_document(element)?;
        if old_attribute.is_some() { self.reserve_node_document(attribute_owner)?; }
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
        if style_changed {
            let allowed = if qualified_name == "style" && namespace_uri.is_none() {
                match &self.inline_style_attribute_policy {
                    Some(policy) => policy(self, element, &value)?,
                    None => true,
                }
            } else { true };
            self.node_mut_checked(element)?.style_attribute_blocked = !allowed;
            self.replace_inline_cssom_state(element, None);
        }
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
            self.set_node_document(old_attribute, attribute_owner)?;
            if let NodeKind::Attribute { owner_element, .. } =
                &mut self.node_mut(old_attribute).kind
            {
                *owner_element = None;
            }
        }
        if let NodeKind::Attribute { owner_element, .. } = &mut self.node_mut(attribute).kind {
            *owner_element = Some(Box::new(element));
        }
        if let Ok(index) = self.inert_document_roots.binary_search_by_key(&attribute.index, |id| id.index) {
            self.inert_document_roots.remove(index);
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
        let style_attribute = matches!(self.kind(attribute)?, NodeKind::Attribute { qualified_name, namespace_uri, .. }
            if qualified_name == "style" && namespace_uri.is_none());
        let style_changed = if style_attribute {
            let allowed = match &self.inline_style_attribute_policy {
                Some(policy) => policy(self, owner, value)?,
                None => true,
            };
            let policy_changed = self.node(owner)?.style_attribute_blocked == allowed;
            self.node_mut_checked(owner)?.style_attribute_blocked = !allowed;
            self.replace_inline_cssom_state(owner, None) || policy_changed
        } else { false };
        if unchanged {
            if style_changed {
                self.mark_dirty(owner, Dirty::STYLE, MutationKind::Attribute(Name::new("style")));
            }
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
            || self.foreign_template_links.iter().any(|(local, _)| *local == id)
            || self
                .shadow_trees
                .binary_search_by_key(&id.key(), |tree| tree.host.key())
                .is_ok()
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
                self.shadow_host_from_index(id)
            }) {
                id = parent;
            } else {
                break;
            }
        }
        self.version = self.version.wrapping_add(1);
        // A rebuild summarizes changes through its original version. Keep
        // later identities precise: a source accepted after that barrier must
        // not become stale merely because an unrelated node changes afterward.
        // Actual journal overflow still introduces a new current barrier.
        if self.journal.len() == MAX_JOURNAL {
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
        self.reserve_inert_detach(&inserting)?;
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
        self.collect_insertion_roots(nodes, Some(parent))
    }

    /// Snapshot the unique roots moved by a node list, in its final insertion
    /// order. Fragments expand to their children; later occurrences win.
    /// This does not validate a destination or mutate the source tree.
    pub fn insertion_roots(&self, nodes: &[NodeId]) -> Result<Vec<NodeId>, Error> {
        self.collect_insertion_roots(nodes, None)
    }

    fn collect_insertion_roots(&self, nodes: &[NodeId], parent: Option<NodeId>) -> Result<Vec<NodeId>, Error> {
        let mut inserting = Vec::new();
        for (index, &node) in nodes.iter().enumerate() {
            if Some(node) == parent {
                return Err(Error::Hierarchy);
            }
            // Expanding the same fragment repeatedly cannot contribute any
            // earlier roots: its final occurrence moves all of them again.
            if nodes[index + 1..].contains(&node) { continue; }
            for &candidate in self.inserting_nodes(node)?.as_slice() {
                if parent.is_some() && (Some(candidate) == parent || self.can_be_ancestor(candidate)) {
                    if self.is_host_including_inclusive_ancestor(candidate, parent.unwrap())? { return Err(Error::Hierarchy); }
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
        let before = if before == Some(child) { self.next_sibling(child)? } else { before };
        self.reserve_inert_detach(inserting.as_slice())?;
        match inserting {
            InsertingNodes::One([candidate]) => self.insert_validated(parent, candidate, before)?,
            InsertingNodes::Fragment(added) => {
                if added.is_empty() { return Ok(()); }
                let observing = self.mutation_observer_sink.is_some();
                let mut fragment_removed = Vec::new();
                if observing {
                    fragment_removed.try_reserve_exact(added.len()).map_err(|_| Error::LimitExceeded)?;
                    fragment_removed.extend_from_slice(&added);
                }
                for &candidate in &added { self.remove_with_observers(candidate, false)?; }
                if observing {
                    self.notify_observer(&observe::ObservedMutation { target: child,
                        kind: observe::ObservedKind::ChildListMany { added: Vec::new(), removed: fragment_removed } });
                }
                let previous_sibling = match before { Some(reference) => self.previous_sibling(reference)?, None => self.last_child(parent)? };
                for &candidate in &added { self.insert_validated_with_observers(parent, candidate, before, false)?; }
                if observing && !added.is_empty() {
                    self.notify_observer(&observe::ObservedMutation { target: parent,
                        kind: observe::ObservedKind::ChildListReplacement { added, removed: Vec::new(), previous_sibling, next_sibling: before } });
                }
            }
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

    pub(crate) fn validate_parser_insert_from(
        &self,source:&Document,parent:NodeId,child:NodeId,before:Option<NodeId>,
        ancestor:impl FnOnce(NodeId,NodeId)->Result<bool,Error>,
    )->Result<(),Error> {
        self.validate_parent_reference_from_with_ancestor(source,parent,child,before,ancestor)?;
        let inserting=source.inserting_nodes(child)?;
        // A tree-builder token or reconstruction node is always a single node.
        if matches!(inserting,InsertingNodes::Fragment(_)) {return Err(Error::Hierarchy);}
        if before!=Some(child) {self.validate_insertion_from(source,parent,inserting.as_slice(),before,None)?;}
        Ok(())
    }
    pub(crate) fn insert_parser_validated(&mut self,parent:NodeId,child:NodeId,before:Option<NodeId>)->Result<(),Error> {
        self.reserve_inert_detach(&[child])?;
        self.insert_validated(parent,child,before)
    }

    /// The fragment removal phase of DOM insert. Internal tree hooks run for
    /// every child; observers see only the fragment's complete removal record.
    pub fn remove_fragment_children(&mut self, fragment: NodeId) -> Result<Vec<NodeId>, Error> {
        if !matches!(self.kind(fragment)?, NodeKind::DocumentFragment) || self.shadow_host(fragment)?.is_some() { return Err(Error::Hierarchy); }
        let added = self.insertion_roots(&[fragment])?;
        self.reserve_inert_detach(&added)?;
        for &child in &added { self.remove_with_observers(child, false)?; }
        if !added.is_empty() && self.mutation_observer_sink.is_some() {
            self.notify_observer(&observe::ObservedMutation { target: fragment,
                kind: observe::ObservedKind::ChildListMany { added: Vec::new(), removed: added.clone() } });
        }
        Ok(added)
    }

    /// Finish a fragment insertion after the host has adopted its children.
    /// No synthetic fragment is allocated and only one observer record is
    /// published, while each inserted child retains the normal internal hooks.
    pub fn insert_fragment_roots(&mut self, parent: NodeId, added: Vec<NodeId>, before: Option<NodeId>) -> Result<(), Error> {
        if added.is_empty() { return Ok(()); }
        self.validate_insertion(parent, &added, before, None)?;
        self.reserve_inert_detach(&added)?;
        let previous_sibling = match before { Some(reference) => self.previous_sibling(reference)?, None => self.last_child(parent)? };
        for &child in &added { self.insert_validated_with_observers(parent, child, before, false)?; }
        if self.mutation_observer_sink.is_some() {
            self.notify_observer(&observe::ObservedMutation { target: parent,
                kind: observe::ObservedKind::ChildListReplacement { added, removed: Vec::new(), previous_sibling, next_sibling: before } });
        }
        Ok(())
    }

    /// Finish replacing one child with already adopted fragment children.
    pub fn replace_fragment_roots(&mut self, old: NodeId, added: Vec<NodeId>) -> Result<(), Error> {
        let parent = self.parent(old)?.ok_or(Error::Hierarchy)?;
        let before = self.next_sibling(old)?;
        self.validate_insertion(parent, &added, before, Some(old))?;
        self.reserve_inert_detach(&added)?;
        self.reserve_inert_detach(&[old])?;
        let previous_sibling = self.previous_sibling(old)?;
        self.remove_with_observers(old, false)?;
        for &child in &added { self.insert_validated_with_observers(parent, child, before, false)?; }
        if self.mutation_observer_sink.is_some() {
            self.notify_observer(&observe::ObservedMutation { target: parent,
                kind: observe::ObservedKind::ChildListReplacement { added, removed: alloc::vec![old], previous_sibling, next_sibling: before } });
        }
        Ok(())
    }

    /// DOM replace-all removal phase, retaining per-child internal hooks.
    pub fn remove_children_for_replacement(&mut self, parent: NodeId) -> Result<Vec<NodeId>, Error> {
        let mut removed = Vec::new();
        let mut child = self.first_child(parent)?;
        while let Some(id) = child { removed.push(id); child = self.next_sibling(id)?; }
        self.reserve_inert_detach(&removed)?;
        for &id in &removed { self.remove_with_observers(id, false)?; }
        Ok(removed)
    }

    pub fn insert_replacement_roots(&mut self, parent: NodeId, added: Vec<NodeId>, removed: Vec<NodeId>) -> Result<(), Error> {
        self.validate_insertion(parent, &added, None, None)?;
        self.reserve_inert_detach(&added)?;
        for &id in &added { self.insert_validated_with_observers(parent, id, None, false)?; }
        if (!added.is_empty() || !removed.is_empty()) && self.mutation_observer_sink.is_some() {
            self.notify_observer(&observe::ObservedMutation { target: parent,
                kind: observe::ObservedKind::ChildListMany { added, removed } });
        }
        Ok(())
    }

    fn validate_parent_reference_from(
        &self,
        source: &Document,
        parent: NodeId,
        child: NodeId,
        before: Option<NodeId>,
    ) -> Result<(), Error> {
        self.validate_parent_reference_from_with_ancestor(source,parent,child,before,|ancestor,node|self.is_host_including_inclusive_ancestor(ancestor,node))
    }

    fn validate_parent_reference_from_with_ancestor(
        &self,source:&Document,parent:NodeId,child:NodeId,before:Option<NodeId>,
        ancestor:impl FnOnce(NodeId,NodeId)->Result<bool,Error>,
    )->Result<(),Error> {
        let parent_kind = self.kind(parent)?;
        if !matches!(
            parent_kind,
            NodeKind::Document | NodeKind::DocumentFragment | NodeKind::Element { .. }
        ) {
            return Err(Error::Hierarchy);
        }
        source.kind(child)?;
        if child == parent || source.can_be_ancestor(child) {
            if ancestor(child, parent)? { return Err(Error::Hierarchy); }
        }
        if let Some(before) = before {
            if before.document != self.id || self.parent(before)? != Some(parent) {
                return Err(Error::NotFound);
            }
        }
        Ok(())
    }

    fn prepare_insert_from(&self, source: &Document, parent: NodeId, child: NodeId, before: Option<NodeId>) -> Result<InsertingNodes, Error> {
        self.validate_parent_reference_from(source, parent, child, before)?;
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
        self.insert_validated_with_observers(parent, child, before, true)
    }

    fn insert_validated_with_observers(&mut self, parent: NodeId, child: NodeId, before: Option<NodeId>, observers: bool) -> Result<(), Error> {
        if self.parent(child)?.is_some() {
            self.remove(child)?;
        }
        let prev = self.link_before(parent, child, before)?;
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
            let mutation = observe::ObservedMutation {
                target: parent,
                kind: observe::ObservedKind::ChildList {
                    added: Some(child),
                    removed: None,
                    previous_sibling: prev,
                    next_sibling: before,
                },
            };
            self.notify_internal(&mutation);
            if observers { self.notify_observer(&mutation); }
        }
        self.process_inserted_language_pragmas(child)?;
        self.hide_inserted_nonces(child)?;
        self.details_inserted(child)?;
        Ok(())
    }

    /// Rewire an already validated insertion without removal/insertion steps.
    fn link_before(&mut self, parent: NodeId, child: NodeId, before: Option<NodeId>) -> Result<Option<NodeId>, Error> {
        let prev = if let Some(before) = before {
            self.previous_sibling(before)?
        } else {
            self.last_child(parent)?
        };
        self.clear_detached_node_document(child);
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
        Ok(prev)
    }

    /// State-preserving move within one shadow-including tree. Observers see
    /// the committed tree for both records; interaction and owner state never
    /// pass through the ordinary detached-node lifecycle.
    pub fn move_before(&mut self, parent: NodeId, child: NodeId, before: Option<NodeId>) -> Result<(), Error> {
        self.validate_move_before(parent, child, before)?;
        let before = if before == Some(child) { self.next_sibling(child)? } else { before };
        let old_parent = self.parent(child)?.ok_or(Error::Hierarchy)?;
        let old_previous = self.previous_sibling(child)?;
        let old_next = self.next_sibling(child)?;
        self.detach_links(child)?;
        let previous = self.link_before(parent, child, before)?;
        for (target, added, removed, previous_sibling, next_sibling) in [
            (old_parent, None, Some(child), old_previous, old_next),
            (parent, Some(child), None, previous, before),
        ] {
            self.mark_dirty(target, Dirty::STYLE, MutationKind::Tree { added, removed, styles_changed: false });
            if self.observing_mutations() {
                self.notify(observe::ObservedMutation { target, kind: observe::ObservedKind::ChildList {
                    added, removed, previous_sibling, next_sibling,
                }});
            }
        }
        Ok(())
    }

    pub fn validate_move_before(&self, parent: NodeId, child: NodeId, before: Option<NodeId>) -> Result<(), Error> {
        self.kind(parent)?;
        self.kind(child)?;
        if self.root_node(parent, true)? != self.root_node(child, true)? { return Err(Error::Hierarchy); }
        let before = if before == Some(child) { self.next_sibling(child)? } else { before };
        self.validate_parent_reference_from(self, parent, child, before)?;
        // Reuse pre-insertion's ancestor/reference checks and ordinary
        // document hierarchy validation. Move additionally excludes doctype,
        // fragment, and attribute nodes and existing document element moves.
        if !matches!(self.kind(child)?, NodeKind::Element { .. } | NodeKind::Text(_) | NodeKind::CData(_) |
            NodeKind::Comment(_) | NodeKind::ProcessingInstruction { .. }) { return Err(Error::Hierarchy); }
        if matches!(self.kind(parent)?, NodeKind::Document) && matches!(self.kind(child)?, NodeKind::Element { .. }) {
            let mut current = self.first_child(parent)?;
            while let Some(id) = current {
                if matches!(self.kind(id)?, NodeKind::Element { .. }) { return Err(Error::Hierarchy); }
                current = self.next_sibling(id)?;
            }
        }
        self.validate_insertion(parent, &[child], before, None)?;
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
        self.remove_with_observers(child, true)
    }

    fn remove_with_observers(&mut self, child: NodeId, observers: bool) -> Result<(), Error> {
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
                let mutation = observe::ObservedMutation {
                    target: parent,
                    kind: observe::ObservedKind::ChildList {
                        added: None,
                        removed: Some(child),
                        previous_sibling: siblings.0,
                        next_sibling: siblings.1,
                    },
                };
                self.notify_internal(&mutation);
                if observers { self.notify_observer(&mutation); }
            }
        }
        Ok(())
    }

    fn detach(&mut self, child: NodeId) -> Result<Option<NodeId>, Error> {
        let node = self.node(child)?;
        if self.link_id(node.parent).is_none() {
            return Ok(None);
        }
        let owner = self.node_document(child)?;
        self.set_node_document(child, owner)?;
        self.clear_interaction_subtree(child);
        self.detach_links(child)
    }

    fn detach_links(&mut self, child: NodeId) -> Result<Option<NodeId>, Error> {
        let node = self.node(child)?;
        let Some(parent) = self.link_id(node.parent) else { return Ok(None); };
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
            if self.is_host_including_inclusive_ancestor(candidate, parent)? { return Err(Error::Hierarchy); }
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
        let (parent, inserting, before) = self.prepare_replacement(self, old, new)?;
        self.reserve_inert_detach(inserting.as_slice())?;
        self.reserve_inert_detach(&[old])?;
        let observing = self.mutation_observer_sink.is_some();
        let previous = self.previous_sibling(old)?;
        let mut removed = Vec::new();
        if observing && matches!(&inserting, InsertingNodes::Fragment(_)) {
            removed.try_reserve_exact(1).map_err(|_| Error::LimitExceeded)?;
        }
        let mut fragment_removed = Vec::new();
        if observing && matches!(&inserting, InsertingNodes::Fragment(_)) {
            fragment_removed.try_reserve_exact(inserting.as_slice().len()).map_err(|_| Error::LimitExceeded)?;
            fragment_removed.extend_from_slice(inserting.as_slice());
        }
        // Adoption/removal of the replacement precedes removal of the old
        // child. Its original parent still receives its own observer record.
        for &candidate in inserting.as_slice() {
            self.remove_with_observers(candidate, !matches!(&inserting, InsertingNodes::Fragment(_)))?;
        }
        if !fragment_removed.is_empty() {
            self.notify_observer(&observe::ObservedMutation { target: new,
                kind: observe::ObservedKind::ChildListMany { added: Vec::new(), removed: fragment_removed } });
        }
        let removed_old = self.parent(old)? == Some(parent);
        if removed_old {
            self.remove_with_observers(old, false)?;
            if observing && matches!(&inserting, InsertingNodes::Fragment(_)) { removed.push(old); }
        }
        for &candidate in inserting.as_slice() {
            self.insert_validated_with_observers(parent, candidate, before, false)?;
        }
        if observing {
            match inserting {
                InsertingNodes::One([added]) => self.notify_observer(&observe::ObservedMutation {
                    target: parent, kind: observe::ObservedKind::ChildList {
                        added: Some(added), removed: removed_old.then_some(old), previous_sibling: previous, next_sibling: before,
                    },
                }),
                InsertingNodes::Fragment(added) => self.notify_observer(&observe::ObservedMutation {
                    target: parent, kind: observe::ObservedKind::ChildListReplacement {
                        added, removed, previous_sibling: previous, next_sibling: before,
                    },
                }),
            }
        }
        Ok(())
    }

    /// Ensure pre-insert validity with all destination children excluded,
    /// before a host adopts a foreign replaceChildren singleton.
    pub fn validate_replace_children_from(&self, source: &Document, parent: NodeId, replacement: NodeId) -> Result<(), Error> {
        self.validate_parent_reference_from(source, parent, replacement, None)?;
        let inserting = source.inserting_nodes(replacement)?;
        if matches!(self.kind(parent)?, NodeKind::Document) {
            self.validate_document_children_from(source, inserting.as_slice())
        } else {
            self.validate_insertion_from(source, parent, inserting.as_slice(), None, None)
        }
    }

    /// Replace an element or fragment's children as one update.
    pub fn replace_children(&mut self, parent: NodeId, replacement: NodeId) -> Result<(), Error> {
        self.validate_replace_children_from(self, parent, replacement)?;
        self.reserve_inert_detach(&[replacement])?;
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
        let mut detached_count = 0usize;
        let observing = self.observing_mutations();
        let mut removed = Vec::new();
        while let Some(id) = old_child {
            detached_count += 1;
            if observing {
                removed.push(id);
            }
            styles_changed |= session::subtree_has_style(self, id);
            old_child = self.next_sibling(id)?;
        }
        if self.template_owner_document.is_some() && (
            self.is_template_owner_document(self.node_document(parent)?) ||
            inserting.iter().any(|id| self.node_document(*id).ok().is_some_and(|owner| self.is_template_owner_document(owner)))) {
            self.inert_document_roots.try_reserve(detached_count.saturating_add(inserting.len())).map_err(|_| Error::LimitExceeded)?;
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
                self.detach_all_children(node)?;
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
        self.detach_all_children(parent)?;
        for &candidate in &inserting {
            self.attach_detached(parent, candidate);
            self.process_inserted_language_pragmas(candidate)?;
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

    fn detach_all_children(&mut self, parent: NodeId) -> Result<(), Error> {
        let owner = self.node_document(parent)?;
        if self.is_template_owner_document(owner) {
            let mut count = 0usize;
            let mut child = self.first_child(parent)?;
            while let Some(node) = child { count += 1; child = self.next_sibling(node)?; }
            self.inert_document_roots.try_reserve(count).map_err(|_| Error::LimitExceeded)?;
        }
        let mut child = self.nodes[parent.index as usize].first_child;
        self.node_mut(parent).first_child = NO_LINK;
        self.node_mut(parent).last_child = NO_LINK;
        while child != NO_LINK {
            let next = self.nodes[child as usize].next_sibling;
            let id = NodeId { document: self.id, index: child, generation: self.nodes[child as usize].generation };
            self.set_node_document(id, owner)?;
            let node = &mut self.nodes[child as usize];
            node.parent = NO_LINK;
            node.prev_sibling = NO_LINK;
            node.next_sibling = NO_LINK;
            child = next;
        }
        Ok(())
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
        if index.is_none() {
            if let NodeKind::Element { attributes, .. } = &mut self.node_mut_checked(id)?.kind {
                attributes.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
            }
        }
        self.nonce_attribute_changed(id,namespace_uri.as_deref(),name,Some(value))?;
        let cssom_changed = if name == "style" && namespace_uri.is_none() {
            let allowed = match &self.inline_style_attribute_policy {
                Some(policy) => policy(self, id, value)?,
                None => true,
            };
            let policy_changed = self.node(id)?.style_attribute_blocked == allowed;
            self.node_mut_checked(id)?.style_attribute_blocked = !allowed;
            self.replace_inline_cssom_state(id, None) || policy_changed
        } else { false };
        let observing = self.observing_mutations();
        let (key, old_value, changed) = if let Some(index) = index {
            let (key, old_value, changed) = {
                let NodeKind::Element { attributes, .. } = &mut self.node_mut_checked(id)?.kind
                else {
                    return Err(Error::WrongKind);
                };
                let (key, old) = &mut attributes[index];
                let changed = old.as_str() != value || cssom_changed;
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
        let materialized = self.attribute_nodes.iter().find(|(owner, _)| *owner == id)
            .and_then(|(_, nodes)| nodes.binary_search_by_key(&index, |(known, _)| *known)
                .ok().map(|position| nodes[position].1));
        if let Some(attribute) = materialized {
            let owner = self.node_document(id)?;
            self.reserve_node_document(owner)?;
            self.set_node_document(attribute, owner)?;
        }
        let nonce_removed=matches!(self.kind(id)?,NodeKind::Element {attributes,..} if attributes[index].0=="nonce") && self.attribute_namespace_uri_at(id,index).is_none();
        if nonce_removed {self.nonce_attribute_changed(id,None,"nonce",None)?;}
        let details_before = self.details_open_state(id);
        let name_changed = matches!(&self.node(id)?.kind, NodeKind::Element { attributes, .. } if attributes[index].0 == "name")
            && self.attribute_namespace_uri_at(id, index).is_none();
        if matches!(&self.node(id)?.kind, NodeKind::Element { attributes, .. } if attributes[index].0 == "style")
            && self.attribute_namespace_uri_at(id, index).is_none() {
            self.node_mut_checked(id)?.style_attribute_blocked = false;
            self.replace_inline_cssom_state(id, None);
        }
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
        if qualified_name == "style" && self.replace_inline_cssom_state(id, None) {
            self.mark_dirty(id, Dirty::STYLE, MutationKind::Attribute(Name::new("style")));
        }
        self.set_attribute_namespace_at(id, index, namespace_uri);
        let (name, value) = match &self.node(id)?.kind {
            NodeKind::Element { attributes, .. } => {
                (attributes[index].0.clone(), attributes[index].1.clone())
            }
            _ => return Err(Error::WrongKind),
        };
        self.sync_materialized_attribute(id, index, &name, &value, namespace_uri);
        if qualified_name=="nonce" && namespace_uri.is_some() && self.includes_nonce(id)? {
            let nonce=String::from(self.get_attribute_ns_ref(id,None,"nonce")?.unwrap_or(""));
            self.set_cryptographic_nonce(id,&nonce)?;
        }
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
        self.set_attribute_ns_with_style(id, namespace_uri, qualified_name, value, None, false)
    }

    /// Borrow retained CSSOM declarations without allocating for ordinary elements.
    pub fn inline_cssom_style(&self, id: NodeId) -> Option<&css::DeclarationBlock> {
        self.inline_cssom_styles.binary_search_by_key(&id.index, |(owner, _)| owner.index)
            .ok().and_then(|position| {
                let (owner, block) = &self.inline_cssom_styles[position];
                (*owner == id).then_some(block.as_ref())
            })
    }

    /// Return the retained semantic state or lazily parse the last read inline
    /// attribute. Reading never rewrites attributes or publishes DOM mutations.
    pub fn read_inline_cssom_style(&mut self, id: NodeId) -> Result<Option<Rc<css::DeclarationBlock>>, css::CssError> {
        if !matches!(self.kind(id), Ok(NodeKind::Element { .. })) { return Ok(None); }
        if let Ok(position) = self.inline_cssom_styles.binary_search_by_key(&id.index, |(owner, _)| owner.index) {
            let (owner, block) = &self.inline_cssom_styles[position];
            if *owner == id { return Ok(Some(block.clone())); }
        }
        if let Some((owner, block)) = &self.inline_cssom_read_cache {
            if *owner == id { return Ok(Some(block.clone())); }
        }
        let raw = self.get_attribute_ns_ref(id, None, "style").ok().flatten().unwrap_or("");
        let block = Rc::new(css::DeclarationBlock::parse(raw)?);
        self.inline_cssom_read_cache = Some((id, block.clone()));
        Ok(Some(block))
    }

    /// Commit attribute text and parsed declarations together, before observers
    /// can read them. Reserve sparse storage before changing the DOM.
    pub fn set_inline_cssom_style(&mut self, id: NodeId, block: css::DeclarationBlock) -> Result<(), Error> {
        if !matches!(self.kind(id)?, NodeKind::Element { .. }) { return Err(Error::WrongKind); }
        let text = block.serialize().map_err(|_| Error::LimitExceeded)?;
        if self.inline_cssom_style(id).is_none() {
            self.inline_cssom_styles.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
        }
        self.set_attribute_ns_with_style(id, None, "style", &text, Some(Rc::new(block)), false)
    }

    fn replace_inline_cssom_state(&mut self, id: NodeId, block: Option<Rc<css::DeclarationBlock>>) -> bool {
        if self.inline_cssom_read_cache.as_ref().is_some_and(|(owner, _)| *owner == id) {
            self.inline_cssom_read_cache = None;
        }
        match (self.inline_cssom_styles.binary_search_by_key(&id.index, |(owner, _)| owner.index), block) {
            (Ok(position), Some(block)) => {
                if self.inline_cssom_styles[position].0 == id && *self.inline_cssom_styles[position].1 == *block {
                    return false;
                }
                self.inline_cssom_styles[position] = (id, block);
                true
            }
            (Err(position), Some(block)) => {
                self.inline_cssom_styles.insert(position, (id, block));
                true
            }
            (Ok(position), None) => { self.inline_cssom_styles.remove(position); true }
            (Err(_), None) => false,
        }
    }

    fn set_attribute_ns_with_style(
        &mut self,
        id: NodeId,
        namespace_uri: Option<&str>,
        qualified_name: &str,
        value: &str,
        block: Option<Rc<css::DeclarationBlock>>,
        preserve_cryptographic_nonce: bool,
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
        if !preserve_cryptographic_nonce {self.nonce_attribute_changed(id,namespace_uri,local,Some(value))?;}
        let style_attribute = namespace_uri.is_none() && qualified_name == "style";
        let mut policy_changed = false;
        if style_attribute {
            // CSSOM updates carry the declaration block's updating flag. They
            // are actual property edits, not a new authored attribute parse.
            let allowed = block.is_some() || match &self.inline_style_attribute_policy {
                Some(policy) => policy(self, id, value)?,
                None => true,
            };
            let node = self.node_mut_checked(id)?;
            policy_changed = node.style_attribute_blocked == allowed;
            node.style_attribute_blocked = !allowed;
        }
        let cssom_changed = style_attribute && self.replace_inline_cssom_state(id, block);
        let NodeKind::Element { attributes, .. } = &mut self.node_mut_checked(id)?.kind else {
            return Err(Error::WrongKind);
        };
        let mut changed = true;
        let (index, mutation_name, old_value) = if let Some(index) = existing {
            let (old_name, old) = &mut attributes[index];
            let mutation_name = old_name.clone();
            if old == value && !cssom_changed && !policy_changed {
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

    /// Content language explicitly declared on an element, without inheritance.
    pub fn declared_content_language(&self, node: NodeId) -> Result<Option<&str>, Error> {
        language::declared(self, node)
    }

    /// Language inherited through DOM parent elements and direct shadow hosts.
    /// Explicit empty and unknown tags terminate inheritance without validation.
    pub fn content_language(&self, node: NodeId) -> Result<Option<&str>, Error> {
        language::determine(self, node, || true)
    }

    /// Install the higher-level response fallback before parser/script processing.
    pub fn set_content_language_headers(&mut self, headers: &[(String, String)]) -> Result<(), Error> {
        let candidate = language::protocol_candidate(headers);
        let previous = self.language_defaults.as_ref().and_then(|defaults| defaults.protocol.as_deref());
        if previous == candidate { return Ok(()); }
        let pragma_bytes = self.language_defaults.as_ref().and_then(|defaults| defaults.pragma.as_ref()).map_or(0, String::len);
        let value = Self::language_owned(candidate, pragma_bytes)?;
        self.language_defaults.get_or_insert_with(|| Box::new(language::DefaultLanguages::default())).protocol = value;
        self.mark_dirty(self.root(), Dirty::STYLE, MutationKind::Attribute(Name::new("lang")));
        Ok(())
    }

    fn language_owned(candidate: Option<&str>, other_bytes: usize) -> Result<Option<String>, Error> {
        let Some(candidate) = candidate else { return Ok(None); };
        lumen_common::limits::size::sum(other_bytes, candidate.len(), html::MAX_HTML_BYTES).map_err(|_| Error::LimitExceeded)?;
        let mut value = String::new();
        value.try_reserve_exact(candidate.len()).map_err(|_| Error::LimitExceeded)?;
        value.push_str(candidate);
        Ok(Some(value))
    }

    /// Meta processing is an insertion step, not a live attribute-derived default.
    fn process_inserted_language_pragmas(&mut self, root: NodeId) -> Result<(), Error> {
        if forms::html_element_local_name(self, root) != Some("meta") && self.first_child(root)?.is_none() { return Ok(()); }
        if !self.is_connected_element(root) || self.node_document(root)? != self.root() { return Ok(()); }
        let mut node = Some(root);
        while let Some(id) = node {
            if forms::html_element_local_name(self, id) == Some("meta")
                && self.get_attribute_ns_ref(id, None, "http-equiv")?.is_some_and(|value| value.eq_ignore_ascii_case("content-language"))
            {
                if let Some(candidate) = self.get_attribute_ns_ref(id, None, "content")?.and_then(language::pragma_candidate) {
                    let old = self.language_defaults.as_ref().and_then(|defaults| defaults.pragma.as_deref());
                    if old != Some(candidate) {
                        let protocol_bytes = self.language_defaults.as_ref().and_then(|defaults| defaults.protocol.as_ref()).map_or(0, String::len);
                        let value = Self::language_owned(Some(candidate), protocol_bytes)?;
                        self.language_defaults.get_or_insert_with(|| Box::new(language::DefaultLanguages::default())).pragma = value;
                        self.mark_dirty(self.root(), Dirty::STYLE, MutationKind::Attribute(Name::new("lang")));
                    }
                }
            }
            node = selector::next_descendant(self, root, id)?;
        }
        Ok(())
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
        let removed = self.character_data_length(id)?;
        self.replace_character_data(id, data, 0, removed, lumen_common::smuggle::utf16_unit_len(data))
    }

    fn replace_character_data(&mut self, id: NodeId, data: &str, offset: usize, removed: usize, inserted: usize) -> Result<(), Error> {
        let observing = self.observing_mutations();
        let kind = &mut self.node_mut_checked(id)?.kind;
        let text = match kind {
            NodeKind::Text(text) | NodeKind::CData(text) | NodeKind::Comment(text) => text,
            NodeKind::ProcessingInstruction { data, .. } => data,
            _ => return Err(Error::WrongKind),
        };
        let old_value = observing.then(|| text.clone());
        if text != data {
            text.clear();
            text.push_str(data);
            self.mark_dirty(id, Dirty::STYLE, MutationKind::CharacterData);
        }
        if let Some(old_value) = old_value {
            self.notify(observe::ObservedMutation {
                target: id,
                kind: observe::ObservedKind::CharacterData { old_value, offset, removed, inserted },
            });
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
        self.replace_character_data(id, &replacement, offset, end - offset, lumen_common::smuggle::utf16_unit_len(data))
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

    #[test]
    fn inline_cssom_read_cache_is_single_entry_and_reclaims_invalidated_sources() {
        let mut document = html::parse("<div id=a style='margin:1px'></div><div id=b style='color:red'></div>", 32).unwrap();
        let a = selector::query_selector(&document, document.root(), "#a").unwrap().unwrap();
        let b = selector::query_selector(&document, document.root(), "#b").unwrap().unwrap();
        let first = document.read_inline_cssom_style(a).unwrap().unwrap();
        let again = document.read_inline_cssom_style(a).unwrap().unwrap();
        assert!(Rc::ptr_eq(&first, &again));
        assert_eq!(document.get_attribute_ns_ref(a, None, "style").unwrap(), Some("margin:1px"));
        assert!(document.inline_cssom_styles.is_empty());
        document.read_inline_cssom_style(b).unwrap().unwrap();
        assert_eq!(document.inline_cssom_read_cache.as_ref().unwrap().0, b);
        document.set_attribute(b, "style", "color:blue").unwrap();
        assert!(document.inline_cssom_read_cache.is_none());
        let attr = document.attribute_node_by_ns(b, None, "style").unwrap().unwrap();
        document.read_inline_cssom_style(b).unwrap().unwrap();
        document.set_attribute_node_value(attr, "color:green").unwrap();
        assert!(document.inline_cssom_read_cache.is_none());
        assert_eq!(document.read_inline_cssom_style(b).unwrap().unwrap().value("color").unwrap().0, "green");
        document.remove(b).unwrap();
        document.destroy_subtree(b).unwrap();
        assert!(document.inline_cssom_read_cache.is_none());
        document.read_inline_cssom_style(a).unwrap().unwrap();
        let mut destination = Document::new(32);
        let (adopted, _) = destination.adopt_subtree_from(&mut document, a).unwrap();
        assert!(document.inline_cssom_read_cache.is_none());
        assert_eq!(destination.read_inline_cssom_style(adopted).unwrap().unwrap().value("margin").unwrap().0, "1px");
    }

    #[test]
    fn retained_inline_cssom_survives_adoption_and_attribute_writes_reparse() {
        let mut source = Document::new(16);
        let node = source.create(element("div")).unwrap();
        let mut block = css::DeclarationBlock::parse("margin: var(--edges) !important").unwrap();
        block.set("margin-left", "9px", false).unwrap();
        source.set_inline_cssom_style(node, block).unwrap();
        assert_eq!(source.inline_cssom_style(node).unwrap().value("margin-left"), Some((String::from("9px"), false)));
        assert_eq!(source.inline_cssom_style(node).unwrap().value("margin-top"), Some((String::new(), true)));
        let clone = source.clone_shallow(node).unwrap();
        assert!(source.inline_cssom_style(clone).is_none());
        let mut destination = Document::new(16);
        let (adopted, _) = destination.adopt_subtree_from(&mut source, node).unwrap();
        assert!(source.inline_cssom_style(node).is_none());
        let retained = destination.inline_cssom_style(adopted).unwrap().clone();
        assert_eq!(retained.value("margin-left"), Some((String::from("9px"), false)));
        destination.set_attribute_ns(adopted, Some("urn:custom"), "style", "color:red").unwrap();
        assert!(destination.inline_cssom_style(adopted).is_some());
        let text = destination.get_attribute_ns(adopted, None, "style").unwrap().unwrap();
        destination.set_attribute_ns(adopted, None, "style", &text).unwrap();
        assert!(destination.inline_cssom_style(adopted).is_none());
        destination.set_inline_cssom_style(adopted, retained.clone()).unwrap();
        let attribute = destination.attribute_node_by_ns(adopted, None, "style").unwrap().unwrap();
        destination.set_attribute_node_value(attribute, &text).unwrap();
        assert!(destination.inline_cssom_style(adopted).is_none());
        destination.set_inline_cssom_style(adopted, retained).unwrap();
        destination.destroy_subtree(adopted).unwrap();
        assert!(destination.inline_cssom_styles.is_empty());
        let replacement = destination.create(element("div")).unwrap();
        assert!(destination.inline_cssom_style(replacement).is_none());
    }
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
    fn state_preserving_move_is_atomic_for_observers_and_keeps_interaction_and_owner_identity() {
        let mut doc = Document::new(32);
        let html = doc.create(element("html")).unwrap();
        doc.append(doc.root(), html).unwrap();
        let old = doc.create(element("div")).unwrap();
        let destination = doc.create(element("div")).unwrap();
        doc.append(html, old).unwrap(); doc.append(html, destination).unwrap();
        let child = doc.create(element("input")).unwrap(); doc.append(old, child).unwrap();
        doc.set_interaction_state(interaction::InteractionState { focused: Some(child), ..Default::default() });
        let records = Rc::new(core::cell::RefCell::new(Vec::new()));
        let captured = records.clone();
        doc.set_mutation_sink(Some(Rc::new(move |document, mutation| {
            if let observe::ObservedKind::ChildList { added, removed, .. } = &mutation.kind {
                captured.borrow_mut().push((mutation.target, *added, *removed, document.parent(child).unwrap()));
            }
        })));
        let version = doc.version();
        assert_eq!(doc.move_before(child, html, None), Err(Error::Hierarchy));
        assert_eq!(doc.version(), version);
        assert!(records.borrow().is_empty());
        doc.move_before(destination, child, None).unwrap();
        assert_eq!(doc.interaction_state().focused, Some(child));
        assert_eq!(doc.node_document(child).unwrap(), doc.root());
        assert_eq!(&*records.borrow(), &[(old, None, Some(child), Some(destination)),
            (destination, Some(child), None, Some(destination))]);
        records.borrow_mut().clear();
        doc.move_before(destination, child, Some(child)).unwrap();
        assert_eq!(records.borrow().len(), 2, "moving before itself still follows the move algorithm");
        let detached = doc.create(element("div")).unwrap();
        assert_eq!(doc.move_before(detached, child, None), Err(Error::Hierarchy));
        assert_eq!(doc.parent(child).unwrap(), Some(destination));
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
    fn template_node_documents_are_shared_inert_and_survive_clone_detach_import() {
        let mut source = html::parse("<main><template><div><template><b>nested</b></template></div></template></main>", 64).unwrap();
        let host = selector::query_selector(&source, source.root(), "template").unwrap().unwrap();
        let content = source.template_content(host).unwrap().unwrap();
        let owner = source.node_document(content).unwrap();
        assert_ne!(owner, source.root());
        assert!(matches!(source.kind(owner), Ok(NodeKind::Document)));
        let child = source.first_child(content).unwrap().unwrap();
        let nested = source.first_child(child).unwrap().unwrap();
        let nested_content = source.template_content(nested).unwrap().unwrap();
        assert_eq!(source.node_document(child), Ok(owner));
        assert_eq!(source.node_document(nested_content), Ok(owner));
        let copy = source.clone_node(content, true).unwrap();
        assert_eq!(source.node_document(copy), Ok(owner));
        assert_eq!(source.node_document(source.first_child(copy).unwrap().unwrap()), Ok(owner));
        source.remove(child).unwrap();
        assert_eq!(source.node_document(child), Ok(owner));
        let main = selector::query_selector(&source, source.root(), "main").unwrap().unwrap();
        source.append(main, child).unwrap();
        assert_eq!(source.node_document(child), Ok(source.root()));
        assert_eq!(source.node_document(nested_content), Ok(owner));
        let mut target = Document::new(64);
        target.set_html_document(true);
        let imported = target.clone_subtree_from(&source, host, true).unwrap();
        let imported_content = target.template_content(imported).unwrap().unwrap();
        let imported_owner = target.node_document(imported_content).unwrap();
        assert_ne!(imported_owner, owner);
        assert!(target.is_template_owner_document(imported_owner));
        assert_eq!(target.node_document(imported), Ok(target.root()));
        source.destroy_subtree(copy).unwrap();
        assert!(source.inert_document_roots.iter().all(|id| source.kind(*id).is_ok()));
        let mut limited = Document::new(3);
        limited.set_html_document(true);
        assert_eq!(limited.create(element("template")), Err(Error::LimitExceeded));
        assert_eq!(limited.live_nodes, 1);
        assert!(limited.template_owner_document.is_none());
        assert_eq!(limited.adopt_subtree_from(&mut source, host), Err(Error::LimitExceeded));
        assert_eq!(limited.node_count(), 1);
        assert!(limited.template_owner_document.is_none());
        let mut names = Document::new(32);
        names.set_html_document(true);
        let literal = names.create_unprefixed_element(Namespace::Html, Name::from("x:template"), Vec::new()).unwrap();
        assert!(!names.is_html_template(literal).unwrap());
        assert_eq!(names.template_content(literal), Ok(None));
        assert!(names.template_owner_document.is_none());
        let literal_copy = names.clone_node(literal, true).unwrap();
        assert_eq!(names.template_content(literal_copy), Ok(None));
        let qualified = names.create(NodeKind::Element { namespace: Namespace::Html, name: Name::from("x:template"), attributes: Vec::new() }).unwrap();
        assert!(names.is_html_template(qualified).unwrap());
        assert!(names.template_content(qualified).unwrap().is_some());
        let mut xml = Document::new(8);
        let xml_template = xml.create(element("template")).unwrap();
        assert_eq!(xml.node_document(xml.template_content(xml_template).unwrap().unwrap()), Ok(xml.root()));
        assert!(xml.template_owner_document.is_none());
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
    fn specification_template_direct_content_adoption_preserves_association() {
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
        assert_eq!(source.template_content(template).unwrap(), Some(adopted));
        assert_eq!(destination.template_host(adopted).unwrap(), Some(template));
        assert_eq!(destination.parent(adopted).unwrap(), None);
        assert!(source.kind(content).is_err());
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
                    keep_custom_element_registry_null: false,
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
    #[test]
    fn specification_nonce_slots_preserve_internal_value_and_bound_reclamation() {
        let mut source=Document::new(32);
        let element=source.create_unprefixed_element(Namespace::Html,Name::new("div"),alloc::vec![(Name::new("nonce"),String::from("content"))]).unwrap();
        source.set_cryptographic_nonce(element,"internal").unwrap();
        let copy=source.clone_shallow(element).unwrap();
        assert_eq!(source.cryptographic_nonce(copy).unwrap(),"internal");
        assert_eq!(source.get_attribute_ns_ref(copy,None,"nonce").unwrap(),Some("content"));
        let oversized="x".repeat(html::MAX_HTML_BYTES);
        assert_eq!(source.set_cryptographic_nonce(copy,&oversized),Err(Error::LimitExceeded));
        assert_eq!(source.cryptographic_nonce(copy).unwrap(),"internal");
        let mut target=Document::new(32);
        let (adopted,_)=target.adopt_subtree_from(&mut source,copy).unwrap();
        assert_eq!(target.cryptographic_nonce(adopted).unwrap(),"internal");
        target.destroy_subtree(adopted).unwrap();
        assert!(target.cryptographic_nonces.is_empty());assert_eq!(target.cryptographic_nonce_bytes,0);
        source.destroy_subtree(element).unwrap();
        assert!(source.cryptographic_nonces.is_empty());assert_eq!(source.cryptographic_nonce_bytes,0);
    }

}
