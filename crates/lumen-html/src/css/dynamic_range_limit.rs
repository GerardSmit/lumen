//! `dynamic-range-limit-mix()` for `dynamic-range-limit`: parsing, percentage
//! normalization, serialization and interpolation of the keyword mixture.
use super::*;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

const KEYWORDS: [&str; 3] = ["standard", "constrained", "no-limit"];
const FUNCTION: &str = "dynamic-range-limit-mix(";

type Weights = [f64; 3];

pub(super) fn is_mix(raw: &str) -> bool {
    let raw = raw.trim_start();
    raw.get(..FUNCTION.len()).is_some_and(|head| head.eq_ignore_ascii_case(FUNCTION)) && raw.trim_end().ends_with(')')
}

/// Canonical text of a `dynamic-range-limit-mix()` value, or `None` when invalid.
/// With `computed`, the mixture is flattened into normalized keyword weights.
pub(super) fn canonicalize(raw: &str, computed: bool, context: LengthContext) -> Option<String> {
    let query = ContainerUnitContext::no_container(context.viewport);
    let (text, weights) = parse(raw.trim(), computed, context, query, 0)?;
    if computed {
        return Some(serialize(weights?));
    }
    Some(text)
}

const MAX_DEPTH: usize = 16;

fn parse(
    raw: &str,
    computed: bool,
    context: LengthContext,
    query: ContainerUnitContext,
    depth: usize,
) -> Option<(String, Option<Weights>)> {
    if depth > MAX_DEPTH {
        return None;
    }
    let lowered = raw.to_ascii_lowercase();
    if let Some(index) = KEYWORDS.iter().position(|keyword| *keyword == lowered) {
        let mut weights = [0.0; 3];
        weights[index] = 1.0;
        return Some((lowered, Some(weights)));
    }
    if !is_mix(raw) {
        return None;
    }
    let open = FUNCTION.len() - 1;
    if syntax::matching_close(raw, open)? != raw.len() - 1 {
        return None;
    }
    let parts = top_level_split(&raw[FUNCTION.len()..raw.len() - 1], b',', usize::MAX)?;
    if parts.len() < 2 {
        return None;
    }
    let mut texts = Vec::new();
    let mut total = 0.0;
    let mut mixed: Option<Weights> = Some([0.0; 3]);
    for part in parts {
        let words = components(part)?;
        let [first, second] = words.as_slice() else { return None };
        let (limit, percentage) = if is_percentage(first) { (*second, *first) } else { (*first, *second) };
        if is_percentage(limit) || !is_percentage(percentage) {
            return None;
        }
        let (limit_text, limit_weights) = parse(limit, computed, context, query, depth + 1)?;
        let (percentage_text, value) = parse_percentage(percentage, context, query)?;
        let value = if computed { value.clamp(0.0, 100.0) } else if (0.0..=100.0).contains(&value) { value } else { return None };
        total += value;
        mixed = match (mixed, limit_weights) {
            (Some(mut sum), Some(child)) => {
                for (slot, weight) in sum.iter_mut().zip(child) {
                    *slot += weight * value;
                }
                Some(sum)
            }
            _ => None,
        };
        texts.push(format!("{limit_text} {percentage_text}"));
    }
    if !(total > 0.0) {
        return None;
    }
    let weights = mixed.map(|sum| sum.map(|weight| weight / total));
    Some((format!("{FUNCTION}{})", texts.join(", ")), weights))
}

fn is_percentage(word: &str) -> bool {
    word.ends_with('%') || word.get(..5).is_some_and(|head| head.eq_ignore_ascii_case("calc("))
}

fn parse_percentage(word: &str, context: LengthContext, query: ContainerUnitContext) -> Option<(String, f64)> {
    use typed_numeric::{NumericType, NumericUnit};
    let expression = typed_numeric::parse_numeric_expression(word)?;
    if expression.numeric_type()? != NumericType::from_unit(NumericUnit::Percent) {
        return None;
    }
    let value = expression.evaluate(&mut FontAngleContext { length: Some(context), query, percent_scale: 1.0 })?;
    value.is_finite().then_some(())?;
    Some((expression.serialize_specified()?, value))
}

fn number(value: f64) -> String {
    let rounded = (value * 1e6).round() / 1e6;
    if rounded.fract() == 0.0 { format!("{}", rounded as i64) } else { format!("{rounded}") }
}

fn serialize(weights: Weights) -> String {
    let present: Vec<usize> = (0..3).filter(|index| weights[*index] * 100.0 >= 5e-7).collect();
    if let [only] = present.as_slice() {
        return String::from(KEYWORDS[*only]);
    }
    let parts: Vec<String> = present.iter().map(|index| format!("{} {}%", KEYWORDS[*index], number(weights[*index] * 100.0))).collect();
    format!("{FUNCTION}{})", parts.join(", "))
}

fn weights_of(computed: &str) -> Option<Weights> {
    let context = static_length_context();
    parse(computed.trim(), true, context, ContainerUnitContext::no_container(context.viewport), 0)?.1
}

pub(super) fn interpolate(from: &str, to: &str, progress: f64) -> Option<String> {
    if !progress.is_finite() {
        return None;
    }
    let (from, to) = (weights_of(from)?, weights_of(to)?);
    let progress = progress.clamp(0.0, 1.0);
    let mut mixed = [0.0; 3];
    for index in 0..3 {
        mixed[index] = from[index] + (to[index] - from[index]) * progress;
    }
    Some(serialize(mixed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn computed(raw: &str) -> Option<String> {
        canonicalize(raw, true, static_length_context())
    }

    #[test]
    fn computed_values_flatten_and_normalize() {
        assert_eq!(computed("dynamic-range-limit-mix(no-limit 10%, standard 30%)").as_deref(), Some("dynamic-range-limit-mix(standard 75%, no-limit 25%)"));
        assert_eq!(computed("dynamic-range-limit-mix(no-limit 100%, standard 0%)").as_deref(), Some("no-limit"));
        assert!(computed("dynamic-range-limit-mix(no-limit 0%, standard 0%)").is_none());
        assert!(computed("dynamic-range-limit-mix(no-limit 1%)").is_none());
    }

    #[test]
    fn interpolation_mixes_weights() {
        assert_eq!(interpolate("no-limit", "standard", 0.25).as_deref(), Some("dynamic-range-limit-mix(standard 25%, no-limit 75%)"));
        assert_eq!(interpolate("no-limit", "standard", 1.0).as_deref(), Some("standard"));
    }
}
