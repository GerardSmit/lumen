//! `CSSPseudoElement` handles returned by `Element.pseudo()`.
use super::*;

const ELEMENT_PSEUDOS: &[&str] = &[
    "::before", "::after", "::marker", "::first-line", "::first-letter", "::backdrop", "::placeholder",
    "::selection", "::file-selector-button",
];

#[lumen_bind::class(name="CSSPseudoElement",hint(js(webidl)))]
pub(crate) struct DomCssPseudoElement {
    element:Value,
    parent:Value,
    selector:String,
}

fn child_pseudo(ctx:&mut Ctx,owner:&Value,element:&Value,selector:&str)->OpResult<Value> {
    let slot=format!("#lumen_pseudo_element\u{1}{selector}");
    if let Some(existing)=ctx.native_private_value_slot(owner,&slot) {return Ok(existing);}
    let pseudo=ctx.new_instance(DomCssPseudoElement {
        element:element.clone(),
        parent:owner.clone(),
        selector:selector.to_owned(),
    });
    ctx.define_native_internal_value_slot(owner,&slot,pseudo.clone()).map_err(OpError::thrown)?;
    Ok(pseudo)
}

pub(crate) fn element_pseudo(ctx:&mut Ctx,element:&Value,kind:&str)->OpResult<Value> {
    let selector=kind.to_ascii_lowercase();
    if !ELEMENT_PSEUDOS.contains(&selector.as_str()) {return Ok(Value::Null);}
    child_pseudo(ctx,element,element,&selector)
}

#[lumen_bind::methods]
impl DomCssPseudoElement {
    #[getter(name="type")]
    fn kind(&self)->String {self.selector.clone()}
    #[getter]
    fn element(&self)->Value {self.element.clone()}
    #[getter]
    fn parent(&self)->Value {self.parent.clone()}
    #[getter(name="selectorText")]
    fn selector_text(&self)->String {self.selector.clone()}

    #[method(coerce)]
    fn pseudo(&self,ctx:&mut Ctx,this:lumen_bind::This<Value>,kind:&str)->OpResult<Value> {
        let nested=kind.to_ascii_lowercase();
        let allowed=matches!(self.selector.as_str(),"::before"|"::after") && nested=="::marker";
        if !allowed {return Ok(Value::Null);}
        child_pseudo(ctx,&this.0,&self.element,&format!("{}{nested}",self.selector))
    }
}

pub(crate) fn install(ctx:&mut Ctx)->OpResult<()> {
    let global=ctx.global_object();
    let constructor=ctx.class_constructor::<DomCssPseudoElement>();
    crate::install_interface(ctx,&global,"CSSPseudoElement",constructor)
        .map_err(|_|OpError::new("Error","CSSPseudoElement installation failed"))
}
