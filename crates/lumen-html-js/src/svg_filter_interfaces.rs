//! Filter Effects SVG element interfaces and the attribute-backed `SVGAnimated*` values they expose.
//! Every animated value reads and writes its content attribute through the owning element;
//! `animVal` equals `baseVal` because no SMIL/CSS animation drives SVG attributes.
use super::svg_interfaces::{DomSvgElement, FromNode};
use super::{DomElement, DomNode};
use lumen::embed::{Ctx, OpError, OpResult, Value};
use lumen_bind::This;
use lumen_html::NodeId;

const OWNER: &str = "#lumen_svg\u{1}owner";

fn owner_node<T>(
    ctx: &mut Ctx,
    this: &Value,
    read: impl FnOnce(&DomNode) -> OpResult<T>,
) -> OpResult<T> {
    let owner = ctx
        .native_private_value_slot(this, OWNER)
        .ok_or_else(|| OpError::type_error("Illegal invocation"))?;
    ctx.with_instance::<DomElement, _>(&owner, |element| read(&element.base))?
}

fn number_text(ctx: &mut Ctx, value: f64) -> OpResult<String> {
    if !value.is_finite() {
        return Err(OpError::type_error("The provided float value is non-finite"));
    }
    Ok(ctx
        .coerce_string(&Value::Num(value))
        .map_err(OpError::thrown)?
        .to_string())
}

fn parse_numbers(text: &str) -> Vec<f64> {
    text.split(|c: char| c.is_ascii_whitespace() || c == ',')
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<f64>().ok())
        .filter(|number| number.is_finite())
        .collect()
}

fn pair_component(text: Option<&str>, index: Option<usize>, default: f64) -> f64 {
    let numbers = text.map(parse_numbers).unwrap_or_default();
    match index {
        None => numbers.first().copied().unwrap_or(default),
        Some(index) => numbers
            .get(index)
            .or_else(|| numbers.first())
            .copied()
            .unwrap_or(default),
    }
}

fn written_component(
    ctx: &mut Ctx,
    current: Option<&str>,
    index: Option<usize>,
    default: f64,
    value: f64,
) -> OpResult<String> {
    let Some(index) = index else {
        return number_text(ctx, value);
    };
    let first = pair_component(current, Some(0), default);
    let second = pair_component(current, Some(1), default);
    let (first, second) = if index == 0 { (value, second) } else { (first, value) };
    Ok(format!("{} {}", number_text(ctx, first)?, number_text(ctx, second)?))
}

#[derive(Clone, Copy)]
pub(crate) enum Anim {
    Str(&'static str),
    Enumeration(&'static str, &'static [&'static str], u16),
    Number(&'static str, Option<usize>, f64),
    Integer(&'static str, Option<usize>, i32),
    Boolean(&'static str),
    Length(&'static str, &'static str),
    NumberList(&'static str),
    AspectRatio(&'static str),
}

impl Anim {
    fn instantiate(self, ctx: &mut Ctx) -> Value {
        match self {
            Anim::Str(attr) => ctx.new_instance(DomSvgAnimatedString { attr }),
            Anim::Enumeration(attr, keywords, default) => {
                ctx.new_instance(DomSvgAnimatedEnumeration { attr, keywords, default })
            }
            Anim::Number(attr, index, default) => {
                ctx.new_instance(DomSvgAnimatedNumber { attr, index, default })
            }
            Anim::Integer(attr, index, default) => {
                ctx.new_instance(DomSvgAnimatedInteger { attr, index, default })
            }
            Anim::Boolean(attr) => ctx.new_instance(DomSvgAnimatedBoolean { attr }),
            Anim::Length(attr, default) => ctx.new_instance(DomSvgAnimatedLength { attr, default }),
            Anim::NumberList(attr) => ctx.new_instance(DomSvgAnimatedNumberList { attr }),
            Anim::AspectRatio(attr) => ctx.new_instance(DomSvgAnimatedPreserveAspectRatio { attr }),
        }
    }
}

fn cached_child(
    ctx: &mut Ctx,
    holder: &Value,
    slot: &str,
    make: impl FnOnce(&mut Ctx) -> Value,
) -> OpResult<Value> {
    if let Some(existing) = ctx.native_private_value_slot(holder, slot) {
        return Ok(existing);
    }
    let owner = ctx.native_private_value_slot(holder, OWNER);
    let value = make(ctx);
    if let Some(owner) = owner {
        ctx.set_native_internal_value_slot(&value, OWNER, owner)
            .map_err(OpError::thrown)?;
    }
    ctx.set_native_internal_value_slot(holder, slot, value.clone())
        .map_err(OpError::thrown)?;
    Ok(value)
}

fn animated(ctx: &mut Ctx, element: &Value, key: &'static str, anim: Anim) -> OpResult<Value> {
    let slot = format!("#lumen_svg\u{1}a:{key}");
    if let Some(existing) = ctx.native_private_value_slot(element, &slot) {
        return Ok(existing);
    }
    let value = anim.instantiate(ctx);
    ctx.set_native_internal_value_slot(&value, OWNER, element.clone())
        .map_err(OpError::thrown)?;
    ctx.set_native_internal_value_slot(element, &slot, value.clone())
        .map_err(OpError::thrown)?;
    Ok(value)
}

#[lumen_bind::class(name = "SVGAnimatedString", hint(js(webidl)))]
pub(crate) struct DomSvgAnimatedString {
    attr: &'static str,
}
#[lumen_bind::methods]
impl DomSvgAnimatedString {
    #[getter(name = "baseVal")]
    fn base_val(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<String> {
        owner_node(ctx, &this.0, |node| Ok(node.get_null_attribute(self.attr)?.unwrap_or_default()))
    }
    #[setter(name = "baseVal", coerce)]
    fn set_base_val(&self, ctx: &mut Ctx, this: This<Value>, value: &str) -> OpResult<()> {
        owner_node(ctx, &this.0, |node| node.set_attribute_core(self.attr, value))
    }
    #[getter(name = "animVal")]
    fn anim_val(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<String> {
        self.base_val(ctx, this)
    }
}

#[lumen_bind::class(name = "SVGAnimatedEnumeration", hint(js(webidl)))]
pub(crate) struct DomSvgAnimatedEnumeration {
    attr: &'static str,
    keywords: &'static [&'static str],
    default: u16,
}
#[lumen_bind::methods]
impl DomSvgAnimatedEnumeration {
    #[getter(name = "baseVal")]
    fn base_val(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<u16> {
        let text = owner_node(ctx, &this.0, |node| node.get_null_attribute(self.attr))?;
        Ok(match text {
            None => self.default,
            Some(text) => self
                .keywords
                .iter()
                .position(|keyword| *keyword == text.trim())
                .map_or(0, |index| index as u16 + 1),
        })
    }
    #[setter(name = "baseVal", coerce)]
    fn set_base_val(&self, ctx: &mut Ctx, this: This<Value>, value: u16) -> OpResult<()> {
        let Some(keyword) = value.checked_sub(1).and_then(|index| self.keywords.get(usize::from(index))) else {
            return Err(OpError::type_error("The enumeration value is out of range"));
        };
        owner_node(ctx, &this.0, |node| node.set_attribute_core(self.attr, keyword))
    }
    #[getter(name = "animVal")]
    fn anim_val(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<u16> {
        self.base_val(ctx, this)
    }
}

#[lumen_bind::class(name = "SVGAnimatedNumber", hint(js(webidl)))]
pub(crate) struct DomSvgAnimatedNumber {
    attr: &'static str,
    index: Option<usize>,
    default: f64,
}
#[lumen_bind::methods]
impl DomSvgAnimatedNumber {
    #[getter(name = "baseVal")]
    fn base_val(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<f64> {
        let text = owner_node(ctx, &this.0, |node| node.get_null_attribute(self.attr))?;
        Ok(pair_component(text.as_deref(), self.index, self.default))
    }
    #[setter(name = "baseVal", coerce)]
    fn set_base_val(&self, ctx: &mut Ctx, this: This<Value>, value: f64) -> OpResult<()> {
        let text = owner_node(ctx, &this.0, |node| node.get_null_attribute(self.attr))?;
        let written = written_component(ctx, text.as_deref(), self.index, self.default, value)?;
        owner_node(ctx, &this.0, |node| node.set_attribute_core(self.attr, &written))
    }
    #[getter(name = "animVal")]
    fn anim_val(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<f64> {
        self.base_val(ctx, this)
    }
}

#[lumen_bind::class(name = "SVGAnimatedInteger", hint(js(webidl)))]
pub(crate) struct DomSvgAnimatedInteger {
    attr: &'static str,
    index: Option<usize>,
    default: i32,
}
#[lumen_bind::methods]
impl DomSvgAnimatedInteger {
    #[getter(name = "baseVal")]
    fn base_val(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<i32> {
        let text = owner_node(ctx, &this.0, |node| node.get_null_attribute(self.attr))?;
        Ok(pair_component(text.as_deref(), self.index, f64::from(self.default)) as i32)
    }
    #[setter(name = "baseVal", coerce)]
    fn set_base_val(&self, ctx: &mut Ctx, this: This<Value>, value: i32) -> OpResult<()> {
        let text = owner_node(ctx, &this.0, |node| node.get_null_attribute(self.attr))?;
        let written =
            written_component(ctx, text.as_deref(), self.index, f64::from(self.default), f64::from(value))?;
        owner_node(ctx, &this.0, |node| node.set_attribute_core(self.attr, &written))
    }
    #[getter(name = "animVal")]
    fn anim_val(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<i32> {
        self.base_val(ctx, this)
    }
}

#[lumen_bind::class(name = "SVGAnimatedBoolean", hint(js(webidl)))]
pub(crate) struct DomSvgAnimatedBoolean {
    attr: &'static str,
}
#[lumen_bind::methods]
impl DomSvgAnimatedBoolean {
    #[getter(name = "baseVal")]
    fn base_val(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<bool> {
        let text = owner_node(ctx, &this.0, |node| node.get_null_attribute(self.attr))?;
        Ok(text.as_deref().map(str::trim) == Some("true"))
    }
    #[setter(name = "baseVal", coerce)]
    fn set_base_val(&self, ctx: &mut Ctx, this: This<Value>, value: bool) -> OpResult<()> {
        owner_node(ctx, &this.0, |node| {
            node.set_attribute_core(self.attr, if value { "true" } else { "false" })
        })
    }
    #[getter(name = "animVal")]
    fn anim_val(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<bool> {
        self.base_val(ctx, this)
    }
}

const LENGTH_UNITS: [&str; 11] = ["", "", "%", "em", "ex", "px", "cm", "mm", "in", "pt", "pc"];

fn split_number(text: &str) -> (&str, &str) {
    let bytes = text.as_bytes();
    let digits = |mut at: usize| {
        while at < bytes.len() && bytes[at].is_ascii_digit() {
            at += 1;
        }
        at
    };
    let mut end = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    end = digits(end);
    if bytes.get(end) == Some(&b'.') {
        end = digits(end + 1);
    }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let sign = usize::from(matches!(bytes.get(end + 1), Some(b'+' | b'-')));
        let exponent = digits(end + 1 + sign);
        if exponent > end + 1 + sign {
            end = exponent;
        }
    }
    text.split_at(end)
}

fn parse_length(text: &str) -> Option<(f64, u16)> {
    let (number, unit) = split_number(text.trim());
    let value: f64 = number.parse().ok().filter(|value: &f64| value.is_finite())?;
    let unit_type = match unit {
        "" => 1,
        other => LENGTH_UNITS.iter().position(|name| *name == other && !name.is_empty())? as u16,
    };
    Some((value, unit_type))
}

fn pixels_per_unit(unit_type: u16) -> Option<f64> {
    Some(match unit_type {
        1 | 5 => 1.0,
        3 => 16.0,
        4 => 8.0,
        6 => 96.0 / 2.54,
        7 => 96.0 / 25.4,
        8 => 96.0,
        9 => 96.0 / 72.0,
        10 => 16.0,
        _ => return None,
    })
}

#[lumen_bind::class(name = "SVGAnimatedLength", hint(js(webidl)))]
pub(crate) struct DomSvgAnimatedLength {
    attr: &'static str,
    default: &'static str,
}
#[lumen_bind::methods]
impl DomSvgAnimatedLength {
    #[getter(name = "baseVal")]
    fn base_val(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        let (attr, default) = (self.attr, self.default);
        cached_child(ctx, &this.0, "#lumen_svg\u{1}base", |ctx| {
            ctx.new_instance(DomSvgLength { attr, default, read_only: false })
        })
    }
    #[getter(name = "animVal")]
    fn anim_val(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        let (attr, default) = (self.attr, self.default);
        cached_child(ctx, &this.0, "#lumen_svg\u{1}anim", |ctx| {
            ctx.new_instance(DomSvgLength { attr, default, read_only: true })
        })
    }
}

#[lumen_bind::class(name = "SVGLength", hint(js(webidl)))]
pub(crate) struct DomSvgLength {
    attr: &'static str,
    default: &'static str,
    read_only: bool,
}
impl DomSvgLength {
    fn read(&self, ctx: &mut Ctx, this: &Value) -> OpResult<(f64, u16)> {
        let text = owner_node(ctx, this, |node| node.get_null_attribute(self.attr))?;
        Ok(text
            .as_deref()
            .and_then(parse_length)
            .or_else(|| parse_length(self.default))
            .unwrap_or((0.0, 1)))
    }
    fn write(&self, ctx: &mut Ctx, this: &Value, value: f64, unit_type: u16) -> OpResult<()> {
        if self.read_only {
            return Err(crate::error_reporting::dom_exception(
                ctx,
                "NoModificationAllowedError",
                "The animated value is read-only",
            ));
        }
        let text = format!("{}{}", number_text(ctx, value)?, LENGTH_UNITS[usize::from(unit_type)]);
        owner_node(ctx, this, |node| node.set_attribute_core(self.attr, &text))
    }
}
#[lumen_bind::methods]
impl DomSvgLength {
    #[constant]
    const SVG_LENGTHTYPE_UNKNOWN: u16 = 0;
    #[constant]
    const SVG_LENGTHTYPE_NUMBER: u16 = 1;
    #[constant]
    const SVG_LENGTHTYPE_PERCENTAGE: u16 = 2;
    #[constant]
    const SVG_LENGTHTYPE_EMS: u16 = 3;
    #[constant]
    const SVG_LENGTHTYPE_EXS: u16 = 4;
    #[constant]
    const SVG_LENGTHTYPE_PX: u16 = 5;
    #[constant]
    const SVG_LENGTHTYPE_CM: u16 = 6;
    #[constant]
    const SVG_LENGTHTYPE_MM: u16 = 7;
    #[constant]
    const SVG_LENGTHTYPE_IN: u16 = 8;
    #[constant]
    const SVG_LENGTHTYPE_PT: u16 = 9;
    #[constant]
    const SVG_LENGTHTYPE_PC: u16 = 10;

    #[getter(name = "unitType")]
    fn unit_type(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<u16> {
        Ok(self.read(ctx, &this.0)?.1)
    }
    #[getter]
    fn value(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<f64> {
        let (value, unit) = self.read(ctx, &this.0)?;
        Ok(pixels_per_unit(unit).map_or(0.0, |factor| value * factor))
    }
    #[setter(coerce)]
    fn set_value(&self, ctx: &mut Ctx, this: This<Value>, value: f64) -> OpResult<()> {
        let (_, unit) = self.read(ctx, &this.0)?;
        match pixels_per_unit(unit) {
            Some(factor) => self.write(ctx, &this.0, value / factor, unit),
            None => self.write(ctx, &this.0, value, 5),
        }
    }
    #[getter(name = "valueInSpecifiedUnits")]
    fn value_in_specified_units(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<f64> {
        Ok(self.read(ctx, &this.0)?.0)
    }
    #[setter(name = "valueInSpecifiedUnits", coerce)]
    fn set_value_in_specified_units(&self, ctx: &mut Ctx, this: This<Value>, value: f64) -> OpResult<()> {
        let (_, unit) = self.read(ctx, &this.0)?;
        self.write(ctx, &this.0, value, unit.max(1))
    }
    #[getter(name = "valueAsString")]
    fn value_as_string(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<String> {
        let (value, unit) = self.read(ctx, &this.0)?;
        Ok(format!("{}{}", number_text(ctx, value)?, LENGTH_UNITS[usize::from(unit)]))
    }
    #[setter(name = "valueAsString", coerce)]
    fn set_value_as_string(&self, ctx: &mut Ctx, this: This<Value>, text: &str) -> OpResult<()> {
        let Some((value, unit)) = parse_length(text) else {
            return Err(crate::error_reporting::dom_exception(ctx, "SyntaxError", "Invalid SVG length"));
        };
        self.write(ctx, &this.0, value, unit)
    }
    #[method(name = "newValueSpecifiedUnits", coerce)]
    fn new_value_specified_units(&self, ctx: &mut Ctx, this: This<Value>, unit_type: u16, value: f64) -> OpResult<()> {
        if !(1..=10).contains(&unit_type) {
            return Err(crate::error_reporting::dom_exception(ctx, "NotSupportedError", "Unsupported length unit"));
        }
        self.write(ctx, &this.0, value, unit_type)
    }
    #[method(name = "convertToSpecifiedUnits", coerce)]
    fn convert_to_specified_units(&self, ctx: &mut Ctx, this: This<Value>, unit_type: u16) -> OpResult<()> {
        let (value, unit) = self.read(ctx, &this.0)?;
        let factors = pixels_per_unit(unit).zip(pixels_per_unit(unit_type));
        let Some((from, to)) = factors.filter(|_| (1..=10).contains(&unit_type)) else {
            return Err(crate::error_reporting::dom_exception(ctx, "NotSupportedError", "Cannot convert length units"));
        };
        self.write(ctx, &this.0, value * from / to, unit_type)
    }
}

#[lumen_bind::class(name = "SVGNumber", hint(js(webidl)))]
pub(crate) struct DomSvgNumber {
    value: std::cell::Cell<f64>,
}
#[lumen_bind::methods]
impl DomSvgNumber {
    #[getter]
    fn value(&self) -> f64 {
        self.value.get()
    }
    #[setter(coerce)]
    fn set_value(&self, value: f64) -> OpResult<()> {
        if !value.is_finite() {
            return Err(OpError::type_error("The provided float value is non-finite"));
        }
        self.value.set(value);
        Ok(())
    }
}

#[lumen_bind::class(name = "SVGAnimatedNumberList", hint(js(webidl)))]
pub(crate) struct DomSvgAnimatedNumberList {
    attr: &'static str,
}
#[lumen_bind::methods]
impl DomSvgAnimatedNumberList {
    #[getter(name = "baseVal")]
    fn base_val(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        let attr = self.attr;
        cached_child(ctx, &this.0, "#lumen_svg\u{1}base", |ctx| {
            ctx.new_instance(DomSvgNumberList { attr, read_only: false })
        })
    }
    #[getter(name = "animVal")]
    fn anim_val(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        let attr = self.attr;
        cached_child(ctx, &this.0, "#lumen_svg\u{1}anim", |ctx| {
            ctx.new_instance(DomSvgNumberList { attr, read_only: true })
        })
    }
}

#[lumen_bind::class(name = "SVGNumberList", hint(js(webidl)))]
pub(crate) struct DomSvgNumberList {
    attr: &'static str,
    read_only: bool,
}
impl DomSvgNumberList {
    fn items(&self, ctx: &mut Ctx, this: &Value) -> OpResult<Vec<f64>> {
        let text = owner_node(ctx, this, |node| node.get_null_attribute(self.attr))?;
        Ok(text.as_deref().map(parse_numbers).unwrap_or_default())
    }
    fn store(&self, ctx: &mut Ctx, this: &Value, items: &[f64]) -> OpResult<()> {
        if self.read_only {
            return Err(crate::error_reporting::dom_exception(
                ctx,
                "NoModificationAllowedError",
                "The animated value is read-only",
            ));
        }
        let mut parts = Vec::with_capacity(items.len());
        for item in items {
            parts.push(number_text(ctx, *item)?);
        }
        let text = parts.join(" ");
        owner_node(ctx, this, |node| node.set_attribute_core(self.attr, &text))
    }
    fn item_value(ctx: &mut Ctx, item: &Value) -> OpResult<f64> {
        ctx.with_instance::<DomSvgNumber, _>(item, |number| number.value.get())
    }
    fn number(ctx: &mut Ctx, value: f64) -> Value {
        ctx.new_instance(DomSvgNumber { value: std::cell::Cell::new(value) })
    }
    fn index_error(ctx: &mut Ctx) -> OpError {
        crate::error_reporting::dom_exception(ctx, "IndexSizeError", "The index is out of range")
    }
}
#[lumen_bind::methods]
impl DomSvgNumberList {
    #[getter(name = "numberOfItems")]
    fn number_of_items(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<u32> {
        Ok(self.items(ctx, &this.0)?.len() as u32)
    }
    #[getter]
    fn length(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<u32> {
        Ok(self.items(ctx, &this.0)?.len() as u32)
    }
    fn clear(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<()> {
        self.store(ctx, &this.0, &[])
    }
    fn initialize(&self, ctx: &mut Ctx, this: This<Value>, new_item: Value) -> OpResult<Value> {
        let value = Self::item_value(ctx, &new_item)?;
        self.store(ctx, &this.0, &[value])?;
        Ok(new_item)
    }
    #[method(name = "getItem", coerce)]
    fn get_item(&self, ctx: &mut Ctx, this: This<Value>, index: u32) -> OpResult<Value> {
        let items = self.items(ctx, &this.0)?;
        match items.get(index as usize) {
            Some(value) => Ok(Self::number(ctx, *value)),
            None => Err(Self::index_error(ctx)),
        }
    }
    #[method(name = "insertItemBefore", coerce)]
    fn insert_item_before(&self, ctx: &mut Ctx, this: This<Value>, new_item: Value, index: u32) -> OpResult<Value> {
        let value = Self::item_value(ctx, &new_item)?;
        let mut items = self.items(ctx, &this.0)?;
        items.insert((index as usize).min(items.len()), value);
        self.store(ctx, &this.0, &items)?;
        Ok(new_item)
    }
    #[method(name = "replaceItem", coerce)]
    fn replace_item(&self, ctx: &mut Ctx, this: This<Value>, new_item: Value, index: u32) -> OpResult<Value> {
        let value = Self::item_value(ctx, &new_item)?;
        let mut items = self.items(ctx, &this.0)?;
        let Some(slot) = items.get_mut(index as usize) else {
            return Err(Self::index_error(ctx));
        };
        *slot = value;
        self.store(ctx, &this.0, &items)?;
        Ok(new_item)
    }
    #[method(name = "removeItem", coerce)]
    fn remove_item(&self, ctx: &mut Ctx, this: This<Value>, index: u32) -> OpResult<Value> {
        let mut items = self.items(ctx, &this.0)?;
        if index as usize >= items.len() {
            return Err(Self::index_error(ctx));
        }
        let removed = items.remove(index as usize);
        self.store(ctx, &this.0, &items)?;
        Ok(Self::number(ctx, removed))
    }
    #[method(name = "appendItem")]
    fn append_item(&self, ctx: &mut Ctx, this: This<Value>, new_item: Value) -> OpResult<Value> {
        let value = Self::item_value(ctx, &new_item)?;
        let mut items = self.items(ctx, &this.0)?;
        items.push(value);
        self.store(ctx, &this.0, &items)?;
        Ok(new_item)
    }
}

const ASPECT_ALIGN: [&str; 10] = [
    "none", "xMinYMin", "xMidYMin", "xMaxYMin", "xMinYMid", "xMidYMid", "xMaxYMid", "xMinYMax", "xMidYMax", "xMaxYMax",
];

#[lumen_bind::class(name = "SVGAnimatedPreserveAspectRatio", hint(js(webidl)))]
pub(crate) struct DomSvgAnimatedPreserveAspectRatio {
    attr: &'static str,
}
#[lumen_bind::methods]
impl DomSvgAnimatedPreserveAspectRatio {
    #[getter(name = "baseVal")]
    fn base_val(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        let attr = self.attr;
        cached_child(ctx, &this.0, "#lumen_svg\u{1}base", |ctx| {
            ctx.new_instance(DomSvgPreserveAspectRatio { attr, read_only: false })
        })
    }
    #[getter(name = "animVal")]
    fn anim_val(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        let attr = self.attr;
        cached_child(ctx, &this.0, "#lumen_svg\u{1}anim", |ctx| {
            ctx.new_instance(DomSvgPreserveAspectRatio { attr, read_only: true })
        })
    }
}

#[lumen_bind::class(name = "SVGPreserveAspectRatio", hint(js(webidl)))]
pub(crate) struct DomSvgPreserveAspectRatio {
    attr: &'static str,
    read_only: bool,
}
impl DomSvgPreserveAspectRatio {
    fn read(&self, ctx: &mut Ctx, this: &Value) -> OpResult<(u16, u16)> {
        let text = owner_node(ctx, this, |node| node.get_null_attribute(self.attr))?.unwrap_or_default();
        let mut tokens = text.split_ascii_whitespace().filter(|token| *token != "defer");
        let align = tokens
            .next()
            .and_then(|token| ASPECT_ALIGN.iter().position(|name| *name == token))
            .map_or(6, |index| index as u16 + 1);
        let meet_or_slice = if tokens.next() == Some("slice") { 2 } else { 1 };
        Ok((align, meet_or_slice))
    }
    fn write(&self, ctx: &mut Ctx, this: &Value, align: u16, meet_or_slice: u16) -> OpResult<()> {
        if self.read_only {
            return Err(crate::error_reporting::dom_exception(
                ctx,
                "NoModificationAllowedError",
                "The animated value is read-only",
            ));
        }
        let text = format!(
            "{} {}",
            ASPECT_ALIGN[usize::from(align - 1)],
            if meet_or_slice == 2 { "slice" } else { "meet" }
        );
        owner_node(ctx, this, |node| node.set_attribute_core(self.attr, &text))
    }
}
#[lumen_bind::methods]
impl DomSvgPreserveAspectRatio {
    #[constant]
    const SVG_PRESERVEASPECTRATIO_UNKNOWN: u16 = 0;
    #[constant]
    const SVG_PRESERVEASPECTRATIO_NONE: u16 = 1;
    #[constant]
    const SVG_PRESERVEASPECTRATIO_XMINYMIN: u16 = 2;
    #[constant]
    const SVG_PRESERVEASPECTRATIO_XMIDYMIN: u16 = 3;
    #[constant]
    const SVG_PRESERVEASPECTRATIO_XMAXYMIN: u16 = 4;
    #[constant]
    const SVG_PRESERVEASPECTRATIO_XMINYMID: u16 = 5;
    #[constant]
    const SVG_PRESERVEASPECTRATIO_XMIDYMID: u16 = 6;
    #[constant]
    const SVG_PRESERVEASPECTRATIO_XMAXYMID: u16 = 7;
    #[constant]
    const SVG_PRESERVEASPECTRATIO_XMINYMAX: u16 = 8;
    #[constant]
    const SVG_PRESERVEASPECTRATIO_XMIDYMAX: u16 = 9;
    #[constant]
    const SVG_PRESERVEASPECTRATIO_XMAXYMAX: u16 = 10;
    #[constant]
    const SVG_MEETORSLICE_UNKNOWN: u16 = 0;
    #[constant]
    const SVG_MEETORSLICE_MEET: u16 = 1;
    #[constant]
    const SVG_MEETORSLICE_SLICE: u16 = 2;

    #[getter]
    fn align(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<u16> {
        Ok(self.read(ctx, &this.0)?.0)
    }
    #[setter(coerce)]
    fn set_align(&self, ctx: &mut Ctx, this: This<Value>, value: u16) -> OpResult<()> {
        if !(1..=10).contains(&value) {
            return Err(OpError::type_error("The alignment value is out of range"));
        }
        let (_, meet_or_slice) = self.read(ctx, &this.0)?;
        self.write(ctx, &this.0, value, meet_or_slice)
    }
    #[getter(name = "meetOrSlice")]
    fn meet_or_slice(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<u16> {
        Ok(self.read(ctx, &this.0)?.1)
    }
    #[setter(name = "meetOrSlice", coerce)]
    fn set_meet_or_slice(&self, ctx: &mut Ctx, this: This<Value>, value: u16) -> OpResult<()> {
        if !(1..=2).contains(&value) {
            return Err(OpError::type_error("The meetOrSlice value is out of range"));
        }
        let (align, _) = self.read(ctx, &this.0)?;
        self.write(ctx, &this.0, align, value)
    }
}

macro_rules! filter_element {
    (
        $ty:ident, $name:literal, $base:ident,
        { $( $cname:ident = $cvalue:literal ),* $(,)? },
        { $( $getter:ident $js:literal => $anim:expr; )* }
        $( , { $($extra:tt)* } )?
    ) => {
        #[lumen_bind::class(name = $name, extends = $base, hint(js(webidl)))]
        pub(crate) struct $ty { base: $base }
        impl FromNode for $ty {
            fn from_node(node: DomNode) -> Self { Self { base: $base::from_node(node) } }
        }
        #[lumen_bind::methods]
        impl $ty {
            $( #[constant] const $cname: u16 = $cvalue; )*
            $(
                #[getter(name = $js)]
                fn $getter(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
                    animated(ctx, &this.0, $js, $anim)
                }
            )*
            $( $($extra)* )?
        }
    };
}

macro_rules! filter_primitive {
    (
        $ty:ident, $name:literal,
        { $( $cname:ident = $cvalue:literal ),* $(,)? },
        { $( $getter:ident $js:literal => $anim:expr; )* }
        $( , { $($extra:tt)* } )?
    ) => {
        filter_element! {
            $ty, $name, DomSvgElement,
            { $( $cname = $cvalue ),* },
            {
                $( $getter $js => $anim; )*
                x "x" => Anim::Length("x", "");
                y "y" => Anim::Length("y", "");
                width "width" => Anim::Length("width", "");
                height "height" => Anim::Length("height", "");
                result "result" => Anim::Str("result");
            }
            $( , { $($extra)* } )?
        }
    };
}

filter_element! {
    DomSvgFilterElement, "SVGFilterElement", DomSvgElement,
    {},
    {
        filter_units "filterUnits" => Anim::Enumeration("filterUnits", &["userSpaceOnUse", "objectBoundingBox"], 2);
        primitive_units "primitiveUnits" => Anim::Enumeration("primitiveUnits", &["userSpaceOnUse", "objectBoundingBox"], 1);
        x "x" => Anim::Length("x", "");
        y "y" => Anim::Length("y", "");
        width "width" => Anim::Length("width", "");
        height "height" => Anim::Length("height", "");
    }
}

const BLEND_MODES: &[&str] = &[
    "normal", "multiply", "screen", "darken", "lighten", "overlay", "color-dodge", "color-burn", "hard-light",
    "soft-light", "difference", "exclusion", "hue", "saturation", "color", "luminosity",
];

filter_primitive! {
    DomSvgFeBlendElement, "SVGFEBlendElement",
    {
        SVG_FEBLEND_MODE_UNKNOWN = 0, SVG_FEBLEND_MODE_NORMAL = 1, SVG_FEBLEND_MODE_MULTIPLY = 2,
        SVG_FEBLEND_MODE_SCREEN = 3, SVG_FEBLEND_MODE_DARKEN = 4, SVG_FEBLEND_MODE_LIGHTEN = 5,
        SVG_FEBLEND_MODE_OVERLAY = 6, SVG_FEBLEND_MODE_COLOR_DODGE = 7, SVG_FEBLEND_MODE_COLOR_BURN = 8,
        SVG_FEBLEND_MODE_HARD_LIGHT = 9, SVG_FEBLEND_MODE_SOFT_LIGHT = 10, SVG_FEBLEND_MODE_DIFFERENCE = 11,
        SVG_FEBLEND_MODE_EXCLUSION = 12, SVG_FEBLEND_MODE_HUE = 13, SVG_FEBLEND_MODE_SATURATION = 14,
        SVG_FEBLEND_MODE_COLOR = 15, SVG_FEBLEND_MODE_LUMINOSITY = 16,
    },
    {
        in1 "in1" => Anim::Str("in");
        in2 "in2" => Anim::Str("in2");
        mode "mode" => Anim::Enumeration("mode", BLEND_MODES, 1);
    }
}

filter_primitive! {
    DomSvgFeColorMatrixElement, "SVGFEColorMatrixElement",
    {
        SVG_FECOLORMATRIX_TYPE_UNKNOWN = 0, SVG_FECOLORMATRIX_TYPE_MATRIX = 1, SVG_FECOLORMATRIX_TYPE_SATURATE = 2,
        SVG_FECOLORMATRIX_TYPE_HUEROTATE = 3, SVG_FECOLORMATRIX_TYPE_LUMINANCETOALPHA = 4,
    },
    {
        in1 "in1" => Anim::Str("in");
        type_ "type" => Anim::Enumeration("type", &["matrix", "saturate", "hueRotate", "luminanceToAlpha"], 1);
        values "values" => Anim::NumberList("values");
    }
}

filter_primitive! {
    DomSvgFeComponentTransferElement, "SVGFEComponentTransferElement",
    {},
    { in1 "in1" => Anim::Str("in"); }
}

filter_element! {
    DomSvgComponentTransferFunctionElement, "SVGComponentTransferFunctionElement", DomSvgElement,
    {
        SVG_FECOMPONENTTRANSFER_TYPE_UNKNOWN = 0, SVG_FECOMPONENTTRANSFER_TYPE_IDENTITY = 1,
        SVG_FECOMPONENTTRANSFER_TYPE_TABLE = 2, SVG_FECOMPONENTTRANSFER_TYPE_DISCRETE = 3,
        SVG_FECOMPONENTTRANSFER_TYPE_LINEAR = 4, SVG_FECOMPONENTTRANSFER_TYPE_GAMMA = 5,
    },
    {
        type_ "type" => Anim::Enumeration("type", &["identity", "table", "discrete", "linear", "gamma"], 0);
        table_values "tableValues" => Anim::NumberList("tableValues");
        slope "slope" => Anim::Number("slope", None, 1.0);
        intercept "intercept" => Anim::Number("intercept", None, 0.0);
        amplitude "amplitude" => Anim::Number("amplitude", None, 1.0);
        exponent "exponent" => Anim::Number("exponent", None, 1.0);
        offset "offset" => Anim::Number("offset", None, 0.0);
    }
}

macro_rules! transfer_function_element {
    ($( $ty:ident, $name:literal; )+) => {
        $(
            #[lumen_bind::class(name = $name, extends = DomSvgComponentTransferFunctionElement, hint(js(webidl)))]
            pub(crate) struct $ty { base: DomSvgComponentTransferFunctionElement }
            impl FromNode for $ty {
                fn from_node(node: DomNode) -> Self {
                    Self { base: DomSvgComponentTransferFunctionElement::from_node(node) }
                }
            }
            #[lumen_bind::methods]
            impl $ty {}
        )+
    };
}
transfer_function_element! {
    DomSvgFeFuncRElement, "SVGFEFuncRElement";
    DomSvgFeFuncGElement, "SVGFEFuncGElement";
    DomSvgFeFuncBElement, "SVGFEFuncBElement";
    DomSvgFeFuncAElement, "SVGFEFuncAElement";
}

filter_primitive! {
    DomSvgFeCompositeElement, "SVGFECompositeElement",
    {
        SVG_FECOMPOSITE_OPERATOR_UNKNOWN = 0, SVG_FECOMPOSITE_OPERATOR_OVER = 1, SVG_FECOMPOSITE_OPERATOR_IN = 2,
        SVG_FECOMPOSITE_OPERATOR_OUT = 3, SVG_FECOMPOSITE_OPERATOR_ATOP = 4, SVG_FECOMPOSITE_OPERATOR_XOR = 5,
        SVG_FECOMPOSITE_OPERATOR_ARITHMETIC = 6,
    },
    {
        in1 "in1" => Anim::Str("in");
        in2 "in2" => Anim::Str("in2");
        operator "operator" => Anim::Enumeration("operator", &["over", "in", "out", "atop", "xor", "arithmetic"], 1);
        k1 "k1" => Anim::Number("k1", None, 0.0);
        k2 "k2" => Anim::Number("k2", None, 0.0);
        k3 "k3" => Anim::Number("k3", None, 0.0);
        k4 "k4" => Anim::Number("k4", None, 0.0);
    }
}

const EDGE_MODES: &[&str] = &["duplicate", "wrap", "none"];

filter_primitive! {
    DomSvgFeConvolveMatrixElement, "SVGFEConvolveMatrixElement",
    {
        SVG_EDGEMODE_UNKNOWN = 0, SVG_EDGEMODE_DUPLICATE = 1, SVG_EDGEMODE_WRAP = 2, SVG_EDGEMODE_NONE = 3,
    },
    {
        in1 "in1" => Anim::Str("in");
        order_x "orderX" => Anim::Integer("order", Some(0), 3);
        order_y "orderY" => Anim::Integer("order", Some(1), 3);
        kernel_matrix "kernelMatrix" => Anim::NumberList("kernelMatrix");
        divisor "divisor" => Anim::Number("divisor", None, 0.0);
        bias "bias" => Anim::Number("bias", None, 0.0);
        target_x "targetX" => Anim::Integer("targetX", None, 0);
        target_y "targetY" => Anim::Integer("targetY", None, 0);
        edge_mode "edgeMode" => Anim::Enumeration("edgeMode", EDGE_MODES, 1);
        kernel_unit_length_x "kernelUnitLengthX" => Anim::Number("kernelUnitLength", Some(0), 0.0);
        kernel_unit_length_y "kernelUnitLengthY" => Anim::Number("kernelUnitLength", Some(1), 0.0);
        preserve_alpha "preserveAlpha" => Anim::Boolean("preserveAlpha");
    }
}

filter_primitive! {
    DomSvgFeDiffuseLightingElement, "SVGFEDiffuseLightingElement",
    {},
    {
        in1 "in1" => Anim::Str("in");
        surface_scale "surfaceScale" => Anim::Number("surfaceScale", None, 1.0);
        diffuse_constant "diffuseConstant" => Anim::Number("diffuseConstant", None, 1.0);
        kernel_unit_length_x "kernelUnitLengthX" => Anim::Number("kernelUnitLength", Some(0), 0.0);
        kernel_unit_length_y "kernelUnitLengthY" => Anim::Number("kernelUnitLength", Some(1), 0.0);
    }
}

filter_element! {
    DomSvgFeDistantLightElement, "SVGFEDistantLightElement", DomSvgElement,
    {},
    {
        azimuth "azimuth" => Anim::Number("azimuth", None, 0.0);
        elevation "elevation" => Anim::Number("elevation", None, 0.0);
    }
}

filter_element! {
    DomSvgFePointLightElement, "SVGFEPointLightElement", DomSvgElement,
    {},
    {
        x "x" => Anim::Number("x", None, 0.0);
        y "y" => Anim::Number("y", None, 0.0);
        z "z" => Anim::Number("z", None, 0.0);
    }
}

filter_element! {
    DomSvgFeSpotLightElement, "SVGFESpotLightElement", DomSvgElement,
    {},
    {
        x "x" => Anim::Number("x", None, 0.0);
        y "y" => Anim::Number("y", None, 0.0);
        z "z" => Anim::Number("z", None, 0.0);
        points_at_x "pointsAtX" => Anim::Number("pointsAtX", None, 0.0);
        points_at_y "pointsAtY" => Anim::Number("pointsAtY", None, 0.0);
        points_at_z "pointsAtZ" => Anim::Number("pointsAtZ", None, 0.0);
        specular_exponent "specularExponent" => Anim::Number("specularExponent", None, 1.0);
        limiting_cone_angle "limitingConeAngle" => Anim::Number("limitingConeAngle", None, 0.0);
    }
}

filter_primitive! {
    DomSvgFeDisplacementMapElement, "SVGFEDisplacementMapElement",
    {
        SVG_CHANNEL_UNKNOWN = 0, SVG_CHANNEL_R = 1, SVG_CHANNEL_G = 2, SVG_CHANNEL_B = 3, SVG_CHANNEL_A = 4,
    },
    {
        in1 "in1" => Anim::Str("in");
        in2 "in2" => Anim::Str("in2");
        scale "scale" => Anim::Number("scale", None, 0.0);
        x_channel_selector "xChannelSelector" => Anim::Enumeration("xChannelSelector", &["R", "G", "B", "A"], 4);
        y_channel_selector "yChannelSelector" => Anim::Enumeration("yChannelSelector", &["R", "G", "B", "A"], 4);
    }
}

filter_primitive! {
    DomSvgFeDropShadowElement, "SVGFEDropShadowElement",
    {},
    {
        in1 "in1" => Anim::Str("in");
        dx "dx" => Anim::Number("dx", None, 2.0);
        dy "dy" => Anim::Number("dy", None, 2.0);
        std_deviation_x "stdDeviationX" => Anim::Number("stdDeviation", Some(0), 2.0);
        std_deviation_y "stdDeviationY" => Anim::Number("stdDeviation", Some(1), 2.0);
    },
    {
        #[method(name = "setStdDeviation", coerce)]
        fn set_std_deviation(&self, ctx: &mut Ctx, this: This<Value>, x: f64, y: f64) -> OpResult<()> {
            let text = format!("{} {}", number_text(ctx, x)?, number_text(ctx, y)?);
            ctx.with_instance::<DomElement, _>(&this.0, |element| {
                element.base.set_attribute_core("stdDeviation", &text)
            })?
        }
    }
}

filter_primitive! {
    DomSvgFeFloodElement, "SVGFEFloodElement",
    {},
    {}
}

filter_primitive! {
    DomSvgFeGaussianBlurElement, "SVGFEGaussianBlurElement",
    {
        SVG_EDGEMODE_UNKNOWN = 0, SVG_EDGEMODE_DUPLICATE = 1, SVG_EDGEMODE_WRAP = 2, SVG_EDGEMODE_NONE = 3,
    },
    {
        in1 "in1" => Anim::Str("in");
        std_deviation_x "stdDeviationX" => Anim::Number("stdDeviation", Some(0), 0.0);
        std_deviation_y "stdDeviationY" => Anim::Number("stdDeviation", Some(1), 0.0);
        edge_mode "edgeMode" => Anim::Enumeration("edgeMode", EDGE_MODES, 3);
    },
    {
        #[method(name = "setStdDeviation", coerce)]
        fn set_std_deviation(&self, ctx: &mut Ctx, this: This<Value>, x: f64, y: f64) -> OpResult<()> {
            let text = format!("{} {}", number_text(ctx, x)?, number_text(ctx, y)?);
            ctx.with_instance::<DomElement, _>(&this.0, |element| {
                element.base.set_attribute_core("stdDeviation", &text)
            })?
        }
    }
}

filter_primitive! {
    DomSvgFeImageElement, "SVGFEImageElement",
    {},
    {
        preserve_aspect_ratio "preserveAspectRatio" => Anim::AspectRatio("preserveAspectRatio");
        cross_origin "crossOrigin" => Anim::Str("crossorigin");
        href "href" => Anim::Str("href");
    }
}

filter_primitive! {
    DomSvgFeMergeElement, "SVGFEMergeElement",
    {},
    {}
}

filter_element! {
    DomSvgFeMergeNodeElement, "SVGFEMergeNodeElement", DomSvgElement,
    {},
    { in1 "in1" => Anim::Str("in"); }
}

filter_primitive! {
    DomSvgFeMorphologyElement, "SVGFEMorphologyElement",
    {
        SVG_MORPHOLOGY_OPERATOR_UNKNOWN = 0, SVG_MORPHOLOGY_OPERATOR_ERODE = 1, SVG_MORPHOLOGY_OPERATOR_DILATE = 2,
    },
    {
        in1 "in1" => Anim::Str("in");
        operator "operator" => Anim::Enumeration("operator", &["erode", "dilate"], 1);
        radius_x "radiusX" => Anim::Number("radius", Some(0), 0.0);
        radius_y "radiusY" => Anim::Number("radius", Some(1), 0.0);
    }
}

filter_primitive! {
    DomSvgFeOffsetElement, "SVGFEOffsetElement",
    {},
    {
        in1 "in1" => Anim::Str("in");
        dx "dx" => Anim::Number("dx", None, 0.0);
        dy "dy" => Anim::Number("dy", None, 0.0);
    }
}

filter_primitive! {
    DomSvgFeSpecularLightingElement, "SVGFESpecularLightingElement",
    {},
    {
        in1 "in1" => Anim::Str("in");
        surface_scale "surfaceScale" => Anim::Number("surfaceScale", None, 1.0);
        specular_constant "specularConstant" => Anim::Number("specularConstant", None, 1.0);
        specular_exponent "specularExponent" => Anim::Number("specularExponent", None, 1.0);
        kernel_unit_length_x "kernelUnitLengthX" => Anim::Number("kernelUnitLength", Some(0), 0.0);
        kernel_unit_length_y "kernelUnitLengthY" => Anim::Number("kernelUnitLength", Some(1), 0.0);
    }
}

filter_primitive! {
    DomSvgFeTileElement, "SVGFETileElement",
    {},
    { in1 "in1" => Anim::Str("in"); }
}

filter_primitive! {
    DomSvgFeTurbulenceElement, "SVGFETurbulenceElement",
    {
        SVG_TURBULENCE_TYPE_UNKNOWN = 0, SVG_TURBULENCE_TYPE_FRACTALNOISE = 1, SVG_TURBULENCE_TYPE_TURBULENCE = 2,
        SVG_STITCHTYPE_UNKNOWN = 0, SVG_STITCHTYPE_STITCH = 1, SVG_STITCHTYPE_NOSTITCH = 2,
    },
    {
        base_frequency_x "baseFrequencyX" => Anim::Number("baseFrequency", Some(0), 0.0);
        base_frequency_y "baseFrequencyY" => Anim::Number("baseFrequency", Some(1), 0.0);
        num_octaves "numOctaves" => Anim::Integer("numOctaves", None, 1);
        seed "seed" => Anim::Number("seed", None, 0.0);
        stitch_tiles "stitchTiles" => Anim::Enumeration("stitchTiles", &["stitch", "noStitch"], 2);
        type_ "type" => Anim::Enumeration("type", &["fractalNoise", "turbulence"], 2);
    }
}

pub(crate) fn wrap(ctx: &mut Ctx, id: NodeId, node: DomNode, local: &str) -> Value {
    match local {
        "filter" => ctx.cached_instance(id, || DomSvgFilterElement::from_node(node)),
        "feBlend" => ctx.cached_instance(id, || DomSvgFeBlendElement::from_node(node)),
        "feColorMatrix" => ctx.cached_instance(id, || DomSvgFeColorMatrixElement::from_node(node)),
        "feComponentTransfer" => ctx.cached_instance(id, || DomSvgFeComponentTransferElement::from_node(node)),
        "feFuncR" => ctx.cached_instance(id, || DomSvgFeFuncRElement::from_node(node)),
        "feFuncG" => ctx.cached_instance(id, || DomSvgFeFuncGElement::from_node(node)),
        "feFuncB" => ctx.cached_instance(id, || DomSvgFeFuncBElement::from_node(node)),
        "feFuncA" => ctx.cached_instance(id, || DomSvgFeFuncAElement::from_node(node)),
        "feComposite" => ctx.cached_instance(id, || DomSvgFeCompositeElement::from_node(node)),
        "feConvolveMatrix" => ctx.cached_instance(id, || DomSvgFeConvolveMatrixElement::from_node(node)),
        "feDiffuseLighting" => ctx.cached_instance(id, || DomSvgFeDiffuseLightingElement::from_node(node)),
        "feDistantLight" => ctx.cached_instance(id, || DomSvgFeDistantLightElement::from_node(node)),
        "fePointLight" => ctx.cached_instance(id, || DomSvgFePointLightElement::from_node(node)),
        "feSpotLight" => ctx.cached_instance(id, || DomSvgFeSpotLightElement::from_node(node)),
        "feDisplacementMap" => ctx.cached_instance(id, || DomSvgFeDisplacementMapElement::from_node(node)),
        "feDropShadow" => ctx.cached_instance(id, || DomSvgFeDropShadowElement::from_node(node)),
        "feFlood" => ctx.cached_instance(id, || DomSvgFeFloodElement::from_node(node)),
        "feGaussianBlur" => ctx.cached_instance(id, || DomSvgFeGaussianBlurElement::from_node(node)),
        "feImage" => ctx.cached_instance(id, || DomSvgFeImageElement::from_node(node)),
        "feMerge" => ctx.cached_instance(id, || DomSvgFeMergeElement::from_node(node)),
        "feMergeNode" => ctx.cached_instance(id, || DomSvgFeMergeNodeElement::from_node(node)),
        "feMorphology" => ctx.cached_instance(id, || DomSvgFeMorphologyElement::from_node(node)),
        "feOffset" => ctx.cached_instance(id, || DomSvgFeOffsetElement::from_node(node)),
        "feSpecularLighting" => ctx.cached_instance(id, || DomSvgFeSpecularLightingElement::from_node(node)),
        "feTile" => ctx.cached_instance(id, || DomSvgFeTileElement::from_node(node)),
        "feTurbulence" => ctx.cached_instance(id, || DomSvgFeTurbulenceElement::from_node(node)),
        _ => ctx.cached_instance(id, || DomSvgElement::from_node(node)),
    }
}

pub(crate) fn constructors(ctx: &mut Ctx) -> Vec<(&'static str, Value)> {
    vec![
        ("SVGAnimatedString", ctx.class_constructor::<DomSvgAnimatedString>()),
        ("SVGAnimatedEnumeration", ctx.class_constructor::<DomSvgAnimatedEnumeration>()),
        ("SVGAnimatedNumber", ctx.class_constructor::<DomSvgAnimatedNumber>()),
        ("SVGAnimatedInteger", ctx.class_constructor::<DomSvgAnimatedInteger>()),
        ("SVGAnimatedBoolean", ctx.class_constructor::<DomSvgAnimatedBoolean>()),
        ("SVGAnimatedLength", ctx.class_constructor::<DomSvgAnimatedLength>()),
        ("SVGLength", ctx.class_constructor::<DomSvgLength>()),
        ("SVGNumber", ctx.class_constructor::<DomSvgNumber>()),
        ("SVGAnimatedNumberList", ctx.class_constructor::<DomSvgAnimatedNumberList>()),
        ("SVGNumberList", ctx.class_constructor::<DomSvgNumberList>()),
        ("SVGAnimatedPreserveAspectRatio", ctx.class_constructor::<DomSvgAnimatedPreserveAspectRatio>()),
        ("SVGPreserveAspectRatio", ctx.class_constructor::<DomSvgPreserveAspectRatio>()),
        ("SVGFilterElement", ctx.class_constructor::<DomSvgFilterElement>()),
        ("SVGFEBlendElement", ctx.class_constructor::<DomSvgFeBlendElement>()),
        ("SVGFEColorMatrixElement", ctx.class_constructor::<DomSvgFeColorMatrixElement>()),
        ("SVGFEComponentTransferElement", ctx.class_constructor::<DomSvgFeComponentTransferElement>()),
        ("SVGComponentTransferFunctionElement", ctx.class_constructor::<DomSvgComponentTransferFunctionElement>()),
        ("SVGFEFuncRElement", ctx.class_constructor::<DomSvgFeFuncRElement>()),
        ("SVGFEFuncGElement", ctx.class_constructor::<DomSvgFeFuncGElement>()),
        ("SVGFEFuncBElement", ctx.class_constructor::<DomSvgFeFuncBElement>()),
        ("SVGFEFuncAElement", ctx.class_constructor::<DomSvgFeFuncAElement>()),
        ("SVGFECompositeElement", ctx.class_constructor::<DomSvgFeCompositeElement>()),
        ("SVGFEConvolveMatrixElement", ctx.class_constructor::<DomSvgFeConvolveMatrixElement>()),
        ("SVGFEDiffuseLightingElement", ctx.class_constructor::<DomSvgFeDiffuseLightingElement>()),
        ("SVGFEDistantLightElement", ctx.class_constructor::<DomSvgFeDistantLightElement>()),
        ("SVGFEPointLightElement", ctx.class_constructor::<DomSvgFePointLightElement>()),
        ("SVGFESpotLightElement", ctx.class_constructor::<DomSvgFeSpotLightElement>()),
        ("SVGFEDisplacementMapElement", ctx.class_constructor::<DomSvgFeDisplacementMapElement>()),
        ("SVGFEDropShadowElement", ctx.class_constructor::<DomSvgFeDropShadowElement>()),
        ("SVGFEFloodElement", ctx.class_constructor::<DomSvgFeFloodElement>()),
        ("SVGFEGaussianBlurElement", ctx.class_constructor::<DomSvgFeGaussianBlurElement>()),
        ("SVGFEImageElement", ctx.class_constructor::<DomSvgFeImageElement>()),
        ("SVGFEMergeElement", ctx.class_constructor::<DomSvgFeMergeElement>()),
        ("SVGFEMergeNodeElement", ctx.class_constructor::<DomSvgFeMergeNodeElement>()),
        ("SVGFEMorphologyElement", ctx.class_constructor::<DomSvgFeMorphologyElement>()),
        ("SVGFEOffsetElement", ctx.class_constructor::<DomSvgFeOffsetElement>()),
        ("SVGFESpecularLightingElement", ctx.class_constructor::<DomSvgFeSpecularLightingElement>()),
        ("SVGFETileElement", ctx.class_constructor::<DomSvgFeTileElement>()),
        ("SVGFETurbulenceElement", ctx.class_constructor::<DomSvgFeTurbulenceElement>()),
    ]
}
