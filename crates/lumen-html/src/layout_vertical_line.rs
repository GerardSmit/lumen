//! Line alignment in vertical block flow. A vertical paragraph is solved in
//! logical coordinates under one rotating transform, so the open line aligns
//! by moving the content inside that transform along the logical inline axis.
use super::*;

impl<'a> Layout<'a> {
    /// The physical height a vertical block's lines fill.
    pub(super) fn vertical_inline_extent(&self, style: &Style) -> f32 {
        constrained_height(
            style,
            style
                .height
                .map(|height| content_height_dimension(style, height))
                .or(self.parent_height)
                .unwrap_or(self.viewport_height()),
        )
        .max(0.0)
    }

    /// True for the outermost transform pair that carries a vertical
    /// paragraph's logical content; nested pairs belong to that content.
    pub(super) fn is_vertical_paragraph_frame(command: &Command, depth: &mut i32) -> bool {
        match command {
            Command::PushTransform(_) => {
                let outermost = *depth == 0;
                *depth += 1;
                outermost
            }
            Command::PopTransform if *depth > 0 => {
                *depth -= 1;
                *depth == 0
            }
            _ => false,
        }
    }
}
