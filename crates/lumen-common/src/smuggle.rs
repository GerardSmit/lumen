//! Lone surrogates, and every other code point, carried inside Rust strings.
//!
//! A Rust `str` holds Unicode scalars only, but both languages' strings can hold lone surrogates.
//! They are *smuggled* as the plane-16 private-use scalars `U+10F800 + (unit - 0xD800)`, so any
//! string round-trips through a valid `str`. That frees the real characters U+10F800..U+10FFFF
//! of their own spelling, and each language gives them one that keeps its strings unique:
//!
//! - **UTF-16 strings** (JavaScript) are sequences of code units, so a real character there is
//!   the same string as its two surrogate units: it is stored as its smuggled surrogate *pair*,
//!   and adjacent smuggled high+low scalars always mean that character.
//! - **Code-point strings** (Python) are sequences of code points, where `chr(0x10FFFF)` and
//!   `'􏿿'` differ. Every real character of the reserved block U+10F000..U+10FFFF is
//!   stored as an *escape*: a head scalar `U+10F000 + (high - 0xDBFC)` followed by a tail scalar
//!   `U+10F400 + (low - 0xDC00)`, where high/low are the character's UTF-16 units. Heads and
//!   tails occur nowhere else, so the spelling is self-synchronizing: byte-level search, split
//!   and replace never match across an escape, and a smuggled lone surrogate never matches
//!   inside one.
//!
//! Everything that makes or reads the spelling lives here; strings never hold a reserved-block
//! scalar outside these forms.

use std::borrow::Cow;
use std::cmp::Ordering;

/// First smuggled scalar: encodes the lone surrogate U+D800.
pub const SMUGGLE_BASE: u32 = 0x10F800;

/// First code point a code-point string stores as an escape.
pub const RESERVED_BASE: u32 = 0x10F000;
const HEAD_BASE: u32 = 0x10F000;
const TAIL_BASE: u32 = 0x10F400;
/// The UTF-16 high unit of `RESERVED_BASE`.
const RESERVED_HIGH: u32 = 0xDBFC;

/// How a string type carries characters a Rust `str` cannot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spelling {
    /// Plain text: a lone surrogate becomes U+FFFD.
    Plain,
    /// JavaScript strings: UTF-16 strings, lone surrogates smuggled.
    Utf16,
    /// Python strings: code-point strings.
    CodePoints,
}

impl Spelling {
    /// Append code point `cp` (a scalar or a lone surrogate); whether it was a lone surrogate.
    #[inline]
    pub fn push(self, out: &mut String, cp: u32) -> bool {
        let lone = (0xD800..0xE000).contains(&cp);
        match self {
            Spelling::Plain => out.push(char::from_u32(cp).unwrap_or('\u{FFFD}')),
            Spelling::CodePoints => {
                push_code_point(out, cp);
            }
            Spelling::Utf16 => match char::from_u32(cp) {
                Some(c) => push_char_utf16(out, c),
                None => out.push(smuggle(cp as u16)),
            },
        }
        lone
    }

    /// Decoded text `s` (no lone surrogates) in this spelling.
    pub fn text(self, s: &str) -> Cow<'_, str> {
        match self {
            Spelling::Plain => Cow::Borrowed(s),
            Spelling::Utf16 => utf16_text(s),
            Spelling::CodePoints => escape_text(s),
        }
    }
}

/// If `c` is a smuggled lone surrogate, the surrogate code unit it encodes.
#[inline]
pub fn smuggled(c: char) -> Option<u16> {
    let v = c as u32;
    if (SMUGGLE_BASE..SMUGGLE_BASE + 0x800).contains(&v) {
        Some((v - SMUGGLE_BASE + 0xD800) as u16)
    } else {
        None
    }
}

/// Smuggle a surrogate code unit (0xD800..=0xDFFF) into its private-use scalar.
#[inline]
pub fn smuggle(unit: u16) -> char {
    debug_assert!((0xD800..0xE000).contains(&(unit as u32)));
    char::from_u32(SMUGGLE_BASE + (unit as u32 - 0xD800)).unwrap()
}

/// Every smuggled or escape scalar is UTF-8 encoded starting with `F4 8F`; a string without an
/// `F4` byte holds none.
#[inline]
pub fn may_contain(s: &str) -> bool {
    s.as_bytes().contains(&0xF4)
}

#[inline]
pub fn smuggled_high(c: char) -> Option<u16> {
    smuggled(c).filter(|u| (0xD800..0xDC00).contains(&(*u as u32)))
}

#[inline]
pub fn smuggled_low(c: char) -> Option<u16> {
    smuggled(c).filter(|u| (0xDC00..0xE000).contains(&(*u as u32)))
}

/// If `a` and `b` are a smuggled high+low pair, the real character they encode (UTF-16 strings).
pub fn paired_char(a: char, b: char) -> Option<char> {
    let hi = smuggled_high(a)?;
    let lo = smuggled_low(b)?;
    char::from_u32(0x10000 + ((hi as u32 - 0xD800) << 10) + (lo as u32 - 0xDC00))
}

/// Append character `c` to a UTF-16 string: a real character in U+10F800..U+10FFFF goes in as
/// its smuggled surrogate pair.
#[inline]
pub fn push_char_utf16(out: &mut String, c: char) {
    let v = c as u32;
    if v < SMUGGLE_BASE {
        out.push(c);
    } else {
        let v = v - 0x10000;
        out.push(smuggle(0xD800 + (v >> 10) as u16));
        out.push(smuggle(0xDC00 + (v & 0x3FF) as u16));
    }
}

/// Decoded text (no lone surrogates) as a UTF-16 string.
pub fn utf16_text(s: &str) -> Cow<'_, str> {
    if !may_contain(s) || !s.chars().any(|c| c as u32 >= SMUGGLE_BASE) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        push_char_utf16(&mut out, c);
    }
    Cow::Owned(out)
}

/// [`utf16_text`] on an owned string.
pub fn utf16_text_owned(s: String) -> String {
    match utf16_text(&s) {
        Cow::Borrowed(_) => s,
        Cow::Owned(o) => o,
    }
}

// ---- code-point strings ----

#[inline]
fn is_tail(v: u32) -> bool {
    (TAIL_BASE..TAIL_BASE + 0x400).contains(&v)
}

#[inline]
fn is_head(v: u32) -> bool {
    (HEAD_BASE..HEAD_BASE + 4).contains(&v)
}

/// Append code point `cp` (lone surrogates included) to a code-point string; `false` above
/// U+10FFFF.
#[inline]
pub fn push_code_point(out: &mut String, cp: u32) -> bool {
    if cp < 0xD800 || (0xE000..RESERVED_BASE).contains(&cp) {
        out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
    } else if cp < 0xE000 {
        out.push(smuggle(cp as u16));
    } else if cp <= 0x10FFFF {
        push_escape(out, cp);
    } else {
        return false;
    }
    true
}

#[cold]
fn push_escape(out: &mut String, cp: u32) {
    let v = cp - 0x10000;
    let head = HEAD_BASE + (0xD800 + (v >> 10) - RESERVED_HIGH);
    let tail = TAIL_BASE + (v & 0x3FF);
    out.push(char::from_u32(head).unwrap_or('\u{fffd}'));
    out.push(char::from_u32(tail).unwrap_or('\u{fffd}'));
}

/// The one-code-point string for `cp`; `None` above U+10FFFF.
pub fn code_point_str(cp: u32) -> Option<String> {
    let mut s = String::with_capacity(8);
    push_code_point(&mut s, cp).then_some(s)
}

/// A code-point string holding the characters of decoded text `s` (no lone surrogates): the
/// reserved-block characters are escaped.
pub fn escape_text(s: &str) -> Cow<'_, str> {
    if !may_contain(s) || !s.chars().any(|c| c as u32 >= RESERVED_BASE) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        push_code_point(&mut out, c as u32);
    }
    Cow::Owned(out)
}

/// [`escape_text`] on an owned string.
pub fn escape_text_owned(s: String) -> String {
    match escape_text(&s) {
        Cow::Borrowed(_) => s,
        Cow::Owned(o) => o,
    }
}

/// The inverse of [`escape_text`]: escapes become their characters (lone surrogates stay
/// smuggled, having no UTF-8 form).
pub fn unescape_text(s: &str) -> Cow<'_, str> {
    if !may_contain(s) || !s.chars().any(|c| is_head(c as u32)) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut it = code_points(s);
    let mut at = 0;
    while let Some(cp) = it.next() {
        match char::from_u32(cp) {
            Some(c) if cp >= RESERVED_BASE => out.push(c),
            _ => out.push_str(&s[at..it.offset()]),
        }
        at = it.offset();
    }
    Cow::Owned(out)
}

/// The code point starting at byte `i` of a code-point string and its length in bytes.
#[inline]
pub fn decode_at(s: &str, i: usize) -> (u32, usize) {
    let c = s[i..].chars().next().map_or(0, |c| c as u32);
    if c < 0x80 {
        return (c, 1);
    }
    if c < SMUGGLE_BASE - 0x800 {
        return (c, char_len(c));
    }
    decode_slow(s, i, c)
}

#[inline]
fn char_len(c: u32) -> usize {
    if c < 0x800 {
        2
    } else if c < 0x10000 {
        3
    } else {
        4
    }
}

#[cold]
fn decode_slow(s: &str, i: usize, c: u32) -> (u32, usize) {
    if c >= SMUGGLE_BASE {
        return (c - SMUGGLE_BASE + 0xD800, 4);
    }
    if is_head(c) {
        if let Some(t) = s[i + 4..].chars().next().map(|t| t as u32).filter(|&t| is_tail(t)) {
            let high = c - HEAD_BASE + RESERVED_HIGH;
            let low = t - TAIL_BASE + 0xDC00;
            return (0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00), 8);
        }
    }
    (c, 4)
}

/// The code points of a code-point string.
pub fn code_points(s: &str) -> CodePoints<'_> {
    CodePoints { s, i: 0 }
}

/// Iterator over a code-point string's code points; [`CodePoints::offset`] is the byte position
/// of the next one.
#[derive(Clone)]
pub struct CodePoints<'a> {
    s: &'a str,
    i: usize,
}

impl CodePoints<'_> {
    pub fn offset(&self) -> usize {
        self.i
    }
}

impl Iterator for CodePoints<'_> {
    type Item = u32;

    #[inline]
    fn next(&mut self) -> Option<u32> {
        if self.i >= self.s.len() {
            return None;
        }
        let (cp, n) = decode_at(self.s, self.i);
        self.i += n;
        Some(cp)
    }
}

/// Code points in a code-point string.
pub fn count_code_points(s: &str) -> usize {
    let n = s.chars().count();
    if !may_contain(s) {
        return n;
    }
    n - s.chars().filter(|&c| is_tail(c as u32)).count()
}

/// Byte offset of code point `idx` (the length when `idx` is past the end).
pub fn code_point_offset(s: &str, idx: usize) -> usize {
    if !may_contain(s) {
        return s.char_indices().nth(idx).map_or(s.len(), |(i, _)| i);
    }
    let mut it = code_points(s);
    for _ in 0..idx {
        if it.next().is_none() {
            return s.len();
        }
    }
    it.offset()
}

/// Code-point order (CPython's string comparison). Byte order except where the first difference
/// falls in an `F4`-led character, which may be smuggled or escaped.
pub fn cmp_code_points(a: &str, b: &str) -> Ordering {
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let Some(i) = ab.iter().zip(bb).position(|(x, y)| x != y) else {
        return ab.len().cmp(&bb.len());
    };
    // With a common prefix, the difference starts a character in both strings or falls inside
    // one character that both share the lead byte of.
    let mut lead = i;
    while lead > 0 && ab[lead] & 0xC0 == 0x80 {
        lead -= 1;
    }
    if ab[lead] != 0xF4 && bb[lead] != 0xF4 {
        return ab[i].cmp(&bb[i]);
    }
    code_points(a).cmp(code_points(b))
}

/// The code point a single-scalar string element stands for (no escapes).
#[inline]
pub fn code_point(c: char) -> u32 {
    match smuggled(c) {
        Some(u) => u as u32,
        None => c as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cps(s: &str) -> Vec<u32> {
        code_points(s).collect()
    }

    #[test]
    fn every_code_point_is_distinct() {
        let mut seen = std::collections::HashSet::new();
        for cp in (0xD7F0..0xE010).chain(0x10EFF0..=0x10FFFF) {
            let s = code_point_str(cp).unwrap();
            assert_eq!(cps(&s), [cp]);
            assert_eq!(count_code_points(&s), 1);
            assert!(seen.insert(s));
        }
        let pair = format!("{}{}", code_point_str(0xDBFF).unwrap(), code_point_str(0xDFFF).unwrap());
        assert_ne!(pair, code_point_str(0x10FFFF).unwrap());
        assert_eq!(cps(&pair), [0xDBFF, 0xDFFF]);
    }

    #[test]
    fn order_and_offsets() {
        let s: String = [0x41, 0x10FFFF, 0xDC00, 0x10F000, 0x10EFFF]
            .iter()
            .map(|&c| code_point_str(c).unwrap())
            .collect();
        assert_eq!(count_code_points(&s), 5);
        assert_eq!(cps(&s[code_point_offset(&s, 2)..]), [0xDC00, 0x10F000, 0x10EFFF]);
        assert_eq!(code_point_offset(&s, 9), s.len());
        let order = [0x41, 0xD800, 0xE000, 0xFFFF, 0x10000, 0x10EFFF, 0x10F000, 0x10F7FF, 0x10F800, 0x10FFFF];
        for w in order.windows(2) {
            let (a, b) = (code_point_str(w[0]).unwrap(), code_point_str(w[1]).unwrap());
            assert_eq!(cmp_code_points(&a, &b), Ordering::Less, "{:x} < {:x}", w[0], w[1]);
            assert_eq!(cmp_code_points(&b, &a), Ordering::Greater);
        }
        assert_eq!(cmp_code_points("ab", "abc"), Ordering::Less);
        assert_eq!(escape_text("x\u{10FFFF}"), code_point_str(0x78).unwrap() + &code_point_str(0x10FFFF).unwrap());
    }
}
