//! # parse-srcset — parse an HTML `srcset` attribute
//!
//! Parse the value of an `<img srcset="…">` attribute into a list of image
//! candidates, each with a URL and an optional density (`x`), width (`w`), or height
//! (`h`) descriptor. A faithful Rust port of the
//! [`parse-srcset`](https://www.npmjs.com/package/parse-srcset) npm package, which
//! follows the [WHATWG algorithm](https://html.spec.whatwg.org/multipage/images.html#parsing-a-srcset-attribute).
//! Zero dependencies and `#![no_std]`.
//!
//! ```
//! use parse_srcset::parse_srcset;
//!
//! let c = parse_srcset("small.jpg 480w, large.jpg 800w, fallback.jpg");
//! assert_eq!(c.len(), 3);
//! assert_eq!(c[0].url, "small.jpg");
//! assert_eq!(c[0].width, Some(480));
//! assert_eq!(c[2].width, None);
//!
//! let c = parse_srcset("a.png 1x, b.png 2x");
//! assert_eq!(c[1].density, Some(2.0));
//! ```
//!
//! A candidate whose descriptors are invalid (e.g. mixing `w` and `x`) is skipped,
//! matching the reference implementation.

#![no_std]
#![doc(html_root_url = "https://docs.rs/parse-srcset/0.1.0")]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

// Compile-test the README's examples as part of `cargo test`.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

/// One image candidate from a `srcset` attribute.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImageCandidate {
    /// The image URL.
    pub url: String,
    /// The pixel density descriptor (`2x` → `2.0`), if present.
    pub density: Option<f64>,
    /// The width descriptor (`480w` → `480`), if present.
    pub width: Option<u64>,
    /// The (future-compat) height descriptor (`200h` → `200`), if present.
    pub height: Option<u64>,
}

/// Parse a `srcset` attribute value into its image candidates.
///
/// Invalid candidates are skipped (the reference implementation logs them and
/// continues); a fully invalid or empty input yields an empty list.
///
/// ```
/// use parse_srcset::parse_srcset;
/// assert_eq!(parse_srcset("img.png").len(), 1);
/// assert!(parse_srcset("img.png 1w 2x").is_empty()); // mixed descriptors
/// ```
#[must_use]
pub fn parse_srcset(input: &str) -> Vec<ImageCandidate> {
    parse_srcset_bounded(input,usize::MAX).unwrap_or_default()
}

/// Parse with a maximum candidate count; exhaustion returns None rather than
/// selecting from a silently truncated source set. Callers bound input bytes.
#[must_use]
pub fn parse_srcset_bounded(input:&str,max_candidates:usize)->Option<Vec<ImageCandidate>> {
    // Structural delimiters are ASCII. Byte offsets avoid a whole-input char
    // buffer, while all borrowed slice boundaries remain UTF-8 boundaries.
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut pos = 0;
    let mut candidates: Vec<ImageCandidate> = Vec::new();

    loop {
        // Skip leading commas and whitespace.
        while pos < len && is_comma_or_space(bytes[pos]) {
            pos += 1;
        }
        if pos >= len {
            return Some(candidates);
        }

        // Collect a run of non-space characters as the URL.
        let url_start = pos;
        while pos < len && !is_space(bytes[pos]) {
            pos += 1;
        }
        let url_token = &input[url_start..pos];

        if url_token.ends_with(',') {
            // A URL ending in commas has no descriptors; strip them and emit it.
            let url = url_token.trim_end_matches(',');
            if let Some(c) = parse_descriptors(url, &[]) {
                if candidates.len()==max_candidates{return None;}
                candidates.push(c);
            }
        } else if let Some((descriptors, count)) = tokenize(input, &mut pos) {
            if let Some(c) = parse_descriptors(url_token, &descriptors[..count]) {
            if candidates.len()==max_candidates{return None;}
                candidates.push(c);
            }
        }
    }
}

#[derive(Clone, Copy)]
enum State {
    InDescriptor,
    InParens,
    AfterDescriptor,
}

/// Tokenize borrowed descriptors, advancing through invalid candidates as well.
/// Only width plus height can form a valid pair; a third descriptor invalidates
/// the candidate. Keep scanning the same states without retaining further text
/// so a comma inside parentheses cannot become a spurious candidate boundary.
fn tokenize<'a>(input: &'a str, pos: &mut usize) -> Option<([&'a str; 2], usize)> {
    let bytes = input.as_bytes();
    while *pos < bytes.len() && is_space(bytes[*pos]) {
        *pos += 1;
    }
    let mut descriptors = [""; 2];
    let mut count = 0;
    let mut invalid = false;
    let mut start = None;
    let mut state = State::InDescriptor;
    loop {
        let c = bytes.get(*pos).copied();
        match state {
            State::InDescriptor => match c {
                Some(ch) if is_space(ch) => {
                    if start.is_some() {
                        finish_descriptor(input, &mut start, *pos, &mut descriptors, &mut count, &mut invalid);
                        state = State::AfterDescriptor;
                    }
                }
                Some(b',') | None => {
                    finish_descriptor(input, &mut start, *pos, &mut descriptors, &mut count, &mut invalid);
                    if c.is_some() { *pos += 1; }
                    return (!invalid).then_some((descriptors, count));
                }
                Some(b'(') => {
                    start.get_or_insert(*pos);
                    state = State::InParens;
                }
                Some(_) => { start.get_or_insert(*pos); }
            },
            State::InParens => match c {
                Some(b')') => state = State::InDescriptor,
                None => {
                    finish_descriptor(input, &mut start, *pos, &mut descriptors, &mut count, &mut invalid);
                    return (!invalid).then_some((descriptors, count));
                }
                Some(_) => {}
            },
            State::AfterDescriptor => match c {
                Some(ch) if is_space(ch) => {}
                None => return (!invalid).then_some((descriptors, count)),
                Some(_) => {
                    state = State::InDescriptor;
                    continue;
                }
            },
        }
        *pos += 1;
    }
}

fn finish_descriptor<'a>(input: &'a str, start: &mut Option<usize>, end: usize,
                         descriptors: &mut [&'a str; 2], count: &mut usize,
                         invalid: &mut bool) {
    if let Some(begin) = start.take() {
        if *count < descriptors.len() {
            descriptors[*count] = &input[begin..end];
            *count += 1;
        } else {
            *invalid = true;
        }
    }
}

/// Apply the descriptors to a URL, returning a candidate or `None` on a parse error.
fn parse_descriptors(url: &str, descriptors: &[&str]) -> Option<ImageCandidate> {
    let mut error = false;
    let mut width: Option<u64> = None;
    let mut density: Option<f64> = None;
    let mut height: Option<u64> = None;

    // HTML descriptors use presence, including an explicit zero density.
    let truthy_w = |w: Option<u64>| w.is_some();
    let truthy_h = |h: Option<u64>| h.is_some();
    let truthy_d = |d: Option<f64>| d.is_some();

    for desc in descriptors {
        let last = desc.as_bytes().last().copied().unwrap_or(0);
        if !matches!(last, b'w' | b'x' | b'h') { return None; }
        let value = &desc[..desc.len() - 1];

        if last == b'w' && is_non_negative_integer(value) {
            if truthy_w(width) || truthy_d(density) {
                error = true;
            }
            match value.parse::<u64>() {
                Ok(0) | Err(_) => error = true,
                Ok(n) => width = Some(n),
            }
        } else if last == b'x' && is_floating_point(value) {
            if truthy_w(width) || truthy_d(density) || truthy_h(height) {
                error = true;
            }
            match value.parse::<f64>() {
                Ok(f) if f.is_finite() && f >= 0.0 => density = Some(f),
                _ => error = true,
            }
        } else if last == b'h' && is_non_negative_integer(value) {
            if truthy_h(height) || truthy_d(density) {
                error = true;
            }
            match value.parse::<u64>() {
                Ok(0) | Err(_) => error = true,
                Ok(n) => height = Some(n),
            }
        } else {
            error = true;
        }
    }

    if error || height.is_some() && width.is_none() {
        return None;
    }
    Some(ImageCandidate {
        url: url.to_string(),
        density: if truthy_d(density) { density } else { None },
        width: if truthy_w(width) { width } else { None },
        height: if truthy_h(height) { height } else { None },
    })
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0c | b'\r')
}

fn is_comma_or_space(c: u8) -> bool {
    c == b',' || is_space(c)
}

/// `/^\d+$/`
fn is_non_negative_integer(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `/^-?(?:[0-9]+|[0-9]*\.[0-9]+)(?:[eE][+-]?[0-9]+)?$/`
fn is_floating_point(s: &str) -> bool {
    let b = s.as_bytes();
    let len = b.len();
    let mut i = 0;
    if i < len && b[i] == b'-' {
        i += 1;
    }
    let int_digits = count_digits(b, &mut i);
    if i < len && b[i] == b'.' {
        i += 1;
        if count_digits(b, &mut i) == 0 {
            return false; // a dot must be followed by a digit
        }
    } else if int_digits == 0 {
        return false; // need at least one digit
    }
    if i < len && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < len && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        if count_digits(b, &mut i) == 0 {
            return false; // exponent must have digits
        }
    }
    i == len
}

fn count_digits(b: &[u8], i: &mut usize) -> usize {
    let start = *i;
    while *i < b.len() && b[*i].is_ascii_digit() {
        *i += 1;
    }
    *i - start
}

#[cfg(test)]
mod bounded_allocation_tests {
    use super::*;

    #[test]
    fn specification_borrowed_descriptor_scanning_preserves_utf8_and_recovery() {
        let input = "图片.png 20w 10h, data:image/png;base64,é 2x, bad 1w 2h 3x (a,b), 最後.png 3x";
        let parsed = parse_srcset_bounded(input, 3).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].url, "图片.png");
        assert_eq!(parsed[0].height, Some(10));
        assert_eq!(parsed[1].url, "data:image/png;base64,é");
        assert_eq!(parsed[2].url, "最後.png");
        assert!(parse_srcset_bounded(input, 2).is_none());
        let mut many = String::from("bad ");
        for _ in 0..32_000 { many.push_str("1w "); }
        many.push_str("(hidden,comma), valid.png 0x");
        let parsed = parse_srcset_bounded(&many, 1).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].url, "valid.png");
        assert_eq!(parsed[0].density, Some(0.0));
        assert!(parse_srcset("bad 1w 2h (unterminated, next.png 2x").is_empty());
    }
}
