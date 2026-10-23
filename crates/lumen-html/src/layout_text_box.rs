//! `text-box-trim` / `text-box-edge` for block containers in horizontal flow.
use super::*;

impl Layout<'_> {
    /// Distances the first and last line of a block container give up at its
    /// block-start and block-end content edge.
    pub(super) fn text_box_trim_amounts(&self, node: NodeId, style: &Style) -> Result<(f32, f32), LayoutError> {
        if style.writing_mode != css::WritingMode::HorizontalTb || style.font_size == 0.0 {
            return Ok((0.0, 0.0));
        }
        let (start, end) = match css::extra_props::keyword(style, "text-box-trim") {
            Some("trim-start") => (true, false),
            Some("trim-end") => (false, true),
            Some("trim-both") => (true, true),
            _ => return Ok((0.0, 0.0)),
        };
        let start = start && self.text_box_trim_reaches_line(node, style, false, 0)?;
        let end = end && self.text_box_trim_reaches_line(node, style, true, 0)?;
        if !start && !end {
            return Ok((0.0, 0.0));
        }
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
        let half_leading = self.half_leading(style);
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
        Ok((
            if start { half_leading + over_offset } else { 0.0 },
            if end { half_leading + under_offset } else { 0.0 },
        ))
    }

    /// Trimming reaches a line box only through in-flow block descendants that
    /// have content and no padding or border on the trimmed edge.
    fn text_box_trim_reaches_line(&self, node: NodeId, style: &Style, end: bool, depth: usize) -> Result<bool, LayoutError> {
        if depth > 64 {
            return Ok(false);
        }
        let pseudo = if end { css::PseudoElement::After } else { css::PseudoElement::Before };
        if self.virtual_generated_child(node, style, pseudo, 0.0)?.is_some() {
            return Ok(true);
        }
        let preserved = matches!(style.white_space, WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::BreakSpaces | WhiteSpace::PreLine);
        let invalid = |_| LayoutError::InvalidTree;
        let mut child = (if end { self.document.last_child(node) } else { self.document.first_child(node) }).map_err(invalid)?;
        while let Some(current) = child {
            child = (if end { self.document.previous_sibling(current) } else { self.document.next_sibling(current) }).map_err(invalid)?;
            match self.document.kind(current).map_err(invalid)? {
                NodeKind::Text(value) | NodeKind::CData(value) => {
                    if preserved || value.chars().any(|c| !c.is_ascii_whitespace()) {
                        return Ok(true);
                    }
                }
                NodeKind::Element { .. } => {
                    let child_style = self.computed_style(current, Some(style)).map_err(LayoutError::Css)?;
                    if child_style.display == Display::None
                        || child_style.float != Float::None
                        || matches!(child_style.position, Position::Absolute | Position::Fixed)
                    {
                        continue;
                    }
                    if !matches!(child_style.display, Display::Block | Display::FlowRoot | Display::ListItem) {
                        return Ok(child_style.is_inline_level() || child_style.display == Display::Contents);
                    }
                    let edge = if end { 2 } else { 0 };
                    if child_style.padding_sides()[edge] != 0.0 || child_style.used_border_widths()[edge] != 0.0 {
                        return Ok(false);
                    }
                    return self.text_box_trim_reaches_line(current, &child_style, end, depth + 1);
                }
                _ => {}
            }
        }
        Ok(false)
    }
}
