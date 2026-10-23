//! Grid lines referenced by an absolutely positioned grid child (CSS Grid
//! section 9.1): a line outside the existing grid, or `auto`, stands for the
//! padding edge of the grid container instead of creating implicit tracks.
use super::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AbsoluteGridLines {
    pub start: Option<usize>,
    pub end: Option<usize>,
}

impl AbsoluteGridLines {
    pub fn is_auto(self) -> bool {
        self.start.is_none() && self.end.is_none()
    }
}

/// Zero-based line indexes bounding the child's grid area on one axis.
/// `track_count` is the number of tracks of the existing (implicit) grid.
pub fn absolute_grid_lines(
    fallback: GridPlacement,
    raw: Option<&str>,
    names: &[GridNamedLine],
    explicit: usize,
    areas: &[GridArea],
    horizontal: bool,
    track_count: usize,
    lead: usize,
) -> AbsoluteGridLines {
    let (start, end) = match raw {
        None => (fallback.start, fallback.start.map(|start| start.saturating_add(fallback.span))),
        Some(raw) => {
            let mut parts = raw.split('/');
            let first = parts.next().unwrap_or("auto");
            let second = parts.next().unwrap_or("auto");
            let parse = |raw: &str, end: bool| parse_grid_line_reference_signed(raw, end, names, explicit, areas, horizontal, lead);
            let (Some((start, start_span, start_is_span)), Some((end, end_span, end_is_span))) =
                (parse(first, false), parse(second, true))
            else {
                return AbsoluteGridLines::default();
            };
            let line = |line: Option<i64>| line.and_then(|line| usize::try_from(line).ok());
            let span = |span: usize| i64::try_from(span).unwrap_or(i64::MAX);
            match (start, end) {
                (Some(start), Some(end)) => (line(Some(start.min(end))), line(Some(start.max(end)))),
                (Some(start), None) => (line(Some(start)), end_is_span.then(|| line(Some(start.saturating_add(span(end_span))))).flatten()),
                (None, Some(end)) => (start_is_span.then(|| line(Some(end.saturating_sub(span(start_span))))).flatten(), line(Some(end))),
                (None, None) => (None, None),
            }
        }
    };
    let within = |line: Option<usize>| line.filter(|line| *line <= track_count);
    let (start, end) = (within(start), within(end));
    AbsoluteGridLines { start, end: if start.is_some() && start == end { None } else { end } }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(raw: &str, tracks: usize) -> AbsoluteGridLines {
        absolute_grid_lines(GridPlacement { start: None, span: 1 }, Some(raw), &[], tracks, &[], true, tracks, 0)
    }

    #[test]
    fn lines_outside_the_grid_are_auto() {
        assert_eq!(lines("1 / auto", 4), AbsoluteGridLines { start: Some(0), end: None });
        assert_eq!(lines("auto / 3", 4), AbsoluteGridLines { start: None, end: Some(2) });
        assert_eq!(lines("2 / 6", 4), AbsoluteGridLines { start: Some(1), end: None });
        assert_eq!(lines("6 / auto", 4), AbsoluteGridLines::default());
        assert_eq!(lines("3 / span 2", 4), AbsoluteGridLines { start: Some(2), end: Some(4) });
    }
}
