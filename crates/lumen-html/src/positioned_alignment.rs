//! Self-alignment of absolutely positioned boxes within their
//! inset-modified containing block (CSS Box Alignment 3 section 5, CSS
//! Positioned Layout 3 section 5).
use crate::css::{AlignItems, AlignmentKeyword, ItemAlignment, Style};

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
    /// Center of the default anchor along this axis, for `anchor-center`.
    pub anchor_center: Option<f32>,
    /// The real containing block range when `cb_*` describe a `position-area`.
    pub outer: Option<(f32, f32)>,
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
        let alignment = if alignment.is_baseline() {
            ItemAlignment {
                alignment: if alignment.is_last_baseline() { AlignItems::End } else { AlignItems::Start },
                keyword: None,
                safe: Some(true),
            }
        } else {
            alignment
        };
        let imcb_start = self.cb_start + self.inset_start;
        let imcb_end = self.cb_start + self.cb_size - self.inset_end;
        let margin_box = self.size + self.margin_start + self.margin_end;
        let free = imcb_end - imcb_start - margin_box;
        if let (Some(center), Some(AlignmentKeyword::AnchorCenter)) = (self.anchor_center, alignment.keyword) {
            let start = center - margin_box * 0.5;
            return if alignment.safe == Some(false) {
                start
            } else {
                start.min(imcb_end - margin_box).max(imcb_start)
            };
        }
        let mut start =
            imcb_start + alignment.offset(free, container, subject, self.horizontal, false);
        if free < 0.0 && alignment.safe.is_none() {
            let (outer_start, outer_end) = self.outer.unwrap_or((self.cb_start, self.cb_start + self.cb_size));
            let limit_start = imcb_start.min(outer_start);
            let limit_end = imcb_end.max(outer_end);
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
