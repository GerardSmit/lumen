//! CSSOM algorithms that build on the shared HTML CSS parser and cascade.
//!
//! The DOM adapter installs `CSS.supports` and `matchMedia` from these
//! declarations. Stylesheet edits share the renderer's validated source graph
//! and effective-text overlay while preserving the owning node's author text.
use super::*;
#[path="cssom_font_features.rs"]
mod font_features;
#[path="cssom_transforms.rs"]
mod transforms;
use crate::realm_services::RealmServices;
use lumen::embed::{Deferred, Promise};
use lumen::embed::{JsFunction, JsHost, JsObject};
use lumen_bind::{CtorRet, FromArg, Host, IntoRet, Passed, This};
use lumen_html::css;
#[cfg(test)]
use lumen_html::css::MediaEnvironment;
use std::{borrow::Cow, cell::Cell, collections::HashSet, rc::Weak, sync::Arc};

fn css_error(error: css::CssError) -> OpError {
    OpError::new(
        "SyntaxError",
        format!("CSS error at {}: {}", error.offset, error.message),
    )
}

fn css_rule_dom_exception(ctx: &mut Ctx, error: css::CssError) -> OpError {
    crate::error_reporting::dom_exception(
        ctx,
        "SyntaxError",
        &format!("CSS error at {}: {}", error.offset, error.message),
    )
}

fn starts_css_at_rule(text: &str, name: &str) -> bool {
    css::nesting::at_rule(text.trim_start(), name)
}

#[lumen_bind::class(name = "CSSStyleValue", hint(js(webidl)))]
pub struct DomCssStyleValue {
    serialized_value: RefCell<String>,
}

#[lumen_bind::class(name = "CSSKeywordValue", extends = DomCssStyleValue, hint(js(webidl)))]
pub struct DomCssKeywordValue {
    base: DomCssStyleValue,
}

/// Typed OM intentionally keeps image values opaque and uses the ordinary
/// CSSStyleValue stringifier; URL loading remains the existing style authority.
#[lumen_bind::class(name = "CSSImageValue", extends = DomCssStyleValue, hint(js(webidl)))]
pub struct DomCssImageValue { base: DomCssStyleValue }

#[lumen_bind::methods]
impl DomCssImageValue {}

#[lumen_bind::class(name = "CSSNumericValue", extends = DomCssStyleValue, hint(js(webidl)))]
pub struct DomCssNumericValue {
    base: DomCssStyleValue,
    value: Cell<f64>,
    unit: css::typed_numeric::NumericUnit,
    expression: Option<css::typed_numeric::NumericExpression>,
    values_key: Option<Value>,
}

#[lumen_bind::class(name = "CSSUnitValue", extends = DomCssNumericValue, hint(js(webidl)))]
pub struct DomCssUnitValue {
    base: DomCssNumericValue,
}

#[lumen_bind::class(name = "CSSMathValue", extends = DomCssNumericValue, hint(js(webidl)))]
pub struct DomCssMathValue {
    base: DomCssNumericValue,
}

#[lumen_bind::class(name = "CSSMathSum", extends = DomCssMathValue, hint(js(webidl)))]
pub struct DomCssMathSum {
    base: DomCssMathValue,
}

#[lumen_bind::class(name = "CSSMathProduct", extends = DomCssMathValue, hint(js(webidl)))]
pub struct DomCssMathProduct {
    base: DomCssMathValue,
}

#[lumen_bind::class(name = "CSSMathMin", extends = DomCssMathValue, hint(js(webidl)))]
pub struct DomCssMathMin {
    base: DomCssMathValue,
}

#[lumen_bind::class(name = "CSSMathMax", extends = DomCssMathValue, hint(js(webidl)))]
pub struct DomCssMathMax {
    base: DomCssMathValue,
}

#[lumen_bind::class(name = "CSSMathClamp", extends = DomCssMathValue, hint(js(webidl)))]
pub struct DomCssMathClamp {
    base: DomCssMathValue,
}

#[lumen_bind::class(name = "CSSMathNegate", extends = DomCssMathValue, hint(js(webidl)))]
pub struct DomCssMathNegate {
    base: DomCssMathValue,
}

#[lumen_bind::class(name = "CSSMathInvert", extends = DomCssMathValue, hint(js(webidl)))]
pub struct DomCssMathInvert {
    base: DomCssMathValue,
}

#[lumen_bind::class(name = "CSSNumericArray", hint(js(webidl)))]
pub struct DomCssNumericArray {
    length: usize,
    data_key: Value,
}

#[lumen_bind::class(name = "CSSNumericArrayIterator")]
struct DomCssNumericArrayIterator {
    owner_key: Value,
    kind: NumericArrayIteratorKind,
    index: Cell<usize>,
}

#[derive(Clone, Copy)]
enum NumericArrayIteratorKind {
    Keys,
    Values,
    Entries,
}

#[lumen_bind::class(name = "CSSUnparsedValue", extends = DomCssStyleValue, hint(js(webidl)))]
pub struct DomCssUnparsedValue {
    base: DomCssStyleValue,
    length: Cell<usize>,
    byte_length: Cell<usize>,
}

#[lumen_bind::class(name = "CSSVariableReferenceValue", hint(js(webidl)))]
pub struct DomCssVariableReferenceValue {
    variable: String,
    fallback_key: Value,
}

#[derive(Clone, Copy)]
enum UnparsedIteratorKind {
    Keys,
    Values,
    Entries,
}

#[lumen_bind::class(name = "CSSUnparsedValueIterator")]
struct DomCssUnparsedValueIterator {
    owner_key: Value,
    kind: UnparsedIteratorKind,
    index: Cell<usize>,
}

struct UnparsedValueConstructor {
    segments: Vec<Value>,
}

struct VariableReferenceConstructor {
    variable: String,
    fallback_key: Value,
    fallback: Value,
}

struct UnitValueConstructor {
    value: f64,
    unit: css::typed_numeric::NumericUnit,
}

#[derive(Clone, Copy)]
enum NumericMathOperator {
    Sum,
    Product,
    Min,
    Max,
    Clamp,
    Negate,
    Invert,
}

struct CssNumberish {
    value: Value,
    expression: css::typed_numeric::NumericExpression,
}

struct MathValueConstructor {
    expression: css::typed_numeric::NumericExpression,
    values: Vec<Value>,
}

impl<'a> FromArg<'a, JsHost> for CssNumberish {
    fn from_arg(
        cx: &'a <JsHost as Host>::Cx<'_>,
        value: &'a Value,
        at: lumen_bind::Slot,
    ) -> Result<Self, Value> {
        let numeric = <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
            let Ok(state) = ctx.with_instance::<DomCssNumericValue, _>(value, |numeric| {
                (
                    numeric.expression.is_some(),
                    numeric.value.get(),
                    numeric.unit,
                )
            }) else {
                return Ok(None);
            };
            let expression = if state.0 {
                live_math_expression(ctx, value).map_err(|error| error.to_value(ctx))?
            } else {
                css::typed_numeric::NumericExpression::Value(css::typed_numeric::NumericValue {
                    value: state.1,
                    unit: state.2,
                })
            };
            Ok(Some(expression))
        })?;
        if let Some(expression) = numeric {
            return Ok(Self {
                value: value.clone(),
                expression,
            });
        }
        let number = <JsHost as Host>::to_f64(cx, value, at)?;
        if !number.is_finite() {
            return Err(<JsHost as Host>::with_ctx(cx, |ctx| {
                ctx.make_error("TypeError", "CSS numeric values must be finite")
            }));
        }
        Ok(Self {
            value: Value::Undefined,
            expression: css::typed_numeric::NumericExpression::Value(
                css::typed_numeric::NumericValue {
                    value: number,
                    unit: css::typed_numeric::NumericUnit::Number,
                },
            ),
        })
    }
}

macro_rules! numeric_unit_factory {
    ($function:ident, $js_name:literal, $unit:ident) => {
        #[lumen_bind::op(name = $js_name, coerce)]
        pub fn $function(ctx: &mut Ctx, value: f64) -> OpResult<Value> {
            if !value.is_finite() {
                return Err(OpError::type_error("CSS numeric values must be finite"));
            }
            Ok(unit_value(
                ctx,
                value,
                css::typed_numeric::NumericUnit::$unit,
            ))
        }
    };
}

mod numeric_factories {
    use super::*;



    numeric_unit_factory!(number, "number", Number);
    numeric_unit_factory!(percent, "percent", Percent);
    numeric_unit_factory!(cap, "cap", Cap);
    numeric_unit_factory!(ch, "ch", Ch);
    numeric_unit_factory!(em, "em", Em);
    numeric_unit_factory!(ex, "ex", Ex);
    numeric_unit_factory!(ic, "ic", Ic);
    numeric_unit_factory!(lh, "lh", Lh);
    numeric_unit_factory!(rcap, "rcap", Rcap);
    numeric_unit_factory!(rch, "rch", Rch);
    numeric_unit_factory!(rem, "rem", Rem);
    numeric_unit_factory!(rex, "rex", Rex);
    numeric_unit_factory!(ric, "ric", Ric);
    numeric_unit_factory!(rlh, "rlh", Rlh);
    numeric_unit_factory!(vw, "vw", Vw);
    numeric_unit_factory!(vh, "vh", Vh);
    numeric_unit_factory!(vi, "vi", Vi);
    numeric_unit_factory!(vb, "vb", Vb);
    numeric_unit_factory!(vmin, "vmin", Vmin);
    numeric_unit_factory!(vmax, "vmax", Vmax);
    numeric_unit_factory!(svw, "svw", Svw);
    numeric_unit_factory!(svh, "svh", Svh);
    numeric_unit_factory!(svi, "svi", Svi);
    numeric_unit_factory!(svb, "svb", Svb);
    numeric_unit_factory!(svmin, "svmin", Svmin);
    numeric_unit_factory!(svmax, "svmax", Svmax);
    numeric_unit_factory!(lvw, "lvw", Lvw);
    numeric_unit_factory!(lvh, "lvh", Lvh);
    numeric_unit_factory!(lvi, "lvi", Lvi);
    numeric_unit_factory!(lvb, "lvb", Lvb);
    numeric_unit_factory!(lvmin, "lvmin", Lvmin);
    numeric_unit_factory!(lvmax, "lvmax", Lvmax);
    numeric_unit_factory!(dvw, "dvw", Dvw);
    numeric_unit_factory!(dvh, "dvh", Dvh);
    numeric_unit_factory!(dvi, "dvi", Dvi);
    numeric_unit_factory!(dvb, "dvb", Dvb);
    numeric_unit_factory!(dvmin, "dvmin", Dvmin);
    numeric_unit_factory!(dvmax, "dvmax", Dvmax);
    numeric_unit_factory!(cqw, "cqw", Cqw);
    numeric_unit_factory!(cqh, "cqh", Cqh);
    numeric_unit_factory!(cqi, "cqi", Cqi);
    numeric_unit_factory!(cqb, "cqb", Cqb);
    numeric_unit_factory!(cqmin, "cqmin", Cqmin);
    numeric_unit_factory!(cqmax, "cqmax", Cqmax);
    numeric_unit_factory!(cm, "cm", Cm);
    numeric_unit_factory!(mm, "mm", Mm);
    numeric_unit_factory!(q, "Q", Q);
    numeric_unit_factory!(inch, "in", In);
    numeric_unit_factory!(pt, "pt", Pt);
    numeric_unit_factory!(pc, "pc", Pc);
    numeric_unit_factory!(px, "px", Px);
    numeric_unit_factory!(deg, "deg", Deg);
    numeric_unit_factory!(grad, "grad", Grad);
    numeric_unit_factory!(rad, "rad", Rad);
    numeric_unit_factory!(turn, "turn", Turn);
    numeric_unit_factory!(s, "s", S);
    numeric_unit_factory!(ms, "ms", Ms);
    numeric_unit_factory!(hz, "Hz", Hz);
    numeric_unit_factory!(khz, "kHz", KHz);
    numeric_unit_factory!(dpi, "dpi", Dpi);
    numeric_unit_factory!(dpcm, "dpcm", Dpcm);
    numeric_unit_factory!(dppx, "dppx", Dppx);
    numeric_unit_factory!(fr, "fr", Fr);
}

macro_rules! install_numeric_factory {
    ($ctx:ident, $namespace:ident, $name:literal, $operation:path) => {{
        let factory = $ctx.bound_function(&lumen_bind::FnItem::of::<$operation>());
        $ctx.set_member(&$namespace, $name, factory)
            .map_err(|_| OpError::new("Error", "CSS numeric factory installation failed"))?;
    }};
}

#[lumen_bind::class(name = "StylePropertyMapReadOnly", hint(js(webidl)))]
pub struct DomStylePropertyMapReadOnly {
    pub(super) realm: Rc<DomRealm>,
    pub(super) node: NodeId,
    computed: bool,
    _owner: Value,
}

#[lumen_bind::class(name = "StylePropertyMap", extends = DomStylePropertyMapReadOnly, hint(js(webidl)))]
pub struct DomStylePropertyMap {
    base: DomStylePropertyMapReadOnly,
}

fn css_identifier_value(input: &str) -> Option<String> {
    fn name_start(character: char) -> bool {
        character == '_' || character.is_ascii_alphabetic() || character >= '\u{80}'
    }
    fn name_character(character: char) -> bool {
        name_start(character) || character == '-' || character.is_ascii_digit()
    }
    fn escape(iter: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<char> {
        let first = iter.next()?;
        if matches!(first, '\n' | '\r' | '\u{c}') {
            return None;
        }
        if !first.is_ascii_hexdigit() {
            return Some(first);
        }
        let mut value = first.to_digit(16)?;
        let mut digits = 1;
        while digits < 6
            && iter
                .peek()
                .is_some_and(|character| character.is_ascii_hexdigit())
        {
            value = value * 16 + iter.next()?.to_digit(16)?;
            digits += 1;
        }
        if iter
            .peek()
            .is_some_and(|character| matches!(character, '\t' | '\n' | '\u{c}' | '\r' | ' '))
        {
            iter.next();
        }
        Some(
            char::from_u32(value)
                .filter(|character| *character != '\0')
                .unwrap_or('\u{fffd}'),
        )
    }

    let mut iter = input.chars().peekable();
    let first = iter.next()?;
    let mut value = String::new();
    if first == '-' {
        value.push('-');
        match iter.next()? {
            '\\' => value.push(escape(&mut iter)?),
            '-' => value.push('-'),
            character if name_start(character) => value.push(character),
            _ => return None,
        }
    } else if first == '\\' {
        value.push(escape(&mut iter)?);
    } else if name_start(first) {
        value.push(first);
    } else {
        return None;
    }
    while let Some(character) = iter.next() {
        match character {
            '\\' => value.push(escape(&mut iter)?),
            character if name_character(character) => value.push(character),
            _ => return None,
        }
    }
    (!value.is_empty()).then_some(value)
}

pub(crate) fn style_value_from_text(ctx: &mut Ctx, property: &str, css_text: &str) -> OpResult<Value> {
    let mut values = style_values_from_text(ctx, property, css_text, false)?;
    values
        .pop()
        .ok_or_else(|| OpError::type_error("CSS value did not produce a Typed OM value"))
}

pub(crate) fn registered_style_value(ctx: &mut Ctx, property: &str, text: &str, syntax: Option<&str>) -> OpResult<Value> {
    if syntax.is_some_and(|syntax|syntax.trim()!="*") {
        if let Some(value)=css::typed_numeric::parse_numeric_value(text) {
            return Ok(unit_value(ctx,value.value,value.unit));
        }
        if syntax==Some("<color>") {return style_value_from_text(ctx,"color",text);}
        if let Some(grammar)=syntax.and_then(|syntax|syntax.split('|').find(|grammar|css::registered_properties::accepts_value(grammar,text))) {
            let grammar=grammar.trim().trim_end_matches(['+','#']);
            if grammar=="<image>" {
                return Ok(ctx.new_instance(DomCssImageValue{base:DomCssStyleValue{serialized_value:RefCell::new(text.to_owned())}}));
            }
            if matches!(grammar,"<transform-function>"|"<transform-list>") {
                let value=transforms::parse(ctx,text)?;
                if grammar=="<transform-function>"&&transforms::is_value(ctx,&value) {return ctx.reflect_get(&value,&Value::str("0"),&value).map_err(OpError::thrown);}
                return Ok(value);
            }
        }
        return style_value_from_iteration(ctx, property, text);
    }
    style_value_from_text(ctx,property,text)
}

fn style_values_from_text(
    ctx: &mut Ctx,
    property: &str,
    css_text: &str,
    parse_multiple: bool,
) -> OpResult<Vec<Value>> {
    if css_text.len() > MAX_UNPARSED_BYTES {
        return Err(OpError::type_error(
            "CSS text exceeds the Typed OM component-value limit",
        ));
    }
    let property = if property.starts_with("--") {
        property.to_owned()
    } else {
        property.to_ascii_lowercase()
    };
    let components = css::parse_unparsed_value(css_text)
        .map_err(|_| OpError::type_error("CSS text is invalid for the requested property"))?;
    // Empty custom token sequences remain distinct from an absent property.
    if property.starts_with("--") && components.is_empty() {
        return Ok(vec![unparsed_value_from_components(ctx,&components)?]);
    }
    // A transform list reifies specified function identities directly, including
    // legal 3D geometry values independently of used rendering reference boxes.
    if property=="transform" && !has_variable_component(&components) && !css_identifier_value(css_text.trim()).is_some_and(|keyword|matches!(keyword.to_ascii_lowercase().as_str(),"none"|"initial"|"inherit"|"unset"|"revert"|"revert-layer")) {
        return Ok(vec![transforms::parse(ctx,css_text)?]);
    }
    if !css::supports_declaration(&property, css_text) {
        return Err(OpError::type_error(
            "CSS text is invalid for the requested property",
        ));
    }

    let custom_property = css::is_valid_custom_property_name(&property);
    let contains_variable = has_variable_component(&components);
    if custom_property || contains_variable {
        return Ok(vec![unparsed_value_from_components(ctx, &components)?]);
    }

    let iterations = css::typed_numeric::property_value_iterations(&property, css_text)
        .ok_or_else(|| OpError::type_error("CSS value could not be subdivided"))?;
    let iterations = if parse_multiple {
        iterations
    } else {
        iterations.into_iter().take(1).collect()
    };
    let mut values = Vec::with_capacity(iterations.len());
    for iteration in iterations {
        values.push(style_value_from_iteration(ctx, &property, &iteration)?);
    }
    Ok(values)
}

fn style_value_from_iteration(ctx: &mut Ctx, property: &str, css_text: &str) -> OpResult<Value> {
    if css::declaration_block::is_shorthand(property) {
        if let Some(keyword)=css_identifier_value(css_text.trim()).filter(|keyword|
            matches!(keyword.to_ascii_lowercase().as_str(),"initial"|"inherit"|"unset"|"revert"|"revert-layer")) {
            return Ok(ctx.new_instance(keyword_value(keyword)));
        }
        return Ok(ctx.new_instance(DomCssStyleValue {
            serialized_value:RefCell::new(css::serialize_cssom_property_value(property,css_text)
                .unwrap_or_else(||css_text.trim().to_owned())),
        }));
    }
    if let Some(numeric) = css::typed_numeric::parse_property_numeric_value(property, css_text) {
        return Ok(unit_value(ctx, numeric.value, numeric.unit));
    }
    if let Some(mut expression) = css::typed_numeric::parse_numeric_expression(css_text) {
        if !expression.contains_sign() && !expression.contains_tree_functions() && expression.numeric_type().is_some() {
            expression.simplify_absolute_units();
            return reify_numeric_expression(ctx, &expression);
        }
    }
    if let Some(value) = css_identifier_value(css_text.trim()) {
        return Ok(ctx.new_instance(keyword_value(value)));
    }
    if matches!(property,"background-image"|"list-style-image"|"border-image-source")
        && css::registered_properties::accepts_value("<image>",css_text) {
        return Ok(ctx.new_instance(DomCssImageValue{base:DomCssStyleValue{serialized_value:RefCell::new(
            css::serialize_cssom_property_value(property,css_text).unwrap_or_else(||css_text.trim().to_owned()))}}));
    }
    Ok(ctx.new_instance(DomCssStyleValue {
        serialized_value: RefCell::new(
            css::serialize_cssom_property_value(property, css_text)
                .unwrap_or_else(|| css_text.trim().to_owned()),
        ),
    }))
}

fn unit_value(ctx: &mut Ctx, value: f64, unit: css::typed_numeric::NumericUnit) -> Value {
    let serialized_value = css::typed_numeric::serialize_numeric_value(value, unit);
    ctx.new_instance(DomCssUnitValue {
        base: DomCssNumericValue {
            base: DomCssStyleValue {
                serialized_value: RefCell::new(serialized_value),
            },
            value: Cell::new(value),
            unit,
            expression: None,
            values_key: None,
        },
    })
}
pub(crate) fn timeline_percent(ctx: &mut Ctx, value: f64) -> Value {
    unit_value(ctx, value, css::typed_numeric::NumericUnit::Percent)
}

fn math_operator_name(expression: &css::typed_numeric::NumericExpression) -> &'static str {
    use css::typed_numeric::NumericExpression as Expression;
    match expression {
        Expression::Sum(_) | Expression::Calc(_) => "sum",
        Expression::Product(_) => "product",
        Expression::Min(_) => "min",
        Expression::Max(_) => "max",
        Expression::Clamp(..) => "clamp",
        Expression::Negate(_) => "negate",
        Expression::Invert(_) => "invert",
        Expression::Sign(_) | Expression::Function(..) | Expression::Value(_) | Expression::Identifier(_) => "sum",
    }
}

fn numeric_type_object(
    ctx: &mut Ctx,
    numeric_type: css::typed_numeric::NumericType,
) -> OpResult<Value> {
    use css::typed_numeric::NumericDimension as Dimension;
    // CSSNumericType is a WebIDL dictionary. Return a fresh ordinary object,
    // with only the nonzero members present, and create the own data properties
    // directly so a modified Object.prototype setter cannot intercept them.
    let object = Value::Obj(ctx.new_object());
    for (name, exponent) in [
        ("length", numeric_type.length),
        ("angle", numeric_type.angle),
        ("time", numeric_type.time),
        ("frequency", numeric_type.frequency),
        ("resolution", numeric_type.resolution),
        ("flex", numeric_type.flex),
        ("percent", numeric_type.percent),
    ] {
        if exponent != 0 {
            define_data_property(
                ctx,
                &object,
                Value::str(name),
                Value::Num(f64::from(exponent)),
                true,
                true,
                true,
            )
            .map_err(OpError::thrown)?;
        }
    }
    if let Some(hint) = numeric_type.percent_hint {
        let name = match hint {
            Dimension::Number => "number",
            Dimension::Percent => "percent",
            Dimension::Length => "length",
            Dimension::Angle => "angle",
            Dimension::Time => "time",
            Dimension::Frequency => "frequency",
            Dimension::Resolution => "resolution",
            Dimension::Flex => "flex",
        };
        define_data_property(
            ctx,
            &object,
            Value::str("percentHint"),
            Value::str(name),
            true,
            true,
            true,
        )
        .map_err(OpError::thrown)?;
    }
    Ok(object)
}

fn numeric_array(ctx: &mut Ctx, values: Vec<Value>) -> Result<Value, Value> {
    let data_key = ctx.new_symbol(Some("CSSNumericArray.data".into()));
    let array = ctx.new_instance(DomCssNumericArray {
        length: values.len(),
        data_key: data_key.clone(),
    });
    let values = JsHost::from_list(ctx, values);
    define_data_property(ctx, &array, data_key, values, false, false, false)?;
    Ok(array)
}

fn math_numeric_base(
    expression: css::typed_numeric::NumericExpression,
    values_key: Value,
) -> OpResult<DomCssMathValue> {
    if !expression.within_limits() || expression.numeric_type().is_none() {
        return Err(OpError::type_error(
            "CSS math expression is invalid or exceeds its limit",
        ));
    }
    let serialized = expression
        .serialize()
        .ok_or_else(|| OpError::range_error("CSS math serialization exceeds its limit"))?;
    Ok(DomCssMathValue {
        base: DomCssNumericValue {
            base: DomCssStyleValue {
                serialized_value: RefCell::new(serialized),
            },
            value: Cell::new(0.0),
            unit: css::typed_numeric::NumericUnit::Number,
            expression: Some(expression),
            values_key: Some(values_key),
        },
    })
}

fn create_math_instance<T>(
    ctx: &mut Ctx,
    expression: css::typed_numeric::NumericExpression,
    values: Vec<Value>,
    make: impl FnOnce(DomCssMathValue) -> T,
) -> OpResult<Value>
where
    T: lumen_bind::Methods<JsHost>,
{
    let values_key = ctx.new_symbol(Some("CSSMathValue.values".into()));
    let base = math_numeric_base(expression, values_key.clone())?;
    let instance = ctx.new_instance(make(base));
    let values = numeric_array(ctx, values).map_err(OpError::thrown)?;
    define_data_property(ctx, &instance, values_key, values, false, false, false)
        .map_err(OpError::thrown)?;
    Ok(instance)
}

fn math_values(ctx: &mut Ctx, this: &Value) -> OpResult<Value> {
    let key =
        ctx.with_instance::<DomCssNumericValue, _>(this, |numeric| numeric.values_key.clone())?;
    let key = key.ok_or_else(|| OpError::type_error("CSS math value has no numeric arguments"))?;
    ctx.reflect_get(this, &key, this).map_err(OpError::thrown)
}

fn math_child(ctx: &mut Ctx, this: &Value, index: usize) -> OpResult<Value> {
    let values = math_values(ctx, this)?;
    ctx.reflect_get(&values, &Value::str(index.to_string()), &values)
        .map_err(OpError::thrown)
}

fn numeric_expression_from_value(
    ctx: &mut Ctx,
    value: &Value,
) -> OpResult<css::typed_numeric::NumericExpression> {
    let is_math =
        ctx.with_instance::<DomCssNumericValue, _>(value, |numeric| numeric.expression.is_some())?;
    if is_math {
        // Keep nested math values live as well. A parent stores the actual child
        // objects in its CSSNumericArray, so a later mutation to a nested
        // CSSUnitValue must be observed when the parent is serialized or used
        // as a StylePropertyMap value.
        live_math_expression(ctx, value)
    } else {
        ctx.with_instance::<DomCssNumericValue, _>(value, |numeric| {
            css::typed_numeric::NumericExpression::Value(css::typed_numeric::NumericValue {
                value: numeric.value.get(),
                unit: numeric.unit,
            })
        })
    }
}

fn live_math_expression(
    ctx: &mut Ctx,
    this: &Value,
) -> OpResult<css::typed_numeric::NumericExpression> {
    use css::typed_numeric::NumericExpression as Expression;

    let template = ctx.with_instance::<DomCssNumericValue, _>(this, |numeric| {
        (numeric.expression.clone(), numeric.values_key.clone())
    })?;
    let (Some(template), Some(_)) = template else {
        return Err(OpError::type_error("CSS value is not a CSSMathValue"));
    };
    let values = math_values(ctx, this)?;
    let length = ctx.with_instance::<DomCssNumericArray, _>(&values, |array| array.length)?;
    if length == 0 || length > css::typed_numeric::MAX_NUMERIC_EXPRESSION_ARGS {
        return Err(OpError::range_error(
            "CSS math values exceed their argument limit",
        ));
    }
    let mut children = Vec::new();
    children
        .try_reserve_exact(length)
        .map_err(|_| OpError::range_error("CSS math values exceed their argument limit"))?;
    for index in 0..length {
        let child = ctx
            .reflect_get(&values, &Value::str(index.to_string()), &values)
            .map_err(OpError::thrown)?;
        children.push(numeric_expression_from_value(ctx, &child)?);
    }
    let expression = match template {
        Expression::Sum(_) | Expression::Calc(_) if !children.is_empty() => {
            Expression::Sum(children)
        }
        Expression::Product(_) if !children.is_empty() => Expression::Product(children),
        Expression::Min(_) if !children.is_empty() => Expression::Min(children),
        Expression::Max(_) if !children.is_empty() => Expression::Max(children),
        Expression::Clamp(..) if children.len() == 3 => {
            let mut children = children.into_iter();
            let lower = children
                .next()
                .ok_or_else(|| OpError::type_error("invalid clamp"))?;
            let value = children
                .next()
                .ok_or_else(|| OpError::type_error("invalid clamp"))?;
            let upper = children
                .next()
                .ok_or_else(|| OpError::type_error("invalid clamp"))?;
            Expression::Clamp(Box::new(lower), Box::new(value), Box::new(upper))
        }
        Expression::Negate(_) if children.len() == 1 => Expression::Negate(Box::new(
            children
                .into_iter()
                .next()
                .ok_or_else(|| OpError::type_error("invalid negate"))?,
        )),
        Expression::Invert(_) if children.len() == 1 => Expression::Invert(Box::new(
            children
                .into_iter()
                .next()
                .ok_or_else(|| OpError::type_error("invalid invert"))?,
        )),
        _ => return Err(OpError::type_error("CSS math value has invalid arguments")),
    };
    if !expression.within_limits() || expression.numeric_type().is_none() {
        return Err(OpError::type_error(
            "CSS math value has incompatible numeric types",
        ));
    }
    Ok(expression)
}

fn numeric_value_text(ctx: &mut Ctx, this: &Value) -> OpResult<String> {
    numeric_value_text_with_minimum(ctx,this,None)
}

fn numeric_value_text_with_minimum(ctx: &mut Ctx, this: &Value, minimum: Option<css::typed_numeric::NumericValue>) -> OpResult<String> {
    let expression = ctx.with_instance::<DomCssNumericValue, _>(this, |numeric| {
        (
            numeric.expression.is_some(),
            numeric.value.get(),
            numeric.unit,
        )
    })?;
    let (is_math, value, unit) = expression;
    if is_math {
        live_math_expression(ctx, this)?
            .serialize()
            .ok_or_else(|| OpError::range_error("CSS math serialization exceeds its limit"))
    } else {
        let text=css::typed_numeric::serialize_numeric_value(value, unit);
        let outside=minimum.is_some_and(|minimum|{
            match (unit.canonical_unit_and_factor(),minimum.unit.canonical_unit_and_factor()) {
                (Some((unit,factor)),Some((other,other_factor))) if unit==other=>value*factor<minimum.value*other_factor,
                _=>true,
            }
        });
        Ok(if outside{format!("calc({text})")}else{text})
    }
}

fn numeric_array_iterator(
    ctx: &mut Ctx,
    owner: Value,
    kind: NumericArrayIteratorKind,
) -> OpResult<Value> {
    let owner_key = ctx.new_symbol(Some("CSSNumericArrayIterator.owner".into()));
    let iterator = ctx.new_instance(DomCssNumericArrayIterator {
        owner_key: owner_key.clone(),
        kind,
        index: Cell::new(0),
    });
    define_data_property(ctx, &iterator, owner_key, owner, false, false, false)
        .map_err(OpError::thrown)?;
    Ok(iterator)
}

fn make_math_constructor(
    ctx: &mut Ctx,
    operator: NumericMathOperator,
    values: Vec<CssNumberish>,
) -> OpResult<MathValueConstructor> {
    use css::typed_numeric::NumericExpression as Expression;
    use NumericMathOperator as Operator;
    if values.is_empty() {
        return Err(crate::error_reporting::dom_exception(
            ctx,
            "SyntaxError",
            "CSS math constructors require at least one numeric value",
        ));
    }
    if values.len() > css::typed_numeric::MAX_NUMERIC_EXPRESSION_ARGS {
        return Err(OpError::range_error("too many CSS math values"));
    }
    let mut expressions = Vec::new();
    expressions
        .try_reserve_exact(values.len())
        .map_err(|_| OpError::range_error("too many CSS math values"))?;
    let mut js_values = Vec::new();
    js_values
        .try_reserve_exact(values.len())
        .map_err(|_| OpError::range_error("too many CSS math values"))?;
    for value in values {
        let CssNumberish { value, expression } = value;
        let value = if matches!(value, Value::Undefined) {
            let Expression::Value(numeric) = expression else {
                return Err(OpError::type_error("invalid CSSNumberish conversion"));
            };
            unit_value(ctx, numeric.value, numeric.unit)
        } else {
            value
        };
        expressions.push(expression);
        js_values.push(value);
    }
    let expression =
        match operator {
            Operator::Sum => Expression::Sum(expressions),
            Operator::Product => Expression::Product(expressions),
            Operator::Min => Expression::Min(expressions),
            Operator::Max => Expression::Max(expressions),
            Operator::Clamp if expressions.len() == 3 => {
                let mut values = expressions.into_iter();
                Expression::Clamp(
                    Box::new(values.next().ok_or_else(|| {
                        OpError::type_error("CSSMathClamp requires three values")
                    })?),
                    Box::new(values.next().ok_or_else(|| {
                        OpError::type_error("CSSMathClamp requires three values")
                    })?),
                    Box::new(values.next().ok_or_else(|| {
                        OpError::type_error("CSSMathClamp requires three values")
                    })?),
                )
            }
            Operator::Negate if expressions.len() == 1 => Expression::Negate(Box::new(
                expressions
                    .into_iter()
                    .next()
                    .ok_or_else(|| OpError::type_error("CSSMathNegate requires one value"))?,
            )),
            Operator::Invert if expressions.len() == 1 => Expression::Invert(Box::new(
                expressions
                    .into_iter()
                    .next()
                    .ok_or_else(|| OpError::type_error("CSSMathInvert requires one value"))?,
            )),
            _ => return Err(OpError::type_error("invalid number of CSS math values")),
        };
    if !expression.within_limits() || expression.numeric_type().is_none() {
        return Err(OpError::type_error(
            "CSS math values have incompatible numeric types",
        ));
    }
    Ok(MathValueConstructor {
        expression,
        values: js_values,
    })
}

fn reify_numeric_expression(
    ctx: &mut Ctx,
    expression: &css::typed_numeric::NumericExpression,
) -> OpResult<Value> {
    use css::typed_numeric::NumericExpression as Expression;

    fn children(ctx: &mut Ctx, values: &[Expression]) -> OpResult<Vec<Value>> {
        let mut result = Vec::new();
        result
            .try_reserve_exact(values.len())
            .map_err(|_| OpError::range_error("CSS math values exceed their argument limit"))?;
        for value in values {
            result.push(reify_numeric_expression(ctx, value)?);
        }
        Ok(result)
    }

    if !expression.within_limits() {
        return Err(OpError::range_error(
            "CSS math expression exceeds its limit",
        ));
    }
    match expression {
        Expression::Value(value) => Ok(unit_value(ctx, value.value, value.unit)),
        Expression::Identifier(_)=>Err(OpError::type_error("color channels are not standalone CSS numeric values")),
        Expression::Calc(value) => {
            // `calc()` is syntax around the numeric expression, not a Typed
            // OM operator of its own. Keep the historical singleton-sum
            // representation for `calc(10px)`, but reify an actual product
            // (or min/max/clamp/etc.) as its corresponding math interface.
            // In particular `calc(2 * min(...))` is a CSSMathProduct, not a
            // one-child CSSMathSum containing that product.
            if matches!(value.as_ref(), Expression::Value(_)) {
                let sum = Expression::Sum(vec![value.as_ref().clone()]);
                let values = match &sum {
                    Expression::Sum(values) => children(ctx, values)?,
                    _ => return Err(OpError::type_error("invalid calc expression")),
                };
                create_math_instance(ctx, sum, values, |base| DomCssMathSum { base })
            } else {
                reify_numeric_expression(ctx, value)
            }
        }
        Expression::Sum(values) => {
            let children = children(ctx, values)?;
            create_math_instance(ctx, expression.clone(), children, |base| DomCssMathSum {
                base,
            })
        }
        Expression::Product(values) => {
            let children = children(ctx, values)?;
            create_math_instance(ctx, expression.clone(), children, |base| {
                DomCssMathProduct { base }
            })
        }
        Expression::Min(values) => {
            let children = children(ctx, values)?;
            create_math_instance(ctx, expression.clone(), children, |base| DomCssMathMin {
                base,
            })
        }
        Expression::Max(values) => {
            let children = children(ctx, values)?;
            create_math_instance(ctx, expression.clone(), children, |base| DomCssMathMax {
                base,
            })
        }
        Expression::Clamp(lower, value, upper) => {
            let values = children(
                ctx,
                &[
                    lower.as_ref().clone(),
                    value.as_ref().clone(),
                    upper.as_ref().clone(),
                ],
            )?;
            create_math_instance(ctx, expression.clone(), values, |base| DomCssMathClamp {
                base,
            })
        }
        Expression::Negate(value) => {
            let values = vec![reify_numeric_expression(ctx, value)?];
            create_math_instance(ctx, expression.clone(), values, |base| DomCssMathNegate {
                base,
            })
        }
        Expression::Invert(value) => {
            let values = vec![reify_numeric_expression(ctx, value)?];
            create_math_instance(ctx, expression.clone(), values, |base| DomCssMathInvert {
                base,
            })
        }
        Expression::Function(..)=>{
            let mut computed=expression.clone();computed.simplify_absolute_units();
            if let Some(value)=computed.single_numeric_value(){Ok(unit_value(ctx,value.value,value.unit))}
            else{Err(OpError::type_error("unresolved math function cannot be reified as a Typed OM operator"))}
        },
        Expression::Sign(_) => Err(OpError::type_error(
            "CSSNumericValue does not support sign() reification",
        )),
    }
}

fn keyword_value(value: String) -> DomCssKeywordValue {
    DomCssKeywordValue {
        base: DomCssStyleValue {
            serialized_value: RefCell::new(value),
        },
    }
}

const MAX_UNPARSED_COMPONENTS: usize = 1024;
const MAX_UNPARSED_DEPTH: usize = 32;
const MAX_UNPARSED_BYTES: usize = 8192;

fn define_data_property(
    ctx: &mut Ctx,
    target: &Value,
    key: Value,
    value: Value,
    writable: bool,
    enumerable: bool,
    configurable: bool,
) -> Result<(), Value> {
    let descriptor = ctx.new_object_with_proto(&Value::Null);
    for (name, field) in [
        ("value", value),
        ("writable", Value::Bool(writable)),
        ("enumerable", Value::Bool(enumerable)),
        ("configurable", Value::Bool(configurable)),
    ] {
        ctx.set_member(&descriptor, name, field)
            .map_err(lumen::embed::abrupt_value)?;
    }
    ctx.define_property_value(target, key, &descriptor)
}

impl CtorRet<JsHost, DomCssUnparsedValue> for UnparsedValueConstructor {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
            let value = ctx.new_instance(DomCssUnparsedValue {
                base: DomCssStyleValue {
                    serialized_value: RefCell::new(String::new()),
                },
                length: Cell::new(0),
                byte_length: Cell::new(0),
            });
            for (index, segment) in self.segments.into_iter().enumerate() {
                ctx.set_member(&value, &index.to_string(), segment)
                    .map_err(lumen::embed::abrupt_value)?;
            }
            Ok(value)
        })
    }
}

impl CtorRet<JsHost, DomCssVariableReferenceValue> for VariableReferenceConstructor {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
            let value = ctx.new_instance(DomCssVariableReferenceValue {
                variable: self.variable,
                fallback_key: self.fallback_key.clone(),
            });
            define_data_property(
                ctx,
                &value,
                self.fallback_key,
                self.fallback,
                false,
                false,
                false,
            )?;
            Ok(value)
        })
    }
}

impl CtorRet<JsHost, DomCssUnitValue> for UnitValueConstructor {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
            Ok(unit_value(ctx, self.value, self.unit))
        })
    }
}

macro_rules! impl_math_value_ctor_ret {
    ($class:ty, $wrapper:ident) => {
        impl CtorRet<JsHost, $class> for MathValueConstructor {
            fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
                let Self { expression, values } = self;
                <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
                    create_math_instance(ctx, expression, values, |base| $wrapper { base })
                        .map_err(|error| error.to_value(ctx))
                })
            }
        }
    };
}

impl_math_value_ctor_ret!(DomCssMathSum, DomCssMathSum);
impl_math_value_ctor_ret!(DomCssMathProduct, DomCssMathProduct);
impl_math_value_ctor_ret!(DomCssMathMin, DomCssMathMin);
impl_math_value_ctor_ret!(DomCssMathMax, DomCssMathMax);
impl_math_value_ctor_ret!(DomCssMathClamp, DomCssMathClamp);
impl_math_value_ctor_ret!(DomCssMathNegate, DomCssMathNegate);
impl_math_value_ctor_ret!(DomCssMathInvert, DomCssMathInvert);

fn variable_reference_value(ctx: &mut Ctx, variable: String, fallback: Value) -> OpResult<Value> {
    let fallback_key = ctx.new_symbol(Some("CSSVariableReferenceValue.fallback".into()));
    let value = ctx.new_instance(DomCssVariableReferenceValue {
        variable,
        fallback_key: fallback_key.clone(),
    });
    define_data_property(ctx, &value, fallback_key, fallback, false, false, false)
        .map_err(OpError::thrown)?;
    Ok(value)
}

fn segment_value(ctx: &mut Ctx, value: Value) -> OpResult<Value> {
    if ctx
        .with_instance::<DomCssVariableReferenceValue, _>(&value, |_| ())
        .is_ok()
    {
        return Ok(value);
    }
    let text = ctx.coerce_string(&value).map_err(OpError::thrown)?;
    Ok(Value::str(lumen::well_formed_utf8(&text).into_owned()))
}

fn segment_byte_length(ctx: &mut Ctx, segment: &Value) -> OpResult<usize> {
    if let Value::Str(text) = segment {
        return Ok(text.len());
    }
    ctx.with_instance::<DomCssVariableReferenceValue, _>(segment, |reference| {
        reference.variable.len()
    })
}

fn unparsed_value_from_members(ctx: &mut Ctx, segments: Vec<Value>) -> OpResult<Value> {
    let value = ctx.new_instance(DomCssUnparsedValue {
        base: DomCssStyleValue {
            serialized_value: RefCell::new(String::new()),
        },
        length: Cell::new(0),
        byte_length: Cell::new(0),
    });
    for (index, segment) in segments.into_iter().enumerate() {
        ctx.set_member(&value, &index.to_string(), segment)
            .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
    }
    Ok(value)
}

fn unparsed_value_from_components(
    ctx: &mut Ctx,
    components: &[css::UnparsedComponent],
) -> OpResult<Value> {
    let mut segments = Vec::with_capacity(components.len());
    for component in components {
        let segment = match component {
            css::UnparsedComponent::Text(text) => Value::str(text),
            css::UnparsedComponent::Variable { name, fallback } => {
                let fallback = match fallback {
                    Some(components) => unparsed_value_from_components(ctx, components)?,
                    None => Value::Null,
                };
                variable_reference_value(ctx, name.clone(), fallback)?
            }
        };
        segments.push(segment);
    }
    unparsed_value_from_members(ctx, segments)
}

fn has_variable_component(components: &[css::UnparsedComponent]) -> bool {
    components.iter().any(|component| match component {
        css::UnparsedComponent::Text(_) => false,
        css::UnparsedComponent::Variable { .. } => true,
    })
}

fn collect_unparsed_components(
    ctx: &mut Ctx,
    value: &Value,
    depth: usize,
    active: &mut HashSet<usize>,
    budget: &mut usize,
    byte_budget: &mut usize,
) -> OpResult<Vec<css::UnparsedComponent>> {
    if depth > MAX_UNPARSED_DEPTH {
        return Err(OpError::range_error(
            "CSSUnparsedValue nesting limit exceeded",
        ));
    }
    if *budget == 0 {
        return Err(OpError::range_error(
            "CSSUnparsedValue has too many segments",
        ));
    }
    let address = ctx
        .object_addr(value)
        .ok_or_else(|| OpError::type_error("value must be a CSSUnparsedValue"))?;
    if !active.insert(address) {
        return Err(OpError::range_error("cyclic CSSUnparsedValue fallback"));
    }
    let length =
        ctx.with_instance::<DomCssUnparsedValue, _>(value, |unparsed| unparsed.length.get())?;
    if length > *budget {
        active.remove(&address);
        return Err(OpError::range_error(
            "CSSUnparsedValue has too many segments",
        ));
    }
    *budget -= length;
    let mut result = Vec::with_capacity(length);
    for index in 0..length {
        let segment = ctx
            .reflect_get(value, &Value::str(index.to_string()), value)
            .map_err(OpError::thrown)?;
        if let Value::Str(text) = segment {
            if text.len() > *byte_budget {
                active.remove(&address);
                return Err(OpError::range_error(
                    "serialized CSSUnparsedValue exceeds its size limit",
                ));
            }
            *byte_budget -= text.len();
            result.push(css::UnparsedComponent::Text(text.to_string()));
            continue;
        }
        let (name, fallback_key) = ctx
            .with_instance::<DomCssVariableReferenceValue, _>(&segment, |reference| {
                (reference.variable.clone(), reference.fallback_key.clone())
            })?;
        if name.len() > *byte_budget {
            active.remove(&address);
            return Err(OpError::range_error(
                "serialized CSSUnparsedValue exceeds its size limit",
            ));
        }
        *byte_budget -= name.len();
        let fallback = ctx
            .reflect_get(&segment, &fallback_key, &segment)
            .map_err(OpError::thrown)?;
        let fallback = if matches!(fallback, Value::Null | Value::Undefined) {
            None
        } else {
            Some(collect_unparsed_components(
                ctx,
                &fallback,
                depth + 1,
                active,
                budget,
                byte_budget,
            )?)
        };
        result.push(css::UnparsedComponent::Variable { name, fallback });
    }
    active.remove(&address);
    Ok(result)
}

fn serialize_unparsed_value(ctx: &mut Ctx, value: &Value) -> OpResult<String> {
    let mut active = HashSet::new();
    let mut budget = MAX_UNPARSED_COMPONENTS;
    let mut byte_budget = MAX_UNPARSED_BYTES;
    let components =
        collect_unparsed_components(ctx, value, 0, &mut active, &mut budget, &mut byte_budget)?;
    css::serialize_unparsed_value(&components).map_err(css_error)
}

#[lumen_bind::methods]
impl DomCssStyleValue {
    #[classmethod(coerce)]
    fn parse(
        ctx: &mut Ctx,
        _class: This<Value>,
        property: &str,
        css_text: &str,
    ) -> OpResult<Value> {
        style_value_from_text(ctx, property, css_text)
    }

    #[classmethod(coerce)]
    fn parse_all(
        ctx: &mut Ctx,
        _class: This<Value>,
        property: &str,
        css_text: &str,
    ) -> OpResult<Value> {
        let values = style_values_from_text(ctx, property, css_text, true)?;
        Ok(JsHost::from_list(ctx, values))
    }

    #[method(name = "toString")]
fn to_string(&self,ctx:&mut Ctx,this:This<Value>) -> OpResult<String> {
        if transforms::is_value(ctx,&this.0){transforms::serialize(ctx,&this.0)}else{Ok(self.serialized_value.borrow().clone())}
    }
}

#[lumen_bind::methods]
impl DomCssKeywordValue {
    #[constructor(coerce)]
    fn new(value: &str) -> OpResult<Self> {
        if value.is_empty() {
            return Err(OpError::new(
                "TypeError",
                "CSSKeywordValue value must not be empty",
            ));
        }
        Ok(keyword_value(value.to_owned()))
    }

    #[getter]
    fn value(&self) -> String {
        self.base.serialized_value.borrow().clone()
    }

    #[proto(str)]
    fn string_coercion(&self) -> String {
        css::serialize_identifier(&self.base.serialized_value.borrow())
    }

    #[setter(coerce)]
    fn set_value(&self, value: &str) -> OpResult<()> {
        if value.is_empty() {
            return Err(OpError::new(
                "TypeError",
                "CSSKeywordValue value must not be empty",
            ));
        }
        *self.base.serialized_value.borrow_mut() = value.to_owned();
        Ok(())
    }
}

#[lumen_bind::methods]
impl DomCssNumericValue {
    #[classmethod(coerce)]
    fn parse(ctx: &mut Ctx, _class: This<Value>, css_text: &str) -> OpResult<Value> {
        let Some(mut expression) = css::typed_numeric::parse_numeric_expression(css_text) else {
            return Err(crate::error_reporting::dom_exception(
                ctx,
                "SyntaxError",
                "CSS text is not a valid CSS numeric expression",
            ));
        };
        if expression.contains_sign() || expression.contains_tree_functions() || expression.numeric_type().is_none() {
            return Err(crate::error_reporting::dom_exception(
                ctx,
                "SyntaxError",
                "CSS numeric expression has an unsupported or incompatible type",
            ));
        }
        expression.simplify_absolute_units();
        reify_numeric_expression(ctx, &expression)
    }

    #[method(name = "type")]
    fn numeric_type(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        let numeric_type = if self.expression.is_some() {
            live_math_expression(ctx, &this.0)?
                .numeric_type()
                .ok_or_else(|| OpError::type_error("CSS math value has no valid numeric type"))?
        } else {
            css::typed_numeric::NumericType::from_unit(self.unit)
        };
        numeric_type_object(ctx, numeric_type)
    }

    #[method(name = "toString")]
    fn to_string(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<String> {
        numeric_value_text(ctx, &this.0)
    }

    #[proto(str)]
    fn string_coercion(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<String> {
        numeric_value_text(ctx, &this.0)
    }
}

#[lumen_bind::methods]
impl DomCssUnitValue {
    #[constructor(coerce)]
    fn new(value: f64, unit: &str) -> OpResult<UnitValueConstructor> {
        if !value.is_finite() {
            return Err(OpError::type_error("CSS numeric values must be finite"));
        }
        let unit = css::typed_numeric::NumericUnit::parse(unit)
            .ok_or_else(|| OpError::type_error("unknown CSS numeric unit"))?;
        Ok(UnitValueConstructor { value, unit })
    }

    #[getter]
    fn value(&self) -> f64 {
        self.base.value.get()
    }

    #[setter]
    fn set_value(&self, value: f64) -> OpResult<()> {
        if !value.is_finite() {
            return Err(OpError::type_error("CSS numeric values must be finite"));
        }
        self.base.value.set(value);
        *self.base.base.serialized_value.borrow_mut() =
            css::typed_numeric::serialize_numeric_value(value, self.base.unit);
        Ok(())
    }

    #[getter]
    fn unit(&self) -> String {
        self.base.unit.as_str().to_owned()
    }
}

#[lumen_bind::methods]
impl DomCssMathValue {
    #[getter]
    fn operator(&self) -> OpResult<String> {
        self.base
            .expression
            .as_ref()
            .map(|expression| math_operator_name(expression).to_owned())
            .ok_or_else(|| OpError::type_error("CSSMathValue has no numeric expression"))
    }
}

macro_rules! impl_math_values_getter {
    ($class:ty, $operator:ident) => {
        #[lumen_bind::methods]
        impl $class {
            #[constructor(coerce)]
            fn new(
                ctx: &mut Ctx,
                #[varargs] values: Vec<CssNumberish>,
            ) -> OpResult<MathValueConstructor> {
                make_math_constructor(ctx, NumericMathOperator::$operator, values)
            }

            #[getter]
            fn values(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
                math_values(ctx, &this.0)
            }
        }
    };
}

impl_math_values_getter!(DomCssMathSum, Sum);
impl_math_values_getter!(DomCssMathProduct, Product);
impl_math_values_getter!(DomCssMathMin, Min);
impl_math_values_getter!(DomCssMathMax, Max);

#[lumen_bind::methods]
impl DomCssMathClamp {
    #[constructor(coerce)]
    fn new(
        ctx: &mut Ctx,
        lower: CssNumberish,
        value: CssNumberish,
        upper: CssNumberish,
    ) -> OpResult<MathValueConstructor> {
        make_math_constructor(ctx, NumericMathOperator::Clamp, vec![lower, value, upper])
    }

    #[getter]
    fn lower(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        math_child(ctx, &this.0, 0)
    }

    #[getter]
    fn value(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        math_child(ctx, &this.0, 1)
    }

    #[getter]
    fn upper(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        math_child(ctx, &this.0, 2)
    }
}

macro_rules! impl_math_unary {
    ($class:ty, $operator:ident) => {
        #[lumen_bind::methods]
        impl $class {
            #[constructor(coerce)]
            fn new(ctx: &mut Ctx, value: CssNumberish) -> OpResult<MathValueConstructor> {
                make_math_constructor(ctx, NumericMathOperator::$operator, vec![value])
            }

            #[getter]
            fn value(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
                math_child(ctx, &this.0, 0)
            }
        }
    };
}

impl_math_unary!(DomCssMathNegate, Negate);
impl_math_unary!(DomCssMathInvert, Invert);

#[lumen_bind::methods]
impl DomCssNumericArray {
    #[proto(len)]
    fn indexed_length(&self) -> usize {
        self.length
    }

    #[getter]
    fn length(&self) -> usize {
        self.length
    }

    #[proto(getitem)]
    fn indexed(&self, ctx: &mut Ctx, this: This<Value>, index: usize) -> OpResult<Value> {
        if index >= self.length {
            return Ok(Value::Undefined);
        }
        let values = ctx
            .reflect_get(&this.0, &self.data_key, &this.0)
            .map_err(OpError::thrown)?;
        ctx.reflect_get(&values, &Value::str(index.to_string()), &values)
            .map_err(OpError::thrown)
    }

    #[proto(iter)]
    fn iter(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        numeric_array_iterator(ctx, this.0, NumericArrayIteratorKind::Values)
    }

    fn keys(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        numeric_array_iterator(ctx, this.0, NumericArrayIteratorKind::Keys)
    }

    fn values(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        numeric_array_iterator(ctx, this.0, NumericArrayIteratorKind::Values)
    }

    fn entries(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        numeric_array_iterator(ctx, this.0, NumericArrayIteratorKind::Entries)
    }

    fn for_each(
        &self,
        ctx: &mut Ctx,
        this: This<Value>,
        callback: JsFunction,
        this_arg: Option<Value>,
    ) -> OpResult<()> {
        let values = ctx
            .reflect_get(&this.0, &self.data_key, &this.0)
            .map_err(OpError::thrown)?;
        for index in 0..self.length {
            let value = ctx
                .reflect_get(&values, &Value::str(index.to_string()), &values)
                .map_err(OpError::thrown)?;
            callback.call(
                ctx,
                this_arg.clone().unwrap_or(Value::Undefined),
                &[value, Value::Num(index as f64), this.0.clone()],
            )?;
        }
        Ok(())
    }
}

#[lumen_bind::methods]
impl DomCssNumericArrayIterator {
    #[proto(iter)]
    fn iter(&self, this: This<Value>) -> Value {
        this.0
    }

    #[proto(next)]
    fn next(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Option<Value>> {
        let owner = ctx
            .reflect_get(&this.0, &self.owner_key, &this.0)
            .map_err(OpError::thrown)?;
        let length = ctx.with_instance::<DomCssNumericArray, _>(&owner, |array| array.length)?;
        let index = self.index.get();
        if index >= length {
            return Ok(None);
        }
        self.index.set(index + 1);
        let item = match self.kind {
            NumericArrayIteratorKind::Keys => Value::Num(index as f64),
            NumericArrayIteratorKind::Values => ctx
                .reflect_get(&owner, &Value::str(index.to_string()), &owner)
                .map_err(OpError::thrown)?,
            NumericArrayIteratorKind::Entries => {
                let value = ctx
                    .reflect_get(&owner, &Value::str(index.to_string()), &owner)
                    .map_err(OpError::thrown)?;
                JsHost::from_list(ctx, vec![Value::Num(index as f64), value])
            }
        };
        Ok(Some(item))
    }
}

#[lumen_bind::methods]
impl DomCssUnparsedValue {
    #[constructor]
    fn new(ctx: &mut Ctx, members: Value) -> OpResult<UnparsedValueConstructor> {
        let mut byte_length = 0usize;
        let segments =
            ctx.convert_iterable(&members, MAX_UNPARSED_COMPONENTS, |ctx, segment| {
                let segment = segment_value(ctx, segment)?;
                byte_length = byte_length.saturating_add(segment_byte_length(ctx, &segment)?);
                if byte_length > MAX_UNPARSED_BYTES {
                    return Err(OpError::range_error(
                        "CSSUnparsedValue exceeds its component size limit",
                    ));
                }
                Ok(segment)
            })?;
        Ok(UnparsedValueConstructor { segments })
    }

    #[proto(getitem)]
    fn item(ctx: &mut Ctx, this: This<Value>, index: usize) -> OpResult<Value> {
        ctx.member_get(&this.0, &index.to_string())
            .map_err(OpError::thrown)
    }

    #[proto(setitem)]
    fn set_item(ctx: &mut Ctx, this: This<Value>, index: usize, value: Value) -> OpResult<()> {
        let length =
            ctx.with_instance::<DomCssUnparsedValue, _>(&this.0, |unparsed| unparsed.length.get())?;
        if index > length || index >= u32::MAX as usize {
            return Err(OpError::range_error(
                "CSSUnparsedValue index is out of range",
            ));
        }
        let segment = segment_value(ctx, value)?;
        let old_byte_length = if index < length {
            let old = ctx
                .member_get(&this.0, &index.to_string())
                .map_err(OpError::thrown)?;
            segment_byte_length(ctx, &old)?
        } else {
            0
        };
        let segment_size = segment_byte_length(ctx, &segment)?;
        let byte_length = ctx.with_instance::<DomCssUnparsedValue, _>(&this.0, |unparsed| {
            unparsed
                .byte_length
                .get()
                .saturating_sub(old_byte_length)
                .saturating_add(segment_size)
        })?;
        if byte_length > MAX_UNPARSED_BYTES {
            return Err(OpError::range_error(
                "CSSUnparsedValue exceeds its component size limit",
            ));
        }
        define_data_property(
            ctx,
            &this.0,
            Value::str(index.to_string()),
            segment,
            true,
            true,
            true,
        )
        .map_err(OpError::thrown)?;
        if index == length {
            ctx.with_instance_mut::<DomCssUnparsedValue, _>(&this.0, |unparsed| {
                unparsed.length.set(length + 1)
            })?;
        }
        ctx.with_instance_mut::<DomCssUnparsedValue, _>(&this.0, |unparsed| {
            unparsed.byte_length.set(byte_length)
        })?;
        Ok(())
    }

    #[proto(len)]
    fn indexed_length(&self) -> usize {
        self.length.get()
    }

    #[getter]
    fn length(&self) -> usize {
        self.length.get()
    }

    #[proto(str)]
    fn string_coercion(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<String> {
        serialize_unparsed_value(ctx, &this.0)
    }

    #[proto(iter)]
    fn iter(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        unparsed_iterator(ctx, this.0, UnparsedIteratorKind::Values)
    }

    #[method]
    fn keys(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        unparsed_iterator(ctx, this.0, UnparsedIteratorKind::Keys)
    }

    #[method]
    fn values(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        unparsed_iterator(ctx, this.0, UnparsedIteratorKind::Values)
    }

    #[method]
    fn entries(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        unparsed_iterator(ctx, this.0, UnparsedIteratorKind::Entries)
    }
}

#[lumen_bind::methods]
impl DomCssVariableReferenceValue {
    #[constructor(coerce)]
    fn new(
        ctx: &mut Ctx,
        variable: String,
        fallback: Option<Value>,
    ) -> OpResult<VariableReferenceConstructor> {
        let variable = lumen::well_formed_utf8(&variable).into_owned();
        if variable.len() > MAX_UNPARSED_BYTES {
            return Err(OpError::range_error(
                "CSS variable name exceeds its size limit",
            ));
        }
        if !css::is_valid_custom_property_name(&variable) {
            return Err(OpError::type_error("invalid CSS custom property name"));
        }
        let fallback = match fallback.unwrap_or(Value::Null) {
            Value::Null | Value::Undefined => Value::Null,
            value => {
                ctx.with_instance::<DomCssUnparsedValue, _>(&value, |_| ())
                    .map_err(|_| {
                        OpError::type_error(
                            "CSSVariableReferenceValue fallback must be CSSUnparsedValue or null",
                        )
                    })?;
                value
            }
        };
        Ok(VariableReferenceConstructor {
            variable,
            fallback_key: ctx.new_symbol(Some("CSSVariableReferenceValue.fallback".into())),
            fallback,
        })
    }

    #[getter]
    fn variable(&self) -> String {
        self.variable.clone()
    }

    #[setter(coerce)]
    fn set_variable(&mut self, variable: String) -> OpResult<()> {
        let variable = lumen::well_formed_utf8(&variable).into_owned();
        if variable.len() > MAX_UNPARSED_BYTES {
            return Err(OpError::range_error(
                "CSS variable name exceeds its size limit",
            ));
        }
        if !css::is_valid_custom_property_name(&variable) {
            return Err(OpError::type_error("invalid CSS custom property name"));
        }
        self.variable = variable;
        Ok(())
    }

    #[getter]
    fn fallback(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        ctx.reflect_get(&this.0, &self.fallback_key, &this.0)
            .map_err(OpError::thrown)
    }
}

#[lumen_bind::methods]
impl DomCssUnparsedValueIterator {
    #[proto(iter)]
    fn iter(&self, this: This<Value>) -> Value {
        this.0
    }

    #[proto(next)]
    fn next(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Option<Value>> {
        let owner = ctx
            .reflect_get(&this.0, &self.owner_key, &this.0)
            .map_err(OpError::thrown)?;
        let length =
            ctx.with_instance::<DomCssUnparsedValue, _>(&owner, |unparsed| unparsed.length.get())?;
        let index = self.index.get();
        if index >= length {
            return Ok(None);
        }
        self.index.set(index + 1);
        let value = ctx
            .reflect_get(&owner, &Value::str(index.to_string()), &owner)
            .map_err(OpError::thrown)?;
        let item = match self.kind {
            UnparsedIteratorKind::Keys => Value::Num(index as f64),
            UnparsedIteratorKind::Values => value,
            UnparsedIteratorKind::Entries => {
                JsHost::from_list(ctx, vec![Value::Num(index as f64), value])
            }
        };
        Ok(Some(item))
    }
}

fn unparsed_iterator(ctx: &mut Ctx, owner: Value, kind: UnparsedIteratorKind) -> OpResult<Value> {
    let owner_key = ctx.new_symbol(Some("CSSUnparsedValueIterator.owner".into()));
    let iterator = ctx.new_instance(DomCssUnparsedValueIterator {
        owner_key: owner_key.clone(),
        kind,
        index: Cell::new(0),
    });
    define_data_property(ctx, &iterator, owner_key, owner, false, false, false)
        .map_err(OpError::thrown)?;
    Ok(iterator)
}

fn inline_declaration(realm: &DomRealm, node: NodeId) -> OpResult<css::DeclarationBlock> {
    let session = realm.session.borrow();
    match session.document().kind(node).map_err(dom_error)? {
        NodeKind::Element { .. } => {
            if let Some(block) = session.document().inline_cssom_style(node) {
                return Ok(block.clone());
            }
            css::DeclarationBlock::parse(session.document()
                .get_attribute_ns_ref(node, None, "style").map_err(dom_error)?
                .unwrap_or("")).map_err(css_error)
        }
        _ => Err(OpError::new(
            "TypeError",
            "attributeStyleMap requires an Element",
        )),
    }
}

fn write_inline_property(
    realm: &DomRealm,
    node: NodeId,
    property: &str,
    value: &str,
) -> OpResult<()> {
    let mut declaration = inline_declaration(realm, node)?;
    if !declaration.set(property, value, false).map_err(css_error)? { return Ok(()); }
    realm
        .session
        .borrow_mut()
        .document_mut()
        .set_inline_cssom_style(node, declaration)
        .map_err(dom_error)
}

fn inline_property_value(realm: &DomRealm, node: NodeId, property: &str) -> OpResult<Option<(String, bool)>> {
    let session = realm.session.borrow();
    if let Some(block) = session.document().inline_cssom_style(node) {
        return Ok(block.value(property));
    }
    css::declaration_value(session.document().get_attribute_ns_ref(node, None, "style")
        .map_err(dom_error)?.unwrap_or(""), property).map_err(css_error)
}

pub(crate) fn style_map_property(property: &str) -> OpResult<Cow<'_, str>> {
    let property=if !property.starts_with("--") && property.bytes().any(|byte|byte.is_ascii_uppercase()) {
        Cow::Owned(property.to_ascii_lowercase())
    } else {Cow::Borrowed(property)};
    if !css::is_cssom_property_name(&property) {
        return Err(OpError::type_error("unrecognized CSS property"));
    }
    Ok(property)
}

enum StyleMapSnapshot {
    Computed(Option<crate::style::ComputedPropertyMapSnapshot>),
    Inline(css::DeclarationBlock),
}
impl StyleMapSnapshot {
    fn capture(ctx: &mut Ctx, map: &DomStylePropertyMapReadOnly) -> OpResult<Self> {
        let (realm,node)=map.realm.resolve_adopted_node(map.node);
        if map.computed {
            crate::animations::flush_css_transitions_for(ctx,&realm,Some(node))?;
            Ok(Self::Computed(crate::style::computed_property_map_snapshot(&realm,node)?))
        } else {Ok(Self::Inline(inline_declaration(&realm,node)?))}
    }
    fn names(&self) -> Vec<&str> {
        match self {
            Self::Inline(block)=>block.names().collect(),
            Self::Computed(None)=>Vec::new(),
            Self::Computed(Some(snapshot))=>{
                let mut names=crate::style::computed_property_names().to_vec();
                names.extend(snapshot.style.custom_properties().iter().filter(|(_,value)|value.is_some()).map(|(name,_)|name.as_str()));
                if let Some(registrations)=snapshot.registrations.as_deref() {
                    names.extend(registrations.iter().map(|entry|entry.name.as_str()));
                }
                names.sort_unstable_by(|left,right|css::cssom_property_order(left,right));
                names.dedup();
                names
            }
        }
    }
    fn values(&self,ctx:&mut Ctx,property:&str)->OpResult<Vec<Value>> {
        match self {
            Self::Inline(block)=>match block.value(property) {
                Some((text,_))=>style_values_from_text(ctx,property,&text,true),
                None=>Ok(Vec::new()),
            },
            Self::Computed(None)=>Ok(Vec::new()),
            Self::Computed(Some(snapshot))=>{
                let syntax=snapshot.registrations.as_deref().and_then(|entries|
                    entries.iter().find(|entry|entry.name==property)).map(|entry|entry.syntax.as_str());
                let text=snapshot.style.computed_css_value(property,snapshot.context);
                match text {
                    Some(text)=>registered_style_values(ctx,property,&text,syntax,true),
                    None if syntax.is_some()=>registered_style_values(ctx,property,"",syntax,true),
                    None=>Err(OpError::new("InvalidStateError","supported property has no computed value")),
                }
            }
        }
    }
    fn iterator(&self,ctx:&mut Ctx,kind:u8)->OpResult<Value> {
        let names=self.names();
        let mut values=Vec::with_capacity(names.len());
        for name in names {
            let value=if kind==0 {Value::str(name)} else {
                let parts=self.values(ctx,&name)?;
                let parts=JsHost::from_list(ctx,parts);
                if kind==1 {parts} else {JsHost::from_list(ctx,vec![Value::str(name),parts])}
            };
            values.push(value);
        }
        let collection=JsHost::from_list(ctx,values);
        Ok(ctx.new_instance(DomCssomCollectionIterator::new(collection)))
    }
}

fn property_map_text(
    ctx: &mut Ctx,
    map: &DomStylePropertyMapReadOnly,
    property: &str,
) -> OpResult<Option<String>> {
    if map.computed {
        let (realm,node)=map.realm.resolve_adopted_node(map.node);
        crate::animations::flush_css_transitions_for_property(ctx,&realm,Some(node),Some(property))?;
        if !crate::style::rendered_ancestry(&realm,node,false)? {return Ok(None);}
        let value = crate::style::computed_property_value(&realm, node, property)?;
        if !value.is_empty() {return Ok(Some(value));}
        if property.starts_with("--") {
            let fonts=crate::canvas::initialized_realm_font_source(&realm)?;
            let mut session=realm.session.borrow_mut();
            let style=session.computed_style_with_text(node,fonts.as_ref().map(|fonts|fonts as &dyn lumen_html::paint::TextShaper))
                .map_err(|error|OpError::new("InvalidStateError",format!("computed style failed: {error:?}")))?;
            if style.custom_properties().iter().any(|(name,value)|name==property && value.is_some())
                || session.registered_custom_property_snapshot().as_deref().is_some_and(|entries|entries.iter().any(|entry|entry.name==property)) {
                return Ok(Some(value));
            }
        }
        Ok(None)
    } else {
        let (realm,node)=map.realm.resolve_adopted_node(map.node);
        Ok(inline_property_value(&realm, node, property)?.map(|(value, _)| value))
    }
}

pub(crate) fn registered_style_values(ctx: &mut Ctx, property: &str, text: &str,
    syntax: Option<&str>, multiple: bool) -> OpResult<Vec<Value>> {
    if let Some(parts) = syntax.and_then(|syntax|
        css::registered_properties::computed_value_items(syntax, text)) {
        let mut values = Vec::new();
        for part in parts.into_iter().take(if multiple { usize::MAX } else { 1 }) {
            values.push(registered_style_value(ctx, property, part, syntax)?);
        }
        return Ok(values);
    }
    style_values_from_text(ctx, property, text, multiple)
}

fn computed_map_values(ctx: &mut Ctx, map: &DomStylePropertyMapReadOnly,
    property: &str, text: &str, multiple: bool) -> OpResult<Vec<Value>> {
    if property.starts_with("--") {
        let (realm, _) = map.realm.resolve_adopted_node(map.node);
        let registrations = realm.session.borrow().registered_custom_property_snapshot();
        let syntax = registrations.as_deref().and_then(|entries|
            entries.iter().find(|entry| entry.name == property)).map(|entry| entry.syntax.as_str());
        return registered_style_values(ctx, property, text, syntax, multiple);
    }
    style_values_from_text(ctx, property, text, multiple)
}

fn value_array(ctx: &mut Ctx, values: impl IntoIterator<Item = Value>) -> OpResult<Value> {
    Ok(JsHost::from_list(ctx, values.into_iter().collect()))
}

#[lumen_bind::methods]
impl DomStylePropertyMapReadOnly {
    #[method(coerce)]
    fn get(&self, ctx: &mut Ctx, property: &str) -> OpResult<Value> {
        let property=style_map_property(property)?;
        let value=property_map_text(ctx,self,&property)?;
        let values=match value {
            Some(text) if self.computed=>computed_map_values(ctx,self,&property,&text,false)?,
            Some(text)=>style_values_from_text(ctx,&property,&text,false)?,
            None=>Vec::new(),
        };
        Ok(values.into_iter().next().unwrap_or(Value::Undefined))
    }
    #[method(coerce)]
    fn get_all(&self, ctx: &mut Ctx, property: &str) -> OpResult<Value> {
        let property=style_map_property(property)?;
        let value=property_map_text(ctx,self,&property)?;
        let values=match value {
            Some(text) if self.computed=>computed_map_values(ctx,self,&property,&text,true)?,
            Some(text)=>style_values_from_text(ctx,&property,&text,true)?,
            None=>Vec::new(),
        };
        value_array(ctx,values)
    }
    #[method(coerce)]
    fn has(&self, ctx: &mut Ctx, property: &str) -> OpResult<bool> {
        let property=style_map_property(property)?;
        Ok(property_map_text(ctx,self,&property)?.is_some())
    }
    #[getter]
    fn size(&self,ctx:&mut Ctx)->OpResult<usize> {
        Ok(StyleMapSnapshot::capture(ctx,self)?.names().len())
    }
    #[proto(iter)]
    fn iter(&self,ctx:&mut Ctx)->OpResult<Value> {
        StyleMapSnapshot::capture(ctx,self)?.iterator(ctx,2)
    }
    fn entries(&self,ctx:&mut Ctx)->OpResult<Value> {
        StyleMapSnapshot::capture(ctx,self)?.iterator(ctx,2)
    }
    fn keys(&self,ctx:&mut Ctx)->OpResult<Value> {
        StyleMapSnapshot::capture(ctx,self)?.iterator(ctx,0)
    }
    fn values(&self,ctx:&mut Ctx)->OpResult<Value> {
        StyleMapSnapshot::capture(ctx,self)?.iterator(ctx,1)
    }
    fn for_each(&self,ctx:&mut Ctx,this:This<Value>,callback:JsFunction,this_arg:Option<Value>)->OpResult<()> {
        let snapshot=StyleMapSnapshot::capture(ctx,self)?;
        for name in snapshot.names() {
            let values=snapshot.values(ctx,&name)?;
            let values=JsHost::from_list(ctx,values);
            callback.call(ctx,this_arg.clone().unwrap_or(Value::Undefined),
                &[values,Value::str(name),this.0.clone()])?;
        }
        Ok(())
    }
}

#[lumen_bind::methods]
impl DomStylePropertyMap {
    #[method(coerce)]
    fn set(&self, ctx: &mut Ctx, property: &str, value: Value) -> OpResult<()> {
let value = if transforms::is_value(ctx,&value) {
            if !property.eq_ignore_ascii_case("transform"){return Err(OpError::type_error("CSSTransformValue is only valid for transform"));}
            transforms::serialize(ctx,&value)?
        } else if ctx
            .with_instance::<DomCssUnparsedValue, _>(&value, |_| ())
            .is_ok()
        {
            serialize_unparsed_value(ctx, &value)?
        } else if ctx
            .with_instance::<DomCssNumericValue, _>(&value, |_| ())
            .is_ok()
        {
            numeric_value_text(ctx, &value)?
        } else if let Ok(value) = ctx.with_instance::<DomCssStyleValue, _>(&value, |style| {
            style.serialized_value.borrow().clone()
        }) {
            value
        } else {
            ctx.coerce_string(&value)
                .map_err(OpError::thrown)?
                .to_string()
        };
        if !css::supports_declaration(property, &value) {
            return Err(OpError::new(
                "TypeError",
                "CSSStyleValue is invalid for the requested property",
            ));
        }
        write_inline_property(&self.base.realm, self.base.node, property, &value)
    }

    #[method(coerce)]
    fn delete(&self, property: &str) -> OpResult<()> {
        write_inline_property(&self.base.realm, self.base.node, property, "")
    }
}

pub(crate) fn attribute_style_map(
    ctx: &mut Ctx,
    realm: Rc<DomRealm>,
    node: NodeId,
    owner: Value,
) -> Value {
    ctx.new_instance(DomStylePropertyMap {
        base: DomStylePropertyMapReadOnly {
            realm,
            node,
            computed: false,
            _owner: owner,
        },
    })
}

pub(crate) fn computed_style_map(
    ctx: &mut Ctx,
    realm: Rc<DomRealm>,
    node: NodeId,
    owner: Value,
) -> Value {
    ctx.new_instance(DomStylePropertyMapReadOnly {
        realm,
        node,
        computed: true,
        _owner: owner,
    })
}

/// `CSS.supports` overloads, evaluated by the shared parser used by stylesheets
/// and inline style. `Passed` distinguishes the conditionText overload from a
/// two-argument call whose second argument is explicitly null or undefined.
#[lumen_bind::op(name = "supports", coerce)]
pub fn supports(condition_or_property: &str, value: Passed<String>) -> bool {
    match value.0 {
        Some(value) => css::supports_property_value(condition_or_property, &value),
        None => css::supports_condition(condition_or_property),
    }
}

#[lumen_bind::op(name = "escape", coerce)]
pub fn escape(identifier: &str) -> String {
    css::serialize_identifier(identifier)
}

struct PropertyDefinition {
    name: String,
    syntax: String,
    inherits: bool,
    initial_value: Option<String>,
}

impl<'a> FromArg<'a, JsHost> for PropertyDefinition {
    fn from_arg(cx: &'a lumen::embed::ArgCx<'_>, value: &'a Value, at: lumen_bind::Slot) -> Result<Self, Value> {
        if !matches!(value, Value::Obj(_) | Value::Null | Value::Undefined) {
            return Err(JsHost::with_ctx(cx, |ctx| ctx.make_error("TypeError", "PropertyDefinition must be a dictionary")));
        }
        let member = |name| JsHost::with_ctx(cx, |ctx| {
            if matches!(value, Value::Null | Value::Undefined) { Ok(Value::Undefined) }
            else { ctx.member_get(value, name) }
        });
        // WebIDL dictionaries convert members in lexicographic order. All
        // conversions use the host's existing typed FromArg implementations.
        let inherits = member("inherits")?;
        let inherits = if matches!(inherits, Value::Undefined) { true }
            else { <bool as FromArg<'_, JsHost>>::from_arg(cx, &inherits, at)? };
        let initial = member("initialValue")?;
        let initial_value = if matches!(initial, Value::Undefined) { None }
            else { Some(<String as FromArg<'_, JsHost>>::from_arg(cx, &initial, at)?) };
        let name = member("name")?;
        if matches!(name, Value::Undefined) { return Err(JsHost::with_ctx(cx, |ctx| ctx.make_error("TypeError", "PropertyDefinition.name is required"))); }
        let name = <String as FromArg<'_, JsHost>>::from_arg(cx, &name, at)?;
        let syntax = member("syntax")?;
        let syntax = if matches!(syntax, Value::Undefined) { "*".into() }
            else { <String as FromArg<'_, JsHost>>::from_arg(cx, &syntax, at)? };
        Ok(Self { name, syntax, inherits, initial_value })
    }
}

#[lumen_bind::op(name = "registerProperty", coerce)]
fn register_property(ctx: &mut Ctx, definition: PropertyDefinition) -> OpResult<()> {
    let realm = crate::window_globals::current_dom_realm(ctx)
        .ok_or_else(|| OpError::new("InvalidStateError", "CSS.registerProperty has no associated document"))?;
    let result = realm.session.borrow_mut().register_custom_property(definition.name, &definition.syntax,
        definition.inherits, definition.initial_value);
    result.map_err(|error| {
        use css::registered_properties::RegistrationError::*;
        match error {
            Duplicate => crate::error_reporting::dom_exception(ctx, "InvalidModificationError", "custom property is already registered"),
            UnsupportedSyntax => crate::error_reporting::dom_exception(ctx, "NotSupportedError", "typed registered custom-property syntaxes are not implemented"),
            Capacity => crate::error_reporting::dom_exception(ctx, "QuotaExceededError", "registered custom-property storage exhausted"),
            InvalidName | InvalidSyntax | InvalidInitialValue => crate::error_reporting::dom_exception(ctx, "SyntaxError", "invalid custom-property registration descriptor"),
        }
    })
}

/// Shared specified declarations with a lexical mode for font-face descriptors.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CssDeclaration {
    text: String,
    block: Option<Rc<css::DeclarationBlock>>,
}

impl CssDeclaration {
    pub fn parse(text: &str) -> Self {
        let block = css::DeclarationBlock::parse(text).unwrap_or_default();
        Self {
            text: block.serialize().unwrap_or_default(),
            block: Some(Rc::new(block)),
        }
    }

    fn descriptors(text: &str) -> Self {
        Self { text: css::cssom_font_face_declaration_text(text), block: None }
    }

    pub fn css_text(&self) -> &str {
        &self.text
    }

    pub fn get_property_value(&self, name: &str) -> Result<Option<(String, bool)>, css::CssError> {
        Ok(
            match &self.block {
                Some(block) => block.value(name),
                None => css::descriptor_declaration_value(&self.text, name)?,
            }.map(|(value, important)| {
                (
                    super::style::canonical_content_property(name, value),
                    important,
                )
            }),
        )
    }

    pub fn set_property(
        &mut self,
        name: &str,
        value: &str,
        important: bool,
    ) -> Result<(), css::CssError> {
        if let Some(block) = &mut self.block {
            if Rc::make_mut(block).set(name, value, important)? { self.text = block.serialize()?; }
        } else {
            self.text = css::set_descriptor_declaration(&self.text, name, value, important)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CssRuleText {
    pub css_text: String,
    pub selector_text: Option<String>,
    pub style: CssDeclaration,
    pub nested: Vec<CssRuleText>,
    pub nested_declarations: bool,
    pub font_face: bool,
    pub keyframes: Option<css::KeyframesRuleText>,
    header: RuleHeader,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuleHeader { Import, Namespace, LayerStatement, Other }

/// Mutation phases have distinct DOM exceptions; parser diagnostics remain
/// diagnostics rather than an error-name protocol.
#[derive(Clone, Debug)]
pub enum CssRuleMutationError {
    Syntax(css::CssError),
    IndexSize,
    Hierarchy,
    InvalidState,
}
impl From<css::CssError> for CssRuleMutationError {
    fn from(error: css::CssError) -> Self { Self::Syntax(error) }
}
fn rule_mutation_exception(ctx: &mut Ctx, error: CssRuleMutationError) -> OpError {
    let (name,message)=match error {
        CssRuleMutationError::Syntax(error)=>return css_rule_dom_exception(ctx,error),
        CssRuleMutationError::IndexSize=>("IndexSizeError","CSS rule index out of range"),
        CssRuleMutationError::Hierarchy=>("HierarchyRequestError","CSS rule violates rule-list ordering"),
        CssRuleMutationError::InvalidState=>("InvalidStateError","namespace rules cannot change while other rule types exist"),
    };
    crate::error_reporting::dom_exception(ctx,name,message)
}
fn parse_inserted_rule(text: &str, namespace_source: &str) -> Result<CssRuleText, css::CssError> {
    let namespaces=css::namespaces::NamespaceMap::from_stylesheet(namespace_source)?;
    let source=css::nesting::parse_one_source_rule_in_namespace_context(text,&[],&namespaces)?;
    let mut rules=source_rules_to_cssom_with_namespaces(text,core::slice::from_ref(&source),false,&namespaces)?;
    if rules.len()!=1 {return Err(css::CssError{offset:0,message:"insertRule requires exactly one rule"});}
    Ok(rules.remove(0))
}
fn source_rule_header(text: &str, source: &css::nesting::SourceRule) -> RuleHeader {
    if source.kind!=css::nesting::SourceRuleKind::Statement {return RuleHeader::Other;}
    let prelude=text[source.prelude.clone()].trim();
    if css::nesting::at_rule(prelude,"@import") {RuleHeader::Import}
    else if css::namespaces::parse_rule(&text[source.range.clone()]).is_ok() {RuleHeader::Namespace}
    else if css::nesting::at_rule(prelude,"@layer") {RuleHeader::LayerStatement}
    else {RuleHeader::Other}
}
fn retain_valid_stylesheet_headers(text: &str, source: &mut Vec<css::nesting::SourceRule>) {
    let mut phase=0u8;
    source.retain(|rule| match source_rule_header(text,rule) {
        RuleHeader::Import=>{if phase>1 {false}else{phase=1;true}},
        RuleHeader::Namespace=>{if phase==3 {false}else{phase=2;true}},
        RuleHeader::LayerStatement if phase==0=>true,
        _=>{phase=3;true},
    });
}
fn check_rule_order(rules: &[CssRuleText], index: usize, inserted: &CssRuleText) -> Result<(),CssRuleMutationError> {
    // Cascade5 orders header sections: layer statements, then imports,
    // then namespaces. A layer statement after imports closes the header.
    let mut phase=0u8;
    for rule in rules[..index].iter().chain(core::iter::once(inserted)).chain(rules[index..].iter()) {
        match rule.header {
            RuleHeader::Import if phase>1=>return Err(CssRuleMutationError::Hierarchy),
            RuleHeader::Import=>phase=1,
            RuleHeader::Namespace if phase==3=>return Err(CssRuleMutationError::Hierarchy),
            RuleHeader::Namespace=>phase=2,
            RuleHeader::LayerStatement if phase==0=>{},
            _=>phase=3,
        }
    }
    if inserted.header==RuleHeader::Namespace && rules.iter().any(|rule|!matches!(rule.header,RuleHeader::Import|RuleHeader::Namespace)) {
        return Err(CssRuleMutationError::InvalidState);
    }
    Ok(())
}

enum OwnedNestingContext {
    StartingStyle,
    Style(String),
    Scope(String),
}

impl OwnedNestingContext {
    fn borrowed(&self) -> css::nesting::NestingContext<'_> {
        match self {
            Self::StartingStyle => css::nesting::NestingContext::StartingStyle,
            Self::Style(selector) => css::nesting::NestingContext::Style(selector),
            Self::Scope(prelude) => css::nesting::NestingContext::Scope(prelude),
        }
    }
}

fn source_rules_in_cssom_context(text:&str,nested:bool,scoped:bool,boundaries:&[css::nesting::DeclarationBoundary],namespaces:&css::namespaces::NamespaceMap)->Result<Vec<css::nesting::SourceRule>,css::CssError> {
    let context=if scoped {Some(css::nesting::NestingContext::Scope("@scope"))}else if nested{Some(css::nesting::NestingContext::Style("*"))}else{None};
    css::nesting::parse_source_rules_with_boundaries_in_namespace_context(text,context.as_slice(),false,boundaries,namespaces)
}

/// Mutable stylesheet source associated with one `<style>` node. Every edit is
/// validated first, then callers commit the resulting text to the document;
/// `RenderSession` reparses that same node for cascade evaluation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CssStyleSheetText {
    text: String,
    retained: Vec<(Arc<[usize]>, Rc<css::DeclarationBlock>)>,
    boundaries: Arc<[css::nesting::DeclarationBoundary]>,
    nested_context: bool,
    scoped_context: bool,
    singleton_declarations: bool,
    namespaces: Option<Arc<css::namespaces::NamespaceMap>>,
    import_occurrences: Option<Vec<Option<usize>>>,
}

impl CssStyleSheetText {
    fn set_rules(&mut self, rules: &[CssRuleText]) -> Result<(), css::CssError> {
        let mut text = String::new();
        let mut boundaries = Vec::new();
        for rule in rules {
            if !text.is_empty() { text.push('\n'); }
            serialize_rule_into(rule, &mut text, None, &mut boundaries);
            if text.len() > 1024 * 1024 || boundaries.len() > css::nesting::MAX_DECLARATION_BOUNDARIES {
                return Err(css::CssError { offset: 0, message: "CSSOM source topology exceeds limit" });
            }
        }
        let namespaces=self.namespaces.clone().map(Ok).unwrap_or_else(||css::namespaces::NamespaceMap::from_stylesheet(&text).map(Arc::new))?;
        source_rules_in_cssom_context(&text, self.nested_context, self.scoped_context, &boundaries,&namespaces)?;
        self.text = text;
        self.boundaries = boundaries.into();
        Ok(())
    }
    fn rebase_retained(&mut self, prefix: &[usize], index: usize, insert: bool) {
        self.retained.retain_mut(|(path, _)| {
            if !path.starts_with(prefix) || path.len() <= prefix.len() { return true; }
            let position = path[prefix.len()];
            if !insert && position == index { return false; }
            if position >= index {
                let mut next = path.to_vec();
                next[prefix.len()] = if insert { position + 1 } else { position - 1 };
                *path = next.into();
            }
            true
        });
    }
    pub fn parse(text: &str) -> Result<Self, css::CssError> {
        css::parse(text)?;
        Ok(Self { text: text.into(), retained: Vec::new(), boundaries: Arc::from([]), nested_context: false, scoped_context: false, singleton_declarations: false, namespaces:None, import_occurrences: None })
    }

    pub fn css_text(&self) -> &str {
        &self.text
    }

    pub fn css_rules(&self) -> Result<Vec<CssRuleText>, css::CssError> {
        let mut rules = if self.singleton_declarations {
            let style = CssDeclaration::parse(&self.text);
            vec![CssRuleText { css_text: style.css_text().to_owned(), selector_text: None, style, nested: Vec::new(), nested_declarations: true, font_face: false, keyframes: None, header: RuleHeader::Other }]
        } else {
            let namespaces=self.namespaces.clone().map(Ok).unwrap_or_else(||css::namespaces::NamespaceMap::from_stylesheet(&self.text).map(Arc::new))?;
            let mut source = source_rules_in_cssom_context(&self.text, self.nested_context, self.scoped_context, &self.boundaries,&namespaces)?;
            if !self.nested_context && !self.scoped_context {retain_valid_stylesheet_headers(&self.text,&mut source);}
            source_rules_to_cssom_with_namespaces(&self.text, &source, self.nested_context && !self.scoped_context,&namespaces)?
        };
        for (path, block) in &self.retained {
            let mut current = &mut rules;
            for (depth, &index) in path.iter().enumerate() {
                let Some(rule) = current.get_mut(index) else { break; };
                if depth + 1 == path.len() {
                    if !rule.font_face && (rule.selector_text.is_some() || rule.nested_declarations) {
                        rule.style = CssDeclaration { text: block.serialize()?, block: Some(block.clone()) };
                        rule.css_text = serialize_rule(rule);
                    }
                    break;
                }
                current = &mut rule.nested;
            }
        }
        Ok(rules)
    }

    fn rule_list(&self, parent_path: &[usize]) -> Result<Vec<CssRuleText>, css::CssError> {
        if parent_path.is_empty() {
            return self.css_rules();
        }
        let mut rules = self.css_rules()?;
        for index in parent_path {
            let Some(parent) = rules.get(*index) else {
                return Err(css::CssError {
                    offset: *index,
                    message: "CSS rule index out of range",
                });
            };
            rules = parent.nested.clone();
        }
        Ok(rules)
    }

    fn rule_at_path(&self, path: &[usize]) -> Result<CssRuleText, css::CssError> {
        let Some((&index, parents)) = path.split_last() else {
            return Err(css::CssError {
                offset: 0,
                message: "CSS rule index out of range",
            });
        };
        let rules = self.rule_list(parents)?;
        rules.get(index).cloned().ok_or(css::CssError {
            offset: index,
            message: "CSS rule index out of range",
        })
    }

    fn set_rule_style(&mut self, path: &[usize], declarations: &str) -> Result<(), css::CssError> {
        self.mutate_rule(path, |rule| {
            if rule.selector_text.is_some() || rule.font_face || rule.nested_declarations {
                rule.style = if rule.font_face {
                    CssDeclaration::descriptors(declarations)
                } else {
                    CssDeclaration::parse(declarations)
                };
                rule.css_text = serialize_rule(rule);
            }
            Ok(())
        })?;
        self.retained.retain(|(known, _)| known.as_ref() != path);
        Ok(())
    }

    fn insert_nested_rule(
        &mut self,
        parent_path: &[usize],
        rule: &str,
        index: usize,
    ) -> Result<(), css::CssError> {
        let context = self.selector_context(parent_path)?;
        let context = context.iter().map(OwnedNestingContext::borrowed).collect::<Vec<_>>();
        let namespaces=if self.namespaces.is_some(){css::namespaces::NamespaceMap::default()}else{css::namespaces::NamespaceMap::from_stylesheet(&self.text)?};
        let inserted = parse_group_child(rule, &context, &namespaces)?;
        self.mutate_nested_list(parent_path, |children| {
            if index > children.len() {
                return Err(css::CssError {
                    offset: index,
                    message: "CSS rule index out of range",
                });
            }
            children.insert(index, inserted);
            Ok(())
        })?;
        self.rebase_retained(parent_path, index, true);
        Ok(())
    }

    fn delete_nested_rule(
        &mut self,
        parent_path: &[usize],
        index: usize,
    ) -> Result<String, css::CssError> {
        let mut removed = None;
        self.mutate_nested_list(parent_path, |children| {
            if index >= children.len() {
                return Err(css::CssError {
                    offset: index,
                    message: "CSS rule index out of range",
                });
            }
            removed = Some(children.remove(index).css_text);
            Ok(())
        })?;
        self.rebase_retained(parent_path, index, false);
        Ok(removed.unwrap_or_default())
    }

    fn set_rule_selector(&mut self, path: &[usize], selector: &str) -> Result<(), css::CssError> {
        let parent_path = path
            .split_last()
            .map(|(_, parents)| parents)
            .unwrap_or_default();
        let context = self.selector_context(parent_path)?;
        let context = context.iter().map(OwnedNestingContext::borrowed).collect::<Vec<_>>();
        let selector = selector.trim();
        if selector.is_empty() {
            return Ok(());
        }
        let namespaces=if self.namespaces.is_some(){css::namespaces::NamespaceMap::default()}else{css::namespaces::NamespaceMap::from_stylesheet(&self.text)?};
        let parsed = css::nesting::parse_one_source_rule_in_namespace_context(&format!("{selector} {{}}"), &context, &namespaces)?;
        if parsed.kind != css::nesting::SourceRuleKind::Style {
            return Err(css::CssError { offset: 0, message: "invalid selector list" });
        }
        let selector = implicit_nested_selector(selector, matches!(context.iter().rev().find(|context| !matches!(context, css::nesting::NestingContext::StartingStyle)), Some(css::nesting::NestingContext::Style(_))));
        let selector=css::selector_serialization::serialize(&selector,&namespaces,!context.is_empty())?;
        self.mutate_rule(path, |rule| {
            if rule.selector_text.is_none() {
                return Ok(());
            }
            rule.selector_text = Some(selector.clone());
            rule.css_text = serialize_rule(rule);
            Ok(())
        })
    }

    fn selector_context(&self, path: &[usize]) -> Result<Vec<OwnedNestingContext>, css::CssError> {
        let mut rules = self.css_rules()?;
        let mut context = Vec::new();
        if self.scoped_context { context.push(OwnedNestingContext::Scope("@scope".into())); }
        else if self.nested_context { context.push(OwnedNestingContext::Style("*".into())); }
        for index in path {
            let Some(parent) = rules.get(*index) else {
                return Err(css::CssError {
                    offset: *index,
                    message: "CSS rule index out of range",
                });
            };
            if let Some(selector) = parent.selector_text.as_ref() {
                context.push(OwnedNestingContext::Style(selector.clone()));
            } else if starts_css_at_rule(&parent.css_text, "@starting-style") {
                context.push(OwnedNestingContext::StartingStyle);
            } else if starts_css_at_rule(&parent.css_text, "@scope") {
                let open = css::nesting::source_rule_block_open(&parent.css_text)
                    .ok_or(css::CssError { offset: 0, message: "scope rule has no block" })?;
                context.push(OwnedNestingContext::Scope(parent.css_text[..open].trim().into()));
            }
            rules = parent.nested.clone();
        }
        Ok(context)
    }

    fn mutate_nested_list(
        &mut self,
        parent_path: &[usize],
        edit: impl FnOnce(&mut Vec<CssRuleText>) -> Result<(), css::CssError>,
    ) -> Result<(), css::CssError> {
        self.mutate_rule(parent_path, |parent| {
            edit(&mut parent.nested)?;
            parent.css_text = serialize_rule(parent);
            Ok(())
        })
    }

    fn mutate_rule(
        &mut self,
        path: &[usize],
        edit: impl FnOnce(&mut CssRuleText) -> Result<(), css::CssError>,
    ) -> Result<(), css::CssError> {
        let Some((&top_index, _)) = path.split_first() else {
            return Err(css::CssError {
                offset: 0,
                message: "CSS rule index out of range",
            });
        };
        let mut rules = self.css_rules()?;
        fn descend(
            rules: &mut [CssRuleText],
            path: &[usize],
            edit: impl FnOnce(&mut CssRuleText) -> Result<(), css::CssError>,
        ) -> Result<(), css::CssError> {
            let Some((&index, rest)) = path.split_first() else {
                return Err(css::CssError {
                    offset: 0,
                    message: "CSS rule index out of range",
                });
            };
            let Some(rule) = rules.get_mut(index) else {
                return Err(css::CssError {
                    offset: index,
                    message: "CSS rule index out of range",
                });
            };
            if rest.is_empty() {
                edit(rule)
            } else {
                descend(&mut rule.nested, rest, edit)?;
                rule.css_text = serialize_rule(rule);
                Ok(())
            }
        }
        descend(&mut rules, path, edit)?;
        let Some(_) = rules.get(top_index) else {
            return Err(css::CssError {
                offset: top_index,
                message: "CSS rule index out of range",
            });
        };
        self.set_rules(&rules)
    }

    #[cfg(test)]
    pub fn insert_rule(&mut self, rule: &str, index: usize) -> Result<(), CssRuleMutationError> {
        let rule=parse_inserted_rule(rule,&self.text)?;
        self.insert_parsed_rule(rule,index)
    }
    fn insert_parsed_rule(&mut self, rule: CssRuleText, index: usize) -> Result<(), CssRuleMutationError> {
        let mut current=self.css_rules()?;
        if index>current.len() {return Err(CssRuleMutationError::IndexSize);}
        check_rule_order(&current,index,&rule)?;
        let import_map=self.edited_import_occurrences(&current,index,true,rule.header==RuleHeader::Import)?;
        current.insert(index,rule);
        self.set_rules(&current)?;
        self.import_occurrences=import_map;
        self.rebase_retained(&[],index,true);
        Ok(())
    }
    pub fn delete_rule(&mut self, index: usize) -> Result<(), CssRuleMutationError> {
        let mut current=self.css_rules()?;
        let rule=current.get(index).ok_or(CssRuleMutationError::IndexSize)?;
        if rule.header==RuleHeader::Namespace && current.iter().any(|rule|!matches!(rule.header,RuleHeader::Import|RuleHeader::Namespace)) {
            return Err(CssRuleMutationError::InvalidState);
        }
        let import_map=self.edited_import_occurrences(&current,index,false,rule.header==RuleHeader::Import)?;
        current.remove(index);
        self.set_rules(&current)?;
        self.import_occurrences=import_map;
        self.rebase_retained(&[],index,false);
        Ok(())
    }

    fn edited_import_occurrences(&self, rules: &[CssRuleText], index: usize, insert: bool, is_import: bool) -> Result<Option<Vec<Option<usize>>>, css::CssError> {
        if !is_import { return Ok(self.import_occurrences.clone()); }
        let count = rules.iter().filter(|rule| starts_css_at_rule(&rule.css_text, "@import")).count();
        let ordinal = rules[..index].iter().filter(|rule| starts_css_at_rule(&rule.css_text, "@import")).count();
        if count + usize::from(insert) > css::MAX_CSS_GRAPH_IMPORTS { return Err(css::CssError { offset: index, message: "too many CSS imports" }); }
        let mut mapping = Vec::new();
        mapping.try_reserve_exact(count + usize::from(insert)).map_err(|_| css::CssError { offset: index, message: "CSS import occurrence allocation failed" })?;
        match &self.import_occurrences { Some(existing) => mapping.extend_from_slice(existing), None => mapping.extend((0..count).map(Some)) }
        if insert { mapping.insert(ordinal, None); } else { mapping.remove(ordinal); }
        Ok(Some(mapping))
    }

    #[cfg(test)]
    pub fn replace(&mut self, text: &str) -> Result<(), css::CssError> {
        let parsed = Self::parse(text)?;
        *self = parsed;
        Ok(())
    }

    fn replace_constructed(&mut self, text: &str) -> Result<(), css::CssError> {
        // CSSOM parses the rule list first, then removes its import rules.
        // Removing authored spans before parsing can promote an invalid late
        // import across a surviving layer statement into the valid header.
        let parsed=Self::parse(text)?;
        let mut rules=parsed.css_rules()?;
        rules.retain(|rule|rule.header!=RuleHeader::Import);
        let mut replacement=Self::default();
        replacement.set_rules(&rules)?;
        *self=replacement;
        Ok(())
    }

    fn replace_rule_text(&mut self, index: usize, replacement: &str) -> Result<(), css::CssError> {
        let mut rules = self.css_rules()?;
        let Some(target) = rules.get_mut(index) else {
            return Err(css::CssError { offset: index, message: "CSS rule index out of range" });
        };
        let parsed = css::nesting::parse_one_source_rule(replacement, &[])?;
        let mut replacement = source_rules_to_cssom(replacement, &[parsed], false)?;
        *target = replacement.remove(0);
        self.set_rules(&rules)
    }

    fn keyframes_rule(&self, index: usize) -> Result<css::KeyframesRuleText, css::CssError> {
        let rules = self.css_rules()?;
        let Some(rule) = rules.get(index) else {
            return Err(css::CssError {
                offset: index,
                message: "CSS rule index out of range",
            });
        };
        rule.keyframes.clone().ok_or(css::CssError {
            offset: index,
            message: "rule is not a keyframes rule",
        })
    }

    fn replace_keyframes(
        &mut self,
        index: usize,
        keyframes: &css::KeyframesRuleText,
    ) -> Result<(), css::CssError> {
        let replacement = css::serialize_keyframes_rule(&keyframes.name_text, &keyframes.rules);
        let parsed = css::parse_keyframes_rule(&replacement)?;
        if parsed.is_none() {
            return Err(css::CssError {
                offset: index,
                message: "invalid keyframes rule",
            });
        }
        self.replace_rule_text(index, &replacement)
    }

    fn append_keyframe(&mut self, index: usize, rule: &str) -> Result<(), css::CssError> {
        let frame = css::parse_keyframe_rule(rule)?;
        let mut keyframes = self.keyframes_rule(index)?;
        let position = keyframes.rules.len();
        keyframes.rules.push(frame);
        self.replace_keyframes(index, &keyframes)?;
        self.rebase_retained(&[index], position, true);
        Ok(())
    }

    fn delete_keyframe(&mut self, index: usize, key_text: &str) -> Result<(), css::CssError> {
        let selector = css::parse_keyframe_rule(&format!("{key_text} {{}}"))?.key_text;
        let mut keyframes = self.keyframes_rule(index)?;
        if let Some(position) = keyframes
            .rules
            .iter()
            .position(|rule| rule.key_text.eq_ignore_ascii_case(&selector))
        {
            keyframes.rules.remove(position);
            self.replace_keyframes(index, &keyframes)?;
            self.rebase_retained(&[index], position, false);
        }
        Ok(())
    }

    fn set_keyframe_key_text(
        &mut self,
        index: usize,
        frame_index: usize,
        key_text: &str,
    ) -> Result<(), css::CssError> {
        let frame = css::parse_keyframe_rule(&format!("{key_text} {{}}"))?;
        let mut keyframes = self.keyframes_rule(index)?;
        let Some(target) = keyframes.rules.get_mut(frame_index) else {
            return Err(css::CssError {
                offset: frame_index,
                message: "keyframe rule index out of range",
            });
        };
        target.key_text = frame.key_text.clone();
        target.css_text = format!("{} {{ {} }}", frame.key_text, target.style);
        self.replace_keyframes(index, &keyframes)
    }

    fn set_keyframe_declarations(
        &mut self,
        index: usize,
        frame_index: usize,
        declarations: &str,
    ) -> Result<(), css::CssError> {
        let mut keyframes = self.keyframes_rule(index)?;
        let Some(target) = keyframes.rules.get_mut(frame_index) else {
            return Err(css::CssError {
                offset: frame_index,
                message: "keyframe rule index out of range",
            });
        };
        target.style = css::parse_keyframe_rule(&format!("{} {{ {} }}",target.key_text,declarations))?.style;
        target.css_text = format!("{} {{ {} }}", target.key_text, target.style);
        self.replace_keyframes(index, &keyframes)?;
        self.retained.retain(|(path, _)| path.as_ref() != [index, frame_index].as_slice());
        Ok(())
    }

    fn set_import_media(&mut self, index: usize, media: &str) -> Result<(), css::CssError> {
        let rules = self.css_rules()?;
        let Some(rule) = rules.get(index) else {
            return Err(css::CssError {
                offset: index,
                message: "CSS rule index out of range",
            });
        };
        let Some(mut import) = css::imports(&rule.css_text)?.into_iter().next() else {
            return Err(css::CssError {
                offset: index,
                message: "rule is not an import rule",
            });
        };
        let media = css::media_query_list_items(media);
        import.media = (!media.is_empty()).then(|| Arc::from(media.join(", ")));
        self.replace_rule_text(index, &css::serialize_import_rule(&import))
    }
}

fn source_rules_to_cssom(text: &str, source: &[css::nesting::SourceRule], has_parent: bool) -> Result<Vec<CssRuleText>, css::CssError> {
    let namespaces=css::namespaces::NamespaceMap::from_stylesheet(text)?;
    source_rules_to_cssom_with_namespaces(text,source,has_parent,&namespaces)
}
fn source_rules_to_cssom_with_namespaces(text:&str,source:&[css::nesting::SourceRule],has_parent:bool,namespaces:&css::namespaces::NamespaceMap)->Result<Vec<CssRuleText>,css::CssError> {

    use css::nesting::SourceRuleKind;
    let mut result = Vec::with_capacity(source.len());
    for node in source {
        let raw = text[node.range.clone()].trim();
        let raw_prelude = text[node.prelude.clone()].trim();
        let completed_prelude=css::complete_css_component_value(raw_prelude)?;
        let prelude=completed_prelude.as_deref().unwrap_or(raw_prelude);
        let nested_declarations = node.kind == SourceRuleKind::NestedDeclarations;
        let font_face = node.kind == SourceRuleKind::FontFace;
        let keyframes = if node.kind == SourceRuleKind::Keyframes { css::parse_keyframes_rule(raw)? } else { None };
        let frame = if node.kind == SourceRuleKind::Keyframe { Some(css::parse_keyframe_rule(raw)?) } else { None };
        let style = if font_face { CssDeclaration::descriptors(&node.declaration_text(text)) }
            else if let Some(frame) = &frame { CssDeclaration::parse(&frame.style) }
            else if matches!(node.kind, SourceRuleKind::Style | SourceRuleKind::NestedDeclarations) { CssDeclaration::parse(&node.declaration_text(text)) }
            else { CssDeclaration::default() };
        let selector_text = match node.kind {
            SourceRuleKind::Style => Some(css::selector_serialization::serialize(&implicit_nested_selector(prelude,has_parent),namespaces,has_parent)?),
            SourceRuleKind::Keyframe => frame.map(|frame| frame.key_text),
            _ => None,
        };
        result.push(CssRuleText {
            css_text: if nested_declarations { style.css_text().into() }
                else if node.kind==SourceRuleKind::Statement { format!("{};",prelude.trim_end_matches(';')) }
                else { raw.into() },
            selector_text, style,
            nested: source_rules_to_cssom_with_namespaces(text, &node.children, node.kind != SourceRuleKind::Scope && (has_parent || node.kind == SourceRuleKind::Style),namespaces)?,
            nested_declarations, font_face, keyframes,
            header: source_rule_header(text,node),
        });
    }
    Ok(result)
}

fn parse_group_child(text: &str, parent_context: &[css::nesting::NestingContext<'_>], namespaces: &css::namespaces::NamespaceMap) -> Result<CssRuleText, css::CssError> {
    let allow_declaration = !parent_context.is_empty();
    let source = css::nesting::parse_one_source_rule_in_namespace_context(text, parent_context, namespaces)?;
    let has_style_parent = matches!(parent_context.iter().rev().find(|context| !matches!(context, css::nesting::NestingContext::StartingStyle)), Some(css::nesting::NestingContext::Style(_)));
    let parsed = source_rules_to_cssom_with_namespaces(text, core::slice::from_ref(&source), has_style_parent,namespaces)?;
    if parsed.len() != 1 {
        return Err(css::CssError {
            offset: 0,
            message: "insertRule requires exactly one rule",
        });
    }
    let mut rule = parsed.into_iter().next().ok_or(css::CssError {
        offset: 0,
        message: "insertRule requires exactly one rule",
    })?;
    if rule.nested_declarations {
        if !allow_declaration {
            return Err(css::CssError {
                offset: 0,
                message: "declarations require a nesting context",
            });
        }
        let declarations = css::cssom_declaration_text(&rule.css_text);
        if declarations.is_empty() {
            return Err(css::CssError {
                offset: 0,
                message: "inserted declaration is invalid",
            });
        }
        rule.css_text = declarations.clone();
        rule.style = CssDeclaration::parse(&declarations);
    }
    Ok(rule)
}

fn implicit_nested_selector(selector: &str, has_parent: bool) -> String {
    if has_parent { css::nesting::absolutize_nested_selector(selector) } else { selector.into() }
}

fn serialize_rule(rule: &CssRuleText) -> String {
    let mut output = String::new();
    serialize_rule_into(rule, &mut output, None, &mut Vec::new());
    output
}

// Stream one canonical source and record only declaration boundaries that a
// textual roundtrip would otherwise merge. Ordinary rules need no retained tree.
fn serialize_rule_into(
    rule: &CssRuleText,
    output: &mut String,
    parent: Option<(usize, usize)>,
    boundaries: &mut Vec<css::nesting::DeclarationBoundary>,
) {
    if starts_css_at_rule(&rule.css_text,"@font-feature-values") {
        if let Ok(parsed)=css::parse_stylesheet(&rule.css_text) {
            if let Some(values)=parsed.family_display.first() {
                output.push_str(&css::font_feature_values::serialize_rule(&values.families,values.display_specified.then_some(values.display),&values.aliases));
                return;
            }
        }
    }
    // Descriptors were validated in their own grammar. The ordinary property
    // serializer would drop src, unicode-range, and descriptor ranges.
    let declarations = || if rule.font_face {
        Cow::Borrowed(rule.style.css_text())
    } else {
        css::serialize_cssom_declaration_block(rule.style.css_text())
            .map(Cow::Owned).unwrap_or_else(|_| Cow::Borrowed(rule.style.css_text()))
    };
    if rule.nested_declarations {
        let start = output.len();
        output.push_str(&declarations());
        if let Some((parent_start, child_index)) = parent {
            boundaries.push(css::nesting::DeclarationBoundary {
                parent_start, child_index, range: start..output.len(),
            });
        }
        return;
    }
    let Some(open) = css::nesting::source_rule_block_open(&rule.css_text) else {
        // Import locations are URL tokens in CSSOM, irrespective of the
        // authored string/url spelling. Reuse the graph parser so escaped
        // layer identifiers and import conditions share one grammar.
        if starts_css_at_rule(&rule.css_text, "@import") {
            if let Ok(imports) = css::imports(&rule.css_text) {
                if let Some(import) = imports.first() {
                    output.push_str(&css::serialize_import_rule(import));
                    return;
                }
            }
        }
        if rule.header==RuleHeader::Namespace {
            if let Ok(namespace)=css::namespaces::parse_rule(&rule.css_text) {
                output.push_str(&namespace.serialize());return;
            }
        }
        output.push_str(&rule.css_text);
        return;
    };
    let start = output.len();
    let prelude = rule.selector_text.as_deref().unwrap_or_else(|| rule.css_text[..open].trim());
    if starts_css_at_rule(&rule.css_text, "@scope") {
        output.push_str(&css::nesting::serialize_scope_prelude(prelude).unwrap_or_else(|_| prelude.to_owned()));
    } else {
        output.push_str(prelude);
    }
    if rule.selector_text.is_some() && rule.nested.is_empty() {
        output.push_str(" { ");
        let declarations = declarations();
        output.push_str(&declarations);
        if !declarations.is_empty() { output.push(' '); }
        output.push('}');
        return;
    }
    output.push_str(" {\n");
    let mut has_body = false;
    if !rule.style.css_text().is_empty() {
        output.push_str("  ");
        output.push_str(&declarations());
        has_body = true;
    }
    for (index, child) in rule.nested.iter().enumerate() {
        if has_body { output.push('\n'); }
        output.push_str("  ");
        serialize_rule_into(child, output, Some((start, index)), boundaries);
        has_body = true;
    }
    if has_body { output.push('\n'); }
    output.push('}');
}

/// A viewport-bound MediaQueryList snapshot. The DOM wrapper can query it on
/// each `matches` read; dispatching `change` requires a host resize source.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq)]
pub struct MediaQueryList {
    query: String,
    environment: MediaEnvironment,
}

#[cfg(test)]
impl MediaQueryList {
    pub fn new(query: &str, environment: MediaEnvironment) -> Self {
        Self {
            query: query.to_owned(),
            environment,
        }
    }

    pub fn matches(&self) -> bool {
        css::media_query_matches(&self.query, self.environment)
    }
}

struct MediaQueryData {
    realm: Weak<DomRealm>,
    query: String,
    was_matching: Cell<bool>,
    target: DomEventTarget,
    wrapper: RefCell<Option<WeakValue>>,
}

pub(crate) struct MediaQueryRegistry {
    read_cache: Rc<RefCell<Option<CssomReadCache>>>,
    realm: Weak<DomRealm>,
    lists: RefCell<Vec<Weak<MediaQueryData>>>,
    sheets: RefCell<HashMap<NodeId, WeakValue>>,
    sheet_positions: RefCell<HashMap<NodeId, Weak<RulePositions>>>,
    adopted: RefCell<Vec<(Option<NodeId>, Vec<AdoptedSheet>)>>,
    observable:AdoptedArrayIntrinsics,
}

#[derive(Default)]
struct RulePositions {
    // Documentless CSSOM uses the same weak owner caches as Window CSSOM.
    // A live source retains their registry; the registry only weakly retains
    // positions and sheet Values, so no owner/source cycle is formed.
    _registry: Option<Rc<MediaQueryRegistry>>,
    origin_metadata:Rc<RefCell<Arc<[(Arc<str>,bool)]>>>,
    namespaces:RefCell<Arc<css::namespaces::NamespaceMap>>,
    inherited_namespaces:bool,
    source_epoch: RefCell<Option<(Rc<Cell<u64>>, u64)>>,
    read_cache: Rc<RefCell<Option<CssomReadCache>>>,
    frame_rules: bool,
    nested_context: bool,
    scoped_context: bool,
    text: RefCell<Option<String>>,
    live: RefCell<Vec<Weak<RulePosition>>>,
    declarations: RefCell<Vec<(Arc<[usize]>, Rc<css::DeclarationBlock>)>>,
    boundaries: RefCell<Arc<[css::nesting::DeclarationBoundary]>>,
}

struct CssomReadCache {
    namespaces:Option<Arc<css::namespaces::NamespaceMap>>,
    nested_context: bool,
    scoped_context: bool,
    text: String,
    boundaries: Arc<[css::nesting::DeclarationBoundary]>,
    rules: Vec<css::nesting::SourceRule>,
    declaration: Option<(Vec<usize>, Rc<CssDeclaration>)>,
}

fn cssom_read_cache<'a>(slot: &'a mut Option<CssomReadCache>, text: &str, boundaries: Arc<[css::nesting::DeclarationBoundary]>, nested: bool, scoped: bool, namespaces:Option<Arc<css::namespaces::NamespaceMap>>) -> OpResult<&'a mut CssomReadCache> {
    if slot.as_ref().is_none_or(|cached| cached.text != text || cached.boundaries != boundaries || cached.nested_context != nested || cached.scoped_context != scoped || cached.namespaces!=namespaces) {
        let map=namespaces.clone().map(Ok).unwrap_or_else(||css::namespaces::NamespaceMap::from_stylesheet(text).map(Arc::new)).map_err(css_error)?;
        let rules = source_rules_in_cssom_context(text, nested, scoped, &boundaries,&map).map_err(css_error)?;
        *slot = Some(CssomReadCache { namespaces,nested_context: nested, scoped_context: scoped, text: text.to_owned(), boundaries, rules, declaration: None });
    }
    Ok(slot.as_mut().unwrap())
}

enum ReadCssDeclaration {
    Parsed(Rc<CssDeclaration>),
    Retained(Rc<css::DeclarationBlock>),
}

impl ReadCssDeclaration {
    fn block(&self) -> Option<&css::DeclarationBlock> {
        match self { Self::Parsed(declaration) => declaration.block.as_deref(), Self::Retained(block) => Some(block) }
    }
    fn descriptor_text(&self) -> &str {
        match self { Self::Parsed(declaration) => declaration.css_text(), Self::Retained(_) => "" }
    }
    fn value(&self, name: &str) -> Result<Option<(String, bool)>, css::CssError> {
        match self { Self::Parsed(declaration) => declaration.get_property_value(name), Self::Retained(block) => Ok(block.value(name)) }
    }
}

struct RulePosition {
    imported_sheet: RefCell<Option<Rc<ImportedSheetData>>>,
    style_context: Cell<bool>,
    scope_context: Cell<bool>,
    nested_declarations: Cell<bool>,
    rule_type: Cell<u16>,
    detached_boundaries: RefCell<Arc<[css::nesting::DeclarationBoundary]>>,
    detached_namespaces:RefCell<Arc<css::namespaces::NamespaceMap>>,
    detached_declarations: RefCell<Vec<(Arc<[usize]>, Rc<css::DeclarationBlock>)>>,
    index: Cell<usize>,
    detached: RefCell<Option<String>>,
    owner: Rc<RulePositions>,
    wrapper: RefCell<Option<WeakValue>>,
    children: RefCell<Option<Rc<RulePositions>>>,
}

impl RulePosition {
    fn get(&self) -> usize {
        self.index.get()
    }
    fn children(&self) -> Rc<RulePositions> {
        self.children_with_mode(true)
    }

    fn rule_children(&self) -> Rc<RulePositions> {
        self.children_with_mode(false)
    }

    fn children_with_mode(&self, frame_rules: bool) -> Rc<RulePositions> {
        self.children
            .borrow_mut()
            .get_or_insert_with(|| {
                Rc::new(RulePositions {
                    frame_rules,
                    nested_context: !frame_rules && self.style_context.get(),
                    scoped_context: !frame_rules && self.scope_context.get(),
                    read_cache: self.owner.read_cache.clone(),
                    namespaces:RefCell::new(self.owner.namespaces.borrow().clone()),
                    inherited_namespaces:true,
                    ..Default::default()
                })
            })
            .clone()
    }
}

impl RulePositions {
    // Child-list source has no containing rule. A sparse sentinel distinguishes
    // direct synthetic runs from nested boundaries whose first child starts at0.
    fn list_snapshot(&self, rules: &[CssRuleText]) -> OpResult<(String, Arc<[css::nesting::DeclarationBoundary]>)> {
        let mut text = String::new(); let mut boundaries = Vec::new();
        for (index, rule) in rules.iter().enumerate() {
            if !text.is_empty() { text.push('\n'); }
            serialize_rule_into(rule, &mut text, self.nested_context.then_some((usize::MAX, index)), &mut boundaries);
            if text.len() > 1024 * 1024 || boundaries.len() > css::nesting::MAX_DECLARATION_BOUNDARIES { return Err(OpError::new("QuotaExceededError", "CSSOM child topology exceeds limit")); }
        }
        Ok((text, boundaries.into()))
    }
    fn parsed_snapshot(&self, text: &str, boundaries: &[css::nesting::DeclarationBoundary]) -> OpResult<Vec<CssRuleText>> {
        if !self.nested_context { let namespaces=self.namespaces.borrow(); return source_rules_in_cssom_context(text,false,self.scoped_context,boundaries,&namespaces).and_then(|tree| source_rules_to_cssom_with_namespaces(text,&tree,false,&namespaces)).map_err(css_error); }
        let prefix = if self.scoped_context { "@scope {\n" } else { "* {\n" };
        let mut wrapped = String::new(); wrapped.try_reserve(text.len().checked_add(prefix.len() + 2).ok_or_else(|| OpError::new("QuotaExceededError", "CSSOM child source overflow"))?).map_err(|_| OpError::new("QuotaExceededError", "CSSOM child source allocation"))?;
        wrapped.push_str(prefix); wrapped.push_str(text); wrapped.push_str("\n}");
        let mut rebased = Vec::new(); rebased.try_reserve_exact(boundaries.len()).map_err(|_| OpError::new("QuotaExceededError", "CSSOM boundary allocation"))?;
        for boundary in boundaries {
            rebased.push(css::nesting::DeclarationBoundary { parent_start: if boundary.parent_start == usize::MAX { 0 } else { boundary.parent_start + prefix.len() }, child_index: boundary.child_index, range: boundary.range.start + prefix.len()..boundary.range.end + prefix.len() });
        }
        let namespaces=self.namespaces.borrow(); let tree = source_rules_in_cssom_context(&wrapped,false,false,&rebased,&namespaces).map_err(css_error)?;
        let parent = tree.first().ok_or_else(|| OpError::new("InvalidStateError", "CSSOM child context missing"))?;
        source_rules_to_cssom_with_namespaces(&wrapped, &parent.children, !self.scoped_context,&namespaces).map_err(css_error)
    }
    fn synchronize_rules(&self, rules: &[CssRuleText]) -> OpResult<()> {
        let (text, boundaries) = self.list_snapshot(rules)?;
        self.synchronize(&text)?;
        *self.boundaries.borrow_mut() = boundaries;
        Ok(())
    }
    fn remember_rules(&self, rules: &[CssRuleText]) -> OpResult<()> {
        let (text, boundaries) = self.list_snapshot(rules)?;
        self.remember(&text)?; *self.boundaries.borrow_mut() = boundaries;
        Ok(())
    }
    fn detach_rule(position: &RulePosition, rule: &CssRuleText) -> OpResult<()> {
        let mut text = String::new(); let mut boundaries = Vec::new();
        serialize_rule_into(rule, &mut text, None, &mut boundaries);
        if text.len() > 1024 * 1024 || boundaries.len() > css::nesting::MAX_DECLARATION_BOUNDARIES { return Err(OpError::new("QuotaExceededError", "detached CSSOM topology exceeds limit")); }
        position.nested_declarations.set(rule.nested_declarations);
        *position.detached_namespaces.borrow_mut()=position.owner.namespaces.borrow().clone();
        *position.detached.borrow_mut() = Some(text);
        *position.detached_boundaries.borrow_mut() = boundaries.into();
        position.index.set(0);
        Ok(())
    }
    fn retain_detached_declarations(&self, prefix: &[usize], deleted: Option<usize>, retained: &[(Arc<[usize]>, Rc<css::DeclarationBlock>)]) {
        if retained.is_empty() { return; }
        for position in self.live.borrow().iter().filter_map(Weak::upgrade) {
            if position.detached.borrow().is_some() || deleted.is_some_and(|index| position.get() != index) { continue; }
            let mut removed = prefix.to_vec();
            removed.push(position.get());
            let mut blocks = position.detached_declarations.borrow_mut();
            blocks.clear();
            for (path, block) in retained {
                if path.starts_with(&removed) {
                    let mut rebased = Vec::with_capacity(path.len() - removed.len() + 1);
                    rebased.push(0);
                    rebased.extend_from_slice(&path[removed.len()..]);
                    blocks.push((rebased.into(), block.clone()));
                }
            }
        }
    }
    fn at(self: &Rc<Self>, index: usize) -> Rc<RulePosition> {
        let mut live = self.live.borrow_mut();
        live.retain(|position| position.strong_count() != 0);
        if let Some(position) = live
            .iter()
            .filter_map(Weak::upgrade)
            .find(|position| position.get() == index && position.detached.borrow().is_none())
        {
            return position;
        }
        let position = Rc::new(RulePosition {
            imported_sheet: RefCell::new(None),
            style_context: Cell::new(false),
            scope_context: Cell::new(false),
            nested_declarations: Cell::new(false),
            rule_type: Cell::new(0),
            detached_boundaries: RefCell::new(Arc::from([])),
            detached_namespaces:RefCell::new(self.namespaces.borrow().clone()),
            detached_declarations: RefCell::new(Vec::new()),
            index: Cell::new(index),
            detached: RefCell::new(None),
            owner: self.clone(),
            wrapper: RefCell::new(None),
            children: RefCell::new(None),
        });
        live.push(Rc::downgrade(&position));
        position
    }

    fn remember(&self, text:&str)->OpResult<()> {
        if self.text.borrow().as_deref()==Some(text){return Ok(());}
        let namespaces=if !self.inherited_namespaces && !self.frame_rules {Some(Arc::new(css::namespaces::NamespaceMap::from_stylesheet(text).map_err(css_error)?))}else{None};
        *self.text.borrow_mut()=Some(text.to_owned());
        if let Some(namespaces)=namespaces{*self.namespaces.borrow_mut()=namespaces;}
        Ok(())
    }


    fn synchronize(&self, text: &str) -> OpResult<()> {
        if self.text.borrow().as_deref() == Some(text) {
            return Ok(());
        }
        self.replaced(text)
    }

    fn replaced(&self, text: &str) -> OpResult<()> {
        let previous_boundaries = self.boundaries.borrow().clone();
        self.live.borrow_mut().retain(|position| position.strong_count() != 0);
        let previous = self.text.borrow();
        if let Some(previous) = previous.as_deref().filter(|_| !self.live.borrow().is_empty()) {
            if self.frame_rules {
                let rules = css::parse_keyframes_rule(&format!("@keyframes identity {{ {previous} }}")).map_err(css_error)?
                    .ok_or_else(|| OpError::new("SyntaxError", "invalid retained keyframes"))?.rules;
                for position in self.live.borrow().iter().filter_map(Weak::upgrade) {
                    if position.detached.borrow().is_none() {
                        if let Some(rule) = rules.get(position.get()) { *position.detached.borrow_mut() = Some(rule.css_text.clone()); position.index.set(0); }
                    }
                }
            } else {
                let rules = self.parsed_snapshot(previous, &previous_boundaries)?;
                self.retain_detached_declarations(&[], None, &self.declarations.borrow());
                for position in self.live.borrow().iter().filter_map(Weak::upgrade) {
                    if position.detached.borrow().is_none() {
                        if let Some(rule) = rules.get(position.get()) { Self::detach_rule(&position, rule)?; }
                    }
                }
            }
            self.live.borrow_mut().clear();
        }
        drop(previous);
        self.declarations.borrow_mut().clear();
        *self.boundaries.borrow_mut() = Arc::from([]);
        self.remember(text)?;
        Ok(())
    }

    fn deleted_rule(&self, index: usize, rule: &CssRuleText, text: &str) -> OpResult<()> {
        for position in self.live.borrow().iter().filter_map(Weak::upgrade) {
            if position.detached.borrow().is_some() { continue; }
            if position.get() == index { Self::detach_rule(&position, rule)?; }
            else if position.get() > index { position.index.set(position.get() - 1); }
        }
        self.remember(text)?;
        Ok(())
    }

    fn inserted(&self, index: usize, text: &str)->OpResult<()> {
        for position in self.live.borrow().iter().filter_map(Weak::upgrade) {
            if position.detached.borrow().is_none() && position.get() >= index {
                position.index.set(position.get() + 1);
            }
        }
        self.remember(text)?;
        Ok(())
    }

    fn deleted(&self, index: usize, removed: &str, text: &str)->OpResult<()> {
        for position in self.live.borrow().iter().filter_map(Weak::upgrade) {
            if position.detached.borrow().is_some() {
                continue;
            }
            if position.get() == index {
                *position.detached.borrow_mut() = Some(removed.to_owned());
                position.index.set(0);
            } else if position.get() > index {
                position.index.set(position.get() - 1);
            }
        }
        self.remember(text)?;
        Ok(())
    }
}

struct ConstructedSheetData {
    realm: Weak<DomRealm>,
    constructor_document: NodeId,
    registry: Weak<MediaQueryRegistry>,
    text: RefCell<String>,
    positions: Rc<RulePositions>,
    modifying: Cell<bool>,
    base_url: Option<Arc<str>>,
    media: RefCell<String>,
    disabled: Cell<bool>,
}

struct ConstructedReplacementLock(Rc<ConstructedSheetData>);
impl Drop for ConstructedReplacementLock {
    fn drop(&mut self) { self.0.modifying.set(false); }
}

struct ImportedSheetData {
    owner: NodeId,
    parent: SheetSource,
    owner_position: Weak<RulePosition>,
    detached_graph: RefCell<Option<Box<css::StylesheetSource>>>,
    lease: RefCell<Option<Rc<lumen_html::session::StylesheetImportLease>>>,
    positions: Rc<RulePositions>,
}

impl ImportedSheetData {
    fn rule_start(&self, realm: &DomRealm) -> OpResult<Option<usize>> {
        let position = self.owner_position.upgrade().ok_or_else(|| OpError::new("InvalidStateError", "import owner unavailable"))?;
        if position.detached.borrow().is_some() { return Ok(None); }
        with_source_text(realm, &self.parent, &mut |text| {
            position.owner.synchronize(text)?;
            if position.detached.borrow().is_some() { return Ok(None); }
            let boundaries = self.parent.boundary_state().borrow().clone();
            let mut cache = self.positions.read_cache.borrow_mut();
            let cached = cssom_read_cache(&mut cache, text, boundaries, false, false,self.parent.detached_position().map(|position|position.detached_namespaces.borrow().clone()))?;
            Ok(cached.rules.get(position.get()).map(|rule| rule.range.start))
        })
    }

    fn with_graph<R>(&self, realm: &DomRealm, read: &mut dyn FnMut(Option<&css::StylesheetSource>) -> OpResult<R>) -> OpResult<R> {
        if let Some(graph) = self.detached_graph.borrow().as_deref() { return read(Some(graph)); }
        if let Some(result) = self.lease.borrow().as_ref().and_then(|lease| lease.with_detached_source(|graph| read(graph))) { return result; }
        let Some(start) = self.rule_start(realm)? else { return read(None); };
        with_source_graph(realm, &self.parent, &mut |parent| {
            read(parent.and_then(|parent| parent.imports.iter().find(|import| import.rule.span.start == start)).and_then(|import| import.source.as_deref()))
        })
    }

    fn live_path(&self, realm: &DomRealm) -> OpResult<Option<Vec<usize>>> {
        if self.detached_graph.borrow().is_some() { return Ok(None); }
        if self.lease.borrow().as_ref().is_some_and(|lease| lease.is_detached()) { return Ok(None); }
        let Some(start) = self.rule_start(realm)? else { return Ok(None); };
        let mut path = match self.parent.root() {
            SheetSource::Element { lease, .. } => { if lease.is_detached() { return Ok(None); } Vec::new() },
            SheetSource::Imported(parent) => match parent.live_path(realm)? { Some(path) => path, None => return Ok(None) },
            _ => return Ok(None),
        };
        let ordinal = with_source_graph(realm, &self.parent, &mut |graph| Ok(graph.and_then(|graph| graph.imports.iter().position(|import| import.rule.span.start == start))))?;
        let Some(ordinal) = ordinal else { return Ok(None); };
        if path.len() >= 32 { return Err(OpError::new("QuotaExceededError", "CSS import depth limit")); }
        path.push(ordinal);
        Ok(Some(path))
    }

    fn with_local_graph_mut<R>(&self, realm: &DomRealm, edit: &mut dyn FnMut(&mut css::StylesheetSource) -> OpResult<R>) -> OpResult<R> {
        if let Some(graph) = self.detached_graph.borrow_mut().as_deref_mut() { return edit(graph); }
        if let Some(result) = self.lease.borrow().as_ref().and_then(|lease| lease.with_detached_source_mut(|graph| {
            edit(graph.ok_or_else(|| OpError::new("InvalidStateError", "retained import graph unavailable"))?)
        })) { return result; }
        let Some(start) = self.rule_start(realm)? else { return Err(OpError::new("InvalidStateError", "imported graph unavailable")); };
        with_local_source_graph_mut(realm, &self.parent, &mut |graph| {
            let child = graph.imports.iter_mut().find(|import| import.rule.span.start == start).and_then(|import| import.source.as_deref_mut())
                .ok_or_else(|| OpError::new("InvalidStateError", "imported graph unavailable"))?;
            edit(child)
        })
    }

    fn take_graph(&self, realm: &DomRealm) -> OpResult<Option<Box<css::StylesheetSource>>> {
        if let Some(path) = self.live_path(realm)? { return Ok(realm.session.borrow_mut().take_imported_stylesheet(self.owner, &path)); }
        let Some(start) = self.rule_start(realm)? else { return Ok(None); };
        with_local_source_graph_mut(realm, &self.parent, &mut |graph| Ok(graph.imports.iter_mut().find(|import| import.rule.span.start == start).and_then(|import| import.source.take())))
    }

    fn restore_graph(&self, realm: &DomRealm) -> OpResult<()> {
        let graph = self.detached_graph.borrow_mut().take();
        let Some(graph) = graph else { return Ok(()); };
        if let Some(path) = self.live_path(realm)? {
            if let Err(graph) = realm.session.borrow_mut().restore_imported_stylesheet(self.owner, &path, graph) {
                *self.detached_graph.borrow_mut() = Some(graph);
                return Err(OpError::new("InvalidStateError", "import graph rollback unavailable"));
            }
            return Ok(());
        }
        let Some(start) = self.rule_start(realm)? else { *self.detached_graph.borrow_mut() = Some(graph); return Err(OpError::new("InvalidStateError", "import rollback owner unavailable")); };
        let mut graph = Some(graph);
        let result = with_local_source_graph_mut(realm, &self.parent, &mut |parent| {
            let target = parent.imports.iter_mut().find(|import| import.rule.span.start == start)
                .ok_or_else(|| OpError::new("InvalidStateError", "import rollback occurrence unavailable"))?;
            if target.source.is_some() { return Err(OpError::new("InvalidStateError", "import rollback occurrence occupied")); }
            target.source = graph.take();
            Ok(())
        });
        if let Some(graph) = graph { *self.detached_graph.borrow_mut() = Some(graph); }
        result
    }
}

fn with_local_source_graph_mut<R>(realm: &DomRealm, source: &SheetSource, edit: &mut dyn FnMut(&mut css::StylesheetSource) -> OpResult<R>) -> OpResult<R> {
    match source.root() {
        SheetSource::Element { lease, .. } if lease.is_detached() => lease.with_detached_source_mut(|graph| edit(graph.ok_or_else(|| OpError::new("InvalidStateError", "retained stylesheet graph unavailable"))?)),
        SheetSource::Imported(parent) => parent.with_local_graph_mut(realm, edit),
        _ => Err(OpError::new("InvalidStateError", "stylesheet graph is not detached")),
    }
}

fn with_source_graph<R>(realm: &DomRealm, source: &SheetSource, read: &mut dyn FnMut(Option<&css::StylesheetSource>) -> OpResult<R>) -> OpResult<R> {
    match source.root() {
        SheetSource::Element { node, lease, .. } => {
            if lease.is_detached() { lease.with_detached_source(read) }
            else { let session = realm.session.borrow(); read(session.stylesheet_source(*node)) }
        },
        SheetSource::Imported(data) => data.with_graph(realm, read),
        _ => read(None),
    }
}

#[derive(Clone)]
enum SheetSource {
    Element {
        node: NodeId,
        positions: Rc<RulePositions>,
        lease: Rc<lumen_html::session::StylesheetGraphLease>,
    },
    Rule {
        source: Rc<SheetSource>,
        position: Rc<RulePosition>,
    },
    Constructed(Rc<ConstructedSheetData>),
    Imported(Rc<ImportedSheetData>),
}

impl SheetSource {
    fn detached_position(&self) -> Option<&RulePosition> {
        match self {
            Self::Rule { source, position } => {
                if position.detached.borrow().is_some() { Some(position) } else { source.detached_position() }
            }
            _ => None,
        }
    }

    fn boundary_state(&self) -> &RefCell<Arc<[css::nesting::DeclarationBoundary]>> {
        match self {
            Self::Rule { source, position } => if position.detached.borrow().is_some() { &position.detached_boundaries } else { source.boundary_state() },
            Self::Element { positions, .. } => &positions.boundaries,
            Self::Constructed(data) => &data.positions.boundaries,
            Self::Imported(data) => &data.positions.boundaries,
        }
    }
    fn declaration_state(&self) -> &RefCell<Vec<(Arc<[usize]>, Rc<css::DeclarationBlock>)>> {
        match self {
            Self::Rule { source, position } => {
                if position.detached.borrow().is_some() { &position.detached_declarations } else { source.declaration_state() }
            }
            Self::Element { positions, .. } => &positions.declarations,
            Self::Constructed(data) => &data.positions.declarations,
            Self::Imported(data) => &data.positions.declarations,
        }
    }
    fn positions(&self) -> Rc<RulePositions> {
        match self {
            Self::Element { positions, .. } => positions.clone(),
            Self::Constructed(data) => data.positions.clone(),
            Self::Imported(data) => data.positions.clone(),
            Self::Rule { position, .. } => position.owner.clone(),
        }
    }
    fn root(&self) -> &Self {
        match self {
            Self::Rule { source, .. } => source.root(),
            _ => self,
        }
    }
}

fn rule_path_indices(path: &[Rc<RulePosition>]) -> Vec<usize> {
    if let Some(start) = path.iter().rposition(|position| position.detached.borrow().is_some()) {
        core::iter::once(0).chain(path[start + 1..].iter().map(|position| position.get())).collect()
    } else { path.iter().map(|position| position.get()).collect() }
}
struct RuleLocation { source: SheetSource, indices: Vec<usize> }
fn rule_location(realm: &DomRealm, source: &SheetSource, path: &[Rc<RulePosition>]) -> OpResult<RuleLocation> {
    realm.session.borrow_mut().reclaim_stale_stylesheet_sources();
    if !path.iter().any(|position| position.detached.borrow().is_some()) {
        with_source_text(realm, source, &mut |_| Ok(()))?;
    }
    // A child independently removed before its ancestor owns a separate subtree.
    // Its detached source takes precedence over that ancestor's later snapshot.
    let source = if let Some(position) = path.iter().rev().find(|position| position.detached.borrow().is_some()) {
        SheetSource::Rule { source: Rc::new(source.root().clone()), position: position.clone() }
    } else { source.clone() };
    Ok(RuleLocation { source, indices: rule_path_indices(path) })
}

fn rules_text(rules: &[CssRuleText]) -> String {
    rules
        .iter()
        .map(|rule| rule.css_text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn positions_for_rule_list(
    source: &SheetSource,
    parent_path: &[Rc<RulePosition>],
) -> Rc<RulePositions> {
    parent_path
        .last()
        .map_or_else(|| source.positions(), |parent| parent.rule_children())
}

fn remember_rule_path(
    source: &SheetSource,
    sheet: &CssStyleSheetText,
    path: &[Rc<RulePosition>],
) -> OpResult<()> {
    if source.detached_position().is_none() { source.positions().remember(sheet.css_text())?; }
    let mut rules = sheet.css_rules().map_err(css_error)?;
    let start = path.iter().rposition(|position| position.detached.borrow().is_some()).unwrap_or(0);
    for (depth, parent) in path[start..].iter().enumerate() {
        let index = if depth == 0 && parent.detached.borrow().is_some() { 0 } else { parent.get() };
        let Some(rule) = rules.get(index) else {
            return Ok(());
        };
        let children = parent.rule_children();
        children.remember_rules(&rule.nested)?;
        rules = rule.nested.clone();
    }
    Ok(())
}

fn rule_from_path(sheet: &CssStyleSheetText, path: &[Rc<RulePosition>]) -> OpResult<CssRuleText> {
    sheet
        .rule_at_path(&rule_path_indices(path))
        .map_err(|error| {
            if path
                .iter()
                .any(|position| position.detached.borrow().is_some())
            {
                OpError::new("InvalidStateError", "stylesheet rule was removed")
            } else {
                css_error(error)
            }
        })
}

#[derive(Clone)]
struct AdoptedSheet {
    data: Rc<ConstructedSheetData>,
    value: Value,
}

#[lumen_bind::class(name = "MediaQueryList", extends = DomEventTarget, hint(js(webidl)))]
pub struct DomMediaQueryList {
    base: DomEventTarget,
    data: Rc<MediaQueryData>,
}

#[lumen_bind::methods]
impl DomMediaQueryList {
    #[getter]
    fn media(&self) -> String {
        self.data.query.clone()
    }

    #[getter]
    fn matches(&self) -> bool {
        self.data.realm.upgrade().is_some_and(|realm| {
            realm.effective_media_environment().is_ok_and(|environment|css::media_query_matches(&self.data.query,environment))
        })
    }

    // The standard EventTarget methods are inherited. This override records
    // the wrapper so host-driven environment changes can dispatch `change`.
    fn add_event_listener(
        &self,
        ctx: &mut Ctx,
        this: This<Value>,
        kind: &str,
        callback: Value,
        options: Option<Value>,
    ) -> OpResult<()> {
        *self.data.wrapper.borrow_mut() = ctx.weak_value(&this.0);
        crate::events::add_event_listener(ctx, this, kind, callback, options)
    }

    fn add_listener(&self, ctx: &mut Ctx, this: This<Value>, callback: Value) -> OpResult<()> {
        self.add_event_listener(ctx, this, "change", callback, None)
    }

    fn remove_listener(&self, ctx: &mut Ctx, this: This<Value>, callback: Value) -> OpResult<()> {
        crate::events::remove_event_listener(ctx, this, "change", callback, None)
    }

    #[getter]
    fn onchange(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.handler_value(ctx, &this.0, "change")
    }

    #[setter]
    fn set_onchange(&self, ctx: &mut Ctx, this: This<Value>, callback: crate::events::EventHandler) {
        self.base.set_event_handler(ctx, &this.0, "change", callback);
        *self.data.wrapper.borrow_mut() = ctx.weak_value(&this.0);
    }
}

#[lumen_bind::op(name = "matchMedia")]
pub fn match_media(ctx: &mut Ctx, query: &str) -> OpResult<Value> {
    let registry = RealmServices::<MediaQueryRegistry>::current(ctx)
        .ok_or_else(|| OpError::new("InvalidStateError", "CSSOM is not installed"))?;
    let realm = registry
        .realm
        .upgrade()
        .ok_or_else(|| OpError::new("InvalidStateError", "document realm was released"))?;
    let environment = realm.effective_media_environment()?;
    let data = Rc::new(MediaQueryData {
        realm: Rc::downgrade(&realm),
        query: query.trim().to_owned(),
        was_matching: Cell::new(css::media_query_matches(query, environment)),
        target: DomEventTarget::independent(&realm),
        wrapper: RefCell::new(None),
    });
    registry.lists.borrow_mut().push(Rc::downgrade(&data));
    let value=ctx.new_instance(DomMediaQueryList {
        base: DomEventTarget::from_data(data.target.data_handle()),
        data,
    });
    // Both embedded EventTargets share one physical TargetData. Register its
    // existing base tracer once on the actual MediaQueryList wrapper.
    ctx.set_native_identity_owner::<DomEventTarget>(&value)?;
    Ok(value)
}

/// Install CSS Typed OM value classes and the numeric `CSS` factories.
/// This subset has no document/session dependency, so browser workers use the
/// same installation path as Window realms instead of maintaining a second
/// list of interfaces and unit factories.
pub fn install_typed_numeric(ctx: &mut Ctx) -> OpResult<()> {
    ctx.class_constructor::<DomCssStyleValue>();
    ctx.class_constructor::<DomCssImageValue>();
    ctx.class_constructor::<DomCssKeywordValue>();
    ctx.class_constructor::<DomCssNumericValue>();
    ctx.class_constructor::<DomCssUnitValue>();
    ctx.class_constructor::<DomCssMathValue>();
    ctx.class_constructor::<DomCssMathSum>();
    ctx.class_constructor::<DomCssMathProduct>();
    ctx.class_constructor::<DomCssMathMin>();
    ctx.class_constructor::<DomCssMathMax>();
    ctx.class_constructor::<DomCssMathClamp>();
    ctx.class_constructor::<DomCssMathNegate>();
    ctx.class_constructor::<DomCssMathInvert>();
    ctx.class_constructor::<DomCssNumericArray>();
    ctx.class_constructor::<DomCssNumericArrayIterator>();
    ctx.class_constructor::<DomCssUnparsedValue>();
    ctx.class_constructor::<DomCssVariableReferenceValue>();
    ctx.class_constructor::<DomCssUnparsedValueIterator>();

    let global = ctx.global_object();
    macro_rules! install_interface {
        ($name:literal, $class:ty) => {{
            let constructor = ctx.class_constructor::<$class>();
            crate::install_interface(ctx, &global, $name, constructor)
                .map_err(|_| OpError::new("Error", concat!($name, " installation failed")))?;
        }};
    }
    install_interface!("CSSStyleValue", DomCssStyleValue);
    install_interface!("CSSImageValue", DomCssImageValue);
    install_interface!("CSSKeywordValue", DomCssKeywordValue);
    install_interface!("CSSNumericValue", DomCssNumericValue);
    install_interface!("CSSUnitValue", DomCssUnitValue);
    install_interface!("CSSMathValue", DomCssMathValue);
    install_interface!("CSSMathSum", DomCssMathSum);
    install_interface!("CSSMathProduct", DomCssMathProduct);
    install_interface!("CSSMathMin", DomCssMathMin);
    install_interface!("CSSMathMax", DomCssMathMax);
    install_interface!("CSSMathClamp", DomCssMathClamp);
    install_interface!("CSSMathNegate", DomCssMathNegate);
    install_interface!("CSSMathInvert", DomCssMathInvert);
    install_interface!("CSSNumericArray", DomCssNumericArray);
    install_interface!("CSSUnparsedValue", DomCssUnparsedValue);
    install_interface!("CSSVariableReferenceValue", DomCssVariableReferenceValue);
    transforms::install(ctx,&global)?;

    let css_namespace = Value::Obj(ctx.new_object());
    let supports = ctx.bound_function(&lumen_bind::FnItem::of::<supports::Op>());
    ctx.set_member(&css_namespace, "supports", supports)
        .map_err(|_| OpError::new("Error", "CSS.supports installation failed"))?;
    let escape = ctx.bound_function(&lumen_bind::FnItem::of::<escape::Op>());
    ctx.set_member(&css_namespace, "escape", escape)
        .map_err(|_| OpError::new("Error", "CSS.escape installation failed"))?;
    install_numeric_factory!(ctx, css_namespace, "number", numeric_factories::number::Op);
    install_numeric_factory!(
        ctx,
        css_namespace,
        "percent",
        numeric_factories::percent::Op
    );
    install_numeric_factory!(ctx, css_namespace, "cap", numeric_factories::cap::Op);
    install_numeric_factory!(ctx, css_namespace, "ch", numeric_factories::ch::Op);
    install_numeric_factory!(ctx, css_namespace, "em", numeric_factories::em::Op);
    install_numeric_factory!(ctx, css_namespace, "ex", numeric_factories::ex::Op);
    install_numeric_factory!(ctx, css_namespace, "ic", numeric_factories::ic::Op);
    install_numeric_factory!(ctx, css_namespace, "lh", numeric_factories::lh::Op);
    install_numeric_factory!(ctx, css_namespace, "rcap", numeric_factories::rcap::Op);
    install_numeric_factory!(ctx, css_namespace, "rch", numeric_factories::rch::Op);
    install_numeric_factory!(ctx, css_namespace, "rem", numeric_factories::rem::Op);
    install_numeric_factory!(ctx, css_namespace, "rex", numeric_factories::rex::Op);
    install_numeric_factory!(ctx, css_namespace, "ric", numeric_factories::ric::Op);
    install_numeric_factory!(ctx, css_namespace, "rlh", numeric_factories::rlh::Op);
    install_numeric_factory!(ctx, css_namespace, "vw", numeric_factories::vw::Op);
    install_numeric_factory!(ctx, css_namespace, "vh", numeric_factories::vh::Op);
    install_numeric_factory!(ctx, css_namespace, "vi", numeric_factories::vi::Op);
    install_numeric_factory!(ctx, css_namespace, "vb", numeric_factories::vb::Op);
    install_numeric_factory!(ctx, css_namespace, "vmin", numeric_factories::vmin::Op);
    install_numeric_factory!(ctx, css_namespace, "vmax", numeric_factories::vmax::Op);
    install_numeric_factory!(ctx, css_namespace, "svw", numeric_factories::svw::Op);
    install_numeric_factory!(ctx, css_namespace, "svh", numeric_factories::svh::Op);
    install_numeric_factory!(ctx, css_namespace, "svi", numeric_factories::svi::Op);
    install_numeric_factory!(ctx, css_namespace, "svb", numeric_factories::svb::Op);
    install_numeric_factory!(ctx, css_namespace, "svmin", numeric_factories::svmin::Op);
    install_numeric_factory!(ctx, css_namespace, "svmax", numeric_factories::svmax::Op);
    install_numeric_factory!(ctx, css_namespace, "lvw", numeric_factories::lvw::Op);
    install_numeric_factory!(ctx, css_namespace, "lvh", numeric_factories::lvh::Op);
    install_numeric_factory!(ctx, css_namespace, "lvi", numeric_factories::lvi::Op);
    install_numeric_factory!(ctx, css_namespace, "lvb", numeric_factories::lvb::Op);
    install_numeric_factory!(ctx, css_namespace, "lvmin", numeric_factories::lvmin::Op);
    install_numeric_factory!(ctx, css_namespace, "lvmax", numeric_factories::lvmax::Op);
    install_numeric_factory!(ctx, css_namespace, "dvw", numeric_factories::dvw::Op);
    install_numeric_factory!(ctx, css_namespace, "dvh", numeric_factories::dvh::Op);
    install_numeric_factory!(ctx, css_namespace, "dvi", numeric_factories::dvi::Op);
    install_numeric_factory!(ctx, css_namespace, "dvb", numeric_factories::dvb::Op);
    install_numeric_factory!(ctx, css_namespace, "dvmin", numeric_factories::dvmin::Op);
    install_numeric_factory!(ctx, css_namespace, "dvmax", numeric_factories::dvmax::Op);
    install_numeric_factory!(ctx, css_namespace, "cqw", numeric_factories::cqw::Op);
    install_numeric_factory!(ctx, css_namespace, "cqh", numeric_factories::cqh::Op);
    install_numeric_factory!(ctx, css_namespace, "cqi", numeric_factories::cqi::Op);
    install_numeric_factory!(ctx, css_namespace, "cqb", numeric_factories::cqb::Op);
    install_numeric_factory!(ctx, css_namespace, "cqmin", numeric_factories::cqmin::Op);
    install_numeric_factory!(ctx, css_namespace, "cqmax", numeric_factories::cqmax::Op);
    install_numeric_factory!(ctx, css_namespace, "cm", numeric_factories::cm::Op);
    install_numeric_factory!(ctx, css_namespace, "mm", numeric_factories::mm::Op);
    install_numeric_factory!(ctx, css_namespace, "Q", numeric_factories::q::Op);
    install_numeric_factory!(ctx, css_namespace, "in", numeric_factories::inch::Op);
    install_numeric_factory!(ctx, css_namespace, "pt", numeric_factories::pt::Op);
    install_numeric_factory!(ctx, css_namespace, "pc", numeric_factories::pc::Op);
    install_numeric_factory!(ctx, css_namespace, "px", numeric_factories::px::Op);
    install_numeric_factory!(ctx, css_namespace, "deg", numeric_factories::deg::Op);
    install_numeric_factory!(ctx, css_namespace, "grad", numeric_factories::grad::Op);
    install_numeric_factory!(ctx, css_namespace, "rad", numeric_factories::rad::Op);
    install_numeric_factory!(ctx, css_namespace, "turn", numeric_factories::turn::Op);
    install_numeric_factory!(ctx, css_namespace, "s", numeric_factories::s::Op);
    install_numeric_factory!(ctx, css_namespace, "ms", numeric_factories::ms::Op);
    install_numeric_factory!(ctx, css_namespace, "Hz", numeric_factories::hz::Op);
    install_numeric_factory!(ctx, css_namespace, "kHz", numeric_factories::khz::Op);
    install_numeric_factory!(ctx, css_namespace, "dpi", numeric_factories::dpi::Op);
    install_numeric_factory!(ctx, css_namespace, "dpcm", numeric_factories::dpcm::Op);
    install_numeric_factory!(ctx, css_namespace, "dppx", numeric_factories::dppx::Op);
    install_numeric_factory!(ctx, css_namespace, "fr", numeric_factories::fr::Op);
    ctx.set_member(&global, "CSS", css_namespace)
        .map_err(|_| OpError::new("Error", "CSS namespace installation failed"))?;
    Ok(())
}

/// Install the standards-facing CSSOM and `matchMedia` operation for a
/// document realm. The typed numeric subset above is shared with workers.
pub fn install(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    ctx.install_module_synthetic_factory("css", Rc::new(|ctx, source| {
        create_module_stylesheet(ctx, source).map_err(|error| error.to_value(ctx))
    }));
    ctx.class_constructor::<DomMediaQueryList>();
    ctx.class_constructor::<DomStylePropertyMapReadOnly>();
    ctx.class_constructor::<DomStylePropertyMap>();
    ctx.class_constructor::<DomCssStyleSheet>();
    ctx.class_constructor::<DomCssRule>();
    ctx.class_constructor::<DomCssRuleList>();
    ctx.class_constructor::<DomCssomCollectionIterator>();
    ctx.class_constructor::<DomCssGroupingRule>();
    ctx.class_constructor::<DomCssConditionRule>();
    ctx.class_constructor::<DomCssStyleRule>();
    ctx.class_constructor::<DomCssMediaRule>();
    ctx.class_constructor::<DomCssSupportsRule>();
    ctx.class_constructor::<DomCssScopeRule>();
    ctx.class_constructor::<DomCssStartingStyleRule>();
    ctx.class_constructor::<DomCssNestedDeclarations>();
    ctx.class_constructor::<DomCssFontFaceRule>();
    ctx.class_constructor::<DomCssPropertyRule>();
    font_features::install(ctx)?;
    ctx.class_constructor::<DomCssKeyframesRule>();
    ctx.class_constructor::<DomCssKeyframeRule>();
    ctx.class_constructor::<DomCssImportRule>();
    ctx.class_constructor::<DomCssNamespaceRule>();
    ctx.class_constructor::<DomCssMediaList>();
    ctx.class_constructor::<DomCssRuleStyle>();
    ctx.class_constructor::<DomCssFontFaceDescriptors>();
    ctx.class_constructor::<DomStyleSheetList>();
    super::style::install_css_property_aliases(ctx)?;
    let observable = AdoptedArrayIntrinsics::capture(ctx)?;
    let registry = Rc::new(MediaQueryRegistry {
            read_cache: Rc::default(),
            realm: Rc::downgrade(realm),
            lists: RefCell::new(Vec::new()),
            sheets: RefCell::new(HashMap::new()),
            sheet_positions: RefCell::new(HashMap::new()),
            adopted: RefCell::new(Vec::new()),
            observable,
        });
    *realm.cssom_registry.borrow_mut()=Rc::downgrade(&registry);
    RealmServices::replace_shared_current(ctx, registry);
    install_typed_numeric(ctx)?;
    crate::highlight::install(ctx, realm)?;
    let css_namespace = ctx.member_get(&ctx.global_object(), "CSS").map_err(OpError::thrown)?;
    let register = ctx.bound_function(&lumen_bind::FnItem::of::<register_property::Op>());
    ctx.set_member(&css_namespace, "registerProperty", register)
        .map_err(|_| OpError::error("CSS.registerProperty installation failed"))?;
    let global = ctx.global_object();
    let rule_constructor = ctx.class_constructor::<DomCssRule>();
    crate::install_interface(ctx, &global, "CSSRule", rule_constructor)
        .map_err(|_| OpError::new("Error", "CSSRule installation failed"))?;
    let rule_list_constructor = ctx.class_constructor::<DomCssRuleList>();
    crate::install_interface(ctx, &global, "CSSRuleList", rule_list_constructor)
        .map_err(|_| OpError::new("Error", "CSSRuleList installation failed"))?;
    let grouping_constructor = ctx.class_constructor::<DomCssGroupingRule>();
    crate::install_interface(ctx, &global, "CSSGroupingRule", grouping_constructor)
        .map_err(|_| OpError::new("Error", "CSSGroupingRule installation failed"))?;
    let condition_rule_constructor = ctx.class_constructor::<DomCssConditionRule>();
    crate::install_interface(ctx, &global, "CSSConditionRule", condition_rule_constructor)
        .map_err(|_| OpError::new("Error", "CSSConditionRule installation failed"))?;
    let style_rule_constructor = ctx.class_constructor::<DomCssStyleRule>();
    crate::install_interface(ctx, &global, "CSSStyleRule", style_rule_constructor)
        .map_err(|_| OpError::new("Error", "CSSStyleRule installation failed"))?;
    let media_rule_constructor = ctx.class_constructor::<DomCssMediaRule>();
    crate::install_interface(ctx, &global, "CSSMediaRule", media_rule_constructor)
        .map_err(|_| OpError::new("Error", "CSSMediaRule installation failed"))?;
    let supports_rule_constructor = ctx.class_constructor::<DomCssSupportsRule>();
    crate::install_interface(ctx, &global, "CSSSupportsRule", supports_rule_constructor)
        .map_err(|_| OpError::new("Error", "CSSSupportsRule installation failed"))?;
    let starting_style_constructor = ctx.class_constructor::<DomCssStartingStyleRule>();
    crate::install_interface(ctx, &global, "CSSStartingStyleRule", starting_style_constructor)
        .map_err(|_| OpError::new("Error", "CSSStartingStyleRule installation failed"))?;
    let scope_rule_constructor = ctx.class_constructor::<DomCssScopeRule>();
    crate::install_interface(ctx, &global, "CSSScopeRule", scope_rule_constructor)
        .map_err(|_| OpError::new("Error", "CSSScopeRule installation failed"))?;
    let nested_declarations_constructor = ctx.class_constructor::<DomCssNestedDeclarations>();
    crate::install_interface(
        ctx,
        &global,
        "CSSNestedDeclarations",
        nested_declarations_constructor,
    )
    .map_err(|_| OpError::new("Error", "CSSNestedDeclarations installation failed"))?;
    let property_constructor = ctx.class_constructor::<DomCssPropertyRule>();
    crate::install_interface(ctx, &global, "CSSPropertyRule", property_constructor)
        .map_err(|_| OpError::new("Error", "CSSPropertyRule installation failed"))?;
    let font_face_constructor = ctx.class_constructor::<DomCssFontFaceRule>();
    crate::install_interface(ctx, &global, "CSSFontFaceRule", font_face_constructor)
        .map_err(|_| OpError::new("Error", "CSSFontFaceRule installation failed"))?;
    let keyframes_constructor = ctx.class_constructor::<DomCssKeyframesRule>();
    crate::install_interface(ctx, &global, "CSSKeyframesRule", keyframes_constructor)
        .map_err(|_| OpError::new("Error", "CSSKeyframesRule installation failed"))?;
    let keyframe_constructor = ctx.class_constructor::<DomCssKeyframeRule>();
    crate::install_interface(ctx, &global, "CSSKeyframeRule", keyframe_constructor)
        .map_err(|_| OpError::new("Error", "CSSKeyframeRule installation failed"))?;
    let import_constructor = ctx.class_constructor::<DomCssImportRule>();
    crate::install_interface(ctx, &global, "CSSImportRule", import_constructor)
        .map_err(|_| OpError::new("Error", "CSSImportRule installation failed"))?;
    let namespace_constructor=ctx.class_constructor::<DomCssNamespaceRule>();
    crate::install_interface(ctx,&global,"CSSNamespaceRule",namespace_constructor)
        .map_err(|_|OpError::error("CSSNamespaceRule installation failed"))?;
    let media_list_constructor = ctx.class_constructor::<DomCssMediaList>();
    crate::install_interface(ctx, &global, "MediaList", media_list_constructor)
        .map_err(|_| OpError::new("Error", "MediaList installation failed"))?;
    let descriptors_constructor = ctx.class_constructor::<DomCssFontFaceDescriptors>();
    crate::install_interface(
        ctx,
        &global,
        "CSSFontFaceDescriptors",
        descriptors_constructor,
    )
    .map_err(|_| OpError::new("Error", "CSSFontFaceDescriptors installation failed"))?;
    let sheet_constructor = ctx.class_constructor::<DomCssStyleSheet>();
    crate::install_interface(ctx, &global, "CSSStyleSheet", sheet_constructor)
        .map_err(|_| OpError::new("Error", "CSSStyleSheet installation failed"))?;
    let property_map_readonly_constructor = ctx.class_constructor::<DomStylePropertyMapReadOnly>();
    ctx.set_member(
        &global,
        "StylePropertyMapReadOnly",
        property_map_readonly_constructor,
    )
    .map_err(|_| OpError::new("Error", "StylePropertyMapReadOnly installation failed"))?;
    let property_map_constructor = ctx.class_constructor::<DomStylePropertyMap>();
    crate::install_interface(ctx, &global, "StylePropertyMap", property_map_constructor)
        .map_err(|_| OpError::new("Error", "StylePropertyMap installation failed"))?;
    let match_media = ctx.bound_function(&lumen_bind::FnItem::of::<match_media::Op>());
    ctx.set_member(&global, "matchMedia", match_media)
        .map_err(|_| OpError::new("Error", "matchMedia installation failed"))?;
    Ok(())
}

/// Re-evaluate live query lists and dispatch `change` when a host viewport or
/// display change crosses a media-query boundary.
pub fn notify_media(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    // The host must enter the target window's JS realm before dispatching. Apart from
    // isolating the registries, this ensures the UA-created Event comes from that
    // realm's intrinsics.
    let Some(registry) = registry_for_realm(ctx, realm) else {
        return Ok(());
    };
    let environment = realm.effective_media_environment()?;
    // Never hold the registry borrow while running script: a `change`
    // listener may call matchMedia() and append another list reentrantly.
    let snapshot = registry.lists.borrow().clone();
    let mut live = Vec::with_capacity(snapshot.len());
    for weak in snapshot {
        let Some(data) = weak.upgrade() else {
            continue;
        };
        live.push(Rc::downgrade(&data));
        let matches = css::media_query_matches(&data.query, environment);
        let previous = data.was_matching.replace(matches);
        if previous != matches {
            if let Some(wrapper) = data.wrapper.borrow().as_ref().and_then(WeakValue::upgrade) {
                let options = Value::Obj(ctx.new_object());
                if ctx
                    .set_member(&options, "bubbles", Value::Bool(false))
                    .is_err()
                    || ctx
                        .set_member(&options, "cancelable", Value::Bool(false))
                        .is_err()
                {
                    continue;
                }
                let event = match DomEvent::new(ctx, "change", Some(options)) {
                    Ok(event) => ctx.new_instance(event),
                    Err(_) => continue,
                };
                if ctx
                    .set_member(&event, "matches", Value::Bool(matches))
                    .is_err()
                    || ctx
                        .set_member(&event, "media", Value::str(&data.query))
                        .is_err()
                {
                    continue;
                }
                if let Some(event) = JsObject::from_value(event) {
                    let _ = crate::events::dispatch_event(ctx, This(wrapper), event);
                }
            }
        }
    }
    *registry.lists.borrow_mut() = live;
    Ok(())
}

#[derive(Default)]
struct CssStyleSheetInit { base_url: Option<String>, media: String, disabled: bool }
impl<'a> FromArg<'a, JsHost> for CssStyleSheetInit {
    fn from_arg(cx: &'a lumen::embed::ArgCx<'_>, value: &'a Value, at: lumen_bind::Slot) -> Result<Self, Value> {
        if !matches!(value,Value::Obj(_)|Value::Null|Value::Undefined) {
            return Err(JsHost::with_ctx(cx,|ctx|ctx.make_error("TypeError","CSSStyleSheetInit must be a dictionary")));
        }
        let member=|name|JsHost::with_ctx(cx,|ctx|if matches!(value,Value::Null|Value::Undefined){Ok(Value::Undefined)}else{ctx.member_get(value,name)});
        let base=member("baseURL")?;
        let base_url=if matches!(base,Value::Null|Value::Undefined){None}else{Some(<String as FromArg<'_,JsHost>>::from_arg(cx,&base,at)?)};
        let disabled=member("disabled")?;
        let disabled=if matches!(disabled,Value::Undefined){false}else{<bool as FromArg<'_,JsHost>>::from_arg(cx,&disabled,at)?};
        let media=member("media")?;
        let media=if matches!(media,Value::Undefined){String::new()}else {
            JsHost::with_ctx(cx,|ctx| {
                if ctx.instance_data::<DomCssMediaList>(&media).is_some() {
                    ctx.with_instance::<DomCssMediaList,_>(&media,|list|list.media_text()).and_then(|result|result)
                        .map_err(|error|error.to_value(ctx))
                } else { ctx.coerce_string(&media).map(|value|value.to_string()) }
            })?
        };
        Ok(Self{base_url,media,disabled})
    }
}

#[lumen_bind::class(name = "CSSStyleSheet", hint(js(webidl)))]
pub struct DomCssStyleSheet {
    parent_sheet: Value,
    realm: Rc<DomRealm>,
    source: SheetSource,
    owner: Value,
    owner_rule: Value,
    href_value: Option<String>,
    rule_list_wrapper: RefCell<Option<WeakValue>>,
    origin_clean: bool,
    origin_metadata: Rc<RefCell<Arc<[(Arc<str>,bool)]>>>,
}

#[lumen_bind::class(name = "CSSRule", hint(js(webidl)))]
pub struct DomCssRule {
    realm: Rc<DomRealm>,
    source: SheetSource,
    index: Rc<RulePosition>,
    path: Vec<Rc<RulePosition>>,
    keyframe_index: Option<Rc<RulePosition>>,
    owner: Value,
    sheet_owner: Value,
    style_wrapper: RefCell<Option<WeakValue>>,
    rule_list_wrapper: RefCell<Option<WeakValue>>,
}

#[lumen_bind::class(name = "CSSRuleList", hint(js(webidl)))]
pub struct DomCssRuleList {
    realm: Rc<DomRealm>,
    source: SheetSource,
    rule_path: Vec<Rc<RulePosition>>,
    owner: Value,
    sheet_owner: Value,
    keyframes_index: Option<Rc<RulePosition>>,
}

#[lumen_bind::class(name = "StyleSheetList", hint(js(webidl)))]
pub struct DomStyleSheetList {
    realm: Rc<DomRealm>,
    document: NodeId,
    owner: Value,
}

#[lumen_bind::class(name = "CSSOMCollectionIterator")]
pub(crate) struct DomCssomCollectionIterator {
    collection: Value,
    index: Cell<usize>,
}

impl DomCssomCollectionIterator {
    pub(crate) fn new(collection: Value) -> Self {
        Self { collection, index: Cell::new(0) }
    }
}

#[lumen_bind::class(name = "CSSGroupingRule", extends = DomCssRule, hint(js(webidl)))]
pub struct DomCssGroupingRule {
    base: DomCssRule,
}

#[lumen_bind::class(name = "CSSStyleRule", extends = DomCssGroupingRule, hint(js(webidl)))]
pub struct DomCssStyleRule {
    base: DomCssGroupingRule,
}

#[lumen_bind::class(name = "CSSConditionRule", extends = DomCssGroupingRule, hint(js(webidl)))]
pub struct DomCssConditionRule {
    base: DomCssGroupingRule,
}

#[lumen_bind::class(name = "CSSMediaRule", extends = DomCssConditionRule, hint(js(webidl)))]
pub struct DomCssMediaRule {
    base: DomCssConditionRule,
}

#[lumen_bind::class(name = "CSSSupportsRule", extends = DomCssConditionRule, hint(js(webidl)))]
pub struct DomCssSupportsRule {
    base: DomCssConditionRule,
}

#[lumen_bind::class(name = "CSSStartingStyleRule", extends = DomCssGroupingRule, hint(js(webidl)))]
pub struct DomCssStartingStyleRule {
    base: DomCssGroupingRule,
}

#[lumen_bind::methods]
impl DomCssStartingStyleRule {}

#[lumen_bind::class(name = "CSSScopeRule", extends = DomCssGroupingRule, hint(js(webidl)))]
pub struct DomCssScopeRule {
    base: DomCssGroupingRule,
}

#[lumen_bind::class(name = "CSSNestedDeclarations", extends = DomCssRule, hint(js(webidl)))]
pub struct DomCssNestedDeclarations {
    base: DomCssRule,
}

#[lumen_bind::class(name = "CSSPropertyRule", extends = DomCssRule, hint(js(webidl)))]
pub struct DomCssPropertyRule { base: DomCssRule }

#[lumen_bind::class(name = "CSSFontFaceRule", extends = DomCssRule, hint(js(webidl)))]
pub struct DomCssFontFaceRule {
    base: DomCssRule,
}

#[lumen_bind::class(name = "CSSKeyframesRule", extends = DomCssRule, hint(js(webidl)))]
pub struct DomCssKeyframesRule {
    base: DomCssRule,
}

#[lumen_bind::class(name = "CSSKeyframeRule", extends = DomCssRule, hint(js(webidl)))]
pub struct DomCssKeyframeRule {
    base: DomCssRule,
}

#[lumen_bind::class(name = "CSSImportRule", extends = DomCssRule, hint(js(webidl)))]
pub struct DomCssImportRule {
    base: DomCssRule,
    media_wrapper: RefCell<Option<WeakValue>>,
    stylesheet_wrapper: RefCell<Option<WeakValue>>,
}

#[lumen_bind::class(name = "CSSNamespaceRule", extends = DomCssRule, hint(js(webidl)))]
pub struct DomCssNamespaceRule { base: DomCssRule }

#[lumen_bind::methods]
impl DomCssNamespaceRule {
    #[getter]
    fn prefix(&self)->OpResult<String> {
        Ok(css::namespaces::parse_rule(&self.base.current_rule()?.css_text).map_err(css_error)?.prefix.map_or_else(String::new,|prefix|prefix.to_string()))
    }
    #[getter(name="namespaceURI")]
    fn namespace_uri(&self)->OpResult<String> {
        Ok(css::namespaces::parse_rule(&self.base.current_rule()?.css_text).map_err(css_error)?.uri.to_string())
    }
}

#[lumen_bind::class(name = "MediaList", hint(js(webidl)))]
pub struct DomCssMediaList {
    realm: Rc<DomRealm>,
    source: SheetSource,
    index: Option<Rc<RulePosition>>,
}

#[lumen_bind::class(name = "CSSRuleStyleDeclaration", extends = DomStyle, hint(js(webidl)))]
pub struct DomCssRuleStyle {
    base: DomStyle,
    realm: Rc<DomRealm>,
    source: SheetSource,
    index: Rc<RulePosition>,
    path: Vec<Rc<RulePosition>>,
    keyframe_index: Option<Rc<RulePosition>>,
    owner: Value,
}

#[lumen_bind::class(name = "CSSFontFaceDescriptors", extends = DomCssRuleStyle, hint(js(webidl)))]
pub struct DomCssFontFaceDescriptors {
    base: DomCssRuleStyle,
}

use lumen_host::webidl::LegacyNullToEmptyString as CssDescriptorString;

#[lumen_bind::methods]
impl DomCssFontFaceDescriptors {
    #[proto(len)]
    fn length(&self) -> OpResult<usize> {
        self.base.length()
    }
    #[proto(getitem)]
    fn indexed(&self, index: usize) -> OpResult<Value> {
        self.base.indexed(index)
    }
    #[getter(name = "src")]
    fn src(&self) -> OpResult<String> {
        self.base.get_property_value("src")
    }
    #[setter(name = "src", coerce)]
    fn set_src(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("src", value.0, None)
    }
    #[getter(name = "fontFamily")]
    fn font_family(&self) -> OpResult<String> {
        self.base.get_property_value("font-family")
    }
    #[setter(name = "fontFamily", coerce)]
    fn set_font_family(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("font-family", value.0, None)
    }
    #[getter(name = "font-family")]
    fn font_family_css(&self) -> OpResult<String> {
        self.base.get_property_value("font-family")
    }
    #[setter(name = "font-family", coerce)]
    fn set_font_family_css(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("font-family", value.0, None)
    }
    #[getter(name = "fontStyle")]
    fn font_style(&self) -> OpResult<String> {
        self.base.get_property_value("font-style")
    }
    #[setter(name = "fontStyle", coerce)]
    fn set_font_style(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("font-style", value.0, None)
    }
    #[getter(name = "font-style")]
    fn font_style_css(&self) -> OpResult<String> {
        self.base.get_property_value("font-style")
    }
    #[setter(name = "font-style", coerce)]
    fn set_font_style_css(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("font-style", value.0, None)
    }
    #[getter(name = "fontWeight")]
    fn font_weight(&self) -> OpResult<String> {
        self.base.get_property_value("font-weight")
    }
    #[setter(name = "fontWeight", coerce)]
    fn set_font_weight(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("font-weight", value.0, None)
    }
    #[getter(name = "font-weight")]
    fn font_weight_css(&self) -> OpResult<String> {
        self.base.get_property_value("font-weight")
    }
    #[setter(name = "font-weight", coerce)]
    fn set_font_weight_css(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("font-weight", value.0, None)
    }
    #[getter(name = "fontStretch")]
    fn font_stretch(&self) -> OpResult<String> {
        self.base.get_property_value("font-stretch")
    }
    #[setter(name = "fontStretch", coerce)]
    fn set_font_stretch(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("font-stretch", value.0, None)
    }
    #[getter(name = "font-stretch")]
    fn font_stretch_css(&self) -> OpResult<String> {
        self.base.get_property_value("font-stretch")
    }
    #[setter(name = "font-stretch", coerce)]
    fn set_font_stretch_css(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("font-stretch", value.0, None)
    }
    #[getter(name = "fontWidth")]
    fn font_width(&self) -> OpResult<String> {
        self.base.get_property_value("font-width")
    }
    #[setter(name = "fontWidth", coerce)]
    fn set_font_width(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("font-width", value.0, None)
    }
    #[getter(name = "font-width")]
    fn font_width_css(&self) -> OpResult<String> {
        self.base.get_property_value("font-width")
    }
    #[setter(name = "font-width", coerce)]
    fn set_font_width_css(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("font-width", value.0, None)
    }
    #[getter(name = "unicodeRange")]
    fn unicode_range(&self) -> OpResult<String> {
        self.base.get_property_value("unicode-range")
    }
    #[setter(name = "unicodeRange", coerce)]
    fn set_unicode_range(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("unicode-range", value.0, None)
    }
    #[getter(name = "unicode-range")]
    fn unicode_range_css(&self) -> OpResult<String> {
        self.base.get_property_value("unicode-range")
    }
    #[setter(name = "unicode-range", coerce)]
    fn set_unicode_range_css(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("unicode-range", value.0, None)
    }
    #[getter(name = "fontDisplay")]
    fn font_display(&self) -> OpResult<String> {
        self.base.get_property_value("font-display")
    }
    #[setter(name = "fontDisplay", coerce)]
    fn set_font_display(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("font-display", value.0, None)
    }
    #[getter(name = "font-display")]
    fn font_display_css(&self) -> OpResult<String> {
        self.base.get_property_value("font-display")
    }
    #[setter(name = "font-display", coerce)]
    fn set_font_display_css(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("font-display", value.0, None)
    }
    #[getter(name = "fontFeatureSettings")]
    fn font_feature_settings(&self) -> OpResult<String> {
        self.base.get_property_value("font-feature-settings")
    }
    #[setter(name = "fontFeatureSettings", coerce)]
    fn set_font_feature_settings(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base
            .set_property("font-feature-settings", value.0, None)
    }
    #[getter(name = "font-feature-settings")]
    fn font_feature_settings_css(&self) -> OpResult<String> {
        self.base.get_property_value("font-feature-settings")
    }
    #[setter(name = "font-feature-settings", coerce)]
    fn set_font_feature_settings_css(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base
            .set_property("font-feature-settings", value.0, None)
    }
    #[getter(name = "fontVariationSettings")]
    fn font_variation_settings(&self) -> OpResult<String> {
        self.base.get_property_value("font-variation-settings")
    }
    #[setter(name = "fontVariationSettings", coerce)]
    fn set_font_variation_settings(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base
            .set_property("font-variation-settings", value.0, None)
    }
    #[getter(name = "font-variation-settings")]
    fn font_variation_settings_css(&self) -> OpResult<String> {
        self.base.get_property_value("font-variation-settings")
    }
    #[setter(name = "font-variation-settings", coerce)]
    fn set_font_variation_settings_css(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base
            .set_property("font-variation-settings", value.0, None)
    }
    #[getter(name = "sizeAdjust")]
    fn size_adjust(&self) -> OpResult<String> {
        self.base.get_property_value("size-adjust")
    }
    #[setter(name = "sizeAdjust", coerce)]
    fn set_size_adjust(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("size-adjust", value.0, None)
    }
    #[getter(name = "size-adjust")]
    fn size_adjust_css(&self) -> OpResult<String> {
        self.base.get_property_value("size-adjust")
    }
    #[setter(name = "size-adjust", coerce)]
    fn set_size_adjust_css(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("size-adjust", value.0, None)
    }
    #[getter(name = "ascentOverride")]
    fn ascent_override(&self) -> OpResult<String> {
        self.base.get_property_value("ascent-override")
    }
    #[setter(name = "ascentOverride", coerce)]
    fn set_ascent_override(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("ascent-override", value.0, None)
    }
    #[getter(name = "ascent-override")]
    fn ascent_override_css(&self) -> OpResult<String> {
        self.base.get_property_value("ascent-override")
    }
    #[setter(name = "ascent-override", coerce)]
    fn set_ascent_override_css(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("ascent-override", value.0, None)
    }
    #[getter(name = "descentOverride")]
    fn descent_override(&self) -> OpResult<String> {
        self.base.get_property_value("descent-override")
    }
    #[setter(name = "descentOverride", coerce)]
    fn set_descent_override(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("descent-override", value.0, None)
    }
    #[getter(name = "descent-override")]
    fn descent_override_css(&self) -> OpResult<String> {
        self.base.get_property_value("descent-override")
    }
    #[setter(name = "descent-override", coerce)]
    fn set_descent_override_css(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("descent-override", value.0, None)
    }
    #[getter(name = "lineGapOverride")]
    fn line_gap_override(&self) -> OpResult<String> {
        self.base.get_property_value("line-gap-override")
    }
    #[setter(name = "lineGapOverride", coerce)]
    fn set_line_gap_override(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("line-gap-override", value.0, None)
    }
    #[getter(name = "line-gap-override")]
    fn line_gap_override_css(&self) -> OpResult<String> {
        self.base.get_property_value("line-gap-override")
    }
    #[setter(name = "line-gap-override", coerce)]
    fn set_line_gap_override_css(&self, value: CssDescriptorString<'_>) -> OpResult<()> {
        self.base.set_property("line-gap-override", value.0, None)
    }
}

fn source_text(realm: &DomRealm, source: &SheetSource) -> OpResult<String> {
    realm.session.borrow_mut().reclaim_stale_stylesheet_sources();
    with_source_text(realm, source, &mut |text| Ok(text.to_owned()))
}

// Keep source borrows local to pure Rust readers. A cache hit must not copy the
// whole stylesheet merely to inspect one declaration.
fn with_source_text<R>(
    realm: &DomRealm,
    source: &SheetSource,
    read: &mut dyn FnMut(&str) -> OpResult<R>,
) -> OpResult<R> {
    match source {
        SheetSource::Element { lease, .. } => lease.with_text(read),
        SheetSource::Rule { source, position } => {
            if let Some(detached) = position.detached.borrow().as_deref() { return read(detached); }
            with_source_text(realm, source, &mut |text| {
                position.owner.synchronize(text)?;
                let detached = position.detached.borrow();
                read(detached.as_deref().unwrap_or(text))
            })
        }
        SheetSource::Constructed(data) => read(&data.text.borrow()),
        SheetSource::Imported(data) => {
            data.with_graph(realm, &mut |graph| match graph { Some(graph) => read(&graph.text), None => Err(OpError::new("InvalidStateError", "imported stylesheet graph unavailable")) })
        }
    }
}

fn source_sheet(realm: &DomRealm, source: &SheetSource) -> OpResult<CssStyleSheetText> {
    let text = source_text(realm, source)?;
    if text.len() > 1024 * 1024 { return Err(OpError::new("QuotaExceededError", "CSSOM source exceeds limit")); }
    // Source bytes were committed through the parser or are ordinary author
    // text. The first actual rule read validates grammar once, including the
    // detached rule kind/context that a root stylesheet parse would discard.
    let detached = source.detached_position();
    let mut sheet = CssStyleSheetText { text, retained: Vec::new(), boundaries: source.boundary_state().borrow().clone(), nested_context: detached.is_some_and(|position| position.owner.nested_context), scoped_context: detached.is_some_and(|position| position.owner.scoped_context), singleton_declarations: detached.is_some_and(|position| position.nested_declarations.get()), namespaces:detached.map(|position|position.detached_namespaces.borrow().clone()), import_occurrences: None };
    let positions = source.positions();
    if source.detached_position().is_some() || positions.text.borrow().as_deref() == Some(sheet.css_text()) {
        sheet.retained = source.declaration_state().borrow().clone();
        sheet.boundaries = source.boundary_state().borrow().clone();
    }
    let identities = source_identities(realm, source, sheet.css_text());
    let session = realm.session.borrow();
    for retained in session.cssom_rule_overrides() {
        if retained.source_text.as_ref() == sheet.text && identities.iter().any(|(owner, url)|
            *owner == retained.owner && url.as_deref() == retained.source_url.as_deref()) &&
            !sheet.retained.iter().any(|(path, _)| path == &retained.cssom_path) {
            sheet.retained.push((retained.cssom_path.clone(), retained.block.clone()));
        }
    }
    Ok(sheet)
}

fn source_identities(realm: &DomRealm, source: &SheetSource, text: &str) -> Vec<(css::StylesheetIdentity, Option<Arc<str>>)> {
    if source.detached_position().is_some() { return Vec::new(); }
    match source.root() {
        SheetSource::Element { node, lease, .. } => {
            if lease.is_detached() { return Vec::new(); }
            let session = realm.session.borrow();
            let url = session.stylesheet_source(*node).filter(|source| source.text.as_ref() == text)
                .map(|source| source.url.clone());
            vec![(css::StylesheetIdentity::Dom(*node), url)]
        }
        SheetSource::Imported(data) => {
            let Ok(Some(path)) = data.live_path(realm) else { return Vec::new(); };
            let url = realm.session.borrow().imported_stylesheet_url(data.owner, &path).map(Arc::from);
            vec![(css::StylesheetIdentity::Dom(data.owner), url)]
        }
        SheetSource::Constructed(data) => {
            let Some(registry) = data.registry.upgrade() else { return Vec::new(); };
            let adopted = registry.adopted.borrow();
            adopted.iter().flat_map(|(scope, sheets)| sheets.iter().enumerate().filter_map(|(index, sheet)|
                Rc::ptr_eq(&sheet.data, data).then_some((css::StylesheetIdentity::Adopted { scope: *scope, index }, data.base_url.clone()))))
                .collect()
        }
        SheetSource::Rule { .. } => unreachable!(),
    }
}

fn commit_rule_declaration(
    realm: &DomRealm, source: &SheetSource, before: &CssStyleSheetText,
    after: &CssStyleSheetText, path: &[usize], block: Option<Rc<css::DeclarationBlock>>,
) -> OpResult<()> {
    realm.session.borrow_mut().reclaim_stale_stylesheet_sources();
    let identities = source_identities(realm, source, before.css_text());
    let import_owner=if after.import_occurrences.is_some() {identities.iter().find_map(|(identity,_)|match identity {css::StylesheetIdentity::Dom(owner)=>Some(*owner),_=>None})}else{None};
    if let Some(owner)=import_owner {realm.stylesheet_links.prepare_cssom_import_update(owner)?;}

    let mut retained = after.retained.clone();
    retained.retain(|(known, _)| known.as_ref() != path);
    if let Some(block) = block {
        if retained.len() >= 1024 { return Err(OpError::new("QuotaExceededError", "too many retained CSS declarations")); }
        retained.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "CSS declaration storage exhausted"))?;
        retained.push((Arc::from(path), block));
    }
    let retained_bytes = retained.iter().try_fold(0usize, |bytes, (_, block)| bytes.checked_add(block.retained_text_bytes()));
    if retained_bytes.is_none_or(|bytes| bytes > 8 * 1024 * 1024) {
        return Err(OpError::new("QuotaExceededError", "retained CSS declaration budget exceeded"));
    }
    let previous = realm.session.borrow().cssom_rule_overrides().to_vec();
    let previous_topologies = realm.session.borrow().cssom_topology_overrides().to_vec();
    let mut next = previous.clone();
    let mut next_topologies = previous_topologies.clone();
    next.retain(|entry| !identities.iter().any(|(owner, url)|
        entry.owner == *owner && entry.source_url.as_deref() == url.as_deref()));
    next_topologies.retain(|entry| !identities.iter().any(|(owner, url)|
        entry.owner == *owner && entry.source_url.as_deref() == url.as_deref()));
    let text: Arc<str> = Arc::from(after.css_text());
    if !after.boundaries.is_empty() {
        for (owner, url) in &identities {
            next_topologies.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "CSS topology storage exhausted"))?;
            next_topologies.push(css::RuleTopologyOverride { owner: *owner, source_text: text.clone(), source_url: url.clone(), boundaries: after.boundaries.clone() });
        }
    }
    let paths: Vec<_> = retained.iter().map(|(path, _)| path.clone()).collect();
    let offsets = if identities.is_empty() || paths.is_empty() { Vec::new() } else {
        let tree = css::nesting::parse_source_rules_with_boundaries(&text, &[], false, &after.boundaries).map_err(css_error)?;
        paths.iter().map(|path| css::nesting::source_rule_at_path(&tree, path)
            .and_then(|rule| rule.declaration_offset)).collect()
    };
    for ((path, block), offset) in retained.iter().zip(offsets) {
        let Some(declaration_offset) = offset else { continue; };
        for (owner, url) in &identities {
            if next.len() >= 1024 { return Err(OpError::new("QuotaExceededError", "too many retained CSS declarations")); }
            next.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "CSS declaration storage exhausted"))?;
            next.push(css::RuleDeclarationOverride { owner: *owner, source_text: text.clone(), source_url: url.clone(),
                declaration_offset, cssom_path: path.clone(), block: block.clone() });
        }
    }
    if !identities.is_empty() {
        realm.session.borrow_mut().stage_cssom_state(next, next_topologies)
            .map_err(|error| OpError::new("InvalidStateError", format!("CSS declaration commit failed: {error:?}")))?;
    }
    let positions = source.positions();
    let removed_imports = match retain_removed_import_graphs(realm, source, before, after) {
        Ok(removed) => removed,
        Err(error) => {
            if !identities.is_empty() { let _ = realm.session.borrow_mut().stage_cssom_state(previous, previous_topologies); }
            return Err(error);
        }
    };
    let previous_local = core::mem::replace(&mut *source.declaration_state().borrow_mut(), retained);
    let previous_boundaries = core::mem::replace(&mut *source.boundary_state().borrow_mut(), after.boundaries.clone());
    if source.detached_position().is_none() { positions.remember(after.css_text())?; }
    if let Err(error) = write_source_text_with_import_map(realm, source, after.css_text(), after.import_occurrences.as_deref()) {
        *source.declaration_state().borrow_mut() = previous_local;
        *source.boundary_state().borrow_mut() = previous_boundaries;
        if source.detached_position().is_none() { positions.remember(before.css_text())?; }
        if !identities.is_empty() { let _ = realm.session.borrow_mut().stage_cssom_state(previous, previous_topologies); }
        for import in removed_imports { import.restore_graph(realm)?; }
        return Err(error);
    }
    if let Some(owner)=import_owner {realm.stylesheet_links.cssom_imports_changed(owner);}
    Ok(())
}

fn retain_removed_import_graphs(realm: &DomRealm, source: &SheetSource, before: &CssStyleSheetText, after: &CssStyleSheetText) -> OpResult<Vec<Rc<ImportedSheetData>>> {
    let Some(mapping) = &after.import_occurrences else { return Ok(Vec::new()); };
    let rules = before.css_rules().map_err(css_error)?;
    let positions = source.positions();
    let mut removed = Vec::new();
    let mut surviving = [false; css::MAX_CSS_GRAPH_IMPORTS];
    for &ordinal in mapping.iter().flatten() {
        let Some(slot) = surviving.get_mut(ordinal) else { return Err(OpError::new("InvalidStateError", "invalid import occurrence map")); };
        *slot = true;
    }
    removed.try_reserve_exact(positions.live.borrow().len()).map_err(|_| OpError::new("QuotaExceededError", "import retention allocation failed"))?;
    for position in positions.live.borrow().iter().filter_map(Weak::upgrade) {
        if position.detached.borrow().is_some() { continue; }
        let Some(data) = position.imported_sheet.borrow().clone() else { continue; };
        let index = position.get();
        if index >= rules.len() { continue; }
        let ordinal = rules[..index].iter().filter(|rule| starts_css_at_rule(&rule.css_text, "@import")).count();
        if surviving.get(ordinal).copied().unwrap_or(false) { continue; }
        match data.take_graph(realm) {
            Ok(Some(graph)) => { *data.detached_graph.borrow_mut() = Some(graph); removed.push(data); },
            Ok(None) => {},
            Err(error) => { for data in removed { data.restore_graph(realm)?; } return Err(error); },
        }
    }
    Ok(removed)
}

fn keyframe_text(rule: &css::KeyframesRuleText) -> String {
    rule.rules
        .iter()
        .map(|frame| frame.css_text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn source_keyframe(
    realm: &DomRealm,
    source: &SheetSource,
    rule_index: &Rc<RulePosition>,
    frame: &Rc<RulePosition>,
) -> OpResult<css::KeyframeRuleText> {
    let sheet = source_sheet(realm, source)?;
    let keyframes = sheet.keyframes_rule(rule_index.get()).map_err(css_error)?;
    frame.owner.synchronize(&keyframe_text(&keyframes))?;
    if let Some(text) = frame.detached.borrow().as_ref() {
        return css::parse_keyframe_rule(text).map_err(css_error);
    }
    keyframes
        .rules
        .get(frame.get())
        .cloned()
        .ok_or_else(|| OpError::new("InvalidStateError", "keyframe rule was removed"))
}

fn write_keyframe_style(
    realm: &DomRealm,
    source: &SheetSource,
    rule_index: &Rc<RulePosition>,
    frame: &Rc<RulePosition>,
    declarations: &str,
) -> OpResult<()> {
    let current = source_keyframe(realm, source, rule_index, frame)?;
    let replacement =
        css::parse_keyframe_rule(&format!("{} {{ {} }}", current.key_text, declarations))
            .map_err(css_error)?;
    if frame.detached.borrow().is_some() {
        *frame.detached.borrow_mut() = Some(replacement.css_text);
        return Ok(());
    }
    let mut sheet = source_sheet(realm, source)?;
    let before = sheet.clone();
    sheet
        .set_keyframe_declarations(rule_index.get(), frame.get(), declarations)
        .map_err(css_error)?;
    commit_rule_declaration(realm, source, &before, &sheet, &[], None)?;
    frame.owner.remember(&keyframe_text(
        &sheet.keyframes_rule(rule_index.get()).map_err(css_error)?,
    ))?;
    Ok(())
}

fn source_import(
    realm: &DomRealm,
    source: &SheetSource,
    index: &Rc<RulePosition>,
) -> OpResult<css::ImportRule> {
    let rules = source_sheet(realm, source)?
        .css_rules()
        .map_err(css_error)?;
    let rule = rules
        .get(index.get())
        .ok_or_else(|| OpError::new("InvalidStateError", "stylesheet rule was removed"))?;
    css::imports(&rule.css_text)
        .map_err(css_error)?
        .into_iter()
        .next()
        .ok_or_else(|| OpError::new("InvalidStateError", "import rule is no longer valid"))
}

fn source_dom_node(realm: &DomRealm, source: &SheetSource) -> NodeId {
    match source {
        SheetSource::Element { node, .. } => *node,
        SheetSource::Rule { source, .. } => source_dom_node(realm, source),
        SheetSource::Constructed(_) => realm.session.borrow().document().root(),
        SheetSource::Imported(data) => data.owner,
    }
}

fn sync_adopted(registry: &MediaQueryRegistry, realm: &DomRealm) -> OpResult<()> {
    realm.session.borrow_mut().reclaim_stale_stylesheet_sources();
    let adopted = registry.borrowed_adoptions()?;
    let previous = realm.session.borrow().cssom_rule_overrides().to_vec();
    let previous_topologies = realm.session.borrow().cssom_topology_overrides().to_vec();
    let mut overrides: Vec<_> = previous.iter().filter(|entry| matches!(entry.owner, css::StylesheetIdentity::Dom(_))).cloned().collect();
    let mut topologies: Vec<_> = previous_topologies.iter().filter(|entry| matches!(entry.owner, css::StylesheetIdentity::Dom(_))).cloned().collect();
    let mut source_texts = HashMap::<*const ConstructedSheetData, Arc<str>>::new();
    for (scope, sheets) in registry.adopted.borrow().iter() {
        for (index, sheet) in sheets.iter().enumerate() {
            let text = sheet.data.text.borrow();
            let positions = &sheet.data.positions;
            if positions.text.borrow().as_deref() != Some(text.as_str()) { continue; }
            let declarations = positions.declarations.borrow();
            let boundaries = positions.boundaries.borrow();
            if declarations.is_empty() && boundaries.is_empty() { continue; }
            let paths: Vec<_> = declarations.iter().map(|(path, _)| path.clone()).collect();
            let offsets: Vec<_> = if declarations.is_empty() { Vec::new() } else {
                let tree = css::nesting::parse_source_rules_with_boundaries(&text, &[], false, &boundaries).map_err(css_error)?;
                paths.iter().map(|path| css::nesting::source_rule_at_path(&tree, path).and_then(|rule| rule.declaration_offset)).collect()
            };
            let source_text = source_texts.entry(Rc::as_ptr(&sheet.data))
                .or_insert_with(|| Arc::from(text.as_str())).clone();
            if !boundaries.is_empty() {
                topologies.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "CSS topology storage exhausted"))?;
                topologies.push(css::RuleTopologyOverride { owner: css::StylesheetIdentity::Adopted { scope: *scope, index }, source_text: source_text.clone(), source_url: sheet.data.base_url.clone(), boundaries: boundaries.clone() });
            }
            for ((path, block), offset) in declarations.iter().zip(offsets) {
                let Some(declaration_offset) = offset else { continue; };
                if overrides.len() >= 1024 { return Err(OpError::new("QuotaExceededError", "too many retained CSS declarations")); }
                overrides.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "CSS declaration storage exhausted"))?;
                overrides.push(css::RuleDeclarationOverride { owner: css::StylesheetIdentity::Adopted { scope: *scope, index },
                    source_text: source_text.clone(), source_url: sheet.data.base_url.clone(), declaration_offset, cssom_path: path.clone(), block: block.clone() });
            }
        }
    }
    let mut session = realm.session.borrow_mut();
    session.stage_cssom_state(overrides, topologies).map_err(|error|
        OpError::new("SyntaxError", format!("adopted stylesheet declarations failed: {error:?}")))?;
    if let Err(error)=session.replace_adopted_stylesheet_sources(adopted) {
        let _=session.stage_cssom_state(previous,previous_topologies);
        return Err(OpError::new("SyntaxError",format!("adopted stylesheet failed: {error:?}")));
    }
    Ok(())
}

const ADOPTED_ARRAY_SLOT:&str="#lumen_adopted_style_sheets\u{1}array";
const ADOPTED_HANDLER_SLOT:&str="#lumen_adopted_style_sheets\u{1}handler";
struct AdoptedArrayIntrinsics {
    proxy:WeakValue,
    reflect:HashMap<&'static str,WeakValue>,
}
impl AdoptedArrayIntrinsics {
    fn capture(ctx:&mut Ctx)->OpResult<Self> {
        let global=ctx.global_object();
        let proxy=ctx.member_get(&global,"Proxy").map_err(OpError::thrown)?;
        let proxy=crate::realm_services::capture_realm_value(ctx,proxy)?;
        let object=ctx.member_get(&global,"Reflect").map_err(OpError::thrown)?;
        let mut reflect=HashMap::new();
        for name in ["get","set","deleteProperty","has","ownKeys","getOwnPropertyDescriptor","defineProperty"] {
            let function=ctx.member_get(&object,name).map_err(OpError::thrown)?;
            reflect.insert(name,crate::realm_services::capture_realm_value(ctx,function)?);
        }
        Ok(Self{proxy,reflect})
    }
    fn call(&self,ctx:&mut Ctx,name:&str,args:&[Value])->OpResult<Value> {
        let function=self.reflect.get(name).and_then(WeakValue::upgrade)
            .ok_or_else(||OpError::new("InvalidStateError","observable array intrinsic is unavailable"))?;
        ctx.invoke(function,Value::Undefined,args).map_err(OpError::thrown)
    }
}
#[lumen_bind::class(name="AdoptedStyleSheetArrayHandler")]
struct AdoptedStyleSheetArrayHandler {
    location:RefCell<AdoptedArrayLocation>,
    owner:Value,
}
#[derive(Clone)]
struct AdoptedArrayLocation {
    registry:Rc<MediaQueryRegistry>,
    realm:Rc<DomRealm>,
    scope:Option<NodeId>,
    document:NodeId,
}
impl lumen::embed::NativeIdentityOwner for AdoptedStyleSheetArrayHandler {
    const TRACES_NATIVE_VALUES:bool=true;
    fn trace_native_identities(&self,_:u64,_:&mut dyn FnMut(&Value)){}
    fn trace_native_values(&self,visit:&mut dyn FnMut(&Value)) {
        visit(&self.owner);
        let location=self.location.borrow();
        if let Some((_,sheets))=location.registry.adopted.borrow().iter().find(|(scope,_)|*scope==location.scope) {
            for sheet in sheets {visit(&sheet.value);}
        };
    }
}
fn adopted_array_index(key:&Value)->Option<usize> {
    if let Value::Str(key)=key {lumen::embed::array_index(key.as_str()).map(|index|index as usize)}else{None}
}
fn adopted_length_key(key:&Value)->bool {matches!(key,Value::Str(key) if key.as_str()=="length")}
fn adopted_sheet(ctx:&mut Ctx,realm:&Rc<DomRealm>,document:NodeId,value:Value)->OpResult<AdoptedSheet> {
    let source=ctx.with_instance::<DomCssStyleSheet,_>(&value,|sheet|sheet.source.clone())?;
    let SheetSource::Constructed(data)=source else{return Err(crate::error_reporting::dom_exception(ctx,"NotAllowedError","only constructed stylesheets can be adopted"));};
    if data.constructor_document!=document || data.realm.upgrade().is_none_or(|owner|!Rc::ptr_eq(&owner,realm)) {
        return Err(crate::error_reporting::dom_exception(ctx,"NotAllowedError","stylesheet was constructed in another document"));
    }
    Ok(AdoptedSheet{data,value})
}
impl AdoptedStyleSheetArrayHandler {
    fn retarget(&self,ctx:&mut Ctx)->OpResult<()> {
        let (realm,node)=ctx.with_instance::<DomNode,_>(&self.owner,|node|node.realm.resolve_adopted_node(node.id))?;
        let (document,inert,scope)={let session=realm.session.borrow();let tree=session.document();
            let document=tree.node_document(node).map_err(dom_error)?;
            (document,tree.is_template_owner_document(document),
             (!matches!(tree.kind(node),Ok(NodeKind::Document))).then_some(node))};
        // Moving into the associated inert template document preserves the
        // sheet list and its observable object. It becomes active again when
        // moved back to the constructor document.
        if inert {return Ok(());}
        let old=self.location.borrow().clone();
        if Rc::ptr_eq(&old.realm,&realm) && old.scope==scope && old.document==document {return Ok(());}
        let registry=adopted_registry(ctx,&realm)?;
        let position=old.registry.adopted.borrow().iter().position(|(candidate,_)|*candidate==old.scope);
        if let Some(position)=position {old.registry.adopted.borrow_mut().remove(position);}
        {let mut adopted=registry.adopted.borrow_mut();
            if let Some((_,sheets))=adopted.iter_mut().find(|(candidate,_)|*candidate==scope) {sheets.clear();}
            else {adopted.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","adopted scope storage exhausted"))?;adopted.push((scope,Vec::new()));}}
        *self.location.borrow_mut()=AdoptedArrayLocation{registry:registry.clone(),realm:realm.clone(),scope,document};
        sync_adopted(&old.registry,&old.realm)?;
        sync_adopted(&registry,&realm)?;
        Ok(())
    }

    fn len(&self)->usize {let location=self.location.borrow();let length=location.registry.adopted.borrow().iter().find(|(scope,_)|*scope==location.scope).map_or(0,|(_,sheets)|sheets.len());length}
    fn item(&self,index:usize)->Option<Value> {let location=self.location.borrow();let value=location.registry.adopted.borrow().iter().find(|(scope,_)|*scope==location.scope).and_then(|(_,sheets)|sheets.get(index)).map(|sheet|sheet.value.clone());value}
    fn reflect(&self,ctx:&mut Ctx,name:&str,args:&[Value])->OpResult<Value> {let registry=self.location.borrow().registry.clone();registry.observable.call(ctx,name,args)}
    fn set_index(&self,ctx:&mut Ctx,index:usize,value:Value)->OpResult<bool> {
        let location=self.location.borrow().clone();
        let length=self.len();if index>length{return Ok(false);}
        if index>=4096{return Err(OpError::new("QuotaExceededError","too many adopted stylesheets"));}
        // The preserved backing list can temporarily belong to an inert
        // template document. Validate new values against the actual node
        // document, rather than the list's retained constructor location.
        let (owner_realm,owner_node)=ctx.with_instance::<DomNode,_>(&self.owner,|owner|owner.realm.resolve_adopted_node(owner.id))?;
        let owner_document=owner_realm.session.borrow().document().node_document(owner_node).map_err(dom_error)?;
        let sheet=adopted_sheet(ctx,&owner_realm,owner_document,value)?;
        let previous={let mut adopted=location.registry.adopted.borrow_mut();
            let sheets=&mut adopted.iter_mut().find(|(scope,_)|*scope==location.scope).expect("observable scope initialized").1;
            if index==length {sheets.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","adopted stylesheet storage exhausted"))?;sheets.push(sheet);None}
            else{Some(core::mem::replace(&mut sheets[index],sheet))}};
        if let Err(error)=sync_adopted(&location.registry,&location.realm) {
            let mut adopted=location.registry.adopted.borrow_mut();let sheets=&mut adopted.iter_mut().find(|(scope,_)|*scope==location.scope).expect("observable scope initialized").1;
            if let Some(previous)=previous {sheets[index]=previous;}else{sheets.pop();}return Err(error);
        }
        Ok(true)
    }
    fn shrink(&self,length:usize)->OpResult<bool> {
        let location=self.location.borrow().clone();
        if length>self.len(){return Ok(false);}
        {let mut adopted=location.registry.adopted.borrow_mut();adopted.iter_mut().find(|(scope,_)|*scope==location.scope).expect("observable scope initialized").1.truncate(length);}
        sync_adopted(&location.registry,&location.realm)?;Ok(true)
    }
    fn set_length(&self,ctx:&mut Ctx,value:&Value)->OpResult<bool> {
        let uint=ctx.coerce_number(value).map_err(OpError::thrown)?;
        let uint=if !uint.is_finite() || uint==0.0 {0}else{uint.trunc().rem_euclid(4294967296.0) as u32};
        let number=ctx.coerce_number(value).map_err(OpError::thrown)?;
        if f64::from(uint)!=number{return Err(OpError::new("RangeError","invalid observable array length"));}
        self.shrink(uint as usize)
    }
}
#[lumen_bind::methods]
impl AdoptedStyleSheetArrayHandler {
    fn get(&self,ctx:&mut Ctx,target:Value,key:Value,receiver:Value)->OpResult<Value> {
        self.retarget(ctx)?;
        if adopted_length_key(&key){return Ok(Value::Num(self.len() as f64));}
        if let Some(index)=adopted_array_index(&key){return Ok(self.item(index).unwrap_or(Value::Undefined));}
        self.reflect(ctx,"get",&[target,key,receiver])
    }
    fn set(&self,ctx:&mut Ctx,target:Value,key:Value,value:Value,receiver:Value)->OpResult<Value> {
        self.retarget(ctx)?;
        if adopted_length_key(&key){return self.set_length(ctx,&value).map(Value::Bool);}
        if let Some(index)=adopted_array_index(&key){return self.set_index(ctx,index,value).map(Value::Bool);}
        self.reflect(ctx,"set",&[target,key,value,receiver])
    }
    #[method(name="deleteProperty")]
    fn delete_property(&self,ctx:&mut Ctx,target:Value,key:Value)->OpResult<Value> {
        self.retarget(ctx)?;
        if adopted_length_key(&key){return Ok(Value::Bool(false));}
        if let Some(index)=adopted_array_index(&key){let length=self.len();return Ok(Value::Bool(length>0 && index==length-1 && self.shrink(length-1)?));}
        self.reflect(ctx,"deleteProperty",&[target,key])
    }
    fn has(&self,ctx:&mut Ctx,target:Value,key:Value)->OpResult<Value> {
        self.retarget(ctx)?;
        if adopted_length_key(&key){return Ok(Value::Bool(true));}
        if let Some(index)=adopted_array_index(&key){return Ok(Value::Bool(index<self.len()));}
        self.reflect(ctx,"has",&[target,key])
    }
    #[method(name="getOwnPropertyDescriptor")]
    fn get_own_property_descriptor(&self,ctx:&mut Ctx,target:Value,key:Value)->OpResult<Value> {
        self.retarget(ctx)?;
        let value=if adopted_length_key(&key){Some((Value::Num(self.len() as f64),false,false))}
            else if let Some(index)=adopted_array_index(&key){self.item(index).map(|value|(value,true,true))}else{return self.reflect(ctx,"getOwnPropertyDescriptor",&[target,key]);};
        let Some((value,configurable,enumerable))=value else{return Ok(Value::Undefined);};
        let descriptor=ctx.new_object_with_proto(&Value::Null);
        for (name,value) in [("value",value),("configurable",Value::Bool(configurable)),("enumerable",Value::Bool(enumerable)),("writable",Value::Bool(true))] {ctx.member_set(&descriptor,name,value).map_err(OpError::thrown)?;}
        Ok(descriptor)
    }
    #[method(name="ownKeys")]
    fn own_keys(&self,ctx:&mut Ctx,target:Value)->OpResult<Value> {
        self.retarget(ctx)?;
        let mut keys=Vec::new();keys.try_reserve(self.len()+1).map_err(|_|OpError::new("QuotaExceededError","observable array keys exhausted"))?;
        for index in 0..self.len(){keys.push(Value::from_string(index.to_string()));}
        keys.extend(ctx.reflect_own_keys(&target).map_err(OpError::thrown)?);Ok(ctx.make_array(keys))
    }
    #[method(name="preventExtensions")]
    fn prevent_extensions(&self,_target:Value)->bool {false}
    #[method(name="defineProperty")]
    fn define_property(&self,ctx:&mut Ctx,target:Value,key:Value,descriptor:Value)->OpResult<Value> {
        self.retarget(ctx)?;
        let length=adopted_length_key(&key);let index=adopted_array_index(&key);
        if !length && index.is_none(){return self.reflect(ctx,"defineProperty",&[target,key,descriptor]);}
        let keys=ctx.reflect_own_keys(&descriptor).map_err(OpError::thrown)?;
        let contains=|name:&str|keys.iter().any(|key|matches!(key,Value::Str(key) if key.as_str()==name));
        if contains("get") || contains("set"){return Ok(Value::Bool(false));}
        for (name,forbidden) in [("configurable",length),("enumerable",length),("writable",false)] {
            if contains(name) && matches!(ctx.member_get(&descriptor,name).map_err(OpError::thrown)?,Value::Bool(value) if value==forbidden){return Ok(Value::Bool(false));}
        }
        if !contains("value"){return Ok(Value::Bool(true));}
        let value=ctx.member_get(&descriptor,"value").map_err(OpError::thrown)?;
        if length {self.set_length(ctx,&value).map(Value::Bool)}else{self.set_index(ctx,index.expect("indexed descriptor"),value).map(Value::Bool)}
    }
}

pub fn adopted_stylesheets(ctx:&mut Ctx,realm:&Rc<DomRealm>,scope:Option<NodeId>)->OpResult<Value> {
    let registry=adopted_registry(ctx,realm)?;
    let root=realm.session.borrow().document().root();let owner=realm.wrap(ctx,scope.unwrap_or(root));
    if let Some(array)=ctx.native_private_value_slot(&owner,ADOPTED_ARRAY_SLOT){return Ok(array);}
    {let mut adopted=registry.adopted.borrow_mut();if !adopted.iter().any(|(candidate,_)|*candidate==scope){adopted.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","adopted scope storage exhausted"))?;adopted.push((scope,Vec::new()));}}
    let document=realm.session.borrow().document().node_document(scope.unwrap_or(root)).map_err(dom_error)?;
    let handler=ctx.new_instance(AdoptedStyleSheetArrayHandler{location:RefCell::new(AdoptedArrayLocation{registry:registry.clone(),realm:realm.clone(),scope,document}),owner:owner.clone()});
    ctx.set_native_identity_owner::<AdoptedStyleSheetArrayHandler>(&handler)?;
    let target=ctx.make_array(Vec::new());
    let constructor=registry.observable.proxy.upgrade().ok_or_else(||OpError::new("InvalidStateError","observable array Proxy is unavailable"))?;
    let array=ctx.construct_value(constructor,&[target,handler.clone()]).map_err(OpError::thrown)?;
    ctx.define_native_internal_value_slot(&owner,ADOPTED_HANDLER_SLOT,handler).map_err(OpError::thrown)?;
    ctx.define_native_internal_value_slot(&owner,ADOPTED_ARRAY_SLOT,array.clone()).map_err(OpError::thrown)?;
    Ok(array)
}
pub(crate) fn retarget_adopted_stylesheets(ctx:&mut Ctx,owner:&Value)->OpResult<()> {
    if let Some(handler)=ctx.native_private_value_slot(owner,ADOPTED_HANDLER_SLOT) {
        let handler=ctx.with_instance::<AdoptedStyleSheetArrayHandler,_>(&handler,|handler|
            (handler.location.borrow().clone(),handler.owner.clone()))?;
        // The temporary view uses the existing canonical storage and owner;
        // no second observable object or sheet list is created.
        let view=AdoptedStyleSheetArrayHandler{location:RefCell::new(handler.0),owner:handler.1};
        view.retarget(ctx)?;
        let location=view.location.into_inner();
        let handler=ctx.native_private_value_slot(owner,ADOPTED_HANDLER_SLOT).expect("adopted handler retained by owner");
        ctx.with_instance::<AdoptedStyleSheetArrayHandler,_>(&handler,|handler|*handler.location.borrow_mut()=location)?;
    }
    Ok(())
}
pub fn set_adopted_stylesheets(ctx:&mut Ctx,realm:&Rc<DomRealm>,scope:Option<NodeId>,value:Value)->OpResult<()> {
    let values=ctx.convert_iterable(&value,4096,|ctx,value|{ctx.with_instance::<DomCssStyleSheet,_>(&value,|_|())?;Ok(value)})?;
    let array=adopted_stylesheets(ctx,realm,scope)?;
    // Web IDL conversion precedes all mutations; then the observable setter
    // deletes the old list and invokes the CSSOM set algorithm in list order.
    ctx.member_set(&array,"length",Value::Num(0.0)).map_err(OpError::thrown)?;
    for (index,value) in values.into_iter().enumerate(){ctx.member_set(&array,&index.to_string(),value).map_err(OpError::thrown)?;}
    Ok(())
}

impl MediaQueryRegistry {
    fn borrowed_adoptions(&self) -> OpResult<Vec<(Option<NodeId>, Vec<lumen_html::session::AdoptedStylesheetSource>)>> {
        use lumen_html::session::AdoptedStylesheetSource;
        let fail=||OpError::new("QuotaExceededError","adopted stylesheet snapshot budget exceeded");
        let adopted=self.adopted.borrow();
        // Admit the whole operation before duplicating source text. Duplicate
        // occurrences require independent cascade positions and are all charged.
        let mut bytes=adopted.len().checked_mul(core::mem::size_of::<(Option<NodeId>,Vec<AdoptedStylesheetSource>)>()).ok_or_else(fail)?;
        for (_,sheets) in adopted.iter() {
            bytes=bytes.checked_add(sheets.len().checked_mul(core::mem::size_of::<AdoptedStylesheetSource>()).ok_or_else(fail)?).ok_or_else(fail)?;
            for sheet in sheets {
                bytes=bytes.checked_add(sheet.data.text.borrow().len())
                    .and_then(|bytes|bytes.checked_add(sheet.data.media.borrow().len()))
                    .and_then(|bytes|bytes.checked_add(4*core::mem::size_of::<usize>())).ok_or_else(fail)?;
            }
            if bytes>lumen_html::html::MAX_HTML_BYTES {return Err(fail());}
        }
        let mut scopes=Vec::new();scopes.try_reserve_exact(adopted.len()).map_err(|_|fail())?;
        for (scope,sheets) in adopted.iter() {
            let mut sources=Vec::new();sources.try_reserve_exact(sheets.len()).map_err(|_|fail())?;
            for sheet in sheets {
                let text=sheet.data.text.borrow();
                let mut copied=String::new();copied.try_reserve_exact(text.len()).map_err(|_|fail())?;copied.push_str(&text);
                sources.push(AdoptedStylesheetSource{text:copied,base_url:sheet.data.base_url.clone(),media:Arc::from(sheet.data.media.borrow().as_str()),disabled:sheet.data.disabled.get()});
            }
            scopes.push((*scope,sources));
        }
        Ok(scopes)
    }

}

fn registry_for_realm(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> Option<Rc<MediaQueryRegistry>> {
    if let Some(registry)=realm.cssom_registry.borrow().upgrade(){return Some(registry);}
    let registry = RealmServices::<MediaQueryRegistry>::current(ctx)?;
    registry
        .realm
        .upgrade()
        .is_some_and(|owner| Rc::ptr_eq(&owner, realm))
        .then_some(registry)
}

fn adopted_registry(ctx:&mut Ctx,realm:&Rc<DomRealm>)->OpResult<Rc<MediaQueryRegistry>> {
    if let Some(registry)=registry_for_realm(ctx,realm){return Ok(registry);}
    // A document constructed without a Window still has the same CSSOM
    // storage. Its observable handler owns it; realm metadata stays weak.
    let registry=Rc::new(MediaQueryRegistry {
        read_cache:Rc::default(),realm:Rc::downgrade(realm),lists:RefCell::new(Vec::new()),
        sheets:RefCell::new(HashMap::new()),sheet_positions:RefCell::new(HashMap::new()),
        adopted:RefCell::new(Vec::new()),observable:AdoptedArrayIntrinsics::capture(ctx)?,
    });
    *realm.cssom_registry.borrow_mut()=Rc::downgrade(&registry);
    Ok(registry)
}

fn write_source_text_with_import_map(realm: &DomRealm, source: &SheetSource, text: &str, imports: Option<&[Option<usize>]>) -> OpResult<()> {
    match source {
        SheetSource::Element { node, lease, .. } => realm.session.borrow_mut().replace_cssom_stylesheet_text(*node, lease, text, imports)
            .map_err(|error| OpError::new("InvalidStateError", format!("stylesheet text commit failed: {error:?}"))),
        SheetSource::Rule { source, position } => {
            if position.detached.borrow().is_some() {
                *position.detached.borrow_mut() = Some(text.to_owned());
                return Ok(());
            }
            write_source_text_with_import_map(realm, source, text, imports)?;
            position.owner.remember(text)?;
            Ok(())
        }
        SheetSource::Constructed(data) => {
            let previous=core::mem::replace(&mut *data.text.borrow_mut(),text.to_owned());
            if let Some(registry) = data.registry.upgrade() {
                let adopted = registry.adopted.borrow().iter()
                    .any(|(_, sheets)| sheets.iter().any(|sheet| Rc::ptr_eq(&sheet.data, data)));
                if adopted {if let Err(error)=sync_adopted(&registry,realm) {*data.text.borrow_mut()=previous;return Err(error);}}
            }
            Ok(())
        }
        SheetSource::Imported(data) => {
            if let Some(path) = data.live_path(realm)? {
                realm.session.borrow_mut().replace_imported_stylesheet_text_with_import_map(data.owner, &path, text, imports)
                    .map_err(|error| OpError::new("InvalidStateError", format!("imported stylesheet replacement failed: {error:?}")))?;
            } else {
                let environment = realm.session.borrow().media_environment();
                data.with_local_graph_mut(realm, &mut |graph| {
                    let edit = css::prepare_stylesheet_text_edit_with_import_map(graph, Arc::from(text), imports).map_err(css_error)?;
                    if let Err(error) = css::parse_graph(graph, environment) { edit.rollback(graph); return Err(css_error(error)); }
                    Ok(())
                })?;
                if let (Some(lease), Some(imports)) = (data.lease.borrow().as_ref(), imports) { lease.rebase_imports(imports); }
            }
            Ok(())
        }
    }
}

/// Both HTMLStyleElement and SVGStyleElement use the same associated sheet.
pub(crate) fn style_element_disabled(ctx:&mut Ctx,realm:&Rc<DomRealm>,node:NodeId)->OpResult<bool> {
    let (owner,node)=realm.resolve_adopted_node(node);
    if !owner.ensure_inline_stylesheet(ctx,node)? {return Ok(false)}
    let disabled=owner.session.borrow().stylesheet_disabled(node);Ok(disabled)
}
pub(crate) fn set_style_element_disabled(ctx:&mut Ctx,realm:&Rc<DomRealm>,node:NodeId,value:bool)->OpResult<()> {
    let (owner,node)=realm.resolve_adopted_node(node);
    if !owner.ensure_inline_stylesheet(ctx,node)? {return Ok(())}
    owner.stylesheet_links.set_sheet_disabled(node,value)?;
    let result=owner.session.borrow_mut().set_stylesheet_disabled(node,value)
        .map_err(|error|OpError::new("InvalidStateError",format!("inline stylesheet disable: {error:?}")));result
}

/// Creates the live stylesheet object for a `<style>` node. The DOM adapter
/// uses this from `HTMLStyleElement.sheet` and caches the returned instance on
/// the element wrapper.
pub fn style_element_sheet(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    owner: Value,
) -> OpResult<Value> {
    let (actual,node)=realm.resolve_adopted_node(node);
    let realm=&actual;
    let instruction=lumen_html::xml_stylesheet::is_candidate(realm.session.borrow().document(),node);
    if instruction {realm.queue_stylesheet_tasks(ctx)?;}
    let link=instruction || matches!(realm.session.borrow().document().kind(node),Ok(NodeKind::Element {name,namespace:Namespace::Html,..}) if name=="link");
    let link_state=realm.session.borrow().link_stylesheet_state(node);
    if link && link_state.is_none() {return Ok(Value::Null)}
    if !link && !crate::stylesheet_loading::is_style(realm.session.borrow().document(),node)
    {
        return Err(OpError::new(
            "TypeError",
            "sheet requires a stylesheet owner",
        ));
    }
    if !link && !realm.ensure_inline_stylesheet(ctx,node)? {return Ok(Value::Null)}
    let token = realm.session.borrow_mut().stylesheet_change_token(node)
        .map_err(|error| OpError::new("QuotaExceededError", format!("stylesheet owner tracking failed: {error:?}")))?;
    let epoch = token.get();
    let current = |positions: &RulePositions| positions.source_epoch.borrow().as_ref().is_some_and(|(clock, seen)| Rc::ptr_eq(clock, &token) && *seen == epoch);
    let registry = adopted_registry(ctx, realm)?;
    {
        if registry.sheet_positions.borrow().get(&node).and_then(Weak::upgrade).is_some_and(|positions| current(&positions)) {
            if let Some(sheet) = registry.sheets.borrow().get(&node).and_then(WeakValue::upgrade) {
                return Ok(sheet);
            }
        }
    }
    let positions = {
        let mut positions = registry.sheet_positions.borrow_mut();
        positions.retain(|_, position| position.strong_count() != 0);
        let state = positions
            .get(&node)
            .and_then(Weak::upgrade)
            .filter(|positions| current(positions))
            .unwrap_or_else(|| Rc::new(RulePositions { _registry: Some(registry.clone()), read_cache: registry.read_cache.clone(), ..Default::default() }));
        positions.insert(node, Rc::downgrade(&state));
        state
    };
    if positions.source_epoch.borrow().is_none() {
        let seen = token.get();
        *positions.source_epoch.borrow_mut() = Some((token, seen));
    }
    let lease = realm.session.borrow_mut().stylesheet_root_lease(node)
        .map_err(|error| OpError::new("QuotaExceededError", format!("stylesheet source tracking failed: {error:?}")))?;
    *positions.origin_metadata.borrow_mut()=realm.stylesheet_links.origin_metadata(node);
    let origin_metadata=positions.origin_metadata.clone();
    let sheet = ctx.new_instance(DomCssStyleSheet {
        parent_sheet: Value::Null,
        realm: realm.clone(),
        source: SheetSource::Element { node, positions, lease },
        owner,
        owner_rule: Value::Null,
        href_value: if link {realm.stylesheet_links.location(node)}else{None},
        rule_list_wrapper: RefCell::new(None),
        origin_clean:link_state.is_none_or(|state|state.origin_clean),
        origin_metadata,
    });
    {
        if let Some(weak) = ctx.weak_value(&sheet) {
            registry.sheets.borrow_mut().insert(node, weak);
        }
    }
    Ok(sheet)
}

pub(crate) fn update_stylesheet_origin_metadata(realm:&DomRealm,node:NodeId,metadata:Arc<[(Arc<str>,bool)]>) {
    let Some(registry)=realm.cssom_registry.borrow().upgrade() else{return};
    let positions=registry.sheet_positions.borrow().get(&node).and_then(Weak::upgrade);
    if let Some(positions)=positions {*positions.origin_metadata.borrow_mut()=metadata;}
}

/// Return the document's live `StyleSheetList`. Entries are discovered from
/// the current tree on each access, in document order, and share the cached
/// CSSStyleSheet wrapper returned by `HTMLStyleElement.sheet`.
pub fn document_style_sheets(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    document: NodeId,
    owner: Value,
) -> Value {
    ctx.new_instance(DomStyleSheetList {
        realm: realm.clone(),
        document,
        owner,
    })
}

fn style_elements(realm:&Rc<DomRealm>,root:NodeId,ctx:&mut Ctx)->OpResult<Vec<NodeId>> {
    let mut candidates=Vec::new();
    {let session=realm.session.borrow();let document=session.document();let mut node=root;
        loop {
            if matches!(document.kind(node),Ok(NodeKind::Element{name,namespace,..})
                if matches!(namespace,Namespace::Html|Namespace::Svg) && lumen_html::svg::local_name(name)=="style")
                || (matches!(document.kind(node),Ok(NodeKind::Element{name,namespace:Namespace::Html,..}) if name=="link")
                    && session.link_stylesheet_state(node).is_some())
                || (lumen_html::xml_stylesheet::is_candidate(document,node) && session.link_stylesheet_state(node).is_some()) {
                candidates.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","stylesheet list admission"))?;candidates.push(node);
            }
            let Some(next)=lumen_html::selector::next_descendant(document,root,node).map_err(dom_error)? else {break};node=next;
        }
    }
    let mut result=Vec::new();result.try_reserve(candidates.len()).map_err(|_|OpError::new("QuotaExceededError","stylesheet list admission"))?;
    for node in candidates {
        let link=lumen_html::xml_stylesheet::is_candidate(realm.session.borrow().document(),node)
            || matches!(realm.session.borrow().document().kind(node),Ok(NodeKind::Element{name,namespace:Namespace::Html,..}) if name=="link");
        if link || realm.ensure_inline_stylesheet(ctx,node)? {result.push(node);}
    }
    Ok(result)
}

/// HTML CSS module creation uses the same constructed-sheet operations as
/// CSSOM, in the owning settings realm. The resulting native sheet is the
/// synthetic module's actual default export, with shared live rule ownership.
pub(crate) fn create_module_stylesheet(ctx: &mut Ctx, source: &str) -> OpResult<Value> {
    let sheet = DomCssStyleSheet::new(ctx, None)?;
    sheet.replace_sync(ctx,lumen_host::webidl::Usv(lumen::well_formed_utf8(source).into_owned()))?;
    Ok(ctx.new_instance(sheet))
}

impl DomCssStyleSheet {
    fn check_modification(&self,ctx:&mut Ctx)->OpResult<()> {
        self.check_origin(ctx)?;
        if matches!(self.source.root(),SheetSource::Constructed(data) if data.modifying.get()) {
            return Err(crate::error_reporting::dom_exception(ctx,"NotAllowedError","stylesheet replacement is pending"));
        }
        Ok(())
    }
    fn check_origin(&self,ctx:&mut Ctx)->OpResult<()> {
        if !self.origin_clean {return Err(crate::error_reporting::dom_exception(ctx,"SecurityError","stylesheet is not origin-clean"))}
        Ok(())
    }
    fn replace_rules(&self,text:&str)->OpResult<()> {
        if text.len()>css::MAX_CSS_BYTES {return Err(OpError::new("QuotaExceededError","stylesheet source exceeds limit"));}
        let mut parsed=CssStyleSheetText::default();
        parsed.replace_constructed(text).map_err(css_error)?;
        self.commit_replacement(parsed)
    }
    fn commit_replacement(&self,parsed:CssStyleSheetText)->OpResult<()> {
        let before=source_sheet(&self.realm,&self.source)?;
        let positions=self.source.positions();positions.synchronize(before.css_text())?;
        commit_rule_declaration(&self.realm,&self.source,&before,&parsed,&[],None)?;
        positions.retain_detached_declarations(&[],None,&before.retained);
        positions.remember(before.css_text())?;positions.replaced(parsed.css_text())
    }
    // Only the intrinsic AsyncHost parse result can enter this method. The
    // immutable source carrier has already been validated and imports removed.
    fn commit_validated_replacement(&self,source:&str)->OpResult<()> {
        self.commit_replacement(CssStyleSheetText{text:source.to_owned(),..CssStyleSheetText::default()})
    }
}

#[lumen_bind::methods]
impl DomCssStyleSheet {
    #[constructor]
    fn new(ctx: &mut Ctx, options: Option<CssStyleSheetInit>) -> OpResult<Self> {
        let options = options.unwrap_or_default();
        let registry = RealmServices::<MediaQueryRegistry>::current(ctx)
            .ok_or_else(|| OpError::new("InvalidStateError", "CSSOM is not installed"))?;
        let realm = registry
            .realm
            .upgrade()
            .ok_or_else(|| OpError::new("InvalidStateError", "document realm was released"))?;
        let location=realm.base_url();
        let base_url=Some(Arc::from(lumen_common::url::parse(options.base_url.as_deref().unwrap_or(&location),Some(&location))
            .map_err(|_|crate::error_reporting::dom_exception(ctx,"NotAllowedError","invalid stylesheet base URL"))?.href()));
        let constructor_document = realm.session.borrow().document().root();
        Ok(Self {
            parent_sheet: Value::Null,
            realm: realm.clone(),
            source: SheetSource::Constructed(Rc::new(ConstructedSheetData {
                realm: Rc::downgrade(&realm),
                constructor_document,
                registry: Rc::downgrade(&registry),
                text: RefCell::new(String::new()),
                positions: Rc::new(RulePositions { read_cache: registry.read_cache.clone(), ..Default::default() }),
                modifying: Cell::new(false),
                base_url,
                media: RefCell::new(css::media_query_list_items(&options.media).join(", ")),
                disabled: Cell::new(options.disabled),
            })),
            owner: Value::Null,
            owner_rule: Value::Null,
            href_value: Some(location),
            rule_list_wrapper: RefCell::new(None),
            origin_clean:true,
            origin_metadata:Rc::new(RefCell::new(Arc::from([]))),
        })
    }

    #[getter]
    fn css_rules(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        self.check_origin(ctx)?;
        if let Some(wrapper) = self
            .rule_list_wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
        {
            return Ok(wrapper);
        }
        let wrapper = ctx.new_instance(DomCssRuleList {
            realm: self.realm.clone(),
            source: self.source.clone(),
            rule_path: Vec::new(),
            owner: this.0.clone(),
            sheet_owner: this.0,
            keyframes_index: None,
        });
        *self.rule_list_wrapper.borrow_mut() = ctx.weak_value(&wrapper);
        Ok(wrapper)
    }

    #[getter]
    fn rules(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        self.css_rules(ctx, this)
    }

    #[getter(name = "ownerNode")]
    fn owner_node(&self) -> Value {
        self.realm.session.borrow_mut().reclaim_stale_stylesheet_sources();
        if matches!(self.source.root(), SheetSource::Element { lease, .. } if lease.is_detached()) { return Value::Null; }
        self.owner.clone()
    }

    #[getter(name = "parentStyleSheet")]
    fn parent_style_sheet(&self) -> OpResult<Value> {
        self.realm.session.borrow_mut().reclaim_stale_stylesheet_sources();
        if let SheetSource::Imported(data) = self.source.root() {
            if data.detached_graph.borrow().is_some() || data.rule_start(&self.realm)?.is_none() { return Ok(Value::Null); }
            return Ok(self.parent_sheet.clone());
        }
        Ok(Value::Null)
    }

    #[getter(name = "ownerRule")]
    fn owner_rule(&self) -> Value {
        self.owner_rule.clone()
    }

    #[getter]
    fn href(&self) -> Value {
        self.href_value
            .clone()
            .map_or(Value::Null, |href| Value::Str(href.into()))
    }

    #[getter]
    fn media(&self,ctx:&mut Ctx,this:This<Value>)->OpResult<Value> {
        const SLOT:&str="#lumen_stylesheet_media\u{1}value";
        if let Some(value)=ctx.native_private_value_slot(&this.0,SLOT) {return Ok(value);}
        let value=if matches!(self.source.root(),SheetSource::Imported(_)) {import_rule_media(ctx,&self.owner_rule)?}
            else {ctx.new_instance(DomCssMediaList{realm:self.realm.clone(),source:self.source.clone(),index:None})};
        ctx.define_native_internal_value_slot(&this.0,SLOT,value.clone()).map_err(OpError::thrown)?;Ok(value)
    }
    #[getter]
    fn disabled(&self)->OpResult<bool> {
        self.realm.session.borrow_mut().reclaim_stale_stylesheet_sources();
        Ok(match self.source.root() {
            SheetSource::Constructed(data)=>data.disabled.get(),
            SheetSource::Element{node,lease,..}=>{
                if lease.is_detached() {return Ok(lease.disabled())}
                let (realm,node)=self.realm.resolve_adopted_node(*node);
                let disabled=realm.session.borrow().stylesheet_disabled(node);
                lease.set_disabled(disabled);
                disabled
            }
            SheetSource::Imported(data)=>data.with_graph(&self.realm,&mut |source|Ok(source.is_some_and(|source|source.disabled)))?,
            _=>false,
        })
    }
    #[setter]
    fn set_disabled(&self,value:bool)->OpResult<()> {
        self.realm.session.borrow_mut().reclaim_stale_stylesheet_sources();
        match self.source.root() {
            SheetSource::Constructed(data)=>{
                let previous=data.disabled.replace(value);
                if let Some(registry)=data.registry.upgrade() {if let Err(error)=sync_adopted(&registry,&self.realm) {data.disabled.set(previous);return Err(error);}}
                Ok(())
            }
            SheetSource::Element{node,lease,..}=>{
                if lease.is_detached() {lease.set_disabled(value);return Ok(())}
                let (realm,node)=self.realm.resolve_adopted_node(*node);
                realm.stylesheet_links.set_sheet_disabled(node,value)?;
                let result=realm.session.borrow_mut().set_stylesheet_disabled(node,value)
                    .map_err(|error|OpError::new("InvalidStateError",format!("stylesheet disabled state: {error:?}")));
                result
            }
            SheetSource::Imported(data)=>{
                let lease=data.lease.borrow().clone();
                if data.detached_graph.borrow().is_none() && lease.as_ref().is_some_and(|lease|lease.with_live_path(|path|path.is_some())) {
                    let lease=lease.expect("live occurrence lease");
                    self.realm.session.borrow_mut().set_stylesheet_import_disabled(data.owner,&lease,value)
                        .map_err(|error|OpError::new("InvalidStateError",format!("imported stylesheet disabled state: {error:?}")))
                } else {
                    data.with_local_graph_mut(&self.realm,&mut |source|{source.disabled=value;Ok(())})
                }
            }
            _=>Err(OpError::new("InvalidStateError","stylesheet owner unavailable")),
        }
    }
    #[getter]
    fn title(&self)->Value {
        if let SheetSource::Element{node,lease,..}=self.source.root() {
            if let Some(metadata)=lease.metadata() {return if metadata.title.is_empty(){Value::Null}else{Value::from_string(metadata.title.to_string())};}
            let (realm,node)=self.realm.resolve_adopted_node(*node);
            let session=realm.session.borrow();let document=session.document();
            let title=crate::stylesheet_loading::inline_stylesheet_title(document,node).map(str::to_owned);
            return title.filter(|title|!title.is_empty()).map_or(Value::Null,|title|Value::from_string(title));
        }
        Value::Null
    }
    #[getter(name="type")]
    fn sheet_type(&self)->&'static str {"text/css"}

    #[method(coerce)]
    fn insert_rule(&self, ctx: &mut Ctx, rule: &str, index: Option<u32>) -> OpResult<u32> {
        self.check_modification(ctx)?;
        let mut sheet = source_sheet(&self.realm, &self.source)?;
        let before = sheet.clone();
        self.source.positions().synchronize(sheet.css_text())?;
        let position = index.unwrap_or(0) as usize;
        // CSSStyleSheet parses before invoking the generic list algorithm,
        // including its index check. Constructed imports fail in this phase.
        let parsed=parse_inserted_rule(rule,sheet.css_text()).map_err(|error|css_rule_dom_exception(ctx,error))?;
        if matches!(&self.source,SheetSource::Constructed(_)) && parsed.header==RuleHeader::Import {
            return Err(crate::error_reporting::dom_exception(ctx,"SyntaxError","constructed stylesheets cannot insert import rules"));
        }
        sheet.insert_parsed_rule(parsed,position).map_err(|error|rule_mutation_exception(ctx,error))?;
        commit_rule_declaration(&self.realm, &self.source, &before, &sheet, &[], None)?;
        self.source.positions().inserted(position, sheet.css_text())?;
        Ok(position as u32)
    }

    fn delete_rule(&self, ctx: &mut Ctx, index: u32) -> OpResult<()> {
        self.check_modification(ctx)?;
        let index = index as usize;
        let mut sheet = source_sheet(&self.realm, &self.source)?;
        let before = sheet.clone();
        let positions = self.source.positions();
        positions.synchronize(sheet.css_text())?;
        let rules = sheet.css_rules().map_err(css_error)?;
        let removed_rule = rules
            .get(index)
            .cloned()
            .ok_or_else(|| crate::error_reporting::dom_exception(ctx, "IndexSizeError", "CSS rule index out of range"))?;
        sheet.delete_rule(index).map_err(|error|rule_mutation_exception(ctx,error))?;
        commit_rule_declaration(&self.realm, &self.source, &before, &sheet, &[], None)?;
        positions.retain_detached_declarations(&[], Some(index), &before.retained);
        positions.deleted_rule(index, &removed_rule, sheet.css_text())?;
        Ok(())
    }

    fn remove_rule(&self, ctx: &mut Ctx, index: Option<u32>) -> OpResult<()> {
        self.delete_rule(ctx, index.unwrap_or(0))
    }

    #[method(coerce)]
    fn replace_sync(&self, ctx:&mut Ctx, text: lumen_host::webidl::Usv) -> OpResult<()> {
        if !matches!(&self.source, SheetSource::Constructed(_)) {
            return Err(crate::error_reporting::dom_exception(ctx,"NotAllowedError","replaceSync requires a constructed stylesheet"));
        }
        self.check_modification(ctx)?;
        self.replace_rules(&text.0)
    }

    #[method(coerce)]
    fn replace(&self, ctx:&mut Ctx, this: This<Value>, text: lumen_host::webidl::Usv) -> Promise<Value> {
        let SheetSource::Constructed(data)=&self.source else {
            return Promise::rejected(crate::error_reporting::dom_exception(ctx,"NotAllowedError","replace requires a constructed stylesheet"));
        };
        if let Err(error)=self.check_modification(ctx) {return Promise::rejected(error);}
        if text.0.len()>css::MAX_CSS_BYTES {return Promise::rejected(OpError::new("QuotaExceededError","stylesheet source exceeds limit"));}
        data.modifying.set(true);
        let lock=ConstructedReplacementLock(data.clone());
        let deferred=Deferred::new(ctx);
        let pending=Promise::pending(&deferred);
        // The shared AsyncHost owns parsing and completion. No Document task is
        // used: navigation must not cancel a retained sheet's replacement.
        let completion=Rc::new(RefCell::new(Some((deferred,lock))));
        let source=text.0;
        let parsed=ctx.spawn_blocking(move || {
            let mut sheet=CssStyleSheetText::parse("").expect("empty stylesheet");
            sheet.replace_constructed(&source).map(|()|sheet.text)
                .map_err(|error|lumen::embed::SendError::new("SyntaxError",format!("CSS error at {}: {}",error.offset,error.message)))
        }).into_ret(ctx);
        let parsed=match parsed {Ok(value)=>value,Err(reason)=>{
            let (deferred,lock)=completion.borrow_mut().take().expect("replacement completion");drop(lock);deferred.reject(ctx,OpError::thrown(reason));return pending;
        }};
        let sheet=this.0;
        let success=completion.clone();
        let on_ok=ctx.new_native_fn("stylesheetReplacementComplete",1,Rc::new(move |ctx,_,args| {
            let state=success.borrow_mut().take();
            if let Some((deferred,lock))=state {
                let result=match args.first() {
                    Some(Value::Str(source))=>ctx.with_instance::<DomCssStyleSheet,_>(&sheet,|sheet|sheet.commit_validated_replacement(source.as_str())).and_then(|result|result),
                    _=>Err(OpError::new("InvalidStateError","stylesheet parser completion is invalid")),
                };
                drop(lock);
                match result {Ok(())=>deferred.resolve(ctx,sheet.clone()),Err(error)=>deferred.reject(ctx,error)}
            }
            Ok(Value::Undefined)
        }));
        let on_err=ctx.new_native_fn("stylesheetReplacementFailed",1,Rc::new(move |ctx,_,args| {
            let state=completion.borrow_mut().take();
            if let Some((deferred,lock))=state {drop(lock);deferred.reject(ctx,OpError::thrown(args.first().cloned().unwrap_or(Value::Undefined)));}
            Ok(Value::Undefined)
        }));
        ctx.then_value(parsed,on_ok,on_err);
        pending
    }

}

#[lumen_bind::methods]
impl DomCssRuleList {
    #[proto(len)]
    fn length(&self) -> OpResult<usize> {
        let location = rule_location(&self.realm, &self.source, &self.rule_path)?;
        let sheet = source_sheet(&self.realm, &location.source)?;
        if let Some(index) = self.keyframes_index.as_ref() {
            let keyframes = sheet.keyframes_rule(index.get()).map_err(css_error)?;
            index.children().synchronize(&keyframe_text(&keyframes))?;
            return Ok(sheet
                .keyframes_rule(index.get())
                .map_err(css_error)?
                .rules
                .len());
        }
        let rules = sheet
            .rule_list(&location.indices.clone())
            .map_err(css_error)?;
        let positions = positions_for_rule_list(&location.source, &self.rule_path);
        let position_text = if self.rule_path.is_empty() {
            sheet.css_text().to_owned()
        } else {
            rules_text(&rules)
        };
        if self.rule_path.is_empty() { positions.synchronize(&position_text)?; } else { positions.synchronize_rules(&rules)?; }
        Ok(rules.len())
    }

    #[proto(getitem)]
    fn indexed(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        let location = rule_location(&self.realm, &self.source, &self.rule_path)?;
        let sheet = source_sheet(&self.realm, &location.source)?;
        if let Some(keyframes_index) = self.keyframes_index.as_ref() {
            let keyframes = sheet
                .keyframes_rule(keyframes_index.get())
                .map_err(css_error)?;
            if index >= keyframes.rules.len() {
                return Ok(Value::Undefined);
            }
            let positions = keyframes_index.children();
            positions.synchronize(&keyframe_text(&keyframes))?;
            let position = positions.at(index);
            if let Some(wrapper) = position
                .wrapper
                .borrow()
                .as_ref()
                .and_then(WeakValue::upgrade)
            {
                return Ok(wrapper);
            }
            let base = DomCssRule {
                realm: self.realm.clone(),
                source: location.source.clone(),
                index: keyframes_index.clone(),
                path: self.rule_path.clone(),
                keyframe_index: Some(position.clone()),
                owner: self.owner.clone(),
                sheet_owner: self.sheet_owner.clone(),
                style_wrapper: RefCell::new(None),
                rule_list_wrapper: RefCell::new(None),
            };
            let wrapper = ctx.new_instance(DomCssKeyframeRule { base });
            *position.wrapper.borrow_mut() = ctx.weak_value(&wrapper);
            return Ok(wrapper);
        }
        let rules = sheet
            .rule_list(&location.indices.clone())
            .map_err(css_error)?;
        let Some(rule) = rules.get(index) else {
            return Ok(Value::Undefined);
        };
        let positions = positions_for_rule_list(&location.source, &self.rule_path);
        let position_text = if self.rule_path.is_empty() {
            sheet.css_text().to_owned()
        } else {
            rules_text(&rules)
        };
        if self.rule_path.is_empty() { positions.synchronize(&position_text)?; } else { positions.synchronize_rules(&rules)?; }
        let position = positions.at(index);
        if let Some(wrapper) = position
            .wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
        {
            return Ok(wrapper);
        }
        position.nested_declarations.set(rule.nested_declarations);
        position.style_context.set(rule.selector_text.is_some() || starts_css_at_rule(&rule.css_text, "@scope") || self.rule_path.last().is_some_and(|parent| parent.style_context.get()));
        position.scope_context.set(starts_css_at_rule(&rule.css_text, "@scope") || (rule.selector_text.is_none() && self.rule_path.last().is_some_and(|parent| parent.scope_context.get())));
        let mut path = self.rule_path.clone();
        path.push(position.clone());
        let source = if self.rule_path.is_empty() {
            SheetSource::Rule {
                source: Rc::new(location.source.clone()),
                position: position.clone(),
            }
        } else {
            location.source.clone()
        };
        let base = DomCssRule {
            realm: self.realm.clone(),
            source,
            index: position.clone(),
            path,
            keyframe_index: None,
            owner: self.owner.clone(),
            sheet_owner: self.sheet_owner.clone(),
            style_wrapper: RefCell::new(None),
            rule_list_wrapper: RefCell::new(None),
        };
        let (wrapper, rule_type) = if rule.nested_declarations {
            (ctx.new_instance(DomCssNestedDeclarations { base }), 0)
        } else if rule.keyframes.is_some() {
            (ctx.new_instance(DomCssKeyframesRule { base }), 7)
        } else if starts_css_at_rule(&rule.css_text, "@import") {
            (ctx.new_instance(DomCssImportRule {
                base,
                media_wrapper: RefCell::new(None),
                stylesheet_wrapper: RefCell::new(None),
            }), 3)
        } else if rule.header==RuleHeader::Namespace {
            (ctx.new_instance(DomCssNamespaceRule {base}),10)
        } else if starts_css_at_rule(&rule.css_text,"@font-feature-values") {
            (font_features::wrap(ctx,base)?,14)
        } else if starts_css_at_rule(&rule.css_text, "@property") {
            (ctx.new_instance(DomCssPropertyRule { base }), 0)
        } else if rule.font_face {
            (ctx.new_instance(DomCssFontFaceRule { base }), 5)
        } else if rule.selector_text.is_some() {
            (ctx.new_instance(DomCssStyleRule {
                base: DomCssGroupingRule { base },
            }), 1)
        } else if starts_css_at_rule(&rule.css_text, "@starting-style") {
            (ctx.new_instance(DomCssStartingStyleRule { base: DomCssGroupingRule { base } }), 0)
        } else if starts_css_at_rule(&rule.css_text, "@scope") {
            (ctx.new_instance(DomCssScopeRule { base: DomCssGroupingRule { base } }), 0)
        } else if starts_css_at_rule(&rule.css_text, "@media") {
            (ctx.new_instance(DomCssMediaRule {
                base: DomCssConditionRule {
                    base: DomCssGroupingRule { base },
                },
            }), 4)
        } else if starts_css_at_rule(&rule.css_text, "@supports") {
            (ctx.new_instance(DomCssSupportsRule {
                base: DomCssConditionRule {
                    base: DomCssGroupingRule { base },
                },
            }), 12)
        } else if !rule.nested.is_empty() {
            (ctx.new_instance(DomCssGroupingRule { base }), 0)
        } else {
            (ctx.new_instance(base), 0)
        };
        position.rule_type.set(rule_type);
        *position.wrapper.borrow_mut() = ctx.weak_value(&wrapper);
        Ok(wrapper)
    }

    fn item(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        let value = self.indexed(ctx, index)?;
        Ok(if matches!(value, Value::Undefined) {
            Value::Null
        } else {
            value
        })
    }

    #[proto(iter)]
    fn values(this: This<Value>) -> DomCssomCollectionIterator {
        DomCssomCollectionIterator {
            collection: this.0,
            index: Cell::new(0),
        }
    }
}

#[lumen_bind::methods]
impl DomStyleSheetList {
    #[proto(len)]
    fn length(&self,ctx:&mut Ctx) -> OpResult<usize> {
        Ok(style_elements(&self.realm,self.document,ctx)?.len())
    }

    #[proto(getitem)]
    fn indexed(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        let Some(node) = style_elements(&self.realm,self.document,ctx)?
            .get(index)
            .copied()
        else {
            return Ok(Value::Undefined);
        };
        let owner = self.realm.wrap(ctx, node);
        style_element_sheet(ctx, &self.realm, node, owner)
    }

    fn item(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        let value = self.indexed(ctx, index)?;
        Ok(if matches!(value, Value::Undefined) {
            Value::Null
        } else {
            value
        })
    }

    #[proto(iter)]
    fn values(this: This<Value>) -> DomCssomCollectionIterator {
        DomCssomCollectionIterator {
            collection: this.0,
            index: Cell::new(0),
        }
    }
}

#[lumen_bind::methods]
impl DomCssomCollectionIterator {
    #[proto(iter)]
    fn iter(&self, this: This<Value>) -> Value {
        this.0
    }

    #[proto(next)]
    fn next(&self, ctx: &mut Ctx) -> OpResult<Option<Value>> {
        let length = ctx
            .get_member(&self.collection, "length")
            .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
        let length = match length {
            Value::Num(length) if length.is_finite() && length >= 0.0 => length as usize,
            _ => return Err(OpError::new("TypeError", "invalid CSSOM collection length")),
        };
        let index = self.index.get();
        if index >= length {
            return Ok(None);
        }
        let value = ctx
            .get_member(&self.collection, &index.to_string())
            .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
        self.index.set(index + 1);
        Ok(Some(value))
    }
}

impl DomCssRule {
    fn current_rule(&self) -> OpResult<CssRuleText> {
        let location = rule_location(&self.realm, &self.source, &self.path)?;
        if let Some(frame) = self.keyframe_index.as_ref() {
            let _ = source_keyframe(&self.realm, &location.source, &self.index, frame)?;
        }
        rule_from_path(&source_sheet(&self.realm, &location.source)?, &self.path)
    }

    fn with_source_rule<R>(&self, read: &mut dyn FnMut(&str, &css::nesting::SourceRule) -> OpResult<R>) -> OpResult<R> {
        let location = rule_location(&self.realm, &self.source, &self.path)?;
        with_source_text(&self.realm, &location.source, &mut |text| {
            let detached = location.source.detached_position();
            let nested = detached.is_some_and(|position| position.owner.nested_context);
            let scoped = detached.is_some_and(|position| position.owner.scoped_context);
            let positions = location.source.positions();
            let mut slot = positions.read_cache.borrow_mut();
            let boundaries = location.source.boundary_state().borrow().clone();
            let cached = cssom_read_cache(&mut slot, text, boundaries, nested, scoped,detached.map(|position|position.detached_namespaces.borrow().clone()))?;
            let rule = css::nesting::source_rule_at_path(&cached.rules, &location.indices)
                .ok_or_else(|| OpError::new("InvalidStateError", "stylesheet rule was removed"))?;
            read(text, rule)
        })
    }

    fn condition_text(&self) -> OpResult<String> {
        self.with_source_rule(&mut |text, rule| {
            let prelude = text[rule.prelude.clone()].trim();
            let name = if starts_css_at_rule(prelude, "@media") { "@media" } else { "@supports" };
            let condition = prelude.get(name.len()..).unwrap_or_default();
            Ok(condition.trim().to_owned())
        })
    }

    fn css_rules(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        if let Some(wrapper) = self
            .rule_list_wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
        {
            return Ok(wrapper);
        }
        let wrapper = ctx.new_instance(DomCssRuleList {
            realm: self.realm.clone(),
            source: self.source.clone(),
            rule_path: self.path.clone(),
            owner: this.0,
            sheet_owner: self.sheet_owner.clone(),
            keyframes_index: None,
        });
        *self.rule_list_wrapper.borrow_mut() = ctx.weak_value(&wrapper);
        Ok(wrapper)
    }

    fn insert_nested_rule(&self, ctx: &mut Ctx, rule: &str, index: Option<u32>) -> OpResult<u32> {
        let location = rule_location(&self.realm, &self.source, &self.path)?;
        let mut sheet = source_sheet(&self.realm, &location.source)?;
        let before = sheet.clone();
        let parent_path = location.indices.clone();
        let current = sheet.rule_list(&parent_path).map_err(css_error)?;
        let positions = positions_for_rule_list(&location.source, &self.path);
        positions.synchronize_rules(&current)?;
        let index = index.unwrap_or(0) as usize;
        if index > current.len() {
            return Err(crate::error_reporting::dom_exception(
                ctx,
                "IndexSizeError",
                "CSS rule index out of range",
            ));
        }
        if starts_css_at_rule(rule,"@import") || starts_css_at_rule(rule,"@namespace") {
            // Valid header rules are forbidden here, but malformed headers
            // still fail parsing before the hierarchy check.
            let parsed=parse_inserted_rule(rule,sheet.css_text()).map_err(|error|css_rule_dom_exception(ctx,error))?;
            if matches!(parsed.header,RuleHeader::Import|RuleHeader::Namespace) {
                return Err(rule_mutation_exception(ctx,CssRuleMutationError::Hierarchy));
            }
        }
        sheet
            .insert_nested_rule(&parent_path, rule, index)
            .map_err(|error| css_rule_dom_exception(ctx, error))?;
        commit_rule_declaration(&self.realm, &location.source, &before, &sheet, &[], None)?;
        let updated = source_sheet(&self.realm, &location.source)?;
        let children = updated.rule_list(&parent_path).map_err(css_error)?;
        positions.inserted(index, &rules_text(&children))?;
        positions.remember_rules(&children)?;
        remember_rule_path(&location.source, &updated, &self.path)?;
        Ok(index as u32)
    }

    fn delete_nested_rule(&self, ctx: &mut Ctx, index: u32) -> OpResult<()> {
        let location = rule_location(&self.realm, &self.source, &self.path)?;
        let index = index as usize;
        let mut sheet = source_sheet(&self.realm, &location.source)?;
        let before = sheet.clone();
        let parent_path = location.indices.clone();
        let current = sheet.rule_list(&parent_path).map_err(css_error)?;
        if index >= current.len() {
            return Err(crate::error_reporting::dom_exception(
                ctx,
                "IndexSizeError",
                "CSS rule index out of range",
            ));
        }
        let positions = positions_for_rule_list(&location.source, &self.path);
        positions.synchronize_rules(&current)?;
        let _removed = sheet
            .delete_nested_rule(&parent_path, index)
            .map_err(|error| css_rule_dom_exception(ctx, error))?;
        commit_rule_declaration(&self.realm, &location.source, &before, &sheet, &[], None)?;
        let updated = source_sheet(&self.realm, &location.source)?;
        let children = updated.rule_list(&parent_path).map_err(css_error)?;
        positions.retain_detached_declarations(&parent_path, Some(index), &before.retained);
        positions.deleted_rule(index, &current[index], &rules_text(&children))?;
        positions.remember_rules(&children)?;
        remember_rule_path(&location.source, &updated, &self.path)?;
        Ok(())
    }

    fn set_selector_text(&self, value: &str) -> OpResult<()> {
        let location = rule_location(&self.realm, &self.source, &self.path)?;
        let mut sheet = source_sheet(&self.realm, &location.source)?;
        let before = sheet.clone();
        if sheet
            .set_rule_selector(&location.indices.clone(), value)
            .is_err()
        {
            // CSSStyleRule.selectorText ignores selectors that cannot be
            // parsed in the rule's actual (possibly nested) context.
            return Ok(());
        }
        commit_rule_declaration(&self.realm, &location.source, &before, &sheet, &[], None)?;
        let updated = source_sheet(&self.realm, &location.source)?;
        remember_rule_path(&location.source, &updated, &self.path)
    }
}

#[lumen_bind::methods]
impl DomCssRule {
    #[constant]
    const NAMESPACE_RULE: u16 = 10;
    #[constant]
    const FONT_FEATURE_VALUES_RULE: u16 = 14;
    #[getter(name = "type")]
    fn rule_type(&self) -> OpResult<u16> {
        Ok(if self.keyframe_index.is_some() { 8 } else { self.index.rule_type.get() })
    }

    #[getter(name = "parentStyleSheet")]
    fn parent_style_sheet(&self) -> OpResult<Value> {
        let _ = rule_location(&self.realm, &self.source, &self.path)?;
        Ok(
            if self
                .path
                .iter()
                .any(|position| position.detached.borrow().is_some())
                || self
                    .keyframe_index
                    .as_ref()
                    .is_some_and(|frame| frame.detached.borrow().is_some())
            {
                Value::Null
            } else {
                self.sheet_owner.clone()
            },
        )
    }

    #[getter(name = "parentRule")]
    fn parent_rule(&self) -> OpResult<Value> {
        let location = rule_location(&self.realm, &self.source, &self.path)?;
        if let Some(frame) = self.keyframe_index.as_ref() {
            let _ = source_keyframe(&self.realm, &location.source, &self.index, frame)?;
            Ok(if frame.detached.borrow().is_some() {
                Value::Null
            } else {
                self.owner.clone()
            })
        } else if self.path.len() > 1 && self.index.detached.borrow().is_none() {
            Ok(self.owner.clone())
        } else {
            Ok(Value::Null)
        }
    }
    #[getter(name = "cssText")]
    fn css_text(&self) -> OpResult<String> {
        if let Some(frame_index) = self.keyframe_index.as_ref() {
            return Ok(
                source_keyframe(&self.realm, &self.source, &self.index, frame_index)?.css_text,
            );
        }
        let rule = self.current_rule()?;
        Ok(if rule.nested_declarations {
            rule.style.css_text().to_owned()
        } else if rule.selector_text.is_some() || !rule.nested.is_empty() || rule.header==RuleHeader::Namespace || starts_css_at_rule(&rule.css_text, "@scope") || starts_css_at_rule(&rule.css_text,"@font-feature-values") {
            serialize_rule(&rule)
        } else if rule.font_face {
            format!("@font-face {{ {} }}", rule.style.css_text())
        } else {
            rule.css_text
        })
    }

    #[getter(name = "selectorText")]
    fn selector_text(&self) -> OpResult<Option<String>> {
        Ok(self.current_rule()?.selector_text)
    }

    #[getter]
    fn style(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        if let Some(wrapper) = self
            .style_wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
        {
            return Ok(wrapper);
        }
        let rule = self.current_rule()?;
        let frame_style = self
            .keyframe_index
            .as_ref()
            .map(|frame| source_keyframe(&self.realm, &self.source, &self.index, frame))
            .transpose()?;
        if rule.selector_text.is_none()
            && !rule.font_face
            && !rule.nested_declarations
            && frame_style.is_none()
        {
            return Err(OpError::new(
                "InvalidStateError",
                "rule has no style declaration",
            ));
        }
        let declaration = DomCssRuleStyle {
            base: DomStyle {
                realm: self.realm.clone(),
                node: source_dom_node(&self.realm, &self.source),
                computed: false,
                pseudo: None,
                invalid_pseudo: false,
                _owner: this.0.clone(),
            },
            realm: self.realm.clone(),
            source: self.source.clone(),
            index: self.index.clone(),
            path: self.path.clone(),
            keyframe_index: self.keyframe_index.clone(),
            owner: this.0,
        };
        let wrapper = if rule.font_face {
            ctx.new_instance(DomCssFontFaceDescriptors { base: declaration })
        } else {
            ctx.new_instance(declaration)
        };
        *self.style_wrapper.borrow_mut() = ctx.weak_value(&wrapper);
        Ok(wrapper)
    }
}

impl DomCssPropertyRule {
    fn registration(&self) -> OpResult<css::registered_properties::RegisteredCustomProperty> {
        self.base.with_source_rule(&mut |text, source| {
            css::registered_properties::parse_rule(text[source.prelude.clone()].trim(), &source.declaration_source(text))
                .map_err(css_error)?.ok_or_else(|| OpError::new("InvalidStateError", "property rule is invalid"))
        })
    }
}
#[lumen_bind::methods]
impl DomCssPropertyRule {
    #[getter]
    fn name(&self) -> OpResult<String> { Ok(self.registration()?.name) }
    #[getter]
    fn syntax(&self) -> OpResult<String> { Ok(self.registration()?.syntax) }
    #[getter]
    fn inherits(&self) -> OpResult<bool> { Ok(self.registration()?.inherits) }
    #[getter(name = "initialValue")]
    fn initial_value(&self) -> OpResult<Option<String>> { Ok(self.registration()?.initial_value) }
    #[getter(name = "cssText")]
    fn css_text(&self) -> OpResult<String> {
        let registration = self.registration()?;
        let mut text = format!("@property {} {{ syntax: {}; inherits: {};", css::serialize_identifier(&registration.name),
            css::serialize_string(&registration.syntax), registration.inherits);
        if let Some(initial) = registration.initial_value { text.push_str(&format!(" initial-value: {initial};")); }
        text.push_str(" }");
        Ok(text)
    }
}

#[lumen_bind::methods]
impl DomCssFontFaceRule {
    #[getter]
    fn style(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        self.base.style(ctx, this)
    }
    #[setter(coerce)]
    fn set_style(&self, value: &str) -> OpResult<()> {
        let location = rule_location(&self.base.realm, &self.base.source, &self.base.path)?;
        let mut sheet = source_sheet(&self.base.realm, &location.source)?;
        let before = sheet.clone();
        sheet
            .set_rule_style(&location.indices.clone(), value)
            .map_err(css_error)?;
        commit_rule_declaration(&self.base.realm, &location.source, &before, &sheet, &[], None)?;
        let updated = source_sheet(&self.base.realm, &location.source)?;
        remember_rule_path(&location.source, &updated, &self.base.path)
    }
}

#[lumen_bind::methods]
impl DomCssGroupingRule {
    #[getter(name = "cssRules")]
    fn css_rules(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        self.base.css_rules(ctx, this)
    }

    #[method(coerce)]
    fn insert_rule(&self, ctx: &mut Ctx, rule: &str, index: Option<u32>) -> OpResult<u32> {
        self.base.insert_nested_rule(ctx, rule, index)
    }

    fn delete_rule(&self, ctx: &mut Ctx, index: u32) -> OpResult<()> {
        self.base.delete_nested_rule(ctx, index)
    }
}

#[lumen_bind::methods]
impl DomCssConditionRule {
    #[getter(name = "conditionText")]
    fn condition_text(&self) -> OpResult<String> {
        self.base.base.condition_text()
    }
}

#[lumen_bind::methods]
impl DomCssMediaRule {}

#[lumen_bind::methods]
impl DomCssSupportsRule {}

impl DomCssScopeRule {
    fn scope_selector(&self, end: bool) -> OpResult<Option<String>> {
        self.base.base.with_source_rule(&mut |text, rule| {
            let prelude = rule.scope_prelude.as_ref()
                .ok_or_else(|| OpError::new("InvalidStateError", "rule is not a scope"))?;
            let range = if end { &prelude.end } else { &prelude.start };
            Ok(range.as_ref().map(|range| text[range.clone()].trim().to_owned()))
        })
    }
}

#[lumen_bind::methods]
impl DomCssScopeRule {
    #[getter]
    fn start(&self) -> OpResult<Nullable<String>> { self.scope_selector(false).map(Nullable) }

    #[getter]
    fn end(&self) -> OpResult<Nullable<String>> { self.scope_selector(true).map(Nullable) }
}

#[lumen_bind::methods]
impl DomCssStyleRule {
    #[getter(name = "cssText")]
    fn css_text(&self) -> OpResult<String> {
        self.base.base.css_text()
    }
    #[getter(name = "selectorText")]
    fn selector_text(&self) -> OpResult<Option<String>> {
        self.base.base.selector_text()
    }
    #[setter(name = "selectorText", coerce)]
    fn set_selector_text(&self, value: &str) -> OpResult<()> {
        self.base.base.set_selector_text(value)
    }
    #[getter]
    fn style(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        self.base.base.style(ctx, this)
    }
}

#[lumen_bind::methods]
impl DomCssNestedDeclarations {
    #[getter]
    fn style(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        self.base.style(ctx, this)
    }
}

#[lumen_bind::methods]
impl DomCssKeyframesRule {
    #[getter]
    fn name(&self) -> OpResult<String> {
        Ok(source_sheet(&self.base.realm, &self.base.source)?
            .keyframes_rule(self.base.index.get())
            .map_err(css_error)?
            .name)
    }

    #[getter(name = "cssRules")]
    fn css_rules(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        if let Some(wrapper) = self
            .base
            .rule_list_wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
        {
            return Ok(wrapper);
        }
        let wrapper = ctx.new_instance(DomCssRuleList {
            realm: self.base.realm.clone(),
            source: self.base.source.clone(),
            rule_path: self.base.path.clone(),
            owner: this.0,
            sheet_owner: self.base.sheet_owner.clone(),
            keyframes_index: Some(self.base.index.clone()),
        });
        *self.base.rule_list_wrapper.borrow_mut() = ctx.weak_value(&wrapper);
        Ok(wrapper)
    }

    #[method(coerce)]
    fn append_rule(&self, rule: &str) -> OpResult<()> {
        let mut sheet = source_sheet(&self.base.realm, &self.base.source)?;
        let original = sheet.clone();
        let before = sheet
            .keyframes_rule(self.base.index.get())
            .map_err(css_error)?;
        let positions = self.base.index.children();
        positions.synchronize(&keyframe_text(&before))?;
        sheet
            .append_keyframe(self.base.index.get(), rule)
            .map_err(css_error)?;
        commit_rule_declaration(&self.base.realm, &self.base.source, &original, &sheet, &[], None)?;
        positions.inserted(
            before.rules.len(),
            &keyframe_text(
                &sheet
                    .keyframes_rule(self.base.index.get())
                    .map_err(css_error)?,
            ),
        )?;
        Ok(())
    }

    #[method(coerce)]
    fn delete_rule(&self, key_text: &str) -> OpResult<()> {
        let mut sheet = source_sheet(&self.base.realm, &self.base.source)?;
        let original = sheet.clone();
        let before = sheet
            .keyframes_rule(self.base.index.get())
            .map_err(css_error)?;
        let positions = self.base.index.children();
        positions.synchronize(&keyframe_text(&before))?;
        let normalized = css::parse_keyframe_rule(&format!("{key_text} {{}}"))
            .map_err(css_error)?
            .key_text;
        let deleted = before
            .rules
            .iter()
            .position(|rule| rule.key_text.eq_ignore_ascii_case(&normalized));
        sheet
            .delete_keyframe(self.base.index.get(), key_text)
            .map_err(css_error)?;
        commit_rule_declaration(&self.base.realm, &self.base.source, &original, &sheet, &[], None)?;
        if let Some(index) = deleted {
            positions.retain_detached_declarations(&[self.base.index.get()], Some(index), &original.retained);
            positions.deleted(
                index,
                &before.rules[index].css_text,
                &keyframe_text(
                    &sheet
                        .keyframes_rule(self.base.index.get())
                        .map_err(css_error)?,
                ),
            )?;
        }
        Ok(())
    }

    #[method(coerce)]
    fn find_rule(&self, ctx: &mut Ctx, this: This<Value>, key_text: &str) -> OpResult<Value> {
        let key_text = css::parse_keyframe_rule(&format!("{key_text} {{}}"))
            .map_err(css_error)?
            .key_text;
        let keyframes = source_sheet(&self.base.realm, &self.base.source)?
            .keyframes_rule(self.base.index.get())
            .map_err(css_error)?;
        let Some(index) = keyframes
            .rules
            .iter()
            .position(|rule| rule.key_text.eq_ignore_ascii_case(&key_text))
        else {
            return Ok(Value::Null);
        };
        let positions = self.base.index.children();
        positions.synchronize(&keyframe_text(&keyframes))?;
        let position = positions.at(index);
        if let Some(wrapper) = position
            .wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
        {
            return Ok(wrapper);
        }
        let wrapper = ctx.new_instance(DomCssKeyframeRule {
            base: DomCssRule {
                realm: self.base.realm.clone(),
                source: self.base.source.clone(),
                index: self.base.index.clone(),
                path: self.base.path.clone(),
                keyframe_index: Some(position.clone()),
                owner: this.0,
                sheet_owner: self.base.sheet_owner.clone(),
                style_wrapper: RefCell::new(None),
                rule_list_wrapper: RefCell::new(None),
            },
        });
        *position.wrapper.borrow_mut() = ctx.weak_value(&wrapper);
        Ok(wrapper)
    }
}

#[lumen_bind::methods]
impl DomCssKeyframeRule {
    #[getter(name = "keyText")]
    fn key_text(&self) -> OpResult<String> {
        let frame_index = self
            .base
            .keyframe_index
            .as_ref()
            .ok_or_else(|| OpError::new("InvalidStateError", "not a keyframe rule"))?;
        Ok(source_keyframe(
            &self.base.realm,
            &self.base.source,
            &self.base.index,
            frame_index,
        )?
        .key_text)
    }

    #[setter(name = "keyText", coerce)]
    fn set_key_text(&self, value: &str) -> OpResult<()> {
        let frame_index = self
            .base
            .keyframe_index
            .as_ref()
            .ok_or_else(|| OpError::new("InvalidStateError", "not a keyframe rule"))?;
        let frame = source_keyframe(
            &self.base.realm,
            &self.base.source,
            &self.base.index,
            frame_index,
        )?;
        let replacement = css::parse_keyframe_rule(&format!("{value} {{ {} }}", frame.style))
            .map_err(css_error)?;
        if frame_index.detached.borrow().is_some() {
            *frame_index.detached.borrow_mut() = Some(replacement.css_text);
            return Ok(());
        }
        let mut sheet = source_sheet(&self.base.realm, &self.base.source)?;
        let before = sheet.clone();
        sheet
            .set_keyframe_key_text(self.base.index.get(), frame_index.get(), value)
            .map_err(css_error)?;
        commit_rule_declaration(&self.base.realm, &self.base.source, &before, &sheet, &[], None)?;
        frame_index.owner.remember(&keyframe_text(
            &sheet
                .keyframes_rule(self.base.index.get())
                .map_err(css_error)?,
        ))?;
        Ok(())
    }

    #[getter]
    fn style(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        self.base.style(ctx, this)
    }
}

// A loaded import's sheet and its owner rule expose one canonical MediaList.
// Its existing rule-position source also retains the list after detachment.
fn import_rule_media(ctx:&mut Ctx,owner:&Value)->OpResult<Value> {
    let (existing,realm,source,index)=ctx.with_instance::<DomCssImportRule,_>(owner,|rule|(
        rule.media_wrapper.borrow().as_ref().and_then(WeakValue::upgrade),
        rule.base.realm.clone(),rule.base.source.clone(),rule.base.index.clone()))?;
    if let Some(value)=existing {return Ok(value);}
    let value=ctx.new_instance(DomCssMediaList{realm,source,index:Some(index)});
    let weak=ctx.weak_value(&value);
    ctx.with_instance::<DomCssImportRule,_>(owner,|rule|*rule.media_wrapper.borrow_mut()=weak)?;
    Ok(value)
}

#[lumen_bind::methods]
impl DomCssImportRule {
    #[getter]
    fn href(&self) -> OpResult<String> {
        Ok(
            source_import(&self.base.realm, &self.base.source, &self.base.index)?
                .url
                .to_string(),
        )
    }

    #[getter]
    fn media(&self, ctx: &mut Ctx,this:This<Value>) -> OpResult<Value> {
        import_rule_media(ctx,&this.0)
    }

    #[getter(name = "supportsText")]
    fn supports_text(&self) -> OpResult<String> {
        Ok(
            source_import(&self.base.realm, &self.base.source, &self.base.index)?
                .supports
                .map_or_else(String::new, |supports| supports.to_string()),
        )
    }

    #[getter(name = "layerName")]
    fn layer_name(&self) -> OpResult<Nullable<String>> {
        Ok(Nullable(
            source_import(&self.base.realm, &self.base.source, &self.base.index)?
                .layer
                .map(|layer| match layer {
                    css::ImportLayer::Anonymous => String::new(),
                    css::ImportLayer::Named(name) => name.to_string(),
                }),
        ))
    }

    #[getter(name = "styleSheet")]
    fn style_sheet(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        if let Some(value) = self
            .stylesheet_wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
        {
            return Ok(value);
        }
        let _ = source_import(&self.base.realm, &self.base.source, &self.base.index)?;
        let node = source_dom_node(&self.base.realm, &self.base.source);
        if matches!(self.base.source.root(), SheetSource::Constructed(_)) { return Ok(Value::Null); }
        let mut parent = self.base.source.root();
        let mut depth = 0;
        while let SheetSource::Imported(data) = parent {
            depth += 1;
            if depth >= 32 { return Err(OpError::new("QuotaExceededError", "CSS import depth limit")); }
            parent = data.parent.root();
        }
        let existing = self.base.index.imported_sheet.borrow().clone();
        let data = existing.unwrap_or_else(|| Rc::new(ImportedSheetData {
            owner: node,
            parent: self.base.source.root().clone(),
            owner_position: Rc::downgrade(&self.base.index),
            detached_graph: RefCell::new(None),
            lease: RefCell::new(None),
            positions: Rc::new(RulePositions { read_cache: self.base.source.positions().read_cache.clone(), ..Default::default() }),
        }));
        let Some(href) = data.with_graph(&self.base.realm, &mut |graph| Ok(graph.map(|graph| graph.url.to_string())))? else { return Ok(Value::Null); };
        if data.lease.borrow().is_none() {
            if let Some(path) = data.live_path(&self.base.realm)? {
                let lease = self.base.realm.session.borrow_mut().stylesheet_import_lease(node, path)
                    .map_err(|error| OpError::new("QuotaExceededError", format!("import graph lease failed: {error:?}")))?;
                *data.lease.borrow_mut() = Some(lease);
            }
        }
        *self.base.index.imported_sheet.borrow_mut() = Some(data.clone());
        let origin_metadata=ctx.with_instance::<DomCssStyleSheet,_>(&self.base.sheet_owner,|sheet|sheet.origin_metadata.clone())
            .unwrap_or_else(|_|Rc::new(RefCell::new(Arc::from([]))));
        let origin_metadata=Rc::new(RefCell::new(origin_metadata.borrow().clone()));
        let origin_clean=origin_metadata.borrow().iter().find(|(url,_)|url.as_ref()==href).is_none_or(|(_,clean)|*clean);
        let value = ctx.new_instance(DomCssStyleSheet {
            parent_sheet: self.base.sheet_owner.clone(),
            realm: self.base.realm.clone(),
            source: SheetSource::Imported(data),
            owner: Value::Null,
            owner_rule: this.0,
            href_value: Some(href),
            rule_list_wrapper: RefCell::new(None),
            origin_clean,
            origin_metadata,
        });
        *self.stylesheet_wrapper.borrow_mut() = ctx.weak_value(&value);
        Ok(value)
    }
}

#[lumen_bind::methods]
impl DomCssMediaList {
    #[getter(name = "mediaText")]
    fn media_text(&self) -> OpResult<String> {
        if let Some(index)=&self.index {return Ok(source_import(&self.realm,&self.source,index)?.media.map_or_else(String::new,|media|media.to_string()));}
        if let SheetSource::Element{node,lease,..}=self.source.root() {
            if let Some(metadata)=lease.metadata() {return Ok(metadata.media.borrow().to_string());}
            let (realm,node)=self.realm.resolve_adopted_node(*node);
            return Ok(realm.session.borrow().document().get_attribute_ns(node,None,"media").map_err(dom_error)?.unwrap_or_default());
        }
        let SheetSource::Constructed(data)=self.source.root() else {return Err(OpError::new("InvalidStateError","sheet media unavailable"));};
        Ok(data.media.borrow().clone())
    }

    #[setter(name = "mediaText", coerce)]
    fn set_media_text(&self, value: &str) -> OpResult<()> {
        let value=css::media_query_list_items(value).join(", ");
        if let Some(index)=&self.index {
            let mut sheet=source_sheet(&self.realm,&self.source)?;let before=sheet.clone();
            sheet.set_import_media(index.get(),&value).map_err(css_error)?;
            return commit_rule_declaration(&self.realm,&self.source,&before,&sheet,&[],None);
        }
        if let SheetSource::Element{node,lease,..}=self.source.root() {
            if let Some(metadata)=lease.metadata() {
                return self.realm.session.borrow_mut().set_stylesheet_media(*node,metadata,&value)
                    .map_err(|error|OpError::new("QuotaExceededError",format!("stylesheet media: {error:?}")));
            }
            let (realm,node)=self.realm.resolve_adopted_node(*node);
            realm.session.borrow_mut().document_mut().set_attribute_ns(node,None,"media",&value).map_err(dom_error)?;
            return Ok(());
        }
        let SheetSource::Constructed(data)=self.source.root() else {return Err(OpError::new("InvalidStateError","sheet media unavailable"));};
        let previous=core::mem::replace(&mut *data.media.borrow_mut(),value);
        if let Some(registry)=data.registry.upgrade() {if let Err(error)=sync_adopted(&registry,&self.realm) {*data.media.borrow_mut()=previous;return Err(error);}}
        Ok(())
    }

    #[method(coerce)]
    fn append_medium(&self,medium:&str)->OpResult<()> {
        let parsed=css::media_query_list_items(medium);if parsed.len()!=1 {return Ok(());}
        let mut media=css::media_query_list_items(&self.media_text()?);
        if !media.contains(&parsed[0]) {media.push(parsed[0].clone());self.set_media_text(&media.join(", "))?;}Ok(())
    }
    #[method(coerce)]
    fn delete_medium(&self,ctx:&mut Ctx,medium:&str)->OpResult<()> {
        let parsed=css::media_query_list_items(medium);if parsed.len()!=1 {return Ok(());}
        let mut media=css::media_query_list_items(&self.media_text()?);let before=media.len();media.retain(|value|value!=&parsed[0]);
        if before==media.len() {return Err(crate::error_reporting::dom_exception(ctx,"NotFoundError","media query is absent"));}
        self.set_media_text(&media.join(", "))
    }
    #[proto(len)]
    fn length(&self) -> OpResult<usize> {
        Ok(css::media_query_list_items(&self.media_text()?).len())
    }

    #[proto(getitem)]
    fn indexed(&self, index: usize) -> OpResult<Value> {
        Ok(css::media_query_list_items(&self.media_text()?)
            .get(index)
            .map_or(Value::Undefined, |item| Value::from_string(item.clone())))
    }

    fn item(&self, index: usize) -> OpResult<String> {
        Ok(css::media_query_list_items(&self.media_text()?)
            .get(index)
            .cloned()
            .unwrap_or_default())
    }
}

#[lumen_bind::methods]
impl DomCssRuleStyle {
    #[proto(iter)]
    fn values(this: This<Value>) -> DomCssomCollectionIterator {
        DomCssomCollectionIterator::new(this.0)
    }

    #[getter(name = "parentRule")]
    fn parent_rule(&self) -> Value {
        self.owner.clone()
    }

    #[proto(len)]
    fn length(&self) -> OpResult<usize> {
        let declaration = self.read_declaration()?;
        Ok(match declaration.block() { Some(block) => block.len(), None => css::descriptor_declaration_names(declaration.descriptor_text()).map_err(css_error)?.len() })
    }

    fn item(&self, index: usize) -> OpResult<String> {
        let declaration = self.read_declaration()?;
        Ok(match declaration.block() { Some(block) => block.names().nth(index).unwrap_or("").to_owned(), None => css::descriptor_declaration_names(declaration.descriptor_text()).map_err(css_error)?.get(index).cloned().unwrap_or_default() })
    }

    #[proto(getitem)]
    fn indexed(&self, index: usize) -> OpResult<Value> {
        let declaration = self.read_declaration()?;
        Ok(match declaration.block() {
            Some(block) => block.names().nth(index).map_or(Value::Undefined, |name| Value::from_string(name.to_owned())),
            None => css::descriptor_declaration_names(declaration.descriptor_text()).map_err(css_error)?.get(index).map_or(Value::Undefined, |name| Value::from_string(name.clone())),
        })
    }

    #[getter(name = "cssText")]
    fn css_text(&self) -> OpResult<String> {
        if let Some(frame_index) = self.keyframe_index.as_ref() {
            return Ok(source_keyframe(&self.realm, &self.source, &self.index, frame_index)?.style);
        }
        match self.read_declaration()? {
            ReadCssDeclaration::Parsed(declaration) => Ok(declaration.css_text().to_owned()),
            ReadCssDeclaration::Retained(block) => block.serialize().map_err(css_error),
        }
    }

    #[setter(name = "cssText", coerce, hint(js(ce_reactions)))]
    fn set_css_text(&self, value: &str) -> OpResult<()> {
        let location = rule_location(&self.realm, &self.source, &self.path)?;
        let before = source_sheet(&self.realm, &location.source)?;
        let mut sheet = before.clone();
        if let Some(frame_index) = self.keyframe_index.as_ref() {
            return write_keyframe_style(
                &self.realm,
                &location.source,
                &self.index,
                frame_index,
                value,
            );
        }
        sheet
            .set_rule_style(&location.indices.clone(), value)
            .map_err(css_error)?;
        let path = location.indices.clone();
        let rule = sheet.rule_at_path(&path).map_err(css_error)?;
        commit_rule_declaration(&self.realm, &location.source, &before, &sheet, &path,
            rule.style.block)?;
        let updated = source_sheet(&self.realm, &location.source)?;
        remember_rule_path(&location.source, &updated, &self.path)
    }

    fn get_property_value(&self, name: &str) -> OpResult<String> {
        let declaration = self.read_declaration()?;
        let name = if declaration.block().is_none() && name.eq_ignore_ascii_case("font-stretch") {
            "font-width"
        } else {
            name
        };
        Ok(declaration
            .value(name)
            .map_err(css_error)?
            .map_or(String::new(), |(value, _)| {
                super::style::canonical_content_property(name, value)
            }))
    }

    fn get_property_priority(&self, name: &str) -> OpResult<String> {
        Ok(if self.read_declaration()?
            .value(name)
            .map_err(css_error)?
            .is_some_and(|(_, important)| important)
        {
            "important"
        } else {
            ""
        }
        .into())
    }

    #[method(coerce, hint(js(ce_reactions)))]
    fn set_property(&self, name: &str, value: &str, priority: Option<&str>) -> OpResult<()> {
        let location = rule_location(&self.realm, &self.source, &self.path)?;
        let before = source_sheet(&self.realm, &location.source)?;
        let mut sheet = before.clone();
        if let Some(frame_index) = self.keyframe_index.as_ref() {
            let frame = source_keyframe(&self.realm, &location.source, &self.index, frame_index)?;
            let mut declaration = CssDeclaration::parse(&frame.style);
            let priority = priority.unwrap_or("");
            if !value.is_empty() && !priority.is_empty() && !priority.eq_ignore_ascii_case("important") {
                return Ok(());
            }
            if !value.is_empty() && priority.eq_ignore_ascii_case("important") {
                return Ok(());
            }
            declaration
                .set_property(name, value, !priority.is_empty())
                .map_err(css_error)?;
            return write_keyframe_style(
                &self.realm,
                &location.source,
                &self.index,
                frame_index,
                declaration.css_text(),
            );
        }
        let rule = sheet
            .rule_at_path(&location.indices.clone())
            .map_err(css_error)?;
        let name = if rule.font_face && name.eq_ignore_ascii_case("font-stretch") {
            "font-width"
        } else {
            name
        };
        let priority = priority.unwrap_or("");
        if rule.font_face {
            if !value.is_empty() && !priority.is_empty() {
                return Ok(());
            }
            if !value.is_empty() {
                let candidate = css::set_descriptor_declaration("", name, value, false).map_err(css_error)?;
                if css::cssom_font_face_declaration_text(&candidate).is_empty() {
                    return Ok(());
                }
            }
        }
        if !value.is_empty() && !priority.is_empty() && !priority.eq_ignore_ascii_case("important") {
            return Ok(());
        }
        let mut declaration = rule.style;
        declaration.set_property(name, value, !priority.is_empty()).map_err(css_error)?;
        sheet
            .set_rule_style(&location.indices.clone(), declaration.css_text())
            .map_err(css_error)?;
        commit_rule_declaration(&self.realm, &location.source, &before, &sheet,
            &location.indices.clone(), declaration.block)?;
        let updated = source_sheet(&self.realm, &location.source)?;
        remember_rule_path(&location.source, &updated, &self.path)
    }

    fn remove_property(&self, name: &str) -> OpResult<String> {
        let old = self.get_property_value(name)?;
        self.set_property(name, "", None)?;
        Ok(old)
    }

    #[getter]
    fn width(&self) -> OpResult<String> {
        self.get_property_value("width")
    }
    #[setter]
    fn set_width(&self, value: &str) -> OpResult<()> {
        self.set_property("width", value, None)
    }
    #[getter]
    fn height(&self) -> OpResult<String> {
        self.get_property_value("height")
    }
    #[setter]
    fn set_height(&self, value: &str) -> OpResult<()> {
        self.set_property("height", value, None)
    }
    #[getter]
    fn color(&self) -> OpResult<String> {
        self.get_property_value("color")
    }
    #[setter]
    fn set_color(&self, value: &str) -> OpResult<()> {
        self.set_property("color", value, None)
    }
    #[getter]
    fn display(&self) -> OpResult<String> {
        self.get_property_value("display")
    }
    #[setter]
    fn set_display(&self, value: &str) -> OpResult<()> {
        self.set_property("display", value, None)
    }
    #[getter]
    fn background_color(&self) -> OpResult<String> {
        self.get_property_value("background-color")
    }
    #[setter]
    fn set_background_color(&self, value: &str) -> OpResult<()> {
        self.set_property("background-color", value, None)
    }
    #[getter]
    fn font_size(&self) -> OpResult<String> {
        self.get_property_value("font-size")
    }
    #[setter]
    fn set_font_size(&self, value: &str) -> OpResult<()> {
        self.set_property("font-size", value, None)
    }
    #[getter]
    fn font_family(&self) -> OpResult<String> {
        self.get_property_value("font-family")
    }
    #[setter]
    fn set_font_family(&self, value: &str) -> OpResult<()> {
        self.set_property("font-family", value, None)
    }
    #[getter]
    fn font_weight(&self) -> OpResult<String> {
        self.get_property_value("font-weight")
    }
    #[setter]
    fn set_font_weight(&self, value: &str) -> OpResult<()> {
        self.set_property("font-weight", value, None)
    }
    #[getter]
    fn font_style(&self) -> OpResult<String> {
        self.get_property_value("font-style")
    }
    #[setter]
    fn set_font_style(&self, value: &str) -> OpResult<()> {
        self.set_property("font-style", value, None)
    }
    #[getter]
    fn src(&self) -> OpResult<String> {
        self.get_property_value("src")
    }
    #[setter]
    fn set_src(&self, value: &str) -> OpResult<()> {
        self.set_property("src", value, None)
    }
    #[getter]
    fn unicode_range(&self) -> OpResult<String> {
        self.get_property_value("unicode-range")
    }
    #[setter]
    fn set_unicode_range(&self, value: &str) -> OpResult<()> {
        self.set_property("unicode-range", value, None)
    }
    #[getter]
    fn font_display(&self) -> OpResult<String> {
        self.get_property_value("font-display")
    }
    #[setter]
    fn set_font_display(&self, value: &str) -> OpResult<()> {
        self.set_property("font-display", value, None)
    }
    #[getter]
    fn font(&self) -> OpResult<String> {
        self.get_property_value("font")
    }
    #[setter]
    fn set_font(&self, value: &str) -> OpResult<()> {
        self.set_property("font", value, None)
    }
}

impl DomCssRuleStyle {
    fn read_declaration(&self) -> OpResult<ReadCssDeclaration> {
        let location = rule_location(&self.realm, &self.source, &self.path)?;
        if let Some(frame) = self.keyframe_index.as_ref() {
            let frame = source_keyframe(&self.realm, &location.source, &self.index, frame)?;
            return Ok(ReadCssDeclaration::Parsed(Rc::new(CssDeclaration::parse(&frame.style))));
        }
        with_source_text(&self.realm, &location.source, &mut |text| {
        let path = location.indices.clone();
        let positions = location.source.positions();
        if location.source.detached_position().is_some() || positions.text.borrow().as_deref() == Some(text) {
            if let Some((_, block)) = location.source.declaration_state().borrow().iter().find(|(known, _)| known.as_ref() == path.as_slice()) {
                return Ok(ReadCssDeclaration::Retained(block.clone()));
            }
        }
        if !self.realm.session.borrow().cssom_rule_overrides().is_empty() {
            let identities = source_identities(&self.realm, &location.source, text);
            let session = self.realm.session.borrow();
            if let Some(entry) = session.cssom_rule_overrides().iter().find(|entry|
                entry.source_text.as_ref() == text && entry.cssom_path.as_ref() == path.as_slice() &&
                identities.iter().any(|(owner, url)| *owner == entry.owner && *url == entry.source_url)) {
                return Ok(ReadCssDeclaration::Retained(entry.block.clone()));
            }
        }
        let detached = location.source.detached_position();
        if detached.is_some_and(|position| position.nested_declarations.get()) {
            return Ok(ReadCssDeclaration::Parsed(Rc::new(CssDeclaration::parse(text))));
        }
        let nested_context = detached.is_some_and(|position| position.owner.nested_context);
        let scoped_context = detached.is_some_and(|position| position.owner.scoped_context);
        // One tree and one last declaration are shared by all stylesheet owners
        // in this realm. Exact source checks keep external DOM edits coherent.
        let mut cache = positions.read_cache.borrow_mut();
        let boundaries = location.source.boundary_state().borrow().clone();
        let cached = cssom_read_cache(&mut cache, text, boundaries, nested_context, scoped_context,detached.map(|position|position.detached_namespaces.borrow().clone()))?;
        if let Some((known, declaration)) = &cached.declaration {
            if *known == path { return Ok(ReadCssDeclaration::Parsed(declaration.clone())); }
        }
        let rule = css::nesting::source_rule_at_path(&cached.rules, &path)
            .ok_or_else(|| OpError::new("InvalidStateError", "stylesheet rule was removed"))?;
        let raw = rule.declaration_source(&cached.text);
        let declaration = Rc::new(if rule.kind == css::nesting::SourceRuleKind::FontFace {
            CssDeclaration::descriptors(&raw)
        } else { CssDeclaration::parse(&raw) });
        cached.declaration = Some((path, declaration.clone()));
        Ok(ReadCssDeclaration::Parsed(declaration))
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn specification_readonly_style_maps_share_live_values_iteration_and_registry_epoch() {
        let mut engine=lumen::Engine::new();
        let realm=crate::install(engine.ctx(),r#"<style id=registry>@property --items {syntax: '<length>+';inherits:false;initial-value:1px 2px}</style><div id=parent style='--inherited:parent'><div id=target style='--Z:z; width:50%; --A:a; transition-duration:1s, 2s; --empty:;'></div></div>"#,512).unwrap();
        realm.set_layout_flusher(Rc::new(|session|session.display_list(200,100,crate::canvas::canvas_fallback_fonts()).map(|_|()).map_err(|error|format!("{error:?}"))));
        let result=engine.eval_value(r#"(()=>{
            const target=document.getElementById('target'),map=target.computedStyleMap();
            const check=(condition,message)=>{if(!condition)throw Error(message)};
            check(target.computedStyleMap()===map,'same object');
            check(map.get('WiDtH').unit==='percent'&&map.get('width').value===50,'computed percentage');
            for(const operation of ['get','getAll','has']){let threw=false;try{map[operation]('unrecognized-lumen-property')}catch(error){threw=error instanceof TypeError}check(threw,'invalid '+operation)}
            check(map.get('--Missing')===undefined&&map.getAll('--Missing').length===0&&!map.has('--Missing'),'missing custom');
            for(const name of ['--','--a b','--a\nb',String.raw`--\41`])check(map.get(name)===undefined&&map.getAll(name).length===0&&!map.has(name),'literal custom property string');
            check(map.get('--A') instanceof CSSUnparsedValue && map.get(String.raw`--\41`)===undefined,'custom lookup does not decode CSS escapes');
            for(const operation of ['get','getAll','has']){let threw=false;try{map[operation](String.raw`\77 idth`)}catch(error){threw=error instanceof TypeError}check(threw,'escaped standard lookup '+operation)}
            check(map.get('--empty') instanceof CSSUnparsedValue && map.get('--empty').length===0 && map.has('--empty'),'present empty custom');
            const entries=Array.from(map),keys=Array.from(map.keys()),values=Array.from(map.values());
            check(entries.length===map.size&&keys.length===map.size&&values.length===map.size,'size');
            check(entries.every((entry,index)=>entry[0]===keys[index]&&Array.isArray(entry[1])&&entry[1].length>0&&entry[1].every(value=>value instanceof CSSStyleValue)),'iterable shape and all catalog values');
            check(map.get('rx').value==='auto'&&map.get('ry').value==='auto','SVG auto radius grammar');
            check(keys.includes('--inherited')&&!keys.includes('margin'),'canonical longhands and inherited customs');
            const category=name=>name.startsWith('--')?2:name.startsWith('-')?1:0;
            check(keys.every((name,index)=>index===0||category(keys[index-1])<category(name)||category(keys[index-1])===category(name)&&keys[index-1]<name),'ordering');
            const itemIndex=keys.indexOf('--items');check(values[itemIndex].length===2&&values[itemIndex][1].value===2,'registered repeated reification');
            const durationIndex=keys.indexOf('transition-duration');check(values[durationIndex].length===2&&values[durationIndex][1].value===2,'ordinary list reification');
            target.style.setProperty('--豈','BMP');target.style.setProperty('--💩','nonBMP');const unicodeKeys=Array.from(map.keys());check(unicodeKeys.indexOf('--豈')<unicodeKeys.indexOf('--💩'),'modern code-point ordering');
            const receiver={};let called=0;map.forEach(function(items,name,owner){check(this===receiver&&owner===map&&Array.isArray(items),'callback');called++},receiver);check(called===map.size,'callback count');
            const oldItems=entries[itemIndex][1];document.getElementById('registry').textContent="@property --items {syntax:'<number>';inherits:false;initial-value:7}";
            check(map.get('--items').unit==='number'&&map.get('--items').value===7&&oldItems[1].unit==='px'&&oldItems[1].value===2,'new registry and frozen earlier values');
            target.style.setProperty('--A','updated');check(String(map.get('--A'))==='updated','live mutation');
            target.remove();check(map.size===0&&Array.from(map).length===0&&map.get('width')===undefined,'disconnected declarations');
            document.body.appendChild(target);check(map.get('width').unit==='percent','reconnected same map');
            const inline=target.attributeStyleMap,inlineKeys=Array.from(inline.keys());
            check(inlineKeys[0]==='--Z'&&inlineKeys[1]==='width'&&inlineKeys[2]==='--A','inline declaration order');
            check(inline.getAll('transition-duration').length===2&&inline.size===inlineKeys.length&&Array.from(inline).length===inline.size,'inline list and size');
            return true;
        })()"#).unwrap();
        assert!(matches!(result,Ok(Value::Bool(true))),"readonly map lifecycle");
    }
    #[test]
    fn specification_individual_transform_cssom_owner_and_animation(){
        let mut runtime=lumen_runtime::Runtime::new_browser();let engine=runtime.engine();
        let realm=crate::install(engine.ctx(),"<div id=target style='width:200px;height:100px'></div>",128).unwrap();install_cssom_layout(&realm);
        let result=engine.eval_value(r#"(()=>{const t=document.getElementById('target'),s=t.style;const check=(ok,message)=>{if(!ok)throw Error(message)};
            s.translate='50%';s.rotate='400grad -0.5 0 0';s.scale='calc(200%)';
            check(s.translate==='50%'&&s.rotate==='x -400grad'&&s.scale==='calc(200%)','specified canonical trees');
            check(getComputedStyle(t).translate==='50%'&&getComputedStyle(t).rotate==='x -360deg'&&getComputedStyle(t).scale==='2','computed independent values');
            s.rotate='none';s.scale='progress(5%,0%,10%)';check(getComputedStyle(t).scale==='0.5','shared progress');
            const a=t.animate({scale:['none','3']},{duration:1000,fill:'both'});a.currentTime=500;check(getComputedStyle(t).scale==='2','none interpolation identity');a.cancel();
            s.scale='2';const b=t.animate({scale:['5','5']},{duration:1000,fill:'both',composite:'add'});b.currentTime=500;check(getComputedStyle(t).scale==='10','scale multiplicative addition');b.cancel();
            const c=t.animate({rotate:['y 0deg','y 720deg']},{duration:1000,fill:'both'});c.currentTime=250;check(getComputedStyle(t).rotate==='y 180deg','unwrapped matched-axis rotation');c.cancel();
            s.all='initial';check(getComputedStyle(t).translate==='none'&&getComputedStyle(t).rotate==='none'&&getComputedStyle(t).scale==='none','registry reset');return true})()"#);
        assert!(matches!(result,Ok(Ok(Value::Bool(true)))),"individual transform CSSOM");
    }

    #[test]
    fn specification_sibling_numeric_typed_om_and_owner_values() {
        let mut runtime=lumen_runtime::Runtime::new_browser();let engine=runtime.engine();
        let realm=crate::install(engine.ctx(),"<section style='--tokens:sibling-index()'><div></div>text<div id=target></div></section>",128).unwrap();install_cssom_layout(&realm);
        let result=engine.eval_value(r#"(()=>{const target=document.getElementById('target');target.style.cssText='width:calc(10px * var(--tokens));height:calc(10px * sibling-count());background-image:image-set(url("x") calc(1dppx * sibling-index()))';if(getComputedStyle(target).width!=='20px'||getComputedStyle(target).height!=='20px')throw Error('owner index/count');if(!target.style.backgroundImage.includes('sibling-index()'))throw Error('specified unresolved image');const value=CSSStyleValue.parse('width','calc(10px * sibling-index())');if(!(value instanceof CSSStyleValue)||value instanceof CSSNumericValue||!String(value).includes('sibling-index()'))throw Error('typed unresolved source');let threw=false;try{CSSNumericValue.parse('sibling-index()')}catch(e){threw=e.name==='SyntaxError'}if(!threw)throw Error('numeric context-free parser');target.parentNode.appendChild(document.createElement('div'));if(getComputedStyle(target).height!=='30px')throw Error('sibling mutation');return true})()"#);
        assert!(matches!(result,Ok(Ok(Value::Bool(true)))));
    }

    #[test]
    fn specification_cssom_border_image_uses_canonical_longhands_resets_and_live_rule_identity() {
        let mut engine=lumen::Engine::new();let realm=crate::install(engine.ctx(),"<div id=subject></div>",256).unwrap();install_cssom_layout(&realm);
        let result=engine.eval_value(r#"(() => {
            const check=(ok,message)=>{if(!ok)throw Error(message)};
            const style=document.getElementById('subject').style;
            style.borderImage='linear-gradient(red, blue) 10% fill / 2 auto / 3px round space';
            check(style.borderImageSlice==='10% fill','slice canonical serialization');
            check(style.borderImageWidth==='2 auto','width canonical serialization');
            check(style.borderImageOutset==='3px','outset canonical serialization');
            check(style.borderImageRepeat==='round space','axis repeat serialization');
            check(style.borderImage.includes('linear-gradient'),'real shorthand source');
            style.border='1px solid red';
            check(style.borderImageSource==='none' && style.borderImageWidth==='1' && style.borderImageSlice==='100%','border shorthand resets all five image components');
            style.borderImageWidth='2em';check(style.borderImageWidth==='2em','specified font-relative units are preserved');
            style.borderImageWidth='25%';style.borderImageOutset='-2px';
            check(style.borderImageWidth==='25%' && style.borderImageOutset==='0','invalid outset leaves canonical reset untouched');
            style.all='initial';check(style.borderImageSource==='initial' && style.borderImageRepeat==='initial','all registry includes every new component');
            style.borderImage='linear-gradient(green, blue) 1 / 2px';
            const computed=getComputedStyle(document.getElementById('subject'));
            check(computed.borderImageWidth==='2px' && computed.borderImageRepeat==='stretch','computed metadata shares typed state: width='+computed.borderImageWidth+' repeat='+computed.borderImageRepeat+' source='+computed.borderImageSource+' slice='+computed.borderImageSlice+' authored='+style.cssText);
            const sheet=new CSSStyleSheet();sheet.replaceSync('div{border-image:linear-gradient(red,blue) 1 / 3px}');
            const rule=sheet.cssRules[0],declarations=rule.style;
            declarations.borderImageRepeat='round';
            check(sheet.cssRules[0]===rule && rule.style===declarations && rule.cssText.includes('round'),'live rule edit retains identity and actual source');
            return true;
        })()"#).unwrap();
        if let Err(error)=result {let message=engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_else(|_|"unprintable exception".into());panic!("border image CSSOM: {message}");}
    }
    #[test]
    fn specification_cssom_all_removed_component_family_restores_real_ua_fallback(){
        let mut engine=lumen::Engine::new();let realm=crate::install(engine.ctx(),"<input id=subject type=button value=label>",128).unwrap();install_cssom_layout(&realm);
        let result=engine.eval_value(r#"(() => {
            const element=document.getElementById('subject'),style=element.style;
            style.borderStyle='solid';const expected=getComputedStyle(element).borderTopWidth;
            if(parseFloat(expected)<=0)throw Error('fixture needs actual positive UA width');
            style.all='initial';style.borderStyle='solid';
            const initial=getComputedStyle(element).borderTopWidth;
            if(initial!=='3px')throw Error('CSS medium initial width '+initial);
            style.borderStyle='none';if(getComputedStyle(element).borderTopWidth!=='0px')throw Error('none suppresses used width');style.borderStyle='solid';
            style.removeProperty('border-width');
            if(style.borderInlineStartWidth!=='initial' || style.borderBlockStartWidth!=='initial' || getComputedStyle(element).borderTopWidth!==initial)throw Error('remaining logical reset '+JSON.stringify([initial,getComputedStyle(element).borderTopWidth,style.cssText]));
            for(const name of ['border-inline-start-width','border-inline-end-width','border-block-start-width','border-block-end-width'])style.removeProperty(name);
            if(getComputedStyle(element).borderTopWidth!==expected)throw Error('aggregate reset survived complete component removal '+JSON.stringify([expected,getComputedStyle(element).borderTopWidth,style.cssText]));
            style.all='initial';style.borderStyle='solid';style.removeProperty('border-top-width');
            if(getComputedStyle(element).borderTopWidth!==initial)throw Error('surviving logical width reset disappeared');
            for(const name of ['border-inline-start-width','border-inline-end-width','border-block-start-width','border-block-end-width'])style.removeProperty(name);
            const computed=getComputedStyle(element);
            if(computed.borderTopWidth!==expected || computed.borderRightWidth!==initial)throw Error('sparse component removal lost ordinary fallback or other resets '+JSON.stringify([expected,initial,computed.borderTopWidth,computed.borderRightWidth,style.cssText]));
            return true;
        })()"#).unwrap();
        if let Err(error)=result{let message=engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_else(|_|"unprintable exception".into());panic!("all metadata projection: {message}");}
    }

    #[test]
    fn specification_svg_style_element_uses_shared_associated_stylesheet_lifecycle() {
        let mut engine=lumen::Engine::new();let realm=crate::install(engine.ctx(),"<!doctype html><body></body>",512).unwrap();install_cssom_layout(&realm);
        let result=engine.eval_value(r#"(() => {
const check=(value,message)=>{if(!value)throw Error(message)};
const ns='http://www.w3.org/2000/svg';
const owner=document.createElementNS(ns,'style');
check(owner instanceof SVGStyleElement && owner instanceof SVGElement && !(owner instanceof HTMLStyleElement),'native SVG identity');
check(owner.type==='' && owner.media==='' && owner.title==='' && owner.sheet===null && owner.disabled===false,'absent reflected attrs and disconnected association');
for(const attribute of ['type','media','title']) {owner[attribute]='arbitrary/value';check(owner.getAttribute(attribute)==='arbitrary/value','IDL reflection '+attribute);owner.setAttribute(attribute,'content');check(owner[attribute]==='content','content reflection '+attribute);owner.removeAttribute(attribute);check(owner[attribute]==='','reflection removal '+attribute);}
const svg=document.createElementNS(ns,'svg'),rect=document.createElementNS(ns,'rect');svg.setAttribute('width','100');svg.setAttribute('height','100');rect.setAttribute('class','target');rect.setAttribute('width','50');rect.setAttribute('height','50');owner.textContent='.target {fill:red}';svg.append(owner,rect);document.body.append(svg);
owner.style.setProperty('display','block','important');check(getComputedStyle(owner).display==='none','mandatory never-rendered SVG style UA rule');
const sheet=owner.sheet;
check(sheet instanceof CSSStyleSheet && sheet===owner.sheet && sheet===document.styleSheets[0] && sheet.ownerNode===owner && sheet.cssRules.length===1,'shared associated sheet identity');
check(getComputedStyle(rect).fill==='rgb(255, 0, 0)','SVG source rule applies');
owner.disabled=true;check(owner.disabled && sheet.disabled && getComputedStyle(rect).fill==='rgb(0, 0, 0)','owner disabling controls actual renderer');
sheet.disabled=false;check(!owner.disabled && getComputedStyle(rect).fill==='rgb(255, 0, 0)','sheet and owner share disabled source');
owner.media='not all';check(getComputedStyle(rect).fill==='rgb(0, 0, 0)','media mutation deactivates actual source');owner.removeAttribute('media');check(getComputedStyle(rect).fill==='rgb(255, 0, 0)','media removal restores source');
sheet.cssRules[0].style.fill='blue';check(getComputedStyle(rect).fill==='rgb(0, 0, 255)','shared CSSOM declaration edit drives renderer');
owner.type='no/mime';check(owner.sheet===null && document.styleSheets.length===0 && getComputedStyle(rect).fill==='rgb(0, 0, 0)','type mutation removes association');owner.type='text/css';check(owner.sheet instanceof CSSStyleSheet && document.styleSheets.length===1 && getComputedStyle(rect).fill==='rgb(255, 0, 0)','valid type rebuilds authored source');
const prefixed=document.createElementNS(ns,'s:style');prefixed.textContent='.target{stroke:green}';svg.append(prefixed);check(prefixed.sheet instanceof CSSStyleSheet && prefixed.sheet===document.styleSheets[1] && getComputedStyle(rect).stroke==='rgb(0, 128, 0)','prefixed XML style shares maintained owner classification');prefixed.remove();check(document.styleSheets.length===1 && getComputedStyle(rect).stroke==='none','prefixed source removal follows the same lifecycle');
owner.remove();check(owner.sheet===null && document.styleSheets.length===0 && getComputedStyle(rect).fill==='rgb(0, 0, 0)','detached source cannot control live rendering');svg.prepend(owner);check(owner.sheet===document.styleSheets[0] && getComputedStyle(rect).fill==='rgb(255, 0, 0)','reconnection restores shared lifecycle');
return true;})()"#).unwrap();
        if let Err(error)=result {let message=engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_else(|_|"unprintable exception".into());panic!("SVG stylesheet lifecycle: {message}");}
    }

    #[test]
    fn specification_cssom_all_live_inline_rule_and_detached_declarations(){
        let mut engine=lumen::Engine::new();let realm=crate::install(engine.ctx(),"<div id=subject></div>",256).unwrap();install_cssom_layout(&realm);
        let result=engine.eval_value(r#"(() => {
            const check=(value,message)=>{if(!value)throw Error(message)};
            const element=document.getElementById('subject'),style=element.style;
            style.cssText='direction:rtl;unicode-bidi:isolate;--retained:green;all:initial';
            const names=Array.from({length:style.length},(_,index)=>style.item(index));
            check(style.length>128 && names.includes('width') && !names.includes('all'),'virtual CSSOM longhand enumeration');
            const widthPosition=names.indexOf('width');
            style.width='37px';style.color='green';
            const updatedNames=Array.from({length:style.length},(_,index)=>style.item(index)),newWidth=updatedNames.indexOf('width');
            check(newWidth>updatedNames.indexOf('inline-size') && newWidth>updatedNames.indexOf('block-size') && newWidth>widthPosition && style.all==='' && style.font==='initial','CSSOM logical mapping position and unaffected shorthand reset');
            check(getComputedStyle(element).width==='37px' && getComputedStyle(element).color==='rgb(0, 128, 0)','actual inline rendering '+JSON.stringify([getComputedStyle(element).width,getComputedStyle(element).color,style.cssText]));
            const serialized=style.cssText;style.cssText=serialized;
            check(style.width==='37px' && style.direction==='rtl' && style.unicodeBidi==='isolate' && style.getPropertyValue('--retained')==='green','partial reset source roundtrip');
            style.setProperty('all','inherit','important');
            check(style.all==='inherit' && style.width==='inherit' && style.getPropertyPriority('width')==='important','setter overrides previous per-longhand priority');
            check(style.direction==='rtl' && style.unicodeBidi==='isolate' && style.getPropertyValue('--retained')==='green','all exclusions');
            check(style.removeProperty('all')==='inherit' && style.length===3 && style.width==='','all removal and excluded declarations');
            const sheet=new CSSStyleSheet();sheet.replaceSync('div{all:initial;width:19px!important}');
            document.adoptedStyleSheets=[sheet];const rule=sheet.cssRules[0],declaration=rule.style;
            check(declaration.width==='19px' && declaration.getPropertyPriority('width')==='important' && declaration.all==='','source priority winner');
            declaration.setProperty('all','unset');
            check(declaration.all==='unset' && declaration.width==='unset' && declaration.getPropertyPriority('width')==='','CSSOM setter replaces important winner');
            declaration.width='41px';
            check(getComputedStyle(element).width==='41px','live adopted declaration renderer');
            sheet.deleteRule(0);const detached=rule.style;
            detached.setProperty('all','initial');detached.width='53px';
            check(rule.parentStyleSheet===null && detached===declaration && detached.width==='53px' && detached.font==='initial','detached reset state and identity');
            check(getComputedStyle(element).width!=='53px','detached reset cannot affect live rendering');
            return true;
        })()"#).unwrap();
        if let Err(error)=result{let message=engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_else(|_|"unprintable exception".into());panic!("all CSSOM pipeline: {message}");}
    }

    #[test]
    fn specification_cssom_serialization_rule_and_declaration_roundtrips_keep_escaped_tokens() {
        // Use the actual escaped-token spellings from the upstream roundtrip
        // fixture. Unsupported rule interfaces remain outside this assertion.
        let source = r#"
            @import 'abc' layer(\{\});
            @counter-style abc\{\}oops {}
            @font-feature-values abc\{\}oops {}
            @font-palette-values --abc\{\}oops {}
            @keyframes abc\{\}oops {}
            @layer abc\;oops\!;
            @media \( {}
            @position-try abc\{\}oops {}
            @namespace abc\{\}oops "test";
            @page abc\{\}def {}
            .abc\{\}oops { content: "a\1 b\"c\\d"; background-image: url("a)b\"c\\d"); font-family: "a\"b\\c"; }
        "#;
        let sheet = CssStyleSheetText::parse(source).expect("CSS source recovery");
        let rules = sheet.css_rules().expect("actual CSSOM source rules");
        assert!(rules.iter().any(|rule| starts_css_at_rule(&rule.css_text,"@import")));
        assert!(rules.iter().any(|rule| starts_css_at_rule(&rule.css_text,"@keyframes")));
        assert!(rules.iter().any(|rule| rule.selector_text.is_some()));
        let import = rules.iter().find(|rule| starts_css_at_rule(&rule.css_text,"@import")).unwrap();
        assert_eq!(serialize_rule(import),r#"@import url("abc") layer(\{\});"#,"CSSImportRule uses canonical URL and decoded layer tokens");
        let serialized = rules.iter().map(serialize_rule).collect::<Vec<_>>().join("\n");
        let next = CssStyleSheetText::parse(&serialized).expect("serialized source remains valid");
        let next_rules = next.css_rules().expect("serialized rule ownership");
        assert_eq!(next_rules.iter().map(serialize_rule).collect::<Vec<_>>().join("\n"),serialized);
        let rule = rules.iter().find(|rule| rule.selector_text.is_some()).unwrap();
        let declarations = css::serialize_cssom_declaration_block(rule.style.css_text()).unwrap();
        assert_eq!(css::serialize_cssom_declaration_block(&declarations).unwrap(),declarations);
        let block = css::DeclarationBlock::parse(&declarations).unwrap();
        assert!(block.value("content").is_some(),"string tokens survive the declaration parser");
        assert!(block.value("background-image").is_some(),"quoted URL delimiters remain data");
        assert!(block.value("font-family").is_some(),"quoted family names survive escaping");
    }

    #[test]
    fn specification_cssom_shadow_sheet_association_media_disabled_and_retained_replay() {
        let mut engine=lumen::Engine::new();let realm=crate::install(engine.ctx(),"<head></head><body><div id=host></div></body>",256).unwrap();
        install_cssom_layout(&realm);
        let result=engine.eval_value(r#"(() => {
            const check=(ok,message)=>{if(!ok)throw Error(message)};
            globalThis.shadow=document.getElementById('host').attachShadow({mode:'open'});
            globalThis.owner=document.createElement('style');shadow.append(owner);
            globalThis.sheet=owner.sheet;check(sheet instanceof CSSStyleSheet && sheet.cssRules.length===0,'empty associated shadow sheet');
            globalThis.target=document.createElement('div');target.id='target';shadow.append(target);
            sheet.insertRule('#target{width:21px;height:10px;color:green}',0);
            check(owner.textContent==='' && target.getBoundingClientRect().width===21,'effective CSSOM source without author text mutation');
            owner.disabled=true;check(sheet.disabled && target.getBoundingClientRect().width!==21,'shadow disabled actual rule exclusion');
            owner.disabled=false;check(!sheet.disabled && target.getBoundingClientRect().width===21,'shadow reenabled same sheet');
            owner.media='not all';check(owner.sheet===sheet && target.getBoundingClientRect().width!==21,'live shadow owner media');
            owner.media='all';check(owner.sheet===sheet && target.getBoundingClientRect().width===21,'media restores source identity');
            owner.type='text/plain';check(owner.sheet===sheet && target.getBoundingClientRect().width===21,'type change does not recreate existing style block');
            owner.textContent='#target{width:31px;height:10px}';check(owner.sheet===null && sheet.ownerNode===null && sheet.cssRules.length===1 && target.getBoundingClientRect().width!==31,'unsupported type on real block update');
            owner.type='text/css';check(owner.sheet===null,'type alone does not run style block update');
            owner.textContent=owner.textContent;globalThis.fresh=owner.sheet;check(fresh!==sheet && target.getBoundingClientRect().width===31,'authored block update creates supported sheet');
            sheet.insertRule('#target{width:99px}',1);check(target.getBoundingClientRect().width===31,'detached CSSOM edit remains private');
            return true;
        })()"#).unwrap();
        let value=result.unwrap_or_else(|error|panic!("shadow sheet lifecycle: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));assert!(matches!(value,Value::Bool(true)));
        let mut session=realm.session.borrow_mut();let retained=session.display_list(300,200,&NoText).unwrap().clone();session.invalidate_image_resources();let replay=session.display_list(300,200,&NoText).unwrap().clone();assert_eq!(retained,replay,"fresh shadow stylesheet paint matches retained replay");
    }
    #[test]
    fn specification_cssom_shadow_csp_policy_and_unloaded_import_rule_order_remain_authoritative() {
        let mut engine=lumen::Engine::new();let realm=crate::install(engine.ctx(),"<head></head><body><div id=host></div></body>",256).unwrap();
        install_cssom_layout(&realm);
        realm.set_content_security_policy_headers(&[("Content-Security-Policy".into(),"style-src 'none'".into())]).unwrap();
        let result=engine.eval_value(r#"(() => {
            const check=(ok,message)=>{if(!ok)throw Error(message)};
            const shadow=document.getElementById('host').attachShadow({mode:'open'});
            const owner=document.createElement('style');owner.textContent='div{width:97px;height:10px}';shadow.append(owner);
            const target=document.createElement('div');shadow.append(target);
            check(owner.sheet===null && target.getBoundingClientRect().width!==97,'shadow parser/compiler respects actual blocked association');
            return true;
        })()"#).unwrap();
        let value=result.unwrap_or_else(|error|panic!("shadow stylesheet policy: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));assert!(matches!(value,Value::Bool(true)));
        let mut other=lumen::Engine::new();let _realm=crate::install(other.ctx(),"<head></head><body></body>",256).unwrap();
        let result=other.eval_value(r#"(() => {
            const check=(ok,message)=>{if(!ok)throw Error(message)};
            const owner=document.createElement('style');document.head.append(owner);
            owner.textContent='@import "absent.css" supports(not ((selector(::backdrop)))); }';
            const sheet=owner.sheet;check(sheet.cssRules.length===1 && sheet.cssRules[0].styleSheet===null,'unloaded import rule persists through forgiving syntax');
            let error=null;try{sheet.insertRule('HTML {transition: normal}',0)}catch(e){error=e}
            check(error instanceof DOMException && error.name==='HierarchyRequestError' && sheet.cssRules.length===1,'no-sheet import retains ordinary rule ordering');
            return true;
        })()"#).unwrap();
        let value=result.unwrap_or_else(|error|panic!("unloaded import ordering: {}",other.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));assert!(matches!(value,Value::Bool(true)));
    }
    #[test]
    fn specification_css_module_default_uses_installed_settings_factory_and_live_adopted_sheet() {
        let mut engine=lumen::Engine::new();
        let realm=crate::install(engine.ctx(),"<div id=target></div>",128).unwrap();
        install_cssom_layout(&realm);
        let calls=Rc::new(Cell::new(0));
        let fetched=calls.clone();
        engine.ctx().install_module_fetch_loader(Rc::new(move |request| {
            assert_eq!(request.attribute_type.as_deref(),Some("css"));
            fetched.set(fetched.get()+1);
            Some(lumen::ModuleFetchResult { key:request.specifier,
                source:"@import 'ignored.css'; #target { width:12px }".into(),script_context:request.script_context })
        }));
        let handle=engine.ctx().run_prepared_module_for_host(
            "import sheet from './sheet.css' with {type:'css'}; import same from './sheet.css' with {type:'css'}; export {sheet,same};",
            "https://example.test/main.js","https://example.test/main.js","https://example.test/main.js",None).expect("CSS module graph");
        let sheet=engine.ctx().member_get(handle.namespace(),"sheet").ok().expect("genuine default export");
        let same=engine.ctx().member_get(handle.namespace(),"same").ok().expect("cached default export");
        assert_eq!(sheet.object_identity(),same.object_identity());
        assert_eq!(calls.get(),1,"one settings-qualified fetch and sheet instance");
        let global=engine.ctx().global_object();
        engine.ctx().set_member(&global,"moduleSheet",sheet).ok().expect("publish genuine sheet for guard");
        let result=engine.eval_value(r#"(() => {
            const check=(ok,message)=>{if(!ok)throw Error(message)};
            check(moduleSheet instanceof CSSStyleSheet,'genuine CSSStyleSheet');
            check(moduleSheet.cssRules.length===1,'canonical replacement removes imports');
            document.adoptedStyleSheets=[moduleSheet];
            check(document.getElementById('target').getBoundingClientRect().width===12,'module sheet participates in adopted cascade');
            moduleSheet.cssRules[0].style.width='24px';
            check(document.getElementById('target').getBoundingClientRect().width===24,'default instance retains live rule mutation');
            return true;
        })()"#).unwrap();
        let result=result.unwrap_or_else(|error|panic!("CSS module threw: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));
        assert!(matches!(result,Value::Bool(true)));
    }

    #[test]
    fn specification_constructed_sheet_async_locking_retention_and_document_cancellation() {
        let mut engine=lumen::Engine::new();let realm=crate::install(engine.ctx(),"<div id=target></div>",128).unwrap();install_cssom_layout(&realm);
        let result=engine.eval_value(r#"(() => {
            const check=(ok,message)=>{if(!ok)throw Error(message)};
            globalThis.sheet=new CSSStyleSheet();sheet.replaceSync('#target{width:12px}');document.adoptedStyleSheets=[sheet];
            globalThis.oldRule=sheet.cssRules[0];globalThis.done=false;globalThis.secondRejected=false;globalThis.lifecycleError=null;
            globalThis.pending=sheet.replace('@import "unused.css";@layer x;@import "unused-again.css";#target{width:24px}');
            check(sheet.cssRules[0]===oldRule && oldRule.style.width==='12px','old rules remain visible while worker pending');
            for(const mutation of [()=>sheet.insertRule('div{}',99),()=>sheet.deleteRule(99),()=>sheet.replaceSync('')]) {
                let rejected=false;try{mutation()}catch(e){rejected=e instanceof DOMException && e.name==='NotAllowedError'}check(rejected,'lock precedes index and syntax checks');
            }
            sheet.replace('div{}').catch(e=>{check(e instanceof DOMException && e.name==='NotAllowedError','second replace rejects');secondRejected=true}).catch(error=>{lifecycleError=String(error)});
            pending.then(value=>{check(value===sheet,'actual sheet promise result');check(sheet.cssRules.length===2,'both imports removed around layer statement');check(oldRule.parentStyleSheet===null && oldRule.style.width==='12px','retained old rule detached');check(document.getElementById('target').getBoundingClientRect().width===24,'actual new cascade');sheet.insertRule('p{}',2);done=true}).catch(error=>{lifecycleError=String(error)});
            return true;
        })()"#).unwrap();
        let result=result.unwrap_or_else(|error|panic!("synchronous replacement lifecycle threw: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));
        assert!(matches!(result,Value::Bool(true)),"synchronous replacement lifecycle guard");
        crate::scheduling::cancel_tasks_for_document(engine.ctx(),&realm);
        engine.ctx().collect_garbage();engine.ctx().poll_async();engine.ctx().drain_microtasks_for_host();
        let result=engine.eval_value("done && secondRejected && sheet.cssRules.length===3").unwrap();
        if !matches!(result,Ok(Value::Bool(true))) {
            let details=engine.eval_value("JSON.stringify({done,secondRejected,lifecycleError,rules:sheet.cssRules.length,oldParent:oldRule.parentStyleSheet===null,oldWidth:oldRule.style.width,currentWidth:document.getElementById('target').getBoundingClientRect().width})").unwrap();
            let details=details.unwrap_or_else(|error|error);
            panic!("native parse completion must survive Document cancellation and GC: {}",engine.ctx().coerce_string(&details).map(|value|value.to_string()).unwrap_or_default());
        }
    }

    #[test]
    fn specification_cssom_replacement_recovery_and_single_rule_entry_points_share_parser() {
        let mut engine=lumen::Engine::new();
        let realm=crate::install(engine.ctx(),"<div id=target></div>",128).unwrap();
        install_cssom_layout(&realm);
        let result=engine.eval_value(r#"(() => {
            const check=(value,message)=>{if(!value)throw Error(message)};
            globalThis.recoverySheet=new CSSStyleSheet();
            recoverySheet.replaceSync('#target { width:7px; height:calc(2px + 3px');
            document.adoptedStyleSheets=[recoverySheet];
            check(recoverySheet.cssRules.length===1,'EOF closes rule and function');
            check(document.getElementById('target').getBoundingClientRect().width===7,'actual adopted cascade');
            globalThis.recoveryOldRule=recoverySheet.cssRules[0];
            recoverySheet.insertRule('.other { height:2px',1);
            const length=recoverySheet.cssRules.length;
            for(const text of ['.one{} .two{}','.one{} trailing']) {
                let rejected=false;try{recoverySheet.insertRule(text,0)}catch(error){rejected=error.name==='SyntaxError'}
                check(rejected && recoverySheet.cssRules.length===length,'one-rule parser rejects extra input transactionally');
            }
            recoverySheet.replaceSync('@keyframes motion { invalid {opacity:0} from {opacity:0} to {opacity:1} } #target { width:9px');
            check(recoverySheet.cssRules.length===2 && recoverySheet.cssRules[0].cssRules.length===2,'invalid keyframe is a local recovery');
            check(document.getElementById('target').getBoundingClientRect().width===9,'replacement commits recovered declarations');
            check(recoveryOldRule.parentStyleSheet===null,'old rule detaches');
            recoveryOldRule.style.width='11px';
            check(recoveryOldRule.style.width==='11px' && document.getElementById('target').getBoundingClientRect().width===9,'detached old declaration cannot modify replacement');
            globalThis.recoveryDone=false;globalThis.recoveryError=null;
            recoverySheet.replace('!@#$%garbage').then(value=>{
                check(value===recoverySheet && recoverySheet.cssRules.length===0,'forgiving replacement clears invalid stylesheet');
                recoverySheet.replaceSync('#target { width:13px');
                check(document.getElementById('target').getBoundingClientRect().width===13,'sheet remains usable after recovery');
                recoveryDone=true;
            }).catch(error=>{recoveryError=String(error)});
            return true;
        })()"#).unwrap();
        let result=result.unwrap_or_else(|error|panic!("CSS recovery guard threw: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));
        assert!(matches!(result,Value::Bool(true)));
        engine.ctx().poll_async();engine.ctx().drain_microtasks_for_host();
        let result=engine.eval_value("recoveryDone && recoveryError===null").unwrap();
        if !matches!(result,Ok(Value::Bool(true))) {
            let details=engine.eval_value("String(recoveryError)").unwrap().unwrap_or_else(|error|error);
            panic!("asynchronous replacement must settle after forgiving parse: {}",engine.ctx().coerce_string(&details).map(|value|value.to_string()).unwrap_or_default());
        }
    }

    #[test]
    fn specification_constructed_sheet_options_media_disabled_and_source_url_reach_cascade() {
        let mut engine=lumen::Engine::new();let realm=crate::install(engine.ctx(),"<div id=target></div>",128).unwrap();
        realm.set_document_url("https://example.test/document/page.html");install_cssom_layout(&realm);
        let result=engine.eval_value(r#"(() => {
            const check=(ok,message)=>{if(!ok)throw Error(message)};let order=[];
            const sheet=new CSSStyleSheet({get baseURL(){order.push('base');return '../assets/'},get disabled(){order.push('disabled');return true},get media(){order.push('media');return 'screen, print'}});
            check(order.join(',')==='base,disabled,media','typed dictionary lexical conversion order');
            check(sheet.title===null && sheet.ownerNode===null && sheet.ownerRule===null,'constructor sheet ownership');
            check(sheet.media===sheet.media && sheet.media instanceof MediaList && sheet.media.length===2,'live stable typed media list');
            sheet.replaceSync('#target{width:24px;background-image:url(icon.png)}');document.adoptedStyleSheets=[sheet];
            check(getComputedStyle(document.getElementById('target')).width!=='24px','disabled prevents cascade');
            sheet.disabled=false;check(getComputedStyle(document.getElementById('target')).width==='24px','reenabling actual adopted source');
            check(getComputedStyle(document.getElementById('target')).backgroundImage.includes('https://example.test/assets/icon.png'),'constructor source URL applies to declaration');
            sheet.media.mediaText='not all';check(getComputedStyle(document.getElementById('target')).width!=='24px','media mutation changes actual cascade');
            sheet.media.mediaText='screen';sheet.media.appendMedium('print');check(sheet.media.length===2,'appendMedium');sheet.media.deleteMedium('print');
            const copied=new CSSStyleSheet({media:sheet.media});sheet.media.mediaText='print';check(copied.media.mediaText==='screen','MediaList option copies queries without alias');
            let rejected=false;try{new CSSStyleSheet({baseURL:'https://test:test/'})}catch(e){rejected=e instanceof DOMException && e.name==='NotAllowedError'}check(rejected,'invalid constructor URL');
            sheet.replaceSync('@unknown; #target{width:33px;does-not-exist:bad}');sheet.media.mediaText='screen';check(getComputedStyle(document.getElementById('target')).width==='33px','shared parser recovers ignored rules and declarations');
            return true;
        })()"#).unwrap();
        let result=result.unwrap_or_else(|error|panic!("constructible options threw: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));assert!(matches!(result,Value::Bool(true)));
    }

    #[test]
    fn specification_constructed_sheet_replacement_removes_imports_and_keeps_live_rule_owners() {
        let mut engine=lumen::Engine::new();
        let realm=crate::install(engine.ctx(),"<div id=target></div>",128).unwrap();
        install_cssom_layout(&realm);
        let result=engine.eval_value(r#"(() => {
            const check=(ok,message)=>{if(!ok)throw Error(message)};
            const sheet=new CSSStyleSheet();sheet.replaceSync('@import "unused.css"; @layer declared; @import "also-unused.css"; #target { width: 12px }');
            check(sheet.cssRules.length===2 && sheet.cssRules[1] instanceof CSSStyleRule,'replacement removes imports separated by layer statements');
            sheet.deleteRule(0);
            document.adoptedStyleSheets=[sheet];const rule=sheet.cssRules[0],style=rule.style;
            check(document.getElementById('target').getBoundingClientRect().width===12,'actual adopted cascade');
            let rejected=false;try{sheet.insertRule('@import "other.css";',0)}catch(e){rejected=e instanceof DOMException && e.name==='SyntaxError'}
            check(rejected && sheet.cssRules.length===1 && sheet.cssRules[0]===rule,'constructed insert rejects without mutation');
            sheet.replaceSync('@import "unused-again.css"; #target { width: 24px }');
            check(sheet.cssRules.length===1 && document.getElementById('target').getBoundingClientRect().width===24,'same shared live replacement');
            check(rule.parentStyleSheet===null && style.width==='12px','old rule retains detached declarations');
            return true;
        })()"#).unwrap();
        let result=result.unwrap_or_else(|error|panic!("constructed replacement threw: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));
        assert!(matches!(result,Value::Bool(true)));
    }

    #[test]
    fn registered_property_universal_initial_inheritance_errors_and_invalidation() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let realm = crate::install(engine.ctx(), "<style id=rules>#parent{--tone:red;--ordinary:blue}#child{color:var(--tone,pink)}#target{display:var(--my-display,block)}</style><div id=parent><span id=child>child</span></div><div id=target></div>", 192).unwrap();
        let source = r#"(() => {
            const check=(value,message)=>{if(!value)throw Error(message)};
            const child=document.querySelector('#child'), target=document.querySelector('#target');
            check(getComputedStyle(child).color==='rgb(255, 0, 0)','unregistered inherited value');
            check(getComputedStyle(target).display==='block','initial fallback');
            CSS.registerProperty({name:'--my-display',syntax:'*',inherits:false,initialValue:'none'});
            check(getComputedStyle(target).display==='none','registration invalidates existing computed style');
            CSS.registerProperty({name:'--tone',syntax:'*',inherits:false,initialValue:'green'});
            check(getComputedStyle(child).color==='rgb(0, 128, 0)','noninherited initial value');
            child.style.setProperty('--tone','inherit');check(getComputedStyle(child).color==='rgb(255, 0, 0)','explicit inherit');
            child.style.setProperty('--tone','initial');check(getComputedStyle(child).color==='rgb(0, 128, 0)','registered initial keyword');
            child.style.setProperty('--tone','unset');check(getComputedStyle(child).color==='rgb(0, 128, 0)','noninherited unset');
            CSS.registerProperty({name:'--ordinary'});child.style.color='var(--ordinary,pink)';
            check(getComputedStyle(child).color==='rgb(0, 0, 255)','default universal syntax and inherited flag');
            CSS.registerProperty({name:'--missing',inherits:false});child.style.color='var(--missing,green)';
            check(getComputedStyle(child).color==='rgb(0, 128, 0)','absent initial stays guaranteed invalid');
            const error=(definition,name)=>{let actual='';try{CSS.registerProperty(definition)}catch(e){actual=e.name}check(actual===name,name+' actual='+actual)};
            CSS.registerProperty({name:'--name, no escapes needed',initialValue:'green'});
            error({name:'--name, no escapes needed'},'InvalidModificationError');
            error({name:'--'},'SyntaxError');
            error({name:'--tone',syntax:'**'},'InvalidModificationError');
            error({name:'ordinary'},'SyntaxError');error({name:'--invalid',syntax:'**'},'SyntaxError');
            CSS.registerProperty({name:'--typed',syntax:'<length>',initialValue:'1px'});
            check(getComputedStyle(child).getPropertyValue('--typed')==='1px','typed length initial value');
            error({name:'--unsupported-typed',syntax:'<transform-list>',initialValue:'none'},'NotSupportedError');
            error({name:'--invalid-value',initialValue:'var(--ordinary)'},'SyntaxError');
            error({name:'--unbalanced',initialValue:'('},'SyntaxError');error({},'TypeError');
            document.querySelector('#rules').textContent+=' #target{height:10px}';
            check(getComputedStyle(target).display==='none','registration persists stylesheet rebuild');
            return true;
        })()"#;
        let wrapped = format!("try {{ {source} }} catch(e) {{ String(e.stack || e) }}");
        match engine.eval_value(&wrapped) {
            Ok(Ok(Value::Bool(true))) => (),
            Ok(Ok(Value::Str(message))) => panic!("registration regression: {}", message.as_str()),
            _ => panic!("registration regression did not return true"),
        }
        realm.with_session(|session| assert_eq!(session.computed_style(crate::selector::query_selector(session.document(),session.document().root(),"#target").unwrap().unwrap()).unwrap().display, css::Display::None));
    }

    #[test]
    fn specification_registered_image_native_opaque_class_currentcolor_and_source() {
        let mut runtime=lumen_runtime::Runtime::new_browser();let engine=runtime.engine();
        let realm=crate::install(engine.ctx(),"<div id=target></div>",128).unwrap();install_cssom_layout(&realm);
        let result=engine.eval_value(r#"(()=>{const target=document.getElementById('target');CSS.registerProperty({name:'--image',syntax:'<image>',inherits:false,initialValue:'linear-gradient(red, blue)'});CSS.registerProperty({name:'--color',syntax:'<color>',inherits:false,initialValue:'red'});target.style.cssText='color:color(srgb .123 .456 .789);--color:currentColor;--image:linear-gradient(var(--color), blue)';const value=target.computedStyleMap().get('--image');if(!(value instanceof CSSImageValue)||!(value instanceof CSSStyleValue)||value instanceof CSSUnparsedValue)throw Error('image class');if(!getComputedStyle(target).getPropertyValue('--color').startsWith('color(srgb '))throw Error('current color provenance');if(!String(value).includes('color(srgb '))throw Error('image color provenance');let thrown=false;try{new CSSImageValue()}catch(e){thrown=e instanceof TypeError}if(!thrown)throw Error('opaque constructor');return true})()"#);
        assert!(matches!(result,Ok(Ok(Value::Bool(true)))));
    }

    #[test]
    fn specification_registered_codec_native_transform_and_empty_universal() {
        let mut runtime=lumen_runtime::Runtime::new_browser();let engine=runtime.engine();
        let realm=crate::install(engine.ctx(),"<div id=target></div>",128).unwrap();install_cssom_layout(&realm);
        let result=engine.eval_value(r#"(()=>{const target=document.getElementById('target');CSS.registerProperty({name:'--move',syntax:'<transform-function>',inherits:false,initialValue:'translateX(0px)'});CSS.registerProperty({name:'--empty',syntax:'*',inherits:false});target.style.cssText='font-size:10px;--move:translateX(2em);--empty:;background-color:var(--empty) green';const value=target.computedStyleMap().get('--move');if(!(value instanceof CSSTranslate)||value.x.value!==20)throw Error('typed transform');if(getComputedStyle(target).backgroundColor!=='rgb(0, 128, 0)')throw Error('empty universal');return true})()"#);
        assert!(matches!(result,Ok(Ok(Value::Bool(true)))));
    }

    #[test]
    fn specification_property_rule_native_descriptor_identity_and_live_removal() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let realm = crate::install(engine.ctx(), r#"<style>@property --measure{initial-value:2px;inherits:false;syntax:" <length> "} div{width:var(--measure)} @property --{syntax:"<color>";initial-value:3px}</style><div id=target></div>"#,128).unwrap();
        install_cssom_layout(&realm);
        let result = engine.eval_value(r#"(() => {
            const sheet=document.styleSheets[0], target=document.getElementById('target');
            const eq=(a,b,label)=>{if(a!==b)throw Error(label+': '+a+' != '+b)};
            const rule=sheet.cssRules[0];
            eq(rule instanceof CSSPropertyRule,true,'native class');eq(rule.type,0,'rule type');
            eq(rule.name,'--measure','name');eq(rule.syntax,' <length> ','specified syntax');
            eq(rule.inherits,false,'inheritance');eq(rule.initialValue,'2px','initial descriptor');
            eq(sheet.cssRules.length,2,'invalid property rule omitted');
            eq(getComputedStyle(target).width,'2px','stylesheet registration');
            eq(target.computedStyleMap().get('--measure').value,2,'typed reification');
            sheet.deleteRule(0);eq(getComputedStyle(target).getPropertyValue('--measure'),'','registration removed');
            sheet.insertRule('@property --measure { syntax: "<length>"; inherits: false; initial-value: 3px; }',0);
            eq(getComputedStyle(target).width,'3px','registration inserted');
            CSS.registerProperty({name:'--measure',syntax:'<length>',inherits:false,initialValue:'7px'});
            eq(getComputedStyle(target).width,'7px','API precedence');
            return true;
        })()"#);
        assert!(matches!(result,Ok(Ok(Value::Bool(true)))),"property native guard failed");
    }

    #[test]
    fn specification_registered_computed_native_font_var_cssom_and_typed_lists() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let realm = crate::install(engine.ctx(), "<style>#p{font-size:20px;line-height:30px;--length:2em;--line:2lh;--alias:var(--length)}#c{font-size:10px;width:var(--alias);height:var(--line)}</style><div id=p><div id=c></div></div>", 128).unwrap();
        install_cssom_layout(&realm);
        let result = engine.eval_value(r#"(() => {
            const p=document.getElementById('p'),c=document.getElementById('c');
            const eq=(actual,expected,label)=>{if(actual!==expected)throw Error(label+': '+actual+' != '+expected)};
            CSS.registerProperty({name:'--length',syntax:'<length>',inherits:true,initialValue:'3px'});
            CSS.registerProperty({name:'--line',syntax:'<length>',inherits:true,initialValue:'4px'});
            eq(getComputedStyle(c).getPropertyValue('--length'),'40px','declaring font computed before inheritance');
            eq(getComputedStyle(c).getPropertyValue('--alias'),'40px','unregistered alias receives computed tokens');
            eq(getComputedStyle(c).getPropertyValue('--line'),'60px','declaring line height');
            eq(getComputedStyle(c).width,'40px','ordinary var substitution');
            eq(getComputedStyle(c).height,'60px','inherited registered line unit');
            const numeric=c.computedStyleMap().get('--length');
            eq(numeric instanceof CSSUnitValue,true,'registered numeric reification');eq(numeric.unit,'px','canonical unit');eq(numeric.value,40,'canonical value');
            c.style.setProperty('--length','blue');
            eq(c.style.getPropertyValue('--length'),'blue','parse-time custom syntax unchanged');
            eq(CSS.supports('--length','blue'),true,'supports checks generic specified syntax');
            eq(getComputedStyle(c).getPropertyValue('--length'),'40px','invalid computed value inherits');
            CSS.registerProperty({name:'--items',syntax:'<length>#',inherits:false,initialValue:'1in, 2px'});
            const list=c.computedStyleMap().getAll('--items');
            eq(list.length,2,'registered list grammar');eq(list[0].value,96,'absolute unit canonicalization');eq(list[1].value,2,'second computed item');
            let error='';try{CSS.registerProperty({name:'--invalid-initial',syntax:'<length>',inherits:false,initialValue:'calc(1em + 1px)'})}catch(e){error=e.name}
            eq(error,'SyntaxError','context-dependent initial rejected');
            p.style.fontSize='25px';eq(getComputedStyle(c).getPropertyValue('--length'),'50px','font change invalidates computed source');
            c.style.cssText='font-size:var(--length);--length:2em';
            eq(getComputedStyle(c).fontSize,'25px','relative-unit cycle unsets font-size');
            eq(getComputedStyle(c).getPropertyValue('--length'),'50px','invalid computed registered source uses inherited value');
            c.style.cssText='font-size:var(--length);font-size:15px;--length:2em';
            eq(getComputedStyle(c).getPropertyValue('--length'),'30px','overridden dependency cannot create a cycle');
            return true;
        })()"#);
        match result {
            Ok(Ok(Value::Bool(true))) => (),
            Ok(Err(error)) => panic!("registered computation: {}", engine.ctx().coerce_string(&error).map(|value| value.to_string()).unwrap_or_else(|_| "unprintable exception".into())),
            _ => panic!("registered computation did not return true"),
        }
    }

    #[test]
    fn specification_registered_computed_native_query_basis_and_percentage_math() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let realm = crate::install(engine.ctx(), "<style>body{margin:0}#container{container-type:inline-size;width:300px}#p{--length:min(calc(10cqw + 10%),80px);width:var(--length);height:20px}#c{width:var(--length);height:10px}</style><div id=container><div id=p><div id=c></div></div></div>", 128).unwrap();
        install_cssom_layout(&realm);
        let result = engine.eval_value(r#"(() => {
            CSS.registerProperty({name:'--length',syntax:'<length-percentage>',inherits:true,initialValue:'0px'});
            const p=document.getElementById('p'),c=document.getElementById('c');
            const eq=(a,b,label)=>{if(a!==b)throw Error(label+': '+a+' != '+b)};
            eq(getComputedStyle(p).width,'60px','declaring query plus parent percentage');
            eq(getComputedStyle(c).width,'36px','inherited computed query plus own containing block percentage');
            eq(c.computedStyleMap().get('--length') instanceof CSSMathMin,true,'nonlinear computed math retained');
            document.getElementById('container').style.width='500px';
            eq(getComputedStyle(p).width,'80px','nonlinear branch crosses after real container mutation');
            eq(getComputedStyle(c).width,'58px','child percentage remains separate after mutation');
            return true;
        })()"#);
        match result {
            Ok(Ok(Value::Bool(true))) => (),
            Ok(Err(error)) => panic!("registered query computation: {}", engine.ctx().coerce_string(&error).map(|value| value.to_string()).unwrap_or_else(|_| "unprintable exception".into())),
            _ => panic!("registered query computation did not return true"),
        }
    }

    struct NoText;
    impl lumen_html::paint::TextShaper for NoText {
        fn shape(&self, _: &str, _: f32) -> Result<lumen_html::paint::ShapedRun, ()> { Err(()) }
        fn ascent(&self, size: f32) -> f32 { size * 0.8 }
        fn line_height(&self, size: f32) -> f32 { size * 1.2 }
    }
    fn install_cssom_layout(realm: &Rc<DomRealm>) {
        realm.set_layout_flusher(Rc::new(|session| {
            session.display_list(300, 200, &NoText).map(|_| ()).map_err(|error| format!("{error:?}"))
        }));
    }

    #[test]
    fn specification_fieldset_cssom_computed_display_and_live_legend_box_ownership() {
        let mut runtime=lumen_runtime::Runtime::new_browser();
        let engine=runtime.engine();
        let realm=crate::install(engine.ctx(),"<!doctype html><style>body{margin:0}fieldset{display:flex;width:100px;height:40px;padding:4px;border:2px solid red;margin:0;min-inline-size:0}legend{display:block;width:30px;height:20px;padding:0;margin:0}div{width:10px;height:10px}</style><fieldset id=f><legend id=l></legend><div id=c></div></fieldset>",128).unwrap();
        install_cssom_layout(&realm);
        let result=engine.eval_value(r#"(() => {
            const f=document.getElementById('f'),l=document.getElementById('l'),c=document.getElementById('c');
            const eq=(actual,expected,label)=>{if(actual!==expected)throw new Error(label+': '+actual+' != '+expected)};
            eq(getComputedStyle(f).display,'flex','computed fieldset formatting type');
            eq(getComputedStyle(l).display,'block','computed legend display remains author value');
            const fr=f.getBoundingClientRect(),lr=l.getBoundingClientRect(),cr=c.getBoundingClientRect();
            eq(fr.width,112,'fieldset border width');eq(fr.height,52,'definite block size reserves legend');
            eq(lr.left,6,'legend respects transferred inline padding');eq(lr.top,0,'legend border center');
            eq(cr.top,24,'anonymous content block start');
            eq(f.children.length,2,'anonymous box has no DOM identity');eq(f.firstElementChild,l,'legend DOM identity');
            l.style.display='none';eq(c.getBoundingClientRect().top,6,'live legend eligibility');
            l.style.display='inline';l.style.float='left';eq(c.getBoundingClientRect().top,6,'floated legend is ordinary anonymous content');
            l.style.float='none';eq(c.getBoundingClientRect().top,24,'eligible legend returns without wrapper replacement');
            f.style.appearance='base';eq(l.getBoundingClientRect().top,6,'appearance base opts out of special fieldset formatting');
            f.style.appearance='auto';f.style.display='block';l.style.display='inline';
            eq(getComputedStyle(l).display,'inline','legend used blockification preserves ordinary computed display');
            eq(l.getBoundingClientRect().width,30,'legend used blockification honors inline-size');
            eq(f.firstElementChild,l,'all used-layout changes retain real nodes');
            f.style.cssText='display:block;width:100px;height:200px;padding:0;margin:0;border:2px solid;overflow:scroll;min-inline-size:0';
            l.style.cssText='display:block;width:30px;height:20px;padding:0;margin:0';c.style.height='400px';
            eq(f.scrollHeight,400,'anonymous scrolling viewport counted once');
            f.scrollTop=500;eq(f.scrollTop,218,'anonymous content scroll range excludes legend');
            f.style.transform='translate(10px,15px)';eq(f.scrollHeight,400,'transform preserves local scrolling dimensions');
            const host=document.createElement('div');document.body.append(host);
            const light=document.createElement('legend');light.style.cssText='width:30px;height:20px;padding:0;margin:0';host.append(light);
            const shadow=host.attachShadow({mode:'open'});
            shadow.innerHTML='<fieldset style="width:100px;margin:0;padding:4px;border:2px solid;min-inline-size:0"><slot></slot><div style="height:10px"></div></fieldset>';
            const sf=shadow.firstElementChild,sr=sf.getBoundingClientRect(),sl=light.getBoundingClientRect();
            eq(sl.top,sr.top,'first eligible slotted legend uses actual box tree');eq(sl.left-sr.left,6,'slotted legend inline padding');
            eq(light.parentNode,host,'slotted legend never reparents DOM nodes');

            return true;
        })()"#).unwrap().unwrap_or_else(|error|panic!("fieldset native geometry failed: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));
        assert!(matches!(result,Value::Bool(true)));
    }

    #[test]
    fn cssom_edits_preserve_author_text_observers_and_independent_sheet_generations() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let realm = crate::install(engine.ctx(), "<style id=source>.box {width:7px}</style><div class=box></div><div id=host></div>", 192).unwrap();
        install_cssom_layout(&realm);
        let result = engine.eval_value(r#"(() => {
            const fail = message => { throw new Error(message); };
            const eq = (a,b,message) => { if (a !== b) fail(message + ': ' + a + ' !== ' + b); };
            const owner = document.getElementById('source'), target = document.querySelector('.box');
            owner.appendChild(document.createTextNode('.box {height:3px}'));
            const first = owner.firstChild, second = first.nextSibling, author = owner.textContent;
            const observer = new MutationObserver(() => {});
            observer.observe(owner,{subtree:true,childList:true,characterData:true});
            const sheet = owner.sheet, rules = sheet.cssRules, original = rules[0];
            original.style.width = '19px'; original.selectorText = '.box';
            sheet.insertRule('.box {height:11px}',rules.length); sheet.deleteRule(1);
            eq(owner.textContent,author,'author bytes'); eq(owner.firstChild,first,'first author node');
            eq(first.nextSibling,second,'second author node'); eq(observer.takeRecords().length,0,'CSSOM mutation records');
            eq(getComputedStyle(target).width,'19px','effective cascade');
            eq(owner.sheet,sheet,'CSSOM edit retains sheet'); eq(sheet.ownerNode,owner,'live owner');
            owner.textContent = author;
            const fresh = owner.sheet;
            if (fresh === sheet || fresh.cssRules[0] === original) fail('equal author replacement must create a fresh sheet');
            eq(sheet.ownerNode,null,'old owner'); eq(original.parentStyleSheet,sheet,'old rule parent');
            eq(rules[0],original,'old list identity'); eq(original.style.width,'19px','old effective bytes');
            eq(fresh.cssRules[0].style.width,'7px','fresh author bytes');
            original.style.width = '29px'; sheet.insertRule('.box {height:31px}',rules.length);
            eq(getComputedStyle(target).width,'7px','old edit cannot render');
            fresh.cssRules[0].style.width = '41px'; eq(getComputedStyle(target).width,'41px','fresh independent edit');
            const shadow = document.getElementById('host').attachShadow({mode:'open'});
            shadow.innerHTML = '<style>.inside {width:5px}</style><div class=inside></div>';
            const shadowOwner = shadow.querySelector('style'), shadowText = shadowOwner.firstChild;
            shadowOwner.sheet.cssRules[0].style.width = '23px';
            eq(shadowOwner.firstChild,shadowText,'shadow author node');
            eq(shadowText.data,'.inside {width:5px}','shadow author bytes');
            eq(getComputedStyle(shadow.querySelector('.inside')).width,'23px','shadow effective cascade');
            const other = new DOMParser().parseFromString('<html><body></body></html>','text/html');
            other.adoptNode(owner);
            eq(fresh.ownerNode,null,'adopted owner expires');
            original.style.width = '43px'; fresh.cssRules[0].style.width = '47px';
            eq(original.style.width,'43px','old sheet survives adoption');
            eq(fresh.cssRules[0].style.width,'47px','second generation survives adoption');
            observer.disconnect();
            return true;
        })()"#).unwrap().unwrap_or_else(|error| panic!("CSSOM author isolation failed: {}", engine.ctx().coerce_string(&error).map(|value| value.to_string()).unwrap_or_default()));
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn scope_cssom_shares_typed_grouping_context_live_cascade_and_detached_children() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let realm = crate::install(engine.ctx(), r#"<style id="source">
            @scope {} @scope (.outer) {} @scope (.outer) to (.limit) {
                > .target { width: 17px; color: green; }
                @media screen {} @supports (display: block) {}
            } @scope to (.limit) {}
        </style><div class="outer" id="outer"><div class="target" id="inside"></div>
          <div class="limit"><div class="target" id="limited"></div></div>
        </div><div class="target" id="outside"></div>"#, 192).unwrap();
        install_cssom_layout(&realm);
        let result = engine.eval_value(r#"(() => {
            const fail = message => { throw new Error(message); };
            const eq = (a,b,label) => { if (a!==b) fail(label + ': ' + a + ' !== ' + b); };
            const sheet = document.getElementById('source').sheet, scopes = sheet.cssRules;
            eq(scopes.length, 4, 'source tree');
            const expected = [[null,null],['.outer',null],['.outer','.limit'],[null,'.limit']];
            for (let i=0; i<4; i++) {
                const scope = scopes[i];
                if (!(scope instanceof CSSScopeRule) || !(scope instanceof CSSGroupingRule) || scope instanceof CSSConditionRule) fail('typed scope hierarchy');
                eq(scope.start,expected[i][0],'start'); eq(scope.end,expected[i][1],'end');
                eq(scope.type,0,'scope legacy rule type');
                eq(scope.parentStyleSheet,sheet,'owner sheet');
            }
            eq(scopes[0].cssText,'@scope {\n}','implicit serialization');
            eq(scopes[1].cssText,'@scope (.outer) {\n}','root serialization');
            eq(scopes[3].cssText,'@scope to (.limit) {\n}','limit serialization');
            const scope = scopes[2], children = scope.cssRules, child = children[0];
            const media = children[1], supports = children[2];
            eq(child.type,1,'style type'); eq(media.type,4,'media type'); eq(supports.type,12,'supports type');
            eq(media.conditionText,'screen','media condition'); eq(supports.conditionText,'(display: block)','supports condition');
            eq(child.selectorText,'> .target','scoped relative selector'); eq(child.parentRule,scope,'child owner');
            const outer = document.getElementById('outer'), inside = document.getElementById('inside');
            eq(inside.getBoundingClientRect().width,17,'scoped live geometry');
            scope.insertRule('color: purple;',0);
            const declaration = children[0];
            if (!(declaration instanceof CSSNestedDeclarations)) fail('scope-root declaration type');
            eq(declaration.type,0,'synthetic legacy type');
            eq(children[1],child,'retained child identity');
            eq(getComputedStyle(outer).color,'rgb(128, 0, 128)','root-only cascade');
            eq(getComputedStyle(inside).color,'rgb(0, 128, 0)','child cascade');
            child.selectorText = '.target'; eq(child.selectorText,'.target','scoped setter');
            eq(getComputedStyle(document.getElementById('limited')).color,'rgb(128, 0, 128)','donut exclusion');
            child.style.width = '23px'; eq(inside.getBoundingClientRect().width,23,'child edit');
            const saved = scope.cssText;
            try { scope.insertRule(':is(.bad) {} trailing',1); fail('bad insert accepted'); }
            catch (error) { if (!(error instanceof DOMException) || error.name!=='SyntaxError') throw error; }
            eq(scope.cssText,saved,'atomic insertion');
            sheet.deleteRule(2); eq(scope.parentStyleSheet,null,'detached owner');
            eq(media.type,4,'detached media type'); eq(media.conditionText,'screen','detached media condition');
            eq(supports.type,12,'detached supports type'); eq(supports.conditionText,'(display: block)','detached supports condition');
            eq(child.parentRule,scope,'detached child owner');
            child.style.width = '29px'; eq(scope.cssRules[1],child,'detached child identity');
            eq(child.style.width,'29px','detached child edit'); eq(declaration.style.color,'purple','detached root declaration');
            scope.deleteRule(1); eq(child.parentRule,null,'independently removed scoped child');
            eq(child.type,1,'independently removed scoped style type');
            eq(child.selectorText,'.target','removed child preserves scoped selector');
            child.selectorText = '> .target'; eq(child.selectorText,'> .target','removed child scoped setter');
            child.style.width = '31px'; eq(child.style.width,'31px','independently removed child edit');
            eq(scope.cssRules.length,3,'removed child leaves retained declaration and groups');
            eq(scope.cssRules[0],declaration,'retained scoped declaration identity');
            return true;
        })()"#).unwrap().unwrap_or_else(|error| panic!("scope CSSOM failed: {}", engine.ctx().coerce_string(&error).map(|value| value.to_string()).unwrap_or_default()));
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn stylesheet_source_epoch_prevents_aba_topology_and_rule_identity_resurrection() {
        let mut engine = lumen::Engine::new();
        crate::install(engine.ctx(), "<style id=source>.host {}</style>", 64).unwrap();
        let result = engine.eval_value(r#"(() => {
            const owner = document.getElementById('source'), sheet = owner.sheet;
            const parent = sheet.cssRules[0];
            parent.insertRule('width:20px; height:30px;', 0);
            const child = parent.cssRules[0], saved = owner.textContent;
            if (!(child instanceof CSSNestedDeclarations) || parent.style.width !== '') return false;
            owner.textContent = '.different {}';
            owner.textContent = saved;
            if (parent.type !== 1 || child.type !== 0) return false;
            const freshSheet = owner.sheet, fresh = freshSheet.cssRules[0];
            return freshSheet !== sheet && fresh !== parent && fresh.cssRules.length === 0 && fresh.style.width === ''
                && parent.cssRules[0] === child && child.style.height === '30px'
                && sheet.cssRules[0] === parent && parent.parentStyleSheet === sheet && sheet.ownerNode === null;
        })()"#).unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)), "external source generation revived stale CSSOM topology");
    }

    #[test]
    fn stylesheet_legacy_aliases_share_live_rules_and_dom_exception_errors() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        crate::install(engine.ctx(), "<style>.host {}</style>", 64).unwrap();
        let result = engine.eval_value(r#"(() => {
            const sheet = document.styleSheets[0];
            const rules = sheet.rules;
            if (rules !== sheet.cssRules || rules.length !== 1) return false;
            sheet.insertRule('.next { > .child {color:green} }', 1);
            if (rules.length !== 2 || rules[1].cssRules[0].selectorText !== '& > .child') return false;
            const code = (action, expectedName, expectedCode) => {
                try { action(); } catch (error) {
                    return error instanceof DOMException && error.name === expectedName && error.code === expectedCode;
                }
                return false;
            };
            if (!code(() => sheet.insertRule('width:1px;'), 'SyntaxError', 12)) return false;
            if (!code(() => sheet.insertRule('.wrong{}', 3), 'IndexSizeError', 1)) return false;
            if (!code(() => sheet.removeRule(2), 'IndexSizeError', 1)) return false;
            sheet.removeRule();
            return rules.length === 1 && rules[0].selectorText === '.next';
        })()"#).unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)), "legacy CSSOM aliases or typed rule exceptions changed");
    }

    #[test]
    fn stylesheet_structural_edits_rebase_retained_declarations_and_batch_source_offsets() {
        use super::*;
        let block = Rc::new(css::DeclarationBlock::parse("margin:var(--m);margin-left:7px").unwrap());
        let mut sheet = CssStyleSheetText::parse("@unknown ignored {} .a {} .b {}").unwrap();
        sheet.retained.push((Arc::from([0usize]), block.clone()));
        sheet.insert_rule(".inserted {}", 0).unwrap();
        assert_eq!(sheet.retained[0].0.as_ref(), &[1]);
        assert!(Rc::ptr_eq(&sheet.retained[0].1, &block));
        sheet.delete_rule(0).unwrap();
        assert_eq!(sheet.retained[0].0.as_ref(), &[0]);
        sheet.set_rule_selector(&[0], ".renamed").unwrap();
        assert!(sheet.css_rules().unwrap()[0].selector_text.as_deref().unwrap().contains("renamed"));
        assert!(Rc::ptr_eq(&sheet.retained[0].1, &block));
        let before = sheet.clone();
        assert!(sheet.insert_rule(".invalid {} trailing", 0).is_err());
        assert_eq!(sheet, before);
        let mut eof = CssStyleSheetText::parse("").unwrap();
        eof.insert_rule(".valid { color: blue", 0).unwrap();
        assert_eq!(eof.css_rules().unwrap()[0].style.text, "color: blue;");
        let mut nested = CssStyleSheetText::parse("@media all {.a{} .b{}}").unwrap();
        nested.retained.push((Arc::from([0usize, 1]), block.clone()));
        nested.insert_nested_rule(&[0], ".first{}", 0).unwrap();
        assert_eq!(nested.retained[0].0.as_ref(), &[0, 2]);
        nested.delete_nested_rule(&[0], 1).unwrap();
        assert_eq!(nested.retained[0].0.as_ref(), &[0, 1]);
        let positions = Rc::new(RulePositions::default());
        let removed = positions.at(1);
        positions.retain_detached_declarations(&[0], Some(1), &nested.retained);
        assert_eq!(removed.detached_declarations.borrow()[0].0.as_ref(), &[0]);
        assert!(Rc::ptr_eq(&removed.detached_declarations.borrow()[0].1, &block));
        nested.delete_nested_rule(&[0], 1).unwrap();
        assert!(nested.retained.is_empty());
        sheet.replace(".replacement{}").unwrap();
        assert!(sheet.retained.is_empty());
        let source = "/*α*/ @media all { .a {color:red} .a {color:blue}}";
        let offsets = css::nesting::declaration_offsets(source, &[Arc::from([0usize, 0]), Arc::from([0usize, 1])]).unwrap();
        assert_eq!(offsets, vec![Some(source.find("{color:red}").unwrap() + 1), Some(source.find("{color:blue}").unwrap() + 1)]);
        let source = ".a {color:red;.b{} background:green; width:10px}";
        let offsets = css::nesting::declaration_offsets(source, &[Arc::from([0usize]), Arc::from([0usize, 0]), Arc::from([0usize, 1])]).unwrap();
        assert_eq!(offsets, vec![Some(source.find('{').unwrap() + 1), Some(source.find(".b{").unwrap() + 3), Some(source.find("background").unwrap())]);
    }

    #[test]
    fn cssom_detached_child_snapshot_keeps_adjacent_synthetic_identity_and_boundaries() {
        let mut sheet = CssStyleSheetText::parse(".a {color:red; > .child {width:10px} background:blue}").unwrap();
        sheet.insert_nested_rule(&[0], "color:orange", 0).unwrap();
        sheet.insert_nested_rule(&[0], "background:green", 1).unwrap();
        let children = sheet.rule_list(&[0]).unwrap();
        assert_eq!(children.len(), 4);
        let positions = Rc::new(RulePositions { nested_context: true, ..Default::default() });
        positions.synchronize_rules(&children).unwrap();
        let first = positions.at(0);
        let second = positions.at(1);
        let tail = positions.at(3);
        positions.replaced(".replacement {}").unwrap();
        assert!(first.nested_declarations.get());
        assert!(second.nested_declarations.get());
        assert!(tail.nested_declarations.get());
        assert!(first.detached.borrow().as_ref().unwrap().contains("color: orange"));
        assert!(second.detached.borrow().as_ref().unwrap().contains("background: green"));
        assert!(tail.detached.borrow().as_ref().unwrap().contains("background: blue"));
        assert_eq!(first.get(), 0);
        assert_eq!(second.get(), 0);
        assert!(positions.live.borrow().is_empty());
    }

    #[test]
    fn cssom_detached_nested_rules_keep_live_styles_descendants_and_external_source_identity() {
        let mut engine = lumen::Engine::new();
        let _realm = crate::install(engine.ctx(), r#"<style id="source">
            .outer { color:red; > .child {width:10px} background:blue }
            .other {height:11px}
        </style>"#, 128).unwrap();
        let result = engine.eval_value(r#"(() => {
            const fail = message => {throw new Error(message)};
            const element = document.getElementById("source");
            const sheet = element.sheet, parent = sheet.cssRules[0];
            parent.insertRule("color:orange", 0);
            parent.insertRule("background:green", 1);
            const first = parent.cssRules[0], second = parent.cssRules[1];
            const child = parent.cssRules[2], tail = parent.cssRules[3];
            if (!(first instanceof CSSNestedDeclarations) || !(second instanceof CSSNestedDeclarations) || first === second) fail("adjacent synthetic native identities");
            first.style.setProperty("--edge", "7px");
            first.style.setProperty("margin", "var(--edge)", "important");
            parent.deleteRule(0);
            if (first.parentRule !== null || first.parentStyleSheet !== null || first.style.margin !== "var(--edge)" || first.style.getPropertyPriority("margin") !== "important") fail("removed synthetic retains projected cohort");
            first.style.color = "purple";
            if (first.style.color !== "purple" || parent.cssRules[0] !== second || second.style.backgroundColor !== "green" || parent.style.color !== "red") fail("removed synthetic mutation must not target sibling");
            sheet.deleteRule(0);
            if (first.style.margin !== "var(--edge)" || first.style.color !== "purple") fail("independently removed child must retain its own source after ancestor deletion");
            first.style.width = "19px";
            if (first.style.width !== "19px" || second.style.width !== "") fail("removed child source takes precedence over subsequently removed ancestor");
            if (child.parentRule !== parent || tail.parentRule !== parent || child.parentStyleSheet !== null || parent.parentRule !== null) fail("detached ancestor ownership");
            child.style.width = "23px";
            tail.style.padding = "var(--edge)";
            if (parent.cssRules[1] !== child || child.style.width !== "23px" || parent.cssRules[2] !== tail || tail.style.padding !== "var(--edge)" || sheet.cssRules[0].style.height !== "11px") fail("detached ancestor descendants remain live independently");
            const replacement = sheet.cssRules[0];
            element.textContent = ".replacement {height:31px}";
            const freshSheet = element.sheet;
            if (freshSheet === sheet || sheet.ownerNode !== null || replacement.parentStyleSheet !== sheet || replacement.style.height !== "11px" || freshSheet.cssRules[0].style.height !== "31px") fail("external source replacement creates independent sheets");
            replacement.style.height = "17px";
            if (freshSheet.cssRules[0].style.height !== "31px" || replacement.style.height !== "17px") fail("external replacement detached identity");
            const other = new DOMParser().parseFromString("<html><body></body></html>", "text/html");
            other.adoptNode(element);
            child.style.width = "29px";
            if (child.style.width !== "29px") fail("detached child width after adoption: " + child.style.width);
            if (parent.cssRules[1] !== child) fail("detached child identity after adoption");
            if (first.style.margin !== "var(--edge)") fail("independently removed margin after adoption: " + first.style.margin);
            return true;
        })()"#).unwrap();
        let result = result.unwrap_or_else(|error| panic!("detached CSSOM script threw: {}", engine.ctx().coerce_string(&error).map(|value| value.to_string()).unwrap_or_default()));
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn shared_nesting_cssom_mutations_update_real_cascade_and_child_geometry() {
        let mut engine = lumen::Engine::new();
        let realm = crate::install(engine.ctx(), r#"<style>
            .outer, #missing { --edge: 7px; color: red;
                > .child { width: 20px; height: 10px; }
                color: blue;
                @media screen { color: green; > .child { height: 12px; } }
                color: purple;
            }
            .child { width: 3px; height: 4px; }
        </style><div class="outer"><div class="child"></div></div>"#, 128).unwrap();
        install_cssom_layout(&realm);
        let result = engine.eval_value(r#"(() => {
            const sheet = document.styleSheets[0], parent = sheet.cssRules[0];
            const outer = document.querySelector('.outer'), child = document.querySelector('.child');
            const fail = message => { throw new Error(message); };
            if (parent.cssRules.length !== 4 || parent.cssRules[0].selectorText !== '& > .child') fail('shared canonical children');
            const tail = parent.cssRules[1];
            if (!(tail instanceof CSSNestedDeclarations) || parent.style.color !== 'red' || tail.style.color !== 'blue') fail('leading and trailing declarations');
            if (getComputedStyle(outer).color !== 'rgb(128, 0, 128)' || child.getBoundingClientRect().width !== 20 || child.getBoundingClientRect().height !== 12) fail('nested cascade and geometry');
            tail.style.setProperty('margin', 'var(--edge)');
            parent.insertRule('& > .unmatched { width: 77px; }', 1);
            if (parent.cssRules[2] !== tail || tail.style.margin !== 'var(--edge)' || getComputedStyle(outer).marginLeft !== '7px') fail('retained tail rebasing and source offset');
            parent.selectorText = '.changed, #missing';
            if (child.getBoundingClientRect().width !== 3 || child.getBoundingClientRect().height !== 4) fail('parent selector invalidation');
            outer.className = 'changed';
            if (child.getBoundingClientRect().width !== 20 || child.getBoundingClientRect().height !== 12 || getComputedStyle(outer).marginLeft !== '7px') fail('live parent matching and pending declaration');
            return true;
        })()"#).unwrap();
        let result = result.unwrap_or_else(|error| panic!("nesting script threw: {}", engine.ctx().coerce_string(&error).map(|value| value.to_string()).unwrap_or_default()));
        assert!(matches!(result, Value::Bool(true)));
    }
    use super::*;

    #[test]
    fn inline_cssom_pending_shorthands_keep_live_cascade_and_mixed_priority() {
        let mut engine = lumen::Engine::new();
        crate::install(engine.ctx(), "<div id=target></div>", 64).unwrap();
        let result = engine.eval_value(r#"
            const element = document.getElementById('target');
            element.style.cssText = '--edges: 1px 2px 3px 4px; margin: var(--edges) !important';
            const style = element.style;
            const original = style.getPropertyValue('margin') === 'var(--edges)' &&
                style.getPropertyValue('margin-top') === '' &&
                style.getPropertyPriority('margin-top') === 'important' &&
                style.length === 5 && style[0] === '--edges' &&
                style.item(1) === 'margin-top' && style.item(5) === '';
            style.setProperty('margin-left', '9px');
            const partial = style.getPropertyValue('margin') === '' &&
                style.getPropertyValue('margin-left') === '9px' &&
                style.getPropertyPriority('margin-left') === '' &&
                getComputedStyle(element).getPropertyValue('margin-top') === '1px' &&
                getComputedStyle(element).getPropertyValue('margin-left') === '9px';
            element.attributeStyleMap.set('--edges', new CSSUnparsedValue(['5px 6px 7px 8px']));
            const updated = getComputedStyle(element).getPropertyValue('margin-top') === '5px' &&
                getComputedStyle(element).getPropertyValue('margin-left') === '9px';
            style.removeProperty('margin');
            original && partial && updated && style.getPropertyValue('margin-left') === '' &&
                getComputedStyle(element).getPropertyValue('margin-top') === '0px'
        "#).unwrap().unwrap_or_else(|_| panic!("retained CSSOM shorthand contract threw"));
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn stylesheet_cssom_pending_shorthands_survive_adoption_reindex_and_detachment() {
        let mut engine = lumen::Engine::new();
        crate::install(engine.ctx(), "<div id=target></div>", 64).unwrap();
        let result = engine.eval_value(r#"
            const element = document.getElementById('target');
            const sheet = new CSSStyleSheet();
            sheet.replaceSync('#target { --edges: 1px 2px 3px 4px; margin: var(--edges) !important }');
            const rule = sheet.cssRules[0];
            rule.style.setProperty('margin-left', '9px');
            document.adoptedStyleSheets = [sheet];
            const adopted = getComputedStyle(element).getPropertyValue('margin-top') === '1px' &&
                getComputedStyle(element).getPropertyValue('margin-left') === '9px';
            sheet.insertRule('#other { color: red }', 0);
            const reindexed = sheet.cssRules[1] === rule && rule.style.getPropertyValue('margin') === '' &&
                rule.style.getPropertyPriority('margin-top') === 'important' &&
                getComputedStyle(element).getPropertyValue('margin-left') === '9px';
            sheet.deleteRule(1);
            const removed = rule.style.getPropertyValue('margin-left') === '9px' &&
                rule.style.getPropertyPriority('margin-top') === 'important';
            rule.style.setProperty('margin-left', '7px');
            adopted && reindexed && removed && rule.style.getPropertyValue('margin-left') === '7px' &&
                getComputedStyle(element).getPropertyValue('margin-top') === '0px' && sheet.cssRules.length === 1
        "#).unwrap().unwrap_or_else(|_| panic!("retained stylesheet shorthand contract threw"));
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn css_escape_preserves_code_units_conversion_and_selector_roundtrip() {
        let mut engine = lumen::Engine::new();
        crate::install(engine.ctx(), "<div id=target></div>", 64).unwrap();
        let result = engine
            .eval_value(
                r#"
            const target = document.getElementById('target');
            target.id = '3:a b';
            const sentinel = {};
            let preserved = false, missing = false, symbol = false;
            try { CSS.escape({toString() { throw sentinel; }}); }
            catch (error) { preserved = error === sentinel; }
            try { CSS.escape(); } catch (error) { missing = error instanceof TypeError; }
            try { CSS.escape(Symbol()); } catch (error) { symbol = error instanceof TypeError; }
            const converted = CSS.escape({toString() { CSS.escape('-'); return 'a b'; }});
            preserved && missing && symbol && CSS.escape.length === 1 &&
                CSS.escape(null) === 'null' && CSS.escape(undefined) === 'undefined' &&
                CSS.escape(123) === '\\31 23' && converted === 'a\\ b' &&
                CSS.escape('\0') === '\uFFFD' &&
                CSS.escape('\uD834') === '\uD834' && CSS.escape('\uDF06') === '\uDF06' &&
                CSS.escape('\uD834\uDF06') === '\uD834\uDF06' &&
                document.querySelector('#' + CSS.escape(target.id)) === target &&
                new CSSKeywordValue('3:a b').toString() === CSS.escape('3:a b')
        "#,
            )
            .unwrap()
            .unwrap_or_else(|_| panic!("CSS.escape contract threw"));
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn typed_om_unparsed_values_reify_mutate_iterate_and_bound_cycles() {
        let mut engine = lumen::Engine::new();
        crate::install(engine.ctx(), "<main><div id='typed'></div></main>", 64).unwrap();
        let result = engine
            .eval_value(
                r#"
                const parsed = CSSStyleValue.parse('--source', 'calc(1px + var(--gap, var(--inner, 2px)))');
                const parsedReference = parsed[1];
                const nestedReference = parsedReference.fallback[1];
                const parsedShape = parsed instanceof CSSUnparsedValue && parsed.length === 3 &&
                    parsedReference instanceof CSSVariableReferenceValue &&
                    parsedReference.variable === '--gap' &&
                    parsedReference.fallback.length === 2 &&
                    parsedReference.fallback[0] === ' ' &&
                    parsedReference.fallback[1] instanceof CSSVariableReferenceValue &&
                    nestedReference.variable === '--inner' &&
                    parsed.toString() === 'calc(1px + var(--gap, var(--inner, 2px)))';

                const value = new CSSUnparsedValue(['left ', new CSSVariableReferenceValue('--x',
                    new CSSUnparsedValue(['fallback'])), ' right']);
                const descriptor = Object.getOwnPropertyDescriptor(value, '1');
                value[3] = ' end';
                value[1] = new CSSVariableReferenceValue('--y', null);
                value.length = 99;
                let gapRejected = false;
                try { value[5] = 'gap'; } catch (error) { gapRejected = error instanceof RangeError; }
                const iterated = Array.from(value, item => typeof item === 'string' ? item : item.variable);
                const entries = Array.from(value.entries());
                const indexed = descriptor.enumerable && descriptor.configurable && descriptor.writable &&
                    value.length === 4 && value[1].variable === '--y' &&
                    value.toString() === 'left var(--y) right/**/ end' &&
                    iterated.join('|') === 'left |--y| right| end' &&
                    entries.length === 4 && entries[1][0] === 1 &&
                    entries[1][1].variable === '--y' && gapRejected &&
                    Array.from(value.keys()).join(',') === '0,1,2,3' &&
                    Object.keys(value).join(',') === '0,1,2,3';

                const element = document.getElementById('typed');
                const map = element.attributeStyleMap;
                map.set('--stored', new CSSUnparsedValue(['a', new CSSVariableReferenceValue('--b'), 'c']));
                const stored = map.get('--stored');
                const styleMap = stored instanceof CSSUnparsedValue &&
                    stored.toString() === 'avar(--b)c' && map.has('--stored');

                const cyclic = new CSSUnparsedValue([]);
                cyclic[0] = new CSSVariableReferenceValue('--cycle', cyclic);
                let cycleRejected = false;
                try { cyclic.toString(); } catch (error) { cycleRejected = error instanceof RangeError; }
                let invalidNameRejected = false;
                try { new CSSVariableReferenceValue('not-a-custom-property'); }
                catch (error) { invalidNameRejected = error instanceof TypeError; }
                let invalidUpdateRejected = false;
                const variable = new CSSVariableReferenceValue('--valid');
                try { variable.variable = 'invalid'; }
                catch (error) { invalidUpdateRejected = error instanceof TypeError; }
                const fallbackDefault = variable.fallback === null &&
                    Object.getOwnPropertyDescriptor(variable, 'fallback') === undefined;
                const usvConversion = new CSSUnparsedValue(['\uD800'])[0] === '\uFFFD';
                [
                    !parsedShape && 'parsed-shape',
                    !indexed && 'indexed-getter-setter-iteration',
                    !styleMap && 'style-property-map-roundtrip',
                    !cycleRejected && 'cycle-bound',
                    !invalidNameRejected && 'constructor-name-validation',
                    !invalidUpdateRejected && 'variable-setter-validation',
                    !fallbackDefault && 'fallback-default-and-visibility',
                    !usvConversion && 'usv-segment-conversion'
                ].filter(Boolean).join('|')
            "#,
            )
            .unwrap()
            .unwrap_or_else(|_| panic!("CSS Typed OM unparsed-value contract threw"));
        let Value::Str(failures) = result else {
            panic!("CSS Typed OM unparsed-value diagnostics must be a string");
        };
        assert!(failures.is_empty(), "failed checks: {failures}");
    }

    #[test]
    fn color_scheme_typed_values_and_escaped_custom_properties_round_trip() {
        let mut engine = lumen::Engine::new();
        crate::install(
            engine.ctx(),
            "<div id='parent' style='color-scheme: dark only'><span id='child'></span></div><div id='typed' style='color-scheme: bar/*comment*/var(--foo)'></div><div id='escaped' style='--a\\,fail: pass; --unparsed:var(--a\\,fail)'></div>",
            64,
        )
        .unwrap();
        let result = engine
            .eval_value(
                r#"
                (() => {
                    const typed = document.getElementById('typed');
                    const escaped = document.getElementById('escaped');
                    const keyword = CSSStyleValue.parse('color-scheme', 'dark');
                    const list = CSSStyleValue.parse('color-scheme', 'light dark');
                    const only = CSSStyleValue.parse('color-scheme', 'only light');
                    let invalid = false;
                    try { CSSStyleValue.parse('color-scheme', 'light normal'); }
                    catch (error) { invalid = error instanceof TypeError; }
                    const unparsed = typed.attributeStyleMap.get('color-scheme');
                    const specified = escaped.attributeStyleMap.get('--unparsed');
                    const computedBefore = getComputedStyle(escaped).getPropertyValue('--unparsed');
                    escaped.style.setProperty('--unparsed', specified.toString());
                    const computedAfter = getComputedStyle(escaped).getPropertyValue('--unparsed');
                    const inherited = getComputedStyle(document.getElementById('child'))
                        .getPropertyValue('color-scheme');
                    return [
                        !(keyword instanceof CSSKeywordValue && keyword.value === 'dark') && 'keyword',
                        !(list instanceof CSSStyleValue && !(list instanceof CSSKeywordValue) &&
                            list.toString() === 'light dark') && 'list-value',
                        !(only instanceof CSSStyleValue && !(only instanceof CSSKeywordValue) &&
                            only.toString() === 'light only') && 'only-order',
                        !invalid && 'invalid-grammar',
                        !(unparsed instanceof CSSUnparsedValue && unparsed.toString() === 'bar/**/var(--foo)') && 'variable-unparsed',
                        computedBefore !== 'pass' && 'escaped-var-computed',
                        computedAfter !== 'pass' && 'escaped-var-roundtrip',
                        inherited !== 'dark only' && 'inherited-color-scheme'
                    ].filter(Boolean).join('|')
                })()
            "#,
            )
            .unwrap()
            .unwrap_or_else(|_| panic!("color-scheme Typed OM contract threw"));
        let Value::Str(failures) = result else {
            panic!("color-scheme Typed OM diagnostics must be a string");
        };
        assert!(failures.is_empty(), "failed checks: {failures}");
    }

    #[test]
    fn import_stylesheet_is_backed_by_loaded_child_and_edits_renderer_source() {
        let mut engine = lumen::Engine::new();
        let realm = crate::install(
            engine.ctx(),
            "<style>@import 'theme.css';</style><div class='after'></div>",
            64,
        )
        .unwrap();
        let (style_node, root_source) = realm.with_session(|session| {
            let document = session.document();
            let style_node =
                lumen_html::selector::query_selector(document, document.root(), "style")
                    .unwrap()
                    .unwrap();
            let text = "@import 'theme.css';";
            let rule = css::imports(text).unwrap().remove(0);
            let child = css::StylesheetSource {disabled:false,
                url: Arc::from("https://example.test/theme.css"),
                text: Arc::from(".before { opacity: 0.25 }"),
                imports: Vec::new(),
            };
            let root = css::StylesheetSource {disabled:false,
                url: Arc::from("https://example.test/main.css"),
                text: Arc::from(text),
                imports: vec![css::LoadedImport {
                    rule,
                    source: Some(Box::new(child)),
                }],
            };
            (style_node, root)
        });
        realm.with_session(|session| {
            session
                .set_stylesheet_source(style_node, Some(root_source))
                .unwrap()
        });
        let script = "const imp=document.querySelector('style').sheet.cssRules[0]; const child=imp.styleSheet; if(!(child instanceof CSSStyleSheet)||child.ownerRule!==imp||child.href!=='https://example.test/theme.css'||child.cssRules[0].selectorText!=='.before') throw new Error('loaded child sheet'); child.insertRule('.after { opacity: 0.75 }', 1); if(child.cssRules.length!==2) throw new Error('child mutation');";
        if let Err(error) = engine.eval_value(script).unwrap() {
            let message = engine
                .ctx()
                .coerce_string(&error)
                .map(|value| value.to_string())
                .unwrap_or_else(|_| "unprintable exception".into());
            panic!("import stylesheet CSSOM failed: {message}");
        }
        realm.with_session(|session| {
            let document = session.document();
            let node = lumen_html::selector::query_selector(document, document.root(), ".after")
                .unwrap()
                .unwrap();
            assert_eq!(session.computed_style(node).unwrap().opacity, 0.75);
        });
    }

    #[test]
    fn specification_cssom_import_disabled_preserves_occurrences_nested_graphs_and_detached_state() {
        let mut engine=lumen::Engine::new();
        let text="@import 'dup.css'; @import 'dup.css'; div{background-color:blue}";
        let realm=crate::install(engine.ctx(),&format!("<style>{text}</style><div id=box></div>"),192).unwrap();
        let node=realm.with_session(|session|lumen_html::selector::query_selector(session.document(),session.document().root(),"style").unwrap().unwrap());
        let first_text="@import 'nested.css'; div{width:7px}";
        realm.with_session(|session|session.set_stylesheet_source(node,Some(css::StylesheetSource{disabled:false,
            url:Arc::from("https://example.test/root.css"),text:Arc::from(text),
            imports:css::imports(text).unwrap().into_iter().zip([
                css::StylesheetSource{disabled:false,url:Arc::from("https://example.test/dup.css"),text:Arc::from(first_text),imports:vec![css::LoadedImport{
                    rule:css::imports(first_text).unwrap().remove(0),source:Some(Box::new(css::StylesheetSource{disabled:false,
                        url:Arc::from("https://example.test/nested.css"),text:Arc::from("div{opacity:.25}"),imports:Vec::new()}))}]},
                css::StylesheetSource{disabled:false,url:Arc::from("https://example.test/dup.css"),text:Arc::from("div{width:9px}"),imports:Vec::new()},
            ]).map(|(rule,source)|css::LoadedImport{rule,source:Some(Box::new(source))}).collect(),
        })).unwrap());
        let result=engine.eval_value(r#"(() => {
            const check=(ok,message)=>{if(!ok)throw Error(message)};
            const root=document.querySelector('style').sheet, a=root.cssRules[0].styleSheet, b=root.cssRules[1].styleSheet;
            const nested=a.cssRules[0].styleSheet, aRules=a.cssRules, aRule=aRules[1], nestedRule=nested.cssRules[0];
            const style=()=>getComputedStyle(document.getElementById('box'));
            check(a!==b && !a.disabled && !b.disabled && !nested.disabled,'distinct enabled source occurrences');
            check(style().width==='9px' && style().opacity==='0.25','initial loaded cascade');
            a.disabled=true;
            check(a.disabled && !b.disabled && !nested.disabled && style().width==='9px' && style().opacity==='1','disabled parent suppresses nested rendering only');
            check(a.cssRules===aRules && aRules[1]===aRule && nested.cssRules[0]===nestedRule,'disabled state retains accessible rules and identity');
            b.disabled=true;check(style().width!=='9px','same URL second occurrence changes independently');
            a.disabled=false;check(style().width==='7px' && style().opacity==='0.25','reenable uses retained graph');
            nested.disabled=true;check(style().width==='7px' && style().opacity==='1','nested occurrence independent flag');
            a.disabled=true;a.disabled=false;check(nested.disabled && style().opacity==='1','parent toggle preserves child flag');
            nested.disabled=false;check(style().opacity==='0.25','nested reenable');
            root.insertRule('@import "new.css";',0);
            check(root.cssRules[1].styleSheet===a && root.cssRules[2].styleSheet===b && b.disabled,'structural rebase preserves actual flag identities');
            a.disabled=true;root.deleteRule(1);
            check(a.parentStyleSheet===null && a.disabled && a.cssRules===aRules,'detached graph retains flag and wrappers');
            b.disabled=false;check(style().width==='9px' && style().opacity==='1','removed nested graph no longer contributes');
            a.disabled=false;nested.disabled=true;aRule.style.width='23px';
            check(!a.disabled && nested.disabled && aRule.style.width==='23px' && style().width==='9px' && style().opacity==='1','detached flag edits never affect live document');
            return true;
        })()"#).unwrap();
        let value=result.unwrap_or_else(|error|panic!("import disabled occurrence semantics: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));
        assert!(matches!(value,Value::Bool(true)));
    }

    #[test]
    fn specification_cssom_import_media_identity_detachment_and_replacement_errors() {
        let mut engine = lumen::Engine::new();
        let text = "@import 'theme.css' screen;";
        let realm = crate::install(engine.ctx(), &format!("<style>{text}</style><div></div>"), 64).unwrap();
        let node = realm.with_session(|session| lumen_html::selector::query_selector(session.document(), session.document().root(), "style").unwrap().unwrap());
        let source = css::StylesheetSource {disabled:false,
            url: Arc::from("https://example.test/main.css"), text: Arc::from(text),
            imports: vec![css::LoadedImport {
                rule: css::imports(text).unwrap().remove(0),
                source: Some(Box::new(css::StylesheetSource {disabled:false,
                    url: Arc::from("https://example.test/theme.css"), text: Arc::from("div {width:7px}"), imports: Vec::new(),
                })),
            }],
        };
        realm.with_session(|session| session.set_stylesheet_source(node, Some(source)).unwrap());
        let result = engine.eval_value(r#"(() => {
            const fail = message => {throw new Error(message)};
            const root = document.querySelector('style').sheet, rule = root.cssRules[0], child = rule.styleSheet;
            if (!child || child.ownerRule !== rule || child.parentStyleSheet !== root) fail('actual loaded import ownership');
            const media = child.media;
            if (media !== rule.media || media !== child.media || media.mediaText !== 'screen') fail('import and child share their media object');
            media.mediaText = 'print';
            if (rule.media.mediaText !== 'print' || !rule.cssText.includes('print')) fail('child media edits the owning import');
            for (const sheet of [root, child]) {
                let rejected = false;
                try {sheet.replaceSync('div {width:99px}')} catch (error) {rejected = error instanceof DOMException && error.name === 'NotAllowedError'}
                if (!rejected) fail('nonconstructed synchronous replacement');
            }
            globalThis.importReplacementRejected = 0;
            for (const sheet of [root, child]) sheet.replace('div {width:99px}').then(() => fail('nonconstructed async replacement resolved'), error => {
                if (!(error instanceof DOMException) || error.name !== 'NotAllowedError') fail('canonical rejected replacement');
                ++importReplacementRejected;
            });
            root.deleteRule(0);
            if (child.parentStyleSheet !== null || child.ownerRule !== rule || child.media !== media || rule.media !== media) fail('detached import retains actual media identity');
            media.mediaText = 'screen';
            if (rule.media.mediaText !== 'screen' || child.cssRules[0].style.width !== '7px' || root.cssRules.length !== 0) fail('detached editing must not reinsert or replace rules');
            return true;
        })()"#).unwrap();
        let value=result.unwrap_or_else(|error|panic!("import media ownership failed: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));
        assert!(matches!(value,Value::Bool(true)));
        engine.ctx().drain_microtasks_for_host();
        let value=engine.eval_value("importReplacementRejected === 2").unwrap().ok().unwrap();
        assert!(matches!(value,Value::Bool(true)));
    }

    #[test]
    fn imported_cssom_duplicate_occurrences_rebase_live_owners_and_detached_nested_graphs() {
        let mut engine = lumen::Engine::new();
        let text = "@import 'dup.css'; @import 'dup.css'; .box {height:3px}";
        let realm = crate::install(engine.ctx(), &format!("<style>{text}</style><div class=box></div>"), 64).unwrap();
        let node = realm.with_session(|session| lumen_html::selector::query_selector(session.document(), session.document().root(), "style").unwrap().unwrap());
        let nested_text = "@import 'nested.css'; .box {width:7px}";
        let first = css::StylesheetSource {disabled:false,
            url: Arc::from("https://example.test/dup.css"), text: Arc::from(nested_text),
            imports: vec![css::LoadedImport { rule: css::imports(nested_text).unwrap().remove(0), source: Some(Box::new(css::StylesheetSource {disabled:false,
                url: Arc::from("https://example.test/nested.css"), text: Arc::from(".nested {height:5px}"), imports: Vec::new(),
            })) }],
        };
        let second_text = "@import 'later.css'; .box {width:9px}";
        let second = css::StylesheetSource {disabled:false,  url: Arc::from("https://example.test/dup.css"), text: Arc::from(second_text), imports: vec![css::LoadedImport {
            rule: css::imports(second_text).unwrap().remove(0), source: Some(Box::new(css::StylesheetSource {disabled:false,
                url: Arc::from("https://example.test/later.css"), text: Arc::from(".later {height:6px}"), imports: Vec::new(),
            })),
        }] };
        realm.with_session(|session| session.set_stylesheet_source(node, Some(css::StylesheetSource {disabled:false,
            url: Arc::from("https://example.test/root.css"), text: Arc::from(text),
            imports: css::imports(text).unwrap().into_iter().zip([first, second]).map(|(rule, source)| css::LoadedImport { rule, source: Some(Box::new(source)) }).collect(),
        })).unwrap());
        let result = engine.eval_value(r#"(() => {
            const fail = message => {throw new Error(message)};
            const element = document.querySelector('style'), sheet = element.sheet;
            const firstRule = sheet.cssRules[0], secondRule = sheet.cssRules[1];
            const first = firstRule.styleSheet, second = secondRule.styleSheet;
            if (first === second || first.ownerRule !== firstRule || second.ownerRule !== secondRule || first.parentStyleSheet !== sheet || second.parentStyleSheet !== sheet) fail('distinct live import ownership');
            if (first.cssRules[1].style.width !== '7px' || second.cssRules[1].style.width !== '9px') fail('same URL must select actual occurrence');
            const nestedRule = first.cssRules[0], nested = nestedRule.styleSheet;
            first.cssRules[1].style.width = '17px'; second.cssRules[1].style.width = '19px';
            nested.cssRules[0].style.height = '13px';
            sheet.insertRule('@import "dup.css";', 0);
            if (sheet.cssRules[1] !== firstRule || sheet.cssRules[2] !== secondRule || sheet.cssRules[1].styleSheet !== first || second.cssRules[1].style.width !== '19px' || first.cssRules[1].style.width !== '17px') fail('inserted duplicate must not consume an edited occurrence');
            sheet.deleteRule(1);
            if (first.parentStyleSheet !== null || first.ownerRule !== firstRule || sheet.cssRules[1] !== secondRule || second.parentStyleSheet !== sheet) fail('removed import unlinking and surviving rebasing');
            if (getComputedStyle(document.querySelector('.box')).width !== '19px') fail('renderer must retain the surviving edited occurrence');
            let rejected = false;
            try { sheet.insertRule('@import "dup.css"; .extra{}', 0); } catch (error) { rejected = error.name === 'SyntaxError'; }
            if (!rejected || sheet.cssRules[1] !== secondRule || second.cssRules[1].style.width !== '19px') fail('invalid insertion must not alter live graph identities');
            if (nested.parentStyleSheet !== first || nested.cssRules[0].style.height !== '13px') fail('removed import retains loaded descendants');
            first.cssRules[1].style.width = '23px'; nested.cssRules[0].style.height = '17px';
            if (second.cssRules[1].style.width !== '19px' || first.cssRules[1].style.width !== '23px') fail('detached graph edits are independent');
            first.deleteRule(0);
            if (nested.parentStyleSheet !== null || nested.ownerRule !== nestedRule || nested.cssRules[0].style.height !== '17px') fail('independently removed nested import retains its graph');
            nested.cssRules[0].style.height = '29px';
            const saved = element.textContent;
            element.textContent = '.replacement {width:41px}';
            element.textContent = saved;
            const freshSheet = element.sheet, freshRule = freshSheet.cssRules[1];
            if (freshSheet === sheet || freshRule === secondRule || second.parentStyleSheet !== sheet || sheet.ownerNode !== null || second.cssRules[1].style.width !== '19px') fail('external ABA creates fresh sheet and preserves old occurrence');
            const laterRule = second.cssRules[0], later = laterRule.styleSheet;
            if (!later || later.parentStyleSheet !== second || later.cssRules[0].style.height !== '6px') fail('external graph escrow preserves previously unaccessed descendants');
            freshSheet.cssRules[2].style.width = '41px';
            second.cssRules[1].style.width = '31px'; later.cssRules[0].style.height = '37px';
            if (getComputedStyle(document.querySelector('.box')).width !== '41px' || second.cssRules[1].style.width !== '31px') fail('expired graph mutations must not affect fresh source rendering');
            const other = new DOMParser().parseFromString('<html><body></body></html>', 'text/html');
            other.adoptNode(element);
            if (first.cssRules[0].style.width !== '23px' || nested.cssRules[0].style.height !== '29px' || second.cssRules[1].style.width !== '31px' || later.cssRules[0].style.height !== '37px') fail('retained detached imports survive original owner adoption');
            return true;
        })()"#).unwrap();
        let result = result.unwrap_or_else(|error| panic!("import occurrence script threw: {}", engine.ctx().coerce_string(&error).map(|value| value.to_string()).unwrap_or_default()));
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn font_face_cssom_mutations_update_the_renderer_font_records() {
        let mut engine = lumen::Engine::new();
        let realm = crate::install(engine.ctx(),
            "<style>@FONT-FACE /* descriptors */ {font-family: ExistingFace;src:url('initial.woff');font-weight:700/* weight */;font-weight:invalid;unicode-range:U+41}</style><main>AB</main>", 64).unwrap();
        let before = realm.session.borrow_mut().font_faces().unwrap();
        assert_eq!(before[0].family.as_ref(), "ExistingFace");
        assert_eq!(before[0].weight, 700);
        let source = r#"
            var sheet=document.querySelector('style').sheet;
            var face=sheet.cssRules[0], declaration=face.style;
            if (!(face instanceof CSSFontFaceRule) || !(face instanceof CSSRule)
                || face.type!==5 || face.parentStyleSheet!==sheet || face.parentRule!==null
                || face.style!==declaration || declaration.parentRule!==face
                || !(declaration instanceof CSSFontFaceDescriptors))
                throw new Error('font-face interfaces and ownership');
            if(declaration.fontWeight!=='700' || declaration.length!==4
                || declaration.item(0)!=='font-family' || declaration[1]!=='src'
                || declaration.item(99)!=='') throw new Error('font descriptor enumeration: '
                    +declaration.fontWeight+'|'+declaration.length+'|'+declaration.item(0)
                    +'|'+declaration[1]+'|'+declaration.item(99));
            declaration.fontFamily='Updated';
            declaration.src='url("updated;face.woff")';
            declaration.fontWeight='400 800';
            declaration.unicodeRange='U+42-43';
            declaration.fontDisplay='swap';
            declaration.fontStretch='75%';
            if(declaration.fontWidth!=='75%' || declaration['font-width']!=='75%')
                throw new Error('font-width legacy alias: '+declaration.fontWidth+'|'
                    +declaration['font-width']+'|'+declaration.fontStretch+'|'+declaration.cssText);
            declaration['font-weight']=600;
            if(declaration.fontWeight!=='600') throw new Error('descriptor aliases and coercion');
            declaration['font-weight']='400 800';
            declaration.fontDisplay=null;
            if(declaration.fontDisplay!=='') throw new Error('descriptor null conversion');
            declaration.fontDisplay='swap';
            declaration.setProperty('color','red');
            declaration.setProperty('font-weight','900','important');
            declaration.setProperty('font-weight','invalid');
            if(declaration.fontWeight!=='400 800' || declaration.getPropertyValue('color')!==''
                || declaration.getPropertyPriority('font-weight')!=='')
                throw new Error('font descriptor validation');
            if(!face.cssText.includes('updated;face.woff') || declaration.length!==6)
                throw new Error('font descriptor serialization');
            if(declaration.fontDisplay!=='swap' || declaration.fontWidth!=='75%'
                || declaration.unicodeRange!=='U+42-43')
                throw new Error('font descriptors lost through live mutation');
        "#;
        if let Err(error) = engine.eval_value(source).unwrap() {
            let message = engine
                .ctx()
                .coerce_string(&error)
                .map(|value| value.to_string())
                .unwrap_or_else(|_| "unprintable exception".into());
            panic!("font-face CSSOM script failed: {message}");
        }
        let after = realm.session.borrow_mut().font_faces().unwrap();
        assert_eq!(after[0].family.as_ref(), "Updated");
        assert_eq!(after[0].weight_range, [400, 800]);
        assert_eq!(after[0].display, css::FontDisplay::Swap);
        assert_eq!(after[0].stretch, 75.0);
        assert_eq!(after[0].unicode_range.as_deref(), Some("U+42-43"));
        assert_eq!(
            after[0].sources.as_ref(),
            [css::FontFaceSource::Url("updated;face.woff".into())]
        );
        if let Err(error) = engine.eval_value(r#"
            face.style='font-family: Replaced; src: url(replaced.woff); font-weight: 600; font-weight: broken; font-display: block !important; color:red';
            if(declaration.fontFamily!=='Replaced' || declaration.fontWeight!=='600'
                || declaration.length!==3 || declaration.fontDisplay!=='')
                throw new Error('font descriptor replacement');
            if(declaration.removeProperty('font-weight')!=='600' || declaration.length!==2)
                throw new Error('font descriptor removal');
            var detachedSheet=new CSSStyleSheet();
            detachedSheet.replaceSync('@font-face {font-family: Adopted; src:url(adopted.woff)}');
            detachedSheet.cssRules[0].style.fontFamily='AdoptedUpdated';
            document.adoptedStyleSheets=[detachedSheet];
        "#).unwrap() {
            let message = engine.ctx().coerce_string(&error)
                .map(|value| value.to_string()).unwrap_or_else(|_| "unprintable exception".into());
            panic!("font-face replacement script failed: {message}");
        }
        let replaced = realm.session.borrow_mut().font_faces().unwrap();
        assert_eq!(replaced.len(), 2);
        assert!(replaced
            .iter()
            .any(|face| face.family.as_ref() == "Replaced" && face.weight == 400));
        assert!(replaced
            .iter()
            .any(|face| face.family.as_ref() == "AdoptedUpdated"));
    }

    #[test]
    fn supports_uses_the_renderer_property_registry() {
        assert!(css::supports_property_value("display", "grid"));
        assert!(css::supports_property_value("width", "calc(50% - 2px)"));
        assert!(!css::supports_property_value("display", "spaceship"));
        assert!(!css::supports_property_value("unknown-property", "1px"));
    }

    #[test]
    fn declaration_cssom_preserves_importance_and_rejects_invalid_values() {
        let mut declaration = CssDeclaration::parse("color: red !important; width: 10px");
        declaration.set_property("width", "invalid", false).unwrap();
        assert_eq!(
            declaration.get_property_value("width").unwrap(),
            Some(("10px".into(), false))
        );
        declaration.set_property("width", "12px", true).unwrap();
        assert_eq!(
            declaration.get_property_value("width").unwrap(),
            Some(("12px".into(), true))
        );
    }

    #[test]
    fn specification_cssom_selector_serialization_live_detached_and_namespace_context() {
        let mut engine=lumen::Engine::new();let _realm=crate::install(engine.ctx(),"<style id=owner>@namespace url('http://www.w3.org/1999/xhtml');@namespace same url('http://www.w3.org/1999/xhtml');@namespace foreign url('urn:foreign'); same|*.target {color:green} @media all {foreign|e{height:3px}}</style><span class=target></span>",256).unwrap();
        let result=engine.eval_value(r#"(() => {
            const check=(value,message)=>{if(!value)throw Error(message)};
            const sheet=document.getElementById('owner').sheet, rule=sheet.cssRules[3], group=sheet.cssRules[4], child=group.cssRules[0];
            check(rule.selectorText==='.target' && rule.cssText.startsWith('.target {'),'parsed type namespace and redundant universal serialization');
            rule.selectorText='same|*.target > *|*.child';
            check(rule.selectorText==='.target > *|*.child','setter uses associated stylesheet namespace context');
            rule.selectorText=':is(*.target, .target, missing|e)';
            check(rule.selectorText===':is(*.target, .target)','logical default-namespace subject stays explicit and invalid forgiving branch is discarded');
            rule.selectorText=':not(missing|e)';check(rule.selectorText===':is(*.target, .target)','strict invalid setter preserves existing parsed rule');
            sheet.deleteRule(4);
            check(group.parentStyleSheet===null && child.parentStyleSheet===null && child.parentRule===group && child.selectorText==='foreign|e','detached nested rules retain literal namespace identities');
            child.style.setProperty('width','4px');
            check(child.selectorText==='foreign|e' && child.style.width==='4px' && group.cssRules[0]===child,'detached declaration mutation preserves namespaced parsed subtree');
            child.selectorText='foreign|changed';check(child.selectorText==='foreign|e','detached selector setter has no associated namespace scope');
            child.selectorText=' *.replacement ';
            check(child.selectorText==='.replacement' && group.cssRules[0]===child,'detached selector replacement uses empty associated namespace context and stable wrapper');
            sheet.insertRule('foreign|e {height:7px}',sheet.cssRules.length);
            check(sheet.cssRules[sheet.cssRules.length-1].selectorText==='foreign|e','inserted rule shares actual stylesheet context');
            const fresh=new CSSStyleSheet();fresh.replaceSync(':nth-child(odd){color:red}:lang(EN-us){color:blue}');
            check(fresh.cssRules[0].selectorText===':nth-child(2n+1)' && fresh.cssRules[1].selectorText===':lang("EN-us")','functional arguments share canonical parser and string serializer');
            return true;
        })()"#).unwrap();
        let value=result.unwrap_or_else(|error|panic!("selector serialization: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));
        assert!(matches!(value,Value::Bool(true)));
    }

    #[test]
    fn specification_cssom_namespace_interfaces_scoped_mutations_and_detached_identity() {
        let mut engine=lumen::Engine::new();let realm=crate::install(engine.ctx(),"<body><div id=container></div></body>",192).unwrap();install_cssom_layout(&realm);
        let result=engine.eval_value(r#"(() => {
            const check=(ok,message)=>{if(!ok)throw Error(message)};
            const sheet=new CSSStyleSheet({baseURL:'https://namespace.test/base/'});
            sheet.replaceSync('@namespace "http://www.w3.org/1999/xhtml";@namespace S "http://www.w3.org/2000/svg";S|*.x{width:12px}');
            const rules=sheet.cssRules,namespace=rules[1],rule=rules[2];
            check(namespace instanceof CSSNamespaceRule && namespace instanceof CSSRule && namespace.type===10 && CSSRule.NAMESPACE_RULE===10,'typed namespace inheritance and constant');
            check(namespace.prefix==='S' && namespace.namespaceURI==='http://www.w3.org/2000/svg' && namespace.parentStyleSheet===sheet && namespace.parentRule===null,'actual namespace metadata and parentage');
            check(rules[0].prefix==='' && rules[0].cssText==='@namespace url("http://www.w3.org/1999/xhtml");','default prefix and canonical URL token');
            namespace.prefix='wrong';namespace.namespaceURI='wrong';namespace.cssText='@namespace changed "wrong";';
            check(namespace.prefix==='S' && namespace.namespaceURI==='http://www.w3.org/2000/svg','readonly namespace values');
            const svg=document.createElementNS('http://www.w3.org/2000/svg','different:item');svg.setAttribute('class','x');document.getElementById('container').append(svg);
            document.adoptedStyleSheets=[sheet];check(getComputedStyle(svg).width==='12px','stylesheet prefix independent of XML element prefix');
            rule.selectorText='.x';check(getComputedStyle(svg).width!=='12px','implicit universal uses default namespace');
            rule.selectorText='S|*.x';check(getComputedStyle(svg).width==='12px','selector setter resolves actual sheet namespace');
            const before=rules.length;sheet.insertRule('S|item {height:7px}',before);check(getComputedStyle(svg).height==='7px','insertRule stylesheet context');
            rule.selectorText='missing|*.x';check(rule.selectorText==='S|*.x','unknown setter namespace rejected without mutation');
            let syntax=false;try{document.querySelector('S|item')}catch(e){syntax=e.name==='SyntaxError'}check(syntax,'DOM queries cannot borrow sheet namespace declarations');
            const groupIndex=sheet.cssRules.length;sheet.insertRule('@media all {}',groupIndex);const group=sheet.cssRules[groupIndex];group.insertRule('S|item {opacity:.5}',0);check(getComputedStyle(svg).opacity==='0.5','nested CSSOM insertion keeps sheet namespace scope');
            const sameType=document.createElementNS('http://www.w3.org/2000/svg','another:item');document.getElementById('container').append(sameType);
            sheet.insertRule('S|item:nth-of-type(2){height:9px}',sheet.cssRules.length);
            check(getComputedStyle(sameType).height==='9px','type sibling identity uses namespace plus local name, not authored prefix');
            const host=document.createElement('div');document.body.append(host);const shadow=host.attachShadow({mode:'open'});
            shadow.innerHTML='<style>@namespace "urn:foreign"; :is(:host){width:21px;height:10px}</style>';
            check(getComputedStyle(host).width==='21px','implicit default namespace cannot exclude a real featureless host through logical selectors');
            sheet.replaceSync('@namespace P "../literal#name";');
            check(namespace.parentStyleSheet===null && namespace.parentRule===null && namespace.prefix==='S','replacement detaches namespace wrapper snapshot');
            check(sheet.cssRules[0].namespaceURI==='../literal#name' && sheet.cssRules[0].cssText==='@namespace P url("../literal#name");','namespace URI never resolved against sheet URL');
            return true;
        })()"#).unwrap();
        let value=result.unwrap_or_else(|error|panic!("namespace CSSOM: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));assert!(matches!(value,Value::Bool(true)));
    }

    #[test]
    fn specification_cssom_rule_headers_enforce_order_and_namespace_state_transactionally() {
        let recovered=CssStyleSheetText::parse(".body{} @import url(late.css); @namespace late url(urn:late);").unwrap();
        assert_eq!(recovered.css_rules().unwrap().len(),1,"initial parsing discards late header rules rather than exposing invalid CSSOM objects");
        let blocks=CssStyleSheetText::parse("@layer base{} @import url(late.css);").unwrap();
        assert_eq!(blocks.css_rules().unwrap().len(),1,"a layer block closes the import prefix");
        let mut sheet=CssStyleSheetText::parse("@layer base; @import url(a.css); @namespace n url(urn:n);").unwrap();
        let before=sheet.clone();
        assert!(matches!(sheet.insert_rule(".before {}",0),Err(CssRuleMutationError::Hierarchy)));
        assert_eq!(sheet,before,"failed insertion preserves source, occurrences and retained declarations");
        assert!(matches!(sheet.insert_rule("@import url(b.css);",3),Err(CssRuleMutationError::Hierarchy)));
        assert!(matches!(sheet.insert_rule("@namespace m url(urn:m);",3),Err(CssRuleMutationError::InvalidState)));
        sheet.insert_rule(".after {}",3).unwrap();
        let before=sheet.clone();
        assert!(matches!(sheet.delete_rule(2),Err(CssRuleMutationError::InvalidState)));
        assert_eq!(sheet,before,"failed namespace removal preserves live topology");
        sheet.delete_rule(3).unwrap();
        assert!(matches!(sheet.delete_rule(2),Err(CssRuleMutationError::InvalidState)),"a layer statement is another rule type even after body rules are removed");
        sheet.delete_rule(0).unwrap();
        sheet.delete_rule(1).unwrap();
        let mut headers=CssStyleSheetText::parse("@import url(a.css);").unwrap();
        headers.insert_rule("@namespace m url(urn:m);",1).unwrap();
        headers.delete_rule(1).unwrap();
        headers.insert_rule("@layer trailing;",1).unwrap();
        assert!(matches!(headers.insert_rule("@import url(b.css);",2),Err(CssRuleMutationError::Hierarchy)),"layer statements cannot interleave consecutive imports");
        headers.insert_rule("@layer leading;",0).unwrap();
        assert_eq!(headers.css_rules().unwrap().len(),3,"leading layer statements retain the import-prefix exception");
        let recovered=CssStyleSheetText::parse("@import url(a.css);@layer end;@import url(b.css);").unwrap();
        assert_eq!(recovered.css_rules().unwrap().len(),2,"interleaved late imports are discarded during initial parsing");
    }

    #[test]
    fn specification_cssom_rule_mutation_errors_follow_method_and_parser_phases() {
        let mut engine=lumen::Engine::new();
        crate::install(engine.ctx(),"<style id=source>@import url(a.css); @namespace n url(urn:n);</style>",64).unwrap();
        let value=engine.eval_value(r#"(() => {
            const sheet=document.getElementById("source").sheet;
            const error=(action,name,code)=>{try{action();return false;}catch(e){return e instanceof DOMException&&e.name===name&&e.code===code;}};
            if(!error(()=>sheet.insertRule("???",99),"SyntaxError",12))return false;
            if(!error(()=>sheet.insertRule(".valid{}",99),"IndexSizeError",1))return false;
            if(!error(()=>sheet.insertRule(".wrong{}",0),"HierarchyRequestError",3))return false;
            sheet.insertRule(".body{}",2);
            if(!error(()=>sheet.insertRule("@namespace m url(urn:m);",2),"InvalidStateError",11))return false;
            const namespace=sheet.cssRules[1], length=sheet.cssRules.length;
            if(!error(()=>sheet.deleteRule(1),"InvalidStateError",11)||sheet.cssRules[1]!==namespace||sheet.cssRules.length!==length)return false;
            sheet.removeRule(2);sheet.deleteRule(1);
            if(namespace.parentStyleSheet!==null||namespace.parentRule!==null)return false;
            const constructed=new CSSStyleSheet();
            if(!error(()=>constructed.insertRule("@import url(a.css);",99),"SyntaxError",12))return false;
            sheet.insertRule("@media all{}",1);
            const group=sheet.cssRules[1];
            return error(()=>group.insertRule("@namespace ???;",0),"SyntaxError",12)
                &&error(()=>group.insertRule("@namespace url(urn:n);",0),"HierarchyRequestError",3)
                &&error(()=>group.insertRule("???",2),"IndexSizeError",1);
        })()"#).unwrap().ok().unwrap();
        assert!(matches!(value,Value::Bool(true)),"CSSOM typed header errors and mutation phases");
    }

    #[test]
    fn stylesheet_statement_rules_can_precede_blocks_and_be_removed() {
        let mut sheet = CssStyleSheetText::parse("").unwrap();
        sheet
            .insert_rule(
                "@import url('data:text/css,div { background: red !important }');",
                0,
            )
            .unwrap();
        assert_eq!(sheet.css_rules().unwrap().len(), 1);
        assert_eq!(
            css::imports(sheet.css_text()).unwrap()[0].url.as_ref(),
            "data:text/css,div { background: red !important }"
        );
        sheet.insert_rule("div { background: green }", 1).unwrap();
        let rules = sheet.css_rules().unwrap();
        assert_eq!(rules.len(), 2);
        assert!(rules[0].selector_text.is_none());
        assert_eq!(rules[1].selector_text.as_deref(), Some("div"));
        sheet.delete_rule(0).unwrap();
        assert_eq!(sheet.css_rules().unwrap().len(), 1);
        assert!(!sheet.css_text().contains("@import"));
        sheet.insert_rule("@layer base, override;", 0).unwrap();
        assert_eq!(sheet.css_rules().unwrap().len(), 2);
        sheet.delete_rule(0).unwrap();
        assert_eq!(
            sheet.css_rules().unwrap()[0].selector_text.as_deref(),
            Some("div")
        );
    }

    #[test]
    fn specification_starting_style_cssom_is_live_grouping_and_preserves_nested_global_rules() {
        let mut engine=lumen::Engine::new();
        crate::install(engine.ctx(),"<style id=source>@starting-style{.target{opacity:0}}.target{@starting-style{opacity:.2}}</style><div class=target></div>",64).unwrap();
        let result=engine.eval_value(r#"
            const sheet=document.getElementById('source').sheet;
            const group=sheet.cssRules[0];
            if(!(group instanceof CSSStartingStyleRule) || !(group instanceof CSSGroupingRule) || group.type!==0 || group.cssRules.length!==1)
                throw Error('starting-style grouping identity: '+[group instanceof CSSStartingStyleRule,group instanceof CSSGroupingRule,group.type,group.cssRules.length].join(','));
            group.insertRule('.target {width:0px}',1);
            if(group.cssRules.length!==2 || group.cssRules[1].style.width!=='0px') throw Error('live starting-style insertion');
            const detached=group.cssRules[1];group.deleteRule(1);
            detached.style.width='9px';
            if(detached.style.width!=='9px' || group.cssRules.length!==1) throw Error('detached grouped rule ownership');
            const nested=sheet.cssRules[1].cssRules[0];
            if(!(nested instanceof CSSStartingStyleRule)) throw Error('nested starting-style identity');
            nested.insertRule('@keyframes globalName {from{opacity:0}to{opacity:1}}',0);
            if(!(nested.cssRules[0] instanceof CSSKeyframesRule)) throw Error('global naming rule within nested starting-style');
            true
        "#).unwrap();
        if let Err(error)=&result { let message=engine.ctx().coerce_string(error).map(|value|value.to_string()).unwrap_or_default(); panic!("starting-style CSSOM: {message}"); }
        assert!(matches!(result,Ok(Value::Bool(true))),"starting-style CSSOM behavior");
    }

    #[test]
    fn specification_keyframe_cssom_applies_shared_declaration_policy_to_live_and_detached_rules() {
        let mut engine=lumen::Engine::new();
        crate::install(engine.ctx(),"<style id=source>@keyframes policy{from{width:10px;width:999px!important;animation-timing-function:steps(2,start);animation-duration:7s;animation-name:invalid}to{width:100px}}</style>",64).unwrap();
        let script=r#"
            const keys=document.getElementById('source').sheet.cssRules[0];
            const frame=keys.findRule('from');
            if(frame.style.length!==2 || frame.style.width!=='10px' ||
                frame.style.animationTimingFunction!=='steps(2, start)' ||
                frame.style.animationDuration!=='' || frame.style.animationName!=='')
                throw Error('initial keyframe declaration policy');
            frame.style.setProperty('width','50px','important');
            if(frame.style.width!=='10px' || frame.style.getPropertyPriority('width')!=='')
                throw Error('invalid important setter must preserve an existing valid declaration');
            frame.style.cssText='width:30px;width:99px!important;animation-timing-function:linear;animation-play-state:paused';
            if(frame.style.length!==2 || frame.style.width!=='30px' || frame.style.animationPlayState!=='')
                throw Error('keyframe cssText replacement policy');
            keys.appendRule('50% {width:40px;opacity:.5!important;animation-timing-function:steps(3,end)}');
            const appended=keys.findRule('50%');
            if(appended.style.length!==2 || appended.style.opacity!=='' || appended.style.width!=='40px')
                throw Error('appended rule policy');
            keys.deleteRule('50%');
            appended.style.cssText='width:60px;animation-duration:4s;opacity:.2!important';
            if(appended.style.length!==1 || appended.style.width!=='60px' || appended.style.animationDuration!=='')
                throw Error('detached rule policy');
            true
        "#;
        let result=engine.eval_value(script).unwrap();
        if let Err(error)=&result {let message=engine.ctx().coerce_string(error).map(|value|value.to_string()).unwrap_or_default();panic!("keyframe CSSOM: {message}");}
        assert!(matches!(result,Ok(Value::Bool(true))),"keyframe CSSOM mutations use the canonical specification parser");
    }

    #[test]
    fn keyframes_and_import_cssom_edit_the_live_stylesheet_source() {
        let mut engine = lumen::Engine::new();
        let realm = crate::install(
            engine.ctx(),
            "<style>@import url('theme.css') layer(theme) supports(display: grid) screen; @keyframes pulse { from { opacity: 0 } 50% { opacity: .5 } to { opacity: 1 } }</style>",
            64,
        )
        .unwrap();
        let script = r#"
            const author=document.querySelector('style'), authorText=author.textContent, authorNode=author.firstChild;
            const sheet=author.sheet;
            const imp=sheet.cssRules[0], keys=sheet.cssRules[1];
            if(!(imp instanceof CSSImportRule) || imp.type!==3 || imp.href!=='theme.css'
                || imp.layerName!=='theme' || imp.supportsText!=='display: grid'
                || imp.media.mediaText!=='screen' || imp.media.item(0)!=='screen')
                throw new Error('import rule reflection');
            imp.media.mediaText='screen, print';
            if(imp.media.length!==2 || imp.media[1]!=='print') throw new Error('media list mutation');
            if(!(keys instanceof CSSKeyframesRule)) throw new Error('keyframes rule instanceof');
            if(keys.type!==7) throw new Error('keyframes rule type');
            if(keys.name!=='pulse') throw new Error('keyframes rule name');
            if(keys.cssRules.length!==3) throw new Error('keyframes rule list length');
            const frame=keys.findRule('50%');
            if(!(frame instanceof CSSKeyframeRule)) throw new Error('keyframe instanceof');
            if(frame.type!==8) throw new Error('keyframe type');
            if(frame.keyText!=='50%') throw new Error('keyframe keyText');
            if(frame.style.opacity!=='0.5') throw new Error('keyframe style opacity');
            if(frame.parentRule!==keys) throw new Error('keyframe parentRule');
            if(frame.parentStyleSheet!==sheet) throw new Error('keyframe parentStyleSheet');
            frame.keyText='40%, 60%';
            frame.style.opacity='0.4';
            keys.appendRule('75% { opacity: .75 }');
            if(keys.cssRules.length!==4 || keys.findRule('40%, 60%').style.opacity!=='0.4'
                || keys.findRule('60%')!==null
                || keys.findRule('75%').style.opacity!=='0.75') throw new Error('keyframe mutation');
            keys.deleteRule('from');
            if(keys.cssRules.length!==3 || keys.findRule('from')!==null) throw new Error('keyframe removal');
            const text=imp.cssText+' '+keys.cssText;
            if(!text.includes('screen, print') || !text.includes('40%, 60%')
                || !text.includes('75%')) throw new Error('live source mutation');
            if(author.textContent!==authorText || author.firstChild!==authorNode)
                throw new Error('CSSOM must preserve author source');
        "#;
        if let Err(error) = engine.eval_value(script).unwrap() {
            let message = engine
                .ctx()
                .coerce_string(&error)
                .map(|value| value.to_string())
                .unwrap_or_else(|_| "unprintable exception".into());
            panic!("keyframes/import CSSOM script failed: {message}");
        }
        let snapshot = realm.session.borrow_mut().animation_snapshot().unwrap();
        let keyframes = snapshot.keyframes.iter().find(|rule| rule.name == "pulse").unwrap();
        assert_eq!(keyframes.rules.len(), 3);
        let index = css::StyleIndex::new(Vec::new());
        for (key, opacity) in [("40%, 60%", 0.4), ("75%", 0.75)] {
            let frame = keyframes.rules.iter().find(|frame| frame.key_text == key)
                .unwrap_or_else(|| panic!("renderer lost keyframe {key}: {:?}", keyframes.rules));
            // Renderer keyframes retain valid authored numeric spelling (`.75`).
            // Read the typed shared declaration value rather than its bytes.
            let kind = NodeKind::Element { name: "div".into(), namespace: lumen_html::Namespace::Html,
                attributes: vec![("style".into(), frame.style.clone())] };
            assert_eq!(css::compute(&kind, None, &index).unwrap().opacity,
                opacity, "renderer keyframe {key}: {}", frame.style);
        }
        assert!(!keyframes.rules.iter().any(|frame| frame.key_text == "0%"));
    }

    #[test]
    fn stylesheet_declaration_tails_are_accepted_only_inside_style_blocks() {
        let sheet = CssStyleSheetText::parse(".a { color: red; width: calc(50% - 2px) }").unwrap();
        let rules = sheet.css_rules().unwrap();
        assert_eq!(
            rules[0].style.get_property_value("width").unwrap(),
            Some(("calc(50% - 2px)".into(), false))
        );
        let sheet =
            CssStyleSheetText::parse(".a { color: red; & .b { display: grid } width: 12px }")
                .unwrap();
        let rules = sheet.css_rules().unwrap();
        assert_eq!(rules[0].nested.len(), 2);
        assert!(rules[0].nested[1].nested_declarations);
        assert_eq!(
            rules[0].nested[1]
                .style
                .get_property_value("width")
                .unwrap(),
            Some(("12px".into(), false))
        );
        assert!(css::nesting::source_rule_ranges("color: red", false).unwrap().is_empty());
        assert!(css::nesting::parse_one_source_rule("color: red", &[]).is_err());
        let text = ".recovered { color: red";
        let source = css::nesting::parse_source_rules(text, &[], false).unwrap();
        let recovered = source_rules_to_cssom(text, &source, false).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].selector_text.as_deref(), Some(".recovered"));
        assert_eq!(
            recovered[0].style.get_property_value("color").unwrap(),
            Some(("red".into(), false))
        );
        assert!(css::nesting::parse_one_source_rule(".broken { color: red } trailing", &[]).is_err());
    }

    #[test]
    fn stylesheet_rule_edits_validate_before_replacement() {
        let mut sheet =
            CssStyleSheetText::parse(".a { color: red } @media screen { .b { width: 2px } }")
                .unwrap();
        let rules = sheet.css_rules().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].selector_text.as_deref(), Some(".a"));
        assert_eq!(
            rules[0].style.get_property_value("color").unwrap(),
            Some(("red".into(), false))
        );
        assert_eq!(rules[1].nested.len(), 1);
        let before = sheet.css_text().to_owned();
        assert!(sheet.insert_rule(".broken { width: nope } trailing", 1).is_err());
        assert_eq!(sheet.css_text(), before);
        let mut recovered = sheet.clone();
        recovered.insert_rule(".recovered { width: nope", 1).unwrap();
        let recovered_rules = recovered.css_rules().unwrap();
        assert_eq!(recovered_rules[1].selector_text.as_deref(), Some(".recovered"));
        assert!(recovered_rules[1].style.get_property_value("width").unwrap().is_none());
        sheet.insert_rule(".c { display: grid }", 1).unwrap();
        assert_eq!(
            sheet.css_rules().unwrap()[1].selector_text.as_deref(),
            Some(".c")
        );
        sheet.delete_rule(1).unwrap();
        assert_eq!(sheet.css_rules().unwrap().len(), 2);

        let supports =
            CssStyleSheetText::parse("@supports (display: grid) { .supported { color: red } }")
                .unwrap();
        let rules = supports.css_rules().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].nested.len(), 1);
        assert_eq!(
            rules[0].nested[0].selector_text.as_deref(),
            Some(".supported")
        );
    }

    #[test]
    fn inserted_rule_css_text_serializes_declaration_spacing_and_omitted_index() {
        let mut engine = lumen::Engine::new();
        crate::install(engine.ctx(), "<style>nosuchelement { color: red; }</style>", 64).unwrap();
        assert!(matches!(engine.eval_value(r#"(() => {
            const sheet = document.querySelector('style').sheet;
            sheet.insertRule('p { color: green; }');
            if (sheet.cssRules[0].cssText !== 'p { color: green; }') return false;
            sheet.insertRule('p { color: yellow; }', undefined);
            if (sheet.cssRules[0].cssText !== 'p { color: yellow; }') return false;
            sheet.insertRule('p { --quoted: "a:b;c"; color: blue !important; }');
            const rule = sheet.cssRules[0];
            return rule.cssText === 'p { --quoted: "a:b;c"; color: blue !important; }'
                && rule.style.getPropertyValue('--quoted') === '"a:b;c"'
                && rule.style.getPropertyPriority('color') === 'important';
        })()"#), Ok(Ok(Value::Bool(true)))));
    }

    #[test]
    fn grouping_rule_mutations_keep_live_lists_parentage_and_order() {
        let mut engine = lumen::Engine::new();
        crate::install(engine.ctx(), "<style></style>", 64).unwrap();
        let result = engine
            .eval_value(
                r#"
                (() => {
                    const sheet = document.styleSheets[0];
                    sheet.insertRule('.parent { color: red; }', 0);
                    sheet.insertRule('@media screen { .inside { color: blue; } }', 1);
                    const parent = sheet.cssRules[0];
                    const parentList = parent.cssRules;
                    const media = sheet.cssRules[1];
                    const mediaList = media.cssRules;
                    const original = mediaList[0];
                    media.insertRule('.before { color: green; }', 0);
                    const orderAndIdentity = media.cssRules === mediaList &&
                        mediaList.length === 2 && mediaList[1] === original &&
                        mediaList[0].parentRule === media &&
                        mediaList[0].parentStyleSheet === sheet &&
                        original.parentRule === media &&
                        original.parentStyleSheet === sheet &&
                        Array.from(mediaList).length === 2 &&
                        mediaList[2] === undefined && mediaList.item(2) === null;
                    parent.insertRule('& .child { color: purple; }', 0);
                    const nestedIdentity = parent.cssRules === parentList &&
                        parentList.length === 1 &&
                        parentList[0].selectorText === '& .child' &&
                        parentList[0].parentRule === parent &&
                        parentList[0].parentStyleSheet === sheet;
                    const condition = media instanceof CSSMediaRule &&
                        media instanceof CSSConditionRule &&
                        media instanceof CSSGroupingRule &&
                        media.conditionText === 'screen' &&
                        CSSStyleRule.__proto__ === CSSGroupingRule &&
                        CSSMediaRule.__proto__ === CSSConditionRule &&
                        CSSSupportsRule.__proto__ === CSSConditionRule;
                    return [
                        !orderAndIdentity && 'live-list-order-parentage',
                        !nestedIdentity && 'nested-style-rule',
                        !condition && 'condition-rule-hierarchy'
                    ].filter(Boolean).join('|');
                })()
                "#,
            )
            .unwrap()
            .unwrap_or_else(|error| {
                let message = engine
                    .ctx()
                    .coerce_string(&error)
                    .map(|value| value.to_string())
                    .unwrap_or_else(|_| "unprintable exception".into());
                panic!("grouping-rule mutation script threw: {message}");
            });
        let Value::Str(failures) = result else {
            panic!("grouping-rule mutation diagnostics must be a string");
        };
        assert!(failures.is_empty(), "failed checks: {failures}");
    }

    #[test]
    fn grouping_rule_mutations_reject_bad_index_syntax_and_hierarchy() {
        let mut runtime = lumen_runtime::Runtime::new();
        let engine = runtime.engine();
        crate::install(engine.ctx(), "<style></style>", 64).unwrap();
        let result = engine
            .eval_value(
                r#"
                (() => {
                    const sheet = document.styleSheets[0];
                    sheet.insertRule('@media screen { .inside { color: blue; } }', 0);
                    const media = sheet.cssRules[0];
                    const before = media.cssRules[0].cssText;
                    let indexFirst = false, syntax = false, hierarchyImport = false;
                    let hierarchyNamespace = false, deleteIndex = false;
                    try { media.insertRule('???', 2); }
                    catch (error) { indexFirst = error.name === 'IndexSizeError' && error.code === 1; }
                    try { media.insertRule('???', 0); }
                    catch (error) { syntax = error.name === 'SyntaxError' && error.code === 12; }
                    try { media.insertRule('@import url("x.css");', 0); }
                    catch (error) { hierarchyImport = error.name === 'HierarchyRequestError' && error.code === 3; }
                    try { media.insertRule('@namespace url("urn:x");', 0); }
                    catch (error) { hierarchyNamespace = error.name === 'HierarchyRequestError' && error.code === 3; }
                    try { media.deleteRule(1); }
                    catch (error) { deleteIndex = error.name === 'IndexSizeError' && error.code === 1; }
                    let negativeIndex = false;
                    try { media.insertRule('???', -1); }
                    catch (error) { negativeIndex = error.name === 'IndexSizeError' && error.code === 1; }
                    return [
                        !indexFirst && 'index-precedes-parse',
                        !syntax && 'syntax-dom-exception',
                        !hierarchyImport && 'import-hierarchy',
                        !hierarchyNamespace && 'namespace-hierarchy',
                        !deleteIndex && 'delete-index',
                        !negativeIndex && 'unsigned-long-negative-index',
                        media.cssRules.length !== 1 && 'failed-insert-preserved-length',
                        media.cssRules[0].cssText !== before && 'failed-insert-preserved-rule'
                    ].filter(Boolean).join('|');
                })()
                "#,
            )
            .unwrap()
            .unwrap_or_else(|error| {
                let message = engine
                    .ctx()
                    .coerce_string(&error)
                    .map(|value| value.to_string())
                    .unwrap_or_else(|_| "unprintable exception".into());
                panic!("grouping-rule error script threw: {message}");
            });
        let Value::Str(failures) = result else {
            panic!("grouping-rule error diagnostics must be a string");
        };
        assert!(failures.is_empty(), "failed checks: {failures}");
    }

    #[test]
    fn media_query_list_reuses_cascade_media_matching() {
        let environment = MediaEnvironment {color_schemes:Default::default(),
            width: 800.0,
            height: 600.0,
            resolution: 2.0,
            print: false,
        };
        assert!(MediaQueryList::new("screen and (min-width: 700px)", environment).matches());
        assert!(!MediaQueryList::new("print, (max-width: 500px)", environment).matches());
    }

    #[test]
    fn public_css_supports_and_media_query_events_are_live_and_reentrant() {
        use lumen::Engine;

        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<main></main>", 64).unwrap();
        realm
            .session
            .borrow_mut()
            .set_media_environment(MediaEnvironment {color_schemes:Default::default(),
                width: 600.0,
                height: 400.0,
                resolution: 1.0,
                print: false,
            })
            .unwrap();
        let supports = engine
            .eval_value("CSS.supports('display', 'grid') && !CSS.supports('display', 'spaceship')")
            .unwrap()
            .ok()
            .unwrap();
        assert!(
            matches!(supports, Value::Bool(true)),
            "CSS.supports rejected supported declarations"
        );
        for (name, source) in [
            ("conditionText declaration", "CSS.supports('color: red')"),
            (
                "conditionText logical condition",
                "CSS.supports('(display: grid) and (width: 1px)')",
            ),
            (
                "selector() valid selector",
                "CSS.supports('selector(div > .item)')",
            ),
            (
                "selector() rejects selector lists",
                "!CSS.supports('selector(div, .item)')",
            ),
            (
                "two-argument value string coercion",
                "CSS.supports('display', { toString() { return 'grid'; } })",
            ),
            (
                "required first argument explicit undefined coercion",
                "!CSS.supports(undefined)",
            ),
            (
                "explicit null is stringified",
                "CSS.supports('--explicit', null)",
            ),
            (
                "explicit undefined is stringified",
                "CSS.supports('--explicit', undefined)",
            ),
            (
                "explicit empty custom-property value",
                "CSS.supports('--explicit', '')",
            ),
            (
                "omitted second argument selects conditionText",
                "!CSS.supports('--omitted')",
            ),
        ] {
            let result = engine.eval_value(source).unwrap().ok();
            let passed = matches!(result, Some(Value::Bool(true)));
            assert!(passed, "CSS.supports {name} failed");
        }
        let coercion_throws = engine
            .eval_value(
                "let threw = false; \
                 try { CSS.supports('display', { toString() { throw new Error('coerce'); } }); } \
                 catch (error) { threw = error.message === 'coerce'; } \
                 threw",
            )
            .unwrap()
            .ok()
            .unwrap();
        assert!(
            matches!(coercion_throws, Value::Bool(true)),
            "CSS.supports swallowed a throwing string conversion"
        );
        let missing_required_throws = engine
            .eval_value(
                "let missingSupportsTypeError = false; \
                 try { CSS.supports(); } \
                 catch (error) { missingSupportsTypeError = error instanceof TypeError; } \
                 missingSupportsTypeError",
            )
            .unwrap()
            .ok()
            .unwrap();
        assert!(
            matches!(missing_required_throws, Value::Bool(true)),
            "CSS.supports did not reject its omitted required first argument"
        );
        let dom_stringified_undefined = engine
            .eval_value("document.getElementById(undefined) === null")
            .unwrap()
            .ok()
            .unwrap();
        assert!(
            matches!(dom_stringified_undefined, Value::Bool(true)),
            "Document.getElementById did not coerce an explicit undefined DOMString"
        );
        let optional_default = engine
            .eval_value(
                "const omittedTitleDocument = document.implementation.createHTMLDocument(); \
                 const explicitUndefinedTitleDocument = document.implementation.createHTMLDocument(undefined); \
                 omittedTitleDocument.getElementsByTagName('title').length === 0 && \
                 explicitUndefinedTitleDocument.getElementsByTagName('title').length === 0",
            )
            .unwrap()
            .ok()
            .unwrap();
        assert!(
            matches!(optional_default, Value::Bool(true)),
            "explicit undefined changed an optional/defaulted native argument"
        );
        let initial_match = engine.eval_value(
            "window.__changes = 0; window.__m = matchMedia('(min-width: 700px)'); !window.__m.matches"
        ).unwrap().ok().unwrap();
        assert!(
            matches!(initial_match, Value::Bool(true)),
            "initial matchMedia did not start false"
        );
        engine.eval_value(
            "window.__m.addEventListener('change', e => { window.__changes++; matchMedia('print'); })"
        ).unwrap().ok().unwrap();

        realm
            .session
            .borrow_mut()
            .set_media_environment(MediaEnvironment {color_schemes:Default::default(),
                width: 800.0,
                height: 400.0,
                resolution: 1.0,
                print: false,
            })
            .unwrap();
        crate::notify_media(engine.ctx(), &realm).unwrap();
        let result = engine
            .eval_value("window.__m.matches && window.__changes === 1")
            .unwrap()
            .ok()
            .unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn css_typed_numeric_units_factories_and_property_reification() {
        let mut engine = lumen::Engine::new();
        crate::install(engine.ctx(), "<main><div id='typed'></div></main>", 64).unwrap();
        let result = engine
            .eval_value(
                r#"
                (() => {
                    const failures = [];
                    const check = (name, condition) => { if (!condition) failures.push(name); };
                    check('constructors', typeof CSSNumericValue === 'function' &&
                        typeof CSSUnitValue === 'function');

                    const parsed = CSSNumericValue.parse('1.5PX');
                    const percent = CSS.percent(25);
                    const root = CSS.rcap(2);
                    const q = CSS.Q(3);
                    const frequency = CSS.kHz(4);
                    const units = [
                        'number', 'percent', 'cap', 'ch', 'em', 'ex', 'ic', 'lh', 'rcap', 'rch',
                        'rem', 'rex', 'ric', 'rlh', 'vw', 'vh', 'vi', 'vb', 'vmin', 'vmax',
                        'svw', 'svh', 'svi', 'svb', 'svmin', 'svmax', 'lvw', 'lvh', 'lvi',
                        'lvb', 'lvmin', 'lvmax', 'dvw', 'dvh', 'dvi', 'dvb', 'dvmin', 'dvmax',
                        'cqw', 'cqh', 'cqi', 'cqb', 'cqmin', 'cqmax', 'cm', 'mm', 'Q', 'in',
                        'pt', 'pc', 'px', 'deg', 'grad', 'rad', 'turn', 's', 'ms', 'Hz', 'kHz',
                        'dpi', 'dpcm', 'dppx', 'fr'
                    ];
                    const allFactories = units.every(unit => {
                        const factory = CSS[unit];
                        const direct = new CSSUnitValue(1, unit);
                        return typeof factory === 'function' && factory(1) instanceof CSSUnitValue &&
                            factory(1).unit === unit.toLowerCase() &&
                            direct.unit === unit.toLowerCase();
                    });
                    check('parsed primitive brand and canonical unit',
                        parsed instanceof CSSNumericValue && parsed instanceof CSSUnitValue &&
                        parsed.value === 1.5 && parsed.unit === 'px' && parsed.toString() === '1.5px');
                    const parsedProduct = CSSNumericValue.parse(
                        'calc(2 * min(10px, 20%))');
                    check('calc product reifies as its math operator',
                        parsedProduct instanceof CSSMathProduct &&
                        parsedProduct.values.length === 2 &&
                        parsedProduct.toString() === 'calc(2 * min(10px, 20%))');
                    check('unit factories', percent instanceof CSSUnitValue &&
                        percent.value === 25 && percent.unit === 'percent' && percent.toString() === '25%' &&
                        root.unit === 'rcap' && q.unit === 'q' && frequency.unit === 'khz' &&
                        allFactories);
                    parsed.value = 2;
                    const lengthType = parsed.type();
                    const numberType = CSS.number(4).type();
                    const percentType = percent.type();
                    check('numeric types and mutable value', parsed.toString() === '2px' &&
                        lengthType.length === 1 && lengthType.angle === undefined &&
                        lengthType.percentHint === undefined && numberType.length === undefined &&
                        numberType.percent === undefined && numberType.percentHint === undefined &&
                        percentType.percent === 1 && percentType.length === undefined);
                    let rejectsNonFiniteConstructor = false;
                    let rejectsNonFiniteFactory = false;
                    let rejectsNonFiniteSetter = false;
                    try { new CSSUnitValue(Infinity, 'px'); }
                    catch (error) { rejectsNonFiniteConstructor = error instanceof TypeError; }
                    try { CSS.px(NaN); }
                    catch (error) { rejectsNonFiniteFactory = error instanceof TypeError; }
                    try { parsed.value = -Infinity; }
                    catch (error) { rejectsNonFiniteSetter = error instanceof TypeError; }
                    check('restricted double inputs stay finite', rejectsNonFiniteConstructor &&
                        rejectsNonFiniteFactory && rejectsNonFiniteSetter && parsed.value === 2);
                    const typedElement = document.getElementById('typed');
                    const mutableLength = CSS.px(1);
                    typedElement.attributeStyleMap.set('width', mutableLength);
                    mutableLength.value = 7;
                    typedElement.attributeStyleMap.set('width', mutableLength);
                    check('property map serializes live numeric state',
                        typedElement.style.getPropertyValue('width') === '7px');

                    const attempt = (name, callback) => {
                        try { return callback(); }
                        catch (error) {
                            failures.push(name + ': ' + error.name + ': ' + error.message);
                            return null;
                        }
                    };
                    const zeroWidth = attempt('parse width zero',
                        () => CSSStyleValue.parse('width', '0'));
                    const zeroLineHeight = attempt('parse line-height zero',
                        () => CSSStyleValue.parse('line-height', '0'));
                    const durations = attempt('parse animation duration list',
                        () => CSSStyleValue.parseAll('animation-duration', '1s, 250ms, 0s'));
                    const transitionDurations = attempt('parse transition duration list',
                        () => CSSStyleValue.parseAll('transition-duration', '1s, 250ms, 0s'));
                    const transitionDelays = attempt('parse transition delay list',
                        () => CSSStyleValue.parseAll('transition-delay', '-25ms, 0s'));
                    let negativeTransitionDurationRejected = false;
                    try { CSSStyleValue.parseAll('transition-duration', '-1ms'); }
                    catch (error) { negativeTransitionDurationRejected = error instanceof TypeError; }
                    const margin = attempt('parse margin shorthand',
                        () => CSSStyleValue.parse('margin', '1px 2px'));
                    typedElement.style.setProperty('transition-duration', '250ms');
                    const computedMap = typedElement.computedStyleMap();
                    const computedDurationValue = computedMap.get('transition-duration');
                    check('computed map is readonly and same-object',
                        computedMap === typedElement.computedStyleMap() &&
                        typeof computedMap.set !== 'function');
                    typedElement.style.setProperty('transition-duration', '0.5s, 250ms');
                    typedElement.style.setProperty('transition-delay', '-25ms, 0s');
                    const updatedDurationValue = computedMap.get('transition-duration');
                    const computedTransition = getComputedStyle(typedElement);
                    const specifiedTransitionDuration =
                        typedElement.style.getPropertyValue('transition-duration');
                    const specifiedTransitionDelay =
                        typedElement.style.getPropertyValue('transition-delay');
                    check('property-aware numeric reification', zeroWidth instanceof CSSUnitValue &&
                        zeroWidth.value === 0 && zeroWidth.unit === 'px' &&
                        zeroLineHeight instanceof CSSUnitValue && zeroLineHeight.unit === 'number' &&
                        durations && durations.length === 3 && durations[0].unit === 's' &&
                        durations[1].value === 250 && durations[1].unit === 'ms' &&
                        durations[2].toString() === '0s' &&
                        margin instanceof CSSStyleValue && !(margin instanceof CSSNumericValue) &&
                        margin.toString() === '1px 2px');
                    check('transition Typed OM lists and computed serialization',
                        transitionDurations && transitionDurations.length === 3 &&
                        transitionDurations[0].toString() === '1s' &&
                        transitionDurations[1].toString() === '250ms' &&
                        transitionDurations[2].toString() === '0s' &&
                        transitionDelays && transitionDelays.length === 2 &&
                        transitionDelays[0].toString() === '-25ms' &&
                        transitionDelays[1].toString() === '0s' &&
                        negativeTransitionDurationRejected &&
                        computedDurationValue instanceof CSSUnitValue &&
                        computedDurationValue.unit === 's' && computedDurationValue.value === 0.25 &&
                        updatedDurationValue instanceof CSSUnitValue &&
                        updatedDurationValue.unit === 's' && updatedDurationValue.value === 0.5 &&
                        typedElement.computedStyleMap() === computedMap &&
                        specifiedTransitionDuration === '0.5s, 250ms' &&
                        specifiedTransitionDelay === '-25ms, 0s' &&
                        computedTransition.getPropertyValue('transition-duration') === '0.5s, 0.25s' &&
                        computedTransition.getPropertyValue('transition-delay') === '-0.025s, 0s');
                    typedElement.style.setProperty('transition-duration', 'calc(2 * 3s)');
                    const calculatedDuration = computedMap.get('transition-duration');
                    check('transition numeric expression folds without changing primitive units',
                        calculatedDuration instanceof CSSUnitValue &&
                        calculatedDuration.unit === 's' && calculatedDuration.value === 6 &&
                        getComputedStyle(typedElement).getPropertyValue('transition-duration') === '6s');
                    typedElement.style.setProperty('transition-duration', '0.5s, 250ms');
                    CSS.registerProperty({name:'--readonly-adoption',syntax:'<number>',initialValue:'5',inherits:false});
                    typedElement.style.setProperty('--readonly-adoption','9');
                    const adoptedDocument = new DOMParser().parseFromString('<main></main>', 'text/html');
                    adoptedDocument.querySelector('main').appendChild(adoptedDocument.adoptNode(typedElement));
                    const adoptedDurationValue = computedMap.get('transition-duration');
                    check('computed map follows inactive adopted owner availability',
                        adoptedDurationValue === undefined && computedMap.size === 0 &&
                        Array.from(computedMap).length === 0 &&
                        adoptedDocument.querySelector('main').firstChild === typedElement);
                    document.body.appendChild(typedElement);
                    const reconnectedDurationValue = computedMap.get('transition-duration');
                    check('computed map follows reconnected adopted owner',
                        reconnectedDurationValue instanceof CSSUnitValue &&
                        reconnectedDurationValue.unit === 's' && reconnectedDurationValue.value === 0.5 &&
                        typedElement.computedStyleMap() === computedMap &&
                        computedMap.get('--readonly-adoption').unit === 'number' &&
                        computedMap.get('--readonly-adoption').value === 9);

                    const custom = attempt('parse custom property with variable',
                        () => CSSStyleValue.parse('--tokens', 'calc(1px + var(--gap, 2px))'));
                    let constructorCaseSensitive = false;
                    let invalidUnitRejected = false;
                    let invalidPropertyRejected = false;
                    try { new CSSUnitValue(1, 'PX'); }
                    catch (error) { constructorCaseSensitive = error instanceof TypeError; }
                    try { new CSSUnitValue(1, 'unknown'); }
                    catch (error) { invalidUnitRejected = error instanceof TypeError; }
                    try { CSSStyleValue.parse('not-a-css-property', '1px'); }
                    catch (error) { invalidPropertyRejected = error instanceof TypeError; }
                    check('custom properties and type errors', custom instanceof CSSUnparsedValue &&
                        custom.toString() === 'calc(1px + var(--gap, 2px))' &&
                        constructorCaseSensitive && invalidUnitRejected && invalidPropertyRejected);
                    return failures.join('|');
                })()
                "#,
            )
            .unwrap()
            .unwrap_or_else(|error| {
                let message = engine
                    .ctx()
                    .coerce_string(&error)
                    .map(|text| text.to_string())
                    .unwrap_or_else(|_| "<unprintable exception>".into());
                panic!("Typed OM numeric contract threw: {message}");
            });
        let Value::Str(failures) = result else {
            panic!("Typed OM numeric diagnostics must be a string");
        };
        assert!(failures.is_empty(), "failed checks: {failures}");
    }

    #[test]
    fn css_math_values_keep_children_live_and_reify_bounded_trees() {
        let mut runtime = lumen_runtime::Runtime::new();
        let engine = runtime.engine();
        crate::install(engine.ctx(), "<main></main>", 64).unwrap();
        let result = engine
            .eval_value(
                r#"(() => {
                    const failures = [];
                    const check = (name, condition) => {
                        if (!condition) failures.push(name);
                    };
                    const expression = CSSNumericValue.parse('calc(1px + 2px * 3)');
                    const expressionValues = expression.values;
                    const product = expressionValues[1];
                    check('parse reifies operator classes and children',
                        expression instanceof CSSMathSum && expression.operator === 'sum' &&
                        expressionValues instanceof CSSNumericArray &&
                        expressionValues === expression.values && expressionValues.length === 2 &&
                        expressionValues[0] instanceof CSSUnitValue &&
                        product instanceof CSSMathProduct && product.operator === 'product' &&
                        product.values.length === 2 && product.values[1].value === 3 &&
                        expression.toString() === 'calc(1px + (2px * 3))');
                    const iterated = [...expressionValues];
                    const entries = [...expressionValues.entries()];
                    check('numeric arrays iterate with indexed entry identity',
                        iterated.length === 2 && iterated[0] === expressionValues[0] &&
                        entries.length === 2 && entries[0][0] === 0 &&
                        entries[0][1] === expressionValues[0]);
                    const type = expression.type();
                    check('numeric type is a dictionary with present dimensions',
                        Object.getPrototypeOf(type) === Object.prototype &&
                        type.length === 1 && type.angle === undefined &&
                        Object.keys(type).join(',') === 'length');

                    const innerValue = CSS.px(1);
                    const inner = new CSSMathSum(innerValue, CSS.px(2));
                    const nested = new CSSMathProduct(inner, CSS.number(2));
                    innerValue.value = 3;
                    check('nested child mutation changes parent serialization',
                        nested.values[0] === inner &&
                        nested.toString() === 'calc((3px + 2px) * 2)' &&
                        inner.toString() === 'calc(3px + 2px)');
                    const target = document.querySelector('main');
                    target.attributeStyleMap.set('width', inner);
                    check('style map uses live child values',
                        target.style.getPropertyValue('width') === 'calc(3px + 2px)');

                    const clamp = new CSSMathClamp(CSS.px(0), CSS.px(5), CSS.px(10));
                    const negated = new CSSMathNegate(clamp.value);
                    const inverse = new CSSMathInvert(CSS.number(0));
                    const minimum = new CSSMathMin(CSS.px(1), CSS.px(2));
                    const maximum = new CSSMathMax(CSS.px(3), CSS.px(4));
                    check('fixed and unary math accessors retain child objects',
                        clamp.operator === 'clamp' && clamp.lower.toString() === '0px' &&
                        clamp.value.toString() === '5px' && clamp.upper.toString() === '10px' &&
                        negated.operator === 'negate' && negated.value === clamp.value &&
                        negated.toString() === 'calc(-5px)' &&
                        inverse.operator === 'invert' && inverse.value.unit === 'number' &&
                        inverse.toString() === 'calc(1 / 0)' &&
                        minimum.operator === 'min' && minimum.values.length === 2 &&
                        maximum.operator === 'max' && maximum.values.length === 2);

                    let zeroArgumentsAreSyntaxError = false;
                    let incompatibleDimensionsAreTypeError = false;
                    let missingClampArgumentsAreTypeError = false;
                    try { new CSSMathSum(); }
                    catch (error) {
                        zeroArgumentsAreSyntaxError = error.name === 'SyntaxError' && error.code === 12;
                    }
                    try { new CSSMathSum(CSS.px(1), CSS.deg(1)); }
                    catch (error) { incompatibleDimensionsAreTypeError = error instanceof TypeError; }
                    try { new CSSMathClamp(CSS.px(1), CSS.px(2)); }
                    catch (error) { missingClampArgumentsAreTypeError = error instanceof TypeError; }
                    check('constructors enforce arity and dimensions',
                        zeroArgumentsAreSyntaxError && incompatibleDimensionsAreTypeError &&
                        missingClampArgumentsAreTypeError);
                    return failures.join('|');
                })()"#,
            )
            .unwrap()
            .unwrap_or_else(|error| {
                let message = match engine.ctx().member_get(&error, "message") {
                    Ok(Value::Str(message)) => message.to_string(),
                    _ => "non-string JavaScript exception".into(),
                };
                panic!("CSS math values contract threw: {message}");
            });
        let Value::Str(failures) = result else {
            panic!("CSS math diagnostics must be a string");
        };
        assert!(failures.is_empty(), "failed checks: {failures}");
    }

    #[test]
    fn media_query_registries_and_change_events_are_realm_scoped() {
        use lumen::Engine;

        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let parent_realm = crate::install(ctx, "<main></main>", 64).unwrap();
        let parent_global = ctx.global_object();
        parent_realm
            .session
            .borrow_mut()
            .set_media_environment(MediaEnvironment {color_schemes:Default::default(),
                width: 500.0,
                height: 400.0,
                resolution: 1.0,
                print: false,
            })
            .unwrap();
        assert!(matches!(
            ctx.eval_in_realm(
                &parent_global,
                "window.parentList = matchMedia('(min-width: 700px)'); \
                 window.parentChanges = 0; \
                 parentList.addEventListener('change', () => parentChanges++); \
                 !parentList.matches",
            ),
            Ok(Value::Bool(true))
        ));

        let child_handle = ctx.create_host_realm();
        let child_global = child_handle.global();
        let child_realm = ctx
            .with_host_realm(&child_handle, |ctx| {
                let realm = crate::install(ctx, "<main></main>", 64).unwrap();
                realm
                    .session
                    .borrow_mut()
                    .set_media_environment(MediaEnvironment {color_schemes:Default::default(),
                        width: 400.0,
                        height: 300.0,
                        resolution: 1.0,
                        print: false,
                    })
                    .unwrap();
                assert!(matches!(
                    ctx.eval_in_realm(
                        &child_global,
                        "window.childList = matchMedia('(min-width: 700px)'); \
                         window.childChanges = 0; \
                         childList.addEventListener('change', () => childChanges++); \
                         !childList.matches",
                    ),
                    Ok(Value::Bool(true))
                ));
                realm
            })
            .expect("install the child browsing-context realm");

        // A child viewport update must only dispatch through the child's native
        // MQL and EventTarget. The parent registry remains installed in OpState.
        child_realm
            .session
            .borrow_mut()
            .set_media_environment(MediaEnvironment {color_schemes:Default::default(),
                width: 800.0,
                height: 300.0,
                resolution: 1.0,
                print: false,
            })
            .unwrap();
        ctx.with_host_realm(&child_handle, |ctx| {
            crate::notify_media(ctx, &child_realm).unwrap();
        })
        .expect("dispatch in the child browsing-context realm");
        assert!(matches!(
            ctx.eval_in_realm(&child_global, "childList.matches && childChanges === 1"),
            Ok(Value::Bool(true))
        ));
        assert!(matches!(
            ctx.eval_in_realm(&parent_global, "!parentList.matches && parentChanges === 0"),
            Ok(Value::Bool(true))
        ));

        parent_realm
            .session
            .borrow_mut()
            .set_media_environment(MediaEnvironment {color_schemes:Default::default(),
                width: 900.0,
                height: 400.0,
                resolution: 1.0,
                print: false,
            })
            .unwrap();
        crate::notify_media(ctx, &parent_realm).unwrap();
        assert!(matches!(
            ctx.eval_in_realm(&parent_global, "parentList.matches && parentChanges === 1"),
            Ok(Value::Bool(true))
        ));
        assert!(matches!(
            ctx.eval_in_realm(&child_global, "childList.matches && childChanges === 1"),
            Ok(Value::Bool(true))
        ));
    }

    #[test]
    fn specification_media_query_handler_owners_preserve_callback_identity_and_collect_unretained_cycles() {
        let mut engine=lumen::Engine::new();
        let _realm=crate::install(engine.ctx(),"<body></body>",128).unwrap();
        for callback in ["({list})","function(){return list.media}"] {
            let source=format!("(()=>{{const list=matchMedia('(width:100px)');list.onchange={callback};return list}})()");
            let child_handle=engine.ctx().create_host_realm();
            let (list,retired)=engine.ctx().with_host_realm(&child_handle,|ctx| {
                let realm=crate::install(ctx,"<body></body>",128).unwrap();
                let global=ctx.global_object();let list=ctx.eval_in_realm(&global,&source).ok().expect("actual media query list");
                let retired=realm.retire_browsing_context_group(ctx);
                (list,retired)
            }).ok().expect("media subscription origin realm");
            for retired in retired {engine.ctx().dispose_host_realm(&retired).expect("dispose retired media subscription realm");}
            drop(child_handle);
            let weak_list=engine.ctx().weak_value(&list).expect("MediaQueryList object");
            let callback=engine.ctx().member_get(&list,"onchange").ok().expect("raw handler identity");
            let identity=callback.object_identity();let weak_callback=engine.ctx().weak_value(&callback).expect("object or function handler");
            drop(callback);engine.collect_garbage();
            let retained=engine.ctx().member_get(&list,"onchange").ok().expect("retained raw handler");
            assert_eq!(retained.object_identity(),identity,"a retained list preserves its exact callback object");
            assert!(weak_callback.upgrade().is_some());
            drop(retained);drop(list);engine.collect_garbage();
            assert!(weak_list.upgrade().is_none(),"a retired document subscription cannot retain an unreferenced list/callback cycle");
            assert!(weak_callback.upgrade().is_none(),"the physical callback handle is traced and discounted once");
        }
    }

    #[test]
    fn css_declaration_read_cache_is_shared_bounded_and_coherent_with_source_edits() {
        let mut engine = lumen::Engine::new();
        let _realm = crate::install(engine.ctx(), "<style id=first>div {color:red; margin:1px}</style><style id=second>div {color:blue}</style><div></div>", 128).unwrap();
        let result = engine.eval_value("globalThis.firstStyle=document.getElementById('first').sheet.cssRules[0].style; firstStyle.length===5 && firstStyle[0]==='color' && firstStyle.getPropertyValue('color')==='red'").unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
        let registry = RealmServices::<MediaQueryRegistry>::current(engine.ctx()).unwrap();
        let (tree, declaration) = {
            let cache = registry.read_cache.borrow();
            let cache = cache.as_ref().unwrap();
            (cache.rules.as_ptr(), cache.declaration.as_ref().unwrap().1.clone())
        };
        let result = engine.eval_value("firstStyle.item(1)==='margin-top' && firstStyle.getPropertyPriority('color')==='' && firstStyle.getPropertyValue('color')==='red' && firstStyle.cssText==='color: red; margin: 1px;'").unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
        {
            let cache = registry.read_cache.borrow();
            let cache = cache.as_ref().unwrap();
            assert_eq!(tree, cache.rules.as_ptr());
            assert!(Rc::ptr_eq(&declaration, &cache.declaration.as_ref().unwrap().1));
        }
        let result = engine.eval_value(r#"(() => {
            const second = document.getElementById('second').sheet.cssRules[0].style;
            if (second.getPropertyValue('color') !== 'blue' || firstStyle.getPropertyValue('color') !== 'red') return false;
            document.getElementById('first').textContent = 'div {color:green}';
            const replacement = document.getElementById('first').sheet.cssRules[0].style;
            if (replacement.getPropertyValue('color') !== 'green' || firstStyle.getPropertyValue('color') !== 'red') return false;
            replacement.setProperty('margin', 'var(--edges)');
            replacement.setProperty('margin-left', '7px');
            return replacement.length === 5 && replacement.item(4) === 'margin-left'
                && replacement.getPropertyValue('margin-top') === '' && replacement.getPropertyValue('margin-left') === '7px';
        })()"#).unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)), "read cache changed ownership or retained substitution state");
    }

    #[test]
    fn css_declaration_iteration_reuses_live_indexed_collection_protocol() {
        let mut engine = lumen::Engine::new();
        let _realm = crate::install(engine.ctx(), "<div id=target style='margin:1px;top:2px'></div><style>div {padding:3px}</style>", 128).unwrap();
        let result = engine.eval_value(r#"(() => {
            const inline = document.getElementById('target').style;
            const iterator = inline[Symbol.iterator]();
            if (iterator.next().value !== 'margin-top') return false;
            inline.setProperty('left', '4px');
            const names = [];
            for (const name of inline) names.push(name);
            if (names.length !== 6 || names[5] !== 'left') return false;
            const rule = document.styleSheets[0].cssRules[0].style;
            const ruleNames = [];
            for (const name of rule) ruleNames.push(name);
            return ruleNames.join(',') === 'padding-top,padding-right,padding-bottom,padding-left';
        })()"#).unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)), "CSS declarations did not expose live indexed iteration");
    }

    #[test]
    fn specification_cssom_replacement_uses_canonical_usv_conversion_before_lock_and_parser() {
        let mut engine=lumen::Engine::new();let _realm=crate::install(engine.ctx(),"<span></span>",128).unwrap();
        let result=engine.eval_value(r#"(() => {
            const check=(ok,message)=>{if(!ok)throw Error(message)};
            globalThis.scalarSheet=new CSSStyleSheet();
            scalarSheet.replaceSync('.s-\uD800{color:red}');
            check(scalarSheet.cssRules[0].selectorText==='.s-\uFFFD','replaceSync converts lone surrogate before CSS parser');
            let calls=0;
            scalarSheet.replaceSync({toString(){calls++;scalarSheet.replaceSync('p{height:1px}');return '.s-\uDC00{color:blue}'}});
            check(calls===1 && scalarSheet.cssRules[0].selectorText==='.s-\uFFFD','conversion runs once before replacement state');
            globalThis.scalarDone=false;globalThis.scalarError=null;
            scalarSheet.replace({toString(){calls++;scalarSheet.replaceSync('p{height:2px}');return '.s-\uD800{color:green}'}}).then(value=>{
                check(value===scalarSheet && scalarSheet.cssRules[0].selectorText==='.s-\uFFFD','async replace owns converted scalar source');scalarDone=true;
            }).catch(error=>{scalarError=String(error)});
            check(calls===2 && scalarSheet.cssRules[0].selectorText==='p','conversion precedes async lock and old rules remain');
            return true;
        })()"#).unwrap();
        let value=result.unwrap_or_else(|error|panic!("replacement scalar conversion: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));
        assert!(matches!(value,Value::Bool(true)));
        assert!(lumen_host::owner_loop::pump(&mut engine,16).is_empty(),"replacement host settlement");
        engine.ctx().poll_async();engine.ctx().drain_microtasks_for_host();
        let result=engine.eval_value("scalarDone && scalarError===null").unwrap();
        let value=result.unwrap_or_else(|error|panic!("async replacement scalar conversion: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));
        assert!(matches!(value,Value::Bool(true)));
    }

    #[test]
    fn specification_cssom_adoption_checks_logical_documents_and_real_dom_exceptions() {
        let mut engine=lumen::Engine::new();let _realm=crate::install(engine.ctx(),"<div id=host></div>",256).unwrap();
        let result=engine.eval_value(r#"(() => {
            const check=(ok,message)=>{if(!ok)throw Error(message)};
            const sheet=new CSSStyleSheet();sheet.replaceSync('span{color:green}');
            const host=document.getElementById('host'),shadow=host.attachShadow({mode:'open'});
            shadow.innerHTML='<span>text</span>';const list=shadow.adoptedStyleSheets;list.push(sheet);
            const template=document.createElement('template');template.content.appendChild(host);
            check(list.length===1 && list[0]===sheet,'existing adopted list survives associated inert move');
            let error;try{list.push(sheet)}catch(failure){error=failure}
            check(error instanceof DOMException && error.name==='NotAllowedError' && error.code===0 && list.length===1,'new value must match actual inert node document');
            document.body.appendChild(host);list.push(sheet);check(list.length===2 && list===shadow.adoptedStyleSheets,'constructor document return restores new admissions');
            const foreign=new Document();foreign.adoptNode(host);
            check(list===shadow.adoptedStyleSheets && list.length===0,'foreign adoption clears same backing array');
            error=undefined;try{list.push(sheet)}catch(failure){error=failure}
            check(error instanceof DOMException && error.name==='NotAllowedError' && list.length===0,'foreign constructor document rejection');
            const style=document.createElement('style');style.title='';document.head.appendChild(style);
            check(style.sheet.title===null,'empty title has canonical nullable value');style.title='Named';check(style.sheet.title==='Named','nonempty title stays live');style.removeAttribute('title');check(style.sheet.title===null,'removed title returns null');
            error=undefined;try{document.adoptedStyleSheets.push(style.sheet)}catch(failure){error=failure}
            check(error instanceof DOMException && error.name==='NotAllowedError' && document.adoptedStyleSheets.length===0,'regular sheets reject canonical adoption');
            const values=document.adoptedStyleSheets;
            let leaked=false;Object.defineProperty(Array.prototype,'1',{configurable:true,set(){leaked=true}});
            try{values.push(sheet,sheet)}finally{delete Array.prototype['1']}
            check(!leaked && values.length===2 && values[0]===values[1],'indexed writes never expose the backing target to prototype setters');
            return true;
        })()"#).unwrap();
        let value=result.unwrap_or_else(|error|panic!("stylesheet ownership: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));
        assert!(matches!(value,Value::Bool(true)));
    }

    #[test]
    fn specification_cssom_adopted_snapshot_budget_admits_before_copy_and_rolls_back() {
        let mut engine=lumen::Engine::new();let _realm=crate::install(engine.ctx(),"<span id=target>text</span>",128).unwrap();
        let result=engine.eval_value(r#"(() => {
            const payload='x'.repeat(7000);
            const source=Array.from({length:100},(_,index)=>'.budget'+index+'{--payload:"'+payload+'"}').join('\n')+'span{color:green}';
            const sheet=new CSSStyleSheet();sheet.replaceSync(source);
            if(sheet.cssRules.length!==101||sheet.cssRules[0].style.getPropertyValue('--payload').length!==7002)throw Error('budget fixture did not retain actual declaration payload');
            const sheets=document.adoptedStyleSheets;
            for(let i=0;i<5;i++)sheets.push(sheet);
            let error;try{sheets.push(sheet)}catch(failure){error=failure}
            if(!error||error.name!=='QuotaExceededError'||sheets.length!==5||sheets[4]!==sheet)throw Error('snapshot admission did not roll back actual list');
            if(getComputedStyle(target).color!=='rgb(0, 128, 0)')throw Error('failed admission altered existing cascade');
            sheets.pop();sheets.push(sheet);if(sheets.length!==5)throw Error('budget was not reusable after real deletion');
            return true;
        })()"#).unwrap();
        let value=result.unwrap_or_else(|error|panic!("adopted snapshot admission: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));
        assert!(matches!(value,Value::Bool(true)));
    }

    #[test]
    fn specification_adopted_stylesheets_are_live_observable_arrays_with_canonical_mutations() {
        let mut engine = lumen::Engine::new();
        let _realm = crate::install(engine.ctx(), "<div id=target></div><div id=host></div>", 256).unwrap();
        let result = engine.eval_value(r#"(() => {
            const check = (ok, message) => { if (!ok) throw new Error(message); };
            const red = new CSSStyleSheet(); red.replaceSync('#target {color:red}');
            const blue = new CSSStyleSheet(); blue.replaceSync('#target {color:blue}');
            const target = document.getElementById('target'), host = document.getElementById('host');
            const sheets = document.adoptedStyleSheets;
            check(Array.isArray(sheets) && sheets === document.adoptedStyleSheets, 'stable genuine observable array');
            sheets.push(red, blue, red);
            check(sheets.length === 3 && sheets[0] === sheets[2] && getComputedStyle(target).color === 'rgb(255, 0, 0)', 'duplicates preserve cascade order');
            sheets.pop(); check(getComputedStyle(target).color === 'rgb(0, 0, 255)', 'pop mutates authoritative cascade');
            sheets.reverse(); check(sheets[0] === blue && sheets[1] === red, 'reverse replaces live indexed entries');
            sheets.splice(0, 1, red); check(sheets.length === 2 && sheets[0] === red, 'splice uses observable indexed operations');
            check(!Reflect.set(sheets, '4', blue) && !Reflect.set(sheets, 'length', 4), 'holes and growing length rejected');
            check(!Reflect.deleteProperty(sheets, '0') && Reflect.deleteProperty(sheets, '1') && sheets.length === 1, 'only terminal deletion is permitted');
            const descriptor = Object.getOwnPropertyDescriptor(sheets, '0');
            check(descriptor.value === red && descriptor.writable && descriptor.enumerable && descriptor.configurable, 'indexed descriptor flags');
            check(!Reflect.defineProperty(sheets, '0', {writable:false}) && !Reflect.preventExtensions(sheets), 'observable structural invariants');
            let conversions = 0;
            sheets.length = {valueOf() {conversions++; return 0;}};
            check(conversions === 2 && sheets.length === 0, 'length converts twice and shrinks');
            document.adoptedStyleSheets = new Set([blue, red]);
            check(sheets === document.adoptedStyleSheets && sheets[0] === blue && sheets[1] === red, 'iterable assignment retains array identity');
            let invalid = false; try {document.adoptedStyleSheets = [blue, 'invalid'];} catch (error) {invalid = error instanceof TypeError;}
            check(invalid && sheets.length === 2 && sheets[1] === red, 'sequence conversion finishes before mutation');
            const shadow = host.attachShadow({mode:'open'});
            shadow.innerHTML = '<div id=target></div>';
            const shadowSheets = shadow.adoptedStyleSheets;
            shadowSheets.push(blue);
            check(shadowSheets === shadow.adoptedStyleSheets && getComputedStyle(shadow.firstChild).color === 'rgb(0, 0, 255)', 'shadow array mutates actual scoped cascade');
            const template = document.createElement('template');
            template.content.append(host); document.body.append(host);
            check(shadow.adoptedStyleSheets === shadowSheets && shadowSheets[0] === blue, 'associated template adoption preserves list identity');
            const other = new Document(); other.adoptNode(host);
            check(shadow.adoptedStyleSheets === shadowSheets && shadowSheets.length === 0, 'cross-document adoption clears list without replacing array');
            return true;
        })()"#).unwrap().unwrap_or_else(|_| panic!("observable stylesheet array guard threw"));
        assert!(matches!(result, Value::Bool(true)), "observable stylesheet mutation changed identity, invariants, or real cascade");
    }

    #[test]
    fn duplicated_adopted_stylesheets_preserve_occurrences_and_typed_mutations() {
        let mut engine = lumen::Engine::new();
        let _realm = crate::install(engine.ctx(), "<div id=target></div><div id=host></div>", 128).unwrap();
        let result = engine.eval_value(r#"(() => {
            const red = new CSSStyleSheet(); red.replaceSync('#target {color:red}');
            const green = new CSSStyleSheet(); green.replaceSync('#target {color:green}');
            const target = document.getElementById('target');
            document.adoptedStyleSheets = [red, green, red];
            if (document.adoptedStyleSheets.length !== 3 || getComputedStyle(target).color !== 'rgb(255, 0, 0)') return false;
            document.adoptedStyleSheets = [green, red, green];
            if (getComputedStyle(target).color !== 'rgb(0, 128, 0)') return false;
            green.cssRules[0].style.setProperty('color', 'blue');
            if (getComputedStyle(target).color !== 'rgb(0, 0, 255)') return false;
            const empty = new CSSStyleSheet();
            document.adoptedStyleSheets = [empty, green, red, green];
            if (getComputedStyle(target).color !== 'rgb(0, 0, 255)') return false;
            const shadow = document.getElementById('host').attachShadow({mode:'open'});
            shadow.innerHTML = '<div id=target></div>';
            shadow.adoptedStyleSheets = [green, red, green];
            return shadow.adoptedStyleSheets[0] === shadow.adoptedStyleSheets[2]
                && getComputedStyle(shadow.querySelector('#target')).color === 'rgb(0, 0, 255)';
        })()"#).unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)), "duplicated adoption lost order or shared mutation state");
    }

    #[test]
    fn constructed_stylesheets_are_adopted_scoped_and_mutate_the_real_cascade() {
        use lumen::Engine;

        let mut engine = Engine::new();
        let realm = crate::install(
            engine.ctx(),
            "<div id=outside></div><div id=host></div>",
            128,
        )
        .unwrap();
        let result = engine.eval_value(
            "(() => { const sheet = new CSSStyleSheet(); sheet.replaceSync('div { width: 12px }'); document.adoptedStyleSheets = [sheet]; const host = document.getElementById('host'); const root = host.attachShadow({mode:'open'}); root.innerHTML = '<p></p>'; const shadowSheet = new CSSStyleSheet(); shadowSheet.replaceSync('p { height: 9px }'); root.adoptedStyleSheets = [shadowSheet]; window.__promiseResult = false; const promise = shadowSheet.replace('p { height: 11px }').then(result => window.__promiseResult = result === shadowSheet); return document.adoptedStyleSheets[0] === sheet && root.adoptedStyleSheets[0] === shadowSheet && sheet.cssRules.length === 1 && sheet.ownerNode === null && promise instanceof Promise; })()"
        ).unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
        engine.ctx().drain_microtasks_for_host();
        let resolved = engine
            .eval_value("window.__promiseResult")
            .unwrap()
            .ok()
            .unwrap();
        assert!(
            matches!(resolved, Value::Bool(true)),
            "replace() did not fulfill with its stylesheet"
        );
        realm.with_session(|session| {
            let document = session.document();
            let outside = selector::query_selector(document, document.root(), "#outside")
                .unwrap()
                .unwrap();
            let host = selector::query_selector(document, document.root(), "#host")
                .unwrap()
                .unwrap();
            let shadow = document.shadow_root(host).unwrap().unwrap();
            let paragraph = selector::query_selector(document, shadow, "p")
                .unwrap()
                .unwrap();
            assert_eq!(session.computed_style(outside).unwrap().width, Some(12.0));
            assert_eq!(
                session.computed_style(paragraph).unwrap().height,
                Some(11.0)
            );
        });
    }
}
