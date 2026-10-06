//! CSSOM algorithms that build on the shared HTML CSS parser and cascade.
//!
//! The DOM adapter installs `CSS.supports` and `matchMedia` from these
//! declarations. Stylesheet text edits are validated by the same parser before
//! the owning `<style>` node is changed, so the renderer and CSSOM observe one
//! source of truth.
use super::*;
use crate::realm_services::RealmServices;
use lumen::embed::Promise;
use lumen::embed::{JsFunction, JsHost, JsObject};
use lumen_bind::{CtorRet, FromArg, Host, Passed, This};
use lumen_html::css::{self, MediaEnvironment};
use std::{cell::Cell, collections::HashSet, rc::Weak, sync::Arc};

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
    let lower = text.trim_start().to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix(name) else {
        return false;
    };
    rest.is_empty()
        || rest
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_whitespace() || character == '(')
}

#[lumen_bind::class(name = "CSSStyleValue", hint(js(webidl)))]
pub struct DomCssStyleValue {
    serialized_value: RefCell<String>,
}

#[lumen_bind::class(name = "CSSKeywordValue", extends = DomCssStyleValue, hint(js(webidl)))]
pub struct DomCssKeywordValue {
    base: DomCssStyleValue,
}

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

fn style_value_from_text(ctx: &mut Ctx, property: &str, css_text: &str) -> OpResult<Value> {
    let mut values = style_values_from_text(ctx, property, css_text, false)?;
    values
        .pop()
        .ok_or_else(|| OpError::type_error("CSS value did not produce a Typed OM value"))
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
    if let Some(numeric) = css::typed_numeric::parse_property_numeric_value(property, css_text) {
        return Ok(unit_value(ctx, numeric.value, numeric.unit));
    }
    if let Some(mut expression) = css::typed_numeric::parse_numeric_expression(css_text) {
        if !expression.contains_sign() && expression.numeric_type().is_some() {
            expression.simplify_absolute_units();
            return reify_numeric_expression(ctx, &expression);
        }
    }
    if let Some(value) = css_identifier_value(css_text.trim()) {
        return Ok(ctx.new_instance(keyword_value(value)));
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
        Expression::Sign(_) | Expression::Value(_) => "sum",
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
    ctx: &mut Ctx,
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
    let base = math_numeric_base(ctx, expression, values_key.clone())?;
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
        Ok(css::typed_numeric::serialize_numeric_value(value, unit))
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
    fn to_string(&self) -> String {
        self.serialized_value.borrow().clone()
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
        if expression.contains_sign() || expression.numeric_type().is_none() {
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

fn inline_declaration(realm: &DomRealm, node: NodeId) -> OpResult<String> {
    let session = realm.session.borrow();
    match session.document().kind(node).map_err(dom_error)? {
        NodeKind::Element { .. } => Ok(session
            .document()
            .get_attribute_ns_ref(node, None, "style")
            .map_err(dom_error)?
            .unwrap_or("")
            .to_owned()),
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
    let mut declaration = CssDeclaration::parse(&inline_declaration(realm, node)?);
    declaration
        .set_property(property, value, false)
        .map_err(css_error)?;
    realm
        .session
        .borrow_mut()
        .document_mut()
        .set_attribute_ns(node, None, "style", declaration.css_text())
        .map_err(dom_error)
}

fn value_from_inline_style(
    ctx: &mut Ctx,
    realm: &DomRealm,
    node: NodeId,
    property: &str,
) -> OpResult<Option<Value>> {
    let declaration = inline_declaration(realm, node)?;
    let Some((value, _)) = css::declaration_value(&declaration, property).map_err(css_error)?
    else {
        return Ok(None);
    };
    Ok(Some(style_value_from_text(ctx, property, &value)?))
}

fn property_map_text(
    map: &DomStylePropertyMapReadOnly,
    property: &str,
) -> OpResult<Option<String>> {
    if map.computed {
        let value = crate::style::computed_property_value(&map.realm, map.node, property)?;
        Ok((!value.is_empty()).then_some(value))
    } else {
        let declaration = inline_declaration(&map.realm, map.node)?;
        Ok(css::declaration_value(&declaration, property)
            .map_err(css_error)?
            .map(|(value, _)| value))
    }
}

fn value_array(ctx: &mut Ctx, values: impl IntoIterator<Item = Value>) -> OpResult<Value> {
    Ok(JsHost::from_list(ctx, values.into_iter().collect()))
}

#[lumen_bind::methods]
impl DomStylePropertyMapReadOnly {
    #[method(coerce)]
    fn get(&self, ctx: &mut Ctx, property: &str) -> OpResult<Value> {
        let value = if self.computed {
            property_map_text(self, property)?
                .map(|text| style_value_from_text(ctx, property, &text))
                .transpose()?
        } else {
            value_from_inline_style(ctx, &self.realm, self.node, property)?
        };
        Ok(value.unwrap_or(Value::Undefined))
    }

    #[method(coerce)]
    fn get_all(&self, ctx: &mut Ctx, property: &str) -> OpResult<Value> {
        if self.computed {
            let values = property_map_text(self, property)?
                .map(|text| style_values_from_text(ctx, property, &text, true))
                .transpose()?
                .unwrap_or_default();
            value_array(ctx, values)
        } else {
            let value = value_from_inline_style(ctx, &self.realm, self.node, property)?;
            value_array(ctx, value.into_iter())
        }
    }

    #[method(coerce)]
    fn has(&self, property: &str) -> OpResult<bool> {
        if self.computed {
            Ok(property_map_text(self, property)?.is_some())
        } else {
            let declaration = inline_declaration(&self.realm, self.node)?;
            Ok(css::declaration_value(&declaration, property)
                .map_err(css_error)?
                .is_some())
        }
    }
}

#[lumen_bind::methods]
impl DomStylePropertyMap {
    #[method(coerce)]
    fn set(&self, ctx: &mut Ctx, property: &str, value: Value) -> OpResult<()> {
        let value = if ctx
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

/// Text-backed CSSOM for a single style attribute or stylesheet rule.
/// Invalid declarations are discarded using the renderer's supported-property
/// registry, matching the existing `CSSStyleDeclaration` adapter.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CssDeclaration {
    text: String,
}

impl CssDeclaration {
    pub fn parse(text: &str) -> Self {
        Self {
            text: css::cssom_declaration_text(text),
        }
    }

    pub fn css_text(&self) -> &str {
        &self.text
    }

    pub fn get_property_value(&self, name: &str) -> Result<Option<(String, bool)>, css::CssError> {
        Ok(
            css::declaration_value(&self.text, name)?.map(|(value, important)| {
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
        if value.is_empty() {
            self.text = css::set_declaration(&self.text, name, "", false)?;
        } else if css::supports_declaration(name, value) {
            self.text = css::set_declaration(&self.text, name, value, important)?;
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
}

/// Mutable stylesheet source associated with one `<style>` node. Every edit is
/// validated first, then callers commit the resulting text to the document;
/// `RenderSession` reparses that same node for cascade evaluation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CssStyleSheetText {
    text: String,
}

impl CssStyleSheetText {
    pub fn parse(text: &str) -> Result<Self, css::CssError> {
        css::parse(text)?;
        Ok(Self { text: text.into() })
    }

    pub fn css_text(&self) -> &str {
        &self.text
    }

    pub fn css_rules(&self) -> Result<Vec<CssRuleText>, css::CssError> {
        rules_from_text(&self.text)
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
                rule.style = CssDeclaration {
                    text: if rule.font_face {
                        css::cssom_font_face_declaration_text(declarations)
                    } else {
                        css::cssom_declaration_text(declarations)
                    },
                };
                rule.css_text = serialize_rule(rule);
            }
            Ok(())
        })
    }

    fn insert_nested_rule(
        &mut self,
        parent_path: &[usize],
        rule: &str,
        index: usize,
    ) -> Result<(), css::CssError> {
        let context = self.selector_context(parent_path)?;
        let context = context.iter().map(String::as_str).collect::<Vec<_>>();
        let inserted = parse_group_child(rule, &context)?;
        self.mutate_nested_list(parent_path, |children| {
            if index > children.len() {
                return Err(css::CssError {
                    offset: index,
                    message: "CSS rule index out of range",
                });
            }
            children.insert(index, inserted);
            Ok(())
        })
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
        Ok(removed.unwrap_or_default())
    }

    fn set_rule_selector(&mut self, path: &[usize], selector: &str) -> Result<(), css::CssError> {
        let parent_path = path
            .split_last()
            .map(|(_, parents)| parents)
            .unwrap_or_default();
        let context = self.selector_context(parent_path)?;
        let context = context.iter().map(String::as_str).collect::<Vec<_>>();
        let selector = selector.trim();
        if selector.is_empty() {
            return Ok(());
        }
        if context.is_empty() {
            if css::parse(&format!("{selector} {{}}"))?.is_empty() {
                return Err(css::CssError {
                    offset: 0,
                    message: "invalid selector list",
                });
            }
        } else {
            css::validate_nested_selector_list(selector, &context)?;
        }
        let selector = implicit_nested_selector(selector, !context.is_empty());
        self.mutate_rule(path, |rule| {
            if rule.selector_text.is_none() {
                return Ok(());
            }
            rule.selector_text = Some(selector.clone());
            rule.css_text = serialize_rule(rule);
            Ok(())
        })
    }

    fn selector_context(&self, path: &[usize]) -> Result<Vec<String>, css::CssError> {
        let mut rules = self.css_rules()?;
        let mut context = Vec::new();
        for index in path {
            let Some(parent) = rules.get(*index) else {
                return Err(css::CssError {
                    offset: *index,
                    message: "CSS rule index out of range",
                });
            };
            if let Some(selector) = parent.selector_text.as_ref() {
                context.push(selector.clone());
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
        let Some((&top_index, rest)) = path.split_first() else {
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
        let Some(root_rule) = rules.get(top_index) else {
            return Err(css::CssError {
                offset: top_index,
                message: "CSS rule index out of range",
            });
        };
        self.replace_rule_text(top_index, &root_rule.css_text)
    }

    pub fn insert_rule(&mut self, rule: &str, index: usize) -> Result<(), css::CssError> {
        let mut current = self.css_rules()?;
        if index > current.len() {
            return Err(css::CssError {
                offset: index,
                message: "CSS rule index out of range",
            });
        }
        let proposed = rules_from_text(rule)?;
        if proposed.len() != 1 {
            return Err(css::CssError {
                offset: 0,
                message: "insertRule requires exactly one rule",
            });
        }
        let Some(rule) = proposed.into_iter().next() else {
            return Err(css::CssError {
                offset: 0,
                message: "insertRule requires exactly one rule",
            });
        };
        current.insert(index, rule);
        self.text = current
            .iter()
            .map(|rule| rule.css_text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        css::parse(&self.text)?;
        Ok(())
    }

    pub fn delete_rule(&mut self, index: usize) -> Result<(), css::CssError> {
        let mut current = self.css_rules()?;
        if index >= current.len() {
            return Err(css::CssError {
                offset: index,
                message: "CSS rule index out of range",
            });
        }
        current.remove(index);
        self.text = current
            .iter()
            .map(|rule| rule.css_text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        Ok(())
    }

    pub fn replace(&mut self, text: &str) -> Result<(), css::CssError> {
        let parsed = Self::parse(text)?;
        *self = parsed;
        Ok(())
    }

    pub fn set_style_rule_declarations(
        &mut self,
        index: usize,
        declarations: &str,
    ) -> Result<(), css::CssError> {
        let rules = self.css_rules()?;
        let Some(rule) = rules.get(index) else {
            return Err(css::CssError {
                offset: index,
                message: "CSS rule index out of range",
            });
        };
        let Some(selector) = rule
            .selector_text
            .as_deref()
            .or(rule.font_face.then_some("@font-face"))
        else {
            return Err(css::CssError {
                offset: index,
                message: "rule is not a style rule",
            });
        };
        let ranges = top_level_rule_ranges(&self.text)?;
        let mut output = String::new();
        for (rule_index, (start, end, open, close)) in ranges.into_iter().enumerate() {
            if !output.is_empty() {
                output.push('\n');
            }
            if rule_index == index {
                if open.is_none() || close.is_none() {
                    return Err(css::CssError {
                        offset: start,
                        message: "rule is not a style rule",
                    });
                }
                output.push_str(selector);
                output.push_str(" {");
                if !declarations.trim().is_empty() {
                    output.push(' ');
                    output.push_str(&if rule.font_face {
                        css::cssom_font_face_declaration_text(declarations)
                    } else {
                        css::cssom_declaration_text(declarations)
                    });
                    output.push(' ');
                }
                output.push('}');
            } else {
                output.push_str(self.text[start..end].trim());
            }
        }
        css::parse(&output)?;
        self.text = output;
        Ok(())
    }

    fn replace_rule_text(&mut self, index: usize, replacement: &str) -> Result<(), css::CssError> {
        let ranges = top_level_rule_ranges(&self.text)?;
        let Some((start, end, _, _)) = ranges.get(index).copied() else {
            return Err(css::CssError {
                offset: index,
                message: "CSS rule index out of range",
            });
        };
        let mut output = String::with_capacity(self.text.len() + replacement.len());
        output.push_str(&self.text[..start]);
        output.push_str(replacement);
        output.push_str(&self.text[end..]);
        css::parse(&output)?;
        self.text = output;
        Ok(())
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
        keyframes.rules.push(frame);
        self.replace_keyframes(index, &keyframes)
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
        target.style = css::cssom_declaration_text(declarations);
        target.css_text = format!("{} {{ {} }}", target.key_text, target.style);
        self.replace_keyframes(index, &keyframes)
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

fn rules_from_text(text: &str) -> Result<Vec<CssRuleText>, css::CssError> {
    css::parse(text)?;
    rules_from_text_in_context(text, &[], false)
}

fn rules_from_text_in_context(
    text: &str,
    parent_context: &[&str],
    validate_nested_selectors: bool,
) -> Result<Vec<CssRuleText>, css::CssError> {
    let allow_nested_declarations = !parent_context.is_empty();
    let ranges = top_level_rule_ranges_in_context(text, allow_nested_declarations)?;
    let mut result = Vec::with_capacity(ranges.len());
    let mut pending_declarations = String::new();
    for (start, end, open, close) in ranges {
        let raw = text[start..end].trim();
        if let (Some(open), Some(close)) = (open, close) {
            append_nested_declarations(
                &mut result,
                &mut pending_declarations,
                allow_nested_declarations,
            );
            let prelude = text[start..open].trim();
            let body = &text[open + 1..close];
            let font_face = css::is_font_face_prelude(prelude);
            let keyframes = css::parse_keyframes_rule(raw)?;
            let grouping = starts_css_at_rule(prelude, "@media")
                || starts_css_at_rule(prelude, "@layer")
                || starts_css_at_rule(prelude, "@supports")
                || keyframes.is_some();
            let (style, nested) = if font_face {
                (
                    CssDeclaration {
                        text: css::cssom_font_face_declaration_text(body),
                    },
                    Vec::new(),
                )
            } else if let Some(keyframes) = keyframes.as_ref() {
                (
                    CssDeclaration::default(),
                    keyframes
                        .rules
                        .iter()
                        .map(|frame| CssRuleText {
                            css_text: frame.css_text.clone(),
                            selector_text: Some(frame.key_text.clone()),
                            style: CssDeclaration::parse(&frame.style),
                            nested: Vec::new(),
                            nested_declarations: false,
                            font_face: false,
                            keyframes: None,
                        })
                        .collect(),
                )
            } else if prelude.starts_with('@') {
                (
                    CssDeclaration::parse(body),
                    if grouping {
                        rules_from_text_in_context(body, parent_context, validate_nested_selectors)?
                    } else {
                        Vec::new()
                    },
                )
            } else {
                let mut context = parent_context.to_vec();
                context.push(prelude);
                let (declarations, nested) =
                    nested_style_body(body, &context, validate_nested_selectors)?;
                (CssDeclaration { text: declarations }, nested)
            };
            if prelude.starts_with('@') && !grouping && !font_face {
                continue;
            }
            if !allow_nested_declarations
                && !prelude.starts_with('@')
                && css::parse(raw)?.is_empty()
            {
                continue;
            }
            let selector_text = if prelude.starts_with('@') {
                None
            } else if parent_context.is_empty() {
                Some(prelude.to_owned())
            } else {
                let valid = css::validate_nested_selector_list(prelude, parent_context).is_ok();
                if validate_nested_selectors && !valid {
                    css::validate_nested_selector_list(prelude, parent_context)?;
                }
                Some(if valid {
                    implicit_nested_selector(prelude, true)
                } else {
                    // Stylesheet parsing is forgiving. Keep authored selector text so the
                    // containing CSSStyleRule can still serialize the source faithfully.
                    prelude.to_owned()
                })
            };
            result.push(CssRuleText {
                css_text: raw.to_owned(),
                selector_text,
                style,
                font_face,
                keyframes: keyframes.clone(),
                nested,
                nested_declarations: false,
            });
        } else {
            if allow_nested_declarations && !raw.starts_with('@') {
                if !pending_declarations.is_empty() {
                    pending_declarations.push(' ');
                }
                pending_declarations.push_str(raw);
            } else if raw.starts_with('@') {
                result.push(CssRuleText {
                    css_text: raw.to_owned(),
                    selector_text: None,
                    style: CssDeclaration::default(),
                    nested: Vec::new(),
                    nested_declarations: false,
                    font_face: false,
                    keyframes: None,
                });
            }
        }
    }
    append_nested_declarations(
        &mut result,
        &mut pending_declarations,
        allow_nested_declarations,
    );
    Ok(result)
}

fn append_nested_declarations(output: &mut Vec<CssRuleText>, pending: &mut String, enabled: bool) {
    if !enabled || pending.is_empty() {
        pending.clear();
        return;
    }
    let declarations = css::cssom_declaration_text(pending);
    pending.clear();
    if !declarations.is_empty() {
        output.push(CssRuleText {
            css_text: declarations.clone(),
            selector_text: None,
            style: CssDeclaration { text: declarations },
            nested: Vec::new(),
            nested_declarations: true,
            font_face: false,
            keyframes: None,
        });
    }
}

fn nested_style_body(
    text: &str,
    parent_context: &[&str],
    validate_nested_selectors: bool,
) -> Result<(String, Vec<CssRuleText>), css::CssError> {
    let ranges = top_level_rule_ranges_in_context(text, true)?;
    let mut own_declarations = String::new();
    let mut nested_rules = Vec::new();
    let mut trailing_declarations = String::new();
    let mut encountered_nested_rule = false;
    for (start, end, open, close) in ranges {
        let raw = text[start..end].trim();
        if open.is_none() || close.is_none() {
            let destination = if encountered_nested_rule {
                &mut trailing_declarations
            } else {
                &mut own_declarations
            };
            if !destination.is_empty() {
                destination.push(' ');
            }
            destination.push_str(raw);
            continue;
        }
        append_nested_declarations(&mut nested_rules, &mut trailing_declarations, true);
        encountered_nested_rule = true;
        nested_rules.extend(rules_from_text_in_context(
            raw,
            parent_context,
            validate_nested_selectors,
        )?);
    }
    append_nested_declarations(&mut nested_rules, &mut trailing_declarations, true);
    Ok((css::cssom_declaration_text(&own_declarations), nested_rules))
}

fn parse_group_child(text: &str, parent_context: &[&str]) -> Result<CssRuleText, css::CssError> {
    let allow_declaration = !parent_context.is_empty();
    if allow_declaration
        && !text.trim_start().starts_with('@')
        && !text.contains('{')
        && !text.contains('}')
    {
        let declarations = css::cssom_declaration_text(text);
        if !declarations.is_empty() {
            return Ok(CssRuleText {
                css_text: declarations.clone(),
                selector_text: None,
                style: CssDeclaration { text: declarations },
                nested: Vec::new(),
                nested_declarations: true,
                font_face: false,
                keyframes: None,
            });
        }
    }
    let parsed = if allow_declaration {
        rules_from_text_in_context(text, parent_context, true)?
    } else {
        rules_from_text(text)?
    };
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
        rule.style = CssDeclaration { text: declarations };
    }
    Ok(rule)
}

fn implicit_nested_selector(selector: &str, has_parent: bool) -> String {
    if !has_parent || contains_nesting_selector(selector) {
        selector.to_owned()
    } else {
        format!("& {selector}")
    }
}

fn contains_nesting_selector(input: &str) -> bool {
    let bytes = input.as_bytes();
    let mut index = 0usize;
    let mut quote = None;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(delimiter) = quote {
            if byte == b'\\' {
                index = index.saturating_add(2);
                continue;
            }
            if byte == delimiter {
                quote = None;
            }
            index += 1;
            continue;
        }
        if bytes.get(index..index + 2) == Some(b"/*") {
            let Some(end) = input[index + 2..].find("*/") else {
                return false;
            };
            index += end + 4;
            continue;
        }
        match byte {
            b'\\' => index = index.saturating_add(2),
            b'\'' | b'"' => {
                quote = Some(byte);
                index += 1;
            }
            b'&' => return true,
            _ => index += 1,
        }
    }
    false
}

fn serialize_rule(rule: &CssRuleText) -> String {
    let declarations = || {
        css::serialize_cssom_declaration_block(rule.style.css_text())
            .unwrap_or_else(|_| rule.style.css_text().to_owned())
    };
    if rule.nested_declarations {
        return declarations();
    }
    let Some(open) = rule.css_text.find('{') else {
        return rule.css_text.clone();
    };
    let prelude = rule
        .selector_text
        .as_deref()
        .unwrap_or_else(|| rule.css_text[..open].trim());
    if rule.selector_text.is_some() && rule.nested.is_empty() {
        let declarations = declarations();
        if declarations.is_empty() {
            format!("{prelude} {{ }}")
        } else {
            format!("{prelude} {{ {declarations} }}")
        }
    } else {
        let mut body = Vec::new();
        if !rule.style.css_text().is_empty() {
            body.push(format!("  {}", declarations()));
        }
        body.extend(
            rule.nested
                .iter()
                .map(|child| format!("  {}", serialize_rule(child))),
        );
        if body.is_empty() {
            format!("{prelude} {{\n}}")
        } else {
            format!("{prelude} {{\n{}\n}}", body.join("\n"))
        }
    }
}

fn top_level_rule_ranges(
    text: &str,
) -> Result<Vec<(usize, usize, Option<usize>, Option<usize>)>, css::CssError> {
    top_level_rule_ranges_in_context(text, false)
}

fn top_level_rule_ranges_in_context(
    text: &str,
    allow_declaration_tail: bool,
) -> Result<Vec<(usize, usize, Option<usize>, Option<usize>)>, css::CssError> {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut start = 0usize;
    while start < bytes.len() {
        while start < bytes.len() && bytes[start].is_ascii_whitespace() {
            start += 1;
        }
        if start == bytes.len() {
            break;
        }
        if bytes.get(start..start + 2) == Some(b"/*") {
            let Some(end) = text[start + 2..].find("*/") else {
                return Err(css::CssError {
                    offset: start,
                    message: "unterminated comment",
                });
            };
            start += end + 4;
            continue;
        }
        let mut index = start;
        let mut quote = None;
        let mut parens = 0usize;
        let mut open = None;
        let mut statement = false;
        while index < bytes.len() {
            let byte = bytes[index];
            if let Some(delimiter) = quote {
                if byte == b'\\' {
                    index = index.saturating_add(2);
                    continue;
                }
                if byte == delimiter {
                    quote = None;
                }
                index += 1;
                continue;
            }
            if bytes.get(index..index + 2) == Some(b"/*") {
                let Some(end) = text[index + 2..].find("*/") else {
                    return Err(css::CssError {
                        offset: index,
                        message: "unterminated comment",
                    });
                };
                index += end + 4;
                continue;
            }
            match byte {
                b'\'' | b'"' => quote = Some(byte),
                b'(' => parens += 1,
                b')' => parens = parens.saturating_sub(1),
                b'{' if parens == 0 => {
                    open = Some(index);
                    break;
                }
                b';' if parens == 0 => {
                    ranges.push((start, index + 1, None, None));
                    start = index + 1;
                    statement = true;
                    break;
                }
                _ => {}
            }
            index += 1;
        }
        if statement {
            continue;
        }
        if let Some(open) = open {
            let mut depth = 1usize;
            index = open + 1;
            quote = None;
            while index < bytes.len() && depth != 0 {
                let byte = bytes[index];
                if let Some(delimiter) = quote {
                    if byte == b'\\' {
                        index = index.saturating_add(2);
                        continue;
                    }
                    if byte == delimiter {
                        quote = None;
                    }
                } else if bytes.get(index..index + 2) == Some(b"/*") {
                    let Some(end) = text[index + 2..].find("*/") else {
                        return Err(css::CssError {
                            offset: index,
                            message: "unterminated comment",
                        });
                    };
                    index += end + 4;
                    continue;
                } else {
                    match byte {
                        b'\'' | b'"' => quote = Some(byte),
                        b'{' => depth += 1,
                        b'}' => depth -= 1,
                        _ => {}
                    }
                }
                index += 1;
            }
            if depth != 0 {
                return Err(css::CssError {
                    offset: open,
                    message: "unterminated rule",
                });
            }
            let close = index - 1;
            ranges.push((start, index, Some(open), Some(close)));
            start = index;
        } else if allow_declaration_tail && !text[start..].trim_start().starts_with('@') {
            // The final declaration in a style block need not have a semicolon.
            // Its grammar and recovery remain owned by cssom_declaration_text.
            ranges.push((start, bytes.len(), None, None));
            start = bytes.len();
        } else if start < bytes.len() {
            return Err(css::CssError {
                offset: start,
                message: "expected rule block",
            });
        }
    }
    Ok(ranges)
}

/// A viewport-bound MediaQueryList snapshot. The DOM wrapper can query it on
/// each `matches` read; dispatching `change` requires a host resize source.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaQueryList {
    query: String,
    environment: MediaEnvironment,
}

impl MediaQueryList {
    pub fn new(query: &str, environment: MediaEnvironment) -> Self {
        Self {
            query: query.to_owned(),
            environment,
        }
    }

    pub fn media(&self) -> &str {
        &self.query
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

struct MediaQueryRegistry {
    realm: Weak<DomRealm>,
    lists: RefCell<Vec<Weak<MediaQueryData>>>,
    sheets: RefCell<HashMap<NodeId, WeakValue>>,
    sheet_positions: RefCell<HashMap<NodeId, Weak<RulePositions>>>,
    adopted: RefCell<Vec<(Option<NodeId>, Vec<AdoptedSheet>)>>,
}

#[derive(Default)]
struct RulePositions {
    frame_rules: bool,
    text: RefCell<Option<String>>,
    live: RefCell<Vec<Weak<RulePosition>>>,
}

struct RulePosition {
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
                    ..Default::default()
                })
            })
            .clone()
    }
}

impl RulePositions {
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
            index: Cell::new(index),
            detached: RefCell::new(None),
            owner: self.clone(),
            wrapper: RefCell::new(None),
            children: RefCell::new(None),
        });
        live.push(Rc::downgrade(&position));
        position
    }

    fn remember(&self, text: &str) {
        *self.text.borrow_mut() = Some(text.to_owned());
    }

    fn synchronize(&self, text: &str) -> OpResult<()> {
        if self.text.borrow().as_deref() == Some(text) {
            return Ok(());
        }
        self.replaced(text)
    }

    fn replaced(&self, text: &str) -> OpResult<()> {
        self.live
            .borrow_mut()
            .retain(|position| position.strong_count() != 0);
        let previous = self.text.borrow();
        if let Some(previous) = previous
            .as_deref()
            .filter(|_| !self.live.borrow().is_empty())
        {
            let rules: Vec<String> = if self.frame_rules {
                css::parse_keyframes_rule(&format!("@keyframes identity {{ {previous} }}"))
                    .map_err(css_error)?
                    .ok_or_else(|| OpError::new("SyntaxError", "invalid retained keyframes"))?
                    .rules
                    .into_iter()
                    .map(|rule| rule.css_text)
                    .collect()
            } else {
                rules_from_text(previous)
                    .map_err(css_error)?
                    .into_iter()
                    .map(|rule| rule.css_text)
                    .collect()
            };
            for position in self.live.borrow().iter().filter_map(Weak::upgrade) {
                if position.detached.borrow().is_none() {
                    if let Some(rule) = rules.get(position.get()) {
                        *position.detached.borrow_mut() = Some(rule.clone());
                        position.index.set(0);
                    }
                }
            }
            self.live.borrow_mut().clear();
        }
        drop(previous);
        self.remember(text);
        Ok(())
    }

    fn inserted(&self, index: usize, text: &str) {
        for position in self.live.borrow().iter().filter_map(Weak::upgrade) {
            if position.detached.borrow().is_none() && position.get() >= index {
                position.index.set(position.get() + 1);
            }
        }
        self.remember(text);
    }

    fn deleted(&self, index: usize, removed: &str, text: &str) {
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
        self.remember(text);
    }
}

struct ConstructedSheetData {
    realm: Weak<DomRealm>,
    registry: Weak<MediaQueryRegistry>,
    text: RefCell<String>,
    positions: Rc<RulePositions>,
}

struct ImportedSheetData {
    owner: NodeId,
    path: Vec<usize>,
    text: RefCell<String>,
    positions: Rc<RulePositions>,
}

#[derive(Clone)]
enum SheetSource {
    Element {
        node: NodeId,
        positions: Rc<RulePositions>,
    },
    Rule {
        source: Rc<SheetSource>,
        position: Rc<RulePosition>,
    },
    Constructed(Rc<ConstructedSheetData>),
    Imported(Rc<ImportedSheetData>),
}

impl SheetSource {
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
    path.iter().map(|position| position.get()).collect()
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
    source.positions().remember(sheet.css_text());
    let mut rules = sheet.css_rules().map_err(css_error)?;
    for parent in path {
        let Some(rule) = rules.get(parent.get()) else {
            return Ok(());
        };
        let children = parent.rule_children();
        children.remember(&rules_text(&rule.nested));
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
            let environment = realm.session.borrow().media_environment();
            css::media_query_matches(&self.data.query, environment)
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
    fn onchange(&self) -> Nullable<JsFunction> {
        Nullable(self.base.handler("change"))
    }

    #[setter]
    fn set_onchange(&self, ctx: &mut Ctx, this: This<Value>, callback: Option<JsFunction>) {
        self.base.set_handler(ctx, &this.0, "change", callback);
        *self.data.wrapper.borrow_mut() = ctx.weak_value(&this.0);
    }
}

#[lumen_bind::op(name = "matchMedia")]
pub fn match_media(ctx: &mut Ctx, query: &str) -> OpResult<DomMediaQueryList> {
    let registry = RealmServices::<MediaQueryRegistry>::current(ctx)
        .ok_or_else(|| OpError::new("InvalidStateError", "CSSOM is not installed"))?;
    let realm = registry
        .realm
        .upgrade()
        .ok_or_else(|| OpError::new("InvalidStateError", "document realm was released"))?;
    let environment = realm.session.borrow().media_environment();
    let data = Rc::new(MediaQueryData {
        realm: Rc::downgrade(&realm),
        query: query.trim().to_owned(),
        was_matching: Cell::new(css::media_query_matches(query, environment)),
        target: DomEventTarget::independent(&realm),
        wrapper: RefCell::new(None),
    });
    registry.lists.borrow_mut().push(Rc::downgrade(&data));
    Ok(DomMediaQueryList {
        base: DomEventTarget::from_data(data.target.data_handle()),
        data,
    })
}

/// Install CSS Typed OM value classes and the numeric `CSS` factories.
/// This subset has no document/session dependency, so browser workers use the
/// same installation path as Window realms instead of maintaining a second
/// list of interfaces and unit factories.
pub fn install_typed_numeric(ctx: &mut Ctx) -> OpResult<()> {
    ctx.class_constructor::<DomCssStyleValue>();
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
    ctx.class_constructor::<DomCssNestedDeclarations>();
    ctx.class_constructor::<DomCssFontFaceRule>();
    ctx.class_constructor::<DomCssKeyframesRule>();
    ctx.class_constructor::<DomCssKeyframeRule>();
    ctx.class_constructor::<DomCssImportRule>();
    ctx.class_constructor::<DomCssMediaList>();
    ctx.class_constructor::<DomCssRuleStyle>();
    ctx.class_constructor::<DomCssFontFaceDescriptors>();
    ctx.class_constructor::<DomStyleSheetList>();
    super::style::install_css_property_aliases(ctx)?;
    RealmServices::replace_current(
        ctx,
        MediaQueryRegistry {
            realm: Rc::downgrade(realm),
            lists: RefCell::new(Vec::new()),
            sheets: RefCell::new(HashMap::new()),
            sheet_positions: RefCell::new(HashMap::new()),
            adopted: RefCell::new(Vec::new()),
        },
    );
    install_typed_numeric(ctx)?;
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
    let nested_declarations_constructor = ctx.class_constructor::<DomCssNestedDeclarations>();
    crate::install_interface(
        ctx,
        &global,
        "CSSNestedDeclarations",
        nested_declarations_constructor,
    )
    .map_err(|_| OpError::new("Error", "CSSNestedDeclarations installation failed"))?;
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
    let environment = realm.session.borrow().media_environment();
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
                    let target = DomEventTarget::from_data(data.target.data_handle());
                    let _ = crate::events::dispatch_event(ctx, This(wrapper), event);
                }
            }
        }
    }
    *registry.lists.borrow_mut() = live;
    Ok(())
}

#[lumen_bind::class(name = "CSSStyleSheet", hint(js(webidl)))]
pub struct DomCssStyleSheet {
    realm: Rc<DomRealm>,
    source: SheetSource,
    owner: Value,
    owner_rule: Value,
    href_value: Option<String>,
    rule_list_wrapper: RefCell<Option<WeakValue>>,
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
struct DomCssomCollectionIterator {
    collection: Value,
    index: Cell<usize>,
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

#[lumen_bind::class(name = "CSSNestedDeclarations", extends = DomCssRule, hint(js(webidl)))]
pub struct DomCssNestedDeclarations {
    base: DomCssRule,
}

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

#[lumen_bind::class(name = "MediaList", hint(js(webidl)))]
pub struct DomCssMediaList {
    realm: Rc<DomRealm>,
    source: SheetSource,
    index: Rc<RulePosition>,
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

/// Web IDL's LegacyNullToEmptyString conversion, expressed as one typed
/// binding parameter so every descriptor setter shares the same conversion.
struct CssDescriptorString(String);

impl<'a> lumen_bind::FromArg<'a, lumen::embed::JsHost> for CssDescriptorString {
    fn from_arg(
        cx: &'a <lumen::embed::JsHost as lumen_bind::Host>::Cx<'_>,
        value: &'a Value,
        at: lumen_bind::Slot,
    ) -> Result<Self, Value> {
        if matches!(value, Value::Null) {
            Ok(Self(String::new()))
        } else {
            <String as lumen_bind::FromArg<'a, lumen::embed::JsHost>>::from_arg(cx, value, at)
                .map(Self)
        }
    }
}

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
    fn set_src(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("src", &value.0, None)
    }
    #[getter(name = "fontFamily")]
    fn font_family(&self) -> OpResult<String> {
        self.base.get_property_value("font-family")
    }
    #[setter(name = "fontFamily", coerce)]
    fn set_font_family(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("font-family", &value.0, None)
    }
    #[getter(name = "font-family")]
    fn font_family_css(&self) -> OpResult<String> {
        self.base.get_property_value("font-family")
    }
    #[setter(name = "font-family", coerce)]
    fn set_font_family_css(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("font-family", &value.0, None)
    }
    #[getter(name = "fontStyle")]
    fn font_style(&self) -> OpResult<String> {
        self.base.get_property_value("font-style")
    }
    #[setter(name = "fontStyle", coerce)]
    fn set_font_style(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("font-style", &value.0, None)
    }
    #[getter(name = "font-style")]
    fn font_style_css(&self) -> OpResult<String> {
        self.base.get_property_value("font-style")
    }
    #[setter(name = "font-style", coerce)]
    fn set_font_style_css(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("font-style", &value.0, None)
    }
    #[getter(name = "fontWeight")]
    fn font_weight(&self) -> OpResult<String> {
        self.base.get_property_value("font-weight")
    }
    #[setter(name = "fontWeight", coerce)]
    fn set_font_weight(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("font-weight", &value.0, None)
    }
    #[getter(name = "font-weight")]
    fn font_weight_css(&self) -> OpResult<String> {
        self.base.get_property_value("font-weight")
    }
    #[setter(name = "font-weight", coerce)]
    fn set_font_weight_css(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("font-weight", &value.0, None)
    }
    #[getter(name = "fontStretch")]
    fn font_stretch(&self) -> OpResult<String> {
        self.base.get_property_value("font-stretch")
    }
    #[setter(name = "fontStretch", coerce)]
    fn set_font_stretch(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("font-stretch", &value.0, None)
    }
    #[getter(name = "font-stretch")]
    fn font_stretch_css(&self) -> OpResult<String> {
        self.base.get_property_value("font-stretch")
    }
    #[setter(name = "font-stretch", coerce)]
    fn set_font_stretch_css(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("font-stretch", &value.0, None)
    }
    #[getter(name = "fontWidth")]
    fn font_width(&self) -> OpResult<String> {
        self.base.get_property_value("font-width")
    }
    #[setter(name = "fontWidth", coerce)]
    fn set_font_width(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("font-width", &value.0, None)
    }
    #[getter(name = "font-width")]
    fn font_width_css(&self) -> OpResult<String> {
        self.base.get_property_value("font-width")
    }
    #[setter(name = "font-width", coerce)]
    fn set_font_width_css(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("font-width", &value.0, None)
    }
    #[getter(name = "unicodeRange")]
    fn unicode_range(&self) -> OpResult<String> {
        self.base.get_property_value("unicode-range")
    }
    #[setter(name = "unicodeRange", coerce)]
    fn set_unicode_range(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("unicode-range", &value.0, None)
    }
    #[getter(name = "unicode-range")]
    fn unicode_range_css(&self) -> OpResult<String> {
        self.base.get_property_value("unicode-range")
    }
    #[setter(name = "unicode-range", coerce)]
    fn set_unicode_range_css(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("unicode-range", &value.0, None)
    }
    #[getter(name = "fontDisplay")]
    fn font_display(&self) -> OpResult<String> {
        self.base.get_property_value("font-display")
    }
    #[setter(name = "fontDisplay", coerce)]
    fn set_font_display(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("font-display", &value.0, None)
    }
    #[getter(name = "font-display")]
    fn font_display_css(&self) -> OpResult<String> {
        self.base.get_property_value("font-display")
    }
    #[setter(name = "font-display", coerce)]
    fn set_font_display_css(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("font-display", &value.0, None)
    }
    #[getter(name = "fontFeatureSettings")]
    fn font_feature_settings(&self) -> OpResult<String> {
        self.base.get_property_value("font-feature-settings")
    }
    #[setter(name = "fontFeatureSettings", coerce)]
    fn set_font_feature_settings(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base
            .set_property("font-feature-settings", &value.0, None)
    }
    #[getter(name = "font-feature-settings")]
    fn font_feature_settings_css(&self) -> OpResult<String> {
        self.base.get_property_value("font-feature-settings")
    }
    #[setter(name = "font-feature-settings", coerce)]
    fn set_font_feature_settings_css(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base
            .set_property("font-feature-settings", &value.0, None)
    }
    #[getter(name = "fontVariationSettings")]
    fn font_variation_settings(&self) -> OpResult<String> {
        self.base.get_property_value("font-variation-settings")
    }
    #[setter(name = "fontVariationSettings", coerce)]
    fn set_font_variation_settings(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base
            .set_property("font-variation-settings", &value.0, None)
    }
    #[getter(name = "font-variation-settings")]
    fn font_variation_settings_css(&self) -> OpResult<String> {
        self.base.get_property_value("font-variation-settings")
    }
    #[setter(name = "font-variation-settings", coerce)]
    fn set_font_variation_settings_css(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base
            .set_property("font-variation-settings", &value.0, None)
    }
    #[getter(name = "sizeAdjust")]
    fn size_adjust(&self) -> OpResult<String> {
        self.base.get_property_value("size-adjust")
    }
    #[setter(name = "sizeAdjust", coerce)]
    fn set_size_adjust(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("size-adjust", &value.0, None)
    }
    #[getter(name = "size-adjust")]
    fn size_adjust_css(&self) -> OpResult<String> {
        self.base.get_property_value("size-adjust")
    }
    #[setter(name = "size-adjust", coerce)]
    fn set_size_adjust_css(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("size-adjust", &value.0, None)
    }
    #[getter(name = "ascentOverride")]
    fn ascent_override(&self) -> OpResult<String> {
        self.base.get_property_value("ascent-override")
    }
    #[setter(name = "ascentOverride", coerce)]
    fn set_ascent_override(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("ascent-override", &value.0, None)
    }
    #[getter(name = "ascent-override")]
    fn ascent_override_css(&self) -> OpResult<String> {
        self.base.get_property_value("ascent-override")
    }
    #[setter(name = "ascent-override", coerce)]
    fn set_ascent_override_css(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("ascent-override", &value.0, None)
    }
    #[getter(name = "descentOverride")]
    fn descent_override(&self) -> OpResult<String> {
        self.base.get_property_value("descent-override")
    }
    #[setter(name = "descentOverride", coerce)]
    fn set_descent_override(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("descent-override", &value.0, None)
    }
    #[getter(name = "descent-override")]
    fn descent_override_css(&self) -> OpResult<String> {
        self.base.get_property_value("descent-override")
    }
    #[setter(name = "descent-override", coerce)]
    fn set_descent_override_css(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("descent-override", &value.0, None)
    }
    #[getter(name = "lineGapOverride")]
    fn line_gap_override(&self) -> OpResult<String> {
        self.base.get_property_value("line-gap-override")
    }
    #[setter(name = "lineGapOverride", coerce)]
    fn set_line_gap_override(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("line-gap-override", &value.0, None)
    }
    #[getter(name = "line-gap-override")]
    fn line_gap_override_css(&self) -> OpResult<String> {
        self.base.get_property_value("line-gap-override")
    }
    #[setter(name = "line-gap-override", coerce)]
    fn set_line_gap_override_css(&self, value: CssDescriptorString) -> OpResult<()> {
        self.base.set_property("line-gap-override", &value.0, None)
    }
}

fn sheet_text(realm: &DomRealm, node: NodeId) -> OpResult<String> {
    fn append(
        document: &lumen_html::Document,
        node: NodeId,
        output: &mut String,
    ) -> Result<(), Error> {
        let mut child = document.first_child(node)?;
        while let Some(id) = child {
            match document.kind(id)? {
                NodeKind::Text(text) | NodeKind::CData(text) => output.push_str(text),
                _ => append(document, id, output)?,
            }
            child = document.next_sibling(id)?;
        }
        Ok(())
    }
    let session = realm.session.borrow();
    let mut text = String::new();
    append(session.document(), node, &mut text).map_err(dom_error)?;
    Ok(text)
}

fn write_sheet_text(realm: &DomRealm, node: NodeId, text: &str) -> OpResult<()> {
    let mut session = realm.session.borrow_mut();
    let document = session.document_mut();
    let mut old = Vec::new();
    let mut child = document.first_child(node).map_err(dom_error)?;
    while let Some(id) = child {
        old.push(id);
        child = document.next_sibling(id).map_err(dom_error)?;
    }
    let existing_text = old
        .iter()
        .copied()
        .find(|id| matches!(document.kind(*id), Ok(NodeKind::Text(_))));
    let text_node = if let Some(existing_text) = existing_text {
        existing_text
    } else {
        document
            .create(NodeKind::Text(String::new()))
            .map_err(dom_error)?
    };
    document.replace_data(text_node, text).map_err(dom_error)?;
    document
        .replace_children_many(node, &[text_node])
        .map_err(dom_error)?;
    for removed in old {
        if removed != text_node {
            document.destroy_subtree(removed).map_err(dom_error)?;
        }
    }
    Ok(())
}

fn source_text(realm: &DomRealm, source: &SheetSource) -> OpResult<String> {
    match source {
        SheetSource::Element { node, .. } => sheet_text(realm, *node),
        SheetSource::Rule { source, position } => {
            let text = source_text(realm, source)?;
            position.owner.synchronize(&text)?;
            Ok(position.detached.borrow().clone().unwrap_or(text))
        }
        SheetSource::Constructed(data) => Ok(data.text.borrow().clone()),
        SheetSource::Imported(data) => Ok(realm
            .session
            .borrow()
            .imported_stylesheet_text(data.owner, &data.path)
            .unwrap_or_else(|| data.text.borrow().clone())),
    }
}

fn source_sheet(realm: &DomRealm, source: &SheetSource) -> OpResult<CssStyleSheetText> {
    CssStyleSheetText::parse(&source_text(realm, source)?).map_err(css_error)
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
    sheet
        .set_keyframe_declarations(rule_index.get(), frame.get(), declarations)
        .map_err(css_error)?;
    write_source_text(realm, source, sheet.css_text())?;
    frame.owner.remember(&keyframe_text(
        &sheet.keyframes_rule(rule_index.get()).map_err(css_error)?,
    ));
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
    let adopted = registry.borrowed_adoptions();
    let mut session = realm.session.borrow_mut();
    for (scope, sheets) in adopted {
        session
            .set_adopted_stylesheets(scope, sheets)
            .map_err(|error| {
                OpError::new(
                    "SyntaxError",
                    format!("adopted stylesheet failed: {error:?}"),
                )
            })?;
    }
    Ok(())
}

pub fn adopted_stylesheets(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    scope: Option<NodeId>,
) -> OpResult<Value> {
    let registry = registry_for_realm(ctx, realm)
        .ok_or_else(|| OpError::new("InvalidStateError", "CSSOM is not installed"))?;
    let values = registry
        .adopted
        .borrow()
        .iter()
        .find(|(candidate, _)| *candidate == scope)
        .map(|(_, sheets)| {
            sheets
                .iter()
                .map(|sheet| sheet.value.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let global = ctx.global_object();
    let constructor = ctx
        .get_member(&global, "Array")
        .map_err(|_| OpError::new("Error", "Array constructor is unavailable"))?;
    let constructor = JsFunction::from_value(constructor)
        .ok_or_else(|| OpError::new("Error", "Array is not callable"))?;
    let array = constructor.call(ctx, Value::Undefined, &[Value::Num(values.len() as f64)])?;
    for (index, value) in values.into_iter().enumerate() {
        ctx.set_member(&array, &index.to_string(), value)
            .map_err(|_| OpError::new("TypeError", "could not create adopted stylesheet array"))?;
    }
    Ok(array)
}

pub fn set_adopted_stylesheets(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    scope: Option<NodeId>,
    value: Value,
) -> OpResult<()> {
    if !ctx.is_array_value(&value).map_err(OpError::thrown)? {
        return Err(OpError::new(
            "TypeError",
            "adoptedStyleSheets must be an array",
        ));
    }
    let Value::Num(length) = ctx
        .get_member(&value, "length")
        .map_err(|_| OpError::new("TypeError", "could not read adoptedStyleSheets length"))?
    else {
        return Err(OpError::new(
            "TypeError",
            "invalid adoptedStyleSheets length",
        ));
    };
    if !length.is_finite() || length < 0.0 || length > 4096.0 || length.fract() != 0.0 {
        return Err(OpError::new(
            "RangeError",
            "invalid adoptedStyleSheets length",
        ));
    }
    let registry = registry_for_realm(ctx, realm)
        .ok_or_else(|| OpError::new("InvalidStateError", "CSSOM is not installed"))?;
    let mut sheets = Vec::with_capacity(length as usize);
    for index in 0..length as usize {
        let item = ctx
            .get_member(&value, &index.to_string())
            .map_err(|_| OpError::new("TypeError", "could not read adopted stylesheet"))?;
        let native = ctx
            .instance_data::<DomCssStyleSheet>(&item)
            .ok_or_else(|| {
                OpError::new(
                    "TypeError",
                    "adoptedStyleSheets entries must be CSSStyleSheet objects",
                )
            })?;
        let source = native.borrow().source.clone();
        let SheetSource::Constructed(data) = source else {
            return Err(OpError::new(
                "NotAllowedError",
                "only constructed stylesheets can be adopted",
            ));
        };
        if data
            .realm
            .upgrade()
            .is_none_or(|owner| !Rc::ptr_eq(&owner, realm))
        {
            return Err(OpError::new(
                "NotAllowedError",
                "stylesheet was constructed in another document",
            ));
        }
        if sheets
            .iter()
            .any(|sheet: &AdoptedSheet| Rc::ptr_eq(&sheet.data, &data))
        {
            return Err(OpError::new(
                "NotAllowedError",
                "a stylesheet cannot be adopted twice",
            ));
        }
        sheets.push(AdoptedSheet { data, value: item });
    }
    let previous = {
        let mut adopted = registry.adopted.borrow_mut();
        if let Some((_, current)) = adopted
            .iter_mut()
            .find(|(candidate, _)| *candidate == scope)
        {
            core::mem::replace(current, sheets)
        } else {
            adopted.push((scope, sheets));
            Vec::new()
        }
    };
    if let Err(error) = sync_adopted(&registry, realm) {
        let mut adopted = registry.adopted.borrow_mut();
        if let Some((_, current)) = adopted
            .iter_mut()
            .find(|(candidate, _)| *candidate == scope)
        {
            *current = previous;
        }
        return Err(error);
    }
    Ok(())
}

impl MediaQueryRegistry {
    fn borrowed_adoptions(&self) -> Vec<(Option<NodeId>, Vec<String>)> {
        self.adopted
            .borrow()
            .iter()
            .map(|(scope, sheets)| {
                (
                    *scope,
                    sheets
                        .iter()
                        .map(|sheet| sheet.data.text.borrow().clone())
                        .collect(),
                )
            })
            .collect()
    }
}

fn registry_for_realm(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> Option<Rc<MediaQueryRegistry>> {
    let registry = RealmServices::<MediaQueryRegistry>::current(ctx)?;
    registry
        .realm
        .upgrade()
        .is_some_and(|owner| Rc::ptr_eq(&owner, realm))
        .then_some(registry)
}

fn write_source_text(realm: &DomRealm, source: &SheetSource, text: &str) -> OpResult<()> {
    match source {
        SheetSource::Element { node, .. } => write_sheet_text(realm, *node, text),
        SheetSource::Rule { source, position } => {
            if position.detached.borrow().is_some() {
                *position.detached.borrow_mut() = Some(text.to_owned());
                return Ok(());
            }
            write_source_text(realm, source, text)?;
            position.owner.remember(text);
            Ok(())
        }
        SheetSource::Constructed(data) => {
            *data.text.borrow_mut() = text.to_owned();
            if let Some(registry) = data.registry.upgrade() {
                sync_adopted(&registry, realm)?;
            }
            Ok(())
        }
        SheetSource::Imported(data) => {
            realm
                .session
                .borrow_mut()
                .replace_imported_stylesheet_text(data.owner, &data.path, text)
                .map_err(|_| {
                    OpError::new("SyntaxError", "imported stylesheet replacement failed")
                })?;
            *data.text.borrow_mut() = text.to_owned();
            Ok(())
        }
    }
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
    if !matches!(realm.session.borrow().document().kind(node), Ok(NodeKind::Element { name, .. }) if name == "style")
    {
        return Err(OpError::new(
            "TypeError",
            "sheet is only available on a style element",
        ));
    }
    if let Some(registry) = registry_for_realm(ctx, realm) {
        if let Some(sheet) = registry
            .sheets
            .borrow()
            .get(&node)
            .and_then(WeakValue::upgrade)
        {
            return Ok(sheet);
        }
    }
    let positions = if let Some(registry) = registry_for_realm(ctx, realm) {
        let mut positions = registry.sheet_positions.borrow_mut();
        positions.retain(|_, position| position.strong_count() != 0);
        let state = positions
            .get(&node)
            .and_then(Weak::upgrade)
            .unwrap_or_default();
        positions.insert(node, Rc::downgrade(&state));
        state
    } else {
        Rc::default()
    };
    let sheet = ctx.new_instance(DomCssStyleSheet {
        realm: realm.clone(),
        source: SheetSource::Element { node, positions },
        owner,
        owner_rule: Value::Null,
        href_value: None,
        rule_list_wrapper: RefCell::new(None),
    });
    if let Some(registry) = registry_for_realm(ctx, realm) {
        if let Some(weak) = ctx.weak_value(&sheet) {
            registry.sheets.borrow_mut().insert(node, weak);
        }
    }
    Ok(sheet)
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

fn style_elements(realm: &DomRealm, root: NodeId) -> Vec<NodeId> {
    let session = realm.session.borrow();
    let document = session.document();
    let mut result = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        let mut children = Vec::new();
        let mut child = document.first_child(node).ok().flatten();
        while let Some(id) = child {
            children.push(id);
            child = document.next_sibling(id).ok().flatten();
        }
        pending.extend(children.iter().rev().copied());
        if matches!(document.kind(node), Ok(NodeKind::Element { name, .. }) if name == "style") {
            result.push(node);
        }
    }
    result
}

#[lumen_bind::methods]
impl DomCssStyleSheet {
    #[constructor]
    fn new(ctx: &mut Ctx) -> OpResult<Self> {
        let registry = RealmServices::<MediaQueryRegistry>::current(ctx)
            .ok_or_else(|| OpError::new("InvalidStateError", "CSSOM is not installed"))?;
        let realm = registry
            .realm
            .upgrade()
            .ok_or_else(|| OpError::new("InvalidStateError", "document realm was released"))?;
        Ok(Self {
            realm: realm.clone(),
            source: SheetSource::Constructed(Rc::new(ConstructedSheetData {
                realm: Rc::downgrade(&realm),
                registry: Rc::downgrade(&registry),
                text: RefCell::new(String::new()),
                positions: Rc::default(),
            })),
            owner: Value::Null,
            owner_rule: Value::Null,
            href_value: None,
            rule_list_wrapper: RefCell::new(None),
        })
    }

    #[getter]
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
            rule_path: Vec::new(),
            owner: this.0.clone(),
            sheet_owner: this.0,
            keyframes_index: None,
        });
        *self.rule_list_wrapper.borrow_mut() = ctx.weak_value(&wrapper);
        Ok(wrapper)
    }

    #[getter(name = "ownerNode")]
    fn owner_node(&self) -> Value {
        self.owner.clone()
    }

    #[getter(name = "parentStyleSheet")]
    fn parent_style_sheet(&self) -> Value {
        Value::Null
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

    #[method(coerce)]
    fn insert_rule(&self, rule: &str, index: Option<u32>) -> OpResult<u32> {
        let mut sheet = source_sheet(&self.realm, &self.source)?;
        self.source.positions().synchronize(sheet.css_text())?;
        let position = index.unwrap_or(0) as usize;
        sheet.insert_rule(rule, position).map_err(css_error)?;
        write_source_text(&self.realm, &self.source, sheet.css_text())?;
        self.source.positions().inserted(position, sheet.css_text());
        Ok(position as u32)
    }

    fn delete_rule(&self, index: u32) -> OpResult<()> {
        let index = index as usize;
        let mut sheet = source_sheet(&self.realm, &self.source)?;
        let positions = self.source.positions();
        positions.synchronize(sheet.css_text())?;
        let rules = sheet.css_rules().map_err(css_error)?;
        let removed = rules
            .get(index)
            .ok_or_else(|| OpError::new("IndexSizeError", "CSS rule index out of range"))?
            .css_text
            .clone();
        sheet.delete_rule(index).map_err(css_error)?;
        write_source_text(&self.realm, &self.source, sheet.css_text())?;
        positions.deleted(index, &removed, sheet.css_text());
        Ok(())
    }

    #[method(coerce)]
    fn replace_sync(&self, text: &str) -> OpResult<()> {
        if !matches!(&self.source, SheetSource::Constructed(_)) {
            return Err(OpError::new(
                "NotAllowedError",
                "replaceSync requires a constructed stylesheet",
            ));
        }
        let mut sheet = source_sheet(&self.realm, &self.source)?;
        let positions = self.source.positions();
        positions.synchronize(sheet.css_text())?;
        sheet.replace(text).map_err(css_error)?;
        write_source_text(&self.realm, &self.source, sheet.css_text())?;
        positions.replaced(sheet.css_text())
    }

    #[method(coerce)]
    fn replace(&self, this: This<Value>, text: &str) -> Promise<Value> {
        Promise::ready(self.replace_sync(text).map(|()| this.0))
    }
}

#[lumen_bind::methods]
impl DomCssRuleList {
    #[proto(len)]
    fn length(&self) -> OpResult<usize> {
        let sheet = source_sheet(&self.realm, &self.source)?;
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
            .rule_list(&rule_path_indices(&self.rule_path))
            .map_err(css_error)?;
        let positions = positions_for_rule_list(&self.source, &self.rule_path);
        let position_text = if self.rule_path.is_empty() {
            sheet.css_text().to_owned()
        } else {
            rules_text(&rules)
        };
        positions.synchronize(&position_text)?;
        Ok(rules.len())
    }

    #[proto(getitem)]
    fn indexed(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        let sheet = source_sheet(&self.realm, &self.source)?;
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
                source: self.source.clone(),
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
            .rule_list(&rule_path_indices(&self.rule_path))
            .map_err(css_error)?;
        let Some(rule) = rules.get(index) else {
            return Ok(Value::Undefined);
        };
        let positions = positions_for_rule_list(&self.source, &self.rule_path);
        let position_text = if self.rule_path.is_empty() {
            sheet.css_text().to_owned()
        } else {
            rules_text(&rules)
        };
        positions.synchronize(&position_text)?;
        let position = positions.at(index);
        if let Some(wrapper) = position
            .wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
        {
            return Ok(wrapper);
        }
        let mut path = self.rule_path.clone();
        path.push(position.clone());
        let base = DomCssRule {
            realm: self.realm.clone(),
            source: self.source.clone(),
            index: position.clone(),
            path,
            keyframe_index: None,
            owner: self.owner.clone(),
            sheet_owner: self.sheet_owner.clone(),
            style_wrapper: RefCell::new(None),
            rule_list_wrapper: RefCell::new(None),
        };
        let wrapper = if rule.nested_declarations {
            ctx.new_instance(DomCssNestedDeclarations { base })
        } else if rule.keyframes.is_some() {
            ctx.new_instance(DomCssKeyframesRule { base })
        } else if starts_css_at_rule(&rule.css_text, "@import") {
            ctx.new_instance(DomCssImportRule {
                base,
                media_wrapper: RefCell::new(None),
                stylesheet_wrapper: RefCell::new(None),
            })
        } else if rule.font_face {
            ctx.new_instance(DomCssFontFaceRule { base })
        } else if rule.selector_text.is_some() {
            ctx.new_instance(DomCssStyleRule {
                base: DomCssGroupingRule { base },
            })
        } else if starts_css_at_rule(&rule.css_text, "@media") {
            ctx.new_instance(DomCssMediaRule {
                base: DomCssConditionRule {
                    base: DomCssGroupingRule { base },
                },
            })
        } else if starts_css_at_rule(&rule.css_text, "@supports") {
            ctx.new_instance(DomCssSupportsRule {
                base: DomCssConditionRule {
                    base: DomCssGroupingRule { base },
                },
            })
        } else if !rule.nested.is_empty() {
            ctx.new_instance(DomCssGroupingRule { base })
        } else {
            ctx.new_instance(base)
        };
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
    fn length(&self) -> usize {
        style_elements(&self.realm, self.document).len()
    }

    #[proto(getitem)]
    fn indexed(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        let Some(node) = style_elements(&self.realm, self.document)
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
        if let Some(frame) = self.keyframe_index.as_ref() {
            let _ = source_keyframe(&self.realm, &self.source, &self.index, frame)?;
        }
        rule_from_path(&source_sheet(&self.realm, &self.source)?, &self.path)
    }

    fn condition_text(&self, at_rule: &str) -> OpResult<String> {
        let rule = self.current_rule()?;
        let open = rule
            .css_text
            .find('{')
            .ok_or_else(|| OpError::new("InvalidStateError", "grouping rule has no block"))?;
        let prelude = rule.css_text[..open].trim();
        let condition = prelude
            .get(..at_rule.len())
            .filter(|name| name.eq_ignore_ascii_case(at_rule))
            .map(|_| &prelude[at_rule.len()..])
            .unwrap_or_default();
        Ok(condition.trim().to_owned())
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
        let mut sheet = source_sheet(&self.realm, &self.source)?;
        let parent_path = rule_path_indices(&self.path);
        let current = sheet.rule_list(&parent_path).map_err(css_error)?;
        let positions = positions_for_rule_list(&self.source, &self.path);
        positions.synchronize(&rules_text(&current))?;
        let index = index.unwrap_or(0) as usize;
        if index > current.len() {
            return Err(crate::error_reporting::dom_exception(
                ctx,
                "IndexSizeError",
                "CSS rule index out of range",
            ));
        }
        if starts_css_at_rule(rule, "@import") || starts_css_at_rule(rule, "@namespace") {
            return Err(crate::error_reporting::dom_exception(
                ctx,
                "HierarchyRequestError",
                "this rule type cannot be inserted into a grouping rule",
            ));
        }
        sheet
            .insert_nested_rule(&parent_path, rule, index)
            .map_err(|error| css_rule_dom_exception(ctx, error))?;
        write_source_text(&self.realm, &self.source, sheet.css_text())?;
        let updated = source_sheet(&self.realm, &self.source)?;
        let children = updated.rule_list(&parent_path).map_err(css_error)?;
        positions.inserted(index, &rules_text(&children));
        remember_rule_path(&self.source, &updated, &self.path)?;
        Ok(index as u32)
    }

    fn delete_nested_rule(&self, ctx: &mut Ctx, index: u32) -> OpResult<()> {
        let index = index as usize;
        let mut sheet = source_sheet(&self.realm, &self.source)?;
        let parent_path = rule_path_indices(&self.path);
        let current = sheet.rule_list(&parent_path).map_err(css_error)?;
        if index >= current.len() {
            return Err(crate::error_reporting::dom_exception(
                ctx,
                "IndexSizeError",
                "CSS rule index out of range",
            ));
        }
        let positions = positions_for_rule_list(&self.source, &self.path);
        positions.synchronize(&rules_text(&current))?;
        let removed = sheet
            .delete_nested_rule(&parent_path, index)
            .map_err(|error| css_rule_dom_exception(ctx, error))?;
        write_source_text(&self.realm, &self.source, sheet.css_text())?;
        let updated = source_sheet(&self.realm, &self.source)?;
        let children = updated.rule_list(&parent_path).map_err(css_error)?;
        positions.deleted(index, &removed, &rules_text(&children));
        remember_rule_path(&self.source, &updated, &self.path)?;
        Ok(())
    }

    fn set_selector_text(&self, value: &str) -> OpResult<()> {
        let mut sheet = source_sheet(&self.realm, &self.source)?;
        if sheet
            .set_rule_selector(&rule_path_indices(&self.path), value)
            .is_err()
        {
            // CSSStyleRule.selectorText ignores selectors that cannot be
            // parsed in the rule's actual (possibly nested) context.
            return Ok(());
        }
        write_source_text(&self.realm, &self.source, sheet.css_text())?;
        let updated = source_sheet(&self.realm, &self.source)?;
        remember_rule_path(&self.source, &updated, &self.path)
    }
}

#[lumen_bind::methods]
impl DomCssRule {
    #[getter(name = "type")]
    fn rule_type(&self) -> OpResult<u16> {
        if self.keyframe_index.is_some() {
            return Ok(8);
        }
        let rule = self.current_rule()?;
        Ok(if rule.keyframes.is_some() {
            7
        } else if rule.font_face {
            5
        } else if rule.selector_text.is_some() {
            1
        } else if starts_css_at_rule(&rule.css_text, "@media") {
            4
        } else if starts_css_at_rule(&rule.css_text, "@supports") {
            12
        } else if starts_css_at_rule(&rule.css_text, "@import") {
            3
        } else {
            0
        })
    }

    #[getter(name = "parentStyleSheet")]
    fn parent_style_sheet(&self) -> OpResult<Value> {
        let _ = source_text(&self.realm, &self.source)?;
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
        if let Some(frame) = self.keyframe_index.as_ref() {
            let _ = source_keyframe(&self.realm, &self.source, &self.index, frame)?;
            Ok(if frame.detached.borrow().is_some() {
                Value::Null
            } else {
                self.owner.clone()
            })
        } else if self.path.len() > 1 && self.index.detached.borrow().is_none() {
            Ok(
                if self
                    .path
                    .iter()
                    .any(|position| position.detached.borrow().is_some())
                {
                    Value::Null
                } else {
                    self.owner.clone()
                },
            )
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
        } else if rule.selector_text.is_some() || !rule.nested.is_empty() {
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

#[lumen_bind::methods]
impl DomCssFontFaceRule {
    #[getter]
    fn style(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        self.base.style(ctx, this)
    }
    #[setter(coerce)]
    fn set_style(&self, value: &str) -> OpResult<()> {
        let mut sheet = source_sheet(&self.base.realm, &self.base.source)?;
        sheet
            .set_rule_style(&rule_path_indices(&self.base.path), value)
            .map_err(css_error)?;
        write_source_text(&self.base.realm, &self.base.source, sheet.css_text())?;
        let updated = source_sheet(&self.base.realm, &self.base.source)?;
        remember_rule_path(&self.base.source, &updated, &self.base.path)
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
        let rule = self.base.base.current_rule()?;
        let at_rule = if rule.css_text.to_ascii_lowercase().starts_with("@media") {
            "@media"
        } else {
            "@supports"
        };
        self.base.base.condition_text(at_rule)
    }
}

#[lumen_bind::methods]
impl DomCssMediaRule {}

#[lumen_bind::methods]
impl DomCssSupportsRule {}

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
        let before = sheet
            .keyframes_rule(self.base.index.get())
            .map_err(css_error)?;
        let positions = self.base.index.children();
        positions.synchronize(&keyframe_text(&before))?;
        sheet
            .append_keyframe(self.base.index.get(), rule)
            .map_err(css_error)?;
        write_source_text(&self.base.realm, &self.base.source, sheet.css_text())?;
        positions.inserted(
            before.rules.len(),
            &keyframe_text(
                &sheet
                    .keyframes_rule(self.base.index.get())
                    .map_err(css_error)?,
            ),
        );
        Ok(())
    }

    #[method(coerce)]
    fn delete_rule(&self, key_text: &str) -> OpResult<()> {
        let mut sheet = source_sheet(&self.base.realm, &self.base.source)?;
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
        write_source_text(&self.base.realm, &self.base.source, sheet.css_text())?;
        if let Some(index) = deleted {
            positions.deleted(
                index,
                &before.rules[index].css_text,
                &keyframe_text(
                    &sheet
                        .keyframes_rule(self.base.index.get())
                        .map_err(css_error)?,
                ),
            );
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
        sheet
            .set_keyframe_key_text(self.base.index.get(), frame_index.get(), value)
            .map_err(css_error)?;
        write_source_text(&self.base.realm, &self.base.source, sheet.css_text())?;
        frame_index.owner.remember(&keyframe_text(
            &sheet
                .keyframes_rule(self.base.index.get())
                .map_err(css_error)?,
        ));
        Ok(())
    }

    #[getter]
    fn style(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        self.base.style(ctx, this)
    }
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
    fn media(&self, ctx: &mut Ctx) -> OpResult<Value> {
        if let Some(value) = self
            .media_wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
        {
            return Ok(value);
        }
        let value = ctx.new_instance(DomCssMediaList {
            realm: self.base.realm.clone(),
            source: self.base.source.clone(),
            index: self.base.index.clone(),
        });
        *self.media_wrapper.borrow_mut() = ctx.weak_value(&value);
        Ok(value)
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
        let import = source_import(&self.base.realm, &self.base.source, &self.base.index)?;
        let node = source_dom_node(&self.base.realm, &self.base.source);
        let parent_path = match self.base.source.root() {
            SheetSource::Element { .. } => Vec::new(),
            SheetSource::Imported(data) => data.path.clone(),
            SheetSource::Constructed(_) => return Ok(Value::Null),
            SheetSource::Rule { .. } => unreachable!(),
        };
        let Some((import_index, text, href)) = self
            .base
            .realm
            .session
            .borrow()
            .imported_child_stylesheet(node, &parent_path, &import.url)
        else {
            return Ok(Value::Null);
        };
        let mut path = parent_path;
        path.push(import_index);
        let value = ctx.new_instance(DomCssStyleSheet {
            realm: self.base.realm.clone(),
            source: SheetSource::Imported(Rc::new(ImportedSheetData {
                owner: node,
                path,
                text: RefCell::new(text),
                positions: Rc::default(),
            })),
            owner: Value::Null,
            owner_rule: this.0,
            href_value: Some(href),
            rule_list_wrapper: RefCell::new(None),
        });
        *self.stylesheet_wrapper.borrow_mut() = ctx.weak_value(&value);
        Ok(value)
    }
}

#[lumen_bind::methods]
impl DomCssMediaList {
    #[getter(name = "mediaText")]
    fn media_text(&self) -> OpResult<String> {
        Ok(source_import(&self.realm, &self.source, &self.index)?
            .media
            .map_or_else(String::new, |media| media.to_string()))
    }

    #[setter(name = "mediaText", coerce)]
    fn set_media_text(&self, value: &str) -> OpResult<()> {
        let mut sheet = source_sheet(&self.realm, &self.source)?;
        sheet
            .set_import_media(self.index.get(), value)
            .map_err(css_error)?;
        write_source_text(&self.realm, &self.source, sheet.css_text())
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
    #[getter(name = "parentRule")]
    fn parent_rule(&self) -> Value {
        self.owner.clone()
    }

    #[proto(len)]
    fn length(&self) -> OpResult<usize> {
        Ok(self.property_names()?.len())
    }

    fn item(&self, index: usize) -> OpResult<String> {
        Ok(self
            .property_names()?
            .get(index)
            .cloned()
            .unwrap_or_default())
    }

    #[proto(getitem)]
    fn indexed(&self, index: usize) -> OpResult<Value> {
        Ok(self
            .property_names()?
            .get(index)
            .map_or(Value::Undefined, |name| Value::from_string(name.clone())))
    }

    #[getter(name = "cssText")]
    fn css_text(&self) -> OpResult<String> {
        if let Some(frame_index) = self.keyframe_index.as_ref() {
            return Ok(source_keyframe(&self.realm, &self.source, &self.index, frame_index)?.style);
        }
        let sheet = source_sheet(&self.realm, &self.source)?;
        Ok(sheet
            .rule_at_path(&rule_path_indices(&self.path))
            .map_err(css_error)?
            .style
            .css_text()
            .to_owned())
    }

    #[setter(name = "cssText", coerce)]
    fn set_css_text(&self, value: &str) -> OpResult<()> {
        let mut sheet = source_sheet(&self.realm, &self.source)?;
        if let Some(frame_index) = self.keyframe_index.as_ref() {
            return write_keyframe_style(
                &self.realm,
                &self.source,
                &self.index,
                frame_index,
                value,
            );
        }
        sheet
            .set_rule_style(&rule_path_indices(&self.path), value)
            .map_err(css_error)?;
        write_source_text(&self.realm, &self.source, sheet.css_text())?;
        let updated = source_sheet(&self.realm, &self.source)?;
        remember_rule_path(&self.source, &updated, &self.path)
    }

    fn get_property_value(&self, name: &str) -> OpResult<String> {
        if let Some(frame_index) = self.keyframe_index.as_ref() {
            let frame = source_keyframe(&self.realm, &self.source, &self.index, frame_index)?;
            return Ok(CssDeclaration::parse(&frame.style)
                .get_property_value(name)
                .map_err(css_error)?
                .map_or(String::new(), |(value, _)| {
                    super::style::canonical_content_property(name, value)
                }));
        }
        let sheet = source_sheet(&self.realm, &self.source)?;
        let rule = sheet
            .rule_at_path(&rule_path_indices(&self.path))
            .map_err(css_error)?;
        let name = if rule.font_face && name.eq_ignore_ascii_case("font-stretch") {
            "font-width"
        } else {
            name
        };
        Ok(rule
            .style
            .get_property_value(name)
            .map_err(css_error)?
            .map_or(String::new(), |(value, _)| {
                super::style::canonical_content_property(name, value)
            }))
    }

    fn get_property_priority(&self, name: &str) -> OpResult<String> {
        if let Some(frame_index) = self.keyframe_index.as_ref() {
            let frame = source_keyframe(&self.realm, &self.source, &self.index, frame_index)?;
            return Ok(if CssDeclaration::parse(&frame.style)
                .get_property_value(name)
                .map_err(css_error)?
                .is_some_and(|(_, important)| important)
            {
                "important"
            } else {
                ""
            }
            .into());
        }
        let sheet = source_sheet(&self.realm, &self.source)?;
        Ok(if sheet
            .rule_at_path(&rule_path_indices(&self.path))
            .map_err(css_error)?
            .style
            .get_property_value(name)
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
        let mut sheet = source_sheet(&self.realm, &self.source)?;
        if let Some(frame_index) = self.keyframe_index.as_ref() {
            let frame = source_keyframe(&self.realm, &self.source, &self.index, frame_index)?;
            let mut declaration = CssDeclaration::parse(&frame.style);
            let priority = priority.unwrap_or("");
            if !priority.is_empty() && !priority.eq_ignore_ascii_case("important") {
                return Ok(());
            }
            declaration
                .set_property(name, value, !priority.is_empty())
                .map_err(css_error)?;
            return write_keyframe_style(
                &self.realm,
                &self.source,
                &self.index,
                frame_index,
                declaration.css_text(),
            );
        }
        let rule = sheet
            .rule_at_path(&rule_path_indices(&self.path))
            .map_err(css_error)?;
        let name = if rule.font_face && name.eq_ignore_ascii_case("font-stretch") {
            "font-width"
        } else {
            name
        };
        let priority = priority.unwrap_or("");
        if rule.font_face {
            if !priority.is_empty() {
                return Ok(());
            }
            if !value.is_empty() {
                let candidate = css::set_declaration("", name, value, false).map_err(css_error)?;
                if css::cssom_font_face_declaration_text(&candidate).is_empty() {
                    return Ok(());
                }
            }
        } else if !value.is_empty() && !css::supports_declaration(name, value) {
            return Ok(());
        }
        if !priority.is_empty() && !priority.eq_ignore_ascii_case("important") {
            return Ok(());
        }
        let text = css::set_declaration(rule.style.css_text(), name, value, !priority.is_empty())
            .map_err(css_error)?;
        sheet
            .set_rule_style(&rule_path_indices(&self.path), &text)
            .map_err(css_error)?;
        write_source_text(&self.realm, &self.source, sheet.css_text())?;
        let updated = source_sheet(&self.realm, &self.source)?;
        remember_rule_path(&self.source, &updated, &self.path)
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
    fn property_names(&self) -> OpResult<Vec<String>> {
        let text = self.css_text()?;
        // Every accepted declaration is already canonicalized by the shared
        // parser; quoted semicolons must not be treated as separators.
        css::cssom_declaration_names(&text).map_err(css_error)
    }
}

/// Validate that a proposed rule parses as a stylesheet before callers mutate
/// the owning style element. Returning an error leaves the original unchanged.
pub fn validate_stylesheet(text: &str) -> Result<(), css::CssError> {
    css::parse(text).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let child = css::StylesheetSource {
                url: Arc::from("https://example.test/theme.css"),
                text: Arc::from(".before { opacity: 0.25 }"),
                imports: Vec::new(),
            };
            let root = css::StylesheetSource {
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
    fn stylesheet_statement_rules_can_precede_blocks_and_be_removed() {
        let mut sheet = CssStyleSheetText::parse("").unwrap();
        sheet
            .insert_rule(
                "@import url('data:text/css,div { background: red !important }');",
                0,
            )
            .unwrap();
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
    fn keyframes_and_import_cssom_edit_the_live_stylesheet_source() {
        let mut engine = lumen::Engine::new();
        crate::install(
            engine.ctx(),
            "<style>@import url('theme.css') layer(theme) supports(display: grid) screen; @keyframes pulse { from { opacity: 0 } 50% { opacity: .5 } to { opacity: 1 } }</style>",
            64,
        )
        .unwrap();
        let script = r#"
            const sheet=document.querySelector('style').sheet;
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
            const text=document.querySelector('style').textContent;
            if(!text.includes('screen, print') || !text.includes('40%, 60%')
                || !text.includes('75%')) throw new Error('live source mutation');
        "#;
        if let Err(error) = engine.eval_value(script).unwrap() {
            let message = engine
                .ctx()
                .coerce_string(&error)
                .map(|value| value.to_string())
                .unwrap_or_else(|_| "unprintable exception".into());
            panic!("keyframes/import CSSOM script failed: {message}");
        }
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
        assert!(top_level_rule_ranges("color: red").is_err());
        assert!(rules_from_text(".broken { color: red").is_err());
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
        assert!(sheet.insert_rule(".broken { width: nope", 1).is_err());
        assert_eq!(sheet.css_text(), before);
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
        let environment = MediaEnvironment {
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
            .set_media_environment(MediaEnvironment {
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
            .set_media_environment(MediaEnvironment {
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
                    const adoptedDocument = new DOMParser().parseFromString('<main></main>', 'text/html');
                    adoptedDocument.querySelector('main').appendChild(adoptedDocument.adoptNode(typedElement));
                    const adoptedDurationValue = computedMap.get('transition-duration');
                    check('computed map follows adopted element',
                        adoptedDurationValue instanceof CSSUnitValue &&
                        adoptedDurationValue.unit === 's' && adoptedDurationValue.value === 0.5 &&
                        adoptedDocument.querySelector('main').firstChild === typedElement);

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
            .set_media_environment(MediaEnvironment {
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
                    .set_media_environment(MediaEnvironment {
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
            .set_media_environment(MediaEnvironment {
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
            .set_media_environment(MediaEnvironment {
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
