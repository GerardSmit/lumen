//! DOM Range and Selection bindings backed by the document arena.
use super::*;
use core::cmp::Ordering;
use lumen_bind::{FromArg, Host, Slot};

pub(crate) use lumen_html::ranges::Boundary;

pub(crate) struct RangeData {
    start: Cell<Boundary>,
    end: Cell<Boundary>,
    realm: RefCell<Rc<DomRealm>>,
    retained: RefCell<[NodeRetention; 2]>,
    wrapper: RefCell<Option<WeakValue>>,
}

#[derive(Clone)]
struct StaticEndpoint {
    point: Boundary,
    realm: Rc<DomRealm>,
}

/// StaticRange keeps node identities and offsets exactly as supplied. Unlike
/// RangeData, these points are deliberately not adjusted by mutation hooks.
pub(crate) struct StaticRangeData {
    start: RefCell<StaticEndpoint>,
    end: RefCell<StaticEndpoint>,
    // NodeRetention follows the node through adoption and reaps it only after
    // the last StaticRange wrapper/event releases its reference.
    _retained: RefCell<[NodeRetention; 2]>,
}

impl StaticRangeData {
    fn new(start: StaticEndpoint, end: StaticEndpoint) -> Rc<Self> {
        let retained = [
            NodeRetention::new(&start.realm, start.point.container),
            NodeRetention::new(&end.realm, end.point.container),
        ];
        Rc::new(Self {
            start: RefCell::new(start),
            end: RefCell::new(end),
            _retained: RefCell::new(retained),
        })
    }

    fn endpoint(&self, start: bool) -> StaticEndpoint {
        if start {
            self.start.borrow().clone()
        } else {
            self.end.borrow().clone()
        }
    }

    /// Remap only endpoint nodes that belong to the source document. Static
    /// ranges may be invalid and may even refer to nodes in different roots.
    /// Keep the registry entry in each realm still owning one endpoint.
    fn adopt_nodes(
        &self,
        source: &Rc<DomRealm>,
        target: &Rc<DomRealm>,
        mapping: &[(NodeId, NodeId)],
    ) -> (bool, bool) {
        let mut moved = false;
        for endpoint in [&self.start, &self.end] {
            let mut endpoint = endpoint.borrow_mut();
            if !Rc::ptr_eq(&endpoint.realm, source) {
                continue;
            }
            if let Some((_, new)) = mapping
                .iter()
                .find(|(old, _)| *old == endpoint.point.container)
            {
                endpoint.point.container = *new;
                endpoint.realm = target.clone();
                moved = true;
            }
        }
        for retention in self._retained.borrow_mut().iter_mut() {
            retention.adopt_nodes(source, target, mapping);
        }
        let remains_in_source = [&self.start, &self.end]
            .into_iter()
            .any(|endpoint| Rc::ptr_eq(&endpoint.borrow().realm, source));
        (moved, remains_in_source)
    }
}

impl RangeData {
    fn new(root: NodeId, realm: &Rc<DomRealm>) -> Rc<Self> {
        let point = Boundary {
            container: root,
            offset: 0,
        };
        let data = Rc::new(Self {
            start: Cell::new(point),
            end: Cell::new(point),
            realm: RefCell::new(realm.clone()),
            retained: RefCell::new([NodeRetention::new(realm, root), NodeRetention::new(realm, root)]),
            wrapper: RefCell::new(None),
        });
        data
    }

    fn copy(&self, realm: &Rc<DomRealm>) -> Rc<Self> {
        let data = Self::new(self.start.get().container, realm);
        data.set_start(self.start.get());
        data.set_end(self.end.get());
        data
    }

    fn set_start(&self, point: Boundary) {
        if self.start.get().container != point.container {
            let realm = self.realm.borrow().clone();
            let retained = NodeRetention::new(&realm, point.container);
            self.start.set(point);
            self.retained.borrow_mut()[0] = retained;
        }
        self.start.set(point);
    }

    fn set_end(&self, point: Boundary) {
        if self.end.get().container != point.container {
            let realm = self.realm.borrow().clone();
            let retained = NodeRetention::new(&realm, point.container);
            self.end.set(point);
            self.retained.borrow_mut()[1] = retained;
        }
        self.end.set(point);
    }

    fn current_realm(&self) -> OpResult<Rc<DomRealm>> {
        Ok(self.realm.borrow().clone())
    }

    fn set_in_document(&self, point: Boundary, start: bool, document: &lumen_html::Document) {
        let realm = self.realm.borrow().clone();
        self.retained.borrow_mut()[usize::from(!start)].replace_in_document(&realm, point.container, document);
        if start { self.start.set(point); } else { self.end.set(point); }
    }

    fn relocate(self: &Rc<Self>, realm: &Rc<DomRealm>, point: Boundary) {
        let old = self.realm.borrow().clone();
        let retained = [NodeRetention::new(realm, point.container), NodeRetention::new(realm, point.container)];
        old.ranges.ranges.borrow_mut().retain(|weak| !weak.ptr_eq(&Rc::downgrade(self)));
        self.start.set(point);
        self.end.set(point);
        *self.realm.borrow_mut() = realm.clone();
        *self.retained.borrow_mut() = retained;
        realm.ranges.register(self);
    }

    fn adopt_nodes(&self, target: &Rc<DomRealm>, mapping: &[(NodeId, NodeId)]) -> bool {
        let map = |point: Boundary| {
            mapping
                .iter()
                .find_map(|(old, new)| (*old == point.container).then_some(*new))
                .map(|container| Boundary { container, ..point })
        };
        let (Some(start), Some(end)) = (map(self.start.get()), map(self.end.get())) else {
            return false;
        };
        self.start.set(start);
        self.end.set(end);
        let source = self.realm.borrow().clone();
        for retention in self.retained.borrow_mut().iter_mut() { retention.adopt_nodes(&source, target, mapping); }
        *self.realm.borrow_mut() = target.clone();
        true
    }
}

pub(crate) struct RangeRegistry {
    ranges: RefCell<Vec<std::rc::Weak<RangeData>>>,
    static_ranges: RefCell<Vec<std::rc::Weak<StaticRangeData>>>,
}

impl RangeRegistry {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self {
            ranges: RefCell::new(Vec::new()),
            static_ranges: RefCell::new(Vec::new()),
        })
    }
    fn register(&self, range: &Rc<RangeData>) {
        let mut ranges = self.ranges.borrow_mut();
        ranges.retain(|entry| entry.strong_count() > 0);
        ranges.push(Rc::downgrade(range));
    }

    fn register_static(&self, range: &Rc<StaticRangeData>) {
        let mut ranges = self.static_ranges.borrow_mut();
        ranges.retain(|entry| entry.strong_count() > 0);
        if !ranges
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .any(|existing| Rc::ptr_eq(&existing, range))
        {
            ranges.push(Rc::downgrade(range));
        }
    }

    pub(crate) fn adopt_nodes(
        &self,
        source_realm: &Rc<DomRealm>,
        target: &RangeRegistry,
        target_realm: &Rc<DomRealm>,
        mapping: &[(NodeId, NodeId)],
    ) {
        let mut ranges = self.ranges.borrow_mut();
        ranges.retain(|entry| entry.strong_count() > 0);
        let mut migrated = Vec::new();
        for range in ranges.iter().filter_map(std::rc::Weak::upgrade) {
            if range.adopt_nodes(target_realm, mapping) {
                migrated.push(Rc::downgrade(&range));
            }
        }
        ranges.retain(|entry| !migrated.iter().any(|moved| moved.ptr_eq(entry)));
        target.ranges.borrow_mut().extend(migrated);

        let mut static_ranges = self.static_ranges.borrow_mut();
        static_ranges.retain(|entry| entry.strong_count() > 0);
        let mut retained_here = Vec::with_capacity(static_ranges.len());
        let mut moved_to_target = Vec::new();
        for weak in static_ranges.drain(..) {
            let Some(range) = weak.upgrade() else {
                continue;
            };
            let (moved, remains_here) = range.adopt_nodes(source_realm, target_realm, mapping);
            let weak = Rc::downgrade(&range);
            if moved {
                moved_to_target.push(weak.clone());
            }
            if remains_here || !moved {
                retained_here.push(weak);
            }
        }
        *static_ranges = retained_here;
        drop(static_ranges);
        if !moved_to_target.is_empty() {
            let mut target_ranges = target.static_ranges.borrow_mut();
            target_ranges.retain(|entry| entry.strong_count() > 0);
            for moved in moved_to_target {
                if !target_ranges.iter().any(|entry| entry.ptr_eq(&moved)) {
                    target_ranges.push(moved);
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PointOrder {
    Before,
    Equal,
    After,
}

fn child_count(document: &lumen_html::Document, node: NodeId) -> OpResult<usize> {
    lumen_html::ranges::child_count(document, node).map_err(dom_error)
}

fn validate_boundary(document: &lumen_html::Document, point: Boundary) -> OpResult<()> {
    let kind = document.kind(point.container).map_err(dom_error)?;
    if matches!(kind, NodeKind::DocumentType(_)) {
        return Err(OpError::new("InvalidNodeTypeError", "DocumentType cannot be a Range boundary"));
    }
    let limit = lumen_html::ranges::length(document, point.container).map_err(dom_error)?;
    if point.offset > limit {
        return Err(OpError::new(
            "IndexSizeError",
            "boundary offset is outside its container",
        ));
    }
    Ok(())
}

fn tree_root(document: &lumen_html::Document, node: NodeId) -> OpResult<NodeId> {
    document.root_node(node, false).map_err(dom_error)
}

pub(crate) fn child_index(document: &lumen_html::Document, node: NodeId) -> OpResult<usize> {
    lumen_html::ranges::child_index(document, node).map_err(dom_error)
}

fn common_ancestor(document: &lumen_html::Document, start: NodeId, end: NodeId) -> OpResult<NodeId> {
    lumen_html::ranges::common_ancestor(document, start, end).map_err(dom_error)?
        .ok_or_else(|| OpError::new("WrongDocumentError", "Range boundaries are disconnected"))
}

fn is_in_subtree(document: &lumen_html::Document, node: NodeId, root: NodeId) -> bool {
    let mut current = Some(node);
    while let Some(id) = current {
        if id == root {
            return true;
        }
        current = document.parent(id).ok().flatten();
    }
    false
}

fn compare_points(document: &lumen_html::Document, a: Boundary, b: Boundary) -> OpResult<PointOrder> {
    Ok(match lumen_html::ranges::compare(document, a, b).map_err(dom_error)? {
        Some(Ordering::Less) => PointOrder::Before,
        Some(Ordering::Equal) => PointOrder::Equal,
        Some(Ordering::Greater) => PointOrder::After,
        None => return Err(OpError::new("WrongDocumentError", "Range boundaries are disconnected")),
    })
}

fn visit_range_text(document: &lumen_html::Document, data: &RangeData,
    mut visit: impl FnMut(NodeId, usize, usize) -> OpResult<()>) -> OpResult<()> {
    let start = data.start.get();
    let end = data.end.get();
    let root = common_ancestor(document, start.container, end.container)?;
    let mut cursor = Some(root);
    while let Some(node) = cursor {
        if matches!(document.kind(node).map_err(dom_error)?, NodeKind::Text(_) | NodeKind::CData(_)) {
            let length = document.character_data_length(node).map_err(dom_error)?;
            if compare_points(document, Boundary { container: node, offset: length }, start)? == PointOrder::After
                && compare_points(document, Boundary { container: node, offset: 0 }, end)? == PointOrder::Before {
                let lo = if start.container == node { start.offset } else { 0 };
                let hi = if end.container == node { end.offset } else { length };
                visit(node, lo, hi - lo)?;
            }
        }
        cursor = document.next_in_subtree(root, node).map_err(dom_error)?;
    }
    Ok(())
}

fn string_for_range(document: &lumen_html::Document, data: &RangeData) -> OpResult<String> {
    let mut text = String::new();
    visit_range_text(document, data, |node, offset, count| {
        text.push_str(&document.substring_data(node, offset, count).map_err(dom_error)?);
        Ok(())
    })?;
    Ok(text)
}

#[derive(Clone)]
pub(crate) enum AbstractRangeBacking {
    Live(Rc<RangeData>),
    Static(Rc<StaticRangeData>),
}

impl AbstractRangeBacking {
    pub(crate) fn same_range(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Live(a), Self::Live(b)) => Rc::ptr_eq(a, b),
            (Self::Static(a), Self::Static(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }
    pub(crate) fn highlight_points(&self, realm: &Rc<DomRealm>) -> Option<(Boundary, Boundary)> {
        let start = self.endpoint(true);
        let end = self.endpoint(false);
        (Rc::ptr_eq(&start.realm, realm) && Rc::ptr_eq(&end.realm, realm))
            .then_some((start.point, end.point))
    }
    fn endpoint(&self, start: bool) -> StaticEndpoint {
        match self {
            Self::Live(data) => StaticEndpoint {
                point: if start {
                    data.start.get()
                } else {
                    data.end.get()
                },
                realm: data.realm.borrow().clone(),
            },
            Self::Static(data) => data.endpoint(start),
        }
    }
}

#[lumen_bind::class(name = "AbstractRange", hint(js(webidl)))]
pub struct DomAbstractRange {
    pub(crate) backing: AbstractRangeBacking,
}

impl DomAbstractRange {
    fn live(data: Rc<RangeData>) -> Self {
        Self {
            backing: AbstractRangeBacking::Live(data),
        }
    }

    fn static_range(data: Rc<StaticRangeData>) -> Self {
        Self {
            backing: AbstractRangeBacking::Static(data),
        }
    }
}

#[lumen_bind::methods]
impl DomAbstractRange {
    #[getter]
    fn start_container(&self, ctx: &mut Ctx) -> Value {
        let endpoint = self.backing.endpoint(true);
        let (realm, node) = endpoint
            .realm
            .resolve_adopted_node(endpoint.point.container);
        realm.wrap(ctx, node)
    }

    #[getter]
    fn start_offset(&self) -> usize {
        self.backing.endpoint(true).point.offset
    }

    #[getter]
    fn end_container(&self, ctx: &mut Ctx) -> Value {
        let endpoint = self.backing.endpoint(false);
        let (realm, node) = endpoint
            .realm
            .resolve_adopted_node(endpoint.point.container);
        realm.wrap(ctx, node)
    }

    #[getter]
    fn end_offset(&self) -> usize {
        self.backing.endpoint(false).point.offset
    }

    #[getter]
    fn collapsed(&self) -> bool {
        let start = self.backing.endpoint(true);
        let end = self.backing.endpoint(false);
        Rc::ptr_eq(&start.realm, &end.realm) && start.point == end.point
    }
}

#[lumen_bind::class(name = "Range", extends = DomAbstractRange, hint(js(webidl)))]
pub struct DomRange {
    base: DomAbstractRange,
    pub(crate) data: Rc<RangeData>,
}

impl DomRange {
    pub(crate) fn into_value(self, ctx: &mut Ctx) -> Value {
        if let Some(value) = self.data.wrapper.borrow().as_ref().and_then(WeakValue::upgrade) { return value; }
        let data = self.data.clone();
        let value = ctx.new_instance(self);
        remember_range(ctx, &data, &value);
        value
    }
    pub(crate) fn new_at(realm: Rc<DomRealm>, registry: Rc<RangeRegistry>, root: NodeId) -> Self {
        let data = RangeData::new(root, &realm);
        registry.register(&data);
        Self::from_data(data)
    }

    fn from_data(data: Rc<RangeData>) -> Self {
        Self {
            base: DomAbstractRange::live(data.clone()),
            data,
        }
    }

    fn set_boundary_in(&self, realm: &Rc<DomRealm>, point: Boundary, start: bool) -> OpResult<()> {
        let session = realm.session.borrow();
        validate_boundary(session.document(), point)?;
        if !Rc::ptr_eq(realm, &self.data.current_realm()?) {
            drop(session);
            self.data.relocate(realm, point);
            return Ok(());
        }
        let current = if start {
            self.data.end.get()
        } else {
            self.data.start.get()
        };
        if tree_root(session.document(), point.container)?
            != tree_root(session.document(), current.container)?
        {
            drop(session);
            self.data.set_start(point);
            self.data.set_end(point);
            return Ok(());
        }
        let (other, order) = if start {
            let end = self.data.end.get();
            (end, compare_points(session.document(), point, end)?)
        } else {
            let begin = self.data.start.get();
            (begin, compare_points(session.document(), begin, point)?)
        };
        drop(session);
        if start {
            if order == PointOrder::After {
                self.data.set_start(point);
                self.data.set_end(point);
            } else {
                self.data.set_start(point);
            }
        } else if order == PointOrder::After {
            self.data.set_start(point);
            self.data.set_end(point);
        } else {
            self.data.set_end(point);
        }
        let _ = other;
        Ok(())
    }

    fn collapse_to(&self, start: bool) {
        let point = if start {
            self.data.start.get()
        } else {
            self.data.end.get()
        };
        self.data.set_start(point);
        self.data.set_end(point);
    }
}

fn remember_range(ctx: &mut Ctx, data: &RangeData, value: &Value) {
    *data.wrapper.borrow_mut() = ctx.weak_value(value);
    ctx.set_native_identity_owner::<DomRange>(value).expect("Range has its native brand");
}

impl lumen::embed::NativeIdentityOwner for DomRange {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, epoch: u64, visit: &mut dyn FnMut(&Value)) {
        let realm = self.data.realm.borrow();
        for point in [self.data.start.get(), self.data.end.get()] {
            let root = { let session = realm.session.borrow(); selector::native_identity_root(session.document(), point.container).ok() };
            if let Some(root) = root { realm.trace_native_identity_component(epoch, root, visit); }
        }
    }
}

struct RangeConstructorResult(DomRange);
impl lumen_bind::CtorRet<lumen::embed::JsHost, DomRange> for RangeConstructorResult {
    fn into_ctor(self, cx: &<lumen::embed::JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        let data = self.0.data.clone();
        let value = <lumen::embed::JsHost as Host>::construct(cx, self.0)?;
        <lumen::embed::JsHost as Host>::with_ctx(cx, |ctx| remember_range(ctx, &data, &value));
        Ok(value)
    }
}

#[lumen_bind::methods]
impl DomRange {
    #[constructor]
    fn new(ctx: &mut Ctx) -> OpResult<RangeConstructorResult> {
        let realm = crate::window_globals::current_dom_realm(ctx)
            .ok_or_else(|| OpError::type_error("Range constructor has no associated document"))?;
        let root = realm.session.borrow().document().root();
        Ok(RangeConstructorResult(Self::new_at(realm.clone(), realm.ranges.clone(), root)))
    }

    #[getter]
    fn common_ancestor_container(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let realm = self.data.current_realm()?;
        let session = realm.session.borrow();
        let common = common_ancestor(session.document(), self.data.start.get().container, self.data.end.get().container)?;
        drop(session);
        Ok(realm.wrap(ctx, common))
    }
    #[method(coerce)]
    fn set_start(&self, node: &DomNode, offset: u32) -> OpResult<()> {
        self.set_boundary_in(&node.realm,
            Boundary {
                container: node.id,
                offset: offset as usize,
            },
            true,
        )
    }
    #[method(coerce)]
    fn set_end(&self, node: &DomNode, offset: u32) -> OpResult<()> {
        self.set_boundary_in(&node.realm,
            Boundary {
                container: node.id,
                offset: offset as usize,
            },
            false,
        )
    }
    fn set_start_before(&self, node: &DomNode) -> OpResult<()> {
        self.set_before_after(node, true)
    }
    fn set_start_after(&self, node: &DomNode) -> OpResult<()> {
        self.set_before_after(node, false)
    }
    fn set_end_before(&self, node: &DomNode) -> OpResult<()> {
        self.set_end_relative(node, true)
    }
    fn set_end_after(&self, node: &DomNode) -> OpResult<()> {
        self.set_end_relative(node, false)
    }
    fn collapse(&self, to_start: Option<bool>) {
        self.collapse_to(to_start.unwrap_or(false));
    }
    fn select_node(&self, node: &DomNode) -> OpResult<()> {
        let realm = node.realm.clone();
        let session = realm.session.borrow();
        let parent = session
            .document()
            .parent(node.id)
            .map_err(dom_error)?
            .ok_or_else(|| OpError::new("InvalidNodeTypeError", "node has no parent"))?;
        let index = child_index(session.document(), node.id)?;
        drop(session);
        self.set_boundary_in(&realm,
            Boundary {
                container: parent,
                offset: index,
            },
            true,
        )?;
        self.set_boundary_in(&realm,
            Boundary {
                container: parent,
                offset: index + 1,
            },
            false,
        )
    }
    fn select_node_contents(&self, node: &DomNode) -> OpResult<()> {
        let realm = node.realm.clone();
        let session = realm.session.borrow();
        validate_boundary(session.document(), Boundary { container: node.id, offset: 0 })?;
        let end = lumen_html::ranges::length(session.document(), node.id).map_err(dom_error)?;
        drop(session);
        self.set_boundary_in(&realm,
            Boundary {
                container: node.id,
                offset: 0,
            },
            true,
        )?;
        self.set_boundary_in(&realm,
            Boundary {
                container: node.id,
                offset: end,
            },
            false,
        )
    }
    #[method(coerce)]
    fn compare_boundary_points(&self, how: u16, source: &DomRange) -> OpResult<i16> {
        if how > 3 {
            return Err(OpError::new("NotSupportedError", "invalid comparison mode"));
        }
        self.check_realm(&source.data.current_realm()?)?;
        let (a, b) = match how {
            0 => (self.data.start.get(), source.data.start.get()),
            1 => (self.data.end.get(), source.data.start.get()),
            2 => (self.data.end.get(), source.data.end.get()),
            _ => (self.data.start.get(), source.data.end.get()),
        };
        let realm = self.data.current_realm()?;
        let order = compare_points(realm.session.borrow().document(), a, b)?;
        Ok(match order {
            PointOrder::Before => -1,
            PointOrder::Equal => 0,
            PointOrder::After => 1,
        })
    }
    #[method(coerce)]
    fn compare_point(&self, node: &DomNode, offset: u32) -> OpResult<i16> {
        let realm = self.data.current_realm()?;
        self.check_realm(&node.realm)?;
        let session = realm.session.borrow();
        let document = session.document();
        if tree_root(document, node.id)? != tree_root(document, self.data.start.get().container)? {
            return Err(OpError::new("WrongDocumentError", "point is in another tree"));
        }
        let point = Boundary { container: node.id, offset: offset as usize };
        validate_boundary(document, point)?;
        if compare_points(document, point, self.data.start.get())? == PointOrder::Before { return Ok(-1); }
        if compare_points(document, point, self.data.end.get())? == PointOrder::After { return Ok(1); }
        Ok(0)
    }
    #[method(coerce)]
    fn is_point_in_range(&self, node: &DomNode, offset: u32) -> OpResult<bool> {
        let realm = self.data.current_realm()?;
        if !Rc::ptr_eq(&realm, &node.realm) { return Ok(false); }
        let session = realm.session.borrow();
        let document = session.document();
        if tree_root(document, node.id)? != tree_root(document, self.data.start.get().container)? { return Ok(false); }
        let point = Boundary { container: node.id, offset: offset as usize };
        validate_boundary(document, point)?;
        Ok(compare_points(document, point, self.data.start.get())? != PointOrder::Before
            && compare_points(document, point, self.data.end.get())? != PointOrder::After)
    }
    fn intersects_node(&self, node: &DomNode) -> OpResult<bool> {
        let realm = self.data.current_realm()?;
        if !Rc::ptr_eq(&realm, &node.realm) { return Ok(false); }
        let session = realm.session.borrow();
        let document = session.document();
        if tree_root(document, node.id)? != tree_root(document, self.data.start.get().container)? { return Ok(false); }
        let Some(parent) = document.parent(node.id).map_err(dom_error)? else { return Ok(true); };
        let index = child_index(document, node.id)?;
        Ok(compare_points(document, Boundary { container: parent, offset: index }, self.data.end.get())? == PointOrder::Before
            && compare_points(document, Boundary { container: parent, offset: index + 1 }, self.data.start.get())? == PointOrder::After)
    }
    fn clone_range(&self, ctx: &mut Ctx) -> Value {
        let realm = self
            .data
            .current_realm()
            .expect("live Range retains its owning realm");
        let data = self.data.copy(&realm);
        realm.ranges.register(&data);
        Self {
            base: DomAbstractRange::live(data.clone()),
            data,
        }.into_value(ctx)
    }
    fn to_string(&self) -> OpResult<String> {
        let realm = self.data.current_realm()?;
        let session = realm.session.borrow();
        string_for_range(session.document(), &self.data)
    }
    #[method(hint(js(ce_reactions)))]
    fn delete_contents(&self) -> OpResult<()> {
        self.remove_contents(false).map(|_| ())
    }
    #[method(hint(js(ce_reactions)))]
    fn extract_contents(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let realm = self.data.current_realm()?;
        self.remove_contents(true).map(|id| realm.wrap(ctx, id))
    }
    #[method(hint(js(ce_reactions)))]
    fn clone_contents(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let realm = self.data.current_realm()?;
        let (fragment, _) = lumen_html::ranges::contents(realm.session.borrow_mut().document_mut(), self.data.start.get(), self.data.end.get(), lumen_html::ranges::ContentsMode::Clone).map_err(dom_error)?;
        Ok(realm.wrap(ctx, fragment.expect("cloning produces a fragment")))
    }
    #[method(hint(js(ce_reactions)))]
    fn insert_node(&self, ctx: &mut Ctx, node: DomNodeIdentity) -> OpResult<()> {
        let realm = self.data.current_realm()?;
        self.insert(ctx, &node)?;
        realm.flush_script_activations(ctx)
    }

    #[method(hint(js(ce_reactions)))]
    fn surround_contents(&self, ctx: &mut Ctx, new_parent: DomNodeIdentity) -> OpResult<()> {
        let realm = self.data.current_realm()?;
        {
            let session = realm.session.borrow();
            let document = session.document();
            let start = self.data.start.get();
            let end = self.data.end.get();
            let common = common_ancestor(document, start.container, end.container)?;
            // A partially contained node is an inclusive ancestor of exactly
            // one endpoint. Inspect only their ancestor paths.
            for endpoint in [start.container, end.container] {
                let mut cursor = endpoint;
                while cursor != common {
                    if !matches!(document.kind(cursor).map_err(dom_error)?, NodeKind::Text(_) | NodeKind::CData(_)) {
                        return Err(OpError::new("InvalidStateError", "a non-Text node is partially contained"));
                    }
                    cursor = document.parent(cursor).map_err(dom_error)?.ok_or_else(|| OpError::new("WrongDocumentError", "disconnected boundary"))?;
                }
            }
        }
        let (parent_realm, parent_id) = ctx.with_instance::<DomNode, _>(&new_parent.value, |node| (node.realm.clone(), node.id))?;
        if matches!(parent_realm.session.borrow().document().kind(parent_id).map_err(dom_error)?, NodeKind::Document | NodeKind::DocumentType(_) | NodeKind::DocumentFragment) {
            return Err(OpError::new("InvalidNodeTypeError", "invalid surround parent"));
        }
        let fragment = self.remove_contents(true)?;
        let _retained = NodeRetention::new(&realm, fragment);
        {
            let mut session = parent_realm.session.borrow_mut();
            let document = session.document_mut();
            if document.first_child(parent_id).map_err(dom_error)?.is_some() {
                let old = children(document, parent_id).map_err(dom_error)?;
                drop(session);
                let select_mutation = capture_select_mutation(&parent_realm, parent_id, &[], &old)?;
                parent_realm.session.borrow_mut().document_mut().replace_children_many(parent_id, &[]).map_err(dom_error)?;
                apply_select_mutation(&parent_realm, select_mutation)?;
                parent_realm.invalidate_textarea_ancestor(parent_id);
                parent_realm.reap_detached(old);
            }
        }
        self.insert(ctx, &new_parent)?;
        let (owner, parent) = ctx.with_instance::<DomNode, _>(&new_parent.value, |node| (node.realm.clone(), node.id))?;
        let fragment_value = realm.wrap(ctx, fragment);
        insert_dom_node(ctx, &owner, parent, fragment_value, Value::Null)?;
        let (container, index) = {
            let session = owner.session.borrow();
            let document = session.document();
            (document.parent(parent).map_err(dom_error)?.ok_or_else(|| OpError::new("InvalidNodeTypeError", "surround parent has no parent"))?, child_index(document, parent)?)
        };
        self.set_boundary_in(&owner, Boundary { container, offset: index }, true)?;
        self.set_boundary_in(&owner, Boundary { container, offset: index + 1 }, false)?;
        owner.flush_script_activations(ctx)
    }

    #[method(coerce, hint(js(ce_reactions)))]
    fn create_contextual_fragment(&self, ctx: &mut Ctx, markup: &str) -> OpResult<Value> {
        let _html_allocations = enter_html_allocation_category();
        let realm = self.data.current_realm()?;
        let parsed = {
            let mut session = realm.session.borrow_mut();
            contextual_fragment(session.document_mut(), self.data.start.get().container, markup)
        };
        let fragment = parsed.map_err(|error| dom_markup_error(ctx, error))?;
        // Fragment construction neither inserts nodes nor marks scripts already started.
        // The existing insertion mutation hooks prepare/run them when later connected.
        Ok(realm.wrap(ctx, fragment))
    }
    fn detach(&self) {}
}

/// Resolve only the Range's start container. Parsing stays in its current document arena,
/// including after adoption, and uses the existing HTML/XML context and rollback algorithms.
fn contextual_fragment(
    document: &mut lumen_html::Document,
    start: NodeId,
    markup: &str,
) -> OpResult<NodeId> {
    let context = match document.kind(start).map_err(dom_error)? {
        NodeKind::Element { .. } => Some(start),
        NodeKind::Text(_) | NodeKind::CData(_) | NodeKind::Comment(_) => document.parent(start).map_err(dom_error)?.filter(|parent| {
            matches!(document.kind(*parent), Ok(NodeKind::Element { .. }))
        }),
        _ => None,
    };
    let body_fallback = context.is_none() || (document.is_html_document() && context.is_some_and(|id| {
        matches!(document.kind(id), Ok(NodeKind::Element { namespace: Namespace::Html, .. })) &&
            document.element_name_parts(id).is_ok_and(|(_, local)| local == "html")
    }));
    let fragment = if body_fallback {
        // This detached context contributes parser state, not a node in the result. Reclaim
        // it on both success and failure; the parser reclaims partial fragments on error.
        let body = document.create(NodeKind::Element {
            namespace: Namespace::Html,
            name: "body".into(),
            attributes: Vec::new(),
        }).map_err(dom_error)?;
        let parsed = parse_dom_markup_fragment(document, body, markup);
        document.destroy_subtree(body).map_err(dom_error)?;
        parsed?
    } else {
        parse_dom_markup_fragment(document, context.unwrap(), markup)?
    };
    if !document.is_html_document() {
        if let Err(error) = strip_contextual_xml_scaffolding(document, fragment) {
            document.destroy_subtree(fragment).map_err(dom_error)?;
            return Err(error);
        }
    }
    Ok(fragment)
}

/// XML fragment parsing can produce the scaffold which HTML fragment parsing elides.
/// Expand only top-level HTML-namespace html/head/body wrappers, revisiting promoted nodes
/// for nested wrappers. Other namespaces and ordinary descendant elements remain intact.
fn strip_contextual_xml_scaffolding(document: &mut lumen_html::Document, fragment: NodeId) -> OpResult<()> {
    let mut node = document.first_child(fragment).map_err(dom_error)?;
    while let Some(id) = node {
        let next = document.next_sibling(id).map_err(dom_error)?;
        let scaffold = matches!(document.kind(id).map_err(dom_error)?,
            NodeKind::Element { namespace: Namespace::Html, .. }) &&
            document.element_name_parts(id).is_ok_and(|(_, local)| matches!(local, "html" | "head" | "body"));
        if scaffold {
            let first = document.first_child(id).map_err(dom_error)?;
            while let Some(child) = document.first_child(id).map_err(dom_error)? {
                document.insert_before(fragment, child, Some(id)).map_err(dom_error)?;
            }
            document.remove(id).map_err(dom_error)?;
            document.destroy_subtree(id).map_err(dom_error)?;
            node = first.or(next);
        } else {
            node = next;
        }
    }
    Ok(())
}

impl DomRange {
    fn check_realm(&self, realm: &Rc<DomRealm>) -> OpResult<()> {
        if Rc::ptr_eq(&self.data.current_realm()?, realm) {
            Ok(())
        } else {
            Err(OpError::new(
                "WrongDocumentError",
                "Range belongs to another document",
            ))
        }
    }
    fn set_before_after(&self, node: &DomNode, before: bool) -> OpResult<()> {
        let realm = node.realm.clone();
        let session = realm.session.borrow();
        let parent = session
            .document()
            .parent(node.id)
            .map_err(dom_error)?
            .ok_or_else(|| OpError::new("InvalidNodeTypeError", "node has no parent"))?;
        let offset = child_index(session.document(), node.id)? + usize::from(!before);
        drop(session);
        self.set_boundary_in(&realm,
            Boundary {
                container: parent,
                offset,
            },
            true,
        )
    }
    fn set_end_relative(&self, node: &DomNode, before: bool) -> OpResult<()> {
        let realm = node.realm.clone();
        let session = realm.session.borrow();
        let parent = session
            .document()
            .parent(node.id)
            .map_err(dom_error)?
            .ok_or_else(|| OpError::new("InvalidNodeTypeError", "node has no parent"))?;
        let offset = child_index(session.document(), node.id)? + usize::from(!before);
        drop(session);
        self.set_boundary_in(&realm,
            Boundary {
                container: parent,
                offset,
            },
            false,
        )
    }
    fn insert(&self, ctx: &mut Ctx, node: &DomNodeIdentity) -> OpResult<()> {
        let point = self.data.start.get();
        let realm = self.data.current_realm()?;
        let (source, source_id) = ctx.with_instance::<DomNode, _>(&node.value, |node| (node.realm.clone(), node.id))?;
        let (parent, mut reference, split) = {
            let session = realm.session.borrow();
            let document = session.document();
            let text = matches!(document.kind(point.container).map_err(dom_error)?, NodeKind::Text(_) | NodeKind::CData(_));
            if matches!(document.kind(point.container).map_err(dom_error)?, NodeKind::Comment(_) | NodeKind::ProcessingInstruction { .. })
                || (Rc::ptr_eq(&realm, &source) && point.container == source_id) {
                return Err(OpError::new("HierarchyRequestError", "invalid Range insertion boundary"));
            }
            let reference = if text { Some(point.container) } else {
                let mut child = document.first_child(point.container).map_err(dom_error)?;
                for _ in 0..point.offset { child = match child { Some(id) => document.next_sibling(id).map_err(dom_error)?, None => None }; }
                child
            };
            let parent = if text { document.parent(point.container).map_err(dom_error)?.ok_or_else(|| OpError::new("HierarchyRequestError", "Text has no parent"))? } else { point.container };
            if Rc::ptr_eq(&realm, &source) {
                document.validate_insert_from(document, parent, source_id, reference).map_err(dom_error)?;
            } else {
                document.validate_insert_from(source.session.borrow().document(), parent, source_id, reference).map_err(dom_error)?;
            }
            (parent, reference, text)
        };
        if split { reference = Some(realm.session.borrow_mut().document_mut().split_text(point.container, point.offset).map_err(dom_error)?); }
        if Rc::ptr_eq(&realm, &source) && reference == Some(source_id) {
            reference = realm.session.borrow().document().next_sibling(source_id).map_err(dom_error)?;
        }
        if source.session.borrow().document().parent(source_id).map_err(dom_error)?.is_some() {
            remove_dom_node(&source, source_id)?;
        }
        let new_offset = {
            let session = realm.session.borrow();
            let document = session.document();
            let index = match reference { Some(id) => child_index(document, id)?, None => child_count(document, parent)? };
            let donor = source.session.borrow();
            index + if matches!(donor.document().kind(source_id).map_err(dom_error)?, NodeKind::DocumentFragment) { child_count(donor.document(), source_id)? } else { 1 }
        };
        let collapsed = self.data.start.get() == self.data.end.get();
        let before = realm.wrap_option(ctx, reference);
        insert_dom_node(ctx, &realm, parent, node.value.clone(), before)?;
        if collapsed {
            let session = realm.session.borrow();
            self.data.set_in_document(Boundary { container: parent, offset: new_offset }, false, session.document());
        }
        Ok(())
    }
    fn remove_contents(&self, extract: bool) -> OpResult<NodeId> {
        let realm = self.data.current_realm()?;
        let mut session = realm.session.borrow_mut();
        let document = session.document_mut();
        let mode = if extract { lumen_html::ranges::ContentsMode::Extract } else { lumen_html::ranges::ContentsMode::Delete };
        let (fragment, collapse) = lumen_html::ranges::contents(document, self.data.start.get(), self.data.end.get(), mode).map_err(dom_error)?;
        self.data.set_in_document(collapse, true, document);
        self.data.set_in_document(collapse, false, document);
        Ok(fragment.unwrap_or(document.root()))
    }

}

/// State shared by `document.getSelection()` wrappers in one realm.
pub(crate) struct SelectionData {
    pub(crate) registry: Rc<RangeRegistry>,
    pub(crate) ranges: RefCell<Vec<Rc<RangeData>>>,
    pub(crate) anchor: Cell<Option<Boundary>>,
    pub(crate) focus: Cell<Option<Boundary>>,
    backward: Cell<Option<bool>>,
    wrapper: RefCell<Option<Value>>,
}

impl SelectionData {
    pub(crate) fn new(registry: Rc<RangeRegistry>) -> Rc<Self> {
        Rc::new(Self {
            registry,
            ranges: RefCell::new(Vec::new()),
            anchor: Cell::new(None),
            focus: Cell::new(None),
            backward: Cell::new(None),
            wrapper: RefCell::new(None),
        })
    }

    pub(crate) fn trace_values(&self, visit: &mut dyn FnMut(&Value)) {
        if let Some(value) = self.wrapper.borrow().as_ref() { visit(value); }
    }

    /// Reset the existing active range without changing its authored identity.
    pub(crate) fn reset_active_range_to_document(&self,realm:&Rc<DomRealm>,owner:NodeId)->OpResult<()> {
        let range=self.ranges.borrow().first().cloned();
        let Some(range)=range else{return Ok(());};
        if !Rc::ptr_eq(&range.realm.borrow(),realm) {return Ok(());}
        let session=realm.session.borrow();let document=session.document();
        if document.node_document(range.start.get().container).map_err(dom_error)?!=owner
            || document.root_node(range.start.get().container,true).map_err(dom_error)?!=owner
            || document.root_node(range.end.get().container,true).map_err(dom_error)?!=owner {return Ok(());}
        let point=Boundary{container:owner,offset:0};
        range.set_in_document(point,true,document);range.set_in_document(point,false,document);
        drop(session);sync_selection(self);Ok(())
    }

    pub(crate) fn adopt_nodes(&self, mapping: &[(NodeId, NodeId)]) {
        let remap = |point: Option<Boundary>| {
            point.map(|mut point| {
                if let Some((_, new)) = mapping.iter().find(|(old, _)| *old == point.container) {
                    point.container = *new;
                }
                point
            })
        };
        self.anchor.set(remap(self.anchor.get()));
        self.focus.set(remap(self.focus.get()));
    }
}

fn required_static_range_member(
    ctx: &mut Ctx,
    init: &Value,
    name: &'static str,
) -> Result<Value, Value> {
    if matches!(init, Value::Null | Value::Undefined) {
        return Err(ctx.make_error("TypeError", format!("StaticRangeInit.{name} is required")));
    }
    let value = ctx.member_get(init, name)?;
    if matches!(value, Value::Undefined) {
        Err(ctx.make_error("TypeError", format!("StaticRangeInit.{name} is required")))
    } else {
        Ok(value)
    }
}

struct StaticRangeNode {
    endpoint: StaticEndpoint,
}

impl<'a> FromArg<'a, lumen::embed::JsHost> for StaticRangeNode {
    fn from_arg(
        cx: &'a lumen::embed::ArgCx<'_>,
        value: &'a Value,
        at: Slot,
    ) -> Result<Self, Value> {
        let node = <lumen::embed::JsHost as Host>::class_ref::<DomNode>(cx, value, at)?;
        Ok(Self {
            endpoint: StaticEndpoint {
                point: Boundary {
                    container: node.id,
                    offset: 0,
                },
                realm: node.realm.clone(),
            },
        })
    }
}

struct StaticRangeInit {
    end_container: StaticRangeNode,
    end_offset: u32,
    start_container: StaticRangeNode,
    start_offset: u32,
}

impl<'a> FromArg<'a, lumen::embed::JsHost> for StaticRangeInit {
    fn from_arg(cx: &'a lumen::embed::ArgCx<'_>, init: &'a Value, at: Slot) -> Result<Self, Value> {
        // StaticRangeInit members are converted in Web IDL dictionary order.
        // Leave the interpreter context before invoking each typed conversion;
        // numeric ToUint32 conversion then goes through the shared FromArg impl.
        let end_container_value = <lumen::embed::JsHost as Host>::with_ctx(cx, |ctx| {
            required_static_range_member(ctx, init, "endContainer")
        })?;
        let end_container = <StaticRangeNode as FromArg<'_, lumen::embed::JsHost>>::from_arg(
            cx,
            &end_container_value,
            at,
        )?;

        let end_offset_value = <lumen::embed::JsHost as Host>::with_ctx(cx, |ctx| {
            required_static_range_member(ctx, init, "endOffset")
        })?;
        let end_offset =
            <u32 as FromArg<'_, lumen::embed::JsHost>>::from_arg(cx, &end_offset_value, at)?;

        let start_container_value = <lumen::embed::JsHost as Host>::with_ctx(cx, |ctx| {
            required_static_range_member(ctx, init, "startContainer")
        })?;
        let start_container = <StaticRangeNode as FromArg<'_, lumen::embed::JsHost>>::from_arg(
            cx,
            &start_container_value,
            at,
        )?;

        let start_offset_value = <lumen::embed::JsHost as Host>::with_ctx(cx, |ctx| {
            required_static_range_member(ctx, init, "startOffset")
        })?;
        let start_offset =
            <u32 as FromArg<'_, lumen::embed::JsHost>>::from_arg(cx, &start_offset_value, at)?;

        Ok(Self {
            end_container,
            end_offset,
            start_container,
            start_offset,
        })
    }
}

#[lumen_bind::class(name = "StaticRange", extends = DomAbstractRange, hint(js(webidl)))]
pub struct DomStaticRange {
    base: DomAbstractRange,
}

#[lumen_bind::methods]
impl DomStaticRange {
    #[constructor(coerce)]
    fn new(_ctx: &mut Ctx, init: StaticRangeInit) -> OpResult<Self> {
        let mut end = init.end_container.endpoint.clone();
        let end_offset = init.end_offset as usize;
        let mut start = init.start_container.endpoint.clone();
        let start_offset = init.start_offset as usize;
        end.point.offset = end_offset;
        start.point.offset = start_offset;

        for endpoint in [&start, &end] {
            let session = endpoint.realm.session.borrow();
            if matches!(
                session.document().kind(endpoint.point.container),
                Ok(NodeKind::Attribute { .. } | NodeKind::DocumentType(_))
            ) {
                return Err(OpError::new(
                    "InvalidNodeTypeError",
                    "StaticRange boundary containers cannot be Attr or DocumentType nodes",
                ));
            }
        }

        let data = StaticRangeData::new(start.clone(), end.clone());
        start.realm.ranges.register_static(&data);
        if !Rc::ptr_eq(&start.realm, &end.realm) {
            end.realm.ranges.register_static(&data);
        }
        // Keep converted Node wrapper values alive until native retention is
        // established, including when dictionary getters produced temporaries.
        Ok(Self {
            base: DomAbstractRange::static_range(data),
        })
    }
}

#[lumen_bind::class(name = "Selection", hint(js(webidl)))]
pub struct DomSelection {
    pub(crate) realm: Rc<DomRealm>,
    pub(crate) data: Rc<SelectionData>,
}

impl lumen::embed::NativeIdentityOwner for DomSelection {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) { self.data.trace_values(visit); }
}

impl DomSelection {
    fn associate(&self, ctx: &mut Ctx, data: Rc<RangeData>, value: Option<Value>) {
        let wrapper = value.unwrap_or_else(|| DomRange::from_data(data.clone()).into_value(ctx));
        *self.data.ranges.borrow_mut() = vec![data];
        *self.data.wrapper.borrow_mut() = Some(wrapper);
        sync_selection(&self.data);
    }

    fn visible_points(&self) -> Option<(Boundary, Boundary)> {
        sync_selection(&self.data);
        let range = self.data.ranges.borrow().first().cloned()?;
        if !Rc::ptr_eq(&self.realm, &range.realm.borrow()) { return None; }
        let session = self.realm.session.borrow();
        let document = session.document();
        if document.root_node(range.start.get().container, true).ok()? != document.root()
            || document.root_node(range.end.get().container, true).ok()? != document.root() { return None; }
        Some((self.data.anchor.get()?, self.data.focus.get()?))
    }

    fn collapse_to_endpoint(&self, ctx: &mut Ctx, start: bool) -> OpResult<()> {
        let range = self
            .data
            .ranges
            .borrow()
            .first()
            .cloned()
            .ok_or_else(|| OpError::new("InvalidStateError", "Selection has no range"))?;
        let point = if start {
            range.start.get()
        } else {
            range.end.get()
        };
        let realm = range.current_realm()?;
        let replacement = RangeData::new(point.container, &realm);
        replacement.set_start(point);
        replacement.set_end(point);
        realm.ranges.register(&replacement);
        self.associate(ctx, replacement, None);
        Ok(())
    }
}

#[lumen_bind::methods]
impl DomSelection {
    #[getter]
    fn anchor_node(&self, ctx: &mut Ctx) -> Value {
        let point = self.visible_points().map(|points| points.0);
        let realm = self
            .data
            .ranges
            .borrow()
            .first()
            .and_then(|range| range.current_realm().ok())
            .unwrap_or_else(|| self.realm.clone());
        point.map_or(Value::Null, |point| realm.wrap(ctx, point.container))
    }
    #[getter]
    fn anchor_offset(&self) -> usize {
        self.visible_points().map_or(0, |points| points.0.offset)
    }
    #[getter]
    fn focus_node(&self, ctx: &mut Ctx) -> Value {
        let point = self.visible_points().map(|points| points.1);
        let realm = self
            .data
            .ranges
            .borrow()
            .first()
            .and_then(|range| range.current_realm().ok())
            .unwrap_or_else(|| self.realm.clone());
        point.map_or(Value::Null, |point| realm.wrap(ctx, point.container))
    }
    #[getter]
    fn focus_offset(&self) -> usize {
        self.visible_points().map_or(0, |points| points.1.offset)
    }
    #[getter]
    fn is_collapsed(&self) -> bool {
        self.visible_points().is_none_or(|points| points.0 == points.1)
    }
    #[getter]
    fn range_count(&self) -> usize {
        usize::from(self.visible_points().is_some())
    }
    #[getter(name = "type")]
    fn selection_type(&self) -> &'static str {
        match self.visible_points() { None => "None", Some((a, f)) if a == f => "Caret", Some(_) => "Range" }
    }
    #[getter]
    fn direction(&self) -> &'static str {
        if self.data.ranges.borrow().is_empty() { return "none"; }
        match self.data.backward.get() { Some(true) => "backward", Some(false) => "forward", None => "none" }
    }
    #[method(coerce)]
    fn get_range_at(&self, ctx: &mut Ctx, index: u32) -> OpResult<Value> {
        if index != 0 || self.visible_points().is_none() { return Err(OpError::new("IndexSizeError", "selection range index is out of bounds")); }
        let data = self
            .data
            .ranges
            .borrow()
            .get(index as usize)
            .cloned()
            .ok_or_else(|| {
                OpError::new("IndexSizeError", "selection range index is out of bounds")
            })?;
        Ok(DomRange::from_data(data).into_value(ctx))
    }
    fn add_range(&self, ctx: &mut Ctx, range: &DomRange) -> OpResult<()> {
        if !Rc::ptr_eq(&self.realm, &range.data.current_realm()?) { return Ok(()); }
        {
            let session = self.realm.session.borrow();
            if tree_root(session.document(), range.data.start.get().container)? != session.document().root() { return Ok(()); }
        }
        if self.range_count() != 0 { return Ok(()); }
        self.data.backward.set(Some(false));
        self.associate(ctx, range.data.clone(), None);
        Ok(())
    }
    fn remove_range(&self, range: &DomRange) -> OpResult<()> {
        let mut ranges = self.data.ranges.borrow_mut();
        let Some(index) = ranges.iter().position(|item| Rc::ptr_eq(item, &range.data)) else {
            return Err(OpError::new(
                "NotFoundError",
                "range is not in this Selection",
            ));
        };
        ranges.remove(index);
        *self.data.wrapper.borrow_mut() = None;
        if ranges.is_empty() {
            self.data.anchor.set(None);
            self.data.focus.set(None);
            self.data.backward.set(None);
        }
        Ok(())
    }
    fn remove_all_ranges(&self) {
        self.data.ranges.borrow_mut().clear();
        *self.data.wrapper.borrow_mut() = None;
        self.data.anchor.set(None);
        self.data.focus.set(None);
        self.data.backward.set(None);
    }
    fn empty(&self) {
        self.remove_all_ranges();
    }
    #[method(coerce)]
    fn collapse(&self, ctx: &mut Ctx, node: Option<&DomNode>, offset: Option<u32>) -> OpResult<()> {
        let Some(node) = node else {
            self.remove_all_ranges();
            return Ok(());
        };
        let point = Boundary {
            container: node.id,
            offset: offset.unwrap_or(0) as usize,
        };
        validate_boundary(node.realm.session.borrow().document(), point)?;
        if !Rc::ptr_eq(&self.realm, &node.realm) { return Ok(()); }
        {
            let session = self.realm.session.borrow();
            if session.document().root_node(node.id, true).map_err(dom_error)? != session.document().root() { return Ok(()); }
        }
        let range = RangeData::new(point.container, &self.realm);
        range.set_start(point);
        range.set_end(point);
        self.data.registry.register(&range);
        self.associate(ctx, range, None);
        self.data.anchor.set(Some(point));
        self.data.focus.set(Some(point));
        Ok(())
    }
    fn collapse_to_start(&self, ctx: &mut Ctx) -> OpResult<()> {
        self.collapse_to_endpoint(ctx, true)
    }
    #[method(coerce)]
    fn set_position(&self, ctx: &mut Ctx, node: Option<&DomNode>, offset: Option<u32>) -> OpResult<()> {
        self.collapse(ctx, node, offset)
    }
    fn collapse_to_end(&self, ctx: &mut Ctx) -> OpResult<()> {
        self.collapse_to_endpoint(ctx, false)
    }
    #[method(coerce)]
    fn set_base_and_extent(
        &self,
        ctx: &mut Ctx,
        anchor: &DomNode,
        anchor_offset: u32,
        focus: &DomNode,
        focus_offset: u32,
    ) -> OpResult<()> {
        if anchor_offset as usize > lumen_html::ranges::length(anchor.realm.session.borrow().document(), anchor.id).map_err(dom_error)?
            || focus_offset as usize > lumen_html::ranges::length(focus.realm.session.borrow().document(), focus.id).map_err(dom_error)? { return Err(OpError::new("IndexSizeError", "selection offset exceeds node length")); }
        if !Rc::ptr_eq(&self.realm, &anchor.realm) || !Rc::ptr_eq(&self.realm, &focus.realm) { return Ok(()); }
        let a = Boundary {
            container: anchor.id,
            offset: anchor_offset as usize,
        };
        let f = Boundary {
            container: focus.id,
            offset: focus_offset as usize,
        };
        let session = self.realm.session.borrow();
        if session.document().root_node(anchor.id, true).map_err(dom_error)? != session.document().root()
            || session.document().root_node(focus.id, true).map_err(dom_error)? != session.document().root() { return Ok(()); }
        validate_boundary(session.document(), a)?;
        validate_boundary(session.document(), f)?;
        let order = compare_points(session.document(), a, f)?;
        drop(session);
        let range = RangeData::new(
            if order == PointOrder::After {
                focus.id
            } else {
                anchor.id
            },
            &self.realm,
        );
        if order == PointOrder::After {
            range.set_start(f);
            range.set_end(a);
        } else {
            range.set_start(a);
            range.set_end(f);
        }
        self.data.registry.register(&range);
        self.associate(ctx, range, None);
        self.data.anchor.set(Some(a));
        self.data.focus.set(Some(f));
        self.data.backward.set(Some(order == PointOrder::After));
        Ok(())
    }
    #[method(coerce)]
    fn extend(&self, ctx: &mut Ctx, node: &DomNode, offset: Option<u32>) -> OpResult<()> {
        if !Rc::ptr_eq(&self.realm, &node.realm) { return Ok(()); }
        {
            let session = self.realm.session.borrow();
            if session.document().root_node(node.id, true).map_err(dom_error)? != session.document().root() { return Ok(()); }
        }
        sync_selection(&self.data);
        let previous = self.data.ranges.borrow().first().cloned().ok_or_else(|| OpError::new("InvalidStateError", "Selection has no range"))?;
        let focus = Boundary { container: node.id, offset: offset.unwrap_or(0) as usize };
        let anchor = self.data.anchor.get().expect("associated range has an anchor");
        let (start, end, backward) = {
            let session = self.realm.session.borrow();
            let document = session.document();
            validate_boundary(document, focus)?;
            if !Rc::ptr_eq(&self.realm, &previous.realm.borrow()) || tree_root(document, focus.container)? != tree_root(document, previous.start.get().container)? {
                (focus, focus, false)
            } else if compare_points(document, anchor, focus)? == PointOrder::After {
                (focus, anchor, true)
            } else { (anchor, focus, false) }
        };
        let range = RangeData::new(start.container, &self.realm);
        range.set_start(start);
        range.set_end(end);
        self.data.registry.register(&range);
        self.data.backward.set(Some(backward));
        self.associate(ctx, range, None);
        Ok(())
    }
    #[method(hint(js(ce_reactions)))]
    fn delete_from_document(&self) -> OpResult<()> {
        if self.visible_points().is_none() { return Ok(()); }
        if let Some(range) = self.data.ranges.borrow().first() {
            DomRange::from_data(range.clone()).delete_contents()?;
        }
        Ok(())
    }
    fn select_all_children(&self, ctx: &mut Ctx, node: &DomNode) -> OpResult<()> {
        if matches!(node.realm.session.borrow().document().kind(node.id).map_err(dom_error)?, NodeKind::DocumentType(_)) { return Err(OpError::new("InvalidNodeTypeError", "DocumentType cannot be selected")); }
        if !Rc::ptr_eq(&self.realm, &node.realm) { return Ok(()); }
        if tree_root(self.realm.session.borrow().document(), node.id)? != self.realm.session.borrow().document().root() { return Ok(()); }
        let count = child_count(self.realm.session.borrow().document(), node.id)?;
        let range = RangeData::new(node.id, &self.realm);
        range.set_end(Boundary {
            container: node.id,
            offset: count,
        });
        self.data.registry.register(&range);
        self.associate(ctx, range.clone(), None);
        self.data.anchor.set(Some(range.start.get()));
        self.data.focus.set(Some(range.end.get()));
        self.data.backward.set(Some(false));
        Ok(())
    }
    fn to_string(&self) -> OpResult<String> {
        let ranges = self.data.ranges.borrow();
        let mut output = String::new();
        let mut context = None;
        for range in ranges.iter() {
            let realm = range.current_realm()?;
            let mut segments = Vec::new();
            visit_range_text(realm.session.borrow().document(), range, |node, offset, count| {
                if segments.len().saturating_mul(core::mem::size_of::<(NodeId, usize, usize)>()) >= lumen_common::bidi::MAX_TEXT_BYTES {
                    return Err(OpError::new("QuotaExceededError", "selection source limit exceeded"));
                }
                segments.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "selection allocation failed"))?;
                segments.push((node, offset, count));
                Ok(())
            })?;
            let mut session = realm.session.borrow_mut();
            for (node, offset, count) in segments {
                let text = lumen_html::rendered_text::transformed_source_text(&mut session, node, offset, count, &mut context)
                    .map_err(|error| OpError::new("InvalidStateError", format!("{error:?}")))?;
                if output.len().saturating_add(text.len()) > lumen_common::bidi::MAX_TEXT_BYTES {
                    return Err(OpError::new("QuotaExceededError", "selection text limit exceeded"));
                }
                output.try_reserve(text.len()).map_err(|_| OpError::new("QuotaExceededError", "selection allocation failed"))?;
                output.push_str(&text);
            }
        }
        Ok(output)
    }
    fn contains_node(
        &self,
        node: &DomNode,
        allow_partial_containment: Option<bool>,
    ) -> OpResult<bool> {
        if !Rc::ptr_eq(&self.realm, &node.realm) {
            return Ok(false);
        }
        let session = self.realm.session.borrow();
        let parent = session.document().parent(node.id).map_err(dom_error)?;
        let Some(parent) = parent else {
            return Ok(false);
        };
        let index = child_index(session.document(), node.id)?;
        let before = Boundary {
            container: parent,
            offset: index,
        };
        let after = Boundary {
            container: parent,
            offset: index + 1,
        };
        for range in self.data.ranges.borrow().iter() {
            let begins_before_end =
                compare_points(session.document(), before, range.end.get())? == PointOrder::Before;
            let ends_after_start =
                compare_points(session.document(), after, range.start.get())? == PointOrder::After;
            let fully = compare_points(session.document(), range.start.get(), before)?
                != PointOrder::After
                && compare_points(session.document(), after, range.end.get())? != PointOrder::After;
            if begins_before_end
                && ends_after_start
                && (allow_partial_containment.unwrap_or(false) || fully)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

pub(crate) fn constructors(ctx: &mut Ctx) -> [(&'static str, Value); 4] {
    [
        ("AbstractRange", ctx.class_constructor::<DomAbstractRange>()),
        ("Range", ctx.class_constructor::<DomRange>()),
        ("StaticRange", ctx.class_constructor::<DomStaticRange>()),
        ("Selection", ctx.class_constructor::<DomSelection>()),
    ]
}

pub(crate) fn install_node_filter(ctx: &mut Ctx) -> OpResult<()> {
    let object = Value::Obj(ctx.new_object());
    for (name, value) in [
        ("FILTER_ACCEPT", 1),
        ("FILTER_REJECT", 2),
        ("FILTER_SKIP", 3),
        ("SHOW_ALL", u32::MAX as i64),
        ("SHOW_ELEMENT", 1),
        ("SHOW_ATTRIBUTE", 2),
        ("SHOW_TEXT", 4),
        ("SHOW_CDATA_SECTION", 8),
        ("SHOW_ENTITY_REFERENCE", 16),
        ("SHOW_ENTITY", 32),
        ("SHOW_PROCESSING_INSTRUCTION", 64),
        ("SHOW_COMMENT", 128),
        ("SHOW_DOCUMENT", 256),
        ("SHOW_DOCUMENT_TYPE", 512),
        ("SHOW_DOCUMENT_FRAGMENT", 1024),
        ("SHOW_NOTATION", 2048),
    ] {
        ctx.set_member(&object, name, Value::Num(value as f64))
            .map_err(|_| OpError::new("TypeError", "could not define NodeFilter constant"))?;
    }
    let global = ctx.global_object();
    ctx.set_member(&global, "NodeFilter", object)
        .map_err(|_| OpError::new("TypeError", "could not install NodeFilter"))
}

pub(crate) fn install_range_constants(ctx: &mut Ctx) -> OpResult<()> {
    let global = ctx.global_object();
    let constructor = ctx
        .get_member(&global, "Range")
        .map_err(|_| OpError::new("TypeError", "Range constructor is unavailable"))?;
    for (name, value) in [
        ("START_TO_START", 0),
        ("START_TO_END", 1),
        ("END_TO_END", 2),
        ("END_TO_START", 3),
    ] {
        ctx.set_member(&constructor, name, Value::Num(value as f64))
            .map_err(|_| OpError::new("TypeError", "could not define Range constant"))?;
    }
    Ok(())
}

/// Shared mutation hook; the realm's observer sink must call this alongside
/// MutationObserver delivery. Boundary adjustment is deliberately centralized.
pub(crate) fn adjust_ranges(
    registry: &RangeRegistry,
    document: &lumen_html::Document,
    mutation: &lumen_html::observe::ObservedMutation,
    moving: Option<(NodeId, usize)>,
) {
    use lumen_html::observe::ObservedKind;
    let mut ranges = registry.ranges.borrow_mut();
    ranges.retain(|entry| entry.strong_count() > 0);
    for range in ranges.iter().filter_map(std::rc::Weak::upgrade) {
        for (start, point) in [(true, &range.start), (false, &range.end)] {
            let mut boundary = point.get();
            match &mutation.kind {
                ObservedKind::CharacterData { offset, removed, inserted, .. }
                    if boundary.container == mutation.target =>
                {
                    if boundary.offset > *offset && boundary.offset <= offset + removed {
                        boundary.offset = *offset;
                    } else if boundary.offset > offset + removed {
                        boundary.offset = boundary.offset - removed + inserted;
                    }
                }
                ObservedKind::TextSplit { new_node, offset, parent, index } => {
                    if boundary.container == mutation.target && boundary.offset > *offset {
                        boundary.container = *new_node;
                        boundary.offset -= offset;
                    } else if boundary.container == *parent && boundary.offset == index + 1 {
                        boundary.offset += 1;
                    }
                }
                ObservedKind::TextMerge { destination, offset, parent, index } => {
                    if boundary.container == mutation.target {
                        boundary.container = *destination;
                        boundary.offset += offset;
                    } else if boundary.container == *parent && boundary.offset == *index {
                        boundary.container = *destination;
                        boundary.offset = *offset;
                    }
                }
                ObservedKind::ChildList {
                    added,
                    removed,
                    previous_sibling,
                    next_sibling,
                } => {
                    if let Some(removed) = removed {
                        let index = moving.filter(|(root, _)| root == removed).map(|(_, index)| index).or_else(|| next_sibling
                            .and_then(|next| child_index(document, next).ok())
                            .or_else(|| {
                                previous_sibling.and_then(|prev| {
                                    child_index(document, prev).ok().map(|i| i + 1)
                                })
                            })
                            ).unwrap_or(0);
                        if boundary.container != mutation.target
                            && is_in_subtree(document, boundary.container, *removed)
                        {
                            boundary = Boundary {
                                container: mutation.target,
                                offset: index,
                            };
                        } else if boundary.container == mutation.target && boundary.offset > index {
                            boundary.offset -= 1;
                        }
                    }
                    if boundary.container == mutation.target {
                        if let Some(added) = added {
                            if let Ok(index) = child_index(document, *added) {
                                if boundary.offset > index {
                                    boundary.offset += 1;
                                }
                            }
                        }
                    }
                }
                ObservedKind::ChildListMany { added, removed } => {
                    if boundary.container != mutation.target
                        && removed
                            .iter()
                            .any(|root| is_in_subtree(document, boundary.container, *root))
                    {
                        boundary = Boundary {
                            container: mutation.target,
                            offset: 0,
                        };
                    } else if boundary.container == mutation.target && !removed.is_empty() {
                        boundary.offset = 0;
                    }
                    for added in added {
                        if boundary.container == mutation.target {
                            if let Ok(index) = child_index(document, *added) {
                                if boundary.offset > index {
                                    boundary.offset += 1;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            range.set_in_document(boundary, start, document);
        }
    }
}

pub(crate) fn sync_selection(data: &SelectionData) {
    let range = data.ranges.borrow().first().cloned();
    if let Some(range) = range {
        if data.backward.get() == Some(true) {
            data.anchor.set(Some(range.end.get()));
            data.focus.set(Some(range.start.get()));
        } else {
            data.anchor.set(Some(range.start.get()));
            data.focus.set(Some(range.end.get()));
        }
    } else {
        data.anchor.set(None);
        data.focus.set(None);
        data.backward.set(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    #[test]
    fn specification_range_boundaries_queries_and_cross_document_ownership() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<!doctype html><main>abcdef</main>", 256).unwrap();
        assert!(evaluates_true(&mut engine, r#"
          function check(v, label) { if (!v) throw new Error(label); }
          function error(name, f) { try { f(); } catch (e) { check(e.name === name, name + ':' + e.name); return; } throw new Error('missing ' + name); }
          const text = document.querySelector('main').firstChild;
          const a = new Range(), b = new Range();
          a.setStart(text, 1); a.setEnd(text, 3); b.setStart(text, 2); b.setEnd(text, 4);
          check(a.compareBoundaryPoints(1, b) === 1 && a.compareBoundaryPoints(3, b) === -1, 'endpoint pairs');
          check(a.compareBoundaryPoints(65536, b) === -1, 'unsigned short modulo');
          check(a.comparePoint(text, 0) === -1 && a.comparePoint(text, 2) === 0 && a.comparePoint(text, 4) === 1, 'point ordering');
          check(a.isPointInRange(text, 1) && a.isPointInRange(text, 3) && !a.isPointInRange(text, 4), 'inclusive endpoints');
          const detached = document.createElement('div');
          b.selectNodeContents(detached);
          error('NotSupportedError', () => a.compareBoundaryPoints(256, b));
          error('WrongDocumentError', () => a.compareBoundaryPoints(0, b));
          error('WrongDocumentError', () => a.comparePoint(detached, 100));
          check(!a.isPointInRange(detached, 100), 'different root before validation');
          error('InvalidNodeTypeError', () => a.setStart(document.doctype, 100));
          const foreign = document.implementation.createHTMLDocument('other'), other = foreign.createTextNode('xyz');
          foreign.body.appendChild(other); a.setStart(other, 2);
          check(a.collapsed && a.startContainer === other && a.endContainer === other && a.startOffset === 2, 'foreign boundary relocation');
          other.insertData(0, '0'); check(a.startOffset === 3 && a.endOffset === 3, 'target registry updates');
          const attr = document.createAttribute('x'); attr.value = 'nonempty'; a.selectNodeContents(attr);
          check(a.collapsed && a.startContainer === attr && a.comparePoint(attr, 0) === 0 && a.intersectsNode(attr), 'attribute root');
          error('IndexSizeError', () => a.setEnd(attr, 1));
          true
        "#));
    }

    #[test]
    fn specification_range_character_data_mutations_and_retention() {
        let mut engine = Engine::new();
        let realm = super::super::install(engine.ctx(), "<main>aaaaaa</main>", 256).unwrap();
        let text = selector::query_selector(realm.session.borrow().document(), realm.session.borrow().document().root(), "main").unwrap().unwrap();
        let text = realm.session.borrow().document().first_child(text).unwrap().unwrap();
        assert!(evaluates_true(&mut engine, r#"
          function check(v, label) { if (!v) throw new Error(label); }
          const text = document.querySelector('main').firstChild, range = document.createRange();
          const observer = new MutationObserver(() => {}); observer.observe(text, { characterData: true, characterDataOldValue: true });
          range.setStart(text, 2); range.setEnd(text, 5);
          text.replaceData(1, 2, 'aa');
          check(range.startOffset === 1 && range.endOffset === 5, 'authored equal-value interval');
          let records = observer.takeRecords(); check(records.length === 1 && records[0].oldValue === 'aaaaaa', 'equal-value record');
          text.insertData(1, 'X'); check(range.startOffset === 1 && range.endOffset === 6, 'insertion endpoint affinity');
          text.deleteData(2, 999); check(range.endOffset === 2, 'clamped deletion interval');
          observer.disconnect(); observer.takeRecords();
          range.selectNodeContents(text); text.remove();
          globalThis.__retentionRange = range;
          check(range.startContainer === document.querySelector('main') && range.collapsed, 'removal relocation');
          true
        "#));
        engine.collect_garbage();
        assert!(evaluates_true(&mut engine, "__retentionRange.startContainer === document.querySelector('main') && __retentionRange.collapsed"));
        assert!(!realm.retained_nodes.borrow().contains_key(&text), "removed endpoint lease was released");
    }

    #[test]
    fn specification_range_split_normalize_and_selection_identity() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main>abcdef</main>", 256).unwrap();
        assert!(evaluates_true(&mut engine, r#"
          function check(v, label) { if (!v) throw new Error(label); }
          const main = document.querySelector('main'), text = main.firstChild, range = document.createRange(), edge = new Range();
          range.setStart(text, 4); range.setEnd(text, 6); edge.setStart(main, 1); edge.collapse(true);
          const observer = new MutationObserver(() => {}); observer.observe(main, { subtree: true, characterData: true, childList: true });
          const tail = text.splitText(3);
          check(text.data === 'abc' && tail.data === 'def' && range.startContainer === tail && range.startOffset === 1 && range.endOffset === 3, 'split endpoints');
          check(edge.startOffset === 2 && observer.takeRecords().length === 2, 'split parent phase and observer visibility');
          main.normalize(); check(main.childNodes.length === 1 && text.data === 'abcdef' && range.startContainer === text && range.startOffset === 4, 'normalize relocation');
          const selection = document.getSelection(); selection.addRange(range);
          check(selection.getRangeAt(0) === range && selection.getRangeAt(0) === selection.getRangeAt(0), 'reference identity');
          range.setStart(text, 2); check(selection.anchorNode === text && selection.anchorOffset === 2, 'script range mutation is live');
          selection.setBaseAndExtent(text, 5, text, 2); const backward = selection.getRangeAt(0); backward.collapse(true);
          check(selection.direction === 'backward', 'script collapse preserves stored selection direction');
          selection.removeAllRanges(); selection.addRange(range);
          selection.collapseToStart(); check(selection.getRangeAt(0) !== range && range.endOffset === 6, 'collapse replaces range');
          const before = selection.getRangeAt(0); selection.extend(text, 5);
          check(selection.getRangeAt(0) !== before && before.collapsed && selection.focusOffset === 5, 'extend replaces range');
          const foreign = document.implementation.createHTMLDocument('other'), other = foreign.createTextNode('foreign'); foreign.body.appendChild(other);
          const associated = selection.getRangeAt(0); associated.selectNodeContents(other);
          check(selection.rangeCount === 0 && selection.anchorNode === null && associated.startContainer === other, 'donor Selection policy retains associated Range');
          associated.selectNodeContents(text); check(selection.getRangeAt(0) === associated, 'association survives cross-document relocation');
          const surrogate = document.createTextNode('A😀B'); main.appendChild(surrogate); const half = surrogate.splitText(2);
          check(surrogate.length === 2 && half.length === 2 && surrogate.data.charCodeAt(1) === 0xD83D && half.data.charCodeAt(0) === 0xDE00, 'shared UTF16 surrogate split');
          associated.marker = 123; globalThis.__rangeGCSelection = selection;
          true
        "#));
        engine.collect_garbage();
        assert!(evaluates_true(&mut engine, "globalThis.__rangeGCSelection.getRangeAt(0).marker === 123 && globalThis.__rangeGCSelection.getRangeAt(0).startContainer === document.querySelector('main').firstChild"));
    }

    #[test]
    fn specification_range_selection_ownership_reclaims_without_cycles() {
        let weak = {
            let mut engine = Engine::new();
            let realm = super::super::install(engine.ctx(), "<main>abcdef</main>", 64).unwrap();
            assert!(evaluates_true(&mut engine, "const range=document.createRange(); range.selectNodeContents(document.querySelector('main')); document.getSelection().addRange(range); document.getSelection().getRangeAt(0)===range"));
            Rc::downgrade(&realm)
        };
        assert!(weak.upgrade().is_none(), "Selection/Range native ownership must not form an Rc cycle");
    }

    #[test]
    fn specification_range_surround_contents_error_order_and_native_primitives() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main><b>abc</b><i>DEF</i><u>ghi</u></main>", 256).unwrap();
        assert!(evaluates_true(&mut engine, r#"
          function check(v, label) { if (!v) throw new Error(label); }
          function error(name, f) { try { f(); } catch (e) { check(e.name === name, name + ':' + e.name); return; } throw new Error('missing ' + name); }
          const main = document.querySelector('main'), first = main.firstChild.firstChild, last = main.lastChild.firstChild, middle = main.children[1];
          const range = document.createRange(); range.setStart(first, 1); range.setEnd(last, 2);
          error('InvalidStateError', () => range.surroundContents(document));
          const fragment = range.extractContents();
          check(fragment.firstChild.outerHTML + fragment.firstChild.nextSibling.outerHTML + fragment.lastChild.outerHTML === '<b>bc</b><i>DEF</i><u>gh</u>' && fragment.children[1] === middle, 'extract original contained identity');
          check(main.innerHTML === '<b>a</b><u>i</u>' && range.startContainer === main && range.startOffset === 1 && range.collapsed, 'extract collapse point');
          const text = first, wrapper = document.createElement('span'); wrapper.appendChild(document.createElement('old'));
          range.selectNodeContents(text); range.surroundContents(wrapper);
          check(wrapper.textContent === 'a' && wrapper.children.length === 0 && range.startContainer === main.firstChild && range.toString() === 'a', 'surround composition');
          const comment = document.createComment('abc'); main.appendChild(comment); range.setStart(comment, 1); range.collapse(true);
          error('HierarchyRequestError', () => range.insertNode(document.createElement('bad'))); check(comment.data === 'abc', 'comment insertion has no mutation');
          range.setStart(last, 0); range.collapse(true); const unchanged = last.data;
          error('HierarchyRequestError', () => range.insertNode(main)); check(last.data === unchanged, 'validate insertion before splitting');
          const inserted = document.createElement('em'); range.insertNode(inserted);
          check(!range.collapsed && range.endContainer === inserted.parentNode, 'collapsed insertion advances end');
          true
        "#));
    }

    #[test]
    fn specification_range_document_position_and_self_replacement() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main a='1' b='2'><i>abc</i><b></b></main>", 256).unwrap();
        assert!(evaluates_true(&mut engine, r#"
          const check=(v,m)=>{if(!v)throw new Error(m)};
          const main=document.querySelector('main'), i=main.firstChild, b=main.lastChild, text=i.firstChild;
          check(main.compareDocumentPosition(main)===0 && main.compareDocumentPosition(i)===20 && i.compareDocumentPosition(main)===10, 'containment masks');
          check(i.compareDocumentPosition(b)===4 && b.compareDocumentPosition(i)===2, 'sibling order');
          const a=main.getAttributeNode('a'), attrB=main.getAttributeNode('b');
          check(a.compareDocumentPosition(attrB)===36 && attrB.compareDocumentPosition(a)===34, 'ordered attributes');
          check(a.compareDocumentPosition(main)===10 && main.compareDocumentPosition(a)===20, 'attribute owner');
          check(a.compareDocumentPosition(text)===4 && text.compareDocumentPosition(a)===2, 'attribute precedes descendants');
          const detached=document.createElement('div'), detachedAttr=document.createAttribute('x');
          for(const node of [detached,detachedAttr,new DOMParser().parseFromString('<p>x</p>','text/html')]) {
            const forward=main.compareDocumentPosition(node), reverse=node.compareDocumentPosition(main);
            check((forward&33)===33 && (reverse&33)===33 && (forward&6)!==(reverse&6) && forward===main.compareDocumentPosition(node), 'stable disconnected order');
          }
          let type=false;try{main.compareDocumentPosition({})}catch(e){type=e.name==='TypeError'}check(type,'typed node');
          const range=document.createRange();range.setStart(text,1);range.setEnd(text,2);
          const observer=new MutationObserver(()=>{});observer.observe(main,{childList:true});
          check(main.replaceChild(i,i)===i && main.firstChild===i, 'self replacement identity');
          check(range.startContainer===main && range.startOffset===0 && range.collapsed,'self replacement live range');
          const records=observer.takeRecords();
          check(records.length===2 && records[0].removedNodes[0]===i && records[1].addedNodes[0]===i && records[1].removedNodes.length===0,'self replacement phases');
          observer.disconnect();true
        "#));
    }

    #[test]
    fn specification_range_foreign_surround_uses_original_identity() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main>abcdef</main>", 256).unwrap();
        assert!(evaluates_true(&mut engine, r#"
          const check=(v,m)=>{if(!v)throw new Error(m)};
          const donor=document.implementation.createHTMLDocument('donor'), parent=donor.createElement('em');
          parent.textContent='discard';donor.body.appendChild(parent);parent.marker=42;
          const main=document.querySelector('main'), text=main.firstChild, range=document.createRange();
          range.setStart(text,1);range.setEnd(text,4);range.surroundContents(parent);
          check(parent===main.childNodes[1] && parent.marker===42 && parent.ownerDocument===document && parent.textContent==='bcd','foreign surround identity/content');
          check(range.startContainer===main && range.startOffset===1 && range.endOffset===2 && range.toString()==='bcd','foreign surround boundaries');
          const foreignText=donor.createTextNode('x');range.collapse(true);
          let hierarchy=false;try{range.surroundContents(foreignText)}catch(e){hierarchy=e.name==='HierarchyRequestError'}
          check(hierarchy,'foreign CharacterData parent hierarchy');true
        "#));
    }

    #[test]
    fn specification_variadic_conversion_identity_and_failure_phases() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 512).unwrap();
        let collect = engine.ctx().new_native_fn("__variadicCollect", 0, Rc::new(|ctx, _, _| {
            ctx.collect_garbage();
            Ok(Value::Undefined)
        }));
        let global = engine.ctx().global_object();
        engine.ctx().set_member(&global, "__variadicCollect", collect).ok().expect("install variadic collection callback");
        assert!(evaluates_true(&mut engine, r#"
          const check=(v,m)=>{if(!v)throw new Error(m)}, error=(name,fn)=>{let caught=false;try{fn()}catch(e){caught=e.name===name}check(caught,name)};
          const main=document.querySelector('main'), donor=document.implementation.createHTMLDocument('donor'), foreign=donor.createElement('i');
          donor.body.appendChild(foreign);foreign.marker=7;
          const detached=document.createElement('aside');let conversions=0;
          detached.before(foreign,{toString(){conversions++;return 'x'}});
          check(conversions===1 && foreign.parentNode===donor.body && foreign.ownerDocument===donor,'parentless conversion without node mutation');
          const abrupt={};try{main.append(foreign,{toString(){throw abrupt}})}catch(e){check(e===abrupt,'abrupt identity')}
          check(foreign.ownerDocument===donor && foreign.parentNode===donor.body && main.firstChild===null,'coercion precedes mutation');
          error('TypeError',()=>main.append(Symbol('x')));
          main.append(foreign,{toString(){document.adoptNode(foreign);return 'tail'}});
          check(main.firstChild===foreign && foreign.marker===7 && foreign.ownerDocument===document && main.lastChild.data==='tail','later conversion migrates prior node');
          globalThis.__variadicEarlier=donor.createElement('u');__variadicEarlier.marker=43;donor.body.appendChild(__variadicEarlier);
          main.replaceChildren(__variadicEarlier,{toString(){document.adoptNode(__variadicEarlier);__variadicEarlier.remove();__variadicEarlier=null;__variadicCollect();return 'ok'}});
          check(main.firstChild.marker===43 && main.firstChild.ownerDocument===document && main.lastChild.data==='ok','argument node survives coercion adoption removal and GC');
          const abruptNode=donor.createElement('q');donor.body.appendChild(abruptNode);
          let observedThrow=false;try{main.append(abruptNode,{toString(){document.adoptNode(abruptNode);throw abrupt}})}catch(e){observedThrow=e===abrupt}
          check(observedThrow && abruptNode.ownerDocument===document && abruptNode.parentNode===null && main.firstChild.marker===43,'conversion side effects survive abrupt completion');
          main.replaceChildren(null,undefined,12,true);check(main.textContent==='nullundefined12true','DOMString conversion');
          const destination=document.implementation.createHTMLDocument('target'), a=donor.createElement('a'), b=donor.createElement('b');
          donor.body.append(a,b);
          error('HierarchyRequestError',()=>destination.append(a));
          check(a.ownerDocument===donor && a.parentNode===donor.body,'singleton validates before adoption');
          error('HierarchyRequestError',()=>destination.replaceChildren(a,b));
          check(a.ownerDocument===destination && b.ownerDocument===destination && a.parentNode===b.parentNode && a.parentNode.nodeType===11,'multi-input conversion precedes destination failure');
          check(destination.documentElement!==null && donor.body.firstChild===null,'destination preserved and donor mutations authored');
          true
        "#));
    }

    #[test]
    fn specification_variadic_viable_siblings_and_aggregate_observers() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main><a></a><b></b><c></c><d></d></main>", 512).unwrap();
        assert!(evaluates_true(&mut engine, r#"
          const check=(v,m)=>{if(!v)throw new Error(m)}, names=n=>Array.from(n.childNodes,x=>x.nodeName).join(',');
          const main=document.querySelector('main'), a=main.childNodes[0], b=main.childNodes[1], c=main.childNodes[2], d=main.childNodes[3];
          c.before(b,c,a,b);check(names(main)==='C,A,B,D','before viable previous and duplicate last occurrence');
          c.after(c,d,b);check(names(main)==='C,D,B,A','after viable next and moving self');
          c.replaceWith(c,a,c);check(names(main)==='A,C,D,B' && c.parentNode===main,'replaceWith moving self');
          main.prepend(b,a);check(names(main)==='B,A,C,D','prepend reference after fragment conversion');
          const fragment=document.createDocumentFragment(), x=document.createElement('x'), y=document.createElement('y');fragment.append(x,y);
          const observer=new MutationObserver(()=>{}), sourceObserver=new MutationObserver(()=>{});
          observer.observe(main,{childList:true});sourceObserver.observe(fragment,{childList:true});
          main.append(fragment);
          const records=observer.takeRecords(), source=sourceObserver.takeRecords();
          check(records.length===1 && records[0].addedNodes.length===2 && records[0].addedNodes[0]===x && records[0].addedNodes[1]===y && records[0].previousSibling===d && records[0].nextSibling===null,'aggregate destination insertion');
          check(source.length===1 && source[0].removedNodes.length===2 && source[0].removedNodes[0]===x && source[0].removedNodes[1]===y,'aggregate fragment removal');
          main.append(fragment);
          check(observer.takeRecords().length===0 && sourceObserver.takeRecords().length===0,'empty fragment produces no records');
          main.replaceChildren(x,y);const replacement=observer.takeRecords();
          check(replacement.length===3 && replacement[2].addedNodes.length===2 && replacement[2].removedNodes.length===4,'conversion removals precede aggregate replace-all');
          main.replaceChildren(x);const singleton=observer.takeRecords();
          check(singleton.length===1 && singleton[0].removedNodes.length===2 && singleton[0].removedNodes[0]===x && singleton[0].removedNodes[1]===y && singleton[0].addedNodes[0]===x,'replace-all folds existing child into aggregate removal');
          main.replaceChildren(x);const self=observer.takeRecords();
          check(self.length===1 && self[0].removedNodes[0]===x && self[0].addedNodes[0]===x,'replace-all singleton self replacement');
          const donor=document.implementation.createHTMLDocument('donor');let error;
          try{document.replaceChildren(donor)}catch(e){error=e}
          check(error && error.name==='HierarchyRequestError' && donor.documentElement.parentNode===donor,'replace-all hierarchy precedes foreign adoption');
          observer.disconnect();sourceObserver.disconnect();true
        "#));
    }

    #[test]
    fn specification_range_lease_release_defers_reap_during_document_read() {
        let mut engine = Engine::new();
        let realm = super::super::install(engine.ctx(), "<main></main>", 128).unwrap();
        let node = realm.session.borrow_mut().document_mut().create(NodeKind::Text("lease".into())).unwrap();
        let keep = super::super::NodeRetention::new(&realm, node);
        let session = realm.session.borrow();
        drop(keep);
        assert!(session.document().kind(node).is_ok());
        drop(session);
        realm.reap_detached([node]);
        assert!(realm.session.borrow().document().kind(node).is_err());
    }

    #[test]
    fn specification_variadic_template_clone_script_post_connection() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main><div></div><b></b></main><template><span>New </span><script>document.querySelector('b').remove();</script><span>content</span></template>", 512).unwrap();
        assert!(evaluates_true(&mut engine, r#"
          const main=document.querySelector('main'), template=document.querySelector('template');
          const original=template.content.querySelector('script');
          const copy=template.content.cloneNode(true), script=copy.querySelector('script');
          main.firstChild.replaceWith(copy);
          if(main.querySelector('b')!==null)throw new Error('cloned active-parser template script did not execute');
          if(original.parentNode!==template.content || script.parentNode!==main)throw new Error('template clone identity');
          script.remove();main.innerHTML==='<span>New </span><span>content</span>'
        "#));
    }

    #[test]
    fn specification_variadic_selectedness_and_reaction_ownership() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 512).unwrap();
        assert!(evaluates_true(&mut engine, r#"
          const check=(v,m)=>{if(!v)throw new Error(m)}, main=document.querySelector('main');
          const donor=document.implementation.createHTMLDocument('donor'), source=donor.createElement('select'), target=document.createElement('select');
          const a=donor.createElement('option'), b=donor.createElement('option'), c=document.createElement('option');
          a.value='a';b.value='b';c.value='c';source.append(a,b);a.selected=true;target.append(c);main.append(target);donor.body.append(source);
          target.append(a,'tail');
          check(source.value==='b' && target.value==='a' && target.options[1]===a && a.ownerDocument===document,'foreign option ownership through intermediate fragment');
          const log=[];customElements.define('x-variadic-ownership',class extends HTMLElement {
            connectedCallback(){log.push('connected')}
            disconnectedCallback(){log.push('disconnected')}
            adoptedCallback(){log.push('adopted')}
          });
          const custom=document.createElement('x-variadic-ownership');donor.body.appendChild(custom);log.length=0;
          main.append(custom,'text');
          check(custom.parentNode===main && custom.customElementRegistry===customElements && log.join(',')==='disconnected,adopted,connected','reaction owner and ordering through foreign fragment conversion');
          true
        "#));
    }

    #[test]
    fn specification_clone_import_conversion_errors_and_document_metadata() {
        let mut engine = Engine::new();
        let realm = super::super::install(engine.ctx(), "<main><i>content</i></main>", 256).unwrap();
        assert!(realm.set_document_encoding("windows-1252"));
        let collect = engine.ctx().new_native_fn("__cloneCollect", 0, Rc::new(|ctx, _, _| { ctx.collect_garbage(); Ok(Value::Undefined) }));
        let global = engine.ctx().global_object();
        engine.ctx().set_member(&global, "__cloneCollect", collect).ok().expect("install clone GC");
        assert!(evaluates_true(&mut engine, r#"
          const check=(v,m)=>{if(!v)throw new Error(m)}, error=(name,f)=>{try{f()}catch(e){check(e.name===name,name+':'+e.name);return}throw new Error('missing '+name)};
          const source=document.querySelector('main'), donor=document.implementation.createHTMLDocument('donor'), order=[];
          const copy=document.importNode(source,{get customElementRegistry(){order.push('registry');donor.adoptNode(source);__cloneCollect();return customElements},get selfOnly(){order.push('self');return false}});
          check(order.join(',')==='registry,self' && copy.textContent==='content' && copy.ownerDocument===document && source.ownerDocument===donor,'owned import options and reprojection');
          check(document.importNode(source,null).textContent==='content' && document.importNode(source).childNodes.length===0,'null dictionary and missing boolean default');
          const abrupt={marker:1};let caught;try{document.importNode(source,{get selfOnly(){throw abrupt}})}catch(e){caught=e}check(caught===abrupt,'conversion abrupt identity');
          const host=document.createElement('div'), shadow=host.attachShadow({mode:'open'});
          error('NotSupportedError',()=>shadow.cloneNode());error('NotSupportedError',()=>document.importNode(shadow));error('HierarchyRequestError',()=>document.adoptNode(shadow));error('NotSupportedError',()=>document.adoptNode(donor));
          const clone=document.cloneNode(true);check(clone.characterSet==='windows-1252' && clone.URL===document.URL && clone.contentType===document.contentType && clone.compatMode===document.compatMode,'document metadata');true
        "#));
    }

    #[test]
    fn specification_clone_foreign_fragment_owner_observers_and_post_connection() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main><b>old</b></main>", 512).unwrap();
        assert!(evaluates_true(&mut engine, r#"
          const check=(v,m)=>{if(!v)throw new Error(m)}, main=document.querySelector('main'), donor=document.implementation.createHTMLDocument('donor'), fragment=donor.createDocumentFragment();
          const a=donor.createElement('i'), b=donor.createTextNode('tail');a.marker=77;fragment.append(a,b);
          const observer=new MutationObserver(()=>{});observer.observe(fragment,{childList:true});observer.observe(main,{childList:true});
          const range=donor.createRange();range.setStart(fragment,1);range.setEnd(fragment,2);
          check(main.appendChild(fragment)===fragment && fragment.ownerDocument===donor && fragment.childNodes.length===0 && main.childNodes[1]===a && a.ownerDocument===document && a.marker===77,'foreign fragment children-only identity');
          let records=observer.takeRecords();check(records.length===2 && records[0].target===fragment && records[0].removedNodes[0]===a && records[0].removedNodes[1]===b && records[1].target===main && records[1].addedNodes.length===2,'aggregate source/destination records');
          check(range.startContainer===fragment && range.startOffset===0 && range.endOffset===0,'source fragment range removals');
          main.appendChild(fragment);check(fragment.ownerDocument===donor && observer.takeRecords().length===0,'empty fragment no adoption or record');
          fragment.append(a,b);observer.takeRecords();main.replaceChildren(fragment);records=observer.takeRecords();
          check(fragment.ownerDocument===donor && records.length===2 && records[0].target===fragment && records[1].target===main && records[1].removedNodes.length===1 && records[1].addedNodes.length===2,'replace-all fragment phases');
          const template=donor.createElement('template'), script=donor.createElement('script');script.textContent='globalThis.__fragmentAtomic=document.querySelector("main").lastChild.localName';template.content.append(script,donor.createElement('em'));
          const content=template.content, owner=content.ownerDocument;main.replaceChildren(content);
          check(content.ownerDocument===owner && __fragmentAtomic==='em','template fragment owner and full batch before script');observer.disconnect();true
        "#));
    }

    #[test]
    fn specification_template_cross_owner_identity_clone_markup_and_cycles() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 512).unwrap();
        assert!(evaluates_true(&mut engine, r#"
          const check=(v,m)=>{if(!v)throw new Error(m)}, template=document.createElement('template');
          template.innerHTML='<i data-marker="one">text</i><template><b>nested</b></template>';
          const content=template.content, child=content.firstChild, nested=content.lastChild, originalOwner=content.ownerDocument;
          const donor=document.implementation.createHTMLDocument('donor');
          check(donor.adoptNode(content)===content && template.content===content && content.firstChild===child && child.ownerDocument===donor && content.ownerDocument===donor,'direct fragment adoption preserves fragment, children and host association');
          check(content.parentNode===null && content.getRootNode()===content && !content.isConnected && !('host' in content),'internal host is not a parent or authored property');
          check(template.innerHTML.indexOf('nested')>=0 && new XMLSerializer().serializeToString(template).indexOf('nested')>=0,'shared graph serialization');
          const clone=template.cloneNode(true), imported=donor.importNode(template,true);
          check(clone.content!==content && clone.content.firstChild!==child && clone.innerHTML===template.innerHTML && imported.innerHTML===template.innerHTML,'deep clone across real owners');
          let cycle=false;try{content.appendChild(template)}catch(e){cycle=e.name==='HierarchyRequestError'}check(cycle && template.parentNode===null && template.ownerDocument===document && content.ownerDocument===donor && content.firstChild===child,'host-inclusive cycle prevalidation preserves all logical owners');
          template.innerHTML='<em>replacement</em>';check(template.content===content,'markup preserves associated fragment identity');check(content.firstChild.localName==='em','markup replaces foreign content children');check(content.ownerDocument===donor && content.firstChild.ownerDocument===donor,'markup preserves foreign content logical owner');
          content.firstChild.setAttribute('onclick','this.setAttribute("clicked","yes")');
          const third=document.implementation.createHTMLDocument('third');third.adoptNode(template);
          check(template.content===content && content.ownerDocument!==donor && content.ownerDocument!==third && content.firstChild.ownerDocument===content.ownerDocument,'host adoption readopts existing content into destination inert owner');
          check(content.firstChild.getAttribute('onclick')==='this.setAttribute("clicked","yes")','foreign handler initialization reads the actual content arena before readoption');
          const empty=document.createElement('template'), emptyContent=empty.content;donor.adoptNode(emptyContent);third.adoptNode(empty);check(empty.content===emptyContent && emptyContent.ownerDocument===content.ownerDocument,'empty foreign associated content follows the same readoption phases');
          check(nested.content.firstChild.textContent==='nested' && originalOwner!==content.ownerDocument,'retained detached nested identity');
          const xml=new DOMParser().parseFromString('<root/>','application/xml'), xt=xml.createElementNS('http://www.w3.org/1999/xhtml','template');
          check(xt.content.ownerDocument!==xml && xt.content.ownerDocument.contentType==='application/xml','XML document gets distinct XML inert owner');const inertXml=xt.content.ownerDocument, mixed=inertXml.createElement('Mixed'), cdata=inertXml.createCDATASection('data');check(mixed.localName==='Mixed' && mixed.namespaceURI===null && cdata.ownerDocument===inertXml,'XML inert factories preserve XML name, namespace and CDATA semantics');true
        "#));
    }

    #[test]
    fn specification_adoption_alias_identity_survives_intermediate_document_gc() {
        let mut engine = Engine::new();
        let realm = super::super::install(engine.ctx(), "<main></main>", 128).unwrap();
        assert!(evaluates_true(&mut engine, "globalThis.__roundTrip=document.createElement('p');__roundTrip.textContent='retained';true"));
        let value = engine.eval_value("__roundTrip").ok().and_then(Result::ok).unwrap();
        let original = engine.ctx().with_instance::<DomNode, _>(&value, |node| node.id).ok().unwrap();
        drop(value);
        assert!(evaluates_true(&mut engine, "(()=>{const intermediate=document.implementation.createHTMLDocument('intermediate');intermediate.adoptNode(__roundTrip);document.adoptNode(__roundTrip);document.body.append(__roundTrip)})();true"));
        engine.collect_garbage();
        let (owner, current) = realm.resolve_adopted_node(original);
        assert!(Rc::ptr_eq(&owner, &realm));
        assert!(matches!(owner.session.borrow().document().kind(current), Ok(NodeKind::Element { name, .. }) if name.as_str() == "p"));
        let value = engine.eval_value("__roundTrip").ok().and_then(Result::ok).unwrap();
        assert_eq!(engine.ctx().with_instance::<DomNode, _>(&value, |node| node.id).ok().unwrap(), current);
    }

    #[test]
    fn specification_template_graph_gc_retains_both_endpoints_without_rc_cycles() {
        let mut engine = Engine::new();
        let realm = super::super::install(engine.ctx(), "<main></main>", 128).unwrap();
        assert!(evaluates_true(&mut engine, r#"
          const t=document.createElement('template'), donor=document.implementation.createHTMLDocument('donor');
          t.innerHTML='<i>retained</i>';t.marker=42;donor.adoptNode(t.content);
          globalThis.__heldTemplate=t;globalThis.__heldContent=t.content;true
        "#));
        let value = engine.eval_value("__heldTemplate").ok().and_then(Result::ok).expect("host value");
        let id = engine.ctx().with_instance::<DomNode, _>(&value, |node| node.id).ok().expect("host identity");
        let weak_wrapper = engine.ctx().weak_value(&value).expect("host weak identity");
        drop(value);
        assert!(evaluates_true(&mut engine, "__heldTemplate=null;true"));
        engine.collect_garbage();
        assert!(weak_wrapper.upgrade().is_some(), "retained content traces its internal associated host identity");
        assert!(realm.session.borrow().document().kind(id).is_ok());
        assert!(evaluates_true(&mut engine, "__heldContent.firstChild.textContent==='retained'"));
        assert!(evaluates_true(&mut engine, "__heldContent=null;true"));
        engine.collect_garbage();
        assert!(weak_wrapper.upgrade().is_none(), "unreachable reciprocal JS trace edges are collectible");
        realm.reap_detached_for_capacity(128);
        assert!(realm.session.borrow().document().kind(id).is_err(), "detached host is reclaimed after both endpoints die");
    }

    #[test]
    fn specification_template_graph_aggregate_preflight_is_atomic() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(evaluates_true(&mut engine, r#"
          const check=(v,m)=>{if(!v)throw new Error(m)}, donor=document.implementation.createHTMLDocument('donor'), other=document.implementation.createHTMLDocument('content'), t=donor.createElement('template');
          for(let i=0;i<80;i++)t.content.appendChild(donor.createTextNode('x'));
          donor.body.appendChild(t);const content=t.content;other.adoptNode(content);
          let failure;try{document.adoptNode(t)}catch(e){failure=e}
          check(failure && failure.name==='QuotaExceededError' && t.ownerDocument===donor && t.parentNode===donor.body && t.content===content && content.ownerDocument===other && content.childNodes.length===80,'aggregate graph admission precedes any adoption mutation');
          let cloneFailure;try{document.importNode(t,true)}catch(e){cloneFailure=e}
          check(cloneFailure && cloneFailure.name==='QuotaExceededError' && t.content===content && content.childNodes.length===80,'aggregate graph clone admission leaves source intact');true
        "#));
    }

    #[test]
    fn specification_native_allocation_pressure_collects_unreachable_nodes_and_preserves_leases() {
        let mut engine = Engine::new();
        let realm = super::super::install(engine.ctx(), "<main></main>", 512).unwrap();
        realm.frame_contexts(engine.ctx()).expect("register actual browsing-context admission");
        realm.set_frame_resource_limits(crate::FrameResourceLimits { max_document_nodes: 64, ..crate::FrameResourceLimits::default() }).expect("configure shared native budget");
        assert!(evaluates_true(&mut engine, r#"
          const check=(v,m)=>{if(!v)throw new Error(m)}, main=document.querySelector('main');
          const endpoint=document.createTextNode('endpoint'), range=document.createRange();range.selectNodeContents(endpoint);endpoint.marker=91;
          for(let i=0;i<1200;i++){const node=document.createElement('i');node.appendChild(document.createTextNode(String(i)));main.replaceChildren(node)}
          check(main.textContent==='1199' && range.startContainer===endpoint && endpoint.marker===91,'pressure GC preserves current tree and live native endpoint');
          const xml=new Document(), node=xml.createElement('held');xml.appendChild(node);node.marker=55;globalThis.__pressureDocument=xml;
          for(let i=0;i<300;i++){const temporary=document.createComment(String(i));main.replaceChildren(temporary)}
          check(xml.firstChild===node && node.ownerDocument===xml && node.marker===55,'authored Document constructor traces actual root identity');
          const kept=[];let quota;for(let i=0;i<100;i++){try{kept.push(document.createElement('b'))}catch(e){quota=e;break}}
          check(quota && quota.name==='QuotaExceededError' && kept.every(n=>n.ownerDocument===document && n.localName==='b') && range.startContainer===endpoint,'actual retained-node quota remains enforced');true
        "#));
    }

    #[test]
    fn specification_clone_reaper_capacity_and_failed_import_are_bounded() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main><i>one</i><b>two</b></main>", 64).unwrap();
        let collect = engine.ctx().new_native_fn("__cloneCapacityCollect", 0, Rc::new(|ctx, _, _| { ctx.collect_garbage(); Ok(Value::Undefined) }));
        let global = engine.ctx().global_object();
        engine.ctx().set_member(&global, "__cloneCapacityCollect", collect).ok().expect("install clone capacity GC");
        assert!(evaluates_true(&mut engine, r#"
          const check=(v,m)=>{if(!v)throw new Error(m)}, source=document.querySelector('main');
          for(let i=0;i<160;i++){let copy=i%2?document.importNode(source,true):source.cloneNode(true);check(copy.textContent==='onetwo','clone data');copy=null;__cloneCapacityCollect()}
          const donor=document.implementation.createHTMLDocument('donor'), large=donor.createElement('div');for(let i=0;i<80;i++)large.appendChild(donor.createTextNode('x'));donor.body.appendChild(large);
          let error;try{document.importNode(large,true)}catch(e){error=e}check(error && error.name==='QuotaExceededError' && large.parentNode===donor.body && large.childNodes.length===80 && source.textContent==='onetwo','preflight failure leaves source unchanged');true
        "#));
    }

    #[test]
    fn specification_rendered_text_setters_breaks_null_and_outer_merge_phases() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main><div></div><p>left<b>old</b>right</p></main>", 256).unwrap();
        assert!(evaluates_true(&mut engine, r#"
          const check=(v,m)=>{if(!v)throw new Error(m)}, div=document.querySelector('div'), p=document.querySelector('p'), b=p.querySelector('b');
          div.innerText='a\r\nb\rc\n\n';check(div.childNodes.length===7 && div.childNodes[0].data==='a' && div.childNodes[1].localName==='br' && div.childNodes[2].data==='b' && div.childNodes[6].localName==='br','rendered fragment CRLF and empty runs');
          div.innerText=null;check(div.childNodes.length===0,'legacy null empty');div.innerText=undefined;check(div.textContent==='undefined','undefined DOMString');
          const observer=new MutationObserver(()=>{});observer.observe(p,{childList:true,subtree:true,characterData:true,characterDataOldValue:true});
          const right=p.lastChild, range=document.createRange();range.selectNodeContents(right);b.outerText='';
          check(p.childNodes.length===1 && p.firstChild.data==='leftright' && right.parentNode===null,'outer empty Text merges both adjacent Text nodes');
          const records=observer.takeRecords();check(records.length===5 && records[0].type==='childList' && records[0].removedNodes[0]===b && records[1].type==='characterData' && records[1].oldValue==='' && records[2].removedNodes[0]===right && records[3].oldValue==='left','replacement then append-data/remove phase records');
          check(range.startContainer===p && range.endContainer===p,'outer merge follows remove range behavior');
          const detached=document.createElement('div');let rejected=false;try{detached.outerText='x'}catch(e){rejected=e.name==='NoModificationAllowedError'}check(rejected,'detached outerText error');
          observer.disconnect();true
        "#));
    }

    fn evaluates_true(engine: &mut Engine, source: &str) -> bool {
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
    fn specification_selection_case_projection_shares_original_context_and_keeps_range_raw() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<p id=p style='text-transform:uppercase'>aßb</p><p id=g lang=el style='text-transform:uppercase'>Νερά<span>ιδα</span></p><p id=s style='text-transform:uppercase'>A😀B</p>", 64).unwrap();
        assert!(evaluates_true(&mut engine, r#"
            const check=(value,label)=>{if(!value)throw Error(label)};
            const selection=document.getSelection(), range=document.createRange(), text=document.querySelector('#p').firstChild;
            range.setStart(text,1);range.setEnd(text,2);selection.addRange(range);
            check(range.toString()==='ß' && selection.toString()==='SS' && text.data==='aßb','selected expansion preserves DOM and raw Range');
            const greek=document.querySelector('#g span').firstChild;
            range.setStart(greek,0);range.setEnd(greek,1);
            check(selection.toString()==='Ϊ','original accent outside selection supplies Greek context');
            document.querySelector('#g').lang='';check(selection.toString()==='Ι','live unknown language restores default mapping');
            const supplementary=document.querySelector('#s').firstChild;
            range.setStart(supplementary,1);range.setEnd(supplementary,3);
            check(selection.toString()==='😀' && range.toString()==='😀','UTF16 supplementary source');
            range.setEnd(supplementary,2);
            check(selection.toString()===range.toString() && selection.toString().length===1,'split surrogate stays a raw unit');
            true
        "#));
    }

    #[test]
    fn text_range_uses_utf16_offsets_and_mutation_adjusts_live_boundaries() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<p id='p'>A😀B</p>", 32).unwrap();
        assert!(evaluates_true(
            &mut engine,
            "const text=document.querySelector('#p').firstChild; const range=document.createRange(); range.setStart(text,1); range.setEnd(text,3); const cloned=range.cloneRange(); range.toString()==='😀' && range.startOffset===1 && range.endOffset===3 && cloned.toString()==='😀' && (range.deleteContents(), text.data==='AB' && range.collapsed)"
        ));
    }

    #[test]
    fn extract_insert_and_selection_preserve_range_content_and_direction() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<p id='p'>A😀B</p>", 32).unwrap();
        assert!(evaluates_true(
            &mut engine,
            "const p=document.querySelector('#p'), text=p.firstChild, range=document.createRange(); range.setStart(text,1); range.setEnd(text,3); const fragment=range.extractContents(); const extracted=fragment.textContent==='😀' && text.data==='AB' && range.collapsed; const insertion=document.createRange(); insertion.setStart(text,1); insertion.insertNode(document.createElement('i')); const inserted=p.innerHTML==='A<i></i>B'; const selection=document.getSelection(), tail=p.lastChild; selection.setBaseAndExtent(tail,1,tail,0); extracted && inserted && selection===document.getSelection() && selection.direction==='backward' && selection.toString()==='B'"
        ));
    }

    #[test]
    fn cross_container_delete_removes_intermediate_nodes_and_boundary_text() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main><b>A</b><i>B</i><u>C</u></main>", 64).unwrap();
        assert!(evaluates_true(
            &mut engine,
            "const main=document.querySelector('main'), range=document.createRange(); range.setStart(main.firstChild.firstChild,0); range.setEnd(main.lastChild.firstChild,1); range.deleteContents(); main.innerHTML==='<b></b><u></u>' && range.collapsed"
        ));
    }

    #[test]
    fn range_boundary_moves_to_parent_when_ancestor_is_removed() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main><b>retained</b></main>", 64).unwrap();
        assert!(evaluates_true(
            &mut engine,
            "var range=document.createRange(); (()=>{const text=document.querySelector('b').firstChild; range.setStart(text,0); range.setEnd(text,8); document.querySelector('main').remove()})(); range.startContainer===document.body&&range.endContainer===document.body&&range.startOffset===0&&range.endOffset===0&&range.collapsed&&range.toString()===''"
        ));
    }

    #[test]
    fn contextual_fragment_uses_start_context_and_webidl_arguments() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main><table></table><textarea>text</textarea><select></select></main>", 256).unwrap();
        assert!(evaluates_true(&mut engine, r#"
            const r = document.createRange();
            let missing = false;
            try { r.createContextualFragment(); } catch (e) { missing = e instanceof TypeError; }
            r.detach();
            const body = r.createContextualFragment('<span>body fallback</span>');
            r.selectNodeContents(document.documentElement);
            const html = r.createContextualFragment('<span>no scaffold</span>');
            r.selectNodeContents(document.querySelector('table'));
            const table = r.createContextualFragment('<tr><td>cell</td></tr>');
            r.selectNodeContents(document.querySelector('select'));
            const select = r.createContextualFragment('<option value="x">choice</option>');
            const textarea = document.querySelector('textarea');
            r.setStart(textarea.firstChild, 1);
            r.setEnd(document.querySelector('main'), 3);
            const text = r.createContextualFragment('&amp;<b>');
            const prefixed = document.createElementNS('http://www.w3.org/1999/xhtml', 'h:textarea');
            r.selectNodeContents(prefixed);
            const prefixedText = r.createContextualFragment('&amp;<b>');
            const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
            r.selectNodeContents(svg);
            const foreign = r.createContextualFragment('<circle/><text>svg</text>');
            r.selectNodeContents(document.body);
            let conversions = 0;
            const coerced = r.createContextualFragment({toString() { conversions++; return '<em>converted</em>'; }});
            missing && Range.prototype.createContextualFragment.length === 1 &&
                body.nodeType === 11 && body.firstChild.localName === 'span' && body.ownerDocument === document &&
                html.childNodes.length === 1 && html.firstChild.localName === 'span' &&
                table.firstChild.localName === 'tbody' && table.firstChild.firstChild.localName === 'tr' &&
                select.firstChild.localName === 'option' && select.firstChild.value === 'x' &&
                text.childNodes.length === 1 && text.firstChild.nodeType === 3 && text.textContent === '&<b>' &&
                prefixedText.textContent === '&<b>' &&
                foreign.firstChild.namespaceURI === 'http://www.w3.org/2000/svg' &&
                foreign.lastChild.localName === 'text' && coerced.firstChild.localName === 'em' && conversions === 1 &&
                r.createContextualFragment(null).textContent === 'null' &&
                r.createContextualFragment(undefined).textContent === 'undefined'
        "#));
    }

    #[test]
    fn contextual_fragment_scripts_run_on_insertion_once() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main id=target></main><aside></aside>", 128).unwrap();
        assert!(evaluates_true(&mut engine, r#"
            globalThis.contextualRuns = 0;
            const range = document.createRange();
            range.selectNodeContents(document.documentElement);
            const fragment = range.createContextualFragment('<script>contextualRuns++</script><b>inserted</b>');
            const script = fragment.firstChild;
            const before = contextualRuns === 0 && !script.isConnected;
            document.querySelector('main').appendChild(fragment);
            const inserted = contextualRuns === 1 && fragment.childNodes.length === 0 && script.isConnected;
            document.querySelector('aside').appendChild(script);
            document.querySelector('aside').innerHTML = '<script>contextualRuns+=100</script>';
            before && inserted && contextualRuns === 1
        "#));
    }

    #[test]
    fn contextual_fragment_follows_adopted_detached_range_document() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 128).unwrap();
        assert!(evaluates_true(&mut engine, r#"
            const source = document.implementation.createHTMLDocument('source');
            const target = document.implementation.createHTMLDocument('target');
            const table = source.createElement('table');
            const range = source.createRange();
            range.selectNodeContents(table);
            target.adoptNode(table);
            const fragment = range.createContextualFragment('<tr><td>adopted</td></tr>');
            const owned = fragment.ownerDocument === target && fragment.firstChild.ownerDocument === target &&
                range.startContainer === table && fragment.firstChild.localName === 'tbody';
            table.appendChild(fragment);
            owned && table.ownerDocument === target && table.textContent === 'adopted'
        "#));
    }

    #[test]
    fn contextual_xml_fragment_keeps_namespace_and_strips_only_html_scaffolding() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        super::super::install(engine.ctx(), "<main></main>", 256).unwrap();
        assert!(evaluates_true(engine, r#"
            const xml = document.implementation.createDocument('urn:context', 'root', null);
            xml.documentElement.setAttributeNS('http://www.w3.org/2000/xmlns/', 'xmlns:p', 'urn:prefix');
            const range = xml.createRange();
            range.selectNodeContents(xml.documentElement);
            const fragment = range.createContextualFragment('<child/><p:named/>');
            let syntax = false;
            try { range.createContextualFragment('<partial/><broken>'); }
            catch (e) { syntax = e instanceof DOMException && e.name === 'SyntaxError'; }
            const detached = xml.createRange().createContextualFragment('<p>detached</p>');
            const h = 'http://www.w3.org/1999/xhtml';
            const wrappers = range.createContextualFragment('<h:html xmlns:h="'+h+'"><h:html><h:head><h:title>title</h:title></h:head><h:body><h:p>body</h:p></h:body></h:html></h:html>');
            const other = range.createContextualFragment('<html xmlns="urn:other"><head><body/></head></html>');
            const ordinary = range.createContextualFragment('<div xmlns="'+h+'"><head>retained descendant</head></div>');
            syntax && xml.documentElement.childNodes.length === 0 && fragment.ownerDocument === xml &&
                fragment.firstChild.namespaceURI === 'urn:context' && fragment.lastChild.namespaceURI === 'urn:prefix' &&
                detached.textContent === 'detached' && wrappers.childNodes.length === 2 &&
                wrappers.firstChild.localName === 'title' && wrappers.lastChild.localName === 'p' &&
                wrappers.textContent === 'titlebody' && other.firstChild.localName === 'html' &&
                other.firstChild.namespaceURI === 'urn:other' && ordinary.firstChild.firstChild.localName === 'head'
        "#));
    }

    #[test]
    fn contextual_xml_fragment_processing_instruction_uses_body_fallback() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 128).unwrap();
        assert!(evaluates_true(&mut engine, r#"
            const xml = document.implementation.createDocument('urn:context', 'root', null);
            const root = xml.documentElement;
            const pi = xml.createProcessingInstruction('instruction', 'data');
            root.appendChild(pi);
            const range = xml.createRange();
            range.selectNodeContents(pi);
            const fallback = range.createContextualFragment('<child/>');
            const text = xml.createTextNode('text');
            const cdata = xml.createCDATASection('cdata');
            const comment = xml.createComment('comment');
            let inherited = true;
            for (const node of [text, cdata, comment]) {
                root.appendChild(node);
                range.selectNodeContents(node);
                const fragment = range.createContextualFragment('<child/>');
                inherited = inherited && fragment.firstChild.namespaceURI === 'urn:context' &&
                    fragment.ownerDocument === xml;
            }
            inherited && fallback.ownerDocument === xml && fallback.childNodes.length === 1 &&
                fallback.firstChild.namespaceURI === 'http://www.w3.org/1999/xhtml' &&
                fallback.firstChild.ownerDocument === xml && root.childNodes.length === 4 &&
                pi.parentNode === root && pi.data === 'data'
        "#));
    }

    #[test]
    fn contextual_fragment_failures_reclaim_partial_and_fallback_nodes() {
        let mut html_document = html::parse("<main></main>", 24).unwrap();
        let root = html_document.root();
        let before = html_document.node_count();
        let oversized = "<i></i>".repeat(40);
        for _ in 0..100 {
            assert!(contextual_fragment(&mut html_document, root, &oversized).is_err());
            assert_eq!(html_document.node_count(), before, "HTML quota failure retained parser/context nodes");
        }
        let mut xml_document = lumen_html::xml::parse("<root xmlns='urn:context'/>", 24).unwrap();
        let root = xml_document.root();
        let before = xml_document.node_count();
        for _ in 0..100 {
            assert!(contextual_fragment(&mut xml_document, root, "<partial/><broken>").is_err());
            assert_eq!(xml_document.node_count(), before, "XML syntax failure retained parser/context nodes");
        }
        for _ in 0..100 {
            let fragment = contextual_fragment(&mut xml_document, root,
                "<html><html><head><title>title</title></head><body><p>body</p></body></html></html>").unwrap();
            assert_eq!(xml_document.node_count(), before + 5, "XML wrapper flattening retained scaffold nodes");
            xml_document.destroy_subtree(fragment).unwrap();
            assert_eq!(xml_document.node_count(), before, "XML fragment reclamation retained nodes");
        }
    }
}
