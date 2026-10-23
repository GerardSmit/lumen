//! Leading implicit grid tracks (CSS Grid section 8.1): an item placed before
//! the first explicit line makes the grid grow backwards, with the new tracks
//! sized by `grid-auto-columns/rows` counted from the explicit grid outwards.
use super::*;
use alloc::vec::Vec;

fn leading_tracks(auto: Option<&[GridTrack]>, count: usize) -> Vec<GridTrack> {
    let auto = auto.filter(|tracks| !tracks.is_empty());
    (0..count)
        .map(|index| {
            auto.map_or(GridTrack::Auto, |tracks| {
                let distance = count - index - 1;
                tracks[tracks.len() - 1 - distance % tracks.len()]
            })
        })
        .collect()
}

fn shift_names(names: &[GridNamedLine], lead: usize) -> Vec<GridNamedLine> {
    names
        .iter()
        .map(|name| GridNamedLine {
            name: name.name.clone(),
            line: name.line + lead,
        })
        .collect()
}

impl Style {
    /// A copy of this grid container whose track lists, named lines and areas
    /// start with the given number of leading implicit tracks. Subgridded axes
    /// are left alone, so the result is `None` if either shifted axis is one.
    pub(crate) fn with_leading_grid_tracks(&self, columns: usize, rows: usize) -> Option<Style> {
        if columns != 0 && self.grid_columns_subgrid || rows != 0 && self.grid_rows_subgrid {
            return None;
        }
        let mut shifted = self.clone();
        if columns != 0 {
            let leading = leading_tracks(self.grid_auto_columns.as_deref(), columns);
            if let Some(auto) = &self.grid_columns_auto {
                let mut repeated = auto.clone();
                repeated.prefix_tracks = leading.iter().chain(auto.prefix_tracks.iter()).copied().collect();
                repeated.prefix_names = shift_names(&auto.prefix_names, columns).into();
                shifted.grid_columns_auto = Some(repeated);
            } else {
                let existing = self.grid_columns.as_deref().unwrap_or(&[]);
                shifted.grid_columns = Some(leading.iter().chain(existing.iter()).copied().collect());
                shifted.grid_column_names = self
                    .grid_column_names
                    .as_deref()
                    .map(|names| shift_names(names, columns).into());
            }
        }
        if rows != 0 {
            let leading = leading_tracks(self.grid_auto_rows.as_deref(), rows);
            if let Some(auto) = &self.grid_rows_auto {
                let mut repeated = auto.clone();
                repeated.prefix_tracks = leading.iter().chain(auto.prefix_tracks.iter()).copied().collect();
                repeated.prefix_names = shift_names(&auto.prefix_names, rows).into();
                shifted.grid_rows_auto = Some(repeated);
            } else {
                let existing = self.grid_rows.as_deref().unwrap_or(&[]);
                shifted.grid_rows = Some(leading.iter().chain(existing.iter()).copied().collect());
                shifted.grid_row_names = self
                    .grid_row_names
                    .as_deref()
                    .map(|names| shift_names(names, rows).into());
            }
        }
        if let Some(areas) = &self.grid_areas {
            shifted.grid_areas = Some(
                areas
                    .iter()
                    .map(|area| GridArea {
                        name: area.name.clone(),
                        column: area.column + columns,
                        row: area.row + rows,
                        columns: area.columns,
                        rows: area.rows,
                    })
                    .collect(),
            );
        }
        shifted.grid_lead = [columns, rows];
        Some(shifted)
    }
}
