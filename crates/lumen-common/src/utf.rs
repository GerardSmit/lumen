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

/// One decoding step at a position of the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// A code point (never a surrogate) and the bytes it occupied.
    Char(u32, usize),
    Bad(Malformed),
    /// The input ends inside a character; more bytes may complete it.
    Incomplete,
}

/// The malformed range for a UTF-8 error at `start` (`err_len` as `Utf8Error::error_len`);
/// `None` when the input just ends early and more data may follow (`!final_`).
fn utf8_malformed(
    data: &[u8],
    start: usize,
    err_len: Option<usize>,
    final_: bool,
) -> Option<Malformed> {
    match err_len {
        Some(n) => {
            let b = data[start];
            let reason = if (0x80..0xC2).contains(&b) || b >= 0xF5 {
                "invalid start byte"
            } else {
                "invalid continuation byte"
            };
            Some(Malformed {
                start,
                end: start + n,
                reason,
            })
        }
        None if !final_ => None,
        None => Some(Malformed {
            start,
            end: data.len(),
            reason: "unexpected end of data",
        }),
    }
}

/// The UTF-8 character at byte `pos` of `data`.
pub fn step_utf8(data: &[u8], pos: usize, final_: bool) -> Step {
    let window = &data[pos..data.len().min(pos + 4)];
    let err = match std::str::from_utf8(window) {
        Ok(s) => return first_char(s),
        Err(e) if e.valid_up_to() > 0 => {
            // SAFETY: from_utf8 validated these bytes.
            return first_char(unsafe {
                std::str::from_utf8_unchecked(&window[..e.valid_up_to()])
            });
        }
        Err(e) => e,
    };
    match utf8_malformed(data, pos, err.error_len(), final_) {
        Some(m) => Step::Bad(m),
        None => Step::Incomplete,
    }
}

fn first_char(s: &str) -> Step {
    s.chars()
        .next()
        .map_or(Step::Incomplete, |c| Step::Char(c as u32, c.len_utf8()))
}

/// The UTF-16 character at byte `pos` of `data`.
pub fn step_utf16(data: &[u8], pos: usize, big_endian: bool, final_: bool) -> Step {
    let unit = |i: usize| -> u32 {
        let b = [data[i], data[i + 1]];
        (if big_endian {
            u16::from_be_bytes(b)
        } else {
            u16::from_le_bytes(b)
        }) as u32
    };
    let bad = |end: usize, reason: &'static str| {
        Step::Bad(Malformed {
            start: pos,
            end,
            reason,
        })
    };
    let ends_early = |reason: &'static str| {
        if final_ {
            bad(data.len(), reason)
        } else {
            Step::Incomplete
        }
    };
    if pos + 1 >= data.len() {
        return ends_early("truncated data");
    }
    let u = unit(pos);
    if !(0xD800..0xE000).contains(&u) {
        return Step::Char(u, 2);
    }
    if u >= 0xDC00 {
        return bad(pos + 2, "illegal encoding");
    }
    if pos + 3 >= data.len() {
        return ends_early("unexpected end of data");
    }
    let u2 = unit(pos + 2);
    if (0xDC00..0xE000).contains(&u2) {
        Step::Char(0x10000 + ((u - 0xD800) << 10) + (u2 - 0xDC00), 4)
    } else {
        bad(pos + 2, "illegal UTF-16 surrogate")
    }
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
        let Some(bad) = utf8_malformed(data, start, err_len, final_) else {
            return Ok(start);
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
    while pos < data.len() {
        match step_utf16(data, pos, big_endian, final_) {
            Step::Char(cp, n) => {
                spelling.push(out, cp);
                pos += n;
            }
            Step::Bad(m) => pos = on_error(data, m, out)?,
            Step::Incomplete => break,
        }
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
        let n = decode_utf8::<Infallible>(
            &mut Cow::Borrowed(b),
            final_,
            Spelling::Plain,
            &mut out,
            |_, m, out| {
                bad.push(m);
                out.push('\u{FFFD}');
                Ok(m.end)
            },
        )
        .unwrap();
        (out, n, bad)
    }

    #[test]
    fn utf8_maximal_subparts() {
        assert_eq!(utf8(b"a\xF0\x9F\x62", true).0, "a\u{FFFD}b");
        assert_eq!(utf8(b"\xC0\x80", true).0, "\u{FFFD}\u{FFFD}");
        let (_, _, bad) = utf8(b"\x80a\xE2\x28", true);
        assert_eq!(
            bad[0],
            Malformed {
                start: 0,
                end: 1,
                reason: "invalid start byte"
            }
        );
        assert_eq!(
            bad[1],
            Malformed {
                start: 2,
                end: 3,
                reason: "invalid continuation byte"
            }
        );
        assert_eq!(utf8(b"ab\xE2\x82", false), ("ab".to_string(), 2, vec![]));
        assert_eq!(
            utf8(b"ab\xE2\x82", true).2[0],
            Malformed {
                start: 2,
                end: 4,
                reason: "unexpected end of data"
            }
        );
    }

    #[test]
    fn utf16_pairs_and_errors() {
        let mut out = String::new();
        let mut bad = Vec::new();
        let data = [
            0x61, 0, 0x3D, 0xD8, 0x00, 0xDE, 0x00, 0xDC, 0x00, 0xD8, 0x62, 0, 0x01,
        ];
        let n = decode_utf16::<Infallible>(
            &mut Cow::Borrowed(&data[..]),
            0,
            false,
            true,
            Spelling::Plain,
            &mut out,
            |_, m, _| {
                bad.push(m.reason);
                Ok(m.end.min(m.start + 2))
            },
        )
        .unwrap();
        assert_eq!(out, "a\u{1F600}b");
        assert_eq!(n, 13);
        assert_eq!(
            bad,
            [
                "illegal encoding",
                "illegal UTF-16 surrogate",
                "truncated data"
            ]
        );
    }
}
