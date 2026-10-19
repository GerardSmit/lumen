//! Channel messaging and the web event classes beyond the DOM core, as native classes shared by
//! every runtime. Design: `docs/native-messaging.md`.
//!
//! - [`event_bindings`]: `MessageEvent`, `CloseEvent` and `PromiseRejectionEvent`, subclasses of
//!   the native `Event`.
//! - [`channel_bindings`]: `MessagePort`, `MessageChannel` and `BroadcastChannel` over the
//!   endpoints of [`crate::ports`].
//! - [`shared`]: the hidden `__lumenSharedPorts` object the worker glue uses to wrap an adopted
//!   endpoint, and [`install_port_clone`], the default structured-clone port bridge.
//!
//! [`extension`] installs all of it (the event and channel classes as lazy globals, the
//! `__lumenSharedPorts` namespace and the port bridge); a realm also needs
//! [`crate::ports::extension`], [`crate::clone_transfer::extension`] and an event loop (a runtime's,
//! or [`crate::owner_loop`]). [`listen_native`] delivers an endpoint's messages to a native class
//! instead of a `MessagePort`.

use crate::events::same;
use crate::{lazy_globals, namespace, Extension};
use lumen::embed::{Ctx, JsHost, NativeIdentityOwner, OpError, OpResult, Value};
use lumen_bind::{Class, CtorRet, Host};

mod channel;
mod events;

pub use channel::bindings as channel_bindings;
pub use channel::bindings::{BroadcastChannel, MessageChannel, MessagePort};
pub use channel::shared;
pub use channel::install_port_clone;
pub use channel::{listen_native, NativeReceiver, Receiver};
pub use channel::{deserialize_message, serialize_message};
pub(crate) use channel::new_port;
pub use events::bindings as event_bindings;
pub use events::bindings::{CloseEvent, MessageEvent, PromiseRejectionEvent};

/// Install the messaging classes into a realm: `MessageEvent`, `CloseEvent`,
/// `PromiseRejectionEvent`, `MessagePort`, `MessageChannel` and `BroadcastChannel` (lazy
/// globals), the hidden `__lumenSharedPorts` namespace and the default structured-clone port
/// bridge. The one list `lumen-web` and the Bitnest kernel both use.
pub fn install(ctx: &mut Ctx) -> Result<(), Value> {
    lazy_globals::<event_bindings::Module>(ctx)?;
    lazy_globals::<channel_bindings::Module>(ctx)?;
    namespace::<shared::Module>(ctx)?;
    install_port_clone(ctx)
}

/// [`install`] as an extension, for a host that lists extensions (the kernel).
pub fn extension() -> Extension {
    Extension {
        name: "messaging",
        modules: &[install],
        state_init: None,
        js_init: None,
        js_init_snapshot: None,
        lazy_globals: &[],
    }
}

/// A constructor result for a class that is a native identity owner: the instance becomes the
/// owner, so the values it holds are traced from the wrapper.
pub(crate) struct Owned<T>(pub(crate) T);

impl<T: Class + NativeIdentityOwner> CtorRet<JsHost, T> for Owned<T> {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        let instance = <JsHost as Host>::construct(cx, self.0)?;
        <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
            ctx.set_native_identity_owner::<T>(&instance)
                .map_err(|error| error.to_value(ctx))
        })?;
        Ok(instance)
    }
}

/// A constructor result that also stores values in native private slots, which the collector
/// traces from the wrapper.
pub(crate) struct Slotted<T> {
    pub(crate) value: T,
    pub(crate) slots: Vec<(String, Value)>,
}

impl<T: Class> CtorRet<JsHost, T> for Slotted<T> {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        let instance = <JsHost as Host>::construct(cx, self.value)?;
        <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
            for (slot, value) in self.slots {
                ctx.define_native_private_value_slot(&instance, &slot, value)?;
            }
            Ok::<(), Value>(())
        })?;
        Ok(instance)
    }
}

/// `Object.freeze(array)` of the list: a `FrozenArray`.
pub(crate) fn frozen_array(ctx: &mut Ctx, items: Vec<Value>) -> OpResult<Value> {
    let array = ctx.make_array(items);
    let global = ctx.global_object();
    let object = ctx.member_get(&global, "Object").map_err(OpError::thrown)?;
    let freeze = ctx.member_get(&object, "freeze").map_err(OpError::thrown)?;
    ctx.invoke(freeze, object, std::slice::from_ref(&array))
        .map_err(OpError::thrown)?;
    Ok(array)
}

/// `options[key]`, `undefined` when there is no options object.
pub(crate) fn member(ctx: &mut Ctx, options: &Option<Value>, key: &str) -> OpResult<Value> {
    match options {
        Some(options @ Value::Obj(_)) => ctx.member_get(options, key).map_err(OpError::thrown),
        _ => Ok(Value::Undefined),
    }
}

/// Whether `value` inherits from the global interface `name` (a host that replaced the global,
/// as Node does for `MessagePort`, is honored).
pub(crate) fn inherits_global(ctx: &mut Ctx, value: &Value, name: &str) -> bool {
    let global = ctx.global_object();
    let Ok(constructor) = ctx.member_get(&global, name) else {
        return false;
    };
    if !constructor.is_callable() {
        return false;
    }
    let Ok(wanted) = ctx.member_get(&constructor, "prototype") else {
        return false;
    };
    let mut current = ctx.prototype_of(value);
    for _ in 0..64 {
        match current {
            Value::Obj(_) if same(&current, &wanted) => return true,
            Value::Obj(_) => current = ctx.prototype_of(&current),
            _ => return false,
        }
    }
    false
}

/// A `MessagePort`, whichever class the realm exposes under that name, or a port of the
/// installed structured-clone bridge (Node's).
pub(crate) fn is_port(ctx: &mut Ctx, value: &Value) -> bool {
    if !matches!(value, Value::Obj(_)) {
        return false;
    }
    if ctx.with_instance::<MessagePort, _>(value, |_| ()).is_ok()
        || inherits_global(ctx, value, "MessagePort")
    {
        return true;
    }
    let global = ctx.global_object();
    let Ok(bridge @ Value::Obj(_)) = ctx.member_get(&global, "__lumenPortClone") else {
        return false;
    };
    let Ok(is_port) = ctx.member_get(&bridge, "isPort") else {
        return false;
    };
    is_port.is_callable()
        && ctx
            .invoke(is_port, bridge, std::slice::from_ref(value))
            .is_ok_and(|result| ctx.to_boolean(&result))
}
