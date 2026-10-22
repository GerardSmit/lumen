//! `font-variation-settings` is a map from axis tag to number: animation is pairwise
//! over identical tag sets, and composition adds matching axes.
use super::*;
use super::extra_props as extra;

type Axes = Vec<(String, f64)>;

fn parse(value: &str) -> Option<Axes> {
    let mut axes: Axes = Vec::new();
    for item in value.split(',') {
        let item = item.trim();
        let quote = item.chars().next().filter(|quote| matches!(quote, '"' | '\''))?;
        let end = item[1..].find(quote)? + 1;
        let tag = &item[1..end];
        let number: f64 = item[end + 1..].trim().parse().ok()?;
        match axes.iter_mut().find(|(known, _)| known == tag) {
            Some(axis) => axis.1 = number,
            None => axes.push((String::from(tag), number)),
        }
    }
    Some(axes)
}

/// Sampled progress values carry single-precision noise; six significant digits
/// are what a computed axis value can represent.
fn significant(number: f64) -> f64 {
    if number == 0.0 || !number.is_finite() {
        return number;
    }
    let scale = libm::pow(10.0, 5.0 - libm::floor(libm::log10(number.abs())));
    libm::round(number * scale) / scale
}

fn serialize(axes: &[(String, f64)]) -> String {
    axes.iter()
        .map(|(tag, number)| alloc::format!("\"{tag}\" {}", super::computed_values::number(significant(*number) as f32)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn combine(from: &str, to: &str, operation: impl Fn(f64, f64) -> f64) -> Option<String> {
    let from = parse(from)?;
    let to = parse(to)?;
    if from.len() != to.len() {
        return None;
    }
    let mut out = Vec::with_capacity(from.len());
    for (tag, a) in &from {
        let (_, b) = to.iter().find(|(known, _)| known == tag)?;
        out.push((tag.clone(), operation(*a, *b)));
    }
    Some(serialize(&out))
}

pub(crate) fn interpolate(from: &str, to: &str, progress: f64) -> Option<String> {
    combine(from, to, |a, b| extra::mix(a, b, progress))
}

pub(crate) fn compose(base: &str, value: &str) -> Option<String> {
    combine(base, value, |a, b| a + b)
}
