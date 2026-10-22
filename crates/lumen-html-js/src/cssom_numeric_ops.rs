//! CSSNumericValue arithmetic, equality and unit conversion (Typed OM §"Numeric Values").
use super::*;
use css::typed_numeric::{NumericExpression as Expression, NumericType, NumericUnit};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Arithmetic {
    Add,
    Sub,
    Mul,
    Div,
    Min,
    Max,
}

#[derive(Clone)]
struct SumItem {
    value: f64,
    units: Vec<(NumericUnit, i32)>,
}

fn numberish_value(ctx: &mut Ctx, numberish: CssNumberish) -> OpResult<Value> {
    let CssNumberish { value, expression } = numberish;
    if !matches!(value, Value::Undefined) {
        return Ok(value);
    }
    match expression {
        Expression::Value(numeric) => Ok(unit_value(ctx, numeric.value, numeric.unit)),
        _ => Err(OpError::type_error("invalid CSSNumberish conversion")),
    }
}

fn unit_parts(ctx: &mut Ctx, value: &Value) -> OpResult<Option<(f64, NumericUnit)>> {
    ctx.with_instance::<DomCssNumericValue, _>(value, |numeric| {
        numeric
            .expression
            .is_none()
            .then(|| (numeric.value.get(), numeric.unit))
    })
}

fn math_children(ctx: &mut Ctx, value: &Value) -> OpResult<Vec<Value>> {
    let values = math_values(ctx, value)?;
    let length = ctx.with_instance::<DomCssNumericArray, _>(&values, |array| array.length)?;
    let mut children = Vec::with_capacity(length);
    for index in 0..length {
        children.push(
            ctx.reflect_get(&values, &Value::str(index.to_string()), &values)
                .map_err(OpError::thrown)?,
        );
    }
    Ok(children)
}

fn negate(ctx: &mut Ctx, value: Value) -> OpResult<Value> {
    if let Some((number, unit)) = unit_parts(ctx, &value)? {
        return Ok(unit_value(ctx, -number, unit));
    }
    if ctx.with_instance::<DomCssMathNegate, _>(&value, |_| ()).is_ok() {
        return math_child(ctx, &value, 0);
    }
    let expression = Expression::Negate(Box::new(numeric_expression_from_value(ctx, &value)?));
    create_math_instance(ctx, expression, vec![value], |base| DomCssMathNegate { base })
}

fn invert(ctx: &mut Ctx, value: Value) -> OpResult<Value> {
    if let Some((number, unit)) = unit_parts(ctx, &value)? {
        if unit == NumericUnit::Number {
            if number == 0.0 {
                return Err(OpError::range_error("cannot divide by a zero number"));
            }
            return Ok(unit_value(ctx, 1.0 / number, unit));
        }
    }
    if ctx.with_instance::<DomCssMathInvert, _>(&value, |_| ()).is_ok() {
        return math_child(ctx, &value, 0);
    }
    let expression = Expression::Invert(Box::new(numeric_expression_from_value(ctx, &value)?));
    create_math_instance(ctx, expression, vec![value], |base| DomCssMathInvert { base })
}

fn prefixed(ctx: &mut Ctx, this: &Value, operation: Arithmetic) -> OpResult<Vec<Value>> {
    let same_kind = match operation {
        Arithmetic::Add | Arithmetic::Sub => ctx.with_instance::<DomCssMathSum, _>(this, |_| ()).is_ok(),
        Arithmetic::Mul | Arithmetic::Div => ctx.with_instance::<DomCssMathProduct, _>(this, |_| ()).is_ok(),
        Arithmetic::Min => ctx.with_instance::<DomCssMathMin, _>(this, |_| ()).is_ok(),
        Arithmetic::Max => ctx.with_instance::<DomCssMathMax, _>(this, |_| ()).is_ok(),
    };
    if same_kind {
        math_children(ctx, this)
    } else {
        Ok(vec![this.clone()])
    }
}

pub(super) fn arithmetic(
    ctx: &mut Ctx,
    this: &Value,
    operation: Arithmetic,
    arguments: Vec<CssNumberish>,
) -> OpResult<Value> {
    if arguments.len() > css::typed_numeric::MAX_NUMERIC_EXPRESSION_ARGS {
        return Err(OpError::range_error("too many CSS math values"));
    }
    let mut values = prefixed(ctx, this, operation)?;
    for argument in arguments {
        let value = numberish_value(ctx, argument)?;
        values.push(match operation {
            Arithmetic::Sub => negate(ctx, value)?,
            Arithmetic::Div => invert(ctx, value)?,
            _ => value,
        });
    }
    let mut parts = Vec::with_capacity(values.len());
    for value in &values {
        parts.push(unit_parts(ctx, value)?);
    }
    if parts.iter().all(Option::is_some) {
        let parts: Vec<(f64, NumericUnit)> = parts.into_iter().flatten().collect();
        let first_unit = parts[0].1;
        let same_unit = parts.iter().all(|(_, unit)| *unit == first_unit);
        match operation {
            Arithmetic::Add | Arithmetic::Sub if same_unit => {
                let total = parts.iter().map(|(number, _)| number).sum();
                return Ok(unit_value(ctx, total, first_unit));
            }
            Arithmetic::Min if same_unit => {
                let value = parts.iter().map(|(number, _)| *number).reduce(css::typed_numeric::numeric_minimum);
                return Ok(unit_value(ctx, value.unwrap_or(0.0), first_unit));
            }
            Arithmetic::Max if same_unit => {
                let value = parts.iter().map(|(number, _)| *number).reduce(css::typed_numeric::numeric_maximum);
                return Ok(unit_value(ctx, value.unwrap_or(0.0), first_unit));
            }
            Arithmetic::Mul | Arithmetic::Div => {
                let mut dimensioned = parts.iter().filter(|(_, unit)| *unit != NumericUnit::Number);
                let unit = dimensioned.next().map(|(_, unit)| *unit);
                if dimensioned.next().is_none() {
                    let product = parts.iter().fold(1.0, |product, (number, _)| product * number);
                    return Ok(unit_value(ctx, product, unit.unwrap_or(NumericUnit::Number)));
                }
            }
            _ => {}
        }
    }
    let mut expressions = Vec::with_capacity(values.len());
    for value in &values {
        expressions.push(numeric_expression_from_value(ctx, value)?);
    }
    let expression = match operation {
        Arithmetic::Add | Arithmetic::Sub => Expression::Sum(expressions),
        Arithmetic::Mul | Arithmetic::Div => Expression::Product(expressions),
        Arithmetic::Min => Expression::Min(expressions),
        Arithmetic::Max => Expression::Max(expressions),
    };
    if !expression.within_limits() || expression.numeric_type().is_none() {
        return Err(OpError::type_error("CSS math values have incompatible numeric types"));
    }
    match operation {
        Arithmetic::Add | Arithmetic::Sub => create_math_instance(ctx, expression, values, |base| DomCssMathSum { base }),
        Arithmetic::Mul | Arithmetic::Div => create_math_instance(ctx, expression, values, |base| DomCssMathProduct { base }),
        Arithmetic::Min => create_math_instance(ctx, expression, values, |base| DomCssMathMin { base }),
        Arithmetic::Max => create_math_instance(ctx, expression, values, |base| DomCssMathMax { base }),
    }
}

pub(super) fn equals(ctx: &mut Ctx, this: &Value, others: Vec<CssNumberish>) -> OpResult<bool> {
    let expression = numeric_expression_from_value(ctx, this)?;
    Ok(others.iter().all(|other| other.expression == expression))
}

fn sort_units(units: &mut Vec<(NumericUnit, i32)>) {
    units.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
}

fn merge(items: Vec<SumItem>) -> Vec<SumItem> {
    let mut merged: Vec<SumItem> = Vec::with_capacity(items.len());
    for item in items {
        match merged.iter_mut().find(|existing| existing.units == item.units) {
            Some(existing) => existing.value += item.value,
            None => merged.push(item),
        }
    }
    merged
}

fn sum_value(expression: &Expression) -> Option<Vec<SumItem>> {
    match expression {
        Expression::Value(numeric) => {
            if numeric.unit == NumericUnit::Number {
                return Some(vec![SumItem { value: numeric.value, units: Vec::new() }]);
            }
            let (unit, factor) = numeric.unit.canonical_unit_and_factor().unwrap_or((numeric.unit, 1.0));
            Some(vec![SumItem { value: numeric.value * factor, units: vec![(unit, 1)] }])
        }
        Expression::Calc(inner) => sum_value(inner),
        Expression::Sum(children) => {
            if children.is_empty() {
                return None;
            }
            let mut items = Vec::new();
            for child in children {
                items.extend(sum_value(child)?);
            }
            Some(merge(items))
        }
        Expression::Negate(inner) => {
            let mut items = sum_value(inner)?;
            for item in &mut items {
                item.value = -item.value;
            }
            Some(items)
        }
        Expression::Product(children) => {
            let mut accumulated = vec![SumItem { value: 1.0, units: Vec::new() }];
            for child in children {
                let factors = sum_value(child)?;
                let mut next = Vec::with_capacity(accumulated.len() * factors.len());
                for left in &accumulated {
                    for right in &factors {
                        let mut units = left.units.clone();
                        for (unit, exponent) in &right.units {
                            match units.iter_mut().find(|(existing, _)| existing == unit) {
                                Some((_, total)) => *total += exponent,
                                None => units.push((*unit, *exponent)),
                            }
                        }
                        units.retain(|(_, exponent)| *exponent != 0);
                        sort_units(&mut units);
                        next.push(SumItem { value: left.value * right.value, units });
                    }
                }
                accumulated = next;
            }
            Some(merge(accumulated))
        }
        Expression::Invert(inner) => {
            let items = sum_value(inner)?;
            let [item] = items.as_slice() else { return None };
            if item.value == 0.0 {
                return None;
            }
            Some(vec![SumItem {
                value: 1.0 / item.value,
                units: item.units.iter().map(|(unit, exponent)| (*unit, -exponent)).collect(),
            }])
        }
        Expression::Min(children) | Expression::Max(children) => {
            let pick_min = matches!(expression, Expression::Min(_));
            let mut result: Option<SumItem> = None;
            for child in children {
                let items = sum_value(child)?;
                let [item] = items.as_slice() else { return None };
                result = Some(match result {
                    None => item.clone(),
                    Some(current) => {
                        if current.units != item.units {
                            return None;
                        }
                        let value = if pick_min {
                            css::typed_numeric::numeric_minimum(current.value, item.value)
                        } else {
                            css::typed_numeric::numeric_maximum(current.value, item.value)
                        };
                        SumItem { value, units: current.units }
                    }
                });
            }
            result.map(|item| vec![item])
        }
        Expression::Clamp(lower, value, upper) => {
            let mut single = Vec::with_capacity(3);
            for part in [lower, value, upper] {
                let items = sum_value(part)?;
                let [item] = items.as_slice() else { return None };
                single.push(item.clone());
            }
            if single[0].units != single[1].units || single[1].units != single[2].units {
                return None;
            }
            let clamped = css::typed_numeric::numeric_minimum(
                single[2].value,
                css::typed_numeric::numeric_maximum(single[0].value, single[1].value),
            );
            Some(vec![SumItem { value: clamped, units: single.remove(1).units }])
        }
        Expression::Identifier(_) | Expression::Sign(_) | Expression::Function(..) => None,
    }
}

fn parse_unit(ctx: &mut Ctx, unit: &str) -> OpResult<NumericUnit> {
    NumericUnit::parse(unit).ok_or_else(|| {
        crate::error_reporting::dom_exception(ctx, "SyntaxError", "unknown CSS numeric unit")
    })
}

fn target_matches(item: &SumItem, target: NumericUnit) -> Option<f64> {
    let (canonical, factor) = target.canonical_unit_and_factor().unwrap_or((target, 1.0));
    if target == NumericUnit::Number {
        return item.units.is_empty().then_some(item.value);
    }
    (item.units.len() == 1 && item.units[0] == (canonical, 1)).then(|| item.value / factor)
}

fn this_sum(ctx: &mut Ctx, this: &Value) -> OpResult<Vec<SumItem>> {
    let expression = numeric_expression_from_value(ctx, this)?;
    sum_value(&expression).ok_or_else(|| OpError::type_error("CSS numeric value cannot be converted to a sum"))
}

pub(super) fn to(ctx: &mut Ctx, this: &Value, unit: &str) -> OpResult<Value> {
    ctx.with_instance::<DomCssNumericValue, _>(this, |_| ())?;
    let target = parse_unit(ctx, unit)?;
    let items = this_sum(ctx, this)?;
    let [item] = items.as_slice() else {
        return Err(OpError::type_error("CSS numeric value is not a single unit"));
    };
    let value = target_matches(item, target)
        .ok_or_else(|| OpError::type_error("CSS numeric value is incompatible with the requested unit"))?;
    Ok(unit_value(ctx, value, target))
}

pub(super) fn to_sum(ctx: &mut Ctx, this: &Value, units: Vec<String>) -> OpResult<Value> {
    ctx.with_instance::<DomCssNumericValue, _>(this, |_| ())?;
    let mut targets = Vec::with_capacity(units.len());
    for unit in &units {
        targets.push(parse_unit(ctx, unit)?);
    }
    let mut combined: Option<NumericType> = None;
    for target in &targets {
        let kind = NumericType::from_unit(*target);
        combined = Some(match combined {
            None => kind,
            Some(existing) => existing
                .add(kind)
                .ok_or_else(|| OpError::type_error("CSS units are not addable"))?,
        });
    }
    let mut items = this_sum(ctx, this)?;
    let mut results: Vec<(f64, NumericUnit)> = Vec::new();
    if targets.is_empty() {
        items.sort_by(|left, right| {
            let key = |item: &SumItem| item.units.iter().map(|(unit, _)| unit.as_str()).collect::<Vec<_>>();
            key(left).cmp(&key(right))
        });
        for item in &items {
            match item.units.as_slice() {
                [] => results.push((item.value, NumericUnit::Number)),
                [(unit, 1)] => results.push((item.value, *unit)),
                _ => return Err(OpError::type_error("CSS numeric value has compound units")),
            }
        }
    } else {
        for target in &targets {
            let mut total = 0.0;
            let mut index = 0;
            while index < items.len() {
                match target_matches(&items[index], *target) {
                    Some(value) => {
                        total += value;
                        items.remove(index);
                    }
                    None => index += 1,
                }
            }
            results.push((total, *target));
        }
        if !items.is_empty() {
            return Err(OpError::type_error("CSS numeric value has units outside the requested set"));
        }
    }
    let mut values = Vec::with_capacity(results.len());
    let mut expressions = Vec::with_capacity(results.len());
    for (value, unit) in results {
        values.push(unit_value(ctx, value, unit));
        expressions.push(Expression::Value(css::typed_numeric::NumericValue { value, unit }));
    }
    create_math_instance(ctx, Expression::Sum(expressions), values, |base| DomCssMathSum { base })
}
