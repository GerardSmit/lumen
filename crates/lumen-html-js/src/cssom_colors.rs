//! Typed OM color values (CSSColorValue and its coordinate subclasses).
use super::*;
use css::typed_colors::{parse_typed_color, TypedColor};
use css::typed_numeric::{NumericType, NumericUnit};
use lumen_common::color::{Color, ColorSpace};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    Rgb,
    Hsl,
    Hwb,
    Lab,
    Lch,
    Oklab,
    Oklch,
    Color,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Channel {
    Rgb,
    Percent,
    Number,
    Angle,
    Hue,
}

impl Family {
    fn function(self) -> &'static str {
        match self {
            Self::Rgb => "rgb",
            Self::Hsl => "hsl",
            Self::Hwb => "hwb",
            Self::Lab => "lab",
            Self::Lch => "lch",
            Self::Oklab => "oklab",
            Self::Oklch => "oklch",
            Self::Color => "color",
        }
    }
    fn channels(self) -> [(&'static str, Channel); 4] {
        match self {
            Self::Rgb => [("r", Channel::Rgb), ("g", Channel::Rgb), ("b", Channel::Rgb), ("alpha", Channel::Percent)],
            Self::Hsl => [("h", Channel::Angle), ("s", Channel::Percent), ("l", Channel::Percent), ("alpha", Channel::Percent)],
            Self::Hwb => [("h", Channel::Hue), ("w", Channel::Percent), ("b", Channel::Percent), ("alpha", Channel::Percent)],
            Self::Lab | Self::Oklab => [("l", Channel::Percent), ("a", Channel::Number), ("b", Channel::Number), ("alpha", Channel::Percent)],
            Self::Lch | Self::Oklch => [("l", Channel::Percent), ("c", Channel::Percent), ("h", Channel::Angle), ("alpha", Channel::Percent)],
            Self::Color => [("0", Channel::Number), ("1", Channel::Number), ("2", Channel::Number), ("alpha", Channel::Percent)],
        }
    }
}

#[lumen_bind::class(name = "CSSColorValue", extends = DomCssStyleValue, hint(js(webidl)))]
pub struct DomCssColorValue {
    base: DomCssStyleValue,
    family: Family,
    data_key: Value,
}

macro_rules! color_class {
    ($rust:ident, $name:literal) => {
        #[lumen_bind::class(name = $name, extends = DomCssColorValue, hint(js(webidl)))]
        pub struct $rust {
            base: DomCssColorValue,
        }
    };
}
color_class!(DomCssRgb, "CSSRGB");
color_class!(DomCssHsl, "CSSHSL");
color_class!(DomCssHwb, "CSSHWB");
color_class!(DomCssLab, "CSSLab");
color_class!(DomCssLch, "CSSLCH");
color_class!(DomCssOklab, "CSSOKLab");
color_class!(DomCssOklch, "CSSOKLCH");
color_class!(DomCssColor, "CSSColor");

struct ColorConstructor(Value);

macro_rules! ctor_result {
    ($($class:ty),*) => {$(
        impl CtorRet<JsHost, $class> for ColorConstructor {
            fn into_ctor(self, _cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
                Ok(self.0)
            }
        }
    )*};
}
ctor_result!(DomCssRgb, DomCssHsl, DomCssHwb, DomCssLab, DomCssLch, DomCssOklab, DomCssOklch, DomCssColor);

fn syntax_error(ctx: &mut Ctx, message: &str) -> OpError {
    crate::error_reporting::dom_exception(ctx, "SyntaxError", message)
}

fn coerce(ctx: &mut Ctx, value: &Value, channel: Channel) -> OpResult<Value> {
    if ctx.with_instance::<DomCssNumericValue, _>(value, |_| ()).is_ok() {
        let kind = numeric_expression_from_value(ctx, value)?.numeric_type();
        let number = NumericType::from_unit(NumericUnit::Number);
        let percent = NumericType::from_unit(NumericUnit::Percent);
        let angle = NumericType::from_unit(NumericUnit::Deg);
        let accepted = match channel {
            Channel::Rgb | Channel::Number => kind == Some(number) || kind == Some(percent),
            Channel::Percent => kind == Some(percent),
            Channel::Angle | Channel::Hue => kind == Some(angle),
        };
        return if accepted { Ok(value.clone()) } else { Err(syntax_error(ctx, "color channel has an incompatible numeric type")) };
    }
    if channel == Channel::Hue {
        return Err(OpError::type_error("hue must be a CSSNumericValue"));
    }
    if let Value::Num(number) = value {
        return Ok(match channel {
            Channel::Rgb | Channel::Percent => unit_value(ctx, number * 100.0, NumericUnit::Percent),
            Channel::Number => unit_value(ctx, *number, NumericUnit::Number),
            Channel::Angle | Channel::Hue => unit_value(ctx, *number, NumericUnit::Deg),
        });
    }
    let text = match ctx.with_instance::<DomCssKeywordValue, _>(value, |keyword| keyword.base.serialized_value.borrow().clone()) {
        Ok(text) => text,
        Err(_) => ctx.coerce_string(value).map_err(OpError::thrown)?.to_string(),
    };
    if channel == Channel::Angle || text.eq_ignore_ascii_case("none") {
        let text = if text.eq_ignore_ascii_case("none") { "none".to_owned() } else { text };
        return Ok(ctx.new_instance(keyword_value(text)));
    }
    Err(syntax_error(ctx, "color channel must be a numeric value or none"))
}

fn channel_text(ctx: &mut Ctx, value: &Value) -> OpResult<String> {
    if let Ok(text) = ctx.with_instance::<DomCssKeywordValue, _>(value, |keyword| keyword.base.serialized_value.borrow().clone()) {
        return Ok(css::serialize_identifier(&text));
    }
    numeric_value_text(ctx, value)
}

fn data(ctx: &mut Ctx, this: &Value) -> OpResult<Value> {
    let key = ctx.with_instance::<DomCssColorValue, _>(this, |color| color.data_key.clone())?;
    ctx.reflect_get(this, &key, this).map_err(OpError::thrown)
}

fn slot(ctx: &mut Ctx, this: &Value, name: &str) -> OpResult<Value> {
    let data = data(ctx, this)?;
    ctx.member_get(&data, name).map_err(OpError::thrown)
}

fn refresh(ctx: &mut Ctx, this: &Value) -> OpResult<()> {
    let family = ctx.with_instance::<DomCssColorValue, _>(this, |color| color.family)?;
    let mut text = format!("{}(", family.function());
    if family == Family::Color {
        let space = slot(ctx, this, "space")?;
        text.push_str(&channel_text(ctx, &space)?);
        let channels = slot(ctx, this, "channels")?;
        let length = ctx.with_instance::<DomCssNumericArray, _>(&channels, |array| array.length).unwrap_or(0);
        for index in 0..length {
            let value = ctx.member_get(&channels, &index.to_string()).map_err(OpError::thrown)?;
            text.push(' ');
            text.push_str(&channel_text(ctx, &value)?);
        }
    } else {
        for (index, (name, _)) in family.channels().iter().take(3).enumerate() {
            if index != 0 {
                text.push(' ');
            }
            let value = slot(ctx, this, name)?;
            text.push_str(&channel_text(ctx, &value)?);
        }
    }
    let alpha = slot(ctx, this, "alpha")?;
    let opaque = unit_parts_of(ctx, &alpha).is_some_and(|(value, unit)| (unit == NumericUnit::Percent && value == 100.0) || (unit == NumericUnit::Number && value == 1.0));
    if !opaque {
        text.push_str(" / ");
        text.push_str(&channel_text(ctx, &alpha)?);
    }
    text.push(')');
    ctx.with_instance::<DomCssColorValue, _>(this, |color| *color.base.serialized_value.borrow_mut() = text)
}

fn unit_parts_of(ctx: &mut Ctx, value: &Value) -> Option<(f64, NumericUnit)> {
    ctx.with_instance::<DomCssNumericValue, _>(value, |numeric| numeric.expression.is_none().then(|| (numeric.value.get(), numeric.unit))).ok().flatten()
}

fn build(ctx: &mut Ctx, family: Family, values: Vec<(&str, Value)>) -> OpResult<Value> {
    let key = ctx.new_symbol(Some("CSSColorValue.data".into()));
    let base = DomCssColorValue {
        base: DomCssStyleValue { serialized_value: RefCell::new(String::new()) },
        family,
        data_key: key.clone(),
    };
    let value = match family {
        Family::Rgb => ctx.new_instance(DomCssRgb { base }),
        Family::Hsl => ctx.new_instance(DomCssHsl { base }),
        Family::Hwb => ctx.new_instance(DomCssHwb { base }),
        Family::Lab => ctx.new_instance(DomCssLab { base }),
        Family::Lch => ctx.new_instance(DomCssLch { base }),
        Family::Oklab => ctx.new_instance(DomCssOklab { base }),
        Family::Oklch => ctx.new_instance(DomCssOklch { base }),
        Family::Color => ctx.new_instance(DomCssColor { base }),
    };
    let object = ctx.new_object_with_proto(&Value::Null);
    for (name, field) in values {
        ctx.create_data_property(&object, name, field).map_err(OpError::thrown)?;
    }
    define_data_property(ctx, &value, key, object, false, false, false).map_err(OpError::thrown)?;
    refresh(ctx, &value)?;
    Ok(value)
}

fn construct(ctx: &mut Ctx, family: Family, channels: [Value; 3], alpha: Option<Value>) -> OpResult<ColorConstructor> {
    let kinds = family.channels();
    let mut values = Vec::with_capacity(4);
    for (index, channel) in channels.iter().enumerate() {
        values.push((kinds[index].0, coerce(ctx, channel, kinds[index].1)?));
    }
    let alpha = match alpha {
        Some(Value::Undefined) | None => unit_value(ctx, 100.0, NumericUnit::Percent),
        Some(alpha) => coerce(ctx, &alpha, Channel::Percent)?,
    };
    values.push(("alpha", alpha));
    Ok(ColorConstructor(build(ctx, family, values)?))
}

fn set_channel(ctx: &mut Ctx, this: &Value, name: &str, channel: Channel, value: Value) -> OpResult<()> {
    let value = coerce(ctx, &value, channel)?;
    let object = data(ctx, this)?;
    ctx.create_data_property(&object, name, value).map_err(OpError::thrown)?;
    refresh(ctx, this)
}

macro_rules! color_methods {
    ($class:ty, $family:expr, $a:ident, $set_a:ident, $an:literal, $b:ident, $set_b:ident, $bn:literal, $c:ident, $set_c:ident, $cn:literal) => {
        #[lumen_bind::methods]
        impl $class {
            #[constructor]
            fn new(ctx: &mut Ctx, first: Value, second: Value, third: Value, alpha: Option<Value>) -> OpResult<ColorConstructor> {
                construct(ctx, $family, [first, second, third], alpha)
            }
            #[getter]
            fn $a(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
                slot(ctx, &this.0, $an)
            }
            #[setter]
            fn $set_a(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
                set_channel(ctx, &this.0, $an, $family.channels()[0].1, value)
            }
            #[getter]
            fn $b(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
                slot(ctx, &this.0, $bn)
            }
            #[setter]
            fn $set_b(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
                set_channel(ctx, &this.0, $bn, $family.channels()[1].1, value)
            }
            #[getter]
            fn $c(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
                slot(ctx, &this.0, $cn)
            }
            #[setter]
            fn $set_c(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
                set_channel(ctx, &this.0, $cn, $family.channels()[2].1, value)
            }
            #[getter]
            fn alpha(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
                slot(ctx, &this.0, "alpha")
            }
            #[setter]
            fn set_alpha(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
                set_channel(ctx, &this.0, "alpha", Channel::Percent, value)
            }
        }
    };
}

color_methods!(DomCssRgb, Family::Rgb, r, set_r, "r", g, set_g, "g", b, set_b, "b");
color_methods!(DomCssHsl, Family::Hsl, h, set_h, "h", s, set_s, "s", l, set_l, "l");
color_methods!(DomCssHwb, Family::Hwb, h, set_h, "h", w, set_w, "w", b, set_b, "b");
color_methods!(DomCssLab, Family::Lab, l, set_l, "l", a, set_a, "a", b, set_b, "b");
color_methods!(DomCssLch, Family::Lch, l, set_l, "l", c, set_c, "c", h, set_h, "h");
color_methods!(DomCssOklab, Family::Oklab, l, set_l, "l", a, set_a, "a", b, set_b, "b");
color_methods!(DomCssOklch, Family::Oklch, l, set_l, "l", c, set_c, "c", h, set_h, "h");

fn keyword_of(ctx: &mut Ctx, value: &Value) -> OpResult<Value> {
    let text = match ctx.with_instance::<DomCssKeywordValue, _>(value, |keyword| keyword.base.serialized_value.borrow().clone()) {
        Ok(text) => text,
        Err(_) => ctx.coerce_string(value).map_err(OpError::thrown)?.to_string(),
    };
    if text.is_empty() {
        return Err(OpError::type_error("CSSKeywordValue value must not be empty"));
    }
    Ok(ctx.new_instance(keyword_value(text)))
}

fn channel_list(ctx: &mut Ctx, channels: &Value) -> OpResult<Value> {
    let items = ctx.convert_iterable(channels, 8, |_, value| Ok(value))?;
    let mut coerced = Vec::with_capacity(items.len());
    for item in items {
        coerced.push(coerce(ctx, &item, Channel::Number)?);
    }
    let list = numeric_array(ctx, coerced).map_err(OpError::thrown)?;
    Ok(list)
}

#[lumen_bind::methods]
impl DomCssColor {
    #[constructor]
    fn new(ctx: &mut Ctx, space: Value, channels: Value, alpha: Option<Value>) -> OpResult<ColorConstructor> {
        let space = keyword_of(ctx, &space)?;
        let channels = channel_list(ctx, &channels)?;
        let alpha = match alpha {
            Some(Value::Undefined) | None => unit_value(ctx, 100.0, NumericUnit::Percent),
            Some(alpha) => coerce(ctx, &alpha, Channel::Percent)?,
        };
        Ok(ColorConstructor(build(ctx, Family::Color, vec![("space", space), ("channels", channels), ("alpha", alpha)])?))
    }
    #[getter]
    fn color_space(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        slot(ctx, &this.0, "space")
    }
    #[setter]
    fn set_color_space(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
        let value = keyword_of(ctx, &value)?;
        let object = data(ctx, &this.0)?;
        ctx.create_data_property(&object, "space", value).map_err(OpError::thrown)?;
        refresh(ctx, &this.0)
    }
    #[getter]
    fn channels(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        slot(ctx, &this.0, "channels")
    }
    #[setter]
    fn set_channels(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
        let list = channel_list(ctx, &value)?;
        let object = data(ctx, &this.0)?;
        ctx.create_data_property(&object, "channels", list).map_err(OpError::thrown)?;
        refresh(ctx, &this.0)
    }
    #[getter]
    fn alpha(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        slot(ctx, &this.0, "alpha")
    }
    #[setter]
    fn set_alpha(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
        set_channel(ctx, &this.0, "alpha", Channel::Percent, value)
    }
}

#[lumen_bind::methods]
impl DomCssColorValue {
    #[classmethod(coerce)]
    fn parse(ctx: &mut Ctx, _class: This<Value>, css_text: &str) -> OpResult<Value> {
        match parse_typed_color(css_text) {
            Some(TypedColor::Keyword(text)) => Ok(ctx.new_instance(keyword_value(text))),
            Some(TypedColor::Absolute { color, legacy }) => reify(ctx, color, legacy),
            None => Err(syntax_error(ctx, "CSS text is not a valid color")),
        }
    }
}

fn component(ctx: &mut Ctx, color: &Color, index: usize, scale: f64, unit: NumericUnit) -> Value {
    if color.missing & (1 << index) != 0 {
        return ctx.new_instance(keyword_value("none".to_owned()));
    }
    unit_value(ctx, f64::from(color.components[index]) * scale, unit)
}

fn reify(ctx: &mut Ctx, color: Color, legacy: bool) -> OpResult<Value> {
    let alpha = if color.missing & 8 != 0 {
        ctx.new_instance(keyword_value("none".to_owned()))
    } else {
        unit_value(ctx, f64::from(color.alpha) * 100.0, NumericUnit::Percent)
    };
    let (family, values): (Family, Vec<(&str, Value)>) = match color.space {
        ColorSpace::Srgb if legacy => (
            Family::Rgb,
            vec![
                ("r", component(ctx, &color, 0, 255.0, NumericUnit::Number)),
                ("g", component(ctx, &color, 1, 255.0, NumericUnit::Number)),
                ("b", component(ctx, &color, 2, 255.0, NumericUnit::Number)),
            ],
        ),
        ColorSpace::Hsl | ColorSpace::Hwb => {
            let family = if color.space == ColorSpace::Hsl { Family::Hsl } else { Family::Hwb };
            let names = family.channels();
            (
                family,
                vec![
                    (names[0].0, component(ctx, &color, 0, 360.0, NumericUnit::Deg)),
                    (names[1].0, component(ctx, &color, 1, 100.0, NumericUnit::Percent)),
                    (names[2].0, component(ctx, &color, 2, 100.0, NumericUnit::Percent)),
                ],
            )
        }
        ColorSpace::Lab => (
            Family::Lab,
            vec![
                ("l", component(ctx, &color, 0, 1.0, NumericUnit::Percent)),
                ("a", component(ctx, &color, 1, 1.0, NumericUnit::Number)),
                ("b", component(ctx, &color, 2, 1.0, NumericUnit::Number)),
            ],
        ),
        ColorSpace::Oklab => (
            Family::Oklab,
            vec![
                ("l", component(ctx, &color, 0, 100.0, NumericUnit::Percent)),
                ("a", component(ctx, &color, 1, 1.0, NumericUnit::Number)),
                ("b", component(ctx, &color, 2, 1.0, NumericUnit::Number)),
            ],
        ),
        ColorSpace::Lch | ColorSpace::Oklch => {
            let ok = color.space == ColorSpace::Oklch;
            (
                if ok { Family::Oklch } else { Family::Lch },
                vec![
                    ("l", component(ctx, &color, 0, if ok { 100.0 } else { 1.0 }, NumericUnit::Percent)),
                    ("c", component(ctx, &color, 1, if ok { 100.0 / 0.4 } else { 100.0 / 150.0 }, NumericUnit::Percent)),
                    ("h", component(ctx, &color, 2, 360.0, NumericUnit::Deg)),
                ],
            )
        }
        space => {
            let channels = vec![
                component(ctx, &color, 0, 1.0, NumericUnit::Number),
                component(ctx, &color, 1, 1.0, NumericUnit::Number),
                component(ctx, &color, 2, 1.0, NumericUnit::Number),
            ];
            let channels = numeric_array(ctx, channels).map_err(OpError::thrown)?;
            let space = ctx.new_instance(keyword_value(space.name().to_owned()));
            (Family::Color, vec![("space", space), ("channels", channels)])
        }
    };
    let mut values = values;
    values.push(("alpha", alpha));
    build(ctx, family, values)
}

pub(super) fn install(ctx: &mut Ctx, global: &Value) -> OpResult<()> {
    macro_rules! interface {
        ($name:literal, $class:ty) => {
            let constructor = ctx.class_constructor::<$class>();
            crate::install_interface(ctx, global, $name, constructor)
                .map_err(|_| OpError::new("Error", concat!($name, " installation failed")))?;
        };
    }
    interface!("CSSColorValue", DomCssColorValue);
    interface!("CSSRGB", DomCssRgb);
    interface!("CSSHSL", DomCssHsl);
    interface!("CSSHWB", DomCssHwb);
    interface!("CSSLab", DomCssLab);
    interface!("CSSLCH", DomCssLch);
    interface!("CSSOKLab", DomCssOklab);
    interface!("CSSOKLCH", DomCssOklch);
    interface!("CSSColor", DomCssColor);
    Ok(())
}
