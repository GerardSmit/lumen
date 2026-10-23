//! Web IDL interface-shape corrections applied to already-built interface objects.
use super::*;

/// The prototype of an interface that has no parent interface: `Function.prototype` for the
/// interface object and `Object.prototype` for the prototype object. `plain` is any interface
/// constructor without a parent.
pub(crate) fn make_root_interface(ctx: &mut Ctx, interface: &Value, plain: &Value) -> OpResult<()> {
    let function_prototype = ctx.prototype_of(plain);
    let plain_prototype = ctx.member_get(plain, "prototype").map_err(OpError::thrown)?;
    let object_prototype = ctx.prototype_of(&plain_prototype);
    let prototype = ctx.member_get(interface, "prototype").map_err(OpError::thrown)?;
    ctx.reflect_set_prototype_of(interface, &function_prototype).map_err(OpError::thrown)?;
    ctx.reflect_set_prototype_of(&prototype, &object_prototype).map_err(OpError::thrown)?;
    Ok(())
}

/// Redefines `name` on `holder` with the attributes of a Web IDL `[LegacyUnforgeable]` member:
/// non-configurable, and non-writable when it is a data property.
pub(crate) fn lock_member(ctx: &mut Ctx, holder: &Value, name: &str) -> OpResult<()> {
    let key = Value::str(name);
    let descriptor = ctx.reflect_get_own_property_descriptor(holder, &key).map_err(OpError::thrown)?;
    if matches!(descriptor, Value::Undefined) {
        return Ok(());
    }
    ctx.member_set(&descriptor, "configurable", Value::Bool(false)).map_err(OpError::thrown)?;
    if ctx.has_own_property_value(&descriptor, &Value::str("value")).map_err(OpError::thrown)? {
        ctx.member_set(&descriptor, "writable", Value::Bool(false)).map_err(OpError::thrown)?;
    }
    ctx.reflect_define_property(holder, &key, &descriptor).map_err(OpError::thrown)?;
    Ok(())
}

/// Gives `instance` its own copy of each prototype member in `names`, as `[LegacyUnforgeable]`
/// members are own properties of every instance. The prototype keeps its members.
pub(crate) fn define_unforgeable_own(ctx: &mut Ctx, instance: &Value, names: &[&str]) -> OpResult<()> {
    let prototype = ctx.prototype_of(instance);
    for &name in names {
        let key = Value::str(name);
        let descriptor = ctx.reflect_get_own_property_descriptor(&prototype, &key).map_err(OpError::thrown)?;
        if matches!(descriptor, Value::Undefined) {
            continue;
        }
        ctx.reflect_define_property(instance, &key, &descriptor).map_err(OpError::thrown)?;
        lock_member(ctx, instance, name)?;
    }
    Ok(())
}
