//! Interpolation of multi-component computed values and the numeric ranges
//! animated results are clamped to.
use alloc::{borrow::ToOwned, format, string::String, vec::Vec};

/// Whitespace separated top-level components; empty when the text is malformed.
pub fn split_components(input: &str) -> Vec<&str> {
    crate::css::components(input).unwrap_or_default()
}

fn interpolate_component(a: &str, b: &str, progress: f64) -> Option<String> {
    if a == b { return Some(a.to_owned()); }
    super::interpolate_numeric(a, b, progress)
}

fn interpolate_item(a: &str, b: &str, progress: f64) -> Option<String> {
    let (parts_a, parts_b) = (split_components(a), split_components(b));
    if parts_a.len() != parts_b.len() || parts_a.is_empty() { return None; }
    let mut output = Vec::with_capacity(parts_a.len());
    for (a, b) in parts_a.into_iter().zip(parts_b) {
        output.push(interpolate_component(a, b, progress)?);
    }
    Some(output.join(" "))
}

const MAX_REPEATED_LIST_ITEMS: usize = 64;

fn lcm(a: usize, b: usize) -> Option<usize> {
    lumen_common::numeric::lcm_capped(a, b, MAX_REPEATED_LIST_ITEMS)
}

/// Component-wise interpolation of whitespace separated components and comma
/// separated lists of them. Lists of different lengths repeat to their least
/// common multiple only for `repeatable` properties.
pub fn interpolate_components(from: &str, to: &str, progress: f64, repeatable: bool) -> Option<String> {
    if !progress.is_finite() { return None; }
    let (list_a, list_b) = (crate::css::css_list_items(from), crate::css::css_list_items(to));
    if list_a.is_empty() || list_b.is_empty() { return None; }
    let count = if list_a.len() == list_b.len() { list_a.len() }
        else if repeatable { lcm(list_a.len(), list_b.len())? }
        else { return None };
    if count == 1 && split_components(from).len() < 2 { return None; }
    let mut output = Vec::with_capacity(count);
    for index in 0..count {
        output.push(interpolate_item(&list_a[index % list_a.len()], &list_b[index % list_b.len()], progress)?);
    }
    Some(output.join(", "))
}

/// Component-wise addition of whitespace separated components and comma
/// separated lists of them; lists repeat to their least common multiple.
/// `delta` repeats an added iteration value `count` times.
/// transform-origin takes an optional z length; the shorter operand gets `0px`.
pub fn pad_origin(property: &str, a: &str, b: &str) -> Option<(String, String)> {
    if property.starts_with("border-") && property.ends_with("-radius") {
        let (parts_a, parts_b) = (split_components(a), split_components(b));
        if parts_a.len() == parts_b.len() || !(1..=2).contains(&parts_a.len()) || !(1..=2).contains(&parts_b.len()) {
            return None;
        }
        let pad = |parts: &[&str], raw: &str| if parts.len() == 1 { format!("{0} {0}", raw.trim()) } else { raw.to_owned() };
        return Some((pad(&parts_a, a), pad(&parts_b, b)));
    }
    if property != "transform-origin" {
        return None;
    }
    let (na, nb) = (a.split_whitespace().count(), b.split_whitespace().count());
    if na == nb || !(2..=3).contains(&na) || !(2..=3).contains(&nb) {
        return None;
    }
    let pad = |v: &str, n: usize| if n == 2 { format!("{} 0px", v.trim()) } else { v.to_owned() };
    Some((pad(a, na), pad(b, nb)))
}

/// `text-indent` adds its length; the `hanging`/`each-line` keywords must agree.
pub fn add_text_indent(base: &str, value: &str, delta: Option<(&str, u64)>) -> Option<String> {
    let (base_length, mode) = crate::css::split_text_indent(base)?;
    let (value_length, value_mode) = crate::css::split_text_indent(value)?;
    if mode != value_mode {
        return None;
    }
    let delta = match delta {
        Some((raw, count)) => {
            let (length, delta_mode) = crate::css::split_text_indent(raw)?;
            if delta_mode != mode {
                return None;
            }
            Some((length, count))
        }
        None => None,
    };
    let length = super::add_numeric_values(base_length, value_length, delta)?;
    Some(format!("{length}{}", crate::css::text_indent_keywords(mode)))
}

pub fn add_components(base: &str, value: &str, delta: Option<(&str, u64)>) -> Option<String> {
    let (list_base, list_value) = (crate::css::css_list_items(base), crate::css::css_list_items(value));
    if list_base.is_empty() || list_value.is_empty() { return None; }
    let list_delta = match delta {
        Some((delta, count)) => Some((crate::css::css_list_items(delta), count)),
        None => None,
    };
    let mut count = if list_base.len() == list_value.len() { list_base.len() }
        else { lcm(list_base.len(), list_value.len())? };
    if let Some((items, _)) = &list_delta {
        if items.is_empty() { return None; }
        count = if count == items.len() { count } else { lcm(count, items.len())? };
    }
    if count == 1 && split_components(base).len() < 2 { return None; }
    let mut output = Vec::with_capacity(count);
    for index in 0..count {
        let item_base = split_components(&list_base[index % list_base.len()]);
        let item_value = split_components(&list_value[index % list_value.len()]);
        let item_delta = list_delta.as_ref().map(|(items, count)| (split_components(&items[index % items.len()]), *count));
        if item_base.len() != item_value.len() || item_base.is_empty() { return None; }
        let mut parts = Vec::with_capacity(item_base.len());
        for (position, (a, b)) in item_base.iter().zip(&item_value).enumerate() {
            let part_delta = match &item_delta {
                Some((parts, count)) => Some((*parts.get(position)?, *count)),
                None => None,
            };
            parts.push(super::add_numeric_values(a, b, part_delta)?);
        }
        output.push(parts.join(" "));
    }
    Some(output.join(", "))
}

fn parse_ratio(input: &str) -> Option<(bool, f64)> {
    let input = input.trim();
    let (auto, ratio) = match input.strip_prefix("auto") {
        Some(rest) if rest.is_empty() => return None,
        Some(rest) => (true, rest.trim()),
        None => (false, input),
    };
    let (numerator, denominator) = match ratio.split_once('/') {
        Some((a, b)) => (a.trim().parse::<f64>().ok()?, b.trim().parse::<f64>().ok()?),
        None => (ratio.parse::<f64>().ok()?, 1.0),
    };
    (numerator > 0.0 && denominator > 0.0 && numerator.is_finite() && denominator.is_finite())
        .then_some((auto, numerator / denominator))
}

/// Ratios interpolate on the logarithm of their value.
pub fn interpolate_aspect_ratio(from: &str, to: &str, progress: f64) -> Option<String> {
    if !progress.is_finite() { return None; }
    let ((auto_a, a), (auto_b, b)) = (parse_ratio(from)?, parse_ratio(to)?);
    if auto_a != auto_b { return None; }
    let ratio = (a.ln() + (b.ln() - a.ln()) * progress).exp();
    let value = super::serialize_numeric(ratio, "")?;
    Some(if auto_a { format!("auto {value} / 1") } else { format!("{value} / 1") })
}

#[derive(Clone, Copy, PartialEq)]
enum Floor { Length, Number, Percent, Weight }

fn floor(property: &str) -> Option<Floor> {
    Some(match property {
        "width" | "height" | "min-width" | "min-height" | "max-width" | "max-height"
        | "inline-size" | "block-size" | "min-inline-size" | "min-block-size" | "max-inline-size" | "max-block-size"
        | "column-width" | "column-gap" | "row-gap" | "gap" | "font-size" | "border-spacing" | "shape-margin"
        | "line-height" | "flex-basis" | "border-image-width" | "border-image-outset" | "outline-width"
        | "column-rule-width" | "stroke-width" | "text-decoration-thickness" | "contain-intrinsic-width"
        | "contain-intrinsic-size" | "contain-intrinsic-height" | "contain-intrinsic-inline-size" | "contain-intrinsic-block-size" => Floor::Length,
        "flex-grow" | "flex-shrink" | "font-size-adjust" => Floor::Number,
        "font-stretch" => Floor::Percent,
        "font-weight" => Floor::Weight,
        _ if property.starts_with("padding-") || property.starts_with("scroll-padding-") => Floor::Length,
        _ if property.starts_with("border-") && (property.ends_with("-width") || property.ends_with("-radius")) => Floor::Length,
        _ => return None,
    })
}

fn clamp_token(token: &str, floor: Floor) -> String {
    if let Some((number, unit)) = super::numeric_unit(token) {
        if floor == Floor::Weight {
            return if number < 1.0 { "1".to_owned() } else if number > 1000.0 { "1000".to_owned() } else { token.to_owned() };
        }
        return if number < 0.0 { format!("0{unit}") } else { token.to_owned() };
    }
    if token.starts_with("calc(") && floor == Floor::Weight {
        return format!("clamp(1, {token}, 1000)");
    }
    token.to_owned()
}

/// Animated results outside a property's value range are clamped, since the
/// range check at parse time would otherwise discard the sampled declaration.
pub fn clamp_range(property: &str, value: String) -> String {
    let Some(floor) = floor(property) else { return value; };
    if value.contains('/') && property.ends_with("-radius") { return value; }
    let tokens = split_components(&value);
    let clamped: Vec<String> = tokens.iter().map(|token| clamp_token(token, floor)).collect();
    if tokens.iter().zip(&clamped).all(|(token, clamped)| token == clamped) { return value; }
    clamped.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_interpolate_each_value_and_repeat_lists() {
        assert_eq!(interpolate_components("10px 30px", "20px 20px", 0.5, false).as_deref(), Some("15px 25px"));
        assert_eq!(interpolate_components("10px 10px", "20px 20px, 0px 0px", 0.5, true).as_deref(), Some("15px 15px, 5px 5px"));
        assert_eq!(interpolate_components("10px 10px", "20px 20px, 0px 0px", 0.5, false), None);
        assert_eq!(interpolate_components("auto 10px", "auto 20px", 0.5, false).as_deref(), Some("auto 15px"));
        assert_eq!(interpolate_components("left 10px", "right 20px", 0.5, false), None);
    }

    #[test]
    fn aspect_ratio_interpolates_logarithmically() {
        assert_eq!(interpolate_aspect_ratio("0.5", "2", 0.5).as_deref(), Some("1 / 1"));
        assert_eq!(interpolate_aspect_ratio("auto 1 / 1", "2", 0.5), None);
    }

    #[test]
    fn negative_results_clamp_to_the_property_range() {
        assert_eq!(clamp_range("width", "-30px".into()), "0px");
        assert_eq!(clamp_range("border-spacing", "-3px 4px".into()), "0px 4px");
        assert_eq!(clamp_range("flex-grow", "-0.5".into()), "0");
        assert_eq!(clamp_range("font-weight", "1200".into()), "1000");
        assert_eq!(clamp_range("left", "-30px".into()), "-30px");
        assert_eq!(clamp_range("width", "calc(13px - 30%)".into()), "calc(13px - 30%)");
    }
}
