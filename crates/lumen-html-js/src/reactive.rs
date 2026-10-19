use super::*;
use lumen::embed::{JsFunction, JsHost, JsObject, OpError, OpResult};
use lumen_bind::Host;
use std::{collections::VecDeque, rc::Weak};

pub(crate) struct Runtime {
    pub(crate) realm: Weak<DomRealm>,
    active: RefCell<Option<Rc<Effect>>>,
    owner: RefCell<Rc<Scope>>,
    pending: RefCell<VecDeque<Rc<Effect>>>,
    scheduler: RefCell<Option<WeakValue>>,
    scheduled: Cell<bool>,
    batching: Cell<usize>,
    flushing: Cell<bool>,
}

struct Scope {
    runtime: Weak<Runtime>,
    parent: Weak<Scope>,
    effects: RefCell<Vec<Rc<Effect>>>,
    children: RefCell<Vec<Rc<Scope>>>,
    cleanups: RefCell<Vec<JsFunction>>,
    boundary: Option<JsFunction>,
    disposed: Cell<bool>,
}

impl Scope {
    fn new(runtime: Weak<Runtime>, parent: Weak<Scope>, boundary: Option<JsFunction>) -> Rc<Self> {
        Rc::new(Self {
            runtime,
            parent,
            effects: RefCell::new(Vec::new()),
            children: RefCell::new(Vec::new()),
            cleanups: RefCell::new(Vec::new()),
            boundary,
            disposed: Cell::new(false),
        })
    }
    fn dispose(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<()> {
        if self.disposed.replace(true) {
            return Ok(());
        }
        let runtime = self.runtime.upgrade();
        let previous_owner = runtime
            .as_ref()
            .map(|runtime| runtime.owner.replace(self.clone()));
        let previous_active = runtime.as_ref().map(|runtime| runtime.active.replace(None));
        let effects = std::mem::take(&mut *self.effects.borrow_mut());
        let children = std::mem::take(&mut *self.children.borrow_mut());
        let cleanups = std::mem::take(&mut *self.cleanups.borrow_mut());
        let mut error = None;
        for effect in effects {
            if let Err(e) = effect.dispose(ctx) {
                error.get_or_insert(e);
            }
        }
        for child in children {
            if let Err(e) = child.dispose(ctx) {
                error.get_or_insert(e);
            }
        }
        for cleanup in cleanups.into_iter().rev() {
            if let Err(e) = cleanup.call(ctx, Value::Undefined, &[]) {
                error.get_or_insert(e);
            }
        }
        if let (Some(runtime), Some(owner), Some(active)) =
            (runtime, previous_owner, previous_active)
        {
            runtime.active.replace(active);
            runtime.owner.replace(owner);
        }
        if let Some(parent) = self.parent.upgrade() {
            parent
                .children
                .borrow_mut()
                .retain(|child| !Rc::ptr_eq(child, self));
        }
        error.map_or(Ok(()), Err)
    }
}

struct SignalData {
    runtime: Weak<Runtime>,
    value: RefCell<Value>,
    subscribers: RefCell<Vec<Weak<Effect>>>,
    producer: RefCell<Option<Weak<Effect>>>,
}

struct Effect {
    runtime: Weak<Runtime>,
    parent: Weak<Scope>,
    scope: RefCell<Rc<Scope>>,
    callback: RefCell<Option<JsFunction>>,
    dependencies: RefCell<Vec<Weak<SignalData>>>,
    previous: RefCell<Value>,
    output: Output,
    queued: Cell<bool>,
    disposed: Cell<bool>,
}

enum Output {
    None,
    Signal(Rc<SignalData>),
    Text(Rc<DomRealm>, NodeId),
    Attribute(Rc<DomRealm>, NodeId, String),
    Region(Rc<Region>),
}

struct RetainedNodes {
    realm: Rc<DomRealm>,
    ids: Vec<NodeId>,
}
impl RetainedNodes {
    fn new(realm: Rc<DomRealm>, ids: Vec<NodeId>) -> Self {
        for id in &ids {
            *realm.retained_nodes.borrow_mut().entry(*id).or_default() += 1;
        }
        Self { realm, ids }
    }
}
impl Drop for RetainedNodes {
    fn drop(&mut self) {
        let mut retained = self.realm.retained_nodes.borrow_mut();
        for id in &self.ids {
            if let Some(count) = retained.get_mut(id) {
                *count -= 1;
                if *count == 0 {
                    retained.remove(id);
                }
            }
        }
        drop(retained);
        self.realm.release_detached_nodes(self.ids.iter().copied());
    }
}

struct Entry {
    key: Value,
    nodes: RetainedNodes,
    scope: Rc<Scope>,
    index: Rc<SignalData>,
}
enum RegionKind {
    For(JsFunction),
    Show { children: Value, fallback: Value },
    Child,
}
struct Region {
    realm: Rc<DomRealm>,
    marker: NodeId,
    _marker: RetainedNodes,
    entries: RefCell<Vec<Rc<Entry>>>,
    kind: RegionKind,
}

enum PendingNode {
    Existing(NodeId),
    Text(String),
}

const REACTIVE_CHILD_DEPTH_LIMIT: usize = 256;

struct CollectLimits {
    max_nodes: usize,
    max_visits: usize,
    visits: usize,
    output_nodes: usize,
    text_bytes: usize,
}

impl CollectLimits {
    fn resource_error() -> OpError {
        OpError::range_error("reactive child resource limit exceeded")
    }

    fn visit(&mut self) -> OpResult<()> {
        if self.visits >= self.max_visits {
            return Err(Self::resource_error());
        }
        self.visits += 1;
        Ok(())
    }

    fn array_length(&self, length: f64) -> OpResult<usize> {
        let remaining = self.max_visits.saturating_sub(self.visits);
        if !length.is_finite() || length < 0.0 || length.fract() != 0.0
            || length > remaining as f64
        {
            return Err(Self::resource_error());
        }
        Ok(length as usize)
    }

    fn push_existing(&mut self, out: &mut Vec<PendingNode>, node: NodeId) -> OpResult<()> {
        if self.output_nodes >= self.max_nodes || out.try_reserve(1).is_err() {
            return Err(Self::resource_error());
        }
        self.output_nodes += 1;
        out.push(PendingNode::Existing(node));
        Ok(())
    }

    fn extend_existing(
        &mut self,
        out: &mut Vec<PendingNode>,
        nodes: impl IntoIterator<Item = NodeId>,
        count: usize,
    ) -> OpResult<()> {
        if self
            .output_nodes
            .checked_add(count)
            .is_none_or(|total| total > self.max_nodes)
            || out.try_reserve(count).is_err()
        {
            return Err(Self::resource_error());
        }
        self.output_nodes += count;
        out.extend(nodes.into_iter().map(PendingNode::Existing));
        Ok(())
    }

    fn push_text(&mut self, out: &mut Vec<PendingNode>, text: &str) -> OpResult<()> {
        if self.output_nodes >= self.max_nodes {
            return Err(Self::resource_error());
        }
        let next_bytes = self
            .text_bytes
            .checked_add(text.len())
            .filter(|bytes| *bytes <= html::MAX_HTML_BYTES)
            .ok_or_else(Self::resource_error)?;
        out.try_reserve(1).map_err(|_| Self::resource_error())?;
        let mut owned = String::new();
        owned
            .try_reserve(text.len())
            .map_err(|_| Self::resource_error())?;
        owned.push_str(text);
        self.text_bytes = next_bytes;
        self.output_nodes += 1;
        out.push(PendingNode::Text(owned));
        Ok(())
    }
}

fn collect_node_parts(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    value: Value,
    out: &mut Vec<PendingNode>,
    keepalive: &mut Vec<Value>,
    limits: &mut CollectLimits,
    depth: usize,
) -> OpResult<()> {
    if depth > REACTIVE_CHILD_DEPTH_LIMIT {
        return Err(OpError::new(
            "RangeError",
            "reactive child nesting limit exceeded",
        ));
    }
    limits.visit()?;
    if matches!(value, Value::Null | Value::Undefined | Value::Bool(_)) {
        return Ok(());
    }
    if ctx.is_array_value(&value).map_err(OpError::thrown)? {
        let length = ctx
            .get_member(&value, "length")
            .map_err(|_| OpError::new("TypeError", "reactive children length failed"))?;
        let Value::Num(length) = length else {
            return Err(OpError::new("TypeError", "invalid reactive children"));
        };
        let length = limits.array_length(length)?;
        for index in 0..length {
            let child = ctx
                .get_member(&value, &index.to_string())
                .map_err(|_| OpError::new("TypeError", "reactive child read failed"))?;
            collect_node_parts(ctx, realm, child, out, keepalive, limits, depth + 1)?;
        }
    } else if matches!(value, Value::Str(_) | Value::Num(_)) {
        let text = ctx.coerce_string(&value).map_err(OpError::thrown)?;
        limits.push_text(out, text.as_ref())?;
    } else {
        let (owner, node) =
            ctx.with_instance::<DomNode, _>(&value, |node| (node.realm.clone(), node.id))?;
        if !Rc::ptr_eq(realm, &owner) {
            return Err(OpError::new(
                "TypeError",
                "reactive child belongs to a different realm",
            ));
        }
        let fragment_children = {
            let session = realm.session.borrow();
            if matches!(
                session.document().kind(node),
                Ok(NodeKind::DocumentFragment)
            ) {
                Some(children(session.document(), node).map_err(dom_error)?)
            } else {
                None
            }
        };
        if let Some(children) = fragment_children {
            // The fragment wrapper keeps its native identity component alive
            // through capacity preflight and text-node construction.
            if !children.is_empty() {
                keepalive
                    .try_reserve(1)
                    .map_err(|_| CollectLimits::resource_error())?;
                limits.extend_existing(out, children.iter().copied(), children.len())?;
                keepalive.push(value);
            }
        } else {
            keepalive
                .try_reserve(1)
                .map_err(|_| CollectLimits::resource_error())?;
            limits.push_existing(out, node)?;
            keepalive.push(value);
        }
    }
    Ok(())
}

fn collect_nodes(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    value: Value,
    depth: usize,
) -> OpResult<RetainedNodes> {
    let max_nodes = {
        let session = realm.session.borrow();
        let document = session.document();
        document
            .node_count()
            .saturating_add(document.remaining_node_capacity())
    };
    let mut limits = CollectLimits {
        max_nodes,
        max_visits: max_nodes.saturating_mul(REACTIVE_CHILD_DEPTH_LIMIT + 1),
        visits: 0,
        output_nodes: 0,
        text_bytes: 0,
    };
    let mut pending = Vec::new();
    let mut keepalive = Vec::new();
    collect_node_parts(
        ctx,
        realm,
        value,
        &mut pending,
        &mut keepalive,
        &mut limits,
        depth,
    )?;

    // Convert every JS value before allocating nodes, then preflight the exact
    // text-node count in one batch. No GC/capacity pass can reclaim an output
    // between successive factory calls because those calls are performed
    // under the same session borrow after this single preflight.
    let text_nodes = pending
        .iter()
        .filter(|pending| matches!(pending, PendingNode::Text(_)))
        .count();
    realm.reap_detached_for_capacity(text_nodes);

    let mut nodes = Vec::with_capacity(pending.len());
    let mut generated = Vec::with_capacity(text_nodes);
    let mut session = realm.session.borrow_mut();
    let mut create_error = None;
    for pending in pending {
        match pending {
            PendingNode::Existing(node) => nodes.push(node),
            PendingNode::Text(text) => match session.document_mut().create(NodeKind::Text(text)) {
                Ok(node) => {
                    nodes.push(node);
                    generated.push(node);
                }
                Err(error) => {
                    create_error = Some(error);
                    break;
                }
            },
        }
    }
    if let Some(error) = create_error {
        let mut cleanup_failed = Vec::new();
        for node in generated {
            if session.document_mut().destroy_subtree(node).is_err() {
                cleanup_failed.push(node);
            }
        }
        drop(session);
        if !cleanup_failed.is_empty() {
            // These outputs were never handed to a caller or leased. A direct
            // retry is safe after releasing the session borrow.
            realm.reap_detached(cleanup_failed);
        }
        return Err(OpError::from(dom_error(error)));
    }
    drop(session);

    // Seal the lease before `keepalive` wrappers are dropped. If a later
    // callback or capacity pass runs during this region update, every output
    // remains protected by the shared native-retention count.
    let retained = RetainedNodes::new(realm.clone(), nodes);
    drop(keepalive);
    Ok(retained)
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Undefined | Value::Null => false,
        Value::Bool(value) => *value,
        Value::Num(value) => *value != 0.0 && !value.is_nan(),
        Value::Str(value) => !value.is_empty(),
        _ => true,
    }
}

impl Region {
    fn update(
        &self,
        ctx: &mut Ctx,
        runtime: &Rc<Runtime>,
        owner: &Rc<Scope>,
        value: Value,
    ) -> OpResult<()> {
        let items = match &self.kind {
            RegionKind::For(_) => {
                if !ctx.is_array_value(&value).map_err(OpError::thrown)? {
                    return Err(OpError::new("TypeError", "For.each must return an array"));
                }
                let length = ctx
                    .get_member(&value, "length")
                    .map_err(|_| OpError::new("TypeError", "For.each length failed"))?;
                let Value::Num(length) = length else {
                    return Err(OpError::new("TypeError", "invalid For.each array"));
                };
                let mut items = Vec::with_capacity(length as usize);
                for index in 0..length as usize {
                    items.push(
                        ctx.get_member(&value, &index.to_string())
                            .map_err(|_| OpError::new("TypeError", "For item read failed"))?,
                    );
                }
                items
            }
            RegionKind::Show { .. } => vec![Value::Bool(truthy(&value))],
            RegionKind::Child => vec![value.clone()],
        };
        let mut previous = self
            .entries
            .borrow()
            .iter()
            .cloned()
            .map(Some)
            .collect::<Vec<_>>();
        let mut entries = Vec::with_capacity(items.len());
        let mut created = Vec::new();
        let mut moved = Vec::new();
        let build = (|| {
            for (position, item) in items.into_iter().enumerate() {
                if let Some(index) = previous
                    .iter()
                    .position(|entry| entry.as_ref().is_some_and(|entry| same(&entry.key, &item)))
                {
                    let entry = previous[index].take().unwrap();
                    moved.push((entry.index.clone(), position));
                    entries.push(entry);
                    continue;
                }
                let scope = Scope::new(Rc::downgrade(runtime), Rc::downgrade(owner), None);
                created.push(scope.clone());
                let index = Rc::new(SignalData {
                    runtime: Rc::downgrade(runtime),
                    value: RefCell::new(Value::Num(position as f64)),
                    subscribers: RefCell::new(Vec::new()),
                    producer: RefCell::new(None),
                });
                let old_owner = runtime.owner.replace(scope.clone());
                let old_active = runtime.active.replace(None);
                let rendered = match &self.kind {
                    RegionKind::For(render) => {
                        let signal = ctx.new_instance(Signal {
                            data: index.clone(),
                        });
                        bound(ctx, &signal, "get").and_then(|index| {
                            render.call(ctx, Value::Undefined, &[item.clone(), index])
                        })
                    }
                    RegionKind::Show { children, fallback } => {
                        let child = if truthy(&item) {
                            children.clone()
                        } else {
                            fallback.clone()
                        };
                        if let Some(render) = JsFunction::from_value(child.clone()) {
                            render.call(ctx, Value::Undefined, &[value.clone()])
                        } else {
                            Ok(child)
                        }
                    }
                    RegionKind::Child => Ok(item.clone()),
                };
                runtime.owner.replace(old_owner);
                runtime.active.replace(old_active);
                let rendered = match rendered {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = scope.dispose(ctx);
                        return Err(error);
                    }
                };
                let nodes = collect_nodes(ctx, &self.realm, rendered, 0)?;
                entries.push(Rc::new(Entry {
                    key: item,
                    nodes,
                    scope,
                    index,
                }));
            }
            Ok(())
        })();
        if let Err(error) = build {
            for scope in created {
                let _ = scope.dispose(ctx);
            }
            return Err(error);
        }
        let commit = (|| {
            let mut session = self.realm.session.borrow_mut();
            let document = session.document_mut();
            let parent = document
                .parent(self.marker)
                .map_err(|error| OpError::from(dom_error(error)))?
                .ok_or_else(|| OpError::new("InvalidStateError", "reactive region was removed"))?;
            let ids = entries
                .iter()
                .flat_map(|entry| entry.nodes.ids.iter().copied())
                .collect::<Vec<_>>();
            let old_ids = self
                .entries
                .borrow()
                .iter()
                .flat_map(|entry| entry.nodes.ids.iter().copied())
                .collect::<Vec<_>>();
            if old_ids != ids {
                document
                    .insert_many_before(parent, &ids, Some(self.marker))
                    .map_err(|error| OpError::from(dom_error(error)))?;
            }
            for entry in previous.iter().flatten() {
                for node in &entry.nodes.ids {
                    document
                        .remove(*node)
                        .map_err(|error| OpError::from(dom_error(error)))?;
                }
            }
            Ok(())
        })();
        if let Err(error) = commit {
            for scope in created {
                let _ = scope.dispose(ctx);
            }
            return Err(error);
        }
        owner.children.borrow_mut().extend(created);
        *self.entries.borrow_mut() = entries;
        for (index, position) in moved {
            index.write(ctx, Value::Num(position as f64));
        }
        let mut cleanup_error = None;
        for entry in previous.into_iter().flatten() {
            if let Err(error) = entry.scope.dispose(ctx) {
                cleanup_error.get_or_insert(error);
            }
            owner
                .children
                .borrow_mut()
                .retain(|scope| !Rc::ptr_eq(scope, &entry.scope));
        }
        let activation = self.realm.flush_script_activations(ctx);
        if let Some(error) = cleanup_error {
            return Err(error);
        }
        activation
    }
}

fn region(ctx: &mut Ctx, source: JsFunction, kind: RegionKind) -> OpResult<Value> {
    let runtime = runtime(ctx)?;
    let realm = runtime
        .realm
        .upgrade()
        .ok_or_else(|| OpError::new("Error", "DOM realm was dropped"))?;
    let mut session = realm.session.borrow_mut();
    let document = session.document_mut();
    let fragment = document
        .create(NodeKind::DocumentFragment)
        .map_err(|error| OpError::from(dom_error(error)))?;
    let marker = document
        .create(NodeKind::Comment(String::new()))
        .map_err(|error| OpError::from(dom_error(error)))?;
    document
        .append(fragment, marker)
        .map_err(|error| OpError::from(dom_error(error)))?;
    drop(session);
    let region = Rc::new(Region {
        realm: realm.clone(),
        marker,
        _marker: RetainedNodes::new(realm.clone(), vec![marker]),
        entries: RefCell::new(Vec::new()),
        kind,
    });
    let effect = runtime.effect(source, Output::Region(region))?;
    if let Err(error) = effect.run(ctx) {
        let _ = effect.dispose(ctx);
        return Err(error);
    }
    let wrapper = realm.wrap(ctx, fragment);
    // The fragment is now protected by its live wrapper and marker lease.
    // Register it only after construction has finished so a capacity-pressure
    // pass cannot reclaim it while the region is still being assembled.
    realm.defer_detached_root(fragment);
    Ok(wrapper)
}

pub(crate) fn runtime(ctx: &mut Ctx) -> OpResult<Rc<Runtime>> {
    crate::realm_services::RealmServices::<Runtime>::current(ctx)
        .ok_or_else(|| OpError::new("Error", "DOM runtime is not installed"))
}

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Undefined, Value::Undefined) | (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Num(a), Value::Num(b)) => a == b,
        (Value::Str(a), Value::Str(b)) => a == b,
        (Value::Obj(a), Value::Obj(b)) => std::ptr::eq(&**a, &**b),
        _ => false,
    }
}

impl Runtime {
    pub(crate) fn new(realm: &Rc<DomRealm>) -> Rc<Self> {
        Rc::new_cyclic(|runtime| Self {
            realm: Rc::downgrade(realm),
            active: RefCell::new(None),
            owner: RefCell::new(Scope::new(runtime.clone(), Weak::new(), None)),
            pending: RefCell::new(VecDeque::new()),
            scheduler: RefCell::new(None),
            scheduled: Cell::new(false),
            batching: Cell::new(0),
            flushing: Cell::new(false),
        })
    }
    fn schedule(&self, ctx: &mut Ctx) {
        if !self.flushing.get()
            && self.batching.get() == 0
            && !self.pending.borrow().is_empty()
            && !self.scheduled.replace(true)
        {
            if let Some(callback) = self.scheduler.borrow().as_ref().and_then(WeakValue::upgrade) {
                ctx.queue_microtask(callback);
            }
        }
    }
    fn enqueue(&self, ctx: &mut Ctx, effect: Rc<Effect>) {
        if !effect.disposed.get() && !effect.queued.replace(true) {
            self.pending.borrow_mut().push_back(effect);
        }
        self.schedule(ctx);
    }
    fn effect(self: &Rc<Self>, callback: JsFunction, output: Output) -> OpResult<Rc<Effect>> {
        let parent = self.owner.borrow().clone();
        if parent.disposed.get() {
            return Err(OpError::new(
                "InvalidStateError",
                "cannot create an effect in a disposed reactive owner",
            ));
        }
        let effect = Rc::new(Effect {
            runtime: Rc::downgrade(self),
            parent: Rc::downgrade(&parent),
            scope: RefCell::new(Scope::new(
                Rc::downgrade(self),
                Rc::downgrade(&parent),
                None,
            )),
            callback: RefCell::new(Some(callback)),
            dependencies: RefCell::new(Vec::new()),
            previous: RefCell::new(Value::Undefined),
            output,
            queued: Cell::new(false),
            disposed: Cell::new(false),
        });
        parent.effects.borrow_mut().push(effect.clone());
        Ok(effect)
    }
}

impl SignalData {
    fn read(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<Value> {
        let producer = self.producer.borrow().as_ref().and_then(Weak::upgrade);
        if let Some(producer) = producer {
            if producer.queued.get() {
                producer.run(ctx)?;
            }
        }
        if let Some(runtime) = self.runtime.upgrade() {
            if let Some(effect) = runtime.active.borrow().as_ref() {
                if !effect.dependencies.borrow().iter().any(|signal| {
                    signal
                        .upgrade()
                        .is_some_and(|signal| Rc::ptr_eq(&signal, self))
                }) {
                    effect.dependencies.borrow_mut().push(Rc::downgrade(self));
                    self.subscribers.borrow_mut().push(Rc::downgrade(effect));
                }
            }
        }
        Ok(self.value.borrow().clone())
    }
    fn write(&self, ctx: &mut Ctx, value: Value) {
        if same(&self.value.borrow(), &value) {
            return;
        }
        *self.value.borrow_mut() = value;
        if let Some(runtime) = self.runtime.upgrade() {
            self.subscribers.borrow_mut().retain(|subscriber| {
                if let Some(effect) = subscriber.upgrade() {
                    runtime.enqueue(ctx, effect);
                    true
                } else {
                    false
                }
            });
        }
    }
}

impl Effect {
    fn unsubscribe(&self) {
        for dependency in std::mem::take(&mut *self.dependencies.borrow_mut()) {
            if let Some(signal) = dependency.upgrade() {
                signal.subscribers.borrow_mut().retain(|subscriber| {
                    subscriber
                        .upgrade()
                        .is_some_and(|effect| !std::ptr::eq(&*effect, self))
                });
            }
        }
    }
    fn dispose(&self, ctx: &mut Ctx) -> OpResult<()> {
        if self.disposed.replace(true) {
            return Ok(());
        }
        self.unsubscribe();
        self.callback.borrow_mut().take();
        *self.previous.borrow_mut() = Value::Undefined;
        let result = self.scope.borrow().clone().dispose(ctx);
        if let Some(parent) = self.parent.upgrade() {
            parent
                .effects
                .borrow_mut()
                .retain(|effect| !std::ptr::eq(&**effect, self));
        }
        result
    }
    fn run(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<()> {
        self.queued.set(false);
        if self.disposed.get() {
            return Ok(());
        }
        let Some(runtime) = self.runtime.upgrade() else {
            return Ok(());
        };
        match self.run_inner(ctx, &runtime) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = catch_at_boundary(ctx, self.parent.upgrade(), error)?;
                Ok(())
            }
        }
    }
    fn run_inner(self: &Rc<Self>, ctx: &mut Ctx, runtime: &Rc<Runtime>) -> OpResult<()> {
        self.unsubscribe();
        let is_region = matches!(&self.output, Output::Region(_));
        let scope = if is_region {
            self.scope.borrow().clone()
        } else {
            let previous_scope = self.scope.borrow().clone();
            if let Err(error) = previous_scope.dispose(ctx) {
                let _ = self.dispose(ctx);
                return Err(error);
            }
            let scope = Scope::new(Rc::downgrade(runtime), self.parent.clone(), None);
            *self.scope.borrow_mut() = scope.clone();
            scope
        };
        let callback = self
            .callback
            .borrow()
            .as_ref()
            .cloned()
            .ok_or_else(|| OpError::new("Error", "effect was disposed"))?;
        let previous = self.previous.borrow().clone();
        let old_owner = runtime.owner.replace(scope.clone());
        let old_active = runtime.active.replace(Some(self.clone()));
        let result = callback.call(ctx, Value::Undefined, &[previous]);
        runtime.active.replace(old_active);
        runtime.owner.replace(old_owner);
        match result {
            Ok(value) => {
                *self.previous.borrow_mut() = value.clone();
                let output_result = (|| {
                    match &self.output {
                        Output::None => {}
                        Output::Signal(output) => output.write(ctx, value),
                        Output::Text(realm, node) => {
                            let text =
                                if matches!(value, Value::Null | Value::Undefined | Value::Bool(_))
                                {
                                    Rc::from("")
                                } else {
                                    ctx.coerce_string(&value).map_err(OpError::thrown)?
                                };
                            realm
                                .session
                                .borrow_mut()
                                .document_mut()
                                .replace_data(*node, &text)
                                .map_err(|error| OpError::from(dom_error(error)))?;
                        }
                        Output::Attribute(realm, node, name) => {
                            if name == "style" && matches!(value, Value::Obj(_)) {
                                let element = realm.wrap(ctx, *node);
                                super::jsx::apply_style(ctx, &element, &value)?;
                            } else if name == "value"
                                && write_form_value(ctx, realm, *node, &value)?
                            {
                            } else if matches!(
                                value,
                                Value::Null | Value::Undefined | Value::Bool(false)
                            ) {
                                realm
                                    .session
                                    .borrow_mut()
                                    .document_mut()
                                    .remove_attribute(*node, name)
                                    .map_err(|error| OpError::from(dom_error(error)))?;
                            } else {
                                let text = if matches!(value, Value::Bool(true)) {
                                    Rc::from("")
                                } else {
                                    ctx.coerce_string(&value).map_err(OpError::thrown)?
                                };
                                realm
                                    .session
                                    .borrow_mut()
                                    .document_mut()
                                    .set_attribute(*node, name, &text)
                                    .map_err(|error| OpError::from(dom_error(error)))?;
                            }
                        }
                        Output::Region(region) => {
                            region.update(ctx, runtime, &self.scope.borrow(), value)?
                        }
                    }
                    Ok(())
                })();
                if output_result.is_err() && !is_region {
                    let _ = scope.dispose(ctx);
                }
                output_result
            }
            Err(error) => {
                if !is_region {
                    let _ = scope.dispose(ctx);
                }
                Err(error)
            }
        }
    }
}

/// A form control's `value` binding drives the live IDL value: once the user has edited a
/// control, its content attribute no longer affects what it shows.
fn write_form_value(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    value: &Value,
) -> OpResult<bool> {
    let element = realm.wrap(ctx, node);
    let local_name = ctx
        .get_member(&element, "localName")
        .ok()
        .and_then(|name| ctx.coerce_string(&name).ok());
    if !matches!(local_name.as_deref(), Some("input" | "textarea" | "select")) {
        return Ok(false);
    }
    let next = if matches!(value, Value::Null | Value::Undefined) {
        Value::str("")
    } else {
        Value::str(ctx.coerce_string(value).map_err(OpError::thrown)?)
    };
    ctx.set_member(&element, "value", next)
        .map_err(|_| OpError::new("TypeError", "form control value write failed"))?;
    Ok(true)
}

fn catch_at_boundary(
    ctx: &mut Ctx,
    mut owner: Option<Rc<Scope>>,
    mut error: OpError,
) -> OpResult<Value> {
    while let Some(scope) = owner {
        if let Some(boundary) = &scope.boundary {
            let value = error.to_value(ctx);
            match boundary.call(ctx, Value::Undefined, &[value]) {
                Ok(result) => return Ok(result),
                Err(next) => error = next,
            }
        }
        owner = scope.parent.upgrade();
    }
    Err(error)
}

#[lumen_bind::class(name = "Signal")]
pub struct Signal {
    data: Rc<SignalData>,
}

#[lumen_bind::methods]
impl Signal {
    fn get(&self, ctx: &mut Ctx) -> OpResult<Value> {
        self.data.read(ctx)
    }
    fn set(&self, ctx: &mut Ctx, value: Value) -> OpResult<Value> {
        let value = if let Some(update) = JsFunction::from_value(value.clone()) {
            let previous = self.data.value.borrow().clone();
            update.call(ctx, Value::Undefined, &[previous])?
        } else {
            value
        };
        self.data.write(ctx, value.clone());
        Ok(value)
    }
}

#[lumen_bind::class(name = "ReactiveSource")]
pub struct RegionSource {
    props: JsObject,
    property: &'static str,
}
#[lumen_bind::methods]
impl RegionSource {
    fn read(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let value = self.props.get(ctx, self.property)?;
        if let Some(callback) = JsFunction::from_value(value.clone()) {
            callback.call(ctx, Value::Undefined, &[])
        } else {
            Ok(value)
        }
    }
}

#[lumen_bind::class(name = "ReactiveOwner")]
pub struct Owner {
    scope: Rc<Scope>,
}

#[lumen_bind::methods]
impl Owner {
    fn dispose(&self, ctx: &mut Ctx) -> OpResult<()> {
        self.scope.dispose(ctx)
    }
}

#[lumen_bind::class(name = "Effect")]
pub struct EffectHandle {
    effect: Rc<Effect>,
}

#[lumen_bind::methods]
impl EffectHandle {
    fn dispose(&self, ctx: &mut Ctx) -> OpResult<()> {
        self.effect.dispose(ctx)
    }
}

pub(crate) fn bound(ctx: &mut Ctx, object: &Value, method: &str) -> OpResult<Value> {
    let function = ctx
        .get_member(object, method)
        .map_err(|_| OpError::new("TypeError", "native runtime method missing"))?;
    let bind = ctx
        .get_member(&function, "bind")
        .map_err(|_| OpError::new("TypeError", "function bind missing"))?;
    ctx.invoke(bind, function, &[object.clone()])
        .map_err(OpError::thrown)
}

#[lumen_bind::module(name = "lumen")]
pub mod api {
    use super::*;
    #[op(name = "For")]
    pub fn for_each(ctx: &mut Ctx, props: JsObject) -> OpResult<Value> {
        let children = props.get(ctx, "children")?;
        let render = JsFunction::from_value(children)
            .ok_or_else(|| OpError::new("TypeError", "For.children must be a function"))?;
        let source = ctx.new_instance(RegionSource {
            props,
            property: "each",
        });
        let source = JsFunction::from_value(bound(ctx, &source, "read")?).expect("bound source");
        region(ctx, source, RegionKind::For(render))
    }
    #[op(name = "Show")]
    pub fn show(ctx: &mut Ctx, props: JsObject) -> OpResult<Value> {
        let children = props.get(ctx, "children")?;
        let fallback = props.get(ctx, "fallback")?;
        let source = ctx.new_instance(RegionSource {
            props,
            property: "when",
        });
        let source = JsFunction::from_value(bound(ctx, &source, "read")?).expect("bound source");
        region(ctx, source, RegionKind::Show { children, fallback })
    }
    #[op]
    pub fn jsx(
        ctx: &mut Ctx,
        tag: Value,
        props: Option<JsObject>,
        _key: Option<Value>,
    ) -> OpResult<Value> {
        crate::jsx::jsx(ctx, tag, props)
    }
    #[op]
    pub fn jsxs(
        ctx: &mut Ctx,
        tag: Value,
        props: Option<JsObject>,
        _key: Option<Value>,
    ) -> OpResult<Value> {
        crate::jsx::jsx(ctx, tag, props)
    }
    #[op(name = "jsxDEV")]
    pub fn jsx_dev(
        ctx: &mut Ctx,
        tag: Value,
        props: Option<JsObject>,
        _key: Option<Value>,
        #[varargs] _metadata: Vec<Value>,
    ) -> OpResult<Value> {
        crate::jsx::jsx(ctx, tag, props)
    }
    #[op(name = "Fragment")]
    pub fn fragment(ctx: &mut Ctx, props: Option<JsObject>) -> OpResult<Value> {
        crate::jsx::fragment(ctx, props)
    }
    #[op]
    pub fn template(ctx: &mut Ctx, markup: &str) -> OpResult<templates::Template> {
        templates::template(ctx, markup)
    }
    #[op]
    pub fn instantiate(ctx: &mut Ctx, template: &templates::Template) -> OpResult<Value> {
        templates::instantiate(ctx, template)
    }
    #[op(name = "nodeAt")]
    pub fn node_at(ctx: &mut Ctx, root: &DomNode, path: Vec<usize>) -> OpResult<Value> {
        templates::node_at(ctx, root, &path)
    }
    #[op]
    pub fn signal(ctx: &mut Ctx, initial: Option<Value>) -> OpResult<Value> {
        let runtime = runtime(ctx)?;
        let signal = ctx.new_instance(Signal {
            data: Rc::new(SignalData {
                runtime: Rc::downgrade(&runtime),
                value: RefCell::new(initial.unwrap_or(Value::Undefined)),
                subscribers: RefCell::new(Vec::new()),
                producer: RefCell::new(None),
            }),
        });
        let get = bound(ctx, &signal, "get")?;
        let set = bound(ctx, &signal, "set")?;
        Ok(JsHost::from_list(ctx, vec![get, set]))
    }
    #[op]
    pub fn effect(ctx: &mut Ctx, callback: JsFunction) -> OpResult<EffectHandle> {
        let runtime = runtime(ctx)?;
        let effect = runtime.effect(callback, Output::None)?;
        runtime.enqueue(ctx, effect.clone());
        Ok(EffectHandle { effect })
    }
    #[op]
    pub fn memo(ctx: &mut Ctx, callback: JsFunction) -> OpResult<Value> {
        let runtime = runtime(ctx)?;
        let data = Rc::new(SignalData {
            runtime: Rc::downgrade(&runtime),
            value: RefCell::new(Value::Undefined),
            subscribers: RefCell::new(Vec::new()),
            producer: RefCell::new(None),
        });
        let producer = runtime.effect(callback, Output::Signal(data.clone()))?;
        *data.producer.borrow_mut() = Some(Rc::downgrade(&producer));
        producer.run(ctx)?;
        let signal = ctx.new_instance(Signal { data });
        bound(ctx, &signal, "get")
    }
    #[op]
    pub fn batch(ctx: &mut Ctx, callback: JsFunction) -> OpResult<Value> {
        let runtime = runtime(ctx)?;
        runtime.batching.set(runtime.batching.get() + 1);
        let result = callback.call(ctx, Value::Undefined, &[]);
        runtime.batching.set(runtime.batching.get() - 1);
        runtime.schedule(ctx);
        result
    }
    #[op]
    pub fn untrack(ctx: &mut Ctx, callback: JsFunction) -> OpResult<Value> {
        let runtime = runtime(ctx)?;
        let old = runtime.active.replace(None);
        let result = callback.call(ctx, Value::Undefined, &[]);
        runtime.active.replace(old);
        result
    }
    #[op(name = "onCleanup")]
    pub fn on_cleanup(ctx: &mut Ctx, callback: JsFunction) -> OpResult<()> {
        let runtime = runtime(ctx)?;
        let owner = runtime.owner.borrow().clone();
        if owner.disposed.get() {
            return Err(OpError::new(
                "InvalidStateError",
                "cannot register cleanup in a disposed reactive owner",
            ));
        }
        owner.cleanups.borrow_mut().push(callback);
        Ok(())
    }
    #[op(name = "bindChild")]
    pub fn bind_child(
        ctx: &mut Ctx,
        node: &DomNode,
        callback: JsFunction,
    ) -> OpResult<EffectHandle> {
        let runtime = runtime(ctx)?;
        let realm = runtime
            .realm
            .upgrade()
            .ok_or_else(|| OpError::new("Error", "DOM realm was dropped"))?;
        if !Rc::ptr_eq(&realm, &node.realm) {
            return Err(OpError::new(
                "TypeError",
                "child marker belongs to a different realm",
            ));
        }
        if !matches!(
            realm.session.borrow().document().kind(node.id),
            Ok(NodeKind::Comment(_))
        ) {
            return Err(OpError::new(
                "TypeError",
                "bindChild requires a comment marker",
            ));
        }
        let region = Rc::new(Region {
            realm: realm.clone(),
            marker: node.id,
            _marker: RetainedNodes::new(realm, vec![node.id]),
            entries: RefCell::new(Vec::new()),
            kind: RegionKind::Child,
        });
        let effect = runtime.effect(callback, Output::Region(region))?;
        if let Err(error) = effect.run(ctx) {
            let _ = effect.dispose(ctx);
            return Err(error);
        }
        Ok(EffectHandle { effect })
    }
    #[op(name = "bindText")]
    pub fn bind_text(
        ctx: &mut Ctx,
        node: &DomNode,
        callback: JsFunction,
    ) -> OpResult<EffectHandle> {
        let runtime = runtime(ctx)?;
        let effect = runtime.effect(callback, Output::Text(node.realm.clone(), node.id))?;
        effect.run(ctx)?;
        Ok(EffectHandle { effect })
    }
    #[op(name = "bindAttribute")]
    pub fn bind_attribute(
        ctx: &mut Ctx,
        node: &DomNode,
        name: &str,
        callback: JsFunction,
    ) -> OpResult<EffectHandle> {
        let runtime = runtime(ctx)?;
        let effect = runtime.effect(
            callback,
            Output::Attribute(node.realm.clone(), node.id, name.into()),
        )?;
        effect.run(ctx)?;
        Ok(EffectHandle { effect })
    }
    #[op(name = "createRoot")]
    pub fn create_root(ctx: &mut Ctx, callback: JsFunction) -> OpResult<Value> {
        let runtime = runtime(ctx)?;
        let parent = runtime.owner.borrow().clone();
        if parent.disposed.get() {
            return Err(OpError::new(
                "InvalidStateError",
                "cannot create a root in a disposed reactive owner",
            ));
        }
        let scope = Scope::new(Rc::downgrade(&runtime), Rc::downgrade(&parent), None);
        parent.children.borrow_mut().push(scope.clone());
        let owner = ctx.new_instance(Owner {
            scope: scope.clone(),
        });
        let dispose = bound(ctx, &owner, "dispose")?;
        let old_owner = runtime.owner.replace(scope.clone());
        let old_active = runtime.active.replace(None);
        let result = callback.call(ctx, Value::Undefined, &[dispose]);
        runtime.owner.replace(old_owner);
        runtime.active.replace(old_active);
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                let _ = scope.dispose(ctx);
                Err(error)
            }
        }
    }
    #[op(name = "errorBoundary")]
    pub fn error_boundary(
        ctx: &mut Ctx,
        callback: JsFunction,
        handler: JsFunction,
    ) -> OpResult<Value> {
        let runtime = runtime(ctx)?;
        let parent = runtime.owner.borrow().clone();
        if parent.disposed.get() {
            return Err(OpError::new(
                "InvalidStateError",
                "cannot create an error boundary in a disposed reactive owner",
            ));
        }
        let scope = Scope::new(
            Rc::downgrade(&runtime),
            Rc::downgrade(&parent),
            Some(handler.clone()),
        );
        parent.children.borrow_mut().push(scope.clone());
        let old_owner = runtime.owner.replace(scope.clone());
        let old_active = runtime.active.replace(None);
        let result = callback.call(ctx, Value::Undefined, &[]);
        runtime.active.replace(old_active);
        runtime.owner.replace(old_owner);
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                let _ = scope.dispose(ctx);
                catch_at_boundary(ctx, Some(scope), error)
            }
        }
    }
    #[op(name = "__flush")]
    pub fn flush(ctx: &mut Ctx) -> OpResult<()> {
        let runtime = runtime(ctx)?;
        runtime.scheduled.set(false);
        runtime.flushing.set(true);
        let mut runs = 0;
        let result = (|| {
            loop {
                let effect = {
                    let mut pending = runtime.pending.borrow_mut();
                    if let Some(index) = pending
                        .iter()
                        .position(|effect| matches!(effect.output, Output::Signal(_)))
                    {
                        pending.remove(index)
                    } else {
                        pending.pop_front()
                    }
                };
                let Some(effect) = effect else {
                    break;
                };
                if !effect.queued.get() {
                    continue;
                }
                runs += 1;
                if runs > 100_000 {
                    for effect in runtime.pending.borrow_mut().drain(..) {
                        effect.queued.set(false);
                    }
                    return Err(OpError::new("RangeError", "reactive update limit exceeded"));
                }
                effect.run(ctx)?;
            }
            Ok(())
        })();
        runtime.flushing.set(false);
        runtime.schedule(ctx);
        result
    }
}

pub(crate) fn install(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<Value> {
    let runtime = Runtime::new(realm);
    crate::realm_services::RealmServices::replace_shared_current(ctx, runtime.clone());
    ctx.class_constructor::<Signal>();
    ctx.class_constructor::<Owner>();
    ctx.class_constructor::<EffectHandle>();
    ctx.class_constructor::<templates::Template>();
    ctx.class_constructor::<RegionSource>();
    let module = ctx
        .module_object::<api::Module>()
        .map_err(OpError::thrown)?;
    let flush = ctx
        .get_member(&module, "__flush")
        .map_err(|_| OpError::new("Error", "reactive scheduler missing"))?;
    *runtime.scheduler.borrow_mut() = Some(crate::realm_services::capture_realm_value(ctx, flush)?);
    Ok(module)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    #[test]
    fn specification_reactive_scheduler_uses_origin_realm_without_pinning_retired_globals() {
        let mut engine=Engine::new();
        let parent=crate::install(engine.ctx(),"<main></main>",128).unwrap();
        let parent_runtime=runtime(engine.ctx()).expect("parent runtime");
        let child=engine.ctx().create_host_realm();
        let (child_runtime,weak_global,weak_document,retired)=engine.ctx().with_host_realm(&child,|ctx| {
            let document=crate::install(ctx,"<main></main>",128).unwrap();
            let selected=runtime(ctx).expect("child runtime");
            assert!(Rc::ptr_eq(&selected.realm.upgrade().unwrap(),&document));
            assert!(!Rc::ptr_eq(&selected,&parent_runtime));
            let global=ctx.global_object();
            let weak_global=ctx.weak_value(&global).expect("child global");
            let retired=document.retire_browsing_context_group(ctx);
            (selected,weak_global,Rc::downgrade(&document),retired)
        }).expect("child installation");
        assert!(Rc::ptr_eq(&runtime(engine.ctx()).unwrap(),&parent_runtime));
        assert!(Rc::ptr_eq(&parent_runtime.realm.upgrade().unwrap(),&parent));
        for handle in retired {engine.ctx().dispose_host_realm(&handle).expect("dispose retired browser realm");}
        drop(child);
        engine.collect_garbage();
        engine.collect_garbage();
        assert!(weak_global.upgrade().is_none(),"retained native scheduling metadata must not root the old global");
        assert!(weak_document.upgrade().is_none(),"unused reactive scheduler must not pin its document");
        assert!(child_runtime.scheduler.borrow().as_ref().and_then(WeakValue::upgrade).is_none());
    }
}
