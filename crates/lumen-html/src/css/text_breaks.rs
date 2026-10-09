//! CSS tailoring over the shared Unicode line and grapheme iterators.
//! Normal text keeps the existing allocation-free UAX #14 path. Emergency
//! wrapping is separate from ordinary opportunities so words move to a new
//! line before they are split, and break-word does not shrink min-content.
use super::{OverflowWrap, Style, WhiteSpace, WordBreak};
use lumen_common::ucd::{self, BreakClass, BreakOpportunity};
use lumen_common::bidi::{Script, UnicodeScript};

impl Style {
    pub fn emergency_wrap(&self) -> bool {
        !matches!(self.white_space, WhiteSpace::NoWrap | WhiteSpace::Pre)
            && (self.word_break == WordBreak::BreakWord
                || self.overflow_wrap != OverflowWrap::Normal)
    }

    pub fn text_line_breaks<'a>(
        &self,
        text: &'a str,
        min_content: bool,
    ) -> impl Iterator<Item = (usize, BreakOpportunity)> + 'a {
        opportunities(text, self.word_break, self.overflow_wrap, min_content)
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
    opportunities_with(
        text,
        min_content,
        needs_tailoring(word_break, overflow_wrap, min_content),
        move |_| (word_break, overflow_wrap),
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

/// The policy callback selects the containing inline style for each unit.
/// Untailored paragraphs never invoke it or traverse the grapheme iterator.
pub fn opportunities_with<'a>(
    text: &'a str,
    min_content: bool,
    tailored: bool,
    mut policy: impl FnMut(usize) -> (WordBreak, OverflowWrap) + 'a,
) -> impl Iterator<Item = (usize, BreakOpportunity)> + 'a {
    // AutoPhrase deliberately stays on the Normal opportunity path: this UA
    // has no language-specific phrase detector, for which CSS Text requires
    // the normal fallback.
    let tailored = tailored || requires_lexical_fallback(text);
    let mut ordinary = ucd::line_breaks(text).peekable();
    let mut clusters = ucd::graphemes(text).peekable();
    core::iter::from_fn(move || {
        if !tailored {
            return ordinary.next();
        }
        loop {
            let (start, cluster) = clusters.next()?;
            let end = start + cluster.len();
            let (word_break, overflow_wrap) = policy(start);
            let intrinsic_anywhere = min_content
                && (overflow_wrap == OverflowWrap::Anywhere || word_break == WordBreak::BreakWord);
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
