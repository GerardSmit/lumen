//! Byte-level text codecs shared by the JS and Python runtimes: hex and base64, each with a
//! strict decoder that reports where it failed and the lenient Node-style one.

// `as_chunks` needs a newer toolchain than the workspace MSRV.
#![allow(clippy::chunks_exact_to_as_chunks)]

use std::fmt;

const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";
const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

const B64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Why a strict decode stopped; positions are byte offsets into the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// A byte outside the alphabet.
    InvalidByte(usize),
    /// An odd number of hex digits, or a base64 group of a single character.
    InvalidLength,
    /// Misplaced, missing or excess `=` padding, at the offset where it was detected.
    InvalidPadding(usize),
    /// Non-zero bits below the last decoded byte (non-canonical base64), at the last character.
    TrailingBits(usize),
}

impl DecodeError {
    pub fn position(self) -> Option<usize> {
        match self {
            DecodeError::InvalidByte(p) | DecodeError::InvalidPadding(p) | DecodeError::TrailingBits(p) => Some(p),
            DecodeError::InvalidLength => None,
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::InvalidByte(p) => write!(f, "invalid byte at offset {p}"),
            DecodeError::InvalidLength => f.write_str("invalid length"),
            DecodeError::InvalidPadding(p) => write!(f, "invalid padding at offset {p}"),
            DecodeError::TrailingBits(p) => write!(f, "non-zero trailing bits at offset {p}"),
        }
    }
}

impl std::error::Error for DecodeError {}

// ---- hex ----------------------------------------------------------------------------------------

#[inline]
pub fn hex_encode(bytes: &[u8]) -> String {
    hex_encode_with(bytes, HEX_LOWER)
}

#[inline]
pub fn hex_encode_upper(bytes: &[u8]) -> String {
    hex_encode_with(bytes, HEX_UPPER)
}

fn hex_encode_with(bytes: &[u8], digits: &[u8; 16]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(digits[(b >> 4) as usize] as char);
        out.push(digits[(b & 15) as usize] as char);
    }
    out
}

/// The value of one hex digit, or `None`.
#[inline]
pub fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Node's hex decode: pairs until the first invalid one (an odd trailing digit is dropped).
pub fn hex_decode_lenient(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in s.chunks_exact(2) {
        match (hex_digit(pair[0]), hex_digit(pair[1])) {
            (Some(hi), Some(lo)) => out.push(hi << 4 | lo),
            _ => break,
        }
    }
    out
}

pub fn hex_decode_strict(s: &[u8]) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::with_capacity(s.len() / 2);
    for (i, pair) in s.chunks(2).enumerate() {
        let hi = hex_digit(pair[0]).ok_or(DecodeError::InvalidByte(i * 2))?;
        match pair.get(1) {
            None => return Err(DecodeError::InvalidLength),
            Some(&c) => {
                let lo = hex_digit(c).ok_or(DecodeError::InvalidByte(i * 2 + 1))?;
                out.push(hi << 4 | lo);
            }
        }
    }
    Ok(out)
}

// ---- base64 -------------------------------------------------------------------------------------

#[inline]
pub fn base64_encode(bytes: &[u8], url: bool, pad: bool) -> String {
    let alpha = if url { B64_URL } else { B64_STD };
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut put = |i: u32| out.push(alpha[i as usize & 63] as char);
    let mut chunks = bytes.chunks_exact(3);
    for c in &mut chunks {
        let n = (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32;
        put(n >> 18);
        put(n >> 12);
        put(n >> 6);
        put(n);
    }
    let rest = chunks.remainder();
    match rest.len() {
        1 => {
            let n = (rest[0] as u32) << 16;
            put(n >> 18);
            put(n >> 12);
            if pad {
                out.push_str("==");
            }
        }
        2 => {
            let n = (rest[0] as u32) << 16 | (rest[1] as u32) << 8;
            put(n >> 18);
            put(n >> 12);
            put(n >> 6);
            if pad {
                out.push('=');
            }
        }
        _ => {}
    }
    out
}

const fn unb64_table(alpha: &[u8; 64], other: Option<&[u8; 64]>) -> [u8; 256] {
    let mut t = [0xffu8; 256];
    let mut i = 0;
    while i < 64 {
        t[alpha[i] as usize] = i as u8;
        if let Some(o) = other {
            t[o[i] as usize] = i as u8;
        }
        i += 1;
    }
    t
}

/// Both alphabets decode to their value; 0xff = in neither.
const UNB64_ANY: [u8; 256] = unb64_table(B64_STD, Some(B64_URL));
const UNB64_STD: [u8; 256] = unb64_table(B64_STD, None);
const UNB64_URL: [u8; 256] = unb64_table(B64_URL, None);

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
pub fn base64_decode_lenient(src: &[u8]) -> Vec<u8> {
    let cap = base64_decoded_size(src);
    let mut out = Vec::with_capacity(cap);
    let mut i = 0;
    while i + 4 <= src.len() && out.len() + 3 <= cap {
        let a = UNB64_ANY[src[i] as usize];
        let b = UNB64_ANY[src[i + 1] as usize];
        let c = UNB64_ANY[src[i + 2] as usize];
        let d = UNB64_ANY[src[i + 3] as usize];
        if (a | b | c | d) & 0x80 != 0 {
            break;
        }
        let n = (a as u32) << 18 | (b as u32) << 12 | (c as u32) << 6 | d as u32;
        out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
        i += 4;
    }
    // Node's base64_decode_group_slow, from wherever the fast path stopped.
    let mut quad = [0u8; 4];
    let mut q = 0;
    while i < src.len() {
        let c = src[i];
        i += 1;
        let v = UNB64_ANY[c as usize];
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
    if q >= 2 && out.len() < cap {
        out.push(quad[0] << 2 | quad[1] >> 4);
        if q == 3 && out.len() < cap {
            out.push(quad[1] << 4 | quad[2] >> 2);
        }
    }
    out
}

/// How a strict base64 decode treats trailing `=`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Padding {
    /// The input length must be a multiple of four, completed with `=`.
    Required,
    /// `=` is accepted only where it completes a group; a short final group is fine without it.
    Optional,
    /// `=` is an invalid byte.
    Forbidden,
}

/// Strict base64: only the chosen alphabet, canonical padding as requested, no whitespace, and
/// zero bits below the final byte. Errors carry the byte offset.
pub fn base64_decode_strict(src: &[u8], url: bool, padding: Padding) -> Result<Vec<u8>, DecodeError> {
    let table = if url { &UNB64_URL } else { &UNB64_STD };
    let pads = if padding == Padding::Forbidden { 0 } else { src.iter().rev().take_while(|&&c| c == b'=').count() };
    let data_len = src.len() - pads;
    if pads > 2 {
        return Err(DecodeError::InvalidPadding(data_len + 2));
    }
    let mut out = Vec::with_capacity(data_len / 4 * 3 + 2);
    let mut chunks = src[..data_len].chunks_exact(4);
    let mut at = 0;
    for c in &mut chunks {
        let mut n = 0u32;
        for (k, &b) in c.iter().enumerate() {
            let v = table[b as usize];
            if v == 0xff {
                return Err(DecodeError::InvalidByte(at + k));
            }
            n = n << 6 | v as u32;
        }
        out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
        at += 4;
    }
    let rest = chunks.remainder();
    let mut n = 0u32;
    for (k, &b) in rest.iter().enumerate() {
        let v = table[b as usize];
        if v == 0xff {
            return Err(DecodeError::InvalidByte(at + k));
        }
        n = n << 6 | v as u32;
    }
    let want_pads = 4 - rest.len();
    match rest.len() {
        0 => {
            if pads != 0 {
                return Err(DecodeError::InvalidPadding(data_len));
            }
        }
        1 => return Err(DecodeError::InvalidLength),
        len => {
            let bad_pads = if padding == Padding::Required { pads != want_pads } else { pads != 0 && pads != want_pads };
            if bad_pads {
                return Err(DecodeError::InvalidPadding(src.len()));
            }
            if len == 2 {
                if n & 0xf != 0 {
                    return Err(DecodeError::TrailingBits(at + 1));
                }
                out.push((n >> 4) as u8);
            } else {
                if n & 0x3 != 0 {
                    return Err(DecodeError::TrailingBits(at + 2));
                }
                out.extend_from_slice(&[(n >> 10) as u8, (n >> 2) as u8]);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        assert_eq!(hex_encode(&[0, 15, 255]), "000fff");
        assert_eq!(hex_encode_upper(&[0xab]), "AB");
        assert_eq!(hex_decode_lenient(b"00ffzz11"), vec![0, 255]);
        assert_eq!(hex_decode_lenient(b"abc"), vec![0xab]);
        assert_eq!(hex_decode_strict(b"00FF").unwrap(), vec![0, 255]);
        assert_eq!(hex_decode_strict(b"0g"), Err(DecodeError::InvalidByte(1)));
        assert_eq!(hex_decode_strict(b"abc"), Err(DecodeError::InvalidLength));
    }

    #[test]
    fn base64_encode_cases() {
        assert_eq!(base64_encode(b"", false, true), "");
        assert_eq!(base64_encode(b"f", false, true), "Zg==");
        assert_eq!(base64_encode(b"fo", false, true), "Zm8=");
        assert_eq!(base64_encode(b"foo", false, true), "Zm9v");
        assert_eq!(base64_encode(b"fo", false, false), "Zm8");
        assert_eq!(base64_encode(&[0xfb, 0xff], true, false), "-_8");
        assert_eq!(base64_encode(&[0xfb, 0xff], false, true), "+/8=");
    }

    #[test]
    fn base64_lenient() {
        assert_eq!(base64_decode_lenient(b"Zm9v\nYmFy"), b"foobar");
        assert_eq!(base64_decode_lenient(b"Zm8"), b"fo");
        assert_eq!(base64_decode_lenient(b"-_8="), vec![0xfb, 0xff]);
        assert_eq!(base64_decode_lenient(b"Zg==Zg"), b"f");
    }

    #[test]
    fn base64_strict() {
        let req = Padding::Required;
        assert_eq!(base64_decode_strict(b"Zm9vYg==", false, req).unwrap(), b"foob");
        assert_eq!(base64_decode_strict(b"Zm8=", false, req).unwrap(), b"fo");
        assert_eq!(base64_decode_strict(b"Zm8", false, req), Err(DecodeError::InvalidPadding(3)));
        assert_eq!(base64_decode_strict(b"Zm8", false, Padding::Optional).unwrap(), b"fo");
        assert_eq!(base64_decode_strict(b"Zm9v\n", false, req), Err(DecodeError::InvalidByte(4)));
        assert_eq!(base64_decode_strict(b"Zm9!", false, req), Err(DecodeError::InvalidByte(3)));
        assert_eq!(base64_decode_strict(b"Zm9=Zm9v", false, req), Err(DecodeError::InvalidByte(3)));
        assert_eq!(base64_decode_strict(b"Zh==", false, req), Err(DecodeError::TrailingBits(1)));
        assert_eq!(base64_decode_strict(b"Z", false, Padding::Optional), Err(DecodeError::InvalidLength));
        assert_eq!(base64_decode_strict(b"-_8", true, Padding::Forbidden).unwrap(), vec![0xfb, 0xff]);
        assert_eq!(base64_decode_strict(b"+/8", true, Padding::Forbidden), Err(DecodeError::InvalidByte(0)));
        assert_eq!(base64_decode_strict(b"Zg===", false, req), Err(DecodeError::InvalidPadding(4)));
    }
}
