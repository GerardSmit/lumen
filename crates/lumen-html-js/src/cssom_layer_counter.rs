//! `CSSLayerBlockRule`, `CSSLayerStatementRule` and `CSSCounterStyleRule`
//! over the canonical CSSOM source.
use super::*;

#[lumen_bind::class(name="CSSLayerBlockRule",extends=DomCssGroupingRule,hint(js(webidl)))]
pub(super) struct DomCssLayerBlockRule {
    base:DomCssGroupingRule,
}

#[lumen_bind::class(name="CSSLayerStatementRule",extends=DomCssRule,hint(js(webidl)))]
pub(super) struct DomCssLayerStatementRule {
    base:DomCssRule,
}

#[lumen_bind::class(name="CSSCounterStyleRule",extends=DomCssRule,hint(js(webidl)))]
pub(super) struct DomCssCounterStyleRule {
    base:DomCssRule,
}

fn layer_prelude(rule:&DomCssRule)->OpResult<String> {
    rule.with_source_rule(&mut |text,source| {
        let prelude=text[source.prelude.clone()].trim();
        Ok(prelude.get("@layer".len()..).unwrap_or_default().trim().to_owned())
    })
}

#[lumen_bind::methods]
impl DomCssLayerBlockRule {
    #[getter]
    fn name(&self)->OpResult<String> {layer_prelude(&self.base.base)}
}

#[lumen_bind::methods]
impl DomCssLayerStatementRule {
    #[getter(name="nameList")]
    fn name_list(&self,ctx:&mut Ctx,this:This<Value>)->OpResult<Value> {
        const SLOT:&str="#lumen_layer_statement_rule\u{1}nameList";
        if let Some(value)=ctx.native_private_value_slot(&this.0,SLOT) {return Ok(value);}
        let names=layer_prelude(&self.base)?
            .split(',')
            .map(str::trim)
            .filter(|name|!name.is_empty())
            .map(|name|Value::from_string(name.to_owned()))
            .collect();
        let list=ctx.make_array(names);
        ctx.freeze_native_object(&list);
        ctx.define_native_internal_value_slot(&this.0,SLOT,list.clone()).map_err(OpError::thrown)?;
        Ok(list)
    }
}

impl DomCssCounterStyleRule {
    fn declarations(&self)->OpResult<String> {
        self.base.with_source_rule(&mut |text,source| Ok(source.declaration_source(text).into_owned()))
    }

    /// Counter-style descriptor writes are not applied: the stylesheet model
    /// has no mutation path for `@counter-style` declarations.
    fn ignore_descriptor(&self,_name:&str,_value:&str)->OpResult<()> {Ok(())}

    fn descriptor(&self,name:&str)->OpResult<String> {
        Ok(css::descriptor_declaration_value(&self.declarations()?,name).map_err(css_error)?.map_or_else(String::new,|(value,_)|value))
    }
}

#[lumen_bind::methods]
impl DomCssCounterStyleRule {
    #[getter]
    fn name(&self)->OpResult<String> {
        self.base.with_source_rule(&mut |text,source| {
            let prelude=text[source.prelude.clone()].trim();
            Ok(prelude.get("@counter-style".len()..).unwrap_or_default().trim().to_owned())
        })
    }
    #[setter(name="name",coerce)]
    fn set_name(&self,_value:&str)->OpResult<()> {Ok(())}
    #[getter(name = "system")]
    fn system(&self) -> OpResult<String> { self.descriptor("system") }
    #[setter(name = "system", coerce)]
    fn set_system(&self, value: &str) -> OpResult<()> { self.ignore_descriptor("system", value) }
    #[getter(name = "symbols")]
    fn symbols(&self) -> OpResult<String> { self.descriptor("symbols") }
    #[setter(name = "symbols", coerce)]
    fn set_symbols(&self, value: &str) -> OpResult<()> { self.ignore_descriptor("symbols", value) }
    #[getter(name = "additiveSymbols")]
    fn additive_symbols(&self) -> OpResult<String> { self.descriptor("additive-symbols") }
    #[setter(name = "additiveSymbols", coerce)]
    fn set_additive_symbols(&self, value: &str) -> OpResult<()> { self.ignore_descriptor("additive-symbols", value) }
    #[getter(name = "negative")]
    fn negative(&self) -> OpResult<String> { self.descriptor("negative") }
    #[setter(name = "negative", coerce)]
    fn set_negative(&self, value: &str) -> OpResult<()> { self.ignore_descriptor("negative", value) }
    #[getter(name = "prefix")]
    fn prefix(&self) -> OpResult<String> { self.descriptor("prefix") }
    #[setter(name = "prefix", coerce)]
    fn set_prefix(&self, value: &str) -> OpResult<()> { self.ignore_descriptor("prefix", value) }
    #[getter(name = "suffix")]
    fn suffix(&self) -> OpResult<String> { self.descriptor("suffix") }
    #[setter(name = "suffix", coerce)]
    fn set_suffix(&self, value: &str) -> OpResult<()> { self.ignore_descriptor("suffix", value) }
    #[getter(name = "range")]
    fn range(&self) -> OpResult<String> { self.descriptor("range") }
    #[setter(name = "range", coerce)]
    fn set_range(&self, value: &str) -> OpResult<()> { self.ignore_descriptor("range", value) }
    #[getter(name = "pad")]
    fn pad(&self) -> OpResult<String> { self.descriptor("pad") }
    #[setter(name = "pad", coerce)]
    fn set_pad(&self, value: &str) -> OpResult<()> { self.ignore_descriptor("pad", value) }
    #[getter(name = "speakAs")]
    fn speak_as(&self) -> OpResult<String> { self.descriptor("speak-as") }
    #[setter(name = "speakAs", coerce)]
    fn set_speak_as(&self, value: &str) -> OpResult<()> { self.ignore_descriptor("speak-as", value) }
    #[getter(name = "fallback")]
    fn fallback(&self) -> OpResult<String> { self.descriptor("fallback") }
    #[setter(name = "fallback", coerce)]
    fn set_fallback(&self, value: &str) -> OpResult<()> { self.ignore_descriptor("fallback", value) }
}

pub(super) fn wrap_layer_block(ctx:&mut Ctx,base:DomCssRule)->Value {
    ctx.new_instance(DomCssLayerBlockRule{base:DomCssGroupingRule{base}})
}

pub(super) fn wrap_layer_statement(ctx:&mut Ctx,base:DomCssRule)->Value {
    ctx.new_instance(DomCssLayerStatementRule{base})
}

pub(super) fn wrap_counter_style(ctx:&mut Ctx,base:DomCssRule)->Value {
    ctx.new_instance(DomCssCounterStyleRule{base})
}

pub(super) fn install(ctx:&mut Ctx)->OpResult<()> {
    let global=ctx.global_object();
    let block=ctx.class_constructor::<DomCssLayerBlockRule>();
    crate::install_interface(ctx,&global,"CSSLayerBlockRule",block).map_err(|_|OpError::new("Error","layer rule installation failed"))?;
    let statement=ctx.class_constructor::<DomCssLayerStatementRule>();
    crate::install_interface(ctx,&global,"CSSLayerStatementRule",statement).map_err(|_|OpError::new("Error","layer rule installation failed"))?;
    let counter=ctx.class_constructor::<DomCssCounterStyleRule>();
    crate::install_interface(ctx,&global,"CSSCounterStyleRule",counter).map_err(|_|OpError::new("Error","counter style rule installation failed"))
}
