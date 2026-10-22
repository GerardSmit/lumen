//! Interpolation and addition of computed `grid-template-columns/rows` track
//! lists (CSS Grid 2 section 7.2.3 and 7.2.4).
use super::extra_props::{mix, mix_lp};
use super::*;
use alloc::{string::String, vec::Vec};

pub(crate) fn is_track_list_property(name: &str) -> bool {
    matches!(name, "grid-template-columns" | "grid-template-rows" | "flow-tolerance")
}

#[derive(Clone, Debug, PartialEq)]
enum Comp {
    Lp(String),
    Fr(f64),
    Kw(String),
}

#[derive(Clone, Debug, PartialEq)]
enum Track {
    Lp(String),
    Fr(f64),
    Kw(String),
    Fit(String),
    Minmax(Comp, Comp),
}

#[derive(Clone, Debug, PartialEq)]
enum Item {
    Names(String),
    Track(Track),
    Repeat(usize, Vec<Item>),
}

fn tokens(raw: &str) -> Option<Vec<&str>> {
    let bytes = raw.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = None;
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'(' | b'[' => {
                depth += 1;
                start.get_or_insert(index);
            }
            b')' | b']' => {
                depth = depth.checked_sub(1)?;
            }
            b' ' | b'\t' | b'\n' if depth == 0 => {
                if let Some(begin) = start.take() {
                    out.push(&raw[begin..index]);
                }
            }
            _ => {
                start.get_or_insert(index);
            }
        }
    }
    if depth != 0 {
        return None;
    }
    if let Some(begin) = start {
        out.push(&raw[begin..]);
    }
    Some(out)
}

fn function_argument<'a>(token: &'a str, name: &str) -> Option<&'a str> {
    let head = token.get(..name.len() + 1)?;
    if !head[..name.len()].eq_ignore_ascii_case(name) || !head.ends_with('(') || !token.ends_with(')') {
        return None;
    }
    Some(&token[name.len() + 1..token.len() - 1])
}

fn length_percentage(token: &str) -> Option<String> {
    let first = token.bytes().next()?;
    let numeric = first.is_ascii_digit() || matches!(first, b'.' | b'-' | b'+');
    if numeric || function_argument(token, "calc").is_some() {
        Some(token.to_owned())
    } else {
        None
    }
}

fn flex(token: &str) -> Option<f64> {
    let number = token.strip_suffix("fr").or_else(|| token.strip_suffix("FR"))?;
    number.parse().ok()
}

fn component(token: &str) -> Option<Comp> {
    let token = token.trim();
    if let Some(value) = flex(token) {
        return Some(Comp::Fr(value));
    }
    if matches!(token, "auto" | "min-content" | "max-content") {
        return Some(Comp::Kw(token.to_owned()));
    }
    length_percentage(token).map(Comp::Lp)
}

fn track(token: &str) -> Option<Track> {
    if let Some(argument) = function_argument(token, "fit-content") {
        return length_percentage(argument.trim()).map(Track::Fit);
    }
    if let Some(argument) = function_argument(token, "minmax") {
        let parts = top_level_split(argument, b',', usize::MAX)?;
        if parts.len() != 2 {
            return None;
        }
        return Some(Track::Minmax(component(parts[0])?, component(parts[1])?));
    }
    Some(match component(token)? {
        Comp::Lp(value) => Track::Lp(value),
        Comp::Fr(value) => Track::Fr(value),
        Comp::Kw(value) => Track::Kw(value),
    })
}

fn parse_items(raw: &str, depth: usize) -> Option<Vec<Item>> {
    if depth > 2 {
        return None;
    }
    let mut items = Vec::new();
    for token in tokens(raw)? {
        if token.starts_with('[') {
            items.push(Item::Names(token.to_owned()));
        } else if let Some(argument) = function_argument(token, "repeat") {
            let parts = top_level_split(argument, b',', usize::MAX)?;
            if parts.len() != 2 {
                return None;
            }
            let count: usize = parts[0].parse().ok()?;
            items.push(Item::Repeat(count, parse_items(parts[1], depth + 1)?));
        } else {
            items.push(Item::Track(track(token)?));
        }
    }
    Some(items)
}

fn parse(raw: &str) -> Option<Vec<Item>> {
    let items = parse_items(raw.trim(), 0)?;
    items.iter().any(|item| !matches!(item, Item::Names(_))).then_some(items)
}

fn comp_text(comp: &Comp) -> String {
    match comp {
        Comp::Lp(value) | Comp::Kw(value) => value.clone(),
        Comp::Fr(value) => alloc::format!("{}fr", super::computed_values::number((*value) as f32)),
    }
}

fn track_text(track: &Track) -> String {
    match track {
        Track::Lp(value) | Track::Kw(value) => value.clone(),
        Track::Fr(value) => alloc::format!("{}fr", super::computed_values::number((*value) as f32)),
        Track::Fit(value) => alloc::format!("fit-content({value})"),
        Track::Minmax(min, max) => alloc::format!("minmax({}, {})", comp_text(min), comp_text(max)),
    }
}

fn serialize(items: &[Item]) -> String {
    let mut parts = Vec::with_capacity(items.len());
    for item in items {
        parts.push(match item {
            Item::Names(text) => text.clone(),
            Item::Track(track) => track_text(track),
            Item::Repeat(count, inner) => alloc::format!("repeat({count}, {})", serialize(inner)),
        });
    }
    parts.join(" ")
}

fn clamp_length(value: String) -> String {
    match value.strip_prefix('-') {
        Some(rest) if rest.ends_with("px") && !rest.contains('(') && !value.contains('%') => String::from("0px"),
        _ => value,
    }
}

fn sum_length(a: &str, b: &str) -> Option<String> {
    let context = static_length_context();
    let query = extra_props::query_for(context);
    let a = transform_length_with_query(a, context, query)?;
    let b = transform_length_with_query(b, context, query)?;
    Some(computed_values::percentage(a.pixels + b.pixels, a.percent + b.percent))
}

fn view(track: &Track) -> Option<(Comp, Comp)> {
    Some(match track {
        Track::Lp(value) => (Comp::Lp(value.clone()), Comp::Lp(value.clone())),
        Track::Fr(value) => (Comp::Kw(String::from("auto")), Comp::Fr(*value)),
        Track::Kw(value) => (Comp::Kw(value.clone()), Comp::Kw(value.clone())),
        Track::Minmax(min, max) => (min.clone(), max.clone()),
        Track::Fit(_) => return None,
    })
}

fn flip<T: Clone>(a: &T, b: &T, progress: f64) -> T {
    if progress < 0.5 { a.clone() } else { b.clone() }
}

fn mix_component(a: &Comp, b: &Comp, progress: f64) -> Option<Comp> {
    match (a, b) {
        (Comp::Lp(a), Comp::Lp(b)) => Some(Comp::Lp(clamp_length(mix_lp(a, b, progress)?))),
        (Comp::Fr(a), Comp::Fr(b)) => Some(Comp::Fr(mix(*a, *b, progress).max(0.0))),
        (Comp::Kw(_) | Comp::Fr(_), Comp::Kw(_) | Comp::Fr(_)) => Some(flip(a, b, progress)),
        _ => None,
    }
}

fn mix_track(a: &Track, b: &Track, progress: f64) -> Option<Track> {
    match (a, b) {
        (Track::Fit(a), Track::Fit(b)) => return Some(Track::Fit(clamp_length(mix_lp(a, b, progress)?))),
        (Track::Fit(_), Track::Kw(_)) | (Track::Kw(_), Track::Fit(_)) => return Some(flip(a, b, progress)),
        (Track::Fit(_), _) | (_, Track::Fit(_)) => return None,
        _ => {}
    }
    let (min_a, max_a) = view(a)?;
    let (min_b, max_b) = view(b)?;
    let min = mix_component(&min_a, &min_b, progress)?;
    let max = mix_component(&max_a, &max_b, progress)?;
    Some(match (a, b) {
        (Track::Lp(_), Track::Lp(_)) => match min {
            Comp::Lp(value) => Track::Lp(value),
            _ => return None,
        },
        (Track::Fr(_), Track::Fr(_)) => match max {
            Comp::Fr(value) => Track::Fr(value),
            _ => return None,
        },
        (Track::Minmax(..), _) | (_, Track::Minmax(..)) => Track::Minmax(min, max),
        _ => flip(a, b, progress),
    })
}

fn add_component(base: &Comp, value: &Comp) -> Comp {
    match (base, value) {
        (Comp::Lp(a), Comp::Lp(b)) => sum_length(a, b).map_or_else(|| value.clone(), Comp::Lp),
        (Comp::Fr(a), Comp::Fr(b)) => Comp::Fr(a + b),
        _ => value.clone(),
    }
}

fn add_track(base: &Track, value: &Track) -> Track {
    match (base, value) {
        (Track::Fit(a), Track::Fit(b)) => sum_length(a, b).map_or_else(|| value.clone(), Track::Fit),
        (Track::Fit(_), _) | (_, Track::Fit(_)) => value.clone(),
        _ => {
            let (Some((min_a, max_a)), Some((min_b, max_b))) = (view(base), view(value)) else {
                return value.clone();
            };
            let min = add_component(&min_a, &min_b);
            let max = add_component(&max_a, &max_b);
            match (base, value) {
                (Track::Lp(_), Track::Lp(_)) => match min {
                    Comp::Lp(sum) => Track::Lp(sum),
                    _ => value.clone(),
                },
                (Track::Fr(_), Track::Fr(_)) => match max {
                    Comp::Fr(sum) => Track::Fr(sum),
                    _ => value.clone(),
                },
                (Track::Minmax(..), _) | (_, Track::Minmax(..)) => Track::Minmax(min, max),
                _ => value.clone(),
            }
        }
    }
}

fn pair_items(
    a: &[Item],
    b: &[Item],
    combine: &dyn Fn(&Item, &Item) -> Option<Item>,
    names_from_second: bool,
) -> Option<Vec<Item>> {
    let first: Vec<&Item> = a.iter().filter(|item| !matches!(item, Item::Names(_))).collect();
    let second: Vec<&Item> = b.iter().filter(|item| !matches!(item, Item::Names(_))).collect();
    if first.len() != second.len() {
        return None;
    }
    let mut combined = Vec::with_capacity(first.len());
    for (x, y) in first.iter().zip(second.iter()) {
        combined.push(combine(x, y)?);
    }
    let mut combined = combined.into_iter();
    let chosen = if names_from_second { b } else { a };
    let mut out = Vec::with_capacity(chosen.len());
    for item in chosen {
        out.push(match item {
            Item::Names(_) => item.clone(),
            _ => combined.next()?,
        });
    }
    Some(out)
}

fn mix_items(a: &[Item], b: &[Item], progress: f64) -> Option<Vec<Item>> {
    let combine = |x: &Item, y: &Item| -> Option<Item> {
        match (x, y) {
            (Item::Track(x), Item::Track(y)) => Some(Item::Track(mix_track(x, y, progress)?)),
            (Item::Repeat(count_x, inner_x), Item::Repeat(count_y, inner_y)) if count_x == count_y => {
                Some(Item::Repeat(*count_x, mix_items(inner_x, inner_y, progress)?))
            }
            _ => None,
        }
    };
    pair_items(a, b, &combine, progress >= 0.5)
}

fn add_items(base: &[Item], value: &[Item]) -> Option<Vec<Item>> {
    let combine = |x: &Item, y: &Item| -> Option<Item> {
        match (x, y) {
            (Item::Track(x), Item::Track(y)) => Some(Item::Track(add_track(x, y))),
            (Item::Repeat(count_x, inner_x), Item::Repeat(count_y, inner_y)) if count_x == count_y => {
                Some(Item::Repeat(*count_x, add_items(inner_x, inner_y)?))
            }
            _ => None,
        }
    };
    pair_items(base, value, &combine, true)
}

fn plain_length(raw: &str) -> Option<String> {
    length_percentage(raw.trim())
}

pub(crate) fn interpolate(property: &str, from: &str, to: &str, progress: f64) -> Option<String> {
    if !progress.is_finite() {
        return None;
    }
    if property == "flow-tolerance" {
        return mix_lp(&plain_length(from)?, &plain_length(to)?, progress).map(clamp_length);
    }
    let from = parse(from)?;
    let to = parse(to)?;
    Some(serialize(&mix_items(&from, &to, progress)?))
}

pub(crate) fn compose(property: &str, base: &str, value: &str) -> Option<String> {
    if property == "flow-tolerance" {
        return sum_length(&plain_length(base)?, &plain_length(value)?);
    }
    let base = parse(base)?;
    let value = parse(value)?;
    Some(serialize(&add_items(&base, &value)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_matching_lists_and_flips_incompatible_tracks() {
        assert_eq!(interpolate("grid-template-columns", "1fr 1fr 1fr", "2fr auto 2fr", 0.4).as_deref(), Some("1.4fr 1fr 1.4fr"));
        assert_eq!(interpolate("grid-template-columns", "1fr 1fr 1fr", "2fr auto 2fr", 0.6).as_deref(), Some("1.6fr auto 1.6fr"));
        assert_eq!(interpolate("grid-template-columns", "10px 10px", "1fr 1fr", 0.5), None);
        assert_eq!(interpolate("grid-template-columns", "10px 20px", "20px", 0.5), None);
        assert_eq!(
            interpolate("grid-template-columns", "1fr [a b] 1fr [d] 1fr", "2fr [c] auto 2fr", 0.6).as_deref(),
            Some("1.6fr [c] auto 1.6fr")
        );
    }

    #[test]
    fn adds_matching_tracks() {
        assert_eq!(compose("grid-template-columns", "1fr 1fr", "1fr [a] auto").as_deref(), Some("2fr [a] auto"));
    }
}
