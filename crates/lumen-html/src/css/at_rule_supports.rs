//! The `at-rule()` function of `@supports` and `CSS.supports()`.
use super::*;

const SUPPORTED: &[&str] = &[
    "annotation", "character-variant", "container", "counter-style", "font-face", "font-feature-values",
    "font-palette-values", "historical-forms", "import", "keyframes", "layer", "media", "namespace", "ornaments",
    "page", "position-try", "property", "scope", "starting-style", "styleset", "stylistic", "supports", "swash",
    "view-transition",
];

/// The decoded at-keyword name of an `at-rule()` argument, which is `@` followed by an identifier.
fn at_keyword(argument: &str) -> Option<String> {
    let mut position = 0;
    skip_css_space_comments(argument, &mut position)?;
    if argument.as_bytes().get(position) != Some(&b'@') {
        return None;
    }
    position += 1;
    let name = consume_selector_identifier(argument, &mut position)?;
    skip_css_space_comments(argument, &mut position)?;
    (position == argument.len()).then_some(name)
}

pub(super) fn supports(argument: &str) -> bool {
    at_keyword(argument).is_some_and(|name| SUPPORTED.iter().any(|known| name.eq_ignore_ascii_case(known)))
}

/// `condition` with each well-formed `at-rule()` argument serialized as `@` and the canonical identifier.
pub fn serialize_condition(condition: &str) -> String {
    let mut output = String::with_capacity(condition.len());
    let mut position = 0;
    while position < condition.len() {
        let rest = &condition[position..];
        let Some(found) = find_function(rest) else {
            output.push_str(rest);
            break;
        };
        let open = position + found + "at-rule".len();
        output.push_str(&condition[position..=open]);
        let Some(end) = matching_css_block(condition, open) else {
            position = open + 1;
            continue;
        };
        let argument = &condition[open + 1..end - 1];
        match at_keyword(argument) {
            Some(name) => {
                output.push('@');
                output.push_str(&identifier::serialize_identifier(&name));
            }
            None => output.push_str(argument),
        }
        output.push(')');
        position = end;
    }
    output
}

fn find_function(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index + "at-rule(".len() <= bytes.len() {
        let preceded = index > 0 && (bytes[index - 1].is_ascii_alphanumeric() || matches!(bytes[index - 1], b'-' | b'_' | b'\\'));
        if !preceded && text.get(index..index + "at-rule(".len()).is_some_and(|candidate| candidate.eq_ignore_ascii_case("at-rule(")) {
            return Some(index);
        }
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_at_rules_are_supported() {
        assert!(supports("@supports"));
        assert!(supports(" @media "));
        assert!(!supports("@charset"));
        assert!(!supports("@doesnotexist"));
        assert!(!supports("@counter-style; system: fixed"));
    }

    #[test]
    fn serialization_canonicalizes_the_identifier() {
        assert_eq!(serialize_condition("at-rule( @supports )"), "at-rule(@supports)");
        assert_eq!(serialize_condition("at-rule(@--\\31 23)"), "at-rule(@--123)");
        assert_eq!(serialize_condition("not at-rule(@media) and (color: red)"), "not at-rule(@media) and (color: red)");
    }
}
