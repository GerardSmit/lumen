//! The native DOM event core shared by every runtime: `Event`, `EventTarget`, `CustomEvent`,
//! `ErrorEvent`, `AbortSignal`, `AbortController` and `DOMException`, plus Node's
//! `NodeEventTarget` and `internal/event_target` extensions. Design: `docs/native-events.md`.
//!
//! [`bindings::Module`] publishes the Web classes (install it with [`crate::lazy_globals`]);
//! [`internals::Module`] publishes the hidden `__eventTargetInternals` object Node's glue and
//! the Web glue read. Hosts with a DOM tree attach [`TargetHooks`] to their targets and register
//! a [`RealmPolicy`].

use crate::realm_services::RealmServices;
use crate::webidl::{invalid_arg_type, invalid_this};
use lumen::embed::{Ctx, JsFunction, OpError, OpResult, Value, WeakValue};
use std::rc::Rc;

mod abort;
mod event;
mod node;
mod target;
mod web;

pub use node::internals;
pub use node::{node_handler_get, node_handler_set};
pub use web::bindings;

pub use abort::{abort_signal, dom_exception, new_signal};
pub use bindings::{
    AbortController, AbortSignal, CustomEvent, DomException, ErrorEvent, Event, EventTarget,
};
pub use event::{EventInit, EventState};
pub use internals::NodeEventTarget;
pub use target::{
    report_exception, Callback, ChangeObserver, DeferredCompile, EventPath, HandlerKind, ListenerOptions,
    PathEntry, TargetData, TargetHooks,
};

const WEAK_HANDLER: &str = "nodejs.internal.kWeakHandler";
const RESIST_STOP_PROPAGATION: &str = "nodejs.internal.kResistStopPropagation";
const EVENTS: &str = "lumen.kEvents";
const MAX_LISTENERS: &str = "events.maxEventTargetListeners";
const MAX_LISTENERS_WARNED: &str = "events.maxEventTargetListenersWarned";
const TRANSFERABLE_SIGNAL: &str = "nodejs.abortsignal.transferable";

/// How a realm reports listener exceptions and which errors its dispatch throws. HTML registers
/// one per window; realms without one follow Node.
pub struct RealmPolicy {
    /// Report an exception thrown by a listener.
    pub report: fn(&mut Ctx, Value),
    /// Throw DOM `InvalidStateError`s instead of Node's coded errors.
    pub dom_errors: bool,
}

impl RealmPolicy {
    /// Register `policy` for the current realm.
    pub fn install(ctx: &mut Ctx, policy: RealmPolicy) {
        RealmServices::replace_current(ctx, policy);
    }

    pub(crate) fn current(ctx: &mut Ctx) -> Option<Rc<RealmPolicy>> {
        RealmServices::<RealmPolicy>::current(ctx)
    }
}

/// The per-realm private symbols of `internal/event_target`. They exist once the internals
/// object is created; before that no script can hold them, so the core skips the hooks.
pub(crate) struct EventSymbols {
    pub(crate) trust: Value,
    pub(crate) new_listener: Value,
    pub(crate) remove_listener: Value,
}

impl EventSymbols {
    pub(crate) fn existing(ctx: &mut Ctx) -> Option<Rc<EventSymbols>> {
        RealmServices::<EventSymbols>::current(ctx)
    }

    pub(crate) fn get(ctx: &mut Ctx) -> Rc<EventSymbols> {
        if let Some(symbols) = Self::existing(ctx) {
            return symbols;
        }
        let symbols = EventSymbols {
            trust: ctx.new_symbol(Some("kTrustEvent".into())),
            new_listener: ctx.new_symbol(Some("kNewListener".into())),
            remove_listener: ctx.new_symbol(Some("kRemoveListener".into())),
        };
        RealmServices::replace_current(ctx, symbols)
    }
}

pub(crate) fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Obj(a), Value::Obj(b)) => std::ptr::eq(&**a, &**b),
        _ => false,
    }
}

/// `object[Symbol.for(key)]`.
pub(crate) fn symbol_get(ctx: &mut Ctx, object: &Value, key: &str) -> OpResult<Value> {
    let key = ctx.symbol_for(key);
    ctx.reflect_get(object, &key, object)
        .map_err(OpError::thrown)
}

/// CreateDataProperty(object, key, value) for a symbol or string key.
pub(crate) fn set_data_property(
    ctx: &mut Ctx,
    object: &Value,
    key: Value,
    value: Value,
) -> OpResult<()> {
    let descriptor = ctx.new_object_with_proto(&Value::Null);
    for (name, field) in [
        ("value", value),
        ("writable", Value::Bool(true)),
        ("enumerable", Value::Bool(true)),
        ("configurable", Value::Bool(true)),
    ] {
        ctx.member_set(&descriptor, name, field)
            .map_err(OpError::thrown)?;
    }
    ctx.define_property_value(object, key, &descriptor)
        .map_err(OpError::thrown)
}

/// Node's `inspect.custom` shape: `Name { ...fields }`, or the bare name below depth 0.
pub(crate) fn inspect_object(
    ctx: &mut Ctx,
    this: &Value,
    fields: Vec<(&str, Value)>,
    depth: &Value,
    options: &Value,
    inspect: &Value,
    named: bool,
) -> OpResult<Value> {
    let name = target::constructor_name(ctx, this).unwrap_or_else(|| "Object".into());
    if matches!(depth, Value::Num(depth) if *depth < 0.0) {
        return Ok(if named { Value::str(&name) } else { this.clone() });
    }
    let object_proto = {
        let global = ctx.global_object();
        let object = ctx.member_get(&global, "Object").map_err(OpError::thrown)?;
        ctx.member_get(&object, "prototype").map_err(OpError::thrown)?
    };
    let shown_fields = ctx.new_object_with_proto(&object_proto);
    for (key, value) in fields {
        ctx.member_set(&shown_fields, key, value)
            .map_err(OpError::thrown)?;
    }
    let nested = ctx.new_object_with_proto(&object_proto);
    if matches!(options, Value::Obj(_)) {
        for key in ctx.reflect_own_keys(options).map_err(OpError::thrown)? {
            let value = ctx
                .reflect_get(options, &key, options)
                .map_err(OpError::thrown)?;
            set_data_property(ctx, &nested, key, value)?;
        }
    }
    let nested_depth = match ctx.member_get(options, "depth").map_err(OpError::thrown)? {
        Value::Num(depth) if depth.fract() == 0.0 && depth.is_finite() => Value::Num(depth - 1.0),
        other => other,
    };
    ctx.member_set(&nested, "depth", nested_depth)
        .map_err(OpError::thrown)?;
    let text = ctx
        .invoke(inspect.clone(), Value::Undefined, &[shown_fields, nested])
        .map_err(OpError::thrown)?;
    let text = ctx.coerce_string(&text).map_err(OpError::thrown)?;
    Ok(Value::str(&format!("{name} {text}")))
}
