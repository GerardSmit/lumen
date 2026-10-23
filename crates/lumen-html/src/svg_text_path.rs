//! Glyph placement for SVG `<textPath>`.
use alloc::{string::String, vec::Vec};
use crate::paint::Glyph;
use lumen_common::svg_path_measure::MeasuredPath;

pub(crate) struct PlacedGlyph {
    pub glyph: Glyph,
    pub advance: f32,
    pub x: f32,
    pub y: f32,
    pub tangent: [f32; 2],
}

/// CSS white space collapsing as SVG 2 text uses it: segment breaks and tabs
/// become spaces, runs of spaces collapse and the ends are trimmed. A no-break
/// space is preserved.
pub(crate) fn collapse_whitespace(raw: &str) -> String {
    collapse_whitespace_with(raw, |ch| matches!(ch, ' ' | '\n' | '\r' | '\t'))
}

/// Collapses runs of characters accepted by `is_space` into one space and
/// trims both ends.
pub(crate) fn collapse_whitespace_with(raw: &str, is_space: impl Fn(char) -> bool) -> String {
    let mut output = String::with_capacity(raw.len());
    let mut pending_space = false;
    for ch in raw.chars() {
        if is_space(ch) {
            pending_space = !output.is_empty();
        } else {
            if pending_space {
                output.push(' ');
                pending_space = false;
            }
            output.push(ch);
        }
    }
    output
}

/// Place each glyph by its midpoint along `path`. `text_length` stretches the
/// gaps between glyphs (`lengthAdjust="spacing"`); glyphs whose midpoint falls
/// off the path are dropped.
pub(crate) fn place_glyphs(
    glyphs: &[Glyph],
    total_width: f32,
    path: &MeasuredPath,
    start_offset: f32,
    text_length: Option<f32>,
) -> Vec<PlacedGlyph> {
    let gap = match text_length {
        Some(target) if glyphs.len() > 1 && target.is_finite() => {
            (target - total_width) / (glyphs.len() - 1) as f32
        }
        _ => 0.0,
    };
    let mut placed = Vec::with_capacity(glyphs.len());
    for (index, glyph) in glyphs.iter().enumerate() {
        let pen = glyph.x - glyph.x_offset;
        let next_pen = glyphs
            .get(index + 1)
            .map_or(total_width, |next| next.x - next.x_offset);
        let advance = (next_pen - pen).max(0.0);
        let middle = start_offset + pen + gap * index as f32 + advance * 0.5;
        let Some(point) = path.at(middle) else {
            continue;
        };
        let mut glyph = *glyph;
        glyph.x = glyph.x_offset;
        placed.push(PlacedGlyph {
            glyph,
            advance,
            x: point.x,
            y: point.y,
            tangent: point.tangent,
        });
    }
    placed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_breaks_and_keeps_no_break_space() {
        assert_eq!(
            collapse_whitespace("\n  \u{a0}\u{a0}/\n   /  \t/\n"),
            "\u{a0}\u{a0}/ / /"
        );
        assert_eq!(collapse_whitespace(" \n "), "");
    }
}
