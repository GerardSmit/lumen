//! Buffer's string codecs (utf8, utf16le, latin1, ascii, hex, base64, base64url),
//! with Node's exact semantics — including its lenient decoders: base64 skips characters
//! outside the alphabet and stops at `=`, hex stops at the first invalid pair, utf8 decodes with
//! U+FFFD replacement of maximal subparts (WHATWG / V8).
//!
//! JS strings reach Rust as lumen's internal UTF-8 in which a lone surrogate is *smuggled* as the
//! scalar `U+10F800 + (unit - 0xD800)` and a real character in that range is written as its
//! smuggled surrogate pair (see `lumen::jstr`). Encoders undo that: utf8 turns a lone surrogate
//! into U+FFFD's bytes and a smuggled pair back into the real 4-byte character; the unit-based
//! encoders (latin1, utf16le) see the original UTF-16 units. Decoders producing a character at or
//! above U+10F800 write it as its smuggled pair.

/// Encoding ids shared with `buffer.js` (`ENC` there).
pub const UTF8: u32 = 0;
pub const LATIN1: u32 = 1;
pub const ASCII: u32 = 2;
pub const HEX: u32 = 3;
pub const BASE64: u32 = 4;
pub const BASE64URL: u32 = 5;
pub const UTF16LE: u32 = 6;

use lumen_common::smuggle::{may_contain as may_smuggle, smuggle, smuggled, Spelling};
use lumen_common::utf;
use std::borrow::Cow;
use std::convert::Infallible;

/// Call `f` with each UTF-16 code unit of `s` (smuggled scalars decode to their surrogates).
#[inline]
fn for_each_unit(s: &str, mut f: impl FnMut(u16)) {
    for c in s.chars() {
        if let Some(u) = smuggled(c) {
            f(u);
        } else {
            let v = c as u32;
            if v < 0x10000 {
                f(v as u16);
            } else {
                let v = v - 0x10000;
                f(0xD800 + (v >> 10) as u16);
                f(0xDC00 + (v & 0x3FF) as u16);
            }
        }
    }
}

/// UTF-16 length of `s`.
pub fn unit_len(s: &str) -> usize {
    if s.is_ascii() {
        return s.len();
    }
    let mut n = 0;
    for_each_unit(s, |_| n += 1);
    n
}

// ---- utf8 -------------------------------------------------------------------------------------

/// The UTF-8 encoding Node produces for `s` (lone surrogates become U+FFFD).
pub fn utf8_encode(s: &str) -> Vec<u8> {
    if !may_smuggle(s) {
        return s.as_bytes().to_vec();
    }
    let mut out = Vec::with_capacity(s.len());
    let mut units = Vec::with_capacity(2);
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match smuggled(c) {
            None => {
                let mut b = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            }
            Some(u) => {
                units.clear();
                units.push(u);
                // A smuggled high followed by a smuggled low is one real character.
                if (0xD800..0xDC00).contains(&u) {
                    if let Some(lo) = chars.peek().and_then(|&n| smuggled(n)) {
                        if (0xDC00..0xE000).contains(&lo) {
                            chars.next();
                            let cp = 0x10000 + (((u as u32) - 0xD800) << 10) + (lo as u32 - 0xDC00);
                            let mut b = [0u8; 4];
                            let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
                            out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
                            continue;
                        }
                    }
                }
                out.extend_from_slice("\u{FFFD}".as_bytes());
            }
        }
    }
    out
}

/// `Buffer.byteLength(s, 'utf8')` without encoding.
pub fn utf8_len(s: &str) -> usize {
    if !may_smuggle(s) {
        return s.len();
    }
    // Each smuggled scalar is 4 internal bytes: a lone one encodes to 3 (U+FFFD), a pair to 4.
    let mut n = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match smuggled(c) {
            None => n += c.len_utf8(),
            Some(u) => {
                if (0xD800..0xDC00).contains(&u)
                    && chars
                        .peek()
                        .and_then(|&c2| smuggled(c2))
                        .is_some_and(|lo| (0xDC00..0xE000).contains(&lo))
                {
                    chars.next();
                    n += 4;
                } else {
                    n += 3;
                }
            }
        }
    }
    n
}

/// Rewrite characters >= U+10F800 (which a decoder may produce from valid input) as their
/// smuggled surrogate pairs, the only form lumen strings hold them in.
pub(crate) fn canonical(s: String) -> String {
    lumen_common::smuggle::utf16_text_owned(s)
}

pub fn utf8_decode(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return canonical(s.to_owned());
    }
    let mut out = String::with_capacity(bytes.len() + 8);
    let _ = utf::decode_utf8::<Infallible>(
        &mut Cow::Borrowed(bytes),
        true,
        Spelling::Utf16,
        &mut out,
        |_, m, out| {
            out.push('\u{FFFD}');
            Ok(m.end)
        },
    );
    out
}

// ---- latin1 / ascii / utf16le -------------------------------------------------------------------

/// Each UTF-16 unit's low byte (Node's `latin1`, and `ascii` for writing).
pub fn latin1_encode(s: &str) -> Vec<u8> {
    if s.is_ascii() {
        return s.as_bytes().to_vec();
    }
    let mut out = Vec::with_capacity(s.len());
    for_each_unit(s, |u| out.push(u as u8));
    out
}

pub fn latin1_decode(bytes: &[u8]) -> String {
    if bytes.is_ascii() {
        // SAFETY: ASCII is UTF-8.
        return unsafe { String::from_utf8_unchecked(bytes.to_vec()) };
    }
    bytes.iter().map(|&b| b as char).collect()
}

pub fn ascii_decode(bytes: &[u8]) -> String {
    let v: Vec<u8> = bytes.iter().map(|&b| b & 0x7f).collect();
    // SAFETY: every byte is < 0x80.
    unsafe { String::from_utf8_unchecked(v) }
}

pub fn utf16le_encode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 2);
    if s.is_ascii() {
        for &b in s.as_bytes() {
            out.push(b);
            out.push(0);
        }
        return out;
    }
    for_each_unit(s, |u| out.extend_from_slice(&u.to_le_bytes()));
    out
}

/// Node's utf16le: lone surrogates are kept and an odd last byte is dropped.
pub fn utf16le_decode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let _ = utf::decode_utf16::<Infallible>(
        &mut Cow::Borrowed(bytes),
        0,
        false,
        true,
        Spelling::Utf16,
        &mut out,
        |data, m, out| {
            if m.end - m.start < 2 {
                return Ok(m.end);
            }
            out.push(smuggle(u16::from_le_bytes([
                data[m.start],
                data[m.start + 1],
            ])));
            Ok(m.start + 2)
        },
    );
    out
}

// ---- hex / base64 (shared byte codecs; Node's lenient decoders) --------------------------------

pub use lumen_common::codec::hex_encode;

#[inline]
pub fn hex_decode(s: &str) -> Vec<u8> {
    lumen_common::codec::hex_decode_lenient(s.as_bytes())
}

#[inline]
pub fn base64_encode(bytes: &[u8], url: bool) -> String {
    lumen_common::codec::base64_encode(bytes, url, !url)
}

#[inline]
pub fn base64_decode(s: &str) -> Vec<u8> {
    lumen_common::codec::base64_decode_lenient(s.as_bytes())
}

/// `Buffer.byteLength(s, 'base64')`, Node's formula (no decoding).
pub fn base64_byte_length(s: &str) -> usize {
    let b = s.as_bytes();
    let mut n = unit_len(s);
    if n > 0 && b.last() == Some(&b'=') {
        n -= 1;
    }
    if n > 1 && b.len() > 1 && b[b.len() - 2] == b'=' {
        n -= 1;
    }
    n * 3 >> 2
}

// ---- dispatch -----------------------------------------------------------------------------------

pub fn encode(s: &str, enc: u32) -> Vec<u8> {
    match enc {
        LATIN1 | ASCII => latin1_encode(s),
        HEX => hex_decode(s),
        BASE64 | BASE64URL => base64_decode(s),
        UTF16LE => utf16le_encode(s),
        _ => utf8_encode(s),
    }
}

pub fn decode(bytes: &[u8], enc: u32) -> String {
    match enc {
        LATIN1 => latin1_decode(bytes),
        ASCII => ascii_decode(bytes),
        HEX => hex_encode(bytes),
        BASE64 => base64_encode(bytes, false),
        BASE64URL => base64_encode(bytes, true),
        UTF16LE => utf16le_decode(bytes),
        _ => utf8_decode(bytes),
    }
}

pub fn byte_length(s: &str, enc: u32) -> usize {
    match enc {
        LATIN1 | ASCII => unit_len(s),
        HEX => unit_len(s) >> 1,
        BASE64 | BASE64URL => base64_byte_length(s),
        UTF16LE => unit_len(s) * 2,
        _ => utf8_len(s),
    }
}

/// Encode `s` into `dst` the way `buf.write(string, offset, length, encoding)` does: never a
/// partial character (utf8) or a half unit (utf16le). Returns the bytes written.
pub fn write(dst: &mut [u8], s: &str, enc: u32) -> usize {
    match enc {
        UTF8 if !may_smuggle(s) => {
            let b = s.as_bytes();
            if b.len() <= dst.len() {
                dst[..b.len()].copy_from_slice(b);
                return b.len();
            }
            // Back off to a character boundary.
            let mut n = dst.len();
            while n > 0 && !s.is_char_boundary(n) {
                n -= 1;
            }
            dst[..n].copy_from_slice(&b[..n]);
            n
        }
        UTF8 => {
            // Encode character by character so a character that does not fit is not split.
            let bytes = utf8_encode(s);
            let mut n = bytes.len().min(dst.len());
            if n < bytes.len() {
                while n > 0 && (bytes[n] & 0xC0) == 0x80 {
                    n -= 1;
                }
            }
            dst[..n].copy_from_slice(&bytes[..n]);
            n
        }
        UTF16LE => {
            let bytes = utf16le_encode(s);
            let n = bytes.len().min(dst.len() & !1);
            dst[..n].copy_from_slice(&bytes[..n]);
            n
        }
        _ => {
            let bytes = encode(s, enc);
            let n = bytes.len().min(dst.len());
            dst[..n].copy_from_slice(&bytes[..n]);
            n
        }
    }
}

// ---- validation ---------------------------------------------------------------------------------

pub fn is_utf8(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_common::smuggle::SMUGGLE_BASE;

    #[test]
    fn base64_node_semantics() {
        assert_eq!(base64_decode("aGVsbG8="), b"hello");
        assert_eq!(base64_decode("aGVsbG8"), b"hello");
        assert_eq!(base64_decode("aGV sbG\n8="), b"hello");
        assert_eq!(base64_decode("aGVs=bG8="), b"hel");
        assert_eq!(base64_decode("_-8"), [0xff, 0xef]);
        assert_eq!(base64_decode("/+8="), [0xff, 0xef]);
        assert_eq!(base64_decode("a"), b"");
        assert_eq!(base64_encode(b"hello", false), "aGVsbG8=");
        assert_eq!(base64_encode(&[0xff, 0xef], true), "_-8");
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        assert_eq!(base64_decode(&base64_encode(&data, false)), data);
        assert_eq!(base64_decode(&base64_encode(&data, true)), data);
    }

    #[test]
    fn hex_node_semantics() {
        assert_eq!(hex_decode("48656c6C6f"), b"Hello");
        assert_eq!(hex_decode("486"), b"H");
        assert_eq!(hex_decode("48zz65"), b"H");
        assert_eq!(hex_encode(&[0, 15, 255]), "000fff");
    }

    #[test]
    fn smuggled_surrogates() {
        // A lone high surrogate (smuggled) encodes as U+FFFD.
        let lone: String = [char::from_u32(SMUGGLE_BASE).unwrap()].iter().collect();
        assert_eq!(utf8_encode(&lone), "\u{FFFD}".as_bytes());
        assert_eq!(utf8_len(&lone), 3);
        assert_eq!(latin1_encode(&lone), [0x00]);
        assert_eq!(utf16le_encode(&lone), [0x00, 0xD8]);
        // U+10FFFF arrives as a smuggled pair and round-trips.
        let pair = utf16le_decode(&[0xFF, 0xDB, 0xFF, 0xDF]);
        assert_eq!(pair.chars().count(), 2);
        assert_eq!(utf8_encode(&pair), "\u{10FFFF}".as_bytes());
        assert_eq!(utf8_decode("\u{10FFFF}".as_bytes()), pair);
        assert_eq!(utf8_len(&pair), 4);
        // Ordinary astral characters are single chars.
        assert_eq!(utf16le_decode(&[0x3D, 0xD8, 0x00, 0xDE]), "\u{1F600}");
        assert_eq!(latin1_encode("\u{1F600}"), [0x3D, 0x00]);
    }

    #[test]
    fn utf8_replacement() {
        assert_eq!(utf8_decode(&[0x61, 0xF0, 0x9F, 0x62]), "a\u{FFFD}b");
        assert_eq!(utf8_decode(&[0xC0, 0x80]), "\u{FFFD}\u{FFFD}");
    }

    #[test]
    fn write_does_not_split() {
        let mut d = [0u8; 4];
        assert_eq!(write(&mut d, "ab\u{e9}", UTF8), 4);
        let mut d = [0u8; 3];
        assert_eq!(write(&mut d, "ab\u{e9}", UTF8), 2);
        let mut d = [0u8; 3];
        assert_eq!(write(&mut d, "ab", UTF16LE), 2);
    }
}
