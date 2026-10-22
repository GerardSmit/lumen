//! `palette-mix()` for `font-palette`: parsing, percentage normalization and serialization.
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use super::{components, syntax, top_level_split};

const RECTANGULAR_SPACES: [&str; 11] = [
    "srgb", "srgb-linear", "display-p3", "display-p3-linear", "a98-rgb", "prophoto-rgb", "rec2020", "lab", "oklab", "xyz", "xyz-d50",
];
const POLAR_SPACES: [&str; 4] = ["hsl", "hwb", "lch", "oklch"];
const HUE_METHODS: [&str; 4] = ["shorter", "longer", "increasing", "decreasing"];

pub fn is_palette_mix(raw: &str) -> bool {
    let raw = raw.trim_start();
    raw.get(..12).is_some_and(|head| head.eq_ignore_ascii_case("palette-mix(")) && raw.ends_with(')')
}

/// Canonical text of a `palette-mix()` value, or `None` when it is invalid. With `computed`,
/// `calc()` percentages are resolved to numbers.
pub fn canonicalize(raw: &str, computed: bool) -> Option<String> {
    let raw = raw.trim();
    if !is_palette_mix(raw) {
        return None;
    }
    let inner = &raw[12..raw.len() - 1];
    if syntax::matching_close(raw, 11)? != raw.len() - 1 {
        return None;
    }
    let mut parts = top_level_split(inner, b',', usize::MAX)?;
    let method = parse_method(&mut parts)?;
    if parts.is_empty() {
        return None;
    }
    let mut components = Vec::new();
    for part in parts {
        components.push(parse_component(part, computed)?);
    }
    normalize(&mut components);

    let mut output = String::from("palette-mix(");
    if let Some(method) = method {
        output.push_str(&method);
        output.push_str(", ");
    }
    for (index, component) in components.iter().enumerate() {
        if index > 0 {
            output.push_str(", ");
        }
        output.push_str(&component.palette);
        match &component.percentage {
            Percentage::None => {}
            Percentage::Number(value) => output.push_str(&format!(" {}%", number(*value))),
            Percentage::Calc(text) => {
                output.push(' ');
                output.push_str(text);
            }
        }
    }
    output.push(')');
    Some(output)
}

enum Percentage {
    None,
    Number(f64),
    Calc(String),
}

struct Component {
    palette: String,
    percentage: Percentage,
}

fn parse_method(parts: &mut Vec<&str>) -> Option<Option<String>> {
    let first = parts.first()?.trim();
    let words = components(first)?;
    let leads_with_in = words.first().is_some_and(|word| word.eq_ignore_ascii_case("in"));
    if !leads_with_in {
        return Some(None);
    }
    let space = words.get(1)?.to_ascii_lowercase();
    let mut hue = None;
    match words.len() {
        2 => {}
        4 => {
            let method = words[2].to_ascii_lowercase();
            if !words[3].eq_ignore_ascii_case("hue") || !HUE_METHODS.contains(&method.as_str()) || !POLAR_SPACES.contains(&space.as_str()) {
                return None;
            }
            hue = Some(method);
        }
        _ => return None,
    }
    if !RECTANGULAR_SPACES.contains(&space.as_str()) && !POLAR_SPACES.contains(&space.as_str()) {
        return None;
    }
    parts.remove(0);
    let space = if space == "xyz" { String::from("xyz-d65") } else { space };
    if space == "oklab" && hue.is_none() {
        return Some(None);
    }
    Some(Some(match hue.as_deref() {
        None | Some("shorter") => format!("in {space}"),
        Some(method) => format!("in {space} {method} hue"),
    }))
}

fn parse_component(part: &str, computed: bool) -> Option<Component> {
    let words = components(part)?;
    let (palette, percentage) = match words.as_slice() {
        [palette] => (*palette, None),
        [first, second] => {
            if is_percentage(first) {
                (*second, Some(*first))
            } else {
                (*first, Some(*second))
            }
        }
        _ => return None,
    };
    let palette = parse_palette(palette, computed)?;
    let percentage = match percentage {
        None => Percentage::None,
        Some(text) => parse_percentage(text, computed)?,
    };
    Some(Component { palette, percentage })
}

fn is_percentage(word: &str) -> bool {
    word.ends_with('%') || word.to_ascii_lowercase().starts_with("calc(")
}

fn parse_palette(word: &str, computed: bool) -> Option<String> {
    if is_palette_mix(word) {
        return canonicalize(word, computed);
    }
    let lowered = word.to_ascii_lowercase();
    if matches!(lowered.as_str(), "normal" | "light" | "dark") {
        return Some(lowered);
    }
    let valid = word.len() > 2
        && word.starts_with("--")
        && word.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || !byte.is_ascii());
    valid.then(|| String::from(word))
}

fn parse_percentage(word: &str, computed: bool) -> Option<Percentage> {
    let lowered = word.to_ascii_lowercase();
    if let Some(inner) = lowered.strip_prefix("calc(").and_then(|rest| rest.strip_suffix(')')) {
        if !computed {
            return Some(Percentage::Calc(lowered.clone()));
        }
        let value = plain_percentage(inner.trim())?;
        return (0.0..=100.0).contains(&value).then_some(Percentage::Number(value));
    }
    let value = plain_percentage(&lowered)?;
    (0.0..=100.0).contains(&value).then_some(Percentage::Number(value))
}

fn plain_percentage(text: &str) -> Option<f64> {
    let digits = text.strip_suffix('%')?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b'+' | b'-' | b'e')) {
        return None;
    }
    digits.parse::<f64>().ok().filter(|value| value.is_finite())
}

fn normalize(components: &mut [Component]) {
    if components.iter().any(|component| matches!(component.percentage, Percentage::Calc(_))) {
        return;
    }
    let specified: f64 = components
        .iter()
        .filter_map(|component| match component.percentage {
            Percentage::Number(value) => Some(value),
            _ => None,
        })
        .sum();
    let missing = components.iter().filter(|component| matches!(component.percentage, Percentage::None)).count();
    if missing > 0 && missing < components.len() {
        let share = ((100.0 - specified) / missing as f64).max(0.0);
        for component in components.iter_mut() {
            if matches!(component.percentage, Percentage::None) {
                component.percentage = Percentage::Number(share);
            }
        }
    }
    if missing == components.len() {
        return;
    }
    let even = 100.0 / components.len() as f64;
    let all_even = components.iter().all(|component| matches!(component.percentage, Percentage::Number(value) if (value - even).abs() < 1e-9));
    if all_even {
        for component in components.iter_mut() {
            component.percentage = Percentage::None;
        }
    }
}

fn number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        assert_eq!(canonicalize("palette-mix(in oklab, light 30%, dark)", false).unwrap(), "palette-mix(light 30%, dark 70%)");
        assert_eq!(canonicalize("palette-mix(in srgb, light 50%, dark 50%)", false).unwrap(), "palette-mix(in srgb, light, dark)");
        assert_eq!(canonicalize("palette-mix(in srgb, dark 100%)", false).unwrap(), "palette-mix(in srgb, dark)");
        assert_eq!(canonicalize("palette-mix(in hsl shorter hue, light, dark 90%)", false).unwrap(), "palette-mix(in hsl, light 10%, dark 90%)");
        assert!(canonicalize("palette-mix(in oklab dark 60%, light 40%)", false).is_none());
        assert!(canonicalize("palette-mix(in oklab, dark -10%, light 40%)", false).is_none());
    }
}
