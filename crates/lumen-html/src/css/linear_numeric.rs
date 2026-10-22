//! Collapse weighted sums of lengths and percentages into the canonical
//! `calc(<percentage> + <length>)` pair that value combination produces.
use super::typed_numeric::{serialize_numeric_value, NumericExpression as E, NumericUnit, NumericValue};
use alloc::{boxed::Box, vec};

fn collect(expression: &E, scale: f64, percent: &mut f64, length: &mut f64) -> Option<()> {
    match expression {
        E::Value(value) => {
            if value.unit == NumericUnit::Percent {
                *percent += scale * value.value;
            } else {
                let (unit, factor) = value.unit.canonical_unit_and_factor()?;
                if unit != NumericUnit::Px { return None; }
                *length += scale * value.value * factor;
            }
            Some(())
        }
        E::Calc(inner) => collect(inner, scale, percent, length),
        E::Negate(inner) => collect(inner, -scale, percent, length),
        E::Sum(terms) => terms.iter().try_for_each(|term| collect(term, scale, percent, length)),
        E::Product(factors) => {
            let mut scale = scale;
            let mut rest = None;
            for factor in factors {
                match factor.single_numeric_value() {
                    Some(NumericValue { value, unit: NumericUnit::Number }) => scale *= value,
                    _ if rest.is_none() => rest = Some(factor),
                    _ => return None,
                }
            }
            collect(rest?, scale, percent, length)
        }
        _ => None,
    }
}

fn has_scaled_sum(expression: &E) -> bool {
    match expression {
        E::Calc(inner) | E::Negate(inner) => has_scaled_sum(inner),
        E::Sum(terms) => terms.iter().any(has_scaled_sum),
        E::Product(factors) => factors.iter().any(|factor| !matches!(factor, E::Value(_))),
        _ => false,
    }
}

/// Flatten a weighted combination that mixes percentages with lengths. Only
/// trees that still contain an unexpanded scaled sum are rewritten.
pub fn flatten(expression: &E) -> Option<E> {
    if !has_scaled_sum(expression) || !expression.contains_unit(|unit| unit == NumericUnit::Percent) { return None; }
    let (mut percent, mut length) = (0.0, 0.0);
    collect(expression, 1.0, &mut percent, &mut length)?;
    if !percent.is_finite() || !length.is_finite() { return None; }
    let value = |value, unit| E::Value(NumericValue { value, unit });
    Some(if length == 0.0 {
        value(percent, NumericUnit::Percent)
    } else {
        E::Calc(Box::new(E::Sum(vec![value(percent, NumericUnit::Percent), value(length, NumericUnit::Px)])))
    })
}

pub fn serialize_flat(expression: &E) -> Option<alloc::string::String> {
    let flat = flatten(expression)?;
    match flat.single_numeric_value() {
        Some(value) => Some(serialize_numeric_value(value.value, value.unit)),
        None => flat.serialize(),
    }
}
