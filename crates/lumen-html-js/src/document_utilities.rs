//! DOM parsing and node traversal utilities backed by Lumen's shared document arena.
use super::*;
use lumen::embed::JsFunction;

const SHOW_ALL: u32 = 0xFFFF_FFFF;
const FILTER_ACCEPT: u8 = 1;
const FILTER_REJECT: u8 = 2;
const FILTER_SKIP: u8 = 3;
const MAX_PARSED_NODES: usize = 100_000;

/// Capture the relevant Document's live identity without retaining its arena
/// or Window. Borrowing another realm's method must not change this identity.
#[lumen_bind::class(name = "DOMParser", hint(js(webidl)))]
pub struct DomParser {
    document_identity: Rc<DocumentIdentity>,
}

#[lumen_bind::methods]
impl DomParser {
    #[constructor]
    fn new(ctx: &mut Ctx) -> Self {
        let document_identity = window_globals::current_dom_realm(ctx)
            .map(|realm| realm.document_identity.clone())
            .unwrap_or_else(|| {
                Rc::new(DocumentIdentity {
                    origin: RefCell::new(browsing_context::invocation_origin(ctx)),
                    url: RefCell::new(None),
                })
            });
        Self { document_identity }
    }

    #[method(coerce)]
    fn parse_from_string(&self, ctx: &mut Ctx, input: &str, mime_type: &str) -> OpResult<Value> {
        if mime_type == "text/html" {
            let controller = super::dialog_popover::DetailsController::prepare(ctx)?;
            let document = html::parse_with_options_initialized(
                input, MAX_PARSED_NODES, html::ParseOptions::default(),
                |document| controller.attach(document),
            ).map_err(|error| {
                OpError::new(
                    "SyntaxError",
                    format!("HTML parse error at {}: {}", error.offset, error.message),
                )
            })?;
            let realm = DomRealm::realm_from_document(document);
            self.set_parsed_document_identity(&realm);
            super::observers::attach_parsed_realm(ctx, &realm)?;
            controller.bind(ctx, &realm);
            return Ok(realm.document_value(ctx));
        }
        let xml_type = [
            "text/xml",
            "application/xml",
            "application/xhtml+xml",
            "image/svg+xml",
        ]
        .iter()
        .any(|supported| mime_type == *supported);
        if !xml_type {
            return Err(OpError::new("TypeError", "unsupported DOMParser MIME type"));
        }
        let prepared = super::dialog_popover::DetailsController::prepare(ctx)?;
        let (document, controller) = match lumen_html::xml::parse_initialized(
            input, MAX_PARSED_NODES, |document| prepared.attach(document),
        ) {
            Ok(document) => (document, prepared),
            Err(error) => {
                // The failed arena's NodeIds must never be rebound to the
                // parsererror replacement. Its weak queued work becomes inert.
                drop(prepared);
                let controller = super::dialog_popover::DetailsController::prepare(ctx)?;
                let mut document = parser_error_document(error.offset, error.message);
                controller.attach(&mut document);
                (document, controller)
            }
        };
        let realm = DomRealm::realm_from_document_with_metadata(document, mime_type, false, false);
        self.set_parsed_document_identity(&realm);
        super::observers::attach_parsed_realm(ctx, &realm)?;
        controller.bind(ctx, &realm);
        // The realm helper constructs the document wrapper and retains it in the
        // realm, so the returned value keeps its arena alive independently.
        // A document parsed this way never aliases the active page's NodeIds.
        Ok(realm.document_value(ctx))
    }
}

impl DomParser {
    fn set_parsed_document_identity(&self, realm: &DomRealm) {
        realm.set_document_url(
            self.document_identity
                .url
                .borrow()
                .as_deref()
                .unwrap_or("about:blank"),
        );
        realm.set_document_origin(
            self.document_identity
                .origin
                .borrow()
                .clone()
                .unwrap_or_else(browsing_context::Origin::opaque),
        );
    }
}

/// XMLSerializer has no per-instance state; serialization uses the owner
/// document captured by the passed Node wrapper.
#[lumen_bind::class(name = "XMLSerializer", hint(js(webidl)))]
pub struct XmlSerializer;

#[lumen_bind::methods]
impl XmlSerializer {
    #[constructor]
    fn new() -> Self {
        Self
    }

    fn serialize_to_string(&self, ctx: &mut Ctx, root: &DomNode) -> OpResult<String> {
        let result = {
            let session = root.realm.session.borrow();
            lumen_html::xml::serialize_xml(session.document(), root.id, false)
        };
        result.map_err(|error| {
            if error == lumen_html::Error::WrongKind {
                super::error_reporting::dom_exception(
                    ctx,
                    "InvalidStateError",
                    "XML serialization could not represent the supplied node",
                )
            } else {
                dom_error(error)
            }
        })
    }
}

fn parser_error_document(offset: usize, message: &str) -> lumen_html::Document {
    let mut document = lumen_html::Document::new(MAX_PARSED_NODES);
    let root = document.root();
    let error = document
        .create(NodeKind::Element {
            namespace: Namespace::Other(std::rc::Rc::from(
                "http://www.mozilla.org/newlayout/xml/parsererror.xml",
            )),
            name: lumen_html::Name::new("parsererror"),
            attributes: Vec::new(),
        })
        .expect("parsererror document has available capacity");
    document
        .append(root, error)
        .expect("parsererror is a document child");
    let details = document
        .create(NodeKind::Text(format!(
            "XML parse error at {offset}: {message}"
        )))
        .expect("parsererror details have available capacity");
    document
        .append(error, details)
        .expect("parsererror details are text content");
    document
}

/// A filter is kept as a JS value so either a callback or an acceptNode object
/// follows the standard NodeFilter shape.
#[derive(Clone)]
struct Filter {
    what_to_show: u32,
    callback: Option<Value>,
}

impl Filter {
    fn new(what_to_show: Option<u32>, callback: Option<Value>) -> Self {
        Self {
            what_to_show: what_to_show.unwrap_or(SHOW_ALL),
            callback: callback.filter(|value| !matches!(value, Value::Null | Value::Undefined)),
        }
    }

    fn accept(&self, ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId) -> OpResult<u8> {
        let node_type = {
            let session = realm.session.borrow();
            node_type(session.document().kind(node).map_err(dom_error)?)
        };
        if node_type == 0 || (self.what_to_show & (1_u32 << (node_type - 1))) == 0 {
            return Ok(FILTER_SKIP);
        }
        let Some(callback) = self.callback.as_ref() else {
            return Ok(FILTER_ACCEPT);
        };
        let value = realm.wrap(ctx, node);
        let result = if let Some(function) = JsFunction::from_value(callback.clone()) {
            function.call(ctx, Value::Undefined, &[value])?
        } else {
            let method = ctx
                .get_member(callback, "acceptNode")
                .map_err(|_| OpError::new("TypeError", "NodeFilter.acceptNode getter failed"))?;
            let function = JsFunction::from_value(method).ok_or_else(|| {
                OpError::new("TypeError", "NodeFilter.acceptNode is not callable")
            })?;
            function.call(ctx, callback.clone(), &[value])?
        };
        let result = match result {
            Value::Num(value) if value.is_finite() && value.fract() == 0.0 => value as i64,
            _ => {
                return Err(OpError::new(
                    "TypeError",
                    "NodeFilter must return a filter result",
                ));
            }
        };
        match result {
            1 => Ok(FILTER_ACCEPT),
            2 => Ok(FILTER_REJECT),
            3 => Ok(FILTER_SKIP),
            _ => Err(OpError::new(
                "TypeError",
                "NodeFilter returned an invalid result",
            )),
        }
    }
}

fn node_type(kind: &NodeKind) -> u32 {
    match kind {
        NodeKind::Element { .. } => 1,
        NodeKind::Attribute { .. } => 2,
        NodeKind::Text(_) => 3,
        NodeKind::CData(_) => 4,
        NodeKind::ProcessingInstruction { .. } => 7,
        NodeKind::Comment(_) => 8,
        NodeKind::Document => 9,
        NodeKind::DocumentType(_) => 10,
        NodeKind::DocumentFragment => 11,
    }
}

fn descend_last(document: &lumen_html::Document, mut node: NodeId) -> OpResult<NodeId> {
    while let Some(child) = document.last_child(node).map_err(dom_error)? {
        node = child;
    }
    Ok(node)
}

fn next_in_subtree(
    document: &lumen_html::Document,
    root: NodeId,
    node: NodeId,
) -> OpResult<Option<NodeId>> {
    if let Some(child) = document.first_child(node).map_err(dom_error)? {
        return Ok(Some(child));
    }
    let mut current = node;
    loop {
        if current == root {
            return Ok(None);
        }
        if let Some(sibling) = document.next_sibling(current).map_err(dom_error)? {
            return Ok(Some(sibling));
        }
        let Some(parent) = document.parent(current).map_err(dom_error)? else {
            return Ok(None);
        };
        current = parent;
    }
}

fn previous_in_subtree(
    document: &lumen_html::Document,
    root: NodeId,
    node: NodeId,
) -> OpResult<Option<NodeId>> {
    if node == root {
        return Ok(None);
    }
    if let Some(sibling) = document.previous_sibling(node).map_err(dom_error)? {
        return descend_last(document, sibling).map(Some);
    }
    Ok(document.parent(node).map_err(dom_error)?)
}

struct TraversalGuard<'a>(&'a Cell<bool>);

impl<'a> TraversalGuard<'a> {
    fn enter(busy: &'a Cell<bool>) -> OpResult<Self> {
        if busy.replace(true) {
            return Err(OpError::new(
                "InvalidStateError",
                "reentrant node traversal",
            ));
        }
        Ok(Self(busy))
    }
}

impl Drop for TraversalGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

fn next_after_subtree(
    realm: &Rc<DomRealm>,
    root: NodeId,
    node: NodeId,
) -> OpResult<Option<NodeId>> {
    let mut current = node;
    loop {
        if current == root {
            return Ok(None);
        }
        let (sibling, parent) = {
            let session = realm.session.borrow();
            let document = session.document();
            (
                document.next_sibling(current).map_err(dom_error)?,
                document.parent(current).map_err(dom_error)?,
            )
        };
        if sibling.is_some() {
            return Ok(sibling);
        }
        let Some(parent) = parent else {
            return Ok(None);
        };
        current = parent;
    }
}

fn first_accepted_child(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    filter: &Filter,
    parent: NodeId,
    reverse: bool,
) -> OpResult<Option<NodeId>> {
    let mut candidate = {
        let session = realm.session.borrow();
        if reverse {
            session.document().last_child(parent)
        } else {
            session.document().first_child(parent)
        }
        .map_err(dom_error)?
    };
    while let Some(node) = candidate {
        let sibling = {
            let session = realm.session.borrow();
            if reverse {
                session.document().previous_sibling(node)
            } else {
                session.document().next_sibling(node)
            }
            .map_err(dom_error)?
        };
        match filter.accept(ctx, realm, node)? {
            FILTER_ACCEPT => return Ok(Some(node)),
            FILTER_REJECT => (),
            FILTER_SKIP => {
                if let Some(descendant) = first_accepted_child(ctx, realm, filter, node, reverse)? {
                    return Ok(Some(descendant));
                }
            }
            _ => unreachable!(),
        }
        candidate = sibling;
    }
    Ok(None)
}

#[lumen_bind::class(name = "TreeWalker", hint(js(webidl)))]
pub struct DomTreeWalker {
    state: Rc<TreeWalkerState>,
    filter: Filter,
    busy: Cell<bool>,
    _retained_root: DomNodeList,
    retained_current: RefCell<DomNodeList>,
}

pub(crate) struct TreeWalkerState {
    realm: RefCell<Rc<DomRealm>>,
    root: Cell<NodeId>,
    current: Cell<NodeId>,
}

impl TreeWalkerState {
    fn realm(&self) -> OpResult<Rc<DomRealm>> {
        Ok(self.realm.borrow().clone())
    }
}

pub(crate) struct TreeWalkerRegistry {
    walkers: RefCell<Vec<std::rc::Weak<TreeWalkerState>>>,
}

impl TreeWalkerRegistry {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self {
            walkers: RefCell::new(Vec::new()),
        })
    }

    fn register(&self, state: &Rc<TreeWalkerState>) {
        let mut walkers = self.walkers.borrow_mut();
        walkers.retain(|entry| entry.strong_count() > 0);
        walkers.push(Rc::downgrade(state));
    }

    pub(crate) fn adopt_nodes(
        &self,
        source: &Rc<DomRealm>,
        target: &Rc<DomRealm>,
        target_registry: &TreeWalkerRegistry,
        mapping: &[(NodeId, NodeId)],
    ) {
        let mapped = |node: NodeId| {
            mapping
                .iter()
                .find_map(|(old, new)| (*old == node).then_some(*new))
        };
        let mut walkers = self.walkers.borrow_mut();
        walkers.retain(|entry| entry.strong_count() > 0);
        let mut migrated = Vec::new();
        for state in walkers.iter().filter_map(std::rc::Weak::upgrade) {
            if !Rc::ptr_eq(&state.realm.borrow(), source) {
                continue;
            }
            let Some(root) = mapped(state.root.get()) else {
                continue;
            };
            state.root.set(root);
            if let Some(current) = mapped(state.current.get()) {
                state.current.set(current);
            }
            *state.realm.borrow_mut() = target.clone();
            migrated.push(Rc::downgrade(&state));
        }
        walkers.retain(|entry| {
            entry
                .upgrade()
                .is_some_and(|state| !migrated.iter().any(|moved| moved.ptr_eq(entry)))
        });
        target_registry.walkers.borrow_mut().extend(migrated);
    }
}

impl DomTreeWalker {
    pub(crate) fn new(
        realm: Rc<DomRealm>,
        root: NodeId,
        what_to_show: Option<u32>,
        filter: Option<Value>,
        registry: Rc<TreeWalkerRegistry>,
    ) -> Self {
        let state = Rc::new(TreeWalkerState {
            realm: RefCell::new(realm.clone()),
            root: Cell::new(root),
            current: Cell::new(root),
        });
        registry.register(&state);
        let retained_root = DomNodeList::snapshot(realm.clone(), vec![root], Value::Undefined);
        let retained_current = DomNodeList::snapshot(realm.clone(), vec![root], Value::Undefined);
        Self {
            state,
            filter: Filter::new(what_to_show, filter),
            busy: Cell::new(false),
            _retained_root: retained_root,
            retained_current: RefCell::new(retained_current),
        }
    }
}

#[lumen_bind::methods]
impl DomTreeWalker {
    #[getter]
    fn root(&self, ctx: &mut Ctx) -> Value {
        self.state
            .realm()
            .map_or(Value::Null, |realm| realm.wrap(ctx, self.state.root.get()))
    }

    #[getter]
    fn current_node(&self, ctx: &mut Ctx) -> Value {
        self.state.realm().map_or(Value::Null, |realm| {
            realm.wrap(ctx, self.state.current.get())
        })
    }

    #[setter]
    fn set_current_node(&self, node: &DomNode) -> OpResult<()> {
        let realm = self.state.realm()?;
        if !Rc::ptr_eq(&realm, &node.realm) {
            return Err(OpError::new(
                "WrongDocumentError",
                "currentNode belongs to another document",
            ));
        }
        realm
            .session
            .borrow()
            .document()
            .kind(node.id)
            .map_err(dom_error)?;
        self.state.current.set(node.id);
        *self.retained_current.borrow_mut() =
            DomNodeList::snapshot(realm, vec![node.id], Value::Undefined);
        Ok(())
    }

    #[getter]
    fn what_to_show(&self) -> u32 {
        self.filter.what_to_show
    }

    #[getter]
    fn filter(&self) -> Option<Value> {
        self.filter.callback.clone()
    }

    fn parent_node(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let _guard = TraversalGuard::enter(&self.busy)?;
        let realm = self.state.realm()?;
        let root = self.state.root.get();
        let mut current = self.state.current.get();
        while current != root {
            let parent = realm
                .session
                .borrow()
                .document()
                .parent(current)
                .map_err(dom_error)?;
            let Some(parent) = parent else { break };
            if self.filter.accept(ctx, &realm, parent)? == FILTER_ACCEPT {
                self.state.current.set(parent);
                return Ok(realm.wrap(ctx, parent));
            }
            current = parent;
        }
        Ok(Value::Null)
    }

    fn first_child(&self, ctx: &mut Ctx) -> OpResult<Value> {
        self.child(ctx, false)
    }
    fn last_child(&self, ctx: &mut Ctx) -> OpResult<Value> {
        self.child(ctx, true)
    }
    fn previous_sibling(&self, ctx: &mut Ctx) -> OpResult<Value> {
        self.sibling(ctx, true)
    }
    fn next_sibling(&self, ctx: &mut Ctx) -> OpResult<Value> {
        self.sibling(ctx, false)
    }

    fn next_node(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let _guard = TraversalGuard::enter(&self.busy)?;
        let realm = self.state.realm()?;
        let root = self.state.root.get();
        let mut cursor = self.state.current.get();
        let mut skip_subtree = false;
        loop {
            let next = if skip_subtree {
                skip_subtree = false;
                next_after_subtree(&realm, root, cursor)?
            } else {
                let session = realm.session.borrow();
                next_in_subtree(session.document(), root, cursor)?
            };
            let Some(next) = next else {
                return Ok(Value::Null);
            };
            cursor = next;
            match self.filter.accept(ctx, &realm, cursor)? {
                FILTER_ACCEPT => {
                    self.state.current.set(cursor);
                    return Ok(realm.wrap(ctx, cursor));
                }
                FILTER_REJECT => skip_subtree = true,
                FILTER_SKIP => (),
                _ => unreachable!(),
            }
        }
    }

    fn previous_node(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let _guard = TraversalGuard::enter(&self.busy)?;
        let realm = self.state.realm()?;
        let root = self.state.root.get();
        let mut candidate = self.state.current.get();
        while let Some(previous) = {
            let session = realm.session.borrow();
            previous_in_subtree(session.document(), root, candidate)?
        } {
            candidate = previous;
            if self.filter.accept(ctx, &realm, candidate)? == FILTER_ACCEPT {
                self.state.current.set(candidate);
                return Ok(realm.wrap(ctx, candidate));
            }
        }
        Ok(Value::Null)
    }

    fn child(&self, ctx: &mut Ctx, reverse: bool) -> OpResult<Value> {
        let _guard = TraversalGuard::enter(&self.busy)?;
        let realm = self.state.realm()?;
        let root = self.state.root.get();
        let mut current = self.state.current.get();
        loop {
            let candidate = first_accepted_child(ctx, &realm, &self.filter, current, reverse)?;
            if let Some(candidate) = candidate {
                self.state.current.set(candidate);
                return Ok(realm.wrap(ctx, candidate));
            }
            let result = self.filter.accept(ctx, &realm, current)?;
            if current == root || result == FILTER_REJECT {
                return Ok(Value::Null);
            }
            let Some(parent) = realm
                .session
                .borrow()
                .document()
                .parent(current)
                .map_err(dom_error)?
            else {
                return Ok(Value::Null);
            };
            current = parent;
        }
    }

    fn sibling(&self, ctx: &mut Ctx, reverse: bool) -> OpResult<Value> {
        let _guard = TraversalGuard::enter(&self.busy)?;
        let realm = self.state.realm()?;
        let root = self.state.root.get();
        let mut current = self.state.current.get();
        loop {
            if current == root {
                return Ok(Value::Null);
            }
            let parent = realm
                .session
                .borrow()
                .document()
                .parent(current)
                .map_err(dom_error)?;
            let Some(parent) = parent else {
                return Ok(Value::Null);
            };
            let mut candidate = {
                let session = realm.session.borrow();
                if reverse {
                    session.document().previous_sibling(current)
                } else {
                    session.document().next_sibling(current)
                }
                .map_err(dom_error)?
            };
            while let Some(node) = candidate {
                let sibling = {
                    let session = realm.session.borrow();
                    if reverse {
                        session.document().previous_sibling(node)
                    } else {
                        session.document().next_sibling(node)
                    }
                    .map_err(dom_error)?
                };
                match self.filter.accept(ctx, &realm, node)? {
                    FILTER_ACCEPT => {
                        self.state.current.set(node);
                        return Ok(realm.wrap(ctx, node));
                    }
                    FILTER_REJECT => (),
                    FILTER_SKIP => {
                        let nested =
                            first_accepted_child(ctx, &realm, &self.filter, node, reverse)?;
                        if let Some(node) = nested {
                            self.state.current.set(node);
                            return Ok(realm.wrap(ctx, node));
                        }
                    }
                    _ => unreachable!(),
                }
                candidate = sibling;
            }
            if self.filter.accept(ctx, &realm, parent)? != FILTER_SKIP {
                return Ok(Value::Null);
            }
            current = parent;
        }
    }
}

pub(crate) struct IteratorState {
    realm: RefCell<Rc<DomRealm>>,
    pub(crate) root: Cell<NodeId>,
    pub(crate) reference: Cell<NodeId>,
    pub(crate) before_reference: Cell<bool>,
    pub(crate) detached: Cell<bool>,
}

impl IteratorState {
    fn realm(&self) -> OpResult<Rc<DomRealm>> {
        Ok(self.realm.borrow().clone())
    }
}

pub(crate) struct IteratorRegistry {
    iterators: RefCell<Vec<std::rc::Weak<IteratorState>>>,
}

impl IteratorRegistry {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self {
            iterators: RefCell::new(Vec::new()),
        })
    }
    fn register(&self, state: &Rc<IteratorState>) {
        let mut iterators = self.iterators.borrow_mut();
        iterators.retain(|entry| entry.strong_count() > 0);
        iterators.push(Rc::downgrade(state));
    }

    pub(crate) fn adopt_nodes(
        &self,
        source: &Rc<DomRealm>,
        target: &Rc<DomRealm>,
        target_registry: &IteratorRegistry,
        mapping: &[(NodeId, NodeId)],
    ) {
        let mapped = |node: NodeId| {
            mapping
                .iter()
                .find_map(|(old, new)| (*old == node).then_some(*new))
        };
        let mut iterators = self.iterators.borrow_mut();
        iterators.retain(|entry| entry.strong_count() > 0);
        let mut migrated = Vec::new();
        for state in iterators.iter().filter_map(std::rc::Weak::upgrade) {
            if !Rc::ptr_eq(&state.realm.borrow(), source) {
                continue;
            }
            let Some(root) = mapped(state.root.get()) else {
                continue;
            };
            state.root.set(root);
            if let Some(reference) = mapped(state.reference.get()) {
                state.reference.set(reference);
            }
            *state.realm.borrow_mut() = target.clone();
            migrated.push(Rc::downgrade(&state));
        }
        iterators.retain(|entry| {
            entry
                .upgrade()
                .is_some_and(|state| !migrated.iter().any(|moved| moved.ptr_eq(entry)))
        });
        target_registry.iterators.borrow_mut().extend(migrated);
    }
}

#[lumen_bind::class(name = "NodeIterator", hint(js(webidl)))]
pub struct DomNodeIterator {
    state: Rc<IteratorState>,
    filter: Filter,
    busy: Cell<bool>,
    _retained_root: DomNodeList,
}

impl DomNodeIterator {
    pub(crate) fn new(
        realm: Rc<DomRealm>,
        root: NodeId,
        what_to_show: Option<u32>,
        filter: Option<Value>,
        registry: Rc<IteratorRegistry>,
    ) -> Self {
        let retained_root = DomNodeList::snapshot(realm.clone(), vec![root], Value::Undefined);
        let state = Rc::new(IteratorState {
            realm: RefCell::new(realm.clone()),
            root: Cell::new(root),
            reference: Cell::new(root),
            before_reference: Cell::new(true),
            detached: Cell::new(false),
        });
        registry.register(&state);
        Self {
            state,
            filter: Filter::new(what_to_show, filter),
            busy: Cell::new(false),
            _retained_root: retained_root,
        }
    }

    fn traverse(&self, ctx: &mut Ctx, forward: bool) -> OpResult<Value> {
        let _guard = TraversalGuard::enter(&self.busy)?;
        if self.state.detached.get() {
            return Err(OpError::new(
                "InvalidStateError",
                "NodeIterator is detached",
            ));
        }
        let mut reference = self.state.reference.get();
        let mut before = self.state.before_reference.get();
        loop {
            let realm = self.state.realm()?;
            let candidate = if forward {
                if before {
                    before = false;
                    Some(reference)
                } else {
                    let session = realm.session.borrow();
                    next_in_subtree(session.document(), self.state.root.get(), reference)?
                }
            } else if before {
                let session = realm.session.borrow();
                previous_in_subtree(session.document(), self.state.root.get(), reference)?
            } else {
                before = true;
                Some(reference)
            };
            let Some(candidate) = candidate else {
                return Ok(Value::Null);
            };
            reference = candidate;
            if self.filter.accept(ctx, &realm, candidate)? != FILTER_REJECT {
                self.state.reference.set(reference);
                self.state.before_reference.set(before);
                return Ok(realm.wrap(ctx, candidate));
            }
            // NodeIterator treats REJECT as SKIP and continues in tree order.
            if !forward {
                before = true;
            }
        }
    }
}

#[lumen_bind::methods]
impl DomNodeIterator {
    #[getter]
    fn root(&self, ctx: &mut Ctx) -> Value {
        self.state
            .realm()
            .map_or(Value::Null, |realm| realm.wrap(ctx, self.state.root.get()))
    }
    #[getter]
    fn reference_node(&self, ctx: &mut Ctx) -> Value {
        self.state.realm().map_or(Value::Null, |realm| {
            realm.wrap(ctx, self.state.reference.get())
        })
    }
    #[getter]
    fn pointer_before_reference_node(&self) -> bool {
        self.state.before_reference.get()
    }
    #[getter]
    fn what_to_show(&self) -> u32 {
        self.filter.what_to_show
    }
    #[getter]
    fn filter(&self) -> Option<Value> {
        self.filter.callback.clone()
    }
    fn next_node(&self, ctx: &mut Ctx) -> OpResult<Value> {
        self.traverse(ctx, true)
    }
    fn previous_node(&self, ctx: &mut Ctx) -> OpResult<Value> {
        self.traverse(ctx, false)
    }
    fn detach(&self) {
        self.state.detached.set(true);
    }
}

// Keep the binding declarations rooted here; lib.rs installs these constructors
// after constructing the page's DOM realm.
pub(crate) fn constructors(ctx: &mut Ctx) -> [(&'static str, Value); 4] {
    [
        ("DOMParser", ctx.class_constructor::<DomParser>()),
        ("XMLSerializer", ctx.class_constructor::<XmlSerializer>()),
        ("TreeWalker", ctx.class_constructor::<DomTreeWalker>()),
        ("NodeIterator", ctx.class_constructor::<DomNodeIterator>()),
    ]
}

pub(crate) fn adjust_iterators(
    registry: &IteratorRegistry,
    document: &lumen_html::Document,
    mutation: &lumen_html::observe::ObservedMutation,
) {
    use lumen_html::observe::ObservedKind;
    let mut iterators = registry.iterators.borrow_mut();
    iterators.retain(|entry| entry.strong_count() > 0);
    for state in iterators.iter().filter_map(std::rc::Weak::upgrade) {
        if state.detached.get() {
            continue;
        }
        let removed = match &mutation.kind {
            ObservedKind::ChildList {
                removed,
                next_sibling,
                ..
            } => {
                let Some(removed) = removed else { continue };
                Some((*removed, *next_sibling))
            }
            ObservedKind::ChildListMany { removed, added } => {
                if removed.is_empty() {
                    continue;
                }
                let first_added = added.first().copied();
                if removed
                    .iter()
                    .any(|root| is_descendant(document, state.root.get(), *root))
                    || !removed
                        .iter()
                        .any(|root| is_descendant(document, state.reference.get(), *root))
                {
                    continue;
                }
                if let Some(next) = first_added {
                    state.reference.set(next);
                    state.before_reference.set(true);
                } else {
                    state.reference.set(mutation.target);
                    state.before_reference.set(false);
                }
                continue;
            }
            _ => continue,
        };
        let Some((removed, next)) = removed else {
            continue;
        };
        if is_descendant(document, state.root.get(), removed)
            || !is_descendant(document, state.reference.get(), removed)
        {
            continue;
        }
        if let Some(next) = next {
            state.reference.set(next);
            state.before_reference.set(true);
            continue;
        }
        state.reference.set(mutation.target);
        state.before_reference.set(false);
    }
}

fn is_descendant(document: &lumen_html::Document, node: NodeId, root: NodeId) -> bool {
    let mut current = Some(node);
    while let Some(id) = current {
        if id == root {
            return true;
        }
        current = document.parent(id).ok().flatten();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    fn eval_bool(engine: &mut Engine, source: &str) -> bool {
        let source = format!(
            "try {{ {source} }} catch (e) {{ '__JS_ERROR__' + String(e && (e.stack || e.message || e)) }}"
        );
        match engine.eval_value(&source) {
            Ok(Ok(Value::Bool(value))) => value,
            Ok(Ok(Value::Str(message))) => panic!("JavaScript exception: {}", message.as_str()),
            Ok(Err(_)) => panic!("JavaScript evaluation ended abruptly"),
            Err(_) => panic!("JavaScript evaluation could not start"),
            Ok(Ok(_)) => panic!("JavaScript assertion did not return a boolean"),
        }
    }

    #[test]
    fn detached_details_parser_initial_transitions_are_native_and_retained() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(&mut engine, r#"
            globalThis.parsedDetailsEvents = [];
            for (const [mime, source] of [
                ['text/html', '<details open></details>'],
                ['application/xhtml+xml', '<details xmlns="http://www.w3.org/1999/xhtml" open=""/>']
            ]) {
                const parsed = new DOMParser().parseFromString(source, mime);
                const details = parsed.querySelector('details');
                details.addEventListener('toggle', event => parsedDetailsEvents.push(
                    event instanceof ToggleEvent && event.isTrusted && !event.bubbles &&
                    !event.cancelable && event.oldState === 'closed' && event.newState === 'open' &&
                    event.target.ownerDocument.defaultView === null));
            }
            parsedDetailsEvents.length === 0
        "#));
        engine.collect_garbage();
        assert!(crate::scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(eval_bool(&mut engine, "parsedDetailsEvents.length === 2 && parsedDetailsEvents.every(Boolean)"));
    }

    #[test]
    fn detached_details_implementation_documents_use_live_transition_sink() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(&mut engine, r#"
            globalThis.implementationDetailsEvents = [];
            for (const parsed of [document.implementation.createHTMLDocument('details'),
                document.implementation.createDocument('http://www.w3.org/1999/xhtml', 'html', null)]) {
                const details = parsed.createElementNS('http://www.w3.org/1999/xhtml', 'details');
                parsed.documentElement.appendChild(details);
                details.addEventListener('toggle', event => implementationDetailsEvents.push(
                    event instanceof ToggleEvent && event.isTrusted && event.oldState === 'closed' &&
                    event.newState === 'open' && event.target.ownerDocument.defaultView === null));
                details.setAttribute('open', '');
                details.removeAttribute('open');
                details.setAttribute('open', '');
            }
            implementationDetailsEvents.length === 0
        "#));
        engine.collect_garbage();
        assert!(crate::scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(eval_bool(&mut engine, "implementationDetailsEvents.length === 2 && implementationDetailsEvents.every(Boolean)"));
    }

    #[test]
    fn malformed_xml_details_work_is_abandoned_without_rebinding_stale_nodes() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(&mut engine, r#"
            globalThis.parserErrorToggles = 0;
            const failed = new DOMParser().parseFromString(
                '<details xmlns="http://www.w3.org/1999/xhtml" open=""><broken></details>',
                'application/xhtml+xml');
            failed.documentElement.addEventListener('toggle', () => parserErrorToggles++);
            failed.documentElement.localName === 'parsererror'
        "#));
        engine.collect_garbage();
        assert!(crate::scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(eval_bool(&mut engine, "parserErrorToggles === 0"));
    }

    #[test]
    fn dom_parser_creates_an_independent_document() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main>live</main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "const ownSelection=document.getSelection(), parsed = new DOMParser().parseFromString('<section><b>parsed</b></section>', 'text/html'); parsed !== document && parsed.querySelector('b').textContent === 'parsed' && parsed.body.innerHTML === '<section><b>parsed</b></section>' && (parsed.querySelector('b') instanceof Node) && document.querySelector('main').textContent === 'live' && ownSelection===document.getSelection() && ownSelection===window.getSelection() && parsed.getSelection()!==ownSelection"
        ));
    }

    #[test]
    fn dom_parser_identity_tracks_its_creation_document_across_methods_and_navigation() {
        let mut runtime = lumen_runtime::Runtime::new();
        let engine = runtime.engine();
        let parent = crate::install(
            engine.ctx(),
            "<base href='https://base.test/ignored/'><iframe id=child></iframe>",
            128,
        )
        .unwrap();
        parent.set_document_url("https://parent.test/dir/page.html");
        let frame = parent
            .frame_contexts(engine.ctx())
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert!(eval_bool(
            engine,
            r#"(() => {
            globalThis.parentParser = new DOMParser();
            return parentParser.parseFromString('<p>x</p>', 'text/html').URL === document.URL;
        })()"#
        ));
        parent.set_document_url("https://parent.test/dir/updated.html?live=1");
        assert!(eval_bool(
            engine,
            "document.querySelector('iframe').src='https://parent.test/child/one.html'; true"
        ));
        let request = frame.navigation_request();
        let child = frame
            .install_response_for_request(
                engine.ctx(),
                &request,
                "https://parent.test/child/one.html",
                "text/html",
                "<body>one</body>",
                64,
            )
            .unwrap();
        assert!(eval_bool(
            engine,
            r#"(() => {
            const childWindow = document.querySelector('iframe').contentWindow;
            globalThis.oldChildParser = new childWindow.DOMParser();
            for (const mime of ['text/html','text/xml','application/xml','application/xhtml+xml','image/svg+xml']) {
                for (const text of ['<root/>','<unclosed>']) {
                    const own = childWindow.DOMParser.prototype.parseFromString.call(parentParser, text, mime);
                    const foreign = DOMParser.prototype.parseFromString.call(oldChildParser, text, mime);
                    if (own.URL !== document.URL || own.documentURI !== document.URL || own.baseURI !== document.URL ||
                        foreign.URL !== childWindow.document.URL || foreign.documentURI !== childWindow.document.URL ||
                        foreign.baseURI !== childWindow.document.URL || own.defaultView !== null || foreign.defaultView !== null)
                        throw new Error('parser identity or base follows invocation instead of creation');
                }
            }
            let invalid = false;
            try { parentParser.parseFromString('', 'TEXT/HTML'); } catch (e) { invalid = e.name === 'TypeError'; }
            return invalid && parentParser.parseFromString(null, 'text/html').body.textContent === 'null';
        })()"#
        ));
        child.set_document_url("https://parent.test/child/one.html?changed=1");
        assert!(eval_bool(engine, "oldChildParser.parseFromString('<root/>','application/xml').URL === 'https://parent.test/child/one.html?changed=1'"));
        assert!(eval_bool(
            engine,
            "document.querySelector('iframe').src='https://parent.test/child/two.html'; true"
        ));
        let request = frame.navigation_request();
        frame
            .install_response_for_request(
                engine.ctx(),
                &request,
                "https://parent.test/child/two.html",
                "text/html",
                "<body>two</body>",
                64,
            )
            .unwrap();
        assert!(eval_bool(
            engine,
            r#"(() => {
            const current = document.querySelector('iframe').contentWindow;
            const old = current.DOMParser.prototype.parseFromString.call(oldChildParser, '<root/>', 'application/xml');
            const fresh = new current.DOMParser().parseFromString('<root/>','application/xml');
            return old.URL === 'https://parent.test/child/one.html?changed=1' && fresh.URL === current.document.URL;
        })()"#
        ));
    }

    #[test]
    fn dom_parser_preserves_xml_namespaces_and_svg_case() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "const parsed=new DOMParser().parseFromString('<svg xmlns=\"http://www.w3.org/2000/svg\"><linearGradient id=\"G\"><stop offset=\"1\"/></linearGradient></svg>','image/svg+xml'); const svg=parsed.documentElement, gradient=parsed.querySelector('linearGradient'); svg.namespaceURI==='http://www.w3.org/2000/svg' && gradient.namespaceURI==='http://www.w3.org/2000/svg' && gradient.localName==='linearGradient' && gradient.getAttribute('id')==='G' && parsed.querySelector('lineargradient')===null"
        ));
    }

    #[test]
    fn namespace_aware_attributes_match_by_uri_and_local_name() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "const parsed=new DOMParser().parseFromString('<root xmlns:x=\"urn:keys\" xmlns:y=\"urn:keys\" x:key=\"old\" plain=\"p\"/>','application/xml'), root=parsed.documentElement; const attr=root.getAttributeNodeNS('urn:keys','key'); const observer=new MutationObserver(()=>{}); observer.observe(root,{attributes:true,attributeOldValue:true}); const initial=root.getAttributeNS('urn:keys','key')==='old'&&root.hasAttributeNS('urn:keys','key')&&root.getAttributeNS(null,'plain')==='p'; root.setAttributeNS('urn:keys','y:key','new'); const records=observer.takeRecords(); const retained=root.getAttribute('x:key')==='new'&&root.getAttribute('y:key')===null&&attr===root.getAttributeNodeNS('urn:keys','key')&&attr.name==='x:key'&&root.getAttributeNS('urn:keys','key')==='new'&&records.length===1&&records[0].attributeName==='key'&&records[0].oldValue==='old'; root.removeAttributeNS('urn:keys','key'); initial&&retained&&!root.hasAttributeNS('urn:keys','key')"
        ));
    }

    #[test]
    fn dom_parser_returns_parsererror_document_for_malformed_xml() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "const parsed=new DOMParser().parseFromString('<root><item></root>','application/xml'); parsed.documentElement.localName==='parsererror' && parsed.documentElement.textContent.includes('mismatched end tag') && parsed.querySelector('root')===null"
        ));
    }

    #[test]
    fn dom_parser_rejects_external_entity_expansion() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "const parsed=new DOMParser().parseFromString('<!DOCTYPE root [<!ENTITY e SYSTEM \"file:///etc/passwd\">]><root>&e;</root>','text/xml'); parsed.documentElement.localName==='parsererror'"
        ));
    }

    #[test]
    fn node_constructors_preserve_document_identity_coercion_and_tree_wrappers() {
        let mut engine = Engine::new();
        let realm =
            super::super::install(engine.ctx(), "<main></main><iframe></iframe>", 32).unwrap();
        assert!(eval_bool(
            &mut engine,
            r#"(() => {
            const owner = document;
            const Fragment = DocumentFragment;
            const text = new Text(null);
            const empty = new Text();
            const comment = new Comment({toString() { return 'comment'; }});
            const fragment = new Fragment();
            if (text.data !== 'null' || empty.data !== '' || comment.data !== 'comment') throw new Error('constructor data: '+text.data+' / '+empty.data+' / '+comment.data);
            if (!(fragment instanceof DocumentFragment) || !(text instanceof Text) || !(comment instanceof Comment)) throw new Error('constructor prototypes');
            if (fragment.ownerDocument !== owner || text.ownerDocument !== owner || comment.ownerDocument !== owner) throw new Error('constructor ownerDocument');
            if (fragment.appendChild(text) !== text) throw new Error('constructor append return identity');
            if (fragment.firstChild !== text) throw new Error('constructor firstChild identity');
            if (fragment.childNodes[0] !== text) throw new Error('constructor childNodes identity');
            fragment.appendChild(comment);
            document.querySelector('main').appendChild(fragment);
            if (fragment.childNodes.length !== 0 || document.querySelector('main').firstChild !== text) throw new Error('fragment consumption identity');
            class DerivedText extends Text {}
            const derived = new DerivedText('derived');
            const derivedFragment = new Fragment();
            derivedFragment.appendChild(derived);
            if (!(derived instanceof DerivedText) || derivedFragment.firstChild !== derived) throw new Error('subclass constructor identity');
            document = {};
            try { if (new Fragment().ownerDocument !== owner) throw new Error('associated document after global assignment'); return true; }
            finally { document = owner; }
        })()"#
        ));
        realm.frame_contexts(engine.ctx()).unwrap();
        assert!(eval_bool(
            &mut engine,
            r#"(() => {
            const child = document.querySelector('iframe').contentWindow;
            const fragment = new child.DocumentFragment();
            const text = new child.Text('child');
            return fragment.ownerDocument === child.document && text.ownerDocument === child.document &&
                fragment.appendChild(text) === text && fragment.firstChild === text;
        })()"#
        ));
    }

    #[test]
    fn xml_serializer_walks_documents_fragments_and_namespace_fixup() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            r#"const serializer=new XMLSerializer(), parsed=new DOMParser().parseFromString('<!DOCTYPE foo PUBLIC "pub" "sys"><foo/>','application/xml'); const documentText=serializer.serializeToString(parsed); const fragment=document.createDocumentFragment(); fragment.append(document.createElement('div'),document.createElement('span')); const fragmentText=serializer.serializeToString(fragment); const namespaced=parsed.createElementNS('urn:item','x:item'); const attribute=parsed.createAttribute('sample'); const attrRoot=parsed.createElement('root'); attrRoot.setAttribute('gt','>'); const img=document.createElement('img'); img.append(document.createElement('style')); documentText==='<!DOCTYPE foo PUBLIC "pub" "sys"><foo/>'&&fragmentText==='<div xmlns="http://www.w3.org/1999/xhtml"></div><span xmlns="http://www.w3.org/1999/xhtml"></span>'&&serializer.serializeToString(namespaced)==='<x:item xmlns:x="urn:item"/>'&&serializer.serializeToString(attribute)===''&&serializer.serializeToString(attrRoot)==='<root gt="&gt;"/>'&&serializer.serializeToString(img)==='<img xmlns="http://www.w3.org/1999/xhtml"><style></style></img>'"#
        ));
    }

    #[test]
    fn xml_serializer_is_lenient_without_weakening_xml_outer_html() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            r#"const parsed=new DOMParser().parseFromString('<root/>','application/xml'), root=parsed.documentElement, comment=parsed.createComment('bad--comment'); root.appendChild(comment); const serializer=new XMLSerializer(); let strict=false; try{root.outerHTML}catch(error){strict=error.name==='InvalidStateError'} serializer.serializeToString(comment)==='<!--bad--comment-->'&&serializer.serializeToString(root)==='<root><!--bad--comment--></root>'&&strict"#
        ));
    }

    #[test]
    fn tree_walker_applies_show_mask_filter_and_reentrancy_guard() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main><b><i></i></b><u></u></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "const root=document.querySelector('main'); const skipped=document.createTreeWalker(root,1,n=>n.tagName==='B'?3:1); const skipOk=skipped.nextNode().tagName==='I'&&skipped.nextNode().tagName==='U'&&skipped.nextNode()===null; const rejected=document.createTreeWalker(root,1,n=>n.tagName==='B'?2:1); const rejectOk=rejected.nextNode().tagName==='U'; let walker, guarded=false, once=false; walker=document.createTreeWalker(root,1,n=>{if(!once){once=true;try{walker.nextNode()}catch(e){guarded=e.name==='InvalidStateError'}}return 1}); const guardOk=walker.nextNode().tagName==='B'&&guarded; skipOk&&rejectOk&&guardOk"
        ));
    }

    #[test]
    fn node_iterator_traverses_in_both_directions_and_treats_reject_as_skip() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main><b><i></i></b><u></u></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "const root=document.querySelector('main'); const it=document.createNodeIterator(root,1,n=>n.tagName==='B'?2:1); const first=it.nextNode()===root; const second=it.nextNode().tagName==='I'; const third=it.nextNode().tagName==='U'; const end=it.nextNode()===null; const back=it.previousNode().tagName==='U'; first&&second&&third&&end&&back&&it.root===root&&it.referenceNode.tagName==='U'&&it.pointerBeforeReferenceNode"
        ));
    }

    #[test]
    fn node_iterator_reference_moves_before_next_sibling_when_subtree_is_removed() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main><b><i></i></b><u></u></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "const main=document.querySelector('main'), b=main.firstChild, iterator=document.createNodeIterator(main,NodeFilter.SHOW_ELEMENT); iterator.nextNode()===main && iterator.nextNode()===b && (main.removeChild(b), iterator.referenceNode===main.lastChild && iterator.pointerBeforeReferenceNode && iterator.nextNode()===main.lastChild)"
        ));
    }

    fn named_element(document: &lumen_html::Document, parent: NodeId, name: &str) -> NodeId {
        let mut child = document.first_child(parent).unwrap();
        while let Some(id) = child {
            if matches!(document.kind(id), Ok(NodeKind::Element { name: tag, .. }) if tag == name) {
                return id;
            }
            child = document.next_sibling(id).unwrap();
        }
        panic!("missing <{name}>");
    }

    #[test]
    fn preorder_traversal_and_reverse_stop_at_the_requested_root() {
        let document =
            html::parse("<main><b><i></i></b><u></u></main><aside></aside>", 64).unwrap();
        let main = named_element(&document, document.root(), "html");
        let body = named_element(&document, main, "body");
        let root = named_element(&document, body, "main");
        let b = named_element(&document, root, "b");
        let i = named_element(&document, b, "i");
        let u = named_element(&document, root, "u");

        assert_eq!(next_in_subtree(&document, root, root).unwrap(), Some(b));
        assert_eq!(next_in_subtree(&document, root, b).unwrap(), Some(i));
        assert_eq!(next_in_subtree(&document, root, i).unwrap(), Some(u));
        assert_eq!(next_in_subtree(&document, root, u).unwrap(), None);
        assert_eq!(previous_in_subtree(&document, root, u).unwrap(), Some(i));
        assert_eq!(previous_in_subtree(&document, root, i).unwrap(), Some(b));
        assert_eq!(previous_in_subtree(&document, root, b).unwrap(), Some(root));
        assert_eq!(previous_in_subtree(&document, root, root).unwrap(), None);
    }

    #[test]
    fn node_show_mask_matches_dom_node_type_bits() {
        let document = html::parse("<main>text<!--comment--></main>", 32).unwrap();
        let html_id = named_element(&document, document.root(), "html");
        let body = named_element(&document, html_id, "body");
        let main = named_element(&document, body, "main");
        let text = document.first_child(main).unwrap().unwrap();
        let comment = document.next_sibling(text).unwrap().unwrap();
        assert_eq!(node_type(document.kind(main).unwrap()), 1);
        assert_eq!(node_type(document.kind(text).unwrap()), 3);
        assert_eq!(node_type(document.kind(comment).unwrap()), 8);
        assert_ne!(
            SHOW_ALL & (1 << (node_type(document.kind(comment).unwrap()) - 1)),
            0
        );
        assert_eq!(SHOW_ALL & (1 << 30), 1 << 30);
    }
}
