//! `font-variant`: the shorthand over the ligature, caps, alternates, numeric,
//! east-asian, position and emoji longhands.
use super::*;
use super::extra_props as extra;

const NUMERIC: &[&str] = &[
    "lining-nums", "oldstyle-nums", "proportional-nums", "tabular-nums", "diagonal-fractions",
    "stacked-fractions", "ordinal", "slashed-zero",
];
const EAST_ASIAN: &[&str] = &[
    "jis78", "jis83", "jis90", "jis04", "simplified", "traditional", "full-width", "proportional-width", "ruby",
];
const POSITION: &[&str] = &["sub", "super"];

pub(super) const RESET_LONGHANDS: [&str; 4] =
    ["font-variant-numeric", "font-variant-east-asian", "font-variant-position", "font-variant-emoji"];

/// Longhands the `font` shorthand resets without being able to express them.
pub(super) const FONT_RESETS: [(&str, &str); 4] = [
    ("font-kerning", "auto"),
    ("font-optical-sizing", "auto"),
    ("font-variation-settings", "normal"),
    ("font-language-override", "normal"),
];

pub(super) fn font_resets() -> Vec<Value> {
    FONT_RESETS.iter().flat_map(|(name, initial)| extra::declarations(name, initial).unwrap_or_default()).collect()
}

pub(super) fn normal_resets() -> Vec<Value> {
    RESET_LONGHANDS.iter().flat_map(|name| extra::declarations(name, "normal").unwrap_or_default()).collect()
}

pub(super) fn parse(raw: &str) -> Option<Vec<Value>> {
    let mut values = Vec::new();
    if raw.eq_ignore_ascii_case("normal") || raw.eq_ignore_ascii_case("none") {
        values.push(Value::FontLigatures(font_ligatures(raw)?));
        values.push(Value::FontVariantCaps(FontVariantCaps::Normal));
        values.push(Value::FontAlternates(None));
        values.extend(normal_resets());
        return Some(values);
    }
    let mut caps = FontVariantCaps::Normal;
    let mut ligatures = FontLigatures::NORMAL;
    let mut alternate_tokens = Vec::new();
    let mut groups: [Vec<&str>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for token in components(raw)? {
        let lower = ascii_lower(token);
        if let Some(value) = FontVariantCaps::parse(&lower).filter(|value| *value != FontVariantCaps::Normal) {
            if caps != FontVariantCaps::Normal {
                return None;
            }
            caps = value;
        } else if let Some((group, enabled)) = FontLigatureGroup::parse(token) {
            if !ligatures.set(group, enabled) {
                return None;
            }
        } else if NUMERIC.contains(&&*lower) {
            groups[0].push(token);
        } else if EAST_ASIAN.contains(&&*lower) {
            groups[1].push(token);
        } else if POSITION.contains(&&*lower) {
            groups[2].push(token);
        } else {
            alternate_tokens.push(token);
        }
    }
    let alternates = if alternate_tokens.is_empty() {
        None
    } else {
        Some(font_feature_values::parse_alternates(&alternate_tokens.join(" "))??)
    };
    if caps == FontVariantCaps::Normal && ligatures.is_normal() && alternates.is_none() && groups.iter().all(Vec::is_empty) {
        return None;
    }
    values.push(Value::FontLigatures(ligatures));
    values.push(Value::FontVariantCaps(caps));
    values.push(Value::FontAlternates(alternates));
    for (name, tokens) in ["font-variant-numeric", "font-variant-east-asian", "font-variant-position"].into_iter().zip(&groups) {
        let text = if tokens.is_empty() { String::from("normal") } else { tokens.join(" ") };
        values.extend(extra::declarations(name, &text)?);
    }
    values.extend(extra::declarations("font-variant-emoji", "normal")?);
    Some(values)
}

pub(crate) fn serialize(
    caps: FontVariantCaps,
    ligatures: FontLigatures,
    alternates: Option<&font_feature_values::Alternates>,
    rest: [&str; 4],
) -> Option<String> {
    let [numeric, east_asian, position, emoji] = rest;
    if emoji != "normal" {
        return None;
    }
    let extras: Vec<&str> = [numeric, east_asian, position].into_iter().filter(|value| *value != "normal").collect();
    let base = serialize_font_variant(caps, ligatures, alternates)?;
    if extras.is_empty() {
        return Some(base);
    }
    if ligatures.is_none() {
        return None;
    }
    let mut result = if base == "normal" { String::new() } else { base };
    for extra in extras {
        if !result.is_empty() {
            result.push(' ');
        }
        result.push_str(extra);
    }
    Some(result)
}
