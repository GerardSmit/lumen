//! CSS tailoring over the shared Unicode line and grapheme iterators.
//! Normal text keeps the existing allocation-free UAX #14 path. Emergency
//! wrapping is separate from ordinary opportunities so words move to a new
//! line before they are split, and break-word does not shrink min-content.
use super::{OverflowWrap, Style, WhiteSpace, WordBreak};
use lumen_common::ucd::{self, BreakClass, BreakOpportunity};
use lumen_common::bidi::{Script, UnicodeScript};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineBreak {
    #[default]
    Auto,
    Loose,
    Normal,
    Strict,
    Anywhere,
}

/// Per-unit line breaking policy: the cascaded properties of the containing
/// inline box plus whether its language is Japanese or Chinese.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BreakPolicy {
    pub word_break: WordBreak,
    pub overflow_wrap: OverflowWrap,
    pub line_break: LineBreak,
    pub east_asian: bool,
}

pub fn east_asian_language(tag: &str) -> bool {
    let primary = tag.split('-').next().unwrap_or("");
    primary.eq_ignore_ascii_case("ja") || primary.eq_ignore_ascii_case("zh")
}

impl Style {
    pub fn emergency_wrap(&self) -> bool {
        !matches!(self.white_space, WhiteSpace::NoWrap | WhiteSpace::Pre)
            && (self.word_break == WordBreak::BreakWord
                || self.overflow_wrap != OverflowWrap::Normal)
    }

    pub fn line_break(&self) -> LineBreak {
        match super::extra_props::keyword(self, "line-break") {
            Some("loose") => LineBreak::Loose,
            Some("normal") => LineBreak::Normal,
            Some("strict") => LineBreak::Strict,
            Some("anywhere") => LineBreak::Anywhere,
            _ => LineBreak::Auto,
        }
    }

    pub fn break_policy(&self, east_asian: bool) -> BreakPolicy {
        BreakPolicy {
            word_break: self.word_break,
            overflow_wrap: self.overflow_wrap,
            line_break: self.line_break(),
            east_asian,
        }
    }

    pub fn needs_break_tailoring(&self, min_content: bool) -> bool {
        needs_tailoring(self.word_break, self.overflow_wrap, min_content)
            || self.line_break() == LineBreak::Anywhere
    }

    pub fn text_line_breaks<'a>(
        &self,
        text: &'a str,
        min_content: bool,
        east_asian: bool,
    ) -> impl Iterator<Item = (usize, BreakOpportunity)> + 'a {
        let policy = self.break_policy(east_asian);
        opportunities_with(text, min_content, self.needs_break_tailoring(min_content), move |_| policy)
    }
}

fn letter_unit(cluster: &str) -> bool {
    cluster.chars().next().is_some_and(|ch| {
        ch.is_alphabetic()
            || matches!(
                ucd::break_property(ch as u32),
                BreakClass::Numeric
                    | BreakClass::Alphabetic
                    | BreakClass::Ambiguous
                    | BreakClass::Ideographic
            )
    })
}

pub fn opportunities(
    text: &str,
    word_break: WordBreak,
    overflow_wrap: OverflowWrap,
    min_content: bool,
) -> impl Iterator<Item = (usize, BreakOpportunity)> + '_ {
    let policy = BreakPolicy { word_break, overflow_wrap, line_break: LineBreak::Strict, east_asian: false };
    opportunities_with(
        text,
        min_content,
        needs_tailoring(word_break, overflow_wrap, min_content),
        move |_| policy,
    )
}

pub fn needs_tailoring(
    word_break: WordBreak,
    overflow_wrap: OverflowWrap,
    min_content: bool,
) -> bool {
    matches!(
        word_break,
        WordBreak::BreakAll | WordBreak::KeepAll | WordBreak::Manual
    ) || (min_content
        && (overflow_wrap == OverflowWrap::Anywhere || word_break == WordBreak::BreakWord))
}

fn complex_context_unit(cluster: &str) -> bool {
    cluster
        .chars()
        .find(|ch| {
            !matches!(
                ucd::break_property(*ch as u32),
                BreakClass::CombiningMark | BreakClass::ZeroWidthJoiner
            )
        })
        .is_some_and(|ch| ucd::break_property(ch as u32) == BreakClass::ComplexContext)
}

// CSS Text §5.5 requires letter-unit fallback when the UA lacks the
// lexical/orthographic analyzer. SA covers dictionary-dependent scripts;
// Javanese and Balinese also require syllable analysis despite their AL class.
// Reuse the common Unicode providers, with no private range table or allocation.
fn lexical_character(ch: char) -> bool {
    !ch.is_ascii() && ch.is_alphabetic()
        && (ucd::break_property(ch as u32) == BreakClass::ComplexContext
            || matches!(ch.script(), Script::Javanese | Script::Balinese))
}

pub fn requires_lexical_fallback(text: &str) -> bool {
    !text.is_ascii() && text.chars().any(lexical_character)
}

fn lexical_unit(cluster: &str) -> bool {
    cluster.chars().find(|ch| !matches!(ucd::break_property(*ch as u32),
        BreakClass::CombiningMark | BreakClass::ZeroWidthJoiner))
        .is_some_and(lexical_character)
}

const CJ_EXTRA: [char; 2] = ['\u{30fc}', '\u{ff70}'];
const ITERATION_MARKS: [char; 6] = ['\u{3005}', '\u{303b}', '\u{309d}', '\u{309e}', '\u{30fd}', '\u{30fe}'];
const HYPHENS: [char; 4] = ['\u{2010}', '\u{2013}', '\u{301c}', '\u{30a0}'];
const CENTERED_PUNCTUATION: [char; 10] = [
    '\u{30fb}', '\u{ff1a}', '\u{ff1b}', '\u{ff65}', '\u{203c}', '\u{2047}', '\u{2048}', '\u{2049}',
    '\u{ff01}', '\u{ff1f}',
];

fn wide_east_asian_width(ch: char) -> bool {
    matches!(ucd::props(ch as u32, ucd::Version::Current).east_asian_width, "A" | "F" | "W")
}

fn line_break_tailorable(ch: char) -> bool {
    if ch.is_ascii() {
        return false;
    }
    match ucd::break_property(ch as u32) {
        BreakClass::ConditionalJapaneseStarter | BreakClass::Inseparable | BreakClass::Prefix
        | BreakClass::Postfix => true,
        _ => {
            CJ_EXTRA.contains(&ch)
                || ITERATION_MARKS.contains(&ch)
                || HYPHENS.contains(&ch)
                || CENTERED_PUNCTUATION.contains(&ch)
        }
    }
}

pub fn requires_line_break_tailoring(text: &str) -> bool {
    !text.is_ascii() && text.chars().any(line_break_tailorable)
}

/// Whether UAX #14 would allow a break between `left` and an ideograph, which
/// is how the tailored classes below behave after `left`.
fn ideograph_break_after(left: char) -> bool {
    let mut buffer = [0u8; 8];
    let left_len = left.len_utf8();
    left.encode_utf8(&mut buffer);
    '\u{6587}'.encode_utf8(&mut buffer[left_len..]);
    let probe = core::str::from_utf8(&buffer[..left_len + 3]).unwrap_or("");
    let allowed = ucd::line_breaks(probe).any(|(at, kind)| at == left_len && kind == BreakOpportunity::Allowed);
    allowed
}

/// CSS Text §5.1 `line-break` tailoring of the boundary between `left` and
/// `right`: `Some(true)` allows a break that UAX #14 prohibits, `Some(false)`
/// prohibits one it allows.
fn line_break_tailoring(mode: LineBreak, east_asian: bool, left: char, right: char) -> Option<bool> {
    let relaxed = matches!(mode, LineBreak::Auto | LineBreak::Normal | LineBreak::Loose);
    if !relaxed && !east_asian {
        return None;
    }
    let loose = mode == LineBreak::Loose;
    let class = ucd::break_property(right as u32);
    let small_kana = class == BreakClass::ConditionalJapaneseStarter || CJ_EXTRA.contains(&right);
    let allow = if small_kana {
        relaxed
    } else if ITERATION_MARKS.contains(&right) || class == BreakClass::Inseparable {
        loose
    } else if HYPHENS.contains(&right) {
        east_asian && (loose || matches!(right, '\u{301c}' | '\u{30a0}') && relaxed)
    } else if CENTERED_PUNCTUATION.contains(&right) {
        east_asian && loose
    } else if class == BreakClass::Postfix && wide_east_asian_width(right) {
        east_asian && loose
    } else if class == BreakClass::Prefix && wide_east_asian_width(right) {
        if east_asian && !loose && ucd::break_property(left as u32) == BreakClass::Ideographic {
            return Some(false);
        }
        false
    } else {
        false
    };
    (allow && ideograph_break_after(left)).then_some(true)
}

/// The policy callback selects the containing inline style for each unit.
/// Untailored paragraphs never invoke it or traverse the grapheme iterator.
pub fn opportunities_with<'a>(
    text: &'a str,
    min_content: bool,
    tailored: bool,
    mut policy: impl FnMut(usize) -> BreakPolicy + 'a,
) -> impl Iterator<Item = (usize, BreakOpportunity)> + 'a {
    // AutoPhrase deliberately stays on the Normal opportunity path: this UA
    // has no language-specific phrase detector, for which CSS Text requires
    // the normal fallback.
    let tailored = tailored || requires_lexical_fallback(text) || requires_line_break_tailoring(text);
    let mut ordinary = ucd::line_breaks(text).peekable();
    let mut clusters = ucd::graphemes(text).peekable();
    core::iter::from_fn(move || {
        if !tailored {
            return ordinary.next();
        }
        loop {
            let (start, cluster) = clusters.next()?;
            let end = start + cluster.len();
            let BreakPolicy { word_break, overflow_wrap, line_break, east_asian } = policy(start);
            let intrinsic_anywhere = (min_content
                && (overflow_wrap == OverflowWrap::Anywhere || word_break == WordBreak::BreakWord))
                || line_break == LineBreak::Anywhere;
            let mut kind = None;
            while ordinary.peek().is_some_and(|&(offset, _)| offset <= end) {
                let (offset, opportunity) = ordinary.next()?;
                if offset == end {
                    kind = Some(opportunity);
                }
            }
            if kind == Some(BreakOpportunity::Mandatory) {
                return Some((end, BreakOpportunity::Mandatory));
            }
            if let (Some(left), Some((_, right))) = (cluster.chars().next(), clusters.peek()) {
                if let Some(right) = right.chars().next() {
                    match line_break_tailoring(line_break, east_asian, left, right) {
                        Some(true) if kind.is_none() => kind = Some(BreakOpportunity::Allowed),
                        Some(false) => kind = None,
                        _ => {}
                    }
                }
            }
            // The shared UAX #14 provider resolves SA (ComplexContext) to AL
            // and has no lexical-analysis database. `manual` therefore keeps
            // that fallback and explicitly suppresses any allowed break that
            // lands between adjacent SA typographic units. Explicit separators
            // such as U+200B are separate units and remain unaffected.
            let manual_sa_pair = word_break == WordBreak::Manual
                && complex_context_unit(cluster)
                && clusters
                    .peek()
                    .is_some_and(|(_, right)| complex_context_unit(right));
            if manual_sa_pair && kind == Some(BreakOpportunity::Allowed) {
                kind = None;
            }
            let lexical_pair = !manual_sa_pair && !cluster.ends_with('\u{200d}')
                && lexical_unit(cluster)
                && clusters.peek().is_some_and(|(_, right)| lexical_unit(right));
            if lexical_pair { kind = Some(BreakOpportunity::Allowed); }
            let letters = letter_unit(cluster)
                && clusters.peek().is_some_and(|(_, right)| letter_unit(right));
            if word_break == WordBreak::KeepAll && letters && !lexical_pair && !intrinsic_anywhere {
                kind = None;
            }
            // Explicit joiners still suppress ordinary break-all boundaries.
            // Emergency anywhere may break between intact grapheme clusters.
            if end < text.len()
                && (intrinsic_anywhere
                    || (word_break == WordBreak::BreakAll
                        && letters
                        && !cluster.ends_with('\u{200d}')))
            {
                kind = Some(BreakOpportunity::Allowed);
            }
            if let Some(kind) = kind {
                return Some((end, kind));
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn specification_lexical_fallback_preserves_units_manual_policy_and_joiners() {
        let breaks = |text, word| opportunities(text, word, OverflowWrap::Normal, false)
            .map(|(at, _)| at).collect::<Vec<_>>();
        for text in ["กข", "ກຂ", "កខ", "ကခ", "ꦏꦑ", "ᬓᬔ"] {
            assert_eq!(breaks(text, WordBreak::Normal), [3, 6]);
            assert_eq!(breaks(text, WordBreak::AutoPhrase), [3, 6]);
            assert_eq!(breaks(text, WordBreak::KeepAll), [3, 6], "dictionary fallback survives keep-all: {text}");
        }
        assert_eq!(breaks("ก้ข", WordBreak::Normal), [6, 9]);
        assert_eq!(breaks("กข", WordBreak::Manual), [6]);
        assert_eq!(breaks("ꦏꦑ", WordBreak::Manual), [3, 6], "manual targets SA, not syllable analysis");
        assert_eq!(breaks("ก\u{2060}ข", WordBreak::Normal), [9]);
        assert_eq!(breaks("ก\u{200d}ข", WordBreak::Normal), [9]);
        assert_eq!(breaks("ก\u{200b}ข", WordBreak::Normal), [6, 9]);
        assert_eq!(breaks("กข\nก", WordBreak::Normal), [3, 7, 10]);
        assert_eq!(breaks("latin", WordBreak::Normal), [5]);
        assert_eq!(breaks("abc漢字", WordBreak::KeepAll), [9]);
        assert!(!requires_lexical_fallback("plain Latin text"));
    }

    #[test]
    fn text_break_policy_preserves_clusters_punctuation_and_intrinsic_distinctions() {
        let breaks = |text, word, overflow, intrinsic| {
            opportunities(text, word, overflow, intrinsic)
                .map(|(at, _)| at)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            breaks("abcd", WordBreak::Normal, OverflowWrap::Normal, false),
            [4]
        );
        assert_eq!(
            breaks("abcd", WordBreak::BreakAll, OverflowWrap::Normal, false),
            [1, 2, 3, 4]
        );
        assert_eq!(
            breaks("漢字", WordBreak::KeepAll, OverflowWrap::Normal, false),
            [6]
        );
        assert_eq!(
            breaks(
                "a\u{301}b",
                WordBreak::BreakAll,
                OverflowWrap::Normal,
                false
            ),
            [3, 4]
        );
        assert_eq!(
            breaks("(ab)", WordBreak::BreakAll, OverflowWrap::Normal, false),
            [2, 4]
        );
        assert_eq!(
            breaks("abcd", WordBreak::Normal, OverflowWrap::BreakWord, true),
            [4]
        );
        assert_eq!(
            breaks("abcd", WordBreak::Normal, OverflowWrap::Anywhere, true),
            [1, 2, 3, 4]
        );
        assert_eq!(
            breaks("abcd", WordBreak::BreakWord, OverflowWrap::Normal, true),
            [1, 2, 3, 4]
        );
        let emoji = "👨‍👩‍👧‍👦";
        assert_eq!(
            breaks(emoji, WordBreak::Normal, OverflowWrap::Anywhere, true),
            [emoji.len()]
        );
        assert_eq!(
            breaks("\u{0e01}\u{200b}\u{0e02}", WordBreak::Manual, OverflowWrap::Normal, false),
            [6, 9],
            "manual retains explicit breaks in complex-context text"
        );
        assert_eq!(
            breaks("\u{0e01}\u{0e02}\n\u{0e01}", WordBreak::Manual, OverflowWrap::Normal, false),
            [7, 10],
            "manual retains mandatory breaks"
        );
        assert_eq!(
            breaks("\u{0e01}\u{0e49}\u{0e02}", WordBreak::Manual, OverflowWrap::Anywhere, true),
            [6, 9],
            "intrinsic anywhere preserves SA combining clusters"
        );
        assert_eq!(
            breaks("word \u{0e01}\u{0e02} text", WordBreak::AutoPhrase, OverflowWrap::Normal, false),
            breaks("word \u{0e01}\u{0e02} text", WordBreak::Normal, OverflowWrap::Normal, false),
            "unsupported phrase languages retain normal opportunities"
        );
        let mut style = Style::initial();
        assert!(!style.emergency_wrap());
        assert!(style.extras.is_none());
        style.overflow_wrap = OverflowWrap::BreakWord;
        assert!(style.emergency_wrap());
    }
}
