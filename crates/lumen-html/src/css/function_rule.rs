//! CSS Mixins: the `@function` prelude and the `<dashed-function>` call
//! grammar. Function bodies are not evaluated; calls stay unresolved and
//! compute as invalid at computed-value time.
use super::identifier::serialize_identifier;
pub(super) use super::syntax::matching_close;
use super::syntax::{split_top_level, top_level_find};
use super::registered_properties::{accepts_value, valid_typed_syntax};
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parameter {
    pub name: String,
    pub syntax: String,
    pub default: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionPrelude {
    pub name: String,
    pub parameters: Vec<Parameter>,
    pub return_type: String,
}

impl FunctionPrelude {
    pub fn serialize(&self) -> String {
        let mut output = serialize_identifier(&self.name);
        output.push('(');
        for (index, parameter) in self.parameters.iter().enumerate() {
            if index > 0 {
                output.push_str(", ");
            }
            output.push_str(&serialize_identifier(&parameter.name));
            if parameter.syntax != "*" {
                output.push(' ');
                output.push_str(&serialize_type(&parameter.syntax));
            }
            if let Some(default) = &parameter.default {
                output.push_str(": ");
                output.push_str(default);
            }
        }
        output.push(')');
        if self.return_type != "*" {
            output.push_str(" returns ");
            output.push_str(&serialize_type(&self.return_type));
        }
        output
    }
}

fn serialize_type(syntax: &str) -> String {
    if syntax.contains('|') {
        alloc::format!("type({syntax})")
    } else {
        syntax.into()
    }
}

fn string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let quote = bytes[start];
    let mut position = start + 1;
    while position < bytes.len() {
        match bytes[position] {
            byte if byte == quote => return Some(position),
            b'\\' => position += 1,
            b'\n' => return None,
            _ => {}
        }
        position += 1;
    }
    None
}

fn declaration_value(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut position = 0;
    while position < bytes.len() {
        match bytes[position] {
            b'(' | b'[' | b'{' => {
                if matching_close(text, position).map(|close| position = close).is_none() {
                    return false;
                }
            }
            b')' | b']' | b'}' | b';' | b'!' => return false,
            b'"' | b'\'' => match string_end(bytes, position) {
                Some(end) => position = end,
                None => return false,
            },
            b'\\' => position += 1,
            _ => {}
        }
        position += 1;
    }
    !text.trim().is_empty()
}

fn mentions_substitution(value: &str) -> bool {
    value.to_ascii_lowercase().contains("var(") || contains_call(value)
}

fn parse_name(text: &str) -> Option<(String, usize)> {
    let mut position = 0;
    let name = super::consume_selector_identifier(text, &mut position)?;
    (name.starts_with("--") && name.len() > 2).then_some((name, position))
}

fn parse_type(text: &str, allow_universal_bare: bool) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return Some(String::from("*"));
    }
    if let Some(rest) = text.strip_prefix("type(").or_else(|| text.strip_prefix("TYPE(")) {
        let inner = rest.strip_suffix(')')?.trim();
        if matching_close(text, 4)? != text.len() - 1 {
            return None;
        }
        if inner == "*" {
            return Some(String::from("*"));
        }
        return valid_typed_syntax(inner).then(|| {
            inner.split('|').map(str::trim).collect::<Vec<_>>().join(" | ")
        });
    }
    if text == "*" {
        return allow_universal_bare.then(|| String::from("*"));
    }
    (!text.contains('|') && valid_typed_syntax(text)).then(|| text.into())
}

fn parse_parameter(piece: &str) -> Option<Parameter> {
    let piece = piece.trim();
    let (name, used) = parse_name(piece)?;
    let rest = &piece[used..];
    let (type_text, default_text) = match top_level_find(rest, b':').flatten() {
        Some(colon) => (&rest[..colon], Some(rest[colon + 1..].trim())),
        None => (rest, None),
    };
    if !type_text.is_empty() && !type_text.starts_with(char::is_whitespace) {
        return None;
    }
    let syntax = parse_type(type_text, false)?;
    let default = match default_text {
        Some(default) => {
            if !declaration_value(default) {
                return None;
            }
            if syntax != "*" && !mentions_substitution(default) && !accepts_value(&syntax, default) {
                return None;
            }
            Some(String::from(default))
        }
        None => None,
    };
    Some(Parameter { name, syntax, default })
}

/// Parse the text after `@function`.
pub fn parse_prelude(prelude: &str) -> Option<FunctionPrelude> {
    let prelude = prelude.trim();
    let (name, used) = parse_name(prelude)?;
    if prelude.as_bytes().get(used) != Some(&b'(') {
        return None;
    }
    let close = matching_close(prelude, used)?;
    let params_text = &prelude[used + 1..close];
    let mut parameters = Vec::new();
    if !params_text.trim().is_empty() {
        for piece in split_top_level(params_text, b',', usize::MAX, usize::MAX)? {
            parameters.push(parse_parameter(piece)?);
        }
    }
    let rest = prelude[close + 1..].trim();
    let return_type = if rest.is_empty() {
        String::from("*")
    } else {
        let keyword = rest.get(..7)?;
        if !keyword.eq_ignore_ascii_case("returns") || !rest[7..].starts_with(char::is_whitespace) {
            return None;
        }
        let type_text = rest[7..].trim();
        if type_text.is_empty() {
            return None;
        }
        parse_type(type_text, false)?
    };
    Some(FunctionPrelude { name, parameters, return_type })
}

/// Whether `raw` contains a `--name(` call outside strings.
pub fn contains_call(raw: &str) -> bool {
    !call_positions(raw).is_empty() || super::if_function::contains_if(raw)
}

pub(super) fn call_positions(raw: &str) -> Vec<(usize, usize)> {
    let bytes = raw.as_bytes();
    let mut calls = Vec::new();
    let mut position = 0;
    while position < bytes.len() {
        match bytes[position] {
            b'"' | b'\'' => match string_end(bytes, position) {
                Some(end) => position = end,
                None => break,
            },
            b'\\' => position += 1,
            b'-' if bytes.get(position + 1) == Some(&b'-')
                && (position == 0 || !is_name_byte(bytes[position - 1])) =>
            {
                let mut end = position + 2;
                while end < bytes.len() && is_name_byte(bytes[end]) {
                    end += 1;
                }
                if bytes.get(end) == Some(&b'(') && end > position + 2 {
                    calls.push((position, end));
                }
                position = end.saturating_sub(1);
            }
            _ => {}
        }
        position += 1;
    }
    calls
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || !byte.is_ascii()
}

fn valid_argument(argument: &str) -> bool {
    let argument = argument.trim();
    if argument.is_empty() {
        return false;
    }
    if argument.starts_with('{') {
        return match matching_close(argument, 0) {
            Some(close) if close == argument.len() - 1 => declaration_value(&argument[1..close]),
            _ => false,
        };
    }
    if !declaration_value(argument) || begins_named_argument(argument) {
        return false;
    }
    let bytes = argument.as_bytes();
    let mut position = 0;
    while position < bytes.len() {
        match bytes[position] {
            b'(' | b'[' => match matching_close(argument, position) {
                Some(close) => position = close,
                None => return false,
            },
            b'{' | b'}' => return false,
            b'"' | b'\'' => match string_end(bytes, position) {
                Some(end) => position = end,
                None => return false,
            },
            b'\\' => position += 1,
            _ => {}
        }
        position += 1;
    }
    !has_bad_url(argument)
}

/// `<dashed-ident> <colon>` at the start of an argument is reserved for named arguments.
fn begins_named_argument(argument: &str) -> bool {
    let bytes = argument.as_bytes();
    if !argument.starts_with("--") {
        return false;
    }
    let mut end = 2;
    while end < bytes.len() && is_name_byte(bytes[end]) {
        end += 1;
    }
    end > 2 && argument[end..].trim_start().starts_with(':')
}

fn has_bad_url(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find("url(") {
        let start = from + found + 4;
        let rest = text[start..].trim_start();
        if !rest.starts_with(['"', '\'']) {
            let end = rest.find(')').unwrap_or(rest.len());
            if rest[..end].contains("\\\n") {
                return true;
            }
        }
        from = start;
    }
    false
}

/// Every `--name(...)` call in `raw` has a well-formed argument list.
pub fn calls_valid(raw: &str) -> bool {
    for (_, open) in call_positions(raw) {
        let Some(close) = matching_close(raw, open) else {
            return false;
        };
        let inner = &raw[open + 1..close];
        if inner.trim().is_empty() {
            continue;
        }
        let Some(arguments) = split_top_level(inner, b',', usize::MAX, usize::MAX) else {
            return false;
        };
        if !arguments.into_iter().all(valid_argument) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_preludes() {
        assert!(parse_prelude("--foo()").is_some());
        assert!(parse_prelude("--foo (--x)").is_none());
        let parsed = parse_prelude("--f5(--x type(<length> | auto): 10px, --y, --z: red) returns <length>+").unwrap();
        assert_eq!(parsed.parameters.len(), 3);
        assert_eq!(parsed.parameters[0].syntax, "<length> | auto");
        assert_eq!(parsed.return_type, "<length>+");
        assert!(parse_prelude("--foo(--x <length>: 10deg)").is_none());
        assert!(parse_prelude("--foo(--x) returns *").is_none());
        assert!(parse_prelude("--foo(--x: var(--y, 10px), --z: 2px)").is_some());
    }

    #[test]
    fn validates_calls() {
        assert!(calls_valid("--func({1}, 2)"));
        assert!(!calls_valid("--func(a,,b)"));
        assert!(!calls_valid("--func(red !important)"));
        assert!(!calls_valid("--func({})"));
        assert!(!calls_valid("--func(1{})"));
        assert!(calls_valid("--func(--bar(), --baz(--fez()))"));
        assert!(!calls_valid("--func(--myident :)"));
        assert!(!calls_valid("--func(10px, --myident: )"));
        assert!(calls_valid("--func(50px --myident:)"));
        assert!(calls_valid("--func({--myident:})"));
    }
}
