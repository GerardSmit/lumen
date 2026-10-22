//! Interpolation of `filter` function lists (Filter Effects 1 §10).
use alloc::{format, string::String, vec::Vec};

#[derive(Clone, Debug, PartialEq)]
enum Function {
    Blur(f64),
    Amount(&'static str, f64),
    HueRotate(f64),
    DropShadow { color: String, x: f64, y: f64, blur: f64 },
}

impl Function {
    fn kind(&self) -> &'static str {
        match self {
            Function::Blur(_) => "blur",
            Function::Amount(name, _) => *name,
            Function::HueRotate(_) => "hue-rotate",
            Function::DropShadow { .. } => "drop-shadow",
        }
    }

    fn identity(&self) -> Function {
        match self {
            Function::Blur(_) => Function::Blur(0.0),
            Function::Amount(name, _) => Function::Amount(*name, 1.0),
            Function::HueRotate(_) => Function::HueRotate(0.0),
            Function::DropShadow { .. } => Function::DropShadow { color: "rgba(0, 0, 0, 0)".into(), x: 0.0, y: 0.0, blur: 0.0 },
        }
    }
}

const AMOUNTS: [&str; 7] = ["brightness", "contrast", "grayscale", "invert", "opacity", "saturate", "sepia"];

fn length_px(token: &str) -> Option<f64> {
    let (value, unit) = super::numeric_unit(token)?;
    let scale = match unit.to_ascii_lowercase().as_str() {
        "px" => 1.0,
        "in" => 96.0,
        "cm" => 96.0 / 2.54,
        "mm" => 96.0 / 25.4,
        "q" => 96.0 / 101.6,
        "pt" => 96.0 / 72.0,
        "pc" => 16.0,
        "" if value == 0.0 => 1.0,
        _ => return None,
    };
    Some(value * scale)
}

fn angle_deg(token: &str) -> Option<f64> {
    let (value, unit) = super::numeric_unit(token)?;
    let scale = match unit.to_ascii_lowercase().as_str() {
        "deg" => 1.0,
        "grad" => 0.9,
        "rad" => 180.0 / core::f64::consts::PI,
        "turn" => 360.0,
        "" if value == 0.0 => 1.0,
        _ => return None,
    };
    Some(value * scale)
}

fn amount(token: &str) -> Option<f64> {
    let (value, unit) = super::numeric_unit(token)?;
    let value = match unit {
        "" => value,
        "%" => value / 100.0,
        _ => return None,
    };
    (value >= 0.0).then_some(value)
}

fn parse_function(token: &str) -> Option<Function> {
    let (name, rest) = token.split_once('(')?;
    let arguments = rest.strip_suffix(')')?.trim();
    let name = name.trim().to_ascii_lowercase();
    if name == "blur" {
        let value = if arguments.is_empty() { 0.0 } else { length_px(arguments)? };
        return (value >= 0.0).then_some(Function::Blur(value));
    }
    if name == "hue-rotate" {
        return Some(Function::HueRotate(if arguments.is_empty() { 0.0 } else { angle_deg(arguments)? }));
    }
    if let Some(&amount_name) = AMOUNTS.iter().find(|candidate| **candidate == name) {
        return Some(Function::Amount(amount_name, if arguments.is_empty() { 1.0 } else { amount(arguments)? }));
    }
    if name == "drop-shadow" {
        let mut lengths = Vec::new();
        let mut color = None;
        for part in super::value_range::split_components(arguments) {
            match length_px(part) {
                Some(length) => lengths.push(length),
                None => {
                    if color.replace(part).is_some() { return None; }
                }
            }
        }
        if !(2..=3).contains(&lengths.len()) { return None; }
        let color = color?;
        if color.eq_ignore_ascii_case("currentcolor") { return None; }
        return Some(Function::DropShadow {
            color: color.into(), x: lengths[0], y: lengths[1], blur: lengths.get(2).copied().unwrap_or(0.0),
        });
    }
    None
}

fn parse_list(input: &str) -> Option<Vec<Function>> {
    let input = input.trim();
    if input.eq_ignore_ascii_case("none") { return Some(Vec::new()); }
    let tokens = super::value_range::split_components(input);
    if tokens.is_empty() || tokens.len() > 32 { return None; }
    tokens.into_iter().map(parse_function).collect()
}

use lumen_common::numeric::lerp as mix;

fn interpolate_function(a: &Function, b: &Function, progress: f64) -> Option<Function> {
    Some(match (a, b) {
        (Function::Blur(a), Function::Blur(b)) => Function::Blur(mix(*a, *b, progress).max(0.0)),
        (Function::HueRotate(a), Function::HueRotate(b)) => Function::HueRotate(mix(*a, *b, progress)),
        (Function::Amount(name, a), Function::Amount(_, b)) => {
            let value = mix(*a, *b, progress).max(0.0);
            let bounded = matches!(*name, "grayscale" | "invert" | "opacity" | "sepia");
            Function::Amount(*name, if bounded { value.min(1.0) } else { value })
        }
        (Function::DropShadow { color: ca, x: xa, y: ya, blur: ba }, Function::DropShadow { color: cb, x: xb, y: yb, blur: bb }) => {
            let current = lumen_common::color::Color::rgba8([0, 0, 0, 255]);
            let from = crate::css::parse_animation_unclipped_color(ca, current)?;
            let to = crate::css::parse_animation_unclipped_color(cb, current)?;
            let color = crate::css::interpolate_source_colors(&from, &to, progress as f32).serialize()?;
            Function::DropShadow { color, x: mix(*xa, *xb, progress), y: mix(*ya, *yb, progress), blur: mix(*ba, *bb, progress).max(0.0) }
        }
        _ => return None,
    })
}

fn serialize_function(function: &Function) -> Option<String> {
    let number = |value: f64, unit: &str| super::serialize_numeric(value, unit);
    Some(match function {
        Function::Blur(value) => format!("blur({})", number(*value, "px")?),
        Function::HueRotate(value) => format!("hue-rotate({})", number(*value, "deg")?),
        Function::Amount(name, value) => format!("{name}({})", number(*value, "")?),
        Function::DropShadow { color, x, y, blur } => {
            format!("drop-shadow({color} {} {} {})", number(*x, "px")?, number(*y, "px")?, number(*blur, "px")?)
        }
    })
}

/// Lists interpolate function by function; the shorter of two lists with
/// matching leading functions is padded with identity functions, and `none`
/// is the empty list.
pub fn interpolate_filter_lists(from: &str, to: &str, progress: f64) -> Option<String> {
    if !progress.is_finite() { return None; }
    let (a, b) = (parse_list(from)?, parse_list(to)?);
    let shared = a.len().min(b.len());
    if a.iter().zip(&b).take(shared).any(|(a, b)| a.kind() != b.kind()) { return None; }
    let count = a.len().max(b.len());
    if count == 0 { return Some("none".into()); }
    let mut output = Vec::with_capacity(count);
    for index in 0..count {
        let (a, b) = match (a.get(index), b.get(index)) {
            (Some(a), Some(b)) => (a.clone(), b.clone()),
            (Some(a), None) => (a.clone(), a.identity()),
            (None, Some(b)) => (b.identity(), b.clone()),
            (None, None) => return None,
        };
        output.push(serialize_function(&interpolate_function(&a, &b, progress)?)?);
    }
    Some(output.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_pad_with_identity_and_convert_units() {
        assert_eq!(interpolate_filter_lists("none", "blur(10px)", 0.5).as_deref(), Some("blur(5px)"));
        assert_eq!(interpolate_filter_lists("blur(6px)", "blur(10px) hue-rotate(180deg)", 0.5).as_deref(), Some("blur(8px) hue-rotate(90deg)"));
        assert_eq!(interpolate_filter_lists("hue-rotate(100grad)", "hue-rotate(0deg)", 0.5).as_deref(), Some("hue-rotate(45deg)"));
        assert_eq!(interpolate_filter_lists("opacity(0)", "none", 1.5).as_deref(), Some("opacity(1)"));
        assert_eq!(interpolate_filter_lists("blur(1px)", "hue-rotate(1deg)", 0.5), None);
        assert_eq!(interpolate_filter_lists("grayscale(0)", "grayscale()", 0.5).as_deref(), Some("grayscale(0.5)"));
    }
}
