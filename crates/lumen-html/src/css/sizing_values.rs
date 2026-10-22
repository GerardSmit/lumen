//! Specified-value serialization for sizing properties that are stored in
//! normalized form (`aspect-ratio`).
use super::*;
use alloc::{format, string::String};

fn ratio_term(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    match typed_numeric::parse_numeric_value(raw) {
        Some(value) if value.unit == typed_numeric::NumericUnit::Number => {
            Some(typed_numeric::serialize_numeric_value(value.value, value.unit))
        }
        Some(_) => None,
        None => typed_numeric::parse_numeric_expression(raw)?.serialize_specified(),
    }
}

/// `[auto] <number> [/ <number>]` with `auto` first and an explicit
/// denominator.
pub(super) fn aspect_ratio_specified(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("auto") {
        return Some(String::from("auto"));
    }
    let parts = components(raw)?;
    let (first, last) = (parts.first()?, parts.last()?);
    let (ratio, auto) = if first.eq_ignore_ascii_case("auto") {
        (raw[first.len()..].trim(), true)
    } else if last.eq_ignore_ascii_case("auto") {
        (raw[..raw.len() - last.len()].trim(), true)
    } else {
        (raw, false)
    };
    let terms = top_level_split(ratio, b'/', 2)?;
    let numerator = ratio_term(terms.first()?)?;
    let denominator = match terms.get(1) {
        Some(term) => ratio_term(term)?,
        None => String::from("1"),
    };
    let body = format!("{numerator} / {denominator}");
    Some(if auto { format!("auto {body}") } else { body })
}

impl Style {
    /// `content-visibility` implies containment for as long as it applies:
    /// `auto` adds layout, style and paint containment, `hidden` also adds
    /// size containment.
    pub(super) fn apply_content_visibility_containment(&mut self) {
        let size = match extra_props::keyword(self, "content-visibility") {
            Some("hidden") => true,
            Some("auto") => false,
            _ => return,
        };
        self.contain_layout = true;
        self.contain_style = true;
        self.contain_paint = true;
        self.contain_size |= size;
    }
}
