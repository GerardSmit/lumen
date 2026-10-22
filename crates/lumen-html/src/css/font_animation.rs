//! Interpolation and composition of the font properties whose animation type
//! needs more than numeric mixing: `font-stretch`, `font-style`,
//! `font-size-adjust` and the keyword form of `font-size`.
use alloc::borrow::ToOwned;
use alloc::string::String;

use crate::animation::serialize_numeric;

const ITALIC_ANGLE: f64 = 14.0;

fn mix(a: f64, b: f64, progress: f64) -> f64 {
    a + (b - a) * progress
}

fn stretch_percent(value: &str) -> Option<f64> {
    let value = value.trim();
    let keyword = match value.to_ascii_lowercase().as_str() {
        "ultra-condensed" => Some(50.0),
        "extra-condensed" => Some(62.5),
        "condensed" => Some(75.0),
        "semi-condensed" => Some(87.5),
        "normal" => Some(100.0),
        "semi-expanded" => Some(112.5),
        "expanded" => Some(125.0),
        "extra-expanded" => Some(150.0),
        "ultra-expanded" => Some(200.0),
        _ => None,
    };
    if keyword.is_some() {
        return keyword;
    }
    let (number, unit) = lumen_common::numeric::split_number_unit(value)?;
    (unit == "%" && number.is_finite()).then_some(number)
}

fn interpolate_stretch(from: &str, to: &str, progress: f64) -> Option<String> {
    let a = stretch_percent(from)?;
    let b = stretch_percent(to)?;
    serialize_numeric(mix(a, b, progress).max(0.0), "%")
}

fn oblique_angle(value: &str) -> Option<f64> {
    let value = value.trim().to_ascii_lowercase();
    match value.as_str() {
        "normal" => return Some(0.0),
        "oblique" => return Some(ITALIC_ANGLE),
        _ => {}
    }
    let angle = value.strip_prefix("oblique")?.trim();
    let (number, unit) = lumen_common::numeric::split_number_unit(angle)?;
    let degrees = match unit {
        "deg" => number,
        "grad" => number * 0.9,
        "rad" => number.to_degrees(),
        "turn" => number * 360.0,
        _ => return None,
    };
    degrees.is_finite().then_some(degrees)
}

fn interpolate_style(from: &str, to: &str, progress: f64) -> Option<String> {
    let a = oblique_angle(from)?;
    let b = oblique_angle(to)?;
    if progress == 0.0 {
        return Some(from.trim().to_owned());
    }
    if progress == 1.0 {
        return Some(to.trim().to_owned());
    }
    let angle = mix(a, b, progress).clamp(-90.0, 90.0);
    if angle == 0.0 {
        return Some(String::from("normal"));
    }
    Some(alloc::format!("oblique {}", serialize_numeric(angle, "deg")?))
}

fn size_adjust_parts(value: &str) -> Option<(&'static str, f64)> {
    let mut parts = value.split_ascii_whitespace();
    let first = parts.next()?;
    let (metric, number) = match parts.next() {
        None => ("ex-height", first),
        Some(number) => {
            let metric = match first.to_ascii_lowercase().as_str() {
                "ex-height" => "ex-height",
                "cap-height" => "cap-height",
                "ch-width" => "ch-width",
                "ic-width" => "ic-width",
                "ic-height" => "ic-height",
                _ => return None,
            };
            (metric, number)
        }
    };
    if parts.next().is_some() {
        return None;
    }
    let number: f64 = number.parse().ok()?;
    number.is_finite().then_some((metric, number))
}

fn size_adjust_text(metric: &str, number: f64) -> Option<String> {
    let number = serialize_numeric(number.max(0.0), "")?;
    Some(if metric == "ex-height" { number } else { alloc::format!("{metric} {number}") })
}

fn interpolate_size_adjust(from: &str, to: &str, progress: f64) -> Option<String> {
    let (metric_a, a) = size_adjust_parts(from)?;
    let (metric_b, b) = size_adjust_parts(to)?;
    if metric_a != metric_b {
        return None;
    }
    size_adjust_text(metric_a, mix(a, b, progress))
}

fn compose_size_adjust(base: &str, value: &str) -> Option<String> {
    let (metric_base, a) = size_adjust_parts(base)?;
    let (metric_value, b) = size_adjust_parts(value)?;
    if metric_base != metric_value {
        return None;
    }
    size_adjust_text(metric_base, a + b)
}

/// Pixel size of an absolute-size keyword at the default medium size.
pub(crate) fn absolute_size_pixels(keyword: &str) -> Option<f64> {
    Some(match keyword.trim().to_ascii_lowercase().as_str() {
        "xx-small" => 9.0,
        "x-small" => 10.0,
        "small" => 13.0,
        "medium" => 16.0,
        "large" => 18.0,
        "x-large" => 24.0,
        "xx-large" => 32.0,
        "xxx-large" => 48.0,
        _ => return None,
    })
}

fn size_with_keywords(value: &str) -> Option<String> {
    absolute_size_pixels(value).and_then(|pixels| serialize_numeric(pixels, "px"))
}

/// `Some(result)` when the property is handled here; `result` is `None` when the
/// pair does not interpolate.
pub(crate) fn interpolate(property: &str, from: &str, to: &str, progress: f64) -> Option<Option<String>> {
    if !progress.is_finite() {
        return None;
    }
    Some(match property.to_ascii_lowercase().as_str() {
        "font-stretch" | "font-width" => interpolate_stretch(from, to, progress),
        "font-style" => interpolate_style(from, to, progress),
        "font-size-adjust" => interpolate_size_adjust(from, to, progress),
        _ => return None,
    })
}

/// Additive composition for the properties handled here; `None` when the
/// property is not one of them or the values do not add.
pub(crate) fn compose(property: &str, base: &str, value: &str) -> Option<String> {
    match property.to_ascii_lowercase().as_str() {
        "font-size-adjust" => compose_size_adjust(base, value),
        "font-stretch" | "font-width" => {
            serialize_numeric((stretch_percent(base)? + stretch_percent(value)?).max(0.0), "%")
        }
        "font-size" => {
            let (base, value) = (
                size_with_keywords(base).unwrap_or_else(|| base.to_owned()),
                size_with_keywords(value).unwrap_or_else(|| value.to_owned()),
            );
            crate::animation::add_numeric_values(&base, &value, None)
        }
        _ => None,
    }
}

/// Resolve absolute-size keywords of `font-size` endpoints to pixels so they
/// interpolate with lengths.
pub(crate) fn font_size_endpoints(from: &str, to: &str) -> Option<(String, String)> {
    let (resolved_from, resolved_to) = (size_with_keywords(from), size_with_keywords(to));
    if resolved_from.is_none() && resolved_to.is_none() {
        return None;
    }
    Some((resolved_from.unwrap_or_else(|| from.to_owned()), resolved_to.unwrap_or_else(|| to.to_owned())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stretch_keywords_interpolate_as_percentages() {
        assert_eq!(interpolate("font-stretch", "normal", "ultra-expanded", 0.25), Some(Some("125%".into())));
        assert_eq!(interpolate("font-stretch", "100%", "200%", -2.0), Some(Some("0%".into())));
    }

    #[test]
    fn oblique_angles_interpolate_and_normalize() {
        assert_eq!(interpolate("font-style", "normal", "oblique 10deg", 0.3), Some(Some("oblique 3deg".into())));
        assert_eq!(interpolate("font-style", "oblique -90deg", "oblique 90deg", 0.5), Some(Some("normal".into())));
        assert_eq!(interpolate("font-style", "oblique 5deg", "oblique 15deg", -2.0), Some(Some("oblique -15deg".into())));
    }

    #[test]
    fn size_adjust_adds_matching_metrics() {
        assert_eq!(compose("font-size-adjust", "cap-height 0.5", "cap-height 2"), Some("cap-height 2.5".into()));
        assert_eq!(compose("font-size-adjust", "0.5", "ex-height 2"), Some("2.5".into()));
        assert_eq!(compose("font-size-adjust", "cap-height 0.5", "ch-width 2"), None);
    }
}
