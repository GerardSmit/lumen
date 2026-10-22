//! `<basic-shape>`, geometry-box and motion-path properties. Values are kept as
//! canonical computed strings in the generic property store; parsing,
//! serialization, interpolation and composition all work on one typed model.
use super::*;

mod basic;
mod geom;
mod paths;
mod properties;
mod shape_fn;

use basic::Op;
use geom::{Env, Form};
use properties::parse;
pub(crate) use properties::Prop;

pub(crate) fn compute(prop: Prop, raw: &str, context: LengthContext) -> Option<String> {
    let parsed = parse(prop, raw, Env::new(context))?;
    Some(parsed.compute(prop).text(prop, Form::Computed))
}

pub(crate) fn specified(prop: Prop, raw: &str) -> Option<String> {
    let parsed = parse(prop, raw, Env::new(static_length_context()))?;
    Some(parsed.text(prop, Form::Specified))
}

fn combine(prop: Prop, a: &str, b: &str, op: Op) -> Option<String> {
    let env = Env::new(static_length_context());
    let a = parse(prop, a, env)?.compute(prop);
    let b = parse(prop, b, env)?.compute(prop);
    Some(properties::mix(prop, &a, &b, op)?.text(prop, Form::Computed))
}

pub(crate) fn interpolate(prop: Prop, from: &str, to: &str, progress: f64) -> Option<String> {
    if !progress.is_finite() {
        return None;
    }
    combine(prop, from, to, Op::Interpolate(progress))
}

pub(crate) fn add(prop: Prop, base: &str, value: &str) -> Option<String> {
    combine(prop, base, value, Op::Add)
}

pub(crate) fn accumulate(prop: Prop, base: &str, value: &str, count: f64) -> Option<String> {
    if !count.is_finite() {
        return None;
    }
    combine(prop, base, value, Op::Accumulate(count))
}

pub(crate) fn svg_reference(computed: &str) -> Option<Arc<str>> {
    if !computed.starts_with("url(") {
        return None;
    }
    background_url(computed).filter(|reference| reference.starts_with('#') && reference.len() > 1)
}

pub(crate) const OFFSET_LONGHANDS: [&str; 5] =
    ["offset-position", "offset-path", "offset-distance", "offset-rotate", "offset-anchor"];

fn is_path_token(token: &str) -> bool {
    geom::lower(token) == "none"
        || basic::is_shape_function(token)
        || background_url(token).is_some()
        || ["content-box", "padding-box", "border-box", "fill-box", "stroke-box", "view-box"]
            .iter()
            .any(|keyword| token.eq_ignore_ascii_case(keyword))
}

fn parse_distance_rotate(tokens: &[&str], env: Env) -> Option<(Option<String>, Option<String>)> {
    let distance_ok = |token: &str| geom::parse_len(token, env, false).is_some();
    let rotate_ok = |tokens: &[&str]| parse(Prop::OffsetRotate, &tokens.join(" "), env).is_some();
    match tokens.len() {
        0 => return Some((None, None)),
        1 => {
            if distance_ok(tokens[0]) {
                return Some((Some(tokens[0].into()), None));
            }
            if rotate_ok(tokens) {
                return Some((None, Some(tokens[0].into())));
            }
        }
        _ => {}
    }
    if tokens.len() <= 3 {
        if distance_ok(tokens[0]) && rotate_ok(&tokens[1..]) && tokens.len() > 1 {
            return Some((Some(tokens[0].into()), Some(tokens[1..].join(" "))));
        }
        for rotate_len in 1..=2usize.min(tokens.len()) {
            let rest = &tokens[rotate_len..];
            if rotate_ok(&tokens[..rotate_len]) && rest.len() <= 1 && rest.first().is_none_or(|token| distance_ok(token)) {
                return Some((rest.first().map(|token| (*token).into()), Some(tokens[..rotate_len].join(" "))));
            }
        }
    }
    None
}

pub(crate) fn offset_shorthand(raw: &str) -> Option<[String; 5]> {
    let trimmed = raw.trim();
    if ["initial", "inherit", "unset", "revert", "revert-layer"].iter().any(|keyword| trimmed.eq_ignore_ascii_case(keyword)) {
        return Some(core::array::from_fn(|_| geom::lower(trimmed).into_owned()));
    }
    let env = Env::new(static_length_context());
    let halves = top_level_split(raw.trim(), b'/', 3)?;
    if halves.len() > 2 {
        return None;
    }
    let anchor = match halves.get(1) {
        Some(half) => {
            parse(Prop::OffsetAnchor, half, env)?;
            (*half).trim().to_owned()
        }
        None => String::from("auto"),
    };
    let tokens = components(halves[0])?;
    if tokens.is_empty() {
        return None;
    }
    let path_index = tokens.iter().position(|token| is_path_token(token)).unwrap_or(tokens.len());
    let position_tokens = &tokens[..path_index];
    let position = if position_tokens.is_empty() {
        String::from("normal")
    } else {
        let joined = position_tokens.join(" ");
        parse(Prop::OffsetPosition, &joined, env)?;
        joined
    };
    if path_index == tokens.len() {
        return Some([position, String::from("none"), String::from("0px"), String::from("auto"), anchor]);
    }
    let mut path_end = path_index + 1;
    if let Some(next) = tokens.get(path_end) {
        let first_is_box = geom::lower(tokens[path_index]).ends_with("-box");
        let next_is_box = geom::lower(next).ends_with("-box");
        if first_is_box != next_is_box {
            path_end += 1;
        }
    }
    let path = tokens[path_index..path_end].join(" ");
    parse(Prop::OffsetPath, &path, env)?;
    let (distance, rotate) = parse_distance_rotate(&tokens[path_end..], env)?;
    Some([
        position,
        path,
        distance.unwrap_or_else(|| String::from("0px")),
        rotate.unwrap_or_else(|| String::from("auto")),
        anchor,
    ])
}

pub(crate) fn offset_longhands(raw: &str) -> Option<[(&'static str, String); 5]> {
    let values = offset_shorthand(raw)?;
    let [position, path, distance, rotate, anchor] = values;
    Some([
        (OFFSET_LONGHANDS[0], position),
        (OFFSET_LONGHANDS[1], path),
        (OFFSET_LONGHANDS[2], distance),
        (OFFSET_LONGHANDS[3], rotate),
        (OFFSET_LONGHANDS[4], anchor),
    ])
}

pub(crate) fn computed_offset(style: &Style) -> Option<String> {
    let mut values = [""; 5];
    let owned: Vec<String> = OFFSET_LONGHANDS.iter().map(|name| extra_props::computed(style, name)).collect::<Option<_>>()?;
    for (slot, value) in values.iter_mut().zip(&owned) {
        *slot = value;
    }
    offset_shorthand_text(values)
}

pub(crate) fn offset_shorthand_text(values: [&str; 5]) -> Option<String> {
    let env = Env::new(static_length_context());
    let [position, path, distance, rotate, anchor] = values;
    let mut parts: Vec<String> = Vec::new();
    if position != "normal" {
        parts.push(position.to_owned());
    }
    let distance_default = parse(Prop::OffsetDistance, distance, env)?.compute(Prop::OffsetDistance).text(Prop::OffsetDistance, Form::Computed) == "0px";
    let rotate_default = {
        let parsed = parse(Prop::OffsetRotate, rotate, env)?;
        parsed.compute(Prop::OffsetRotate).text(Prop::OffsetRotate, Form::Computed) == "auto"
            && !rotate.split_whitespace().any(|token| token.eq_ignore_ascii_case("reverse"))
    };
    if path != "none" || !distance_default || !rotate_default {
        parts.push(path.to_owned());
    }
    if !distance_default {
        parts.push(distance.to_owned());
    }
    if !rotate_default {
        parts.push(rotate.to_owned());
    }
    let mut out = parts.join(" ");
    if out.is_empty() {
        out = String::from(if position == "normal" { "normal" } else { "auto" });
    }
    if anchor != "auto" {
        out.push_str(" / ");
        out.push_str(anchor);
    }
    Some(out)
}
