use super::*;
use lumen::embed::{JsFunction, JsHost, JsObject};
use lumen_bind::{Host, This};
use lumen_html::observe::{ObservedKind, ObservedMutation};
use std::rc::Weak;

#[derive(Clone)]
struct Options { subtree: bool, children: bool, attributes: bool, character: bool, attribute_old: bool, character_old: bool, filter: Option<Vec<String>> }
struct Registration { target: NodeId, options: Options, _keep: DomNodeList }
struct Pending { mutation: ObservedMutation, old: bool, _keep: DomNodeList }
struct ObserverData { callback: JsFunction, registrations: RefCell<Vec<Registration>>, transient: RefCell<Vec<Registration>>, pending: RefCell<Vec<Pending>>, wrapper: RefCell<Option<WeakValue>> }

struct Hub { realm: Weak<DomRealm>, observers: RefCell<Vec<Weak<ObserverData>>>, jobs: Rc<RefCell<Vec<Value>>>, delivery: RefCell<Option<Value>>, scheduled: Cell<bool> }

fn within(document: &lumen_html::Document, target: NodeId, root: NodeId, subtree: bool) -> bool {
    if target == root { return true; }
    if !subtree { return false; }
    let mut node = target;
    while let Ok(Some(parent)) = document.parent(node) { if parent == root { return true; } node = parent; }
    false
}

impl Hub {
    fn capture(&self, document: &lumen_html::Document, mutation: &ObservedMutation) {
        let Some(realm) = self.realm.upgrade() else { return; };
        self.observers.borrow_mut().retain(|observer| observer.strong_count() > 0);
        let observers = self.observers.borrow().iter().filter_map(Weak::upgrade).collect::<Vec<_>>();
        let mut queued = false;
        for observer in observers {
            let registrations = observer.registrations.borrow();
            let transient = observer.transient.borrow();
            let matching = registrations.iter().chain(transient.iter()).map(|registration| (registration.target, &registration.options)).filter(|(root, options)| within(document, mutation.target, *root, options.subtree));
            let mut selected = false;
            let mut old = false;
            let mut removed_options = Vec::new();
            for (_, options) in matching {
                if let ObservedKind::ChildList { removed: Some(_), .. } = mutation.kind { if options.subtree { removed_options.push(options.clone()); } }
                match &mutation.kind {
                    ObservedKind::Attribute { name, .. } if options.attributes && options.filter.as_ref().is_none_or(|filter| filter.contains(name)) => { selected = true; old |= options.attribute_old; },
                    ObservedKind::CharacterData { .. } if options.character => { selected = true; old |= options.character_old; },
                    ObservedKind::ChildList { .. } if options.children => selected = true,
                    _ => {},
                }
            }
            drop(transient);
            drop(registrations);
            if let ObservedKind::ChildList { removed: Some(node), .. } = mutation.kind { if !removed_options.is_empty() { queued = true; } observer.transient.borrow_mut().extend(removed_options.into_iter().map(|options| Registration { target: node, options, _keep: DomNodeList::snapshot(realm.clone(), vec![node], Value::Undefined) })); }
            if !selected { continue; }
            let mut ids = vec![mutation.target];
            if let ObservedKind::ChildList { added, removed, previous_sibling, next_sibling } = mutation.kind { ids.extend([added, removed, previous_sibling, next_sibling].into_iter().flatten()); }
            observer.pending.borrow_mut().push(Pending { mutation: mutation.clone(), old, _keep: DomNodeList::snapshot(realm.clone(), ids, Value::Undefined) });
            queued = true;
        }
        if queued && !self.scheduled.replace(true) { if let Some(callback) = self.delivery.borrow().as_ref() { self.jobs.borrow_mut().push(callback.clone()); } }
    }
}

fn property(ctx: &mut Ctx, object: &Value, name: &str, value: Value) -> OpResult<()> { ctx.set_member(object, name, value).map_err(|_| OpError::new("TypeError", "mutation record initialization failed")) }

fn records(ctx: &mut Ctx, realm: &Rc<DomRealm>, observer: &ObserverData) -> OpResult<Vec<Value>> {
    let pending = std::mem::take(&mut *observer.pending.borrow_mut());
    let mut records = Vec::with_capacity(pending.len());
    for pending in pending {
        let object = Value::Obj(ctx.new_object());
        let target = realm.wrap(ctx, pending.mutation.target);
        property(ctx, &object, "target", target)?;
        let (kind, attribute, old, added, removed, previous, next) = match pending.mutation.kind {
            ObservedKind::Attribute { name, old_value } => ("attributes", Some(name), if pending.old { old_value } else { None }, None, None, None, None),
            ObservedKind::CharacterData { old_value } => ("characterData", None, pending.old.then_some(old_value), None, None, None, None),
            ObservedKind::ChildList { added, removed, previous_sibling, next_sibling } => ("childList", None, None, added, removed, previous_sibling, next_sibling),
        };
        property(ctx, &object, "type", Value::str(kind))?;
        property(ctx, &object, "attributeName", attribute.map_or(Value::Null, |value| Value::str(&value)))?;
        property(ctx, &object, "attributeNamespace", Value::Null)?;
        property(ctx, &object, "oldValue", old.map_or(Value::Null, |value| Value::str(&value)))?;
        let added = ctx.new_instance(DomNodeList::snapshot(realm.clone(), added.into_iter().collect(), Value::Undefined));
        let removed = ctx.new_instance(DomNodeList::snapshot(realm.clone(), removed.into_iter().collect(), Value::Undefined));
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

#[lumen_bind::class(name = "MutationObserver")]
pub struct DomMutationObserver { hub: Rc<Hub>, data: Rc<ObserverData> }

#[lumen_bind::methods]
impl DomMutationObserver {
    #[constructor]
    fn new(ctx: &mut Ctx, callback: JsFunction) -> OpResult<Self> {
        let hub = ctx.op_state().get::<Rc<Hub>>().cloned().ok_or_else(|| OpError::new("Error", "DOM observers are not installed"))?;
        let data = Rc::new(ObserverData { callback, registrations: RefCell::new(Vec::new()), transient: RefCell::new(Vec::new()), pending: RefCell::new(Vec::new()), wrapper: RefCell::new(None) });
        hub.observers.borrow_mut().push(Rc::downgrade(&data));
        Ok(Self { hub, data })
    }
    fn observe(&self, ctx: &mut Ctx, this: This<Value>, target: &DomNode, options: JsObject) -> OpResult<()> {
        let realm = self.hub.realm.upgrade().ok_or_else(|| OpError::new("Error", "DOM realm was dropped"))?;
        if !Rc::ptr_eq(&realm, &target.realm) { return Err(OpError::new("WrongDocumentError", "observer target belongs to another realm")); }
        let get = |ctx: &mut Ctx, name: &str| ctx.get_member(options.value(), name).map_err(|_| OpError::new("TypeError", "observer options getter failed"));
        let attribute_old = matches!(get(ctx, "attributeOldValue")?, Value::Bool(true));
        let character_old = matches!(get(ctx, "characterDataOldValue")?, Value::Bool(true));
        let filter = get(ctx, "attributeFilter")?;
        let filter = if matches!(filter, Value::Undefined) { None } else {
            if !ctx.is_array_value(&filter).map_err(OpError::thrown)? { return Err(OpError::new("TypeError", "attributeFilter must be an array")); }
            let Value::Num(length) = ctx.get_member(&filter, "length").map_err(|_| OpError::new("TypeError", "attributeFilter length failed"))? else { return Err(OpError::new("TypeError", "invalid attributeFilter")); };
            let mut values = Vec::with_capacity(length as usize);
            for index in 0..length as usize { let value = ctx.get_member(&filter, &index.to_string()).map_err(|_| OpError::new("TypeError", "attributeFilter read failed"))?; values.push(ctx.coerce_string(&value).map_err(OpError::thrown)?.to_string()); }
            Some(values)
        };
        let attributes = get(ctx, "attributes")?;
        let attributes = if matches!(attributes, Value::Undefined) { attribute_old || filter.is_some() } else { matches!(attributes, Value::Bool(true)) };
        let character = get(ctx, "characterData")?;
        let character = if matches!(character, Value::Undefined) { character_old } else { matches!(character, Value::Bool(true)) };
        let children = matches!(get(ctx, "childList")?, Value::Bool(true));
        if (!attributes && (attribute_old || filter.is_some())) || (!character && character_old) || !(attributes || character || children) { return Err(OpError::new("TypeError", "invalid observer options")); }
        let options = Options { subtree: matches!(get(ctx, "subtree")?, Value::Bool(true)), children, attributes, character, attribute_old, character_old, filter };
        let mut registrations = self.data.registrations.borrow_mut();
        if let Some(registration) = registrations.iter_mut().find(|registration| registration.target == target.id) { registration.options = options; } else { registrations.push(Registration { target: target.id, options, _keep: DomNodeList::snapshot(realm.clone(), vec![target.id], Value::Undefined) }); }
        drop(registrations);
        let weak = Rc::downgrade(&self.hub);
        realm.session.borrow_mut().document_mut().set_mutation_sink(Some(Rc::new(move |document, mutation| { if let Some(hub) = weak.upgrade() { hub.capture(document, mutation); } })));
        *self.data.wrapper.borrow_mut() = ctx.weak_value(&this.0);
        ctx.retain_instance(&this.0, true);
        Ok(())
    }
    fn disconnect(&self, ctx: &mut Ctx, this: This<Value>) {
        self.data.registrations.borrow_mut().clear();
        self.data.transient.borrow_mut().clear();
        self.data.pending.borrow_mut().clear();
        if self.hub.observers.borrow().iter().filter_map(Weak::upgrade).all(|observer| observer.registrations.borrow().is_empty()) { if let Some(realm) = self.hub.realm.upgrade() { realm.session.borrow_mut().document_mut().set_mutation_sink(None); } }
        ctx.retain_instance(&this.0, false);
    }
    fn take_records(&self, ctx: &mut Ctx) -> OpResult<Vec<Value>> { let realm = self.hub.realm.upgrade().ok_or_else(|| OpError::new("Error", "DOM realm was dropped"))?; records(ctx, &realm, &self.data) }
}

#[lumen_bind::class(name = "MutationObserverDelivery")]
struct Delivery { hub: Rc<Hub> }
#[lumen_bind::methods]
impl Delivery {
    fn flush(&self, ctx: &mut Ctx) -> OpResult<()> {
        self.hub.scheduled.set(false);
        let Some(realm) = self.hub.realm.upgrade() else { return Ok(()); };
        let observers = self.hub.observers.borrow().iter().filter_map(Weak::upgrade).collect::<Vec<_>>();
        let mut error = None;
        for observer in observers {
            observer.transient.borrow_mut().clear();
            if observer.pending.borrow().is_empty() { continue; }
            let values = records(ctx, &realm, &observer)?;
            let values = JsHost::from_list(ctx, values);
            let wrapper = observer.wrapper.borrow().as_ref().and_then(WeakValue::upgrade).unwrap_or(Value::Undefined);
            if let Err(failure) = observer.callback.call(ctx, wrapper.clone(), &[values, wrapper]) { error.get_or_insert(failure); }
        }
        error.map_or(Ok(()), Err)
    }
}

pub(crate) fn install(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    let hub = Rc::new(Hub { realm: Rc::downgrade(realm), observers: RefCell::new(Vec::new()), jobs: ctx.deferred_microtasks(), delivery: RefCell::new(None), scheduled: Cell::new(false) });
    ctx.class_constructor::<Delivery>();
    let delivery = ctx.new_instance(Delivery { hub: hub.clone() });
    let flush = ctx.get_member(&delivery, "flush").map_err(|_| OpError::new("Error", "observer delivery missing"))?;
    let bind = ctx.get_member(&flush, "bind").map_err(|_| OpError::new("Error", "observer binding missing"))?;
    *hub.delivery.borrow_mut() = Some(JsFunction::from_value(bind).ok_or_else(|| OpError::new("TypeError", "observer binding invalid"))?.call(ctx, flush, &[delivery])?);
    ctx.op_state().put(hub);
    let constructor = ctx.class_constructor::<DomMutationObserver>();
    let global = ctx.global_object();
    ctx.set_member(&global, "MutationObserver", constructor).map_err(|_| OpError::new("Error", "observer installation failed"))
}
