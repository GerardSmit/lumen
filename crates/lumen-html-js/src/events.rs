//! HTML event targets. The `Event`/`EventTarget` core is shared with every other runtime
//! (`lumen_host::events`); this module adds what only a DOM has: the propagation path through
//! the tree and shadow roots, content-attribute handlers and the realm bookkeeping.
use super::*;
use lumen::embed::{JsFunction, JsHost, JsObject, OpError, OpResult};
use lumen_bind::{CtorRet, Host, This};
use lumen_host::events::{
    Callback, DeferredCompile, EventInit, EventPath, HandlerKind, PathEntry, TargetHooks,
};
use std::any::Any;
use std::rc::Weak;

pub(crate) use lumen_host::events::{Event as DomEvent, EventTarget as DomEventTarget, TargetData};

#[lumen_bind::module(name = "__dom_event_targets")]
pub(crate) mod target_bindings {
    use super::*;
    #[op(rename(js = "hasListeners"))]
    pub fn has_listeners(ctx: &mut Ctx, target: Value, inactive: Vec<Value>) -> OpResult<bool> {
        let target = ctx
            .with_instance::<DomEventTarget, _>(&target, |target| target.data_handle())
            .map_err(|_| OpError::new("TypeError", "Illegal EventTarget receiver"))?;
        Ok(target.has_listener_besides(&inactive))
    }
}

/// The source of a content-attribute event handler, compiled on first dispatch.
#[derive(Clone)]
pub(crate) struct RawContentHandler {
    pub(crate) node: NodeId,
    pub(crate) name: String,
    pub(crate) body: String,
    pub(crate) window_target: bool,
    #[allow(dead_code)]
    pub(crate) location: String,
}

/// The DOM side of an event target: its realm and node, and the dispatch hooks.
pub(crate) struct HtmlTarget {
    realm: RefCell<Weak<DomRealm>>,
    node: Cell<Option<NodeId>>,
    click_in_progress: Cell<bool>,
}

impl HtmlTarget {
    fn data(realm: Option<&Rc<DomRealm>>, node: Option<NodeId>) -> Rc<TargetData> {
        TargetData::new(Some(Rc::new(Self {
            realm: RefCell::new(realm.map_or_else(Weak::new, Rc::downgrade)),
            node: Cell::new(node),
            click_in_progress: Cell::new(false),
        })))
    }

    fn of(data: &TargetData) -> Option<&HtmlTarget> {
        data.hooks_as::<HtmlTarget>()
    }

    fn closed_roots(document: &lumen_html::Document, node: NodeId) -> OpResult<Vec<u128>> {
        let mut root = document.root_node(node, false).map_err(dom_error)?;
        let mut closed = Vec::new();
        while let Some(host) = document.shadow_host(root).map_err(dom_error)? {
            if document.shadow_mode(root).map_err(dom_error)? == Some(lumen_html::ShadowMode::Closed)
            {
                if closed.len() >= 64 {
                    return Err(OpError::new(
                        "RangeError",
                        "event shadow nesting limit exceeded",
                    ));
                }
                closed.push(root.key());
            }
            root = document.root_node(host, false).map_err(dom_error)?;
        }
        Ok(closed)
    }

    fn retarget_related(
        ctx: &mut Ctx,
        realm: &Rc<DomRealm>,
        related: &Value,
        related_node: &Option<(Rc<DomRealm>, NodeId)>,
        boundary: Option<NodeId>,
    ) -> OpResult<Value> {
        let Some((related_realm, id)) = related_node else {
            return Ok(related.clone());
        };
        if !Rc::ptr_eq(related_realm, realm) {
            return Ok(related.clone());
        }
        let id = realm
            .session
            .borrow()
            .document()
            .retarget(*id, boundary)
            .map_err(dom_error)?;
        Ok(realm.wrap(ctx, id))
    }
}

impl TargetHooks for HtmlTarget {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn event_path(
        &self,
        ctx: &mut Ctx,
        data: &Rc<TargetData>,
        receiver: &Value,
        event: &DomEvent,
        initial_target: &Value,
    ) -> OpResult<Option<EventPath>> {
        let (Some(realm), Some(mut node)) = (self.realm.borrow().upgrade(), self.node.get()) else {
            return Ok(None);
        };
        let related = event.related_original();
        let related_node = ctx
            .with_instance::<DomNode, _>(&related, |node| (node.realm.clone(), node.id))
            .ok();
        let original = node;
        let origin_root = realm
            .session
            .borrow()
            .document()
            .root_node(node, false)
            .map_err(dom_error)?;
        let mut entries = vec![PathEntry {
            value: receiver.clone(),
            target: data.clone(),
            adjusted: initial_target.clone(),
            closed: Self::closed_roots(realm.session.borrow().document(), node)?,
            related: related.clone(),
        }];
        loop {
            let parent = realm
                .session
                .borrow()
                .document()
                .event_parent(node, event.composed_flag(), origin_root)
                .map_err(|_| OpError::new("InvalidStateError", "event target was removed"))?;
            let Some(parent) = parent else {
                break;
            };
            if entries.len() >= 1024 {
                return Err(OpError::new("RangeError", "event path limit exceeded"));
            }
            let adjusted = realm
                .session
                .borrow()
                .document()
                .retarget(original, Some(parent))
                .map_err(dom_error)?;
            let adjusted = realm.wrap(ctx, adjusted);
            let related_adjusted =
                Self::retarget_related(ctx, &realm, &related, &related_node, Some(parent))?;
            if same(&adjusted, &related_adjusted) {
                break;
            }
            let closed = Self::closed_roots(realm.session.borrow().document(), parent)?;
            let value = realm.wrap(ctx, parent);
            if let Some(target) = realm.targets.borrow().get(&parent).and_then(Weak::upgrade) {
                entries.push(PathEntry {
                    value,
                    target,
                    adjusted,
                    closed,
                    related: related_adjusted,
                });
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
            let related_adjusted =
                Self::retarget_related(ctx, &realm, &related, &related_node, None)?;
            if let (Some(value), Some(target)) = (
                realm
                    .window_wrapper
                    .borrow()
                    .as_ref()
                    .and_then(WeakValue::upgrade),
                realm.window_target.borrow().as_ref(),
            ) {
                entries.push(PathEntry {
                    value,
                    target: target.clone(),
                    adjusted,
                    closed: Vec::new(),
                    related: related_adjusted,
                });
            }
        }
        if entries.iter().map(|entry| entry.closed.len()).sum::<usize>() > 4096 {
            return Err(OpError::new(
                "RangeError",
                "event shadow path limit exceeded",
            ));
        }
        let clear_target = {
            let session = realm.session.borrow();
            let document = session.document();
            let last = entries
                .last()
                .and_then(|entry| Self::of(&entry.target))
                .and_then(|target| target.node.get());
            let target = document.retarget(original, last).map_err(dom_error)?;
            document
                .shadow_host(document.root_node(target, false).map_err(dom_error)?)
                .map_err(dom_error)?
                .is_some()
        };
        Ok(Some(EventPath {
            entries,
            clear_target,
        }))
    }

    fn compile_deferred(&self, ctx: &mut Ctx, source: &Rc<dyn Any>) -> DeferredCompile {
        use super::event_content_handlers::Compilation;
        let Some(source) = source.downcast_ref::<RawContentHandler>() else {
            return DeferredCompile::Inactive;
        };
        let Some(realm) = self.realm.borrow().upgrade() else {
            return DeferredCompile::Inactive;
        };
        match super::event_content_handlers::compile(ctx, &realm, source) {
            Compilation::Compiled(function) => DeferredCompile::Compiled(function),
            Compilation::Failed if realm.has_browsing_context => DeferredCompile::Failed,
            Compilation::Failed | Compilation::Inactive => DeferredCompile::Inactive,
        }
    }

    fn is_global_scope(&self, ctx: &mut Ctx, current: &Value) -> bool {
        ctx.with_instance::<super::window_globals::DomWindow, _>(current, |_| ())
            .is_ok()
    }
}

/// The HTML-only operations of an event target.
pub(crate) trait HtmlTargetExt: Sized {
    fn window(realm: &Rc<DomRealm>) -> Self;
    fn node(realm: &Rc<DomRealm>, id: NodeId) -> Self;
    /// A target owned by a platform object rather than a DOM node (a `MediaQueryList`).
    fn independent(realm: &Rc<DomRealm>) -> Self;
    fn associated_realm(&self) -> Option<Rc<DomRealm>>;
    fn rebind_node(&self, realm: &Rc<DomRealm>, node: NodeId);
    fn try_begin_click(&self) -> bool;
    fn end_click(&self);
    fn trace_callback_values(&self, visit: &mut dyn FnMut(&Value));
    fn erase_listeners(&self, ctx: &mut Ctx, owner: Option<&Value>);
    fn handler(&self, kind: &str) -> Option<JsFunction>;
    fn has_handler(&self, kind: &str) -> bool;
    fn handler_value(&self, ctx: &mut Ctx, owner: &Value, kind: &str) -> OpResult<Value>;
    fn set_handler(&self, ctx: &mut Ctx, owner: &Value, kind: &str, callback: Option<JsFunction>);
    fn set_content_handler(
        &self,
        ctx: &mut Ctx,
        owner: &Value,
        kind: &str,
        handler: Option<RawContentHandler>,
    );
}

impl HtmlTargetExt for DomEventTarget {
    fn window(realm: &Rc<DomRealm>) -> Self {
        Self::from_data(HtmlTarget::data(Some(realm), None))
    }

    fn node(realm: &Rc<DomRealm>, id: NodeId) -> Self {
        if let Some(data) = realm.targets.borrow().get(&id).and_then(Weak::upgrade) {
            return Self::from_data(data);
        }
        let data = HtmlTarget::data(Some(realm), Some(id));
        realm.targets.borrow_mut().insert(id, Rc::downgrade(&data));
        Self::from_data(data)
    }

    fn independent(realm: &Rc<DomRealm>) -> Self {
        Self::from_data(HtmlTarget::data(Some(realm), None))
    }

    fn associated_realm(&self) -> Option<Rc<DomRealm>> {
        HtmlTarget::of(self.data())?.realm.borrow().upgrade()
    }

    fn rebind_node(&self, realm: &Rc<DomRealm>, node: NodeId) {
        let Some(html) = HtmlTarget::of(self.data()) else {
            return;
        };
        let previous_node = html.node.get();
        if let (Some(previous), Some(previous_node)) = (html.realm.borrow().upgrade(), previous_node)
        {
            previous.targets.borrow_mut().remove(&previous_node);
        }
        *html.realm.borrow_mut() = Rc::downgrade(realm);
        html.node.set(Some(node));
        if previous_node != Some(node) {
            self.data().for_each_deferred(|source| {
                let mut source = source.downcast_ref::<RawContentHandler>()?.clone();
                source.node = node;
                Some(Rc::new(source))
            });
        }
        realm
            .targets
            .borrow_mut()
            .insert(node, Rc::downgrade(self.data()));
    }

    fn try_begin_click(&self) -> bool {
        HtmlTarget::of(self.data()).is_some_and(|html| !html.click_in_progress.replace(true))
    }

    fn end_click(&self) {
        if let Some(html) = HtmlTarget::of(self.data()) {
            html.click_in_progress.set(false);
        }
    }

    /// Enumerate the exact values held by native listener storage. The engine discounts these
    /// bookkeeping edges and traces them from a reachable DOM wrapper, so detached handler/owner
    /// cycles do not become blanket roots.
    fn trace_callback_values(&self, visit: &mut dyn FnMut(&Value)) {
        self.data().trace_callbacks(visit);
    }

    fn erase_listeners(&self, ctx: &mut Ctx, owner: Option<&Value>) {
        if self.data().clear() {
            if let Some(owner) = owner {
                ctx.retain_instance(owner, false);
            }
        }
    }

    fn handler(&self, kind: &str) -> Option<JsFunction> {
        let cell = self.data().handler_cell(kind)?;
        let function = cell.borrow().function();
        function
    }

    fn has_handler(&self, kind: &str) -> bool {
        self.data().handler_cell(kind).is_some()
    }

    fn handler_value(&self, ctx: &mut Ctx, _owner: &Value, kind: &str) -> OpResult<Value> {
        let Some(cell) = self.data().handler_cell(kind) else {
            return Ok(Value::Null);
        };
        Ok(match self.data().resolve_deferred(ctx, &cell) {
            Callback::Function(function)
            | Callback::Deferred {
                compiled: Some(function),
                ..
            } => function.value().clone(),
            _ => Value::Null,
        })
    }

    fn set_handler(&self, ctx: &mut Ctx, owner: &Value, kind: &str, callback: Option<JsFunction>) {
        let _html_allocations = enter_html_allocation_category();
        self.data()
            .set_handler(kind, callback.map(Callback::Function), HandlerKind::Html);
        DomEventTarget::update_retention(ctx, self.data(), owner);
    }

    fn set_content_handler(
        &self,
        ctx: &mut Ctx,
        owner: &Value,
        kind: &str,
        handler: Option<RawContentHandler>,
    ) {
        let _html_allocations = enter_html_allocation_category();
        self.data().set_handler(
            kind,
            handler.map(|source| Callback::Deferred {
                source: Rc::new(source),
                compiled: None,
            }),
            HandlerKind::Html,
        );
        DomEventTarget::update_retention(ctx, self.data(), owner);
    }
}

/// Erase a node's event listeners, including content-attribute and IDL handler
/// callbacks, only when it has a live sparse-registry entry. The owner is the
/// existing JS wrapper, when one is still live, so callback retention is
/// released without creating wrappers for untouched DOM nodes.
pub(crate) fn erase_node_listeners(
    ctx: &mut Ctx,
    realm: &DomRealm,
    node: NodeId,
    owner: Option<&Value>,
) {
    let target = realm.targets.borrow().get(&node).and_then(Weak::upgrade);
    if let Some(target) = target {
        DomEventTarget::from_data(target).erase_listeners(ctx, owner);
    }
}

pub(crate) fn erase_window_listeners(ctx: &mut Ctx, realm: &DomRealm, owner: Option<&Value>) {
    let target = realm.window_target.borrow().clone();
    if let Some(target) = target {
        DomEventTarget::from_data(target).erase_listeners(ctx, owner);
    }
}

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Obj(a), Value::Obj(b)) => std::ptr::eq(&**a, &**b),
        _ => false,
    }
}

/// `dispatchEvent` for an event a platform object creates: its trust stays as constructed.
pub(crate) fn dispatch_event(ctx: &mut Ctx, this: This<Value>, event: JsObject) -> OpResult<bool> {
    DomEventTarget::dispatch(ctx, &this.0, event.value())
}

pub(crate) fn add_event_listener(
    ctx: &mut Ctx,
    this: This<Value>,
    kind: &str,
    callback: Value,
    options: Option<Value>,
) -> OpResult<()> {
    DomEventTarget::add_event_listener(
        ctx,
        this,
        Value::str(kind),
        callback,
        options.unwrap_or(Value::Undefined),
    )
}

pub(crate) fn remove_event_listener(
    ctx: &mut Ctx,
    this: This<Value>,
    kind: &str,
    callback: Value,
    options: Option<Value>,
) -> OpResult<()> {
    DomEventTarget::remove_event_listener(
        ctx,
        this,
        Value::str(kind),
        callback,
        options.unwrap_or(Value::Undefined),
    )
}

/// Dispatch an event the user agent created: `isTrusted` is true.
pub(crate) fn dispatch_user_agent_event(
    ctx: &mut Ctx,
    this: This<Value>,
    event: JsObject,
) -> OpResult<bool> {
    DomEventTarget::dispatch_trusted(ctx, &this.0, event.value())
}

/// HTML's Window lifecycle dispatch uses its associated Document as the
/// legacy target override, while listeners and the event path remain Window's.
pub(crate) fn dispatch_user_agent_event_with_target(
    ctx: &mut Ctx,
    this: This<Value>,
    event: JsObject,
    target_override: Value,
) -> OpResult<bool> {
    DomEventTarget::dispatch_trusted_with_target(ctx, &this.0, event.value(), target_override)
}

pub(crate) fn mark_event_uninitialized(ctx: &mut Ctx, event: &Value) -> OpResult<()> {
    ctx.with_instance::<DomEvent, _>(event, |event| event.mark_uninitialized())
        .map_err(|_| OpError::type_error("Event instance required"))
}

#[lumen_bind::class(name = "SubmitEvent", extends = DomEvent, hint(js(webidl)))]
pub(crate) struct DomSubmitEvent {
    base: DomEvent,
    submitter_slot: Option<String>,
}

struct SubmitEventConstructor {
    event: DomSubmitEvent,
    submitter: Option<Value>,
}

impl SubmitEventConstructor {
    fn into_instance(self, ctx: &mut Ctx) -> Result<Value, Value> {
        let slot = self.event.submitter_slot.clone();
        let Self { event, submitter } = self;
        let instance = ctx.new_instance(event);
        if let (Some(slot), Some(submitter)) = (slot, submitter) {
            ctx.define_native_private_value_slot(&instance, &slot, submitter)?;
        }
        Ok(instance)
    }
}

impl CtorRet<JsHost, DomSubmitEvent> for SubmitEventConstructor {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        let slot = self.event.submitter_slot.clone();
        let Self { event, submitter } = self;
        let instance = <JsHost as Host>::construct(cx, event)?;
        <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
            if let (Some(slot), Some(submitter)) = (slot, submitter) {
                ctx.define_native_private_value_slot(&instance, &slot, submitter)?;
            }
            Ok(())
        })?;
        Ok(instance)
    }
}

#[lumen_bind::methods]
impl DomSubmitEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<SubmitEventConstructor> {
        let dictionary = options.clone();
        let init = EventInit::read(ctx, &options)?;
        let submitter = match super::ui_events::dictionary_member(ctx, &dictionary, "submitter")? {
            None | Some(Value::Null | Value::Undefined) => None,
            Some(value) => {
                ctx.with_instance::<super::DomHtmlElement, _>(&value, |_| ())
                    .map_err(|_| {
                        OpError::type_error("SubmitEvent submitter must be an HTMLElement")
                    })?;
                Some(value)
            }
        };
        let submitter_slot = submitter
            .as_ref()
            .map(|_| ctx.allocate_native_private_slot_name());
        Ok(SubmitEventConstructor {
            event: Self {
                base: DomEvent::from_init(kind, init),
                submitter_slot,
            },
            submitter,
        })
    }

    #[getter]
    fn submitter(&self, ctx: &mut Ctx, this: This<Value>) -> Value {
        self.submitter_slot
            .as_deref()
            .and_then(|slot| ctx.native_private_value_slot(&this.0, slot))
            .unwrap_or(Value::Null)
    }
}

impl DomSubmitEvent {
    pub(crate) fn for_user_agent(
        ctx: &mut Ctx,
        kind: &str,
        submitter: Option<Value>,
    ) -> OpResult<Value> {
        let options = ctx.new_object_with_proto(&Value::Null);
        for (name, value) in [
            ("bubbles", Value::Bool(true)),
            ("cancelable", Value::Bool(true)),
            ("composed", Value::Bool(false)),
        ] {
            ctx.member_set(&options, name, value)
                .map_err(OpError::thrown)?;
        }
        let slot = submitter
            .as_ref()
            .map(|_| ctx.allocate_native_private_slot_name());
        let base = DomEvent::new(ctx, kind, Some(options))?;
        let instance = ctx.new_instance(Self {
            base,
            submitter_slot: slot.clone(),
        });
        if let (Some(slot), Some(submitter)) = (slot, submitter) {
            ctx.define_native_private_value_slot(&instance, &slot, submitter)
                .map_err(OpError::thrown)?;
        }
        Ok(instance)
    }
}

#[lumen_bind::class(name = "FormDataEvent", extends = DomEvent, hint(js(webidl)))]
pub(crate) struct DomFormDataEvent {
    base: DomEvent,
    form_data_slot: String,
}

struct FormDataEventConstructor {
    event: DomFormDataEvent,
    form_data: Value,
}

impl FormDataEventConstructor {
    fn into_instance(self, ctx: &mut Ctx) -> Result<Value, Value> {
        let slot = self.event.form_data_slot.clone();
        let Self { event, form_data } = self;
        let instance = ctx.new_instance(event);
        ctx.define_native_private_value_slot(&instance, &slot, form_data)?;
        Ok(instance)
    }
}

impl CtorRet<JsHost, DomFormDataEvent> for FormDataEventConstructor {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        let slot = self.event.form_data_slot.clone();
        let Self { event, form_data } = self;
        let instance = <JsHost as Host>::construct(cx, event)?;
        <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
            ctx.define_native_private_value_slot(&instance, &slot, form_data)?;
            Ok(())
        })?;
        Ok(instance)
    }
}

#[lumen_bind::methods]
impl DomFormDataEvent {
    #[constructor(coerce)]
    fn new(
        ctx: &mut Ctx,
        kind: &str,
        options: Option<Value>,
    ) -> OpResult<FormDataEventConstructor> {
        let dictionary = options.clone();
        let init = EventInit::read(ctx, &options)?;
        let form_data = super::ui_events::dictionary_member(ctx, &dictionary, "formData")?
            .filter(|value| !matches!(value, Value::Null | Value::Undefined))
            .ok_or_else(|| OpError::type_error("FormDataEvent requires a FormData object"))?;
        if !form_data_bridge::is_form_data(ctx, &form_data)? {
            return Err(OpError::type_error(
                "FormDataEvent requires a FormData object",
            ));
        }
        Ok(FormDataEventConstructor {
            event: Self {
                base: DomEvent::from_init(kind, init),
                form_data_slot: ctx.allocate_native_private_slot_name(),
            },
            form_data,
        })
    }

    #[getter]
    fn form_data(&self, ctx: &mut Ctx, this: This<Value>) -> Value {
        ctx.native_private_value_slot(&this.0, &self.form_data_slot)
            .unwrap_or(Value::Null)
    }
}

impl DomFormDataEvent {
    pub(crate) fn for_user_agent(ctx: &mut Ctx, kind: &str, form_data: Value) -> OpResult<Value> {
        let options = ctx.new_object_with_proto(&Value::Null);
        ctx.member_set(&options, "bubbles", Value::Bool(true))
            .map_err(OpError::thrown)?;
        ctx.member_set(&options, "cancelable", Value::Bool(false))
            .map_err(OpError::thrown)?;
        ctx.member_set(&options, "composed", Value::Bool(false))
            .map_err(OpError::thrown)?;
        let slot = ctx.allocate_native_private_slot_name();
        let base = DomEvent::new(ctx, kind, Some(options))?;
        let instance = ctx.new_instance(Self {
            base,
            form_data_slot: slot.clone(),
        });
        ctx.define_native_private_value_slot(&instance, &slot, form_data)
            .map_err(OpError::thrown)?;
        Ok(instance)
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
    fn event_timestamps_share_the_performance_clock_and_survive_initialization_and_dispatch() {
        let value = eval(
            r#"(() => {
                for (const constructor of [Event, CustomEvent, MouseEvent, KeyboardEvent, WheelEvent, FocusEvent]) {
                    const before = performance.now();
                    const event = new constructor('initial');
                    const after = performance.now();
                    const timestamp = event.timeStamp;
                    if (!Number.isFinite(timestamp) || timestamp < before || timestamp > after)
                        throw new Error('event clock differs from performance clock');
                    if (Math.abs(timestamp * 10 - Math.round(timestamp * 10)) > 1e-6)
                        throw new Error('event timestamp is not on the shared 100us clock grid');
                    event.initEvent('changed', false, false);
                    const target = new EventTarget();
                    target.dispatchEvent(event);
                    target.dispatchEvent(event);
                    if (event.timeStamp !== timestamp)
                        throw new Error('initialization or dispatch changed creation time');
                }
                const legacy = document.createEvent('Event');
                const timestamp = legacy.timeStamp;
                legacy.initEvent('legacy', false, false);
                return legacy.timeStamp === timestamp &&
                    Object.hasOwn(Event.prototype, 'timeStamp');
            })()"#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn submit_and_formdata_events_retain_singleton_values_without_changing_identity() {
        let value = eval(
            r#"(() => {
                const failures = [];
                const check = (ok, name) => { if (!ok) failures.push(name); };
                const button = document.createElement('button');
                const data = new FormData();
                const submit = new SubmitEvent('submit', {submitter: button});
                const formdata = new FormDataEvent('formdata', {formData: data});
                class CustomSubmit extends SubmitEvent {}
                const custom = new CustomSubmit('submit', {submitter: button});
                formdata.formData.append('x', 'y');
                let badSubmitterRejected = false;
                try { new SubmitEvent('submit', {submitter: {}}); }
                catch (error) { badSubmitterRejected = error instanceof TypeError; }
                check(submit.submitter === button, 'submitter-identity');
                check(custom instanceof CustomSubmit && custom.submitter === button,
                    'subclass-construction-and-retention');
                check(new SubmitEvent('submit', {submitter: null}).submitter === null,
                    'null-submitter');
                check(formdata.formData === data, 'formdata-identity');
                check(data.get('x') === 'y', 'formdata-live-mutation');
                check(badSubmitterRejected, 'invalid-submitter-brand');
                return failures.join('|');
            })()"#,
        );
        let Value::Str(failures) = value else {
            panic!("event singleton contract must return diagnostics");
        };
        assert!(failures.is_empty(), "event singleton failures: {failures}");
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

    #[test]
    fn listener_options_control_phase_order_once_passive_and_signal_removal() {
        let value = eval(
            r#"(() => {
                const failures = [];
                const check = (ok, name) => { if (!ok) failures.push(name); };
                const parent = document.createElement('div');
                const child = document.createElement('span');
                parent.appendChild(child);
                const order = [];
                parent.addEventListener('x', () => order.push('parent-bubble'));
                parent.addEventListener('x', () => order.push('parent-capture'), {capture: true});
                child.addEventListener('x', () => order.push('child-bubble'));
                child.addEventListener('x', () => order.push('child-capture'), true);
                child.dispatchEvent(new Event('x', {bubbles: true}));
                check(order.join() === 'parent-capture,child-capture,child-bubble,parent-bubble',
                    'phase-order:' + order.join());

                let onceCalls = 0;
                const once = () => onceCalls++;
                child.addEventListener('once', once, {once: true});
                child.dispatchEvent(new Event('once'));
                child.dispatchEvent(new Event('once'));
                check(onceCalls === 1, 'once');

                let passiveCanceled = true;
                child.addEventListener('p', event => event.preventDefault(), {passive: true});
                const passiveEvent = new Event('p', {cancelable: true});
                child.dispatchEvent(passiveEvent);
                passiveCanceled = passiveEvent.defaultPrevented;
                check(!passiveCanceled, 'passive');

                const controller = new AbortController();
                let signalCalls = 0;
                child.addEventListener('s', () => signalCalls++, {signal: controller.signal});
                child.dispatchEvent(new Event('s'));
                controller.abort();
                child.dispatchEvent(new Event('s'));
                check(signalCalls === 1, 'signal');
                const aborted = AbortSignal.abort();
                child.addEventListener('s', () => signalCalls++, {signal: aborted});
                child.dispatchEvent(new Event('s'));
                check(signalCalls === 1, 'already-aborted-signal');

                let badSignal = false;
                try { child.addEventListener('s', () => {}, {signal: {}}); }
                catch (error) { badSignal = error instanceof TypeError; }
                check(badSignal, 'invalid-signal');

                let immediate = 0;
                child.addEventListener('i', event => { immediate++; event.stopImmediatePropagation(); });
                child.addEventListener('i', () => immediate++);
                child.dispatchEvent(new Event('i'));
                check(immediate === 1, 'stop-immediate');
                return failures.join('|');
            })()"#,
        );
        let Value::Str(failures) = value else {
            panic!("listener option contract must return diagnostics");
        };
        assert!(failures.is_empty(), "listener option failures: {failures}");
    }

    #[test]
    fn is_trusted_is_unforgeable_and_user_agent_events_are_trusted() {
        let value = eval(
            r#"(() => {
                const failures = [];
                const check = (ok, name) => { if (!ok) failures.push(name); };
                const event = new Event('x');
                const own = Object.getOwnPropertyDescriptor(event, 'isTrusted');
                check(own && own.configurable === false && own.enumerable === true,
                    'own-unforgeable-accessor');
                check(own && own.get === Object.getOwnPropertyDescriptor(Event.prototype, 'isTrusted').get,
                    'shared-getter');
                let redefined = false;
                try { Object.defineProperty(event, 'isTrusted', {value: true}); }
                catch (error) { redefined = error instanceof TypeError; }
                check(redefined, 'not-redefinable');
                window.dispatchEvent(event);
                check(event.isTrusted === false, 'script-dispatch-untrusted');
                let trusted = null;
                const button = document.createElement('button');
                button.addEventListener('click', event => { trusted = event.isTrusted; });
                button.click();
                check(trusted !== null, 'click-dispatched');
                return failures.join('|');
            })()"#,
        );
        let Value::Str(failures) = value else {
            panic!("isTrusted contract must return diagnostics");
        };
        assert!(failures.is_empty(), "isTrusted failures: {failures}");
    }

    #[test]
    fn abort_signals_and_dom_exceptions_come_from_the_shared_core() {
        let value = eval(
            r#"(() => {
                const failures = [];
                const check = (ok, name) => { if (!ok) failures.push(name); };
                const aborted = AbortSignal.abort();
                const reason = aborted.reason;
                check(aborted.aborted && reason instanceof DOMException && reason instanceof Error &&
                    reason.name === 'AbortError' && reason.code === 20, 'default-reason');
                const custom = {};
                check(AbortSignal.abort(custom).reason === custom, 'custom-reason');
                const first = new AbortController();
                const second = new AbortController();
                const any = AbortSignal.any([first.signal, second.signal]);
                let anyEvents = 0;
                any.addEventListener('abort', () => anyEvents++);
                second.abort('why');
                check(any.aborted && any.reason === 'why' && anyEvents === 1, 'any');
                let thrown = null;
                try { aborted.throwIfAborted(); } catch (error) { thrown = error; }
                check(thrown === reason, 'throw-if-aborted');
                check(Object.getPrototypeOf(AbortSignal.prototype) === EventTarget.prototype,
                    'signal-prototype');
                let illegal = false;
                try { new AbortSignal(); } catch (error) { illegal = error instanceof TypeError; }
                check(illegal, 'illegal-constructor');
                const exception = new DOMException('message', 'NotFoundError');
                check(exception.code === 8 && DOMException.NOT_FOUND_ERR === 8 &&
                    DOMException.prototype.NOT_FOUND_ERR === 8 && typeof exception.stack === 'string',
                    'dom-exception-shape');
                return failures.join('|');
            })()"#,
        );
        let Value::Str(failures) = value else {
            panic!("abort contract must return diagnostics");
        };
        assert!(failures.is_empty(), "abort failures: {failures}");
    }

    #[test]
    fn closed_shadow_roots_retarget_events_and_hide_composed_path_entries() {
        let value = eval(
            r#"(() => {
                const failures = [];
                const check = (ok, name) => { if (!ok) failures.push(name); };
                const host = document.createElement('div');
                document.body.appendChild(host);
                const root = host.attachShadow({mode: 'closed'});
                const inner = document.createElement('span');
                root.appendChild(inner);
                let outsideTarget = null, outsidePath = null, insideTarget = null, insidePath = null;
                document.body.addEventListener('x', event => {
                    outsideTarget = event.target;
                    outsidePath = event.composedPath();
                });
                root.addEventListener('x', event => {
                    insideTarget = event.target;
                    insidePath = event.composedPath();
                });
                const event = new Event('x', {bubbles: true, composed: true});
                inner.dispatchEvent(event);
                check(outsideTarget === host, 'retargeted-to-host');
                check(outsidePath && !outsidePath.includes(inner) && !outsidePath.includes(root) &&
                    outsidePath[0] === host, 'closed-path-hidden');
                check(insideTarget === inner, 'inside-target');
                check(insidePath && insidePath[0] === inner && insidePath.includes(root),
                    'inside-path-visible');
                check(event.target === host && event.composedPath().length === 0,
                    'retargeted-after-dispatch');
                const contained = new Event('y', {bubbles: true});
                inner.dispatchEvent(contained);
                check(contained.target === null && contained.composedPath().length === 0,
                    'cleared-after-contained-dispatch');
                return failures.join('|');
            })()"#,
        );
        let Value::Str(failures) = value else {
            panic!("shadow retargeting contract must return diagnostics");
        };
        assert!(failures.is_empty(), "shadow retargeting failures: {failures}");
    }

    #[test]
    fn content_attribute_handlers_compile_lazily_and_null_removes_them() {
        let value = eval(
            r#"(() => {
                const failures = [];
                const check = (ok, name) => { if (!ok) failures.push(name); };
                const button = document.createElement('button');
                button.setAttribute('onclick', 'globalThis.attributeClicks = (globalThis.attributeClicks || 0) + 1');
                check(typeof button.onclick === 'function', 'compiled-on-read');
                button.dispatchEvent(new Event('click'));
                check(globalThis.attributeClicks === 1, 'ran');
                button.onclick = null;
                button.dispatchEvent(new Event('click'));
                check(button.onclick === null && globalThis.attributeClicks === 1, 'null-removes');
                button.onclick = event => false;
                const cancelable = new Event('click', {cancelable: true});
                button.dispatchEvent(cancelable);
                check(cancelable.defaultPrevented, 'false-cancels');
                return failures.join('|');
            })()"#,
        );
        let Value::Str(failures) = value else {
            panic!("content handler contract must return diagnostics");
        };
        assert!(failures.is_empty(), "content handler failures: {failures}");
    }
}
