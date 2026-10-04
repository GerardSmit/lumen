//! Case folding and character-category predicates used by the matcher: ECMAScript's legacy
//! `Canonicalize` and full-fold orbits, and Python's `sre` lowercase comparison with its extra
//! equivalence table.

use super::fold_table::{FOLD_CANON, FOLD_ORBITS};
use std::sync::OnceLock;

/// How `ignore_case` compares two code points.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CaseFold {
    /// ECMAScript without `u`/`v`: simple uppercase, never folding a non-ASCII character onto an
    /// ASCII one.
    #[default]
    Legacy,
    /// ECMAScript `u`/`v`: simple case folding (the generated orbit table).
    Full,
    /// Python `str` patterns: compare the lowercase forms, plus CPython's `_casefix` equivalences.
    Python,
    /// Python `re.ASCII`: only `A-Z` fold onto `a-z`.
    PythonAscii,
}

/// The canonical full case-folding representative of a code point (identity outside any orbit).
pub fn fold_canon(u: u32) -> u32 {
    match FOLD_CANON.binary_search_by_key(&u, |&(m, _)| m) {
        Ok(k) => FOLD_CANON[k].1,
        Err(_) => u,
    }
}

/// Every member of `u`'s case-fold orbit (just `u` when it has none).
pub fn fold_orbit(u: u32) -> impl Iterator<Item = u32> {
    let canon = fold_canon(u);
    let t = FOLD_ORBITS;
    let lo = t.partition_point(|&(c, _)| c < canon);
    let hi = t.partition_point(|&(c, _)| c <= canon);
    let mut own = if lo == hi { Some(u) } else { None };
    t[lo..hi]
        .iter()
        .map(|&(_, m)| m)
        .chain(std::iter::from_fn(move || own.take()))
}

/// The legacy (non-Unicode) Canonicalize: the simple uppercase mapping, except that a non-ASCII
/// character never canonicalizes onto an ASCII one (so /K/i does not match 'K' without /u).
pub fn canonicalize_legacy(c: char) -> char {
    let mut up = c.to_uppercase();
    let (first, rest) = (up.next(), up.next());
    match (first, rest) {
        (Some(u), None) => {
            if (c as u32) >= 128 && (u as u32) < 128 {
                c
            } else {
                u
            }
        }
        _ => c,
    }
}

/// Python's simple lowercase mapping (`Py_UNICODE_TOLOWER`): a single code point, so U+0130
/// lowers to `i` rather than its two-character full mapping.
pub fn py_lower(u: u32) -> u32 {
    if u < 128 {
        return if (0x41..=0x5A).contains(&u) {
            u + 32
        } else {
            u
        };
    }
    let Some(c) = char::from_u32(u) else { return u };
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(x), None) => x as u32,
        _ if u == 0x130 => 0x69,
        _ => u,
    }
}

/// Python's simple uppercase mapping; characters whose full mapping is several code points
/// stay unchanged.
pub fn py_upper(u: u32) -> u32 {
    if u < 128 {
        return if (0x61..=0x7A).contains(&u) {
            u - 32
        } else {
            u
        };
    }
    let Some(c) = char::from_u32(u) else { return u };
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(x), None) => x as u32,
        _ => u,
    }
}

/// CPython's `_casefix._EXTRA_CASES`: lowercase characters that share an uppercase with another
/// lowercase character, keyed by code point (sorted).
static PY_EXTRA_CASES: &[(u32, &[u32])] = &[
    (0x0069, &[0x0131]),
    (0x0073, &[0x017f]),
    (0x00b5, &[0x03bc]),
    (0x0131, &[0x0069]),
    (0x017f, &[0x0073]),
    (0x0345, &[0x03b9, 0x1fbe]),
    (0x0390, &[0x1fd3]),
    (0x03b0, &[0x1fe3]),
    (0x03b2, &[0x03d0]),
    (0x03b5, &[0x03f5]),
    (0x03b8, &[0x03d1]),
    (0x03b9, &[0x0345, 0x1fbe]),
    (0x03ba, &[0x03f0]),
    (0x03bc, &[0x00b5]),
    (0x03c0, &[0x03d6]),
    (0x03c1, &[0x03f1]),
    (0x03c2, &[0x03c3]),
    (0x03c3, &[0x03c2]),
    (0x03c6, &[0x03d5]),
    (0x03d0, &[0x03b2]),
    (0x03d1, &[0x03b8]),
    (0x03d5, &[0x03c6]),
    (0x03d6, &[0x03c0]),
    (0x03f0, &[0x03ba]),
    (0x03f1, &[0x03c1]),
    (0x03f5, &[0x03b5]),
    (0x0432, &[0x1c80]),
    (0x0434, &[0x1c81]),
    (0x043e, &[0x1c82]),
    (0x0441, &[0x1c83]),
    (0x0442, &[0x1c84, 0x1c85]),
    (0x044a, &[0x1c86]),
    (0x0463, &[0x1c87]),
    (0x1c80, &[0x0432]),
    (0x1c81, &[0x0434]),
    (0x1c82, &[0x043e]),
    (0x1c83, &[0x0441]),
    (0x1c84, &[0x0442, 0x1c85]),
    (0x1c85, &[0x0442, 0x1c84]),
    (0x1c86, &[0x044a]),
    (0x1c87, &[0x0463]),
    (0x1c88, &[0xa64b]),
    (0x1e61, &[0x1e9b]),
    (0x1e9b, &[0x1e61]),
    (0x1fbe, &[0x0345, 0x03b9]),
    (0x1fd3, &[0x0390]),
    (0x1fe3, &[0x03b0]),
    (0xa64b, &[0x1c88]),
    (0xfb05, &[0xfb06]),
    (0xfb06, &[0xfb05]),
];

fn py_extra(lower: u32) -> &'static [u32] {
    match PY_EXTRA_CASES.binary_search_by_key(&lower, |&(k, _)| k) {
        Ok(i) => PY_EXTRA_CASES[i].1,
        Err(_) => &[],
    }
}

/// Python's case-insensitive key: the lowercase form, mapped to the smallest member of its
/// `_casefix` equivalence group.
pub fn py_fold(u: u32) -> u32 {
    let lower = py_lower(u);
    py_extra(lower).iter().fold(lower, |m, &e| m.min(e))
}

/// Whether some case variant of `u` (as CPython's IGNORECASE charset matching reaches them:
/// its lowercase, the `_casefix` equivalents, uppercases and fold-orbit members) satisfies `hit`.
pub fn py_any_variant(u: u32, mut hit: impl FnMut(u32) -> bool) -> bool {
    let lower = py_lower(u);
    let mut group = [0u32; 3];
    group[0] = lower;
    let extra = py_extra(lower);
    group[1..1 + extra.len()].copy_from_slice(extra);
    group[..1 + extra.len()].iter().any(|&g| {
        hit(g) || hit(py_upper(g)) || fold_orbit(g).any(&mut hit) || (g == 0x69 && hit(0x130))
    })
}

fn ascii_lower(u: u32) -> u32 {
    if (0x41..=0x5A).contains(&u) {
        u + 32
    } else {
        u
    }
}

/// Whether `a` and `b` are equal under `fold`.
#[inline]
pub fn fold_eq(fold: CaseFold, a: u32, b: u32) -> bool {
    if a == b {
        return true;
    }
    match fold {
        CaseFold::Legacy | CaseFold::Full => {
            let (ca, cb) = match (char::from_u32(a), char::from_u32(b)) {
                (Some(x), Some(y)) => (x, y),
                _ => return false,
            };
            if fold == CaseFold::Full {
                fold_canon(a) == fold_canon(b)
            } else {
                canonicalize_legacy(ca) == canonicalize_legacy(cb)
            }
        }
        CaseFold::Python => py_fold(a) == py_fold(b),
        CaseFold::PythonAscii => ascii_lower(a) == ascii_lower(b),
    }
}

/// The JS WhiteSpace + LineTerminator set: includes U+FEFF and NBSP, but NOT U+0085 (NEL) or
/// other control characters Rust's `is_whitespace` accepts.
pub fn js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// A JS LineTerminator code point.
pub fn is_line_terminator_u32(c: u32) -> bool {
    matches!(c, 0x0A | 0x0D | 0x2028 | 0x2029)
}

pub fn is_word(c: u32) -> bool {
    char::from_u32(c)
        .map(|c| c.is_ascii_alphanumeric() || c == '_')
        .unwrap_or(false)
}

/// GetWordCharacters: under unicode case-insensitive matching, characters whose case fold lands
/// in [A-Za-z0-9_] (ſ, K) are word characters too.
pub fn is_word_ic(c: u32, icase: bool, unicode: bool) -> bool {
    if is_word(c) {
        return true;
    }
    if !(icase && unicode) {
        return false;
    }
    fold_orbit(c).any(is_word)
}

/// Python `str.isspace()` for a single code point (`Py_UNICODE_ISSPACE`).
pub fn py_is_space(u: u32) -> bool {
    matches!(
        u,
        0x09..=0x0D
            | 0x1C..=0x20
            | 0x85
            | 0xA0
            | 0x1680
            | 0x2000..=0x200A
            | 0x2028
            | 0x2029
            | 0x202F
            | 0x205F
            | 0x3000
    )
}

/// The `sre` ASCII space set `[ \t\n\r\f\v]`.
pub fn py_is_ascii_space(u: u32) -> bool {
    matches!(u, 0x09..=0x0D | 0x20)
}

fn in_ranges(ranges: &[(u32, u32)], u: u32) -> bool {
    ranges
        .binary_search_by(|&(lo, hi)| {
            if u < lo {
                std::cmp::Ordering::Greater
            } else if u > hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

fn category(slot: &'static OnceLock<&'static [(u32, u32)]>, value: &str) -> &'static [(u32, u32)] {
    slot.get_or_init(|| crate::unicode_props::lookup("gc", Some(value)).unwrap_or(&[]))
}

/// Python `str.isdecimal()`: general category Nd.
pub fn py_is_decimal(u: u32) -> bool {
    static ND: OnceLock<&'static [(u32, u32)]> = OnceLock::new();
    if u < 128 {
        return (0x30..=0x39).contains(&u);
    }
    in_ranges(category(&ND, "Nd"), u)
}

/// A Python `\w` character under Unicode matching: `str.isalnum()` or `_`.
pub fn py_is_word(u: u32) -> bool {
    static LETTER: OnceLock<&'static [(u32, u32)]> = OnceLock::new();
    static NUMBER: OnceLock<&'static [(u32, u32)]> = OnceLock::new();
    if u < 128 {
        return is_word(u);
    }
    in_ranges(category(&LETTER, "L"), u) || in_ranges(category(&NUMBER, "N"), u)
}
