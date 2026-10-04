use super::*;
use lumen::embed::{JsFunction, JsObject, OpError, OpResult};
use lumen_bind::This;
use std::rc::Weak;

#[derive(Clone)]
struct Listener {
    kind: String,
    callback: Rc<RefCell<ListenerCallback>>,
    capture: bool,
    once: bool,
    passive: bool,
    handler: bool,
    removed: Rc<Cell<bool>>,
}

#[derive(Clone)]
enum ListenerCallback {
    Empty,
    Function(JsFunction),
    Object(Value),
    ContentHandler {
        source: RawContentHandler,
        compiled: Option<JsFunction>,
    },
}

#[derive(Clone)]
pub(crate) struct RawContentHandler {
    pub(crate) node: NodeId,
    pub(crate) name: String,
    pub(crate) body: String,
    pub(crate) window_target: bool,
    #[allow(dead_code)]
    pub(crate) location: String,
}

impl ListenerCallback {
    fn from_value(value: Value) -> OpResult<Option<Self>> {
        match value {
            Value::Null | Value::Undefined => Ok(None),
            value if value.is_callable() => Ok(Some(Self::Function(
                JsFunction::from_value(value).expect("callable JS values convert to JsFunction"),
            ))),
            value @ Value::Obj(_) => Ok(Some(Self::Object(value))),
            _ => Err(OpError::type_error(
                "EventListener must be a function, object, null, or undefined",
            )),
        }
    }

    fn identity_value(&self) -> Option<&Value> {
        match self {
            Self::Function(callback) => Some(callback.value()),
            Self::Object(callback) => Some(callback),
            Self::Empty | Self::ContentHandler { .. } => None,
        }
    }

    fn function(&self) -> Option<JsFunction> {
        match self {
            Self::Function(callback) => Some(callback.clone()),
            Self::ContentHandler {
                compiled: Some(callback),
                ..
            } => Some(callback.clone()),
            Self::Empty | Self::Object(_) | Self::ContentHandler { .. } => None,
        }
    }
}

pub(crate) struct TargetData {
    realm: RefCell<Weak<DomRealm>>,
    node: Cell<Option<NodeId>>,
    listeners: RefCell<Vec<Listener>>,
}

#[derive(Clone)]
#[lumen_bind::class(name = "EventTarget", hint(js(webidl)))]
pub struct DomEventTarget {
    data: Rc<TargetData>,
}

impl DomEventTarget {
    pub(crate) fn handler(&self, kind: &str) -> Option<JsFunction> {
        self.data
            .listeners
            .borrow()
            .iter()
            .find(|listener| listener.handler && listener.kind == kind)
            .and_then(|listener| listener.callback.borrow().function())
    }

    pub(crate) fn has_handler(&self, kind: &str) -> bool {
        self.data
            .listeners
            .borrow()
            .iter()
            .any(|listener| listener.handler && listener.kind == kind)
    }

    pub(crate) fn handler_value(
        &self,
        ctx: &mut Ctx,
        owner: &Value,
        kind: &str,
    ) -> OpResult<Value> {
        let callback = self
            .data
            .listeners
            .borrow()
            .iter()
            .find(|listener| listener.handler && listener.kind == kind)
            .map(|listener| listener.callback.clone());
        let Some(callback) = callback else {
            return Ok(Value::Null);
        };
        let current = callback.borrow().clone();
        match current {
            ListenerCallback::Function(function) => Ok(function.value().clone()),
            ListenerCallback::ContentHandler {
                compiled: Some(function),
                ..
            } => Ok(function.value().clone()),
            ListenerCallback::ContentHandler {
                source,
                compiled: None,
            } => {
                let realm = self.data.realm.borrow().upgrade();
                let Some(realm) = realm else {
                    return Ok(Value::Null);
                };
                match super::event_content_handlers::compile(ctx, &realm, &source) {
                    super::event_content_handlers::Compilation::Compiled(function) => {
                        *callback.borrow_mut() = ListenerCallback::ContentHandler {
                            source,
                            compiled: Some(function.clone()),
                        };
                        ctx.retain_instance(owner, true);
                        Ok(function.value().clone())
                    }
                    super::event_content_handlers::Compilation::Failed
                        if realm.has_browsing_context =>
                    {
                        *callback.borrow_mut() = ListenerCallback::Empty;
                        ctx.retain_instance(owner, true);
                        Ok(Value::Null)
                    }
                    super::event_content_handlers::Compilation::Failed
                    | super::event_content_handlers::Compilation::Inactive => Ok(Value::Null),
                }
            }
            ListenerCallback::Empty | ListenerCallback::Object(_) => Ok(Value::Null),
        }
    }
    pub(crate) fn set_handler(
        &self,
        ctx: &mut Ctx,
        owner: &Value,
        kind: &str,
        callback: Option<JsFunction>,
    ) {
        let _html_allocations = enter_html_allocation_category();
        let mut listeners = self.data.listeners.borrow_mut();
        if let Some(callback) = callback {
            if let Some(listener) = listeners
                .iter_mut()
                .find(|listener| listener.handler && listener.kind == kind)
            {
                *listener.callback.borrow_mut() = ListenerCallback::Function(callback);
            } else {
                listeners.push(Listener {
                    kind: kind.into(),
                    callback: Rc::new(RefCell::new(ListenerCallback::Function(callback))),
                    capture: false,
                    once: false,
                    passive: false,
                    handler: true,
                    removed: Rc::new(Cell::new(false)),
                });
            }
        } else {
            listeners.retain(|listener| {
                let remove = listener.handler && listener.kind == kind;
                if remove {
                    listener.removed.set(true);
                }
                !remove
            });
        }
        ctx.retain_instance(owner, !listeners.is_empty());
    }

    pub(crate) fn set_content_handler(
        &self,
        ctx: &mut Ctx,
        owner: &Value,
        kind: &str,
        handler: Option<RawContentHandler>,
    ) {
        let _html_allocations = enter_html_allocation_category();
        if let Some(handler) = handler {
            let mut listeners = self.data.listeners.borrow_mut();
            if let Some(listener) = listeners
                .iter_mut()
                .find(|listener| listener.handler && listener.kind == kind)
            {
                *listener.callback.borrow_mut() = ListenerCallback::ContentHandler {
                    source: handler,
                    compiled: None,
                };
            } else {
                listeners.push(Listener {
                    kind: kind.into(),
                    callback: Rc::new(RefCell::new(ListenerCallback::ContentHandler {
                        source: handler,
                        compiled: None,
                    })),
                    capture: false,
                    once: false,
                    passive: false,
                    handler: true,
                    removed: Rc::new(Cell::new(false)),
                });
            }
            ctx.retain_instance(owner, true);
        } else {
            let mut listeners = self.data.listeners.borrow_mut();
            listeners.retain(|listener| {
                let remove = listener.handler && listener.kind == kind;
                if remove {
                    listener.removed.set(true);
                }
                !remove
            });
            ctx.retain_instance(owner, !listeners.is_empty());
        }
    }
    pub(crate) fn from_data(data: Rc<TargetData>) -> Self {
        Self { data }
    }
    pub(crate) fn data_handle(&self) -> Rc<TargetData> {
        self.data.clone()
    }
    pub(crate) fn rebind_node(&self, realm: &Rc<DomRealm>, node: NodeId) {
        let previous_node = self.data.node.get();
        if let Some(previous) = self.data.realm.borrow().upgrade() {
            if let Some(previous_node) = self.data.node.get() {
                previous.targets.borrow_mut().remove(&previous_node);
            }
        }
        *self.data.realm.borrow_mut() = Rc::downgrade(realm);
        self.data.node.set(Some(node));
        if previous_node != Some(node) {
            for listener in self.data.listeners.borrow().iter() {
                if let ListenerCallback::ContentHandler { source, compiled } =
                    &mut *listener.callback.borrow_mut()
                {
                    source.node = node;
                    *compiled = None;
                }
            }
        }
        realm
            .targets
            .borrow_mut()
            .insert(node, Rc::downgrade(&self.data));
    }
    pub(crate) fn window(realm: &Rc<DomRealm>) -> Self {
        Self {
            data: Rc::new(TargetData {
                realm: RefCell::new(Rc::downgrade(realm)),
                node: Cell::new(None),
                listeners: RefCell::new(Vec::new()),
            }),
        }
    }
    pub(crate) fn node(realm: &Rc<DomRealm>, id: NodeId) -> Self {
        if let Some(data) = realm.targets.borrow().get(&id).and_then(Weak::upgrade) {
            return Self { data };
        }
        let data = Rc::new(TargetData {
            realm: RefCell::new(Rc::downgrade(realm)),
            node: Cell::new(Some(id)),
            listeners: RefCell::new(Vec::new()),
        });
        realm.targets.borrow_mut().insert(id, Rc::downgrade(&data));
        Self { data }
    }

    /// Creates an event target owned by a platform object rather than a DOM
    /// node (for example, a MediaQueryList).
    pub(crate) fn independent(realm: &Rc<DomRealm>) -> Self {
        Self {
            data: Rc::new(TargetData {
                realm: RefCell::new(Rc::downgrade(realm)),
                node: Cell::new(None),
                listeners: RefCell::new(Vec::new()),
            }),
        }
    }

    fn target_for_receiver(ctx: &mut Ctx, receiver: Value) -> OpResult<(Self, Value)> {
        let receiver = match receiver {
            Value::Null | Value::Undefined => ctx.global_object(),
            receiver => receiver,
        };
        let target = ctx
            .with_instance::<Self, _>(&receiver, |target| target.clone())
            .map_err(|_| OpError::new("TypeError", "Illegal invocation"))?;
        Ok((target, receiver))
    }
}

fn flag(ctx: &mut Ctx, options: &Option<Value>, key: &str) -> OpResult<bool> {
    match options {
        Some(Value::Bool(value)) if key == "capture" => Ok(*value),
        Some(value @ Value::Obj(_)) => {
            let value = ctx.member_get(value, key).map_err(OpError::thrown)?;
            Ok(ctx.to_boolean(&value))
        }
        _ => Ok(false),
    }
}

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Obj(a), Value::Obj(b)) => std::ptr::eq(&**a, &**b),
        _ => false,
    }
}

#[lumen_bind::methods]
impl DomEventTarget {
    #[constructor]
    pub(crate) fn new() -> Self {
        Self {
            data: Rc::new(TargetData {
                realm: RefCell::new(Weak::new()),
                node: Cell::new(None),
                listeners: RefCell::new(Vec::new()),
            }),
        }
    }

    pub(crate) fn add_event_listener(
        ctx: &mut Ctx,
        this: This<Value>,
        kind: &str,
        callback: Value,
        options: Option<Value>,
    ) -> OpResult<()> {
        let (target, receiver) = Self::target_for_receiver(ctx, this.0)?;
        let Some(callback) = ListenerCallback::from_value(callback)? else {
            return Ok(());
        };
        let capture = flag(ctx, &options, "capture")?;
        let once = flag(ctx, &options, "once")?;
        let passive = flag(ctx, &options, "passive")?;
        let _html_allocations = enter_html_allocation_category();
        let mut listeners = target.data.listeners.borrow_mut();
        if !listeners.iter().any(|l| {
            !l.handler
                && l.kind == kind
                && l.capture == capture
                && l.callback
                    .borrow()
                    .identity_value()
                    .is_some_and(|existing| same(existing, callback.identity_value().unwrap()))
        }) {
            listeners.push(Listener {
                kind: kind.into(),
                callback: Rc::new(RefCell::new(callback)),
                capture,
                once,
                passive,
                handler: false,
                removed: Rc::new(Cell::new(false)),
            });
            ctx.retain_instance(&receiver, true);
        }
        Ok(())
    }

    pub(crate) fn remove_event_listener(
        ctx: &mut Ctx,
        this: This<Value>,
        kind: &str,
        callback: Value,
        options: Option<Value>,
    ) -> OpResult<()> {
        let (target, receiver) = Self::target_for_receiver(ctx, this.0)?;
        let Some(callback) = ListenerCallback::from_value(callback)? else {
            return Ok(());
        };
        let capture = flag(ctx, &options, "capture")?;
        let _html_allocations = enter_html_allocation_category();
        let mut listeners = target.data.listeners.borrow_mut();
        listeners.retain(|l| {
            let remove = !l.handler
                && l.kind == kind
                && l.capture == capture
                && l.callback
                    .borrow()
                    .identity_value()
                    .is_some_and(|existing| same(existing, callback.identity_value().unwrap()));
            if remove {
                l.removed.set(true);
            }
            !remove
        });
        ctx.retain_instance(&receiver, !listeners.is_empty());
        Ok(())
    }

    pub(crate) fn dispatch_event(
        ctx: &mut Ctx,
        this: This<Value>,
        event: JsObject,
    ) -> OpResult<bool> {
        dispatch_event_core(ctx, this, event, false, None)
    }
}

pub(crate) fn dispatch_user_agent_event(
    ctx: &mut Ctx,
    this: This<Value>,
    event: JsObject,
) -> OpResult<bool> {
    dispatch_event_core(ctx, this, event, true, None)
}

/// HTML's Window lifecycle dispatch uses its associated Document as the
/// legacy target override, while listeners and the event path remain Window's.
pub(crate) fn dispatch_user_agent_event_with_target(
    ctx: &mut Ctx,
    this: This<Value>,
    event: JsObject,
    target_override: Value,
) -> OpResult<bool> {
    dispatch_event_core(ctx, this, event, true, Some(target_override))
}

fn dispatch_event_core(
    ctx: &mut Ctx,
    this: This<Value>,
    event: JsObject,
    trusted: bool,
    target_override: Option<Value>,
) -> OpResult<bool> {
    let (target, receiver) = DomEventTarget::target_for_receiver(ctx, this.0)?;
    let event_handle = ctx
        .with_instance::<DomEvent, _>(event.value(), Clone::clone)
        .map_err(|_| OpError::new("TypeError", "dispatchEvent requires an Event"))?;
    let event_value = event.into_value();
    let event = event_handle;
    if event.dispatching.get() || !event.initialized.get() {
        return Err(super::error_reporting::dom_exception(
            ctx,
            "InvalidStateError",
            "The event is not initialized or is already being dispatched",
        ));
    }
    event.dispatching.set(true);
    event.trusted.set(trusted);
    event.stopped.set(false);
    event.immediate.set(false);
    let initial_target = target_override.unwrap_or_else(|| receiver.clone());
    *event.target.borrow_mut() = initial_target.clone();
    let result = (|| {
        let related = event.related_original.borrow().clone();
        let related_node = ctx
            .with_instance::<DomNode, _>(&related, |node| (node.realm.clone(), node.id))
            .ok();
        let mut path = vec![(
            receiver.clone(),
            target.data.clone(),
            initial_target.clone(),
            Vec::new(),
            related.clone(),
        )];
        if let (Some(realm), Some(mut node)) =
            (target.data.realm.borrow().upgrade(), target.data.node.get())
        {
            let original = node;
            let origin_root = realm
                .session
                .borrow()
                .document()
                .root_node(node, false)
                .map_err(dom_error)?;
            path[0].3 = closed_roots(realm.session.borrow().document(), node)?;
            loop {
                let parent = realm
                    .session
                    .borrow()
                    .document()
                    .event_parent(node, event.composed.get(), origin_root)
                    .map_err(|_| OpError::new("InvalidStateError", "event target was removed"))?;
                let Some(parent) = parent else {
                    break;
                };
                if path.len() >= 1024 {
                    return Err(OpError::new("RangeError", "event path limit exceeded"));
                }
                let adjusted = realm
                    .session
                    .borrow()
                    .document()
                    .retarget(original, Some(parent))
                    .map_err(dom_error)?;
                let adjusted = realm.wrap(ctx, adjusted);
                let related_adjusted = if let Some((related_realm, id)) = &related_node {
                    if Rc::ptr_eq(related_realm, &realm) {
                        let id = realm
                            .session
                            .borrow()
                            .document()
                            .retarget(*id, Some(parent))
                            .map_err(dom_error)?;
                        realm.wrap(ctx, id)
                    } else {
                        related.clone()
                    }
                } else {
                    related.clone()
                };
                if same(&adjusted, &related_adjusted) {
                    break;
                }
                let closed = closed_roots(realm.session.borrow().document(), parent)?;
                let value = realm.wrap(ctx, parent);
                if let Some(target) = realm.targets.borrow().get(&parent).and_then(Weak::upgrade) {
                    path.push((value, target, adjusted, closed, related_adjusted));
                }
                node = parent;
            }
            if node == realm.session.borrow().document().root() {
                let adjusted = realm
                    .session
                    .borrow()
                    .document()
                    .retarget(original, None)
                    .map_err(dom_error)?;
                let adjusted = realm.wrap(ctx, adjusted);
                let related_adjusted = if let Some((related_realm, id)) = &related_node {
                    if Rc::ptr_eq(related_realm, &realm) {
                        let id = realm
                            .session
                            .borrow()
                            .document()
                            .retarget(*id, None)
                            .map_err(dom_error)?;
                        realm.wrap(ctx, id)
                    } else {
                        related.clone()
                    }
                } else {
                    related.clone()
                };
                if let (Some(value), Some(target)) = (
                    realm
                        .window_wrapper
                        .borrow()
                        .as_ref()
                        .and_then(WeakValue::upgrade),
                    realm.window_target.borrow().as_ref(),
                ) {
                    path.push((
                        value,
                        target.clone(),
                        adjusted,
                        Vec::new(),
                        related_adjusted,
                    ));
                }
            }
        }
        if path.iter().map(|entry| entry.3.len()).sum::<usize>() > 4096 {
            return Err(OpError::new(
                "RangeError",
                "event shadow path limit exceeded",
            ));
        }
        let clear_target = if let (Some(realm), Some(original)) =
            (target.data.realm.borrow().upgrade(), target.data.node.get())
        {
            let session = realm.session.borrow();
            let document = session.document();
            let target = document
                .retarget(original, path.last().unwrap().1.node.get())
                .map_err(dom_error)?;
            document
                .shadow_host(document.root_node(target, false).map_err(dom_error)?)
                .map_err(dom_error)?
                .is_some()
        } else {
            false
        };
        *event.path.borrow_mut() = path
            .iter()
            .map(|entry| (entry.0.clone(), entry.3.clone()))
            .collect();
        for (value, target, adjusted, closed, related) in path.iter().skip(1).rev() {
            *event.target.borrow_mut() = adjusted.clone();
            *event.related.borrow_mut() = related.clone();
            *event.visibility.borrow_mut() = closed.clone();
            invoke(
                ctx,
                &event,
                &event_value,
                value,
                target,
                true,
                if same(value, adjusted) { 2 } else { 1 },
            )?;
            if event.stopped.get() {
                break;
            }
        }
        if !event.stopped.get() {
            *event.target.borrow_mut() = initial_target.clone();
            *event.related.borrow_mut() = related.clone();
            *event.visibility.borrow_mut() = path[0].3.clone();
            invoke(ctx, &event, &event_value, &receiver, &target.data, true, 2)?;
            if !event.immediate.get() {
                invoke(ctx, &event, &event_value, &receiver, &target.data, false, 2)?;
            }
        }
        if !event.stopped.get() {
            for (value, target, adjusted, closed, related) in path.iter().skip(1) {
                if !event.bubbles.get() && !same(value, adjusted) {
                    continue;
                }
                *event.target.borrow_mut() = adjusted.clone();
                *event.related.borrow_mut() = related.clone();
                *event.visibility.borrow_mut() = closed.clone();
                invoke(
                    ctx,
                    &event,
                    &event_value,
                    value,
                    target,
                    false,
                    if same(value, adjusted) { 2 } else { 3 },
                )?;
                if event.stopped.get() {
                    break;
                }
            }
        }
        *event.target.borrow_mut() = if clear_target {
            Value::Null
        } else {
            path.last().unwrap().2.clone()
        };
        *event.related.borrow_mut() = if clear_target {
            Value::Null
        } else {
            path.last().unwrap().4.clone()
        };
        Ok(!event.canceled.get())
    })();
    event.dispatching.set(false);
    event.phase.set(0);
    event.stopped.set(false);
    event.immediate.set(false);
    *event.current.borrow_mut() = Value::Null;
    event.path.borrow_mut().clear();
    event.visibility.borrow_mut().clear();
    result
}

fn closed_roots(document: &lumen_html::Document, node: NodeId) -> OpResult<Vec<NodeId>> {
    let mut root = document.root_node(node, false).map_err(dom_error)?;
    let mut closed = Vec::new();
    while let Some(host) = document.shadow_host(root).map_err(dom_error)? {
        if document.shadow_mode(root).map_err(dom_error)? == Some(lumen_html::ShadowMode::Closed) {
            if closed.len() >= 64 {
                return Err(OpError::new(
                    "RangeError",
                    "event shadow nesting limit exceeded",
                ));
            }
            closed.push(root);
        }
        root = document.root_node(host, false).map_err(dom_error)?;
    }
    Ok(closed)
}

fn invoke(
    ctx: &mut Ctx,
    event: &DomEvent,
    event_value: &Value,
    value: &Value,
    target: &TargetData,
    capture: bool,
    phase: u8,
) -> OpResult<()> {
    event.phase.set(phase);
    *event.current.borrow_mut() = value.clone();
    let listeners = target.listeners.borrow().clone();
    for listener in listeners {
        if listener.kind.as_str() != event.kind.borrow().as_str()
            || listener.capture != capture
            || listener.removed.get()
        {
            continue;
        }
        if listener.once {
            listener.removed.set(true);
            target
                .listeners
                .borrow_mut()
                .retain(|l| !Rc::ptr_eq(&l.removed, &listener.removed));
            ctx.retain_instance(value, !target.listeners.borrow().is_empty());
        }
        event.passive.set(listener.passive);
        let window_error_handler = listener.handler
            && listener.kind == "error"
            && ctx
                .with_instance::<super::window_globals::DomWindow, _>(value, |_| ())
                .is_ok()
            && super::error_reporting::is_error_event(ctx, event_value);
        let mut callback = listener.callback.borrow().clone();
        if let ListenerCallback::ContentHandler { source, compiled } = &callback {
            if let Some(function) = compiled {
                callback = ListenerCallback::Function(function.clone());
            } else if let Some(realm) = target.realm.borrow().upgrade() {
                match super::event_content_handlers::compile(ctx, &realm, source) {
                    super::event_content_handlers::Compilation::Compiled(function) => {
                        callback = ListenerCallback::ContentHandler {
                            source: source.clone(),
                            compiled: Some(function.clone()),
                        };
                        *listener.callback.borrow_mut() = callback.clone();
                    }
                    super::event_content_handlers::Compilation::Failed
                        if realm.has_browsing_context =>
                    {
                        callback = ListenerCallback::Empty;
                        *listener.callback.borrow_mut() = ListenerCallback::Empty;
                    }
                    super::event_content_handlers::Compilation::Failed
                    | super::event_content_handlers::Compilation::Inactive => {}
                }
            }
        }
        let result = if window_error_handler {
            invoke_window_error_handler(ctx, &callback, value, event_value)
        } else {
            invoke_listener(ctx, &callback, value, event_value)
        };
        event.passive.set(false);
        match result {
            Ok(Value::Bool(true)) if window_error_handler => event.prevent_default(),
            Ok(Value::Bool(false)) if listener.handler && !window_error_handler => {
                event.prevent_default()
            }
            Ok(_) => {}
            Err(error) => {
                let exception = error.to_value(ctx);
                let _ = DomRealm::report_exception(ctx, exception);
            }
        }
        if event.immediate.get() {
            break;
        }
    }
    Ok(())
}

fn invoke_window_error_handler(
    ctx: &mut Ctx,
    callback: &ListenerCallback,
    current_target: &Value,
    event: &Value,
) -> OpResult<Value> {
    let callback = callback
        .function()
        .expect("event handler IDL attributes store function callbacks only");
    let args = super::ui_events::error_event_handler_arguments(ctx, event)
        .expect("special error handling requires a native ErrorEvent");
    callback.call(ctx, current_target.clone(), &args)
}

fn invoke_listener(
    ctx: &mut Ctx,
    callback: &ListenerCallback,
    current_target: &Value,
    event: &Value,
) -> OpResult<Value> {
    match callback {
        ListenerCallback::Empty | ListenerCallback::ContentHandler { compiled: None, .. } => {
            Ok(Value::Undefined)
        }
        ListenerCallback::ContentHandler {
            compiled: Some(callback),
            ..
        } => callback.call(ctx, current_target.clone(), &[event.clone()]),
        ListenerCallback::Function(callback) => {
            callback.call(ctx, current_target.clone(), &[event.clone()])
        }
        ListenerCallback::Object(object) => {
            let handle_event = JsObject::from_value(object.clone())
                .expect("EventListener object is an object")
                .get(ctx, "handleEvent")?;
            let handle_event = JsFunction::from_value(handle_event)
                .ok_or_else(|| OpError::type_error("EventListener.handleEvent is not callable"))?;
            handle_event.call(ctx, object.clone(), &[event.clone()])
        }
    }
}

#[derive(Clone)]
#[lumen_bind::class(name = "Event", hint(js(webidl)))]
pub struct DomEvent {
    state: Rc<DomEventState>,
}

// A cloned handle keeps the same dispatch state after the native base projection
// is released, allowing callbacks to read and update the original event.
#[doc(hidden)]
pub struct DomEventState {
    kind: RefCell<String>,
    bubbles: Cell<bool>,
    cancelable: Cell<bool>,
    composed: Cell<bool>,
    initialized: Cell<bool>,
    trusted: Cell<bool>,
    path: RefCell<Vec<(Value, Vec<NodeId>)>>,
    visibility: RefCell<Vec<NodeId>>,
    related_original: RefCell<Value>,
    related: RefCell<Value>,
    target: RefCell<Value>,
    current: RefCell<Value>,
    phase: Cell<u8>,
    dispatching: Cell<bool>,
    stopped: Cell<bool>,
    immediate: Cell<bool>,
    canceled: Cell<bool>,
    passive: Cell<bool>,
    movement_x: Cell<f64>,
    movement_y: Cell<f64>,
}

impl std::ops::Deref for DomEvent {
    type Target = DomEventState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

#[lumen_bind::methods]
impl DomEvent {
    #[constructor(coerce)]
    pub(crate) fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Self> {
        if options
            .as_ref()
            .is_some_and(|value| !matches!(value, Value::Obj(_) | Value::Null | Value::Undefined))
        {
            return Err(OpError::type_error(
                "EventInit must be an object, null, or undefined",
            ));
        }
        Ok(Self {
            state: Rc::new(DomEventState {
                kind: RefCell::new(kind.into()),
                bubbles: Cell::new(flag(ctx, &options, "bubbles")?),
                cancelable: Cell::new(flag(ctx, &options, "cancelable")?),
                composed: Cell::new(flag(ctx, &options, "composed")?),
                initialized: Cell::new(true),
                trusted: Cell::new(false),
                path: RefCell::new(Vec::new()),
                visibility: RefCell::new(Vec::new()),
                related_original: RefCell::new(Value::Null),
                related: RefCell::new(Value::Null),
                target: RefCell::new(Value::Null),
                current: RefCell::new(Value::Null),
                phase: Cell::new(0),
                dispatching: Cell::new(false),
                stopped: Cell::new(false),
                immediate: Cell::new(false),
                canceled: Cell::new(false),
                passive: Cell::new(false),
                movement_x: Cell::new(0.0),
                movement_y: Cell::new(0.0),
            }),
        })
    }
    #[getter(name = "type")]
    fn kind(&self) -> String {
        self.kind.borrow().clone()
    }
    #[getter]
    fn bubbles(&self) -> bool {
        self.bubbles.get()
    }
    #[getter]
    fn cancelable(&self) -> bool {
        self.cancelable.get()
    }
    #[getter]
    fn composed(&self) -> bool {
        self.composed.get()
    }
    #[getter]
    fn is_trusted(&self) -> bool {
        self.trusted.get()
    }
    fn composed_path(&self, ctx: &mut Ctx) -> Value {
        let visible = self.visibility.borrow();
        ctx.make_array(
            self.path
                .borrow()
                .iter()
                .filter(|(_, closed)| closed.iter().all(|root| visible.contains(root)))
                .map(|(value, _)| value.clone())
                .collect(),
        )
    }
    #[getter]
    fn target(&self) -> Value {
        self.target.borrow().clone()
    }
    #[getter]
    fn current_target(&self) -> Value {
        self.current.borrow().clone()
    }
    #[getter]
    fn related_target(&self) -> Value {
        self.related.borrow().clone()
    }
    #[getter(name = "srcElement")]
    fn src_element(&self) -> Value {
        self.target.borrow().clone()
    }
    #[getter(name = "movementX")]
    fn movement_x(&self) -> f64 {
        self.movement_x.get()
    }
    #[getter(name = "movementY")]
    fn movement_y(&self) -> f64 {
        self.movement_y.get()
    }
    #[getter]
    fn event_phase(&self) -> u8 {
        self.phase.get()
    }
    #[getter]
    fn default_prevented(&self) -> bool {
        self.canceled.get()
    }
    #[getter(name = "cancelBubble")]
    fn cancel_bubble(&self) -> bool {
        self.stopped.get()
    }
    #[setter(name = "cancelBubble", coerce)]
    fn set_cancel_bubble(&self, value: bool) {
        if value {
            self.stopped.set(true);
        }
    }
    #[getter(name = "returnValue")]
    fn return_value(&self) -> bool {
        !self.canceled.get()
    }
    #[setter(name = "returnValue", coerce)]
    fn set_return_value(&self, value: bool) {
        if !value {
            self.prevent_default();
        }
    }
    #[method(name = "initEvent", coerce)]
    fn init_event(
        &self,
        kind: &str,
        #[default(false)] bubbles: bool,
        #[default(false)] cancelable: bool,
    ) {
        let _ = self.initialize_legacy(kind, bubbles, cancelable);
    }
    fn prevent_default(&self) {
        if self.cancelable.get() && !self.passive.get() {
            self.canceled.set(true);
        }
    }
    fn stop_propagation(&self) {
        self.stopped.set(true);
    }
    fn stop_immediate_propagation(&self) {
        self.stopped.set(true);
        self.immediate.set(true);
    }
}

#[cfg(test)]
mod tests {
    use super::super::install;
    use lumen::embed::Value;
    use lumen_runtime::Runtime;

    fn eval(source: &str) -> Value {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        install(engine.ctx(), "<main></main>", 64).unwrap();
        let value = match engine.eval_value(source).expect("valid event contract") {
            Ok(value) => value,
            Err(error) => match engine.describe_throw(error) {
                lumen::Completion::Throw { name, message } => panic!("{name}: {message}"),
                _ => unreachable!("describe_throw returns a throw completion"),
            },
        };
        if matches!(value, Value::Bool(false)) {
            let diagnostic = engine
                .eval_value(
                    r#"JSON.stringify({
                onerrorOwn: Object.getOwnPropertyDescriptor(window, 'onerror'),
                args: typeof args === 'undefined' ? null : args.map(value => typeof value),
                handlerCalls: typeof handlerCalls === 'undefined' ? null : handlerCalls,
                dispatched: typeof dispatched === 'undefined' ? null : dispatched,
                canceled: typeof event === 'undefined' ? null : event.defaultPrevented
            })"#,
                )
                .expect("valid failure diagnostic")
                .ok();
            if let Some(Value::Str(diagnostic)) = diagnostic {
                panic!("event contract failed: {diagnostic}");
            }
        }
        value
    }

    #[test]
    fn event_init_dictionary_rejects_primitives_and_preserves_abrupt_getters() {
        let value = eval(
            r#"
            let rejected = 0;
            for (const init of [true, 1, 'text', Symbol('init'), 1n]) {
                try { new Event('x', init); } catch (error) { if (error instanceof TypeError) rejected++; }
            }
            const marker = {};
            let preserved = false;
            try { new Event('x', { get bubbles() { throw marker; } }); }
            catch (error) { preserved = error === marker; }
            const reads = [];
            const event = new Event('x', {
                get bubbles() { reads.push('bubbles'); return 1; },
                get cancelable() { reads.push('cancelable'); return 'yes'; },
                get composed() { reads.push('composed'); return {}; }
            });
            rejected === 5 && preserved && event.bubbles && event.cancelable && event.composed &&
                reads.join(',') === 'bubbles,cancelable,composed' &&
                !new Event('x', null).bubbles && !new Event('x', undefined).cancelable
        "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn object_event_listeners_keep_identity_and_dynamic_receiver_semantics() {
        let value = eval(
            r#"
            const target = new EventTarget();
            const listener = {};
            let calls = [];
            target.addEventListener('x', listener);
            target.addEventListener('x', listener);
            listener.handleEvent = function() { calls.push(this === listener ? 'late' : 'bad'); };
            target.dispatchEvent(new Event('x'));
            listener.handleEvent = function() { calls.push(this === listener ? 'changed' : 'bad'); };
            target.dispatchEvent(new Event('x'));
            target.removeEventListener('x', listener, true);
            target.dispatchEvent(new Event('x'));
            target.removeEventListener('x', listener);
            target.dispatchEvent(new Event('x'));

            let functionCalls = 0;
            let functionHandleEventReads = 0;
            function callback() {
              if (this === target) functionCalls++;
            }
            Object.defineProperty(callback, 'handleEvent', {
              get() { functionHandleEventReads++; throw new Error('must not read'); }
            });
            target.addEventListener('function', callback);
            target.dispatchEvent(new Event('function'));

            calls.join(',') === 'late,changed,changed' &&
              functionCalls === 1 && functionHandleEventReads === 0
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn listener_exceptions_are_reported_and_do_not_abort_dispatch() {
        let value = eval(
            r#"
            const target = new EventTarget();
            const thrown = { marker: 'original exception' };
            const methodThrown = { marker: 'handleEvent exception' };
            const reports = [];
            const calls = [];
            window.addEventListener('error', event => reports.push(event.error));
            target.addEventListener('x', () => { calls.push('function'); throw thrown; });
            target.addEventListener('x', { get handleEvent() { throw 'getter exception'; } });
            target.addEventListener('x', { handleEvent() { calls.push('object'); throw methodThrown; } });
            target.addEventListener('x', () => calls.push('last'));
            const dispatched = target.dispatchEvent(new Event('x'));
            dispatched && calls.join(',') === 'function,object,last' &&
              reports.length === 3 && reports[0] === thrown &&
              reports[1] === 'getter exception' && reports[2] === methodThrown
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn window_onerror_uses_error_event_fields_and_true_cancels() {
        let value = eval(
            r#"
            const thrown = { marker: 'reported value' };
            let args;
            let ordinaryListenerGotEvent = false;
            window.onerror = function() {
              args = Array.from(arguments);
              return true;
            };
            window.addEventListener('error', event => {
              ordinaryListenerGotEvent = event instanceof ErrorEvent && event.error === thrown;
            });
            const event = new ErrorEvent('error', {
              message: 'message', filename: 'source.js', lineno: 7, colno: 11,
              error: thrown, cancelable: true
            });
            const dispatched = window.dispatchEvent(event);
            !dispatched && event.defaultPrevented && ordinaryListenerGotEvent &&
              args.length === 5 && args[0] === 'message' && args[1] === 'source.js' &&
              args[2] === 7 && args[3] === 11 && args[4] === thrown
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn window_onerror_receives_ordinary_error_events_as_one_argument() {
        let value = eval(
            r#"
            let args;
            window.onerror = function() {
              args = Array.from(arguments);
              return true;
            };
            const event = new Event('error', {cancelable: true});
            const dispatched = window.dispatchEvent(event);
            const ordinaryArgs = args;
            window.onerror = () => false;
            const genericEvent = new Event('error', {cancelable: true});
            const genericFalseCancelled = !window.dispatchEvent(genericEvent) &&
              genericEvent.defaultPrevented;
            window.onerror = function() { args = Array.from(arguments); return true; };
            const spoof = new Event('error', {cancelable: true});
            Object.setPrototypeOf(spoof, ErrorEvent.prototype);
            const spoofDispatched = window.dispatchEvent(spoof);
            dispatched && !event.defaultPrevented && ordinaryArgs.length === 1 && ordinaryArgs[0] === event &&
              genericFalseCancelled && spoofDispatched && args.length === 1 && args[0] === spoof
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn window_onerror_uses_native_error_event_slots() {
        let value = eval(
            r#"
            const thrown = { marker: 'message getter' };
            let args;
            let handlerCalls = 0;
            let messageGetterCalls = 0;
            window.onerror = function() { handlerCalls++; args = Array.from(arguments); return true; };
            const event = new ErrorEvent('error', {
              message: 'stored message', filename: 'stored.js', lineno: 9, colno: 13,
              error: thrown, cancelable: true
            });
            Object.defineProperty(event, 'message', {
              get() { messageGetterCalls++; throw new Error('native handler must use event slots'); }
            });
            const dispatched = window.dispatchEvent(event);
            !dispatched && event.defaultPrevented && handlerCalls === 1 && messageGetterCalls === 0 &&
              args.length === 5 && args[0] === 'stored message' && args[1] === 'stored.js' &&
              args[2] === 9 && args[3] === 13 && args[4] === thrown
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn legacy_event_attributes_reflect_and_control_dispatch_state() {
        let value = eval(
            r#"
            const parent = document.createElement('div');
            const target = document.createElement('span');
            parent.appendChild(target);
            let reachedParent = false;
            let sourceMatches = false;
            let cancelBubbleDuringDispatch = false;
            parent.addEventListener('stop', () => { reachedParent = true; });
            target.addEventListener('stop', event => {
              sourceMatches = event.srcElement === target && event.target === target;
              event.cancelBubble = true;
              event.cancelBubble = false;
              cancelBubbleDuringDispatch = event.cancelBubble;
            });
            const stopped = new Event('stop', {bubbles: true});
            const stopResult = target.dispatchEvent(stopped);

            target.addEventListener('cancel', event => { event.returnValue = false; });
            const cancelable = new Event('cancel', {cancelable: true});
            const cancelResult = target.dispatchEvent(cancelable);
            !reachedParent && sourceMatches && cancelBubbleDuringDispatch &&
              !stopped.cancelBubble && stopResult &&
              cancelResult === false && cancelable.defaultPrevented && !cancelable.returnValue
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn legacy_events_require_initialization_and_init_event_resets_state() {
        let value = eval(
            r#"
                const legacy = document.createEvent('Event');
                const defaults = legacy.type === '' && !legacy.bubbles && !legacy.cancelable &&
                  !legacy.composed && !legacy.isTrusted && legacy.returnValue &&
                  !legacy.cancelBubble && legacy.srcElement === null;
                const constructed = new Event('before', {composed: true});
                constructed.initEvent('after', true, true);
                const composedIsUnaffected = constructed.type === 'after' && constructed.composed;
                let invalidState = false;
                try {
                  window.dispatchEvent(legacy);
                } catch (error) {
                  invalidState = error instanceof DOMException &&
                    error.name === 'InvalidStateError' && error.code === 11;
                }

                legacy.initEvent('custom', true, true);
                let seen = false;
                window.addEventListener('custom', event => {
                  seen = event === legacy && event.type === 'custom';
                  event.preventDefault();
                  event.initEvent('ignored', false, false);
                });
                const canceled = !window.dispatchEvent(legacy) && legacy.defaultPrevented &&
                  !legacy.returnValue;
                const noOpDuringDispatch = legacy.type === 'custom' && legacy.bubbles &&
                  legacy.cancelable;

                legacy.initEvent('second');
                const resetAfterDispatch = legacy.type === 'second' && !legacy.bubbles &&
                  !legacy.cancelable && !legacy.defaultPrevented && legacy.returnValue &&
                  legacy.target === null && legacy.srcElement === null && !legacy.cancelBubble;
                const dispatchAgain = window.dispatchEvent(legacy);
                defaults && composedIsUnaffected && invalidState && seen && canceled && noOpDuringDispatch &&
                  resetAfterDispatch && dispatchAgain
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn create_event_aliases_use_only_captured_exposed_interfaces() {
        let value = eval(
            r#"
            const eventAlias = document.createEvent('eVeNtS');
            const htmlAlias = document.createEvent('hTmLeVeNtS');
            eventAlias.initEvent('event-alias');
            htmlAlias.initEvent('html-alias');
            const aliases = eventAlias instanceof Event && eventAlias.type === 'event-alias' &&
              htmlAlias instanceof Event && htmlAlias.type === 'html-alias';

            const OriginalCustomEvent = CustomEvent;
            const custom = document.createEvent('cUsToMeVeNt');
            let customUninitialized = false;
            try {
              window.dispatchEvent(custom);
            } catch (error) {
              customUninitialized = error instanceof DOMException &&
                error.name === 'InvalidStateError' && error.code === 11;
            }
            const customDefaults = custom instanceof OriginalCustomEvent &&
              custom instanceof Event && custom.type === '' && custom.detail === null &&
              !custom.bubbles && !custom.cancelable && !custom.composed && !custom.isTrusted;
            globalThis.CustomEvent = function ReplacedCustomEvent() {};
            const captured = document.createEvent('CUSTOMevent');
            const capturedConstructor = captured instanceof OriginalCustomEvent &&
              !(captured instanceof globalThis.CustomEvent);
            captured.initEvent('captured', true, false);
            const capturedInitialized = captured.type === 'captured' && captured.bubbles &&
              !captured.cancelable;

            function legacySubclass(name, constructor) {
              if (typeof constructor !== 'function') {
                try {
                  document.createEvent(name);
                  return false;
                } catch (error) {
                  return error instanceof DOMException &&
                    error.name === 'NotSupportedError' && error.code === 9;
                }
              }
              const event = document.createEvent(name);
              let uninitialized = false;
              try {
                window.dispatchEvent(event);
              } catch (error) {
                uninitialized = error instanceof DOMException &&
                  error.name === 'InvalidStateError' && error.code === 11;
              }
              const defaults = event instanceof constructor && event instanceof Event &&
                event.type === '' && !event.bubbles && !event.cancelable &&
                !event.composed && !event.isTrusted;
              event.initEvent('legacy-subclass');
              return defaults && uninitialized && event.type === 'legacy-subclass';
            }
            const mouse = legacySubclass('mOuSeEvEnTs', globalThis.MouseEvent);
            const ui = legacySubclass('uIeVeNtS', globalThis.UIEvent);
            const composition = legacySubclass('CompositionEvent', globalThis.CompositionEvent);
            const focus = legacySubclass('FocusEvent', globalThis.FocusEvent);
            const keyboard = legacySubclass('KeyboardEvent', globalThis.KeyboardEvent);

            let unsupported = false;
            try {
              document.createEvent('UnknownEventInterface');
            } catch (error) {
              unsupported = error instanceof DOMException &&
                error.name === 'NotSupportedError' && error.code === 9;
            }
            aliases && customDefaults && customUninitialized && capturedConstructor &&
              capturedInitialized && mouse && ui && composition && focus && keyboard && unsupported
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }
}

#[lumen_bind::class(name = "PromiseRejectionEvent", extends = DomEvent, hint(js(webidl)))]
pub(crate) struct DomPromiseRejectionEvent {
    base: DomEvent,
    promise: Value,
    reason: Value,
}

#[lumen_bind::methods]
impl DomPromiseRejectionEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, init: JsObject) -> OpResult<Self> {
        let promise = init.get(ctx, "promise")?;
        if matches!(promise, Value::Undefined) {
            return Err(OpError::type_error(
                "PromiseRejectionEvent requires promise",
            ));
        }
        let promise = ctx.coerce_promise(promise)?;
        let reason = init.get(ctx, "reason")?;
        Ok(Self {
            base: DomEvent::new(ctx, kind, Some(init.into_value()))?,
            promise,
            reason,
        })
    }

    #[getter]
    fn promise(&self) -> Value {
        self.promise.clone()
    }

    #[getter]
    fn reason(&self) -> Value {
        self.reason.clone()
    }
}

impl DomPromiseRejectionEvent {
    pub(crate) fn for_user_agent(
        ctx: &mut Ctx,
        kind: &str,
        promise: Value,
        reason: Value,
    ) -> OpResult<Self> {
        let options = ctx.new_object_with_proto(&Value::Null);
        ctx.member_set(
            &options,
            "cancelable",
            Value::Bool(kind == "unhandledrejection"),
        )
        .map_err(OpError::thrown)?;
        Ok(Self {
            base: DomEvent::new(ctx, kind, Some(options))?,
            promise,
            reason,
        })
    }
}

impl DomEvent {
    /// Apply the common legacy Event initialization steps. Derived event
    /// initializers call this before updating their own interface fields.
    pub(crate) fn initialize_legacy(&self, kind: &str, bubbles: bool, cancelable: bool) -> bool {
        if self.dispatching.get() {
            return false;
        }
        self.initialized.set(true);
        self.stopped.set(false);
        self.immediate.set(false);
        self.canceled.set(false);
        self.trusted.set(false);
        *self.target.borrow_mut() = Value::Null;
        *self.kind.borrow_mut() = kind.into();
        self.bubbles.set(bubbles);
        self.cancelable.set(cancelable);
        true
    }

    pub(crate) fn legacy_uninitialized(ctx: &mut Ctx) -> OpResult<Self> {
        let event = Self::new(ctx, "", None)?;
        mark_uninitialized(&event);
        Ok(event)
    }

    pub(crate) fn set_movement(&self, x: f64, y: f64) {
        self.movement_x.set(x);
        self.movement_y.set(y);
    }

    pub(crate) fn set_related_target(&self, value: Value) {
        *self.related_original.borrow_mut() = value.clone();
        *self.related.borrow_mut() = value;
    }
}

fn mark_uninitialized(event: &DomEvent) {
    event.initialized.set(false);
    *event.kind.borrow_mut() = String::new();
    event.bubbles.set(false);
    event.cancelable.set(false);
    event.composed.set(false);
    event.trusted.set(false);
    *event.target.borrow_mut() = Value::Null;
    *event.current.borrow_mut() = Value::Null;
    *event.related_original.borrow_mut() = Value::Null;
    *event.related.borrow_mut() = Value::Null;
    event.phase.set(0);
    event.dispatching.set(false);
    event.stopped.set(false);
    event.immediate.set(false);
    event.canceled.set(false);
    event.passive.set(false);
    event.path.borrow_mut().clear();
    event.visibility.borrow_mut().clear();
    event.movement_x.set(0.0);
    event.movement_y.set(0.0);
}

pub(crate) fn mark_event_uninitialized(ctx: &mut Ctx, event: &Value) -> OpResult<()> {
    ctx.with_instance_mut::<DomEvent, _>(event, |event| mark_uninitialized(event))
        .map_err(|_| OpError::type_error("Event instance required"))
}
