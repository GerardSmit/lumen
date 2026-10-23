//! `@view-transition`: descriptor grammar and CSSOM serialization.
use super::*;
use alloc::{format, string::String, vec::Vec};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ViewTransitionRule {
    pub navigation: Option<&'static str>,
    /// `Some(empty)` is the keyword `none`.
    pub types: Option<Vec<String>>,
}

/// Whitespace-separated tokens; an escaped space belongs to its token.
fn tokens(value: &str) -> Vec<&str> {
    let bytes = value.as_bytes();
    let mut result = Vec::new();
    let mut start = None;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => {
                start.get_or_insert(index);
                index += 2;
                continue;
            }
            b' ' | b'\t' | b'\n' | b'\r' | 0x0c => {
                if let Some(first) = start.take() {
                    result.push(&value[first..index]);
                }
            }
            _ => {
                start.get_or_insert(index);
            }
        }
        index += 1;
    }
    if let Some(first) = start {
        result.push(&value[first..]);
    }
    result
}

fn type_name(token: &str) -> Option<String> {
    let mut position = 0;
    let name = consume_selector_identifier(token, &mut position)?;
    if position != token.len() {
        return None;
    }
    let reserved = matches!(
        name.to_ascii_lowercase().as_str(),
        "none" | "default" | "initial" | "inherit" | "unset" | "revert" | "revert-layer"
    );
    (!reserved && !name.starts_with("-ua-")).then_some(name)
}

/// The valid descriptors of a complete `@view-transition` rule; later valid
/// declarations replace earlier ones.
pub fn parse(css_text: &str) -> ViewTransitionRule {
    let mut rule = ViewTransitionRule::default();
    let (Some(open), Some(close)) = (css_text.find('{'), css_text.rfind('}')) else {
        return rule;
    };
    if close < open {
        return rule;
    }
    let Some(declarations) = top_level_split(&css_text[open + 1..close], b';', 256) else {
        return rule;
    };
    for declaration in declarations {
        let Some((name, value)) = declaration.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if value.contains('!') {
            continue;
        }
        match name.trim().to_ascii_lowercase().as_str() {
            "navigation" => match value.to_ascii_lowercase().as_str() {
                "auto" => rule.navigation = Some("auto"),
                "none" => rule.navigation = Some("none"),
                _ => {}
            },
            "types" => {
                let parts = tokens(value);
                if parts.len() == 1 && parts[0].eq_ignore_ascii_case("none") {
                    rule.types = Some(Vec::new());
                } else if !parts.is_empty() {
                    let names: Option<Vec<String>> = parts.into_iter().map(type_name).collect();
                    if let Some(names) = names {
                        rule.types = Some(names);
                    }
                }
            }
            _ => {}
        }
    }
    rule
}

impl ViewTransitionRule {
    pub fn serialize(&self) -> String {
        let mut parts = Vec::new();
        if let Some(navigation) = self.navigation {
            parts.push(format!("navigation: {navigation};"));
        }
        if let Some(types) = &self.types {
            let value = if types.is_empty() {
                String::from("none")
            } else {
                types
                    .iter()
                    .map(|name| identifier::serialize_identifier(name))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            parts.push(format!("types: {value};"));
        }
        if parts.is_empty() {
            String::from("@view-transition { }")
        } else {
            format!("@view-transition {{ {} }}", parts.join(" "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_follow_the_grammar() {
        assert_eq!(parse("@view-transition { navigation: auto gunk }").serialize(), "@view-transition { }");
        assert_eq!(parse("@view-transition { navigation: none !important }").navigation, None);
        assert_eq!(parse("@view-transition { types: nOnE }").serialize(), "@view-transition { types: none; }");
        assert_eq!(parse("@view-transition { types: abc\\ xyz }").types, Some(vec![String::from("abc xyz")]));
        assert_eq!(parse("@view-transition { types: abc -ua-x }").types, None);
        assert_eq!(parse("@view-transition { types: abc -uA-x }").types.map(|names| names.len()), Some(2));
    }
}
