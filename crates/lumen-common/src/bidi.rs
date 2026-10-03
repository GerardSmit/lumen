//! Shared, bounded UAX #9 resolution. Shape logical runs in the returned visual order.
pub use unicode_bidi::{BidiInfo, Level, UNICODE_VERSION};
pub use unicode_script::{Script, UnicodeScript};

pub const MAX_TEXT_BYTES: usize = 64 * 1024;

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
        assert!(
            resolve(
                &alloc::string::String::from_iter(core::iter::repeat_n('a', MAX_TEXT_BYTES + 1)),
                None
            )
            .is_err()
        );
        assert_eq!(UNICODE_VERSION, (16, 0, 0));
    }
}
