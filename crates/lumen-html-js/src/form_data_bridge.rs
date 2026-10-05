//! DOM-specific FormData construction over lumen-web's existing FormData class.
use super::*;
use std::collections::HashMap;

#[derive(Default)]
struct FormDataBrandRegistry {
    // The predicate is rooted by its own realm global. Keeping only weak values here lets retired
    // browsing contexts and their private FormData brands collect normally.
    checks: HashMap<usize, (WeakValue, WeakValue)>,
}

#[lumen_bind::op(name = "__lumenPopulateFormData")]
fn populate(
    ctx: &mut Ctx,
    data: Value,
    form: &DomFormElement,
    submitter: Option<&DomNode>,
) -> OpResult<()> {
    let node = &form.base.base.base;
    if let Some(submitter) = submitter {
        if !Rc::ptr_eq(&node.realm, &submitter.realm) {
            return Err(OpError::new(
                "NotFoundError",
                "submitter belongs to another document",
            ));
        }
    }
    forms::populate_form_data(
        ctx,
        &node.realm,
        node.id,
        submitter.map(|node| node.id),
        data,
    )
}

pub(crate) fn install(ctx: &mut Ctx) -> OpResult<()> {
    let function = ctx.bound_function(&lumen_bind::FnItem::of::<populate::Op>());
    let global = ctx.global_object();
    ctx.set_member(&global, "__lumenPopulateFormData", function)
        .map_err(|_| OpError::new("Error", "FormData DOM bridge installation failed"))
        .and_then(|()| register_brand_check(ctx, &global))
}

fn register_brand_check(ctx: &mut Ctx, global: &Value) -> OpResult<()> {
    // Accessing FormData first materializes the existing lazy web-glue unit in host realms. Bare
    // Engine tests do not install web providers, so absence is intentionally a no-op there.
    let _ = ctx.get_member(global, "FormData");
    let Ok(check) = ctx.get_member(global, "__lumenIsFormData") else {
        return Ok(());
    };
    if !check.is_callable() {
        return Ok(());
    }
    let Some(key) = global.object_identity() else {
        return Ok(());
    };
    let weak_global = ctx
        .weak_value(global)
        .expect("the active realm global is an object");
    let weak_check = ctx
        .weak_value(&check)
        .expect("the captured FormData brand predicate is an object");
    if !ctx.op_state().has::<FormDataBrandRegistry>() {
        ctx.op_state().put(FormDataBrandRegistry::default());
    }
    let registry = ctx
        .op_state()
        .get_mut::<FormDataBrandRegistry>()
        .expect("the FormData brand registry was just installed");
    registry.checks.retain(|identity, (global, check)| {
        global
            .upgrade()
            .is_some_and(|value| value.object_identity() == Some(*identity))
            && check.upgrade().is_some()
    });
    registry.checks.insert(key, (weak_global, weak_check));
    Ok(())
}

/// Check a genuine FormData brand without consulting mutable constructors,
/// prototypes, or Symbol.hasInstance. Each realm contributes a weakly-held
/// private-field predicate so genuine cross-realm FormData objects work too.
pub(crate) fn is_form_data(ctx: &mut Ctx, value: &Value) -> OpResult<bool> {
    if !matches!(value, Value::Obj(_)) {
        return Ok(false);
    }
    let Some(registry) = ctx.op_state().get::<FormDataBrandRegistry>() else {
        return Ok(false);
    };
    let checks: Vec<(usize, Value)> = registry
        .checks
        .iter()
        .filter_map(|(identity, (global, check))| {
            let global = global.upgrade()?;
            if global.object_identity() != Some(*identity) {
                return None;
            }
            Some((*identity, check.upgrade()?))
        })
        .collect();
    for (_, check) in checks {
        let result = ctx
            .invoke(check, Value::Undefined, &[value.clone()])
            .map_err(OpError::thrown)?;
        if matches!(result, Value::Bool(true)) {
            return Ok(true);
        }
    }
    Ok(false)
}
