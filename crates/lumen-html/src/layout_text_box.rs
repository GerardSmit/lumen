//! `text-box-trim` / `text-box-edge` for block containers in horizontal flow.
use super::*;

impl Layout<'_> {
    /// Distances the first and last line of a block container give up at its
    /// block-start and block-end content edge.
    pub(super) fn text_box_trim_amounts(&self, style: &Style) -> (f32, f32) {
        if style.writing_mode != css::WritingMode::HorizontalTb || style.font_size == 0.0 {
            return (0.0, 0.0);
        }
        let (start, end) = match css::extra_props::keyword(style, "text-box-trim") {
            Some("trim-start") => (true, false),
            Some("trim-end") => (false, true),
            Some("trim-both") => (true, true),
            _ => return (0.0, 0.0),
        };
        let mut words = css::extra_props::keyword(style, "text-box-edge")
            .unwrap_or("auto")
            .split_whitespace();
        let (over, under) = match (words.next(), words.next()) {
            (None | Some("auto"), _) => ("text", "text"),
            (Some(first), Some(second)) => (first, second),
            (Some(single @ ("ideographic" | "ideographic-ink")), None) => (single, single),
            (Some(single), None) => (single, "text"),
        };
        let font_height = self.font_line_height(style);
        let ascent = self.font_ascent(style);
        let descent = font_height - ascent;
        let half_leading = (self.line_height(style) - font_height) * 0.5;
        let over_offset = match over {
            "cap" => self
                .text
                .font_unit_metrics_styled(style.font_size, &style.font, false, false)
                .cap
                .map_or(0.0, |cap| ascent - cap),
            "ex" => ascent - self.font_relative_metrics(style).ex,
            _ => 0.0,
        };
        let under_offset = if under == "alphabetic" { descent } else { 0.0 };
        (
            if start { half_leading + over_offset } else { 0.0 },
            if end { half_leading + under_offset } else { 0.0 },
        )
    }
}
