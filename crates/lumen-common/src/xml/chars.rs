//! Character classes and the input encodings.

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
        Enc::Utf8 => {
            let b = raw[0];
            if b < 0x80 {
                return Step::Char(b as char, 1);
            }
            let need = match b {
                0xC2..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF4 => 4,
                _ => return Step::Bad(1),
            };
            let avail = raw.len().min(need);
            match std::str::from_utf8(&raw[..avail]) {
                Ok(s) => match s.chars().next() {
                    Some(c) if avail == need => Step::Char(c, need),
                    _ => Step::Incomplete,
                },
                Err(e) => match e.error_len() {
                    None => Step::Incomplete,
                    Some(n) => Step::Bad(n.max(1)),
                },
            }
        }
        Enc::Utf16Le | Enc::Utf16Be => {
            if raw.len() < 2 {
                return Step::Incomplete;
            }
            let unit = |i: usize| -> u16 {
                if *enc == Enc::Utf16Le {
                    u16::from_le_bytes([raw[i], raw[i + 1]])
                } else {
                    u16::from_be_bytes([raw[i], raw[i + 1]])
                }
            };
            let hi = unit(0);
            if (0xD800..0xDC00).contains(&hi) {
                if raw.len() < 4 {
                    return Step::Incomplete;
                }
                let lo = unit(2);
                if (0xDC00..0xE000).contains(&lo) {
                    let cp = 0x10000 + (((hi as u32) - 0xD800) << 10) + ((lo as u32) - 0xDC00);
                    return Step::Char(char::from_u32(cp).unwrap_or(MALFORMED), 4);
                }
                return Step::Bad(2);
            }
            if (0xDC00..0xE000).contains(&hi) {
                return Step::Bad(2);
            }
            Step::Char(char::from_u32(hi as u32).unwrap_or(MALFORMED), 2)
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
    let mut i = 0;
    if *enc == Enc::Utf8 {
        while i < raw.len() {
            match std::str::from_utf8(&raw[i..]) {
                Ok(s) => {
                    out.push_str(s);
                    return raw.len();
                }
                Err(e) => {
                    let ok = e.valid_up_to();
                    out.push_str(std::str::from_utf8(&raw[i..i + ok]).unwrap_or(""));
                    i += ok;
                    match e.error_len() {
                        None => return i,
                        Some(n) => {
                            out.push(MALFORMED);
                            i += n.max(1);
                        }
                    }
                }
            }
        }
        return i;
    }
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
