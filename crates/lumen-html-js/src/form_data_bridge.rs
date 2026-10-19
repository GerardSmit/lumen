//! The DOM side of `new FormData(form, submitter)`, registered with lumen-host's native
//! `FormData`.
use super::*;

fn populate(ctx: &mut Ctx, data: &Value, form: &Value, submitter: &Value) -> OpResult<()> {
    let (realm, form_id) = ctx
        .with_instance::<DomFormElement, _>(form, |form| {
            let node = &form.base.base.base;
            (node.realm.clone(), node.id)
        })
        .map_err(|_| {
            OpError::type_error(
                "Failed to construct 'FormData': parameter 1 is not of type 'HTMLFormElement'.",
            )
        })?;
    let submitter_id = if matches!(submitter, Value::Undefined | Value::Null) {
        None
    } else {
        let (submitter_realm, id) = ctx
            .with_instance::<DomNode, _>(submitter, |node| (node.realm.clone(), node.id))
            .map_err(|_| {
                OpError::type_error(
                    "Failed to construct 'FormData': parameter 2 is not of type 'HTMLElement'.",
                )
            })?;
        if !Rc::ptr_eq(&realm, &submitter_realm) {
            return Err(OpError::new(
                "NotFoundError",
                "submitter belongs to another document",
            ));
        }
        Some(id)
    };
    forms::populate_form_data(ctx, &realm, form_id, submitter_id, data.clone())
}

pub(crate) fn install(ctx: &mut Ctx) -> OpResult<()> {
    lumen_host::blob::FormBridge::install(ctx, populate);
    Ok(())
}

/// Whether `value` is a genuine `FormData` of any realm, without consulting mutable
/// constructors, prototypes or `Symbol.hasInstance`.
pub(crate) fn is_form_data(ctx: &mut Ctx, value: &Value) -> OpResult<bool> {
    Ok(lumen_host::blob::is_form_data(ctx, value))
}
