use super::*;
use super::extra_props as extra;
use crate::paint::Rect;

const EPSILON: f32 = 0.05;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Frame {
    pub vertical: bool,
    pub origin: f32,
    pub extent: f32,
    pub reverse: bool,
}

impl Frame {
    fn physical(self, logical: f32) -> f32 {
        self.origin + if self.reverse { self.extent - logical } else { logical }
    }

    fn logical(self, offset: f32) -> f32 {
        if self.reverse { self.extent - offset } else { offset }
    }
}

pub(crate) struct GridInput {
    pub columns: Vec<(f32, f32)>,
    pub rows: Vec<(f32, f32)>,
    pub items: Vec<[usize; 4]>,
    pub inline: Frame,
    pub block: Frame,
}

pub(crate) struct FlexLine {
    pub cross: (f32, f32),
    pub items: Vec<(f32, f32)>,
}

/// Flex coordinates are physical offsets from each frame's origin.
pub(crate) struct FlexInput {
    pub lines: Vec<FlexLine>,
    pub main_extent: f32,
    pub main_is_inline: bool,
    pub inline: Frame,
    pub block: Frame,
}

pub(crate) struct MulticolRow {
    pub block: (f32, f32),
    pub columns: Vec<(f32, f32)>,
}

pub(crate) struct MulticolInput {
    /// Resolved width, style and color of a plain single-valued `column-rule`.
    pub column_rule: Option<(f32, BorderStyle, Rgba)>,
    pub rows: Vec<MulticolRow>,
    pub inline_extent: f32,
    pub inline: Frame,
    pub block: Frame,
}

pub(crate) enum Input {
    Grid(GridInput),
    Flex(FlexInput),
    Multicol(MulticolInput),
}

pub(crate) struct Paint {
    pub rect: Rect,
    pub color: Rgba,
    pub style: BorderStyle,
    pub vertical: bool,
}

struct Interval {
    start: f32,
    end: f32,
    open: bool,
    before: bool,
    after: bool,
}

struct GapLine {
    index: usize,
    lo: f32,
    hi: f32,
    intervals: Vec<Interval>,
    lead: Option<(f32, f32)>,
    trail: Option<(f32, f32)>,
}

struct Model {
    column: Vec<GapLine>,
    row: Vec<GapLine>,
    inline: Frame,
    block: Frame,
}

fn ordered(a: f32, b: f32) -> (f32, f32) {
    if a <= b { (a, b) } else { (b, a) }
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() <= EPSILON
}

fn grid_model(grid: &GridInput) -> Model {
    let (column_count, row_count) = (grid.columns.len(), grid.rows.len());
    let mut owner = alloc::vec![0usize; column_count.saturating_mul(row_count)];
    for (id, item) in grid.items.iter().enumerate() {
        let [column, row, columns, rows] = *item;
        for r in row..row.saturating_add(rows).min(row_count) {
            for c in column..column.saturating_add(columns).min(column_count) {
                owner[r * column_count + c] = id + 1;
            }
        }
    }
    let cell = |c: usize, r: usize| owner[r * column_count + c];
    let interval = |start: f32, end: f32, first: usize, second: usize| {
        let closed = first != 0 && first == second;
        Interval { start, end, open: !closed, before: first != 0 || closed, after: second != 0 || closed }
    };
    let mut column = Vec::new();
    for index in 0..column_count.saturating_sub(1) {
        let intervals = (0..row_count)
            .map(|r| interval(grid.rows[r].0, grid.rows[r].1, cell(index, r), cell(index + 1, r)))
            .collect();
        column.push(GapLine { index, lo: grid.columns[index].1, hi: grid.columns[index + 1].0, intervals, lead: None, trail: None });
    }
    let mut row = Vec::new();
    for index in 0..row_count.saturating_sub(1) {
        let intervals = (0..column_count)
            .map(|c| interval(grid.columns[c].0, grid.columns[c].1, cell(c, index), cell(c, index + 1)))
            .collect();
        row.push(GapLine { index, lo: grid.rows[index].1, hi: grid.rows[index + 1].0, intervals, lead: None, trail: None });
    }
    Model { column, row, inline: grid.inline, block: grid.block }
}

struct LogicalLine {
    cross: (f32, f32),
    items: Vec<(f32, f32)>,
}

fn line_gutters(line: &LogicalLine) -> Vec<(f32, f32)> {
    line.items
        .windows(2)
        .map(|pair| if pair[0].0 <= pair[1].0 { (pair[0].1, pair[1].0) } else { (pair[1].1, pair[0].0) })
        .collect()
}

fn overlaps(range: (f32, f32), start: f32, end: f32) -> bool {
    range.0 < end - EPSILON && range.1 > start + EPSILON
}

fn flex_model(flex: &FlexInput) -> Model {
    let (main_frame, cross_frame) = if flex.main_is_inline { (flex.inline, flex.block) } else { (flex.block, flex.inline) };
    let lines: Vec<LogicalLine> = flex
        .lines
        .iter()
        .map(|line| LogicalLine {
            cross: ordered(cross_frame.logical(line.cross.0), cross_frame.logical(line.cross.1)),
            items: line.items.iter().map(|item| ordered(main_frame.logical(item.0), main_frame.logical(item.1))).collect(),
        })
        .collect();
    let main_extent = if main_frame.reverse { main_frame.extent } else { flex.main_extent };
    let mut order: Vec<usize> = (0..lines.len()).collect();
    order.sort_by(|a, b| lines[*a].cross.0.partial_cmp(&lines[*b].cross.0).unwrap_or(core::cmp::Ordering::Equal));
    let mut rank = alloc::vec![0usize; lines.len()];
    for (position, &line) in order.iter().enumerate() {
        rank[line] = position;
    }
    let mut main_lines = Vec::new();
    let mut base = 0;
    for (index, line) in lines.iter().enumerate() {
        let position = rank[index];
        let lead = position.checked_sub(1).map(|previous| (lines[order[previous]].cross.1, line.cross.0));
        let trail = order.get(position + 1).map(|next| (line.cross.1, lines[*next].cross.0));
        for (offset, (lo, hi)) in line_gutters(line).into_iter().enumerate() {
            main_lines.push(GapLine {
                index: base + offset,
                lo,
                hi,
                intervals: alloc::vec![Interval { start: line.cross.0, end: line.cross.1, open: true, before: true, after: true }],
                lead,
                trail,
            });
        }
        base += line.items.len().saturating_sub(1);
    }
    let mut cross_lines = Vec::new();
    for k in 0..lines.len().saturating_sub(1) {
        let (a, b) = (&lines[k], &lines[k + 1]);
        let (lo, hi) = if a.cross.0 <= b.cross.0 { (a.cross.1, b.cross.0) } else { (b.cross.1, a.cross.0) };
        let mut gutters = line_gutters(a);
        gutters.extend(line_gutters(b));
        gutters.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap_or(core::cmp::Ordering::Equal));
        let mut merged: Vec<(f32, f32)> = Vec::new();
        for gutter in gutters {
            match merged.last_mut() {
                Some(last) if gutter.0 <= last.1 + EPSILON => last.1 = last.1.max(gutter.1),
                _ => merged.push(gutter),
            }
        }
        let mut bounds = Vec::new();
        let mut cursor = 0.0f32;
        for (start, end) in merged {
            if start - cursor > EPSILON {
                bounds.push((cursor, start));
            }
            cursor = cursor.max(end);
        }
        if main_extent - cursor > EPSILON {
            bounds.push((cursor, main_extent));
        }
        let intervals = bounds
            .into_iter()
            .map(|(start, end)| Interval {
                start,
                end,
                open: true,
                before: a.items.iter().any(|item| overlaps(*item, start, end)),
                after: b.items.iter().any(|item| overlaps(*item, start, end)),
            })
            .collect();
        cross_lines.push(GapLine { index: k, lo, hi, intervals, lead: None, trail: None });
    }
    if flex.main_is_inline {
        Model { column: main_lines, row: cross_lines, inline: flex.inline, block: flex.block }
    } else {
        Model { column: cross_lines, row: main_lines, inline: flex.inline, block: flex.block }
    }
}

fn multicol_model(multicol: &MulticolInput) -> Model {
    let mut order: Vec<usize> = (0..multicol.rows.len()).collect();
    order.sort_by(|a, b| multicol.rows[*a].block.0.partial_cmp(&multicol.rows[*b].block.0).unwrap_or(core::cmp::Ordering::Equal));
    let mut column = Vec::new();
    let mut base = 0;
    for (position, &index) in order.iter().enumerate() {
        let row = &multicol.rows[index];
        let lead = position.checked_sub(1).map(|previous| (multicol.rows[order[previous]].block.1, row.block.0));
        let trail = order.get(position + 1).map(|next| (row.block.1, multicol.rows[*next].block.0));
        for (offset, pair) in row.columns.windows(2).enumerate() {
            column.push(GapLine {
                index: base + offset,
                lo: pair[0].1,
                hi: pair[1].0,
                intervals: alloc::vec![Interval { start: row.block.0, end: row.block.1, open: true, before: true, after: true }],
                lead,
                trail,
            });
        }
        base += row.columns.len().saturating_sub(1);
    }
    let mut row_lines = Vec::new();
    for (k, pair) in order.windows(2).enumerate() {
        let (a, b) = (&multicol.rows[pair[0]], &multicol.rows[pair[1]]);
        let mut gutters: Vec<(f32, f32)> = a.columns.windows(2).map(|p| (p[0].1, p[1].0)).collect();
        gutters.extend(b.columns.windows(2).map(|p| (p[0].1, p[1].0)));
        gutters.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap_or(core::cmp::Ordering::Equal));
        let mut merged: Vec<(f32, f32)> = Vec::new();
        for gutter in gutters {
            match merged.last_mut() {
                Some(last) if gutter.0 <= last.1 + EPSILON => last.1 = last.1.max(gutter.1),
                _ => merged.push(gutter),
            }
        }
        let mut bounds = Vec::new();
        let mut cursor = 0.0f32;
        for (start, end) in merged {
            if start - cursor > EPSILON {
                bounds.push((cursor, start));
            }
            cursor = cursor.max(end);
        }
        if multicol.inline_extent - cursor > EPSILON {
            bounds.push((cursor, multicol.inline_extent));
        }
        let intervals = bounds
            .into_iter()
            .map(|(start, end)| Interval {
                start,
                end,
                open: true,
                before: a.columns.iter().any(|column| overlaps(*column, start, end)),
                after: b.columns.iter().any(|column| overlaps(*column, start, end)),
            })
            .collect();
        row_lines.push(GapLine { index: k, lo: a.block.1, hi: b.block.0, intervals, lead: None, trail: None });
    }
    Model { column, row: row_lines, inline: multicol.inline, block: multicol.block }
}

#[derive(Clone, Copy, PartialEq)]
enum Break {
    None,
    Normal,
    Intersection,
}

#[derive(Clone, Copy, PartialEq)]
enum Visibility {
    All,
    Around,
    Between,
}

#[derive(Clone, Copy)]
enum Inset {
    Length(f32, f32),
    Join,
}

struct Props {
    widths: Vec<f32>,
    colors: Vec<Rgba>,
    styles: Vec<BorderStyle>,
    break_mode: Break,
    visibility: Visibility,
    insets: [Inset; 4],
}

fn parse_pixels(raw: &str) -> f32 {
    raw.trim().strip_suffix("px").and_then(|number| number.trim().parse::<f32>().ok()).unwrap_or(0.0)
}

fn parse_inset(raw: &str) -> Inset {
    let raw = raw.trim();
    if raw == "overlap-join" {
        return Inset::Join;
    }
    if let Some(body) = raw.strip_prefix("calc(").and_then(|rest| rest.strip_suffix(')')) {
        let mut percent = 0.0;
        let mut pixels = 0.0;
        let mut sign = 1.0;
        for token in body.split_whitespace() {
            match token {
                "+" => sign = 1.0,
                "-" => sign = -1.0,
                _ => {
                    if let Some(number) = token.strip_suffix('%') {
                        percent = sign * number.parse::<f32>().unwrap_or(0.0) / 100.0;
                    } else {
                        pixels = sign * parse_pixels(token);
                    }
                }
            }
        }
        return Inset::Length(pixels, percent);
    }
    if let Some(number) = raw.strip_suffix('%') {
        return Inset::Length(0.0, number.parse::<f32>().unwrap_or(0.0) / 100.0);
    }
    Inset::Length(parse_pixels(raw), 0.0)
}

fn read_props(style: &Style, column: bool, count: usize, normal: Visibility) -> Props {
    let prefix = if column { "column" } else { "row" };
    let stored = |suffix: &str| extra::stored(style, &alloc::format!("{prefix}-rule-{suffix}")).unwrap_or_default();
    let context = static_length_context();
    let current = style.color;
    let widths = gap_rules::resolve_items(gap_rules::Part::Width, &stored("width"), count, context, current)
        .iter()
        .map(|item| parse_pixels(item))
        .collect();
    let query = extra::query_for(context);
    let colors = gap_rules::resolve_items(gap_rules::Part::Color, &stored("color"), count, context, current)
        .iter()
        .map(|item| {
            color_values::endpoint_with_current(item, 0, color_values::from_rgba(current), context, query)
                .map_or(current, color_values::rgba)
        })
        .collect();
    let styles = gap_rules::resolve_items(gap_rules::Part::Style, &stored("style"), count, context, current)
        .iter()
        .map(|item| BorderStyle::parse(item.trim()).unwrap_or(BorderStyle::None))
        .collect();
    let break_mode = match extra::keyword(style, &alloc::format!("{prefix}-rule-break")) {
        Some("none") => Break::None,
        Some("intersection") => Break::Intersection,
        _ => Break::Normal,
    };
    let visibility = match extra::keyword(style, &alloc::format!("{prefix}-rule-visibility-items")) {
        Some("all") => Visibility::All,
        Some("around") => Visibility::Around,
        Some("between") => Visibility::Between,
        _ => normal,
    };
    let inset = |suffix: &str| {
        let raw = stored(suffix);
        parse_inset(&gap_rules::compute(gap_rules::Part::Inset, &raw, context, current).unwrap_or(raw))
    };
    let insets = [inset("inset-cap-start"), inset("inset-cap-end"), inset("inset-junction-start"), inset("inset-junction-end")];
    Props { widths, colors, styles, break_mode, visibility, insets }
}

fn shown(interval: &Interval, visibility: Visibility) -> bool {
    match visibility {
        Visibility::All => true,
        Visibility::Around => interval.before || interval.after,
        Visibility::Between => interval.before && interval.after,
    }
}

fn runs(line: &GapLine, props: &Props) -> Vec<(usize, usize)> {
    let visible: Vec<bool> = line.intervals.iter().map(|interval| interval.open && shown(interval, props.visibility)).collect();
    match props.break_mode {
        Break::None => {
            let present: Vec<usize> = line
                .intervals
                .iter()
                .enumerate()
                .filter(|(_, interval)| shown(interval, props.visibility))
                .map(|(index, _)| index)
                .collect();
            match (present.first(), present.last()) {
                (Some(first), Some(last)) => alloc::vec![(*first, *last)],
                _ => Vec::new(),
            }
        }
        Break::Intersection => visible.iter().enumerate().filter(|(_, show)| **show).map(|(index, _)| (index, index)).collect(),
        Break::Normal => {
            let mut out: Vec<(usize, usize)> = Vec::new();
            for (index, show) in visible.iter().enumerate() {
                if !show {
                    continue;
                }
                match out.last_mut() {
                    Some(last) if last.1 + 1 == index => last.1 = index,
                    _ => out.push((index, index)),
                }
            }
            out
        }
    }
}

fn perpendicular_present(perpendicular: &[GapLine], perpendicular_props: &Props, junction: (f32, f32), line: &GapLine) -> bool {
    perpendicular.iter().any(|candidate| {
        near(candidate.lo, junction.0)
            && near(candidate.hi, junction.1)
            && candidate.intervals.iter().any(|interval| {
                (near(interval.end, line.lo) || near(interval.start, line.hi))
                    && interval.open
                    && shown(interval, perpendicular_props.visibility)
            })
    })
}

fn inset_value(inset: Inset, basis: f32) -> f32 {
    match inset {
        Inset::Length(pixels, percent) => pixels + percent * basis,
        Inset::Join => 0.0,
    }
}

fn collect(model: &Model, column: bool, props: &Props, perpendicular_props: &Props, out: &mut Vec<Paint>) {
    let (lines, perpendicular) = if column { (&model.column, &model.row) } else { (&model.row, &model.column) };
    let (separation, run_frame) = if column { (model.inline, model.block) } else { (model.block, model.inline) };
    for line in lines {
        let width = props.widths.get(line.index).copied().unwrap_or(0.0);
        let style = props.styles.get(line.index).copied().unwrap_or(BorderStyle::None);
        let color = props.colors.get(line.index).copied().unwrap_or(Style::initial().color);
        if !style.paints() || width <= 0.0 || color.a == 0 {
            continue;
        }
        for (first, last) in runs(line, props) {
            let junction_before = if first > 0 { Some((line.intervals[first - 1].end, line.intervals[first].start)) } else { line.lead };
            let start = match junction_before {
                None => line.intervals[first].start + inset_value(props.insets[0], 0.0),
                Some(junction) => match props.insets[2] {
                    Inset::Join => {
                        if perpendicular_present(perpendicular, perpendicular_props, junction, line) {
                            (junction.0 + junction.1) * 0.5 - width * 0.5
                        } else {
                            line.intervals[first].start
                        }
                    }
                    inset => line.intervals[first].start + inset_value(inset, (junction.1 - junction.0).abs()),
                },
            };
            let junction_after = if last + 1 < line.intervals.len() { Some((line.intervals[last].end, line.intervals[last + 1].start)) } else { line.trail };
            let end = match junction_after {
                None => line.intervals[last].end - inset_value(props.insets[1], 0.0),
                Some(junction) => match props.insets[3] {
                    Inset::Join => {
                        if perpendicular_present(perpendicular, perpendicular_props, junction, line) {
                            (junction.0 + junction.1) * 0.5 + width * 0.5
                        } else {
                            line.intervals[last].end
                        }
                    }
                    inset => line.intervals[last].end - inset_value(inset, (junction.1 - junction.0).abs()),
                },
            };
            if end - start <= 0.0 {
                continue;
            }
            let center = (line.lo + line.hi) * 0.5;
            let (s0, s1) = ordered(separation.physical(center - width * 0.5), separation.physical(center + width * 0.5));
            let (r0, r1) = ordered(run_frame.physical(start), run_frame.physical(end));
            let rect = if separation.vertical {
                Rect { x: r0, y: s0, width: r1 - r0, height: s1 - s0 }
            } else {
                Rect { x: s0, y: r0, width: s1 - s0, height: r1 - r0 }
            };
            out.push(Paint { rect, color, style, vertical: !separation.vertical });
        }
    }
}

/// Whether the column rule is a single plain value rather than a list.
pub(crate) fn column_rule_is_plain(style: &Style) -> bool {
    ["column-rule-width", "column-rule-style", "column-rule-color"].iter().all(|name| {
        extra::stored(style, name).is_some_and(|value| !value.contains(',') && !value.contains("repeat("))
    })
}

/// Whether a multicol container needs the gap model, rather than the plain
/// column-rule painter, to draw its rules.
pub(crate) fn multicol_needs_model(style: &Style) -> bool {
    const EXTENDED: [&str; 16] = [
        "row-rule-style", "column-rule-break", "row-rule-break", "rule-overlap",
        "column-rule-visibility-items", "row-rule-visibility-items",
        "column-rule-inset-cap-start", "column-rule-inset-cap-end", "column-rule-inset-junction-start", "column-rule-inset-junction-end",
        "row-rule-inset-cap-start", "row-rule-inset-cap-end", "row-rule-inset-junction-start", "row-rule-inset-junction-end",
        "row-rule-width", "row-rule-color",
    ];
    !column_rule_is_plain(style) || EXTENDED.iter().any(|name| extra::keyword(style, name).is_some())
}

/// Whether a container can paint any gap decoration at all.
pub(crate) fn enabled(style: &Style) -> bool {
    ["column-rule-style", "row-rule-style"]
        .iter()
        .any(|name| extra::keyword(style, name).is_some_and(|value| value.trim() != "none"))
}

pub(crate) fn paints(style: &Style, input: &Input) -> Vec<Paint> {
    if !enabled(style) {
        return Vec::new();
    }
    let (model, normal) = match input {
        Input::Grid(grid) => (grid_model(grid), Visibility::All),
        Input::Flex(flex) => (flex_model(flex), Visibility::All),
        Input::Multicol(multicol) => (multicol_model(multicol), Visibility::Between),
    };
    let count = |lines: &[GapLine]| lines.iter().map(|line| line.index + 1).max().unwrap_or(0);
    let mut column_props = read_props(style, true, count(&model.column), normal);
    if let Input::Multicol(MulticolInput { column_rule: Some((width, border, color)), .. }) = input {
        let lines = count(&model.column);
        column_props.widths = alloc::vec![*width; lines];
        column_props.styles = alloc::vec![*border; lines];
        column_props.colors = alloc::vec![*color; lines];
    }
    let row_props = read_props(style, false, count(&model.row), normal);
    let mut columns = Vec::new();
    collect(&model, true, &column_props, &row_props, &mut columns);
    let mut rows = Vec::new();
    collect(&model, false, &row_props, &column_props, &mut rows);
    if extra::keyword(style, "rule-overlap") == Some("column-over-row") {
        rows.extend(columns);
        rows
    } else {
        columns.extend(rows);
        columns
    }
}
