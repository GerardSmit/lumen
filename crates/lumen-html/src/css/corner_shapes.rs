use super::*;
use super::extra_props as extra;

const KEYWORDS: [(&str, f64); 6] = [
    ("round", 1.0),
    ("scoop", -1.0),
    ("notch", f64::NEG_INFINITY),
    ("bevel", 0.0),
    ("squircle", 2.0),
    ("square", f64::INFINITY),
];

struct Short {
    name: &'static str,
    longhands: &'static [&'static str],
}

static TABLE: &[Short] = &[
    Short {
        name: "corner-shape",
        longhands: &["corner-top-left-shape", "corner-top-right-shape", "corner-bottom-right-shape", "corner-bottom-left-shape"],
    },
    Short { name: "corner-top-shape", longhands: &["corner-top-left-shape", "corner-top-right-shape"] },
    Short { name: "corner-right-shape", longhands: &["corner-top-right-shape", "corner-bottom-right-shape"] },
    Short { name: "corner-bottom-shape", longhands: &["corner-bottom-left-shape", "corner-bottom-right-shape"] },
    Short { name: "corner-left-shape", longhands: &["corner-top-left-shape", "corner-bottom-left-shape"] },
    Short { name: "corner-inline-start-shape", longhands: &["corner-start-start-shape", "corner-end-start-shape"] },
    Short { name: "corner-inline-end-shape", longhands: &["corner-start-end-shape", "corner-end-end-shape"] },
    Short { name: "corner-block-start-shape", longhands: &["corner-start-start-shape", "corner-start-end-shape"] },
    Short { name: "corner-block-end-shape", longhands: &["corner-end-start-shape", "corner-end-end-shape"] },
];

fn short(name: &str) -> Option<&'static Short> {
    TABLE.iter().find(|short| short.name == name)
}

pub(crate) fn is_shorthand(name: &str) -> bool {
    short(name).is_some()
}

pub(crate) fn longhands(name: &str) -> &'static [&'static str] {
    short(name).map_or(&[], |short| short.longhands)
}

fn superellipse_argument(raw: &str) -> Option<&str> {
    let head = raw.get(..13)?;
    (head.eq_ignore_ascii_case("superellipse(") && raw.ends_with(')')).then(|| &raw[13..raw.len() - 1])
}

fn curvature(raw: &str, context: LengthContext) -> Option<f64> {
    let raw = raw.trim();
    if let Some((_, value)) = KEYWORDS.iter().find(|(keyword, _)| decoded_css_keyword(raw, keyword)) {
        return Some(*value);
    }
    let argument = superellipse_argument(raw)?.trim();
    if decoded_css_keyword(argument, "infinity") {
        return Some(f64::INFINITY);
    }
    if decoded_css_keyword(argument, "-infinity") {
        return Some(f64::NEG_INFINITY);
    }
    if argument.is_empty() || argument.contains('%') {
        return None;
    }
    extra::parse_number(argument, context)
}

fn serialize_curvature(value: f64) -> String {
    if value == f64::INFINITY {
        String::from("superellipse(infinity)")
    } else if value == f64::NEG_INFINITY {
        String::from("superellipse(-infinity)")
    } else {
        alloc::format!("superellipse({})", super::computed_values::number(value as f32))
    }
}

pub(crate) fn compute(raw: &str, context: LengthContext) -> Option<String> {
    curvature(raw, context).map(serialize_curvature)
}

pub(crate) fn specified(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let context = static_length_context();
    curvature(raw, context)?;
    if KEYWORDS.iter().any(|(keyword, _)| decoded_css_keyword(raw, keyword)) {
        return Some(extra::lower(raw).into_owned());
    }
    let argument = superellipse_argument(raw)?.trim();
    if decoded_css_keyword(argument, "infinity") || decoded_css_keyword(argument, "-infinity") {
        return Some(alloc::format!("superellipse({})", extra::lower(argument)));
    }
    if math_function(argument) {
        let expression = typed_numeric::parse_numeric_expression(argument)?.serialize_specified()?;
        return Some(alloc::format!("superellipse({expression})"));
    }
    Some(serialize_curvature(curvature(raw, context)?))
}

/// Interpolation runs on the diagonal position of the curve, which keeps
/// bevel halfway between notch and square.
fn diagonal(curvature: f64) -> f64 {
    let convex = |k: f64| if k == f64::INFINITY { 1.0 } else { libm::exp2(-1.0 / libm::exp2(k)) };
    if curvature >= 0.0 {
        convex(curvature)
    } else {
        1.0 - convex(-curvature)
    }
}

fn from_diagonal(position: f64) -> f64 {
    let concave = position < 0.5;
    let position = if concave { 1.0 - position } else { position };
    let value = if position >= 1.0 { f64::INFINITY } else { libm::log2(-1.0 / libm::log2(position)) };
    if concave { -value } else { value }
}

fn parse_computed(value: &str) -> Option<f64> {
    curvature(value, static_length_context())
}

pub(crate) fn interpolate(from: &str, to: &str, progress: f64) -> Option<String> {
    let (a, b) = (diagonal(parse_computed(from)?), diagonal(parse_computed(to)?));
    let position = (a + (b - a) * progress).clamp(0.0, 1.0);
    Some(serialize_curvature(from_diagonal(position)))
}

fn expand_values(arity: usize, raw: &str) -> Option<Vec<String>> {
    let tokens = components(raw)?;
    if tokens.is_empty() || tokens.len() > arity {
        return None;
    }
    let values: Vec<String> = tokens.iter().map(|token| specified(token)).collect::<Option<_>>()?;
    let at = |index: usize| values[index].clone();
    Some(match (arity, values.len()) {
        (_, 1) => alloc::vec![at(0); arity],
        (2, _) => values,
        (_, 2) => alloc::vec![at(0), at(1), at(0), at(1)],
        (_, 3) => alloc::vec![at(0), at(1), at(2), at(1)],
        _ => values,
    })
}

pub(crate) fn expand(name: &str, raw: &str) -> Option<Vec<(&'static str, String)>> {
    let short = short(name)?;
    let values = expand_values(short.longhands.len(), raw.trim())?;
    Some(short.longhands.iter().copied().zip(values).collect())
}

pub(crate) fn synthesize(name: &str, values: &[&str]) -> Option<String> {
    let short = short(name)?;
    if values.len() != short.longhands.len() {
        return None;
    }
    Some(match values {
        [a, b] => if a == b { (*a).to_owned() } else { alloc::format!("{a} {b}") },
        [a, b, c, d] => declaration_block::serialize_box_sides([a, b, c, d]),
        _ => return None,
    })
}

pub(crate) fn computed_shorthand(style: &Style, name: &str) -> String {
    let Some(short) = short(name) else { return String::new() };
    let values: Option<Vec<String>> = short.longhands.iter().map(|longhand| extra::computed(style, longhand)).collect();
    let Some(values) = values else { return String::new() };
    let refs: Vec<&str> = values.iter().map(String::as_str).collect();
    synthesize(name, &refs).unwrap_or_default()
}

pub(crate) fn shorthand_declarations(name: &str, raw: &str) -> Option<Vec<Value>> {
    let mut values = Vec::new();
    for (longhand, text) in expand(name, raw)? {
        values.extend(extra::declarations(longhand, &text)?);
    }
    Some(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_compute_to_superellipse() {
        assert_eq!(compute("round", static_length_context()).as_deref(), Some("superellipse(1)"));
        assert_eq!(compute("notch", static_length_context()).as_deref(), Some("superellipse(-infinity)"));
        assert_eq!(compute("superellipse( -1)", static_length_context()).as_deref(), Some("superellipse(-1)"));
        assert_eq!(compute("superellipse(1 abc)", static_length_context()), None);
    }

    #[test]
    fn interpolation_keeps_bevel_between_notch_and_square() {
        assert_eq!(interpolate("superellipse(-infinity)", "superellipse(infinity)", 0.5).as_deref(), Some("superellipse(0)"));
        assert!(interpolate("superellipse(1)", "superellipse(0)", 0.6).unwrap().starts_with("superellipse(0.36"));
        assert_eq!(interpolate("superellipse(0)", "superellipse(infinity)", 1.5).as_deref(), Some("superellipse(infinity)"));
    }

    #[test]
    fn shorthand_expansion_follows_box_side_order() {
        let expanded = expand("corner-shape", "bevel superellipse(2) squircle").unwrap();
        let values: Vec<&str> = expanded.iter().map(|(_, value)| value.as_str()).collect();
        assert_eq!(values, ["bevel", "superellipse(2)", "squircle", "superellipse(2)"]);
        assert_eq!(synthesize("corner-shape", &values).as_deref(), Some("bevel superellipse(2) squircle"));
    }
}
