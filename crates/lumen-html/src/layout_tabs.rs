//! Tab stops for preserved tab characters (CSS Text 3, `tab-size`).
use super::*;

impl Layout<'_> {
    fn tab_stop_spacing(&self, style: &Style) -> Result<Option<(f32, f32)>, LayoutError> {
        if !matches!(style.white_space, WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::BreakSpaces) {
            return Ok(None);
        }
        let raw = css::extra_props::stored(style, "tab-size").unwrap_or_default();
        let space = self.measure_styled_text(" ", style)?;
        let word = style.word_spacing.pixels + style.word_spacing.fraction * space;
        let width = if let Some(length) = raw.strip_suffix("px") {
            length.trim().parse::<f32>().unwrap_or(0.0)
        } else {
            let count = raw.trim().parse::<f32>().unwrap_or(8.0);
            count * (space + style.letter_spacing.unwrap_or(0.0) + word)
        };
        if !width.is_finite() {
            return Err(LayoutError::Text);
        }
        Ok(Some((width.max(0.0), space * 0.5)))
    }

    /// Stretches every preserved tab of `shaped` to the next tab stop, measured
    /// from `base` plus the start of the text or the last forced line break within it.
    /// Returns the extra advance given to the cluster starting at each tab.
    pub(super) fn apply_tab_stops(
        &self,
        shaped: ShapedRun,
        text: &str,
        style: &Style,
        rtl: bool,
        base: f32,
    ) -> Result<(ShapedRun, Vec<(usize, f32)>), LayoutError> {
        if rtl || !text.contains('\t') || style.font_size == 0.0 {
            return Ok((shaped, Vec::new()));
        }
        let Some((tab_width, minimum)) = self.tab_stop_spacing(style)? else {
            return Ok((shaped, Vec::new()));
        };
        let pen = |offset: usize| -> f32 {
            let index = shaped.glyphs.partition_point(|glyph| (glyph.cluster as usize) < offset);
            shaped.glyphs.get(index).map_or(shaped.width, |glyph| glyph.x - glyph.x_offset)
        };
        let mut extras: Vec<(usize, f32)> = Vec::new();
        let mut shift = 0.0f32;
        let mut origin = 0.0f32;
        for (offset, ch) in text.char_indices() {
            let next = offset + ch.len_utf8();
            match ch {
                '\u{200b}' | '\n' => origin = base + pen(next) + shift,
                '\t' => {
                    let start = base + pen(offset) + shift;
                    let natural = pen(next) - pen(offset);
                    let relative = start - origin;
                    let advance = if tab_width <= 0.0 {
                        0.0
                    } else {
                        let mut distance = tab_width - relative.rem_euclid(tab_width);
                        if distance < minimum {
                            distance += tab_width;
                        }
                        distance
                    };
                    let extra = advance - natural;
                    extras.try_reserve(1).map_err(|_| LayoutError::CommandLimit)?;
                    extras.push((offset, extra));
                    shift += extra;
                }
                _ => {}
            }
        }
        if extras.is_empty() || !shift.is_finite() {
            return Ok((shaped, extras));
        }
        let mut glyphs = Vec::new();
        glyphs.try_reserve_exact(shaped.glyphs.len()).map_err(|_| LayoutError::CommandLimit)?;
        let mut cumulative = 0.0f32;
        let mut next_tab = 0usize;
        for glyph in shaped.glyphs.iter() {
            while next_tab < extras.len() && extras[next_tab].0 < glyph.cluster as usize {
                cumulative += extras[next_tab].1;
                next_tab += 1;
            }
            let mut glyph = *glyph;
            glyph.x += cumulative;
            glyphs.push(glyph);
        }
        let width = shaped.width + shift;
        Ok((ShapedRun { glyphs: Arc::from(glyphs), width }, extras))
    }

    /// Resolves the tab stops of one laid out line once the widths of the
    /// preceding tokens (atoms, box edges, other runs) are known.
    pub(super) fn apply_line_tab_stops(
        &self,
        paragraph: &InlineParagraph,
        tokens: &mut [InlineToken],
    ) -> Result<(), LayoutError> {
        let preserves = |token: &InlineToken| -> bool {
            let (Some(style), Some(range)) = (&token.style, &token.source_range) else {
                return false;
            };
            matches!(style.white_space, WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::BreakSpaces)
                && paragraph.text.get(range.clone()).is_some_and(|text| text.contains('\t'))
        };
        if tokens.iter().any(|token| token.rtl && preserves(token)) || !tokens.iter().any(preserves) {
            return Ok(());
        }
        let starts_paragraph = tokens
            .iter()
            .filter_map(|token| token.source_range.as_ref())
            .any(|range| range.start == 0);
        let mut x = if starts_paragraph { paragraph.first_line_indent } else { 0.0 };
        for token in tokens.iter_mut() {
            if preserves(token) && token.shaped.is_some() {
                let (Some(style), Some(range), Some(shaped)) =
                    (token.style.as_ref(), token.source_range.as_ref(), token.shaped.take())
                else {
                    return Err(LayoutError::Text);
                };
                let text = paragraph.text.get(range.clone()).ok_or(LayoutError::Text)?;
                let before = shaped.width;
                let (stretched, _) = self.apply_tab_stops(shaped, text, style, false, x)?;
                token.width += stretched.width - before;
                token.shaped = Some(stretched);
            }
            x += token.width;
        }
        Ok(())
    }
}
