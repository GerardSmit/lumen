//! `@page` rules: page selector lists, margin boxes and the page-only descriptors that the
//! ordinary property registry does not model (`size`, `page-orientation`, `marks`, `bleed`).
use super::nesting::{at_rule_tail, source_rule_ranges, SourceRule, SourceRuleKind};
use super::*;

const MARGIN_BOXES: [&str; 16] = [
    "top-left-corner", "top-left", "top-center", "top-right", "top-right-corner",
    "right-top", "right-middle", "right-bottom",
    "bottom-right-corner", "bottom-right", "bottom-center", "bottom-left", "bottom-left-corner",
    "left-bottom", "left-middle", "left-top",
];

const DESCRIPTORS: [&str; 4] = ["size", "page-orientation", "marks", "bleed"];

/// The canonical name of a margin box at-rule prelude such as `@top-left`.
pub fn margin_box_name(prelude: &str) -> Option<&'static str> {
    let name = prelude.trim().strip_prefix('@')?;
    MARGIN_BOXES.iter().copied().find(|candidate| candidate.eq_ignore_ascii_case(name))
}

/// The text following `@page` in an at-rule prelude.
pub fn page_prelude_tail(prelude: &str) -> Option<&str> {
    at_rule_tail(prelude.trim(), "@page")
}

/// Canonical serialization of a `<page-selector-list>`; the empty list is valid.
pub fn selector_list(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return Some(String::new());
    }
    let mut out = String::new();
    for (index, part) in text.split(',').enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(&page_selector(part.trim())?);
    }
    Some(out)
}

fn page_selector(part: &str) -> Option<String> {
    if part.is_empty() {
        return None;
    }
    let mut out = String::new();
    let mut position = 0;
    if !part.starts_with(':') {
        let name = consume_selector_identifier(part, &mut position)?;
        out.push_str(&serialize_identifier(&name));
    }
    while position < part.len() {
        if part.as_bytes()[position] != b':' {
            return None;
        }
        position += 1;
        let pseudo = consume_selector_identifier(part, &mut position)?.to_ascii_lowercase();
        if !matches!(pseudo.as_str(), "left" | "right" | "first" | "blank") {
            return None;
        }
        out.push(':');
        out.push_str(&pseudo);
    }
    Some(out)
}

/// Fills the declarations and margin box children of a parsed `@page` block.
pub(super) fn fill_body(text: &str, open: usize, close: usize, node: &mut SourceRule, count: &mut usize) -> Result<(), CssError> {
    let base = open + 1;
    node.declaration_offset = Some(base);
    for (start, end, block_open, block_close) in source_rule_ranges(&text[base..close], true)? {
        let (start, end) = (base + start, base + end);
        let (Some(block_open), Some(block_close)) = (block_open, block_close) else {
            if !text[start..end].trim_start().starts_with('@') {
                node.declaration_ranges.push(start..end);
            }
            continue;
        };
        let (block_open, block_close) = (base + block_open, base + block_close);
        if margin_box_name(&text[start..block_open]).is_none() {
            continue;
        }
        *count += 1;
        if *count > MAX_RULES {
            return Err(syntax::error(start, "too many CSS rules"));
        }
        node.children.push(SourceRule {
            kind: SourceRuleKind::MarginBox,
            range: start..end,
            prelude: start..block_open,
            body: Some(block_open + 1..block_close),
            declaration_offset: Some(block_open + 1),
            declaration_ranges: vec![block_open + 1..block_close],
            children: Vec::new(),
            scope_prelude: None,
            css_scope: node.css_scope.clone(),
            selectors: None,
        });
    }
    Ok(())
}

pub fn is_descriptor(name: &str) -> bool {
    DESCRIPTORS.iter().any(|descriptor| descriptor.eq_ignore_ascii_case(name.trim()))
}

fn plain_length(token: &str) -> bool {
    !token.ends_with('%')
        && !token.starts_with('-')
        && token.starts_with(|ch: char| ch.is_ascii_digit() || ch == '.' || ch == '+')
        && supports_declaration("width", token)
}

fn page_size(value: &str) -> Option<String> {
    let tokens: Vec<String> = value.split_ascii_whitespace().map(str::to_ascii_lowercase).collect();
    if tokens.is_empty() || tokens.len() > 2 {
        return None;
    }
    if tokens.len() == 1 && tokens[0] == "auto" {
        return Some(tokens[0].clone());
    }
    if tokens.iter().all(|token| plain_length(token)) {
        return Some(tokens.join(" "));
    }
    let sized = |token: &str| matches!(token, "a5" | "a4" | "a3" | "b5" | "b4" | "jis-b5" | "jis-b4" | "letter" | "legal" | "ledger");
    let oriented = |token: &str| matches!(token, "portrait" | "landscape");
    let valid = match tokens.as_slice() {
        [one] => sized(one) || oriented(one),
        [first, second] => (sized(first) && oriented(second)) || (oriented(first) && sized(second)),
        _ => false,
    };
    valid.then(|| tokens.join(" "))
}

/// The canonical value of a page descriptor; `None` when the value is invalid.
pub fn descriptor_value(name: &str, value: &str) -> Option<String> {
    let value = value.trim();
    match name.trim().to_ascii_lowercase().as_str() {
        "size" => page_size(value),
        "page-orientation" => {
            let keyword = value.to_ascii_lowercase();
            matches!(keyword.as_str(), "upright" | "rotate-left" | "rotate-right").then_some(keyword)
        }
        "marks" => {
            let tokens: Vec<String> = value.split_ascii_whitespace().map(str::to_ascii_lowercase).collect();
            let valid = match tokens.as_slice() {
                [one] => matches!(one.as_str(), "none" | "crop" | "cross"),
                [first, second] => first != second && matches!(first.as_str(), "crop" | "cross") && matches!(second.as_str(), "crop" | "cross"),
                _ => false,
            };
            valid.then(|| tokens.join(" "))
        }
        "bleed" => {
            let keyword = value.to_ascii_lowercase();
            (keyword == "auto" || (plain_length(value) && !value.contains(char::is_whitespace))).then_some(keyword)
        }
        _ => None,
    }
}

/// Declarations split into those handled by the property registry and the page-only descriptors,
/// in source order with later duplicates winning.
pub fn split_descriptors(text: &str) -> (String, Vec<(String, String)>) {
    let mut generic = String::new();
    let mut descriptors: Vec<(String, String)> = Vec::new();
    let Ok(spans) = declaration_spans_with_recovery(text, true) else {
        return (generic, descriptors);
    };
    for (start, end) in spans {
        let part = &text[start..end];
        let Some((name, raw)) = declaration_pair(part) else {
            continue;
        };
        let Some(name) = parsed_declaration_name(name) else {
            continue;
        };
        if !is_descriptor(&name) {
            generic.push_str(part.trim());
            generic.push(';');
            continue;
        }
        let (value, important) = important_value(raw);
        let name = name.to_ascii_lowercase();
        let Some(value) = descriptor_value(&name, value).filter(|_| !important) else {
            continue;
        };
        descriptors.retain(|(known, _)| *known != name);
        descriptors.push((name, value));
    }
    (generic, descriptors)
}

/// `name: value;` pairs appended after the registry declarations.
pub fn descriptors_text(descriptors: &[(String, String)]) -> String {
    let mut out = String::new();
    for (name, value) in descriptors {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(name);
        out.push_str(": ");
        out.push_str(value);
        out.push(';');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_selectors_are_canonicalized_and_validated() {
        assert_eq!(selector_list(""), Some(String::new()));
        assert_eq!(selector_list("named:First").as_deref(), Some("named:first"));
        assert_eq!(selector_list(":left , :right").as_deref(), Some(":left, :right"));
        assert!(selector_list("named :first").is_none());
        assert!(selector_list(":first :left").is_none());
        assert!(selector_list(":notapagepseudo").is_none());
    }

    #[test]
    fn descriptors_validate_their_grammar() {
        assert_eq!(descriptor_value("size", "jis-b5 landscape").as_deref(), Some("jis-b5 landscape"));
        assert_eq!(descriptor_value("size", "216mm").as_deref(), Some("216mm"));
        assert!(descriptor_value("size", "notarealsize").is_none());
        assert!(descriptor_value("page-orientation", "schmotate-schmeft").is_none());
    }
}
