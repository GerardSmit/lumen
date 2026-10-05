//! Shared, bounded UAX #9 resolution. Shape logical runs in the returned visual order.
pub use unicode_bidi::{bidi_class, BidiClass, BidiInfo, Level, UNICODE_VERSION};
pub use unicode_script::{Script, UnicodeScript};

pub const MAX_TEXT_BYTES: usize = 64 * 1024;

/// Direction inferred from the first strong Unicode bidirectional character.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Directionality {
    Ltr,
    Rtl,
}

/// Return the direction of the first Unicode bidi-class L, R, or AL code point.
///
/// This is the bounded, allocation-free first-strong primitive used by HTML
/// directionality and other language-neutral consumers. Weak, neutral, and
/// formatting characters are skipped according to the pinned Unicode data.
pub fn first_strong_direction(text: &str) -> Option<Directionality> {
    text.chars()
        .find_map(|character| match bidi_class(character) {
            BidiClass::L => Some(Directionality::Ltr),
            BidiClass::R | BidiClass::AL => Some(Directionality::Rtl),
            _ => None,
        })
}

pub fn resolve(text: &str, rtl: Option<bool>) -> Result<BidiInfo<'_>, &'static str> {
    if text.len() > MAX_TEXT_BYTES {
        return Err("text run too large");
    }
    Ok(BidiInfo::new(
        text,
        rtl.map(|rtl| if rtl { Level::rtl() } else { Level::ltr() }),
    ))
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    #[test]
    fn first_strong_direction_uses_pinned_unicode_classes() {
        assert_eq!(first_strong_direction("123 \u{2066}\u{2069}😀"), None);
        assert_eq!(
            first_strong_direction("123 אבג abc"),
            Some(Directionality::Rtl)
        );
        assert_eq!(
            first_strong_direction("123 العربية abc"),
            Some(Directionality::Rtl)
        );
        assert_eq!(
            first_strong_direction("123 \u{10400} אבג"),
            Some(Directionality::Ltr)
        );
        assert_eq!(
            first_strong_direction("\u{200e}אבג"),
            Some(Directionality::Ltr)
        );
        assert_eq!(
            first_strong_direction("\u{200f}abc"),
            Some(Directionality::Rtl)
        );
    }

    #[test]
    fn visual_runs_keep_logical_codepoints_and_numeric_direction() {
        let text = "abc אבג 123";
        let info = resolve(text, Some(false)).unwrap();
        let (levels, runs) = info.visual_runs(&info.paragraphs[0], 0..text.len());
        let runs: alloc::vec::Vec<_> = runs
            .into_iter()
            .map(|range| {
                (
                    text.get(range.clone()).unwrap(),
                    levels[range.start].is_rtl(),
                )
            })
            .collect();
        assert_eq!(runs, [("abc ", false), ("123", false), ("אבג ", true)]);
    }
    #[test]
    fn isolates_and_bounds_are_resolved_without_string_reversal() {
        let text = "a\u{2067}אב 12\u{2069}b";
        let info = resolve(text, Some(false)).unwrap();
        assert!(info.has_rtl());
        assert!(info.levels.iter().all(|level| level.number() <= 125));
        assert!(resolve(
            &alloc::string::String::from_iter(core::iter::repeat_n('a', MAX_TEXT_BYTES + 1)),
            None
        )
        .is_err());
        assert_eq!(UNICODE_VERSION, (16, 0, 0));
    }
}
