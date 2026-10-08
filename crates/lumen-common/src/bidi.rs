//! Shared, bounded UAX #9 resolution. Shape logical runs in the returned visual order.
pub use unicode_bidi::{bidi_class, BidiClass, BidiInfo, Level, UNICODE_VERSION};
pub use unicode_script::{Script, ScriptExtension, UnicodeScript};

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

/// A contiguous byte-preserving source piece in a transient CSS-formatted stream.
/// Synthetic controls have no source piece and therefore cannot become glyph,
/// hit-test, rendered-text or Selection source offsets.
pub struct SourceSegment { pub source:std::ops::Range<usize>, pub formatted:std::ops::Range<usize> }
pub fn resolve_formatted<'a>(text:&'a str,formatted:&str,segments:&[SourceSegment],rtl:Option<bool>)->Result<BidiInfo<'a>,&'static str> {
    if text.len()>MAX_TEXT_BYTES || formatted.len()>MAX_TEXT_BYTES {return Err("text run too large");}
    let resolved=resolve(formatted,rtl)?;
    let mut levels=Vec::new();levels.try_reserve(text.len()).map_err(|_|"bidi allocation failed")?;levels.resize(text.len(),Level::ltr());
    let mut classes=Vec::new();classes.try_reserve(text.len()).map_err(|_|"bidi allocation failed")?;classes.resize(text.len(),BidiClass::L);
    let mut end=0;
    for segment in segments {
        if segment.source.start!=end || segment.source.start>segment.source.end || segment.formatted.start>segment.formatted.end || segment.source.end>text.len() || segment.formatted.end>formatted.len() || !text.is_char_boundary(segment.source.start) || !text.is_char_boundary(segment.source.end) || !formatted.is_char_boundary(segment.formatted.start) || !formatted.is_char_boundary(segment.formatted.end) || segment.source.len()!=segment.formatted.len() {return Err("invalid bidi source projection");}
        levels[segment.source.clone()].copy_from_slice(&resolved.levels[segment.formatted.clone()]);
        classes[segment.source.clone()].copy_from_slice(&resolved.original_classes[segment.formatted.clone()]);end=segment.source.end;
    }
    if end!=text.len(){return Err("incomplete bidi source projection");}
    let mut paragraphs:Vec<unicode_bidi::ParagraphInfo>=Vec::new();let mut index=0;
    for paragraph in &resolved.paragraphs {
        while index<segments.len() && segments[index].formatted.end<=paragraph.range.start {index+=1;}
        let mut cursor=index;let mut source:Option<std::ops::Range<usize>>=None;
        while let Some(segment)=segments.get(cursor).filter(|segment|segment.formatted.start<paragraph.range.end) {
            let start=segment.formatted.start.max(paragraph.range.start);let end=segment.formatted.end.min(paragraph.range.end);
            if start<end {let range=segment.source.start+start-segment.formatted.start..segment.source.start+end-segment.formatted.start;
                if let Some(source)=&mut source {source.end=range.end;}else{source=Some(range);}}
            if segment.formatted.end>paragraph.range.end {break;}cursor+=1;
        }
        if let Some(range)=source {paragraphs.try_reserve(1).map_err(|_|"bidi allocation failed")?;paragraphs.push(unicode_bidi::ParagraphInfo{range,level:paragraph.level});}
    }
    Ok(BidiInfo{text,original_classes:classes,levels,paragraphs})
}

/// Visual items suitable for shaping. UAX #9 L1 can reset trailing spaces to
/// the paragraph level without changing their visual direction. Such a level
/// boundary is not a shaping boundary: retain kerning and joining with the
/// adjacent logical text, while keeping isolate initiators/terminators as
/// explicit barriers between inside and outside text.
pub fn shaping_runs(info: &BidiInfo<'_>, paragraph: &unicode_bidi::ParagraphInfo,
    range: std::ops::Range<usize>) -> Result<(Vec<Level>, Vec<std::ops::Range<usize>>), &'static str> {
    let (levels, visual) = info.visual_runs(paragraph, range);
    if visual.len() <= 1 && !visual.iter().any(|run| info.text[run.clone()].chars().any(|ch|
        matches!(bidi_class(ch), BidiClass::LRI | BidiClass::RLI | BidiClass::FSI | BidiClass::PDI))) {
        return Ok((levels, visual));
    }
    let mut output: Vec<std::ops::Range<usize>> = Vec::new();
    let mut previous_barrier = false;
    for run in visual {
        let rtl = levels[run.start].is_rtl();
        let mut pieces = Vec::new();
        let mut start = run.start;
        for (offset, ch) in info.text[run.clone()].char_indices() {
            if matches!(bidi_class(ch), BidiClass::LRI | BidiClass::RLI | BidiClass::FSI | BidiClass::PDI) {
                let at = run.start + offset;
                pieces.try_reserve(2).map_err(|_| "bidi shaping allocation failed")?;
                if start < at {pieces.push((start..at, false));}
                start = at + ch.len_utf8();
                pieces.push((at..start, true));
            }
        }
        if start < run.end {
            pieces.try_reserve(1).map_err(|_| "bidi shaping allocation failed")?;
            pieces.push((start..run.end, false));
        }
        if rtl {pieces.reverse();}
        for (piece, barrier) in pieces {
            if !barrier && !previous_barrier {
                if let Some(previous) = output.last_mut() {
                    if levels[previous.start].is_rtl() == rtl
                        && if rtl {piece.end == previous.start} else {previous.end == piece.start} {
                        previous.start = previous.start.min(piece.start);
                        previous.end = previous.end.max(piece.end);
                        continue;
                    }
                }
            }
            output.try_reserve(1).map_err(|_| "bidi shaping allocation failed")?;
            output.push(piece);
            previous_barrier = barrier;
        }
    }
    Ok((levels, output))
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    #[test]
    fn specification_css_bidi_projection_preserves_source_bytes_and_paragraph_bases() {
        let text="aאבz";let formatted="a\u{2067}אב\u{2069}z";
        let info=resolve_formatted(text,formatted,&[
            SourceSegment{source:0..1,formatted:0..1},SourceSegment{source:1..5,formatted:4..8},SourceSegment{source:5..6,formatted:11..12}
        ],Some(false)).unwrap();
        assert_eq!(info.text,text);assert_eq!(info.levels.len(),text.len());
        assert!(!info.levels[0].is_rtl());assert!(info.levels[1].is_rtl());assert!(!info.levels[5].is_rtl());
        assert_eq!(info.paragraphs[0].range,0..6);
        let text="אב\nab";let info=resolve_formatted(text,text,&[SourceSegment{source:0..text.len(),formatted:0..text.len()}],None).unwrap();
        assert_eq!(info.paragraphs.len(),2);assert!(info.paragraphs[0].level.is_rtl());assert!(!info.paragraphs[1].level.is_rtl());
        assert!(resolve_formatted("a","a",&[],Some(false)).is_err());
    }

    #[test]
    fn shaping_runs_preserve_trailing_space_context_and_isolate_boundaries() {
        for text in ["\u{2066}A \u{2069}", "\u{2066}A \u{2069}B"] {
            let info = resolve(text, Some(false)).unwrap();
            let (_, runs) = shaping_runs(&info, &info.paragraphs[0], 0..text.len()).unwrap();
            let values: alloc::vec::Vec<_> = runs.iter().map(|range| &text[range.clone()]).collect();
            assert!(values.contains(&"A "), "{values:?}");
            assert!(!values.iter().any(|value| value.contains('A') && value.contains('B')), "isolate boundary: {values:?}");
        }
    }
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
