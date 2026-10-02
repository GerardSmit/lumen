use super::*;
use lumen::embed::{JsFunction, JsObject, OpError, OpResult};
use lumen_bind::This;
use std::rc::Weak;

#[derive(Clone)]
struct Listener {
    kind: String,
    callback: JsFunction,
    capture: bool,
    once: bool,
    passive: bool,
    handler: bool,
    removed: Rc<Cell<bool>>,
}

pub(crate) struct TargetData {
    realm: Weak<DomRealm>,
    node: Option<NodeId>,
    listeners: RefCell<Vec<Listener>>,
}

#[lumen_bind::class(name = "EventTarget")]
pub struct DomEventTarget { data: Rc<TargetData> }

impl DomEventTarget {
    pub(crate) fn handler(&self, kind: &str) -> Option<JsFunction> {
        self.data.listeners.borrow().iter().find(|listener| listener.handler && listener.kind == kind).map(|listener| listener.callback.clone())
    }
    pub(crate) fn set_handler(&self, ctx: &mut Ctx, owner: &Value, kind: &str, callback: Option<JsFunction>) {
        let mut listeners = self.data.listeners.borrow_mut();
        if let Some(callback) = callback {
            if let Some(listener) = listeners.iter_mut().find(|listener| listener.handler && listener.kind == kind) { listener.callback = callback; }
            else { listeners.push(Listener { kind: kind.into(), callback, capture: false, once: false, passive: false, handler: true, removed: Rc::new(Cell::new(false)) }); }
        } else {
            listeners.retain(|listener| { let remove = listener.handler && listener.kind == kind; if remove { listener.removed.set(true); } !remove });
        }
        ctx.retain_instance(owner, !listeners.is_empty());
    }
    pub(crate) fn from_data(data: Rc<TargetData>) -> Self { Self { data } }
    pub(crate) fn data_handle(&self) -> Rc<TargetData> { self.data.clone() }
    pub(crate) fn window(realm: &Rc<DomRealm>) -> Self { Self { data: Rc::new(TargetData { realm: Rc::downgrade(realm), node: None, listeners: RefCell::new(Vec::new()) }) } }
    pub(crate) fn node(realm: &Rc<DomRealm>, id: NodeId) -> Self {
        let data = Rc::new(TargetData { realm: Rc::downgrade(realm), node: Some(id), listeners: RefCell::new(Vec::new()) });
        realm.targets.borrow_mut().insert(id, Rc::downgrade(&data));
        Self { data }
    }
}

fn flag(ctx: &mut Ctx, options: &Option<Value>, key: &str) -> OpResult<bool> {
    match options {
        Some(Value::Bool(value)) if key == "capture" => Ok(*value),
        Some(value @ Value::Obj(_)) => ctx.get_member(value, key).map(|v| matches!(v, Value::Bool(true))).map_err(|_| OpError::new("TypeError", "event options getter failed")),
        _ => Ok(false),
    }
}

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) { (Value::Obj(a), Value::Obj(b)) => std::ptr::eq(&**a, &**b), _ => false }
}

#[lumen_bind::methods]
impl DomEventTarget {
    #[constructor]
    fn new() -> Self {
        Self { data: Rc::new(TargetData { realm: Weak::new(), node: None, listeners: RefCell::new(Vec::new()) }) }
    }

    fn add_event_listener(&self, ctx: &mut Ctx, this: This<Value>, kind: &str, callback: Option<JsFunction>, options: Option<Value>) -> OpResult<()> {
        let Some(callback) = callback else { return Ok(()); };
        let capture = flag(ctx, &options, "capture")?;
        let once = flag(ctx, &options, "once")?;
        let passive = flag(ctx, &options, "passive")?;
        let mut listeners = self.data.listeners.borrow_mut();
        if !listeners.iter().any(|l| !l.handler && l.kind == kind && l.capture == capture && same(l.callback.value(), callback.value())) {
            listeners.push(Listener { kind: kind.into(), callback, capture, once, passive, handler: false, removed: Rc::new(Cell::new(false)) });
            ctx.retain_instance(&this.0, true);
        }
        Ok(())
    }

    fn remove_event_listener(&self, ctx: &mut Ctx, this: This<Value>, kind: &str, callback: Option<JsFunction>, options: Option<Value>) -> OpResult<()> {
        let Some(callback) = callback else { return Ok(()); };
        let capture = flag(ctx, &options, "capture")?;
        let mut listeners = self.data.listeners.borrow_mut();
        listeners.retain(|l| {
            let remove = !l.handler && l.kind == kind && l.capture == capture && same(l.callback.value(), callback.value());
            if remove { l.removed.set(true); }
            !remove
        });
        ctx.retain_instance(&this.0, !listeners.is_empty());
        Ok(())
    }

    pub(crate) fn dispatch_event(&self, ctx: &mut Ctx, this: This<Value>, event: JsObject) -> OpResult<bool> {
        let handle = ctx.instance_data::<DomEvent>(event.value()).ok_or_else(|| OpError::new("TypeError", "dispatchEvent requires an Event"))?;
        let event_value = event.into_value();
        let event = handle.borrow();
        if event.dispatching.replace(true) { return Err(OpError::new("InvalidStateError", "event is already dispatching")); }
        event.stopped.set(false);
        event.immediate.set(false);
        *event.target.borrow_mut() = this.0.clone();
        let result = (|| {
        let mut path = vec![(this.0.clone(), self.data.clone())];
        if let (Some(realm), Some(mut node)) = (self.data.realm.upgrade(), self.data.node) {
            loop {
                let parent = realm.session.borrow().document().parent(node).map_err(|_| OpError::new("InvalidStateError", "event target was removed"))?;
                let Some(parent) = parent else { break; };
                let value = realm.wrap(ctx, parent);
                if let Some(target) = realm.targets.borrow().get(&parent).and_then(Weak::upgrade) { path.push((value, target)); }
                node = parent;
            }
            if node == realm.session.borrow().document().root() {
                if let (Some(value), Some(target)) = (realm.window_wrapper.borrow().as_ref().and_then(WeakValue::upgrade), realm.window_target.borrow().as_ref()) { path.push((value, target.clone())); }
            }
        }
            for (value, target) in path.iter().skip(1).rev() {
                invoke(ctx, &event, &event_value, value, target, true, 1)?;
                if event.stopped.get() { break; }
            }
            if !event.stopped.get() {
                invoke(ctx, &event, &event_value, &this.0, &self.data, true, 2)?;
                if !event.immediate.get() { invoke(ctx, &event, &event_value, &this.0, &self.data, false, 2)?; }
            }
            if event.bubbles && !event.stopped.get() {
                for (value, target) in path.iter().skip(1) {
                    invoke(ctx, &event, &event_value, value, target, false, 3)?;
                    if event.stopped.get() { break; }
                }
            }
            Ok(!event.canceled.get())
        })();
        event.dispatching.set(false);
        event.phase.set(0);
        *event.current.borrow_mut() = Value::Null;
        result
    }
}

fn invoke(ctx: &mut Ctx, event: &DomEvent, event_value: &Value, value: &Value, target: &TargetData, capture: bool, phase: u8) -> OpResult<()> {
    event.phase.set(phase);
    *event.current.borrow_mut() = value.clone();
    let listeners = target.listeners.borrow().clone();
    for listener in listeners {
        if listener.kind != event.kind || listener.capture != capture || listener.removed.get() { continue; }
        if listener.once {
            listener.removed.set(true);
            target.listeners.borrow_mut().retain(|l| !Rc::ptr_eq(&l.removed, &listener.removed));
            ctx.retain_instance(value, !target.listeners.borrow().is_empty());
        }
        event.passive.set(listener.passive);
        let result = listener.callback.call(ctx, value.clone(), &[event_value.clone()]);
        event.passive.set(false);
        let result = result?;
        if listener.handler && matches!(result, Value::Bool(false)) { event.prevent_default(); }
        if event.immediate.get() { break; }
    }
    Ok(())
}

#[lumen_bind::class(name = "Event")]
pub struct DomEvent {
    kind: String,
    bubbles: bool,
    cancelable: bool,
    target: RefCell<Value>,
    current: RefCell<Value>,
    phase: Cell<u8>,
    dispatching: Cell<bool>,
    stopped: Cell<bool>,
    immediate: Cell<bool>,
    canceled: Cell<bool>,
    passive: Cell<bool>,
}

#[lumen_bind::methods]
impl DomEvent {
    #[constructor]
    pub(crate) fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Self> {
        Ok(Self { kind: kind.into(), bubbles: flag(ctx, &options, "bubbles")?, cancelable: flag(ctx, &options, "cancelable")?, target: RefCell::new(Value::Null), current: RefCell::new(Value::Null), phase: Cell::new(0), dispatching: Cell::new(false), stopped: Cell::new(false), immediate: Cell::new(false), canceled: Cell::new(false), passive: Cell::new(false) })
    }
    #[getter(name = "type")]
    fn kind(&self) -> String { self.kind.clone() }
    #[getter]
    fn bubbles(&self) -> bool { self.bubbles }
    #[getter]
    fn cancelable(&self) -> bool { self.cancelable }
    #[getter]
    fn target(&self) -> Value { self.target.borrow().clone() }
    #[getter]
    fn current_target(&self) -> Value { self.current.borrow().clone() }
    #[getter]
    fn event_phase(&self) -> u8 { self.phase.get() }
    #[getter]
    fn default_prevented(&self) -> bool { self.canceled.get() }
    fn prevent_default(&self) { if self.cancelable && !self.passive.get() { self.canceled.set(true); } }
    fn stop_propagation(&self) { self.stopped.set(true); }
    fn stop_immediate_propagation(&self) { self.stopped.set(true); self.immediate.set(true); }
}
