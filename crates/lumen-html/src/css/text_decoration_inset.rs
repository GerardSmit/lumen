//! Animation of `text-decoration-inset`: `auto | <length-percentage>{1,2}`. `auto`
//! animates as `0px` against a length and stays `auto` against `auto`.
use super::*;
use super::extra_props as extra;

fn parse_pair(raw: &str) -> Option<[TransformLength; 2]> {
    let context = static_length_context();
    let query = extra::query_for(context);
    let tokens = super::components(raw.trim())?;
    let parse = |token: &str| {
        if token.eq_ignore_ascii_case("auto") {
            Some(TransformLength { pixels: 0.0, percent: 0.0 })
        } else {
            transform_length_with_query(token, context, query)
        }
    };
    match tokens.as_slice() {
        [one] => {
            let value = parse(one)?;
            Some([value, value])
        }
        [first, second] => Some([parse(first)?, parse(second)?]),
        _ => None,
    }
}

fn serialize(values: [(f32, f32); 2]) -> String {
    let [first, second] = values.map(|(pixels, percent)| computed_values::percentage(pixels, percent));
    if first == second { first } else { alloc::format!("{first} {second}") }
}

fn combine(from: &str, to: &str, mix: impl Fn(f32, f32) -> f32) -> Option<String> {
    let (a, b) = (parse_pair(from)?, parse_pair(to)?);
    Some(serialize([0, 1].map(|index| (mix(a[index].pixels, b[index].pixels), mix(a[index].percent, b[index].percent)))))
}

fn is_auto(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case("auto")
}

pub(crate) fn interpolate(from: &str, to: &str, progress: f64) -> Option<String> {
    if is_auto(from) && is_auto(to) {
        return Some(String::from("auto"));
    }
    combine(from, to, |a, b| extra::mix(f64::from(a), f64::from(b), progress) as f32)
}

pub(crate) fn compose(base: &str, value: &str) -> Option<String> {
    if is_auto(base) && is_auto(value) {
        return Some(String::from("auto"));
    }
    combine(base, value, |a, b| a + b)
}
