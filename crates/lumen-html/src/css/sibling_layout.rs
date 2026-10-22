//! Whether a box's children are sized from each other's contributions.
use super::*;

impl Style {
    /// Flex, grid, table and multicol containers, floats, absolutely positioned
    /// boxes, inline-blocks and intrinsic-size keywords make a child's used size
    /// depend on its siblings; a plain block container does not.
    pub fn sibling_sizes_interdependent(&self) -> bool {
        !matches!(self.display, Display::Block | Display::FlowRoot | Display::Inline | Display::ListItem | Display::Contents | Display::None)
            || self.float != Float::None
            || matches!(self.position, Position::Absolute | Position::Fixed)
            || self.column_count.is_some()
            || self.column_width != columns::ColumnWidth::Auto
            || self.intrinsic_size_keywords(true)[0].is_some()
            || self.intrinsic_size_keywords(false)[0].is_some()
    }
}
