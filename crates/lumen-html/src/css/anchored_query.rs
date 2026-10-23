//! `anchored()` container queries: the `fallback` feature's syntax,
//! canonical serialization and truth against an applied fallback.
use super::container_query::{self, Kind, Token};
use super::*;

/// A parsed `fallback` feature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Fallback {
    /// `fallback` in a boolean context: some fallback is applied.
    Applied,
    /// `fallback: none`.
    Unapplied,
    /// `fallback: <dashed-ident> || <try-tactic>`, canonicalized.
    Item(String),
    /// `fallback: <position-area>`, canonicalized.
    Area(String),
    /// A position area with `any` components.
    AnyArea(String),
}

fn item_token(token: &str) -> bool {
    token.starts_with("--") || matches!(token, "flip-block" | "flip-inline" | "flip-start" | "flip-x" | "flip-y")
}

fn legacy_axis_order(token: &str) -> bool {
    let base = token.strip_prefix("span-").unwrap_or(token);
    base.starts_with("x-self-") || base.starts_with("y-self-")
}

pub(super) fn feature(content: &str) -> Option<Fallback> {
    let (name, value) = match content.split_once(':') {
        Some((name, value)) => (name.trim(), Some(value.trim())),
        None => (content.trim(), None),
    };
    if !name.eq_ignore_ascii_case("fallback") {
        return None;
    }
    let Some(value) = value else {
        return Some(Fallback::Applied);
    };
    if value.is_empty() {
        return None;
    }
    if value.eq_ignore_ascii_case("none") {
        return Some(Fallback::Unapplied);
    }
    let words = components(value)?;
    if words.iter().any(|word| word.eq_ignore_ascii_case("any")) {
        if words.len() > 2 {
            return None;
        }
        let mut canonical = Vec::new();
        for word in &words {
            let lower = word.to_ascii_lowercase();
            if lower != "any" && (lower == "none" || legacy_axis_order(&lower) || anchor::position_area(&lower, false).is_none()) {
                return None;
            }
            canonical.push(lower);
        }
        return Some(Fallback::AnyArea(canonical.join(" ")));
    }
    if words.iter().any(|word| legacy_axis_order(&word.to_ascii_lowercase())) {
        return None;
    }
    let canonical = anchor::fallback_item(value)?;
    if canonical.split_ascii_whitespace().all(item_token) {
        Some(Fallback::Item(canonical))
    } else {
        Some(Fallback::Area(canonical))
    }
}

impl Fallback {
    fn serialize(&self) -> String {
        match self {
            Self::Applied => "fallback".into(),
            Self::Unapplied => "fallback: none".into(),
            Self::Item(value) | Self::Area(value) | Self::AnyArea(value) => alloc::format!("fallback: {value}"),
        }
    }
}

/// The physical track ranges a canonical position area selects.
fn tracks(area: &str, sides: &[usize; 4]) -> Option<((u8, u8), (u8, u8))> {
    anchor::area_tracks(area, sides, sides).map(|tracks| (tracks.horizontal, tracks.vertical))
}

/// Whether `feature` holds when the container applied the canonical fallback
/// `applied` (`None` without one).
pub(super) fn truth(feature: &Fallback, applied: Option<&str>, sides: &[usize; 4]) -> bool {
    match feature {
        Fallback::Applied => applied.is_some(),
        Fallback::Unapplied => applied.is_none(),
        Fallback::Item(query) => applied == Some(query.as_str()),
        Fallback::Area(query) => applied.is_some_and(|applied| {
            applied == query
                || match (tracks(query, sides), tracks(applied, sides)) {
                    (Some(wanted), Some(actual)) => wanted == actual,
                    _ => false,
                }
        }),
        Fallback::AnyArea(_) => false,
    }
}

const MAX_DEPTH: usize = 32;

fn group(token: &Token<'_>, depth: usize) -> Option<String> {
    if token.kind != Kind::Paren {
        return None;
    }
    Some(alloc::format!("({})", serialize_nested(&container_query::inner(token, 0), depth + 1)?))
}

/// Canonical text of the inside of `anchored(...)`; `None` when it is not a
/// valid anchored query.
pub(super) fn serialize_content(content: &str) -> Option<String> {
    serialize_nested(content, 0)
}

fn serialize_nested(content: &str, depth: usize) -> Option<String> {
    if depth > MAX_DEPTH {
        return None;
    }
    let tokens = container_query::tokenize(content)?;
    let first = tokens.first()?;
    if first.kind != Kind::Paren && !container_query::keyword(first, "not") {
        return Some(feature(content)?.serialize());
    }
    if container_query::keyword(first, "not") {
        if tokens.len() != 2 {
            return None;
        }
        return Some(alloc::format!("not {}", group(&tokens[1], depth)?));
    }
    let mut output = group(first, depth)?;
    if tokens.len() == 1 {
        return Some(output);
    }
    if tokens.len() % 2 == 0 {
        return None;
    }
    let connective = if container_query::keyword(&tokens[1], "and") {
        "and"
    } else if container_query::keyword(&tokens[1], "or") {
        "or"
    } else {
        return None;
    };
    for pair in tokens[1..].chunks_exact(2) {
        if !container_query::keyword(&pair[0], connective) {
            return None;
        }
        output.push(' ');
        output.push_str(connective);
        output.push(' ');
        output.push_str(&group(&pair[1], depth)?);
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn features_canonicalize() {
        assert_eq!(feature("fallback"), Some(Fallback::Applied));
        assert_eq!(feature("FALLBACK : none"), Some(Fallback::Unapplied));
        assert_eq!(feature("fallback: flip-INLINE"), Some(Fallback::Item("flip-inline".into())));
        assert_eq!(feature("fallback: --a flip-block"), Some(Fallback::Item("--a flip-block".into())));
        assert_eq!(feature("fallback:"), None);
        assert_eq!(feature("fallback: 0"), None);
        assert_eq!(feature("fallback: any none"), None);
        assert_eq!(feature("fallback: x-self-start"), None);
    }

    #[test]
    fn nested_conditions_serialize() {
        assert_eq!(
            serialize_content("( fallback: flip-INLINE) OR ( FALLBACK : --bar  )").as_deref(),
            Some("(fallback: flip-inline) or (fallback: --bar)")
        );
        assert_eq!(serialize_content("fallback <  none"), None);
    }
}
