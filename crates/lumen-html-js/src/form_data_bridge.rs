//! DOM-specific FormData construction over lumen-web's existing FormData class.
use super::*;

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
    ctx.set_member(&ctx.global_object(), "__lumenPopulateFormData", function)
        .map_err(|_| OpError::new("Error", "FormData DOM bridge installation failed"))
}
