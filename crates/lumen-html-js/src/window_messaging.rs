//! Window messaging uses the host's single structured-clone/transfer implementation
//! and the HTML owner task queue. Messages belong to a Window realm, not a mutable proxy.
use super::*;
use lumen::embed::{JsHost, Slot, WeakValue};
use lumen::embed::{WindowProxyOperation, WindowProxyResult};
use lumen_bind::{FromArg, Host, Methods};
use lumen_host::messaging::{deserialize_message, serialize_message, MessageEvent};

pub(crate) const CROSS_ORIGIN_SLOT: &str = "#window-cross-origin\u{1}native";

pub(crate) enum PostMessageTarget {
    Origin(String),
    Options { origin: String, transfer: Value },
}

impl Default for PostMessageTarget {
    fn default() -> Self {
        Self::Options { origin: "/".into(), transfer: Value::Undefined }
    }
}

impl<'a> FromArg<'a, JsHost> for PostMessageTarget {
    fn from_arg(cx: &'a lumen::embed::ArgCx<'_>, value: &'a Value, _at: Slot) -> Result<Self, Value> {
        <JsHost as Host>::with_ctx(cx, |ctx| {
            match value {
                Value::Undefined | Value::Null => Ok(Self::default()),
                Value::Obj(_) => {
                    // Inherited StructuredSerializeOptions precedes WindowPostMessageOptions.
                    let transfer = ctx.member_get(value, "transfer")?;
                    let origin = ctx.member_get(value, "targetOrigin")?;
                    let origin = if matches!(origin, Value::Undefined) {
                        "/".to_owned()
                    } else {
                        lumen_host::webidl::usv_string(ctx, &origin)?
                    };
                    Ok(Self::Options { origin, transfer })
                }
                _ => lumen_host::webidl::usv_string(ctx, value).map(Self::Origin),
            }
        })
    }
}

#[derive(Default)]
struct MessageBytes(Rc<Cell<usize>>);

struct MessageLease { bytes: Rc<Cell<usize>>, amount: usize }
impl Drop for MessageLease {
    fn drop(&mut self) { self.bytes.set(self.bytes.get().saturating_sub(self.amount)); }
}

pub(crate) fn post_message(
    ctx: &mut Ctx,
    receiver: &Value,
    message: Value,
    target: PostMessageTarget,
    transfer: Value,
) -> OpResult<()> {
    let receiver = match receiver {
        Value::Undefined | Value::Null => ctx.global_this_value(),
        receiver => receiver.clone(),
    };
    let destination = browsing_context::message_receiver_context(ctx, &receiver)
        .ok_or_else(|| OpError::type_error("Illegal Window receiver"))?;
    let caller = ctx.invocation_host_realm();
    let sender = browsing_context::metadata_for_realm(ctx, &caller)
        .ok_or_else(|| OpError::type_error("Window message has no incumbent document"))?;
    let sender_origin = browsing_context::metadata_origin(&sender);
    let source = browsing_context::window_self_from_metadata(ctx, &sender);
    let (origin, transfer) = match target {
        PostMessageTarget::Origin(origin) => (origin, transfer),
        PostMessageTarget::Options { origin, transfer } => (origin, transfer),
    };
    let target_origin = match origin.as_str() {
        "*" => None,
        "/" => Some(sender_origin.clone()),
        raw => {
            let parsed = lumen_common::url::parse(raw, None).map_err(|_| {
                crate::error_reporting::dom_exception(ctx, "SyntaxError", "Invalid target origin")
            })?;
            Some(browsing_context::Origin::from_url(&parsed.href()))
        }
    };
    // Transfer is synchronous, including for messages that will fail the origin check.
    let message = ctx.with_host_realm(&caller, |ctx| serialize_message(ctx, message, transfer))
        .map_err(|_| OpError::new("InvalidStateError", "Sender Window realm is unavailable"))??;
    if !ctx.op_state().has::<MessageBytes>() { ctx.op_state().put(MessageBytes::default()); }
    let bytes = ctx.op_state().get::<MessageBytes>().expect("message budget installed").0.clone();
    let amount = message.bytes.len();
    const MAX_QUEUED_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
    let total = bytes.get().checked_add(amount).filter(|total| *total <= MAX_QUEUED_MESSAGE_BYTES)
        .ok_or_else(|| OpError::new("QuotaExceededError", "Queued Window messages exceed the byte budget"))?;
    bytes.set(total);
    let lease = MessageLease { bytes, amount };
    let handle = destination.realm_handle();
    let context = Rc::downgrade(&destination);
    let expected = handle.clone();
    let origin = sender_origin.serialize();
    ctx.with_host_realm(&handle, |ctx| scheduling::queue_task(ctx, move |ctx| {
        let _lease = lease;
        let Some(context) = context.upgrade() else { return Ok(()); };
        if !context.is_active_realm(&expected) { return Ok(()); }
        if target_origin.as_ref().is_some_and(|origin| !context.root_or_child_origin().same_origin(origin)) {
            return Ok(());
        }
        let (data, ports) = deserialize_message(ctx, message)?;
        let event = match data {
            Ok(data) => MessageEvent::create(ctx, "message", data, &origin, source, ports)?,
            Err(_) => MessageEvent::create(ctx, "messageerror", Value::Undefined, &origin, source, Vec::new())?,
        };
        let window = context.proxy().ok_or_else(|| OpError::new("InvalidStateError", "Window is unavailable"))?;
        lumen_host::events::EventTarget::dispatch_trusted(ctx, &window, &event)?;
        Ok(())
    })).map_err(|_| OpError::new("InvalidStateError", "Target Window realm is unavailable"))?
}

/// Traced native operations for cross-origin descriptors. Author properties and
/// prototype mutations cannot substitute a function that receives foreign data.
#[lumen_bind::module(name = "__crossOriginWindow")]
mod cross_origin {
    use super::*;
    fn context(ctx: &mut Ctx, receiver: &Value) -> OpResult<Rc<browsing_context::BrowsingContext>> {
        browsing_context::message_receiver_context(ctx, receiver)
            .ok_or_else(|| OpError::new("InvalidStateError", "Window context is unavailable"))
    }
    #[op]
    fn window(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> { context(ctx, &this.0)?.proxy().ok_or_else(|| OpError::type_error("Window is unavailable")) }
    #[op(name = "self")]
    fn self_value(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> { window(ctx, this) }
    #[op]
    fn frames(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> { window(ctx, this) }
    #[op]
    fn parent(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> { let context = context(ctx, &this.0)?; Ok(browsing_context::window_parent_value(ctx, &context)) }
    #[op]
    fn top(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> { let context = context(ctx, &this.0)?; Ok(browsing_context::window_top_value(ctx, &context)) }
    #[op]
    fn length(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<u32> {
        let context = context(ctx, &this.0)?;
        Ok(browsing_context::window_length(&context))
    }
    #[op]
    fn closed(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> bool { browsing_context::message_receiver_context(ctx, &this.0).is_none_or(|context| !context.is_active()) }
    #[op]
    fn opener() -> Value { Value::Null }
    #[op]
    fn close() {
        // This host has document/iframe contexts, with no script-opened auxiliary
        // traversables. Subframes and non-script-closable roots cannot be closed.
    }
    #[op]
    fn focus(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<()> { let context = context(ctx, &this.0)?; browsing_context::focus_window_context(ctx, &context, false) }
    #[op]
    fn blur(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<()> { let context = context(ctx, &this.0)?; browsing_context::focus_window_context(ctx, &context, true) }
    #[op]
    fn location(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        let context = context(ctx, &this.0)?;
        cross_origin_location(ctx, &context)
    }
    #[op(name = "setLocation", coerce)]
    fn set_location(ctx: &mut Ctx, this: lumen_bind::This<Value>, value: &str) -> OpResult<()> {
        let context = context(ctx, &this.0)?;
        let base = window_globals::entry_base_url(ctx, &context);
        context.request_location_navigation_with_caller(ctx, value, &base)
    }
    #[op(name = "replaceLocation", coerce)]
    fn replace_location(ctx: &mut Ctx, this: lumen_bind::This<Value>, value: &str) -> OpResult<()> {
        let context = context(ctx, &this.0)?;
        let base = window_globals::entry_base_url(ctx, &context);
        context.request_location_navigation_with_handling(ctx, value, &base, true)
    }
}

const LOCATION_SLOT: &str = "#window-cross-origin-location\u{1}native";

fn cross_origin_location(ctx: &mut Ctx, context: &Rc<browsing_context::BrowsingContext>) -> OpResult<Value> {
    let methods = cross_origin_methods(ctx, context)?;
    if let Some(location) = ctx.native_private_value_slot(&methods, LOCATION_SLOT) { return Ok(location); }
    let target = ctx.new_object_with_proto(&Value::Null);
    let handler = ctx.new_instance(CrossOriginLocation { context: Rc::downgrade(context) });
    let location = ctx.create_proxy(target, handler).map_err(OpError::thrown)?;
    ctx.define_native_private_value_slot(&location, CROSS_ORIGIN_SLOT, methods.clone()).map_err(OpError::thrown)?;
    ctx.define_native_private_value_slot(&methods, LOCATION_SLOT, location.clone()).map_err(OpError::thrown)?;
    Ok(location)
}

/// A foreign Location exposes href writes and replace, without exposing the
/// target realm's prototype or Function constructor through an ordinary object.
#[lumen_bind::class(name = "__CrossOriginLocation")]
struct CrossOriginLocation { context: std::rc::Weak<browsing_context::BrowsingContext> }

impl CrossOriginLocation {
    fn context(&self, ctx: &mut Ctx) -> OpResult<Rc<browsing_context::BrowsingContext>> {
        self.context.upgrade().filter(|context| context.is_active())
            .ok_or_else(|| crate::error_reporting::dom_exception(ctx, "SecurityError", "Location document is unavailable"))
    }
    fn deny(ctx: &mut Ctx) -> OpError {
        crate::error_reporting::dom_exception(ctx, "SecurityError", "Cross-origin Location access is forbidden")
    }
    fn native_location(ctx: &mut Ctx, context: &Rc<browsing_context::BrowsingContext>) -> OpResult<Value> {
        let document = context.document().ok_or_else(|| OpError::type_error("Location document is unavailable"))?;
        ctx.with_host_realm(&context.realm_handle(), |ctx| window_globals::location_value(ctx, &document, context))
            .map_err(|_| OpError::type_error("Location realm is unavailable"))?
    }
}

#[lumen_bind::methods]
impl CrossOriginLocation {
    #[method(name = "get")]
    fn get(&self, ctx: &mut Ctx, _target: Value, key: Value, _receiver: Value) -> OpResult<Value> {
        let context = self.context(ctx)?;
        if browsing_context::require_same_origin_context(ctx, &context).is_ok() {
            let location = Self::native_location(ctx, &context)?;
            return ctx.reflect_get(&location, &key, &location).map_err(OpError::thrown);
        }
        if matches!(&key, Value::Str(name) if name.as_str() == "replace") {
            let methods = cross_origin_methods(ctx, &context)?;
            return ctx.member_get(&methods, "replaceLocation").map_err(OpError::thrown);
        }
        if fallback_key(ctx, &key) { return Ok(Value::Undefined); }
        Err(Self::deny(ctx))
    }
    #[method(name = "set")]
    fn set(&self, ctx: &mut Ctx, _target: Value, key: Value, value: Value, _receiver: Value) -> OpResult<bool> {
        let context = self.context(ctx)?;
        if matches!(&key, Value::Str(name) if name.as_str() == "href") {
            let methods = cross_origin_methods(ctx, &context)?;
            let setter = ctx.member_get(&methods, "setLocation").map_err(OpError::thrown)?;
            ctx.invoke(setter, Value::Undefined, &[value]).map_err(OpError::thrown)?;
            return Ok(true);
        }
        browsing_context::require_same_origin_context(ctx, &context)?;
        let location = Self::native_location(ctx, &context)?;
        let Value::Str(name) = key else { return Err(Self::deny(ctx)); };
        ctx.member_set(&location, name.as_str(), value).map_err(OpError::thrown)?;
        Ok(true)
    }
    #[method(name = "getOwnPropertyDescriptor")]
    fn get_own_property_descriptor(&self, ctx: &mut Ctx, _target: Value, key: Value) -> OpResult<Value> {
        let context = self.context(ctx)?;
        let methods = cross_origin_methods(ctx, &context)?;
        if matches!(&key, Value::Str(name) if name.as_str() == "href") {
            let result = Value::Obj(ctx.new_object());
            let setter = ctx.member_get(&methods, "setLocation").map_err(OpError::thrown)?;
            for (name, value) in [("get", Value::Undefined), ("set", setter),
                ("enumerable", Value::Bool(false)), ("configurable", Value::Bool(true))] {
                ctx.create_data_property(&result, name, value).map_err(OpError::thrown)?;
            }
            return Ok(result);
        }
        if matches!(&key, Value::Str(name) if name.as_str() == "replace") {
            let result = Value::Obj(ctx.new_object());
            let function = ctx.member_get(&methods, "replaceLocation").map_err(OpError::thrown)?;
            for (name, value) in [("value", function), ("writable", Value::Bool(false)),
                ("enumerable", Value::Bool(false)), ("configurable", Value::Bool(true))] {
                ctx.create_data_property(&result, name, value).map_err(OpError::thrown)?;
            }
            return Ok(result);
        }
        if fallback_key(ctx, &key) { return descriptor(ctx, None, &methods); }
        Err(Self::deny(ctx))
    }
    #[method(name = "has")]
    fn has(&self, ctx: &mut Ctx, _target: Value, key: Value) -> OpResult<bool> {
        if matches!(&key, Value::Str(name) if matches!(name.as_str(), "href" | "replace")) || fallback_key(ctx, &key) {
            return Ok(true);
        }
        Err(Self::deny(ctx))
    }
    #[method(name = "ownKeys")]
    fn own_keys(&self, ctx: &mut Ctx, _target: Value) -> Vec<Value> {
        let mut keys = vec![Value::str("href"), Value::str("replace"), Value::str("then")];
        for name in ["toStringTag", "hasInstance", "isConcatSpreadable"] {
            if let Some(symbol) = ctx.well_known_symbol(name) { keys.push(symbol); }
        }
        keys
    }
    #[method(name = "getPrototypeOf")]
    fn get_prototype_of(&self, _target: Value) -> Value { Value::Null }
    #[method(name = "setPrototypeOf")]
    fn set_prototype_of(&self, _target: Value, prototype: Value) -> bool { matches!(prototype, Value::Null) }
    #[method(name = "isExtensible")]
    fn is_extensible(&self, _target: Value) -> bool { true }
    #[method(name = "preventExtensions")]
    fn prevent_extensions(&self, _target: Value) -> bool { false }
    #[method(name = "deleteProperty")]
    fn delete_property(&self, ctx: &mut Ctx, _target: Value, _key: Value) -> OpResult<bool> { Err(Self::deny(ctx)) }
    #[method(name = "defineProperty")]
    fn define_property(&self, ctx: &mut Ctx, _target: Value, _key: Value, _descriptor: Value) -> OpResult<bool> { Err(Self::deny(ctx)) }
}

pub(crate) fn install(ctx: &mut Ctx) -> OpResult<()> {
    crate::realm_services::RealmServices::replace_current(ctx, CrossOriginCache::default());
    Ok(())
}

#[derive(Default)]
struct CrossOriginCache { targets: RefCell<HashMap<usize, WeakValue>> }

fn cross_origin_methods(ctx: &mut Ctx, context: &Rc<browsing_context::BrowsingContext>) -> OpResult<Value> {
    let target = context.realm_handle().global().object_identity()
        .ok_or_else(|| OpError::type_error("Window realm has no identity"))?;
    let cache = crate::realm_services::RealmServices::<CrossOriginCache>::current(ctx)
        .unwrap_or_else(|| crate::realm_services::RealmServices::replace_current(ctx, CrossOriginCache::default()));
    if let Some(methods) = cache.targets.borrow().get(&target).and_then(WeakValue::upgrade) { return Ok(methods); }
    let methods = ctx.module_object::<cross_origin::Module>().map_err(OpError::thrown)?;
    // The function captures this Window, rather than a proxy that a later
    // navigation retargets to a successor document.
    let receiver = context.realm_handle().global();
    let mut members = Vec::new();
    window_globals::DomWindow::members(&mut members);
    let declaration = members.iter().find(|member| member.desc.name == "post_message")
        .ok_or_else(|| OpError::type_error("Window postMessage declaration is unavailable"))?;
    let post_message = ctx.bound_function(declaration);
    ctx.create_data_property(&methods, "postMessage", post_message).map_err(OpError::thrown)?;
    for name in ACCESSORS.iter().chain(METHODS).copied().chain(["setLocation", "replaceLocation"]) {
        let function = ctx.member_get(&methods, name).map_err(OpError::thrown)?;
        let bound = ctx.bind_function_this(function, receiver.clone()).map_err(OpError::thrown)?;
        // A retained descriptor/function keeps its weak-cache entry alive through
        // ordinary traced edges. Releasing author references collects the cycle.
        ctx.define_native_private_value_slot(&bound, CROSS_ORIGIN_SLOT, methods.clone()).map_err(OpError::thrown)?;
        ctx.create_data_property(&methods, name, bound).map_err(OpError::thrown)?;
    }
    let weak = ctx.weak_value(&methods).ok_or_else(|| OpError::type_error("Window descriptor cache is unavailable"))?;
    let mut targets = cache.targets.borrow_mut();
    targets.retain(|_, value| value.upgrade().is_some());
    targets.insert(target, weak);
    Ok(methods)
}

const ACCESSORS: &[&str] = &["window", "self", "location", "closed", "frames", "length", "top", "opener", "parent"];
const METHODS: &[&str] = &["close", "focus", "blur", "postMessage"];
const CROSS_ORIGIN_KEYS: &[&str] = &["window", "self", "location", "close", "closed", "focus", "blur",
    "frames", "length", "top", "opener", "parent", "postMessage"];

fn fallback_key(ctx: &mut Ctx, key: &Value) -> bool {
    if matches!(key, Value::Str(name) if name.as_str() == "then") { return true; }
    let Value::Sym(symbol) = key else { return false; };
    ["toStringTag", "hasInstance", "isConcatSpreadable"].into_iter().any(|name| {
        matches!(ctx.well_known_symbol(name), Some(Value::Sym(known)) if known.id == symbol.id)
    })
}

fn descriptor(ctx: &mut Ctx, name: Option<&str>, methods: &Value) -> OpResult<Value> {
    let result = Value::Obj(ctx.new_object());
    ctx.create_data_property(&result, "enumerable", Value::Bool(false)).map_err(OpError::thrown)?;
    ctx.create_data_property(&result, "configurable", Value::Bool(true)).map_err(OpError::thrown)?;
    if name.is_some_and(|name| ACCESSORS.contains(&name)) {
        let name = name.expect("accessor name checked");
        let getter = ctx.member_get(methods, name).map_err(OpError::thrown)?;
        let setter = if name == "location" {
            ctx.member_get(methods, "setLocation").map_err(OpError::thrown)?
        } else { Value::Undefined };
        ctx.create_data_property(&result, "get", getter).map_err(OpError::thrown)?;
        ctx.create_data_property(&result, "set", setter).map_err(OpError::thrown)?;
    } else {
        let value = match name {
            Some(name) => ctx.member_get(methods, name).map_err(OpError::thrown)?,
            None => Value::Undefined,
        };
        ctx.create_data_property(&result, "value", value).map_err(OpError::thrown)?;
        ctx.create_data_property(&result, "writable", Value::Bool(false)).map_err(OpError::thrown)?;
    }
    Ok(result)
}

pub(crate) fn cross_origin_operation(
    ctx: &mut Ctx,
    context: &Rc<browsing_context::BrowsingContext>,
    operation: &WindowProxyOperation,
) -> OpResult<WindowProxyResult> {
    // The same child-selection algorithm serves both authorized engine forwarding
    // and this cross-origin membrane, including the insertion-order sort.
    let methods = cross_origin_methods(ctx, context)?;
        let key = match operation {
            WindowProxyOperation::Get { key, .. } | WindowProxyOperation::GetOwnProperty { key }
            | WindowProxyOperation::Has { key } | WindowProxyOperation::Set { key, .. } => Some(key),
            _ => None,
        };
        let name = key.and_then(|key| match key {
            Value::Str(name) if ACCESSORS.contains(&name.as_str()) || METHODS.contains(&name.as_str()) => Some(name.as_str()),
            _ => None,
        });
        let fallback = key.is_some_and(|key| fallback_key(ctx, key));
        let index = key.and_then(|key| match key {
            Value::Str(name) => name.as_str().parse::<u32>().ok()
                .filter(|index| *index != u32::MAX && index.to_string() == name.as_str()),
            _ => None,
        });
        if let Some(index) = index {
            let value = ctx.with_host_realm(&context.realm_handle(), |ctx| context.indexed_child_proxy(ctx, index))
                .map_err(|_| OpError::type_error("Window realm is unavailable"))??
                .ok_or_else(|| crate::error_reporting::dom_exception(ctx, "SecurityError", "Window index is unavailable"))?;
            return match operation {
                WindowProxyOperation::Get { .. } => Ok(WindowProxyResult::Get(value)),
                WindowProxyOperation::Has { .. } => Ok(WindowProxyResult::Has(true)),
                WindowProxyOperation::GetOwnProperty { .. } => {
                    let descriptor = Value::Obj(ctx.new_object());
                    for (key, value) in [("value", value), ("writable", Value::Bool(false)),
                        ("enumerable", Value::Bool(true)), ("configurable", Value::Bool(true))] {
                        ctx.create_data_property(&descriptor, key, value).map_err(OpError::thrown)?;
                    }
                    Ok(WindowProxyResult::GetOwnProperty(Some(descriptor)))
                }
                _ => Err(crate::error_reporting::dom_exception(ctx, "SecurityError", "Window index is readonly")),
            };
        }
        if name.is_none() && !fallback {
            if let Some(Value::Str(key)) = key {
                let child = ctx.with_host_realm(&context.realm_handle(), |ctx| context.named_child_proxy(ctx, key.as_str()))
                    .map_err(|_| OpError::type_error("Window realm is unavailable"))??;
                if let Some(value) = child {
                    return match operation {
                        WindowProxyOperation::Get { .. } => Ok(WindowProxyResult::Get(value)),
                        WindowProxyOperation::Has { .. } => Ok(WindowProxyResult::Has(true)),
                        WindowProxyOperation::GetOwnProperty { .. } => {
                            let descriptor = Value::Obj(ctx.new_object());
                            for (key, value) in [("value", value), ("writable", Value::Bool(false)),
                                ("enumerable", Value::Bool(false)), ("configurable", Value::Bool(true))] {
                                ctx.create_data_property(&descriptor, key, value).map_err(OpError::thrown)?;
                            }
                            Ok(WindowProxyResult::GetOwnProperty(Some(descriptor)))
                        }
                        _ => Err(crate::error_reporting::dom_exception(ctx, "SecurityError", "Named Window access is readonly")),
                    };
                }
            }
        }
        match operation {
            WindowProxyOperation::Get { .. } if name.is_some() || fallback => {
                let value = match name {
                    Some(name) => {
                        let function = ctx.member_get(&methods, name).map_err(OpError::thrown)?;
                        if ACCESSORS.contains(&name) {
                            ctx.invoke(function, Value::Undefined, &[]).map_err(OpError::thrown)?
                        } else { function }
                    }
                    None => Value::Undefined,
                };
                Ok(WindowProxyResult::Get(value))
            }
            WindowProxyOperation::GetOwnProperty { .. } if name.is_some() || fallback => {
                Ok(WindowProxyResult::GetOwnProperty(Some(descriptor(ctx, name, &methods)?)))
            }
            WindowProxyOperation::Has { .. } if name.is_some() || fallback => Ok(WindowProxyResult::Has(true)),
            WindowProxyOperation::Set { value, .. } if name == Some("location") => {
                let setter = ctx.member_get(&methods, "setLocation").map_err(OpError::thrown)?;
                ctx.invoke(setter, Value::Undefined, &[value.clone()]).map_err(OpError::thrown)?;
                Ok(WindowProxyResult::Set(true))
            }
            WindowProxyOperation::OwnPropertyKeys => {
                let mut keys = (0..context.child_count()).map(|index| Value::str(index.to_string())).collect::<Vec<_>>();
                keys.extend(CROSS_ORIGIN_KEYS.iter().map(|name| Value::str(*name)));
                keys.push(Value::str("then"));
                for name in ["toStringTag", "hasInstance", "isConcatSpreadable"] {
                    if let Some(symbol) = ctx.well_known_symbol(name) { keys.push(symbol); }
                }
                Ok(WindowProxyResult::OwnPropertyKeys(keys))
            }
            WindowProxyOperation::GetPrototypeOf => Ok(WindowProxyResult::GetPrototypeOf(Value::Null)),
            WindowProxyOperation::SetPrototypeOf { prototype } => Ok(WindowProxyResult::SetPrototypeOf(matches!(prototype, Value::Null))),
            WindowProxyOperation::IsExtensible => Ok(WindowProxyResult::IsExtensible(true)),
            WindowProxyOperation::PreventExtensions => Ok(WindowProxyResult::PreventExtensions(false)),
            _ => Err(crate::error_reporting::dom_exception(ctx, "SecurityError", "Cross-origin Window access is forbidden")),
        }
}
