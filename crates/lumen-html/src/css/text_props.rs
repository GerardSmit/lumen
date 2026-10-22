//! Grammars, canonical forms and derived white-space behavior for the CSS Text 4
//! and CSS UI 4 properties stored by `extra_props`.
use super::*;
use lumen_common::color::{Color, ColorSpace, HueInterpolation, InterpolationMethod, PreparedPair};

type Idents<'a> = Vec<alloc::borrow::Cow<'a, str>>;

fn idents(raw: &str) -> Option<Idents<'_>> {
    let tokens = components(raw)?;
    if tokens.is_empty() {
        return None;
    }
    tokens.into_iter().map(color_values::identifier).collect()
}

fn one_of(raw: &str, allowed: &[&str]) -> Option<String> {
    let words = idents(raw)?;
    let [word] = words.as_slice() else { return None };
    allowed.iter().find(|candidate| **candidate == word.as_ref()).map(|word| String::from(*word))
}

pub(super) fn longhands(name: &str) -> &'static [&'static str] {
    match name {
        "text-wrap" => &["text-wrap-mode", "text-wrap-style"],
        "text-spacing" => &["text-spacing-trim", "text-autospace"],
        _ => &[],
    }
}

pub(super) fn is_shorthand(name: &str) -> bool {
    !longhands(name).is_empty()
}

pub(super) fn derives_white_space(name: &str) -> bool {
    matches!(name, "text-wrap-mode" | "white-space-collapse")
}

fn white_space_parts(value: WhiteSpace) -> (&'static str, &'static str) {
    match value {
        WhiteSpace::Normal => ("collapse", "wrap"),
        WhiteSpace::NoWrap => ("collapse", "nowrap"),
        WhiteSpace::Pre => ("preserve", "nowrap"),
        WhiteSpace::PreWrap => ("preserve", "wrap"),
        WhiteSpace::PreLine => ("preserve-breaks", "wrap"),
        WhiteSpace::BreakSpaces => ("break-spaces", "wrap"),
    }
}

/// The nearest `white-space` keyword; a preserved-breaks or break-spaces
/// collapse mode without wrapping has no keyword of its own and keeps its
/// collapse behavior with wrapping.
pub(super) fn white_space_from_parts(collapse: &str, wrap: &str) -> WhiteSpace {
    let nowrap = wrap == "nowrap";
    match (collapse, nowrap) {
        ("collapse", false) => WhiteSpace::Normal,
        ("collapse", true) => WhiteSpace::NoWrap,
        ("preserve", false) => WhiteSpace::PreWrap,
        ("preserve", true) => WhiteSpace::Pre,
        ("preserve-breaks", _) => WhiteSpace::PreLine,
        (_, false) => WhiteSpace::BreakSpaces,
        (_, true) => WhiteSpace::Pre,
    }
}

/// Applies a `white-space-collapse` or `text-wrap-mode` value onto the shared
/// white-space state. Returns whether the property is one of the two.
pub(super) fn apply_white_space(name: &str, value: &str, style: &mut Style) -> bool {
    if !derives_white_space(name) {
        return false;
    }
    let (collapse, wrap) = white_space_parts(style.white_space);
    let next = if name == "white-space-collapse" {
        white_space_from_parts(value, wrap)
    } else {
        white_space_from_parts(collapse, value)
    };
    if style.white_space != next {
        style.white_space = next;
    }
    true
}

pub(super) fn copy_white_space(name: &str, from: &Style, to: &mut Style) {
    let (from_collapse, from_wrap) = white_space_parts(from.white_space);
    let value = if name == "white-space-collapse" { from_collapse } else { from_wrap };
    apply_white_space(name, value, to);
}

fn derived_white_space(name: &str, style: &Style) -> String {
    let (collapse, wrap) = white_space_parts(style.white_space);
    String::from(if name == "white-space-collapse" { collapse } else { wrap })
}

/// Parses the `white-space` shorthand grammar that extends the legacy
/// keywords with `<white-space-collapse> || <text-wrap-mode>`.
pub(super) fn white_space_shorthand(raw: &str) -> Option<WhiteSpace> {
    let words = idents(raw)?;
    if words.len() > 2 {
        return None;
    }
    let mut collapse = None;
    let mut wrap = None;
    for word in &words {
        match word.as_ref() {
            "collapse" | "preserve" | "preserve-breaks" | "break-spaces" => {
                if collapse.replace(word.as_ref()).is_some() {
                    return None;
                }
            }
            "wrap" | "nowrap" => {
                if wrap.replace(word.as_ref()).is_some() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(white_space_from_parts(collapse.unwrap_or("collapse"), wrap.unwrap_or("wrap")))
}

/// Canonical specified `white-space`: the legacy keyword for the longhand
/// combination when one exists, otherwise `<collapse> <wrap>`.
pub(super) fn white_space_specified(raw: &str) -> Option<String> {
    let words = idents(raw)?;
    if words.len() == 1 && matches!(words[0].as_ref(), "normal" | "nowrap" | "pre" | "pre-wrap" | "pre-line" | "break-spaces") {
        return Some(String::from(words[0].as_ref()));
    }
    white_space_shorthand(raw)?;
    let mut collapse = "collapse";
    let mut wrap = "wrap";
    for word in &words {
        match word.as_ref() {
            "wrap" | "nowrap" => wrap = if word.as_ref() == "nowrap" { "nowrap" } else { "wrap" },
            "collapse" | "preserve" | "preserve-breaks" | "break-spaces" => {
                collapse = match word.as_ref() {
                    "preserve" => "preserve",
                    "preserve-breaks" => "preserve-breaks",
                    "break-spaces" => "break-spaces",
                    _ => "collapse",
                }
            }
            _ => return None,
        }
    }
    Some(String::from(match (collapse, wrap) {
        ("collapse", "wrap") => "normal",
        ("collapse", _) => "nowrap",
        ("preserve", "wrap") => "pre-wrap",
        ("preserve", _) => "pre",
        ("preserve-breaks", "wrap") => "pre-line",
        ("break-spaces", "wrap") => "break-spaces",
        ("preserve-breaks", _) => "preserve-breaks nowrap",
        _ => "break-spaces nowrap",
    }))
}

fn text_wrap(raw: &str) -> Option<(&'static str, &'static str)> {
    let words = idents(raw)?;
    if words.len() > 2 {
        return None;
    }
    let mut mode = None;
    let mut style = None;
    for word in &words {
        match word.as_ref() {
            "wrap" | "nowrap" => {
                let value = if word.as_ref() == "wrap" { "wrap" } else { "nowrap" };
                if mode.replace(value).is_some() {
                    return None;
                }
            }
            "auto" | "balance" | "stable" | "pretty" => {
                let value = match word.as_ref() {
                    "auto" => "auto",
                    "balance" => "balance",
                    "stable" => "stable",
                    _ => "pretty",
                };
                if style.replace(value).is_some() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some((mode.unwrap_or("wrap"), style.unwrap_or("auto")))
}

fn text_spacing(raw: &str) -> Option<(&'static str, &'static str)> {
    let words = idents(raw)?;
    if words.len() == 1 {
        match words[0].as_ref() {
            "none" => return Some(("space-all", "no-autospace")),
            "auto" => return Some(("auto", "auto")),
            _ => {}
        }
    }
    if words.len() > 2 {
        return None;
    }
    let mut trim = None;
    let mut autospace = None;
    let mut normals = 0;
    for word in &words {
        match word.as_ref() {
            "space-all" | "space-first" | "trim-start" => {
                let value = match word.as_ref() {
                    "space-all" => "space-all",
                    "space-first" => "space-first",
                    _ => "trim-start",
                };
                if trim.replace(value).is_some() {
                    return None;
                }
            }
            "no-autospace" => {
                if autospace.replace("no-autospace").is_some() {
                    return None;
                }
            }
            "normal" => normals += 1,
            _ => return None,
        }
    }
    for _ in 0..normals {
        if trim.is_none() {
            trim = Some("normal");
        } else if autospace.is_none() {
            autospace = Some("normal");
        } else {
            return None;
        }
    }
    Some((trim.unwrap_or("normal"), autospace.unwrap_or("normal")))
}

/// Longhand declarations a shorthand expands to, in longhand order.
pub(super) fn expand(name: &str, raw: &str) -> Option<Vec<(&'static str, String)>> {
    let [first, second] = <[&str; 2]>::try_from(longhands(name)).ok()?;
    let (a, b) = match name {
        "text-wrap" => text_wrap(raw)?,
        _ => text_spacing(raw)?,
    };
    Some(alloc::vec![(first, String::from(a)), (second, String::from(b))])
}

/// Shorthand serialization from its longhand values.
pub(super) fn shorthand_value(name: &str, values: &[&str]) -> Option<String> {
    let &[a, b] = values else { return None };
    match name {
        "text-wrap" => Some(if b == "auto" {
            String::from(a)
        } else if a == "wrap" {
            String::from(b)
        } else {
            alloc::format!("{a} {b}")
        }),
        "text-spacing" => {
            if a == "space-all" && b == "no-autospace" {
                return Some(String::from("none"));
            }
            if a == "auto" && b == "auto" {
                return Some(String::from("auto"));
            }
            if matches!(a, "auto" | "trim-both" | "trim-all") || b == "auto" {
                return None;
            }
            let mut parts: Vec<&str> = Vec::new();
            for part in [a, b] {
                if part != "normal" {
                    parts.push(part);
                }
            }
            Some(if parts.is_empty() { String::from("normal") } else { parts.join(" ") })
        }
        _ => None,
    }
}

fn autospace(raw: &str) -> Option<String> {
    let words = idents(raw)?;
    if words.len() == 1 && matches!(words[0].as_ref(), "normal" | "auto" | "no-autospace") {
        return Some(words[0].to_string());
    }
    let (mut alpha, mut numeric, mut punctuation, mut mode) = (false, false, false, None);
    for word in &words {
        let seen = match word.as_ref() {
            "ideograph-alpha" => &mut alpha,
            "ideograph-numeric" => &mut numeric,
            "punctuation" => &mut punctuation,
            "insert" | "replace" => {
                if mode.replace(word.to_string()).is_some() {
                    return None;
                }
                continue;
            }
            _ => return None,
        };
        if core::mem::replace(seen, true) {
            return None;
        }
    }
    let mut parts: Vec<String> = Vec::new();
    for (present, keyword) in [(alpha, "ideograph-alpha"), (numeric, "ideograph-numeric"), (punctuation, "punctuation")] {
        if present {
            parts.push(String::from(keyword));
        }
    }
    parts.extend(mode);
    Some(parts.join(" "))
}

fn text_fit(raw: &str) -> Option<String> {
    let words = components(raw)?;
    let mut iter = words.into_iter().map(|word| word.to_ascii_lowercase());
    let first = iter.next()?;
    let rest: Vec<String> = iter.collect();
    if first == "none" {
        return rest.is_empty().then(|| first);
    }
    if first != "grow" && first != "shrink" {
        return None;
    }
    let mut out = first;
    let mut index = 0;
    if let Some(method) = rest.first().filter(|word| matches!(word.as_str(), "consistent" | "per-line" | "per-line-all")) {
        out.push(' ');
        out.push_str(method);
        index = 1;
    }
    if let Some(percentage) = rest.get(index) {
        let value = typed_numeric::parse_numeric_value(percentage)?;
        if value.unit != typed_numeric::NumericUnit::Percent || value.value < 0.0 || !value.value.is_finite() {
            return None;
        }
        index += 1;
        out.push(' ');
        out.push_str(&alloc::format!("{}%", computed_values::number(value.value as f32)));
    }
    (index == rest.len()).then_some(out)
}

fn word_space_transform(raw: &str) -> Option<String> {
    let words = idents(raw)?;
    if words.len() == 1 && words[0].as_ref() == "none" {
        return Some(String::from("none"));
    }
    if words.len() > 2 {
        return None;
    }
    let mut kinds = 0;
    let mut phrase = 0;
    for word in &words {
        match word.as_ref() {
            "space" | "ideographic-space" => kinds += 1,
            "auto-phrase" => phrase += 1,
            _ => return None,
        }
    }
    (kinds == 1 && phrase <= 1).then(|| words.join(" "))
}

fn hanging_punctuation(raw: &str) -> Option<String> {
    let words = idents(raw)?;
    if words.len() == 1 && words[0].as_ref() == "none" {
        return Some(String::from("none"));
    }
    let (mut first, mut last, mut end) = (false, false, false);
    for word in &words {
        let flag = match word.as_ref() {
            "first" => &mut first,
            "last" => &mut last,
            "force-end" | "allow-end" => &mut end,
            _ => return None,
        };
        if core::mem::replace(flag, true) {
            return None;
        }
    }
    Some(words.join(" "))
}

fn hyphenate_character(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if decoded_css_keyword(raw, "auto") {
        return Some(String::from("auto"));
    }
    Some(serialize_css_string(&css_string(raw)?))
}

fn cursor_number(token: &str) -> Option<String> {
    let expression = typed_numeric::parse_numeric_expression(token)?;
    if expression.numeric_type()? != typed_numeric::NumericType::from_unit(typed_numeric::NumericUnit::Number) {
        return None;
    }
    expression.serialize_specified()
}

const CURSOR_KEYWORDS: &[&str] = &[
    "auto", "default", "none", "context-menu", "help", "pointer", "progress", "wait", "cell", "crosshair", "text",
    "vertical-text", "alias", "copy", "move", "no-drop", "not-allowed", "grab", "grabbing", "e-resize", "n-resize",
    "ne-resize", "nw-resize", "s-resize", "se-resize", "sw-resize", "w-resize", "ew-resize", "ns-resize",
    "nesw-resize", "nwse-resize", "col-resize", "row-resize", "all-scroll", "zoom-in", "zoom-out",
];

fn cursor(raw: &str) -> Option<String> {
    let items = top_level_split(raw.trim(), b',', 33)?;
    let (keyword, images) = items.split_last()?;
    let mut out: Vec<String> = Vec::new();
    for item in images {
        let tokens = components(item)?;
        let image = *tokens.first()?;
        let name = image.split_once('(')?.0.trim().to_ascii_lowercase();
        if !matches!(name.as_str(), "url" | "image-set" | "-webkit-image-set") {
            return None;
        }
        let mut text = image_source::serialize(image)?;
        match tokens.len() {
            1 => {}
            3 => {
                text.push(' ');
                text.push_str(&cursor_number(tokens[1])?);
                text.push(' ');
                text.push_str(&cursor_number(tokens[2])?);
            }
            _ => return None,
        }
        out.push(text);
    }
    out.push(one_of(keyword, CURSOR_KEYWORDS)?);
    Some(out.join(", "))
}

fn color_context() -> (LengthContext, ContainerUnitContext) {
    let context = static_length_context();
    (context, ContainerUnitContext::no_container(context.viewport))
}

fn color_token(token: &str, current: Rgba) -> Option<Color> {
    let (context, query) = color_context();
    color_values::endpoint_with_current(token, 0, color_values::from_rgba(current), context, query)
}

fn color_pair(raw: &str, current: Rgba, allow_second: bool) -> Option<String> {
    let tokens = components(raw)?;
    if tokens.is_empty() || tokens.len() > if allow_second { 2 } else { 1 } {
        return None;
    }
    let mut parts = Vec::new();
    for token in tokens {
        if decoded_css_keyword(token, "auto") {
            parts.push(String::from("auto"));
            continue;
        }
        color_token(token, current)?;
        parts.push(color_values::serialize_declared(token, 0)?);
    }
    Some(parts.join(" "))
}

/// Canonical specified form of a value, `None` when it is invalid.
pub(super) fn compute(name: &str, raw: &str, current: Rgba) -> Option<String> {
    match name {
        "hyphens" => one_of(raw, &["none", "manual", "auto"]),
        "text-wrap-mode" => one_of(raw, &["wrap", "nowrap"]),
        "text-wrap-style" => one_of(raw, &["auto", "balance", "stable", "pretty"]),
        "white-space-collapse" => one_of(raw, &["collapse", "preserve", "preserve-breaks", "break-spaces"]),
        "text-spacing-trim" => {
            one_of(raw, &["normal", "space-all", "trim-both", "trim-all", "auto", "trim-start", "space-first"])
        }
        "text-group-align" => one_of(raw, &["none", "start", "end", "left", "right", "center"]),
        "text-align-all" => {
            one_of(raw, &["start", "end", "left", "right", "center", "justify", "match-parent"])
        }
        "resize" => one_of(raw, &["none", "both", "horizontal", "vertical", "block", "inline"]),
        "interactivity" => one_of(raw, &["auto", "inert"]),
        "caret-animation" => one_of(raw, &["auto", "manual"]),
        "caret-shape" => one_of(raw, &["auto", "bar", "block", "underscore"]),
        "text-autospace" => autospace(raw),
        "text-fit" => text_fit(raw),
        "hyphenate-character" => hyphenate_character(raw),
        "word-space-transform" => word_space_transform(raw),
        "hanging-punctuation" => hanging_punctuation(raw),
        "cursor" => cursor(raw),
        "caret-color" => color_pair(raw, current, true),
        "accent-color" => color_pair(raw, current, false),
        _ => None,
    }
}

fn resolved_color(token: &str, current: Rgba) -> Option<String> {
    if decoded_css_keyword(token, "auto") {
        return Some(computed_values::color(current));
    }
    let color = color_token(token, current)?;
    color_values::serialize(color, !color_values::legacy(token))
}

/// Computed value of a stored declaration for the element's style.
pub(super) fn computed(name: &str, stored: &str, style: &Style) -> String {
    match name {
        "white-space-collapse" | "text-wrap-mode" => derived_white_space(name, style),
        "text-fit" => stored.replace(" consistent", ""),
        "caret-color" => {
            let tokens = components(stored).unwrap_or_default();
            let mut parts: Vec<String> = Vec::new();
            for (index, token) in tokens.iter().enumerate() {
                if index == 1 && decoded_css_keyword(token, "auto") {
                    continue;
                }
                parts.push(resolved_color(token, style.color).unwrap_or_else(|| (*token).to_owned()));
            }
            parts.join(" ")
        }
        "accent-color" => {
            if decoded_css_keyword(stored, "auto") {
                String::from("auto")
            } else {
                resolved_color(stored, style.color).unwrap_or_else(|| stored.to_owned())
            }
        }
        _ => stored.to_owned(),
    }
}

pub(super) fn is_color(name: &str) -> bool {
    matches!(name, "caret-color" | "accent-color")
}

fn method(left: Color, right: Color) -> InterpolationMethod {
    if matches!(left.space, ColorSpace::Srgb) && matches!(right.space, ColorSpace::Srgb) {
        InterpolationMethod { space: ColorSpace::Srgb, hue: HueInterpolation::Shorter }
    } else {
        InterpolationMethod { space: ColorSpace::Oklab, hue: HueInterpolation::Shorter }
    }
}

fn absolute_pair(value: &str) -> Option<Vec<Color>> {
    let tokens = components(value)?;
    if tokens.is_empty() || tokens.len() > 2 {
        return None;
    }
    tokens.into_iter().map(|token| {
        if decoded_css_keyword(token, "currentcolor") || decoded_css_keyword(token, "auto") {
            return None;
        }
        color_token(token, Rgba { r: 0, g: 0, b: 0, a: 255 })
    }).collect()
}

pub(super) fn interpolate_color(from: &str, to: &str, progress: f64) -> Option<String> {
    let (from, to) = (absolute_pair(from)?, absolute_pair(to)?);
    if from.len() != to.len() {
        return None;
    }
    let mut parts = Vec::new();
    for (left, right) in from.into_iter().zip(to) {
        let mixed = PreparedPair::new(left, right, method(left, right)).sample(progress as f32);
        parts.push(color_values::serialize(mixed, false)?);
    }
    Some(parts.join(" "))
}

pub(super) fn compose_color(base: &str, value: &str) -> Option<String> {
    let (base, value) = (absolute_pair(base)?, absolute_pair(value)?);
    if base.len() != value.len() {
        return None;
    }
    let mut parts = Vec::new();
    for (left, right) in base.into_iter().zip(value) {
        let (a, b) = (left.to(ColorSpace::Srgb), right.to(ColorSpace::Srgb));
        let alpha = (a.alpha + b.alpha).clamp(0.0, 1.0);
        let mut channels = [0.0f32; 3];
        for (index, channel) in channels.iter_mut().enumerate() {
            let sum = a.components[index] * a.alpha + b.components[index] * b.alpha;
            *channel = if alpha > 0.0 { sum / alpha } else { 0.0 };
        }
        parts.push(color_values::serialize(Color::new(ColorSpace::Srgb, channels, alpha, 0), false)?);
    }
    Some(parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_wrap_shorthand_round_trips() {
        let expanded = expand("text-wrap", "balance nowrap").unwrap();
        assert_eq!(expanded[0].1, "nowrap");
        assert_eq!(expanded[1].1, "balance");
        assert_eq!(shorthand_value("text-wrap", &["nowrap", "balance"]).as_deref(), Some("nowrap balance"));
        assert_eq!(shorthand_value("text-wrap", &["wrap", "auto"]).as_deref(), Some("wrap"));
        assert!(expand("text-wrap", "wrap nowrap").is_none());
    }

    #[test]
    fn text_spacing_normal_fills_the_open_slot() {
        assert_eq!(text_spacing("normal trim-start"), Some(("trim-start", "normal")));
        assert_eq!(text_spacing("no-autospace normal"), Some(("normal", "no-autospace")));
        assert_eq!(shorthand_value("text-spacing", &["space-all", "no-autospace"]).as_deref(), Some("none"));
        assert!(text_spacing("none auto").is_none());
    }

    #[test]
    fn autospace_orders_keywords() {
        assert_eq!(autospace("replace punctuation ideograph-alpha").as_deref(), Some("ideograph-alpha punctuation replace"));
        assert!(autospace("normal insert").is_none());
    }

    #[test]
    fn white_space_parts_compose() {
        assert_eq!(white_space_shorthand("preserve nowrap"), Some(WhiteSpace::Pre));
        assert_eq!(white_space_shorthand("wrap"), Some(WhiteSpace::Normal));
        assert!(white_space_shorthand("balance").is_none());
    }
}
