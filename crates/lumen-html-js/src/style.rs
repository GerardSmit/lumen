use super::*;
use lumen_html::{css, paint::Rgba};

#[lumen_bind::class(name = "CSSStyleDeclaration")]
pub struct DomStyle {
    pub(crate) realm: Rc<DomRealm>,
    pub(crate) node: NodeId,
    pub(crate) computed: bool,
    pub(crate) _owner: Value,
}

fn css_error(error: css::CssError) -> OpError { OpError::new("InvalidStateError", format!("CSS error at {}: {}", error.offset, error.message)) }
fn color(value: Rgba) -> String {
    if value.a == 255 { format!("rgb({}, {}, {})", value.r, value.g, value.b) } else { format!("rgba({}, {}, {}, {})", value.r, value.g, value.b, value.a as f32 / 255.0) }
}

impl DomStyle {
    fn raw(&self) -> OpResult<String> {
        let session = self.realm.session.borrow();
        match session.document().kind(self.node).map_err(dom_error)? {
            NodeKind::Element { attributes, .. } => Ok(attributes.iter().find(|(name, _)| name == "style").map_or(String::new(), |(_, value)| value.clone())),
            _ => Err(OpError::new("TypeError", "style requires an element")),
        }
    }
    fn write(&self, value: &str) -> OpResult<()> {
        if self.computed { return Err(OpError::new("NoModificationAllowedError", "computed style is read-only")); }
        self.realm.session.borrow_mut().document_mut().set_attribute(self.node, "style", value).map_err(dom_error)
    }
}

#[lumen_bind::methods]
impl DomStyle {
    #[getter]
    fn css_text(&self) -> OpResult<String> { if self.computed { Ok(String::new()) } else { self.raw() } }
    #[setter]
    fn set_css_text(&self, value: &str) -> OpResult<()> { css::declaration_value(value, "display").map_err(css_error)?; self.write(value) }
    fn get_property_value(&self, name: &str) -> OpResult<String> {
        if self.computed {
            let style = self.realm.session.borrow_mut().computed_style(self.node).map_err(|error| OpError::new("InvalidStateError", format!("computed style failed: {error:?}")))?;
            return Ok(match name {
                "display" => match style.display { css::Display::Block => "block", css::Display::Inline => "inline", css::Display::Flex => "flex", css::Display::Grid => "grid", css::Display::Table => "table", css::Display::TableRowGroup => "table-row-group", css::Display::TableRow => "table-row", css::Display::TableCell => "table-cell", css::Display::None => "none" }.into(),
                "color" => color(style.color),
                "background-color" | "background" => color(style.background),
                "width" => style.width.map_or("auto".into(), |value| format!("{value}px")),
                "height" => style.height.map_or("auto".into(), |value| format!("{value}px")),
                "margin" => format!("{}px", style.margin),
                "padding" => format!("{}px", style.padding),
                "font-size" => format!("{}px", style.font_size),
                "border-radius" => format!("{}px", style.border_radius),
                "border-width" => format!("{}px", style.border_width),
                "border-color" => color(style.border_color),
                "overflow" => if style.overflow_clip { "hidden" } else { "visible" }.into(),
                "opacity" => style.opacity.to_string(),
                "line-height" => match style.line_height { css::LineHeight::Normal => "normal".into(), css::LineHeight::Number(value) => value.to_string(), css::LineHeight::Pixels(value) => format!("{value}px") },
                _ => String::new(),
            });
        }
        Ok(css::declaration_value(&self.raw()?, name).map_err(css_error)?.map_or(String::new(), |(value, _)| value))
    }
    fn get_property_priority(&self, name: &str) -> OpResult<String> {
        if self.computed { return Ok(String::new()); }
        Ok(if css::declaration_value(&self.raw()?, name).map_err(css_error)?.is_some_and(|(_, important)| important) { "important" } else { "" }.into())
    }
    fn set_property(&self, name: &str, value: &str, priority: Option<&str>) -> OpResult<()> {
        let priority = priority.unwrap_or("");
        if !priority.is_empty() && !priority.eq_ignore_ascii_case("important") { return Ok(()); }
        let raw = css::set_declaration(&self.raw()?, name, value, !priority.is_empty()).map_err(css_error)?;
        self.write(&raw)
    }
    fn remove_property(&self, name: &str) -> OpResult<String> { let old = self.get_property_value(name)?; self.set_property(name, "", None)?; Ok(old) }
    #[getter]
    fn width(&self) -> OpResult<String> { self.get_property_value("width") }
    #[setter]
    fn set_width(&self, value: &str) -> OpResult<()> { self.set_property("width", value, None) }
    #[getter]
    fn height(&self) -> OpResult<String> { self.get_property_value("height") }
    #[setter]
    fn set_height(&self, value: &str) -> OpResult<()> { self.set_property("height", value, None) }
    #[getter]
    fn color(&self) -> OpResult<String> { self.get_property_value("color") }
    #[setter]
    fn set_color(&self, value: &str) -> OpResult<()> { self.set_property("color", value, None) }
    #[getter]
    fn background_color(&self) -> OpResult<String> { self.get_property_value("background-color") }
    #[setter]
    fn set_background_color(&self, value: &str) -> OpResult<()> { self.set_property("background-color", value, None) }
    #[getter]
    fn display(&self) -> OpResult<String> { self.get_property_value("display") }
    #[setter]
    fn set_display(&self, value: &str) -> OpResult<()> { self.set_property("display", value, None) }
    #[getter]
    fn font_size(&self) -> OpResult<String> { self.get_property_value("font-size") }
    #[setter]
    fn set_font_size(&self, value: &str) -> OpResult<()> { self.set_property("font-size", value, None) }
}

#[lumen_bind::op(name = "getComputedStyle")]
pub fn get_computed_style(ctx: &mut Ctx, node: &DomNode) -> OpResult<DomStyle> {
    node.realm.session.borrow_mut().computed_style(node.id).map_err(|error| OpError::new("InvalidStateError", format!("computed style failed: {error:?}")))?;
    Ok(DomStyle { realm: node.realm.clone(), node: node.id, computed: true, _owner: node.realm.wrap(ctx, node.id) })
}
