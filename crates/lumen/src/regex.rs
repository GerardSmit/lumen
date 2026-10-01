//! The engine's regular-expression layer: ECMAScript patterns compiled by the shared core in
//! `lumen_common::regex`, and the subject views the interpreter matches them over.

use lumen_common::regex::js::{self, elem_of_cp};
use lumen_common::regex::{self as core, ExecOptions};
pub use lumen_common::regex::{Abort, BacktrackLimit, Captures, BACKTRACK_LIMIT_MSG};
pub(crate) use lumen_common::regex::{set_host_poll, take_abort};

/// A compiled ECMAScript regular expression.
pub struct Regex {
    core: core::Regex,
    pub unicode: bool,
    pub source: String,
    pub flags: String,
    pub global: bool,
    pub sticky: bool,
}

impl std::ops::Deref for Regex {
    type Target = core::Regex;
    fn deref(&self) -> &core::Regex {
        &self.core
    }
}

impl Regex {
    pub fn new(pattern: &str, flags: &str) -> Result<Regex, String> {
        let f = js::Flags::parse(flags)?;
        let elems = pattern_elements(f.unicode, pattern);
        Ok(Regex {
            core: js::compile(elems, &f)?,
            unicode: f.unicode,
            source: if pattern.is_empty() {
                "(?:)".into()
            } else {
                pattern.to_string()
            },
            global: f.global,
            sticky: f.sticky,
            flags: f.canonical,
        })
    }

    fn exec_options(&self, start: usize) -> ExecOptions {
        if self.sticky {
            ExecOptions::anchored(start)
        } else {
            ExecOptions::search(start)
        }
    }

    /// Match a prepared subject and return shared capture spans.
    pub fn exec_text_shared(
        &self,
        text: &ReText,
        start: usize,
    ) -> Result<Option<Captures>, BacktrackLimit> {
        let opts = self.exec_options(start);
        match &text.ascii_src {
            Some(s) => self.core.exec(s.as_bytes(), opts),
            None => self.core.exec(&text.elems[..], opts),
        }
    }

    /// Match for a caller that only observes matcher side effects.
    pub fn exec_text_discard_shared(
        &self,
        text: &ReText,
        start: usize,
    ) -> Result<Option<Captures>, BacktrackLimit> {
        self.exec_text_shared(text, start)
    }

    /// Whole-match-only search for operations whose JavaScript result is dead. Capture groups
    /// can be recovered lazily if a legacy RegExp static is subsequently observed.
    pub(crate) fn find_text_shared(
        &self,
        text: &ReText,
        start: usize,
    ) -> Result<Option<(usize, usize)>, BacktrackLimit> {
        Ok(self
            .exec_text_shared(text, start)?
            .and_then(|captures| captures[0]))
    }
}

/// The element sequence regular expressions operate over. In unicode (`u`/`v`) mode an element
/// is a code point; otherwise it is a UTF-16 code unit. Surrogate units/code points are carried
/// as their jstr-smuggled plane-16 scalars so every element is a valid `char` — an astral
/// character in a non-unicode pattern or subject is therefore TWO elements (its two halves).
pub fn pattern_elements(unicode: bool, s: &str) -> Vec<char> {
    if unicode {
        crate::jstr::code_points(s)
            .into_iter()
            .map(elem_of_cp)
            .collect()
    } else {
        crate::jstr::units(s)
            .into_iter()
            .map(|u| {
                if (0xD800..0xE000).contains(&(u as u32)) {
                    crate::jstr::smuggle(u)
                } else {
                    char::from_u32(u as u32).unwrap()
                }
            })
            .collect()
    }
}

/// A subject string prepared for matching: its elements plus each element's unit offset.
/// `unit_of` is `None` when element index == unit offset (always true in non-unicode mode, and in
/// unicode mode for BMP-only subjects); otherwise `unit_of.len() == elems.len() + 1` with the last
/// entry the total unit length. JS-visible indices (lastIndex, match.index) are unit offsets.
pub struct ReText {
    /// Wide elements — EMPTY for an ASCII subject, which matches over `ascii_src`'s bytes
    /// directly (see `Regex::exec_text`) with no per-element materialization at all.
    pub elems: Vec<u32>,
    pub unit_of: Option<Vec<usize>>,
    /// Element count (== `ascii_src` byte length for ASCII, else `elems.len()`).
    n_elems: usize,
    unicode: bool,
    /// The source string when it is pure ASCII (element index == byte index): matching runs
    /// over its bytes and `slice` copies straight out of it.
    ascii_src: Option<crate::lstr::LStr>,
}

impl ReText {
    /// Heap bytes this view keeps alive beyond the subject string itself.
    pub fn heap_bytes(&self) -> usize {
        self.elems.capacity() * 4 + self.unit_of.as_ref().map_or(0, |u| u.capacity() * 8)
    }

    /// Prepare `s` for matching, keeping the caller's `Rc` for zero-copy ASCII slicing.
    pub fn new_rc(unicode: bool, s: &crate::lstr::LStr) -> ReText {
        // Engine strings maintain an exact one-way ASCII hint in their allocation header.
        // RegExp workloads commonly stream many distinct ASCII subjects through the tiny
        // identity cache; consulting the hint avoids rescanning every subject just to select
        // the byte matcher.
        if s.ascii_hint() {
            return ReText {
                elems: Vec::new(),
                unit_of: None,
                n_elems: s.len(),
                unicode,
                ascii_src: Some(s.clone()),
            };
        }
        // Keep the engine string itself: `LStr::clone` is one refcount bump and its immutable
        // bytes can be matched and sliced directly.
        Self::build(unicode, s, Some(s.clone()))
    }

    fn build(unicode: bool, s: &str, src: Option<crate::lstr::LStr>) -> ReText {
        // ASCII: elements are the bytes, and element index == unit offset in both modes.
        if s.is_ascii() {
            return ReText {
                elems: Vec::new(),
                unit_of: None,
                n_elems: s.len(),
                unicode,
                ascii_src: Some(src.unwrap_or_else(|| crate::lstr::LStr::from(s))),
            };
        }
        if unicode {
            let cps = crate::jstr::code_points(s);
            if cps.iter().all(|&cp| cp < 0x10000) {
                // BMP-only: one unit per element.
                return ReText {
                    n_elems: cps.len(),
                    elems: cps,
                    unit_of: None,
                    unicode,
                    ascii_src: None,
                };
            }
            let mut unit_of = Vec::with_capacity(cps.len() + 1);
            let mut u = 0usize;
            for &cp in &cps {
                unit_of.push(u);
                u += if cp >= 0x10000 { 2 } else { 1 };
            }
            unit_of.push(u);
            ReText {
                n_elems: cps.len(),
                elems: cps,
                unit_of: Some(unit_of),
                unicode,
                ascii_src: None,
            }
        } else {
            let units = crate::jstr::units(s);
            ReText {
                n_elems: units.len(),
                elems: units.iter().map(|&u| u as u32).collect(),
                unit_of: None,
                unicode,
                ascii_src: None,
            }
        }
    }

    /// The element index containing unit offset `u` (== len when `u` is at/past the end).
    pub fn elem_at_unit(&self, u: usize) -> usize {
        match &self.unit_of {
            None => u.min(self.n_elems),
            Some(unit_of) => match unit_of.binary_search(&u) {
                Ok(k) => k.min(self.n_elems),
                Err(k) => k - 1,
            },
        }
    }

    /// The unit offset of element `e`.
    pub fn unit_index(&self, e: usize) -> usize {
        match &self.unit_of {
            None => e.min(self.n_elems),
            Some(unit_of) => unit_of[e.min(self.n_elems)],
        }
    }

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.n_elems
    }

    /// The canonical string for elements `a..b` (surrogate halves recombine).
    /// [`ReText::slice`] as an engine string: a view of an ASCII subject (see `LStr::sub`).
    pub fn slice_str(&self, a: usize, b: usize) -> crate::lstr::LStr {
        match &self.ascii_src {
            Some(src) => src.sub(&src[a..b]),
            None => self.slice(a, b).into(),
        }
    }

    pub fn slice(&self, a: usize, b: usize) -> String {
        // ASCII subject: element index == byte index — copy straight from the source.
        if let Some(src) = &self.ascii_src {
            return src[a..b].to_string();
        }
        let elems = &self.elems[a..b];
        // ASCII fast path: elements are the bytes.
        if elems.iter().all(|&e| e < 0x80) {
            let bytes: Vec<u8> = elems.iter().map(|&e| e as u8).collect();
            return String::from_utf8(bytes).unwrap();
        }
        if self.unicode {
            crate::jstr::from_code_points(elems)
        } else {
            let units: Vec<u16> = elems.iter().map(|&e| e as u16).collect();
            crate::jstr::from_units(&units)
        }
    }
}

#[cfg(test)]
mod internal_engine_diagnostics {
    #[test]
    fn escaped_open_bracket_patterns_compile() {
        for pattern in [r"\s*([+>~\s])\s*([a-zA-Z#.*:\[])", r"^[\s[]?shapgvba"] {
            super::Regex::new(pattern, "g")
                .unwrap_or_else(|error| panic!("internal matcher rejected {pattern:?}: {error}"));
        }
    }

    #[test]
    fn legacy_identity_escaped_punctuation_compiles() {
        super::Regex::new(r#"(^|[^\\])\"\\\/Qngr\((-?[0-9]+)\)\\\/\""#, "g")
            .expect("internal matcher should accept legacy identity escapes");
    }

    #[test]
    fn guaranteed_ascii_backreference_matches() {
        let re =
            super::Regex::new(r#"^(\[) *@?([\w-]+) *([!*$^~=]*) *('?"?)(.*?)\4 *\]"#, "").unwrap();
        let input = crate::lstr::LStr::from("[glcr=fhozvg]");
        let text = super::ReText::new_rc(false, &input);
        let caps = re.exec_text_shared(&text, 0).unwrap().unwrap();
        assert_eq!(caps[0], Some((0, 13)));
    }

    #[test]
    fn capture_free_ascii_lookahead_matches() {
        let re = super::Regex::new("HF(?=;)", "i").unwrap();
        let input = crate::lstr::LStr::from("xhf;y");
        let text = super::ReText::new_rc(false, &input);
        assert_eq!(re.exec_text_shared(&text, 0).unwrap().unwrap()[0], Some((1, 3)));
    }

    #[test]
    fn legacy_pattern_uses_utf16_element_offsets() {
        let re = super::Regex::new(r"Qngr\((-?[0-9]+)\)", "").unwrap();
        let input = crate::lstr::LStr::from("‰Qngr(-12)");
        let text = super::ReText::new_rc(false, &input);
        let caps = re.exec_text_shared(&text, 0).unwrap().unwrap();
        assert_eq!(caps[0], Some((1, 10)));
        assert_eq!(caps[1], Some((6, 9)));
    }

    #[test]
    fn internal_literal_plan_honors_start_and_sticky() {
        let input = crate::lstr::LStr::from("xxneedle--needle");
        let text = super::ReText::new_rc(false, &input);
        let search = super::Regex::new("needle", "").unwrap();
        assert_eq!(
            search.exec_text_shared(&text, 3).unwrap().unwrap()[0],
            Some((10, 16))
        );

        let sticky = super::Regex::new("needle", "y").unwrap();
        assert!(sticky.exec_text_shared(&text, 3).unwrap().is_none());
        assert_eq!(
            sticky.exec_text_shared(&text, 10).unwrap().unwrap()[0],
            Some((10, 16))
        );
    }

    type Found = Result<Option<Vec<Option<(usize, usize)>>>, super::BacktrackLimit>;

    fn try_find(pattern: &str, flags: &str, subject: &str) -> Found {
        let re = super::Regex::new(pattern, flags).unwrap();
        let input = crate::lstr::LStr::from(subject);
        let text = super::ReText::new_rc(false, &input);
        Ok(re.exec_text_shared(&text, 0)?.map(|c| c.to_vec()))
    }

    fn find(pattern: &str, flags: &str, subject: &str) -> Option<Vec<Option<(usize, usize)>>> {
        try_find(pattern, flags, subject).unwrap()
    }

    fn assert_limit(pattern: &str, subject: &str) {
        let t = std::time::Instant::now();
        assert_eq!(try_find(pattern, "", subject), Err(super::BacktrackLimit), "{pattern}");
        assert!(t.elapsed().as_secs() < 5, "{pattern} took {:?}", t.elapsed());
    }

    #[test]
    fn catastrophic_backtracking_hits_the_limit() {
        assert_limit("(a+)+b", &"a".repeat(40));
        assert_limit("(x+x+)+y", &"x".repeat(5000));
        assert_limit("(a|aa)*b", &"a".repeat(100));
        assert_limit("(?=(a+)+b)", &"a".repeat(40));
        assert_limit("^(\\w+\\s?)*$", &("a".repeat(30) + "!"));
        assert_limit("(a*)*\\1b", &"a".repeat(30));
        assert_limit("a*a*a*a*b", &"a".repeat(3000));
    }

    #[test]
    fn long_linear_matches_stay_under_the_limit() {
        let subject = "ab".repeat(500_000) + "c";
        assert_eq!(find("(a|b)*c", "", &subject).unwrap()[0], Some((0, subject.len())));
        let words = "lorem ipsum, dolor sit amet. ".repeat(40_000);
        let mut pos = 0;
        let re = super::Regex::new("\\w+", "g").unwrap();
        let input = crate::lstr::LStr::from(words.as_str());
        let text = super::ReText::new_rc(false, &input);
        let mut n = 0;
        while let Some(c) = re.exec_text_shared(&text, pos).unwrap() {
            pos = c[0].unwrap().1;
            n += 1;
        }
        assert_eq!(n, 200_000);
        let tail = "x".repeat(1_000_000);
        assert!(find(".*y", "", &tail).is_none());
        assert!(find("[^\"]*\"", "", &tail).is_none());
    }

    #[test]
    fn long_star_of_alternation_matches_at_first_position() {
        let subject = "ab".repeat(50_000) + "c";
        let caps = find("(a|b)*c", "", &subject).unwrap();
        assert_eq!(caps[0], Some((0, subject.len())));
        assert_eq!(caps[1], Some((subject.len() - 2, subject.len() - 1)));
    }

    #[test]
    fn lookaround_failures_and_capture_rollback() {
        assert!(find("(?<=q)z", "", "xyzwxyzw").is_none());
        assert!(find("(?=q)z", "", "xyzw").is_none());
        assert_eq!(find("(?!x)z", "", "xz").unwrap()[0], Some((1, 2)));
        assert!(find("(?!z)z", "", "zz").is_none());
        let caps = find("(?=(a+))a*b\\1", "", "baaabac").unwrap();
        assert_eq!(caps[0], Some((3, 6)));
        assert_eq!(caps[1], Some((3, 4)));
        let caps = find("(?=(\\d+))\\w+x|\\w", "", "12y").unwrap();
        assert_eq!(caps[0], Some((0, 1)));
        assert_eq!(caps[1], None);
        let caps = find("(a)|b", "", "b").unwrap();
        assert_eq!(caps[1], None);
        let caps = find("(z)((a+)?(b+)?(c))*", "", "zaacbbbcac").unwrap();
        assert_eq!(caps[0], Some((0, 10)));
        assert_eq!(caps[2], Some((8, 10)));
        assert_eq!(caps[3], Some((8, 9)));
        assert_eq!(caps[4], None);
        assert_eq!(caps[5], Some((9, 10)));
    }

    #[test]
    fn lazy_run_does_not_scan_whole_subject() {
        let subject = "a1".to_string() + &"x".repeat(200_000);
        assert_eq!(find("a.*?\\d", "g", &subject).unwrap()[0], Some((0, 2)));
        assert_eq!(find("a.*?(\\d)y|a", "", "a1x2y").unwrap()[0], Some((0, 5)));
    }
}
