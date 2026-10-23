//! `CSSViewTransitionRule` over the canonical CSSOM source.
use super::*;

#[lumen_bind::class(name="CSSViewTransitionRule",extends=DomCssRule,hint(js(webidl)))]
pub(super) struct DomCssViewTransitionRule {
    base:DomCssRule,
}

impl DomCssViewTransitionRule {
    fn rule(&self)->OpResult<css::view_transition_rule::ViewTransitionRule> {
        Ok(css::view_transition_rule::parse(&self.base.current_rule()?.css_text))
    }
}

#[lumen_bind::methods]
impl DomCssViewTransitionRule {
    #[getter] fn navigation(&self)->OpResult<String> {Ok(self.rule()?.navigation.unwrap_or_default().to_string())}
    #[getter] fn types(&self,ctx:&mut Ctx)->OpResult<Value> {
        let names=self.rule()?.types.unwrap_or_default();
        Ok(ctx.make_array(names.iter().map(|name|Value::str(name)).collect()))
    }
}

pub(super) fn wrap(ctx:&mut Ctx,base:DomCssRule)->Value {ctx.new_instance(DomCssViewTransitionRule{base})}

pub(super) fn install(ctx:&mut Ctx)->OpResult<()> {
    let global=ctx.global_object();
    let rule=ctx.class_constructor::<DomCssViewTransitionRule>();
    crate::install_interface(ctx,&global,"CSSViewTransitionRule",rule).map_err(|_|OpError::new("Error","view transition rule installation failed"))
}
