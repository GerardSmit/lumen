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
    fn declaration_value(&self, name: &str) -> OpResult<Option<(String, bool)>> {
        let session = self.realm.session.borrow();
        match session.document().kind(self.node).map_err(dom_error)? {
            NodeKind::Element { .. } => {
                let raw = session
                    .document()
                    .get_attribute_ns_ref(self.node, None, "style")
                    .map_err(dom_error)?
                    .unwrap_or("");
                css::declaration_value(raw, name).map_err(css_error)
            }
            _ => Err(OpError::new("TypeError", "style requires an element")),
        }
    }

    pub(crate) fn adopt_node(&mut self, realm: Rc<DomRealm>, node: NodeId) {
        self.realm = realm;
        self.node = node;
    }
}

pub(crate) fn computed_property_value(
    realm: &Rc<DomRealm>,
    node: NodeId,
    property: &str,
) -> OpResult<String> {
    let property = if property.starts_with("--") {
        property.to_owned()
    } else {
        property.to_ascii_lowercase()
    };
    DomStyle {
        realm: realm.clone(),
        node,
        computed: true,
        _owner: Value::Undefined,
    }
    .get_property_value(&property)
}

fn css_error(error: css::CssError) -> OpError {
    OpError::new(
        "InvalidStateError",
        format!("CSS error at {}: {}", error.offset, error.message),
    )
}

pub(crate) fn canonical_content_property(name: &str, value: String) -> String {
    // CSS-wide keywords and deferred variable values use the shared declaration
    // parser's validated representation rather than generated-content grammar.
    css::serialize_cssom_property_value(name, &value).unwrap_or(value)
}

/// Install camel-case CSSStyleDeclaration aliases from the shared CSS
/// property registry. Accessors delegate through the receiver's CSSOM methods,
/// so stylesheet rule declarations and inline styles use the same path.
pub(crate) fn install_css_property_aliases(ctx: &mut Ctx) -> OpResult<()> {
    static ALIASES: std::sync::OnceLock<Vec<(&'static str, Option<String>)>> =
        std::sync::OnceLock::new();
    let aliases = ALIASES.get_or_init(|| {
        css::cssom_property_names()
            .into_iter()
            .filter(|property| !property.starts_with("--"))
            .map(|property| (property, camel_case_alias(property)))
            .collect()
    });
    let constructor = ctx.class_constructor::<DomStyle>();
    let prototype = ctx
        .get_member(&constructor, "prototype")
        .map_err(|_| OpError::new("Error", "CSSStyleDeclaration prototype unavailable"))?;
    for (property, camel) in aliases {
        define_css_property_accessor(ctx, &prototype, property, property);
        if let Some(camel) = camel {
            define_css_property_accessor(ctx, &prototype, camel, property);
        }
    }
    Ok(())
}

fn camel_case_alias(property: &str) -> Option<String> {
    let mut out = String::with_capacity(property.len());
    let mut chars = property.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '-' && matches!(chars.peek(), Some('a'..='z')) {
            out.push(chars.next().unwrap().to_ascii_uppercase());
        } else {
            out.push(c);
        }
    }
    (out != property).then_some(out)
}

fn define_css_property_accessor(
    ctx: &mut Ctx,
    prototype: &Value,
    name: &str,
    property: &'static str,
) {
    let getter = ctx.new_native_fn(
        "get",
        0,
        std::rc::Rc::new(move |ctx: &mut Ctx, this: Value, _args: &[Value]| {
            let method = ctx
                .get_member(&this, "getPropertyValue")
                .map_err(lumen::embed::abrupt_value)?;
            ctx.invoke(method, this, &[Value::str(property)])
        }),
    );
    let setter = ctx.new_native_fn(
        "set",
        1,
        std::rc::Rc::new(move |ctx: &mut Ctx, this: Value, args: &[Value]| {
            let method = ctx
                .get_member(&this, "setProperty")
                .map_err(lumen::embed::abrupt_value)?;
            let value = args.first().cloned().unwrap_or(Value::Undefined);
            ctx.invoke(method, this, &[Value::str(property), value])?;
            Ok(Value::Undefined)
        }),
    );
    ctx.define_accessor_value(prototype, name, Some(getter), Some(setter), true);
}

fn counter_value(
    property: css::CounterProperty,
    directives: Option<&[css::CounterDirective]>,
) -> OpResult<String> {
    css::serialize_counter_directives(property, directives).ok_or_else(|| {
        OpError::new(
            "InvalidStateError",
            "computed counter declaration is invalid",
        )
    })
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
            NodeKind::Element { .. } => Ok(session
                .document()
                .get_attribute_ns_ref(self.node, None, "style")
                .map_err(dom_error)?
                .unwrap_or("")
                .to_owned()),
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
            .set_attribute_ns(self.node, None, "style", value)
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
                    css::Display::FlowRoot => "flow-root",
                    css::Display::Inline => "inline",
                    css::Display::InlineBlock => "inline-block",
                    css::Display::Contents => "contents",
                    css::Display::ListItem => "list-item",
                    css::Display::Flex => "flex",
                    css::Display::Grid => "grid",
                    css::Display::Table => "table",
                    css::Display::TableCaption => "table-caption",
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
                "margin-top" | "margin-right" | "margin-bottom" | "margin-left" => {
                    let side = match name {
                        "margin-top" => 0,
                        "margin-right" => 1,
                        "margin-bottom" => 2,
                        _ => 3,
                    };
                    if style.margin_auto[side] {
                        "auto".into()
                    } else {
                        format!("{}px", style.margin_sides[side])
                    }
                }
                "padding" => format!("{}px", style.padding),
                "font-size" => format!("{}px", style.font_size),
                "font-size-adjust" => computed_font_size_adjust(&self.realm, &style),
                "content" => css::serialize_generated_content(&style.generated_content()),
                "color-scheme" => style.color_scheme.as_deref().unwrap_or("normal").to_owned(),
                "transition-duration" => {
                    css::serialize_transition_time_values(style.transition_duration.as_deref())
                }
                "transition-delay" => {
                    css::serialize_transition_time_values(style.transition_delay.as_deref())
                }
                "quotes" => css::serialize_quotes(style.quotes()),
                "counter-reset" => {
                    counter_value(css::CounterProperty::Reset, style.counter_reset.as_deref())?
                }
                "counter-increment" => counter_value(
                    css::CounterProperty::Increment,
                    style.counter_increment.as_deref(),
                )?,
                "counter-set" => {
                    counter_value(css::CounterProperty::Set, style.counter_set.as_deref())?
                }
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
                "pointer-events" => if style.pointer_events_auto { "auto" } else { "none" }.into(),
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
                property if property.starts_with("--") => style
                    .custom_properties()
                    .iter()
                    .find(|(name, _)| name == property)
                    .and_then(|(_, value)| value.as_deref())
                    .unwrap_or("")
                    .to_owned(),
                "line-height" => match style.line_height {
                    css::LineHeight::Normal => "normal".into(),
                    css::LineHeight::Number(value) => value.to_string(),
                    css::LineHeight::Pixels(value) => format!("{value}px"),
                },
                _ => String::new(),
            });
        }
        Ok(self
            .declaration_value(name)?
            .map_or(String::new(), |(value, _)| {
                canonical_content_property(name, value)
            }))
    }
    #[method(coerce)]
    fn get_property_priority(&self, name: &str) -> OpResult<String> {
        if self.computed {
            return Ok(String::new());
        }
        Ok(if self
            .declaration_value(name)?
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
    fn content(&self) -> OpResult<String> {
        self.get_property_value("content")
    }
    #[setter(coerce)]
    fn set_content(&self, value: &str) -> OpResult<()> {
        self.set_property("content", value, None)
    }
    #[getter]
    fn counter_reset(&self) -> OpResult<String> {
        self.get_property_value("counter-reset")
    }
    #[setter(coerce)]
    fn set_counter_reset(&self, value: &str) -> OpResult<()> {
        self.set_property("counter-reset", value, None)
    }
    #[getter]
    fn counter_increment(&self) -> OpResult<String> {
        self.get_property_value("counter-increment")
    }
    #[setter(coerce)]
    fn set_counter_increment(&self, value: &str) -> OpResult<()> {
        self.set_property("counter-increment", value, None)
    }
    #[getter]
    fn counter_set(&self) -> OpResult<String> {
        self.get_property_value("counter-set")
    }
    #[setter(coerce)]
    fn set_counter_set(&self, value: &str) -> OpResult<()> {
        self.set_property("counter-set", value, None)
    }
    #[getter]
    fn quotes(&self) -> OpResult<String> {
        self.get_property_value("quotes")
    }
    #[setter(coerce)]
    fn set_quotes(&self, value: &str) -> OpResult<()> {
        self.set_property("quotes", value, None)
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
    fn computed_margin_sides_follow_live_native_style_and_animation_values() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<div style='margin:1px 2px 3px 4px'></div>", 128).unwrap();
        let result = engine.eval_value(r#"(() => {
            const element=document.querySelector('div'), style=getComputedStyle(element);
            function expect(actual, expected, label) {
                if (actual!==expected) throw new Error(label+': '+actual+' !== '+expected);
            }
            expect(style.marginTop,'1px','top');
            expect(style.getPropertyValue('margin-right'),'2px','right');
            expect(style.marginBottom,'3px','bottom');
            expect(style.marginLeft,'4px','left');
            element.style.marginLeft='-7.5px';
            expect(style.marginLeft,'-7.5px','live left');
            const animation=element.animate({marginLeft:['0px','300px']},{duration:1000,fill:'both'});
            animation.currentTime=500;
            expect(style.marginLeft,'150px','native animation interpolation');
            animation.cancel();
            expect(style.marginLeft,'-7.5px','underlying left restored');
            element.style.cssText='display:none;font-size:20px;margin:1em auto';
            expect(style.marginTop,'20px','shared font-relative computed length');
            expect(style.marginRight,'auto','unboxed computed automatic margin');
            expect(style.marginLeft,'auto','automatic margin preserved');
            return true;
        })()"#).unwrap().unwrap_or_else(|error| {
            let message=match engine.ctx().member_get(&error,"message") {
                Ok(Value::Str(message)) => message.to_string(),
                _ => "non-string script exception".into(),
            };
            panic!("computed margin contract: {message}");
        });
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn counter_cssom_canonicalizes_defaults_and_keeps_computed_values_live() {
        let mut runtime = lumen_runtime::Runtime::new();
        let engine = runtime.engine();
        install(engine.ctx(), "<main><div></div></main>", 128).unwrap();
        let result = engine.eval_value(r#"(() => {
            const element=document.querySelector('main'), style=element.style;
            function expect(actual, expected, label) {
                if (actual!==expected) throw new Error(label+': '+actual+' !== '+expected);
            }
            const computed=getComputedStyle(element);
            expect(computed.counterReset,'none','initial reset');
            expect(computed.counterIncrement,'none','initial increment');
            expect(computed.counterSet,'none','initial set');
            style.counterReset='chapter section -2';
            style.counterIncrement='chapter section 2';
            style.counterSet='chapter 9 section';
            expect(style.counterReset,'chapter 0 section -2','reset defaults');
            expect(style.counterIncrement,'chapter 1 section 2','increment defaults');
            expect(style.counterSet,'chapter 9 section 0','set defaults');
            expect(computed.counterReset,style.counterReset,'computed reset');
            expect(computed.counterIncrement,style.counterIncrement,'computed increment');
            expect(computed.counterSet,style.counterSet,'computed set');
            expect(getComputedStyle(element.firstElementChild).counterReset,'none','counter reset not inherited');
            style.setProperty('counter-increment','chapter 3','important');
            expect(style.getPropertyPriority('counter-increment'),'important','priority survives canonicalization');
            expect(computed.counterIncrement,'chapter 3','live computed increment');
            style.counterIncrement='none 2';
            expect(style.counterIncrement,'chapter 3','invalid value keeps prior declaration');
            expect(style.removeProperty('counter-reset'),'chapter 0 section -2','removed canonical value');
            expect(computed.counterReset,'none','removal updates computed reset');
            return true;
        })()"#).unwrap().unwrap_or_else(|error| {
            let message=match engine.ctx().member_get(&error,"message") {
                Ok(Value::Str(message)) => message.to_string(),
                _ => "non-string script exception".into(),
            };
            panic!("counter CSSOM contract: {message}");
        });
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn generated_content_cssom_reflects_canonical_values_and_live_computed_styles() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main style='display:none'></main>", 128).unwrap();
        let result = engine
            .eval_value(
                r#"(() => {
            const el=document.querySelector('main'), style=el.style;
            function expect(value, wanted, label) {
                if (value!==wanted) throw new Error(label+': '+value+' !== '+wanted);
            }
            style.content="open-quote 'hello' counter(chapter, DECIMAL) / 'alt'";
            const content='open-quote "hello" counter(chapter) / "alt"';
            expect(style.content,content,'content IDL');
            expect(style.getPropertyValue('content'),content,'content declaration');
            style.quotes="'outer' 'close' 'inner' 'end'";
            expect(style.quotes,'"outer" "close" "inner" "end"','quotes IDL');
            const computed=getComputedStyle(el);
            expect(computed.content,content,'computed content');
            expect(computed.quotes,style.quotes,'computed quotes');
            style.content='counter(';
            expect(style.content,content,'invalid content preserves old value');
            style.content="'changed'";
            expect(computed.content,'"changed"','live computed content');
            style.quotes='none';
            expect(computed.quotes,'none','live computed quotes');
            return true;
        })()"#,
            )
            .unwrap()
            .unwrap_or_else(|error| {
                let message = match engine.ctx().member_get(&error, "message") {
                    Ok(Value::Str(message)) => message.to_string(),
                    _ => "non-string script exception".into(),
                };
                panic!("generated-content CSSOM regression: {message}");
            });
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn inline_styles_and_token_lists_use_only_null_namespace_attributes() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        let result = engine.eval_value(r#"(() => {
            const ns = 'https://example.test/attributes';
            for (const el of [document.createElement('div'), document.createElementNS('http://www.w3.org/2000/svg', 'g')]) {
                el.setAttributeNS(ns, 'style', 'color: red');
                el.setAttributeNS(null, 'style', 'color: green');
                el.setAttributeNS(ns, 'style', 'color: blue');
                if (el.style.color !== 'green') return false;
                el.style.color = 'purple';
                if (el.getAttributeNS(ns, 'style') !== 'color: blue' || !el.getAttributeNS(null, 'style').includes('purple')) return false;
                el.setAttributeNS(ns, 'class', 'foreign');
                el.classList.add('native');
                if (el.classList.contains('foreign') || !el.classList.contains('native') || el.getAttributeNS(ns, 'class') !== 'foreign') return false;
                document.querySelector('main').append(el);
                if (document.getElementsByClassName('foreign').length !== 0) return false;
                const observer = new MutationObserver(() => {});
                observer.observe(el, {attributes: true});
                if (el.classList.toggle('native', true) !== true || el.classList.toggle('absent', false) !== false || observer.takeRecords().length !== 0) return false;
                el.classList.toggle('native', false);
                const records = observer.takeRecords();
                if (records.length !== 1 || records[0].attributeNamespace !== null) return false;
                observer.disconnect();
            }
            return document.getElementsByClassName('native').length === 0;
        })()"#).unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

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
