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

fn same_object(ctx: &mut Ctx, slot: &str, create: impl FnOnce(&mut Ctx) -> Value) -> OpResult<Value> {
    let global = ctx.global_object();
    if let Some(existing) = ctx.native_private_value_slot(&global, slot) {
        return Ok(existing);
    }
    let value = create(ctx);
    ctx.set_native_internal_value_slot(&global, slot, value.clone()).map_err(OpError::thrown)?;
    Ok(value)
}

pub(crate) fn bar(ctx: &mut Ctx, _receiver: Value, name: &str) -> OpResult<Value> {
    let slot = format!("#lumen_window\u{1}bar_{name}");
    same_object(ctx, &slot, |ctx| ctx.new_instance(DomBarProp))
}

pub(crate) fn external(ctx: &mut Ctx, _receiver: Value) -> OpResult<Value> {
    same_object(ctx, "#lumen_window\u{1}external", |ctx| ctx.new_instance(DomExternal))
}

pub(crate) fn status(ctx: &mut Ctx, _receiver: Value) -> OpResult<String> {
    let global = ctx.global_object();
    Ok(match ctx.native_private_value_slot(&global, STATUS_SLOT) {
        Some(Value::Str(text)) => text.as_str().to_owned(),
        _ => String::new(),
    })
}

pub(crate) fn set_status(ctx: &mut Ctx, _receiver: Value, value: String) -> OpResult<()> {
    let global = ctx.global_object();
    ctx.set_native_internal_value_slot(&global, STATUS_SLOT, Value::from_string(value)).map_err(OpError::thrown)
}
