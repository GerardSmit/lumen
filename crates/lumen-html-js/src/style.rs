use super::*;
use lumen_html::{
    css,
    paint::{FontMetric, FontSizeAdjustValue, Rgba},
};

#[lumen_bind::class(name = "CSSStyleDeclaration", hint(js(webidl)))]
pub struct DomStyle {
    pub(crate) realm: Rc<DomRealm>,
    pub(crate) node: NodeId,
    pub(crate) computed: bool,
    pub(crate) _owner: Value,
}

impl DomStyle {
    pub(crate) fn adopt_node(&mut self, realm: Rc<DomRealm>, node: NodeId) {
        self.realm = realm;
        self.node = node;
    }
}

fn css_error(error: css::CssError) -> OpError {
    OpError::new(
        "InvalidStateError",
        format!("CSS error at {}: {}", error.offset, error.message),
    )
}
fn color(value: Rgba) -> String {
    if value.a == 255 {
        format!("rgb({}, {}, {})", value.r, value.g, value.b)
    } else {
        format!(
            "rgba({}, {}, {}, {})",
            value.r,
            value.g,
            value.b,
            value.a as f32 / 255.0
        )
    }
}

fn overflow(value: css::Overflow) -> &'static str {
    match value {
        css::Overflow::Visible => "visible",
        css::Overflow::Hidden => "hidden",
        css::Overflow::Scroll => "scroll",
        css::Overflow::Auto => "auto",
        css::Overflow::Clip => "clip",
    }
}

fn font_metric_name(metric: FontMetric) -> &'static str {
    match metric {
        FontMetric::ExHeight => "ex-height",
        FontMetric::CapHeight => "cap-height",
        FontMetric::ChWidth => "ch-width",
        FontMetric::IcWidth => "ic-width",
        FontMetric::IcHeight => "ic-height",
    }
}

fn computed_font_size_adjust(realm: &DomRealm, style: &css::Style) -> String {
    let Some(adjust) = style.font_spec().size_adjust else {
        return "none".into();
    };
    let value = match adjust.value {
        FontSizeAdjustValue::Number(value) => value,
        FontSizeAdjustValue::FromFont => {
            let Some(value) = realm
                .font_loading
                .primary_metric(style.font_spec(), adjust.metric)
            else {
                return "none".into();
            };
            value
        }
    };
    if !value.is_finite() || value < 0.0 {
        return "none".into();
    }
    if adjust.metric == FontMetric::ExHeight {
        value.to_string()
    } else {
        format!("{} {value}", font_metric_name(adjust.metric))
    }
}

impl DomStyle {
    fn raw(&self) -> OpResult<String> {
        let session = self.realm.session.borrow();
        match session.document().kind(self.node).map_err(dom_error)? {
            NodeKind::Element { attributes, .. } => Ok(attributes
                .iter()
                .find(|(name, _)| name == "style")
                .map_or(String::new(), |(_, value)| value.clone())),
            _ => Err(OpError::new("TypeError", "style requires an element")),
        }
    }
    fn write(&self, value: &str) -> OpResult<()> {
        if self.computed {
            return Err(OpError::new(
                "NoModificationAllowedError",
                "computed style is read-only",
            ));
        }
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .set_attribute(self.node, "style", value)
            .map_err(dom_error)
    }
}

#[lumen_bind::methods]
impl DomStyle {
    #[getter]
    fn css_text(&self) -> OpResult<String> {
        if self.computed {
            Ok(String::new())
        } else {
            self.raw()
        }
    }
    #[setter(coerce)]
    fn set_css_text(&self, value: &str) -> OpResult<()> {
        self.write(&css::cssom_declaration_text(value))
    }
    #[method(coerce)]
    fn get_property_value(&self, name: &str) -> OpResult<String> {
        if self.computed {
            let style = self
                .realm
                .session
                .borrow_mut()
                .computed_style(self.node)
                .map_err(|error| {
                    OpError::new(
                        "InvalidStateError",
                        format!("computed style failed: {error:?}"),
                    )
                })?;
            return Ok(match name {
                "display" => match style.display {
                    css::Display::Block => "block",
                    css::Display::Inline => "inline",
                    css::Display::InlineBlock => "inline-block",
                    css::Display::Flex => "flex",
                    css::Display::Grid => "grid",
                    css::Display::Table => "table",
                    css::Display::TableRowGroup => "table-row-group",
                    css::Display::TableRow => "table-row",
                    css::Display::TableCell => "table-cell",
                    css::Display::None => "none",
                }
                .into(),
                "color" => color(style.color),
                "background-color" | "background" => color(style.background),
                "width" => style
                    .width
                    .map_or("auto".into(), |value| format!("{value}px")),
                "height" => style
                    .height
                    .map_or("auto".into(), |value| format!("{value}px")),
                "margin" => format!("{}px", style.margin),
                "padding" => format!("{}px", style.padding),
                "font-size" => format!("{}px", style.font_size),
                "font-size-adjust" => computed_font_size_adjust(&self.realm, &style),
                "font-family" => style.font.families.as_ref().map_or_else(
                    || "sans-serif".into(),
                    |families| {
                        families
                            .iter()
                            .map(|family| {
                                format!("\"{}\"", family.replace('\\', "\\\\").replace('"', "\\\""))
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    },
                ),
                "font-weight" => style.font.weight.to_string(),
                "font-stretch" => format!("{}%", style.font.stretch),
                "font-style" => match style.font.style {
                    lumen_html::paint::FontStyle::Normal => "normal",
                    lumen_html::paint::FontStyle::Italic => "italic",
                    lumen_html::paint::FontStyle::Oblique => "oblique",
                }
                .into(),
                "border-radius" => style.border_radius_css_value(),
                "border-top-left-radius" => style.border_radius_corner_css_value(0),
                "border-top-right-radius" => style.border_radius_corner_css_value(1),
                "border-bottom-right-radius" => style.border_radius_corner_css_value(2),
                "border-bottom-left-radius" => style.border_radius_corner_css_value(3),
                "border-width" => format!("{}px", style.border_width),
                "border-color" => color(style.border_color),
                "overflow-x" => overflow(style.overflow_x).into(),
                "overflow-y" => overflow(style.overflow_y).into(),
                "overflow" => {
                    if style.overflow_x == style.overflow_y {
                        overflow(style.overflow_x).into()
                    } else {
                        format!(
                            "{} {}",
                            overflow(style.overflow_x),
                            overflow(style.overflow_y)
                        )
                    }
                }
                "background-clip" | "background-origin" => {
                    let values = if name == "background-clip" {
                        style.background_clip.as_ref()
                    } else {
                        style.background_origin.as_ref()
                    };
                    values.map_or_else(
                        || {
                            if name == "background-clip" {
                                "border-box".into()
                            } else {
                                "padding-box".into()
                            }
                        },
                        |values| {
                            values
                                .iter()
                                .map(|value| match value {
                                    lumen_html::paint::BackgroundBox::Border => "border-box",
                                    lumen_html::paint::BackgroundBox::Padding => "padding-box",
                                    lumen_html::paint::BackgroundBox::Content => "content-box",
                                    lumen_html::paint::BackgroundBox::Text => "text",
                                    lumen_html::paint::BackgroundBox::BorderArea => "border-area",
                                    lumen_html::paint::BackgroundBox::BorderAreaText => {
                                        "border-area text"
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join(", ")
                        },
                    )
                }
                "background-attachment" => style.background_attachment.as_ref().map_or_else(
                    || "scroll".into(),
                    |values| {
                        values
                            .iter()
                            .map(|value| match value {
                                css::BackgroundAttachment::Scroll => "scroll",
                                css::BackgroundAttachment::Fixed => "fixed",
                                css::BackgroundAttachment::Local => "local",
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    },
                ),
                "opacity" => style.opacity.to_string(),
                "line-height" => match style.line_height {
                    css::LineHeight::Normal => "normal".into(),
                    css::LineHeight::Number(value) => value.to_string(),
                    css::LineHeight::Pixels(value) => format!("{value}px"),
                },
                _ => String::new(),
            });
        }
        Ok(css::declaration_value(&self.raw()?, name)
            .map_err(css_error)?
            .map_or(String::new(), |(value, _)| value))
    }
    #[method(coerce)]
    fn get_property_priority(&self, name: &str) -> OpResult<String> {
        if self.computed {
            return Ok(String::new());
        }
        Ok(if css::declaration_value(&self.raw()?, name)
            .map_err(css_error)?
            .is_some_and(|(_, important)| important)
        {
            "important"
        } else {
            ""
        }
        .into())
    }
    #[method(coerce)]
    fn set_property(&self, name: &str, value: &str, priority: Option<&str>) -> OpResult<()> {
        if self.computed {
            return Err(OpError::new(
                "NoModificationAllowedError",
                "computed styles are read-only",
            ));
        }
        // CSSOM ignores invalid values and unsupported properties atomically.
        // An empty value removes the declaration even when priority is invalid.
        if !value.is_empty() && !css::supports_declaration(name, value) {
            return Ok(());
        }
        let priority = priority.unwrap_or("");
        if !value.is_empty() && !priority.is_empty() && !priority.eq_ignore_ascii_case("important")
        {
            return Ok(());
        }
        let Ok(raw) = css::set_declaration(&self.raw()?, name, value, !priority.is_empty()) else {
            return Ok(());
        };
        self.write(&raw)
    }
    #[method(coerce)]
    fn remove_property(&self, name: &str) -> OpResult<String> {
        let old = self.get_property_value(name)?;
        self.set_property(name, "", None)?;
        Ok(old)
    }
    #[getter]
    fn width(&self) -> OpResult<String> {
        self.get_property_value("width")
    }
    #[setter(coerce)]
    fn set_width(&self, value: &str) -> OpResult<()> {
        self.set_property("width", value, None)
    }
    #[getter]
    fn height(&self) -> OpResult<String> {
        self.get_property_value("height")
    }
    #[setter(coerce)]
    fn set_height(&self, value: &str) -> OpResult<()> {
        self.set_property("height", value, None)
    }
    #[getter]
    fn color(&self) -> OpResult<String> {
        self.get_property_value("color")
    }
    #[setter(coerce)]
    fn set_color(&self, value: &str) -> OpResult<()> {
        self.set_property("color", value, None)
    }
    #[getter]
    fn background_color(&self) -> OpResult<String> {
        self.get_property_value("background-color")
    }
    #[setter(coerce)]
    fn set_background_color(&self, value: &str) -> OpResult<()> {
        self.set_property("background-color", value, None)
    }
    #[getter]
    fn display(&self) -> OpResult<String> {
        self.get_property_value("display")
    }
    #[setter(coerce)]
    fn set_display(&self, value: &str) -> OpResult<()> {
        self.set_property("display", value, None)
    }
    #[getter]
    fn font_size(&self) -> OpResult<String> {
        self.get_property_value("font-size")
    }
    #[setter(coerce)]
    fn set_font_size(&self, value: &str) -> OpResult<()> {
        self.set_property("font-size", value, None)
    }
    #[getter]
    fn font_size_adjust(&self) -> OpResult<String> {
        self.get_property_value("font-size-adjust")
    }
    #[setter(coerce)]
    fn set_font_size_adjust(&self, value: &str) -> OpResult<()> {
        self.set_property("font-size-adjust", value, None)
    }
    #[getter]
    fn font_family(&self) -> OpResult<String> {
        self.get_property_value("font-family")
    }
    #[setter(coerce)]
    fn set_font_family(&self, value: &str) -> OpResult<()> {
        self.set_property("font-family", value, None)
    }
    #[getter]
    fn font_weight(&self) -> OpResult<String> {
        self.get_property_value("font-weight")
    }
    #[setter(coerce)]
    fn set_font_weight(&self, value: &str) -> OpResult<()> {
        self.set_property("font-weight", value, None)
    }
    #[getter]
    fn font_style(&self) -> OpResult<String> {
        self.get_property_value("font-style")
    }
    #[setter(coerce)]
    fn set_font_style(&self, value: &str) -> OpResult<()> {
        self.set_property("font-style", value, None)
    }
    #[getter]
    fn font(&self) -> OpResult<String> {
        self.get_property_value("font")
    }
    #[setter(coerce)]
    fn set_font(&self, value: &str) -> OpResult<()> {
        self.set_property("font", value, None)
    }
}

#[lumen_bind::op(name = "getComputedStyle")]
pub fn get_computed_style(ctx: &mut Ctx, node: &DomNode) -> OpResult<DomStyle> {
    node.realm
        .session
        .borrow_mut()
        .computed_style(node.id)
        .map_err(|error| {
            OpError::new(
                "InvalidStateError",
                format!("computed style failed: {error:?}"),
            )
        })?;
    Ok(DomStyle {
        realm: node.realm.clone(),
        node: node.id,
        computed: true,
        _owner: node.realm.wrap(ctx, node.id),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    #[test]
    fn cssom_setters_ignore_invalid_values_and_priorities_atomically() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<div></div>", 64).unwrap();
        let result = engine
            .eval_value(
                r#"
            const s = document.querySelector('div').style;
            s.setProperty('width', '10px', 'important');
            s.setProperty('width', 'bad');
            s.setProperty('width', '20px', 'urgent');
            s.setProperty('width', '30px !important');
            s.setProperty('width', 'var(not-custom)');
            const preserved = s.width === '10px' && s.getPropertyPriority('width') === 'important';
            s.setProperty('width', '', 'urgent');
            s.setProperty('width;bad', '');
            s.setProperty('height', 'var(--missing, var(--fallback, 12px))');
            preserved && s.width === '' && s.height === 'var(--missing, var(--fallback, 12px))'
        "#,
            )
            .unwrap()
            .ok()
            .unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn css_text_filters_invalid_declarations_and_preserves_priority() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<div></div>", 64).unwrap();
        let result = engine.eval_value(r#"
            const s = document.querySelector('div').style;
            s.cssText = 'width:10px! /**/ important /*tail*/;width:20px;height:bad;unknown:yes;--Case:red;--case:blue;color:rgb(1,2,3)';
            const valid = s.width === '10px' && s.height === '' && s.getPropertyValue('unknown') === '' &&
                s.getPropertyValue('--Case') === 'red' && s.getPropertyValue('--case') === 'blue';
            s.cssText = 'height:12px;width:calc(';
            valid && s.height === '12px' && s.width === ''
        "#).unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn computed_overflow_keeps_axis_values_and_attachment_layers() {
        let mut engine = Engine::new();
        install(
            engine.ctx(),
            "<div style='overflow:clip hidden;background-attachment:fixed,local'></div>",
            64,
        )
        .unwrap();
        let result = engine.eval_value(r#"
            const s = getComputedStyle(document.querySelector('div'));
            s.getPropertyValue('overflow-x') === 'hidden' && s.getPropertyValue('overflow-y') === 'hidden' &&
            s.getPropertyValue('overflow') === 'hidden' && s.getPropertyValue('background-attachment') === 'fixed, local'
        "#).unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn computed_border_radius_serializes_elliptical_values_and_font_units() {
        let mut engine = Engine::new();
        install(
            engine.ctx(),
            "<div style='font-size:16px;border-radius:calc(10px + 25%) 1em 25% 25px / calc(20px + 25%) 1em 25% 25px'></div>",
            64,
        )
        .unwrap();
        let result = engine
            .eval_value(
                r#"getComputedStyle(document.querySelector('div')).getPropertyValue('border-radius') ===
                    'calc(25% + 10px) 16px 25% 25px / calc(25% + 20px) 16px 25% 25px'"#,
            )
            .unwrap()
            .ok()
            .unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn computed_physical_border_radius_longhands_serialize_each_corner() {
        let mut engine = Engine::new();
        install(
            engine.ctx(),
            "<div style='font-size:20px;border-top-left-radius:1em 25%;border-top-right-radius:10%;border-bottom-right-radius:2rem 10px;border-bottom-left-radius:calc(10px + 5%)'></div>",
            64,
        )
        .unwrap();
        let result = engine
            .eval_value(
                r#"(() => {
                    const s = getComputedStyle(document.querySelector('div'));
                    return s.getPropertyValue('border-top-left-radius') === '20px 25%' &&
                        s.getPropertyValue('border-top-right-radius') === '10%' &&
                        s.getPropertyValue('border-bottom-right-radius') === '32px 10px' &&
                        s.getPropertyValue('border-bottom-left-radius') === 'calc(5% + 10px)';
                })()"#,
            )
            .unwrap()
            .ok()
            .unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }
}
