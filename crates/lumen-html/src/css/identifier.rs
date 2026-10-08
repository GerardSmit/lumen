//! CSSOM identifier serialization shared by CSS values and host utility APIs.

use alloc::string::String;
use core::fmt::Write;

/// Serialize an identifier according to CSSOM's common serializing idioms.
/// Streams the input, allocating only the returned escaped string.
pub fn serialize_identifier(value: &str) -> String {
    let mut serialized = String::with_capacity(value.len());
    let starts_with_dash = value.starts_with('-');
    for (index, character) in value.chars().enumerate() {
        if character == '\0' {
            serialized.push('\u{fffd}');
        } else if ('\u{1}'..='\u{1f}').contains(&character)
            || character == '\u{7f}'
            || (index == 0 && character.is_ascii_digit())
            || (index == 1 && starts_with_dash && character.is_ascii_digit())
        {
            let _ = write!(serialized, "\\{:x} ", character as u32);
        } else if index == 0 && value == "-" {
            serialized.push_str("\\-");
        } else if character >= '\u{80}'
            || character == '-'
            || character == '_'
            || character.is_ascii_alphanumeric()
        {
            serialized.push(character);
        } else {
            serialized.push('\\');
            serialized.push(character);
        }
    }
    serialized
}

/// Serialize a CSSOM string using the same scalar escaping for declarations,
/// descriptor values, URLs and rule text. No intermediate escaped copy is made.
pub fn serialize_string(value: &str) -> String {
    let mut serialized = String::with_capacity(value.len().saturating_add(2));
    append_string(&mut serialized, value);
    serialized
}

/// CSSOM URLs use a quoted string, including empty URLs and delimiter text.
pub fn serialize_url(value: &str) -> String {
    let mut serialized = String::with_capacity(value.len().saturating_add(7));
    serialized.push_str("url(");
    append_string(&mut serialized, value);
    serialized.push(')');
    serialized
}

fn append_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '\0' => output.push('\u{fffd}'),
            '\u{1}'..='\u{1f}' | '\u{7f}' => {
                let _ = write!(output, "\\{:x} ", character as u32);
            }
            '"' | '\\' => {
                output.push('\\');
                output.push(character);
            }
            _ => output.push(character),
        }
    }
    output.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specification_cssom_serialization_lexical_tokens_preserve_decoded_values() {
        for (value, expected) in [
            ("-", "\\-"), ("-9a", "-\\39 a"), ("9a", "\\39 a"),
            ("abc{}oops", "abc\\{\\}oops"), ("abc;oops!", "abc\\;oops\\!"),
            ("\0", "\u{fffd}"), ("é😀", "é😀"),
        ] {
            let serialized = serialize_identifier(value);
            assert_eq!(serialized, expected);
            let mut position = 0;
            let decoded = super::super::consume_selector_identifier(&serialized, &mut position).unwrap();
            assert_eq!(decoded, value.replace('\0', "\u{fffd}"));
            assert_eq!(position, serialized.len());
        }
        let mut value = String::from("null\0 quote\" slash\\ hex9a ");
        for character in 1u8..=31 { value.push(char::from(character)); }
        value.push_str("\u{7f}é😀'(){}");
        let normalized = value.replace('\0', "\u{fffd}");
        let serialized = serialize_string(&value);
        let decoded = super::super::css_string(&serialized).unwrap();
        assert_eq!(decoded, normalized);
        assert_eq!(serialize_string(&decoded), serialized);
        let url = serialize_url(&value);
        assert_eq!(super::super::background_url(&url).unwrap().as_ref(), normalized);
        assert_eq!(serialize_url(""), "url(\"\")");
    }
}
