//! The Unicode character database behind Python's `unicodedata` (and `\N{...}` escapes): property
//! records, decomposition mappings, character names with aliases and named sequences, and the
//! UCD 3.2.0 view IDNA/stringprep use, and the character types behind `str` methods ([`CharType`]).
//! Data lives in the generated `unicode_db`; normalization runs the shared algorithm in
//! `unicode_norm_impl` over it.

use crate::unicode_db as db;
use crate::unicode_norm_impl::{self as norm, NormData};

pub use db::UNIDATA_VERSION;

/// Unicode versions used by the independent segmentation data providers.
pub const GRAPHEME_UNICODE_VERSION: (u64, u64, u64) = unicode_segmentation::UNICODE_VERSION;
pub const WORD_UNICODE_VERSION: (u64, u64, u64) = unicode_segmentation::UNICODE_VERSION;
pub const LINE_BREAK_UNICODE_VERSION: (u8, u8, u8) = unicode_linebreak::UNICODE_VERSION;
pub use unicode_linebreak::{break_property, BreakClass, BreakOpportunity};

/// Allocation-free UAX #14 opportunities as UTF-8 byte offsets, including end of text.
/// Mandatory breaks include paragraph separators and the end of text.
/// Complex-context scripts use the provider's alphabetic fallback; dictionary breaking is absent.
/// Uses the numeric tailoring in UAX #14 §8.2 Example 7, as does LineBreakTest.
pub fn line_breaks(text: &str) -> impl Iterator<Item = (usize, BreakOpportunity)> + '_ {
    crate::linebreak::opportunities(text)
}

/// Allocation-free extended grapheme clusters (UAX #29), with UTF-8 byte offsets.
pub fn graphemes(text: &str) -> unicode_segmentation::GraphemeIndices<'_> {
    unicode_segmentation::UnicodeSegmentation::grapheme_indices(text, true)
}

/// Allocation-free default UAX #29 word-boundary segments, including spaces
/// and punctuation. Offsets are UTF-8 bytes; dictionary tailoring is absent.
pub fn word_boundaries(text: &str) -> impl Iterator<Item = (usize, &str)> + '_ {
    unicode_segmentation::UnicodeSegmentation::split_word_bound_indices(text)
}

/// First caret boundary strictly before a byte offset. Out-of-range offsets clamp to end.
pub fn previous_grapheme_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset += 1;
    }
    unicode_segmentation::GraphemeCursor::new(offset, text.len(), true)
        .prev_boundary(text, 0)
        .ok()
        .flatten()
        .unwrap_or(0)
}

/// First caret boundary strictly after a byte offset; out-of-range offsets clamp to end.
pub fn next_grapheme_boundary(text: &str, offset: usize) -> usize {
    let offset = scalar_boundary(text, offset);
    unicode_segmentation::GraphemeCursor::new(offset, text.len(), true)
        .next_boundary(text, 0)
        .ok()
        .flatten()
        .unwrap_or(text.len())
}

fn scalar_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[cfg(test)]
mod segmentation_tests {
    use super::*;

    #[test]
    fn caret_boundaries_preserve_clusters() {
        assert_eq!(GRAPHEME_UNICODE_VERSION, (16, 0, 0));
        assert_eq!(LINE_BREAK_UNICODE_VERSION, (15, 0, 0));
        for cluster in ["e\u{301}", "\r\n", "🇳🇱", "👩🏽‍💻", "\u{1100}\u{1161}\u{11a8}"]
        {
            let text = format!("a{cluster}z");
            let end = 1 + cluster.len();
            assert_eq!(graphemes(&text).count(), 3, "{cluster}");
            for offset in 1..end {
                assert_eq!(next_grapheme_boundary(&text, offset), end);
            }
            for offset in 2..=end {
                assert_eq!(previous_grapheme_boundary(&text, offset), 1);
            }
        }
        assert_eq!(next_grapheme_boundary("", 100), 0);
        assert_eq!(previous_grapheme_boundary("", 100), 0);
        assert_eq!(next_grapheme_boundary("a", 100), 1);
        assert_eq!(previous_grapheme_boundary("a", 100), 0);
    }

    #[test]
    fn line_breaks_preserve_joiners_and_mandatory_breaks() {
        use BreakOpportunity::{Allowed, Mandatory};
        assert_eq!(
            line_breaks("a b").collect::<Vec<_>>(),
            [(2, Allowed), (3, Mandatory)]
        );
        assert_eq!(
            line_breaks("a\r\nb").collect::<Vec<_>>(),
            [(3, Mandatory), (4, Mandatory)]
        );
        for text in ["a\u{a0}b", "a\u{2060}b", "e\u{301}", "👩🏽‍💻"] {
            assert_eq!(
                line_breaks(text).collect::<Vec<_>>(),
                [(text.len(), Mandatory)]
            );
        }
        assert_eq!(
            line_breaks("中文").collect::<Vec<_>>(),
            [(3, Allowed), (6, Mandatory)]
        );
    }
}

/// Which database version answers: the current one or the UCD 3.2.0 deltas.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Version {
    Current,
    V3_2_0,
}

/// The `unicodedata` properties of one code point.
#[derive(Clone, Copy, Debug)]
pub struct Props {
    pub category: &'static str,
    pub bidirectional: &'static str,
    pub combining: u8,
    pub mirrored: bool,
    pub east_asian_width: &'static str,
    pub decimal: Option<u8>,
    pub digit: Option<u8>,
    pub numeric: Option<f64>,
}

fn record_index(cp: u32) -> usize {
    if cp >= 0x110000 {
        return 0;
    }
    let block = db::INDEX1.get((cp >> db::SHIFT) as usize) as usize;
    db::INDEX2.get((block << db::SHIFT) + (cp & ((1 << db::SHIFT) - 1)) as usize) as usize
}

fn in_ranges(ranges: &[(u32, u32)], cp: u32) -> bool {
    let k = ranges.partition_point(|&(_, hi)| hi < cp);
    k < ranges.len() && ranges[k].0 <= cp
}

/// Whether UCD 3.2.0 leaves `cp` unassigned although the current version assigns it.
fn old_unassigned(cp: u32) -> bool {
    in_ranges(db::OLD_UNASSIGNED, cp)
}

fn to_props(r: (u8, u8, u8, u8, u8, i8, i8, u16)) -> Props {
    let (cat, bidi, combining, mirrored, eaw, decimal, digit, numeric) = r;
    Props {
        category: db::CATEGORIES[cat as usize],
        bidirectional: db::BIDIRECTIONAL[bidi as usize],
        combining,
        mirrored: mirrored != 0,
        east_asian_width: db::EAST_ASIAN_WIDTHS[eaw as usize],
        decimal: (decimal >= 0).then_some(decimal as u8),
        digit: (digit >= 0).then_some(digit as u8),
        numeric: (numeric != u16::MAX).then(|| db::NUMERIC[numeric as usize]),
    }
}

/// The properties of `cp` in `version`. As in CPython, `digit` always answers from the current
/// database.
pub fn props(cp: u32, version: Version) -> Props {
    let current = to_props(db::RECORDS[record_index(cp)]);
    if version == Version::Current {
        return current;
    }
    let old = if old_unassigned(cp) {
        db::OLD_UNASSIGNED_RECORD
    } else {
        match db::OLD_CHANGES.binary_search_by_key(&cp, |&(c, _)| c) {
            Ok(k) => db::OLD_CHANGES[k].1,
            Err(_) => return current,
        }
    };
    Props {
        digit: current.digit,
        ..to_props(db::RECORDS[old as usize])
    }
}

fn decomp_offset(cp: u32, version: Version) -> usize {
    if cp >= 0x110000 || (version == Version::V3_2_0 && old_unassigned(cp)) {
        return 0;
    }
    let block = db::DECOMP_INDEX1.get((cp >> db::SHIFT) as usize) as usize;
    db::DECOMP_INDEX2.get((block << db::SHIFT) + (cp & ((1 << db::SHIFT) - 1)) as usize) as usize
}

/// The decomposition mapping of `cp` as `unicodedata.decomposition` spells it
/// (`"<compat> 0020 0301"`), or "" when it has none.
pub fn decomposition(cp: u32, version: Version) -> String {
    let at = decomp_offset(cp, version);
    if at == 0 {
        return String::new();
    }
    let head = db::DECOMP_DATA.get(at);
    let mut out = String::from(db::DECOMP_PREFIXES[(head >> 8) as usize]);
    for i in 0..(head & 0xff) as usize {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&format!("{:04X}", db::DECOMP_DATA.get(at + 1 + i)));
    }
    out
}

/// The normalization data of one database version.
pub struct Norm(pub Version);

impl NormData for Norm {
    fn ccc(&self, cp: u32) -> u8 {
        db::RECORDS[record_index(cp)].2
    }

    fn push_mapping(&self, cp: u32, compat: bool, stack: &mut Vec<u32>) -> bool {
        if self.0 == Version::V3_2_0 {
            if let Ok(k) = db::OLD_NORMALIZATION.binary_search_by_key(&cp, |&(c, _)| c) {
                stack.push(db::OLD_NORMALIZATION[k].1);
                return true;
            }
        }
        let at = decomp_offset(cp, self.0);
        if at == 0 {
            return false;
        }
        let head = db::DECOMP_DATA.get(at);
        if head >> 8 != 0 && !compat {
            return false;
        }
        for i in (0..(head & 0xff) as usize).rev() {
            stack.push(db::DECOMP_DATA.get(at + 1 + i));
        }
        true
    }

    fn compose(&self, a: u32, b: u32) -> Option<u32> {
        db::COMPOSE
            .binary_search_by(|&(x, y, _)| (x, y).cmp(&(a, b)))
            .ok()
            .map(|k| db::COMPOSE[k].2)
    }
}

/// Normalize `cps` to "NFC", "NFD", "NFKC" or "NFKD" with `version`'s data.
pub fn normalize(cps: &[u32], form: &str, version: Version) -> Vec<u32> {
    norm::normalize_with(&Norm(version), cps, form)
}

const S_BASE: u32 = 0xAC00;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;
const S_COUNT: u32 = 19 * N_COUNT;
const JAMO_L: [&str; 19] = [
    "G", "GG", "N", "D", "DD", "R", "M", "B", "BB", "S", "SS", "", "J", "JJ", "C", "K", "T", "P",
    "H",
];
const JAMO_V: [&str; 21] = [
    "A", "AE", "YA", "YAE", "EO", "E", "YEO", "YE", "O", "WA", "WAE", "OE", "YO", "U", "WEO", "WE",
    "WI", "YU", "EU", "YI", "I",
];
const JAMO_T: [&str; 28] = [
    "", "G", "GG", "GS", "N", "NJ", "NH", "D", "L", "LG", "LM", "LB", "LS", "LT", "LP", "LH", "M",
    "B", "BS", "S", "SS", "NG", "J", "C", "K", "T", "P", "H",
];
const HANGUL_PREFIX: &str = "HANGUL SYLLABLE ";
const CJK_PREFIX: &str = "CJK UNIFIED IDEOGRAPH-";

/// The name stored at `NAME_CODES[i]`.
fn stored_name(i: usize) -> String {
    let mut out = String::new();
    let mut t = db::NAME_STARTS.get(i) as usize;
    loop {
        let token = db::NAME_TOKENS.get(t);
        let w = (token & 0x3fff) as usize;
        out.push_str(
            &db::LEXICON
                [db::LEXICON_OFFSETS.get(w) as usize..db::LEXICON_OFFSETS.get(w + 1) as usize],
        );
        if token & 0x8000 != 0 {
            return out;
        }
        out.push(if token & 0x4000 != 0 { '-' } else { ' ' });
        t += 1;
    }
}

/// The character name of `cp` (never an alias), or None.
pub fn name(cp: u32, version: Version) -> Option<String> {
    if version == Version::V3_2_0 && old_unassigned(cp) {
        return None;
    }
    if (S_BASE..S_BASE + S_COUNT).contains(&cp) {
        let s = cp - S_BASE;
        let (l, v, t) = (
            (s / N_COUNT) as usize,
            ((s % N_COUNT) / T_COUNT) as usize,
            (s % T_COUNT) as usize,
        );
        return Some(format!(
            "{HANGUL_PREFIX}{}{}{}",
            JAMO_L[l], JAMO_V[v], JAMO_T[t]
        ));
    }
    let k = db::NAME_RANGES.partition_point(|r| r.1 < cp);
    if let Some(&(lo, _, prefix, kind, start, width)) = db::NAME_RANGES.get(k).filter(|r| r.0 <= cp)
    {
        return Some(if kind == 0 {
            format!("{prefix}{cp:04X}")
        } else {
            format!("{prefix}{:0w$}", start + (cp - lo), w = width as usize)
        });
    }
    let (mut lo, mut hi) = (0, db::NAME_CODES.len);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let c = db::NAME_CODES.get(mid);
        if c == cp {
            return Some(stored_name(mid));
        }
        if c < cp {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    None
}

/// The longest `table` entry `s` starts with (CPython's `find_syllable`).
fn syllable(s: &str, table: &[&str]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for (i, part) in table.iter().enumerate() {
        if s.starts_with(part) && best.is_none_or(|(_, len)| part.len() > len) {
            best = Some((i, part.len()));
        }
    }
    best
}

fn hangul_lookup(rest: &str) -> Option<u32> {
    let (l, n) = syllable(rest, &JAMO_L)?;
    let rest = &rest[n..];
    let (v, n) = syllable(rest, &JAMO_V)?;
    let rest = &rest[n..];
    let (t, n) = syllable(rest, &JAMO_T)?;
    (n == rest.len()).then(|| S_BASE + (l as u32 * V_COUNT + v as u32) * T_COUNT + t as u32)
}

fn is_unified_ideograph(cp: u32) -> bool {
    db::NAME_RANGES
        .iter()
        .any(|&(lo, hi, prefix, ..)| prefix == CJK_PREFIX && (lo..=hi).contains(&cp))
}

/// The code point(s) named `name`. Hangul syllables and CJK unified ideographs match only in
/// upper case, other names in any case. `Current` also resolves aliases, and named sequences
/// when `named_sequences` is set.
pub fn lookup(name: &str, version: Version, named_sequences: bool) -> Option<Vec<u32>> {
    if let Some(rest) = name.strip_prefix(HANGUL_PREFIX) {
        return hangul_lookup(rest).map(|c| vec![c]);
    }
    if let Some(hex) = name.strip_prefix(CJK_PREFIX) {
        if !(4..=5).contains(&hex.len())
            || !hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
        {
            return None;
        }
        let cp = u32::from_str_radix(hex, 16).ok()?;
        return is_unified_ideograph(cp).then(|| vec![cp]);
    }
    let up = name.to_ascii_uppercase();
    let found = lookup_upper(&up, version, named_sequences)?;
    if version == Version::V3_2_0 && found.len() == 1 && old_unassigned(found[0]) {
        return None;
    }
    Some(found)
}

fn lookup_upper(up: &str, version: Version, named_sequences: bool) -> Option<Vec<u32>> {
    for &(lo, hi, prefix, kind, start, width) in db::NAME_RANGES {
        if prefix == CJK_PREFIX {
            continue;
        }
        let Some(rest) = up.strip_prefix(prefix) else {
            continue;
        };
        let cp = if kind == 0 {
            let cp = u32::from_str_radix(rest, 16).ok()?;
            (format!("{cp:04X}") == rest).then_some(cp)
        } else {
            let n: u32 = rest.parse().ok()?;
            (rest.len() == width as usize && rest.bytes().all(|b| b.is_ascii_digit()) && n >= start)
                .then(|| lo + (n - start))
        };
        if let Some(cp) = cp.filter(|cp| (lo..=hi).contains(cp)) {
            return Some(vec![cp]);
        }
    }
    let (mut lo, mut hi) = (0, db::NAMES_BY_NAME.len);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let i = db::NAMES_BY_NAME.get(mid) as usize;
        match stored_name(i).as_str().cmp(up) {
            std::cmp::Ordering::Equal => return Some(vec![db::NAME_CODES.get(i)]),
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
        }
    }
    if version == Version::V3_2_0 {
        return None;
    }
    if let Ok(k) = db::ALIASES.binary_search_by(|(n, _)| (*n).cmp(up)) {
        return Some(vec![db::ALIASES[k].1]);
    }
    if named_sequences {
        if let Ok(k) = db::NAMED_SEQUENCES.binary_search_by(|(n, _)| (*n).cmp(up)) {
            return Some(db::NAMED_SEQUENCES[k].1.to_vec());
        }
    }
    None
}

// ---- character types (`str` methods) ----------------------------------------------------------

/// The [`CharType`] flags, as CPython's `_PyUnicode_TypeRecord` has them.
pub mod flag {
    pub const ALPHA: u16 = 1 << 0;
    pub const DECIMAL: u16 = 1 << 1;
    pub const DIGIT: u16 = 1 << 2;
    pub const NUMERIC: u16 = 1 << 3;
    pub const SPACE: u16 = 1 << 4;
    pub const PRINTABLE: u16 = 1 << 5;
    /// The derived `Lowercase` property.
    pub const LOWER: u16 = 1 << 6;
    /// The derived `Uppercase` property.
    pub const UPPER: u16 = 1 << 7;
    /// General category Lt.
    pub const TITLE: u16 = 1 << 8;
    pub const CASED: u16 = 1 << 9;
    pub const CASE_IGNORABLE: u16 = 1 << 10;
    pub const XID_START: u16 = 1 << 11;
    pub const XID_CONTINUE: u16 = 1 << 12;
}

/// What `str` methods know about one code point: predicate flags and full case mappings.
#[derive(Clone, Copy, Debug)]
pub struct CharType {
    cp: u32,
    pub flags: u16,
    upper: i32,
    lower: i32,
    title: i32,
    fold: i32,
}

/// The type of `cp` (unassigned beyond U+10FFFF).
#[inline]
pub fn char_type(cp: u32) -> CharType {
    let k = if cp >= 0x110000 {
        0
    } else {
        let block = db::TYPE_INDEX1.get((cp >> db::SHIFT) as usize) as usize;
        db::TYPE_INDEX2.get((block << db::SHIFT) + (cp & ((1 << db::SHIFT) - 1)) as usize) as usize
    };
    let (flags, upper, lower, title, fold) = db::TYPE_RECORDS[k];
    CharType {
        cp,
        flags,
        upper,
        lower,
        title,
        fold,
    }
}

impl CharType {
    /// The code point this is the type of.
    #[inline]
    pub fn cp(&self) -> u32 {
        self.cp
    }

    #[inline]
    pub fn is(&self, flag: u16) -> bool {
        self.flags & flag != 0
    }

    fn mapping(&self, m: i32) -> CaseMapping {
        if m >= db::EXTENDED {
            let at = (m - db::EXTENDED) as usize;
            CaseMapping {
                one: None,
                at: at + 1,
                end: at + 1 + db::TYPE_EXT.get(at) as usize,
            }
        } else {
            CaseMapping {
                one: Some((self.cp as i32 + m) as u32),
                at: 0,
                end: 0,
            }
        }
    }

    pub fn upper(&self) -> CaseMapping {
        self.mapping(self.upper)
    }

    pub fn lower(&self) -> CaseMapping {
        self.mapping(self.lower)
    }

    pub fn title(&self) -> CaseMapping {
        self.mapping(self.title)
    }

    pub fn fold(&self) -> CaseMapping {
        self.mapping(self.fold)
    }
}

/// The code points a character maps to under a case mapping (one to three).
#[derive(Clone, Debug)]
pub struct CaseMapping {
    one: Option<u32>,
    at: usize,
    end: usize,
}

impl CaseMapping {
    /// The mapping to `c` alone.
    pub fn single(c: u32) -> CaseMapping {
        CaseMapping {
            one: Some(c),
            at: 0,
            end: 0,
        }
    }
}

impl Iterator for CaseMapping {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        if let Some(c) = self.one.take() {
            return Some(c);
        }
        if self.at < self.end {
            self.at += 1;
            return Some(db::TYPE_EXT.get(self.at - 1));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn properties() {
        let p = props('9' as u32, Version::Current);
        assert_eq!(
            (p.category, p.bidirectional, p.decimal, p.digit, p.numeric),
            ("Nd", "EN", Some(9), Some(9), Some(9.0))
        );
        let p = props(0x2468, Version::Current);
        assert_eq!((p.decimal, p.digit, p.numeric), (None, Some(9), Some(9.0)));
        assert_eq!(props(0x5341, Version::Current).numeric, Some(10.0));
        assert_eq!(props(0x300, Version::Current).combining, 230);
        assert_eq!(props(0xff21, Version::Current).east_asian_width, "F");
        assert!(props('(' as u32, Version::Current).mirrored);
        assert_eq!(props(0x1f600, Version::V3_2_0).category, "Cn");
        assert_eq!(props(0x1f600, Version::Current).category, "So");
    }

    #[test]
    fn char_types() {
        let t = |c: char| char_type(c as u32);
        let s = |m: CaseMapping| m.map(|c| char::from_u32(c).unwrap()).collect::<String>();
        assert!(t('a').is(flag::ALPHA | flag::LOWER) && !t('a').is(flag::UPPER));
        assert!(
            t('\u{1c5}').is(flag::TITLE)
                && t('\u{2168}').is(flag::NUMERIC)
                && !t('\u{2168}').is(flag::DIGIT)
        );
        assert!(
            t('\u{2460}').is(flag::DIGIT)
                && !t('\u{2460}').is(flag::DECIMAL)
                && t('\u{663}').is(flag::DECIMAL)
        );
        assert!(
            t('\u{1c}').is(flag::SPACE)
                && t('\u{a0}').is(flag::SPACE)
                && !t('\u{a0}').is(flag::PRINTABLE)
        );
        assert!(
            t('\'').is(flag::CASE_IGNORABLE) && t('\u{2b0}').is(flag::CASED | flag::CASE_IGNORABLE)
        );
        assert_eq!(s(t('\u{df}').upper()), "SS");
        assert_eq!(s(t('\u{df}').title()), "Ss");
        assert_eq!(s(t('\u{df}').fold()), "ss");
        assert_eq!(s(t('\u{130}').lower()), "i\u{307}");
        assert_eq!(s(t('\u{1c6}').title()), "\u{1c5}");
        assert_eq!(s(t('\u{3a3}').lower()), "\u{3c3}");
        assert_eq!(s(t('\u{1e9e}').fold()), "ss");
        assert_eq!(char_type(0x110000).upper().collect::<Vec<_>>(), [0x110000]);
    }

    #[test]
    fn decompositions() {
        assert_eq!(decomposition(0xe9, Version::Current), "0065 0301");
        assert_eq!(
            decomposition(0xbc, Version::Current),
            "<fraction> 0031 2044 0034"
        );
        assert_eq!(decomposition('a' as u32, Version::Current), "");
    }

    #[test]
    fn names() {
        assert_eq!(
            name('a' as u32, Version::Current).as_deref(),
            Some("LATIN SMALL LETTER A")
        );
        assert_eq!(
            name(0xf60, Version::Current).as_deref(),
            Some("TIBETAN LETTER -A")
        );
        assert_eq!(
            name(0xac00, Version::Current).as_deref(),
            Some("HANGUL SYLLABLE GA")
        );
        assert_eq!(
            name(0xd7a3, Version::Current).as_deref(),
            Some("HANGUL SYLLABLE HIH")
        );
        assert_eq!(
            name(0x4e00, Version::Current).as_deref(),
            Some("CJK UNIFIED IDEOGRAPH-4E00")
        );
        assert_eq!(
            name(0x18800, Version::Current).as_deref(),
            Some("TANGUT COMPONENT-001")
        );
        assert_eq!(name(0, Version::Current), None);
        assert_eq!(
            lookup("latin small letter a", Version::Current, false),
            Some(vec![0x61])
        );
        assert_eq!(
            lookup("HANGUL SYLLABLE GAG", Version::Current, false),
            Some(vec![0xac01])
        );
        assert_eq!(
            lookup("CJK UNIFIED IDEOGRAPH-4E00", Version::Current, false),
            Some(vec![0x4e00])
        );
        assert_eq!(
            lookup("cjk unified ideograph-4e00", Version::Current, false),
            None
        );
        assert_eq!(
            lookup("TANGUT COMPONENT-002", Version::Current, false),
            Some(vec![0x18801])
        );
        assert_eq!(lookup("NULL", Version::Current, false), Some(vec![0]));
        assert_eq!(lookup("NULL", Version::V3_2_0, false), None);
        assert_eq!(
            lookup("LATIN SMALL LETTER R WITH TILDE", Version::Current, false),
            None
        );
        assert_eq!(
            lookup("LATIN SMALL LETTER R WITH TILDE", Version::Current, true),
            Some(vec![0x72, 0x303])
        );
    }

    #[test]
    fn normalization() {
        assert_eq!(
            normalize(&[0x65, 0x301], "NFC", Version::Current),
            vec![0xe9]
        );
        assert_eq!(
            normalize(&[0xfb01], "NFKD", Version::Current),
            vec![0x66, 0x69]
        );
        assert_eq!(
            normalize(&[0xac01], "NFD", Version::Current),
            vec![0x1100, 0x1161, 0x11a8]
        );
    }
}
