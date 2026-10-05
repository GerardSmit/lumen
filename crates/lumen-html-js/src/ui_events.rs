//! Native UI and input event interfaces.
//!
//! The subclasses keep their interface-specific data beside the shared `DomEvent` handle. Event
//! dispatch, trust, propagation, target retargeting, and cancellation therefore remain owned by
//! the common Event implementation.

use super::*;
use crate::window_globals::DomWindow;
use lumen::embed::{Ctx, JsHost, OpError, OpResult, Value};
use lumen_bind::{CtorRet, Host};

pub(crate) fn dictionary_member(
    ctx: &mut Ctx,
    dictionary: &Option<Value>,
    name: &str,
) -> OpResult<Option<Value>> {
    let Some(dictionary) = dictionary
        .as_ref()
        .filter(|value| !matches!(value, Value::Null | Value::Undefined))
    else {
        return Ok(None);
    };
    let value = ctx.member_get(dictionary, name).map_err(OpError::thrown)?;
    if matches!(value, Value::Undefined) {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

pub(crate) fn dictionary_boolean(
    ctx: &mut Ctx,
    dictionary: &Option<Value>,
    name: &str,
    default: bool,
) -> OpResult<bool> {
    Ok(dictionary_member(ctx, dictionary, name)?
        .as_ref()
        .map_or(default, |value| ctx.to_boolean(value)))
}

pub(crate) fn dictionary_string(
    ctx: &mut Ctx,
    dictionary: &Option<Value>,
    name: &str,
    default: &str,
    usv: bool,
) -> OpResult<String> {
    let Some(value) = dictionary_member(ctx, dictionary, name)? else {
        return Ok(default.to_owned());
    };
    let value = ctx.coerce_string(&value).map_err(OpError::thrown)?;
    if usv {
        Ok(lumen::well_formed_utf8(&value).into_owned())
    } else {
        Ok(value.to_string())
    }
}

pub(crate) fn number_to_unsigned(number: f64, bits: u32) -> u32 {
    if !number.is_finite() || number == 0.0 {
        return 0;
    }
    let modulus = 2_f64.powi(bits as i32);
    number.trunc().rem_euclid(modulus) as u32
}

fn number_to_signed(number: f64, bits: u32) -> i32 {
    let unsigned = number_to_unsigned(number, bits) as i64;
    let half = 1_i64 << (bits - 1);
    let modulus = 1_i64 << bits;
    if unsigned >= half {
        (unsigned - modulus) as i32
    } else {
        unsigned as i32
    }
}

fn dictionary_unsigned(
    ctx: &mut Ctx,
    dictionary: &Option<Value>,
    name: &str,
    default: u32,
    bits: u32,
) -> OpResult<u32> {
    let Some(value) = dictionary_member(ctx, dictionary, name)? else {
        return Ok(default);
    };
    Ok(number_to_unsigned(
        ctx.coerce_number(&value).map_err(OpError::thrown)?,
        bits,
    ))
}

fn dictionary_signed(
    ctx: &mut Ctx,
    dictionary: &Option<Value>,
    name: &str,
    default: i32,
    bits: u32,
) -> OpResult<i32> {
    let Some(value) = dictionary_member(ctx, dictionary, name)? else {
        return Ok(default);
    };
    Ok(number_to_signed(
        ctx.coerce_number(&value).map_err(OpError::thrown)?,
        bits,
    ))
}

fn dictionary_double(
    ctx: &mut Ctx,
    dictionary: &Option<Value>,
    name: &str,
    default: f64,
) -> OpResult<f64> {
    let Some(value) = dictionary_member(ctx, dictionary, name)? else {
        return Ok(default);
    };
    let number = ctx.coerce_number(&value).map_err(OpError::thrown)?;
    if !number.is_finite() {
        return Err(OpError::type_error(
            "event dictionary double must be finite",
        ));
    }
    Ok(number)
}

fn nullable_window(ctx: &mut Ctx, value: Value) -> OpResult<Value> {
    if matches!(value, Value::Null | Value::Undefined) {
        return Ok(Value::Null);
    }
    if ctx.with_instance::<DomWindow, _>(&value, |_| ()).is_ok() {
        Ok(value)
    } else {
        Err(OpError::type_error(
            "UIEventInit.view must be a Window or null",
        ))
    }
}

fn nullable_event_target(ctx: &mut Ctx, value: Value) -> OpResult<Value> {
    if matches!(value, Value::Null | Value::Undefined) {
        return Ok(Value::Null);
    }
    if ctx
        .with_instance::<DomEventTarget, _>(&value, |_| ())
        .is_ok()
    {
        Ok(value)
    } else {
        Err(OpError::type_error(
            "relatedTarget must be an EventTarget or null",
        ))
    }
}

fn dictionary_window(ctx: &mut Ctx, dictionary: &Option<Value>, name: &str) -> OpResult<Value> {
    let value = dictionary_member(ctx, dictionary, name)?.unwrap_or(Value::Null);
    nullable_window(ctx, value)
}

fn dictionary_event_target(
    ctx: &mut Ctx,
    dictionary: &Option<Value>,
    name: &str,
) -> OpResult<Value> {
    let value = dictionary_member(ctx, dictionary, name)?.unwrap_or(Value::Null);
    nullable_event_target(ctx, value)
}

fn ui_event(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<DomUIEvent> {
    let dictionary = options;
    let event = DomEvent::new(ctx, kind, dictionary.clone())?;
    // Dictionary inheritance order is EventInit, then UIEventInit.
    let detail = dictionary_signed(ctx, &dictionary, "detail", 0, 32)?;
    let view = dictionary_window(ctx, &dictionary, "view")?;
    let which = dictionary_unsigned(ctx, &dictionary, "which", 0, 32)?;
    Ok(DomUIEvent {
        base: event,
        view: RefCell::new(view),
        detail: Cell::new(detail),
        which: Cell::new(which),
    })
}

#[derive(Clone, Copy, Default)]
struct Modifiers {
    control: bool,
    shift: bool,
    alt: bool,
    meta: bool,
    alt_graph: bool,
    caps_lock: bool,
    fn_key: bool,
    fn_lock: bool,
    hyper: bool,
    num_lock: bool,
    scroll_lock: bool,
    super_key: bool,
    symbol: bool,
    symbol_lock: bool,
}

impl Modifiers {
    fn from_dictionary(ctx: &mut Ctx, dictionary: &Option<Value>) -> OpResult<Self> {
        // EventModifierInit inherits UIEventInit; callers read this only after `ui_event` has
        // consumed the parent dictionary members.
        Ok(Self {
            alt: dictionary_boolean(ctx, dictionary, "altKey", false)?,
            control: dictionary_boolean(ctx, dictionary, "ctrlKey", false)?,
            meta: dictionary_boolean(ctx, dictionary, "metaKey", false)?,
            alt_graph: dictionary_boolean(ctx, dictionary, "modifierAltGraph", false)?,
            caps_lock: dictionary_boolean(ctx, dictionary, "modifierCapsLock", false)?,
            fn_key: dictionary_boolean(ctx, dictionary, "modifierFn", false)?,
            fn_lock: dictionary_boolean(ctx, dictionary, "modifierFnLock", false)?,
            hyper: dictionary_boolean(ctx, dictionary, "modifierHyper", false)?,
            num_lock: dictionary_boolean(ctx, dictionary, "modifierNumLock", false)?,
            scroll_lock: dictionary_boolean(ctx, dictionary, "modifierScrollLock", false)?,
            super_key: dictionary_boolean(ctx, dictionary, "modifierSuper", false)?,
            symbol: dictionary_boolean(ctx, dictionary, "modifierSymbol", false)?,
            symbol_lock: dictionary_boolean(ctx, dictionary, "modifierSymbolLock", false)?,
            shift: dictionary_boolean(ctx, dictionary, "shiftKey", false)?,
        })
    }

    fn get(self, key: &str) -> bool {
        match key {
            "Control" => self.control,
            "Shift" => self.shift,
            "Alt" => self.alt,
            "Meta" => self.meta,
            "AltGraph" => self.alt_graph,
            "CapsLock" => self.caps_lock,
            "Fn" => self.fn_key,
            "FnLock" => self.fn_lock,
            "Hyper" => self.hyper,
            "NumLock" => self.num_lock,
            "ScrollLock" => self.scroll_lock,
            "Super" => self.super_key,
            "Symbol" => self.symbol,
            "SymbolLock" => self.symbol_lock,
            _ => false,
        }
    }
}

fn legacy_ui(
    ctx: &mut Ctx,
    base: &DomUIEvent,
    kind: &str,
    bubbles: bool,
    cancelable: bool,
    view: Value,
    detail: i32,
) -> OpResult<bool> {
    let view = nullable_window(ctx, view)?;
    Ok(apply_legacy_ui(
        base, kind, bubbles, cancelable, view, detail,
    ))
}

fn apply_legacy_ui(
    base: &DomUIEvent,
    kind: &str,
    bubbles: bool,
    cancelable: bool,
    view: Value,
    detail: i32,
) -> bool {
    if !base.base.initialize_legacy(kind, bubbles, cancelable) {
        return false;
    }
    *base.view.borrow_mut() = view;
    base.detail.set(detail);
    true
}

#[lumen_bind::class(name = "UIEvent", extends = DomEvent, hint(js(webidl)))]
pub(crate) struct DomUIEvent {
    base: DomEvent,
    view: RefCell<Value>,
    detail: Cell<i32>,
    which: Cell<u32>,
}

#[lumen_bind::methods]
impl DomUIEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Self> {
        ui_event(ctx, kind, options)
    }

    #[getter]
    fn view(&self) -> Value {
        self.view.borrow().clone()
    }

    #[getter]
    fn detail(&self) -> i32 {
        self.detail.get()
    }

    #[getter]
    fn which(&self) -> u32 {
        self.which.get()
    }

    #[method(coerce)]
    fn init_ui_event(
        &self,
        ctx: &mut Ctx,
        kind: &str,
        #[default(false)] bubbles: bool,
        #[default(false)] cancelable: bool,
        #[default(Value::Null)] view: Value,
        #[default(0)] detail: i32,
    ) -> OpResult<()> {
        let _ = legacy_ui(ctx, self, kind, bubbles, cancelable, view, detail)?;
        Ok(())
    }
}

#[lumen_bind::class(name = "FocusEvent", extends = DomUIEvent, hint(js(webidl)))]
pub(crate) struct DomFocusEvent {
    base: DomUIEvent,
    related_target: RefCell<Value>,
}

#[lumen_bind::methods]
impl DomFocusEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Self> {
        let dictionary = options.clone();
        let base = ui_event(ctx, kind, options)?;
        // FocusEventInit adds its field after its UIEventInit parent.
        let related_target = dictionary_event_target(ctx, &dictionary, "relatedTarget")?;
        base.base.set_related_target(related_target.clone());
        Ok(Self {
            base,
            related_target: RefCell::new(related_target),
        })
    }

    #[getter]
    fn related_target(&self) -> Value {
        self.related_target.borrow().clone()
    }
}

pub(crate) fn user_agent_focus_event(ctx: &mut Ctx, kind: &str, options: Value) -> OpResult<Value> {
    let event = DomFocusEvent::new(ctx, kind, Some(options))?;
    Ok(ctx.new_instance(event))
}

#[lumen_bind::class(name = "MouseEvent", extends = DomUIEvent, hint(js(webidl)))]
pub(crate) struct DomMouseEvent {
    base: DomUIEvent,
    coordinate_realm: std::rc::Weak<DomRealm>,
    modifiers: Cell<Modifiers>,
    screen_x: Cell<i32>,
    screen_y: Cell<i32>,
    client_x: Cell<i32>,
    client_y: Cell<i32>,
    button: Cell<i16>,
    buttons: Cell<u16>,
    related_target: RefCell<Value>,
}

fn mouse_event(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<DomMouseEvent> {
    let dictionary = options.clone();
    let base = ui_event(ctx, kind, options)?;
    // EventModifierInit fields follow UIEventInit and precede MouseEventInit fields.
    let modifiers = Modifiers::from_dictionary(ctx, &dictionary)?;
    let button = dictionary_signed(ctx, &dictionary, "button", 0, 16)? as i16;
    let buttons = dictionary_unsigned(ctx, &dictionary, "buttons", 0, 16)? as u16;
    let client_x = dictionary_signed(ctx, &dictionary, "clientX", 0, 32)?;
    let client_y = dictionary_signed(ctx, &dictionary, "clientY", 0, 32)?;
    let movement_x = dictionary_double(ctx, &dictionary, "movementX", 0.0)?;
    let movement_y = dictionary_double(ctx, &dictionary, "movementY", 0.0)?;
    let related_target = dictionary_event_target(ctx, &dictionary, "relatedTarget")?;
    let screen_x = dictionary_signed(ctx, &dictionary, "screenX", 0, 32)?;
    let screen_y = dictionary_signed(ctx, &dictionary, "screenY", 0, 32)?;
    base.base.set_related_target(related_target.clone());
    base.base.set_movement(movement_x, movement_y);
    Ok(DomMouseEvent {
        base,
        coordinate_realm: crate::window_globals::current_dom_realm(ctx)
            .map_or_else(std::rc::Weak::new, |realm| Rc::downgrade(&realm)),
        modifiers: Cell::new(modifiers),
        screen_x: Cell::new(screen_x),
        screen_y: Cell::new(screen_y),
        client_x: Cell::new(client_x),
        client_y: Cell::new(client_y),
        button: Cell::new(button),
        buttons: Cell::new(buttons),
        related_target: RefCell::new(related_target),
    })
}

/// Construct the native PointerEvent used by `HTMLElement.click()`. Keeping this
/// constructor beside the UI event implementation preserves its real brand
/// and shared Event state while allowing click activation to use the normal
/// EventTarget dispatch path.
pub(crate) fn synthetic_click_event(ctx: &mut Ctx, view: Value) -> OpResult<Value> {
    let options = ctx.new_object_with_proto(&Value::Null);
    for (name, value) in [
        ("bubbles", Value::Bool(true)),
        ("cancelable", Value::Bool(true)),
        ("composed", Value::Bool(true)),
        ("view", view),
    ] {
        ctx.member_set(&options, name, value)
            .map_err(OpError::thrown)?;
    }
    pointer_event(ctx, "click", Some(options))?
        .into_instance(ctx)
        .map_err(OpError::thrown)
}

/// Construct a trusted mouse PointerEvent for the browser's bounded pointer
/// interaction path. The event is dispatched through `dispatch_user_agent_event`
/// by the caller; script-created PointerEvents continue to use the constructors
/// above and remain untrusted.
pub(crate) fn user_agent_pointer_event(
    ctx: &mut Ctx,
    kind: &str,
    view: Value,
    client_x: f64,
    client_y: f64,
    button: i16,
    buttons: u16,
    detail: i32,
    pressure: f64,
    bubbles: bool,
    cancelable: bool,
) -> OpResult<Value> {
    user_agent_pointer_event_with_state(
        ctx,
        kind,
        view,
        client_x,
        client_y,
        button,
        buttons,
        detail,
        pressure,
        bubbles,
        cancelable,
        Value::Null,
        UserAgentModifiers::default(),
        0.0,
        0.0,
    )
}

#[derive(Clone, Copy, Default)]
pub(crate) struct UserAgentModifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct UserAgentPointerProperties {
    pub pointer_id: i32,
    pub pointer_type: &'static str,
    pub is_primary: bool,
    pub width: f64,
    pub height: f64,
    pub tangential_pressure: f64,
    pub tilt_x: i32,
    pub tilt_y: i32,
    pub twist: i32,
}

impl Default for UserAgentPointerProperties {
    fn default() -> Self {
        Self {
            pointer_id: 1,
            pointer_type: "mouse",
            is_primary: true,
            width: 1.0,
            height: 1.0,
            tangential_pressure: 0.0,
            tilt_x: 0,
            tilt_y: 0,
            twist: 0,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn user_agent_pointer_event_with_state(
    ctx: &mut Ctx,
    kind: &str,
    view: Value,
    client_x: f64,
    client_y: f64,
    button: i16,
    buttons: u16,
    detail: i32,
    pressure: f64,
    bubbles: bool,
    cancelable: bool,
    related_target: Value,
    modifiers: UserAgentModifiers,
    movement_x: f64,
    movement_y: f64,
) -> OpResult<Value> {
    user_agent_pointer_event_with_properties(
        ctx,
        kind,
        view,
        client_x,
        client_y,
        button,
        buttons,
        detail,
        pressure,
        bubbles,
        cancelable,
        related_target,
        modifiers,
        movement_x,
        movement_y,
        UserAgentPointerProperties::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn user_agent_pointer_event_with_properties(
    ctx: &mut Ctx,
    kind: &str,
    view: Value,
    client_x: f64,
    client_y: f64,
    button: i16,
    buttons: u16,
    detail: i32,
    pressure: f64,
    bubbles: bool,
    cancelable: bool,
    related_target: Value,
    modifiers: UserAgentModifiers,
    movement_x: f64,
    movement_y: f64,
    properties: UserAgentPointerProperties,
) -> OpResult<Value> {
    let options = ctx.new_object_with_proto(&Value::Null);
    for (name, value) in [
        ("bubbles", Value::Bool(bubbles)),
        ("cancelable", Value::Bool(cancelable)),
        ("composed", Value::Bool(true)),
        ("view", view),
        ("detail", Value::Num(f64::from(detail))),
        ("clientX", Value::Num(client_x)),
        ("clientY", Value::Num(client_y)),
        ("button", Value::Num(f64::from(button))),
        ("buttons", Value::Num(f64::from(buttons))),
        ("pointerId", Value::Num(f64::from(properties.pointer_id))),
        ("pointerType", Value::str(properties.pointer_type)),
        ("isPrimary", Value::Bool(properties.is_primary)),
        ("width", Value::Num(properties.width)),
        ("height", Value::Num(properties.height)),
        ("pressure", Value::Num(pressure)),
        (
            "tangentialPressure",
            Value::Num(properties.tangential_pressure),
        ),
        ("tiltX", Value::Num(f64::from(properties.tilt_x))),
        ("tiltY", Value::Num(f64::from(properties.tilt_y))),
        ("twist", Value::Num(f64::from(properties.twist))),
        ("relatedTarget", related_target),
        ("ctrlKey", Value::Bool(modifiers.ctrl)),
        ("shiftKey", Value::Bool(modifiers.shift)),
        ("altKey", Value::Bool(modifiers.alt)),
        ("metaKey", Value::Bool(modifiers.meta)),
    ] {
        ctx.member_set(&options, name, value)
            .map_err(OpError::thrown)?;
    }
    let value = pointer_event(ctx, kind, Some(options))?
        .into_instance(ctx)
        .map_err(OpError::thrown)?;
    let event = ctx
        .with_instance::<DomEvent, _>(&value, Clone::clone)
        .map_err(|_| OpError::type_error("native PointerEvent lost its Event base"))?;
    event.set_movement(movement_x, movement_y);
    Ok(value)
}

/// Build the native PointerEvent or MouseEvent for a generic host dispatch of a
/// `pointer*` or `mouse*` event type, reading host fields from `options`.
pub(crate) fn host_pointer_or_mouse_event(
    ctx: &mut Ctx,
    kind: &str,
    options: Value,
) -> OpResult<Option<Value>> {
    if kind.starts_with("pointer") {
        pointer_event(ctx, kind, Some(options))?
            .into_instance(ctx)
            .map(Some)
            .map_err(OpError::thrown)
    } else if kind.starts_with("mouse") {
        let event = mouse_event(ctx, kind, Some(options))?;
        Ok(Some(ctx.new_instance(event)))
    } else {
        Ok(None)
    }
}

/// Construct the compatibility MouseEvent associated with a trusted mouse
/// pointer sequence. Trust is assigned only by the user-agent dispatch path.
pub(crate) fn user_agent_mouse_event(
    ctx: &mut Ctx,
    kind: &str,
    view: Value,
    client_x: f64,
    client_y: f64,
    button: i16,
    buttons: u16,
    detail: i32,
    bubbles: bool,
    cancelable: bool,
) -> OpResult<Value> {
    user_agent_mouse_event_with_state(
        ctx,
        kind,
        view,
        client_x,
        client_y,
        button,
        buttons,
        detail,
        bubbles,
        cancelable,
        Value::Null,
        UserAgentModifiers::default(),
        0.0,
        0.0,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn user_agent_mouse_event_with_state(
    ctx: &mut Ctx,
    kind: &str,
    view: Value,
    client_x: f64,
    client_y: f64,
    button: i16,
    buttons: u16,
    detail: i32,
    bubbles: bool,
    cancelable: bool,
    related_target: Value,
    modifiers: UserAgentModifiers,
    movement_x: f64,
    movement_y: f64,
) -> OpResult<Value> {
    let options = ctx.new_object_with_proto(&Value::Null);
    for (name, value) in [
        ("bubbles", Value::Bool(bubbles)),
        ("cancelable", Value::Bool(cancelable)),
        ("composed", Value::Bool(true)),
        ("view", view),
        ("detail", Value::Num(f64::from(detail))),
        ("clientX", Value::Num(client_x)),
        ("clientY", Value::Num(client_y)),
        ("button", Value::Num(f64::from(button))),
        ("buttons", Value::Num(f64::from(buttons))),
        ("relatedTarget", related_target),
        ("ctrlKey", Value::Bool(modifiers.ctrl)),
        ("shiftKey", Value::Bool(modifiers.shift)),
        ("altKey", Value::Bool(modifiers.alt)),
        ("metaKey", Value::Bool(modifiers.meta)),
    ] {
        ctx.member_set(&options, name, value)
            .map_err(OpError::thrown)?;
    }
    let event = mouse_event(ctx, kind, Some(options))?;
    let value = ctx.new_instance(event);
    let event = ctx
        .with_instance::<DomEvent, _>(&value, Clone::clone)
        .map_err(|_| OpError::type_error("native MouseEvent lost its Event base"))?;
    event.set_movement(movement_x, movement_y);
    Ok(value)
}

/// Construct the real trusted WheelEvent used by WebDriver's wheel source.
/// WebDriver deltas are CSS pixels, so the native event always uses pixel mode.
pub(crate) fn user_agent_wheel_event(
    ctx: &mut Ctx,
    view: Value,
    client_x: f64,
    client_y: f64,
    delta_x: f64,
    delta_y: f64,
    modifiers: UserAgentModifiers,
) -> OpResult<Value> {
    let options = ctx.new_object_with_proto(&Value::Null);
    for (name, value) in [
        ("bubbles", Value::Bool(true)),
        ("cancelable", Value::Bool(true)),
        ("composed", Value::Bool(true)),
        ("view", view),
        ("clientX", Value::Num(client_x)),
        ("clientY", Value::Num(client_y)),
        ("ctrlKey", Value::Bool(modifiers.ctrl)),
        ("shiftKey", Value::Bool(modifiers.shift)),
        ("altKey", Value::Bool(modifiers.alt)),
        ("metaKey", Value::Bool(modifiers.meta)),
    ] {
        ctx.member_set(&options, name, value)
            .map_err(OpError::thrown)?;
    }
    let base = mouse_event(ctx, "wheel", Some(options))?;
    Ok(ctx.new_instance(DomWheelEvent {
        base,
        delta_x: Cell::new(delta_x),
        delta_y: Cell::new(delta_y),
        delta_z: Cell::new(0.0),
        delta_mode: Cell::new(0),
        momentum: Cell::new(false),
    }))
}

#[lumen_bind::methods]
impl DomMouseEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Self> {
        mouse_event(ctx, kind, options)
    }

    #[getter]
    fn screen_x(&self) -> i32 {
        self.screen_x.get()
    }
    #[getter]
    fn screen_y(&self) -> i32 {
        self.screen_y.get()
    }
    #[getter]
    fn client_x(&self) -> i32 {
        self.client_x.get()
    }
    #[getter]
    fn client_y(&self) -> i32 {
        self.client_y.get()
    }
    #[getter]
    fn x(&self) -> i32 {
        self.client_x.get()
    }
    #[getter]
    fn y(&self) -> i32 {
        self.client_y.get()
    }
    #[getter]
    fn page_x(&self) -> f64 {
        self.page_coordinates().0
    }
    #[getter]
    fn page_y(&self) -> f64 {
        self.page_coordinates().1
    }
    #[getter]
    fn offset_x(&self, ctx: &mut Ctx) -> OpResult<f64> {
        Ok(self.offset_coordinates(ctx)?.0)
    }
    #[getter]
    fn offset_y(&self, ctx: &mut Ctx) -> OpResult<f64> {
        Ok(self.offset_coordinates(ctx)?.1)
    }
    #[getter]
    fn ctrl_key(&self) -> bool {
        self.modifiers.get().control
    }
    #[getter]
    fn shift_key(&self) -> bool {
        self.modifiers.get().shift
    }
    #[getter]
    fn alt_key(&self) -> bool {
        self.modifiers.get().alt
    }
    #[getter]
    fn meta_key(&self) -> bool {
        self.modifiers.get().meta
    }
    #[getter]
    fn button(&self) -> i16 {
        self.button.get()
    }
    #[getter]
    fn buttons(&self) -> u16 {
        self.buttons.get()
    }
    #[getter]
    fn related_target(&self) -> Value {
        self.related_target.borrow().clone()
    }

    #[method(coerce)]
    fn get_modifier_state(&self, key: &str) -> bool {
        self.modifiers.get().get(key)
    }

    #[getter(name = "movementX")]
    fn movement_x(&self) -> f64 {
        self.base.base.movement().0
    }

    #[getter(name = "movementY")]
    fn movement_y(&self) -> f64 {
        self.base.base.movement().1
    }

    #[method(coerce)]
    fn init_mouse_event(
        &self,
        ctx: &mut Ctx,
        kind: &str,
        #[default(false)] bubbles: bool,
        #[default(false)] cancelable: bool,
        #[default(Value::Null)] view: Value,
        #[default(0)] detail: i32,
        #[default(0)] screen_x: i32,
        #[default(0)] screen_y: i32,
        #[default(0)] client_x: i32,
        #[default(0)] client_y: i32,
        #[default(false)] ctrl_key: bool,
        #[default(false)] alt_key: bool,
        #[default(false)] shift_key: bool,
        #[default(false)] meta_key: bool,
        #[default(0)] button: i16,
        #[default(Value::Null)] related_target: Value,
    ) -> OpResult<()> {
        let view = nullable_window(ctx, view)?;
        let related_target = nullable_event_target(ctx, related_target)?;
        if !apply_legacy_ui(&self.base, kind, bubbles, cancelable, view, detail) {
            return Ok(());
        }
        self.screen_x.set(screen_x);
        self.screen_y.set(screen_y);
        self.client_x.set(client_x);
        self.client_y.set(client_y);
        self.button.set(button);
        self.buttons.set(0);
        self.modifiers.set(Modifiers {
            control: ctrl_key,
            alt: alt_key,
            shift: shift_key,
            meta: meta_key,
            ..Modifiers::default()
        });
        *self.related_target.borrow_mut() = related_target.clone();
        self.base.base.set_related_target(related_target);
        Ok(())
    }
}

impl DomMouseEvent {
    fn page_coordinates(&self) -> (f64, f64) {
        let scroll = self.coordinate_realm.upgrade().map_or((0.0, 0.0), |realm| {
            let session = realm.session.borrow();
            session.scroll_offset(session.document().root())
        });
        (
            f64::from(self.client_x.get()) + f64::from(scroll.0),
            f64::from(self.client_y.get()) + f64::from(scroll.1),
        )
    }

    fn offset_coordinates(&self, ctx: &mut Ctx) -> OpResult<(f64, f64)> {
        let Some(target) = self.base.base.active_dispatch_target() else {
            return Ok(self.page_coordinates());
        };
        let Some((realm, node)) = ctx
            .with_instance::<DomNode, _>(&target, |node| (node.realm.clone(), node.id))
            .ok()
        else {
            return Ok(self.page_coordinates());
        };
        realm.flush_layout()?;
        let origin = geometry::event_padding_origin(&mut realm.session.borrow_mut(), node);
        Ok(origin.map_or_else(
            || self.page_coordinates(),
            |(x, y)| {
                (
                    f64::from(self.client_x.get()) - f64::from(x),
                    f64::from(self.client_y.get()) - f64::from(y),
                )
            },
        ))
    }
}

fn pointer_optional_double(
    ctx: &mut Ctx,
    options: &Option<Value>,
    name: &str,
) -> OpResult<Option<f64>> {
    let Some(value) = dictionary_member(ctx, options, name)? else {
        return Ok(None);
    };
    let number = ctx.coerce_number(&value).map_err(OpError::thrown)?;
    if !number.is_finite() {
        return Err(OpError::type_error(
            "PointerEvent floating-point values must be finite",
        ));
    }
    Ok(Some(number))
}

fn pointer_optional_long(
    ctx: &mut Ctx,
    options: &Option<Value>,
    name: &str,
) -> OpResult<Option<i32>> {
    let Some(value) = dictionary_member(ctx, options, name)? else {
        return Ok(None);
    };
    Ok(Some(number_to_signed(
        ctx.coerce_number(&value).map_err(OpError::thrown)?,
        32,
    )))
}

fn pointer_sequence(ctx: &mut Ctx, options: &Option<Value>, name: &str) -> OpResult<Vec<Value>> {
    let Some(value) = dictionary_member(ctx, options, name)? else {
        return Ok(Vec::new());
    };
    ctx.convert_iterable(&value, usize::MAX, |ctx, event| {
        ctx.with_instance::<DomPointerEvent, _>(&event, |_| event.clone())
            .map_err(|_| OpError::type_error("PointerEvent sequence contains a non-PointerEvent"))
    })
}

fn input_target_ranges(ctx: &mut Ctx, options: &Option<Value>) -> OpResult<Vec<Value>> {
    let Some(value) = dictionary_member(ctx, options, "targetRanges")? else {
        return Ok(Vec::new());
    };
    // Event initialization snapshots a Web IDL sequence of genuine
    // StaticRange objects, preserving those exact object identities.
    ctx.convert_iterable(&value, 65_536, |ctx, range| {
        ctx.with_instance::<crate::range::DomStaticRange, _>(&range, |_| range.clone())
            .map_err(|_| OpError::type_error("targetRanges must contain StaticRange objects"))
    })
}

fn pointer_event(
    ctx: &mut Ctx,
    kind: &str,
    options: Option<Value>,
) -> OpResult<PointerEventConstructor> {
    let base = mouse_event(ctx, kind, options.clone())?;
    // Read this dictionary layer in Web IDL member order, after its parents.
    let altitude = pointer_optional_double(ctx, &options, "altitudeAngle")?;
    let azimuth = pointer_optional_double(ctx, &options, "azimuthAngle")?;
    let coalesced = pointer_sequence(ctx, &options, "coalescedEvents")?;
    let height = pointer_optional_double(ctx, &options, "height")?.unwrap_or(1.0);
    let is_primary = dictionary_boolean(ctx, &options, "isPrimary", false)?;
    let persistent_device_id = dictionary_signed(ctx, &options, "persistentDeviceId", 0, 32)?;
    let pointer_id = dictionary_signed(ctx, &options, "pointerId", 0, 32)?;
    let pointer_type = dictionary_string(ctx, &options, "pointerType", "", false)?;
    let predicted = pointer_sequence(ctx, &options, "predictedEvents")?;
    let pressure = pointer_optional_double(ctx, &options, "pressure")?.unwrap_or(0.0) as f32;
    let tangential_pressure =
        pointer_optional_double(ctx, &options, "tangentialPressure")?.unwrap_or(0.0) as f32;
    if !pressure.is_finite() || !tangential_pressure.is_finite() {
        return Err(OpError::type_error("PointerEvent float values overflowed"));
    }
    let tilt_x = pointer_optional_long(ctx, &options, "tiltX")?;
    let tilt_y = pointer_optional_long(ctx, &options, "tiltY")?;
    let twist = dictionary_signed(ctx, &options, "twist", 0, 32)?;
    let width = pointer_optional_double(ctx, &options, "width")?.unwrap_or(1.0);
    let coalesced_slot = (!coalesced.is_empty()).then(|| ctx.allocate_native_private_slot_name());
    let predicted_slot = (!predicted.is_empty()).then(|| ctx.allocate_native_private_slot_name());
    let event = DomPointerEvent {
        base,
        pointer_id,
        width,
        height,
        pressure,
        tangential_pressure,
        orientation: lumen_common::pointer::Orientation::from_components(
            tilt_x, tilt_y, altitude, azimuth,
        ),
        twist,
        pointer_type,
        is_primary,
        persistent_device_id,
        coalesced_slot,
        predicted_slot,
    };
    Ok(PointerEventConstructor {
        event,
        coalesced,
        predicted,
    })
}

struct PointerEventConstructor {
    event: DomPointerEvent,
    coalesced: Vec<Value>,
    predicted: Vec<Value>,
}

impl PointerEventConstructor {
    fn into_instance(self, ctx: &mut Ctx) -> Result<Value, Value> {
        let Self {
            event,
            coalesced,
            predicted,
        } = self;
        let coalesced_slot = event.coalesced_slot.clone();
        let predicted_slot = event.predicted_slot.clone();
        let instance = ctx.new_instance(event);
        if let Some(slot) = coalesced_slot {
            ctx.define_native_private_array_slot(&instance, &slot, coalesced)?;
        }
        if let Some(slot) = predicted_slot {
            ctx.define_native_private_array_slot(&instance, &slot, predicted)?;
        }
        Ok(instance)
    }
}

impl CtorRet<JsHost, DomPointerEvent> for PointerEventConstructor {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        let coalesced_slot = self.event.coalesced_slot.clone();
        let predicted_slot = self.event.predicted_slot.clone();
        let Self {
            event,
            coalesced,
            predicted,
        } = self;
        let instance = <JsHost as Host>::construct(cx, event)?;
        <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
            if let Some(slot) = coalesced_slot {
                ctx.define_native_private_array_slot(&instance, &slot, coalesced)?;
            }
            if let Some(slot) = predicted_slot {
                ctx.define_native_private_array_slot(&instance, &slot, predicted)?;
            }
            Ok(())
        })?;
        Ok(instance)
    }
}

#[lumen_bind::class(name = "PointerEvent", extends = DomMouseEvent, hint(js(webidl)))]
pub(crate) struct DomPointerEvent {
    base: DomMouseEvent,
    pointer_id: i32,
    width: f64,
    height: f64,
    pressure: f32,
    tangential_pressure: f32,
    orientation: lumen_common::pointer::Orientation,
    twist: i32,
    pointer_type: String,
    is_primary: bool,
    persistent_device_id: i32,
    coalesced_slot: Option<String>,
    predicted_slot: Option<String>,
}

#[lumen_bind::methods]
impl DomPointerEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<PointerEventConstructor> {
        pointer_event(ctx, kind, options)
    }
    #[getter]
    fn pointer_id(&self) -> i32 {
        self.pointer_id
    }
    #[getter]
    fn width(&self) -> f64 {
        self.width
    }
    #[getter]
    fn height(&self) -> f64 {
        self.height
    }
    #[getter]
    fn pressure(&self) -> f64 {
        f64::from(self.pressure)
    }
    #[getter]
    fn tangential_pressure(&self) -> f64 {
        f64::from(self.tangential_pressure)
    }
    #[getter]
    fn tilt_x(&self) -> i32 {
        self.orientation.tilt_x
    }
    #[getter]
    fn tilt_y(&self) -> i32 {
        self.orientation.tilt_y
    }
    #[getter]
    fn altitude_angle(&self) -> f64 {
        self.orientation.altitude
    }
    #[getter]
    fn azimuth_angle(&self) -> f64 {
        self.orientation.azimuth
    }
    #[getter]
    fn twist(&self) -> i32 {
        self.twist
    }
    #[getter]
    fn pointer_type(&self) -> &str {
        &self.pointer_type
    }
    #[getter]
    fn is_primary(&self) -> bool {
        self.is_primary
    }
    #[getter]
    fn persistent_device_id(&self) -> i32 {
        self.persistent_device_id
    }
    fn get_coalesced_events(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        let values = self
            .coalesced_slot
            .as_deref()
            .and_then(|slot| ctx.native_private_array_slot(&this.0, slot))
            .unwrap_or_default();
        JsHost::from_list(ctx, values)
    }

    fn get_predicted_events(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        let values = self
            .predicted_slot
            .as_deref()
            .and_then(|slot| ctx.native_private_array_slot(&this.0, slot))
            .unwrap_or_default();
        JsHost::from_list(ctx, values)
    }
}

#[lumen_bind::class(name = "WheelEvent", extends = DomMouseEvent, hint(js(webidl)))]
pub(crate) struct DomWheelEvent {
    base: DomMouseEvent,
    delta_x: Cell<f64>,
    delta_y: Cell<f64>,
    delta_z: Cell<f64>,
    delta_mode: Cell<u32>,
    momentum: Cell<bool>,
}

#[lumen_bind::methods]
impl DomWheelEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Self> {
        let dictionary = options.clone();
        let base = mouse_event(ctx, kind, options)?;
        let delta_mode = dictionary_unsigned(ctx, &dictionary, "deltaMode", 0, 32)?;
        let delta_x = dictionary_double(ctx, &dictionary, "deltaX", 0.0)?;
        let delta_y = dictionary_double(ctx, &dictionary, "deltaY", 0.0)?;
        let delta_z = dictionary_double(ctx, &dictionary, "deltaZ", 0.0)?;
        let momentum = dictionary_boolean(ctx, &dictionary, "momentum", false)?;
        Ok(Self {
            base,
            delta_x: Cell::new(delta_x),
            delta_y: Cell::new(delta_y),
            delta_z: Cell::new(delta_z),
            delta_mode: Cell::new(delta_mode),
            momentum: Cell::new(momentum),
        })
    }

    #[getter]
    fn delta_x(&self) -> f64 {
        self.delta_x.get()
    }
    #[getter]
    fn delta_y(&self) -> f64 {
        self.delta_y.get()
    }
    #[getter]
    fn delta_z(&self) -> f64 {
        self.delta_z.get()
    }
    #[getter]
    fn delta_mode(&self) -> u32 {
        self.delta_mode.get()
    }
    #[getter]
    fn momentum(&self) -> bool {
        self.momentum.get()
    }
}

#[lumen_bind::class(name = "KeyboardEvent", extends = DomUIEvent, hint(js(webidl)))]
pub(crate) struct DomKeyboardEvent {
    base: DomUIEvent,
    key: RefCell<String>,
    code: RefCell<String>,
    location: Cell<u32>,
    modifiers: Cell<Modifiers>,
    repeat: Cell<bool>,
    is_composing: Cell<bool>,
    char_code: Cell<u32>,
    key_code: Cell<u32>,
}

#[lumen_bind::methods]
impl DomKeyboardEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Self> {
        let dictionary = options.clone();
        let base = ui_event(ctx, kind, options)?;
        let modifiers = Modifiers::from_dictionary(ctx, &dictionary)?;
        let char_code = dictionary_unsigned(ctx, &dictionary, "charCode", 0, 32)?;
        let code = dictionary_string(ctx, &dictionary, "code", "", false)?;
        let is_composing = dictionary_boolean(ctx, &dictionary, "isComposing", false)?;
        let key = dictionary_string(ctx, &dictionary, "key", "", false)?;
        let key_code = dictionary_unsigned(ctx, &dictionary, "keyCode", 0, 32)?;
        let location = dictionary_unsigned(ctx, &dictionary, "location", 0, 32)?;
        let repeat = dictionary_boolean(ctx, &dictionary, "repeat", false)?;
        Ok(Self {
            base,
            key: RefCell::new(key),
            code: RefCell::new(code),
            location: Cell::new(location),
            modifiers: Cell::new(modifiers),
            repeat: Cell::new(repeat),
            is_composing: Cell::new(is_composing),
            char_code: Cell::new(char_code),
            key_code: Cell::new(key_code),
        })
    }

    #[getter]
    fn key(&self) -> String {
        self.key.borrow().clone()
    }
    #[getter]
    fn code(&self) -> String {
        self.code.borrow().clone()
    }
    #[getter]
    fn location(&self) -> u32 {
        self.location.get()
    }
    #[getter]
    fn ctrl_key(&self) -> bool {
        self.modifiers.get().control
    }
    #[getter]
    fn shift_key(&self) -> bool {
        self.modifiers.get().shift
    }
    #[getter]
    fn alt_key(&self) -> bool {
        self.modifiers.get().alt
    }
    #[getter]
    fn meta_key(&self) -> bool {
        self.modifiers.get().meta
    }
    #[getter]
    fn repeat(&self) -> bool {
        self.repeat.get()
    }
    #[getter]
    fn is_composing(&self) -> bool {
        self.is_composing.get()
    }
    #[getter]
    fn char_code(&self) -> u32 {
        self.char_code.get()
    }
    #[getter]
    fn key_code(&self) -> u32 {
        self.key_code.get()
    }

    #[method(coerce)]
    fn get_modifier_state(&self, key: &str) -> bool {
        self.modifiers.get().get(key)
    }

    #[method(coerce)]
    fn init_keyboard_event(
        &self,
        ctx: &mut Ctx,
        kind: &str,
        #[default(false)] bubbles: bool,
        #[default(false)] cancelable: bool,
        #[default(Value::Null)] view: Value,
        #[default("")] key: &str,
        #[default(0)] location: u32,
        #[default(false)] ctrl_key: bool,
        #[default(false)] alt_key: bool,
        #[default(false)] shift_key: bool,
        #[default(false)] meta_key: bool,
    ) -> OpResult<()> {
        if !legacy_ui(ctx, &self.base, kind, bubbles, cancelable, view, 0)? {
            return Ok(());
        }
        *self.key.borrow_mut() = key.to_owned();
        self.location.set(location);
        self.modifiers.set(Modifiers {
            control: ctrl_key,
            alt: alt_key,
            shift: shift_key,
            meta: meta_key,
            ..Modifiers::default()
        });
        Ok(())
    }
}

/// Build the native KeyboardEvent used by host-driven key input. The owner
/// supplies the already initialized Web IDL dictionary; construction stays
/// here so automation uses the same brand and KeyboardEvent state as script.
pub(crate) fn user_agent_keyboard_event(
    ctx: &mut Ctx,
    kind: &str,
    options: Value,
) -> OpResult<Value> {
    let event = DomKeyboardEvent::new(ctx, kind, Some(options))?;
    Ok(ctx.new_instance(event))
}

/// Construct the native InputEvent used by editing algorithms. The caller
/// dispatches it through the normal target path, which assigns trust without
/// changing its native InputEvent state.
pub(crate) fn input_event(ctx: &mut Ctx, kind: &str, options: Value) -> OpResult<Value> {
    DomInputEvent::new(ctx, kind, Some(options))?
        .into_instance(ctx)
        .map_err(OpError::thrown)
}

/// Install Web IDL interface constants on both interface objects and their prototypes. The
/// caller owns class registration; this helper is deliberately separate so constructors remain
/// native classes and constant installation never invokes page-modified setters.
pub(crate) fn install_ui_event_constants(
    ctx: &mut Ctx,
    event_constructor: &Value,
    keyboard_constructor: &Value,
    wheel_constructor: &Value,
) -> Result<(), Value> {
    let event_constants = [
        ("NONE", 0),
        ("CAPTURING_PHASE", 1),
        ("AT_TARGET", 2),
        ("BUBBLING_PHASE", 3),
    ];
    let keyboard_constants = [
        ("DOM_KEY_LOCATION_STANDARD", 0),
        ("DOM_KEY_LOCATION_LEFT", 1),
        ("DOM_KEY_LOCATION_RIGHT", 2),
        ("DOM_KEY_LOCATION_NUMPAD", 3),
    ];
    let wheel_constants = [
        ("DOM_DELTA_PIXEL", 0),
        ("DOM_DELTA_LINE", 1),
        ("DOM_DELTA_PAGE", 2),
    ];
    for (constructor, constants) in [
        (event_constructor, event_constants.as_slice()),
        (keyboard_constructor, keyboard_constants.as_slice()),
        (wheel_constructor, wheel_constants.as_slice()),
    ] {
        let prototype = ctx.member_get(constructor, "prototype")?;
        for (name, number) in constants {
            let descriptor = ctx.new_object_with_proto(&Value::Null);
            for (key, value) in [
                ("value", Value::Num(*number as f64)),
                ("writable", Value::Bool(false)),
                ("enumerable", Value::Bool(true)),
                ("configurable", Value::Bool(false)),
            ] {
                ctx.member_set(&descriptor, key, value)?;
            }
            for target in [constructor, &prototype] {
                ctx.define_property_value(target, Value::str(*name), &descriptor)?;
            }
        }
    }
    Ok(())
}

#[lumen_bind::class(name = "CompositionEvent", extends = DomUIEvent, hint(js(webidl)))]
pub(crate) struct DomCompositionEvent {
    base: DomUIEvent,
    data: RefCell<String>,
}

#[lumen_bind::methods]
impl DomCompositionEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Self> {
        let dictionary = options.clone();
        let base = ui_event(ctx, kind, options)?;
        let data = dictionary_string(ctx, &dictionary, "data", "", true)?;
        Ok(Self {
            base,
            data: RefCell::new(data),
        })
    }

    #[getter]
    fn data(&self) -> String {
        self.data.borrow().clone()
    }

    #[method(coerce)]
    fn init_composition_event(
        &self,
        ctx: &mut Ctx,
        kind: &str,
        #[default(false)] bubbles: bool,
        #[default(false)] cancelable: bool,
        #[default(Value::Null)] view: Value,
        #[default("")] data: &str,
    ) -> OpResult<()> {
        let data = lumen::well_formed_utf8(data).into_owned();
        if legacy_ui(ctx, &self.base, kind, bubbles, cancelable, view, 0)? {
            *self.data.borrow_mut() = data;
        }
        Ok(())
    }
}

#[lumen_bind::class(name = "InputEvent", extends = DomUIEvent, hint(js(webidl)))]
pub(crate) struct DomInputEvent {
    base: DomUIEvent,
    data: RefCell<Option<String>>,
    is_composing: Cell<bool>,
    input_type: RefCell<String>,
    target_ranges_slot: Option<String>,
}

struct InputEventConstructor {
    event: DomInputEvent,
    target_ranges: Vec<Value>,
}

impl InputEventConstructor {
    fn into_instance(self, ctx: &mut Ctx) -> Result<Value, Value> {
        let Self {
            event,
            target_ranges,
        } = self;
        let slot = event.target_ranges_slot.clone();
        let instance = ctx.new_instance(event);
        if let Some(slot) = slot {
            ctx.define_native_private_array_slot(&instance, &slot, target_ranges)?;
        }
        Ok(instance)
    }
}

impl CtorRet<JsHost, DomInputEvent> for InputEventConstructor {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        let slot = self.event.target_ranges_slot.clone();
        let Self {
            event,
            target_ranges,
        } = self;
        let instance = <JsHost as Host>::construct(cx, event)?;
        <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
            if let Some(slot) = slot {
                ctx.define_native_private_array_slot(&instance, &slot, target_ranges)?;
            }
            Ok(())
        })?;
        Ok(instance)
    }
}

#[lumen_bind::methods]
impl DomInputEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<InputEventConstructor> {
        let dictionary = options.clone();
        let base = ui_event(ctx, kind, options)?;
        // InputEventInit declares a nullable DOMString data member, while the exposed attribute is
        // a nullable USVString.
        let data = match dictionary_member(ctx, &dictionary, "data")? {
            None | Some(Value::Null) => None,
            Some(value) => {
                let value = ctx.coerce_string(&value).map_err(OpError::thrown)?;
                Some(lumen::well_formed_utf8(&value).into_owned())
            }
        };
        let input_type = dictionary_string(ctx, &dictionary, "inputType", "", false)?;
        let is_composing = dictionary_boolean(ctx, &dictionary, "isComposing", false)?;
        let target_ranges = input_target_ranges(ctx, &dictionary)?;
        let target_ranges_slot =
            (!target_ranges.is_empty()).then(|| ctx.allocate_native_private_slot_name());
        Ok(InputEventConstructor {
            event: Self {
                base,
                data: RefCell::new(data),
                is_composing: Cell::new(is_composing),
                input_type: RefCell::new(input_type),
                target_ranges_slot,
            },
            target_ranges,
        })
    }

    #[getter]
    fn data(&self) -> Option<String> {
        self.data.borrow().clone()
    }
    #[getter]
    fn is_composing(&self) -> bool {
        self.is_composing.get()
    }
    #[getter]
    fn input_type(&self) -> String {
        self.input_type.borrow().clone()
    }

    fn get_target_ranges(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        let values = self
            .target_ranges_slot
            .as_deref()
            .and_then(|slot| ctx.native_private_array_slot(&this.0, slot))
            .unwrap_or_default();
        JsHost::from_list(ctx, values)
    }
}

#[lumen_bind::class(name = "ErrorEvent", extends = DomEvent, hint(js(webidl)))]
pub(crate) struct DomErrorEvent {
    base: DomEvent,
    message: String,
    filename: String,
    lineno: u32,
    colno: u32,
    error: Value,
}

#[lumen_bind::methods]
impl DomErrorEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Self> {
        let dictionary = options.clone();
        let base = DomEvent::new(ctx, kind, options)?;
        let colno = dictionary_unsigned(ctx, &dictionary, "colno", 0, 32)?;
        let error = dictionary_member(ctx, &dictionary, "error")?.unwrap_or(Value::Undefined);
        let filename = dictionary_string(ctx, &dictionary, "filename", "", true)?;
        let lineno = dictionary_unsigned(ctx, &dictionary, "lineno", 0, 32)?;
        let message = dictionary_string(ctx, &dictionary, "message", "", false)?;
        Ok(Self {
            base,
            message,
            filename,
            lineno,
            colno,
            error,
        })
    }

    #[getter]
    fn message(&self) -> String {
        self.message.clone()
    }
    #[getter]
    fn filename(&self) -> String {
        self.filename.clone()
    }
    #[getter]
    fn lineno(&self) -> u32 {
        self.lineno
    }
    #[getter]
    fn colno(&self) -> u32 {
        self.colno
    }
    #[getter]
    fn error(&self) -> Value {
        self.error.clone()
    }
}

/// Snapshot the ErrorEvent fields consumed by Window.onerror's special handling algorithm.
/// This reads the native IDL attribute state rather than performing JavaScript property gets, so
/// author-defined own accessors cannot run while the platform prepares callback arguments. The
/// returned values are detached from the native borrow before script is invoked.
pub(crate) fn error_event_handler_arguments(ctx: &mut Ctx, event: &Value) -> Option<[Value; 5]> {
    ctx.with_instance::<DomErrorEvent, _>(event, |event| {
        [
            Value::str(&event.message),
            Value::str(&event.filename),
            Value::Num(event.lineno as f64),
            Value::Num(event.colno as f64),
            event.error.clone(),
        ]
    })
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;
    use lumen_runtime::Runtime;

    fn expose_test_constructors(engine: &mut Engine) {
        let global = engine.ctx().global_object();
        let constructors = [
            ("Event", engine.ctx().class_constructor::<DomEvent>()),
            ("UIEvent", engine.ctx().class_constructor::<DomUIEvent>()),
            (
                "FocusEvent",
                engine.ctx().class_constructor::<DomFocusEvent>(),
            ),
            (
                "MouseEvent",
                engine.ctx().class_constructor::<DomMouseEvent>(),
            ),
            (
                "PointerEvent",
                engine.ctx().class_constructor::<DomPointerEvent>(),
            ),
            (
                "WheelEvent",
                engine.ctx().class_constructor::<DomWheelEvent>(),
            ),
            (
                "KeyboardEvent",
                engine.ctx().class_constructor::<DomKeyboardEvent>(),
            ),
            (
                "CompositionEvent",
                engine.ctx().class_constructor::<DomCompositionEvent>(),
            ),
            (
                "InputEvent",
                engine.ctx().class_constructor::<DomInputEvent>(),
            ),
            (
                "ErrorEvent",
                engine.ctx().class_constructor::<DomErrorEvent>(),
            ),
        ];
        let keyboard = constructors
            .iter()
            .find(|(name, _)| *name == "KeyboardEvent")
            .unwrap()
            .1
            .clone();
        let wheel = constructors
            .iter()
            .find(|(name, _)| *name == "WheelEvent")
            .unwrap()
            .1
            .clone();
        let event = engine.ctx().class_constructor::<DomEvent>();
        install_ui_event_constants(engine.ctx(), &event, &keyboard, &wheel)
            .ok()
            .expect("native event constants");
        for (name, constructor) in constructors {
            engine
                .ctx()
                .set_member(&global, name, constructor)
                .ok()
                .expect("event constructor");
        }
    }

    fn boolean(engine: &mut Engine, source: &str) {
        let result = match engine
            .eval_value(source)
            .expect("valid UI event test script")
        {
            Ok(value) => value,
            Err(error) => {
                let message = engine
                    .ctx()
                    .coerce_string(&error)
                    .map(|message| message.to_string())
                    .unwrap_or_else(|_| "unknown exception".into());
                panic!("UI event test threw: {message}");
            }
        };
        assert!(
            matches!(result, Value::Bool(true)),
            "UI event assertion failed: {source}"
        );
    }

    #[test]
    fn movement_properties_belong_to_mouse_event_interfaces() {
        let mut engine = Engine::new();
        expose_test_constructors(&mut engine);
        let mouse = engine
            .eval_value("new MouseEvent('mousemove')")
            .expect("valid mouse event")
            .ok()
            .expect("construct mouse event");
        engine
            .ctx()
            .with_instance::<DomEvent, _>(&mouse, |event| event.set_movement(2.25, -0.75))
            .unwrap();
        let global = engine.ctx().global_object();
        engine
            .ctx()
            .set_member(&global, "movementEvent", mouse)
            .ok()
            .expect("expose mouse event");
        boolean(
            &mut engine,
            r#"(() => {
                for (const constructor of [Event, UIEvent, FocusEvent]) {
                    const event = new constructor('test');
                    if ('movementX' in event || 'movementY' in event) return false;
                }
                const initialized = new MouseEvent('mousemove', {movementX: '1.25', movementY: -0.5});
                for (const [constructor, dictionary] of [
                    [MouseEvent, {movementX: Infinity}],
                    [WheelEvent, {deltaX: NaN}],
                ]) {
                    try { new constructor('test', dictionary); return false; }
                    catch (error) { if (!(error instanceof TypeError)) return false; }
                }
                return initialized.movementX === 1.25 && initialized.movementY === -0.5 &&
                    movementEvent.movementX === 2.25 && movementEvent.movementY === -0.75 &&
                    Object.hasOwn(MouseEvent.prototype, 'movementX') &&
                    Object.hasOwn(MouseEvent.prototype, 'movementY') &&
                    new PointerEvent('pointermove').movementX === 0 &&
                    new WheelEvent('wheel').movementY === 0;
            })()"#,
        );
    }

    #[test]
    fn mouse_coordinates_use_dispatch_state_and_untransformed_padding_edges() {
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
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = install(engine.ctx(), "<style>body{margin:0}#target{position:absolute;left:20px;top:30px;width:10px;height:10px;border:2px solid;padding:5px;transform:translate(100px,80px)}svg{position:absolute;left:70px;top:80px}</style><div id=target></div><svg width=100 height=100><rect id=svg-target x=50 y=50 width=30 height=30></rect></svg>", 64).unwrap();
        realm.set_layout_flusher(Rc::new(|session| {
            session
                .display_list(300, 200, &NoText)
                .map(|_| ())
                .map_err(|error| format!("{error:?}"))
        }));
        expose_test_constructors(engine);
        boolean(
            engine,
            r#"(() => {
            for (const [name, value] of [['NONE',0],['CAPTURING_PHASE',1],['AT_TARGET',2],['BUBBLING_PHASE',3]]) {
                for (const object of [Event, Event.prototype]) {
                    const descriptor = Object.getOwnPropertyDescriptor(object, name);
                    if (!descriptor || descriptor.value !== value || descriptor.writable || descriptor.configurable || !descriptor.enumerable)
                        throw new Error('Event phase constant descriptor ' + name);
                }
            }
            const event = new MouseEvent('move', {clientX:37, clientY:48});
            if (event.x !== 37 || event.y !== 48 || event.pageX !== 37 || event.pageY !== 48 ||
                event.offsetX !== 37 || event.offsetY !== 48) throw new Error('undispatched coordinates');
            let dispatched = false;
            const target = document.getElementById('target');
            target.addEventListener('move', e => {
                if (e.eventPhase !== Event.AT_TARGET || e.offsetX !== 15 || e.offsetY !== 16)
                    throw new Error('offset must use untransformed padding edge during dispatch: ' + e.offsetX + ',' + e.offsetY);
                dispatched = true;
            });
            target.dispatchEvent(event);
            if (!dispatched || event.eventPhase !== Event.NONE || event.offsetX !== 37 || event.offsetY !== 48)
                throw new Error('coordinates after dispatch');
            let svgDispatched = false;
            const rect = document.getElementById('svg-target');
            rect.addEventListener('move', e => {
                if (e.offsetX !== 60 || e.offsetY !== 60)
                    throw new Error('SVG offsets must use the outer SVG CSS box');
                svgDispatched = true;
            });
            rect.dispatchEvent(new MouseEvent('move', {clientX:130,clientY:140}));
            if (!svgDispatched) throw new Error('SVG dispatch did not observe its coordinate origin');
            return true;
        })()"#,
        );
    }

    #[test]
    fn pointer_event_constructor_reuses_event_state_and_retains_typed_sequences() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        install(engine.ctx(), "<div></div>", 64).unwrap();
        expose_test_constructors(engine);
        boolean(
            engine,
            r#"(() => {
            const base = new PointerEvent('pointermove');
            if (!(base instanceof PointerEvent) || !(base instanceof MouseEvent) ||
                !(base instanceof UIEvent) || !(base instanceof Event) ||
                Object.getPrototypeOf(PointerEvent.prototype) !== MouseEvent.prototype)
                throw new Error('PointerEvent native inheritance');
            if (base.pointerId !== 0 || base.width !== 1 || base.height !== 1 ||
                base.pressure !== 0 || base.pointerType !== '' || base.isPrimary ||
                base.tiltX !== 0 || base.tiltY !== 0 || base.altitudeAngle !== Math.PI/2 ||
                base.azimuthAngle !== 0 || base.getPredictedEvents().length !== 0)
                throw new Error('PointerEvent scalar defaults');
            const tilted = new PointerEvent('pointermove', {tiltX:-45});
            if (tilted.tiltX !== -45 || tilted.tiltY !== 0 ||
                tilted.azimuthAngle !== Math.PI || tilted.altitudeAngle !== Math.PI/4)
                throw new Error('tilt to spherical orientation');
            const spherical = new PointerEvent('pointermove', {altitudeAngle:0, azimuthAngle:3*Math.PI/2});
            if (spherical.tiltX !== 0 || spherical.tiltY !== -90)
                throw new Error('spherical to tilt boundary orientation');
            const mixed = new PointerEvent('pointermove', {tiltX:45,azimuthAngle:Math.PI/4});
            if (mixed.tiltX !== 45 || mixed.tiltY !== 0 ||
                mixed.altitudeAngle !== Math.PI/2 || mixed.azimuthAngle !== Math.PI/4)
                throw new Error('explicit mixed orientation values');
            const child = new PointerEvent('pointermove', {pointerId:17,clientX:13,pointerType:'pen'});
            const input = new Set([child]);
            const event = new PointerEvent('pointermove', {coalescedEvents:input,predictedEvents:input});
            input.clear();
            const first = event.getPredictedEvents();
            const coalesced = event.getCoalescedEvents();
            if (first.length !== 1 || first[0] !== child || first === event.getPredictedEvents() ||
                coalesced.length !== 1 || coalesced[0] !== child || coalesced === event.getCoalescedEvents())
                throw new Error('typed sequence snapshot and event identity');
            class DerivedPointerEvent extends PointerEvent {}
            const derived = new DerivedPointerEvent('pointermove', {predictedEvents:[child]});
            if (!(derived instanceof DerivedPointerEvent) || !(derived instanceof PointerEvent) ||
                derived.getPredictedEvents()[0] !== child)
                throw new Error('PointerEvent constructor preserves new.target and subclass prototype');
            let closed = false;
            function* invalid() { try { yield {}; } finally { closed = true; } }
            try { new PointerEvent('x', {predictedEvents:invalid()}); throw new Error('sequence accepted a plain object'); }
            catch (error) { if (!(error instanceof TypeError)) throw error; }
            if (!closed) throw new Error('abrupt sequence conversion did not close its iterator');
            try { new PointerEvent('x', {width:NaN}); throw new Error('nonfinite width accepted'); }
            catch (error) { if (!(error instanceof TypeError)) throw error; }
            globalThis.retainedPointer = event;
            return true;
        })()"#,
        );
        engine.collect_garbage();
        boolean(
            engine,
            "retainedPointer.getPredictedEvents()[0].pointerId === 17 && retainedPointer.getPredictedEvents()[0].clientX === 13 && retainedPointer.getCoalescedEvents()[0] === retainedPointer.getPredictedEvents()[0]",
        );
    }

    #[test]
    fn error_event_handler_arguments_read_native_fields_without_js_getters() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        install(engine.ctx(), "<div></div>", 64).unwrap();
        expose_test_constructors(engine);
        let event = engine
            .eval_value(
                r#"(() => {
                    globalThis.errorEventSentinel = {identity: true};
                    const event = new ErrorEvent('error', {
                        message: 'stored message', filename: 'stored.js',
                        lineno: 17, colno: 23, error: errorEventSentinel
                    });
                    for (const key of ['message', 'filename', 'lineno', 'colno', 'error']) {
                        Object.defineProperty(event, key, {
                            configurable: true,
                            get() { throw new Error('unexpected JS property access: ' + key); }
                        });
                    }
                    return event;
                })()"#,
            )
            .expect("valid ErrorEvent setup script")
            .ok()
            .expect("setup script returned a value");
        let arguments =
            error_event_handler_arguments(engine.ctx(), &event).expect("native ErrorEvent brand");
        let global = engine.ctx().global_object();
        let sentinel = engine
            .ctx()
            .member_get(&global, "errorEventSentinel")
            .ok()
            .expect("sentinel remains accessible");
        assert!(matches!(&arguments[0], Value::Str(value) if value.as_str() == "stored message"));
        assert!(matches!(&arguments[1], Value::Str(value) if value.as_str() == "stored.js"));
        assert!(matches!(arguments[2], Value::Num(value) if value == 17.0));
        assert!(matches!(arguments[3], Value::Num(value) if value == 23.0));
        assert!(matches!(
            (&arguments[4], &sentinel),
            (Value::Obj(actual), Value::Obj(expected)) if std::ptr::eq(&**actual, &**expected)
        ));
    }

    #[test]
    fn ui_event_constructors_share_native_event_state_and_convert_webidl_fields() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        install(engine.ctx(), "<div></div>", 64).unwrap();
        expose_test_constructors(engine);
        boolean(
            engine,
            r#"(() => {
                const ui = new UIEvent('ui');
                const focusTarget = document.body;
                const focus = new FocusEvent('focus', {view: window, detail: '7', relatedTarget: focusTarget});
                const key = new KeyboardEvent('keydown', {
                    view: window, detail: 4, which: 9, key: 12, code: 'KeyA', location: -1,
                    ctrlKey: true, modifierAltGraph: true, modifierCapsLock: true,
                    repeat: true, isComposing: true, charCode: -1, keyCode: 0x1_0000_0002
                });
                const mouse = new MouseEvent('mousedown', {
                    screenX: 0x1_0000_0003, screenY: -4, clientX: '5.9', clientY: -6,
                    button: 0xFFFF, buttons: 0x1_0001, modifierSymbol: true
                });
                const wheel = new WheelEvent('wheel', {deltaX: '2.5', deltaY: -3, deltaZ: 4, deltaMode: 2, momentum: true});
                const composition = new CompositionEvent('compositionupdate', {data: '\uD800'});
                const input = new InputEvent('beforeinput', {data: 'text', isComposing: true, inputType: 'insertText'});
                const error = {sentinel: true};
                const errorEvent = new ErrorEvent('error', {message: 5, filename: 'f.js', lineno: -1, colno: 0x1_0000_0002, error});
                const absentError = new ErrorEvent('error');
                const undefinedError = new ErrorEvent('error', {error: undefined});
                const nullError = new ErrorEvent('error', {error: null});
                const malformedFilename = new ErrorEvent('error', {filename: '\uD800'});
                const keyLocationDescriptor = Object.getOwnPropertyDescriptor(KeyboardEvent, 'DOM_KEY_LOCATION_LEFT');
                let badView = false;
                try { new UIEvent('bad', {view: {}}); } catch (exception) { badView = exception instanceof TypeError; }
                let badTarget = false;
                try { new FocusEvent('bad', {relatedTarget: {}}); } catch (exception) { badTarget = exception instanceof TypeError; }
                return ui instanceof Event && ui instanceof UIEvent && ui.view === null && ui.detail === 0 && ui.which === 0 &&
                    focus instanceof UIEvent && focus.view === window && focus.detail === 7 && focus.relatedTarget === focusTarget &&
                    key.key === '12' && key.code === 'KeyA' && key.location === 0xFFFFFFFF && key.ctrlKey && key.repeat && key.isComposing &&
                    key.charCode === 0xFFFFFFFF && key.keyCode === 2 && key.getModifierState('Control') && key.getModifierState('AltGraph') && key.getModifierState('CapsLock') &&
                    !key.getModifierState('Fn') && mouse.screenX === 3 && mouse.screenY === -4 && mouse.clientX === 5 && mouse.clientY === -6 &&
                    mouse.button === -1 && mouse.buttons === 1 && mouse.getModifierState('Symbol') &&
                    wheel.deltaX === 2.5 && wheel.deltaY === -3 && wheel.deltaZ === 4 && wheel.deltaMode === 2 && wheel.momentum &&
                    composition.data.charCodeAt(0) === 0xFFFD && input.data === 'text' && input.isComposing && input.inputType === 'insertText' &&
                    errorEvent.message === '5' && errorEvent.filename === 'f.js' && errorEvent.lineno === 0xFFFFFFFF &&
                    errorEvent.colno === 2 && errorEvent.error === error && absentError.error === undefined &&
                    undefinedError.error === undefined && nullError.error === null &&
                    malformedFilename.filename.charCodeAt(0) === 0xFFFD && badView && badTarget &&
                    KeyboardEvent.DOM_KEY_LOCATION_STANDARD === 0 && KeyboardEvent.prototype.DOM_KEY_LOCATION_NUMPAD === 3 &&
                    keyLocationDescriptor.value === 1 && !keyLocationDescriptor.writable && keyLocationDescriptor.enumerable && !keyLocationDescriptor.configurable &&
                    WheelEvent.DOM_DELTA_PIXEL === 0 && WheelEvent.prototype.DOM_DELTA_PAGE === 2;
            })()"#,
        );
        boolean(
            engine,
            r#"(() => {
                const order = [];
                new MouseEvent('x', {
                    get bubbles() { order.push('bubbles'); return true; },
                    get cancelable() { order.push('cancelable'); return true; },
                    get composed() { order.push('composed'); return true; },
                    get view() { order.push('view'); return window; },
                    get detail() { order.push('detail'); return 1; },
                    get which() { order.push('which'); return 2; },
                    get ctrlKey() { order.push('ctrlKey'); return true; },
                    get shiftKey() { order.push('shiftKey'); return false; },
                    get altKey() { order.push('altKey'); return false; },
                    get metaKey() { order.push('metaKey'); return false; },
                    get modifierAltGraph() { order.push('modifierAltGraph'); return false; },
                    get modifierCapsLock() { order.push('modifierCapsLock'); return false; },
                    get modifierFn() { order.push('modifierFn'); return false; },
                    get modifierFnLock() { order.push('modifierFnLock'); return false; },
                    get modifierHyper() { order.push('modifierHyper'); return false; },
                    get modifierNumLock() { order.push('modifierNumLock'); return false; },
                    get modifierScrollLock() { order.push('modifierScrollLock'); return false; },
                    get modifierSuper() { order.push('modifierSuper'); return false; },
                    get modifierSymbol() { order.push('modifierSymbol'); return false; },
                    get modifierSymbolLock() { order.push('modifierSymbolLock'); return false; },
                    get screenX() { order.push('screenX'); return 0; },
                    get screenY() { order.push('screenY'); return 0; },
                    get clientX() { order.push('clientX'); return 0; },
                    get clientY() { order.push('clientY'); return 0; },
                    get movementX() { order.push('movementX'); return 0; },
                    get movementY() { order.push('movementY'); return 0; },
                    get button() { order.push('button'); return 0; },
                    get buttons() { order.push('buttons'); return 0; },
                    get relatedTarget() { order.push('relatedTarget'); return null; }
                });
                return order.join(',') === 'bubbles,cancelable,composed,detail,view,which,altKey,ctrlKey,metaKey,modifierAltGraph,modifierCapsLock,modifierFn,modifierFnLock,modifierHyper,modifierNumLock,modifierScrollLock,modifierSuper,modifierSymbol,modifierSymbolLock,shiftKey,button,buttons,clientX,clientY,movementX,movementY,relatedTarget,screenX,screenY';
            })()"#,
        );
    }

    #[test]
    fn event_dictionaries_follow_inherited_lexicographic_member_order() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        install(engine.ctx(), "<div></div>", 64).unwrap();
        expose_test_constructors(engine);
        boolean(
            engine,
            r#"(() => {
                function keys(Constructor) {
                    const seen = [];
                    const values = {view: null, relatedTarget: null};
                    const dictionary = new Proxy({}, {
                        get(_target, key) {
                            if (typeof key === 'string') seen.push(key);
                            return values[key];
                        }
                    });
                    new Constructor('event', dictionary);
                    return seen.join(',');
                }
                const base = 'bubbles,cancelable,composed';
                const ui = base + ',detail,view,which';
                const modifiers = ',altKey,ctrlKey,metaKey,modifierAltGraph,modifierCapsLock,modifierFn,modifierFnLock,' +
                    'modifierHyper,modifierNumLock,modifierScrollLock,modifierSuper,modifierSymbol,modifierSymbolLock,shiftKey';
                const mouse = ui + modifiers + ',button,buttons,clientX,clientY,movementX,movementY,relatedTarget,screenX,screenY';
                return keys(UIEvent) === ui &&
                    keys(FocusEvent) === ui + ',relatedTarget' &&
                    keys(MouseEvent) === mouse &&
                    keys(WheelEvent) === mouse + ',deltaMode,deltaX,deltaY,deltaZ,momentum' &&
                    keys(KeyboardEvent) === ui + modifiers + ',charCode,code,isComposing,key,keyCode,location,repeat' &&
                    keys(CompositionEvent) === ui + ',data' &&
                    keys(InputEvent) === ui + ',data,inputType,isComposing,targetRanges' &&
                    keys(ErrorEvent) === base + ',colno,error,filename,lineno,message';
            })()"#,
        );
    }

    #[test]
    fn event_dictionary_getter_abrupts_preserve_identity_and_stop_conversion() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        install(engine.ctx(), "<div></div>", 64).unwrap();
        expose_test_constructors(engine);
        boolean(
            engine,
            r#"(() => {
                const thrown = {sentinel: true};
                const seen = [];
                const dictionary = new Proxy({}, {
                    get(_target, key) {
                        if (typeof key !== 'string') return undefined;
                        seen.push(key);
                        if (key === 'detail') throw thrown;
                        return undefined;
                    }
                });
                let caught;
                try { new KeyboardEvent('key', dictionary); }
                catch (error) { caught = error; }
                return caught === thrown &&
                    seen.join(',') === 'bubbles,cancelable,composed,detail';
            })()"#,
        );
    }

    #[test]
    fn static_ranges_and_input_event_target_ranges_preserve_brands_and_identity() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        install(engine.ctx(), "<!doctype html><p id='p'>abc</p>", 64).unwrap();
        expose_test_constructors(engine);
        boolean(
            engine,
            r#"(() => {
                const paragraph = document.querySelector('#p');
                const text = paragraph.firstChild;
                const range = new StaticRange({
                    startContainer: text, startOffset: 1,
                    endContainer: text, endOffset: 3
                });
                if (!(range instanceof StaticRange) || !(range instanceof AbstractRange) ||
                    Object.getPrototypeOf(StaticRange.prototype) !== AbstractRange.prototype ||
                    Object.getPrototypeOf(Range.prototype) !== AbstractRange.prototype ||
                    range.startContainer !== text || range.endContainer !== text ||
                    range.startOffset !== 1 || range.endOffset !== 3 || range.collapsed)
                    throw new Error('StaticRange and AbstractRange inheritance/state');

                const dictionaryOrder = [];
                const dictionary = new Proxy({}, {get(_target, key) {
                    dictionaryOrder.push(key);
                    return ({
                        endContainer: text, endOffset: 2,
                        startContainer: text, startOffset: 1
                    })[key];
                }});
                new StaticRange(dictionary);
                if (dictionaryOrder.join(',') !== 'endContainer,endOffset,startContainer,startOffset')
                    throw new Error('StaticRangeInit dictionary member order');
                const requiredOrder = [];
                let requiredError = false;
                try {
                    new StaticRange(new Proxy({}, {get(_target, key) {
                        requiredOrder.push(key);
                        return key === 'endContainer' ? text : undefined;
                    }}));
                } catch (error) { requiredError = error instanceof TypeError; }
                if (!requiredError || requiredOrder.join(',') !== 'endContainer,endOffset')
                    throw new Error('StaticRangeInit required member conversion');

                // Static boundaries are retained verbatim across text and tree mutations.
                text.replaceData(0, text.length, 'x');
                if (range.startContainer !== text || range.startOffset !== 1 || range.endOffset !== 3)
                    throw new Error('StaticRange boundary changed after character data mutation');
                const detached = document.createElement('b');
                const invalidButStored = new StaticRange({
                    startContainer: text, startOffset: 99,
                    endContainer: detached, endOffset: 8
                });
                if (invalidButStored.startOffset !== 99 || invalidButStored.endContainer !== detached)
                    throw new Error('StaticRange constructor incorrectly validated offsets or roots');
                paragraph.remove();
                if (range.startContainer !== text || range.startOffset !== 1 || range.endOffset !== 3)
                    throw new Error('StaticRange lost a detached boundary node');

                const otherDocument = document.implementation.createHTMLDocument('other');
                const adoptedText = otherDocument.createTextNode('abc');
                const adoptedRange = new StaticRange({
                    startContainer: adoptedText, startOffset: 1,
                    endContainer: adoptedText, endOffset: 2
                });
                document.adoptNode(adoptedText);
                if (adoptedRange.startContainer !== adoptedText ||
                    adoptedRange.startContainer.ownerDocument !== document ||
                    adoptedRange.startOffset !== 1 || adoptedRange.endOffset !== 2)
                    throw new Error('StaticRange endpoint identity after node adoption');

                const attr = document.createAttribute('data-x');
                let attrRejected = false;
                try {
                    new StaticRange({startContainer: attr, startOffset: 0,
                        endContainer: text, endOffset: 0});
                } catch (error) { attrRejected = error.name === 'InvalidNodeTypeError'; }
                let doctypeRejected = false;
                try {
                    new StaticRange({startContainer: document.doctype, startOffset: 0,
                        endContainer: text, endOffset: 0});
                } catch (error) { doctypeRejected = error.name === 'InvalidNodeTypeError'; }
                if (!attrRejected || !doctypeRejected)
                    throw new Error('Attr/DocumentType endpoint rejection');

                const source = new Set([range, adoptedRange]);
                const event = new InputEvent('beforeinput', {targetRanges: source});
                source.clear();
                const first = event.getTargetRanges();
                const second = event.getTargetRanges();
                if (!(event instanceof UIEvent) || first === second || first.length !== 2 ||
                    second.length !== 2 || first[0] !== range || second[0] !== range ||
                    first[1] !== adoptedRange || second[1] !== adoptedRange)
                    throw new Error('targetRanges sequence identity/fresh-array semantics');
                class DerivedInputEvent extends InputEvent {}
                const derived = new DerivedInputEvent('beforeinput', {targetRanges:[range]});
                if (!(derived instanceof DerivedInputEvent) || !(derived instanceof InputEvent) ||
                    derived.getTargetRanges()[0] !== range)
                    throw new Error('InputEvent constructor preserves new.target and subclass prototype');
                if (new InputEvent('input').getTargetRanges().length !== 0)
                    throw new Error('absent targetRanges should be empty');

                let closed = false;
                function* invalid() {
                    try { yield range; yield {}; }
                    finally { closed = true; }
                }
                let brandRejected = false;
                try { new InputEvent('beforeinput', {targetRanges: invalid()}); }
                catch (error) { brandRejected = error instanceof TypeError; }
                if (!brandRejected || !closed)
                    throw new Error('targetRanges brand conversion/iterator close');

                const abrupt = {sentinel: true};
                const throwsWhileIterating = {
                    [Symbol.iterator]() {
                        return {next() { throw abrupt; }};
                    }
                };
                let caught;
                try { new InputEvent('beforeinput', {targetRanges: throwsWhileIterating}); }
                catch (error) { caught = error; }
                if (caught !== abrupt)
                    throw new Error('targetRanges preserves iterator-thrown value');

                // Dropping the local range variable must not collect the object
                // retained by InputEvent's sequence.
                globalThis.retainedInputEvent = event;
                return true;
            })()"#,
        );
        engine.collect_garbage();
        boolean(
            engine,
            "retainedInputEvent.getTargetRanges()[0] instanceof StaticRange && retainedInputEvent.getTargetRanges()[0].startOffset === 1 && retainedInputEvent.getTargetRanges()[0].startContainer.data === 'x' && retainedInputEvent.getTargetRanges()[1].startContainer.ownerDocument === document",
        );
    }

    #[test]
    fn native_sequence_slots_trace_values_but_do_not_root_cycles() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        install(engine.ctx(), "<p id='gc'>x</p>", 64).unwrap();
        expose_test_constructors(engine);
        boolean(
            engine,
            r#"(() => {
                const text = document.querySelector('#gc').firstChild;
                globalThis.nativeSlotRange = new StaticRange({
                    startContainer: text, startOffset: 0, endContainer: text, endOffset: 1
                });
                nativeSlotRange.expando = {sentinel: 37};
                globalThis.nativeSlotEvent = new InputEvent('beforeinput', {
                    targetRanges: [nativeSlotRange]
                });
                return true;
            })()"#,
        );

        let (event_weak, range_weak, slot_name) = {
            let global = engine.ctx().global_object();
            let event = engine
                .ctx()
                .member_get(&global, "nativeSlotEvent")
                .ok()
                .expect("InputEvent wrapper exists");
            let range = engine
                .ctx()
                .member_get(&global, "nativeSlotRange")
                .ok()
                .expect("StaticRange wrapper exists");
            let slot_name = engine
                .ctx()
                .with_instance::<DomInputEvent, _>(&event, |event| {
                    event
                        .target_ranges_slot
                        .clone()
                        .expect("nonempty targetRanges has a native slot")
                })
                .ok()
                .expect("native InputEvent brand");
            engine
                .ctx()
                .member_set(
                    &global,
                    "nativeSlotKey",
                    Value::from_string(slot_name.clone()),
                )
                .ok()
                .expect("test obtains the otherwise hidden native slot name");
            let event_weak = engine.ctx().weak_value(&event).expect("event is an object");
            let range_weak = engine.ctx().weak_value(&range).expect("range is an object");
            (event_weak, range_weak, slot_name)
        };

        boolean(
            engine,
            r#"(() => {
                const event = nativeSlotEvent;
                const key = nativeSlotKey;
                const backing = event[key];
                let slotWriteRejected = false;
                let itemWriteRejected = false;
                try { Object.defineProperty(event, key, {value: []}); }
                catch (_) { slotWriteRejected = true; }
                try { Object.defineProperty(backing, '0', {value: null}); }
                catch (_) { itemWriteRejected = true; }
                return slotWriteRejected && itemWriteRejected && Object.isFrozen(backing) &&
                    Reflect.ownKeys(event).indexOf(key) === -1 &&
                    Object.getOwnPropertyDescriptor(event, key) === undefined &&
                    event.getTargetRanges()[0] === nativeSlotRange &&
                    event.getTargetRanges()[0].expando.sentinel === 37;
            })()"#,
        );

        boolean(
            engine,
            "nativeSlotRange = null; nativeSlotEvent.getTargetRanges()[0].expando.sentinel === 37",
        );
        engine.collect_garbage();
        let retained_range = range_weak.upgrade().expect("event slot traces its range");
        drop(retained_range);

        boolean(
            engine,
            r#"(() => {
                const event = nativeSlotEvent;
                event.getTargetRanges()[0].cycle = event;
                nativeSlotEvent = null;
                nativeSlotKey = null;
                return true;
            })()"#,
        );
        engine.collect_garbage();
        assert!(
            event_weak.upgrade().is_none(),
            "event/range cycle is collectible"
        );
        assert!(
            range_weak.upgrade().is_none(),
            "range/event cycle is collectible"
        );
        assert!(!slot_name.is_empty());
    }

    #[test]
    fn user_agent_text_edit_input_events_have_empty_target_ranges() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = install(engine.ctx(), "<input id='text' value=''>", 64).unwrap();
        expose_test_constructors(engine);
        let node = {
            let session = realm.session.borrow();
            let document = session.document();
            lumen_html::selector::query_selector(document, document.root(), "#text")
                .unwrap()
                .expect("input exists")
        };
        let setup = engine
            .eval_value(
                r#"(() => {
                    const input = document.querySelector('#text');
                    globalThis.uaBeforeInput = false;
                    globalThis.uaInput = false;
                    input.addEventListener('beforeinput', event => {
                        uaBeforeInput = event.isTrusted && event instanceof InputEvent &&
                            event.getTargetRanges().length === 0;
                    });
                    input.addEventListener('input', event => {
                        uaInput = event.isTrusted && event instanceof InputEvent &&
                            event.getTargetRanges().length === 0;
                    });
                    return true;
                })()"#,
            )
            .expect("listener setup script evaluates");
        assert!(matches!(setup, Ok(Value::Bool(true))));
        super::keyboard_automation::trusted_send_keys(engine.ctx(), &realm, node, "a")
            .expect("trusted text edit succeeds");
        boolean(
            engine,
            "document.querySelector('#text').value === 'a' && uaBeforeInput && uaInput",
        );
    }

    #[test]
    fn legacy_ui_initializers_update_only_before_dispatch() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        install(engine.ctx(), "<div></div>", 64).unwrap();
        expose_test_constructors(engine);
        boolean(
            engine,
            r#"(() => {
                const target = document.body;
                const related = document.createElement('span');
                const event = new MouseEvent('old', {screenX: 1, button: 2});
                event.initMouseEvent('new', true, true, window, 8, 10, 11, 12, 13, true, false, true, false, 4, related);
                const initialized = event.type === 'new' && event.bubbles && event.cancelable && event.view === window &&
                    event.detail === 8 && event.screenX === 10 && event.screenY === 11 && event.clientX === 12 && event.clientY === 13 &&
                    event.ctrlKey && event.shiftKey && !event.altKey && event.button === 4 && event.buttons === 0 && event.relatedTarget === related;
                const during = new KeyboardEvent('before', {key: 'before', bubbles: true});
                let inside = false;
                target.addEventListener('before', e => {
                    e.initKeyboardEvent('after', false, false, window, 'after', 2, true, true, true, true);
                    inside = e.type === 'before' && e.key === 'before' && e.bubbles && !e.ctrlKey;
                }, {once: true});
                target.dispatchEvent(during);
                return initialized && inside && during.type === 'before' && during.key === 'before' && during.bubbles && !during.ctrlKey;
            })()"#,
        );
    }
}
