//! Buffer's string codecs (utf8, utf16le, latin1, ascii, hex, base64, base64url) and byte
//! search, with Node's exact semantics — including its lenient decoders: base64 skips characters
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

const SMUGGLE_BASE: u32 = 0x10F800;

/// The UTF-16 unit a smuggled scalar stands for.
#[inline]
fn smuggled(c: char) -> Option<u16> {
    let v = c as u32;
    if (SMUGGLE_BASE..SMUGGLE_BASE + 0x800).contains(&v) {
        Some((v - SMUGGLE_BASE + 0xD800) as u16)
    } else {
        None
    }
}

/// Every smuggled scalar starts `F4 8F`; a string without an `F4` byte has none.
#[inline]
fn may_smuggle(s: &str) -> bool {
    s.as_bytes().contains(&0xF4)
}

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
    if !may_smuggle(&s) {
        return s;
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        let v = c as u32;
        if v >= SMUGGLE_BASE {
            let w = v - 0x10000;
            let hi = 0xD800 + (w >> 10);
            let lo = 0xDC00 + (w & 0x3FF);
            out.push(char::from_u32(SMUGGLE_BASE + hi - 0xD800).unwrap());
            out.push(char::from_u32(SMUGGLE_BASE + lo - 0xD800).unwrap());
        } else {
            out.push(c);
        }
    }
    out
}

pub fn utf8_decode(bytes: &[u8]) -> String {
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    };
    canonical(s)
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

/// Units -> string: valid pairs combine, lone surrogates are smuggled.
pub fn from_units(units: impl Iterator<Item = u16>) -> String {
    let mut out = String::new();
    let mut pending: Option<u16> = None;
    let push_lone = |out: &mut String, u: u16| {
        out.push(char::from_u32(SMUGGLE_BASE + (u as u32 - 0xD800)).unwrap());
    };
    for u in units {
        if let Some(hi) = pending.take() {
            if (0xDC00..0xE000).contains(&u) {
                let cp = 0x10000 + (((hi as u32) - 0xD800) << 10) + (u as u32 - 0xDC00);
                if cp >= SMUGGLE_BASE {
                    push_lone(&mut out, hi);
                    push_lone(&mut out, u);
                } else {
                    out.push(char::from_u32(cp).unwrap());
                }
                continue;
            }
            push_lone(&mut out, hi);
        }
        match u {
            0xD800..=0xDBFF => pending = Some(u),
            0xDC00..=0xDFFF => push_lone(&mut out, u),
            _ => out.push(char::from_u32(u as u32).unwrap()),
        }
    }
    if let Some(hi) = pending {
        push_lone(&mut out, hi);
    }
    out
}

pub fn utf16le_decode(bytes: &[u8]) -> String {
    from_units(
        bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]])),
    )
}

// ---- hex ----------------------------------------------------------------------------------------

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = vec![0u8; bytes.len() * 2];
    for (o, &b) in out.chunks_exact_mut(2).zip(bytes) {
        o[0] = HEX_DIGITS[(b >> 4) as usize];
        o[1] = HEX_DIGITS[(b & 15) as usize];
    }
    // SAFETY: hex digits are ASCII.
    unsafe { String::from_utf8_unchecked(out) }
}

#[inline]
fn unhex(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0xff,
    }
}

/// Node's hex decode: pairs until the first invalid one (an odd trailing digit is dropped).
pub fn hex_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    for pair in b.chunks_exact(2) {
        let (hi, lo) = (unhex(pair[0]), unhex(pair[1]));
        if hi == 0xff || lo == 0xff {
            break;
        }
        out.push(hi << 4 | lo);
    }
    out
}

// ---- base64 -------------------------------------------------------------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn base64_encode(bytes: &[u8], url: bool) -> String {
    let alpha = if url { B64URL } else { B64 };
    let mut out = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut chunks = bytes.chunks_exact(3);
    for c in &mut chunks {
        let n = (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32;
        out.extend_from_slice(&[
            alpha[(n >> 18) as usize & 63],
            alpha[(n >> 12) as usize & 63],
            alpha[(n >> 6) as usize & 63],
            alpha[n as usize & 63],
        ]);
    }
    let rest = chunks.remainder();
    match rest.len() {
        1 => {
            let n = (rest[0] as u32) << 16;
            out.push(alpha[(n >> 18) as usize & 63]);
            out.push(alpha[(n >> 12) as usize & 63]);
            if !url {
                out.extend_from_slice(b"==");
            }
        }
        2 => {
            let n = (rest[0] as u32) << 16 | (rest[1] as u32) << 8;
            out.push(alpha[(n >> 18) as usize & 63]);
            out.push(alpha[(n >> 12) as usize & 63]);
            out.push(alpha[(n >> 6) as usize & 63]);
            if !url {
                out.push(b'=');
            }
        }
        _ => {}
    }
    // SAFETY: the alphabet is ASCII.
    unsafe { String::from_utf8_unchecked(out) }
}

/// Both alphabets decode in both encodings, as in Node. 0xff = not in the alphabet.
const UNB64: [u8; 256] = {
    let mut t = [0xffu8; 256];
    let mut i = 0;
    while i < 64 {
        t[B64[i] as usize] = i as u8;
        t[B64URL[i] as usize] = i as u8;
        i += 1;
    }
    t
};

/// Node's `base64_decoded_size`: the buffer it allocates (decoding never writes past it).
fn base64_decoded_size(src: &[u8]) -> usize {
    let mut size = src.len();
    if size < 2 {
        return 0;
    }
    if src[size - 1] == b'=' {
        size -= 1;
        if src[size - 1] == b'=' {
            size -= 1;
        }
    }
    let rem = size % 4;
    let mut n = size / 4 * 3;
    if rem != 0 {
        if n == 0 && rem == 1 {
            n = 0;
        } else {
            n += 1 + (rem == 3) as usize;
        }
    }
    n
}

/// Node's lenient base64 decode (either alphabet): characters outside the alphabet are skipped,
/// `=` ends the input, and a trailing partial group yields the bytes it completes.
pub fn base64_decode(s: &str) -> Vec<u8> {
    let src = s.as_bytes();
    let cap = base64_decoded_size(src);
    let mut out = Vec::with_capacity(cap);
    // Fast path: whole groups of clean alphabet characters.
    let mut i = 0;
    while i + 4 <= src.len() && out.len() + 3 <= cap {
        let a = UNB64[src[i] as usize];
        let b = UNB64[src[i + 1] as usize];
        let c = UNB64[src[i + 2] as usize];
        let d = UNB64[src[i + 3] as usize];
        if (a | b | c | d) & 0x80 != 0 {
            break;
        }
        let n = (a as u32) << 18 | (b as u32) << 12 | (c as u32) << 6 | d as u32;
        out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
        i += 4;
    }
    // Slow path (Node's base64_decode_group_slow), from wherever the fast path stopped.
    let mut quad = [0u8; 4];
    let mut q = 0;
    while i < src.len() {
        let c = src[i];
        i += 1;
        let v = UNB64[c as usize];
        if v == 0xff {
            if c == b'=' {
                break;
            }
            continue;
        }
        quad[q] = v;
        q += 1;
        if q == 4 {
            if out.len() >= cap {
                break;
            }
            out.push(quad[0] << 2 | quad[1] >> 4);
            if out.len() >= cap {
                break;
            }
            out.push(quad[1] << 4 | quad[2] >> 2);
            if out.len() >= cap {
                break;
            }
            out.push(quad[2] << 6 | quad[3]);
            q = 0;
        }
    }
    // A partial final group: 2 characters make 1 byte, 3 make 2.
    if q >= 2 && out.len() < cap {
        out.push(quad[0] << 2 | quad[1] >> 4);
        if q == 3 && out.len() < cap {
            out.push(quad[1] << 4 | quad[2] >> 2);
        }
    }
    out
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

// ---- search -------------------------------------------------------------------------------------

/// Index of the first `b` in `hay` (word-at-a-time: 8 bytes per step on the long runs between
/// candidates, which is what a byte search spends its time on).
pub fn memchr(b: u8, hay: &[u8]) -> Option<usize> {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    let pat = LO.wrapping_mul(b as u64);
    let mut i = 0;
    while i + 8 <= hay.len() {
        let w = u64::from_le_bytes(hay[i..i + 8].try_into().unwrap()) ^ pat;
        if (w.wrapping_sub(LO) & !w & HI) != 0 {
            break; // a match in this word: finish bytewise
        }
        i += 8;
    }
    hay[i..].iter().position(|&x| x == b).map(|p| p + i)
}

/// First index >= `from` where `needle` occurs in `hay`.
pub fn index_of(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from > hay.len() {
        return None;
    }
    if needle.is_empty() {
        return Some(from);
    }
    let first = needle[0];
    let last_start = hay.len().checked_sub(needle.len())?;
    let mut i = from;
    while i <= last_start {
        match memchr(first, &hay[i..=last_start]) {
            None => return None,
            Some(p) => {
                i += p;
                if &hay[i..i + needle.len()] == needle {
                    return Some(i);
                }
                i += 1;
            }
        }
    }
    None
}

/// Last index <= `from` where `needle` occurs in `hay`.
pub fn last_index_of(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    let start = from.min(hay.len() - needle.len());
    if needle.is_empty() {
        return Some(start);
    }
    (0..=start)
        .rev()
        .find(|&i| hay[i] == needle[0] && &hay[i..i + needle.len()] == needle)
}

// ---- validation ---------------------------------------------------------------------------------

pub fn is_utf8(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn search() {
        assert_eq!(index_of(b"abcabc", b"ca", 0), Some(2));
        assert_eq!(index_of(b"abcabc", b"abc", 1), Some(3));
        assert_eq!(index_of(b"abc", b"", 2), Some(2));
        assert_eq!(index_of(b"abc", b"d", 0), None);
        assert_eq!(last_index_of(b"abcabc", b"abc", 6), Some(3));
        assert_eq!(last_index_of(b"abcabc", b"abc", 2), Some(0));
        let hay: Vec<u8> = (0..1000u32).map(|i| (i * 31 + 7) as u8).collect();
        for b in 0..=255u8 {
            assert_eq!(memchr(b, &hay), hay.iter().position(|&x| x == b), "byte {b}");
            assert_eq!(memchr(b, &hay[3..17]), hay[3..17].iter().position(|&x| x == b));
        }
        assert_eq!(index_of(&hay, &hay[500..505], 0), Some(500 % 256));
    }
}
