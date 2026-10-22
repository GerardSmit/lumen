//! `background-position-x`, `background-position-y` and the `background-position`
//! shorthand over them. The longhands live in `extra_props`' string slots; the
//! typed `Style::background_position` that layout reads is derived from them.
use super::*;
use super::extra_props as extra;
use lumen_common::numeric::lcm_capped;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Axis {
    X,
    Y,
}

const LONGHANDS: [&str; 2] = ["background-position-x", "background-position-y"];

pub(crate) fn is_shorthand(name: &str) -> bool {
    name == "background-position"
}

pub(crate) fn longhands(name: &str) -> &'static [&'static str] {
    if is_shorthand(name) { &LONGHANDS } else { &[] }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Edge {
    Start,
    End,
    Center,
}

struct Part<'a> {
    keyword: Option<&'a str>,
    edge: Option<Edge>,
    offset: Option<&'a str>,
}

fn edge(axis: Axis, token: &str, logical: bool) -> Option<Edge> {
    let (start, end, logical_start, logical_end) = match axis {
        Axis::X => ("left", "right", "x-start", "x-end"),
        Axis::Y => ("top", "bottom", "y-start", "y-end"),
    };
    if decoded_css_keyword(token, "center") {
        Some(Edge::Center)
    } else if decoded_css_keyword(token, start) || logical && decoded_css_keyword(token, logical_start) {
        Some(Edge::Start)
    } else if decoded_css_keyword(token, end) || logical && decoded_css_keyword(token, logical_end) {
        Some(Edge::End)
    } else {
        None
    }
}

fn offset(token: &str, context: LengthContext) -> Option<TransformLength> {
    transform_length_with_query(token, context, extra::query_for(context))
}

fn valid_offset(token: &str) -> bool {
    offset(token, static_length_context()).is_some()
}

fn axis_part<'a>(axis: Axis, tokens: &[&'a str], logical: bool) -> Option<Part<'a>> {
    match *tokens {
        [token] => Some(match edge(axis, token, logical) {
            Some(edge) => Part { keyword: Some(token), edge: Some(edge), offset: None },
            None => {
                if !valid_offset(token) {
                    return None;
                }
                Part { keyword: None, edge: None, offset: Some(token) }
            }
        }),
        [keyword, token] => {
            let edge = edge(axis, keyword, logical).filter(|edge| *edge != Edge::Center)?;
            if !valid_offset(token) {
                return None;
            }
            Some(Part { keyword: Some(keyword), edge: Some(edge), offset: Some(token) })
        }
        _ => None,
    }
}

fn layers(raw: &str) -> Option<Vec<&str>> {
    let layers = top_level_split(raw, b',', MAX_BACKGROUND_GEOMETRY_VALUES)?;
    layers.iter().all(|layer| !layer.trim().is_empty()).then_some(layers)
}

fn specified_offset(token: &str) -> Option<String> {
    if token == "0" {
        return Some(String::from("0px"));
    }
    typed_numeric::parse_numeric_expression(token)?.serialize_specified()
}

fn specified_part(part: &Part<'_>) -> Option<String> {
    let mut output = String::new();
    if let Some(keyword) = part.keyword {
        output.push_str(&extra::lower(keyword));
    }
    if let Some(token) = part.offset {
        if !output.is_empty() {
            output.push(' ');
        }
        output.push_str(&specified_offset(token)?);
    }
    Some(output)
}

pub(crate) fn specified(axis: Axis, raw: &str) -> Option<String> {
    let mut output = Vec::new();
    for layer in layers(raw)? {
        let part = axis_part(axis, &components(layer)?, true)?;
        output.push(specified_part(&part)?);
    }
    Some(output.join(", "))
}

fn resolve(part: &Part<'_>, context: LengthContext) -> Option<String> {
    let amount = match part.offset {
        Some(token) => offset(token, context)?,
        None => TransformLength { pixels: 0.0, percent: 0.0 },
    };
    let value = match part.edge {
        None => amount,
        Some(Edge::Start) => {
            if part.offset.is_none() {
                return Some(String::from("0%"));
            }
            amount
        }
        Some(Edge::Center) => return Some(String::from("50%")),
        Some(Edge::End) => {
            if part.offset.is_none() {
                return Some(String::from("100%"));
            }
            TransformLength { pixels: -amount.pixels, percent: 100.0 - amount.percent }
        }
    };
    if value.pixels == 0.0 && value.percent == 0.0 && part.offset.is_some_and(|token| token.contains('%')) {
        return Some(String::from("0%"));
    }
    Some(computed_values::percentage(value.pixels, value.percent))
}

pub(crate) fn compute(axis: Axis, raw: &str, context: LengthContext) -> Option<String> {
    let mut output = Vec::new();
    for layer in layers(raw)? {
        let part = axis_part(axis, &components(layer)?, true)?;
        output.push(resolve(&part, context)?);
    }
    Some(output.join(", "))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Word {
    X,
    Y,
    Center,
    Offset,
}

fn word(token: &str) -> Option<Word> {
    word_with(token, false)
}

fn word_with(token: &str, logical: bool) -> Option<Word> {
    if decoded_css_keyword(token, "center") {
        Some(Word::Center)
    } else if decoded_css_keyword(token, "left") || decoded_css_keyword(token, "right")
        || logical && (decoded_css_keyword(token, "x-start") || decoded_css_keyword(token, "x-end")) {
        Some(Word::X)
    } else if decoded_css_keyword(token, "top") || decoded_css_keyword(token, "bottom")
        || logical && (decoded_css_keyword(token, "y-start") || decoded_css_keyword(token, "y-end")) {
        Some(Word::Y)
    } else if valid_offset(token) {
        Some(Word::Offset)
    } else {
        None
    }
}

const CENTER: &[&str] = &["center"];

fn split_layer<'a>(tokens: &[&'a str]) -> Option<(Vec<&'a str>, Vec<&'a str>)> {
    split_layer_with(tokens, false)
}

fn split_layer_with<'a>(tokens: &[&'a str], logical: bool) -> Option<(Vec<&'a str>, Vec<&'a str>)> {
    let word = |token: &str| word_with(token, logical);
    let center = || CENTER.to_vec();
    match *tokens {
        [token] => Some(match word(token)? {
            Word::Y => (center(), alloc::vec![token]),
            Word::Center => (center(), center()),
            _ => (alloc::vec![token], center()),
        }),
        [first, second] => {
            let (a, b) = (word(first)?, word(second)?);
            let x_fits = |word: Word| matches!(word, Word::X | Word::Center | Word::Offset);
            let y_fits = |word: Word| matches!(word, Word::Y | Word::Center | Word::Offset);
            if x_fits(a) && y_fits(b) {
                Some((alloc::vec![first], alloc::vec![second]))
            } else if matches!(a, Word::Y | Word::Center) && matches!(b, Word::X | Word::Center) {
                Some((alloc::vec![second], alloc::vec![first]))
            } else {
                None
            }
        }
        [_, _, _] | [_, _, _, _] => {
            let mut groups: Vec<(Word, &[&'a str])> = Vec::new();
            let mut index = 0;
            while index < tokens.len() {
                let kind = word(tokens[index])?;
                let end = match kind {
                    Word::Offset => return None,
                    Word::Center => index + 1,
                    _ if tokens.get(index + 1).and_then(|token| word(token)) == Some(Word::Offset) => index + 2,
                    _ => index + 1,
                };
                groups.push((kind, &tokens[index..end]));
                index = end;
            }
            let [(first_kind, first), (second_kind, second)] = groups[..] else { return None };
            match (first_kind, second_kind) {
                (Word::X, Word::Y) | (Word::X, Word::Center) | (Word::Center, Word::Y) => Some((first.to_vec(), second.to_vec())),
                (Word::Y, Word::X) | (Word::Y, Word::Center) | (Word::Center, Word::X) => Some((second.to_vec(), first.to_vec())),
                _ => None,
            }
        }
        _ => None,
    }
}

const LOGICAL_WORDS: [&str; 4] = ["x-start", "x-end", "y-start", "y-end"];

/// `<position>` as `object-position` takes it: one, two or four components, logical keywords included.
fn split_position<'a>(tokens: &[&'a str]) -> Option<(Vec<&'a str>, Vec<&'a str>)> {
    if tokens.len() == 3 {
        return None;
    }
    split_layer_with(tokens, true)
}

pub(crate) fn has_logical_word(raw: &str) -> bool {
    components(raw).is_some_and(|tokens| tokens.iter().any(|token| LOGICAL_WORDS.iter().any(|word| decoded_css_keyword(token, word))))
}

/// The canonical specified serialization of a `<position>`, keywords kept.
pub(crate) fn position_specified(raw: &str) -> Option<String> {
    let (x, y) = split_position(&components(raw)?)?;
    Some(alloc::format!("{} {}", specified_part(&axis_part(Axis::X, &x, true)?)?, specified_part(&axis_part(Axis::Y, &y, true)?)?))
}

/// The computed `<position>`; a value naming logical keywords stays specified until the
/// element's writing mode and direction are known.
pub(crate) fn position_compute(raw: &str, context: LengthContext) -> Option<String> {
    if has_logical_word(raw) {
        return position_specified(raw);
    }
    let (x, y) = split_position(&components(raw)?)?;
    let x = resolve(&axis_part(Axis::X, &x, true)?, context)?;
    let y = resolve(&axis_part(Axis::Y, &y, true)?, context)?;
    Some(alloc::format!("{x} {y}"))
}

/// A specified `<position>` in its computed form, for animation endpoints that arrive unresolved.
pub(crate) fn position_endpoint(raw: &str) -> Option<String> {
    position_compute(raw, static_length_context())
}

/// Adds two `<position>` values per axis.
pub(crate) fn compose_position(base: &str, value: &str) -> Option<String> {
    let (base, value) = (position_endpoint(base)?, position_endpoint(value)?);
    let (base, value) = (components(&base)?, components(&value)?);
    let (&[bx, by], &[vx, vy]) = (&base[..], &value[..]) else { return None };
    Some(alloc::format!("{} {}", compose(bx, vx)?, compose(by, vy)?))
}

/// `stored` with its logical keywords turned physical for the given writing mode, then computed.
pub(crate) fn position_physical(stored: &str, writing_mode: WritingMode, direction: Direction) -> Option<String> {
    let rtl = direction == Direction::Rtl;
    let (inline_far, block_far) = match writing_mode {
        WritingMode::HorizontalTb => (rtl, false),
        WritingMode::VerticalRl | WritingMode::SidewaysRl => (true, rtl),
        WritingMode::VerticalLr => (false, rtl),
        WritingMode::SidewaysLr => (false, !rtl),
    };
    let mapped: Vec<String> = components(stored)?.iter().map(|token| {
        let lower = ascii_lower(token);
        String::from(match &*lower {
            "x-start" => if inline_far { "right" } else { "left" },
            "x-end" => if inline_far { "left" } else { "right" },
            "y-start" => if block_far { "bottom" } else { "top" },
            "y-end" => if block_far { "top" } else { "bottom" },
            _ => *token,
        })
    }).collect();
    let refs: Vec<&str> = mapped.iter().map(String::as_str).collect();
    position_compute(&refs.join(" "), static_length_context())
}

/// The specified `(x, y)` longhand lists of a `background-position` value.
pub(crate) fn shorthand_lists(raw: &str) -> Option<(String, String)> {
    let mut xs = Vec::new();
    let mut ys = Vec::new();
    for layer in layers(raw)? {
        let (x, y) = split_layer(&components(layer)?)?;
        xs.push(specified_part(&axis_part(Axis::X, &x, false)?)?);
        ys.push(specified_part(&axis_part(Axis::Y, &y, false)?)?);
    }
    Some((xs.join(", "), ys.join(", ")))
}

pub(crate) fn expand(name: &str, raw: &str) -> Option<Vec<(&'static str, String)>> {
    if !is_shorthand(name) {
        return None;
    }
    let (x, y) = shorthand_lists(raw)?;
    Some(alloc::vec![(LONGHANDS[0], x), (LONGHANDS[1], y)])
}

pub(crate) fn shorthand_declarations(name: &str, raw: &str) -> Option<Vec<Value>> {
    let mut values = Vec::new();
    for (longhand, text) in expand(name, raw)? {
        values.extend(extra::declarations(longhand, &text)?);
    }
    Some(values)
}

fn items(list: &str) -> Vec<&str> {
    top_level_split(list, b',', MAX_BACKGROUND_GEOMETRY_VALUES).unwrap_or_default().into_iter().map(str::trim).collect()
}

/// Pairs the layers of the two axis lists, repeating the shorter one.
pub(crate) fn join_axes(x: &str, y: &str) -> Option<String> {
    let (xs, ys) = (items(x), items(y));
    let count = lcm_capped(xs.len(), ys.len(), MAX_BACKGROUND_GEOMETRY_VALUES)?;
    let layers: Vec<String> = (0..count).map(|index| alloc::format!("{} {}", xs[index % xs.len()], ys[index % ys.len()])).collect();
    Some(layers.join(", "))
}

pub(crate) fn synthesize(name: &str, values: &[&str]) -> Option<String> {
    let &[x, y] = values else { return None };
    if !is_shorthand(name) || items(x).len() != items(y).len() {
        return None;
    }
    join_axes(x, y)
}

pub(crate) fn computed_shorthand(style: &Style) -> String {
    let (Some(x), Some(y)) = (extra::stored(style, LONGHANDS[0]), extra::stored(style, LONGHANDS[1])) else {
        return String::new();
    };
    join_axes(&x, &y).unwrap_or_default()
}

fn length_percentage(value: &str) -> Option<LengthPercentage> {
    let value = offset(value, static_length_context())?;
    Some(LengthPercentage { pixels: value.pixels, fraction: value.percent / 100.0 })
}

/// Rebuilds the typed layout carrier after either axis changed.
pub(crate) fn mirror(style: &mut Style) {
    let (Some(x), Some(y)) = (extra::stored(style, LONGHANDS[0]), extra::stored(style, LONGHANDS[1])) else {
        return;
    };
    let (xs, ys) = (items(&x), items(&y));
    let Some(count) = lcm_capped(xs.len(), ys.len(), MAX_BACKGROUND_GEOMETRY_VALUES) else { return };
    let mut values = Vec::with_capacity(count);
    for index in 0..count {
        let (Some(x), Some(y)) = (length_percentage(xs[index % xs.len()]), length_percentage(ys[index % ys.len()])) else {
            return;
        };
        values.push([x, y]);
    }
    let initial = [LengthPercentage::default(); 2];
    let carrier = if values.iter().all(|value| *value == initial) { None } else { Some(Arc::from(values)) };
    if style.background_position != carrier {
        style.background_position = carrier;
    }
}

fn cycle(from: &str, to: &str, combine: &dyn Fn(&str, &str) -> Option<String>) -> Option<String> {
    let (from, to) = (items(from), items(to));
    let count = lcm_capped(from.len(), to.len(), MAX_BACKGROUND_GEOMETRY_VALUES)?;
    let layers: Option<Vec<String>> = (0..count).map(|index| combine(from[index % from.len()], to[index % to.len()])).collect();
    Some(layers?.join(", "))
}

struct Mixed {
    value: TransformLength,
    percent_form: bool,
}

fn mixed(token: &str) -> Option<Mixed> {
    let context = static_length_context();
    let value = transform_length_with_query(token, context, extra::query_for(context))?;
    Some(Mixed { value, percent_form: token.contains('%') })
}

fn serialize(pixels: f32, percent: f32, percent_form: bool) -> String {
    if pixels == 0.0 && percent_form {
        alloc::format!("{}%", computed_values::number(percent))
    } else {
        computed_values::percentage(pixels, percent)
    }
}

pub(crate) fn interpolate(from: &str, to: &str, progress: f64) -> Option<String> {
    cycle(from, to, &|a, b| {
        let (a, b) = (mixed(a)?, mixed(b)?);
        let mix = |a: f32, b: f32| extra::mix(f64::from(a), f64::from(b), progress) as f32;
        Some(serialize(mix(a.value.pixels, b.value.pixels), mix(a.value.percent, b.value.percent), a.percent_form || b.percent_form))
    })
}

/// Addition and accumulation agree for lengths: the endpoints sum.
pub(crate) fn compose(base: &str, value: &str) -> Option<String> {
    cycle(base, value, &|a, b| {
        let (a, b) = (mixed(a)?, mixed(b)?);
        Some(serialize(a.value.pixels + b.value.pixels, a.value.percent + b.value.percent, a.percent_form || b.percent_form))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lists(raw: &str) -> Option<(String, String)> {
        shorthand_lists(raw)
    }

    #[test]
    fn shorthand_splits_into_axis_lists() {
        assert_eq!(lists("top left"), Some(("left".into(), "top".into())));
        assert_eq!(lists("center"), Some(("center".into(), "center".into())));
        assert_eq!(lists("10px"), Some(("10px".into(), "center".into())));
        assert_eq!(lists("top 15px center"), Some(("center".into(), "top 15px".into())));
        assert_eq!(lists("bottom 16% left"), Some(("left".into(), "bottom 16%".into())));
        assert_eq!(lists("left 20px top 10px, 5px 6px"), Some(("left 20px, 5px".into(), "top 10px, 6px".into())));
        assert_eq!(lists("10px left"), None);
        assert_eq!(lists("top 10px"), None);
    }

    #[test]
    fn edges_compute_to_percentages() {
        let context = static_length_context();
        assert_eq!(compute(Axis::X, "right 10px", context).as_deref(), Some("calc(100% - 10px)"));
        assert_eq!(compute(Axis::X, "right -10px", context).as_deref(), Some("calc(100% + 10px)"));
        assert_eq!(compute(Axis::Y, "bottom 16%", context).as_deref(), Some("84%"));
        assert_eq!(compute(Axis::X, "left, center, x-end", context).as_deref(), Some("0%, 50%, 100%"));
    }

    #[test]
    fn mismatched_lists_repeat_to_common_length() {
        assert_eq!(join_axes("1px, 2px", "3px").as_deref(), Some("1px 3px, 2px 3px"));
        assert_eq!(interpolate("0px, 10px", "20px", 0.5).as_deref(), Some("10px, 15px"));
    }
}
