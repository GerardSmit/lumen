//! DOM Range and Selection bindings backed by the document arena.
use super::*;
use core::cmp::Ordering;
use lumen_bind::{FromArg, Host, Slot};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Boundary {
    pub container: NodeId,
    pub offset: usize,
}

pub(crate) struct RangeData {
    start: Cell<Boundary>,
    end: Cell<Boundary>,
    realm: RefCell<Rc<DomRealm>>,
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
        });
        data.retain(root);
        data.retain(root);
        data
    }

    fn copy(&self, realm: &Rc<DomRealm>) -> Rc<Self> {
        let data = Self::new(self.start.get().container, realm);
        data.set_start(self.start.get());
        data.set_end(self.end.get());
        data
    }

    fn retain(&self, node: NodeId) {
        *self
            .realm
            .borrow()
            .retained_nodes
            .borrow_mut()
            .entry(node)
            .or_default() += 1;
    }

    fn release(&self, node: NodeId) {
        let realm = self.realm.borrow();
        let mut retained = realm.retained_nodes.borrow_mut();
        if let Some(count) = retained.get_mut(&node) {
            *count -= 1;
            if *count == 0 {
                retained.remove(&node);
            }
        }
    }

    fn set_start(&self, point: Boundary) {
        let old = self.start.get().container;
        if old != point.container {
            self.release(old);
            self.retain(point.container);
        }
        self.start.set(point);
    }

    fn set_end(&self, point: Boundary) {
        let old = self.end.get().container;
        if old != point.container {
            self.release(old);
            self.retain(point.container);
        }
        self.end.set(point);
    }

    fn current_realm(&self) -> OpResult<Rc<DomRealm>> {
        Ok(self.realm.borrow().clone())
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
        *self.realm.borrow_mut() = target.clone();
        true
    }
}

impl Drop for RangeData {
    fn drop(&mut self) {
        self.release(self.start.get().container);
        self.release(self.end.get().container);
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

fn as_ordering(order: PointOrder) -> Ordering {
    match order {
        PointOrder::Before => Ordering::Less,
        PointOrder::Equal => Ordering::Equal,
        PointOrder::After => Ordering::Greater,
    }
}

fn child_count(document: &lumen_html::Document, node: NodeId) -> OpResult<usize> {
    let mut count = 0;
    let mut child = document.first_child(node).map_err(dom_error)?;
    while let Some(id) = child {
        count += 1;
        child = document.next_sibling(id).map_err(dom_error)?;
    }
    Ok(count)
}

fn text_len(kind: &NodeKind) -> Option<usize> {
    match kind {
        NodeKind::Text(text) | NodeKind::CData(text) | NodeKind::Comment(text) => {
            Some(text.encode_utf16().count())
        }
        NodeKind::ProcessingInstruction { data, .. } => Some(data.encode_utf16().count()),
        _ => None,
    }
}

fn validate_boundary(document: &lumen_html::Document, point: Boundary) -> OpResult<()> {
    let kind = document.kind(point.container).map_err(dom_error)?;
    let limit = text_len(kind).unwrap_or(child_count(document, point.container)?);
    if point.offset > limit {
        return Err(OpError::new(
            "IndexSizeError",
            "boundary offset is outside its container",
        ));
    }
    if matches!(kind, NodeKind::DocumentType(_)) {
        return Err(OpError::new(
            "InvalidNodeTypeError",
            "DocumentType cannot be a Range boundary",
        ));
    }
    Ok(())
}

fn ancestors(document: &lumen_html::Document, node: NodeId) -> OpResult<Vec<NodeId>> {
    let mut result = vec![node];
    let mut current = node;
    while let Some(parent) = document.parent(current).map_err(dom_error)? {
        result.push(parent);
        current = parent;
    }
    Ok(result)
}

fn tree_root(document: &lumen_html::Document, mut node: NodeId) -> OpResult<NodeId> {
    while let Some(parent) = document.parent(node).map_err(dom_error)? {
        node = parent;
    }
    Ok(node)
}

fn child_index(document: &lumen_html::Document, node: NodeId) -> OpResult<usize> {
    let parent = document
        .parent(node)
        .map_err(dom_error)?
        .ok_or_else(|| OpError::new("WrongDocumentError", "node has no parent"))?;
    let mut child = document.first_child(parent).map_err(dom_error)?;
    let mut index = 0;
    while let Some(id) = child {
        if id == node {
            return Ok(index);
        }
        index += 1;
        child = document.next_sibling(id).map_err(dom_error)?;
    }
    Err(OpError::new(
        "WrongDocumentError",
        "node is not a child of its parent",
    ))
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

fn compare_points(
    document: &lumen_html::Document,
    a: Boundary,
    b: Boundary,
) -> OpResult<PointOrder> {
    if a.container == b.container {
        return Ok(match a.offset.cmp(&b.offset) {
            Ordering::Less => PointOrder::Before,
            Ordering::Equal => PointOrder::Equal,
            Ordering::Greater => PointOrder::After,
        });
    }
    let a_chain = ancestors(document, a.container)?;
    let b_chain = ancestors(document, b.container)?;
    if let Some(child_on_a) = b_chain
        .iter()
        .position(|id| *id == a.container)
        .and_then(|index| index.checked_sub(1).map(|i| b_chain[i]))
    {
        return Ok(if a.offset <= child_index(document, child_on_a)? {
            PointOrder::Before
        } else {
            PointOrder::After
        });
    }
    if let Some(child_on_b) = a_chain
        .iter()
        .position(|id| *id == b.container)
        .and_then(|index| index.checked_sub(1).map(|i| a_chain[i]))
    {
        return Ok(if child_index(document, child_on_b)? < b.offset {
            PointOrder::Before
        } else {
            PointOrder::After
        });
    }
    let mut a_path = a_chain;
    let mut b_path = b_chain;
    while a_path.last() == b_path.last() {
        a_path.pop();
        b_path.pop();
    }
    let (Some(a_child), Some(b_child)) = (a_path.last(), b_path.last()) else {
        return Err(OpError::new(
            "WrongDocumentError",
            "Range boundaries are in disconnected trees",
        ));
    };
    let a_index = child_index(document, *a_child)?;
    let b_index = child_index(document, *b_child)?;
    Ok(if a_index < b_index {
        PointOrder::Before
    } else {
        PointOrder::After
    })
}

fn utf16_byte_offset(text: &str, offset: usize) -> OpResult<usize> {
    if offset == 0 {
        return Ok(0);
    }
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units == offset {
            return Ok(byte);
        }
        units += ch.len_utf16();
        if units > offset {
            return Err(OpError::new(
                "IndexSizeError",
                "offset splits a UTF-16 surrogate pair",
            ));
        }
    }
    if units == offset {
        Ok(text.len())
    } else {
        Err(OpError::new(
            "IndexSizeError",
            "offset exceeds character data",
        ))
    }
}

fn string_for_range(document: &lumen_html::Document, data: &RangeData) -> OpResult<String> {
    let start = data.start.get();
    let end = data.end.get();
    let mut text = String::new();
    let mut stack = vec![tree_root(document, start.container)?];
    while let Some(node) = stack.pop() {
        let kind = document.kind(node).map_err(dom_error)?;
        if matches!(kind, NodeKind::Text(_) | NodeKind::CData(_)) {
            let value = text_len(kind).expect("text node length");
            let lo = if start.container == node {
                start.offset
            } else {
                0
            };
            let hi = if end.container == node {
                end.offset
            } else {
                value
            };
            let node_start = Boundary {
                container: node,
                offset: 0,
            };
            let node_end = Boundary {
                container: node,
                offset: value,
            };
            if compare_points(document, node_end, start)? == PointOrder::After
                && compare_points(document, node_start, end)? == PointOrder::Before
            {
                let source = match kind {
                    NodeKind::Text(s) | NodeKind::CData(s) | NodeKind::Comment(s) => s,
                    NodeKind::ProcessingInstruction { data, .. } => data,
                    _ => unreachable!(),
                };
                let lo_byte = utf16_byte_offset(source, lo.min(value))?;
                let hi_byte = utf16_byte_offset(source, hi.min(value))?;
                if lo_byte <= hi_byte {
                    text.push_str(&source[lo_byte..hi_byte]);
                }
            }
        }
        let mut children = Vec::new();
        let mut child = document.first_child(node).map_err(dom_error)?;
        while let Some(id) = child {
            children.push(id);
            child = document.next_sibling(id).map_err(dom_error)?;
        }
        stack.extend(children.into_iter().rev());
    }
    Ok(text)
}

fn node_fully_selected(
    document: &lumen_html::Document,
    start: Boundary,
    end: Boundary,
    node: NodeId,
) -> OpResult<bool> {
    let Some(parent) = document.parent(node).map_err(dom_error)? else {
        return Ok(false);
    };
    let index = child_index(document, node)?;
    let before = Boundary {
        container: parent,
        offset: index,
    };
    let after = Boundary {
        container: parent,
        offset: index + 1,
    };
    Ok(
        compare_points(document, start, before)? != PointOrder::After
            && compare_points(document, after, end)? != PointOrder::After,
    )
}

fn build_contents(
    document: &mut lumen_html::Document,
    parent: NodeId,
    start: Boundary,
    end: Boundary,
    output: Option<NodeId>,
    record_edits: bool,
    whole: &mut Vec<NodeId>,
    replacements: &mut Vec<(NodeId, NodeId)>,
    edits: &mut Vec<(NodeId, String)>,
) -> OpResult<bool> {
    let mut any = false;
    let mut child = document.first_child(parent).map_err(dom_error)?;
    while let Some(node) = child {
        let next = document.next_sibling(node).map_err(dom_error)?;
        if node_fully_selected(document, start, end, node)? {
            any = true;
            whole.push(node);
            if let Some(output) = output {
                let clone = document.clone_subtree(node).map_err(dom_error)?;
                document.append(output, clone).map_err(dom_error)?;
                replacements.push((node, clone));
            }
        } else {
            let kind = document.kind(node).map_err(dom_error)?.clone();
            if let Some(length) = text_len(&kind) {
                if node == start.container || node == end.container {
                    let lo = if node == start.container {
                        start.offset
                    } else {
                        0
                    };
                    let hi = if node == end.container {
                        end.offset
                    } else {
                        length
                    };
                    if lo < hi {
                        let source = match &kind {
                            NodeKind::Text(value)
                            | NodeKind::CData(value)
                            | NodeKind::Comment(value) => value.clone(),
                            NodeKind::ProcessingInstruction { data, .. } => data.clone(),
                            _ => unreachable!(),
                        };
                        let lo_byte = utf16_byte_offset(&source, lo)?;
                        let hi_byte = utf16_byte_offset(&source, hi)?;
                        if let Some(output) = output {
                            let selected = source[lo_byte..hi_byte].to_owned();
                            let selected_kind = match kind {
                                NodeKind::Comment(_) => NodeKind::Comment(selected),
                                NodeKind::CData(_) => NodeKind::CData(selected),
                                NodeKind::ProcessingInstruction { target, .. } => {
                                    NodeKind::ProcessingInstruction {
                                        target,
                                        data: selected,
                                    }
                                }
                                _ => NodeKind::Text(selected),
                            };
                            let clone = document.create(selected_kind).map_err(dom_error)?;
                            document.append(output, clone).map_err(dom_error)?;
                        }
                        if record_edits {
                            edits.push((
                                node,
                                format!("{}{}", &source[..lo_byte], &source[hi_byte..]),
                            ));
                        }
                        any = true;
                    }
                }
            } else if matches!(kind, NodeKind::Element { .. } | NodeKind::DocumentFragment) {
                let clone = if output.is_some() {
                    Some(document.create(kind).map_err(dom_error)?)
                } else {
                    None
                };
                let included = build_contents(
                    document,
                    node,
                    start,
                    end,
                    clone,
                    record_edits,
                    whole,
                    replacements,
                    edits,
                )?;
                if included {
                    any = true;
                    if let (Some(output), Some(clone)) = (output, clone) {
                        document.append(output, clone).map_err(dom_error)?;
                    }
                } else if let Some(clone) = clone {
                    document.destroy_subtree(clone).map_err(dom_error)?;
                }
            }
        }
        child = next;
    }
    Ok(any)
}

enum AbstractRangeBacking {
    Live(Rc<RangeData>),
    Static(Rc<StaticRangeData>),
}

impl AbstractRangeBacking {
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
    backing: AbstractRangeBacking,
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
    pub(crate) fn new(realm: Rc<DomRealm>, registry: Rc<RangeRegistry>) -> Self {
        let root = realm.session.borrow().document().root();
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

    fn set_boundary(&self, point: Boundary, start: bool) -> OpResult<()> {
        let realm = self.data.current_realm()?;
        let session = realm.session.borrow();
        validate_boundary(session.document(), point)?;
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

#[lumen_bind::methods]
impl DomRange {
    #[getter]
    fn common_ancestor_container(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let realm = self.data.current_realm()?;
        let session = realm.session.borrow();
        let chain = ancestors(session.document(), self.data.start.get().container)?;
        let end = ancestors(session.document(), self.data.end.get().container)?;
        let common = chain
            .into_iter()
            .find(|id| end.contains(id))
            .ok_or_else(|| {
                OpError::new("WrongDocumentError", "Range boundaries are disconnected")
            })?;
        drop(session);
        Ok(realm.wrap(ctx, common))
    }
    fn set_start(&self, node: &DomNode, offset: usize) -> OpResult<()> {
        self.check_node(node)?;
        self.set_boundary(
            Boundary {
                container: node.id,
                offset,
            },
            true,
        )
    }
    fn set_end(&self, node: &DomNode, offset: usize) -> OpResult<()> {
        self.check_node(node)?;
        self.set_boundary(
            Boundary {
                container: node.id,
                offset,
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
        self.check_node(node)?;
        let realm = self.data.current_realm()?;
        let session = realm.session.borrow();
        let parent = session
            .document()
            .parent(node.id)
            .map_err(dom_error)?
            .ok_or_else(|| OpError::new("InvalidNodeTypeError", "node has no parent"))?;
        let index = child_index(session.document(), node.id)?;
        drop(session);
        self.set_boundary(
            Boundary {
                container: parent,
                offset: index,
            },
            true,
        )?;
        self.set_boundary(
            Boundary {
                container: parent,
                offset: index + 1,
            },
            false,
        )
    }
    fn select_node_contents(&self, node: &DomNode) -> OpResult<()> {
        self.check_node(node)?;
        let realm = self.data.current_realm()?;
        let session = realm.session.borrow();
        let end = text_len(session.document().kind(node.id).map_err(dom_error)?)
            .unwrap_or(child_count(session.document(), node.id)?);
        drop(session);
        self.set_boundary(
            Boundary {
                container: node.id,
                offset: 0,
            },
            true,
        )?;
        self.set_boundary(
            Boundary {
                container: node.id,
                offset: end,
            },
            false,
        )
    }
    fn compare_boundary_points(&self, how: u8, source: &DomRange) -> OpResult<i8> {
        self.check_realm(&source.data.current_realm()?)?;
        if how > 3 {
            return Err(OpError::new("NotSupportedError", "invalid comparison mode"));
        }
        let (a, b) = match how {
            0 => (self.data.start.get(), source.data.start.get()),
            1 => (self.data.start.get(), source.data.end.get()),
            2 => (self.data.end.get(), source.data.end.get()),
            _ => (self.data.end.get(), source.data.start.get()),
        };
        let realm = self.data.current_realm()?;
        let order = compare_points(realm.session.borrow().document(), a, b)?;
        Ok(match order {
            PointOrder::Before => -1,
            PointOrder::Equal => 0,
            PointOrder::After => 1,
        })
    }
    fn clone_range(&self) -> Self {
        let realm = self
            .data
            .current_realm()
            .expect("live Range retains its owning realm");
        let data = self.data.copy(&realm);
        realm.ranges.register(&data);
        Self {
            base: DomAbstractRange::live(data.clone()),
            data,
        }
    }
    fn to_string(&self) -> OpResult<String> {
        let realm = self.data.current_realm()?;
        let session = realm.session.borrow();
        string_for_range(session.document(), &self.data)
    }
    fn delete_contents(&self) -> OpResult<()> {
        self.remove_contents(false).map(|_| ())
    }
    fn extract_contents(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let realm = self.data.current_realm()?;
        self.remove_contents(true).map(|id| realm.wrap(ctx, id))
    }
    fn clone_contents(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let realm = self.data.current_realm()?;
        let mut session = realm.session.borrow_mut();
        let document = session.document_mut();
        let start = self.data.start.get();
        let end = self.data.end.get();
        let fragment = document
            .create(NodeKind::DocumentFragment)
            .map_err(dom_error)?;
        if start.container == end.container
            && text_len(document.kind(start.container).map_err(dom_error)?).is_some()
        {
            let len = text_len(document.kind(start.container).map_err(dom_error)?).unwrap();
            let source = match document.kind(start.container).map_err(dom_error)? {
                NodeKind::Text(s) | NodeKind::CData(s) | NodeKind::Comment(s) => s.clone(),
                NodeKind::ProcessingInstruction { data, .. } => data.clone(),
                _ => unreachable!(),
            };
            let lo = utf16_byte_offset(&source, start.offset.min(len))?;
            let hi = utf16_byte_offset(&source, end.offset.min(len))?;
            if lo < hi {
                let value = source[lo..hi].to_owned();
                let kind = match document.kind(start.container).map_err(dom_error)? {
                    NodeKind::Comment(_) => NodeKind::Comment(value),
                    NodeKind::CData(_) => NodeKind::CData(value),
                    NodeKind::ProcessingInstruction { target, .. } => {
                        NodeKind::ProcessingInstruction {
                            target: target.clone(),
                            data: value,
                        }
                    }
                    _ => NodeKind::Text(value),
                };
                let clone = document.create(kind).map_err(dom_error)?;
                document.append(fragment, clone).map_err(dom_error)?;
            }
        } else if start.container == end.container {
            let mut child = document.first_child(start.container).map_err(dom_error)?;
            let mut index = 0;
            while let Some(id) = child {
                let next = document.next_sibling(id).map_err(dom_error)?;
                if index >= start.offset && index < end.offset {
                    let clone = document.clone_subtree(id).map_err(dom_error)?;
                    document.append(fragment, clone).map_err(dom_error)?;
                }
                index += 1;
                child = next;
            }
        } else {
            let start_chain = ancestors(document, start.container)?;
            let end_chain = ancestors(document, end.container)?;
            let common = start_chain
                .into_iter()
                .find(|id| end_chain.contains(id))
                .ok_or_else(|| {
                    OpError::new("WrongDocumentError", "Range boundaries are disconnected")
                })?;
            let mut whole = Vec::new();
            let mut replacements = Vec::new();
            let mut edits = Vec::new();
            build_contents(
                document,
                common,
                start,
                end,
                Some(fragment),
                false,
                &mut whole,
                &mut replacements,
                &mut edits,
            )?;
        }
        drop(session);
        Ok(realm.wrap(ctx, fragment))
    }
    fn insert_node(&self, ctx: &mut Ctx, node: &DomNode) -> OpResult<()> {
        let realm = self.data.current_realm()?;
        self.insert(node)?;
        realm.flush_script_activations(ctx)
    }
    fn detach(&self) {}
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
    fn check_node(&self, node: &DomNode) -> OpResult<()> {
        self.check_realm(&node.realm)?;
        Ok(())
    }
    fn set_before_after(&self, node: &DomNode, before: bool) -> OpResult<()> {
        self.check_node(node)?;
        let realm = self.data.current_realm()?;
        let session = realm.session.borrow();
        let parent = session
            .document()
            .parent(node.id)
            .map_err(dom_error)?
            .ok_or_else(|| OpError::new("InvalidNodeTypeError", "node has no parent"))?;
        let offset = child_index(session.document(), node.id)? + usize::from(!before);
        drop(session);
        self.set_boundary(
            Boundary {
                container: parent,
                offset,
            },
            true,
        )
    }
    fn set_end_relative(&self, node: &DomNode, before: bool) -> OpResult<()> {
        self.check_node(node)?;
        let realm = self.data.current_realm()?;
        let session = realm.session.borrow();
        let parent = session
            .document()
            .parent(node.id)
            .map_err(dom_error)?
            .ok_or_else(|| OpError::new("InvalidNodeTypeError", "node has no parent"))?;
        let offset = child_index(session.document(), node.id)? + usize::from(!before);
        drop(session);
        self.set_boundary(
            Boundary {
                container: parent,
                offset,
            },
            false,
        )
    }
    fn insert(&self, node: &DomNode) -> OpResult<()> {
        self.check_node(node)?;
        let point = self.data.start.get();
        let realm = self.data.current_realm()?;
        let mut session = realm.session.borrow_mut();
        let document = session.document_mut();
        if text_len(document.kind(point.container).map_err(dom_error)?).is_some() {
            let source = match document.kind(point.container).map_err(dom_error)? {
                NodeKind::Text(s) | NodeKind::CData(s) | NodeKind::Comment(s) => s.clone(),
                NodeKind::ProcessingInstruction { .. } => {
                    return Err(OpError::new(
                        "HierarchyRequestError",
                        "cannot insert into processing instruction",
                    ));
                }
                _ => unreachable!(),
            };
            let split = utf16_byte_offset(&source, point.offset)?;
            let suffix = source[split..].to_owned();
            let parent = document
                .parent(point.container)
                .map_err(dom_error)?
                .ok_or_else(|| {
                    OpError::new("HierarchyRequestError", "text boundary has no parent")
                })?;
            let next = document.next_sibling(point.container).map_err(dom_error)?;
            let trailing_kind = match document.kind(point.container).map_err(dom_error)? {
                NodeKind::Comment(_) => NodeKind::Comment(suffix),
                NodeKind::CData(_) => NodeKind::CData(suffix),
                NodeKind::ProcessingInstruction { target, .. } => NodeKind::ProcessingInstruction {
                    target: target.clone(),
                    data: suffix,
                },
                _ => NodeKind::Text(suffix),
            };
            let trailing = document.create(trailing_kind).map_err(dom_error)?;
            document
                .replace_data(point.container, &source[..split])
                .map_err(dom_error)?;
            document
                .insert_before(parent, trailing, next)
                .map_err(dom_error)?;
            document
                .insert_before(parent, node.id, Some(trailing))
                .map_err(dom_error)?;
        } else {
            let before = {
                let mut child = document.first_child(point.container).map_err(dom_error)?;
                let mut index = 0;
                while index < point.offset {
                    child = child.and_then(|id| document.next_sibling(id).ok().flatten());
                    index += 1;
                }
                child
            };
            document
                .insert_before(point.container, node.id, before)
                .map_err(dom_error)?;
        }
        Ok(())
    }
    fn remove_contents(&self, extract: bool) -> OpResult<NodeId> {
        let realm = self.data.current_realm()?;
        let mut session = realm.session.borrow_mut();
        let document = session.document_mut();
        let fragment = document
            .create(NodeKind::DocumentFragment)
            .map_err(dom_error)?;
        let start = self.data.start.get();
        let end = self.data.end.get();
        if start.container == end.container
            && text_len(document.kind(start.container).map_err(dom_error)?).is_some()
        {
            let source = match document.kind(start.container).map_err(dom_error)? {
                NodeKind::Text(s) | NodeKind::CData(s) | NodeKind::Comment(s) => s.clone(),
                NodeKind::ProcessingInstruction { data, .. } => data.clone(),
                _ => unreachable!(),
            };
            let a = utf16_byte_offset(&source, start.offset)?;
            let b = utf16_byte_offset(&source, end.offset)?;
            if extract && a < b {
                let kind = document.kind(start.container).map_err(dom_error)?.clone();
                let selected = source[a..b].to_owned();
                let clone = match kind {
                    NodeKind::Comment(_) => NodeKind::Comment(selected),
                    NodeKind::CData(_) => NodeKind::CData(selected),
                    NodeKind::ProcessingInstruction { target, .. } => {
                        NodeKind::ProcessingInstruction {
                            target,
                            data: selected,
                        }
                    }
                    _ => NodeKind::Text(selected),
                };
                let clone = document.create(clone).map_err(dom_error)?;
                document.append(fragment, clone).map_err(dom_error)?;
            }
            let remaining = format!("{}{}", &source[..a], &source[b..]);
            document
                .replace_data(start.container, &remaining)
                .map_err(dom_error)?;
        } else if start.container == end.container {
            let container = start.container;
            let mut selected = Vec::new();
            let mut child = document.first_child(container).map_err(dom_error)?;
            let mut index = 0;
            while let Some(id) = child {
                let next = document.next_sibling(id).map_err(dom_error)?;
                if index >= start.offset && index < end.offset {
                    selected.push(id);
                }
                index += 1;
                child = next;
            }
            for id in selected {
                if extract {
                    document.remove(id).map_err(dom_error)?;
                    document.append(fragment, id).map_err(dom_error)?;
                } else {
                    document.remove(id).map_err(dom_error)?;
                }
            }
        } else {
            let start_chain = ancestors(document, start.container)?;
            let end_chain = ancestors(document, end.container)?;
            let common = start_chain
                .into_iter()
                .find(|id| end_chain.contains(id))
                .ok_or_else(|| {
                    OpError::new("WrongDocumentError", "Range boundaries are disconnected")
                })?;
            let output = extract.then_some(fragment);
            let mut whole = Vec::new();
            let mut replacements = Vec::new();
            let mut edits = Vec::new();
            build_contents(
                document,
                common,
                start,
                end,
                output,
                true,
                &mut whole,
                &mut replacements,
                &mut edits,
            )?;
            if extract {
                for (original, clone) in replacements {
                    document.replace(clone, original).map_err(dom_error)?;
                }
            } else {
                for node in whole {
                    document.remove(node).map_err(dom_error)?;
                }
            }
            for (node, value) in edits {
                document.replace_data(node, &value).map_err(dom_error)?;
            }
        }
        if !extract {
            document.destroy_subtree(fragment).map_err(dom_error)?;
        }
        self.data.set_end(self.data.start.get());
        Ok(fragment)
    }
}

/// State shared by `document.getSelection()` wrappers in one realm.
pub(crate) struct SelectionData {
    pub(crate) registry: Rc<RangeRegistry>,
    pub(crate) ranges: RefCell<Vec<Rc<RangeData>>>,
    pub(crate) anchor: Cell<Option<Boundary>>,
    pub(crate) focus: Cell<Option<Boundary>>,
    backward: Cell<bool>,
}

impl SelectionData {
    pub(crate) fn new(registry: Rc<RangeRegistry>) -> Rc<Self> {
        Rc::new(Self {
            registry,
            ranges: RefCell::new(Vec::new()),
            anchor: Cell::new(None),
            focus: Cell::new(None),
            backward: Cell::new(false),
        })
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
    wrapper: Value,
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
            wrapper: value.clone(),
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

impl DomSelection {
    fn collapse_to_endpoint(&self, start: bool) -> OpResult<()> {
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
        range.set_start(point);
        range.set_end(point);
        self.data.anchor.set(Some(point));
        self.data.focus.set(Some(point));
        self.data.backward.set(false);
        Ok(())
    }
}

#[lumen_bind::methods]
impl DomSelection {
    #[getter]
    fn anchor_node(&self, ctx: &mut Ctx) -> Value {
        let point = self.data.anchor.get();
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
        self.data.anchor.get().map_or(0, |point| point.offset)
    }
    #[getter]
    fn focus_node(&self, ctx: &mut Ctx) -> Value {
        let point = self.data.focus.get();
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
        self.data.focus.get().map_or(0, |point| point.offset)
    }
    #[getter]
    fn is_collapsed(&self) -> bool {
        self.data.anchor.get() == self.data.focus.get()
    }
    #[getter]
    fn range_count(&self) -> usize {
        self.data.ranges.borrow().len()
    }
    #[getter]
    fn direction(&self) -> &'static str {
        let (Some(anchor), Some(focus)) = (self.data.anchor.get(), self.data.focus.get()) else {
            return "none";
        };
        if anchor == focus {
            return "none";
        }
        if self.data.backward.get() {
            return "backward";
        }
        let realm = self
            .data
            .ranges
            .borrow()
            .first()
            .and_then(|range| range.current_realm().ok())
            .unwrap_or_else(|| self.realm.clone());
        let session = realm.session.borrow();
        match compare_points(session.document(), anchor, focus) {
            Ok(PointOrder::Before) => "forward",
            Ok(PointOrder::After) => "backward",
            _ => "none",
        }
    }
    fn get_range_at(&self, index: usize) -> OpResult<DomRange> {
        let data = self
            .data
            .ranges
            .borrow()
            .get(index)
            .cloned()
            .ok_or_else(|| {
                OpError::new("IndexSizeError", "selection range index is out of bounds")
            })?;
        Ok(DomRange::from_data(data))
    }
    fn add_range(&self, range: &DomRange) -> OpResult<()> {
        if !Rc::ptr_eq(&self.realm, &range.data.current_realm()?) {
            return Err(OpError::new(
                "WrongDocumentError",
                "range belongs to another document",
            ));
        }
        let mut ranges = self.data.ranges.borrow_mut();
        if ranges.is_empty() {
            self.data.anchor.set(Some(range.data.start.get()));
            self.data.focus.set(Some(range.data.end.get()));
            self.data.backward.set(false);
            ranges.push(range.data.clone());
        }
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
        if ranges.is_empty() {
            self.data.anchor.set(None);
            self.data.focus.set(None);
            self.data.backward.set(false);
        }
        Ok(())
    }
    fn remove_all_ranges(&self) {
        self.data.ranges.borrow_mut().clear();
        self.data.anchor.set(None);
        self.data.focus.set(None);
        self.data.backward.set(false);
    }
    fn empty(&self) {
        self.remove_all_ranges();
    }
    fn collapse(&self, node: Option<&DomNode>, offset: Option<usize>) -> OpResult<()> {
        let Some(node) = node else {
            self.remove_all_ranges();
            return Ok(());
        };
        if !Rc::ptr_eq(&self.realm, &node.realm) {
            return Err(OpError::new(
                "WrongDocumentError",
                "node belongs to another document",
            ));
        }
        let point = Boundary {
            container: node.id,
            offset: offset.unwrap_or(0),
        };
        validate_boundary(self.realm.session.borrow().document(), point)?;
        let range = RangeData::new(point.container, &self.realm);
        range.set_start(point);
        range.set_end(point);
        self.data.registry.register(&range);
        *self.data.ranges.borrow_mut() = vec![range];
        self.data.anchor.set(Some(point));
        self.data.focus.set(Some(point));
        self.data.backward.set(false);
        Ok(())
    }
    fn collapse_to_start(&self) -> OpResult<()> {
        self.collapse_to_endpoint(true)
    }
    fn collapse_to_end(&self) -> OpResult<()> {
        self.collapse_to_endpoint(false)
    }
    fn set_base_and_extent(
        &self,
        anchor: &DomNode,
        anchor_offset: usize,
        focus: &DomNode,
        focus_offset: usize,
    ) -> OpResult<()> {
        if !Rc::ptr_eq(&self.realm, &anchor.realm) || !Rc::ptr_eq(&self.realm, &focus.realm) {
            return Err(OpError::new(
                "WrongDocumentError",
                "selection endpoints belong to another document",
            ));
        }
        let a = Boundary {
            container: anchor.id,
            offset: anchor_offset,
        };
        let f = Boundary {
            container: focus.id,
            offset: focus_offset,
        };
        let session = self.realm.session.borrow();
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
        *self.data.ranges.borrow_mut() = vec![range];
        self.data.anchor.set(Some(a));
        self.data.focus.set(Some(f));
        self.data.backward.set(order == PointOrder::After);
        Ok(())
    }
    fn extend(&self, node: &DomNode, offset: Option<usize>) -> OpResult<()> {
        if !Rc::ptr_eq(&self.realm, &node.realm) {
            return Err(OpError::new(
                "WrongDocumentError",
                "node belongs to another document",
            ));
        }
        let focus = Boundary {
            container: node.id,
            offset: offset.unwrap_or(0),
        };
        validate_boundary(self.realm.session.borrow().document(), focus)?;
        let anchor = self.data.anchor.get().unwrap_or(focus);
        let session = self.realm.session.borrow();
        let order = compare_points(session.document(), anchor, focus)?;
        drop(session);
        let existing = self.data.ranges.borrow().first().cloned();
        if let Some(range) = existing {
            if order == PointOrder::After {
                range.set_start(focus);
                range.set_end(anchor);
            } else {
                range.set_start(anchor);
                range.set_end(focus);
            }
        } else {
            let range = RangeData::new(
                if order == PointOrder::After {
                    node.id
                } else {
                    anchor.container
                },
                &self.realm,
            );
            if order == PointOrder::After {
                range.set_start(focus);
                range.set_end(anchor);
            } else {
                range.set_start(anchor);
                range.set_end(focus);
            }
            self.data.registry.register(&range);
            self.data.ranges.borrow_mut().push(range);
        }
        self.data.anchor.set(Some(anchor));
        self.data.focus.set(Some(focus));
        self.data.backward.set(order == PointOrder::After);
        Ok(())
    }
    fn delete_from_document(&self) -> OpResult<()> {
        if let Some(range) = self.data.ranges.borrow().first() {
            DomRange::from_data(range.clone()).delete_contents()?;
        }
        Ok(())
    }
    fn select_all_children(&self, node: &DomNode) -> OpResult<()> {
        if !Rc::ptr_eq(&self.realm, &node.realm) {
            return Err(OpError::new(
                "WrongDocumentError",
                "node belongs to another document",
            ));
        }
        let count = child_count(self.realm.session.borrow().document(), node.id)?;
        let range = RangeData::new(node.id, &self.realm);
        range.set_end(Boundary {
            container: node.id,
            offset: count,
        });
        self.data.registry.register(&range);
        *self.data.ranges.borrow_mut() = vec![range.clone()];
        self.data.anchor.set(Some(range.start.get()));
        self.data.focus.set(Some(range.end.get()));
        self.data.backward.set(false);
        Ok(())
    }
    fn to_string(&self) -> OpResult<String> {
        let ranges = self.data.ranges.borrow();
        let mut output = String::new();
        for range in ranges.iter() {
            let realm = range.current_realm()?;
            output.push_str(&string_for_range(realm.session.borrow().document(), range)?);
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
) {
    use lumen_html::observe::ObservedKind;
    let mut ranges = registry.ranges.borrow_mut();
    ranges.retain(|entry| entry.strong_count() > 0);
    for range in ranges.iter().filter_map(std::rc::Weak::upgrade) {
        for point in [&range.start, &range.end] {
            let mut boundary = point.get();
            match &mutation.kind {
                ObservedKind::CharacterData { old_value }
                    if boundary.container == mutation.target =>
                {
                    let current = match document.kind(mutation.target) {
                        Ok(
                            NodeKind::Text(value)
                            | NodeKind::CData(value)
                            | NodeKind::Comment(value),
                        ) => value,
                        Ok(NodeKind::ProcessingInstruction { data, .. }) => data,
                        _ => continue,
                    };
                    let old: Vec<u16> = old_value.encode_utf16().collect();
                    let new: Vec<u16> = current.encode_utf16().collect();
                    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
                    let suffix = old
                        .iter()
                        .rev()
                        .zip(new.iter().rev())
                        .take_while(|(a, b)| a == b)
                        .count();
                    let old_end = old.len().saturating_sub(suffix);
                    let new_end = new.len().saturating_sub(suffix);
                    if boundary.offset > old_end {
                        if new_end >= old_end {
                            boundary.offset = boundary.offset.saturating_add(new_end - old_end);
                        } else {
                            boundary.offset = boundary.offset.saturating_sub(old_end - new_end);
                        }
                    } else if boundary.offset > prefix {
                        boundary.offset = new_end;
                    }
                }
                ObservedKind::ChildList {
                    added,
                    removed,
                    previous_sibling,
                    next_sibling,
                } => {
                    if let Some(removed) = removed {
                        let index = next_sibling
                            .and_then(|next| child_index(document, next).ok())
                            .or_else(|| {
                                previous_sibling.and_then(|prev| {
                                    child_index(document, prev).ok().map(|i| i + 1)
                                })
                            })
                            .unwrap_or(0);
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
            point.set(boundary);
        }
    }
}

pub(crate) fn sync_selection(data: &SelectionData) {
    let range = data.ranges.borrow().first().cloned();
    if let Some(range) = range {
        if data.backward.get() {
            data.anchor.set(Some(range.end.get()));
            data.focus.set(Some(range.start.get()));
        } else {
            data.anchor.set(Some(range.start.get()));
            data.focus.set(Some(range.end.get()));
        }
    } else {
        data.anchor.set(None);
        data.focus.set(None);
        data.backward.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

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
}
