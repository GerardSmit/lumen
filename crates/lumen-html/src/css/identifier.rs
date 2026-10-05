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
