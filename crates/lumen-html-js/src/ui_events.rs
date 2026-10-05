//! Native UI and input event interfaces.
//!
//! The subclasses keep their interface-specific data beside the shared `DomEvent` handle. Event
//! dispatch, trust, propagation, target retargeting, and cancellation therefore remain owned by
//! the common Event implementation.

use super::*;
use crate::window_globals::DomWindow;
use lumen::embed::{Ctx, OpError, OpResult, Value};

fn dictionary_member(
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

fn dictionary_boolean(
    ctx: &mut Ctx,
    dictionary: &Option<Value>,
    name: &str,
    default: bool,
) -> OpResult<bool> {
    Ok(dictionary_member(ctx, dictionary, name)?
        .as_ref()
        .map_or(default, |value| ctx.to_boolean(value)))
}

fn dictionary_string(
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

fn number_to_unsigned(number: f64, bits: u32) -> u32 {
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
    ctx.coerce_number(&value).map_err(OpError::thrown)
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

#[lumen_bind::class(name = "MouseEvent", extends = DomUIEvent, hint(js(webidl)))]
pub(crate) struct DomMouseEvent {
    base: DomUIEvent,
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
    let related_target = dictionary_event_target(ctx, &dictionary, "relatedTarget")?;
    let screen_x = dictionary_signed(ctx, &dictionary, "screenX", 0, 32)?;
    let screen_y = dictionary_signed(ctx, &dictionary, "screenY", 0, 32)?;
    base.base.set_related_target(related_target.clone());
    Ok(DomMouseEvent {
        base,
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

/// Install Web IDL interface constants on both interface objects and their prototypes. The
/// caller owns class registration; this helper is deliberately separate so constructors remain
/// native classes and constant installation never invokes page-modified setters.
pub(crate) fn install_keyboard_and_wheel_constants(
    ctx: &mut Ctx,
    keyboard_constructor: &Value,
    wheel_constructor: &Value,
) -> Result<(), Value> {
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
}

#[lumen_bind::methods]
impl DomInputEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Self> {
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
        Ok(Self {
            base,
            data: RefCell::new(data),
            is_composing: Cell::new(is_composing),
            input_type: RefCell::new(input_type),
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
        install_keyboard_and_wheel_constants(engine.ctx(), &keyboard, &wheel)
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
        let result = engine
            .eval_value(source)
            .expect("valid UI event test script");
        assert!(
            matches!(result, Ok(Value::Bool(true))),
            "UI event assertion failed: {source}"
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
                    get button() { order.push('button'); return 0; },
                    get buttons() { order.push('buttons'); return 0; },
                    get relatedTarget() { order.push('relatedTarget'); return null; }
                });
                return order.join(',') === 'bubbles,cancelable,composed,detail,view,which,altKey,ctrlKey,metaKey,modifierAltGraph,modifierCapsLock,modifierFn,modifierFnLock,modifierHyper,modifierNumLock,modifierScrollLock,modifierSuper,modifierSymbol,modifierSymbolLock,shiftKey,button,buttons,clientX,clientY,relatedTarget,screenX,screenY';
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
                const mouse = ui + modifiers + ',button,buttons,clientX,clientY,relatedTarget,screenX,screenY';
                return keys(UIEvent) === ui &&
                    keys(FocusEvent) === ui + ',relatedTarget' &&
                    keys(MouseEvent) === mouse &&
                    keys(WheelEvent) === mouse + ',deltaMode,deltaX,deltaY,deltaZ,momentum' &&
                    keys(KeyboardEvent) === ui + modifiers + ',charCode,code,isComposing,key,keyCode,location,repeat' &&
                    keys(CompositionEvent) === ui + ',data' &&
                    keys(InputEvent) === ui + ',data,inputType,isComposing' &&
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
