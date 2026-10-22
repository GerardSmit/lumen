//! `CSSFontPaletteValuesRule` over the canonical CSSOM source.
use super::*;

#[lumen_bind::class(name="CSSFontPaletteValuesRule",extends=DomCssRule,hint(js(webidl)))]
pub(super) struct DomCssFontPaletteValuesRule {
    base:DomCssRule,
}

impl DomCssFontPaletteValuesRule {
    fn values(&self)->OpResult<css::font_palette_values::FontPaletteValues> {
        css::font_palette_values::parse(&self.base.current_rule()?.css_text)
            .ok_or_else(||OpError::new("InvalidStateError","invalid font palette values rule"))
    }
}

#[lumen_bind::methods]
impl DomCssFontPaletteValuesRule {
    #[getter] fn name(&self)->OpResult<String> {Ok(self.values()?.name)}
    #[getter(name="fontFamily")] fn font_family(&self)->OpResult<String> {Ok(self.values()?.font_family.unwrap_or_default())}
    #[getter(name="basePalette")] fn base_palette(&self)->OpResult<String> {Ok(self.values()?.base_palette.unwrap_or_default())}
    #[getter(name="overrideColors")] fn override_colors(&self)->OpResult<String> {Ok(self.values()?.override_colors_text())}
}

pub(super) fn wrap(ctx:&mut Ctx,base:DomCssRule)->Value {ctx.new_instance(DomCssFontPaletteValuesRule{base})}

pub(super) fn install(ctx:&mut Ctx)->OpResult<()> {
    let global=ctx.global_object();
    let rule=ctx.class_constructor::<DomCssFontPaletteValuesRule>();
    crate::install_interface(ctx,&global,"CSSFontPaletteValuesRule",rule).map_err(|_|OpError::new("Error","font palette rule installation failed"))
}
