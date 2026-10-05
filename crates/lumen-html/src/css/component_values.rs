//! Bounded CSS component-value reification for Typed OM.
//!
//! This is deliberately separate from declaration grammar parsing: Typed OM must
//! preserve raw component values while exposing nested `var()` functions as
//! objects, even when the containing property's grammar is otherwise unknown.

use super::{
    CssError, MAX_VARIABLE_BYTES, MAX_VARIABLE_DEPTH, matching_css_block,
    quoted_css_end, selector_escape, valid_custom_property_name,
};
use alloc::{borrow::ToOwned, string::String, vec::Vec};

const MAX_COMPONENTS: usize = 1024;

/// One maximal raw-token span or one CSS variable reference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UnparsedComponent {
    Text(String),
    Variable {
        /// CSS serialization of the custom-property name token.
        name: String,
        fallback: Option<Vec<UnparsedComponent>>,
    },
}

/// Split a CSS value into maximal text spans and recursive `var()` references.
/// Strings, comments, escapes, and nested component blocks are scanned according
/// to CSS token boundaries; limits match the shared variable-expansion budget.
pub fn parse_unparsed_value(input: &str) -> Result<Vec<UnparsedComponent>, CssError> {
    let mut remaining = MAX_COMPONENTS;
    parse_inner(input, 0, 0, &mut remaining)
}

/// Serialize a parsed component tree using Typed OM's raw-token serialization.
/// Adjacent text spans are separated with `/**/` so concatenation cannot merge
/// two component values. The output has the same byte and nesting bounds as the
/// parser and variable expansion paths.
pub fn serialize_unparsed_value(
    components: &[UnparsedComponent],
) -> Result<String, CssError> {
    let mut output = String::new();
    serialize_inner(components, 0, &mut output)?;
    Ok(output)
}

fn parse_inner(
    input: &str,
    base_offset: usize,
    depth: usize,
    remaining: &mut usize,
) -> Result<Vec<UnparsedComponent>, CssError> {
    if depth > MAX_VARIABLE_DEPTH {
        return Err(error(base_offset, "CSS variable nesting limit exceeded"));
    }
    if input.len() > MAX_VARIABLE_BYTES {
        return Err(error(base_offset, "CSS component value is too large"));
    }

    let bytes = input.as_bytes();
    let mut position = 0usize;
    let mut text = String::new();
    let mut result = Vec::new();

    while position < bytes.len() {
        let byte = bytes[position];
        if byte == b'/' && bytes.get(position + 1) == Some(&b'*') {
            let Some(end) = input[position + 2..].find("*/") else {
                return Err(error(base_offset + position, "unterminated CSS comment"));
            };
            // Comments disappear during tokenization. Keep a serialization
            // boundary so adjacent identifiers do not become one token.
            text.push_str("/**/");
            position += end + 4;
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            let Some(end) = quoted_css_end(input, position) else {
                return Err(error(base_offset + position, "unterminated CSS string"));
            };
            text.push_str(&input[position..end]);
            position = end;
            continue;
        }
        if byte == b'\\' {
            let start = position;
            selector_escape(input, &mut position)
                .ok_or_else(|| error(base_offset + start, "invalid CSS escape"))?;
            text.push_str(&input[start..position]);
            continue;
        }

        if let Some((ident, end)) = consume_ident(input, position) {
            if ident.eq_ignore_ascii_case("var") && bytes.get(end) == Some(&b'(') {
                let Some(after) = matching_css_block(input, end) else {
                    return Err(error(base_offset + end, "malformed var() component"));
                };
                let body_start = end + 1;
                let body_end = after - 1;
                let body = &input[body_start..body_end];
                let (name, fallback, fallback_offset) =
                    parse_var_arguments(body, base_offset + body_start)?;
                let fallback = fallback
                    .map(|value| {
                        parse_inner(
                            value,
                            base_offset + fallback_offset,
                            depth + 1,
                            remaining,
                        )
                    })
                    .transpose()?;

                if !text.is_empty() {
                    push_component(
                        &mut result,
                        UnparsedComponent::Text(core::mem::take(&mut text)),
                        remaining,
                        base_offset + position,
                    )?;
                }
                push_component(
                    &mut result,
                    UnparsedComponent::Variable { name, fallback },
                    remaining,
                    base_offset + position,
                )?;
                position = after;
                continue;
            }
            text.push_str(&input[position..end]);
            position = end;
            continue;
        }

        let character = input[position..]
            .chars()
            .next()
            .ok_or_else(|| error(base_offset + position, "invalid CSS character"))?;
        text.push(character);
        position += character.len_utf8();
        if text.len() > MAX_VARIABLE_BYTES {
            return Err(error(base_offset + position, "CSS component value is too large"));
        }
    }

    if !text.is_empty() {
        push_component(
            &mut result,
            UnparsedComponent::Text(text),
            remaining,
            base_offset + input.len(),
        )?;
    }
    Ok(result)
}

fn parse_var_arguments<'a>(
    body: &'a str,
    base_offset: usize,
) -> Result<(String, Option<&'a str>, usize), CssError> {
    let comma = top_level_comma(body, base_offset)?;
    let name_end = comma.unwrap_or(body.len());
    let name_slice = &body[..name_end];
    let mut position = 0usize;
    skip_trivia(name_slice, &mut position, base_offset)?;
    let start = position;
    let Some((decoded, end)) = consume_ident(name_slice, position) else {
        return Err(error(base_offset + position, "var() requires a custom property name"));
    };
    if !decoded.starts_with("--") {
        return Err(error(base_offset + start, "var() requires a custom property name"));
    }
    let name = name_slice[start..end].to_owned();
    if !valid_custom_property_name(&name) {
        return Err(error(base_offset + start, "invalid custom property name in var()"));
    }
    position = end;
    skip_trivia(name_slice, &mut position, base_offset)?;
    if position != name_slice.len() {
        return Err(error(base_offset + position, "unexpected token after var() name"));
    }

    let Some(comma) = comma else {
        return Ok((name, None, base_offset + body.len()));
    };
    let fallback_start = comma + 1;
    Ok((
        name,
        Some(&body[fallback_start..]),
        base_offset + fallback_start,
    ))
}

fn top_level_comma(input: &str, base_offset: usize) -> Result<Option<usize>, CssError> {
    let bytes = input.as_bytes();
    let mut position = 0usize;
    while position < bytes.len() {
        match bytes[position] {
            b'/' if bytes.get(position + 1) == Some(&b'*') => {
                let Some(end) = input[position + 2..].find("*/") else {
                    return Err(error(base_offset + position, "unterminated CSS comment"));
                };
                position += end + 4;
            }
            b'\\' => {
                selector_escape(input, &mut position)
                    .ok_or_else(|| error(base_offset + position, "invalid CSS escape"))?;
            }
            quote @ (b'\'' | b'"') => {
                let _ = quote;
                position = quoted_css_end(input, position)
                    .ok_or_else(|| error(base_offset + position, "unterminated CSS string"))?;
            }
            open @ (b'(' | b'[' | b'{') => {
                let Some(after) = matching_css_block(input, position) else {
                    return Err(error(base_offset + position, "unbalanced CSS component block"));
                };
                debug_assert_eq!(bytes[position], open);
                position = after;
            }
            b')' | b']' | b'}' => {
                return Err(error(base_offset + position, "unbalanced CSS component block"));
            }
            b',' => return Ok(Some(position)),
            _ => {
                position += input[position..]
                    .chars()
                    .next()
                    .map_or(1, char::len_utf8);
            }
        }
    }
    Ok(None)
}

fn skip_trivia(input: &str, position: &mut usize, base_offset: usize) -> Result<(), CssError> {
    loop {
        while input
            .as_bytes()
            .get(*position)
            .is_some_and(|byte| matches!(byte, b'\t' | b'\n' | b'\x0c' | b'\r' | b' '))
        {
            *position += 1;
        }
        if input[*position..].starts_with("/*") {
            let Some(end) = input[*position + 2..].find("*/") else {
                return Err(error(base_offset + *position, "unterminated CSS comment"));
            };
            *position += end + 4;
        } else {
            return Ok(());
        }
    }
}

fn consume_ident(input: &str, start: usize) -> Option<(String, usize)> {
    let bytes = input.as_bytes();
    let first = *bytes.get(start)?;
    let begins_name = first == b'\\'
        || first == b'_'
        || first.is_ascii_alphabetic()
        || first >= 0x80
        || first == b'-';
    if !begins_name {
        return None;
    }

    let mut value = String::new();
    let mut position = start;
    let mut first_character = true;
    while position < bytes.len() {
        let byte = bytes[position];
        if byte == b'\\' {
            value.push(selector_escape(input, &mut position)?);
            first_character = false;
        } else if byte == b'-' || byte == b'_' || byte.is_ascii_alphabetic() || byte >= 0x80 {
            let character = input[position..].chars().next()?;
            value.push(character);
            position += character.len_utf8();
            first_character = false;
        } else if byte.is_ascii_digit() && !first_character {
            value.push(byte as char);
            position += 1;
        } else {
            break;
        }
    }
    (position > start).then_some((value, position))
}

fn push_component(
    output: &mut Vec<UnparsedComponent>,
    component: UnparsedComponent,
    remaining: &mut usize,
    offset: usize,
) -> Result<(), CssError> {
    if *remaining == 0 {
        return Err(error(offset, "too many CSS component values"));
    }
    output
        .try_reserve(1)
        .map_err(|_| error(offset, "CSS component value allocation failed"))?;
    output.push(component);
    *remaining -= 1;
    Ok(())
}

fn serialize_inner(
    components: &[UnparsedComponent],
    depth: usize,
    output: &mut String,
) -> Result<(), CssError> {
    if depth > MAX_VARIABLE_DEPTH {
        return Err(error(0, "CSS variable nesting limit exceeded"));
    }
    let mut previous_text = false;
    for component in components {
        match component {
            UnparsedComponent::Text(text) => {
                if previous_text {
                    append_bounded(output, "/**/", 0)?;
                }
                append_bounded(output, text, 0)?;
                previous_text = true;
            }
            UnparsedComponent::Variable { name, fallback } => {
                append_bounded(output, "var(", 0)?;
                append_bounded(output, name, 0)?;
                if let Some(fallback) = fallback {
                    append_bounded(output, ",", 0)?;
                    serialize_inner(fallback, depth + 1, output)?;
                }
                append_bounded(output, ")", 0)?;
                previous_text = false;
            }
        }
    }
    Ok(())
}

fn append_bounded(output: &mut String, value: &str, offset: usize) -> Result<(), CssError> {
    if output.len().saturating_add(value.len()) > MAX_VARIABLE_BYTES {
        return Err(error(offset, "serialized CSS component value is too large"));
    }
    output
        .try_reserve(value.len())
        .map_err(|_| error(offset, "CSS serialization allocation failed"))?;
    output.push_str(value);
    Ok(())
}

fn error(offset: usize, message: &'static str) -> CssError {
    CssError { offset, message }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec};

    #[test]
    fn reifies_nested_variables_without_scanning_strings_or_comments() {
        let parsed = parse_unparsed_value(
            "calc(42px + var(--foo, 15em) + var(--bar, var(--far) + 15px))",
        )
        .unwrap();
        assert_eq!(
            parsed,
            vec![
                UnparsedComponent::Text("calc(42px + ".into()),
                UnparsedComponent::Variable {
                    name: "--foo".into(),
                    fallback: Some(vec![UnparsedComponent::Text(" 15em".into())]),
                },
                UnparsedComponent::Text(" + ".into()),
                UnparsedComponent::Variable {
                    name: "--bar".into(),
                    fallback: Some(vec![
                        UnparsedComponent::Text(" ".into()),
                        UnparsedComponent::Variable {
                            name: "--far".into(),
                            fallback: None,
                        },
                        UnparsedComponent::Text(" + 15px".into()),
                    ]),
                },
                UnparsedComponent::Text(")".into()),
            ]
        );
        assert_eq!(
            parse_unparsed_value("'var(--no)' /* var(--also-no) */ var(--yes)").unwrap(),
            vec![
                UnparsedComponent::Text("'var(--no)' /**/ ".into()),
                UnparsedComponent::Variable {
                    name: "--yes".into(),
                    fallback: None,
                },
            ]
        );
    }

    #[test]
    fn preserves_escaped_names_and_all_fallback_commas() {
        let parsed = parse_unparsed_value(r"var(--a\,b, red, blue)").unwrap();
        assert_eq!(
            parsed,
            vec![UnparsedComponent::Variable {
                name: r"--a\,b".into(),
                fallback: Some(vec![UnparsedComponent::Text(" red, blue".into())]),
            }]
        );
        assert_eq!(
            serialize_unparsed_value(&parsed).unwrap(),
            r"var(--a\,b, red, blue)"
        );
    }

    #[test]
    fn serializes_constructed_adjacent_text_fragments_with_token_boundaries() {
        let value = vec![
            UnparsedComponent::Text("lem".into()),
            UnparsedComponent::Text("on".into()),
            UnparsedComponent::Text("ade".into()),
        ];
        assert_eq!(serialize_unparsed_value(&value).unwrap(), "lem/**/on/**/ade");
    }

    #[test]
    fn rejects_malformed_and_oversized_var_components_with_bounds() {
        for input in ["var()", "var(color)", "var(--x", "var(--x, 'unterminated)"] {
            assert!(parse_unparsed_value(input).is_err(), "accepted {input:?}");
        }
        let large = "x".repeat(MAX_VARIABLE_BYTES + 1);
        assert!(parse_unparsed_value(&large).is_err());
        let nested = format!("{}--x{}", "var(--x, ".repeat(MAX_VARIABLE_DEPTH + 2), ")".repeat(MAX_VARIABLE_DEPTH + 2));
        assert!(parse_unparsed_value(&nested).is_err());
    }
}
