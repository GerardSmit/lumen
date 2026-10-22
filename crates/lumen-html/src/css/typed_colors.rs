//! Typed OM view of an absolute CSS color: the authored function family and its unclipped coordinates.
use super::*;
use lumen_common::color::Color;

pub enum TypedColor {
    Keyword(String),
    Absolute { color: Color, legacy: bool },
}

pub fn parse_typed_color(input: &str) -> Option<TypedColor> {
    let text = input.trim();
    if text.is_empty() || text.len() > MAX_CSS_BYTES {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    if ["initial", "inherit", "unset", "revert", "revert-layer"].contains(&lower.as_str()) {
        return None;
    }
    let bare = !text.contains('(') && !text.starts_with('#');
    if bare && lower != "transparent" && crate::named_colors::NAMED_COLORS.binary_search_by_key(&lower.as_str(), |(name, _)| *name).is_err() {
        let system = color_scheme::system_color(&lower, UsedColorScheme::Light).is_some();
        return (system || lower == "currentcolor").then_some(TypedColor::Keyword(lower));
    }
    let context = static_length_context();
    let color = color_values::endpoint_with_current(text, 0, color_values::from_rgba(Style::initial().color), context, ContainerUnitContext::no_container(context.viewport))?;
    Some(TypedColor::Absolute { color, legacy: color_values::legacy(text) })
}
