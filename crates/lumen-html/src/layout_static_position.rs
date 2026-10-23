//! Static positions of out-of-flow boxes in vertical block flow and in
//! right-to-left inline flow (CSS Position 3 §4.1, CSS 2.1 §10.3.7): the
//! static position is the end of the preceding inline content along the inline
//! axis and the block-start edge of the line that contains it, and a box whose
//! inline start is the right or bottom side is anchored by that edge.
use super::*;

pub(super) struct StaticOrigin {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) right_edge: bool,
    pub(super) bottom_edge: bool,
}

impl<'a> Layout<'a> {
    pub(super) fn vertical_static_origin(
        &self,
        parent: &Style,
        left: f32,
        top: f32,
        width: f32,
        line: LineStart,
        advance: f32,
        block_cursor: f32,
    ) -> StaticOrigin {
        let axes = GridAxes::new(parent);
        let frame = FloatFrame::vertical(parent, left, top, width);
        let open = advance > 0.0 || line.vertical_line_height > 0.0;
        let x = if open {
            frame.physical(Rect { x: 0.0, y: line.vertical_block_cursor, width: 0.0, height: 0.0 }).x
        } else {
            block_cursor
        };
        let extent = self.vertical_inline_extent(parent);
        let y = if axes.reverse_inline { top + extent - advance } else { top + advance };
        StaticOrigin { x, y, right_edge: axes.reverse_block, bottom_edge: axes.reverse_inline }
    }

    pub(super) fn horizontal_static_origin(
        &self,
        parent: &Style,
        left: f32,
        width: f32,
        cursor: f32,
        advance: f32,
    ) -> StaticOrigin {
        let rtl = parent.used_direction() == Direction::Rtl;
        let x = if rtl { left + width - advance } else { left + advance };
        StaticOrigin { x, y: cursor, right_edge: rtl, bottom_edge: false }
    }

    /// The insets that anchor the box at its static position by the edge its
    /// inline or block start occupies. They apply only if the insets on that
    /// axis end up open and the box does not align itself within the
    /// containing block there.
    pub(super) fn static_edge_insets(&self, style: &Style, origin: &StaticOrigin, parent: &Style) -> (Option<f32>, Option<f32>) {
        let block = if style.position == Position::Fixed {
            self.fixed_containing_block.unwrap_or(self.viewport_rect())
        } else {
            self.containing_block.unwrap_or(self.viewport_rect())
        };
        let node = if style.position == Position::Fixed { self.fixed_containing_block_node } else { self.containing_block_node };
        // Insets are resolved against the containing block's scrolled content.
        let scroll = if let Some(node) = node {
            self.scrolls.iter().find(|scroll| scroll.node == node).map_or((0.0, 0.0), |scroll| (scroll.x, scroll.y))
        } else if style.position == Position::Absolute {
            self.root_scroll
        } else {
            (0.0, 0.0)
        };
        let container_vertical = parent.writing_mode != css::WritingMode::HorizontalTb;
        let aligned = |justify: bool| {
            let own = if justify { style.justify_self } else { style.align_self };
            own.is_some_and(|alignment| alignment != AlignItems::Stretch)
        };
        let right = (origin.right_edge && !aligned(!container_vertical))
            .then(|| block.x + block.width - origin.x - scroll.0);
        let bottom = (origin.bottom_edge && !aligned(container_vertical))
            .then(|| block.y + block.height - origin.y - scroll.1);
        (right, bottom)
    }

    pub(super) fn note_static_edges(&mut self, node: NodeId, insets: (Option<f32>, Option<f32>)) {
        if insets.0.is_some() || insets.1.is_some() {
            self.static_edges.insert(node.key(), insets);
        }
    }

    pub(super) fn apply_static_edges(&mut self, node: NodeId, style: &mut Style) {
        let Some((right, bottom)) = self.static_edges.remove(&node.key()) else { return };
        if css::extra_props::computed(style, "position-area").is_some_and(|area| area != "none") {
            return;
        }
        if let Some(right) = right {
            if style.left.is_none() && style.right.is_none() {
                style.right = Some(right);
            }
        }
        if let Some(bottom) = bottom {
            if style.top.is_none() && style.bottom.is_none() {
                style.bottom = Some(bottom);
            }
        }
    }
}
