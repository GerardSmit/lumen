//! The legacy `Window` members with no browsing-context behavior: the `BarProp` objects,
//! `window.external` and `window.status`.
use super::*;

const STATUS_SLOT: &str = "#lumen_window\u{1}status";

#[lumen_bind::class(name = "BarProp", hint(js(webidl)))]
pub struct DomBarProp;

#[lumen_bind::methods]
impl DomBarProp {
    #[getter]
    fn visible(&self) -> bool {
        true
    }
}

#[lumen_bind::class(name = "External", hint(js(webidl)))]
pub struct DomExternal;

#[lumen_bind::methods]
impl DomExternal {
    #[method(name = "AddSearchProvider")]
    fn add_search_provider(&self) {}

    #[method(name = "IsSearchProviderInstalled")]
    fn is_search_provider_installed(&self) {}
}

pub(crate) fn constructors(ctx: &mut Ctx) -> Vec<(&'static str, Value)> {
    vec![
        ("BarProp", ctx.class_constructor::<DomBarProp>()),
        ("External", ctx.class_constructor::<DomExternal>()),
    ]
}

fn window_receiver(ctx: &mut Ctx, receiver: Value) -> OpResult<Value> {
    let receiver = if matches!(receiver, Value::Undefined | Value::Null) { ctx.global_object() } else { receiver };
    ctx.with_instance::<crate::window_globals::DomWindow, _>(&receiver, |_| ())?;
    Ok(receiver)
}

fn same_object(
    ctx: &mut Ctx,
    receiver: Value,
    slot: &str,
    create: impl FnOnce(&mut Ctx) -> Value,
) -> OpResult<Value> {
    let window = window_receiver(ctx, receiver)?;
    if let Some(existing) = ctx.native_private_value_slot(&window, slot) {
        return Ok(existing);
    }
    let value = create(ctx);
    ctx.set_native_internal_value_slot(&window, slot, value.clone()).map_err(OpError::thrown)?;
    Ok(value)
}

pub(crate) fn bar(ctx: &mut Ctx, receiver: Value, name: &str) -> OpResult<Value> {
    let slot = format!("#lumen_window\u{1}bar_{name}");
    same_object(ctx, receiver, &slot, |ctx| ctx.new_instance(DomBarProp))
}

pub(crate) fn external(ctx: &mut Ctx, receiver: Value) -> OpResult<Value> {
    same_object(ctx, receiver, "#lumen_window\u{1}external", |ctx| ctx.new_instance(DomExternal))
}

pub(crate) fn status(ctx: &mut Ctx, receiver: Value) -> OpResult<String> {
    let window = window_receiver(ctx, receiver)?;
    Ok(match ctx.native_private_value_slot(&window, STATUS_SLOT) {
        Some(Value::Str(text)) => text.as_str().to_owned(),
        _ => String::new(),
    })
}

pub(crate) fn set_status(ctx: &mut Ctx, receiver: Value, value: String) -> OpResult<()> {
    let window = window_receiver(ctx, receiver)?;
    ctx.set_native_internal_value_slot(&window, STATUS_SLOT, Value::from_string(value)).map_err(OpError::thrown)
}
