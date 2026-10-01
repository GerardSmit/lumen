//! Character classes: ranges, `\d \w \s` builtins in the supported dialects, and Unicode
//! property tables, matched under a [`CaseFold`] mode.

use super::fold::{
    canonicalize_legacy, fold_orbit, is_word_ic, js_whitespace, py_any_variant, py_is_ascii_space,
    py_is_decimal, py_is_space, py_is_word, py_lower, py_upper, CaseFold,
};

/// A transformation applied to the subject character before it is tested against a class (or
/// compared with a captured character by a back reference). Python's `IGNORECASE` ops test the
/// lowercased character against a set that was itself lowercased at compile time, which is
/// not the same as testing every case variant of the character.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PreMap {
    /// Python's simple Unicode lowercase mapping.
    PyLower,
    /// `A-Z` onto `a-z`; everything else unchanged.
    AsciiLower,
    /// The character matches when its ASCII lowercase or its ASCII uppercase is a member
    /// (C-locale `IGNORECASE`).
    AsciiEither,
}

impl PreMap {
    #[inline]
    pub fn apply(self, u: u32) -> u32 {
        match self {
            PreMap::PyLower => py_lower(u),
            PreMap::AsciiLower | PreMap::AsciiEither => ascii_lower(u),
        }
    }
}

#[inline]
fn ascii_lower(u: u32) -> u32 {
    if (0x41..=0x5A).contains(&u) {
        u + 32
    } else {
        u
    }
}

#[inline]
fn ascii_upper(u: u32) -> u32 {
    if (0x61..=0x7A).contains(&u) {
        u - 32
    } else {
        u
    }
}

/// Which language's definition of `\d`, `\w`, `\s` and `\b` applies.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flavor {
    /// ECMAScript: ASCII digits and word characters, the JS WhiteSpace set.
    Js,
    /// Python `str` patterns: Unicode decimal digits, `isalnum()` word characters, `isspace()`.
    PyUnicode,
    /// Python `re.ASCII`: ASCII digits and word characters, `[ \t\n\r\f\v]`.
    PyAscii,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BuiltinSet {
    Digit,
    Word,
    Space,
}

/// A `\d` / `\w` / `\s` escape (or its negation).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Builtin {
    pub set: BuiltinSet,
    pub negated: bool,
    pub flavor: Flavor,
}

impl Builtin {
    pub const fn new(set: BuiltinSet, negated: bool, flavor: Flavor) -> Builtin {
        Builtin {
            set,
            negated,
            flavor,
        }
    }

    /// The JavaScript escape letter `d D w W s S`.
    pub fn js(letter: char) -> Option<Builtin> {
        let set = match letter.to_ascii_lowercase() {
            'd' => BuiltinSet::Digit,
            'w' => BuiltinSet::Word,
            's' => BuiltinSet::Space,
            _ => return None,
        };
        Some(Builtin::new(set, letter.is_ascii_uppercase(), Flavor::Js))
    }

    #[inline]
    pub(super) fn matches(self, u: u32, icase: bool, fold: CaseFold) -> bool {
        let hit = match (self.set, self.flavor) {
            (BuiltinSet::Digit, Flavor::Js | Flavor::PyAscii) => (0x30..=0x39).contains(&u),
            (BuiltinSet::Digit, Flavor::PyUnicode) => py_is_decimal(u),
            (BuiltinSet::Word, Flavor::Js) => is_word_ic(u, icase, fold == CaseFold::Full),
            (BuiltinSet::Word, Flavor::PyAscii) => is_word_ic(u, false, false),
            (BuiltinSet::Word, Flavor::PyUnicode) => py_is_word(u),
            (BuiltinSet::Space, Flavor::Js) => char::from_u32(u).is_some_and(js_whitespace),
            (BuiltinSet::Space, Flavor::PyAscii) => py_is_ascii_space(u),
            (BuiltinSet::Space, Flavor::PyUnicode) => py_is_space(u),
        };
        hit != self.negated
    }
}

#[derive(Default, Clone)]
pub struct CharClass {
    pub negate: bool,
    pub ranges: Vec<(u32, u32)>,
    pub builtins: Vec<Builtin>,
    /// Unicode property escapes `\p{…}` / `\P{…}`: `(negated, sorted codepoint ranges)`.
    pub props: Vec<(bool, &'static [(u32, u32)])>,
    /// Ranges a character belongs to when it, or its Python simple uppercase, lies inside
    /// (Python's `RANGE_UNI_IGNORE`).
    pub upper_ranges: Vec<(u32, u32)>,
    /// When set, the subject character is transformed before membership is decided and the
    /// class's case-insensitive matching is not used.
    pub pre: Option<PreMap>,
    /// Memoized ASCII membership bitmaps per `(icase, fold)` mode, filled on first match
    /// (a compiled class never changes).
    ascii: [std::cell::Cell<Option<u128>>; 8],
}

impl CharClass {
    pub fn new() -> CharClass {
        CharClass::default()
    }

    pub fn negated(mut self, negate: bool) -> CharClass {
        self.negate = negate;
        self
    }

    pub fn with_range(mut self, lo: u32, hi: u32) -> CharClass {
        self.ranges.push((lo, hi));
        self
    }

    pub fn with_char(self, c: u32) -> CharClass {
        self.with_range(c, c)
    }

    pub fn with_builtin(mut self, builtin: Builtin) -> CharClass {
        self.builtins.push(builtin);
        self
    }

    pub fn with_upper_range(mut self, lo: u32, hi: u32) -> CharClass {
        self.upper_ranges.push((lo, hi));
        self
    }

    pub fn with_pre(mut self, pre: PreMap) -> CharClass {
        self.pre = Some(pre);
        self
    }

    /// Add a sorted, disjoint range table, optionally negated (`\P{…}`).
    pub fn with_prop(mut self, negated: bool, ranges: &'static [(u32, u32)]) -> CharClass {
        self.props.push((negated, ranges));
        self
    }

    /// A copy with a fresh membership memo.
    pub(super) fn duplicate(&self) -> CharClass {
        CharClass {
            negate: self.negate,
            ranges: self.ranges.clone(),
            builtins: self.builtins.clone(),
            props: self.props.clone(),
            upper_ranges: self.upper_ranges.clone(),
            pre: self.pre,
            ascii: Default::default(),
        }
    }

    #[inline]
    pub fn matches(&self, u: u32, icase: bool, fold: CaseFold) -> bool {
        if u < 128 {
            let cell = &self.ascii[usize::from(icase) * 4 + fold as usize];
            let bits = match cell.get() {
                Some(b) => b,
                None => {
                    let mut b = 0u128;
                    for c in 0..128u32 {
                        if self.matches_slow(c, icase, fold) {
                            b |= 1 << c;
                        }
                    }
                    cell.set(Some(b));
                    b
                }
            };
            return bits >> u & 1 != 0;
        }
        self.matches_slow(u, icase, fold)
    }

    fn matches_slow(&self, u: u32, icase: bool, fold: CaseFold) -> bool {
        if let Some(pre) = self.pre {
            let hit = match pre {
                PreMap::AsciiEither => {
                    let (lo, up) = (ascii_lower(u), ascii_upper(u));
                    self.matches_raw(lo, false, fold) || (up != lo && self.matches_raw(up, false, fold))
                }
                _ => self.matches_raw(pre.apply(u), false, fold),
            };
            return hit ^ self.negate;
        }
        let mut hit = self.matches_raw(u, icase, fold);
        if !hit && icase {
            hit = match fold {
                CaseFold::Full => fold_orbit(u).any(|alt| alt != u && self.matches_raw(alt, icase, fold)),
                CaseFold::Legacy => char::from_u32(u).is_some_and(|c| self.legacy_variant(c, icase)),
                CaseFold::Python => py_any_variant(u, |alt| alt != u && self.matches_raw(alt, icase, fold)),
                CaseFold::PythonAscii => {
                    let other = match u {
                        0x41..=0x5A => u + 32,
                        0x61..=0x7A => u - 32,
                        _ => u,
                    };
                    other != u && self.matches_raw(other, icase, fold)
                }
            };
        }
        hit ^ self.negate
    }

    /// Legacy Canonicalize: compare via simple uppercase, never folding a non-ASCII character
    /// onto an ASCII one; a member whose canonical form equals `c`'s also matches (/[k]/i vs 'K').
    fn legacy_variant(&self, c: char, icase: bool) -> bool {
        let fold = CaseFold::Legacy;
        let cu = canonicalize_legacy(c);
        if cu != c && self.matches_raw(cu as u32, icase, fold) {
            return true;
        }
        c.to_lowercase().chain(c.to_uppercase()).any(|alt| {
            alt != c && canonicalize_legacy(alt) == cu && self.matches_raw(alt as u32, icase, fold)
        })
    }

    fn matches_raw(&self, u: u32, icase: bool, fold: CaseFold) -> bool {
        // Class membership is decided in true code-point space: smuggled surrogate atoms in the
        // class's own ranges decode to their surrogate values.
        for &(lo, hi) in &self.ranges {
            if u >= lo && u <= hi {
                return true;
            }
        }
        for &b in &self.builtins {
            if b.matches(u, icase, fold) {
                return true;
            }
        }
        for &(lo, hi) in &self.upper_ranges {
            if (lo..=hi).contains(&u) || (lo..=hi).contains(&py_upper(u)) {
                return true;
            }
        }
        for &(neg, ranges) in &self.props {
            // Ranges are sorted and disjoint: binary-search for the one that could contain `u`.
            let in_range = ranges
                .binary_search_by(|&(lo, hi)| {
                    if u < lo {
                        std::cmp::Ordering::Greater
                    } else if u > hi {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
                .is_ok();
            if in_range ^ neg {
                return true;
            }
        }
        false
    }
}
