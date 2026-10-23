//! `line-clamp` / `max-lines` / `-webkit-line-clamp`: lines of a clamp container
//! beyond the limit are not laid out and the last kept line ends in the block
//! ellipsis.
use super::*;

pub(super) struct LineClamp {
    pub(super) remaining: u32,
    pub(super) exhausted: bool,
    ellipsis: Option<Arc<str>>,
    style: Style,
    tail: Option<(f32, f32)>,
    ellipsis_done: bool,
}

fn parse_lines(value: &str) -> Option<u32> {
    value.split_ascii_whitespace().next()?.parse::<u32>().ok().filter(|count| *count > 0)
}

fn ellipsis_text(value: Option<&str>) -> Option<Arc<str>> {
    match value? {
        "no-ellipsis" => None,
        "ellipsis" | "auto" => Some(Arc::from("\u{2026}")),
        quoted => {
            let inner = quoted.strip_prefix(['"', '\'']).and_then(|rest| rest.strip_suffix(['"', '\''])).unwrap_or(quoted);
            let mut text = String::with_capacity(inner.len());
            let mut escaped = false;
            for c in inner.chars() {
                if !escaped && c == '\\' {
                    escaped = true;
                    continue;
                }
                escaped = false;
                text.push(c);
            }
            (!text.is_empty()).then(|| Arc::from(text))
        }
    }
}

impl LineClamp {
    pub(super) fn from_style(style: &Style) -> Option<LineClamp> {
        if style.writing_mode != css::WritingMode::HorizontalTb || LineClamp::multicol(style) {
            return None;
        }
        if !matches!(style.display, Display::Block | Display::FlowRoot | Display::ListItem | Display::InlineBlock) {
            return None;
        }
        if css::extra_props::keyword(style, "continue") == Some("-webkit-legacy")
            && !(style.display_ruby == 6 && css::extra_props::keyword(style, "-webkit-box-orient") == Some("vertical"))
        {
            return None;
        }
        let (remaining, ellipsis) = if let Some(lines) = css::extra_props::keyword(style, "max-lines").and_then(parse_lines) {
            (lines, ellipsis_text(css::extra_props::keyword(style, "block-ellipsis")))
        } else {
            return None;
        };
        Some(LineClamp { remaining, exhausted: false, ellipsis, style: style.clone(), tail: None, ellipsis_done: false })
    }

    fn multicol(style: &Style) -> bool {
        style.column_count.is_some() || style.column_width != css::columns::ColumnWidth::Auto
    }

    /// Boxes that form their own independent formatting context keep their
    /// lines out of an enclosing container's count.
    pub(super) fn isolates(style: &Style) -> bool {
        style.float != Float::None
            || !matches!(style.overflow_x, css::Overflow::Visible | css::Overflow::Clip)
            || !matches!(style.overflow_y, css::Overflow::Visible | css::Overflow::Clip)
            || matches!(style.position, Position::Absolute | Position::Fixed)
            || matches!(style.display, Display::InlineBlock | Display::FlowRoot | Display::Flex | Display::Grid
                | Display::Table | Display::InlineTable | Display::TableCell | Display::TableCaption)
    }
}

impl Layout<'_> {
    pub(super) fn line_clamp_for(&self, style: &Style) -> Option<LineClamp> {
        if let Some(clamp) = LineClamp::from_style(style) {
            return Some(clamp);
        }
        if style.writing_mode != css::WritingMode::HorizontalTb
            || LineClamp::multicol(style)
            || !matches!(style.display, Display::Block | Display::FlowRoot | Display::ListItem | Display::InlineBlock)
            || !matches!(css::extra_props::keyword(style, "continue"), Some("collapse" | "discard"))
        {
            return None;
        }
        let limit = style.height.or(style.max_height).map(|value| content_height_dimension(style, value))?;
        let line_height = self.line_height(style);
        if !(line_height > 0.0) || !limit.is_finite() {
            return None;
        }
        let lines = ((limit / line_height) + 0.001).floor();
        if lines < 1.0 {
            return None;
        }
        let ellipsis = ellipsis_text(css::extra_props::keyword(style, "block-ellipsis"));
        Some(LineClamp { remaining: lines as u32, exhausted: false, ellipsis, style: style.clone(), tail: None, ellipsis_done: false })
    }

    pub(super) fn line_clamp_out_of_flow(&self, item: &BlockFlowItem) -> Result<bool, LayoutError> {
        let BlockFlowItem::Child(child) = item else { return Ok(false) };
        let FlattenedBoxChildKind::Node(node) = &child.kind else { return Ok(false) };
        let style = match &child.computed_style {
            Some(style) => style.clone(),
            None => self.computed_style(*node, Some(&child.parent_style)).map_err(LayoutError::Css)?,
        };
        Ok(matches!(style.position, Position::Absolute | Position::Fixed))
    }

    pub(super) fn line_clamp_exhausted(&self) -> bool {
        self.line_clamp.as_ref().is_some_and(|clamp| clamp.exhausted)
    }

    pub(super) fn line_clamp_last_line(&self) -> bool {
        self.line_clamp.as_ref().is_some_and(|clamp| clamp.remaining == 1)
    }

    pub(super) fn line_clamp_shape(&self) -> Result<Option<ShapedRun>, LayoutError> {
        let Some(clamp) = self.line_clamp.as_ref() else { return Ok(None); };
        let Some(text) = clamp.ellipsis.as_ref() else { return Ok(None); };
        let rtl = clamp.style.used_direction() == Direction::Rtl;
        self.shape_styled_text(text, &clamp.style, rtl, None).map(Some)
    }

    pub(super) fn line_clamp_note_tail(&mut self, x: f32, baseline_y: f32) {
        if let Some(clamp) = self.line_clamp.as_mut() {
            clamp.tail = Some((x, baseline_y));
        }
    }

    pub(super) fn line_clamp_finish_line(&mut self) {
        if let Some(clamp) = self.line_clamp.as_mut() {
            clamp.remaining = clamp.remaining.saturating_sub(1);
            clamp.exhausted = clamp.remaining == 0;
        }
    }

    pub(super) fn line_clamp_paint_ellipsis(&mut self, shaped: ShapedRun, x: f32, baseline_y: f32) -> Result<(), LayoutError> {
        let Some(clamp) = self.line_clamp.as_mut() else { return Ok(()); };
        clamp.ellipsis_done = true;
        let style = clamp.style.clone();
        if shaped.glyphs.is_empty() || !style.visibility_visible {
            return Ok(());
        }
        let ink = self.begin_text_ink(&style)?;
        self.push_command(Command::GlyphRun {
            origin_x: x,
            baseline_y,
            size: style.font_size,
            color: style.color,
            glyphs: shaped.glyphs,
        })?;
        self.finish_text_ink(ink)
    }

    /// Content exists past a clamped line that ended its paragraph, so that
    /// line still needs its ellipsis.
    pub(super) fn line_clamp_flush_tail(&mut self) -> Result<(), LayoutError> {
        let pending = match self.line_clamp.as_mut() {
            Some(clamp) if !clamp.ellipsis_done => clamp.tail.take(),
            _ => None,
        };
        let Some((x, baseline_y)) = pending else { return Ok(()); };
        let Some(shaped) = self.line_clamp_shape()? else { return Ok(()); };
        self.line_clamp_paint_ellipsis(shaped, x, baseline_y)
    }
}
