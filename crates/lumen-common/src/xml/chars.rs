//! Character classes and the input encodings.

use crate::smuggle::Spelling;
use crate::utf;
use std::borrow::Cow;
use std::convert::Infallible;

/// Stands for an undecodable byte or unpaired surrogate; it is not an XML character, so the
/// tokenizer reports it as an invalid token at that spot.
pub const MALFORMED: char = '\u{FFFF}';

pub fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
}

pub fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

pub fn is_name_start(c: char) -> bool {
    matches!(c,
        ':' | 'A'..='Z' | '_' | 'a'..='z'
        | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}' | '\u{F8}'..='\u{2FF}'
        | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}' | '\u{200C}'..='\u{200D}'
        | '\u{2070}'..='\u{218F}' | '\u{2C00}'..='\u{2FEF}' | '\u{3001}'..='\u{D7FF}'
        | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}' | '\u{10000}'..='\u{EFFFF}')
}

pub fn is_name_char(c: char) -> bool {
    is_name_start(c) || matches!(c, '-' | '.' | '0'..='9' | '\u{B7}' | '\u{300}'..='\u{36F}' | '\u{203F}'..='\u{2040}')
}

pub fn is_pubid_char(c: char) -> bool {
    matches!(c, ' ' | '\r' | '\n' | 'a'..='z' | 'A'..='Z' | '0'..='9'
        | '-' | '\'' | '(' | ')' | '+' | ',' | '.' | '/' | ':' | '=' | '?' | ';' | '!' | '*' | '#' | '@' | '$' | '_' | '%')
}

/// Whether `s` is entirely one name.
pub fn is_name(s: &str) -> bool {
    let mut it = s.chars();
    matches!(it.next(), Some(c) if is_name_start(c)) && it.all(is_name_char)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Enc {
    Utf8,
    Utf16Le,
    Utf16Be,
    Latin1,
    Ascii,
    /// A single-byte table from the handler: `>= 0` is the code point, `< 0` is invalid.
    Table(Vec<i32>),
}

impl Enc {
    pub fn is_utf16(&self) -> bool {
        matches!(self, Enc::Utf16Le | Enc::Utf16Be)
    }

    /// The encodings expat knows by name.
    pub fn from_name(name: &str) -> Option<Enc> {
        let n = name.to_ascii_uppercase();
        Some(match n.as_str() {
            "UTF-8" => Enc::Utf8,
            "ISO-8859-1" => Enc::Latin1,
            "US-ASCII" => Enc::Ascii,
            "UTF-16LE" => Enc::Utf16Le,
            "UTF-16BE" => Enc::Utf16Be,
            _ => return None,
        })
    }

    /// Bytes the character occupied in the input.
    pub fn width(&self, c: char) -> u64 {
        match self {
            Enc::Utf8 => {
                if c == MALFORMED {
                    1
                } else {
                    c.len_utf8() as u64
                }
            }
            Enc::Utf16Le | Enc::Utf16Be => (c.len_utf16() * 2) as u64,
            _ => 1,
        }
    }
}

pub enum Step {
    Char(char, usize),
    Bad(usize),
    Incomplete,
}

/// Decodes one character from the front of `raw`.
pub fn next_char(enc: &Enc, raw: &[u8]) -> Step {
    if raw.is_empty() {
        return Step::Incomplete;
    }
    match enc {
        Enc::Utf8 | Enc::Utf16Le | Enc::Utf16Be => {
            let step = match enc {
                Enc::Utf8 => utf::step_utf8(raw, 0, false),
                _ => utf::step_utf16(raw, 0, *enc == Enc::Utf16Be, false),
            };
            match step {
                utf::Step::Char(cp, n) => Step::Char(char::from_u32(cp).unwrap_or(MALFORMED), n),
                utf::Step::Bad(m) => Step::Bad((m.end - m.start).max(1)),
                utf::Step::Incomplete => Step::Incomplete,
            }
        }
        Enc::Latin1 => Step::Char(raw[0] as char, 1),
        Enc::Ascii => {
            if raw[0] < 0x80 {
                Step::Char(raw[0] as char, 1)
            } else {
                Step::Bad(1)
            }
        }
        Enc::Table(t) => match t.get(raw[0] as usize).copied().and_then(|v| u32::try_from(v).ok()).and_then(char::from_u32) {
            Some(c) => Step::Char(c, 1),
            None => Step::Bad(1),
        },
    }
}

/// Decodes as much of `raw` as possible; returns the bytes used. Stops at an incomplete character.
pub fn decode_into(enc: &Enc, raw: &[u8], out: &mut String) -> usize {
    if matches!(enc, Enc::Utf8 | Enc::Utf16Le | Enc::Utf16Be) {
        let mut data = Cow::Borrowed(raw);
        let on_error = |_: &mut Cow<[u8]>, m: utf::Malformed, out: &mut String| -> Result<usize, Infallible> {
            out.push(MALFORMED);
            Ok(m.end.max(m.start + 1))
        };
        let used = match enc {
            Enc::Utf8 => utf::decode_utf8(&mut data, false, Spelling::Plain, out, on_error),
            _ => utf::decode_utf16(&mut data, 0, *enc == Enc::Utf16Be, false, Spelling::Plain, out, on_error),
        };
        return used.unwrap_or(0);
    }
    let mut i = 0;
    while i < raw.len() {
        match next_char(enc, &raw[i..]) {
            Step::Char(c, n) => {
                out.push(c);
                i += n;
            }
            Step::Bad(n) => {
                out.push(MALFORMED);
                i += n;
            }
            Step::Incomplete => break,
        }
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded(enc: &Enc, raw: &[u8]) -> (String, usize) {
        let mut out = String::new();
        let used = decode_into(enc, raw, &mut out);
        (out, used)
    }

    #[test]
    fn utf8_marks_malformed_and_waits_on_partial() {
        assert_eq!(decoded(&Enc::Utf8, b"a\xFFb\xE2\x82"), ("a\u{FFFF}b".to_string(), 3));
        assert_eq!(decoded(&Enc::Utf8, "é€".as_bytes()), ("é€".to_string(), 5));
    }

    #[test]
    fn utf16_pairs_lone_surrogates_and_partial() {
        let le = [0x61, 0, 0x3D, 0xD8, 0x00, 0xDE, 0x00, 0xDC, 0x62, 0, 0x3D];
        assert_eq!(decoded(&Enc::Utf16Le, &le), ("a\u{1F600}\u{FFFF}b".to_string(), 10));
        assert_eq!(decoded(&Enc::Utf16Be, &[0xD8, 0x3D]).1, 0);
    }

    #[test]
    fn next_char_steps() {
        assert!(matches!(next_char(&Enc::Utf8, "€".as_bytes()), Step::Char('€', 3)));
        assert!(matches!(next_char(&Enc::Utf8, &[0xE2, 0x82]), Step::Incomplete));
        assert!(matches!(next_char(&Enc::Utf8, &[0xC0, 0x80]), Step::Bad(1)));
        assert!(matches!(next_char(&Enc::Utf16Be, &[0xDC, 0x00]), Step::Bad(2)));
        assert!(matches!(next_char(&Enc::Utf16Be, &[0x00]), Step::Incomplete));
    }
}
