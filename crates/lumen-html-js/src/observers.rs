use super::*;
use crate::realm_services::RealmServices;
use lumen::embed::{JsFunction, JsHost, JsObject};
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
struct Registration {
    realm: Rc<DomRealm>,
    target: NodeId,
    options: Options,
    _keep: DomNodeList,
}
struct Pending {
    realm: Rc<DomRealm>,
    mutation: ObservedMutation,
    old: bool,
    _keep: DomNodeList,
}
struct ObserverData {
    callback: JsFunction,
    registrations: RefCell<Vec<Registration>>,
    transient: RefCell<Vec<Registration>>,
    pending: RefCell<Vec<Pending>>,
    wrapper: RefCell<Option<WeakValue>>,
}

struct Hub {
    realm: Weak<DomRealm>,
    observers: RefCell<Vec<Weak<ObserverData>>>,
    // Deferred jobs are agent-wide; each queued delivery function retains its owning realm.
    jobs: Rc<RefCell<Vec<Value>>>,
    delivery: RefCell<Option<Value>>,
    scheduled: Cell<bool>,
    slot_assignments: RefCell<Vec<(NodeId, Vec<NodeId>)>>,
    signal_slots: RefCell<Vec<(NodeId, DomNodeList)>>,
}

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

impl Hub {
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
            ObservedKind::SlotAssignment => (),
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
        let mut old = self.slot_assignments.borrow_mut();
        let mut signals = self.signal_slots.borrow_mut();
        let mut signal = |slot| {
            if !signals.iter().any(|(id, _)| *id == slot) {
                signals.push((
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
                    ObservedKind::ChildList { .. } | ObservedKind::ChildListMany { .. }
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
        drop(old);
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
        {
            let mut observers = self.observers.borrow_mut();
            observers.retain(|observer| observer.strong_count() > 0);
            if observers.is_empty() {
                return;
            }
        }
        let observers = self
            .observers
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        let mut queued = false;
        for observer in observers {
            let registrations = observer.registrations.borrow();
            let transient = observer.transient.borrow();
            let matching = registrations
                .iter()
                .chain(transient.iter())
                .filter(|registration| Rc::ptr_eq(&registration.realm, realm))
                .map(|registration| (registration.target, &registration.options))
                .filter(|(root, options)| {
                    within(document, mutation.target, *root, options.subtree)
                });
            let mut selected = false;
            let mut old = false;
            let mut removed_options = Vec::new();
            for (_, options) in matching {
                if mutation.kind.removed_nodes().next().is_some() {
                    if options.subtree {
                        removed_options.push(options.clone());
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
                    ObservedKind::ChildList { .. } | ObservedKind::ChildListMany { .. }
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
                if !removed_options.is_empty() {
                    queued = true;
                }
                observer
                    .transient
                    .borrow_mut()
                    .extend(removed_options.iter().cloned().map(|options| Registration {
                        realm: realm.clone(),
                        target: node,
                        options,
                        _keep: DomNodeList::snapshot(realm.clone(), vec![node], Value::Undefined),
                    }));
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
            observer.pending.borrow_mut().push(Pending {
                realm: realm.clone(),
                mutation: mutation.clone(),
                old,
                _keep: DomNodeList::snapshot(realm.clone(), ids, Value::Undefined),
            });
            queued = true;
        }
        if queued {
            self.schedule();
        }
    }
}

fn property(ctx: &mut Ctx, object: &Value, name: &str, value: Value) -> OpResult<()> {
    ctx.set_member(object, name, value)
        .map_err(|_| OpError::new("TypeError", "mutation record initialization failed"))
}

fn records(ctx: &mut Ctx, observer: &ObserverData) -> OpResult<Vec<Value>> {
    let pending = std::mem::take(&mut *observer.pending.borrow_mut());
    let mut records = Vec::with_capacity(pending.len());
    for pending in pending {
        let realm = &pending.realm;
        let object = Value::Obj(ctx.new_object());
        let target = realm.wrap(ctx, pending.mutation.target);
        property(ctx, &object, "target", target)?;
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
                ObservedKind::CharacterData { old_value } => (
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
                ObservedKind::SlotAssignment => {
                    ("childList", None, None, None, Vec::new(), Vec::new(), None, None)
                }
            };
        property(ctx, &object, "type", Value::str(kind))?;
        property(
            ctx,
            &object,
            "attributeName",
            attribute.map_or(Value::Null, |value| Value::str(&value)),
        )?;
        property(
            ctx,
            &object,
            "attributeNamespace",
            attribute_namespace.map_or(Value::Null, |value| Value::str(&value)),
        )?;
        property(
            ctx,
            &object,
            "oldValue",
            old.map_or(Value::Null, |value| Value::str(&value)),
        )?;
        let added = ctx.new_instance(DomNodeList::snapshot(
            realm.clone(),
            added.into_iter().collect(),
            Value::Undefined,
        ));
        let removed = ctx.new_instance(DomNodeList::snapshot(
            realm.clone(),
            removed.into_iter().collect(),
            Value::Undefined,
        ));
        property(ctx, &object, "addedNodes", added)?;
        property(ctx, &object, "removedNodes", removed)?;
        let previous = realm.wrap_option(ctx, previous);
        let next = realm.wrap_option(ctx, next);
        property(ctx, &object, "previousSibling", previous)?;
        property(ctx, &object, "nextSibling", next)?;
        records.push(object);
    }
    Ok(records)
}

#[lumen_bind::class(name = "MutationObserver", hint(js(webidl)))]
pub struct DomMutationObserver {
    hub: Rc<Hub>,
    data: Rc<ObserverData>,
}

#[lumen_bind::methods]
impl DomMutationObserver {
    #[constructor]
    fn new(ctx: &mut Ctx, callback: JsFunction) -> OpResult<Self> {
        let hub = RealmServices::<Hub>::current(ctx)
            .ok_or_else(|| OpError::new("Error", "DOM observers are not installed"))?;
        let data = Rc::new(ObserverData {
            callback,
            registrations: RefCell::new(Vec::new()),
            transient: RefCell::new(Vec::new()),
            pending: RefCell::new(Vec::new()),
            wrapper: RefCell::new(None),
        });
        hub.observers.borrow_mut().push(Rc::downgrade(&data));
        Ok(Self { hub, data })
    }
    fn observe(
        &self,
        ctx: &mut Ctx,
        this: This<Value>,
        target: &DomNode,
        options: JsObject,
    ) -> OpResult<()> {
        let target_realm = target.realm.clone();
        let get = |ctx: &mut Ctx, name: &str| {
            ctx.get_member(options.value(), name)
                .map_err(|_| OpError::new("TypeError", "observer options getter failed"))
        };
        let attribute_old = matches!(get(ctx, "attributeOldValue")?, Value::Bool(true));
        let character_old = matches!(get(ctx, "characterDataOldValue")?, Value::Bool(true));
        let filter = get(ctx, "attributeFilter")?;
        let filter = if matches!(filter, Value::Undefined) {
            None
        } else {
            if !ctx.is_array_value(&filter).map_err(OpError::thrown)? {
                return Err(OpError::new(
                    "TypeError",
                    "attributeFilter must be an array",
                ));
            }
            let Value::Num(length) = ctx
                .get_member(&filter, "length")
                .map_err(|_| OpError::new("TypeError", "attributeFilter length failed"))?
            else {
                return Err(OpError::new("TypeError", "invalid attributeFilter"));
            };
            let mut values = Vec::with_capacity(length as usize);
            for index in 0..length as usize {
                let value = ctx
                    .get_member(&filter, &index.to_string())
                    .map_err(|_| OpError::new("TypeError", "attributeFilter read failed"))?;
                values.push(
                    ctx.coerce_string(&value)
                        .map_err(OpError::thrown)?
                        .to_string(),
                );
            }
            Some(values)
        };
        let attributes = get(ctx, "attributes")?;
        let attributes = if matches!(attributes, Value::Undefined) {
            attribute_old || filter.is_some()
        } else {
            matches!(attributes, Value::Bool(true))
        };
        let character = get(ctx, "characterData")?;
        let character = if matches!(character, Value::Undefined) {
            character_old
        } else {
            matches!(character, Value::Bool(true))
        };
        let children = matches!(get(ctx, "childList")?, Value::Bool(true));
        if (!attributes && (attribute_old || filter.is_some()))
            || (!character && character_old)
            || !(attributes || character || children)
        {
            return Err(OpError::new("TypeError", "invalid observer options"));
        }
        let options = Options {
            subtree: matches!(get(ctx, "subtree")?, Value::Bool(true)),
            children,
            attributes,
            character,
            attribute_old,
            character_old,
            filter,
        };
        let mut registrations = self.data.registrations.borrow_mut();
        if let Some(registration) = registrations.iter_mut().find(|registration| {
            registration.target == target.id && Rc::ptr_eq(&registration.realm, &target_realm)
        }) {
            registration.options = options;
        } else {
            registrations.push(Registration {
                realm: target_realm.clone(),
                target: target.id,
                options,
                _keep: DomNodeList::snapshot(
                    target_realm.clone(),
                    vec![target.id],
                    Value::Undefined,
                ),
            });
        }
        drop(registrations);
        *self.data.wrapper.borrow_mut() = ctx.weak_value(&this.0);
        ctx.retain_instance(&this.0, true);
        Ok(())
    }
    fn disconnect(&self, ctx: &mut Ctx, this: This<Value>) {
        self.data.registrations.borrow_mut().clear();
        self.data.transient.borrow_mut().clear();
        self.data.pending.borrow_mut().clear();
        ctx.retain_instance(&this.0, false);
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
        let Some(realm) = self.hub.realm.upgrade() else {
            return Ok(());
        };
        // Take this set before invoking observers: mutations made by callbacks
        // belong to the next delivery, even when they signal the same slot.
        let slots = std::mem::take(&mut *self.hub.signal_slots.borrow_mut());
        let observers = self
            .hub
            .observers
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        let mut error = None;
        for observer in observers {
            observer.transient.borrow_mut().clear();
            if observer.pending.borrow().is_empty() {
                continue;
            }
            let values = records(ctx, &observer)?;
            let values = JsHost::from_list(ctx, values);
            let wrapper = observer
                .wrapper
                .borrow()
                .as_ref()
                .and_then(WeakValue::upgrade)
                .unwrap_or(Value::Undefined);
            if let Err(failure) = observer
                .callback
                .call(ctx, wrapper.clone(), &[values, wrapper])
            {
                error.get_or_insert(failure);
            }
        }
        for (slot, _keep) in slots {
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
    let hub = Rc::new(Hub {
        realm: Rc::downgrade(realm),
        observers: RefCell::new(Vec::new()),
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
    let weak_hub = Rc::downgrade(hub);
    let weak_realm = Rc::downgrade(realm);
    realm.add_mutation_sink(Rc::new(move |document, mutation| {
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

/// Move retained MutationObserver registrations with their observed node identities.
pub(crate) fn adopt_nodes(
    ctx: &mut Ctx,
    source: &Rc<DomRealm>,
    target: &Rc<DomRealm>,
    mapping: &[(NodeId, NodeId)],
) {
    let Some(hub) = RealmServices::<Hub>::current(ctx) else {
        return;
    };
    for observer in hub.observers.borrow().iter().filter_map(Weak::upgrade) {
        let migrate = |registrations: &mut Vec<Registration>| {
            for registration in registrations {
                if !Rc::ptr_eq(&registration.realm, source) {
                    continue;
                }
                let Some((_, new)) = mapping.iter().find(|(old, _)| *old == registration.target)
                else {
                    continue;
                };
                registration.realm = target.clone();
                registration.target = *new;
                registration._keep =
                    DomNodeList::snapshot(target.clone(), vec![*new], Value::Undefined);
            }
        };
        migrate(&mut observer.registrations.borrow_mut());
        migrate(&mut observer.transient.borrow_mut());
    }
    for (node, children) in hub.slot_assignments.borrow_mut().iter_mut() {
        if let Some((_, new)) = mapping.iter().find(|(old, _)| old == node) {
            *node = *new;
        }
        for child in children {
            if let Some((_, new)) = mapping.iter().find(|(old, _)| old == child) {
                *child = *new;
            }
        }
    }
    for (node, list) in hub.signal_slots.borrow_mut().iter_mut() {
        if let Some((_, new)) = mapping.iter().find(|(old, _)| old == node) {
            *node = *new;
            list.adopt_nodes(target.clone(), mapping);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

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
                assert!(ctx
                    .eval_in_realm(
                        &parent_global,
                        "document.querySelector('main').setAttribute('title','parent');",
                    )
                    .is_ok());
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
