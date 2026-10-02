//! UTF-8 and UTF-16 decoding into either language's strings, with the malformed sequences handed
//! to an error handler. Malformed ranges and reasons are CPython's (UTF-8 errors cover maximal
//! subparts, as WHATWG and V8 replace them), so Python's codec error handlers and Node's U+FFFD
//! replacement both sit on top.

use crate::smuggle::Spelling;
use std::borrow::Cow;

/// A malformed sequence: the bytes `start..end` of the input, and CPython's reason for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformed {
    pub start: usize,
    pub end: usize,
    pub reason: &'static str,
}

/// Decode UTF-8 `data` onto `out`, returning how many bytes were consumed (all of them when
/// `final_`; otherwise an incomplete sequence at the end is left over).
///
/// `on_error` handles each malformed sequence: it may append to `out` and replace `data`, and
/// returns the position to go on from.
pub fn decode_utf8<E>(
    data: &mut Cow<'_, [u8]>,
    final_: bool,
    spelling: Spelling,
    out: &mut String,
    mut on_error: impl FnMut(&mut Cow<'_, [u8]>, Malformed, &mut String) -> Result<usize, E>,
) -> Result<usize, E> {
    let mut pos = 0;
    while pos < data.len() {
        let (good, err_len) = match std::str::from_utf8(&data[pos..]) {
            Ok(s) => {
                out.push_str(&spelling.text(s));
                return Ok(data.len());
            }
            Err(e) => (e.valid_up_to(), e.error_len()),
        };
        let start = pos + good;
        // SAFETY: from_utf8 validated these bytes.
        out.push_str(&spelling.text(unsafe { std::str::from_utf8_unchecked(&data[pos..start]) }));
        let bad = match err_len {
            Some(n) => {
                let b = data[start];
                let reason = if (0x80..0xC2).contains(&b) || b >= 0xF5 { "invalid start byte" } else { "invalid continuation byte" };
                Malformed { start, end: start + n, reason }
            }
            None if !final_ => return Ok(start),
            None => Malformed { start, end: data.len(), reason: "unexpected end of data" },
        };
        pos = on_error(data, bad, out)?;
    }
    Ok(pos)
}

/// Decode UTF-16 `data` from byte `pos` onto `out`, as [`decode_utf8`].
pub fn decode_utf16<E>(
    data: &mut Cow<'_, [u8]>,
    mut pos: usize,
    big_endian: bool,
    final_: bool,
    spelling: Spelling,
    out: &mut String,
    mut on_error: impl FnMut(&mut Cow<'_, [u8]>, Malformed, &mut String) -> Result<usize, E>,
) -> Result<usize, E> {
    let unit = |d: &[u8], i: usize| -> u32 {
        let b = [d[i], d[i + 1]];
        (if big_endian { u16::from_be_bytes(b) } else { u16::from_le_bytes(b) }) as u32
    };
    while pos < data.len() {
        let (start, end, reason) = if pos + 1 >= data.len() {
            if !final_ {
                break;
            }
            (pos, data.len(), "truncated data")
        } else {
            let u = unit(data, pos);
            if !(0xD800..0xE000).contains(&u) {
                spelling.push(out, u);
                pos += 2;
                continue;
            }
            if u >= 0xDC00 {
                (pos, pos + 2, "illegal encoding")
            } else if pos + 3 >= data.len() {
                if !final_ {
                    break;
                }
                (pos, data.len(), "unexpected end of data")
            } else {
                let u2 = unit(data, pos + 2);
                if (0xDC00..0xE000).contains(&u2) {
                    spelling.push(out, 0x10000 + ((u - 0xD800) << 10) + (u2 - 0xDC00));
                    pos += 4;
                    continue;
                }
                (pos, pos + 2, "illegal UTF-16 surrogate")
            }
        };
        pos = on_error(data, Malformed { start, end, reason }, out)?;
    }
    Ok(pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

    fn utf8(b: &[u8], final_: bool) -> (String, usize, Vec<Malformed>) {
        let mut out = String::new();
        let mut bad = Vec::new();
        let n = decode_utf8::<Infallible>(&mut Cow::Borrowed(b), final_, Spelling::Plain, &mut out, |_, m, out| {
            bad.push(m);
            out.push('\u{FFFD}');
            Ok(m.end)
        })
        .unwrap();
        (out, n, bad)
    }

    #[test]
    fn utf8_maximal_subparts() {
        assert_eq!(utf8(b"a\xF0\x9F\x62", true).0, "a\u{FFFD}b");
        assert_eq!(utf8(b"\xC0\x80", true).0, "\u{FFFD}\u{FFFD}");
        let (_, _, bad) = utf8(b"\x80a\xE2\x28", true);
        assert_eq!(bad[0], Malformed { start: 0, end: 1, reason: "invalid start byte" });
        assert_eq!(bad[1], Malformed { start: 2, end: 3, reason: "invalid continuation byte" });
        assert_eq!(utf8(b"ab\xE2\x82", false), ("ab".to_string(), 2, vec![]));
        assert_eq!(utf8(b"ab\xE2\x82", true).2[0], Malformed { start: 2, end: 4, reason: "unexpected end of data" });
    }

    #[test]
    fn utf16_pairs_and_errors() {
        let mut out = String::new();
        let mut bad = Vec::new();
        let data = [0x61, 0, 0x3D, 0xD8, 0x00, 0xDE, 0x00, 0xDC, 0x00, 0xD8, 0x62, 0, 0x01];
        let n = decode_utf16::<Infallible>(&mut Cow::Borrowed(&data[..]), 0, false, true, Spelling::Plain, &mut out, |_, m, _| {
            bad.push(m.reason);
            Ok(m.end.min(m.start + 2))
        })
        .unwrap();
        assert_eq!(out, "a\u{1F600}b");
        assert_eq!(n, 13);
        assert_eq!(bad, ["illegal encoding", "illegal UTF-16 surrogate", "truncated data"]);
    }
}
