use super::Style;

const WIDTH_SLOT: usize = 3;

impl Style {
    /// A column group's width is the default width of each column it contains.
    pub(crate) fn inherit_table_column_width(&mut self, group: &Style) {
        let own = self.width.is_some() || self.relative_lengths.iter().any(|(slot, _)| *slot == WIDTH_SLOT);
        if own { return; }
        self.width = group.width;
        self.relative_lengths.extend(group.relative_lengths.iter().filter(|(slot, _)| *slot == WIDTH_SLOT).cloned());
    }
}
