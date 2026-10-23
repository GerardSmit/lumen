//! Preferred aspect ratios of non-replaced block-level boxes (CSS Sizing 4
//! section 5): the ratio-dependent axis, transferred minimum and maximum
//! sizes, and the content-based automatic minimum size.
use super::*;

/// Content-box block size for a content-box inline size.
pub(super) fn ratio_content_height(style: &Style, content_width: f32) -> Option<f32> {
    let height = ratio_height(style, specified_dimension(style, content_width))?;
    Some(content_height_dimension(style, height))
}

/// Content-box inline size for a content-box block size.
pub(super) fn ratio_content_width(style: &Style, content_height: f32) -> Option<f32> {
    let width = ratio_width(style, specified_height(style, content_height))?;
    Some(content_dimension(style, width))
}

/// An automatic inline size, with the block-size limits carried across the
/// ratio. The transferred maximum yields to an explicit minimum, and the
/// transferred minimum to an explicit maximum.
pub(super) fn transferred_inline_size(style: &Style, content_width: f32) -> f32 {
    let mut width = content_width;
    if let Some(maximum) = style.max_height {
        if let Some(limit) = ratio_content_width(style, content_height_dimension(style, maximum)) {
            width = width.min(limit);
        }
    }
    width = width.max(content_dimension(style, style.min_width));
    if style.min_height > 0.0 {
        if let Some(limit) = ratio_content_width(style, content_height_dimension(style, style.min_height)) {
            width = width.max(limit);
        }
    }
    if let Some(maximum) = style.max_width {
        width = width.min(content_dimension(style, maximum));
    }
    width.max(0.0)
}

/// A replaced element whose preferred or maximum inline size is a percentage
/// contributes nothing to its container's min-content size (CSS Sizing 3
/// section 5.2.2).
pub(super) fn compressible_replaced(style: &Style) -> bool {
    style.writing_mode == css::WritingMode::HorizontalTb
        && (style.compressible_percentage || style.has_width_percentage() || style.has_max_width_percentage())
}

/// Boxes whose auto size is derived from the preferred ratio in normal flow.
pub(super) fn block_flow_ratio_box(style: &Style, parent: &Style, tag: &str) -> bool {
    style.writing_mode == css::WritingMode::HorizontalTb
        && matches!(style.display, Display::Block | Display::FlowRoot | Display::ListItem)
        && style.float == Float::None
        && !matches!(style.position, Position::Absolute | Position::Fixed)
        && !matches!(parent.display, Display::Flex | Display::Grid)
        && !matches!(tag, "img" | "canvas" | "video" | "audio" | "iframe" | "svg" | "object" | "embed"
            | "input" | "textarea" | "select" | "button" | "fieldset" | "meter" | "progress")
}

impl Layout<'_> {
    /// Whether the box's preferred ratio comes from `aspect-ratio` alone,
    /// with no replaced content supplying a natural size.
    pub(super) fn positioned_ratio_applies(&self, id: NodeId, style: &Style) -> bool {
        style.preferred_aspect_ratio(None).is_some()
            && !matches!(self.document.kind(id), Ok(NodeKind::Element { name, .. })
                if matches!(name.as_str(), "img" | "canvas" | "video" | "audio" | "iframe" | "svg" | "object"
                    | "embed" | "input" | "textarea" | "select" | "button" | "fieldset" | "meter" | "progress"))
            && !style.display.is_table()
    }

    /// The min-content inline size of the box's own contents, ignoring its
    /// preferred ratio; the content-based automatic minimum of a box whose
    /// width follows from its height.
    pub(super) fn ratio_min_content_width(&self, id: NodeId, style: &Style, depth: usize) -> Result<f32, LayoutError> {
        let mut measured = style.intrinsic_measurement_style(true);
        measured.height = None;
        measured.aspect_ratio = None;
        measured.aspect_ratio_auto = false;
        let size = self.intrinsic_size_mode(id, &measured, depth + 1, true)?;
        Ok((size.0 - box_edges(style, true)).max(0.0))
    }
}
