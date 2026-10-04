//! Character types, case conversion and normalization for `str`, over Python's character
//! database (`lumen_common::ucd`, UCD 15.0.0 as in CPython 3.12).

use lumen_common::smuggle::{code_points, push_code_point, CodePoints};
use lumen_common::ucd::{self, char_type, flag};

/// Whether code point `c` has the [`flag`] `f`.
#[inline]
pub fn has(c: u32, f: u16) -> bool {
    char_type(c).is(f)
}

pub fn is_xid_start(c: char) -> bool {
    c == '_'
        || if c.is_ascii() {
            c.is_ascii_alphabetic()
        } else {
            has(c as u32, flag::XID_START)
        }
}

pub fn is_xid_continue(c: char) -> bool {
    if c.is_ascii() {
        c == '_' || c.is_ascii_alphanumeric()
    } else {
        has(c as u32, flag::XID_CONTINUE)
    }
}

/// `Py_UNICODE_ISSPACE`.
#[inline]
pub fn is_space(c: u32) -> bool {
    if c < 0x80 {
        matches!(c, 0x09..=0x0D | 0x1C..=0x20)
    } else {
        has(c, flag::SPACE)
    }
}

/// `Py_UNICODE_ISPRINTABLE`.
#[inline]
pub fn is_printable(c: u32) -> bool {
    if c < 0x80 {
        (0x20..0x7F).contains(&c)
    } else {
        has(c, flag::PRINTABLE)
    }
}

pub fn nfkc(s: &str) -> String {
    if s.is_ascii() {
        return s.to_string();
    }
    let cps: Vec<u32> = code_points(s).collect();
    let mut out = String::with_capacity(s.len());
    for c in ucd::normalize(&cps, "NFKC", ucd::Version::Current) {
        push_code_point(&mut out, c);
    }
    out
}

/// The case conversions of `str`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Case {
    Upper,
    Lower,
    Fold,
    Title,
    Capitalize,
    SwapCase,
}

/// `s` converted as the `str` method `case` does.
pub fn convert(s: &str, case: Case) -> String {
    if s.is_ascii() {
        return convert_ascii(s, case);
    }
    let mut out = String::with_capacity(s.len() + s.len() / 8);
    let mut cps = code_points(s);
    let mut first = true;
    let mut prev_cased = false;
    // Whether the last character that is not case-ignorable was cased (final sigma, before).
    let mut cased_before = false;
    while let Some(c) = cps.next() {
        let (cased, ignorable);
        if c < 0x80 {
            let b = c as u8;
            let up = match case {
                Case::Upper => true,
                Case::Lower | Case::Fold => false,
                Case::Title => !prev_cased,
                Case::Capitalize => first,
                Case::SwapCase => b.is_ascii_lowercase(),
            };
            out.push(if up {
                b.to_ascii_uppercase()
            } else {
                b.to_ascii_lowercase()
            } as char);
            cased = b.is_ascii_alphabetic();
            ignorable = matches!(b, b'\'' | b'.' | b':' | b'^' | b'`');
        } else {
            let t = char_type(c);
            let lower = |t: &ucd::CharType| lower_with(t, cased_before, &cps);
            let mapped = match case {
                Case::Upper => t.upper(),
                Case::Fold => t.fold(),
                Case::Lower => lower(&t),
                Case::Title if prev_cased => lower(&t),
                Case::Title => t.title(),
                Case::Capitalize if first => t.title(),
                Case::Capitalize => lower(&t),
                Case::SwapCase if t.is(flag::UPPER) => lower(&t),
                Case::SwapCase if t.is(flag::LOWER) => t.upper(),
                Case::SwapCase => ucd::CaseMapping::single(c),
            };
            for m in mapped {
                push_code_point(&mut out, m);
            }
            cased = t.is(flag::CASED);
            ignorable = t.is(flag::CASE_IGNORABLE);
        }
        prev_cased = cased;
        if !ignorable {
            cased_before = cased;
        }
        first = false;
    }
    out
}

fn convert_ascii(s: &str, case: Case) -> String {
    match case {
        Case::Upper => s.to_ascii_uppercase(),
        Case::Lower | Case::Fold => s.to_ascii_lowercase(),
        Case::SwapCase => s
            .chars()
            .map(|c| {
                if c.is_ascii_uppercase() {
                    c.to_ascii_lowercase()
                } else {
                    c.to_ascii_uppercase()
                }
            })
            .collect(),
        Case::Capitalize => {
            let mut out = s.to_ascii_lowercase();
            if let Some(first) = out.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            out
        }
        Case::Title => {
            let mut out = String::with_capacity(s.len());
            let mut prev_cased = false;
            for c in s.chars() {
                out.push(if prev_cased {
                    c.to_ascii_lowercase()
                } else {
                    c.to_ascii_uppercase()
                });
                prev_cased = c.is_ascii_alphabetic();
            }
            out
        }
    }
}

/// The lowercase of `t`, with the final-sigma rule for U+03A3: final when a cased letter comes
/// before it (`cased_before`) and none after (`rest`), skipping case-ignorable characters.
fn lower_with(t: &ucd::CharType, cased_before: bool, rest: &CodePoints) -> ucd::CaseMapping {
    if t.cp() != 0x3A3 {
        return t.lower();
    }
    let after = rest
        .clone()
        .find(|&c| !has(c, flag::CASE_IGNORABLE))
        .is_some_and(|c| has(c, flag::CASED));
    ucd::CaseMapping::single(if cased_before && !after { 0x3C2 } else { 0x3C3 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_classes() {
        assert!(is_xid_start('é') && is_xid_start('_') && is_xid_start('漢'));
        assert!(!is_xid_start('1') && !is_xid_start('\u{300}'));
        assert!(is_xid_continue('1') && is_xid_continue('\u{300}') && is_xid_continue('·'));
        assert!(!is_xid_continue('-') && !is_xid_continue(' '));
    }

    #[test]
    fn ascii_case_classes_match_the_database() {
        for b in 0u8..0x80 {
            let t = char_type(b as u32);
            assert_eq!(t.is(flag::CASED), b.is_ascii_alphabetic(), "{b}");
            assert_eq!(
                t.is(flag::CASE_IGNORABLE),
                matches!(b, b'\'' | b'.' | b':' | b'^' | b'`'),
                "{b}"
            );
        }
    }

    #[test]
    fn nfkc_folds_compat_forms() {
        assert_eq!(nfkc("ｆｏｏ"), "foo");
        assert_eq!(nfkc("µ"), "μ");
        assert_eq!(nfkc("ﬁ"), "fi");
        assert_eq!(nfkc("abc"), "abc");
    }
}
