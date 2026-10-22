//! Specified-value serialization shared by the `<number> | <percentage>` opacity
//! properties: plain values serialize as numbers, calculations keep their
//! percentage notation and fold when every operand is comparable.
use super::*;
use typed_numeric::{NumericExpression, NumericType, NumericUnit};

pub(super) fn specified(value: &str) -> Option<String> {
    let (admission, _) = typed_numeric::substitute_sibling_functions(value, 1, 1)?;
    let scalar = css_scalar(&admission, true)?;
    if !math_function(value.trim()) {
        return Some(computed_values::number(scalar));
    }
    let expression = typed_numeric::parse_numeric_expression(value)?;
    if expression.numeric_type() == Some(NumericType::from_unit(NumericUnit::Percent)) {
        let mut relabeled = expression.clone();
        relabeled.map_numeric_values(|mut leaf| {
            if leaf.unit == NumericUnit::Percent {
                leaf.unit = NumericUnit::Number;
            }
            Some(leaf)
        })?;
        relabeled.simplify_absolute_units();
        if let Some(mut single) = relabeled.single_numeric_value() {
            single.unit = NumericUnit::Percent;
            return NumericExpression::Calc(Box::new(NumericExpression::Value(single))).serialize();
        }
    }
    expression.serialize_specified()
}
