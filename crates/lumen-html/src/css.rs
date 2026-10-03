//! Parsed author rules and a compact computed style for the initial block renderer.
use crate::{
    Document, NodeId, NodeKind,
    paint::{
        Affine, BorderPattern, BoxShadow, Gradient, GradientKind, GradientPosition, GradientStop,
        LengthPercentage, RadialShape, RadialSize, Rect, Rgba,
    },
};
use alloc::{
    boxed::Box,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Display {
    Block,
    Inline,
    Flex,
    Grid,
    Table,
    TableRowGroup,
    TableRow,
    TableCell,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FlexDirection {
    Row,
    RowReverse,
    Column,
    ColumnReverse,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum JustifyContent {
    Stretch,
    Start,
    End,
    Center,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AlignItems {
    Stretch,
    Start,
    End,
    Center,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BoxSizing {
    ContentBox,
    BorderBox,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Position {
    Static,
    Relative,
    Absolute,
    Fixed,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Float {
    None,
    Left,
    Right,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Clear {
    None,
    Left,
    Right,
    Both,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WhiteSpace {
    Normal,
    NoWrap,
    Pre,
    PreWrap,
    PreLine,
    BreakSpaces,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TextAlign {
    Start,
    End,
    Left,
    Right,
    Center,
    Justify,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Direction {
    Ltr,
    Rtl,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransformLength {
    pub pixels: f32,
    pub percent: f32,
}
impl TransformLength {
    fn resolve(self, basis: f32) -> f32 {
        self.pixels + self.percent * basis / 100.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Transform {
    Matrix(Affine),
    Translate(TransformLength, TransformLength),
    Scale(f32, f32),
    Rotate(f32),
    Skew(f32, f32),
}

pub const MAX_GRID_TRACKS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GridTrack {
    Auto,
    Pixels(f32),
    Fraction(f32),
    Percentage(f32),
    Length(f32, f32),
    MinContent,
    MaxContent,
    MinMax(GridBreadth, GridBreadth),
    FitContent(f32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GridBreadth {
    Auto,
    MinContent,
    MaxContent,
    Pixels(f32),
    Percentage(f32),
    Length(f32, f32),
    Fraction(f32),
}

impl GridTrack {
    pub fn minimum(self) -> GridBreadth {
        match self {
            Self::MinMax(min, _) => min,
            Self::Pixels(v) => GridBreadth::Pixels(v),
            Self::Percentage(v) => GridBreadth::Percentage(v),
            Self::Length(px, pct) => GridBreadth::Length(px, pct),
            Self::MinContent => GridBreadth::MinContent,
            Self::MaxContent => GridBreadth::MaxContent,
            _ => GridBreadth::Auto,
        }
    }
    pub fn maximum(self) -> GridBreadth {
        match self {
            Self::MinMax(_, max) => max,
            Self::Fraction(v) => GridBreadth::Fraction(v),
            Self::Pixels(v) => GridBreadth::Pixels(v),
            Self::Percentage(v) => GridBreadth::Percentage(v),
            Self::Length(px, pct) => GridBreadth::Length(px, pct),
            Self::MinContent => GridBreadth::MinContent,
            Self::MaxContent => GridBreadth::MaxContent,
            _ => GridBreadth::Auto,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridAutoFlow {
    pub column: bool,
    pub dense: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GridArea {
    pub name: Arc<str>,
    pub column: usize,
    pub row: usize,
    pub columns: usize,
    pub rows: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GridNamedLine {
    pub name: Arc<str>,
    pub line: usize,
}

fn grid_identifier(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 128
        && !matches!(raw, "auto" | "span")
        && raw
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'-' | b'_'))
        && !raw.as_bytes()[0].is_ascii_digit()
}

fn grid_template(
    raw: &str,
    context: LengthContext,
) -> Option<(Arc<[GridTrack]>, Option<Arc<[GridNamedLine]>>, bool)> {
    if raw == "subgrid" {
        return Some((Arc::from([]), None, true));
    }
    let mut tracks = Vec::new();
    let mut names = Vec::new();
    let mut rest = raw.trim();
    if rest == "none" {
        return Some((tracks.into(), None, false));
    }
    while !rest.is_empty() {
        if rest.starts_with('[') {
            let end = rest.find(']')?;
            for name in rest[1..end].split_ascii_whitespace() {
                if !grid_identifier(name) || names.len() == 256 {
                    return None;
                }
                names.push(GridNamedLine {
                    name: Arc::from(name),
                    line: tracks.len(),
                });
            }
            rest = rest[end + 1..].trim_start();
            continue;
        }
        let mut depth = 0usize;
        let mut end = rest.len();
        for (i, ch) in rest.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => depth = depth.checked_sub(1)?,
                '[' if depth == 0 => {
                    end = i;
                    break;
                }
                _ if ch.is_ascii_whitespace() && depth == 0 => {
                    end = i;
                    break;
                }
                _ => {}
            }
            if depth > 8 {
                return None;
            }
        }
        if depth != 0 {
            return None;
        }
        let part = &rest[..end];
        if let Some(args) = part
            .strip_prefix("repeat(")
            .and_then(|v| v.strip_suffix(')'))
        {
            let args = comma_components(args, 2)?;
            if args.len() != 2 || args[1].contains("repeat(") {
                return None;
            }
            let count = args[0]
                .parse::<usize>()
                .ok()
                .filter(|v| *v > 0 && *v <= MAX_GRID_TRACKS)?;
            let (repeat, lines, subgrid) = grid_template(args[1], context)?;
            if subgrid || repeat.is_empty() || tracks.len() + repeat.len() * count > MAX_GRID_TRACKS
            {
                return None;
            }
            for _ in 0..count {
                let offset = tracks.len();
                if let Some(lines) = &lines {
                    for line in lines.iter() {
                        if names.len() == 256 {
                            return None;
                        }
                        names.push(GridNamedLine {
                            name: line.name.clone(),
                            line: offset + line.line,
                        });
                    }
                }
                tracks.extend_from_slice(&repeat);
            }
        } else {
            let parsed = grid_tracks(part, context)?;
            if tracks.len() + parsed.len() > MAX_GRID_TRACKS {
                return None;
            }
            tracks.extend_from_slice(&parsed);
        }
        rest = rest[end..].trim_start();
    }
    if tracks.is_empty() {
        return None;
    }
    Some((
        tracks.into(),
        (!names.is_empty()).then(|| names.into()),
        false,
    ))
}

fn grid_line_spec(raw: &str) -> bool {
    let mut segments = 0;
    for segment in raw.split('/') {
        segments += 1;
        if segments > 2 {
            return false;
        }
        if segment.trim() == "auto" {
            continue;
        }
        let mut integer = false;
        let mut name = false;
        let mut span = false;
        let mut words = 0;
        for word in segment.split_ascii_whitespace() {
            words += 1;
            if word == "span" {
                if span {
                    return false;
                }
                span = true;
            } else if let Ok(value) = word.parse::<i16>() {
                if integer
                    || value == 0
                    || value.unsigned_abs() as usize > MAX_GRID_TRACKS + 1
                    || span && value < 0
                {
                    return false;
                }
                integer = true;
            } else if grid_identifier(word) {
                if name {
                    return false;
                }
                name = true;
            } else {
                return false;
            }
        }
        if words == 0 || (!integer && !name) || span && name {
            return false;
        }
    }
    true
}

pub fn resolve_grid_placement(
    fallback: GridPlacement,
    raw: Option<&str>,
    names: &[GridNamedLine],
    explicit: usize,
    areas: &[GridArea],
    horizontal: bool,
) -> Option<GridPlacement> {
    let Some(raw) = raw else {
        return Some(fallback);
    };
    let mut parts = raw.split('/');
    let first = parts.next()?;
    let second = parts.next().unwrap_or("auto");
    let parse = |raw: &str, end: bool| -> Option<(Option<usize>, usize, bool)> {
        if raw.trim() == "auto" {
            return Some((None, 1, false));
        }
        let mut count = None;
        let mut name = None;
        let mut span = false;
        for word in raw.split_ascii_whitespace() {
            if word == "span" {
                span = true;
            } else if let Ok(value) = word.parse::<i16>() {
                count = Some(value);
            } else {
                name = Some(word);
            }
        }
        if span {
            return Some((None, count.unwrap_or(1) as usize, true));
        }
        let count = count.unwrap_or(1);
        let line = if let Some(name) = name {
            let candidates: Vec<_> = names
                .iter()
                .filter(|line| line.name.as_ref() == name)
                .map(|line| line.line)
                .collect();
            if !candidates.is_empty() {
                if count > 0 {
                    *candidates.get(count as usize - 1)?
                } else {
                    *candidates.get(candidates.len().checked_sub((-count) as usize)?)?
                }
            } else if let Some(area) = areas.iter().find(|area| area.name.as_ref() == name) {
                let start = if horizontal { area.column } else { area.row };
                start
                    + if end {
                        if horizontal { area.columns } else { area.rows }
                    } else {
                        0
                    }
            } else {
                explicit.checked_add(count.max(1) as usize)?
            }
        } else if count > 0 {
            count as usize - 1
        } else {
            (explicit + 1).checked_sub((-count) as usize)?
        };
        (line <= MAX_GRID_TRACKS).then_some((Some(line), 1, false))
    };
    let (start, start_span, start_is_span) = parse(first, false)?;
    let (end, end_span, end_is_span) = parse(second, true)?;
    let placement = match (start, end) {
        (Some(start), Some(end)) => GridPlacement {
            start: Some(start.min(end)),
            span: start.abs_diff(end).max(1),
        },
        (Some(start), None) => GridPlacement {
            start: Some(start),
            span: if end_is_span { end_span } else { 1 },
        },
        (None, Some(end)) => {
            let span = if start_is_span { start_span } else { 1 };
            GridPlacement {
                start: Some(end.checked_sub(span)?),
                span,
            }
        }
        (None, None) => GridPlacement {
            start: None,
            span: if start_is_span {
                start_span
            } else if end_is_span {
                end_span
            } else {
                1
            },
        },
    };
    (placement.span > 0 && placement.start.unwrap_or(0) + placement.span <= MAX_GRID_TRACKS)
        .then_some(placement)
}

fn grid_areas(raw: &str) -> Option<Arc<[GridArea]>> {
    if raw == "none" {
        return Some(Arc::from([]));
    }
    let mut rest = raw.trim();
    let mut rows = Vec::new();
    while !rest.is_empty() {
        let quote = *rest.as_bytes().first()?;
        if !matches!(quote, b'\'' | b'"') || rows.len() == MAX_GRID_TRACKS {
            return None;
        }
        let end = rest[1..].find(quote as char)? + 1;
        let row: Vec<_> = rest[1..end].split_ascii_whitespace().collect();
        if row.is_empty()
            || row.len() > MAX_GRID_TRACKS
            || rows
                .first()
                .is_some_and(|previous: &Vec<&str>| previous.len() != row.len())
        {
            return None;
        }
        rows.push(row);
        rest = rest[end + 1..].trim_start();
    }
    let mut areas: Vec<GridArea> = Vec::new();
    for (y, row) in rows.iter().enumerate() {
        for (x, name) in row.iter().enumerate() {
            if name.bytes().all(|v| v == b'.') {
                continue;
            }
            if name.len() > 128
                || !name
                    .bytes()
                    .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'-' | b'_'))
            {
                return None;
            }
            if let Some(area) = areas.iter_mut().find(|area| area.name.as_ref() == *name) {
                area.columns = area.columns.max(x + 1 - area.column);
                area.rows = area.rows.max(y + 1 - area.row);
            } else {
                if areas.len() == MAX_GRID_TRACKS {
                    return None;
                }
                areas.push(GridArea {
                    name: Arc::from(*name),
                    column: x,
                    row: y,
                    columns: 1,
                    rows: 1,
                });
            }
        }
    }
    for area in &areas {
        for row in &rows[area.row..area.row + area.rows] {
            if row[area.column..area.column + area.columns]
                .iter()
                .any(|name| *name != area.name.as_ref())
            {
                return None;
            }
        }
    }
    Some(areas.into())
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridPlacement {
    pub start: Option<usize>,
    pub span: usize,
}

fn grid_breadth(raw: &str, context: LengthContext) -> Option<GridBreadth> {
    Some(match raw {
        "auto" => GridBreadth::Auto,
        "min-content" => GridBreadth::MinContent,
        "max-content" => GridBreadth::MaxContent,
        _ if raw.ends_with("fr") => GridBreadth::Fraction(
            raw.strip_suffix("fr")?
                .parse::<f32>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.0)?,
        ),
        _ if raw.ends_with('%') => GridBreadth::Percentage(
            raw.strip_suffix('%')?
                .parse::<f32>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.0)?,
        ),
        _ => {
            let length = transform_length(raw, context)?;
            if length.pixels < 0.0 || length.percent < 0.0 {
                return None;
            }
            if length.percent == 0.0 {
                GridBreadth::Pixels(length.pixels)
            } else {
                GridBreadth::Length(length.pixels, length.percent)
            }
        }
    })
}

fn grid_tracks(raw: &str, context: LengthContext) -> Option<Arc<[GridTrack]>> {
    let mut tracks = Vec::new();
    if raw == "none" {
        return Some(tracks.into());
    }
    for part in components(raw)? {
        if tracks.len() == MAX_GRID_TRACKS {
            return None;
        }
        if let Some(args) = part
            .strip_prefix("repeat(")
            .and_then(|v| v.strip_suffix(')'))
        {
            let args = comma_components(args, 2)?;
            if args.len() != 2 {
                return None;
            }
            let count = args[0]
                .parse::<usize>()
                .ok()
                .filter(|v| *v > 0 && *v <= MAX_GRID_TRACKS)?;
            if args[1].contains("repeat(") {
                return None;
            }
            let repeated = grid_tracks(args[1], context)?;
            if repeated.is_empty() || tracks.len() + count * repeated.len() > MAX_GRID_TRACKS {
                return None;
            }
            for _ in 0..count {
                tracks.extend_from_slice(&repeated);
            }
            continue;
        }
        tracks.push(
            if let Some(args) = part
                .strip_prefix("minmax(")
                .and_then(|v| v.strip_suffix(')'))
            {
                let args = comma_components(args, 2)?;
                if args.len() != 2 {
                    return None;
                }
                let min = grid_breadth(args[0], context)?;
                if matches!(min, GridBreadth::Fraction(_)) {
                    return None;
                }
                GridTrack::MinMax(min, grid_breadth(args[1], context)?)
            } else if let Some(args) = part
                .strip_prefix("fit-content(")
                .and_then(|v| v.strip_suffix(')'))
            {
                GridTrack::FitContent(contextual_length(args, Some(context))?.max(0.0))
            } else {
                match grid_breadth(part, context)? {
                    GridBreadth::Auto => GridTrack::Auto,
                    GridBreadth::Pixels(v) => GridTrack::Pixels(v),
                    GridBreadth::Percentage(v) => GridTrack::Percentage(v),
                    GridBreadth::Length(px, pct) => GridTrack::Length(px, pct),
                    GridBreadth::Fraction(v) => GridTrack::Fraction(v),
                    GridBreadth::MinContent => GridTrack::MinContent,
                    GridBreadth::MaxContent => GridTrack::MaxContent,
                }
            },
        );
    }
    if tracks.is_empty() {
        None
    } else {
        Some(tracks.into())
    }
}

fn grid_placement(raw: &str) -> Option<GridPlacement> {
    let mut parts = raw.split('/');
    let first = parts.next()?.trim();
    let line = |v: &str| {
        v.parse::<usize>()
            .ok()
            .filter(|v| *v > 0 && *v <= MAX_GRID_TRACKS)
    };
    let start = if first == "auto" || first.starts_with("span ") {
        None
    } else {
        Some(line(first)? - 1)
    };
    let mut span = if let Some(v) = first.strip_prefix("span ") {
        line(v.trim())?
    } else {
        1
    };
    if let Some(end) = parts.next() {
        let end = end.trim();
        span = if let Some(v) = end.strip_prefix("span ") {
            line(v.trim())?
        } else {
            line(end)?.checked_sub(start? + 1).filter(|v| *v > 0)?
        };
    }
    if parts.next().is_some() {
        return None;
    }
    Some(GridPlacement { start, span })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LineHeight {
    Normal,
    Number(f32),
    Pixels(f32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct RelativeLength {
    pixels: f32,
    percent: f32,
    nonnegative: bool,
}

#[derive(Clone, Copy)]
struct LengthContext {
    font: f32,
    root_font: f32,
    viewport: MediaEnvironment,
    percent: Option<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StyleExtras {
    pub gap_specified: bool,
    pub grid_auto_columns: Option<Arc<[GridTrack]>>,
    pub grid_auto_rows: Option<Arc<[GridTrack]>>,
    pub grid_auto_flow: GridAutoFlow,
    pub justify_items: AlignItems,
    pub justify_self: Option<AlignItems>,
    pub grid_areas: Option<Arc<[GridArea]>>,
    pub grid_area: Option<Arc<str>>,
    pub grid_column_names: Option<Arc<[GridNamedLine]>>,
    pub grid_row_names: Option<Arc<[GridNamedLine]>>,
    pub grid_column_spec: Option<Arc<str>>,
    pub grid_row_spec: Option<Arc<str>>,
    pub grid_columns_subgrid: bool,
    pub grid_rows_subgrid: bool,
    pub transforms: Option<Arc<[Transform]>>,
    pub transform_origin: [TransformLength; 2],
    pub border_pattern: Option<BorderPattern>,
    pub shadows: Option<Arc<[BoxShadow]>>,
    pub gradients: Option<Arc<[Arc<Gradient>]>>,
    pub white_space: WhiteSpace,
    pub text_align: TextAlign,
    pub direction: Direction,
    pub text_decoration: u8,
    pub root_font_size: f32,
    pub min_height: f32,
    pub max_height: Option<f32>,
    relative_lengths: Vec<(usize, RelativeLength)>,
    pub order: i32,
    pub margin_auto: [bool; 4],
    pub align_content: Option<JustifyContent>,
    pub flex_wrap_reverse: bool,
    pub align_self: Option<AlignItems>,
    pub position: Position,
    pub float: Float,
    pub clear: Clear,
    pub top: Option<f32>,
    pub right: Option<f32>,
    pub bottom: Option<f32>,
    pub left: Option<f32>,
    pub margin_sides: [f32; 4],
    pub padding_sides: [f32; 4],
    pub custom_properties: Vec<(String, Option<String>)>,
}

static INITIAL_EXTRAS: StyleExtras = StyleExtras {
    gap_specified: false,
    grid_auto_columns: None,
    grid_auto_rows: None,
    grid_auto_flow: GridAutoFlow {
        column: false,
        dense: false,
    },
    justify_items: AlignItems::Stretch,
    justify_self: None,
    grid_areas: None,
    grid_area: None,
    grid_column_names: None,
    grid_row_names: None,
    grid_column_spec: None,
    grid_row_spec: None,
    grid_columns_subgrid: false,
    grid_rows_subgrid: false,
    transforms: None,
    transform_origin: [TransformLength {
        pixels: 0.0,
        percent: 50.0,
    }; 2],
    border_pattern: None,
    shadows: None,
    gradients: None,
    white_space: WhiteSpace::Normal,
    text_align: TextAlign::Start,
    direction: Direction::Ltr,
    text_decoration: 0,
    root_font_size: 16.0,
    min_height: 0.0,
    max_height: None,
    relative_lengths: Vec::new(),
    order: 0,
    margin_auto: [false; 4],
    align_content: None,
    flex_wrap_reverse: false,
    align_self: None,
    position: Position::Static,
    float: Float::None,
    clear: Clear::None,
    top: None,
    right: None,
    bottom: None,
    left: None,
    margin_sides: [0.0; 4],
    padding_sides: [0.0; 4],
    custom_properties: Vec::new(),
};

#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    extras: Option<Arc<StyleExtras>>,
    pub display: Display,
    pub opacity: f32,
    pub color: Rgba,
    pub background: Rgba,
    pub width: Option<f32>,
    pub min_width: f32,
    pub max_width: Option<f32>,
    pub height: Option<f32>,
    pub margin: f32,
    pub padding: f32,
    pub font_size: f32,
    pub border_radius: f32,
    pub border_width: f32,
    pub border_color: Rgba,
    pub border_solid: bool,
    pub overflow_clip: bool,
    pub line_height: LineHeight,
    pub flex_direction: FlexDirection,
    pub flex_wrap: bool,
    pub justify_content: JustifyContent,
    pub align_items: AlignItems,
    pub gap: f32,
    pub flex_grow: f32,
    pub flex_shrink: f32,
    pub flex_basis: Option<f32>,
    pub box_sizing: BoxSizing,
    pub table_fixed: bool,
    pub border_spacing: f32,
    pub grid_columns: Option<Arc<[GridTrack]>>,
    pub grid_rows: Option<Arc<[GridTrack]>>,
    pub grid_column: GridPlacement,
    pub grid_row: GridPlacement,
}

impl core::ops::Deref for Style {
    type Target = StyleExtras;
    fn deref(&self) -> &StyleExtras {
        self.extras.as_deref().unwrap_or(&INITIAL_EXTRAS)
    }
}
impl core::ops::DerefMut for Style {
    fn deref_mut(&mut self) -> &mut StyleExtras {
        Arc::make_mut(
            self.extras
                .get_or_insert_with(|| Arc::new(INITIAL_EXTRAS.clone())),
        )
    }
}

impl Style {
    pub fn transform_matrix(&self, rect: Rect) -> Option<Affine> {
        let transforms = self.transforms.as_ref()?;
        let mut matrix = Affine {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 0.0,
            f: 0.0,
        };
        for transform in transforms.iter() {
            let local = match *transform {
                Transform::Matrix(matrix) => matrix,
                Transform::Translate(x, y) => Affine {
                    e: x.resolve(rect.width),
                    f: y.resolve(rect.height),
                    ..Affine::IDENTITY
                },
                Transform::Scale(x, y) => Affine {
                    a: x,
                    d: y,
                    ..Affine::IDENTITY
                },
                Transform::Rotate(angle) => {
                    let (s, c) = (libm::sinf(angle), libm::cosf(angle));
                    Affine {
                        a: c,
                        b: s,
                        c: -s,
                        d: c,
                        e: 0.0,
                        f: 0.0,
                    }
                }
                Transform::Skew(x, y) => Affine {
                    b: libm::tanf(y),
                    c: libm::tanf(x),
                    ..Affine::IDENTITY
                },
            };
            matrix = matrix.then(local);
        }
        let (x, y) = (
            rect.x + self.transform_origin[0].resolve(rect.width),
            rect.y + self.transform_origin[1].resolve(rect.height),
        );
        let matrix = matrix.translated_space(x, y);
        [matrix.a, matrix.b, matrix.c, matrix.d, matrix.e, matrix.f]
            .iter()
            .all(|v| v.is_finite())
            .then_some(matrix)
    }
    pub fn resolve_flex_basis_percentage(&self, basis: Option<f32>) -> Option<f32> {
        if let Some((_, length)) = self.relative_lengths.iter().find(|(slot, _)| *slot == 20) {
            basis
                .map(|basis| (length.pixels + length.percent * basis / 100.0).max(0.0))
                .filter(|v| v.is_finite())
        } else {
            self.flex_basis
        }
    }
    pub fn resolve_percentages(&self, width: f32, height: Option<f32>) -> Style {
        let mut result = self.clone();
        for &(slot, length) in &self.relative_lengths {
            if slot == 20 {
                result.flex_basis = None;
                continue;
            }
            let basis = if matches!(slot, 4 | 35 | 37 | 51 | 52) {
                height
            } else {
                Some(width)
            };
            let value = basis.map(|basis| length.pixels + length.percent * basis / 100.0);
            if let Some(value) = value.filter(|v| v.is_finite()) {
                if let Some(value) = length_value(
                    slot,
                    if length.nonnegative {
                        value.max(0.0)
                    } else {
                        value
                    },
                ) {
                    value.apply(&mut result);
                }
            } else if slot == 4 {
                result.height = None;
            } else if slot == 51 {
                result.min_height = 0.0;
            } else if slot == 52 {
                result.max_height = None;
            }
        }
        if !self.relative_lengths.is_empty() {
            result.relative_lengths.clear();
        }
        result
    }
    pub fn initial() -> Self {
        Self {
            extras: None,
            display: Display::Block,
            opacity: 1.0,
            color: Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            },
            background: Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 0,
            },
            width: None,
            min_width: 0.0,
            max_width: None,
            height: None,
            margin: 0.0,
            padding: 0.0,
            font_size: 16.0,
            border_radius: 0.0,
            border_width: 3.0,
            border_color: Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            },
            border_solid: false,
            overflow_clip: false,
            line_height: LineHeight::Normal,
            flex_direction: FlexDirection::Row,
            flex_wrap: false,
            justify_content: JustifyContent::Stretch,
            align_items: AlignItems::Stretch,
            gap: 0.0,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: None,
            box_sizing: BoxSizing::ContentBox,
            table_fixed: false,
            border_spacing: 2.0,
            grid_columns: None,
            grid_rows: None,
            grid_column: GridPlacement {
                start: None,
                span: 1,
            },
            grid_row: GridPlacement {
                start: None,
                span: 1,
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CssError {
    pub offset: usize,
    pub message: &'static str,
}

#[derive(Clone, Debug)]
pub(crate) struct Selector {
    languages: Vec<Vec<String>>,
    structural: Vec<StructuralPseudo>,
    root: bool,
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    specificity: (u16, u16, u16),
    ancestor: Option<(Relation, Box<Selector>)>,
    attributes: Vec<AttributeSelector>,
}

fn language_tag(value: &str) -> bool {
    if value.len() > 63 || value.is_empty() {
        return false;
    }
    let mut parts = value.split('-');
    parts.next().is_some_and(|v| {
        !v.is_empty() && v.len() <= 8 && v.bytes().all(|b| b.is_ascii_alphabetic())
    }) && parts
        .all(|v| !v.is_empty() && v.len() <= 8 && v.bytes().all(|b| b.is_ascii_alphanumeric()))
}

fn language_ranges(input: &str) -> Option<Vec<String>> {
    let mut ranges = Vec::new();
    for value in input.split(',').map(str::trim) {
        if ranges.len() == 8 {
            return None;
        }
        let quoted = value.starts_with(['\'', '"']);
        let value = if quoted {
            if value.len() < 2 || value.as_bytes().first() != value.as_bytes().last() {
                return None;
            }
            &value[1..value.len() - 1]
        } else {
            value
        };
        if !language_tag(value) && !(quoted && value.is_empty()) {
            return None;
        }
        ranges.push(value.to_ascii_lowercase());
    }
    Some(ranges)
}

#[derive(Clone, Debug)]
enum StructuralPseudo {
    Nth(i32, i32, bool, bool),
    Last(bool),
    Only(bool),
}

fn nth_expression(input: &str) -> Option<(i32, i32)> {
    let input: String = input
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    match input.as_str() {
        "odd" => return Some((2, 1)),
        "even" => return Some((2, 0)),
        _ => {}
    }
    if let Some((a, b)) = input.split_once('n') {
        let a = match a {
            "" | "+" => 1,
            "-" => -1,
            _ => a.parse().ok()?,
        };
        let b = if b.is_empty() {
            0
        } else {
            if !b.starts_with(['+', '-']) {
                return None;
            }
            b.parse().ok()?
        };
        Some((a, b))
    } else {
        Some((0, input.parse().ok()?))
    }
}

#[derive(Clone, Debug)]
struct AttributeSelector {
    name: String,
    value: Option<String>,
}

#[derive(Clone, Copy, Debug)]
enum Relation {
    Descendant,
    Child,
    Adjacent,
    Following,
}

#[derive(Clone, Debug)]
enum Value {
    Transforms(Option<Arc<[Transform]>>),
    TransformRaw(String),
    TransformOrigin([TransformLength; 2]),
    TransformOriginRaw(String),
    BorderPattern(BorderPattern),
    BorderCurrentColor,
    Shadows(Option<Arc<[BoxShadow]>>),
    ShadowsRaw(String),
    WhiteSpace(WhiteSpace),
    TextAlign(TextAlign),
    Direction(Direction),
    TextDecoration(u8),
    GradientNone,
    GradientRaw(String),

    ContextLength(usize, String, bool),
    MinHeight(f32),
    MaxHeight(Option<f32>),
    Order(i32),
    AlignContent(Option<JustifyContent>),
    FlexWrapReverse(bool),
    MarginAuto(usize),
    AlignSelf(Option<AlignItems>),
    Position(Position),
    Float(Float),
    Clear(Clear),
    Offset(usize, Option<f32>),
    MarginSide(usize, f32),
    PaddingSide(usize, f32),
    Custom(String, String),
    Deferred(String, String),
    Default(usize, bool),
    RevertLayer(usize),
    Revert(usize),
    Display(Display),
    Opacity(f32),
    Color(Rgba),
    Background(Rgba),
    Width(f32),
    MinWidth(f32),
    MaxWidth(Option<f32>),
    Height(f32),
    Margin(f32),
    Padding(f32),
    FontSize(f32),
    BorderRadius(f32),
    BorderWidth(f32),
    BorderColor(Rgba),
    BorderSolid(bool),
    OverflowClip(bool),
    LineHeight(LineHeight),
    FlexDirection(FlexDirection),
    FlexWrap(bool),
    JustifyContent(JustifyContent),
    AlignItems(AlignItems),
    Gap(f32),
    FlexGrow(f32),
    FlexShrink(f32),
    FlexBasis(Option<f32>),
    BoxSizing(BoxSizing),
    TableFixed(bool),
    BorderSpacing(f32),
    GridColumns(Arc<[GridTrack]>, Option<Arc<[GridNamedLine]>>, bool),
    GridRows(Arc<[GridTrack]>, Option<Arc<[GridNamedLine]>>, bool),
    GridRaw(usize, Arc<str>),
    GridColumnSpec(Arc<str>),
    GridRowSpec(Arc<str>),
    GridColumn(GridPlacement),
    GridRow(GridPlacement),
    GridAutoColumns(Arc<[GridTrack]>),
    GridAutoRows(Arc<[GridTrack]>),
    GridAutoFlow(GridAutoFlow),
    JustifyItems(AlignItems),
    JustifySelf(Option<AlignItems>),
    GridAreas(Arc<[GridArea]>),
    GridArea(Option<Arc<str>>),
}

impl Value {
    fn slot(&self) -> usize {
        match self {
            Self::GridRaw(slot, _) => *slot,
            Self::GridAutoColumns(_) => 61,
            Self::GridAutoRows(_) => 62,
            Self::GridAutoFlow(_) => 63,
            Self::JustifyItems(_) => 64,
            Self::JustifySelf(_) => 65,
            Self::GridAreas(_) => 66,
            Self::GridArea(_) => 67,
            Self::Transforms(_) | Self::TransformRaw(_) => 59,
            Self::TransformOrigin(_) | Self::TransformOriginRaw(_) => 60,
            Self::BorderPattern(_) => 11,
            Self::BorderCurrentColor => 10,
            Self::Shadows(_) | Self::ShadowsRaw(_) => 58,
            Self::WhiteSpace(_) => 54,
            Self::TextAlign(_) => 55,
            Self::Direction(_) => 56,
            Self::TextDecoration(_) => 57,
            Self::GradientNone | Self::GradientRaw(_) => 53,

            Self::ContextLength(slot, _, _) => *slot,
            Self::MinHeight(_) => 51,
            Self::MaxHeight(_) => 52,
            Self::Order(_) => 48,
            Self::AlignContent(_) => 49,
            Self::FlexWrapReverse(_) => 50,
            Self::MarginAuto(side) => 39 + side,
            Self::AlignSelf(_) => 47,
            Self::Position(_) => 32,
            Self::Float(_) => 33,
            Self::Clear(_) => 34,
            Self::Offset(side, _) => 35 + side,
            Self::MarginSide(side, _) => 39 + side,
            Self::PaddingSide(side, _) => 43 + side,
            Self::Default(slot, _) => *slot,
            Self::RevertLayer(slot) => *slot,
            Self::Revert(slot) => *slot,
            Self::Custom(_, _) | Self::Deferred(_, _) => unreachable!(),
            Self::Display(_) => 0,
            Self::Opacity(_) => 31,
            Self::Color(_) => 1,
            Self::Background(_) => 2,
            Self::Width(_) => 3,
            Self::MinWidth(_) => 23,
            Self::MaxWidth(_) => 24,
            Self::Height(_) => 4,
            Self::Margin(_) => 5,
            Self::Padding(_) => 6,
            Self::FontSize(_) => 7,
            Self::BorderRadius(_) => 8,
            Self::BorderWidth(_) => 9,
            Self::BorderColor(_) => 10,
            Self::BorderSolid(_) => 11,
            Self::OverflowClip(_) => 12,
            Self::LineHeight(_) => 13,
            Self::FlexDirection(_) => 14,
            Self::FlexWrap(_) => 22,
            Self::JustifyContent(_) => 15,
            Self::AlignItems(_) => 16,
            Self::Gap(_) => 17,
            Self::FlexGrow(_) => 18,
            Self::FlexShrink(_) => 19,
            Self::FlexBasis(_) => 20,
            Self::BoxSizing(_) => 21,
            Self::TableFixed(_) => 25,
            Self::BorderSpacing(_) => 26,
            Self::GridColumns(..) => 27,
            Self::GridRows(..) => 28,
            Self::GridColumn(_) | Self::GridColumnSpec(_) => 29,
            Self::GridRow(_) | Self::GridRowSpec(_) => 30,
        }
    }
    fn apply(&self, style: &mut Style) {
        match self {
            Self::Transforms(v) => {
                if &style.transforms != v {
                    style.transforms = v.clone();
                }
            }
            Self::TransformOrigin(v) => {
                if style.transform_origin != *v {
                    style.transform_origin = *v;
                }
            }
            Self::TransformRaw(_) | Self::TransformOriginRaw(_) => unreachable!(),
            Self::BorderPattern(v) => {
                style.border_solid = true;
                style.border_pattern = Some(*v);
            }
            Self::BorderCurrentColor => style.border_color = style.color,
            Self::Shadows(v) => {
                if &style.shadows != v {
                    style.shadows = v.clone();
                }
            }
            Self::ShadowsRaw(_) => unreachable!(),
            Self::WhiteSpace(v) => {
                if style.white_space != *v {
                    style.white_space = *v;
                }
            }
            Self::TextAlign(v) => {
                if style.text_align != *v {
                    style.text_align = *v;
                }
            }
            Self::Direction(v) => {
                if style.direction != *v {
                    style.direction = *v;
                }
            }
            Self::TextDecoration(v) => {
                if style.text_decoration != *v {
                    style.text_decoration = *v;
                }
            }
            Self::GradientNone => {
                if style.gradients.is_some() {
                    style.gradients = None;
                }
            }
            Self::GradientRaw(_) => unreachable!(),

            Self::ContextLength(_, _, _) => unreachable!(),
            Self::MinHeight(v) => style.min_height = *v,
            Self::MaxHeight(v) => style.max_height = *v,
            Self::Order(v) => style.order = *v,
            Self::AlignContent(v) => style.align_content = *v,
            Self::FlexWrapReverse(v) => style.flex_wrap_reverse = *v,
            Self::MarginAuto(side) => {
                style.margin_sides[*side] = 0.0;
                style.margin_auto[*side] = true;
            }
            Self::AlignSelf(v) => style.align_self = *v,
            Self::Position(v) => style.position = *v,
            Self::Float(v) => style.float = *v,
            Self::Clear(v) => style.clear = *v,
            Self::Offset(side, v) => match side {
                0 => style.top = *v,
                1 => style.right = *v,
                2 => style.bottom = *v,
                _ => style.left = *v,
            },
            Self::MarginSide(side, v) => {
                style.margin_sides[*side] = *v;
                style.margin_auto[*side] = false;
            }
            Self::PaddingSide(side, v) => style.padding_sides[*side] = *v,
            Self::Custom(_, _)
            | Self::Deferred(_, _)
            | Self::Default(_, _)
            | Self::RevertLayer(_)
            | Self::Revert(_) => unreachable!(),
            Self::Display(v) => style.display = *v,
            Self::Opacity(v) => style.opacity = *v,
            Self::Color(v) => style.color = *v,
            Self::Background(v) => style.background = *v,
            Self::Width(v) => style.width = Some(*v),
            Self::MinWidth(v) => style.min_width = *v,
            Self::MaxWidth(v) => style.max_width = *v,
            Self::Height(v) => style.height = Some(*v),
            Self::Margin(v) => style.margin = *v,
            Self::Padding(v) => style.padding = *v,
            Self::FontSize(v) => style.font_size = *v,
            Self::BorderRadius(v) => style.border_radius = *v,
            Self::BorderWidth(v) => style.border_width = *v,
            Self::BorderColor(v) => style.border_color = *v,
            Self::BorderSolid(v) => {
                style.border_solid = *v;
                if style.border_pattern.is_some() {
                    style.border_pattern = None;
                }
            }
            Self::OverflowClip(v) => style.overflow_clip = *v,
            Self::LineHeight(v) => style.line_height = *v,
            Self::FlexDirection(v) => style.flex_direction = *v,
            Self::FlexWrap(v) => style.flex_wrap = *v,
            Self::JustifyContent(v) => style.justify_content = *v,
            Self::AlignItems(v) => style.align_items = *v,
            Self::Gap(v) => {
                style.gap = *v;
                style.gap_specified = true;
            }
            Self::FlexGrow(v) => style.flex_grow = *v,
            Self::FlexShrink(v) => style.flex_shrink = *v,
            Self::FlexBasis(v) => style.flex_basis = *v,
            Self::BoxSizing(v) => style.box_sizing = *v,
            Self::TableFixed(v) => style.table_fixed = *v,
            Self::BorderSpacing(v) => style.border_spacing = *v,
            Self::GridRaw(..) => unreachable!(),
            Self::GridAutoColumns(v) => style.grid_auto_columns = Some(v.clone()),
            Self::GridAutoRows(v) => style.grid_auto_rows = Some(v.clone()),
            Self::GridAutoFlow(v) => style.grid_auto_flow = *v,
            Self::JustifyItems(v) => style.justify_items = *v,
            Self::JustifySelf(v) => style.justify_self = *v,
            Self::GridAreas(v) => style.grid_areas = Some(v.clone()),
            Self::GridArea(v) => style.grid_area = v.clone(),
            Self::GridColumns(v, names, subgrid) => {
                style.grid_columns = Some(v.clone());
                style.grid_column_names = names.clone();
                style.grid_columns_subgrid = *subgrid;
            }
            Self::GridRows(v, names, subgrid) => {
                style.grid_rows = Some(v.clone());
                style.grid_row_names = names.clone();
                style.grid_rows_subgrid = *subgrid;
            }
            Self::GridColumn(v) => {
                style.grid_column = *v;
                style.grid_column_spec = None;
            }
            Self::GridRow(v) => {
                style.grid_row = *v;
                style.grid_row_spec = None;
            }
            Self::GridColumnSpec(v) => {
                style.grid_column = GridPlacement {
                    start: None,
                    span: 1,
                };
                style.grid_column_spec = Some(v.clone());
            }
            Self::GridRowSpec(v) => {
                style.grid_row = GridPlacement {
                    start: None,
                    span: 1,
                };
                style.grid_row_spec = Some(v.clone());
            }
        }
    }
}

#[derive(Clone, Debug)]
struct Declaration {
    value: Value,
    important: bool,
}

#[derive(Clone, Debug)]
pub struct Rule {
    selector: Selector,
    declarations: Arc<[Declaration]>,
    media: Vec<Arc<str>>,
    layer: Option<usize>,
    layers: Arc<[String]>,
    layer_path: Option<[usize; 8]>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MediaEnvironment {
    pub width: f32,
    pub height: f32,
    pub resolution: f32,
    pub print: bool,
}
impl Default for MediaEnvironment {
    fn default() -> Self {
        Self {
            width: 1024.0,
            height: 768.0,
            resolution: 1.0,
            print: false,
        }
    }
}

pub struct StyleIndex {
    pub environment: MediaEnvironment,
    rules: Vec<Rule>,
    tags: Vec<usize>,
    ids: Vec<usize>,
    classes: Vec<usize>,
    universal: Vec<usize>,
}

pub(crate) const MAX_RULES: usize = 4096;
const MAX_DECLARATIONS: usize = 128;
const MAX_SELECTOR_BYTES: usize = 256;
const MAX_CSS_BYTES: usize = 1024 * 1024;

impl StyleIndex {
    pub fn new(mut rules: Vec<Rule>) -> Self {
        let mut layers: Vec<String> = Vec::new();
        let mut previous: Option<Arc<[String]>> = None;
        let mut sheet = 0usize;
        for rule in &mut rules {
            if previous
                .as_ref()
                .is_none_or(|previous| !Arc::ptr_eq(previous, &rule.layers))
            {
                sheet += 1;
                for name in rule.layers.iter() {
                    let mut prefix = String::new();
                    for part in name.split('.') {
                        if !prefix.is_empty() {
                            prefix.push('.');
                        }
                        if part.starts_with('#') {
                            prefix.push_str(&alloc::format!("${sheet}{part}"));
                        } else {
                            prefix.push_str(part);
                        }
                        if !layers.contains(&prefix) {
                            layers.push(prefix.clone());
                        }
                    }
                }
                previous = Some(rule.layers.clone());
            }
            if let Some(layer) = rule.layer {
                let name = &rule.layers[layer];
                let mut prefix = String::new();
                let mut path = [usize::MAX - 1; 8];
                for (depth, part) in name.split('.').enumerate() {
                    if !prefix.is_empty() {
                        prefix.push('.');
                    }
                    if part.starts_with('#') {
                        prefix.push_str(&alloc::format!("${sheet}{part}"));
                    } else {
                        prefix.push_str(part);
                    }
                    if depth < 8 {
                        path[depth] = layers.iter().position(|v| v == &prefix).unwrap() + 1;
                    }
                }
                rule.layer_path = Some(path);
            }
        }
        let (mut tags, mut ids, mut classes, mut universal) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (index, rule) in rules.iter().enumerate() {
            if rule.selector.id.is_some() {
                ids.push(index);
            } else if !rule.selector.classes.is_empty() {
                classes.push(index);
            } else if rule.selector.tag.is_some() {
                tags.push(index);
            } else {
                universal.push(index);
            }
        }
        tags.sort_unstable_by(|&a, &b| rules[a].selector.tag.cmp(&rules[b].selector.tag));
        ids.sort_unstable_by(|&a, &b| rules[a].selector.id.cmp(&rules[b].selector.id));
        classes.sort_unstable_by(|&a, &b| {
            rules[a].selector.classes[0].cmp(&rules[b].selector.classes[0])
        });
        Self {
            environment: MediaEnvironment::default(),
            rules,
            tags,
            ids,
            classes,
            universal,
        }
    }

    fn matches<'a>(&'a self, entries: &'a [usize], key: &str, field: u8) -> &'a [usize] {
        let name = |index: usize| -> &str {
            let selector = &self.rules[index].selector;
            match field {
                0 => selector.tag.as_deref().unwrap(),
                1 => selector.id.as_deref().unwrap(),
                _ => &selector.classes[0],
            }
        };
        let start = entries.partition_point(|&index| name(index) < key);
        let end = start + entries[start..].partition_point(|&index| name(index) == key);
        &entries[start..end]
    }
}

// Consume an escape, including the optional whitespace after a hexadecimal escape.
fn selector_escape(input: &str, pos: &mut usize) -> Option<char> {
    *pos += 1;
    let start = *pos;
    let bytes = input.as_bytes();
    while *pos < bytes.len() && *pos - start < 6 && bytes[*pos].is_ascii_hexdigit() {
        *pos += 1;
    }
    if *pos != start {
        let value = u32::from_str_radix(&input[start..*pos], 16).ok()?;
        if bytes.get(*pos).is_some_and(u8::is_ascii_whitespace) {
            let cr = bytes[*pos] == b'\r';
            *pos += 1;
            if cr && bytes.get(*pos) == Some(&b'\n') {
                *pos += 1;
            }
        }
        return Some(
            char::from_u32(value)
                .filter(|ch| *ch != '\0')
                .unwrap_or('\u{fffd}'),
        );
    }
    let ch = input.get(*pos..)?.chars().next()?;
    if matches!(ch, '\n' | '\r' | '\u{c}') {
        return None;
    }
    *pos += ch.len_utf8();
    Some(ch)
}

pub(crate) fn parse_selector_list(input: &str, offset: usize) -> Result<Vec<Selector>, CssError> {
    if input.len() > MAX_SELECTOR_BYTES {
        return Err(CssError {
            offset,
            message: "selector too large",
        });
    }
    let (mut start, mut pos, mut brackets, mut parentheses, mut quote) =
        (0, 0, 0usize, 0usize, 0u8);
    let bytes = input.as_bytes();
    let mut result = Vec::new();
    while pos < bytes.len() {
        let byte = bytes[pos];
        if byte == b'\\' {
            selector_escape(input, &mut pos).ok_or(CssError {
                offset: offset + pos,
                message: "invalid selector escape",
            })?;
            continue;
        }
        if quote != 0 {
            if byte == quote {
                quote = 0;
            }
        } else if matches!(byte, b'\'' | b'"') {
            quote = byte;
        } else if byte == b'[' {
            brackets += 1;
        } else if byte == b']' {
            brackets = brackets.saturating_sub(1);
        } else if byte == b'(' {
            parentheses += 1;
        } else if byte == b')' {
            parentheses = parentheses.saturating_sub(1);
        } else if byte == b',' && brackets == 0 && parentheses == 0 {
            result.push(parse_selector(&input[start..pos], offset + start)?);
            start = pos + 1;
        }
        pos += 1;
    }
    result.push(parse_selector(&input[start..], offset + start)?);
    Ok(result)
}

pub(crate) fn parse_selector(input: &str, offset: usize) -> Result<Selector, CssError> {
    let mut input = input.trim_ascii_start();
    while input.as_bytes().last().is_some_and(u8::is_ascii_whitespace) {
        let last = input.len() - 1;
        if input.as_bytes()[..last]
            .iter()
            .rev()
            .take_while(|&&byte| byte == b'\\')
            .count()
            % 2
            != 0
        {
            break;
        }
        input = &input[..last];
    }
    if input.len() > MAX_SELECTOR_BYTES {
        return Err(CssError {
            offset,
            message: "selector too large",
        });
    }
    let bytes = input.as_bytes();
    let mut pos = 0;
    let mut previous = None;
    let mut relation = Relation::Descendant;
    loop {
        let start = pos;
        let (mut brackets, mut parentheses, mut quote) = (0usize, 0usize, 0u8);
        while pos < bytes.len() {
            let byte = bytes[pos];
            if byte == b'\\' {
                selector_escape(input, &mut pos).ok_or(CssError {
                    offset: offset + pos,
                    message: "invalid selector escape",
                })?;
                continue;
            }
            if quote != 0 {
                if byte == quote {
                    quote = 0;
                }
            } else if brackets != 0 && matches!(byte, b'\'' | b'"') {
                quote = byte;
            } else if byte == b'[' {
                brackets += 1;
            } else if byte == b']' {
                brackets = brackets.saturating_sub(1);
            } else if byte == b'(' {
                parentheses += 1;
            } else if byte == b')' {
                parentheses = parentheses.saturating_sub(1);
            } else if brackets == 0
                && parentheses == 0
                && (byte.is_ascii_whitespace() || matches!(byte, b'>' | b'+' | b'~'))
            {
                break;
            }
            pos += 1;
        }
        let mut current = parse_simple_selector(&input[start..pos], offset + start)?;
        if let Some(previous) = previous {
            let previous: Selector = previous;
            current.specificity.0 += previous.specificity.0;
            current.specificity.1 += previous.specificity.1;
            current.specificity.2 += previous.specificity.2;
            current.ancestor = Some((relation, Box::new(previous)));
        }
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos == bytes.len() {
            return Ok(current);
        }
        relation = match bytes[pos] {
            b'>' => {
                pos += 1;
                Relation::Child
            }
            b'+' => {
                pos += 1;
                Relation::Adjacent
            }
            b'~' => {
                pos += 1;
                Relation::Following
            }
            _ => Relation::Descendant,
        };
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        previous = Some(current);
    }
}

fn parse_simple_selector(input: &str, offset: usize) -> Result<Selector, CssError> {
    if input.len() > MAX_SELECTOR_BYTES {
        return Err(CssError {
            offset,
            message: "selector too large",
        });
    }
    if input == "*" {
        return Ok(Selector {
            languages: Vec::new(),
            structural: Vec::new(),
            root: false,
            tag: None,
            id: None,
            classes: Vec::new(),
            specificity: (0, 0, 0),
            ancestor: None,
            attributes: Vec::new(),
        });
    }
    let mut selector = Selector {
        languages: Vec::new(),
        structural: Vec::new(),
        root: false,
        tag: None,
        id: None,
        classes: Vec::new(),
        specificity: (0, 0, 0),
        ancestor: None,
        attributes: Vec::new(),
    };
    let mut start = usize::from(input.starts_with('*'));
    let bytes = input.as_bytes();
    while start < bytes.len() {
        let kind = bytes[start];
        if kind == b':' {
            let rest = &input[start..];
            let (name, args, end) = if let Some(open) = rest.find('(').filter(|&open| {
                rest[..open]
                    .bytes()
                    .all(|v| v == b':' || v == b'-' || v.is_ascii_alphabetic())
            }) {
                let close = rest[open + 1..].find(')').ok_or(CssError {
                    offset: offset + start,
                    message: "unterminated pseudo-class",
                })? + open
                    + 1;
                (&rest[1..open], Some(&rest[open + 1..close]), close + 1)
            } else {
                let end = 1 + rest[1..]
                    .bytes()
                    .take_while(|v| v.is_ascii_alphabetic() || *v == b'-')
                    .count();
                (&rest[1..end], None, end)
            };
            let pseudo = match name {
                "lang" => {
                    let ranges = args.and_then(language_ranges).ok_or(CssError {
                        offset: offset + start,
                        message: "invalid language range",
                    })?;
                    selector.languages.push(ranges);
                    selector.specificity.1 += 1;
                    start += end;
                    continue;
                }
                "first-child" if args.is_none() => Some(StructuralPseudo::Nth(0, 1, false, false)),
                "first-of-type" if args.is_none() => Some(StructuralPseudo::Nth(0, 1, true, false)),
                "last-child" if args.is_none() => Some(StructuralPseudo::Last(false)),
                "last-of-type" if args.is_none() => Some(StructuralPseudo::Last(true)),
                "only-child" if args.is_none() => Some(StructuralPseudo::Only(false)),
                "only-of-type" if args.is_none() => Some(StructuralPseudo::Only(true)),
                "nth-child" | "nth-of-type" | "nth-last-child" | "nth-last-of-type" => {
                    let (a, b) = args.and_then(nth_expression).ok_or(CssError {
                        offset: offset + start,
                        message: "invalid nth pseudo-class",
                    })?;
                    Some(StructuralPseudo::Nth(
                        a,
                        b,
                        name.ends_with("of-type"),
                        name.starts_with("nth-last"),
                    ))
                }
                _ => None,
            };
            if let Some(pseudo) = pseudo {
                selector.structural.push(pseudo);
                selector.specificity.1 += 1;
                start += end;
                continue;
            }
        }
        if input[start..].starts_with(":root")
            && input
                .as_bytes()
                .get(start + 5)
                .is_none_or(|v| matches!(v, b'.' | b'#' | b'[' | b':'))
        {
            selector.root = true;
            selector.specificity.1 += 1;
            start += 5;
            continue;
        }
        if kind == b'[' {
            let mut end = start + 1;
            let mut quote = 0u8;
            while end < bytes.len() {
                let byte = bytes[end];
                if byte == b'\\' {
                    return Err(CssError {
                        offset: offset + end,
                        message: "unsupported selector escape",
                    });
                }
                if quote != 0 {
                    if byte == quote {
                        quote = 0;
                    }
                } else if matches!(byte, b'\'' | b'"') {
                    quote = byte;
                } else if byte == b']' {
                    break;
                }
                end += 1;
            }
            if end == bytes.len() {
                return Err(CssError {
                    offset: offset + start,
                    message: "unterminated attribute selector",
                });
            }
            let content = input[start + 1..end].trim();
            let (name, value) = if let Some((name, value)) = content.split_once('=') {
                let value = value.trim();
                let value = if value.starts_with(['\'', '"']) {
                    if value.len() < 2 || value.as_bytes().first() != value.as_bytes().last() {
                        return Err(CssError {
                            offset,
                            message: "invalid attribute selector value",
                        });
                    }
                    &value[1..value.len() - 1]
                } else {
                    if value.is_empty() || value.chars().any(char::is_whitespace) {
                        return Err(CssError {
                            offset,
                            message: "invalid attribute selector value",
                        });
                    }
                    value
                };
                (name.trim(), Some(value.to_string()))
            } else {
                (content, None)
            };
            if name.is_empty()
                || !name
                    .chars()
                    .all(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_' | ':'))
            {
                return Err(CssError {
                    offset,
                    message: "unsupported attribute selector",
                });
            }
            selector.attributes.push(AttributeSelector {
                name: name.to_ascii_lowercase(),
                value,
            });
            selector.specificity.1 += 1;
            start = end + 1;
            continue;
        }
        let begin = if kind == b'.' || kind == b'#' {
            start + 1
        } else {
            start
        };
        let mut end = begin;
        let mut name = String::new();
        while end < bytes.len() {
            if bytes[end] == b'\\' {
                name.push(selector_escape(input, &mut end).ok_or(CssError {
                    offset: offset + end,
                    message: "invalid selector escape",
                })?);
            } else {
                let ch = input[end..].chars().next().unwrap();
                if !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') || !ch.is_ascii()) {
                    break;
                }
                name.push(ch);
                end += ch.len_utf8();
            }
        }
        if end == begin {
            return Err(CssError {
                offset,
                message: "unsupported selector",
            });
        }
        if name == "-"
            || bytes[begin].is_ascii_digit()
            || (bytes[begin] == b'-' && bytes.get(begin + 1).is_some_and(u8::is_ascii_digit))
        {
            return Err(CssError {
                offset: offset + begin,
                message: "invalid identifier",
            });
        }
        match kind {
            b'.' => {
                selector.classes.push(name);
                selector.specificity.1 += 1;
            }
            b'#' if selector.id.is_none() => {
                selector.id = Some(name);
                selector.specificity.0 += 1;
            }
            _ if !matches!(kind, b'.' | b'#') && selector.tag.is_none() => {
                selector.tag = Some(name.to_ascii_lowercase());
                selector.specificity.2 += 1;
            }
            _ => {
                return Err(CssError {
                    offset,
                    message: "unsupported selector",
                });
            }
        }
        start = end;
    }
    if selector.specificity == (0, 0, 0) {
        return Err(CssError {
            offset,
            message: "empty selector",
        });
    }
    Ok(selector)
}

fn length(input: &str) -> Option<f32> {
    contextual_length(input, None)
}

fn contextual_length(input: &str, context: Option<LengthContext>) -> Option<f32> {
    let input = input.trim();
    // Absolute callers omit context; computed styles supply real font/viewport bases.
    // Bound source size and recursive nesting; evaluation allocates nothing.
    if input.len() > 1024 {
        return None;
    }
    let mut parser = LengthParser {
        input,
        pos: 0,
        context,
    };
    let calculated = input
        .get(..5)
        .is_some_and(|v| v.eq_ignore_ascii_case("calc("));
    if input.starts_with('(') {
        return None;
    }
    let (value, dimension) = parser.atom(0)?;
    (parser.pos == input.len() && (dimension || (!calculated && value == 0.0))).then_some(value)
}

struct LengthParser<'a> {
    input: &'a str,
    pos: usize,
    context: Option<LengthContext>,
}

fn nonnegative_length(input: &str) -> Option<f32> {
    let value = length(input)?;
    if input
        .trim()
        .get(..5)
        .is_some_and(|v| v.eq_ignore_ascii_case("calc("))
    {
        Some(value.max(0.0))
    } else {
        (value >= 0.0).then_some(value)
    }
}

impl LengthParser<'_> {
    fn space(&mut self) {
        while self
            .input
            .as_bytes()
            .get(self.pos)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.pos += 1;
        }
    }

    fn atom(&mut self, depth: usize) -> Option<(f32, bool)> {
        if depth > 16 {
            return None;
        }
        self.space();
        let rest = &self.input[self.pos..];
        let calc = rest
            .get(..5)
            .is_some_and(|v| v.eq_ignore_ascii_case("calc("));
        if calc || rest.starts_with('(') {
            self.pos += if calc { 5 } else { 1 };
            let value = self.sum(depth + 1)?;
            self.space();
            if self.input.as_bytes().get(self.pos) != Some(&b')') {
                return None;
            }
            self.pos += 1;
            return Some(value);
        }
        let start = self.pos;
        let bytes = self.input.as_bytes();
        if matches!(bytes.get(self.pos), Some(b'+' | b'-')) {
            self.pos += 1;
        }
        let digits = self.pos;
        while bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
        }
        let mut has_digit = self.pos != digits;
        if bytes.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            let decimal = self.pos;
            while bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
                self.pos += 1;
            }
            if decimal == self.pos {
                return None;
            }
            has_digit = true;
        }
        if !has_digit {
            return None;
        }
        if matches!(bytes.get(self.pos), Some(b'e' | b'E'))
            && (bytes.get(self.pos + 1).is_some_and(u8::is_ascii_digit)
                || (matches!(bytes.get(self.pos + 1), Some(b'+' | b'-'))
                    && bytes.get(self.pos + 2).is_some_and(u8::is_ascii_digit)))
        {
            self.pos += 1;
            if matches!(bytes.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            let exponent = self.pos;
            while bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
                self.pos += 1;
            }
            if exponent == self.pos {
                return None;
            }
        }
        let number = self.input[start..self.pos].parse::<f32>().ok()?;
        let unit = self.pos;
        while bytes.get(self.pos).is_some_and(u8::is_ascii_alphabetic) {
            self.pos += 1;
        }
        if bytes.get(self.pos) == Some(&b'%') {
            self.pos += 1;
        }
        let unit = &self.input[unit..self.pos];
        let scale = if unit.is_empty() {
            1.0
        } else if unit.eq_ignore_ascii_case("px") {
            1.0
        } else if unit.eq_ignore_ascii_case("in") {
            96.0
        } else if unit.eq_ignore_ascii_case("cm") {
            96.0 / 2.54
        } else if unit.eq_ignore_ascii_case("mm") {
            96.0 / 25.4
        } else if unit.eq_ignore_ascii_case("q") {
            96.0 / 101.6
        } else if unit.eq_ignore_ascii_case("pt") {
            96.0 / 72.0
        } else if unit.eq_ignore_ascii_case("pc") {
            16.0
        } else if unit.eq_ignore_ascii_case("em") {
            self.context?.font
        } else if unit.eq_ignore_ascii_case("rem") {
            self.context?.root_font
        } else if unit.eq_ignore_ascii_case("vw") {
            self.context?.viewport.width / 100.0
        } else if unit.eq_ignore_ascii_case("vh") {
            self.context?.viewport.height / 100.0
        } else if unit.eq_ignore_ascii_case("vmin") {
            self.context?
                .viewport
                .width
                .min(self.context?.viewport.height)
                / 100.0
        } else if unit.eq_ignore_ascii_case("vmax") {
            self.context?
                .viewport
                .width
                .max(self.context?.viewport.height)
                / 100.0
        } else if unit == "%" {
            self.context?.percent? / 100.0
        } else {
            return None;
        };
        let value = number * scale;
        value.is_finite().then_some((value, !unit.is_empty()))
    }

    fn product(&mut self, depth: usize) -> Option<(f32, bool)> {
        let mut left = self.atom(depth)?;
        loop {
            self.space();
            let Some(&op @ (b'*' | b'/')) = self.input.as_bytes().get(self.pos) else {
                return Some(left);
            };
            self.pos += 1;
            let right = self.atom(depth)?;
            left = match op {
                b'*' if !(left.1 && right.1) => (left.0 * right.0, left.1 || right.1),
                b'/' if !right.1 && right.0 != 0.0 => (left.0 / right.0, left.1),
                _ => return None,
            };
            if !left.0.is_finite() {
                return None;
            }
        }
    }

    fn sum(&mut self, depth: usize) -> Option<(f32, bool)> {
        let mut left = self.product(depth)?;
        loop {
            let Some(&op @ (b'+' | b'-')) = self.input.as_bytes().get(self.pos) else {
                return Some(left);
            };
            // CSS requires whitespace on both sides of binary + and -.
            if self.pos == 0
                || !self.input.as_bytes()[self.pos - 1].is_ascii_whitespace()
                || !self
                    .input
                    .as_bytes()
                    .get(self.pos + 1)
                    .is_some_and(u8::is_ascii_whitespace)
            {
                return None;
            }
            self.pos += 1;
            let right = self.product(depth)?;
            if left.1 != right.1 {
                return None;
            }
            left.0 = if op == b'+' {
                left.0 + right.0
            } else {
                left.0 - right.0
            };
            if !left.0.is_finite() {
                return None;
            }
        }
    }
}

fn color(input: &str) -> Option<Rgba> {
    let value = input.trim();
    let (r, g, b, a) = match value {
        "black" => (0, 0, 0, 255),
        "white" => (255, 255, 255, 255),
        "red" => (255, 0, 0, 255),
        "green" => (0, 128, 0, 255),
        "blue" => (0, 0, 255, 255),
        "gray" | "grey" => (128, 128, 128, 255),
        "yellow" => (255, 255, 0, 255),
        "purple" => (128, 0, 128, 255),
        "orange" => (255, 165, 0, 255),
        "transparent" => (0, 0, 0, 0),
        _ => {
            if let Some(hex) = value.strip_prefix('#') {
                if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return None;
                }
                if matches!(hex.len(), 3 | 4) {
                    let mut n = hex.chars().map(|c| c.to_digit(16).map(|v| (v * 17) as u8));
                    let (r, g, b) = (n.next()??, n.next()??, n.next()??);
                    (r, g, b, n.next().unwrap_or(Some(255))?)
                } else if matches!(hex.len(), 6 | 8) {
                    let (r, g, b) = (
                        u8::from_str_radix(&hex[0..2], 16).ok()?,
                        u8::from_str_radix(&hex[2..4], 16).ok()?,
                        u8::from_str_radix(&hex[4..6], 16).ok()?,
                    );
                    let a = if hex.len() == 8 {
                        u8::from_str_radix(&hex[6..8], 16).ok()?
                    } else {
                        255
                    };
                    (r, g, b, a)
                } else {
                    return None;
                }
            } else if let Some(args) = value.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')'))
            {
                let mut parts = args.split(',').map(str::trim);
                let (r, g, b) = (
                    parts.next()?.parse().ok()?,
                    parts.next()?.parse().ok()?,
                    parts.next()?.parse().ok()?,
                );
                if parts.next().is_some() {
                    return None;
                }
                (r, g, b, 255)
            } else if let Some(args) = value
                .strip_prefix("rgba(")
                .and_then(|s| s.strip_suffix(')'))
            {
                let mut parts = args.split(',').map(str::trim);
                let (r, g, b) = (
                    parts.next()?.parse().ok()?,
                    parts.next()?.parse().ok()?,
                    parts.next()?.parse().ok()?,
                );
                let alpha: f32 = parts.next()?.parse().ok()?;
                if parts.next().is_some() || !alpha.is_finite() {
                    return None;
                }
                (r, g, b, (alpha.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
            } else {
                return None;
            }
        }
    };
    Some(Rgba { r, g, b, a })
}

fn declaration_spans(input: &str) -> Result<Vec<(usize, usize)>, CssError> {
    if input.len() > MAX_CSS_BYTES {
        return Err(CssError {
            offset: 0,
            message: "CSS input too large",
        });
    }
    let bytes = input.as_bytes();
    let (mut start, mut pos, mut depth, mut quote) = (0, 0, 0usize, 0u8);
    let mut spans = Vec::new();
    while pos < bytes.len() {
        let byte = bytes[pos];
        if quote != 0 {
            if byte == b'\\' {
                pos += 1;
            } else if byte == quote {
                quote = 0;
            }
        } else if matches!(byte, b'\'' | b'"') {
            quote = byte;
        } else if byte == b'/' && bytes.get(pos + 1) == Some(&b'*') {
            let end = input[pos + 2..].find("*/").ok_or(CssError {
                offset: pos,
                message: "unterminated comment",
            })?;
            pos += end + 3;
        } else if matches!(byte, b'(' | b'[' | b'{') {
            depth += 1;
        } else if matches!(byte, b')' | b']' | b'}') {
            depth = depth.checked_sub(1).ok_or(CssError {
                offset: pos,
                message: "unbalanced declaration",
            })?;
        } else if byte == b';' && depth == 0 {
            spans.push((start, pos));
            start = pos + 1;
        }
        if spans.len() > MAX_DECLARATIONS {
            return Err(CssError {
                offset: pos,
                message: "too many declarations",
            });
        }
        pos += 1;
    }
    if depth != 0 || quote != 0 {
        return Err(CssError {
            offset: start,
            message: "unterminated declaration",
        });
    }
    if start < input.len() {
        spans.push((start, input.len()));
    }
    if spans.len() > MAX_DECLARATIONS {
        return Err(CssError {
            offset: start,
            message: "too many declarations",
        });
    }
    Ok(spans)
}

fn important_value(raw: &str) -> (&str, bool) {
    let raw = raw.trim();
    if raw.len() >= 10 && raw.as_bytes()[raw.len() - 10..].eq_ignore_ascii_case(b"!important") {
        (raw[..raw.len() - 10].trim_end(), true)
    } else {
        (raw, false)
    }
}

fn property_matches(left: &str, right: &str) -> bool {
    if left.starts_with("--") || right.starts_with("--") {
        left == right
    } else {
        left.eq_ignore_ascii_case(right)
    }
}

fn declaration_pair(part: &str) -> Option<(&str, &str)> {
    let mut part = part.trim();
    while let Some(comment) = part.strip_prefix("/*") {
        part = comment[comment.find("*/")? + 2..].trim_start();
    }
    part.split_once(':')
}

pub fn declaration_value(input: &str, name: &str) -> Result<Option<(String, bool)>, CssError> {
    let mut result = None;
    for (start, end) in declaration_spans(input)? {
        if let Some((property, raw)) = declaration_pair(&input[start..end]) {
            if property_matches(property.trim(), name.trim()) {
                let (value, important) = important_value(raw);
                if important || !result.as_ref().is_some_and(|(_, important)| *important) {
                    result = Some((value.to_string(), important));
                }
            }
        }
    }
    Ok(result)
}

pub fn set_declaration(
    input: &str,
    name: &str,
    value: &str,
    important: bool,
) -> Result<String, CssError> {
    let name = name.trim();
    if name.is_empty()
        || name
            .chars()
            .any(|ch| ch.is_whitespace() || ";:(){}[]\"'".contains(ch))
    {
        return Err(CssError {
            offset: 0,
            message: "invalid property name",
        });
    }
    let value = value.trim();
    if declaration_spans(value)?.len() > 1 || value.contains(';') {
        // Semicolons in strings and functions are valid values.
        let spans = declaration_spans(value)?;
        if spans.len() != 1 || spans[0] != (0, value.len()) {
            return Err(CssError {
                offset: 0,
                message: "invalid property value",
            });
        }
    }
    let mut out = String::new();
    for (start, end) in declaration_spans(input)? {
        let part = input[start..end].trim();
        if declaration_pair(part)
            .is_some_and(|(property, _)| property_matches(property.trim(), name))
        {
            continue;
        }
        if !part.is_empty() {
            out.push_str(part);
            out.push(';');
        }
    }
    if !value.is_empty() {
        if name.starts_with("--") {
            out.push_str(name);
        } else {
            out.push_str(&name.to_ascii_lowercase());
        }
        out.push(':');
        out.push_str(value);
        if important {
            out.push_str(" !important");
        }
        out.push(';');
    }
    Ok(out)
}

fn length_value(slot: usize, value: f32) -> Option<Value> {
    Some(match slot {
        3 => Value::Width(value),
        4 => Value::Height(value),
        5 => Value::Margin(value),
        6 => Value::Padding(value),
        7 => Value::FontSize(value),
        8 => Value::BorderRadius(value),
        9 => Value::BorderWidth(value),
        13 => Value::LineHeight(LineHeight::Pixels(value)),
        17 => Value::Gap(value),
        20 => Value::FlexBasis(Some(value)),
        23 => Value::MinWidth(value),
        24 => Value::MaxWidth(Some(value)),
        26 => Value::BorderSpacing(value),
        35..=38 => Value::Offset(slot - 35, Some(value)),
        39..=42 => Value::MarginSide(slot - 39, value),
        43..=46 => Value::PaddingSide(slot - 43, value),
        51 => Value::MinHeight(value),
        52 => Value::MaxHeight(Some(value)),
        _ => return None,
    })
}

fn parse_context_length(slot: usize, raw: &str, nonnegative: bool) -> Option<Value> {
    if let Some(value) = if nonnegative {
        nonnegative_length(raw)
    } else {
        length(raw)
    } {
        return length_value(slot, value);
    }
    if raw.contains('%')
        && !matches!(
            slot,
            3 | 4 | 5 | 6 | 7 | 13 | 20 | 23 | 24 | 35..=46 | 51 | 52
        )
    {
        return None;
    }
    let context = LengthContext {
        font: 16.0,
        root_font: 16.0,
        viewport: MediaEnvironment::default(),
        percent: Some(100.0),
    };
    let value = contextual_length(raw, Some(context))?;
    if nonnegative
        && value < 0.0
        && !raw
            .get(..5)
            .is_some_and(|v| v.eq_ignore_ascii_case("calc("))
    {
        return None;
    }
    length_value(slot, 0.0)?;
    Some(Value::ContextLength(slot, raw.into(), nonnegative))
}

#[derive(Clone, Copy)]
struct PropertyRegistration {
    name: &'static str,
    ids: &'static [usize],
    inherited: bool,
    in_all: bool,
}

const PROPERTY_COUNT: usize = 68;

const fn same_property_name(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    if left.len() != right.len() {
        return false;
    }
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

const fn validate_registry(properties: &[PropertyRegistration]) {
    let mut seen = [false; PROPERTY_COUNT];
    let mut inherited = [false; PROPERTY_COUNT];
    let mut in_all = [false; PROPERTY_COUNT];
    let mut row = 0;
    while row < properties.len() {
        let property = &properties[row];
        if property.name.is_empty()
            || same_property_name(property.name, "all")
            || property.ids.is_empty()
        {
            panic!("invalid CSS property registration");
        }
        let mut previous = 0;
        while previous < row {
            if same_property_name(property.name, properties[previous].name) {
                panic!("duplicate CSS property name");
            }
            previous += 1;
        }
        let mut index = 0;
        while index < property.ids.len() {
            let id = property.ids[index];
            if id >= PROPERTY_COUNT {
                panic!("invalid CSS property ID");
            }
            let mut earlier = 0;
            while earlier < index {
                if property.ids[earlier] == id {
                    panic!("duplicate CSS property ID");
                }
                earlier += 1;
            }
            if seen[id] && (inherited[id] != property.inherited || in_all[id] != property.in_all) {
                panic!("inconsistent CSS property metadata");
            }
            seen[id] = true;
            inherited[id] = property.inherited;
            in_all[id] = property.in_all;
            index += 1;
        }
        row += 1;
    }
    let mut id = 0;
    while id < PROPERTY_COUNT {
        if !seen[id] {
            panic!("missing CSS property ID");
        }
        id += 1;
    }
}

const fn property_flags(inherited: bool) -> [bool; PROPERTY_COUNT] {
    let mut flags = [false; PROPERTY_COUNT];
    let mut row = 0;
    while row < PROPERTIES.len() {
        let property = &PROPERTIES[row];
        if if inherited {
            property.inherited
        } else {
            property.in_all
        } {
            let mut index = 0;
            while index < property.ids.len() {
                flags[property.ids[index]] = true;
                index += 1;
            }
        }
        row += 1;
    }
    flags
}

const INHERITED_PROPERTIES: [bool; PROPERTY_COUNT] = property_flags(true);
const ALL_PROPERTIES: [bool; PROPERTY_COUNT] = property_flags(false);
const ALL_PROPERTY_COUNT: usize = {
    let (mut id, mut count) = (0, 0);
    while id < PROPERTY_COUNT {
        if ALL_PROPERTIES[id] {
            count += 1;
        }
        id += 1;
    }
    count
};
const ALL_PROPERTY_IDS: [usize; ALL_PROPERTY_COUNT] = {
    let mut ids = [0; ALL_PROPERTY_COUNT];
    let (mut id, mut index) = (0, 0);
    while id < PROPERTY_COUNT {
        if ALL_PROPERTIES[id] {
            ids[index] = id;
            index += 1;
        }
        id += 1;
    }
    ids
};

fn inherited_property(id: usize) -> bool {
    INHERITED_PROPERTIES.get(id).copied().unwrap_or(false)
}

macro_rules! property_registry {
    ($($name:literal => [$($id:literal),+], $inherited:literal, $all:literal;)*) => {
        const PROPERTIES: &[PropertyRegistration] = &[
            $(PropertyRegistration {name: $name, ids: &[$($id),+], inherited: $inherited, in_all: $all},)*
        ];
        const _: () = validate_registry(PROPERTIES);
        fn slots(name: &str) -> &'static [usize] {
            match name {
                "all" => &ALL_PROPERTY_IDS,
                $($name => &[$($id),+],)*
                _ => &[],
            }
        }
    };
}

property_registry! {
    "transform" => [59], false, true;
    "transform-origin" => [60], false, true;
    "order" => [48], false, true;
    "align-content" => [49], false, true;
    "align-self" => [47], false, true;
    "display" => [0], false, true;
    "color" => [1], true, true;
    "background" => [2,53], false, true;
    "background-color" => [2], false, true;
    "background-image" => [53], false, true;
    "box-shadow" => [58], false, true;
    "white-space" => [54], true, true;
    "text-align" => [55], true, true;
    "direction" => [56], true, false;
    "text-decoration" => [57], false, true;
    "text-decoration-line" => [57], false, true;
    "width" => [3], false, true;
    "height" => [4], false, true;
    "margin" => [5, 39, 40, 41, 42], false, true;
    "padding" => [6, 43, 44, 45, 46], false, true;
    "font-size" => [7], true, true;
    "border-radius" => [8], false, true;
    "border-width" => [9], false, true;
    "border-color" => [10], false, true;
    "border-style" => [11], false, true;
    "border" => [9, 10, 11], false, true;
    "overflow" => [12], false, true;
    "line-height" => [13], true, true;
    "flex-direction" => [14], false, true;
    "justify-content" => [15], false, true;
    "align-items" => [16], false, true;
    "gap" => [17], false, true;
    "flex-grow" => [18], false, true;
    "flex-shrink" => [19], false, true;
    "flex-basis" => [20], false, true;
    "flex" => [18, 19, 20], false, true;
    "box-sizing" => [21], false, true;
    "flex-wrap" => [22, 50], false, true;
    "min-width" => [23], false, true;
    "min-height" => [51], false, true;
    "max-height" => [52], false, true;
    "max-width" => [24], false, true;
    "table-layout" => [25], false, true;
    "border-spacing" => [26], true, true;
    "grid-template-columns" => [27], false, true;
    "grid-auto-columns" => [61], false, true;
    "grid-auto-rows" => [62], false, true;
    "grid-auto-flow" => [63], false, true;
    "justify-items" => [64], false, true;
    "justify-self" => [65], false, true;
    "grid-template-areas" => [66], false, true;
    "grid-area" => [67], false, true;
    "grid-template-rows" => [28], false, true;
    "grid-column" => [29], false, true;
    "grid-row" => [30], false, true;
    "opacity" => [31], false, true;
    "position" => [32], false, true;
    "float" => [33], false, true;
    "clear" => [34], false, true;
    "top" => [35], false, true;
    "right" => [36], false, true;
    "bottom" => [37], false, true;
    "left" => [38], false, true;
    "margin-top" => [39], false, true;
    "margin-right" => [40], false, true;
    "margin-bottom" => [41], false, true;
    "margin-left" => [42], false, true;
    "padding-top" => [43], false, true;
    "padding-right" => [44], false, true;
    "padding-bottom" => [45], false, true;
    "padding-left" => [46], false, true;
}

fn copy_slot(slot: usize, from: &Style, to: &mut Style) {
    if let Some((_, length)) = from.relative_lengths.iter().find(|(key, _)| *key == slot) {
        to.relative_lengths.push((slot, *length));
    }
    match slot {
        0 => to.display = from.display,
        1 => to.color = from.color,
        2 => to.background = from.background,
        3 => to.width = from.width,
        4 => to.height = from.height,
        5 => to.margin = from.margin,
        6 => to.padding = from.padding,
        7 => to.font_size = from.font_size,
        8 => to.border_radius = from.border_radius,
        9 => to.border_width = from.border_width,
        10 => to.border_color = from.border_color,
        11 => {
            to.border_solid = from.border_solid;
            if to.border_pattern != from.border_pattern {
                to.border_pattern = from.border_pattern;
            }
        }
        12 => to.overflow_clip = from.overflow_clip,
        13 => to.line_height = from.line_height,
        14 => to.flex_direction = from.flex_direction,
        15 => to.justify_content = from.justify_content,
        16 => to.align_items = from.align_items,
        17 => {
            to.gap = from.gap;
            to.gap_specified = from.gap_specified;
        }
        18 => to.flex_grow = from.flex_grow,
        19 => to.flex_shrink = from.flex_shrink,
        20 => to.flex_basis = from.flex_basis,
        21 => to.box_sizing = from.box_sizing,
        22 => to.flex_wrap = from.flex_wrap,
        23 => to.min_width = from.min_width,
        24 => to.max_width = from.max_width,
        25 => to.table_fixed = from.table_fixed,
        26 => to.border_spacing = from.border_spacing,
        27 => {
            to.grid_columns = from.grid_columns.clone();
            to.grid_column_names = from.grid_column_names.clone();
            to.grid_columns_subgrid = from.grid_columns_subgrid;
        }
        61 => to.grid_auto_columns = from.grid_auto_columns.clone(),
        62 => to.grid_auto_rows = from.grid_auto_rows.clone(),
        63 => to.grid_auto_flow = from.grid_auto_flow,
        64 => to.justify_items = from.justify_items,
        65 => to.justify_self = from.justify_self,
        66 => to.grid_areas = from.grid_areas.clone(),
        67 => to.grid_area = from.grid_area.clone(),
        28 => {
            to.grid_rows = from.grid_rows.clone();
            to.grid_row_names = from.grid_row_names.clone();
            to.grid_rows_subgrid = from.grid_rows_subgrid;
        }
        29 => {
            to.grid_column = from.grid_column;
            to.grid_column_spec = from.grid_column_spec.clone();
        }
        30 => {
            to.grid_row = from.grid_row;
            to.grid_row_spec = from.grid_row_spec.clone();
        }
        31 => to.opacity = from.opacity,
        32 => to.position = from.position,
        33 => to.float = from.float,
        34 => to.clear = from.clear,
        35 => to.top = from.top,
        36 => to.right = from.right,
        37 => to.bottom = from.bottom,
        38 => to.left = from.left,
        39..=42 => {
            to.margin_sides[slot - 39] = from.margin_sides[slot - 39];
            to.margin_auto[slot - 39] = from.margin_auto[slot - 39];
        }
        43..=46 => to.padding_sides[slot - 43] = from.padding_sides[slot - 43],
        47 => to.align_self = from.align_self,
        48 => to.order = from.order,
        49 => to.align_content = from.align_content,
        50 => to.flex_wrap_reverse = from.flex_wrap_reverse,
        51 => to.min_height = from.min_height,
        52 => to.max_height = from.max_height,
        53 => {
            if to.gradients != from.gradients {
                to.gradients = from.gradients.clone();
            }
        }
        54 => Value::WhiteSpace(from.white_space).apply(to),
        55 => Value::TextAlign(from.text_align).apply(to),
        56 => Value::Direction(from.direction).apply(to),
        57 => Value::TextDecoration(from.text_decoration).apply(to),
        58 => Value::Shadows(from.shadows.clone()).apply(to),
        59 => Value::Transforms(from.transforms.clone()).apply(to),
        60 => Value::TransformOrigin(from.transform_origin).apply(to),
        _ => {}
    }
}

fn transform_length(raw: &str, context: LengthContext) -> Option<TransformLength> {
    let pixels = contextual_length(
        raw,
        Some(LengthContext {
            percent: Some(0.0),
            ..context
        }),
    )?;
    let hundred = contextual_length(
        raw,
        Some(LengthContext {
            percent: Some(100.0),
            ..context
        }),
    )?;
    let percent = hundred - pixels;
    percent
        .is_finite()
        .then_some(TransformLength { pixels, percent })
}

fn angle(raw: &str) -> Option<f32> {
    let raw = raw.to_ascii_lowercase();
    let value = if let Some(v) = raw.strip_suffix("deg") {
        v.parse::<f32>().ok()? * core::f32::consts::PI / 180.0
    } else if let Some(v) = raw.strip_suffix("grad") {
        v.parse::<f32>().ok()? * core::f32::consts::PI / 200.0
    } else if let Some(v) = raw.strip_suffix("turn") {
        v.parse::<f32>().ok()? * 2.0 * core::f32::consts::PI
    } else if let Some(v) = raw.strip_suffix("rad") {
        v.parse::<f32>().ok()?
    } else {
        let v = raw.parse::<f32>().ok()?;
        if v != 0.0 {
            return None;
        }
        v
    };
    value.is_finite().then_some(value)
}

fn transform_list(raw: &str, context: LengthContext) -> Option<Arc<[Transform]>> {
    if raw.len() > 8192 {
        return None;
    }
    let mut rest = raw.trim();
    let mut transforms = Vec::new();
    while !rest.is_empty() {
        if transforms.len() == 32 {
            return None;
        }
        let open = rest.find('(')?;
        let name = rest[..open].trim().to_ascii_lowercase();
        let (mut close, mut depth) = (open + 1, 1usize);
        while close < rest.len() && depth != 0 {
            match rest.as_bytes()[close] {
                b'(' => {
                    depth += 1;
                    if depth > 16 {
                        return None;
                    }
                }
                b')' => depth -= 1,
                _ => {}
            }
            close += 1;
        }
        if depth != 0 {
            return None;
        }
        let args = &rest[open + 1..close - 1];
        let args = if args.contains(',') {
            comma_components(args, 6)?
        } else {
            components(args)?
        };
        let scalar = |index: usize| {
            args.get(index)?
                .parse::<f32>()
                .ok()
                .filter(|v| v.is_finite())
        };
        let length = |index: usize| transform_length(args.get(index)?, context);
        let zero = TransformLength {
            pixels: 0.0,
            percent: 0.0,
        };
        let transform = match name.as_str() {
            "matrix" if args.len() == 6 => Transform::Matrix(Affine {
                a: scalar(0)?,
                b: scalar(1)?,
                c: scalar(2)?,
                d: scalar(3)?,
                e: scalar(4)?,
                f: scalar(5)?,
            }),
            "translate" if (1..=2).contains(&args.len()) => {
                Transform::Translate(length(0)?, if args.len() == 2 { length(1)? } else { zero })
            }
            "translatex" if args.len() == 1 => Transform::Translate(length(0)?, zero),
            "translatey" if args.len() == 1 => Transform::Translate(zero, length(0)?),
            "scale" if (1..=2).contains(&args.len()) => Transform::Scale(
                scalar(0)?,
                if args.len() == 2 {
                    scalar(1)?
                } else {
                    scalar(0)?
                },
            ),
            "scalex" if args.len() == 1 => Transform::Scale(scalar(0)?, 1.0),
            "scaley" if args.len() == 1 => Transform::Scale(1.0, scalar(0)?),
            "rotate" if args.len() == 1 => Transform::Rotate(angle(args[0])?),
            "skew" if (1..=2).contains(&args.len()) => Transform::Skew(
                angle(args[0])?,
                if args.len() == 2 {
                    angle(args[1])?
                } else {
                    0.0
                },
            ),
            "skewx" if args.len() == 1 => Transform::Skew(angle(args[0])?, 0.0),
            "skewy" if args.len() == 1 => Transform::Skew(0.0, angle(args[0])?),
            _ => return None,
        };
        transforms.push(transform);
        rest = rest[close..].trim_start();
    }
    (!transforms.is_empty()).then(|| transforms.into())
}

fn transform_origin(raw: &str, context: LengthContext) -> Option<[TransformLength; 2]> {
    let args = components(raw)?;
    if args.is_empty() || args.len() > 2 {
        return None;
    }
    let position = |raw: &str, horizontal: bool| match raw {
        "center" => Some(TransformLength {
            pixels: 0.0,
            percent: 50.0,
        }),
        "left" if horizontal => Some(TransformLength {
            pixels: 0.0,
            percent: 0.0,
        }),
        "right" if horizontal => Some(TransformLength {
            pixels: 0.0,
            percent: 100.0,
        }),
        "top" if !horizontal => Some(TransformLength {
            pixels: 0.0,
            percent: 0.0,
        }),
        "bottom" if !horizontal => Some(TransformLength {
            pixels: 0.0,
            percent: 100.0,
        }),
        _ => transform_length(raw, context),
    };
    let center = TransformLength {
        pixels: 0.0,
        percent: 50.0,
    };
    if args.len() == 1 {
        if matches!(args[0], "top" | "bottom") {
            Some([center, position(args[0], false)?])
        } else {
            Some([position(args[0], true)?, center])
        }
    } else if matches!(args[0], "top" | "bottom") || matches!(args[1], "left" | "right") {
        Some([position(args[1], true)?, position(args[0], false)?])
    } else {
        Some([position(args[0], true)?, position(args[1], false)?])
    }
}

fn gradient_layers(
    raw: &str,
    current_color: Rgba,
    context: Option<LengthContext>,
) -> Option<Arc<[Arc<Gradient>]>> {
    let mut layers = Vec::new();
    for layer in comma_components(raw, 8)? {
        if layer != "none" {
            let function = layer.split_once('(')?.0;
            layers.push(
                if function.eq_ignore_ascii_case("radial-gradient")
                    || function.eq_ignore_ascii_case("repeating-radial-gradient")
                {
                    radial_gradient(layer, current_color, context)?
                } else {
                    linear_gradient(layer, current_color, context)?
                },
            );
        }
    }
    Some(layers.into())
}

fn comma_components(raw: &str, max: usize) -> Option<Vec<&str>> {
    if raw.len() > MAX_VARIABLE_BYTES {
        return None;
    }
    let (mut start, mut depth, mut layers) = (0, 0usize, Vec::new());
    for (pos, byte) in raw.bytes().enumerate() {
        if byte == b'(' {
            depth += 1;
            if depth > 32 {
                return None;
            }
        } else if byte == b')' {
            depth = depth.checked_sub(1)?;
        } else if byte == b',' && depth == 0 {
            layers.push(raw[start..pos].trim());
            if layers.len() >= max {
                return None;
            }
            start = pos + 1;
        }
    }
    if depth != 0 {
        return None;
    }
    layers.push(raw[start..].trim());
    if layers.iter().any(|v| v.is_empty()) {
        return None;
    }
    Some(layers)
}

fn box_shadows(
    raw: &str,
    current_color: Rgba,
    context: Option<LengthContext>,
) -> Option<Arc<[BoxShadow]>> {
    let mut shadows = Vec::new();
    for shadow in comma_components(raw, 16)? {
        let (mut lengths, mut count, mut shadow_color, mut inset) = ([0.0; 4], 0usize, None, false);
        for token in components(shadow)? {
            if token.eq_ignore_ascii_case("inset") {
                if inset {
                    return None;
                }
                inset = true;
            } else if token.eq_ignore_ascii_case("currentcolor") {
                if shadow_color.is_some() {
                    return None;
                }
                shadow_color = Some(current_color);
            } else if let Some(color) = color(token) {
                if shadow_color.is_some() {
                    return None;
                }
                shadow_color = Some(color);
            } else {
                if count == 4 {
                    return None;
                }
                let value = contextual_length(token, context)?;
                if count == 2
                    && value < 0.0
                    && !token
                        .get(..5)
                        .is_some_and(|v| v.eq_ignore_ascii_case("calc("))
                {
                    return None;
                }
                lengths[count] = if count == 2 { value.max(0.0) } else { value };
                count += 1;
            }
        }
        if count < 2 {
            return None;
        }
        shadows.push(BoxShadow {
            offset_x: lengths[0],
            offset_y: lengths[1],
            blur: lengths[2],
            spread: lengths[3],
            color: shadow_color.unwrap_or(current_color),
            inset,
        });
    }
    Some(shadows.into())
}

fn linear_gradient(
    raw: &str,
    current_color: Rgba,
    context: Option<LengthContext>,
) -> Option<Arc<Gradient>> {
    let raw = raw.trim();
    let (repeating, raw) = if raw
        .get(..10)
        .is_some_and(|v| v.eq_ignore_ascii_case("repeating-"))
    {
        (true, &raw[10..])
    } else {
        (false, raw)
    };
    if raw.len() > MAX_VARIABLE_BYTES || !raw.get(..16)?.eq_ignore_ascii_case("linear-gradient(") {
        return None;
    }
    let body = raw.get(16..)?.strip_suffix(')')?;
    let (mut start, mut depth) = (0, 0usize);
    let mut parts = Vec::new();
    for (pos, byte) in body.bytes().enumerate() {
        if byte == b'(' {
            depth += 1;
            if depth > 16 {
                return None;
            }
        } else if byte == b')' {
            depth = depth.checked_sub(1)?;
        } else if byte == b',' && depth == 0 {
            parts.push(body[start..pos].trim());
            start = pos + 1;
        }
        if parts.len() > 33 {
            return None;
        }
    }
    if depth != 0 {
        return None;
    }
    parts.push(body[start..].trim());
    let first = *parts.first()?;
    let first_tokens = components(first)?;
    let token = *first_tokens.first()?;
    let (mut angle, mut corner, mut first_stop) = (180.0, None, 0usize);
    if color(token).is_none() && !token.eq_ignore_ascii_case("currentcolor") {
        first_stop = 1;
        if token.eq_ignore_ascii_case("to") {
            let (mut horizontal, mut vertical) = (0i8, 0i8);
            if !(2..=3).contains(&first_tokens.len()) {
                return None;
            }
            for direction in &first_tokens[1..] {
                match direction.to_ascii_lowercase().as_str() {
                    "left" if horizontal == 0 => horizontal = -1,
                    "right" if horizontal == 0 => horizontal = 1,
                    "top" if vertical == 0 => vertical = 1,
                    "bottom" if vertical == 0 => vertical = -1,
                    _ => return None,
                }
            }
            if horizontal != 0 && vertical != 0 {
                corner = Some((horizontal, vertical));
            } else {
                angle = match (horizontal, vertical) {
                    (1, 0) => 90.0,
                    (-1, 0) => 270.0,
                    (0, 1) => 0.0,
                    (0, -1) => 180.0,
                    _ => return None,
                };
            }
        } else {
            if first_tokens.len() != 1 {
                return None;
            }
            let angle_value = token.to_ascii_lowercase();
            angle = if let Some(v) = angle_value.strip_suffix("deg") {
                v.parse::<f32>().ok()?
            } else if let Some(v) = angle_value.strip_suffix("grad") {
                v.parse::<f32>().ok()? * 0.9
            } else if let Some(v) = angle_value.strip_suffix("rad") {
                v.parse::<f32>().ok()? * 180.0 / core::f32::consts::PI
            } else if let Some(v) = angle_value.strip_suffix("turn") {
                v.parse::<f32>().ok()? * 360.0
            } else if angle_value == "0" {
                0.0
            } else {
                return None;
            };
            if !angle.is_finite() {
                return None;
            }
        }
    }
    Some(Arc::new(Gradient {
        kind: GradientKind::Linear { angle, corner },
        repeating,
        stops: gradient_stops(&parts[first_stop..], current_color, context)?,
    }))
}

fn gradient_stops(
    parts: &[&str],
    current_color: Rgba,
    context: Option<LengthContext>,
) -> Option<Arc<[GradientStop]>> {
    let mut stops = Vec::new();
    for part in parts {
        let tokens = components(part)?;
        if tokens.is_empty() || tokens.len() > 3 {
            return None;
        }
        let color = if tokens[0].eq_ignore_ascii_case("currentcolor") {
            current_color
        } else {
            color(tokens[0])?
        };
        if tokens.len() == 1 {
            stops.push(GradientStop {
                color,
                position: None,
            });
        } else {
            for token in &tokens[1..] {
                let position = if let Some(v) = token.strip_suffix('%') {
                    let value = v.parse::<f32>().ok()?;
                    if !value.is_finite() {
                        return None;
                    }
                    GradientPosition::Fraction(value / 100.0)
                } else if token
                    .get(..5)
                    .is_some_and(|v| v.eq_ignore_ascii_case("calc("))
                {
                    GradientPosition::Mixed(background_length(token, context?)?)
                } else {
                    GradientPosition::Pixels(contextual_length(token, context)?)
                };
                stops.push(GradientStop {
                    color,
                    position: Some(position),
                });
            }
        }
        if stops.len() > 32 {
            return None;
        }
    }
    if stops.len() < 2 {
        return None;
    }
    Some(stops.into())
}

fn background_length(raw: &str, context: LengthContext) -> Option<LengthPercentage> {
    let value = transform_length(raw, context)?;
    Some(LengthPercentage {
        pixels: value.pixels,
        fraction: value.percent * 0.01,
    })
}

fn radial_gradient(
    raw: &str,
    current_color: Rgba,
    context: Option<LengthContext>,
) -> Option<Arc<Gradient>> {
    let context = context.unwrap_or(LengthContext {
        font: 16.0,
        root_font: 16.0,
        viewport: MediaEnvironment::default(),
        percent: None,
    });
    let raw = raw.trim();
    let (repeating, raw) = if raw
        .get(..10)
        .is_some_and(|v| v.eq_ignore_ascii_case("repeating-"))
    {
        (true, &raw[10..])
    } else {
        (false, raw)
    };
    if !raw.get(..16)?.eq_ignore_ascii_case("radial-gradient(") {
        return None;
    }
    let parts = comma_components(raw[16..].strip_suffix(')')?, 33)?;
    let tokens = components(parts[0])?;
    let (mut shape, mut size, mut radii, mut center) = (
        None,
        None,
        Vec::new(),
        [LengthPercentage {
            pixels: 0.0,
            fraction: 0.5,
        }; 2],
    );
    let first_stop = if color(tokens.first()?).is_some()
        || tokens.first()?.eq_ignore_ascii_case("currentcolor")
    {
        0
    } else {
        let mut index = 0;
        while index < tokens.len() {
            let token = tokens[index];
            if token.eq_ignore_ascii_case("at") {
                center = background_position(&tokens[index + 1..], context)?;
                break;
            }
            if token.eq_ignore_ascii_case("circle") || token.eq_ignore_ascii_case("ellipse") {
                if shape.is_some() {
                    return None;
                }
                shape = Some(if token.eq_ignore_ascii_case("circle") {
                    RadialShape::Circle
                } else {
                    RadialShape::Ellipse
                });
            } else if let Some(keyword) = match token.to_ascii_lowercase().as_str() {
                "closest-side" => Some(RadialSize::ClosestSide),
                "farthest-side" => Some(RadialSize::FarthestSide),
                "closest-corner" => Some(RadialSize::ClosestCorner),
                "farthest-corner" => Some(RadialSize::FarthestCorner),
                _ => None,
            } {
                if size.is_some() || !radii.is_empty() {
                    return None;
                }
                size = Some(keyword);
            } else {
                if size.is_some() || radii.len() == 2 {
                    return None;
                }
                let value = background_length(token, context)?;
                if !token.to_ascii_lowercase().starts_with("calc(")
                    && (value.pixels < 0.0 || value.fraction < 0.0)
                {
                    return None;
                }
                radii.push(value);
            }
            index += 1;
        }
        1
    };
    let shape = shape.unwrap_or(if radii.len() == 1 {
        RadialShape::Circle
    } else {
        RadialShape::Ellipse
    });
    let size = if radii.is_empty() {
        size.unwrap_or(RadialSize::FarthestCorner)
    } else if shape == RadialShape::Circle {
        if radii.len() != 1 || radii[0].fraction != 0.0 {
            return None;
        }
        RadialSize::Radii([radii[0]; 2])
    } else {
        if radii.len() != 2 {
            return None;
        }
        RadialSize::Radii([radii[0], radii[1]])
    };
    Some(Arc::new(Gradient {
        kind: GradientKind::Radial {
            shape,
            size,
            center,
        },
        repeating,
        stops: gradient_stops(&parts[first_stop..], current_color, Some(context))?,
    }))
}

fn background_position(tokens: &[&str], context: LengthContext) -> Option<[LengthPercentage; 2]> {
    if tokens.is_empty() || tokens.len() > 4 {
        return None;
    }
    let center = LengthPercentage {
        pixels: 0.0,
        fraction: 0.5,
    };
    if tokens.len() <= 2 {
        let joined = tokens.join(" ");
        let values = transform_origin(&joined, context)?;
        return Some(values.map(|v| LengthPercentage {
            pixels: v.pixels,
            fraction: v.percent * 0.01,
        }));
    }
    let mut result = [None, None];
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index].to_ascii_lowercase();
        let (axis, far) = match token.as_str() {
            "left" => (0, false),
            "right" => (0, true),
            "top" => (1, false),
            "bottom" => (1, true),
            "center" => {
                let axis = if result[0].is_none() { 0 } else { 1 };
                if result[axis].is_some() {
                    return None;
                }
                result[axis] = Some(center);
                index += 1;
                continue;
            }
            _ => return None,
        };
        if result[axis].is_some() {
            return None;
        }
        index += 1;
        let offset = if index < tokens.len()
            && !matches!(
                tokens[index].to_ascii_lowercase().as_str(),
                "left" | "right" | "top" | "bottom" | "center"
            ) {
            let value = background_length(tokens[index], context)?;
            index += 1;
            value
        } else {
            LengthPercentage::default()
        };
        result[axis] = Some(if far {
            LengthPercentage {
                pixels: -offset.pixels,
                fraction: 1.0 - offset.fraction,
            }
        } else {
            offset
        });
    }
    Some([result[0]?, result[1]?])
}

fn components(raw: &str) -> Option<Vec<&str>> {
    if raw.len() > MAX_VARIABLE_BYTES {
        return None;
    }
    let (mut start, mut depth, mut quote, mut escaped) = (0, 0usize, 0u8, false);
    let mut out = Vec::new();
    for (pos, byte) in raw.bytes().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if byte == b'\\' {
            escaped = true;
            continue;
        }
        if quote != 0 {
            if byte == quote {
                quote = 0;
            }
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            quote = byte;
            continue;
        }
        if byte == b'(' {
            depth += 1;
            if depth > 32 {
                return None;
            }
        } else if byte == b')' {
            depth = depth.checked_sub(1)?;
        } else if depth == 0 && byte.is_ascii_whitespace() {
            if start < pos {
                out.push(&raw[start..pos]);
            }
            start = pos + 1;
        }
        if out.len() > 64 {
            return None;
        }
    }
    if depth != 0 || quote != 0 {
        return None;
    }
    if start < raw.len() {
        out.push(&raw[start..]);
    }
    Some(out)
}

fn declarations(input: &str, offset: usize) -> Result<Vec<Declaration>, CssError> {
    let mut out = Vec::new();
    for (start, end) in declaration_spans(input)? {
        let Some((name, raw)) = declaration_pair(&input[start..end]) else {
            continue;
        };
        let name = name.trim();
        let (raw, important) = important_value(raw);
        if name.starts_with("--") {
            if name.len() > 256 || raw.len() > MAX_VARIABLE_BYTES {
                return Err(CssError {
                    offset: offset + start,
                    message: "custom property too large",
                });
            }
            if name.len() > 2 {
                out.push(Declaration {
                    value: Value::Custom(name.into(), raw.into()),
                    important,
                });
            }
            continue;
        }
        let name = name.to_ascii_lowercase();
        if raw
            .as_bytes()
            .windows(4)
            .any(|v| v.eq_ignore_ascii_case(b"var("))
        {
            if !slots(&name).is_empty() {
                out.push(Declaration {
                    value: Value::Deferred(name, raw.into()),
                    important,
                });
            }
            continue;
        }
        if matches!(
            raw,
            "inherit" | "initial" | "unset" | "revert" | "revert-layer"
        ) {
            for &slot in slots(&name) {
                let inherit = raw == "inherit"
                    || (matches!(raw, "unset" | "revert") && inherited_property(slot));
                out.push(Declaration {
                    value: if raw == "revert-layer" {
                        Value::RevertLayer(slot)
                    } else if raw == "revert" {
                        Value::Revert(slot)
                    } else {
                        Value::Default(slot, inherit)
                    },
                    important,
                });
            }
            continue;
        }
        if matches!(name.as_str(), "transform" | "transform-origin") {
            let context = LengthContext {
                font: 16.0,
                root_font: 16.0,
                viewport: MediaEnvironment::default(),
                percent: None,
            };
            let value = if name == "transform" {
                if raw == "none" {
                    Some(Value::Transforms(None))
                } else {
                    transform_list(raw, context).map(|_| Value::TransformRaw(raw.into()))
                }
            } else {
                transform_origin(raw, context).map(|_| Value::TransformOriginRaw(raw.into()))
            };
            if let Some(value) = value {
                out.push(Declaration { value, important });
            }
            continue;
        }
        if name == "box-shadow" {
            if raw == "none" {
                out.push(Declaration {
                    value: Value::Shadows(None),
                    important,
                });
            } else {
                let context = LengthContext {
                    font: 16.0,
                    root_font: 16.0,
                    viewport: MediaEnvironment::default(),
                    percent: None,
                };
                if box_shadows(raw, Style::initial().color, Some(context)).is_some() {
                    out.push(Declaration {
                        value: Value::ShadowsRaw(raw.into()),
                        important,
                    });
                }
            }
            continue;
        }
        if name == "border" {
            let Some(tokens) = components(raw) else {
                continue;
            };
            let (mut width, mut paint, mut border_style, mut valid) = (None, None, None, true);
            for token in tokens {
                let style = match token {
                    "solid" => Some(Value::BorderSolid(true)),
                    "none" | "hidden" => Some(Value::BorderSolid(false)),
                    "dashed" => Some(Value::BorderPattern(BorderPattern::Dashed)),
                    "dotted" => Some(Value::BorderPattern(BorderPattern::Dotted)),
                    _ => None,
                };
                if let Some(value) = style {
                    if border_style.is_some() {
                        valid = false;
                    }
                    border_style = Some(value);
                } else if token.eq_ignore_ascii_case("currentcolor") {
                    if paint.is_some() {
                        valid = false;
                    }
                    paint = Some(Value::BorderCurrentColor);
                } else if let Some(value) = color(token) {
                    if paint.is_some() {
                        valid = false;
                    }
                    paint = Some(Value::BorderColor(value));
                } else {
                    let value = match token {
                        "thin" => Some(Value::BorderWidth(1.0)),
                        "medium" => Some(Value::BorderWidth(3.0)),
                        "thick" => Some(Value::BorderWidth(5.0)),
                        _ => parse_context_length(9, token, true),
                    };
                    if value.is_none() || width.is_some() {
                        valid = false;
                    }
                    width = value;
                }
            }
            if valid && !raw.is_empty() {
                for value in [
                    width.unwrap_or(Value::BorderWidth(3.0)),
                    paint.unwrap_or(Value::BorderCurrentColor),
                    border_style.unwrap_or(Value::BorderSolid(false)),
                ] {
                    out.push(Declaration { value, important });
                }
            }
            continue;
        }
        if matches!(name.as_str(), "background" | "background-image") {
            if raw
                .get(..16)
                .is_some_and(|v| v.eq_ignore_ascii_case("linear-gradient("))
                || (raw.contains(',') && color(raw).is_none())
            {
                let context = LengthContext {
                    font: 16.0,
                    root_font: 16.0,
                    viewport: MediaEnvironment::default(),
                    percent: None,
                };
                if gradient_layers(raw, Style::initial().color, Some(context)).is_some() {
                    if name == "background" {
                        out.push(Declaration {
                            value: Value::Background(Style::initial().background),
                            important,
                        });
                    }
                    out.push(Declaration {
                        value: Value::GradientRaw(raw.into()),
                        important,
                    });
                }
                continue;
            }
            if raw == "none" {
                out.push(Declaration {
                    value: Value::GradientNone,
                    important,
                });
                if name == "background" {
                    out.push(Declaration {
                        value: Value::Background(Style::initial().background),
                        important,
                    });
                }
                continue;
            }
            if name == "background-image" {
                continue;
            }
            if color(raw).is_some() {
                out.push(Declaration {
                    value: Value::GradientNone,
                    important,
                });
            }
        }
        if name == "margin" || name == "padding" {
            let mut tokens = Vec::new();
            let (mut start, mut depth) = (0, 0usize);
            for (pos, byte) in raw.bytes().enumerate() {
                if byte == b'(' {
                    depth += 1;
                } else if byte == b')' {
                    depth = depth.saturating_sub(1);
                }
                if byte.is_ascii_whitespace() && depth == 0 {
                    if start < pos {
                        tokens.push(&raw[start..pos]);
                    }
                    start = pos + 1;
                }
            }
            if start < raw.len() {
                tokens.push(&raw[start..]);
            }
            if !tokens.is_empty() {
                if tokens.len() > 4 {
                    continue;
                }
                if tokens.iter().any(|v| *v != "auto" && length(v).is_none()) {
                    let base = if name == "margin" { 39 } else { 43 };
                    let raws = [
                        tokens[0],
                        *tokens.get(1).unwrap_or(&tokens[0]),
                        *tokens.get(2).unwrap_or(&tokens[0]),
                        *tokens.get(3).unwrap_or(tokens.get(1).unwrap_or(&tokens[0])),
                    ];
                    let parsed: Option<Vec<_>> = raws
                        .iter()
                        .enumerate()
                        .map(|(side, raw)| {
                            if name == "margin" && *raw == "auto" {
                                Some(Value::MarginAuto(side))
                            } else {
                                parse_context_length(base + side, raw, name == "padding")
                            }
                        })
                        .collect();
                    if let Some(values) = parsed {
                        let scalar = if tokens[0] == "auto" {
                            Some(Value::Margin(0.0))
                        } else {
                            parse_context_length(
                                if name == "margin" { 5 } else { 6 },
                                tokens[0],
                                name == "padding",
                            )
                        };
                        if let Some(value) = scalar {
                            out.push(Declaration { value, important });
                        }
                        out.extend(
                            values
                                .into_iter()
                                .map(|value| Declaration { value, important }),
                        );
                    }
                    continue;
                }
                let parsed: Option<Vec<_>> = tokens
                    .iter()
                    .map(|v| {
                        if name == "margin" && *v == "auto" {
                            Some((0.0, true))
                        } else {
                            (if name == "margin" {
                                length(v)
                            } else {
                                nonnegative_length(v)
                            })
                            .map(|v| (v, false))
                        }
                    })
                    .collect();
                if let Some(v) = parsed {
                    let sides = [
                        v[0],
                        *v.get(1).unwrap_or(&v[0]),
                        *v.get(2).unwrap_or(&v[0]),
                        *v.get(3).unwrap_or(v.get(1).unwrap_or(&v[0])),
                    ];
                    out.push(Declaration {
                        value: if name == "margin" {
                            Value::Margin(v[0].0)
                        } else {
                            Value::Padding(v[0].0)
                        },
                        important,
                    });
                    for (side, (value, auto)) in sides.into_iter().enumerate() {
                        out.push(Declaration {
                            value: if auto {
                                Value::MarginAuto(side)
                            } else if name == "margin" {
                                Value::MarginSide(side, value)
                            } else {
                                Value::PaddingSide(side, value)
                            },
                            important,
                        });
                    }
                }
                continue;
            }
        }
        if let Some(&slot) = slots(&name).first() {
            if length(raw).is_none() {
                if let Some(value) = parse_context_length(slot, raw, !matches!(slot, 5 | 35..=42)) {
                    out.push(Declaration { value, important });
                    continue;
                }
            }
        }
        if name == "flex" {
            if let Some(tokens) = components(raw) {
                let mut converted = String::new();
                let mut basis = None;
                let mut invalid = false;
                for token in tokens {
                    if let Some(value @ Value::ContextLength(_, _, _)) =
                        parse_context_length(20, token, true)
                    {
                        if basis.is_some() {
                            invalid = true;
                            break;
                        }
                        basis = Some(value);
                        converted.push_str("0px ");
                    } else {
                        converted.push_str(token);
                        converted.push(' ');
                    }
                }
                if invalid {
                    continue;
                }
                if let Some(basis) = basis {
                    let parsed =
                        typed_declarations(&alloc::format!("flex:{converted}"), offset + start)?;
                    for mut declaration in parsed {
                        if matches!(declaration.value, Value::FlexBasis(_)) {
                            declaration.value = basis.clone();
                        }
                        declaration.important = important;
                        out.push(declaration);
                    }
                    continue;
                }
            }
        }
        let mut parsed = typed_declarations(&alloc::format!("{name}:{raw}"), offset + start)?;
        if name == "flex-wrap" && matches!(raw, "wrap" | "nowrap" | "wrap-reverse") {
            parsed.push(Declaration {
                value: Value::FlexWrapReverse(raw == "wrap-reverse"),
                important,
            });
        }
        for declaration in &mut parsed {
            declaration.important = important;
        }
        for declaration in parsed {
            match declaration.value {
                Value::Margin(v) | Value::Padding(v) => {
                    for side in 0..4 {
                        out.push(Declaration {
                            value: if name == "margin" {
                                Value::MarginSide(side, v)
                            } else {
                                Value::PaddingSide(side, v)
                            },
                            important,
                        });
                    }
                }
                _ => {}
            }
            out.push(declaration);
        }
    }
    Ok(out)
}

fn typed_declarations(input: &str, offset: usize) -> Result<Vec<Declaration>, CssError> {
    let mut out = Vec::new();
    for (start, end) in declaration_spans(input)? {
        let part = &input[start..end];
        let mut part = part.trim();
        while let Some(after_open) = part.strip_prefix("/*") {
            let end = after_open.find("*/").ok_or(CssError {
                offset,
                message: "unterminated comment",
            })?;
            part = after_open[end + 2..].trim_start();
        }
        if part.is_empty() {
            continue;
        }
        let Some((name, raw)) = part.split_once(':') else {
            continue;
        };
        let raw = raw.split("/*").next().unwrap().trim();
        let (raw, important) = important_value(raw);
        let name = name.trim().to_ascii_lowercase();
        if name == "flex" {
            let values = match raw {
                "none" => Some((0.0, 0.0, None)),
                "auto" => Some((1.0, 1.0, None)),
                "initial" => Some((0.0, 1.0, None)),
                _ => {
                    let (mut grow, mut shrink, mut basis, mut valid) = (None, None, None, true);
                    for token in raw.split_ascii_whitespace() {
                        if let Ok(number) = token.parse::<f32>() {
                            if !number.is_finite() || number < 0.0 {
                                valid = false;
                                break;
                            }
                            if grow.is_none() {
                                grow = Some(number);
                            } else if shrink.is_none() {
                                shrink = Some(number);
                            } else if basis.is_none() && number == 0.0 {
                                basis = Some(Some(0.0));
                            } else {
                                valid = false;
                                break;
                            }
                        } else if basis.is_none() {
                            if token == "auto" {
                                basis = Some(None);
                            } else if let Some(value) = nonnegative_length(token) {
                                basis = Some(Some(value));
                            } else {
                                valid = false;
                                break;
                            }
                        } else {
                            valid = false;
                            break;
                        }
                    }
                    if valid && (grow.is_some() || basis.is_some()) {
                        Some((
                            grow.unwrap_or(1.0),
                            shrink.unwrap_or(1.0),
                            basis.unwrap_or(Some(0.0)),
                        ))
                    } else {
                        None
                    }
                }
            };
            if let Some((grow, shrink, basis)) = values {
                if out.len() + 3 > MAX_DECLARATIONS {
                    return Err(CssError {
                        offset,
                        message: "too many declarations",
                    });
                }
                for value in [
                    Value::FlexGrow(grow),
                    Value::FlexShrink(shrink),
                    Value::FlexBasis(basis),
                ] {
                    out.push(Declaration { value, important });
                }
            }
            continue;
        }
        if matches!(
            name.as_str(),
            "grid-template-columns" | "grid-template-rows" | "grid-auto-columns" | "grid-auto-rows"
        ) {
            let context = LengthContext {
                font: 16.0,
                root_font: 16.0,
                viewport: MediaEnvironment::default(),
                percent: None,
            };
            let valid = if name.starts_with("grid-auto-") {
                grid_tracks(raw, context).is_some_and(|v| !v.is_empty())
            } else {
                grid_template(raw, context).is_some()
            };
            if valid {
                out.push(Declaration {
                    value: Value::GridRaw(slots(&name)[0], Arc::from(raw)),
                    important,
                });
            }
            continue;
        }
        let value = match name.as_str() {
            "white-space" => match raw {
                "normal" => Some(Value::WhiteSpace(WhiteSpace::Normal)),
                "nowrap" => Some(Value::WhiteSpace(WhiteSpace::NoWrap)),
                "pre" => Some(Value::WhiteSpace(WhiteSpace::Pre)),
                "pre-wrap" => Some(Value::WhiteSpace(WhiteSpace::PreWrap)),
                "pre-line" => Some(Value::WhiteSpace(WhiteSpace::PreLine)),
                "break-spaces" => Some(Value::WhiteSpace(WhiteSpace::BreakSpaces)),
                _ => None,
            },
            "text-align" => match raw {
                "start" => Some(Value::TextAlign(TextAlign::Start)),
                "end" => Some(Value::TextAlign(TextAlign::End)),
                "left" => Some(Value::TextAlign(TextAlign::Left)),
                "right" => Some(Value::TextAlign(TextAlign::Right)),
                "center" => Some(Value::TextAlign(TextAlign::Center)),
                "justify" => Some(Value::TextAlign(TextAlign::Justify)),
                _ => None,
            },
            "direction" => match raw {
                "ltr" => Some(Value::Direction(Direction::Ltr)),
                "rtl" => Some(Value::Direction(Direction::Rtl)),
                _ => None,
            },
            "text-decoration" | "text-decoration-line" => {
                let (mut bits, mut valid) = (0u8, true);
                if raw != "none" {
                    for token in raw.split_ascii_whitespace() {
                        let bit = match token {
                            "underline" => 1,
                            "overline" => 2,
                            "line-through" => 4,
                            _ => {
                                valid = false;
                                0
                            }
                        };
                        if bits & bit != 0 {
                            valid = false;
                        }
                        bits |= bit;
                    }
                }
                if valid && !raw.is_empty() {
                    Some(Value::TextDecoration(bits))
                } else {
                    None
                }
            }
            "order" => raw.parse::<i32>().ok().map(Value::Order),
            "align-content" => match raw {
                "stretch" => Some(Value::AlignContent(None)),
                "start" | "flex-start" => Some(Value::AlignContent(Some(JustifyContent::Start))),
                "end" | "flex-end" => Some(Value::AlignContent(Some(JustifyContent::End))),
                "center" => Some(Value::AlignContent(Some(JustifyContent::Center))),
                "space-between" => Some(Value::AlignContent(Some(JustifyContent::SpaceBetween))),
                "space-around" => Some(Value::AlignContent(Some(JustifyContent::SpaceAround))),
                "space-evenly" => Some(Value::AlignContent(Some(JustifyContent::SpaceEvenly))),
                _ => None,
            },
            "align-self" => match raw {
                "auto" => Some(Value::AlignSelf(None)),
                "stretch" => Some(Value::AlignSelf(Some(AlignItems::Stretch))),
                "start" | "flex-start" => Some(Value::AlignSelf(Some(AlignItems::Start))),
                "end" | "flex-end" => Some(Value::AlignSelf(Some(AlignItems::End))),
                "center" => Some(Value::AlignSelf(Some(AlignItems::Center))),
                _ => None,
            },
            "position" => match raw {
                "static" => Some(Value::Position(Position::Static)),
                "relative" => Some(Value::Position(Position::Relative)),
                "absolute" => Some(Value::Position(Position::Absolute)),
                "fixed" => Some(Value::Position(Position::Fixed)),
                _ => None,
            },
            "float" => match raw {
                "none" => Some(Value::Float(Float::None)),
                "left" => Some(Value::Float(Float::Left)),
                "right" => Some(Value::Float(Float::Right)),
                _ => None,
            },
            "clear" => match raw {
                "none" => Some(Value::Clear(Clear::None)),
                "left" => Some(Value::Clear(Clear::Left)),
                "right" => Some(Value::Clear(Clear::Right)),
                "both" => Some(Value::Clear(Clear::Both)),
                _ => None,
            },
            "top" | "right" | "bottom" | "left" => {
                let side = ["top", "right", "bottom", "left"]
                    .iter()
                    .position(|v| *v == name)
                    .unwrap();
                if raw == "auto" {
                    Some(Value::Offset(side, None))
                } else {
                    length(raw).map(|v| Value::Offset(side, Some(v)))
                }
            }
            "margin-top" | "margin-right" | "margin-bottom" | "margin-left" => {
                let side = ["margin-top", "margin-right", "margin-bottom", "margin-left"]
                    .iter()
                    .position(|v| *v == name)
                    .unwrap();
                if raw == "auto" {
                    Some(Value::MarginAuto(side))
                } else {
                    length(raw).map(|v| Value::MarginSide(side, v))
                }
            }
            "padding-top" | "padding-right" | "padding-bottom" | "padding-left" => {
                let side = [
                    "padding-top",
                    "padding-right",
                    "padding-bottom",
                    "padding-left",
                ]
                .iter()
                .position(|v| *v == name)
                .unwrap();
                nonnegative_length(raw).map(|v| Value::PaddingSide(side, v))
            }
            "display" => match raw {
                "block" => Some(Value::Display(Display::Block)),
                "inline" => Some(Value::Display(Display::Inline)),
                "flex" => Some(Value::Display(Display::Flex)),
                "grid" => Some(Value::Display(Display::Grid)),
                "table" | "inline-table" => Some(Value::Display(Display::Table)),
                "table-row-group" | "table-header-group" | "table-footer-group" => {
                    Some(Value::Display(Display::TableRowGroup))
                }
                "table-row" => Some(Value::Display(Display::TableRow)),
                "table-cell" => Some(Value::Display(Display::TableCell)),
                "none" => Some(Value::Display(Display::None)),
                _ => None,
            },
            "color" => color(raw).map(Value::Color),
            "background" | "background-color" => color(raw).map(Value::Background),
            "width" => {
                if raw == "auto" {
                    Some(Value::Default(3, false))
                } else {
                    nonnegative_length(raw).map(Value::Width)
                }
            }
            "min-width" => nonnegative_length(raw).map(Value::MinWidth),
            "min-height" => nonnegative_length(raw).map(Value::MinHeight),
            "max-height" => {
                if raw == "none" {
                    Some(Value::MaxHeight(None))
                } else {
                    nonnegative_length(raw).map(|v| Value::MaxHeight(Some(v)))
                }
            }
            "max-width" => {
                if raw == "none" {
                    Some(Value::MaxWidth(None))
                } else {
                    nonnegative_length(raw).map(|v| Value::MaxWidth(Some(v)))
                }
            }
            "height" => {
                if raw == "auto" {
                    Some(Value::Default(4, false))
                } else {
                    nonnegative_length(raw).map(Value::Height)
                }
            }
            "font-size" => length(raw).filter(|v| *v > 0.0).map(Value::FontSize),
            "margin" => length(raw).map(Value::Margin),
            "padding" => nonnegative_length(raw).map(Value::Padding),
            "border-radius" => nonnegative_length(raw).map(Value::BorderRadius),
            "border-width" => nonnegative_length(raw).map(Value::BorderWidth),
            "border-color" => {
                if raw.eq_ignore_ascii_case("currentcolor") {
                    Some(Value::BorderCurrentColor)
                } else {
                    color(raw).map(Value::BorderColor)
                }
            }
            "border-style" => match raw {
                "solid" => Some(Value::BorderSolid(true)),
                "none" | "hidden" => Some(Value::BorderSolid(false)),
                "dashed" => Some(Value::BorderPattern(BorderPattern::Dashed)),
                "dotted" => Some(Value::BorderPattern(BorderPattern::Dotted)),
                _ => None,
            },
            "overflow" => match raw {
                "visible" => Some(Value::OverflowClip(false)),
                "hidden" | "auto" | "scroll" | "clip" => Some(Value::OverflowClip(true)),
                _ => None,
            },
            "flex-wrap" => match raw {
                "nowrap" => Some(Value::FlexWrap(false)),
                "wrap" | "wrap-reverse" => Some(Value::FlexWrap(true)),
                _ => None,
            },
            "flex-direction" => match raw {
                "row" => Some(Value::FlexDirection(FlexDirection::Row)),
                "row-reverse" => Some(Value::FlexDirection(FlexDirection::RowReverse)),
                "column" => Some(Value::FlexDirection(FlexDirection::Column)),
                "column-reverse" => Some(Value::FlexDirection(FlexDirection::ColumnReverse)),
                _ => None,
            },
            "box-sizing" => match raw {
                "content-box" => Some(Value::BoxSizing(BoxSizing::ContentBox)),
                "border-box" => Some(Value::BoxSizing(BoxSizing::BorderBox)),
                _ => None,
            },
            "justify-content" => match raw {
                "normal" | "stretch" => Some(Value::JustifyContent(JustifyContent::Stretch)),
                "start" | "flex-start" => Some(Value::JustifyContent(JustifyContent::Start)),
                "end" | "flex-end" => Some(Value::JustifyContent(JustifyContent::End)),
                "center" => Some(Value::JustifyContent(JustifyContent::Center)),
                "space-between" => Some(Value::JustifyContent(JustifyContent::SpaceBetween)),
                "space-around" => Some(Value::JustifyContent(JustifyContent::SpaceAround)),
                "space-evenly" => Some(Value::JustifyContent(JustifyContent::SpaceEvenly)),
                _ => None,
            },
            "align-items" => match raw {
                "normal" | "stretch" => Some(Value::AlignItems(AlignItems::Stretch)),
                "start" | "flex-start" => Some(Value::AlignItems(AlignItems::Start)),
                "end" | "flex-end" => Some(Value::AlignItems(AlignItems::End)),
                "center" => Some(Value::AlignItems(AlignItems::Center)),
                _ => None,
            },
            "gap" => nonnegative_length(raw).map(Value::Gap),
            "grid-auto-flow" => {
                let parts: Vec<_> = raw.split_ascii_whitespace().collect();
                if parts.is_empty()
                    || parts.len() > 2
                    || parts
                        .iter()
                        .any(|v| !matches!(*v, "row" | "column" | "dense"))
                    || (parts.contains(&"row") && parts.contains(&"column"))
                {
                    None
                } else {
                    Some(Value::GridAutoFlow(GridAutoFlow {
                        column: parts.contains(&"column"),
                        dense: parts.contains(&"dense"),
                    }))
                }
            }
            "justify-items" | "justify-self" => {
                let alignment = match raw {
                    "normal" | "stretch" => Some(AlignItems::Stretch),
                    "start" | "flex-start" => Some(AlignItems::Start),
                    "end" | "flex-end" => Some(AlignItems::End),
                    "center" => Some(AlignItems::Center),
                    _ => None,
                };
                if name == "justify-self" && raw == "auto" {
                    Some(Value::JustifySelf(None))
                } else {
                    alignment.map(|v| {
                        if name == "justify-items" {
                            Value::JustifyItems(v)
                        } else {
                            Value::JustifySelf(Some(v))
                        }
                    })
                }
            }
            "grid-template-areas" => grid_areas(raw).map(Value::GridAreas),
            "grid-area" => {
                if raw == "auto" {
                    Some(Value::GridArea(None))
                } else if raw.len() <= 128
                    && raw
                        .bytes()
                        .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'-' | b'_'))
                {
                    Some(Value::GridArea(Some(Arc::from(raw))))
                } else {
                    None
                }
            }
            "grid-column" => grid_placement(raw)
                .map(Value::GridColumn)
                .or_else(|| grid_line_spec(raw).then(|| Value::GridColumnSpec(Arc::from(raw)))),
            "grid-row" => grid_placement(raw)
                .map(Value::GridRow)
                .or_else(|| grid_line_spec(raw).then(|| Value::GridRowSpec(Arc::from(raw)))),
            "opacity" => raw
                .parse::<f32>()
                .ok()
                .filter(|v| v.is_finite())
                .map(|v| Value::Opacity(v.clamp(0.0, 1.0))),
            "flex-grow" => raw
                .parse::<f32>()
                .ok()
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(Value::FlexGrow),
            "flex-shrink" => raw
                .parse::<f32>()
                .ok()
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(Value::FlexShrink),
            "table-layout" => match raw {
                "fixed" => Some(Value::TableFixed(true)),
                "auto" => Some(Value::TableFixed(false)),
                _ => None,
            },
            "border-spacing" => nonnegative_length(raw).map(Value::BorderSpacing),
            "flex-basis" => {
                if raw == "auto" {
                    Some(Value::FlexBasis(None))
                } else {
                    nonnegative_length(raw).map(|value| Value::FlexBasis(Some(value)))
                }
            }
            "line-height" => {
                if raw == "normal" {
                    Some(Value::LineHeight(LineHeight::Normal))
                } else if let Some(value) = nonnegative_length(raw) {
                    Some(Value::LineHeight(LineHeight::Pixels(value)))
                } else {
                    raw.parse::<f32>()
                        .ok()
                        .filter(|value| value.is_finite() && *value >= 0.0)
                        .map(|value| Value::LineHeight(LineHeight::Number(value)))
                }
            }
            _ => None,
        };
        if let Some(value) = value {
            if out.len() >= MAX_DECLARATIONS {
                return Err(CssError {
                    offset,
                    message: "too many declarations",
                });
            }
            out.push(Declaration { value, important });
        }
    }
    Ok(out)
}

pub fn parse(input: &str) -> Result<Vec<Rule>, CssError> {
    if input.len() > MAX_CSS_BYTES {
        return Err(CssError {
            offset: 0,
            message: "CSS input too large",
        });
    }
    let mut rules = Vec::new();
    let mut layers = Vec::new();
    parse_rules(input, &mut rules, &[], None, &mut layers, 0)?;
    if rules.is_empty() && !layers.is_empty() {
        rules.push(Rule {
            selector: parse_simple_selector("*", 0)?,
            declarations: Arc::from([]),
            media: Vec::new(),
            layer: None,
            layers: Arc::from([]),
            layer_path: None,
        });
    }
    let layers: Arc<[String]> = layers.into();
    for rule in &mut rules {
        rule.layers = layers.clone();
    }
    Ok(rules)
}

fn parse_rules(
    input: &str,
    rules: &mut Vec<Rule>,
    media: &[Arc<str>],
    layer: Option<usize>,
    layers: &mut Vec<String>,
    nesting: usize,
) -> Result<(), CssError> {
    if nesting >= 32 {
        return Err(CssError {
            offset: 0,
            message: "CSS rule nesting limit",
        });
    }
    let mut pos = 0;
    while pos < input.len() {
        loop {
            pos += input[pos..].len() - input[pos..].trim_ascii_start().len();
            if !input[pos..].starts_with("/*") {
                break;
            }
            let end = input[pos + 2..].find("*/").ok_or(CssError {
                offset: pos,
                message: "unterminated comment",
            })?;
            pos += end + 4;
        }
        let rest = &input[pos..];
        if rest.trim().is_empty() {
            break;
        }
        if rest.starts_with("@layer ") {
            if let Some(end) = rest
                .find(';')
                .filter(|&end| rest.find('{').is_none_or(|open| end < open))
            {
                for name in rest[7..end].split(',').map(str::trim) {
                    let name = if let Some(parent) = layer {
                        alloc::format!("{}.{name}", layers[parent])
                    } else {
                        name.into()
                    };
                    if !name.is_empty() && !layers.iter().any(|v| v == &name) {
                        if layers.len() >= 128 || name.split('.').count() >= 8 {
                            return Err(CssError {
                                offset: pos,
                                message: "too many cascade layers",
                            });
                        }
                        layers.push(name);
                    }
                }
                pos += end + 1;
                continue;
            }
        }
        let open = rest.find('{').ok_or(CssError {
            offset: pos,
            message: "expected rule block",
        })?;
        let mut close = open + 1;
        let mut depth = 1usize;
        let mut quote = 0u8;
        let bytes = rest.as_bytes();
        while close < bytes.len() && depth != 0 {
            let byte = bytes[close];
            if quote != 0 {
                if byte == b'\\' {
                    close += 1;
                } else if byte == quote {
                    quote = 0;
                }
            } else if matches!(byte, b'\'' | b'"') {
                quote = byte;
            } else if byte == b'/' && bytes.get(close + 1) == Some(&b'*') {
                let end = rest[close + 2..].find("*/").ok_or(CssError {
                    offset: pos + close,
                    message: "unterminated comment",
                })?;
                close += end + 3;
            } else if byte == b'{' {
                depth += 1;
            } else if byte == b'}' {
                depth -= 1;
            }
            close += 1;
        }
        if depth != 0 {
            return Err(CssError {
                offset: pos + open,
                message: "unterminated rule",
            });
        }
        close -= 1;
        let prelude = rest[..open].trim();
        if let Some(query) = prelude.strip_prefix("@media") {
            let mut nested = media.to_vec();
            nested.push(query.trim().into());
            parse_rules(
                &rest[open + 1..close],
                rules,
                &nested,
                layer,
                layers,
                nesting + 1,
            )?;
            pos += close + 1;
            continue;
        }
        if let Some(name) = prelude.strip_prefix("@layer") {
            let name = name.trim();
            let name = if name.is_empty() {
                alloc::format!("#{}", layers.len())
            } else {
                name.into()
            };
            let name = if let Some(parent) = layer {
                alloc::format!("{}.{name}", layers[parent])
            } else {
                name
            };
            if layers.len() >= 128 || name.split('.').count() >= 8 {
                return Err(CssError {
                    offset: pos,
                    message: "too many cascade layers",
                });
            }
            let rank = if let Some(rank) = layers.iter().position(|v| v == &name) {
                rank
            } else {
                layers.push(name);
                layers.len() - 1
            };
            parse_rules(
                &rest[open + 1..close],
                rules,
                media,
                Some(rank),
                layers,
                nesting + 1,
            )?;
            pos += close + 1;
            continue;
        }
        if prelude.starts_with('@') {
            pos += close + 1;
            continue;
        }
        let declarations: Arc<[Declaration]> =
            declarations(&rest[open + 1..close], pos + open + 1)?.into();
        if let Ok(selectors) = parse_selector_list(&rest[..open], pos) {
            for selector in selectors {
                if rules.len() >= MAX_RULES {
                    return Err(CssError {
                        offset: pos,
                        message: "too many rules",
                    });
                }
                rules.push(Rule {
                    selector,
                    declarations: declarations.clone(),
                    media: media.to_vec(),
                    layer,
                    layers: Arc::from([]),
                    layer_path: None,
                });
            }
        }
        pos += close + 1;
    }
    Ok(())
}

impl Selector {
    pub(crate) fn matches(&self, kind: &NodeKind) -> bool {
        !self.root
            && self.languages.is_empty()
            && self.structural.is_empty()
            && self.ancestor.is_none()
            && self.matches_kind(kind)
    }

    pub(crate) fn matches_node(&self, document: &Document, node: NodeId) -> bool {
        if !self.languages.is_empty() {
            let (mut current, mut language) = (Some(node), None);
            for _ in 0..512 {
                let Some(id) = current else {
                    break;
                };
                if let Ok(NodeKind::Element { attributes, .. }) = document.kind(id) {
                    if let Some((_, value)) = attributes
                        .iter()
                        .find(|(name, _)| name == "xml:lang")
                        .or_else(|| attributes.iter().find(|(name, _)| name == "lang"))
                    {
                        language = Some(value.as_str());
                        break;
                    }
                }
                current = document.parent(id).ok().flatten();
            }
            if language.is_none() && current.is_some() {
                return false;
            }
            let language = language.unwrap_or("");
            if !language.is_empty() && !language_tag(language) {
                return false;
            }
            if !self.languages.iter().all(|ranges| {
                ranges.iter().any(|range| {
                    language.eq_ignore_ascii_case(range)
                        || (!range.is_empty()
                            && language
                                .get(..range.len())
                                .is_some_and(|v| v.eq_ignore_ascii_case(range))
                            && language.as_bytes().get(range.len()) == Some(&b'-'))
                })
            }) {
                return false;
            }
        }
        if self.root
            && !document
                .parent(node)
                .ok()
                .flatten()
                .is_some_and(|parent| matches!(document.kind(parent), Ok(NodeKind::Document)))
        {
            return false;
        }
        if !document
            .kind(node)
            .is_ok_and(|kind| self.matches_kind(kind))
        {
            return false;
        }
        for pseudo in &self.structural {
            let of_type = match pseudo {
                StructuralPseudo::Nth(_, _, of_type, _)
                | StructuralPseudo::Last(of_type)
                | StructuralPseudo::Only(of_type) => *of_type,
            };
            let Some(parent) = document.parent(node).ok().flatten() else {
                return false;
            };
            let mut current = document.first_child(parent).ok().flatten();
            let (mut index, mut count) = (0i64, 0i64);
            while let Some(sibling) = current {
                if let Ok(NodeKind::Element {
                    name, namespace, ..
                }) = document.kind(sibling)
                {
                    if !of_type || document.kind(node).is_ok_and(|kind| matches!(kind,NodeKind::Element {name:expected,namespace:space,..} if name == expected && namespace == space)) {
                        count += 1; if sibling == node {index = count;}
                    }
                }
                current = document.next_sibling(sibling).ok().flatten();
            }
            let matches = match pseudo {
                StructuralPseudo::Last(_) => index == count,
                StructuralPseudo::Only(_) => count == 1,
                StructuralPseudo::Nth(a, b, _, reverse) => {
                    let index = if *reverse { count + 1 - index } else { index };
                    let (a, b) = (*a as i64, *b as i64);
                    if a == 0 {
                        index == b
                    } else {
                        (index - b) % a == 0 && (index - b) / a >= 0
                    }
                }
            };
            if index == 0 || !matches {
                return false;
            }
        }
        let Some((relation, ancestor)) = &self.ancestor else {
            return true;
        };
        match relation {
            Relation::Child => document
                .parent(node)
                .ok()
                .flatten()
                .is_some_and(|parent| ancestor.matches_node(document, parent)),
            Relation::Descendant => {
                let mut current = document.parent(node).ok().flatten();
                while let Some(id) = current {
                    if ancestor.matches_node(document, id) {
                        return true;
                    }
                    current = document.parent(id).ok().flatten();
                }
                false
            }
            Relation::Adjacent | Relation::Following => {
                let mut current = document.previous_sibling(node).ok().flatten();
                while let Some(id) = current {
                    if matches!(document.kind(id), Ok(NodeKind::Element { .. })) {
                        if ancestor.matches_node(document, id) {
                            return true;
                        }
                        if matches!(relation, Relation::Adjacent) {
                            return false;
                        }
                    }
                    current = document.previous_sibling(id).ok().flatten();
                }
                false
            }
        }
    }

    fn matches_kind(&self, kind: &NodeKind) -> bool {
        let NodeKind::Element {
            name, attributes, ..
        } = kind
        else {
            return false;
        };
        if self.tag.as_ref().is_some_and(|tag| tag != name) {
            return false;
        }
        if self.id.as_ref().is_some_and(|id| {
            !attributes
                .iter()
                .any(|(key, value)| key == "id" && value == id)
        }) {
            return false;
        }
        if self.classes.iter().any(|class| {
            !attributes.iter().any(|(key, value)| {
                key == "class" && value.split_ascii_whitespace().any(|token| token == class)
            })
        }) {
            return false;
        }
        if self.attributes.iter().any(|selector| {
            !attributes.iter().any(|(name, value)| {
                name == &selector.name
                    && selector
                        .value
                        .as_ref()
                        .is_none_or(|expected| expected == value)
            })
        }) {
            return false;
        }
        true
    }
}

fn media_matches(query: &str, environment: MediaEnvironment) -> bool {
    query.split(',').any(|alternative| {
        let query = alternative.trim().to_ascii_lowercase();
        let (query, negate) = if let Some(v) = query.strip_prefix("not ") {
            (v, true)
        } else {
            (query.strip_prefix("only ").unwrap_or(&query), false)
        };
        let mut parts = query.split("and").map(str::trim);
        let first = parts.next().unwrap_or("");
        let media_type = match first {
            "all" | "" => true,
            "screen" => !environment.print,
            "print" => environment.print,
            value if value.starts_with('(') => media_feature(value, environment),
            _ => false,
        };
        let matched = media_type && parts.all(|feature| media_feature(feature, environment));
        matched != negate
    })
}

fn media_feature(raw: &str, environment: MediaEnvironment) -> bool {
    let Some(feature) = raw.strip_prefix('(').and_then(|v| v.strip_suffix(')')) else {
        return false;
    };
    let Some((name, value)) = feature.split_once(':') else {
        return false;
    };
    let name = name.trim();
    let (name, comparison) = if let Some(v) = name.strip_prefix("min-") {
        (v, 1)
    } else if let Some(v) = name.strip_prefix("max-") {
        (v, -1)
    } else {
        (name, 0)
    };
    let value = value.trim();
    let (actual, expected) = match name {
        "width" => (environment.width, nonnegative_length(value)),
        "height" => (environment.height, nonnegative_length(value)),
        "resolution" => (
            environment.resolution,
            value
                .strip_suffix("dppx")
                .or_else(|| value.strip_suffix('x'))
                .and_then(|v| v.parse::<f32>().ok())
                .or_else(|| {
                    value
                        .strip_suffix("dpi")
                        .and_then(|v| v.parse::<f32>().ok())
                        .map(|v| v / 96.0)
                })
                .or_else(|| {
                    value
                        .strip_suffix("dpcm")
                        .and_then(|v| v.parse::<f32>().ok())
                        .map(|v| v * 2.54 / 96.0)
                }),
        ),
        _ => return false,
    };
    let Some(expected) = expected.filter(|v| v.is_finite() && *v >= 0.0) else {
        return false;
    };
    match comparison {
        1 => actual >= expected,
        -1 => actual <= expected,
        _ => actual == expected,
    }
}

pub fn compute(
    kind: &NodeKind,
    parent: Option<&Style>,
    index: &StyleIndex,
) -> Result<Style, CssError> {
    compute_for(kind, parent, index, None)
}

const MAX_CUSTOM_PROPERTIES: usize = 128;
const MAX_VARIABLE_DEPTH: usize = 32;
const MAX_VARIABLE_BYTES: usize = 8192;

fn expand_variables(
    raw: &str,
    properties: &[(String, Option<String>)],
    stack: &mut Vec<String>,
) -> Option<String> {
    if stack.len() >= MAX_VARIABLE_DEPTH || raw.len() > MAX_VARIABLE_BYTES {
        return None;
    }
    let mut out = String::new();
    let (mut pos, mut quote) = (0, 0u8);
    let bytes = raw.as_bytes();
    while pos < bytes.len() {
        if quote != 0 || matches!(bytes[pos], b'\'' | b'"') {
            let byte = bytes[pos];
            if quote == 0 {
                quote = byte;
            } else if byte == quote {
                quote = 0;
            }
            let start = pos;
            pos += raw[pos..].chars().next()?.len_utf8();
            if byte == b'\\' && pos < bytes.len() {
                pos += raw[pos..].chars().next()?.len_utf8();
            }
            out.push_str(&raw[start..pos]);
            continue;
        }
        if raw[pos..].starts_with("/*") {
            let end = raw[pos + 2..].find("*/")?;
            out.push(' ');
            pos += end + 4;
            continue;
        }
        if raw
            .as_bytes()
            .get(pos..pos + 4)
            .is_some_and(|v| v.eq_ignore_ascii_case(b"var("))
            && (pos == 0
                || !matches!(bytes[pos-1], b'a'..=b'z'|b'A'..=b'Z'|b'0'..=b'9'|b'_'|b'-'|128..=255))
        {
            let start = pos + 4;
            let (mut end, mut depth, mut comma, mut inner_quote) = (start, 1usize, None, 0u8);
            while end < bytes.len() && depth != 0 {
                let byte = bytes[end];
                if inner_quote != 0 {
                    if byte == b'\\' {
                        end += 1;
                    } else if byte == inner_quote {
                        inner_quote = 0;
                    }
                } else if matches!(byte, b'\'' | b'"') {
                    inner_quote = byte;
                } else if byte == b'(' {
                    depth += 1;
                    if depth > MAX_VARIABLE_DEPTH {
                        return None;
                    }
                } else if byte == b')' {
                    depth -= 1;
                } else if byte == b',' && depth == 1 && comma.is_none() {
                    comma = Some(end);
                }
                end += 1;
            }
            if depth != 0 {
                return None;
            }
            let name = raw[start..comma.unwrap_or(end - 1)].trim();
            if !name.starts_with("--") || name.len() <= 2 {
                return None;
            }
            let value = if stack.iter().any(|v| v == name) {
                None
            } else {
                properties
                    .iter()
                    .find(|(key, _)| key == name)
                    .and_then(|(_, value)| value.as_ref())
                    .and_then(|value| {
                        stack.push(name.into());
                        let result = expand_variables(value, properties, stack);
                        stack.pop();
                        result
                    })
            };
            let value = value.or_else(|| {
                comma
                    .and_then(|comma| expand_variables(&raw[comma + 1..end - 1], properties, stack))
            })?;
            if out.len() + value.len() > MAX_VARIABLE_BYTES {
                return None;
            }
            out.push_str(&value);
            pos = end;
        } else {
            let ch = raw[pos..].chars().next()?;
            if out.len() + ch.len_utf8() > MAX_VARIABLE_BYTES {
                return None;
            }
            out.push(ch);
            pos += ch.len_utf8();
        }
    }
    Some(out)
}

fn custom_cycle(
    name: &str,
    properties: &[(String, Option<String>)],
    stack: &mut Vec<String>,
    budget: &mut usize,
) -> bool {
    if stack.len() >= MAX_VARIABLE_DEPTH || *budget == 0 {
        return true;
    }
    *budget -= 1;
    if stack.iter().any(|v| v == name) {
        return stack.first().is_some_and(|root| root == name);
    }
    let Some(raw) = properties
        .iter()
        .find(|(key, _)| key == name)
        .and_then(|(_, v)| v.as_ref())
    else {
        return false;
    };
    stack.push(name.into());
    let (mut pos, mut quote) = (0, 0u8);
    while pos < raw.len() {
        let byte = raw.as_bytes()[pos];
        if quote != 0 {
            if byte == b'\\' {
                pos += 1;
            } else if byte == quote {
                quote = 0;
            }
        } else if matches!(byte, b'\'' | b'"') {
            quote = byte;
        } else if raw[pos..].starts_with("/*") {
            if let Some(end) = raw[pos + 2..].find("*/") {
                pos += end + 4;
                continue;
            } else {
                stack.pop();
                return true;
            }
        } else if raw
            .as_bytes()
            .get(pos..pos + 4)
            .is_some_and(|v| v.eq_ignore_ascii_case(b"var("))
            && (pos == 0
                || !matches!(raw.as_bytes()[pos-1], b'a'..=b'z'|b'A'..=b'Z'|b'0'..=b'9'|b'_'|b'-'|128..=255))
        {
            let rest = &raw[pos + 4..];
            let end = rest.find([',', ')']).unwrap_or(rest.len());
            let dependency = rest[..end].trim();
            if custom_cycle(dependency, properties, stack, budget) {
                stack.pop();
                return true;
            }
            pos += 4;
            continue;
        }
        pos += 1;
        while pos < raw.len() && !raw.is_char_boundary(pos) {
            pos += 1;
        }
    }
    stack.pop();
    false
}

pub fn compute_node(
    document: &Document,
    node: NodeId,
    parent: Option<&Style>,
    index: &StyleIndex,
) -> Result<Style, CssError> {
    let kind = document.kind(node).map_err(|_| CssError {
        offset: 0,
        message: "invalid style node",
    })?;
    compute_for(kind, parent, index, Some((document, node)))
}

fn compute_for(
    kind: &NodeKind,
    parent: Option<&Style>,
    index: &StyleIndex,
    context: Option<(&Document, NodeId)>,
) -> Result<Style, CssError> {
    let mut style = Style::initial();
    if let NodeKind::Element { name, .. } = kind {
        if matches!(
            name.as_str(),
            "a" | "abbr"
                | "b"
                | "bdi"
                | "bdo"
                | "cite"
                | "code"
                | "em"
                | "i"
                | "label"
                | "mark"
                | "q"
                | "s"
                | "small"
                | "span"
                | "strong"
                | "sub"
                | "sup"
                | "time"
                | "u"
        ) {
            style.display = Display::Inline;
        }
    }
    if let Some(parent) = parent {
        style.color = parent.color;
        style.font_size = parent.font_size;
        style.line_height = parent.line_height;
        style.border_spacing = parent.border_spacing;
        Value::WhiteSpace(parent.white_space).apply(&mut style);
        Value::TextAlign(parent.text_align).apply(&mut style);
        Value::Direction(parent.direction).apply(&mut style);
    }
    if let NodeKind::Element { name, .. } = kind {
        match name.as_str() {
            "pre" => Value::WhiteSpace(WhiteSpace::Pre).apply(&mut style),
            "nobr" => Value::WhiteSpace(WhiteSpace::NoWrap).apply(&mut style),
            _ => {}
        }
        style.display = match name.as_str() {
            "table" => Display::Table,
            "thead" | "tbody" | "tfoot" => Display::TableRowGroup,
            "tr" => Display::TableRow,
            "td" | "th" => Display::TableCell,
            _ => style.display,
        };
    }
    let mut candidates = Vec::new();
    let mut apply = |order: usize| {
        let rule = &index.rules[order];
        if !rule
            .media
            .iter()
            .all(|query| media_matches(query, index.environment))
        {
            return;
        }
        if !context.map_or_else(
            || rule.selector.matches(kind),
            |(document, node)| rule.selector.matches_node(document, node),
        ) {
            return;
        }
        for declaration in rule.declarations.iter() {
            let (ids, classes, tags) = rule.selector.specificity;
            let layer = if declaration.important {
                rule.layer_path
                    .map(|path| path.map(|v| usize::MAX - 1 - v))
                    .unwrap_or([0; 8])
            } else {
                rule.layer_path.unwrap_or([usize::MAX - 1; 8])
            };
            let priority = (
                declaration.important,
                layer,
                (0, ids, classes, tags),
                order + 1,
            );
            candidates.push((priority, declaration.clone()));
        }
    };
    if let NodeKind::Element {
        name, attributes, ..
    } = kind
    {
        for &order in &index.universal {
            apply(order);
        }
        for &order in index.matches(&index.tags, name, 0) {
            apply(order);
        }
        if let Some((_, id)) = attributes.iter().find(|(key, _)| key == "id") {
            for &order in index.matches(&index.ids, id, 1) {
                apply(order);
            }
        }
        if let Some((_, classes)) = attributes.iter().find(|(key, _)| key == "class") {
            for class in classes.split_ascii_whitespace() {
                for &order in index.matches(&index.classes, class, 2) {
                    apply(order);
                }
            }
        }
    }
    if let NodeKind::Element { attributes, .. } = kind {
        if let Some((_, inline)) = attributes.iter().find(|(name, _)| name == "style") {
            for declaration in declarations(inline, 0)? {
                let priority = (
                    declaration.important,
                    [usize::MAX; 8],
                    (1, 0, 0, 0),
                    usize::MAX,
                );
                candidates.push((priority, declaration));
            }
        }
    }
    candidates.sort_by_key(|(priority, _)| *priority);
    let mut custom_properties = parent
        .map(|v| v.custom_properties.clone())
        .unwrap_or_default();
    let mut custom_layer = None;
    let mut custom_baseline = custom_properties.clone();
    for (priority, declaration) in &candidates {
        if custom_layer != Some((priority.0, priority.1)) {
            custom_layer = Some((priority.0, priority.1));
            custom_baseline = custom_properties.clone();
        }
        if let Value::Custom(name, raw) = &declaration.value {
            let value = match raw.as_str() {
                "initial" => None,
                "inherit" | "unset" | "revert" => parent
                    .and_then(|v| v.custom_properties.iter().find(|(key, _)| key == name))
                    .and_then(|(_, v)| v.clone()),
                "revert-layer" => custom_baseline
                    .iter()
                    .find(|(key, _)| key == name)
                    .and_then(|(_, v)| v.clone()),
                _ => Some(raw.clone()),
            };
            if let Some((_, existing)) = custom_properties.iter_mut().find(|(key, _)| key == name) {
                *existing = value;
            } else if custom_properties.len() < MAX_CUSTOM_PROPERTIES {
                custom_properties.push((name.clone(), value));
            } else {
                return Err(CssError {
                    offset: 0,
                    message: "too many custom properties",
                });
            }
        }
    }
    let raw_properties = custom_properties.clone();
    for (name, value) in &mut custom_properties {
        if custom_cycle(name, &raw_properties, &mut Vec::new(), &mut 4096) {
            *value = None;
        }
    }
    let valid_properties = custom_properties.clone();
    for (_, value) in &mut custom_properties {
        *value = value
            .as_ref()
            .and_then(|v| expand_variables(v, &valid_properties, &mut Vec::new()));
    }
    if !custom_properties.is_empty() {
        style.custom_properties = custom_properties;
    }
    let root = context.map_or(parent.is_none(), |(document, node)| {
        document
            .parent(node)
            .ok()
            .flatten()
            .is_some_and(|parent| matches!(document.kind(parent), Ok(NodeKind::Document)))
    });
    let root_font = if root {
        16.0
    } else {
        parent.map(|v| v.root_font_size).unwrap_or(16.0)
    };
    if root_font != 16.0 {
        style.root_font_size = root_font;
    }
    let mut winners = [(false, [0usize; 8], (0u16, 0u16, 0u16, 0u16), 0usize); PROPERTY_COUNT];
    let reverted = style.clone();
    let mut initial = Style::initial();
    initial.display = Display::Inline;
    let mut expanded = Vec::new();
    for (priority, declaration) in candidates {
        let values = match declaration.value {
            Value::Custom(_, _) => continue,
            Value::Deferred(name, raw) => {
                let resolved = expand_variables(&raw, &style.custom_properties, &mut Vec::new());
                let parsed = resolved
                    .and_then(|raw| declarations(&alloc::format!("{name}:{raw}"), 0).ok())
                    .unwrap_or_default();
                if parsed.is_empty() {
                    slots(&name)
                        .iter()
                        .map(|&slot| Declaration {
                            value: Value::Default(slot, inherited_property(slot)),
                            important: priority.0,
                        })
                        .collect()
                } else {
                    parsed
                }
            }
            value => alloc::vec![Declaration {
                value,
                important: priority.0
            }],
        };
        for declaration in values {
            expanded.push((priority, declaration));
        }
    }
    for pass in 0..3 {
        let font_pass = pass == 0;
        if !font_pass && root && style.font_size != style.root_font_size {
            style.root_font_size = style.font_size;
        }
        let mut layer_key = None;
        let mut layer_baseline = style.clone();
        for (priority, declaration) in &expanded {
            let slot = declaration.value.slot();
            if (pass == 0 && slot != 7)
                || (pass == 1 && matches!(slot, 7 | 10 | 53 | 58 | 59 | 60))
                || (pass == 2 && !matches!(slot, 10 | 53 | 58 | 59 | 60))
            {
                continue;
            }
            if layer_key != Some((priority.0, priority.1)) {
                layer_key = Some((priority.0, priority.1));
                layer_baseline = style.clone();
            }
            if *priority >= winners[slot] {
                winners[slot] = *priority;
                if style.relative_lengths.iter().any(|(key, _)| *key == slot) {
                    style.relative_lengths.retain(|(key, _)| *key != slot);
                }
                if let Value::RevertLayer(_) = declaration.value {
                    copy_slot(slot, &layer_baseline, &mut style);
                } else if let Value::Revert(_) = declaration.value {
                    copy_slot(slot, &reverted, &mut style);
                } else if let Value::Default(_, inherit) = declaration.value {
                    copy_slot(
                        slot,
                        if inherit {
                            parent.unwrap_or(&initial)
                        } else {
                            &initial
                        },
                        &mut style,
                    );
                    if slot == 10 && !inherit {
                        style.border_color = style.color;
                    }
                } else if let Value::GridRaw(slot, raw) = &declaration.value {
                    let context = LengthContext {
                        font: style.font_size,
                        root_font: style.root_font_size,
                        viewport: index.environment,
                        percent: None,
                    };
                    let value = match slot {
                        27 | 28 => grid_template(raw, context).map(|(tracks, names, subgrid)| {
                            if *slot == 27 {
                                Value::GridColumns(tracks, names, subgrid)
                            } else {
                                Value::GridRows(tracks, names, subgrid)
                            }
                        }),
                        61 | 62 => grid_tracks(raw, context).map(|tracks| {
                            if *slot == 61 {
                                Value::GridAutoColumns(tracks)
                            } else {
                                Value::GridAutoRows(tracks)
                            }
                        }),
                        _ => None,
                    };
                    if let Some(value) = value {
                        value.apply(&mut style);
                    }
                } else if matches!(
                    declaration.value,
                    Value::TransformRaw(_) | Value::TransformOriginRaw(_)
                ) {
                    let context = LengthContext {
                        font: style.font_size,
                        root_font: style.root_font_size,
                        viewport: index.environment,
                        percent: None,
                    };
                    match &declaration.value {
                        Value::TransformRaw(raw) => style.transforms = transform_list(raw, context),
                        Value::TransformOriginRaw(raw) => {
                            if let Some(origin) = transform_origin(raw, context) {
                                style.transform_origin = origin;
                            }
                        }
                        _ => {}
                    }
                } else if let Value::ShadowsRaw(raw) = &declaration.value {
                    let context = LengthContext {
                        font: style.font_size,
                        root_font: style.root_font_size,
                        viewport: index.environment,
                        percent: None,
                    };
                    style.shadows = box_shadows(raw, style.color, Some(context));
                } else if let Value::GradientRaw(raw) = &declaration.value {
                    let context = LengthContext {
                        font: style.font_size,
                        root_font: style.root_font_size,
                        viewport: index.environment,
                        percent: None,
                    };
                    let layers = gradient_layers(raw, style.color, Some(context));
                    style.gradients = layers.filter(|v| !v.is_empty());
                } else if let Value::ContextLength(_, raw, nonnegative) = &declaration.value {
                    let context = LengthContext {
                        font: if font_pass {
                            parent.map(|v| v.font_size).unwrap_or(16.0)
                        } else {
                            style.font_size
                        },
                        root_font: if font_pass {
                            root_font
                        } else {
                            style.root_font_size
                        },
                        viewport: index.environment,
                        percent: Some(if font_pass {
                            parent.map(|v| v.font_size).unwrap_or(16.0)
                        } else if slot == 13 {
                            style.font_size
                        } else {
                            0.0
                        }),
                    };
                    let Some(pixels) = contextual_length(raw, Some(context)) else {
                        continue;
                    };
                    if raw.contains('%') && !font_pass && slot != 13 {
                        let Some(hundred) = contextual_length(
                            raw,
                            Some(LengthContext {
                                percent: Some(100.0),
                                ..context
                            }),
                        ) else {
                            continue;
                        };
                        style.relative_lengths.push((
                            slot,
                            RelativeLength {
                                pixels,
                                percent: hundred - pixels,
                                nonnegative: *nonnegative,
                            },
                        ));
                    }
                    if let Some(value) = length_value(
                        slot,
                        if *nonnegative {
                            pixels.max(0.0)
                        } else {
                            pixels
                        },
                    ) {
                        value.apply(&mut style);
                    }
                } else {
                    declaration.value.apply(&mut style);
                }
            }
        }
    }
    if winners[10] == (false, [0; 8], (0, 0, 0, 0), 0) {
        style.border_color = style.color;
    }
    if style
        .extras
        .as_deref()
        .is_some_and(|extras| extras == &INITIAL_EXTRAS)
    {
        style.extras = None;
    }
    Ok(style)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Namespace;

    fn element(style: &str) -> NodeKind {
        NodeKind::Element {
            namespace: Namespace::Html,
            name: "div".into(),
            attributes: alloc::vec![("style".into(), style.into())],
        }
    }

    #[test]
    fn transforms_resolve_final_box_font_and_preserve_css_order() {
        let index = StyleIndex::new(Vec::new());
        let style = compute(&element("transform:translate(50%,1em) scale(2) rotate(90deg);transform-origin:0 0;font-size:10px"), None, &index).unwrap();
        let matrix = style
            .transform_matrix(Rect {
                x: 5.0,
                y: 7.0,
                width: 40.0,
                height: 20.0,
            })
            .unwrap();
        let point = matrix.apply(6.0, 7.0);
        assert!((point.0 - 25.0).abs() < 0.001 && (point.1 - 19.0).abs() < 0.001);
        let style = compute(
            &element("transform:scale(2);transform-origin:right bottom"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            style
                .transform_matrix(Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 20.0,
                    height: 10.0
                })
                .unwrap()
                .apply(0.0, 0.0),
            (-20.0, -10.0)
        );
        for invalid in [
            "translate(1px,)",
            "matrix(1,0,0,1,0)",
            "rotate(1px)",
            "scale(NaN)",
            "translateZ(1px)",
        ] {
            assert!(
                transform_list(
                    invalid,
                    LengthContext {
                        font: 16.0,
                        root_font: 16.0,
                        viewport: MediaEnvironment::default(),
                        percent: None
                    }
                )
                .is_none(),
                "{invalid}"
            );
        }
        assert!(
            compute(
                &element("transform:none;transform-origin:50% 50%"),
                None,
                &index
            )
            .unwrap()
            .extras
            .is_none()
        );
        assert_eq!(core::mem::size_of::<Style>(), 200);
    }

    #[test]
    fn variables_inherit_computed_tokens_and_invalidate_cycles() {
        let index = StyleIndex::new(Vec::new());
        let parent = compute(
            &element("--b:red;--a:var(--b);color:var(--a)"),
            None,
            &index,
        )
        .unwrap();
        let child = compute(
            &element("--b:blue;color:var(--a);width:var(--missing,calc(2px * 3))"),
            Some(&parent),
            &index,
        )
        .unwrap();
        assert_eq!(child.color, color("red").unwrap());
        assert_eq!(child.width, Some(6.0));
        let style = compute(&element("--a:var(--b);--b:var(--a);--ok:var(--a,green);color:red;color:var(--a);background:var(--ok);width:var(--absent,5px)"), None, &index).unwrap();
        assert_eq!(style.color, Style::initial().color);
        assert_eq!(style.background, color("green").unwrap());
        assert_eq!(style.width, Some(5.0));
        let style = compute(
            &element("--valid:red;--a:var(--valid,var(--a));color:var(--a,blue)"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(style.color, color("blue").unwrap());
        let style = compute(
            &element("--a:var(--missing,);width:7px;width:var(--a)"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(style.width, None);
    }

    #[test]
    fn media_tracks_dimensions_type_and_resolution() {
        let mut index = StyleIndex::new(parse("div{color:red}@media screen and (min-width:500px){div{color:blue}}@media print and (min-resolution:192dpi){div{color:green}}@media (max-height:100px){div{width:4px}}").unwrap());
        index.environment = MediaEnvironment {
            width: 600.0,
            height: 80.0,
            resolution: 1.0,
            print: false,
        };
        let style = compute(&element(""), None, &index).unwrap();
        assert_eq!(style.color, color("blue").unwrap());
        assert_eq!(style.width, Some(4.0));
        index.environment = MediaEnvironment {
            width: 600.0,
            height: 120.0,
            resolution: 2.0,
            print: true,
        };
        let style = compute(&element(""), None, &index).unwrap();
        assert_eq!(style.color, color("green").unwrap());
        assert_eq!(style.width, None);
        assert!(media_matches("not print", MediaEnvironment::default()));
    }

    #[test]
    fn layers_and_css_wide_keywords_follow_cascade() {
        let index = StyleIndex::new(parse("@layer first,second;@layer second{div{color:blue!important;width:9px}}@layer first{div{color:red!important;width:5px}}div{color:green;width:3px}").unwrap());
        let style = compute(&element(""), None, &index).unwrap();
        assert_eq!(style.color, color("red").unwrap());
        assert_eq!(style.width, Some(3.0));
        let index = StyleIndex::new(
            parse("@layer first{div{width:5px}}@layer second{div{width:9px;width:revert-layer}}")
                .unwrap(),
        );
        assert_eq!(
            compute(&element(""), None, &index).unwrap().width,
            Some(5.0)
        );
        let mut parent = Style::initial();
        parent.width = Some(21.0);
        parent.color = color("red").unwrap();
        let style = compute(&element("width:inherit;color:unset;font-size:initial;padding:1px 2px 3px 4px;padding-left:6px"),Some(&parent),&index).unwrap();
        assert_eq!(style.width, Some(21.0));
        assert_eq!(style.color, parent.color);
        assert_eq!(style.font_size, 16.0);
        assert_eq!(style.padding_sides, [1.0, 2.0, 3.0, 6.0]);
        let mut rules = parse("@layer first,second;").unwrap();
        rules.extend(parse("@layer second{div{color:blue}}@layer first{div{color:red}}").unwrap());
        assert_eq!(
            compute(&element(""), None, &StyleIndex::new(rules))
                .unwrap()
                .color,
            color("blue").unwrap()
        );
        let index = StyleIndex::new(parse("@layer parent{div{width:3px}@layer child{div{width:5px;color:red!important}}div{color:blue!important}}").unwrap());
        let style = compute(&element(""), None, &index).unwrap();
        assert_eq!(style.width, Some(3.0));
        assert_eq!(style.color, color("red").unwrap());
        assert_eq!(
            compute(&element("display:initial"), None, &index)
                .unwrap()
                .display,
            Display::Inline
        );
        assert_eq!(
            compute(&element("display:revert"), None, &index)
                .unwrap()
                .display,
            Display::Block
        );
    }

    #[test]
    fn variables_are_bounded_and_preserve_quoted_unicode() {
        let index = StyleIndex::new(Vec::new());
        let style = compute(&element("--a:'café var(--a)';--b:var(--a)"), None, &index).unwrap();
        assert_eq!(
            style
                .custom_properties
                .iter()
                .find(|(name, _)| name == "--b")
                .unwrap()
                .1
                .as_deref(),
            Some("'café var(--a)'")
        );
        let nested = alloc::format!("width:{}1px{}", "var(--absent,".repeat(40), ")".repeat(40));
        assert_eq!(
            compute(&element(&nested), None, &index).unwrap().width,
            None
        );
        let oversized = alloc::format!("--a:{}", "x".repeat(MAX_VARIABLE_BYTES + 1));
        assert!(compute(&element(&oversized), None, &index).is_err());
    }

    #[test]
    fn optional_style_state_is_lazy_and_flex_controls_cascade() {
        let index = StyleIndex::new(Vec::new());
        assert!(
            compute(&element("color:red;width:5px"), None, &index)
                .unwrap()
                .extras
                .is_none()
        );
        let style = compute(&element("align-self:center;order:-2;align-content:space-around;flex-wrap:wrap-reverse;margin:1px auto;margin-left:3px"),None,&index).unwrap();
        assert_eq!(style.align_self, Some(AlignItems::Center));
        assert_eq!(style.order, -2);
        assert_eq!(style.align_content, Some(JustifyContent::SpaceAround));
        assert!(style.flex_wrap);
        assert!(style.flex_wrap_reverse);
        assert_eq!(style.margin_sides, [1.0, 0.0, 1.0, 3.0]);
        assert_eq!(style.margin_auto, [false, true, false, false]);
        assert!(core::mem::size_of::<Style>() <= 256);
    }

    #[test]
    fn relative_lengths_use_final_fonts_and_definite_percentage_bases() {
        let mut index = StyleIndex::new(Vec::new());
        index.environment = MediaEnvironment {
            width: 400.0,
            height: 200.0,
            ..Default::default()
        };
        let root = compute(&element("font-size:20px"), None, &index).unwrap();
        let child = compute(&element("width:calc(50% - 1em);height:25%;padding:1em 2rem;font-size:150%;line-height:120%;min-height:10%;max-height:80%;flex-basis:30%"),Some(&root),&index).unwrap();
        assert_eq!(child.font_size, 30.0);
        assert_eq!(child.root_font_size, 20.0);
        assert_eq!(child.line_height, LineHeight::Pixels(36.0));
        assert_eq!(child.padding_sides, [30.0, 40.0, 30.0, 40.0]);
        let resolved = child.resolve_percentages(200.0, Some(100.0));
        assert_eq!(resolved.width, Some(70.0));
        assert_eq!(resolved.height, Some(25.0));
        assert_eq!(resolved.min_height, 10.0);
        assert_eq!(resolved.max_height, Some(80.0));
        assert_eq!(child.resolve_flex_basis_percentage(Some(200.0)), Some(60.0));
        assert_eq!(
            resolved.resolve_percentages(70.0, Some(25.0)).width,
            Some(70.0)
        );
        let indefinite = child.resolve_percentages(200.0, None);
        assert_eq!(indefinite.height, None);
        assert_eq!(indefinite.max_height, None);
        let viewport = compute(
            &element("width:10vw;height:10vh;margin:1vmin;font-size:2rem"),
            Some(&root),
            &index,
        )
        .unwrap();
        assert_eq!(viewport.width, Some(40.0));
        assert_eq!(viewport.height, Some(20.0));
        assert_eq!(viewport.margin, 2.0);
        assert_eq!(viewport.font_size, 40.0);
        assert_eq!(
            compute(&element("width:2em;font-size:12px"), None, &index)
                .unwrap()
                .width,
            Some(24.0)
        );
        assert!(
            compute(&element("color:red;width:5px"), None, &index)
                .unwrap()
                .extras
                .is_none()
        );
    }

    #[test]
    fn structural_pseudos_count_elements_and_parse_an_plus_b() {
        let document = crate::html::parse("<div class='flex'>text<span></span><!--x--><p id='a'></p><p id='b'></p><span></span><p id='c'></p></div>",32).unwrap();
        let find = |selector: &str| {
            crate::selector::query_selector_all(&document, document.root(), selector)
                .unwrap()
                .len()
        };
        assert_eq!(find(".flex > :nth-child(2n + 1)"), 3);
        assert_eq!(find(".flex > p:nth-of-type(2)"), 1);
        assert_eq!(find(".flex > p:nth-last-of-type(1)"), 1);
        assert_eq!(find(".flex > :nth-child(-n+3)"), 3);
        assert_eq!(find(".flex > :first-child"), 1);
        assert_eq!(find(".flex > :last-child"), 1);
        assert_eq!(find("p:only-child"), 0);
        for expression in ["2n+-1", "2n3", "n of p", "999999999999999n"] {
            assert!(nth_expression(expression).is_none());
        }
    }

    #[test]
    fn gradient_directions_stops_and_computed_colors_are_bounded() {
        let index = StyleIndex::new(Vec::new());
        let style = compute(&element("background-image:linear-gradient(to right bottom, currentColor 1em 25%, rgba(0, 0, 255, 0.5), red 120%);font-size:10px;color:green"),None,&index).unwrap();
        let gradient = &style.gradients.as_ref().unwrap()[0];
        assert_eq!(
            gradient.kind,
            GradientKind::Linear {
                angle: 180.0,
                corner: Some((1, -1))
            }
        );
        assert_eq!(gradient.stops.len(), 4);
        assert_eq!(gradient.stops[0].color, color("green").unwrap());
        assert_eq!(
            gradient.stops[0].position,
            Some(GradientPosition::Pixels(10.0))
        );
        assert_eq!(
            gradient.stops[1].position,
            Some(GradientPosition::Fraction(0.25))
        );
        assert_eq!(gradient.stops[2].color.a, 128);
        assert_eq!(
            compute(
                &element("background:linear-gradient(0.25turn,red,blue)"),
                None,
                &index
            )
            .unwrap()
            .gradients
            .as_ref()
            .unwrap()
            .first()
            .unwrap()
            .kind,
            GradientKind::Linear {
                angle: 90.0,
                corner: None
            }
        );
        assert!(
            compute(
                &element("background:linear-gradient(red,blue);background:red"),
                None,
                &index
            )
            .unwrap()
            .gradients
            .is_none()
        );
        assert!(
            compute(&element("background:red;width:5px"), None, &index)
                .unwrap()
                .extras
                .is_none()
        );
        for raw in [
            "linear-gradient(to left right,red,blue)",
            "linear-gradient(red)",
            "linear-gradient(blue NaN%,red)",
            "linear-gradient(in oklab,red,blue)",
        ] {
            assert!(
                linear_gradient(raw, Style::initial().color, None).is_none(),
                "{raw}"
            );
        }
        let raw = alloc::format!("linear-gradient({})", ["red"; 33].join(","));
        assert!(linear_gradient(&raw, Style::initial().color, None).is_none());
        let auto = compute(
            &element("height:40px;width:20px;height:auto;width:auto"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(auto.height, None);
        assert_eq!(auto.width, None);
        let style=compute(&element("background-image:linear-gradient(currentColor,transparent),linear-gradient(90deg,red,blue);color:green"),None,&index).unwrap();
        let layers = style.gradients.as_ref().unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].stops[0].color, color("green").unwrap());
        assert_eq!(
            layers[1].kind,
            GradientKind::Linear {
                angle: 90.0,
                corner: None
            }
        );
        let raw = ["linear-gradient(red,blue)"; 9].join(",");
        assert!(gradient_layers(&raw, Style::initial().color, None).is_none());
        assert_eq!(
            compute(&element("background:rgb(0,0,255)"), None, &index)
                .unwrap()
                .background,
            color("blue").unwrap()
        );
    }

    #[test]
    fn text_properties_inherit_and_decoration_is_explicit() {
        let index = StyleIndex::new(Vec::new());
        let parent=compute(&element("white-space:pre-wrap;text-align:end;direction:rtl;text-decoration:underline overline"),None,&index).unwrap();
        let child = compute(&element(""), Some(&parent), &index).unwrap();
        assert_eq!(child.white_space, WhiteSpace::PreWrap);
        assert_eq!(child.text_align, TextAlign::End);
        assert_eq!(child.direction, Direction::Rtl);
        assert_eq!(child.text_decoration, 0);
        assert_eq!(
            compute(&element("text-decoration:inherit"), Some(&parent), &index)
                .unwrap()
                .text_decoration,
            3
        );
        let reset=compute(&element("white-space:initial;text-align:unset;direction:ltr;text-decoration-line:line-through"),Some(&parent),&index).unwrap();
        assert_eq!(reset.white_space, WhiteSpace::Normal);
        assert_eq!(reset.text_align, TextAlign::End);
        assert_eq!(reset.direction, Direction::Ltr);
        assert_eq!(reset.text_decoration, 4);
        assert!(compute(&element("white-space:normal;text-align:start;direction:ltr;text-decoration:none;background:red"),None,&index).unwrap().extras.is_none());
        assert_eq!(
            compute(&element("all:initial"), Some(&parent), &index)
                .unwrap()
                .direction,
            Direction::Rtl
        );
    }

    #[test]
    fn shadow_lists_resolve_context_and_reject_invalid_blur() {
        let index = StyleIndex::new(Vec::new());
        let style=compute(&element("box-shadow:inset 1em 2px 3px -4px rgba(1, 2, 3, 0.5),currentColor 0 0;font-size:10px;color:blue"),None,&index).unwrap();
        let shadows = style.shadows.as_ref().unwrap();
        assert_eq!(shadows.len(), 2);
        assert_eq!(shadows[0].offset_x, 10.0);
        assert_eq!(shadows[0].blur, 3.0);
        assert_eq!(shadows[0].spread, -4.0);
        assert!(shadows[0].inset);
        assert_eq!(shadows[0].color.a, 128);
        assert_eq!(shadows[1].color, color("blue").unwrap());
        let style = compute(
            &element("box-shadow:1px 2px;box-shadow:1px 2px -3px"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(style.shadows.as_ref().unwrap()[0].blur, 0.0);
        for raw in [
            "1px",
            "1px 2px -3px",
            "inset inset 1px 2px",
            "red blue 1px 2px",
            "1% 2px",
            "1px 2px 3px 4px 5px",
        ] {
            assert!(
                box_shadows(raw, Style::initial().color, None).is_none(),
                "{raw}"
            );
        }
        assert!(box_shadows(&["1px 2px"; 17].join(","), Style::initial().color, None).is_none());
        assert!(
            compute(
                &element("box-shadow:none;background:red;width:5px"),
                None,
                &index
            )
            .unwrap()
            .extras
            .is_none()
        );
        assert_eq!(core::mem::size_of::<Style>(), 200);
    }

    #[test]
    fn registry_drives_aliases_shorthands_and_wide_defaulting() {
        let samples = [
            "block",
            "red",
            "1px",
            "0",
            "solid",
            "auto",
            "normal",
            "start",
            "none",
            "row",
            "stretch",
            "wrap",
            "ltr",
            "static",
            "fixed",
            "border-box",
        ];
        for property in PROPERTIES {
            assert_eq!(slots(property.name), property.ids);
            let parsed = samples
                .iter()
                .find_map(|sample| {
                    let parsed =
                        declarations(&alloc::format!("{}:{sample}", property.name), 0).unwrap();
                    (!parsed.is_empty()).then_some(parsed)
                })
                .unwrap_or_else(|| panic!("unparsed registered property: {}", property.name));
            assert!(
                parsed
                    .iter()
                    .all(|declaration| property.ids.contains(&declaration.value.slot())),
                "{}",
                property.name
            );
            let wide = declarations(&alloc::format!("{}:initial", property.name), 0).unwrap();
            assert_eq!(
                wide.iter().map(|v| v.value.slot()).collect::<Vec<_>>(),
                property.ids
            );
            for &id in property.ids {
                assert_eq!(inherited_property(id), property.inherited);
            }
        }
        assert_eq!(ALL_PROPERTY_IDS.len(), PROPERTY_COUNT - 1);
        assert!(!ALL_PROPERTY_IDS.contains(&56));
        assert_eq!(
            declarations("all:inherit", 0).unwrap().len(),
            ALL_PROPERTY_IDS.len()
        );
        let index = StyleIndex::new(Vec::new());
        let mut pre = element("");
        if let NodeKind::Element { name, .. } = &mut pre {
            *name = "pre".into();
        }
        assert_eq!(
            compute(&pre, None, &index).unwrap().white_space,
            WhiteSpace::Pre
        );
        let mut no_wrap = element("");
        if let NodeKind::Element { name, .. } = &mut no_wrap {
            *name = "nobr".into();
        }
        assert_eq!(
            compute(&no_wrap, None, &index).unwrap().white_space,
            WhiteSpace::NoWrap
        );
    }

    #[test]
    fn registry_validation_rejects_duplicate_names_and_invalid_ids() {
        extern crate std;
        let check = |properties: &[PropertyRegistration], expected: &str| {
            let error = std::panic::catch_unwind(|| validate_registry(properties)).unwrap_err();
            assert_eq!(error.downcast_ref::<&str>().copied(), Some(expected));
        };
        let mut duplicate = PROPERTIES.to_vec();
        duplicate.push(PROPERTIES[0]);
        check(&duplicate, "duplicate CSS property name");
        let mut invalid = PROPERTIES.to_vec();
        invalid[0].ids = &[PROPERTY_COUNT];
        check(&invalid, "invalid CSS property ID");
        let mut duplicate_id = PROPERTIES.to_vec();
        duplicate_id[0].ids = &[0, 0];
        check(&duplicate_id, "duplicate CSS property ID");
        let mut inconsistent = PROPERTIES.to_vec();
        inconsistent.push(PropertyRegistration {
            name: "invalid-alias",
            ids: &[1],
            inherited: false,
            in_all: true,
        });
        check(&inconsistent, "inconsistent CSS property metadata");
    }

    #[test]
    fn language_pseudo_inherits_tags_and_stops_at_explicit_unknown() {
        let document=crate::html::parse("<main lang='en-US'><p id='english'></p><p id='unknown' lang=''></p><section lang='fr'><p id='french'></p></section><section xml:lang='nl-NL' lang='de'><p id='dutch'></p></section></main>",32).unwrap();
        let matches = |id: &str, range: &str| {
            let node = crate::selector::query_selector(
                &document,
                document.root(),
                &alloc::format!("#{id}"),
            )
            .unwrap()
            .unwrap();
            parse_selector(&alloc::format!(":lang({range})"), 0)
                .unwrap()
                .matches_node(&document, node)
        };
        assert!(matches("english", "EN"));
        assert!(matches("english", "'en-us'"));
        assert!(!matches("english", "eng"));
        assert!(!matches("unknown", "en"));
        assert!(matches("unknown", "''"));
        assert!(matches("french", "en, fr"));
        assert!(!matches("french", "en"));
        assert!(matches("dutch", "nl"));
        assert!(!matches("dutch", "de"));
        for value in ["", "en_Us", "en--US", "en-*", "123", "en-123456789"] {
            assert!(language_ranges(value).is_none(), "{value}");
        }
    }

    #[test]
    fn patterned_border_shorthand_defaults_and_resolves_context() {
        let index = StyleIndex::new(Vec::new());
        let style = compute(
            &element("border:blue dotted .2em;font-size:10px;color:red"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(style.border_width, 2.0);
        assert_eq!(style.border_color, color("blue").unwrap());
        assert_eq!(style.border_pattern, Some(BorderPattern::Dotted));
        assert!(style.border_solid);
        let style = compute(&element("border:1px dashed;color:green"), None, &index).unwrap();
        assert_eq!(style.border_color, color("green").unwrap());
        assert_eq!(style.border_pattern, Some(BorderPattern::Dashed));
        let style = compute(
            &element("border:2px dotted red;border-style:solid;border-color:initial;color:blue"),
            None,
            &index,
        )
        .unwrap();
        assert!(style.border_solid);
        assert_eq!(style.border_pattern, None);
        assert_eq!(style.border_color, color("blue").unwrap());
        let style = compute(&element("border:2px dotted red;border:none"), None, &index).unwrap();
        assert!(!style.border_solid);
        assert_eq!(style.border_pattern, None);
        assert_eq!(style.border_width, 3.0);
        assert_eq!(
            compute(
                &element("border:1px solid red;border:solid dashed blue"),
                None,
                &index
            )
            .unwrap()
            .border_color,
            color("red").unwrap()
        );
        assert!(
            compute(&element("border:1px solid red"), None, &index)
                .unwrap()
                .extras
                .is_none()
        );
    }

    #[test]
    fn root_custom_properties_reach_descendants() {
        let document =
            crate::html::parse("<html><body><div id='target'></div></body></html>", 16).unwrap();
        let html = crate::selector::query_selector(&document, document.root(), "html")
            .unwrap()
            .unwrap();
        let body = crate::selector::query_selector(&document, document.root(), "body")
            .unwrap()
            .unwrap();
        let target = crate::selector::query_selector(&document, document.root(), "#target")
            .unwrap()
            .unwrap();
        let index = StyleIndex::new(parse(":root{--tone:red}div{color:var(--tone)}").unwrap());
        let html_style = compute_node(&document, html, None, &index).unwrap();
        let body_style = compute_node(&document, body, Some(&html_style), &index).unwrap();
        assert_eq!(
            compute_node(&document, target, Some(&body_style), &index)
                .unwrap()
                .color,
            color("red").unwrap()
        );
    }

    #[test]
    fn absolute_lengths_and_calc_preserve_dimensions_and_precedence() {
        for (source, expected) in [
            ("1in", 96.0),
            ("2.54cm", 96.0),
            ("25.4mm", 96.0),
            ("101.6Q", 96.0),
            ("72pt", 96.0),
            ("6pc", 96.0),
            ("CALC(1in - 2 * 8px)", 80.0),
            ("calc((2px + 4px) * (3 + 1) / 2)", 12.0),
            ("calc(calc(4px * 3) + -2px)", 10.0),
            ("calc(1e2px / 2)", 50.0),
        ] {
            assert!(
                (length(source).unwrap() - expected).abs() < 0.001,
                "{source}"
            );
        }
        for source in [
            "1 px",
            "1.px",
            "NaNpx",
            "infinitypx",
            "2",
            "(2px)",
            "calc(0)",
            "calc(1px + 0)",
            "calc(1px+ 2px)",
            "calc(1px +2px)",
            "calc(1px * 2px)",
            "calc(1px / 2px)",
            "calc(1px / 0)",
            "calc(1px + 20%)",
            "calc(1px + 1em)",
            "calc(1px)garbage",
            "calc(3e38px * 2)",
            "calc(2px +)",
        ] {
            assert_eq!(length(source), None, "{source}");
        }
        assert_eq!(
            length(&alloc::format!(
                "calc({}1px{})",
                "(".repeat(20),
                ")".repeat(20)
            )),
            None
        );
        assert_eq!(
            length(&alloc::format!("calc({}1px)", " ".repeat(1024))),
            None
        );
    }

    #[test]
    fn calculated_lengths_reach_the_cascade() {
        let kind = NodeKind::Element {
            namespace: Namespace::Html,
            name: "div".into(),
            attributes: alloc::vec![("style".into(),
                "width:calc(1in - 16px);height:2pc;padding:calc(2px * 3);margin:calc(1px - 3px);width:calc(1px + 2)".into())],
        };
        let style = compute(&kind, None, &StyleIndex::new(Vec::new())).unwrap();
        assert_eq!(style.width, Some(80.0));
        assert_eq!(style.height, Some(32.0));
        assert_eq!(style.padding, 6.0);
        assert_eq!(style.margin, -2.0);
        assert_eq!(nonnegative_length("calc(2px - 5px)"), Some(0.0));
        assert_eq!(nonnegative_length("-3px"), None);
    }

    #[test]
    fn cascade_specificity_and_inline_priority() {
        let rules = StyleIndex::new(parse("p { color: blue; background: #f00 } #title { color: red } p.title { color: green !important }").unwrap());
        let kind = NodeKind::Element {
            namespace: Namespace::Html,
            name: "p".to_string(),
            attributes: alloc::vec![
                ("id".to_string(), "title".to_string()),
                ("class".to_string(), "title".to_string()),
                ("style".to_string(), "color: white".to_string())
            ],
        };
        let style = compute(&kind, None, &rules).unwrap();
        assert_eq!(
            style.color,
            Rgba {
                r: 0,
                g: 128,
                b: 0,
                a: 255
            }
        );
        assert_eq!(
            style.background,
            Rgba {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            }
        );
    }

    #[test]
    fn compound_classes_and_specificity_do_not_carry_between_columns() {
        let stylesheet = alloc::format!(
            "#title {{color:red}} {} {{color:blue}} *.x.y {{background:green}}",
            ".x".repeat(11)
        );
        let index = StyleIndex::new(parse(&stylesheet).unwrap());
        let mut kind = NodeKind::Element {
            namespace: Namespace::Html,
            name: "p".into(),
            attributes: alloc::vec![
                ("id".into(), "title".into()),
                ("class".into(), "x y".into())
            ],
        };
        let style = compute(&kind, None, &index).unwrap();
        assert_eq!(style.color.r, 255);
        assert_eq!(style.background.g, 128);
        if let NodeKind::Element { attributes, .. } = &mut kind {
            attributes[1].1 = "x".into();
        }
        assert_eq!(compute(&kind, None, &index).unwrap().background.a, 0);
    }

    #[test]
    fn cssom_declarations_preserve_unknown_values_and_custom_case() {
        let input = "/* comment */COLOR:red !IMPORTANT;color:blue;--Tone:'a;b';unknown:fn(a;b);--tone:black";
        assert_eq!(
            declaration_value(input, "color").unwrap(),
            Some(("red".into(), true))
        );
        assert_eq!(
            declaration_value(input, "--Tone").unwrap(),
            Some(("'a;b'".into(), false))
        );
        assert_eq!(
            declaration_value(input, "--tone").unwrap(),
            Some(("black".into(), false))
        );
        let changed = set_declaration(input, "Color", "green", false).unwrap();
        assert_eq!(
            declaration_value(&changed, "color").unwrap(),
            Some(("green".into(), false))
        );
        assert!(changed.contains("unknown:fn(a;b);"));
        let removed = set_declaration(&changed, "--Tone", "", false).unwrap();
        assert_eq!(declaration_value(&removed, "--Tone").unwrap(), None);
        assert!(set_declaration(input, "color", "red;background:blue", false).is_err());
        assert!(set_declaration(input, "content", "'semi;colon'", false).is_ok());
    }

    #[test]
    fn grouped_selectors_share_declarations_and_skip_unknown_properties() {
        let rules =
            parse("/* sheet */ *, p, .note { /* property */ unknown: value; color: red }").unwrap();
        assert_eq!(rules.len(), 3);
        assert!(Arc::ptr_eq(&rules[0].declarations, &rules[1].declarations));
        let index = StyleIndex::new(rules);
        let kind = NodeKind::Element {
            namespace: Namespace::Html,
            name: "p".into(),
            attributes: Vec::new(),
        };
        assert_eq!(compute(&kind, None, &index).unwrap().color.r, 255);
    }

    #[test]
    fn selector_lists_invalidate_the_whole_rule() {
        for selector in ["p, :unknown", "p,", ",p", "p, .1bad"] {
            assert!(
                parse(&alloc::format!("{selector} {{ color:red }}"))
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(parse("[title='a,b'], p {color:red}").unwrap().len(), 2);
        assert_eq!(parse(r".a\,b, p {color:red}").unwrap().len(), 2);
    }

    #[test]
    fn identifiers_decode_css_escapes() {
        assert_eq!(
            parse_selector(r"#\31 23", 0).unwrap().id.as_deref(),
            Some("123")
        );
        assert_eq!(parse_selector(r".a\+b", 0).unwrap().classes, ["a+b"]);
        assert_eq!(parse_selector(r".a\ ", 0).unwrap().classes, ["a "]);
        assert!(parse_selector(".-", 0).is_err());
        assert_eq!(
            parse_selector(".caf\u{e9}", 0).unwrap().classes,
            ["caf\u{e9}"]
        );
        assert_eq!(parse_selector(r".\0", 0).unwrap().classes, ["\u{fffd}"]);
        assert!(parse_selector(".bad\\", 0).is_err());
        assert!(parse_selector(".bad\\\nname", 0).is_err());
        assert_eq!(
            parse_selector(r".\61 > p", 0).unwrap().specificity,
            (0, 1, 1)
        );
    }

    #[test]
    fn parses_common_alpha_colors() {
        assert_eq!(
            color("#1234").unwrap(),
            Rgba {
                r: 17,
                g: 34,
                b: 51,
                a: 68
            }
        );
        assert_eq!(color("#10203080").unwrap().a, 128);
        assert_eq!(color("rgba(16, 32, 48, 0.5)").unwrap().a, 128);
        assert_eq!(color("rgb(16, 32, 48)").unwrap().b, 48);
    }

    #[test]
    fn invalid_unicode_colors_and_negative_sizes_are_ignored() {
        for value in ["#aéaaa", "#aéaaaaa", "#１２３"] {
            assert_eq!(color(value), None);
        }
        let kind = NodeKind::Element {
            namespace: Namespace::Html,
            name: "div".into(),
            attributes: alloc::vec![(
                "style".into(),
                "width:5px;width:-1px;height:-2px;padding:-3px;font-size:0px".into()
            )],
        };
        let style = compute(&kind, None, &StyleIndex::new(Vec::new())).unwrap();
        assert_eq!(style.width, Some(5.0));
        assert_eq!(style.height, None);
        assert_eq!(style.padding, 0.0);
        assert_eq!(style.font_size, 16.0);
    }

    #[test]
    fn unsupported_rule_does_not_abort_later_rules() {
        let rules =
            parse(":root { --color: red } @media print { p { color: red } } p { color: blue }")
                .unwrap();
        assert_eq!(rules.len(), 3);
        let kind = NodeKind::Element {
            namespace: Namespace::Html,
            name: "p".into(),
            attributes: Vec::new(),
        };
        assert_eq!(
            compute(&kind, None, &StyleIndex::new(rules))
                .unwrap()
                .color
                .b,
            255
        );
    }

    #[test]
    fn rejects_oversized_css_rules() {
        let stylesheet = alloc::format!("p {{ {} }}", "color:red;".repeat(MAX_DECLARATIONS + 1));
        assert_eq!(
            parse(&stylesheet).unwrap_err().message,
            "too many declarations"
        );
    }
}
