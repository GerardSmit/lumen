//! Self-alignment of absolutely positioned boxes within their
//! inset-modified containing block (CSS Box Alignment 3 section 5, CSS
//! Positioned Layout 3 section 5).
use crate::css::{AlignItems, ItemAlignment, Style};

/// One physical axis of the containing block's padding box and the box's
/// insets, margins and used border-box size along it.
pub(crate) struct PositionedAxis {
    pub cb_start: f32,
    pub cb_size: f32,
    pub inset_start: f32,
    pub inset_end: f32,
    pub margin_start: f32,
    pub margin_end: f32,
    pub size: f32,
    pub horizontal: bool,
}

pub(crate) fn requests_positioned_alignment(style: &Style, parent: &Style) -> bool {
    [true, false]
        .into_iter()
        .any(|inline| style.self_alignment(parent, inline).alignment != AlignItems::Stretch)
}

impl PositionedAxis {
    /// The margin-edge start coordinate of the aligned box. Without an
    /// explicit overflow keyword, a box overflowing its inset-modified
    /// containing block shifts to stay inside the union of that block and the
    /// containing block, falling back to the container's start side.
    pub fn aligned_start(
        &self,
        alignment: ItemAlignment,
        container: &Style,
        subject: &Style,
    ) -> f32 {
        let imcb_start = self.cb_start + self.inset_start;
        let imcb_end = self.cb_start + self.cb_size - self.inset_end;
        let margin_box = self.size + self.margin_start + self.margin_end;
        let free = imcb_end - imcb_start - margin_box;
        let mut start =
            imcb_start + alignment.offset(free, container, subject, self.horizontal, false);
        if free < 0.0 && alignment.safe.is_none() {
            let limit_start = imcb_start.min(self.cb_start);
            let limit_end = imcb_end.max(self.cb_start + self.cb_size);
            if start < limit_start || start + margin_box > limit_end {
                if margin_box > limit_end - limit_start {
                    let start_side = ItemAlignment {
                        alignment: AlignItems::Start,
                        keyword: None,
                        safe: None,
                    };
                    let far = start_side.offset(1.0, container, container, self.horizontal, false)
                        > 0.5;
                    start = if far { limit_end - margin_box } else { limit_start };
                } else if start < limit_start {
                    start = limit_start;
                } else {
                    start = limit_end - margin_box;
                }
            }
        }
        start
    }
}
