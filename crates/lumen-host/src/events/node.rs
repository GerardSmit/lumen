//! Node's extensions of the event target: `NodeEventTarget` and the hidden
//! `__eventTargetInternals` object that `lumen-node`, `lumen-web` and `lumen-runtime` glue read.

use super::bindings::EventTarget;
use super::target::{self, Callback, HandlerKind};
use super::*;

/// The `on<type>` getter of a Node-style event handler (`defineEventHandler`, `AbortSignal`).
pub(crate) fn node_handler_get(ctx: &mut Ctx, receiver: &Value, kind: &str) -> OpResult<Value> {
    let (data, _) = EventTarget::of_receiver(ctx, receiver)?;
    Ok(data
        .handler_cell(kind)
        .and_then(|cell| cell.borrow().function())
        .map_or(Value::Null, |function| function.value().clone()))
}

/// The `on<type>` setter: a function becomes the handler, anything else empties it. The handler
/// keeps the position of its first registration, also across `null`.
pub(crate) fn node_handler_set(
    ctx: &mut Ctx,
    receiver: &Value,
    kind: &str,
    value: Value,
) -> OpResult<()> {
    let (data, receiver) = EventTarget::of_receiver(ctx, receiver)?;
    let active = |data: &TargetData| {
        data.handler_cell(kind)
            .is_some_and(|cell| cell.borrow().function().is_some())
    };
    let was_active = active(&data);
    let function = JsFunction::from_value(value);
    let callback_value = function.as_ref().map(|function| function.value().clone());
    data.set_handler(kind, function.map(Callback::Function), HandlerKind::Node);
    EventTarget::update_retention(ctx, &data, &receiver);
    let size = data.listener_count(kind);
    match (was_active, callback_value) {
        (false, Some(callback)) => {
            target::new_listener_hook(ctx, &receiver, size, kind, &callback, [false; 4])
        }
        (true, None) => {
            target::remove_listener_hook(ctx, &receiver, size, kind, &Value::Undefined, false)
        }
        _ => Ok(()),
    }
}

fn node_target(ctx: &mut Ctx, value: &Value) -> Option<Rc<TargetData>> {
    ctx.with_instance::<EventTarget, _>(value, |target| target.data.clone())
        .ok()
}

fn is_node_target(ctx: &mut Ctx, value: &Value) -> bool {
    ctx.with_instance::<internals::NodeEventTarget, _>(value, |_| ())
        .is_ok()
}

#[lumen_bind::module(name = "eventTargetInternals")]
pub mod internals {
    use super::super::bindings::{CustomEvent, Event, EventTarget};
    use super::super::target::{default_max_listeners, ListenerOptions};
    use super::super::*;
    use super::{is_node_target, node_handler_get, node_handler_set, node_target};
    use crate::webidl::invalid_arg_type;
    use lumen_bind::This;

    /// `EventTarget` with Node's emitter-style methods (`on`, `once`, `emit`, ...).
    #[class(name = "NodeEventTarget", extends = EventTarget, skip(js), hint(js(webidl, invalid_this)))]
    pub struct NodeEventTarget {
        base: EventTarget,
    }

    #[methods]
    impl NodeEventTarget {
        #[constructor]
        fn new() -> Self {
            Self {
                base: EventTarget::new(),
            }
        }

        fn set_max_listeners(ctx: &mut Ctx, this: This<Value>, n: Value) -> OpResult<Value> {
            if !is_node_target(ctx, &this.0) {
                return Err(invalid_this("NodeEventTarget"));
            }
            let global = ctx.global_object();
            let key = ctx.symbol_for("lumen.EventEmitter");
            let emitter = ctx
                .reflect_get(&global, &key, &global)
                .map_err(OpError::thrown)?;
            if matches!(emitter, Value::Obj(_)) {
                let set = ctx
                    .member_get(&emitter, "setMaxListeners")
                    .map_err(OpError::thrown)?;
                ctx.invoke(set, emitter, &[n, this.0.clone()])
                    .map_err(OpError::thrown)?;
            } else {
                let key = ctx.symbol_for(MAX_LISTENERS);
                set_data_property(ctx, &this.0, key, n)?;
            }
            Ok(this.0)
        }

        fn get_max_listeners(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            if !is_node_target(ctx, &this.0) {
                return Err(invalid_this("NodeEventTarget"));
            }
            let key = ctx.symbol_for(MAX_LISTENERS);
            match ctx
                .reflect_get(&this.0, &key, &this.0)
                .map_err(OpError::thrown)?
            {
                Value::Undefined => Ok(Value::Num(default_max_listeners(ctx))),
                max => Ok(max),
            }
        }

        fn event_names(&self, ctx: &mut Ctx) -> Value {
            let names = self
                .base
                .data
                .event_names()
                .into_iter()
                .map(|name| Value::str(&*name))
                .collect();
            ctx.make_array(names)
        }

        #[method(coerce)]
        fn listener_count(&self, kind: &str) -> usize {
            self.base.data.listener_count(kind)
        }

        fn off(
            ctx: &mut Ctx,
            this: This<Value>,
            kind: Value,
            listener: Value,
            #[default(Value::Undefined)] options: Value,
        ) -> OpResult<Value> {
            remove(ctx, &this.0, kind, listener, options)?;
            Ok(this.0)
        }

        fn remove_listener(
            ctx: &mut Ctx,
            this: This<Value>,
            kind: Value,
            listener: Value,
            #[default(Value::Undefined)] options: Value,
        ) -> OpResult<Value> {
            remove(ctx, &this.0, kind, listener, options)?;
            Ok(this.0)
        }

        fn on(ctx: &mut Ctx, this: This<Value>, kind: Value, listener: Value) -> OpResult<Value> {
            add(ctx, &this.0, kind, listener, false)?;
            Ok(this.0)
        }

        fn add_listener(
            ctx: &mut Ctx,
            this: This<Value>,
            kind: Value,
            listener: Value,
        ) -> OpResult<Value> {
            add(ctx, &this.0, kind, listener, false)?;
            Ok(this.0)
        }

        fn once(ctx: &mut Ctx, this: This<Value>, kind: Value, listener: Value) -> OpResult<Value> {
            add(ctx, &this.0, kind, listener, true)?;
            Ok(this.0)
        }

        fn emit(
            ctx: &mut Ctx,
            this: This<Value>,
            kind: Value,
            #[default(Value::Undefined)] arg: Value,
        ) -> OpResult<bool> {
            if !is_node_target(ctx, &this.0) {
                return Err(invalid_this("NodeEventTarget"));
            }
            let Value::Str(kind) = &kind else {
                return Err(invalid_arg_type(ctx, "type", "of type string", &kind));
            };
            EventTarget::emit(ctx, &this.0, kind, arg)
        }

        fn remove_all_listeners(
            ctx: &mut Ctx,
            this: This<Value>,
            #[default(Value::Undefined)] kind: Value,
        ) -> OpResult<Value> {
            if !is_node_target(ctx, &this.0) {
                return Err(invalid_this("NodeEventTarget"));
            }
            let (data, receiver) = EventTarget::of_receiver(ctx, &this.0)?;
            let kind = match kind {
                Value::Undefined => None,
                kind => Some(ctx.coerce_string(&kind).map_err(OpError::thrown)?),
            };
            EventTarget::remove_all(ctx, &receiver, &data, kind.as_deref());
            Ok(this.0)
        }
    }

    fn add(
        ctx: &mut Ctx,
        receiver: &Value,
        kind: Value,
        listener: Value,
        once: bool,
    ) -> OpResult<()> {
        if !is_node_target(ctx, receiver) {
            return Err(invalid_this("NodeEventTarget"));
        }
        let (data, receiver) = EventTarget::of_receiver(ctx, receiver)?;
        let kind = ctx.coerce_string(&kind).map_err(OpError::thrown)?;
        let options = ListenerOptions {
            once,
            node_style: true,
            ..ListenerOptions::default()
        };
        EventTarget::add_listener(ctx, &receiver, &data, &kind, listener, options)
    }

    fn remove(
        ctx: &mut Ctx,
        receiver: &Value,
        kind: Value,
        listener: Value,
        options: Value,
    ) -> OpResult<()> {
        if !is_node_target(ctx, receiver) {
            return Err(invalid_this("NodeEventTarget"));
        }
        let (data, receiver) = EventTarget::of_receiver(ctx, receiver)?;
        let kind = ctx.coerce_string(&kind).map_err(OpError::thrown)?;
        let capture = ListenerOptions::read(ctx, &options, false)?.capture;
        if !matches!(listener, Value::Obj(_)) {
            return Ok(());
        }
        EventTarget::remove_listener(ctx, &receiver, &data, &kind, &listener, capture)
    }

    /// The hidden object published as `globalThis.__eventTargetInternals`.
    #[class(name = "EventTargetInternals", skip(js))]
    pub struct EventTargetInternals;

    #[constant(name = "__eventTargetInternals")]
    const INTERNALS: EventTargetInternals = EventTargetInternals;

    fn registry_symbol(ctx: &mut Ctx, key: &str) -> Value {
        ctx.symbol_for(key)
    }

    #[methods]
    impl EventTargetInternals {
        #[getter(name = "Event")]
        fn event(ctx: &mut Ctx, _this: This<Value>) -> Value {
            ctx.class_constructor::<Event>()
        }

        #[getter(name = "CustomEvent")]
        fn custom_event(ctx: &mut Ctx, _this: This<Value>) -> Value {
            ctx.class_constructor::<CustomEvent>()
        }

        #[getter(name = "EventTarget")]
        fn event_target(ctx: &mut Ctx, _this: This<Value>) -> Value {
            ctx.class_constructor::<EventTarget>()
        }

        #[getter(name = "NodeEventTarget")]
        fn node_event_target(ctx: &mut Ctx, _this: This<Value>) -> OpResult<Value> {
            let constructor = ctx.class_constructor::<NodeEventTarget>();
            let key = Value::str("defaultMaxListeners");
            if !ctx
                .has_own_property_value(&constructor, &key)
                .map_err(OpError::thrown)?
            {
                let get = ctx.new_native_fn(
                    "get defaultMaxListeners",
                    0,
                    Rc::new(|ctx: &mut Ctx, _: Value, _: &[Value]| {
                        Ok(Value::Num(default_max_listeners(ctx)))
                    }),
                );
                let descriptor = ctx.new_object_with_proto(&Value::Null);
                ctx.member_set(&descriptor, "get", get)
                    .map_err(OpError::thrown)?;
                ctx.member_set(&descriptor, "enumerable", Value::Bool(false))
                    .map_err(OpError::thrown)?;
                ctx.member_set(&descriptor, "configurable", Value::Bool(true))
                    .map_err(OpError::thrown)?;
                ctx.define_property_value(&constructor, key, &descriptor)
                    .map_err(OpError::thrown)?;
            }
            Ok(constructor)
        }

        #[getter(name = "kEvents")]
        fn k_events(ctx: &mut Ctx, _this: This<Value>) -> Value {
            registry_symbol(ctx, EVENTS)
        }

        #[getter(name = "kWeakHandler")]
        fn k_weak_handler(ctx: &mut Ctx, _this: This<Value>) -> Value {
            registry_symbol(ctx, WEAK_HANDLER)
        }

        #[getter(name = "kResistStopPropagation")]
        fn k_resist_stop_propagation(ctx: &mut Ctx, _this: This<Value>) -> Value {
            registry_symbol(ctx, RESIST_STOP_PROPAGATION)
        }

        #[getter(name = "kMaxEventTargetListeners")]
        fn k_max_event_target_listeners(ctx: &mut Ctx, _this: This<Value>) -> Value {
            registry_symbol(ctx, MAX_LISTENERS)
        }

        #[getter(name = "kMaxEventTargetListenersWarned")]
        fn k_max_event_target_listeners_warned(ctx: &mut Ctx, _this: This<Value>) -> Value {
            registry_symbol(ctx, MAX_LISTENERS_WARNED)
        }

        #[getter(name = "kTrustEvent")]
        fn k_trust_event(ctx: &mut Ctx, _this: This<Value>) -> Value {
            EventSymbols::get(ctx).trust.clone()
        }

        #[getter(name = "kNewListener")]
        fn k_new_listener(ctx: &mut Ctx, _this: This<Value>) -> Value {
            EventSymbols::get(ctx).new_listener.clone()
        }

        #[getter(name = "kRemoveListener")]
        fn k_remove_listener(ctx: &mut Ctx, _this: This<Value>) -> Value {
            EventSymbols::get(ctx).remove_listener.clone()
        }

        /// Define Node-style `on<name>` accessors for `type` (default `name`) on `target`.
        #[method(name = "defineEventHandler")]
        fn define_event_handler(
            ctx: &mut Ctx,
            _this: This<Value>,
            target: Value,
            name: String,
            #[default(Value::Undefined)] kind: Value,
        ) -> OpResult<()> {
            let kind: Rc<str> = match kind {
                Value::Undefined => name.as_str().into(),
                kind => ctx.coerce_string(&kind).map_err(OpError::thrown)?,
            };
            let get_kind = kind.clone();
            let getter = ctx.new_native_fn(
                &format!("get on{name}"),
                0,
                Rc::new(move |ctx: &mut Ctx, this: Value, _: &[Value]| {
                    node_handler_get(ctx, &this, &get_kind).map_err(|error| error.to_value(ctx))
                }),
            );
            let setter = ctx.new_native_fn(
                &format!("set on{name}"),
                1,
                Rc::new(move |ctx: &mut Ctx, this: Value, args: &[Value]| {
                    let value = args.first().cloned().unwrap_or(Value::Undefined);
                    node_handler_set(ctx, &this, &kind, value)
                        .map_err(|error| error.to_value(ctx))?;
                    Ok(Value::Undefined)
                }),
            );
            let descriptor = ctx.new_object_with_proto(&Value::Null);
            for (key, value) in [
                ("get", getter),
                ("set", setter),
                ("enumerable", Value::Bool(true)),
                ("configurable", Value::Bool(true)),
            ] {
                ctx.member_set(&descriptor, key, value)
                    .map_err(OpError::thrown)?;
            }
            ctx.define_property_value(&target, Value::str(&format!("on{name}")), &descriptor)
                .map_err(OpError::thrown)
        }

        /// Whether `target` has a listener that would run.
        #[method(name = "hasListeners")]
        fn has_listeners(ctx: &mut Ctx, _this: This<Value>, target: Value) -> bool {
            node_target(ctx, &target).is_some_and(|data| data.has_listeners())
        }

        /// The callbacks registered for `kind` (Node's `getEventListeners`).
        fn listeners(ctx: &mut Ctx, _this: This<Value>, target: Value, kind: String) -> Value {
            let callbacks = node_target(ctx, &target)
                .map(|data| data.callbacks(&kind))
                .unwrap_or_default();
            ctx.make_array(callbacks)
        }

        #[method(name = "cloneTransferableSignal")]
        fn clone_transferable_signal(
            ctx: &mut Ctx,
            _this: This<Value>,
            signal: Value,
        ) -> OpResult<Value> {
            super::super::abort::clone_transferable(ctx, &signal)
        }

        /// `target.emit`-style dispatch: node-style listeners get `arg`; DOM-style ones get the
        /// event `factory()` returns, created when the first of them runs.
        #[method(name = "emitLazy")]
        fn emit_lazy(
            ctx: &mut Ctx,
            _this: This<Value>,
            target: Value,
            kind: String,
            arg: Value,
            factory: Value,
        ) -> OpResult<bool> {
            EventTarget::emit_with(ctx, &target, &kind, arg, Some(factory))
        }

        /// Make an existing object (a realm global, `performance`) an `EventTarget` without
        /// changing its prototype chain. An object that already is one is left alone.
        #[method(name = "initEventTarget")]
        fn init_event_target(ctx: &mut Ctx, _this: This<Value>, target: Value) -> OpResult<()> {
            if node_target(ctx, &target).is_some() {
                return Ok(());
            }
            ctx.attach_native_data(&target, EventTarget::new())
        }

        #[method(name = "isEventTarget")]
        fn is_event_target(ctx: &mut Ctx, _this: This<Value>, value: Value) -> bool {
            node_target(ctx, &value).is_some()
        }

        #[method(name = "isNodeEventTarget")]
        fn is_node_event_target(ctx: &mut Ctx, _this: This<Value>, value: Value) -> bool {
            is_node_target(ctx, &value)
        }

        #[method(name = "isAbortSignal")]
        fn is_abort_signal(ctx: &mut Ctx, _this: This<Value>, value: Value) -> bool {
            super::super::abort::signal_state(ctx, &value).is_some()
        }

        #[method(name = "isEvent")]
        fn is_event(ctx: &mut Ctx, _this: This<Value>, value: Value) -> bool {
            ctx.with_instance::<Event, _>(&value, |_| ()).is_ok()
        }
    }

    impl NodeEventTarget {
        pub fn data(&self) -> &Rc<TargetData> {
            &self.base.data
        }
    }
}
