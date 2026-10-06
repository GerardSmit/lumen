//! Native, bounded support for the WPT testdriver browser actions.
//!
//! The upstream `testdriver.js` resource remains unchanged. After it installs its public API and
//! replaces `test_driver_internal`, the WPT host calls [`install_after_testdriver_script`] to
//! enable automation and supply the native click/send-keys operations. Other callable commands
//! are observed only when invoked; reading namespace objects or ordinary bookkeeping properties
//! does not mark the case unsupported.

use super::*;
use crate::realm_services::RealmServices;
use lumen::embed::{Deferred, JsFunction, JsHost, Promise, Slot, WeakValue};
use lumen_bind::{FromArg, Host};
use std::time::Instant;
use std::{cell::RefCell, collections::HashMap, rc::Rc, time::Duration};

const UNSUPPORTED_COMMAND: &str = "WPT testdriver command is not implemented by the native host";
const UNSUPPORTED_KEY: &str = "WPT testdriver send_keys contains an unsupported key";
const UNSUPPORTED_CONTEXT: &str =
    "WPT testdriver actions on another browsing context are unsupported";
const UNSUPPORTED_CONSTRUCT: &str =
    "WPT testdriver constructor is not implemented by the native host";
const UNSUPPORTED_DESCRIPTOR: &str = "WPT testdriver immutable descriptor cannot be safely wrapped";
const MAX_WRAPPED_NAMESPACE_VALUES: usize = 256;
const UNSUPPORTED_NAMESPACE_LIMIT: &str = "WPT testdriver namespace cache limit was reached";
const UNSUPPORTED_ACTION_PROFILE: &str =
    "WPT testdriver Actions input profile is not implemented by the native host";
const ACTION_INPUT_BUSY: &str = "WPT testdriver Actions sequence is already active in this realm";
const MAX_ACTION_SOURCES: usize = 8;
const MAX_ACTION_TICKS: usize = 256;
const MAX_ACTION_CELLS: usize = 2048;
const MAX_ACTION_DELAY_MS: u64 = 10_000;
const MAX_ACTION_TOTAL_DELAY_MS: u64 = 60_000;
const MAX_ACTION_MOTION_SAMPLES: usize = 625;
const MAX_ACTION_TOTAL_MOTION_SAMPLES: usize = 4096;
const ACTION_MOTION_FRAME: Duration = Duration::from_millis(16);
const MAX_ACTION_KEY_BYTES: usize = 8;
const MAX_HELD_ACTION_KEYS: usize = 64;

/// Per-test, allocation-free record of the first automation capability the host could not run.
/// It is shared by the top document and its child realms, so a caught rejection cannot turn an
/// unsupported browser command into a passing WPT result.
#[derive(Default)]
pub struct TestDriverCaseState {
    unsupported: std::cell::Cell<Option<&'static str>>,
}

impl TestDriverCaseState {
    pub fn record_unsupported(&self, reason: &'static str) {
        self.unsupported
            .set(self.unsupported.get().or(Some(reason)));
    }

    pub fn unsupported_reason(&self) -> Option<&'static str> {
        self.unsupported.get()
    }
}

struct RealmBridge {
    case: Rc<TestDriverCaseState>,
    internal_root: WeakValue,
    public_root: WeakValue,
    internal_proxy: Option<WeakValue>,
    public_proxy: Option<WeakValue>,
    handler: Option<WeakValue>,
    supported_click: WeakValue,
    supported_send_keys: WeakValue,
    supported_action_sequence: WeakValue,
    supported_actions_constructor: Option<WeakValue>,
    harmless_public_functions: Vec<WeakValue>,
    wrapped: HashMap<usize, WeakValue>,
    action_state: ActionState,
}

#[derive(Default)]
struct ActionState {
    pointers: Vec<ActionPointerState>,
    next_pointer_id: i32,
    active_motion: Option<ActiveTimedAction>,
    held_keys: Vec<String>,
    key_source_id: Option<String>,
    last_click: Option<LastClick>,
    busy: bool,
    plan: Option<ActionPlan>,
}

struct ActionPlan {
    ticks: Vec<ActionTick>,
    pointer_sources: Vec<PlannedPointerSource>,
    pointer_state_indices: [usize; MAX_ACTION_SOURCES],
    key_source_id: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PointerKind {
    Mouse,
    Touch,
    Pen,
}

impl PointerKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Mouse => "mouse",
            Self::Touch => "touch",
            Self::Pen => "pen",
        }
    }
}

struct PlannedPointerSource {
    id: String,
    kind: PointerKind,
}

struct ActionPointerState {
    id: String,
    kind: PointerKind,
    state: forms::TrustedPointerState,
}

#[derive(Clone, Copy)]
struct PointerSample {
    width: f64,
    height: f64,
    pressure: Option<f64>,
    tangential_pressure: f64,
    tilt_x: i32,
    tilt_y: i32,
    twist: i32,
}

impl Default for PointerSample {
    fn default() -> Self {
        Self {
            width: 1.0,
            height: 1.0,
            pressure: None,
            tangential_pressure: 0.0,
            tilt_x: 0,
            tilt_y: 0,
            twist: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct LastClick {
    target: NodeId,
    x: f64,
    y: f64,
    detail: i32,
    time: Instant,
}

struct ActionTick {
    actions: Vec<PlannedAction>,
    delay: Duration,
}

#[derive(Clone, Copy)]
enum ActiveTimedAction {
    Pointer(ActivePointerMotion),
    Wheel(ActiveWheelMotion),
}

#[derive(Clone, Copy)]
struct ActivePointerMotion {
    pointer_state_index: usize,
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    duration: Duration,
    samples: usize,
    next_sample: usize,
    post_delay: Duration,
    properties: PointerSample,
}

#[derive(Clone, Copy)]
struct ActiveWheelMotion {
    x: f64,
    y: f64,
    delta_x: f64,
    delta_y: f64,
    duration: Duration,
    samples: usize,
    next_sample: usize,
    post_delay: Duration,
}

enum PointerOrigin {
    Viewport,
    Pointer,
    Element(NodeId),
}

enum WheelOrigin {
    Viewport,
    Element(NodeId),
}

enum PlannedAction {
    Pause(Duration),
    KeyDown(String),
    KeyUp(String),
    PointerMove {
        pointer_slot: usize,
        x: f64,
        y: f64,
        origin: PointerOrigin,
        duration: Duration,
        properties: PointerSample,
    },
    PointerDown {
        pointer_slot: usize,
        properties: PointerSample,
    },
    PointerUp {
        pointer_slot: usize,
    },
    WheelScroll {
        x: f64,
        y: f64,
        delta_x: f64,
        delta_y: f64,
        origin: WheelOrigin,
        duration: Duration,
    },
}

fn bridge_for_current_realm(ctx: &mut Ctx) -> Option<Rc<RefCell<RealmBridge>>> {
    RealmServices::<RefCell<RealmBridge>>::current(ctx)
}

fn is_root(value: &Value, weak: &WeakValue) -> bool {
    let Some(value_id) = value.object_identity() else {
        return false;
    };
    weak.upgrade().and_then(|root| root.object_identity()) == Some(value_id)
}

fn is_allowed_function(value: &Value, bridge: &RealmBridge) -> bool {
    let Some(identity) = value.object_identity() else {
        return false;
    };
    bridge
        .supported_click
        .upgrade()
        .and_then(|value| value.object_identity())
        == Some(identity)
        || bridge
            .supported_send_keys
            .upgrade()
            .and_then(|value| value.object_identity())
            == Some(identity)
        || bridge
            .supported_action_sequence
            .upgrade()
            .and_then(|value| value.object_identity())
            == Some(identity)
        || bridge
            .supported_actions_constructor
            .as_ref()
            .and_then(WeakValue::upgrade)
            .and_then(|value| value.object_identity())
            == Some(identity)
        || bridge
            .harmless_public_functions
            .iter()
            .any(|weak| weak.upgrade().and_then(|value| value.object_identity()) == Some(identity))
}

fn own_property_value(ctx: &mut Ctx, object: &Value, name: &str) -> OpResult<Option<Value>> {
    let key = Value::str(name);
    let descriptor = ctx
        .reflect_get_own_property_descriptor(object, &key)
        .map_err(OpError::thrown)?;
    if matches!(descriptor, Value::Undefined) {
        return Ok(None);
    }
    ctx.reflect_get(object, &key, object)
        .map(Some)
        .map_err(OpError::thrown)
}

fn wrap_namespace_value(
    ctx: &mut Ctx,
    bridge: &Rc<RefCell<RealmBridge>>,
    value: Value,
) -> OpResult<Value> {
    let Some(identity) = value.object_identity() else {
        return Ok(value);
    };
    if let Some(proxy) = bridge
        .borrow()
        .wrapped
        .get(&identity)
        .and_then(WeakValue::upgrade)
    {
        return Ok(proxy);
    }
    {
        let mut bridge = bridge.borrow_mut();
        if bridge.wrapped.len() >= MAX_WRAPPED_NAMESPACE_VALUES {
            bridge.wrapped.retain(|_, weak| weak.upgrade().is_some());
        }
        if bridge.wrapped.len() >= MAX_WRAPPED_NAMESPACE_VALUES {
            bridge.case.record_unsupported(UNSUPPORTED_NAMESPACE_LIMIT);
            return Ok(value);
        }
    }
    let handler = bridge
        .borrow()
        .handler
        .as_ref()
        .and_then(WeakValue::upgrade)
        .ok_or_else(|| OpError::new("InvalidStateError", "testdriver bridge is not initialized"))?;
    let proxy = ctx.create_proxy(value, handler).map_err(OpError::thrown)?;
    let weak = ctx
        .weak_value(&proxy)
        .ok_or_else(|| OpError::new("Error", "testdriver proxy is not an object"))?;
    bridge.borrow_mut().wrapped.insert(identity, weak);
    Ok(proxy)
}

#[lumen_bind::op(name = "get")]
fn proxy_get(ctx: &mut Ctx, target: Value, key: Value, receiver: Value) -> OpResult<Value> {
    let Some(bridge) = bridge_for_current_realm(ctx) else {
        return ctx
            .reflect_get(&target, &key, &receiver)
            .map_err(OpError::thrown);
    };
    let is_internal = is_root(&target, &bridge.borrow().internal_root);
    if is_internal && matches!(&key, Value::Str(name) if name.as_ref() == "in_automation") {
        return Ok(Value::Bool(true));
    }

    // The upstream namespaces are ordinary own-property objects. Leave inherited Object
    // bookkeeping methods alone, and preserve the already-installed supported callables by
    // identity. All other own object/callable values are proxied lazily; invocation, not access,
    // is what records an unsupported command.
    let descriptor = ctx
        .reflect_get_own_property_descriptor(&target, &key)
        .map_err(OpError::thrown)?;
    let value = ctx
        .reflect_get(&target, &key, &receiver)
        .map_err(OpError::thrown)?;
    if !is_internal
        && is_root(&target, &bridge.borrow().public_root)
        && matches!(&key, Value::Str(name) if name.as_ref() == "Actions")
        && bridge
            .borrow()
            .supported_actions_constructor
            .as_ref()
            .and_then(WeakValue::upgrade)
            .and_then(|constructor| constructor.object_identity())
            == value.object_identity()
    {
        return Ok(value);
    }
    if matches!(descriptor, Value::Undefined) || !value.is_callable() && value.as_obj().is_none() {
        return Ok(value);
    }
    if is_allowed_function(&value, &bridge.borrow()) {
        return Ok(value);
    }
    // A Proxy [[Get]] trap must return the exact value of a non-configurable, non-writable data
    // property. Returning a wrapper would violate that invariant before its apply trap could
    // record the unsupported command, so keep the value and mark this immutable namespace shape
    // as outside the host's safely instrumentable profile.
    let descriptor_value = own_property_value(ctx, &descriptor, "value")?;
    let configurable = own_property_value(ctx, &descriptor, "configurable")?;
    let writable = own_property_value(ctx, &descriptor, "writable")?;
    if descriptor_value
        .as_ref()
        .is_some_and(|property| property.is_callable() || property.as_obj().is_some())
        && matches!(configurable, Some(Value::Bool(false)))
        && matches!(writable, Some(Value::Bool(false)))
    {
        bridge
            .borrow()
            .case
            .record_unsupported(UNSUPPORTED_DESCRIPTOR);
        return Ok(value);
    }
    wrap_namespace_value(ctx, &bridge, value)
}

#[lumen_bind::op(name = "getOwnPropertyDescriptor")]
fn proxy_get_own_property_descriptor(ctx: &mut Ctx, target: Value, key: Value) -> OpResult<Value> {
    let descriptor = ctx
        .reflect_get_own_property_descriptor(&target, &key)
        .map_err(OpError::thrown)?;
    if matches!(descriptor, Value::Undefined) {
        return Ok(descriptor);
    }
    let Some(bridge) = bridge_for_current_realm(ctx) else {
        return Ok(descriptor);
    };
    let configurable =
        own_property_value(ctx, &descriptor, "configurable")?.unwrap_or(Value::Bool(false));
    for field in ["value", "get", "set"] {
        let Some(value) = own_property_value(ctx, &descriptor, field)? else {
            continue;
        };
        if matches!(value, Value::Undefined) || !value.is_callable() && value.as_obj().is_none() {
            continue;
        }
        if field != "value" && !value.is_callable() {
            continue;
        }
        if is_allowed_function(&value, &bridge.borrow()) {
            continue;
        }
        let writable = field == "value"
            && matches!(
                own_property_value(ctx, &descriptor, "writable")?,
                Some(Value::Bool(true))
            );
        if !matches!(&configurable, Value::Bool(true)) && !writable {
            bridge
                .borrow()
                .case
                .record_unsupported(UNSUPPORTED_DESCRIPTOR);
            return Ok(descriptor);
        }
        let wrapped = wrap_namespace_value(ctx, &bridge, value)?;
        ctx.member_set(&descriptor, field, wrapped)
            .map_err(OpError::thrown)?;
    }
    Ok(descriptor)
}

#[lumen_bind::op(name = "apply")]
fn proxy_apply(
    ctx: &mut Ctx,
    _target: Value,
    _this_argument: Value,
    _arguments: Value,
) -> OpResult<Value> {
    let bridge = bridge_for_current_realm(ctx);
    if let Some(bridge) = &bridge {
        bridge.borrow().case.record_unsupported(UNSUPPORTED_COMMAND);
    }
    Err(OpError::new(
        "NotSupportedError",
        "WPT testdriver command is not implemented by the native host",
    ))
}

#[lumen_bind::op(name = "construct")]
fn proxy_construct(
    ctx: &mut Ctx,
    _target: Value,
    _arguments: Value,
    _new_target: Value,
) -> OpResult<Value> {
    if let Some(bridge) = bridge_for_current_realm(ctx) {
        bridge
            .borrow()
            .case
            .record_unsupported(UNSUPPORTED_CONSTRUCT);
    }
    Err(OpError::new(
        "NotSupportedError",
        "WPT testdriver constructors are not implemented by the native host",
    ))
}

fn make_handler(ctx: &mut Ctx) -> OpResult<Value> {
    let handler = Value::Obj(ctx.new_object());
    for (name, function) in [
        (
            "get",
            ctx.bound_function(&lumen_bind::FnItem::of::<proxy_get::Op>()),
        ),
        (
            "apply",
            ctx.bound_function(&lumen_bind::FnItem::of::<proxy_apply::Op>()),
        ),
        (
            "getOwnPropertyDescriptor",
            ctx.bound_function(&lumen_bind::FnItem::of::<
                proxy_get_own_property_descriptor::Op,
            >()),
        ),
        (
            "construct",
            ctx.bound_function(&lumen_bind::FnItem::of::<proxy_construct::Op>()),
        ),
    ] {
        ctx.member_set(&handler, name, function)
            .map_err(OpError::thrown)?;
    }
    Ok(handler)
}

/// Install native WPT automation after the unchanged upstream `testdriver.js` has replaced its
/// internal object. Returns `false` if the resource did not install the expected two API objects.
pub fn install_after_testdriver_script(
    ctx: &mut Ctx,
    case: Rc<TestDriverCaseState>,
) -> OpResult<bool> {
    let global = ctx.global_object();
    let internal = ctx
        .member_get(&global, "test_driver_internal")
        .map_err(OpError::thrown)?;
    let public = ctx
        .member_get(&global, "test_driver")
        .map_err(OpError::thrown)?;
    if internal.as_obj().is_none() || public.as_obj().is_none() {
        return Ok(false);
    }
    if let Some(existing) = bridge_for_current_realm(ctx) {
        let existing = existing.borrow();
        let internal_proxy = existing
            .internal_proxy
            .as_ref()
            .and_then(WeakValue::upgrade);
        let public_proxy = existing.public_proxy.as_ref().and_then(WeakValue::upgrade);
        if Rc::ptr_eq(&existing.case, &case)
            && internal_proxy.as_ref().and_then(Value::object_identity)
                == internal.object_identity()
            && public_proxy.as_ref().and_then(Value::object_identity) == public.object_identity()
        {
            return Ok(true);
        }
    }

    let internal_id = internal.object_identity().expect("checked object");
    let public_id = public.object_identity().expect("checked object");
    ctx.member_set(&internal, "in_automation", Value::Bool(true))
        .map_err(OpError::thrown)?;
    let click = ctx.bound_function(&lumen_bind::FnItem::of::<click::Op>());
    let send_keys = ctx.bound_function(&lumen_bind::FnItem::of::<send_keys::Op>());
    ctx.member_set(&internal, "click", click.clone())
        .map_err(OpError::thrown)?;
    ctx.member_set(&internal, "send_keys", send_keys.clone())
        .map_err(OpError::thrown)?;

    let weak_internal = ctx
        .weak_value(&internal)
        .expect("testdriver internal namespace is an object");
    let weak_public = ctx
        .weak_value(&public)
        .expect("testdriver public namespace is an object");
    let weak_click = ctx
        .weak_value(&click)
        .expect("native click operation is an object");
    let weak_send_keys = ctx
        .weak_value(&send_keys)
        .expect("native send_keys operation is an object");
    let mut harmless_public_functions = Vec::with_capacity(5);
    for name in [
        "click",
        "send_keys",
        "action_sequence",
        "bless",
        "set_test_context",
        "message_test",
    ] {
        let value = ctx.member_get(&public, name).map_err(OpError::thrown)?;
        if value.is_callable() {
            if let Some(value) = ctx.weak_value(&value) {
                harmless_public_functions.push(value);
            }
        }
    }

    let action_sequence = ctx.bound_function(&lumen_bind::FnItem::of::<action_sequence::Op>());
    ctx.member_set(&internal, "action_sequence", action_sequence.clone())
        .map_err(OpError::thrown)?;
    let weak_action_sequence = ctx
        .weak_value(&action_sequence)
        .expect("native action_sequence operation is an object");

    let bridge = RealmServices::<RefCell<RealmBridge>>::replace_current(
        ctx,
        RefCell::new(RealmBridge {
            case,
            internal_root: weak_internal,
            public_root: weak_public,
            internal_proxy: None,
            public_proxy: None,
            handler: None,
            supported_click: weak_click,
            supported_send_keys: weak_send_keys,
            supported_action_sequence: weak_action_sequence,
            supported_actions_constructor: None,
            harmless_public_functions,
            wrapped: HashMap::new(),
            action_state: ActionState::default(),
        }),
    );
    let handler = make_handler(ctx)?;
    let weak_handler = ctx
        .weak_value(&handler)
        .expect("testdriver proxy handler is an object");
    bridge.borrow_mut().handler = Some(weak_handler);

    let internal_proxy = ctx
        .create_proxy(internal, handler.clone())
        .map_err(OpError::thrown)?;
    let public_proxy = ctx.create_proxy(public, handler).map_err(OpError::thrown)?;
    let internal_proxy_weak = ctx
        .weak_value(&internal_proxy)
        .expect("testdriver proxy is an object");
    let public_proxy_weak = ctx
        .weak_value(&public_proxy)
        .expect("testdriver proxy is an object");
    {
        let mut bridge = bridge.borrow_mut();
        bridge
            .wrapped
            .insert(internal_id, internal_proxy_weak.clone());
        bridge.wrapped.insert(public_id, public_proxy_weak.clone());
        bridge.internal_proxy = Some(internal_proxy_weak);
        bridge.public_proxy = Some(public_proxy_weak);
    }
    ctx.member_set(&global, "test_driver_internal", internal_proxy)
        .map_err(OpError::thrown)?;
    ctx.member_set(&global, "test_driver", public_proxy)
        .map_err(OpError::thrown)?;
    Ok(true)
}

/// Capture the exact upstream Actions constructor after the unchanged resource assigns it.
/// `proxy_get` returns only this callable unwrapped; the Actions builder remains upstream code.
pub fn install_after_testdriver_actions_script(ctx: &mut Ctx) -> OpResult<bool> {
    let Some(bridge) = bridge_for_current_realm(ctx) else {
        return Ok(false);
    };
    let public =
        bridge.borrow().public_root.upgrade().ok_or_else(|| {
            OpError::new("InvalidStateError", "testdriver public namespace expired")
        })?;
    let constructor = ctx
        .member_get(&public, "Actions")
        .map_err(OpError::thrown)?;
    if !constructor.is_callable() {
        return Ok(false);
    }
    let Some(constructor) = ctx.weak_value(&constructor) else {
        return Ok(false);
    };
    bridge.borrow_mut().supported_actions_constructor = Some(constructor);
    Ok(true)
}

fn unsupported_action(ctx: &mut Ctx) -> OpError {
    if let Some(bridge) = bridge_for_current_realm(ctx) {
        bridge
            .borrow()
            .case
            .record_unsupported(UNSUPPORTED_ACTION_PROFILE);
    }
    OpError::new("NotSupportedError", UNSUPPORTED_ACTION_PROFILE)
}

fn action_array_length(ctx: &mut Ctx, value: &Value, limit: usize) -> OpResult<usize> {
    if value.as_obj().is_none() {
        return Err(OpError::type_error(
            "testdriver action list must be array-like",
        ));
    }
    let length = ctx.member_get(value, "length").map_err(OpError::thrown)?;
    let length = ctx.coerce_number(&length).map_err(OpError::thrown)?;
    if !length.is_finite() || length < 0.0 || length.fract() != 0.0 {
        return Err(OpError::range_error(
            "invalid testdriver action-list length",
        ));
    }
    if length > limit as f64 {
        return Err(unsupported_action(ctx));
    }
    Ok(length as usize)
}

fn action_item(ctx: &mut Ctx, value: &Value, index: usize) -> OpResult<Value> {
    ctx.member_get(value, &index.to_string())
        .map_err(OpError::thrown)
}

fn action_string(ctx: &mut Ctx, object: &Value, name: &str) -> OpResult<String> {
    let value = ctx.member_get(object, name).map_err(OpError::thrown)?;
    let value = ctx.coerce_string(&value).map_err(OpError::thrown)?;
    Ok(value.to_string())
}

fn action_number(ctx: &mut Ctx, object: &Value, name: &str) -> OpResult<f64> {
    let value = ctx.member_get(object, name).map_err(OpError::thrown)?;
    ctx.coerce_number(&value).map_err(OpError::thrown)
}

fn action_optional_number(
    ctx: &mut Ctx,
    object: &Value,
    name: &str,
    default: f64,
) -> OpResult<f64> {
    let value = ctx.member_get(object, name).map_err(OpError::thrown)?;
    if matches!(value, Value::Undefined) {
        return Ok(default);
    }
    ctx.coerce_number(&value).map_err(OpError::thrown)
}

fn action_duration(ctx: &mut Ctx, object: &Value, default_ms: f64) -> OpResult<Duration> {
    let millis = action_optional_number(ctx, object, "duration", default_ms)?;
    if !millis.is_finite() || millis < 0.0 {
        return Err(OpError::range_error("invalid testdriver action duration"));
    }
    if millis > MAX_ACTION_DELAY_MS as f64 {
        return Err(unsupported_action(ctx));
    }
    Duration::try_from_secs_f64(millis / 1000.0)
        .map_err(|_| OpError::range_error("invalid testdriver action duration"))
}

fn motion_sample_count(duration: Duration) -> usize {
    let frame_ns = ACTION_MOTION_FRAME.as_nanos();
    duration
        .as_nanos()
        .saturating_add(frame_ns - 1)
        .checked_div(frame_ns)
        .unwrap_or(1)
        .clamp(1, MAX_ACTION_MOTION_SAMPLES as u128) as usize
}

fn motion_deadline(duration: Duration, samples: usize, sample: usize) -> Duration {
    let nanos = duration.as_nanos().saturating_mul(sample as u128) / samples as u128;
    Duration::from_nanos(nanos.min(u64::MAX as u128) as u64)
}

fn next_motion_delay(motion: ActiveTimedAction) -> Duration {
    let (duration, samples, next_sample) = match motion {
        ActiveTimedAction::Pointer(motion) => (motion.duration, motion.samples, motion.next_sample),
        ActiveTimedAction::Wheel(motion) => (motion.duration, motion.samples, motion.next_sample),
    };
    let previous = motion_deadline(duration, samples, next_sample.saturating_sub(1));
    let next = motion_deadline(duration, samples, next_sample);
    next.saturating_sub(previous).max(Duration::from_nanos(1))
}

fn parse_pointer_kind(ctx: &mut Ctx, source: &Value) -> OpResult<PointerKind> {
    let parameters = ctx
        .member_get(source, "parameters")
        .map_err(OpError::thrown)?;
    if matches!(parameters, Value::Undefined | Value::Null) {
        return Ok(PointerKind::Mouse);
    }
    if !matches!(parameters, Value::Obj(_)) {
        return Err(unsupported_action(ctx));
    }
    let pointer_type = ctx
        .member_get(&parameters, "pointerType")
        .map_err(OpError::thrown)?;
    if matches!(pointer_type, Value::Undefined) {
        return Ok(PointerKind::Mouse);
    }
    let pointer_type = ctx
        .coerce_string(&pointer_type)
        .map_err(OpError::thrown)?
        .to_string();
    match pointer_type.as_str() {
        "mouse" => Ok(PointerKind::Mouse),
        "touch" => Ok(PointerKind::Touch),
        "pen" => Ok(PointerKind::Pen),
        _ => Err(unsupported_action(ctx)),
    }
}

fn parse_pointer_sample(ctx: &mut Ctx, action: &Value) -> OpResult<PointerSample> {
    unsupported_action_properties(ctx, action, &["altitudeAngle", "azimuthAngle"])?;
    let mut sample = PointerSample::default();
    sample.width = action_optional_number(ctx, action, "width", 1.0)?;
    sample.height = action_optional_number(ctx, action, "height", 1.0)?;
    let pressure = ctx
        .member_get(action, "pressure")
        .map_err(OpError::thrown)?;
    sample.pressure = if matches!(pressure, Value::Undefined) {
        None
    } else {
        Some(ctx.coerce_number(&pressure).map_err(OpError::thrown)?)
    };
    sample.tangential_pressure = action_optional_number(ctx, action, "tangentialPressure", 0.0)?;
    let tilt_x = action_optional_number(ctx, action, "tiltX", 0.0)?;
    let tilt_y = action_optional_number(ctx, action, "tiltY", 0.0)?;
    let twist = action_optional_number(ctx, action, "twist", 0.0)?;
    if !sample.width.is_finite()
        || !sample.height.is_finite()
        || sample.width < 0.0
        || sample.height < 0.0
        || sample
            .pressure
            .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        || !sample.tangential_pressure.is_finite()
        || !(-1.0..=1.0).contains(&sample.tangential_pressure)
        || !tilt_x.is_finite()
        || !tilt_y.is_finite()
        || !twist.is_finite()
        || !(-90.0..=90.0).contains(&tilt_x)
        || !(-90.0..=90.0).contains(&tilt_y)
        || !(0.0..=359.0).contains(&twist)
    {
        return Err(OpError::range_error("invalid pointer contact properties"));
    }
    sample.tilt_x = tilt_x as i32;
    sample.tilt_y = tilt_y as i32;
    sample.twist = twist as i32;
    Ok(sample)
}

fn unsupported_action_properties(ctx: &mut Ctx, action: &Value, fields: &[&str]) -> OpResult<()> {
    for field in fields {
        let value = ctx.member_get(action, field).map_err(OpError::thrown)?;
        if !matches!(value, Value::Undefined) {
            return Err(unsupported_action(ctx));
        }
    }
    Ok(())
}

fn parse_action_sequence(ctx: &mut Ctx, value: &Value) -> OpResult<ActionPlan> {
    let source_count = action_array_length(ctx, value, MAX_ACTION_SOURCES)?;
    let mut ticks: Vec<ActionTick> = Vec::new();
    let mut cells = 0usize;
    let mut key_sources = 0usize;
    let mut wheel_sources = 0usize;
    let mut total_motion_samples = 0usize;
    let mut pointer_sources = Vec::new();
    let mut key_source_id = None;
    let mut source_ids = Vec::new();
    source_ids
        .try_reserve_exact(source_count)
        .map_err(|_| OpError::range_error("testdriver action allocation failed"))?;
    pointer_sources
        .try_reserve_exact(source_count)
        .map_err(|_| OpError::range_error("testdriver action allocation failed"))?;

    for source_index in 0..source_count {
        let source = action_item(ctx, value, source_index)?;
        let source_type = action_string(ctx, &source, "type")?;
        let source_id = ctx.member_get(&source, "id").map_err(OpError::thrown)?;
        let Value::Str(source_id) = source_id else {
            return Err(OpError::type_error(
                "testdriver action source id must be a string",
            ));
        };
        let source_id = source_id.to_string();
        if source_id.len() > 256 {
            return Err(unsupported_action(ctx));
        }
        if source_ids.iter().any(|seen| seen == &source_id) {
            return Err(OpError::type_error("duplicate testdriver action source id"));
        }
        source_ids.push(source_id.clone());
        let pointer_slot = if source_type == "pointer" {
            let kind = parse_pointer_kind(ctx, &source)?;
            if kind == PointerKind::Mouse
                && pointer_sources
                    .iter()
                    .any(|source: &PlannedPointerSource| source.kind == PointerKind::Mouse)
            {
                return Err(unsupported_action(ctx));
            }
            let slot = pointer_sources.len();
            pointer_sources.push(PlannedPointerSource {
                id: source_id.clone(),
                kind,
            });
            Some(slot)
        } else {
            None
        };
        match source_type.as_str() {
            "key" => {
                key_sources += 1;
                if key_sources > 1 {
                    return Err(unsupported_action(ctx));
                }
                key_source_id = Some(source_id);
            }
            "pointer" => {
                if pointer_slot.is_none() {
                    return Err(unsupported_action(ctx));
                }
            }
            "wheel" => {
                wheel_sources += 1;
                if wheel_sources > 1 {
                    return Err(unsupported_action(ctx));
                }
                let parameters = ctx
                    .member_get(&source, "parameters")
                    .map_err(OpError::thrown)?;
                if !matches!(parameters, Value::Undefined | Value::Null | Value::Obj(_)) {
                    return Err(unsupported_action(ctx));
                }
            }
            "none" => {}
            _ => return Err(unsupported_action(ctx)),
        }

        let actions = ctx
            .member_get(&source, "actions")
            .map_err(OpError::thrown)?;
        let length = action_array_length(ctx, &actions, MAX_ACTION_TICKS)?;
        cells = cells
            .checked_add(length)
            .filter(|cells| *cells <= MAX_ACTION_CELLS)
            .ok_or_else(|| unsupported_action(ctx))?;
        if length > ticks.len() {
            ticks
                .try_reserve_exact(length - ticks.len())
                .map_err(|_| OpError::range_error("testdriver action allocation failed"))?;
            ticks.resize_with(length, || ActionTick {
                actions: Vec::new(),
                delay: Duration::ZERO,
            });
        }
        for index in 0..length {
            let action = action_item(ctx, &actions, index)?;
            let kind = action_string(ctx, &action, "type")?;
            let parsed_action = match (source_type.as_str(), kind.as_str()) {
                ("none", "pause")
                | ("key", "pause")
                | ("pointer", "pause")
                | ("wheel", "pause") => PlannedAction::Pause(action_duration(ctx, &action, 0.0)?),
                ("key", "keyDown") | ("key", "keyUp") => {
                    let key = action_string(ctx, &action, "value")?;
                    if key.len() > MAX_ACTION_KEY_BYTES {
                        return Err(unsupported_action(ctx));
                    }
                    super::keyboard_automation::validate_action_key(&key)
                        .map_err(|_| unsupported_action(ctx))?;
                    if kind == "keyDown" {
                        PlannedAction::KeyDown(key)
                    } else {
                        PlannedAction::KeyUp(key)
                    }
                }
                ("pointer", "pointerMove") => {
                    let properties = parse_pointer_sample(ctx, &action)?;
                    let duration = action_duration(ctx, &action, 0.0)?;
                    if !duration.is_zero() {
                        let samples = motion_sample_count(duration);
                        total_motion_samples = total_motion_samples
                            .checked_add(samples)
                            .filter(|total| *total <= MAX_ACTION_TOTAL_MOTION_SAMPLES)
                            .ok_or_else(|| unsupported_action(ctx))?;
                    }
                    let x = action_number(ctx, &action, "x")?;
                    let y = action_number(ctx, &action, "y")?;
                    if !x.is_finite() || !y.is_finite() {
                        return Err(OpError::type_error("pointer coordinates must be finite"));
                    }
                    let origin = ctx.member_get(&action, "origin").map_err(OpError::thrown)?;
                    let origin = if matches!(origin, Value::Undefined)
                        || matches!(&origin, Value::Str(name) if name.as_ref() == "viewport")
                    {
                        PointerOrigin::Viewport
                    } else if matches!(&origin, Value::Str(name) if name.as_ref() == "pointer") {
                        PointerOrigin::Pointer
                    } else {
                        let (target_realm, node) = element_data(ctx, &origin)?;
                        let current_realm =
                            window_globals::current_dom_realm(ctx).ok_or_else(|| {
                                OpError::new("InvalidStateError", "testdriver has no document")
                            })?;
                        if !Rc::ptr_eq(&current_realm, &target_realm) {
                            return Err(unsupported_action(ctx));
                        }
                        PointerOrigin::Element(node)
                    };
                    PlannedAction::PointerMove {
                        pointer_slot: pointer_slot.expect("pointer source has a slot"),
                        x,
                        y,
                        origin,
                        duration,
                        properties,
                    }
                }
                ("pointer", "pointerDown") | ("pointer", "pointerUp") => {
                    let button = action_optional_number(ctx, &action, "button", 0.0)?;
                    if button != 0.0 {
                        return Err(unsupported_action(ctx));
                    }
                    if kind == "pointerDown" {
                        PlannedAction::PointerDown {
                            pointer_slot: pointer_slot.expect("pointer source has a slot"),
                            properties: parse_pointer_sample(ctx, &action)?,
                        }
                    } else {
                        PlannedAction::PointerUp {
                            pointer_slot: pointer_slot.expect("pointer source has a slot"),
                        }
                    }
                }
                ("wheel", "scroll") => {
                    unsupported_action_properties(
                        ctx,
                        &action,
                        &[
                            "button",
                            "width",
                            "height",
                            "pressure",
                            "tangentialPressure",
                            "tiltX",
                            "tiltY",
                            "twist",
                            "altitudeAngle",
                            "azimuthAngle",
                        ],
                    )?;
                    let duration = action_duration(ctx, &action, 0.0)?;
                    if !duration.is_zero() {
                        let samples = motion_sample_count(duration);
                        total_motion_samples = total_motion_samples
                            .checked_add(samples)
                            .filter(|total| *total <= MAX_ACTION_TOTAL_MOTION_SAMPLES)
                            .ok_or_else(|| unsupported_action(ctx))?;
                    }
                    let x = action_optional_number(ctx, &action, "x", 0.0)?;
                    let y = action_optional_number(ctx, &action, "y", 0.0)?;
                    let delta_x = action_optional_number(ctx, &action, "deltaX", 0.0)?;
                    let delta_y = action_optional_number(ctx, &action, "deltaY", 0.0)?;
                    if ![x, y, delta_x, delta_y].into_iter().all(f64::is_finite) {
                        return Err(OpError::type_error(
                            "wheel coordinates and deltas must be finite",
                        ));
                    }
                    let origin = ctx.member_get(&action, "origin").map_err(OpError::thrown)?;
                    let origin = if matches!(origin, Value::Undefined)
                        || matches!(&origin, Value::Str(name) if name.as_ref() == "viewport")
                    {
                        WheelOrigin::Viewport
                    } else if matches!(&origin, Value::Str(name) if name.as_ref() == "pointer") {
                        return Err(unsupported_action(ctx));
                    } else {
                        let (target_realm, node) = element_data(ctx, &origin)?;
                        let current_realm =
                            window_globals::current_dom_realm(ctx).ok_or_else(|| {
                                OpError::new("InvalidStateError", "testdriver has no document")
                            })?;
                        if !Rc::ptr_eq(&current_realm, &target_realm) {
                            return Err(unsupported_action(ctx));
                        }
                        WheelOrigin::Element(node)
                    };
                    PlannedAction::WheelScroll {
                        x,
                        y,
                        delta_x,
                        delta_y,
                        origin,
                        duration,
                    }
                }
                _ => return Err(unsupported_action(ctx)),
            };
            match &parsed_action {
                PlannedAction::Pause(duration) => {
                    ticks[index].delay = ticks[index].delay.max(*duration);
                }
                PlannedAction::PointerMove { duration, .. } => {
                    ticks[index].delay = ticks[index].delay.max(*duration);
                }
                PlannedAction::WheelScroll { duration, .. } => {
                    ticks[index].delay = ticks[index].delay.max(*duration);
                }
                _ => {}
            }
            ticks[index]
                .actions
                .try_reserve(1)
                .map_err(|_| OpError::range_error("testdriver action allocation failed"))?;
            ticks[index].actions.push(parsed_action);
        }
    }

    let mut total_delay = Duration::ZERO;
    for tick in &ticks {
        let mut timed_actions = 0usize;
        let mut timed_action_non_pause_count = 0usize;
        for action in &tick.actions {
            if matches!(action, PlannedAction::PointerMove { duration, .. } | PlannedAction::WheelScroll { duration, .. } if !duration.is_zero())
            {
                timed_actions += 1;
            }
            if !matches!(action, PlannedAction::Pause(_)) {
                timed_action_non_pause_count += 1;
            }
        }
        if timed_actions > 1 || (timed_actions == 1 && timed_action_non_pause_count != 1) {
            // The current scheduler interpolates one pointer or wheel trajectory at a time;
            // other sources can synchronize through pause-only actions.
            return Err(unsupported_action(ctx));
        }
        total_delay = total_delay
            .checked_add(tick.delay)
            .ok_or_else(|| OpError::range_error("testdriver action duration overflow"))?;
        if total_delay > Duration::from_millis(MAX_ACTION_TOTAL_DELAY_MS) {
            return Err(unsupported_action(ctx));
        }
    }
    Ok(ActionPlan {
        ticks,
        pointer_sources,
        pointer_state_indices: [usize::MAX; MAX_ACTION_SOURCES],
        key_source_id,
    })
}

fn prepare_pointer_sources(
    bridge: &Rc<RefCell<RealmBridge>>,
    plan: &mut ActionPlan,
) -> OpResult<()> {
    let mut bridge = bridge.borrow_mut();
    let case = bridge.case.clone();
    let state = &mut bridge.action_state;
    let mut missing = 0usize;
    for source in &plan.pointer_sources {
        if let Some(existing) = state
            .pointers
            .iter()
            .find(|pointer| pointer.id == source.id)
        {
            if existing.kind != source.kind {
                case.record_unsupported(UNSUPPORTED_ACTION_PROFILE);
                return Err(OpError::new(
                    "NotSupportedError",
                    "a pointer source cannot change type after registration",
                ));
            }
        } else {
            missing += 1;
        }
    }
    if state.pointers.len().saturating_add(missing) > MAX_ACTION_SOURCES {
        case.record_unsupported(UNSUPPORTED_ACTION_PROFILE);
        return Err(OpError::new(
            "NotSupportedError",
            "the realm pointer source limit was reached",
        ));
    }
    state
        .pointers
        .try_reserve(missing)
        .map_err(|_| OpError::range_error("pointer source allocation failed"))?;
    for (slot, source) in plan.pointer_sources.iter().enumerate() {
        let index = if let Some(index) = state
            .pointers
            .iter()
            .position(|pointer| pointer.id == source.id)
        {
            index
        } else {
            let mut pointer = forms::TrustedPointerState::default();
            let pointer_id = if source.kind == PointerKind::Mouse {
                1
            } else {
                state.next_pointer_id = state.next_pointer_id.max(2);
                let id = state.next_pointer_id;
                state.next_pointer_id = state.next_pointer_id.saturating_add(1);
                id
            };
            pointer.properties.pointer_id = pointer_id;
            pointer.properties.pointer_type = source.kind.as_str();
            let index = state.pointers.len();
            state.pointers.push(ActionPointerState {
                id: source.id.clone(),
                kind: source.kind,
                state: pointer,
            });
            index
        };
        plan.pointer_state_indices[slot] = index;
    }
    Ok(())
}

fn schedule_action_step(
    ctx: &mut Ctx,
    index: usize,
    resolve: &Value,
    reject: &Value,
    delay: Duration,
) -> OpResult<()> {
    let callback = ctx.bound_function(&lumen_bind::FnItem::of::<action_step::Op>());
    let args = [Value::Num(index as f64), resolve.clone(), reject.clone()];
    lumen_timers::schedule_host_callback(ctx, callback, &args, delay)?;
    Ok(())
}

fn schedule_action_motion(
    ctx: &mut Ctx,
    index: usize,
    resolve: &Value,
    reject: &Value,
    delay: Duration,
) -> OpResult<()> {
    let callback = ctx.bound_function(&lumen_bind::FnItem::of::<action_motion_step::Op>());
    let args = [Value::Num(index as f64), resolve.clone(), reject.clone()];
    lumen_timers::schedule_host_callback(ctx, callback, &args, delay)?;
    Ok(())
}

#[lumen_bind::op(name = "action_sequence")]
fn action_sequence(
    ctx: &mut Ctx,
    actions: Value,
    #[default(Value::Null)] context: Value,
) -> Promise<Value> {
    let Some(bridge) = bridge_for_current_realm(ctx) else {
        return Promise::rejected(OpError::new(
            "InvalidStateError",
            "testdriver automation is not installed in this realm",
        ));
    };
    if !matches!(context, Value::Null | Value::Undefined) {
        bridge.borrow().case.record_unsupported(UNSUPPORTED_CONTEXT);
        return Promise::rejected(OpError::new(
            "NotSupportedError",
            "testdriver Actions context must be the current browsing context",
        ));
    }
    if bridge.borrow().action_state.busy {
        bridge.borrow().case.record_unsupported(ACTION_INPUT_BUSY);
        return Promise::rejected(OpError::new("InvalidStateError", ACTION_INPUT_BUSY));
    }
    let mut plan = match parse_action_sequence(ctx, &actions) {
        Ok(plan) => plan,
        Err(error) => return Promise::rejected(error),
    };
    if let Err(error) = validate_action_state(&bridge, &plan) {
        return Promise::rejected(error);
    }
    if plan.ticks.is_empty() {
        return Promise::resolved(Value::Undefined);
    }
    if let Err(error) = prepare_pointer_sources(&bridge, &mut plan) {
        return Promise::rejected(error);
    }
    if bridge.borrow().action_state.busy {
        bridge.borrow().case.record_unsupported(ACTION_INPUT_BUSY);
        return Promise::rejected(OpError::new("InvalidStateError", ACTION_INPUT_BUSY));
    }
    {
        let mut bridge = bridge.borrow_mut();
        let state = &mut bridge.action_state;
        if let Some(next_id) = &plan.key_source_id {
            if state.key_source_id.as_ref() != Some(next_id) {
                state.key_source_id = Some(next_id.clone());
                state.held_keys.clear();
            }
        }
        state.active_motion = None;
        state.plan = Some(plan);
        state.busy = true;
    }

    let deferred = Deferred::new(ctx);
    let (resolve, reject) = deferred.resolving_functions(ctx);
    if let Err(error) = schedule_action_step(ctx, 0, &resolve, &reject, Duration::ZERO) {
        let mut state = bridge.borrow_mut();
        state.action_state.plan = None;
        state.action_state.busy = false;
        return Promise::rejected(error);
    }
    Promise::pending(&deferred)
}

fn validate_action_state(bridge: &Rc<RefCell<RealmBridge>>, plan: &ActionPlan) -> OpResult<()> {
    let (mut keys, current_key_id, case) = {
        let bridge = bridge.borrow();
        (
            bridge.action_state.held_keys.clone(),
            bridge.action_state.key_source_id.clone(),
            bridge.case.clone(),
        )
    };
    if plan
        .key_source_id
        .as_ref()
        .is_some_and(|next| current_key_id.as_ref().is_some_and(|old| old != next))
        && !keys.is_empty()
    {
        case.record_unsupported(UNSUPPORTED_ACTION_PROFILE);
        return Err(OpError::new(
            "NotSupportedError",
            "changing a key source with held keys is unsupported",
        ));
    }
    if plan
        .key_source_id
        .as_ref()
        .is_some_and(|next| current_key_id.as_ref().is_some_and(|old| old != next))
    {
        keys.clear();
    }
    for tick in &plan.ticks {
        for action in &tick.actions {
            match action {
                PlannedAction::KeyDown(key) if !keys.iter().any(|held| held == key) => {
                    if keys.len() >= MAX_HELD_ACTION_KEYS {
                        case.record_unsupported(UNSUPPORTED_ACTION_PROFILE);
                        return Err(OpError::new(
                            "NotSupportedError",
                            "too many concurrently held WebDriver keys",
                        ));
                    }
                    keys.push(key.clone());
                }
                PlannedAction::KeyUp(key) => keys.retain(|held| held != key),
                _ => {}
            }
        }
    }
    Ok(())
}

#[lumen_bind::op(name = "action_step")]
fn action_step(ctx: &mut Ctx, index: usize, resolve: Value, reject: Value) -> OpResult<()> {
    let Some(bridge) = bridge_for_current_realm(ctx) else {
        let reason =
            OpError::new("InvalidStateError", "action sequence realm expired").to_value(ctx);
        settle_action_callback(ctx, &reject, reason)?;
        return Ok(());
    };
    enum Step {
        Tick(ActionTick, [usize; MAX_ACTION_SOURCES]),
        Finished,
        Missing,
    }
    let step = {
        let mut bridge = bridge.borrow_mut();
        let state = &mut bridge.action_state;
        match state.plan.as_ref().map(|plan| plan.ticks.len()) {
            None => {
                state.busy = false;
                Step::Missing
            }
            Some(length) if index >= length => {
                state.plan = None;
                state.busy = false;
                Step::Finished
            }
            Some(_) => {
                let plan = state.plan.as_mut().expect("plan length was present");
                let pointer_state_indices = plan.pointer_state_indices;
                let tick = &mut plan.ticks[index];
                Step::Tick(
                    ActionTick {
                        actions: std::mem::take(&mut tick.actions),
                        delay: tick.delay,
                    },
                    pointer_state_indices,
                )
            }
        }
    };
    let (tick, pointer_state_indices) = match step {
        Step::Tick(tick, pointer_state_indices) => (tick, pointer_state_indices),
        Step::Finished | Step::Missing => {
            settle_action_callback(ctx, &resolve, Value::Undefined)?;
            return Ok(());
        }
    };
    let delay = tick.delay;
    let result = run_action_tick(ctx, &bridge, tick, &pointer_state_indices);
    if let Err(error) = result {
        let mut state = bridge.borrow_mut();
        state.action_state.plan = None;
        state.action_state.busy = false;
        state.action_state.active_motion = None;
        drop(state);
        let reason = error.to_value(ctx);
        settle_action_callback(ctx, &reject, reason)?;
        return Ok(());
    }
    let motion = bridge.borrow().action_state.active_motion;
    let scheduled = if let Some(motion) = motion {
        schedule_action_motion(ctx, index, &resolve, &reject, next_motion_delay(motion))
    } else {
        schedule_action_step(ctx, index + 1, &resolve, &reject, delay)
    };
    if let Err(error) = scheduled {
        let mut state = bridge.borrow_mut();
        state.action_state.plan = None;
        state.action_state.busy = false;
        state.action_state.active_motion = None;
        drop(state);
        let reason = error.to_value(ctx);
        settle_action_callback(ctx, &reject, reason)?;
    }
    Ok(())
}

#[lumen_bind::op(name = "action_motion_step")]
fn action_motion_step(ctx: &mut Ctx, index: usize, resolve: Value, reject: Value) -> OpResult<()> {
    let Some(bridge) = bridge_for_current_realm(ctx) else {
        let reason =
            OpError::new("InvalidStateError", "action sequence realm expired").to_value(ctx);
        settle_action_callback(ctx, &reject, reason)?;
        return Ok(());
    };
    let Some(motion) = bridge.borrow().action_state.active_motion else {
        return schedule_action_step(ctx, index + 1, &resolve, &reject, Duration::ZERO);
    };
    let (final_sample, post_delay, next_motion, result) =
        if let Some(realm) = window_globals::current_dom_realm(ctx) {
            let modifiers = {
                let bridge_state = bridge.borrow();
                pointer_modifiers(&bridge_state.action_state.held_keys)
            };
            match motion {
                ActiveTimedAction::Pointer(mut pointer) => {
                    let fraction = pointer.next_sample as f64 / pointer.samples as f64;
                    let final_sample = pointer.next_sample >= pointer.samples;
                    let x = if final_sample {
                        pointer.to_x
                    } else {
                        pointer.from_x + (pointer.to_x - pointer.from_x) * fraction
                    };
                    let y = if final_sample {
                        pointer.to_y
                    } else {
                        pointer.from_y + (pointer.to_y - pointer.from_y) * fraction
                    };
                    let result = perform_pointer_move(
                        ctx,
                        &realm,
                        &bridge,
                        pointer.pointer_state_index,
                        x,
                        y,
                        modifiers,
                        pointer.properties,
                    );
                    if !final_sample {
                        pointer.next_sample += 1;
                    }
                    (
                        final_sample,
                        pointer.post_delay,
                        ActiveTimedAction::Pointer(pointer),
                        result,
                    )
                }
                ActiveTimedAction::Wheel(mut wheel) => {
                    let final_sample = wheel.next_sample >= wheel.samples;
                    let fraction = 1.0 / wheel.samples as f64;
                    let delta_x = if final_sample {
                        wheel.delta_x - fraction * wheel.delta_x * (wheel.samples - 1) as f64
                    } else {
                        wheel.delta_x * fraction
                    };
                    let delta_y = if final_sample {
                        wheel.delta_y - fraction * wheel.delta_y * (wheel.samples - 1) as f64
                    } else {
                        wheel.delta_y * fraction
                    };
                    let result = perform_wheel_scroll(
                        ctx, &realm, wheel.x, wheel.y, delta_x, delta_y, modifiers,
                    );
                    if !final_sample {
                        wheel.next_sample += 1;
                    }
                    (
                        final_sample,
                        wheel.post_delay,
                        ActiveTimedAction::Wheel(wheel),
                        result,
                    )
                }
            }
        } else {
            (
                true,
                Duration::ZERO,
                motion,
                Err(OpError::new(
                    "InvalidStateError",
                    "testdriver has no document",
                )),
            )
        };
    if let Err(error) = result {
        let mut state = bridge.borrow_mut();
        state.action_state.plan = None;
        state.action_state.busy = false;
        state.action_state.active_motion = None;
        drop(state);
        let reason = error.to_value(ctx);
        settle_action_callback(ctx, &reject, reason)?;
        return Ok(());
    }
    if final_sample {
        bridge.borrow_mut().action_state.active_motion = None;
        if let Err(error) = schedule_action_step(ctx, index + 1, &resolve, &reject, post_delay) {
            let mut state = bridge.borrow_mut();
            state.action_state.plan = None;
            state.action_state.busy = false;
            state.action_state.active_motion = None;
            drop(state);
            let reason = error.to_value(ctx);
            settle_action_callback(ctx, &reject, reason)?;
        }
    } else {
        bridge.borrow_mut().action_state.active_motion = Some(next_motion);
        if let Err(error) = schedule_action_motion(
            ctx,
            index,
            &resolve,
            &reject,
            next_motion_delay(next_motion),
        ) {
            let mut state = bridge.borrow_mut();
            state.action_state.plan = None;
            state.action_state.busy = false;
            state.action_state.active_motion = None;
            drop(state);
            let reason = error.to_value(ctx);
            settle_action_callback(ctx, &reject, reason)?;
        }
    }
    Ok(())
}

fn settle_action_callback(ctx: &mut Ctx, callback: &Value, value: Value) -> OpResult<()> {
    let callback = JsFunction::from_value(callback.clone())
        .ok_or_else(|| OpError::type_error("action sequence resolver is not callable"))?;
    callback.call(ctx, Value::Undefined, &[value]).map(|_| ())
}

fn run_action_tick(
    ctx: &mut Ctx,
    bridge: &Rc<RefCell<RealmBridge>>,
    tick: ActionTick,
    pointer_state_indices: &[usize; MAX_ACTION_SOURCES],
) -> OpResult<()> {
    let realm = window_globals::current_dom_realm(ctx)
        .ok_or_else(|| OpError::new("InvalidStateError", "testdriver has no document"))?;
    let tick_delay = tick.delay;
    for action in tick.actions {
        match action {
            PlannedAction::Pause(_) => {}
            PlannedAction::KeyDown(key) => {
                let held_before = bridge.borrow().action_state.held_keys.clone();
                let repeat = held_before.iter().any(|held| held == &key);
                let mut held_after = held_before;
                if !repeat {
                    held_after.push(key.clone());
                }
                realm.note_keyboard_modality();
                match super::keyboard_automation::trusted_action_key_down(
                    ctx,
                    &realm,
                    &key,
                    &held_after,
                    repeat,
                ) {
                    Ok(()) => bridge.borrow_mut().action_state.held_keys = held_after,
                    Err(super::keyboard_automation::TrustedInputError::UnsupportedKey {
                        ..
                    }) => {
                        bridge
                            .borrow()
                            .case
                            .record_unsupported(UNSUPPORTED_ACTION_PROFILE);
                        return Err(OpError::new(
                            "NotSupportedError",
                            UNSUPPORTED_ACTION_PROFILE,
                        ));
                    }
                    Err(super::keyboard_automation::TrustedInputError::Operation(error)) => {
                        return Err(error)
                    }
                }
            }
            PlannedAction::KeyUp(key) => {
                let held_before = bridge.borrow().action_state.held_keys.clone();
                if held_before.iter().any(|held| held == &key) {
                    super::keyboard_automation::trusted_action_key_up(
                        ctx,
                        &realm,
                        &key,
                        &held_before,
                    )
                    .map_err(|error| match error {
                        super::keyboard_automation::TrustedInputError::UnsupportedKey {
                            ..
                        } => OpError::new("NotSupportedError", UNSUPPORTED_ACTION_PROFILE),
                        super::keyboard_automation::TrustedInputError::Operation(error) => error,
                    })?;
                    bridge
                        .borrow_mut()
                        .action_state
                        .held_keys
                        .retain(|held| held != &key);
                }
            }
            PlannedAction::PointerMove {
                pointer_slot,
                x,
                y,
                origin,
                duration,
                properties,
            } => {
                let pointer_state_index =
                    action_pointer_index(pointer_state_indices, pointer_slot)?;
                let state = action_pointer_state(bridge, pointer_state_index)?;
                let (x, y) = pointer_move_coordinates(&realm, &state, &origin, x, y)?;
                geometry::element_from_point_in_tree(&realm, None, x, y)?.ok_or_else(|| {
                    OpError::new(
                        "MoveTargetOutOfBoundsError",
                        "pointer point is outside the document viewport",
                    )
                })?;
                let modifiers = pointer_modifiers(&bridge.borrow().action_state.held_keys);
                if duration.is_zero() {
                    perform_pointer_move(
                        ctx,
                        &realm,
                        bridge,
                        pointer_state_index,
                        x,
                        y,
                        modifiers,
                        properties,
                    )?;
                } else {
                    let motion = ActiveTimedAction::Pointer(ActivePointerMotion {
                        pointer_state_index,
                        from_x: state.x,
                        from_y: state.y,
                        to_x: x,
                        to_y: y,
                        duration,
                        samples: motion_sample_count(duration),
                        next_sample: 1,
                        post_delay: tick_delay.saturating_sub(duration),
                        properties,
                    });
                    bridge.borrow_mut().action_state.active_motion = Some(motion);
                }
            }
            PlannedAction::PointerDown {
                pointer_slot,
                properties,
            } => {
                let pointer_state_index =
                    action_pointer_index(pointer_state_indices, pointer_slot)?;
                let mut state = action_pointer_state(bridge, pointer_state_index)?;
                apply_pointer_sample(&mut state, properties, true);
                state.properties.is_primary = {
                    let bridge_state = bridge.borrow();
                    !bridge_state.action_state.pointers.iter().enumerate().any(
                        |(index, pointer)| {
                            index != pointer_state_index
                                && pointer.kind == pointer_kind(&state)
                                && pointer.state.buttons != 0
                        },
                    )
                };
                let target = geometry::element_from_point_in_tree(&realm, None, state.x, state.y)?
                    .ok_or_else(|| {
                        OpError::new(
                            "MoveTargetOutOfBoundsError",
                            "pointer is outside the document viewport",
                        )
                    })?;
                let modifiers = pointer_modifiers(&bridge.borrow().action_state.held_keys);
                let mut next = state;
                if next.hover_target != Some(target) {
                    forms::trusted_pointer_move(
                        ctx, &realm, &mut next, target, state.x, state.y, modifiers,
                    )?;
                }
                forms::trusted_pointer_down(ctx, &realm, &mut next, target, modifiers)?;
                set_action_pointer_state(bridge, pointer_state_index, next)?;
            }
            PlannedAction::PointerUp { pointer_slot } => {
                let pointer_state_index =
                    action_pointer_index(pointer_state_indices, pointer_slot)?;
                let mut state = action_pointer_state(bridge, pointer_state_index)?;
                // `pointerup` ends contact but does not replace the last device sample with a
                // fresh default sample. Preserve contact geometry/orientation while reporting
                // the required zero pressure for the released pointer.
                state.pressure = 0.0;
                let target = geometry::element_from_point_in_tree(&realm, None, state.x, state.y)?
                    .ok_or_else(|| {
                        OpError::new(
                            "MoveTargetOutOfBoundsError",
                            "pointer is outside the document viewport",
                        )
                    })?;
                let modifiers = pointer_modifiers(&bridge.borrow().action_state.held_keys);
                let mut next = state;
                if next.hover_target != Some(target) {
                    forms::trusted_pointer_move(
                        ctx, &realm, &mut next, target, state.x, state.y, modifiers,
                    )?;
                }
                let detail = click_detail_for(&bridge, target, state.x, state.y);
                let clicked = forms::trusted_pointer_up_with_click_detail(
                    ctx, &realm, &mut next, target, modifiers, detail,
                )?;
                set_action_pointer_state(bridge, pointer_state_index, next)?;
                if clicked && next.properties.pointer_type == "mouse" {
                    remember_click(&bridge, target, next.x, next.y, detail);
                }
            }
            PlannedAction::WheelScroll {
                x,
                y,
                delta_x,
                delta_y,
                origin,
                duration,
            } => {
                let (x, y) = wheel_coordinates(&realm, &origin, x, y)?;
                let target =
                    geometry::element_from_point_in_tree(&realm, None, x, y)?.ok_or_else(|| {
                        OpError::new(
                            "MoveTargetOutOfBoundsError",
                            "wheel point is outside the document viewport",
                        )
                    })?;
                let modifiers = pointer_modifiers(&bridge.borrow().action_state.held_keys);
                if duration.is_zero() {
                    scrolling::dispatch_wheel(
                        ctx, &realm, target, x, y, delta_x, delta_y, modifiers,
                    )?;
                } else {
                    let motion = ActiveTimedAction::Wheel(ActiveWheelMotion {
                        x,
                        y,
                        delta_x,
                        delta_y,
                        duration,
                        samples: motion_sample_count(duration),
                        next_sample: 1,
                        post_delay: tick_delay.saturating_sub(duration),
                    });
                    bridge.borrow_mut().action_state.active_motion = Some(motion);
                }
            }
        }
    }
    Ok(())
}

fn perform_pointer_move(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    bridge: &Rc<RefCell<RealmBridge>>,
    pointer_state_index: usize,
    x: f64,
    y: f64,
    modifiers: super::ui_events::UserAgentModifiers,
    properties: PointerSample,
) -> OpResult<()> {
    let mut state = action_pointer_state(bridge, pointer_state_index)?;
    apply_pointer_sample(&mut state, properties, false);
    let target = geometry::element_from_point_in_tree(realm, None, x, y)?.ok_or_else(|| {
        OpError::new(
            "MoveTargetOutOfBoundsError",
            "pointer point is outside the document viewport",
        )
    })?;
    let mut next = state;
    forms::trusted_pointer_move(ctx, realm, &mut next, target, x, y, modifiers)?;
    set_action_pointer_state(bridge, pointer_state_index, next)
}

fn perform_wheel_scroll(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    x: f64,
    y: f64,
    delta_x: f64,
    delta_y: f64,
    modifiers: super::ui_events::UserAgentModifiers,
) -> OpResult<()> {
    let target = geometry::element_from_point_in_tree(realm, None, x, y)?.ok_or_else(|| {
        OpError::new(
            "MoveTargetOutOfBoundsError",
            "wheel point is outside the document viewport",
        )
    })?;
    scrolling::dispatch_wheel(ctx, realm, target, x, y, delta_x, delta_y, modifiers)
}

fn action_pointer_index(
    pointer_state_indices: &[usize; MAX_ACTION_SOURCES],
    slot: usize,
) -> OpResult<usize> {
    pointer_state_indices
        .get(slot)
        .copied()
        .filter(|index| *index != usize::MAX)
        .ok_or_else(|| OpError::new("InvalidStateError", "pointer source was not prepared"))
}

fn action_pointer_state(
    bridge: &Rc<RefCell<RealmBridge>>,
    index: usize,
) -> OpResult<forms::TrustedPointerState> {
    bridge
        .borrow()
        .action_state
        .pointers
        .get(index)
        .map(|pointer| pointer.state)
        .ok_or_else(|| OpError::new("InvalidStateError", "pointer state is unavailable"))
}

fn set_action_pointer_state(
    bridge: &Rc<RefCell<RealmBridge>>,
    index: usize,
    state: forms::TrustedPointerState,
) -> OpResult<()> {
    let mut bridge = bridge.borrow_mut();
    let pointer = bridge
        .action_state
        .pointers
        .get_mut(index)
        .ok_or_else(|| OpError::new("InvalidStateError", "pointer state is unavailable"))?;
    pointer.state = state;
    Ok(())
}

fn pointer_kind(state: &forms::TrustedPointerState) -> PointerKind {
    match state.properties.pointer_type {
        "touch" => PointerKind::Touch,
        "pen" => PointerKind::Pen,
        _ => PointerKind::Mouse,
    }
}

fn apply_pointer_sample(state: &mut forms::TrustedPointerState, sample: PointerSample, down: bool) {
    state.properties.width = sample.width;
    state.properties.height = sample.height;
    state.properties.tangential_pressure = sample.tangential_pressure;
    state.properties.tilt_x = sample.tilt_x;
    state.properties.tilt_y = sample.tilt_y;
    state.properties.twist = sample.twist;
    state.pressure =
        sample
            .pressure
            .unwrap_or_else(|| if down || state.buttons != 0 { 0.5 } else { 0.0 });
}

fn click_detail_for(bridge: &Rc<RefCell<RealmBridge>>, target: NodeId, x: f64, y: f64) -> i32 {
    let bridge = bridge.borrow();
    let Some(previous) = bridge.action_state.last_click else {
        return 1;
    };
    if previous.target == target
        && (previous.x - x).abs() <= 4.0
        && (previous.y - y).abs() <= 4.0
        && previous.time.elapsed() <= Duration::from_millis(500)
    {
        previous.detail.saturating_add(1)
    } else {
        1
    }
}

fn remember_click(bridge: &Rc<RefCell<RealmBridge>>, target: NodeId, x: f64, y: f64, detail: i32) {
    bridge.borrow_mut().action_state.last_click = Some(LastClick {
        target,
        x,
        y,
        detail,
        time: Instant::now(),
    });
}

fn pointer_modifiers(held: &[String]) -> super::ui_events::UserAgentModifiers {
    super::ui_events::UserAgentModifiers {
        ctrl: held.iter().any(|key| key == "\u{e009}"),
        shift: held.iter().any(|key| key == "\u{e008}"),
        alt: held.iter().any(|key| key == "\u{e00a}"),
        meta: held.iter().any(|key| key == "\u{e03d}"),
    }
}

fn pointer_move_coordinates(
    realm: &Rc<DomRealm>,
    state: &forms::TrustedPointerState,
    origin: &PointerOrigin,
    x: f64,
    y: f64,
) -> OpResult<(f64, f64)> {
    let (origin_x, origin_y) = match origin {
        PointerOrigin::Viewport => (0.0, 0.0),
        PointerOrigin::Pointer => (state.x, state.y),
        PointerOrigin::Element(node) => visible_element_center(realm, *node)?,
    };
    let x = origin_x + x;
    let y = origin_y + y;
    if !x.is_finite() || !y.is_finite() {
        return Err(OpError::type_error("pointer coordinates must be finite"));
    }
    Ok((x, y))
}

fn wheel_coordinates(
    realm: &Rc<DomRealm>,
    origin: &WheelOrigin,
    x: f64,
    y: f64,
) -> OpResult<(f64, f64)> {
    let (origin_x, origin_y) = match origin {
        WheelOrigin::Viewport => (0.0, 0.0),
        WheelOrigin::Element(node) => visible_element_center(realm, *node)?,
    };
    let x = origin_x + x;
    let y = origin_y + y;
    if !x.is_finite() || !y.is_finite() {
        return Err(OpError::type_error("wheel coordinates must be finite"));
    }
    Ok((x, y))
}

fn visible_element_center(realm: &Rc<DomRealm>, node: NodeId) -> OpResult<(f64, f64)> {
    realm.flush_layout()?;
    let session = realm.session_handle();
    let session = session.borrow();
    let rect = session
        .bounding_client_rect(node)
        .ok_or_else(|| OpError::new("NoSuchElementError", "action origin has no layout box"))?;
    let (viewport_width, viewport_height) = session.viewport_size().ok_or_else(|| {
        OpError::new(
            "MoveTargetOutOfBoundsError",
            "action origin has no viewport intersection",
        )
    })?;
    let left = rect.x.max(0.0);
    let top = rect.y.max(0.0);
    let right = (rect.x + rect.width).min(viewport_width as f32);
    let bottom = (rect.y + rect.height).min(viewport_height as f32);
    if ![left, top, right, bottom].into_iter().all(f32::is_finite) || right <= left || bottom <= top
    {
        return Err(OpError::new(
            "MoveTargetOutOfBoundsError",
            "action origin is outside the viewport",
        ));
    }
    Ok((
        f64::from((left + right) * 0.5),
        f64::from((top + bottom) * 0.5),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    fn eval_value(engine: &mut Engine, source: &str) -> Value {
        match engine.eval_value(source).expect("valid action fixture") {
            Ok(value) => value,
            Err(_) => panic!("action fixture threw"),
        }
    }

    struct NoText;
    impl lumen_html::paint::TextShaper for NoText {
        fn shape(&self, _: &str, _: f32) -> Result<lumen_html::paint::ShapedRun, ()> {
            Err(())
        }
        fn ascent(&self, size: f32) -> f32 {
            size * 0.8
        }
        fn line_height(&self, size: f32) -> f32 {
            size * 1.2
        }
    }

    #[test]
    fn action_preflight_accepts_bounded_timed_mouse_and_wheel_sources() {
        let mut engine = Engine::new();
        let actions = eval_value(
            &mut engine,
            r#"[
                {type:'pointer',id:'mouse',parameters:{pointerType:'mouse'},actions:[
                    {type:'pointerMove',x:40,y:25,duration:48}
                ]},
                {type:'none',id:'wait',actions:[{type:'pause',duration:64}]}
            ]"#,
        );
        let plan = match parse_action_sequence(engine.ctx(), &actions) {
            Ok(plan) => plan,
            Err(_) => panic!("bounded mouse motion was rejected"),
        };
        assert_eq!(plan.ticks.len(), 1);
        assert_eq!(plan.ticks[0].delay, Duration::from_millis(64));
        assert!(matches!(
            plan.ticks[0].actions[0],
            PlannedAction::PointerMove {
                duration,
                x: 40.0,
                y: 25.0,
                ..
            } if duration == Duration::from_millis(48)
        ));

        let wheel = eval_value(
            &mut engine,
            r#"[{type:'wheel',id:'wheel',actions:[{type:'scroll',x:7,y:9,deltaX:3,deltaY:11}]}]"#,
        );
        let plan = match parse_action_sequence(engine.ctx(), &wheel) {
            Ok(plan) => plan,
            Err(_) => panic!("pixel wheel action was rejected"),
        };
        assert!(matches!(
            plan.ticks[0].actions[0],
            PlannedAction::WheelScroll {
                x: 7.0,
                y: 9.0,
                delta_x: 3.0,
                delta_y: 11.0,
                ..
            }
        ));
    }

    #[test]
    fn action_preflight_rejects_unsynchronized_timed_moves_before_dispatch() {
        let mut engine = Engine::new();
        let actions = eval_value(
            &mut engine,
            r#"[
                {type:'pointer',id:'mouse',actions:[{type:'pointerMove',x:8,y:9,duration:32}]},
                {type:'key',id:'keys',actions:[{type:'keyDown',value:'a'}]}
            ]"#,
        );
        assert!(parse_action_sequence(engine.ctx(), &actions).is_err());
    }

    #[test]
    fn timed_mouse_move_interpolates_through_the_shared_timer_heap() {
        let mut engine = Engine::new();
        lumen_timers::install(&mut engine, 64);
        let realm = crate::install(
            engine.ctx(),
            "<style>html,body{margin:0}#target{width:100px;height:80px}</style><div id='target'></div>",
            32,
        )
        .expect("native DOM installs");
        realm.set_layout_flusher(Rc::new(|session| {
            session
                .display_list(100, 80, &NoText)
                .map(|_| ())
                .map_err(|error| format!("{error:?}"))
        }));
        eval_value(
            &mut engine,
            "globalThis.test_driver_internal={in_automation:true}; globalThis.test_driver={}; globalThis.move_events=[]; document.getElementById('target').addEventListener('pointermove', event => move_events.push([event.clientX,event.clientY,event.isTrusted])); true",
        );
        let case = Rc::new(TestDriverCaseState::default());
        assert!(install_after_testdriver_script(engine.ctx(), case).expect("bridge install"));
        eval_value(
            &mut engine,
            "globalThis.action_done=false; test_driver_internal.action_sequence([{type:'pointer',id:'mouse',parameters:{pointerType:'mouse'},actions:[{type:'pointerMove',x:60,y:30,duration:48}]}]).then(()=>action_done=true,()=>action_done='rejected'); true",
        );

        let deadline = lumen_host::time::Instant::now() + Duration::from_secs(2);
        for _ in 0..16 {
            let due = engine
                .ctx()
                .op_state()
                .get_mut::<lumen_timers::Timers>()
                .expect("timer heap installed")
                .take_next_due(deadline);
            let Some((callback, args, owner)) = due else {
                break;
            };
            let callback = JsFunction::from_value(callback).expect("timer callback is callable");
            let result = engine
                .ctx()
                .with_host_realm(&owner, |ctx| callback.call(ctx, Value::Undefined, &args));
            assert!(matches!(result, Ok(Ok(_))), "timer callback completed");
            engine.run_microtasks();
        }
        let result = eval_value(
            &mut engine,
            "action_done===true && move_events.length===3 && move_events[0][0]===20 && move_events[0][1]===10 && move_events[1][0]===40 && move_events[1][1]===20 && move_events[2][0]===60 && move_events[2][1]===30 && move_events.every(event=>event[2]===true)",
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    fn setup_action_test(html: &str) -> (Engine, Rc<TestDriverCaseState>) {
        let mut engine = Engine::new();
        lumen_timers::install(&mut engine, 64);
        let realm = crate::install(engine.ctx(), html, 32).expect("native DOM installs");
        realm.set_layout_flusher(Rc::new(|session| {
            session
                .display_list(100, 80, &NoText)
                .map(|_| ())
                .map_err(|error| format!("{error:?}"))
        }));
        eval_value(
            &mut engine,
            "globalThis.test_driver_internal={}; globalThis.test_driver={}; true",
        );
        let case = Rc::new(TestDriverCaseState::default());
        assert!(install_after_testdriver_script(engine.ctx(), case.clone()).unwrap());
        (engine, case)
    }

    fn finish_action_sequence(engine: &mut Engine) {
        let deadline = lumen_host::time::Instant::now() + Duration::from_secs(2);
        for _ in 0..256 {
            if matches!(
                eval_value(engine, "globalThis.action_done === true"),
                Value::Bool(true)
            ) {
                return;
            }
            let due = engine
                .ctx()
                .op_state()
                .get_mut::<lumen_timers::Timers>()
                .expect("timer heap installed")
                .take_next_due(deadline);
            let Some((callback, args, owner)) = due else {
                break;
            };
            let callback = JsFunction::from_value(callback).expect("timer callback is callable");
            let result = engine
                .ctx()
                .with_host_realm(&owner, |ctx| callback.call(ctx, Value::Undefined, &args));
            assert!(
                matches!(result, Ok(Ok(_))),
                "action timer callback completed"
            );
            engine.run_microtasks();
        }
        assert!(
            matches!(
                eval_value(engine, "globalThis.action_done === true"),
                Value::Bool(true)
            ),
            "action sequence settled through the shared timer heap"
        );
    }

    #[test]
    fn touch_and_pen_actions_keep_source_ids_and_pointer_event_profiles() {
        let (mut engine, _case) = setup_action_test(
            "<style>html,body{margin:0}#target{width:100px;height:80px}</style><div id='target'></div>",
        );
        eval_value(
            &mut engine,
            r#"(() => {
            globalThis.pointerLog=[]; globalThis.mouseLog=[];
            const target=document.getElementById('target');
            for (const type of ['pointerover','pointerdown','pointermove','pointerup','pointerout'])
                target.addEventListener(type, event => pointerLog.push([
                    event.type,event.pointerId,event.pointerType,event.isPrimary,
                    event.width,event.height,event.pressure,event.tiltX,event.tiltY,
                    event.twist,event.buttons,event.isTrusted
                ]));
            for (const type of ['mousedown','mouseup'])
                target.addEventListener(type, event => mouseLog.push([event.type,event.isTrusted]));
            return true;
            })()"#,
        );
        eval_value(
            &mut engine,
            r#"
            globalThis.action_done=false;
            test_driver_internal.action_sequence([
                {type:'pointer',id:'touch-a',parameters:{pointerType:'touch'},actions:[
                    {type:'pointerMove',x:10,y:10},
                    {type:'pointerDown',button:0,width:23,height:31,pressure:0.78},
                    {type:'pointerMove',x:12,y:11,width:39,height:35,pressure:0.91},
                    {type:'pointerUp',button:0}
                ]},
                {type:'pointer',id:'touch-b',parameters:{pointerType:'touch'},actions:[
                    {type:'pointerMove',x:12,y:10},
                    {type:'pointerDown',button:0,width:17,height:19,pressure:0.5},
                    {type:'pointerUp',button:0}
                ]},
                {type:'pointer',id:'pen-a',parameters:{pointerType:'pen'},actions:[
                    {type:'pointerMove',x:10,y:10},
                    {type:'pointerDown',button:0,width:5,height:7,pressure:0.36,tiltX:-72,tiltY:9,twist:86},
                    {type:'pause',duration:0},
                    {type:'pointerUp',button:0}
                ]}
            ]).then(()=>action_done=true, error=>{globalThis.action_error=error; action_done='rejected';});
            true
            "#,
        );
        finish_action_sequence(&mut engine);
        let diagnostic = eval_value(
            &mut engine,
            r#"
                (() => {
                    const down = pointerLog.filter(event => event[0] === 'pointerdown');
                    const move = pointerLog.filter(event => event[0] === 'pointermove');
                    const up = pointerLog.filter(event => event[0] === 'pointerup');
                    const touchA = down.find(event => event[1] === 2);
                    const touchB = down.find(event => event[1] === 3);
                    const pen = down.find(event => event[1] === 4);
                    const touchMove = move.find(event => event[1] === 2);
                    const touchUp = up.find(event => event[1] === 2);
                    const penUp = up.find(event => event[1] === 4);
                    const checks = {
                        completed: action_done === true,
                        downCount: down.length === 3,
                        touchA: !!touchA && touchA[2] === 'touch' && touchA[3] === true &&
                            touchA[4] === 23 && touchA[5] === 31 && touchA[6] === Math.fround(0.78),
                        touchB: !!touchB && touchB[2] === 'touch' && touchB[3] === false,
                        touchMove: !!touchMove && touchMove[4] === 39 && touchMove[5] === 35 &&
                            touchMove[6] === Math.fround(0.91),
                        touchUp: !!touchUp && touchUp[4] === 39 && touchUp[5] === 35 && touchUp[6] === 0,
                        pen: !!pen && pen[2] === 'pen' && pen[3] === true && pen[4] === 5 &&
                            pen[5] === 7 && pen[6] === Math.fround(0.36) && pen[7] === -72 &&
                            pen[8] === 9 && pen[9] === 86,
                        penUp: !!penUp && penUp[4] === 5 && penUp[5] === 7 && penUp[6] === 0,
                        compatibilityMouse: mouseLog.filter(event => event[0] === 'mousedown').length === 1 &&
                            mouseLog.filter(event => event[0] === 'mouseup').length === 1,
                        trusted: pointerLog.every(event => event[11] === true),
                    };
                    return JSON.stringify({checks, down, move, up, mouseLog, actionError:String(globalThis.action_error)});
                })()
                "#,
        );
        let diagnostic = match diagnostic {
            Value::Str(value) => value.to_string(),
            _ => String::from("<non-string Actions diagnostic>"),
        };
        assert!(
            diagnostic.contains("\"completed\":true")
                && diagnostic.contains("\"downCount\":true")
                && diagnostic.contains("\"touchA\":true")
                && diagnostic.contains("\"touchB\":true")
                && diagnostic.contains("\"touchMove\":true")
                && diagnostic.contains("\"touchUp\":true")
                && diagnostic.contains("\"pen\":true")
                && diagnostic.contains("\"penUp\":true")
                && diagnostic.contains("\"compatibilityMouse\":true")
                && diagnostic.contains("\"trusted\":true"),
            "touch/pen Actions checks failed: {diagnostic}"
        );
    }

    #[test]
    fn sequential_mouse_action_sources_keep_persistent_state() {
        let (mut engine, case) = setup_action_test(
            "<style>html,body{margin:0}#target{width:100px;height:80px}</style><div id='target'></div>",
        );
        eval_value(
            &mut engine,
            "globalThis.pointerLog=[]; document.getElementById('target').addEventListener('pointermove', event => pointerLog.push(['move',event.pointerId,event.isTrusted])); document.getElementById('target').addEventListener('pointerdown', event => pointerLog.push(['down',event.pointerId,event.isTrusted])); document.getElementById('target').addEventListener('pointerup', event => pointerLog.push(['up',event.pointerId,event.isTrusted])); true",
        );
        eval_value(
            &mut engine,
            r#"
            globalThis.action_done=false;
            const first=[{type:'pointer',id:'mouse-first',parameters:{pointerType:'mouse'},actions:[
                {type:'pointerMove',x:10,y:10},{type:'pointerDown',button:0},{type:'pointerUp',button:0}
            ]}];
            const second=[{type:'pointer',id:'mouse-second',parameters:{pointerType:'mouse'},actions:[
                {type:'pointerMove',x:20,y:20},{type:'pointerDown',button:0},{type:'pointerUp',button:0}
            ]}];
            test_driver_internal.action_sequence(first)
                .then(()=>test_driver_internal.action_sequence(second))
                .then(()=>action_done=true,error=>{globalThis.action_error=error; action_done='rejected';});
            true
            "#,
        );
        finish_action_sequence(&mut engine);
        let result = eval_value(
            &mut engine,
            "action_done===true && pointerLog.filter(event=>event[0]==='down').length===2 && pointerLog.filter(event=>event[0]==='up').length===2 && pointerLog.every(event=>event[1]===1 && event[2]===true)",
        );
        assert!(
            matches!(result, Value::Bool(true)),
            "multiple mouse sources must run"
        );
        assert_eq!(case.unsupported_reason(), None);
    }

    #[test]
    fn action_click_detail_counts_consecutive_clicks_and_resets_after_motion() {
        let (mut engine, _case) = setup_action_test(
            "<style>html,body{margin:0}#target{width:100px;height:80px}</style><div id='target'></div>",
        );
        eval_value(
            &mut engine,
            "globalThis.clickDetails=[]; document.getElementById('target').addEventListener('click', event => clickDetails.push([event.detail,event.isTrusted])); true",
        );
        eval_value(
            &mut engine,
            r#"
            globalThis.action_done=false;
            test_driver_internal.action_sequence([{type:'pointer',id:'mouse',parameters:{pointerType:'mouse'},actions:[
                {type:'pointerMove',x:10,y:10},
                {type:'pointerDown',button:0},{type:'pointerUp',button:0},
                {type:'pointerDown',button:0},{type:'pointerUp',button:0},
                {type:'pointerMove',x:15,y:15},
                {type:'pointerDown',button:0},{type:'pointerUp',button:0},
                {type:'pointerDown',button:0},{type:'pointerUp',button:0},
                {type:'pointerDown',button:0},{type:'pointerUp',button:0}
            ]}]).then(()=>action_done=true,()=>action_done='rejected');
            true
            "#,
        );
        finish_action_sequence(&mut engine);
        assert!(matches!(
            eval_value(
                &mut engine,
                "action_done===true && clickDetails.length===5 && clickDetails.map(entry=>entry[0]).join(',')==='1,2,1,2,3' && clickDetails.every(entry=>entry[1]===true)",
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn timed_wheel_actions_interpolate_trusted_pixel_deltas() {
        let (mut engine, _case) = setup_action_test(
            "<style>html,body{margin:0}#outer{overflow:auto;width:80px;height:50px}#content{height:200px}</style><div id='outer'><div id='content'></div></div>",
        );
        eval_value(
            &mut engine,
            "globalThis.wheelLog=[]; document.getElementById('outer').addEventListener('wheel', event => wheelLog.push([event.deltaX,event.deltaY,event.deltaMode,event.isTrusted])); true",
        );
        eval_value(
            &mut engine,
            r#"
            globalThis.action_done=false;
            test_driver_internal.action_sequence([{type:'wheel',id:'wheel',actions:[
                {type:'scroll',x:10,y:10,deltaX:4,deltaY:12,duration:32}
            ]}]).then(()=>action_done=true,()=>action_done='rejected');
            true
            "#,
        );
        finish_action_sequence(&mut engine);
        assert!(matches!(
            eval_value(
                &mut engine,
                "action_done===true && wheelLog.length===2 && wheelLog[0][0]===2 && wheelLog[1][0]===2 && wheelLog[0][1]===6 && wheelLog[1][1]===6 && wheelLog.every(event=>event[2]===WheelEvent.DOM_DELTA_PIXEL && event[3]===true) && document.getElementById('outer').scrollTop===12",
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn action_timer_helper_preserves_deadlines_and_releases_retired_callbacks() {
        let mut engine = Engine::new();
        lumen_timers::install(&mut engine, 8);
        let parent = engine.ctx().current_host_realm();
        let child = engine.ctx().create_host_realm();
        let callback = engine
            .ctx()
            .bound_function(&lumen_bind::FnItem::of::<action_step::Op>());
        let root_weak = engine.ctx().weak_value(&callback).unwrap();
        let started = lumen_host::time::Instant::now();
        let root_id = lumen_timers::schedule_host_callback(
            engine.ctx(),
            callback,
            &[],
            Duration::from_millis(80),
        )
        .unwrap();
        let (child_id, child_weak) = engine
            .ctx()
            .with_host_realm(&child, |ctx| {
                let callback = ctx.bound_function(&lumen_bind::FnItem::of::<action_step::Op>());
                let weak = ctx.weak_value(&callback).unwrap();
                let id = lumen_timers::schedule_host_callback(
                    ctx,
                    callback,
                    &[],
                    Duration::from_millis(120),
                )
                .unwrap();
                (id, weak)
            })
            .unwrap();
        assert_ne!(root_id, child_id, "host timer ids stay exact and unique");

        let timers = engine
            .ctx()
            .op_state()
            .get_mut::<lumen_timers::Timers>()
            .expect("timer extension installed");
        let deadline = timers.next_deadline().expect("scheduled timer deadline");
        assert!(deadline >= started + Duration::from_millis(80));
        assert_eq!(timers.pending_for_realm(&parent), 1);
        assert_eq!(timers.pending_for_realm(&child), 1);

        assert_eq!(
            engine
                .ctx()
                .op_state()
                .get_mut::<lumen_timers::Timers>()
                .unwrap()
                .cancel_realm(&child),
            1
        );
        assert_eq!(
            engine
                .ctx()
                .op_state()
                .get::<lumen_timers::Timers>()
                .unwrap()
                .pending_for_realm(&parent),
            1,
            "retiring child input work leaves the parent timer intact"
        );
        engine.ctx().collect_garbage();
        assert!(
            root_weak.upgrade().is_some(),
            "live parent timer roots its callback"
        );
        assert!(
            child_weak.upgrade().is_none(),
            "cancelling a child timer releases its callback graph"
        );
    }
}

struct PointerCoordinates {
    x: f64,
    y: f64,
}

impl<'a> FromArg<'a, JsHost> for PointerCoordinates {
    fn from_arg(
        cx: &'a lumen::embed::ArgCx<'_>,
        value: &'a Value,
        _at: Slot,
    ) -> Result<Self, Value> {
        <JsHost as Host>::with_ctx(cx, |ctx| {
            let x = ctx.member_get(value, "x")?;
            let x = ctx.coerce_number(&x)?;
            let y = ctx.member_get(value, "y")?;
            let y = ctx.coerce_number(&y)?;
            Ok(Self { x, y })
        })
    }
}

fn element_data(ctx: &mut Ctx, value: &Value) -> OpResult<(Rc<DomRealm>, NodeId)> {
    ctx.with_instance::<DomElement, _>(value, |element| {
        (element.base.realm.clone(), element.base.id)
    })
}

fn click_target_contains_hit(realm: &Rc<DomRealm>, target: NodeId, hit: NodeId) -> bool {
    let session = realm.session.borrow();
    let document = session.document();
    let mut current = Some(hit);
    while let Some(node) = current {
        if node == target {
            return true;
        }
        current = document.parent(node).ok().flatten();
    }
    false
}

#[lumen_bind::op(name = "click")]
fn click(ctx: &mut Ctx, element: Value, coordinates: PointerCoordinates) -> Promise<Value> {
    let result = (|| {
        let realm = window_globals::current_dom_realm(ctx)
            .ok_or_else(|| OpError::new("InvalidStateError", "testdriver has no document"))?;
        let (target_realm, node) = element_data(ctx, &element)?;
        if !Rc::ptr_eq(&realm, &target_realm) {
            if let Some(bridge) = bridge_for_current_realm(ctx) {
                bridge.borrow().case.record_unsupported(UNSUPPORTED_CONTEXT);
            }
            return Err(OpError::new(
                "NotSupportedError",
                "native testdriver click currently requires an element in its caller document",
            ));
        }
        let hit = geometry::element_from_point_in_tree(&realm, None, coordinates.x, coordinates.y)?;
        if !hit.is_some_and(|hit| click_target_contains_hit(&realm, node, hit)) {
            return Err(OpError::new(
                "ElementClickInterceptedError",
                "testdriver click target is not the topmost hit-tested element",
            ));
        }
        forms::trusted_pointer_click(ctx, &realm, node, coordinates.x, coordinates.y)
    })();
    Promise::ready(result.map(|()| Value::Undefined))
}

#[lumen_bind::op(name = "send_keys", coerce)]
fn send_keys(ctx: &mut Ctx, element: Value, keys: String) -> Promise<Value> {
    let result = (|| {
        let realm = window_globals::current_dom_realm(ctx)
            .ok_or_else(|| OpError::new("InvalidStateError", "testdriver has no document"))?;
        let (target_realm, node) = element_data(ctx, &element)?;
        if !Rc::ptr_eq(&realm, &target_realm) {
            if let Some(bridge) = bridge_for_current_realm(ctx) {
                bridge.borrow().case.record_unsupported(UNSUPPORTED_CONTEXT);
            }
            return Err(OpError::new(
                "NotSupportedError",
                "native testdriver send_keys currently requires an element in its caller document",
            ));
        }
        match super::keyboard_automation::trusted_send_keys(ctx, &realm, node, &keys) {
            Ok(()) => Ok(()),
            Err(super::keyboard_automation::TrustedInputError::UnsupportedKey { .. }) => {
                if let Some(bridge) = bridge_for_current_realm(ctx) {
                    bridge.borrow().case.record_unsupported(UNSUPPORTED_KEY);
                }
                Err(OpError::new(
                    "NotSupportedError",
                    "native testdriver send_keys does not support one of the requested keys",
                ))
            }
            Err(super::keyboard_automation::TrustedInputError::Operation(error)) => Err(error),
        }
    })();
    Promise::ready(result.map(|()| Value::Undefined))
}
