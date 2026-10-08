use super::*;
use crate::realm_services::RealmServices;
use lumen::embed::{JsFunction, JsHost};
use lumen_bind::{Host, This};
use lumen_html::observe::{ObservedKind, ObservedMutation};
use std::rc::Weak;

#[derive(Clone)]
struct Options {
    subtree: bool,
    children: bool,
    attributes: bool,
    character: bool,
    attribute_old: bool,
    character_old: bool,
    filter: Option<Vec<String>>,
}

struct ObserverOptions(Options);

impl<'a> lumen_bind::FromArg<'a, JsHost> for ObserverOptions {
    fn from_arg(
        cx: &'a lumen::embed::ArgCx<'_>,
        value: &'a Value,
        _: lumen_bind::Slot,
    ) -> Result<Self, Value> {
        cx.with_ctx(|ctx| {
            let convert = |ctx: &mut Ctx| -> OpResult<Self> {
                if !matches!(value, Value::Obj(_) | Value::Undefined | Value::Null) {
                    return Err(OpError::type_error(
                        "MutationObserverInit requires an object",
                    ));
                }
                let dictionary = Some(value.clone());
                // Web IDL dictionary members are read and converted in
                // lexicographic order, before the operation starts.
                let filter =
                    crate::ui_events::dictionary_member(ctx, &dictionary, "attributeFilter")?
                        .map(|value| {
                            ctx.convert_iterable(&value, 65_536, |ctx, item| {
                                ctx.coerce_string(&item)
                                    .map(|text| text.to_string())
                                    .map_err(OpError::thrown)
                            })
                        })
                        .transpose()?;
                let attribute_old =
                    crate::ui_events::dictionary_member(ctx, &dictionary, "attributeOldValue")?
                        .map(|value| ctx.to_boolean(&value));
                let attributes =
                    crate::ui_events::dictionary_member(ctx, &dictionary, "attributes")?
                        .map(|value| ctx.to_boolean(&value));
                let character =
                    crate::ui_events::dictionary_member(ctx, &dictionary, "characterData")?
                        .map(|value| ctx.to_boolean(&value));
                let character_old =
                    crate::ui_events::dictionary_member(ctx, &dictionary, "characterDataOldValue")?
                        .map(|value| ctx.to_boolean(&value));
                let children =
                    crate::ui_events::dictionary_boolean(ctx, &dictionary, "childList", false)?;
                let subtree =
                    crate::ui_events::dictionary_boolean(ctx, &dictionary, "subtree", false)?;
                Ok(Self(Options {
                    subtree,
                    children,
                    attributes: attributes.unwrap_or(attribute_old.is_some() || filter.is_some()),
                    character: character.unwrap_or(character_old.is_some()),
                    attribute_old: attribute_old.unwrap_or(false),
                    character_old: character_old.unwrap_or(false),
                    filter,
                }))
            };
            convert(ctx).map_err(|error| error.to_value(ctx))
        })
    }
}
struct Registration {
    realm: Weak<DomRealm>,
    target: NodeId,
    options: Options,
    source: Rc<()>,
}
struct Pending {
    realm: Rc<DomRealm>,
    mutation: ObservedMutation,
    old: bool,
    _keep: DomNodeList,
}
pub(crate) struct ObserverData {
    hub: Weak<Hub>,
    callback: JsFunction,
    registrations: RefCell<Vec<Registration>>,
    transient: RefCell<Vec<Registration>>,
    pending: RefCell<Vec<Pending>>,
    wrapper: RefCell<Option<WeakValue>>,
}

struct PendingObserver {
    data: Rc<ObserverData>,
    wrapper: Value,
}

struct Hub {
    attached_realms: RefCell<Vec<Weak<DomRealm>>>,
    pending_observers: RefCell<Vec<PendingObserver>>,
    // Deferred jobs are agent-wide; each queued delivery function retains its owning realm.
    jobs: Rc<RefCell<Vec<Value>>>,
    delivery: RefCell<Option<Value>>,
    scheduled: Cell<bool>,
    slot_assignments: RefCell<Vec<(Weak<DomRealm>, Vec<(NodeId, Vec<NodeId>)>)>>,
    signal_slots: RefCell<Vec<(Rc<DomRealm>, NodeId, DomNodeList)>>,
}

struct ObserverAgent(Rc<Hub>);

fn within(document: &lumen_html::Document, target: NodeId, root: NodeId, subtree: bool) -> bool {
    if target == root {
        return true;
    }
    if !subtree {
        return false;
    }
    let mut node = target;
    while let Ok(Some(parent)) = document.parent(node) {
        if parent == root {
            return true;
        }
        node = parent;
    }
    false
}

fn same_realm(registration: &Registration, realm: &Rc<DomRealm>) -> bool {
    registration.realm.ptr_eq(&Rc::downgrade(realm))
}

fn index_registration(realm: &Rc<DomRealm>, node: NodeId, observer: &Rc<ObserverData>) {
    let mut index = realm.observer_registrations.borrow_mut();
    let observers = index.entry(node).or_default();
    observers.retain(|weak| weak.strong_count() > 0);
    let weak = Rc::downgrade(observer);
    if !observers.iter().any(|entry| entry.ptr_eq(&weak)) {
        observers.push(weak);
    }
}

fn refresh_index(observer: &Rc<ObserverData>, registration: &Registration) {
    let Some(realm) = registration.realm.upgrade() else {
        return;
    };
    let present = observer
        .registrations
        .borrow()
        .iter()
        .chain(observer.transient.borrow().iter())
        .any(|entry| entry.target == registration.target && same_realm(entry, &realm));
    if present {
        return;
    }
    let mut index = realm.observer_registrations.borrow_mut();
    if let Some(observers) = index.get_mut(&registration.target) {
        let weak = Rc::downgrade(observer);
        observers.retain(|entry| !entry.ptr_eq(&weak) && entry.strong_count() > 0);
        if observers.is_empty() {
            index.remove(&registration.target);
        }
    }
}

fn clear_transient(observer: &Rc<ObserverData>) {
    let registrations = std::mem::take(&mut *observer.transient.borrow_mut());
    for registration in &registrations {
        refresh_index(observer, registration);
    }
}

pub(crate) fn trace_node_observers(realm: &DomRealm, node: NodeId, visit: &mut dyn FnMut(&Value)) {
    if let Some(observers) = realm.observer_registrations.borrow().get(&node) {
        for observer in observers.iter().filter_map(Weak::upgrade) {
            if let Some(wrapper) = observer
                .wrapper
                .borrow()
                .as_ref()
                .and_then(WeakValue::upgrade)
            {
                visit(&wrapper);
            }
        }
    }
}

/// Reuse the realm's amortized sparse-table retirement sweep. Generation-bearing
/// IDs prevent recycled arena slots from inheriting old subscriptions.
pub(crate) fn reap_registrations(realm: &DomRealm, document: &lumen_html::Document) {
    realm
        .observer_registrations
        .borrow_mut()
        .retain(|node, observers| {
            if document.kind(*node).is_err() {
                for observer in observers.iter().filter_map(Weak::upgrade) {
                    let prune = |registrations: &mut Vec<Registration>| {
                        registrations.retain(|entry| {
                            entry.target != *node
                                || entry
                                    .realm
                                    .upgrade()
                                    .is_some_and(|owner| !std::ptr::eq(owner.as_ref(), realm))
                        })
                    };
                    prune(&mut observer.registrations.borrow_mut());
                    prune(&mut observer.transient.borrow_mut());
                }
                return false;
            }
            observers.retain(|observer| observer.strong_count() > 0);
            !observers.is_empty()
        });
}

impl Hub {
    fn queue_observer(&self, observer: &Rc<ObserverData>) {
        let mut pending = self.pending_observers.borrow_mut();
        if pending
            .iter()
            .any(|entry| Rc::ptr_eq(&entry.data, observer))
        {
            return;
        }
        if let Some(wrapper) = observer
            .wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
        {
            pending.push(PendingObserver {
                data: observer.clone(),
                wrapper,
            });
        }
    }
    fn schedule(&self) {
        if !self.scheduled.replace(true) {
            if let Some(callback) = self.delivery.borrow().as_ref() {
                self.jobs.borrow_mut().push(callback.clone());
            }
        }
    }

    // Assignment lists are snapshots of node identity, not flattened contents:
    // editing a slotted text node does not signal its slot (DOM §4.2.2.4).
    fn capture_slots(
        &self,
        realm: &Rc<DomRealm>,
        document: &lumen_html::Document,
        mutation: &ObservedMutation,
    ) {
        match &mutation.kind {
            ObservedKind::ChildList { .. } | ObservedKind::ChildListMany { .. } => (),
            ObservedKind::SlotAssignment
            | ObservedKind::TextSplit { .. }
            | ObservedKind::TextMerge { .. } => (),
            ObservedKind::Attribute {
                name,
                namespace_uri,
                ..
            } if namespace_uri.is_none() && (name == "slot" || name == "name") => (),
            _ => return,
        }
        let mut current = Vec::new();
        for (_, root, _) in document.shadow_roots() {
            if let Ok(slots) = lumen_html::selector::query_selector_all(document, root, "slot") {
                for slot in slots {
                    if let Ok(nodes) = document.assigned_nodes(slot, false) {
                        current.push((slot, nodes));
                    }
                }
            }
        }
        let mut assignments = self.slot_assignments.borrow_mut();
        assignments.retain(|(owner, _)| owner.strong_count() > 0);
        let owner = Rc::downgrade(realm);
        let index = assignments
            .iter()
            .position(|(entry, _)| entry.ptr_eq(&owner))
            .unwrap_or_else(|| {
                assignments.push((owner, Vec::new()));
                assignments.len() - 1
            });
        let old = &mut assignments[index].1;
        let mut signals = self.signal_slots.borrow_mut();
        let mut signal = |slot| {
            if !signals.iter().any(|(_, id, _)| *id == slot) {
                signals.push((
                    realm.clone(),
                    slot,
                    DomNodeList::snapshot(realm.clone(), vec![slot], Value::Undefined),
                ));
            }
        };
        for (slot, nodes) in &current {
            let previous = old
                .iter()
                .find(|(id, _)| id == slot)
                .map(|(_, nodes)| nodes.as_slice())
                .unwrap_or(&[]);
            if previous != nodes.as_slice() {
                signal(*slot);
            }
            // Changes to the fallback child list signal only an unassigned slot.
            if *slot == mutation.target
                && nodes.is_empty()
                && matches!(
                    mutation.kind,
                    ObservedKind::ChildList { .. }
                        | ObservedKind::ChildListMany { .. }
                        | ObservedKind::ChildListReplacement { .. }
                )
            {
                signal(*slot);
            }
        }
        for (slot, nodes) in old.iter() {
            if !nodes.is_empty()
                && !current.iter().any(|(id, _)| id == slot)
                && document.kind(*slot).is_ok()
            {
                signal(*slot);
            }
        }
        *old = current;
        let queued = !signals.is_empty();
        drop(signals);
        drop(assignments);
        if queued {
            self.schedule();
        }
    }

    fn capture(
        &self,
        realm: &Rc<DomRealm>,
        document: &lumen_html::Document,
        mutation: &ObservedMutation,
    ) {
        // Match registered observers through inclusive ancestors, rather
        // than scan every observer in the agent for every mutation.
        let mut observers = Vec::<Rc<ObserverData>>::new();
        {
            let mut index = realm.observer_registrations.borrow_mut();
            let mut ancestor = Some(mutation.target);
            while let Some(node) = ancestor {
                if let Some(entries) = index.get_mut(&node) {
                    entries.retain(|observer| observer.strong_count() > 0);
                    for observer in entries.iter().filter_map(Weak::upgrade) {
                        if !std::ptr::eq(observer.hub.as_ptr(), self) {
                            continue;
                        }
                        if !observers.iter().any(|entry| Rc::ptr_eq(entry, &observer)) {
                            observers.push(observer);
                        }
                    }
                }
                ancestor = document.parent(node).ok().flatten();
            }
        }
        let mut queued = false;
        for observer in observers {
            let registrations = observer.registrations.borrow();
            let transient = observer.transient.borrow();
            let matching = registrations
                .iter()
                .chain(transient.iter())
                .filter(|registration| same_realm(registration, realm))
                .filter(|registration| {
                    within(
                        document,
                        mutation.target,
                        registration.target,
                        registration.options.subtree,
                    )
                });
            let mut selected = false;
            let mut old = false;
            let mut removed_options = Vec::new();
            for registration in matching {
                let options = &registration.options;
                if mutation.kind.removed_nodes().next().is_some() {
                    if options.subtree {
                        removed_options.push((options.clone(), registration.source.clone()));
                    }
                }
                match &mutation.kind {
                    ObservedKind::Attribute {
                        name,
                        namespace_uri,
                        ..
                    } if options.attributes
                        && options.filter.as_ref().is_none_or(|filter| {
                            namespace_uri.is_none() && filter.contains(name)
                        }) =>
                    {
                        selected = true;
                        old |= options.attribute_old;
                    }
                    ObservedKind::CharacterData { .. } if options.character => {
                        selected = true;
                        old |= options.character_old;
                    }
                    ObservedKind::ChildList { .. }
                    | ObservedKind::ChildListMany { .. }
                    | ObservedKind::ChildListReplacement { .. }
                        if options.children =>
                    {
                        selected = true
                    }
                    _ => {}
                }
            }
            drop(transient);
            drop(registrations);
            for node in mutation.kind.removed_nodes() {
                let mut transient = observer.transient.borrow_mut();
                for (options, source) in &removed_options {
                    if !transient.iter().any(|entry| {
                        entry.target == node
                            && same_realm(entry, realm)
                            && Rc::ptr_eq(&entry.source, source)
                    }) {
                        transient.push(Registration {
                            realm: Rc::downgrade(realm),
                            target: node,
                            options: options.clone(),
                            source: source.clone(),
                        });
                        index_registration(realm, node, &observer);
                    }
                }
            }
            if !selected {
                continue;
            }
            let mut ids = vec![mutation.target];
            if let ObservedKind::ChildList {
                added,
                removed,
                previous_sibling,
                next_sibling,
            } = mutation.kind
            {
                ids.extend(
                    [added, removed, previous_sibling, next_sibling]
                        .into_iter()
                        .flatten(),
                );
            }
            if let ObservedKind::ChildListMany { added, removed } = &mutation.kind {
                ids.extend(added.iter().chain(removed).copied());
            }
            if let ObservedKind::ChildListReplacement {
                added,
                removed,
                previous_sibling,
                next_sibling,
            } = &mutation.kind
            {
                ids.extend(added.iter().chain(removed).copied());
                ids.extend([*previous_sibling, *next_sibling].into_iter().flatten());
            }
            observer.pending.borrow_mut().push(Pending {
                realm: realm.clone(),
                mutation: mutation.clone(),
                old,
                _keep: DomNodeList::snapshot(realm.clone(), ids, Value::Undefined),
            });
            self.queue_observer(&observer);
            queued = true;
        }
        if queued {
            self.schedule();
        }
    }
}

#[lumen_bind::class(name = "MutationRecord", hint(js(webidl)))]
pub(crate) struct DomMutationRecord {
    kind: &'static str,
    target: Value,
    added: Value,
    removed: Value,
    previous: Value,
    next: Value,
    attribute: Option<String>,
    attribute_namespace: Option<String>,
    old: Option<String>,
}

impl lumen::embed::NativeIdentityOwner for DomMutationRecord {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _epoch: u64, visit: &mut dyn FnMut(&Value)) {
        // These are the actual existing wrappers. The runtime follows their
        // native Node/NodeList owner graphs with the same collection epoch.
        self.trace_native_values(visit);
    }
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        for value in [&self.target, &self.added, &self.removed, &self.previous, &self.next] { visit(value); }
    }
}

#[lumen_bind::methods]
impl DomMutationRecord {
    #[getter(name = "type")]
    fn record_type(&self) -> &str { self.kind }
    #[getter]
    fn target(&self) -> Value { self.target.clone() }
    #[getter]
    fn added_nodes(&self) -> Value { self.added.clone() }
    #[getter]
    fn removed_nodes(&self) -> Value { self.removed.clone() }
    #[getter]
    fn previous_sibling(&self) -> Value { self.previous.clone() }
    #[getter]
    fn next_sibling(&self) -> Value { self.next.clone() }
    #[getter]
    fn attribute_name(&self) -> lumen::embed::Nullable<&str> { lumen::embed::Nullable(self.attribute.as_deref()) }
    #[getter]
    fn attribute_namespace(&self) -> lumen::embed::Nullable<&str> { lumen::embed::Nullable(self.attribute_namespace.as_deref()) }
    #[getter]
    fn old_value(&self) -> lumen::embed::Nullable<&str> { lumen::embed::Nullable(self.old.as_deref()) }
}

fn records(ctx: &mut Ctx, observer: &ObserverData) -> OpResult<Vec<Value>> {
    let pending = std::mem::take(&mut *observer.pending.borrow_mut());
    let mut records = Vec::with_capacity(pending.len());
    for pending in pending {
        let realm = &pending.realm;
        let (target_realm, target_id) = realm.resolve_adopted_node(pending.mutation.target);
        let target = target_realm.wrap(ctx, target_id);
        let (kind, attribute, attribute_namespace, old, added, removed, previous, next) =
            match pending.mutation.kind {
                ObservedKind::Attribute {
                    name,
                    namespace_uri,
                    old_value,
                } => {
                    let attribute_name = if namespace_uri.is_some() {
                        name.rsplit_once(':')
                            .map_or(name.as_str(), |(_, local_name)| local_name)
                            .to_owned()
                    } else {
                        name
                    };
                    (
                        "attributes",
                        Some(attribute_name),
                        namespace_uri,
                        if pending.old { old_value } else { None },
                        Vec::new(),
                        Vec::new(),
                        None,
                        None,
                    )
                }
                ObservedKind::CharacterData { old_value, .. } => (
                    "characterData",
                    None,
                    None,
                    pending.old.then_some(old_value),
                    Vec::new(),
                    Vec::new(),
                    None,
                    None,
                ),
                ObservedKind::ChildList {
                    added,
                    removed,
                    previous_sibling,
                    next_sibling,
                } => (
                    "childList",
                    None,
                    None,
                    None,
                    added.into_iter().collect(),
                    removed.into_iter().collect(),
                    previous_sibling,
                    next_sibling,
                ),
                ObservedKind::ChildListMany { added, removed } => {
                    ("childList", None, None, None, added, removed, None, None)
                }
                ObservedKind::ChildListReplacement {
                    added,
                    removed,
                    previous_sibling,
                    next_sibling,
                } => (
                    "childList",
                    None,
                    None,
                    None,
                    added,
                    removed,
                    previous_sibling,
                    next_sibling,
                ),
                // Never selected for an observer above.
                ObservedKind::SlotAssignment
                | ObservedKind::TextSplit { .. }
                | ObservedKind::TextMerge { .. } => continue,
            };
        let added = ctx.new_instance(DomNodeList::snapshot(
            realm.clone(),
            added.into_iter().collect(),
            Value::Undefined,
        ));
        ctx.set_native_identity_owner::<DomNodeList>(&added)?;
        let removed = ctx.new_instance(DomNodeList::snapshot(
            realm.clone(),
            removed.into_iter().collect(),
            Value::Undefined,
        ));
        ctx.set_native_identity_owner::<DomNodeList>(&removed)?;
        let wrap = |ctx: &mut Ctx, node: Option<NodeId>| {
            node.map_or(Value::Null, |node| {
                let (owner, node) = realm.resolve_adopted_node(node);
                owner.wrap(ctx, node)
            })
        };
        let previous = wrap(ctx, previous);
        let next = wrap(ctx, next);
        let object = ctx.new_instance(DomMutationRecord { kind, target, added, removed, previous, next, attribute, attribute_namespace, old });
        ctx.set_native_identity_owner::<DomMutationRecord>(&object)?;
        records.push(object);
    }
    Ok(records)
}

#[lumen_bind::class(name = "MutationObserver", hint(js(webidl)))]
pub struct DomMutationObserver {
    hub: Rc<Hub>,
    data: Rc<ObserverData>,
}

impl lumen::embed::NativeIdentityOwner for DomMutationObserver {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        visit(self.data.callback.value());
    }
    fn trace_native_identities(&self, epoch: u64, visit: &mut dyn FnMut(&Value)) {
        // Records retain native nodes strongly; the observer's node list does
        // not. Reuse the collection's existing adoption-aware identity walk.
        for pending in self.data.pending.borrow().iter() {
            let _ = pending._keep.visit_entries(|realm, document, node| {
                if let Ok(root) = selector::native_identity_root(document, node) {
                    realm.trace_native_identity_component(epoch, root, visit);
                }
                true
            });
        }
    }
}

struct ObserverConstructorResult(DomMutationObserver);
impl lumen_bind::CtorRet<JsHost, DomMutationObserver> for ObserverConstructorResult {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        let data = self.0.data.clone();
        let value = <JsHost as Host>::construct(cx, self.0)?;
        <JsHost as Host>::with_ctx(cx, |ctx| {
            *data.wrapper.borrow_mut() = ctx.weak_value(&value);
            ctx.set_native_identity_owner::<DomMutationObserver>(&value)
                .expect("MutationObserver native brand");
        });
        Ok(value)
    }
}

#[lumen_bind::methods]
impl DomMutationObserver {
    #[constructor]
    fn new(ctx: &mut Ctx, callback: JsFunction) -> OpResult<ObserverConstructorResult> {
        let hub = RealmServices::<Hub>::current(ctx)
            .ok_or_else(|| OpError::new("Error", "DOM observers are not installed"))?;
        let data = Rc::new(ObserverData {
            hub: Rc::downgrade(&hub),
            callback,
            registrations: RefCell::new(Vec::new()),
            transient: RefCell::new(Vec::new()),
            pending: RefCell::new(Vec::new()),
            wrapper: RefCell::new(None),
        });
        Ok(ObserverConstructorResult(Self { hub, data }))
    }
    fn observe(
        &self,
        ctx: &mut Ctx,
        this: This<Value>,
        target: DomNodeIdentity,
        options: ObserverOptions,
    ) -> OpResult<()> {
        let (target_realm, target_id) =
            ctx.with_instance::<DomNode, _>(&target.value, |node| (node.realm.clone(), node.id))?;
        let options = options.0;
        if (!options.attributes && (options.attribute_old || options.filter.is_some()))
            || (!options.character && options.character_old)
            || !(options.attributes || options.character || options.children)
        {
            return Err(OpError::type_error("invalid observer options"));
        }
        let mut registrations = self.data.registrations.borrow_mut();
        let source = if let Some(registration) = registrations.iter_mut().find(|registration| {
            registration.target == target_id && same_realm(registration, &target_realm)
        }) {
            registration.options = options;
            Some(registration.source.clone())
        } else {
            registrations.push(Registration {
                realm: Rc::downgrade(&target_realm),
                target: target_id,
                options,
                source: Rc::new(()),
            });
            None
        };
        drop(registrations);
        if let Some(source) = source {
            let mut transient = self.data.transient.borrow_mut();
            let mut removed = Vec::new();
            let mut index = 0;
            while index < transient.len() {
                if Rc::ptr_eq(&transient[index].source, &source) {
                    removed.push(transient.remove(index));
                } else {
                    index += 1;
                }
            }
            drop(transient);
            for registration in &removed {
                refresh_index(&self.data, registration);
            }
        }
        index_registration(&target_realm, target_id, &self.data);
        attach_hub(&self.hub, &target_realm);
        *self.data.wrapper.borrow_mut() = ctx.weak_value(&this.0);
        Ok(())
    }
    fn disconnect(&self) {
        let registrations = std::mem::take(&mut *self.data.registrations.borrow_mut());
        clear_transient(&self.data);
        for registration in &registrations {
            refresh_index(&self.data, registration);
        }
        self.data.pending.borrow_mut().clear();
    }
    fn take_records(&self, ctx: &mut Ctx) -> OpResult<Vec<Value>> {
        records(ctx, &self.data)
    }
}

#[lumen_bind::class(name = "MutationObserverDelivery")]
struct Delivery {
    hub: Rc<Hub>,
}
#[lumen_bind::methods]
impl Delivery {
    fn flush(&self, ctx: &mut Ctx) -> OpResult<()> {
        self.hub.scheduled.set(false);
        // Snapshot the agent's ordered pending set before callbacks. A newly
        // interested observer runs at the next microtask checkpoint.
        let observers = std::mem::take(&mut *self.hub.pending_observers.borrow_mut());
        let slots = std::mem::take(&mut *self.hub.signal_slots.borrow_mut());
        for pending in observers {
            let observer = pending.data;
            clear_transient(&observer);
            if observer.pending.borrow().is_empty() {
                continue;
            }
            let values = records(ctx, &observer)?;
            let values = JsHost::from_list(ctx, values);
            let wrapper = pending.wrapper;
            let callback_realm = ctx
                .function_host_realm(&observer.callback)
                .map_err(OpError::thrown)?;
            ctx.with_host_realm(&callback_realm, |ctx| {
                if let Err(failure) =
                    observer
                        .callback
                        .call(ctx, wrapper.clone(), &[values, wrapper])
                {
                    let exception = failure.to_value(ctx);
                    DomRealm::report_exception(ctx, exception);
                }
            })
            .map_err(browsing_context::host_realm_error)?;
        }
        let mut error = None;
        for (realm, slot, _keep) in slots {
            if realm.session.borrow().document().kind(slot).is_ok() {
                if let Err(failure) = realm.dispatch(ctx, slot, "slotchange", true, false, &[]) {
                    error.get_or_insert(failure);
                }
            }
        }
        error.map_or(Ok(()), Err)
    }
}

pub(crate) fn install(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    // Mutation observer microtask state belongs to the interpreter's agent,
    // while constructors and callback intrinsics remain realm-specific.
    let hub = if let Some(agent) = ctx.op_state().get::<ObserverAgent>() {
        agent.0.clone()
    } else {
        let hub = Rc::new(Hub {
            attached_realms: RefCell::new(Vec::new()),
            pending_observers: RefCell::new(Vec::new()),
            jobs: ctx.deferred_microtasks(),
            delivery: RefCell::new(None),
            scheduled: Cell::new(false),
            slot_assignments: RefCell::new(Vec::new()),
            signal_slots: RefCell::new(Vec::new()),
        });
        ctx.class_constructor::<Delivery>();
        let delivery = ctx.new_instance(Delivery { hub: hub.clone() });
        let flush = ctx
            .get_member(&delivery, "flush")
            .map_err(|_| OpError::new("Error", "observer delivery missing"))?;
        let bind = ctx
            .get_member(&flush, "bind")
            .map_err(|_| OpError::new("Error", "observer binding missing"))?;
        *hub.delivery.borrow_mut() = Some(
            JsFunction::from_value(bind)
                .ok_or_else(|| OpError::new("TypeError", "observer binding invalid"))?
                .call(ctx, flush, &[delivery])?,
        );
        ctx.op_state().put(ObserverAgent(hub.clone()));
        hub
    };
    attach_hub(&hub, realm);
    let weak = Rc::downgrade(&hub);
    let weak_realm = Rc::downgrade(realm);
    realm
        .session
        .borrow_mut()
        .document_mut()
        .set_shadow_mutation_sink(Some(Rc::new(move |document, mutation| {
            if let (Some(hub), Some(realm)) = (weak.upgrade(), weak_realm.upgrade()) {
                hub.capture_slots(&realm, document, mutation);
            }
        })));
    RealmServices::replace_shared_current(ctx, hub);
    let constructor = ctx.class_constructor::<DomMutationObserver>();
    let global = ctx.global_object();
    let record_constructor = ctx.class_constructor::<DomMutationRecord>();
    crate::install_interface(ctx, &global, "MutationRecord", record_constructor)
        .map_err(|_| OpError::new("Error", "mutation record installation failed"))?;
    crate::install_interface(ctx, &global, "MutationObserver", constructor)
        .map_err(|_| OpError::new("Error", "observer installation failed"))
}

pub(crate) fn attach_parsed_realm(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    let hub = RealmServices::<Hub>::current(ctx)
        .ok_or_else(|| OpError::new("Error", "DOM observers are not installed"))?;
    attach_hub(&hub, realm);
    attach_shadow_hub(&hub, realm);
    Ok(())
}

fn attach_hub(hub: &Rc<Hub>, realm: &Rc<DomRealm>) {
    let weak = Rc::downgrade(realm);
    let mut attached = hub.attached_realms.borrow_mut();
    attached.retain(|realm| realm.strong_count() > 0);
    if attached.iter().any(|entry| entry.ptr_eq(&weak)) {
        return;
    }
    attached.push(weak);
    drop(attached);
    let weak_hub = Rc::downgrade(hub);
    let weak_realm = Rc::downgrade(realm);
    realm
        .mutation_observer_sinks
        .borrow_mut()
        .push(Rc::new(move |document, mutation| {
            if let (Some(hub), Some(realm)) = (weak_hub.upgrade(), weak_realm.upgrade()) {
                hub.capture(&realm, document, mutation);
            }
        }));
}

fn attach_shadow_hub(hub: &Rc<Hub>, realm: &Rc<DomRealm>) {
    let weak_hub = Rc::downgrade(hub);
    let weak_realm = Rc::downgrade(realm);
    realm
        .session
        .borrow_mut()
        .document_mut()
        .set_shadow_mutation_sink(Some(Rc::new(move |document, mutation| {
            if let (Some(hub), Some(realm)) = (weak_hub.upgrade(), weak_realm.upgrade()) {
                hub.capture_slots(&realm, document, mutation);
            }
        })));
}

/// Rehome weak subscriptions while preserving the original observer and source
/// registration identities. Pending records keep their adoption-aware leases.
pub(crate) fn adopt_nodes(
    ctx: &mut Ctx,
    source: &Rc<DomRealm>,
    target: &Rc<DomRealm>,
    mapping: &[(NodeId, NodeId)],
) {
    let mut observers = Vec::<Rc<ObserverData>>::new();
    {
        let mut index = source.observer_registrations.borrow_mut();
        for (old, _) in mapping {
            if let Some(entries) = index.remove(old) {
                for observer in entries.into_iter().filter_map(|weak| weak.upgrade()) {
                    if !observers.iter().any(|entry| Rc::ptr_eq(entry, &observer)) {
                        observers.push(observer);
                    }
                }
            }
        }
    }
    for observer in observers {
        let migrate = |registrations: &mut Vec<Registration>| {
            for registration in registrations {
                if !same_realm(registration, source) {
                    continue;
                }
                let Some((_, new)) = mapping.iter().find(|(old, _)| *old == registration.target)
                else {
                    continue;
                };
                registration.realm = Rc::downgrade(target);
                registration.target = *new;
                index_registration(target, *new, &observer);
            }
        };
        migrate(&mut observer.registrations.borrow_mut());
        migrate(&mut observer.transient.borrow_mut());
        if let Some(hub) = observer.hub.upgrade() {
            attach_hub(&hub, target);
        }
    }
    let Some(hub) = RealmServices::<Hub>::current(ctx) else {
        return;
    };
    let mut assignments = hub.slot_assignments.borrow_mut();
    let mut moved = Vec::new();
    if let Some((_, slots)) = assignments
        .iter_mut()
        .find(|(owner, _)| owner.ptr_eq(&Rc::downgrade(source)))
    {
        let mut index = 0;
        while index < slots.len() {
            if let Some((_, new)) = mapping.iter().find(|(old, _)| *old == slots[index].0) {
                let (_, mut children) = slots.remove(index);
                for child in &mut children {
                    if let Some((_, new)) = mapping.iter().find(|(old, _)| old == child) {
                        *child = *new;
                    }
                }
                moved.push((*new, children));
            } else {
                index += 1;
            }
        }
    }
    if !moved.is_empty() {
        if let Some((_, slots)) = assignments
            .iter_mut()
            .find(|(owner, _)| owner.ptr_eq(&Rc::downgrade(target)))
        {
            slots.extend(moved);
        } else {
            assignments.push((Rc::downgrade(target), moved));
        }
    }
    drop(assignments);
    for (owner, node, list) in hub.signal_slots.borrow_mut().iter_mut() {
        if Rc::ptr_eq(owner, source) {
            if let Some((_, new)) = mapping.iter().find(|(old, _)| old == node) {
                *owner = target.clone();
                *node = *new;
                list.adopt_nodes(target.clone(), mapping);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    fn assert_script(ctx: &mut Ctx, source: &str) {
        let global = ctx.global_object();
        assert!(
            matches!(ctx.eval_in_realm(&global, source), Ok(Value::Bool(true))),
            "observer guard failed: {source}"
        );
    }

    #[test]
    fn specification_observer_mutation_record_interface_identity_and_readonly() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        crate::install(ctx, "<main></main>", 128).expect("install document");
        assert_script(ctx, r#"
          const check=(v,m)=>{if(!v)throw new Error(m)}, main=document.querySelector('main'), observer=new MutationObserver(()=>{});
          observer.observe(main,{childList:true,attributes:true,attributeOldValue:true});const child=document.createElement('i');main.appendChild(child);main.setAttribute('title','one');
          const records=observer.takeRecords(), tree=records[0], attr=records[1];
          check(tree instanceof MutationRecord && attr instanceof MutationRecord && tree.type==='childList' && tree.target===main && tree.addedNodes instanceof NodeList && tree.addedNodes[0]===child,'record interface and node identity');
          check(tree.attributeName===null && tree.oldValue===null && attr.attributeName==='title' && attr.oldValue===null,'nullable record members');
          check(tree.addedNodes===tree.addedNodes && tree.removedNodes===tree.removedNodes,'same-object lists');
          const descriptor=Object.getOwnPropertyDescriptor(MutationRecord.prototype,'target');check(typeof descriptor.get==='function' && descriptor.set===undefined,'readonly prototype getter');
          let rejected=false;try{descriptor.get.call({})}catch(e){rejected=e.name==='TypeError'}check(rejected,'record getter brand');
          rejected=false;try{new MutationRecord()}catch(e){rejected=e.name==='TypeError'}check(rejected,'illegal record constructor');observer.disconnect();true
        "#);
    }

    #[test]
    fn specification_observer_weak_targets_and_generation_retirement() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let realm = crate::install(ctx, "<main></main>", 128).expect("install document");
        assert_script(
            ctx,
            "globalThis.observer = new MutationObserver(() => {}); true",
        );
        let observer = ctx
            .get_member(&ctx.global_object(), "observer")
            .ok()
            .expect("observer");
        for _ in 0..160 {
            let node = realm
                .session
                .borrow_mut()
                .document_mut()
                .create(NodeKind::Text("weak".into()))
                .unwrap();
            let value = realm.wrap(ctx, node);
            ctx.set_member(&ctx.global_object(), "target", value)
                .ok()
                .expect("target");
            assert_script(
                ctx,
                "observer.observe(target, {characterData:true}); target=null; true",
            );
            ctx.collect_garbage();
            realm.reap_detached_inner([node], true);
            assert!(
                realm.session.borrow().document().kind(node).is_err(),
                "observing does not retain detached target"
            );
            let count = ctx
                .with_instance::<DomMutationObserver, _>(&observer, |observer| {
                    observer.data.registrations.borrow().len()
                })
                .unwrap();
            assert_eq!(count, 0, "retired generations leave the weak node list");
        }
        assert!(realm.observer_registrations.borrow().is_empty());
    }

    #[test]
    fn specification_observer_node_edges_and_callback_cycle_gc() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        crate::install(ctx, "<main></main>", 128).expect("install document");
        assert_script(
            ctx,
            r#"
          globalThis.calls=0;globalThis.target=document.createElement('x');
          (()=>{let observer=new MutationObserver(function(records,self){calls++;if(this!==self)throw new Error('callback identity');observer.disconnect()});
            observer.observe(target,{attributes:true});globalThis.observer=observer})();true
        "#,
        );
        let weak = {
            let observer = ctx
                .get_member(&ctx.global_object(), "observer")
                .ok()
                .expect("observer");
            ctx.weak_value(&observer).unwrap()
        };
        assert_script(ctx, "observer=null;true");
        ctx.collect_garbage();
        assert!(
            weak.upgrade().is_some(),
            "a live detached node retains its observer"
        );
        assert_script(ctx, "target.setAttribute('title','changed');true");
        ctx.drain_microtasks_for_host();
        assert_script(ctx, "calls===1");
        ctx.collect_garbage();
        assert!(
            weak.upgrade().is_none(),
            "disconnect releases a callback/observer cycle"
        );
    }

    #[test]
    fn specification_observer_pending_and_returned_records_across_adoption_gc() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let realm = crate::install(ctx, "<main></main>", 256).expect("install document");
        assert_script(
            ctx,
            r#"
          globalThis.recordParent=document.createElement('section');recordParent.marker=11;
          globalThis.recordChild=document.createElement('x');recordChild.marker=77;
          globalThis.observer=new MutationObserver(()=>{});observer.observe(recordParent,{childList:true});
          recordParent.appendChild(recordChild);recordParent.removeChild(recordChild);true
        "#,
        );
        let weak = {
            let child = ctx
                .get_member(&ctx.global_object(), "recordChild")
                .ok()
                .expect("child");
            ctx.weak_value(&child).unwrap()
        };
        assert_script(
            ctx,
            r#"
          let donor=document.implementation.createHTMLDocument('donor');donor.marker=99;donor.adoptNode(recordChild);
          donor=null;recordChild=null;recordParent=null;true
        "#,
        );
        ctx.collect_garbage();
        assert!(
            weak.upgrade().is_some(),
            "queued records retain adopted native identities"
        );
        assert_script(
            ctx,
            r#"
          globalThis.records=observer.takeRecords();
          records.length===2 && records[0].target.marker===11 && records[0].addedNodes[0].marker===77 &&
          records[0].addedNodes[0]===records[1].removedNodes[0] && records[0].addedNodes[0].ownerDocument.marker===99
        "#,
        );
        ctx.collect_garbage();
        assert_script(
            ctx,
            "records[0].addedNodes[0].marker===77 && records[1].removedNodes[0]===records[0].addedNodes[0]",
        );
        assert_script(ctx, "records=null;true");
        ctx.drain_microtasks_for_host();
        ctx.collect_garbage();
        realm.reap_detached_inner(std::iter::empty(), true);
        assert!(
            weak.upgrade().is_none(),
            "drained records release nodes without disconnect"
        );
    }

    #[test]
    fn specification_observer_transient_source_and_take_records_phases() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        crate::install(ctx, "<main><a></a><b></b></main>", 256).expect("install document");
        assert_script(
            ctx,
            r#"
          const a=document.querySelector('a'), b=document.querySelector('b'), child=document.createElement('x');a.appendChild(child);
          const observer=new MutationObserver(()=>{});observer.observe(a,{subtree:true,attributes:true});observer.observe(b,{subtree:true,attributes:true});
          b.appendChild(child);child.remove();observer.observe(a,{subtree:true,attributes:true});
          child.setAttribute('title','one');let records=observer.takeRecords();
          if(records.length!==1 || records[0].target!==child)throw new Error('other source transient lost');
          child.setAttribute('title','two');if(observer.takeRecords().length!==1)throw new Error('takeRecords cleared transient');
          observer.observe(b,{subtree:true,attributes:true});child.setAttribute('title','three');
          if(observer.takeRecords().length!==0)throw new Error('reobserve did not remove source transient');
          observer.disconnect();true
        "#,
        );
    }

    #[test]
    fn specification_observer_typed_options_order_coercion_and_adoption() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        crate::install(ctx, "<main></main>", 256).expect("install document");
        let collect = ctx.new_native_fn("observerCollect", 0, Rc::new(|ctx, _, _| {
            ctx.collect_garbage();
            Ok(Value::Undefined)
        }));
        ctx.set_member(&ctx.global_object(), "observerCollect", collect)
            .ok()
            .expect("GC function");
        assert_script(
            ctx,
            r#"
          const check=(v,m)=>{if(!v)throw new Error(m)}, observer=new MutationObserver(()=>{}), target=document.createElement('x');
          observer.observe(target,{attributeOldValue:false});target.setAttribute('title','one');
          check(observer.takeRecords().length===1,'present false implies attributes');
          observer.observe(target,{characterDataOldValue:false});target.setAttribute('title','two');
          check(observer.takeRecords().length===0,'present false implies characterData');
          const order=[], donor=document.implementation.createHTMLDocument('donor');
          observer.observe(target,{
            get attributeFilter(){order.push('filter');donor.adoptNode(target);observerCollect();return new Set(['title'])},
            get attributeOldValue(){order.push('old');return false},get attributes(){order.push('attributes');return 1},
            get characterData(){order.push('character');return 0},get characterDataOldValue(){order.push('characterOld');return false},
            get childList(){order.push('children');return ''},get subtree(){order.push('subtree');return {}}
          });
          check(order.join(',')==='filter,old,attributes,character,characterOld,children,subtree','dictionary order');
          target.setAttribute('title','three');target.setAttribute('other','ignored');const records=observer.takeRecords();
          check(records.length===1 && records[0].target===target && target.ownerDocument===donor,'iterable filter and reprojected adopted target');
          const abrupt={};let thrown;try{observer.observe(target,{get attributeFilter(){throw abrupt}})}catch(e){thrown=e}
          check(thrown===abrupt,'abrupt getter identity');observer.disconnect();true
        "#,
        );
    }

    #[test]
    fn specification_observer_pending_order_and_notification_snapshot() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        crate::install(ctx, "<main><a></a><b></b></main>", 256).expect("install document");
        assert_script(
            ctx,
            r#"
          globalThis.log=[];const a=document.querySelector('a'),b=document.querySelector('b');
          const first=new MutationObserver(()=>{log.push('first');b.setAttribute('title','next')});
          const second=new MutationObserver(()=>log.push('second'));
          first.observe(a,{attributes:true});second.observe(b,{attributes:true});a.setAttribute('title','start');true
        "#,
        );
        let hub = ctx.op_state().get::<ObserverAgent>().unwrap().0.clone();
        Delivery { hub: hub.clone() }.flush(ctx).unwrap();
        assert_script(ctx, "log.join(',')==='first'");
        Delivery { hub }.flush(ctx).unwrap();
        assert_script(ctx, "log.join(',')==='first,second'");
    }

    #[test]
    fn parent_and_child_realms_deliver_only_their_mutation_observers() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let _parent_realm =
            crate::install(ctx, "<main id='parent'></main>", 64).expect("install parent document");
        let parent_handle = ctx.current_host_realm();
        let parent_global = parent_handle.global();
        ctx.with_host_realm(&parent_handle, |ctx| {
            assert!(matches!(
                ctx.eval_in_realm(
                    &parent_global,
                    "window.parentDeliveries=0; window.parentObserver=new MutationObserver(() => parentDeliveries++); parentObserver.observe(document.querySelector('main'),{attributes:true}); parentObserver instanceof MutationObserver",
                ),
                Ok(Value::Bool(true))
            ));
        })
        .expect("enter the parent realm");

        let child_handle = ctx.create_host_realm();
        let child_global = child_handle.global();
        let _child_realm = ctx
            .with_host_realm(&child_handle, |ctx| {
                let realm = crate::install(ctx, "<main id='child'></main>", 64)
                    .expect("install child document");
                assert!(matches!(
                    ctx.eval_in_realm(
                        &child_global,
                        "window.childDeliveries=0; window.childObserver=new MutationObserver(() => childDeliveries++); childObserver.observe(document.querySelector('main'),{attributes:true}); childObserver instanceof MutationObserver",
                    ),
                    Ok(Value::Bool(true))
                ));
                assert!(ctx
                    .eval_in_realm(
                        &child_global,
                        "document.querySelector('main').setAttribute('title','child');",
                    )
                    .is_ok());
                ctx.drain_microtasks_for_host();
                let delivered = ctx
                    .eval_in_realm(&child_global, "childDeliveries === 1")
                    .unwrap_or_else(|_| panic!("read child observer count"));
                assert!(matches!(delivered, Value::Bool(true)));
                realm
            })
            .expect("enter child realm");

        let parent_delivered = ctx
            .with_host_realm(&parent_handle, |ctx| {
                let parent_untouched = ctx
                    .eval_in_realm(&parent_global, "parentDeliveries === 0")
                    .unwrap_or_else(|_| panic!("read parent observer count"));
                assert!(matches!(parent_untouched, Value::Bool(true)));
                assert!(
                    ctx.eval_in_realm(
                        &parent_global,
                        "document.querySelector('main').setAttribute('title','parent');",
                    )
                    .is_ok()
                );
                ctx.drain_microtasks_for_host();
                ctx.eval_in_realm(&parent_global, "parentDeliveries === 1")
                    .unwrap_or_else(|_| panic!("read parent observer count"))
            })
            .expect("mutate and deliver in the parent realm");
        let child_remains_unchanged = ctx
            .with_host_realm(&child_handle, |ctx| {
                ctx.eval_in_realm(&child_global, "childDeliveries === 1")
                    .unwrap_or_else(|_| panic!("read child observer count"))
            })
            .expect("read the child observer count in its realm");
        assert!(matches!(parent_delivered, Value::Bool(true)));
        assert!(matches!(child_remains_unchanged, Value::Bool(true)));
    }
}
