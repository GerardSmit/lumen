//! Parsed author rules and a compact computed style for the initial block renderer.
pub mod component_values;
mod identifier;
pub mod typed_numeric;

pub use component_values::{parse_unparsed_value, serialize_unparsed_value, UnparsedComponent};
pub use identifier::serialize_identifier;

use crate::{
    paint::{
        Affine, BackgroundBox, BackgroundImage, BackgroundRepeat, BackgroundSize,
        BackgroundSizeKind, BorderPattern, BoxShadow, FontMatchRank, FontMetric,
        FontRelativeMetrics, FontSizeAdjust, FontSizeAdjustValue, FontSpec, FontStyle, Gradient,
        GradientKind, GradientPosition, GradientStop, LengthPercentage, RadialShape, RadialSize,
        Rect, Rgba, TextShaper, MAX_BACKGROUND_LAYERS,
    },
    Document, Namespace, NodeId, NodeKind,
};
use alloc::{
    borrow::ToOwned,
    boxed::Box,
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};
use core::ops::Range;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Overflow {
    #[default]
    Visible,
    Hidden,
    Scroll,
    Auto,
    Clip,
}
impl Overflow {
    pub fn clips(self) -> bool {
        self != Self::Visible
    }
    pub fn scroll_container(self) -> bool {
        matches!(self, Self::Hidden | Self::Scroll | Self::Auto)
    }
    fn parse(raw: &str) -> Option<Self> {
        Some(match &*ascii_lower(raw) {
            "visible" => Self::Visible,
            "hidden" => Self::Hidden,
            "scroll" => Self::Scroll,
            "auto" => Self::Auto,
            "clip" => Self::Clip,
            _ => return None,
        })
    }
}

/// Computed CSS border style. `hidden` remains distinct from `none` because
/// it wins every collapsed-border conflict while suppressing the painted edge.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BorderStyle {
    #[default]
    None,
    Hidden,
    Solid,
    Double,
    Dotted,
    Dashed,
    Groove,
    Ridge,
    Inset,
    Outset,
}

impl BorderStyle {
    pub const fn paints(self) -> bool {
        !matches!(self, Self::None | Self::Hidden)
    }

    /// CSS 2.1 collapsed-border style order, excluding the special `hidden`
    /// and `none` handling performed by the conflict resolver.
    pub const fn rank(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Inset => 1,
            Self::Groove => 2,
            Self::Outset => 3,
            Self::Ridge => 4,
            Self::Dotted => 5,
            Self::Dashed => 6,
            Self::Solid => 7,
            Self::Double => 8,
            Self::Hidden => 9,
        }
    }

    pub const fn pattern(self) -> Option<BorderPattern> {
        match self {
            Self::Groove => Some(BorderPattern::Groove),
            Self::Ridge => Some(BorderPattern::Ridge),
            Self::Inset => Some(BorderPattern::Inset),
            Self::Outset => Some(BorderPattern::Outset),
            Self::Double => Some(BorderPattern::Double),
            Self::Dotted => Some(BorderPattern::Dotted),
            Self::Dashed => Some(BorderPattern::Dashed),
            _ => None,
        }
    }

    pub const fn from_pattern(pattern: BorderPattern) -> Self {
        match pattern {
            BorderPattern::Groove => Self::Groove,
            BorderPattern::Ridge => Self::Ridge,
            BorderPattern::Inset => Self::Inset,
            BorderPattern::Outset => Self::Outset,
            BorderPattern::Double => Self::Double,
            BorderPattern::Dashed => Self::Dashed,
            BorderPattern::Dotted => Self::Dotted,
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        Some(match &*ascii_lower(raw) {
            "none" => Self::None,
            "hidden" => Self::Hidden,
            "solid" => Self::Solid,
            "double" => Self::Double,
            "dotted" => Self::Dotted,
            "dashed" => Self::Dashed,
            "groove" => Self::Groove,
            "ridge" => Self::Ridge,
            "inset" => Self::Inset,
            "outset" => Self::Outset,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BackgroundAttachment {
    #[default]
    Scroll,
    Fixed,
    Local,
}
fn background_attachments(raw: &str) -> Option<Arc<[BackgroundAttachment]>> {
    let values = top_level_split(raw, b',', MAX_BACKGROUND_LAYERS)?
        .into_iter()
        .map(|value| {
            Some(match &*ascii_lower(value.trim()) {
                "scroll" => BackgroundAttachment::Scroll,
                "fixed" => BackgroundAttachment::Fixed,
                "local" => BackgroundAttachment::Local,
                _ => return None,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(values.into())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Display {
    Block,
    FlowRoot,
    Inline,
    InlineBlock,
    ListItem,
    Flex,
    Grid,
    Table,
    TableCaption,
    TableRowGroup,
    TableRow,
    TableCell,
    Contents,
    None,
}

/// CSS `list-style-type` values supported by the shared marker renderer.
/// Other counter styles are retained by name and reported as an unsupported
/// generated-content feature when a marker is actually painted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ListStyleType {
    Disc,
    Circle,
    Square,
    Decimal,
    DecimalLeadingZero,
    LowerRoman,
    UpperRoman,
    LowerAlpha,
    UpperAlpha,
    None,
    String(Arc<str>),
    Unsupported(Arc<str>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListStylePosition {
    Inside,
    Outside,
}

fn list_style_type(raw: &str) -> Option<ListStyleType> {
    let raw = raw.trim();
    Some(match &*ascii_lower(raw) {
        "disc" => ListStyleType::Disc,
        "circle" => ListStyleType::Circle,
        "square" => ListStyleType::Square,
        "decimal" => ListStyleType::Decimal,
        "decimal-leading-zero" => ListStyleType::DecimalLeadingZero,
        "lower-roman" => ListStyleType::LowerRoman,
        "upper-roman" => ListStyleType::UpperRoman,
        "lower-alpha" | "lower-latin" => ListStyleType::LowerAlpha,
        "upper-alpha" | "upper-latin" => ListStyleType::UpperAlpha,
        "none" => ListStyleType::None,
        _ if matches!(raw.as_bytes().first(), Some(b'\'' | b'"')) => {
            ListStyleType::String(Arc::from(css_string(raw)?))
        }
        _ => ListStyleType::Unsupported(Arc::from(parse_counter_ident(raw)?)),
    })
}

fn list_style_position(raw: &str) -> Option<ListStylePosition> {
    Some(match &*ascii_lower(raw.trim()) {
        "inside" => ListStylePosition::Inside,
        "outside" => ListStylePosition::Outside,
        _ => return None,
    })
}

fn list_style_image(raw: &str) -> Option<Option<Arc<str>>> {
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("none") {
        Some(None)
    } else {
        background_url(raw).map(Some)
    }
}

fn list_style_shorthand(
    raw: &str,
) -> Option<(ListStyleType, ListStylePosition, Option<Arc<str>>)> {
    let parts = components_bounded(raw, MAX_GENERATED_CONTENT_BYTES)?;
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }

    let mut marker = None;
    let mut position = None;
    let mut none_count = 0usize;
    let mut image = None;
    let mut image_specified = false;
    for part in parts {
        if let Some(value) = list_style_position(part) {
            if position.replace(value).is_some() {
                return None;
            }
        } else if let Some(value) = background_url(part) {
            if image.replace(value).is_some() {
                return None;
            }
            image_specified = true;
        } else if part.eq_ignore_ascii_case("none") {
            none_count += 1;
        } else {
            let value = list_style_type(part)?;
            if marker.replace(value).is_some() {
                return None;
            }
        }
    }

    // `none` is ambiguous in the shorthand. Assign it to still-unfilled
    // image/type slots in canonical order, with a lone `none` disabling the
    // marker type as required by the shorthand special case.
    if none_count > 2 || none_count > usize::from(marker.is_none()) + usize::from(!image_specified)
    {
        return None;
    }
    if none_count != 0 && marker.is_none() {
        marker = Some(ListStyleType::None);
        none_count -= 1;
    }
    if none_count != 0 && !image_specified {
        image_specified = true;
        none_count -= 1;
    }
    if none_count != 0 {
        return None;
    }

    Some((
        marker.unwrap_or(ListStyleType::Disc),
        position.unwrap_or(ListStylePosition::Outside),
        image,
    ))
}

fn html_list_style_type(value: &str) -> Option<ListStyleType> {
    Some(match value {
        "1" => ListStyleType::Decimal,
        "a" => ListStyleType::LowerAlpha,
        "A" => ListStyleType::UpperAlpha,
        "i" => ListStyleType::LowerRoman,
        "I" => ListStyleType::UpperRoman,
        "disc" => ListStyleType::Disc,
        "circle" => ListStyleType::Circle,
        "square" => ListStyleType::Square,
        _ => return None,
    })
}

fn clamp_css_counter(value: i64) -> i64 {
    value.clamp(-MAX_COUNTER_VALUE, MAX_COUNTER_VALUE)
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
    Baseline,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VerticalAlign {
    Baseline,
    Sub,
    Super,
    TextTop,
    Middle,
    Top,
    Bottom,
    TextBottom,
    Length(LengthPercentage),
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
    Sticky,
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
    MatchParent,
    JustifyAll,
}

impl TextAlign {
    pub const fn justifies(self) -> bool {
        matches!(self, Self::Justify | Self::JustifyAll)
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Direction {
    Ltr,
    Rtl,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WritingMode {
    HorizontalTb,
    VerticalRl,
    VerticalLr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntrinsicSizing {
    MinContent,
    MaxContent,
    FitContent,
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

/// One corner's elliptical border radius, in horizontal and vertical axes.
#[derive(Clone, Debug, PartialEq)]
pub struct BorderRadiusCorner {
    pub horizontal: BorderRadiusLength,
    pub vertical: BorderRadiusLength,
}

/// A border-radius length keeps percentages unresolved until a border box is
/// available. Comparison functions also retain their source expression since
/// `min()`, `max()` and `clamp()` are not affine in the percentage basis.
#[derive(Clone, Debug, PartialEq)]
pub struct BorderRadiusLength {
    pub value: TransformLength,
    expression: Option<Arc<str>>,
    context: LengthContext,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Transform {
    Matrix(Affine),
    Translate(TransformLength, TransformLength),
    Scale(f32, f32),
    Rotate(f32),
    Skew(f32, f32),
}

impl Transform {
    /// Resolve one transform against its untransformed reference box.
    pub fn matrix(self, width: f32, height: f32) -> Affine {
        match self {
            Transform::Matrix(matrix) => matrix,
            Transform::Translate(x, y) => Affine {
                e: x.resolve(width),
                f: y.resolve(height),
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
        }
    }
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
    /// A fit-content cap with a percentage component retained for layout.
    FitContentLength(TransformLength),
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

/// A parsed auto-repeat segment with its track-list context retained. The
/// repetition count is resolved at layout time from the definite available
/// size (CSS Grid §7.2.3.2).
#[derive(Clone, Debug, PartialEq)]
pub struct GridAutoRepeat {
    /// `true` for `auto-fit`, which collapses empty repeated tracks.
    pub fit: bool,
    /// Track sizes inside the auto-repeat; line indices in `repeat_names` are
    /// relative to this sequence.
    pub tracks: Arc<[GridTrack]>,
    /// Fixed track sizes before and after the repeated sequence.
    pub prefix_tracks: Arc<[GridTrack]>,
    pub suffix_tracks: Arc<[GridTrack]>,
    /// Named lines are relative to the corresponding track sequence. Layout
    /// expands `repeat_names` once per repetition and offsets suffix names by
    /// the realized repetition count.
    pub prefix_names: Arc<[GridNamedLine]>,
    pub repeat_names: Arc<[GridNamedLine]>,
    pub suffix_names: Arc<[GridNamedLine]>,
}

/// Resolves a track breadth that is definite against `available`, if it is.
pub fn definite_breadth(breadth: GridBreadth, available: f32) -> Option<f32> {
    match breadth {
        GridBreadth::Pixels(value) => Some(value.max(0.0)),
        GridBreadth::Percentage(value) => Some((value / 100.0 * available).max(0.0)),
        GridBreadth::Length(pixels, percentage) => {
            Some((pixels + percentage / 100.0 * available).max(0.0))
        }
        _ => None,
    }
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
    if raw.is_empty() || raw.len() > 128 {
        return false;
    }
    let mut position = 0usize;
    let mut decoded = String::new();
    let next = |position: &mut usize| -> Option<(char, bool)> {
        if raw.as_bytes().get(*position) == Some(&b'\\') {
            let value = selector_escape(raw, position)?;
            Some((value, true))
        } else {
            let ch = raw.get(*position..)?.chars().next()?;
            *position += ch.len_utf8();
            Some((ch, false))
        }
    };
    let name_start = |ch: char| ch.is_ascii_alphabetic() || ch == '_' || !ch.is_ascii();
    let name_char =
        |ch: char| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') || !ch.is_ascii();
    let Some((first, escaped)) = next(&mut position) else {
        return false;
    };
    if first == '-' && !escaped {
        let Some((second, second_escaped)) = next(&mut position) else {
            return false;
        };
        if !(second == '-' || second_escaped || name_start(second)) {
            return false;
        }
        decoded.push(first);
        decoded.push(second);
    } else if escaped || name_start(first) {
        decoded.push(first);
    } else {
        return false;
    }
    while position < raw.len() {
        let Some((ch, escaped)) = next(&mut position) else {
            return false;
        };
        if !escaped && !name_char(ch) {
            return false;
        }
        decoded.push(ch);
    }
    !matches!(
        decoded.to_ascii_lowercase().as_str(),
        "auto" | "span" | "default" | "initial" | "inherit" | "unset" | "revert" | "revert-layer"
    )
}

fn grid_function_args<'a>(raw: &'a str, name: &str) -> Option<&'a str> {
    let open = name.len();
    (raw.get(..open)?.eq_ignore_ascii_case(name)
        && raw.as_bytes().get(open) == Some(&b'(')
        && raw.ends_with(')'))
    .then(|| &raw[open + 1..raw.len() - 1])
}

fn grid_repeat_function_present(raw: &str) -> bool {
    raw.as_bytes()
        .windows(b"repeat(".len())
        .any(|window| window.eq_ignore_ascii_case(b"repeat("))
}

fn grid_word_slices(raw: &str, limit: usize) -> Option<Vec<&str>> {
    let bytes = raw.as_bytes();
    let mut words = Vec::new();
    let (mut position, mut start) = (0usize, None);
    while position < bytes.len() {
        if bytes[position].is_ascii_whitespace() {
            if let Some(start) = start.take() {
                words.push(&raw[start..position]);
                if words.len() > limit {
                    return None;
                }
            }
            position += 1;
        } else if bytes[position] == b'\\' {
            start.get_or_insert(position);
            selector_escape(raw, &mut position)?;
        } else {
            start.get_or_insert(position);
            let ch = raw[position..].chars().next()?;
            position += ch.len_utf8();
        }
    }
    if let Some(start) = start {
        words.push(&raw[start..]);
    }
    (words.len() <= limit).then_some(words)
}

fn grid_line_names(token: &str, line: usize, names: &mut Vec<GridNamedLine>) -> Option<()> {
    let contents = token.strip_prefix('[')?.strip_suffix(']')?;
    for name in grid_word_slices(contents, 256)? {
        if !grid_identifier(name) || names.len() == 256 {
            return None;
        }
        names.push(GridNamedLine {
            name: Arc::from(name),
            line,
        });
    }
    Some(())
}

fn grid_template_parts(
    raw: &str,
    context: LengthContext,
    depth: usize,
) -> Option<(Vec<GridTrack>, Vec<GridNamedLine>, bool)> {
    if depth > 8 || raw.len() > MAX_VARIABLE_BYTES {
        return None;
    }
    let tokens = grid_components(raw)?;
    if tokens.is_empty() {
        return None;
    }
    let mut tracks = Vec::new();
    let mut names = Vec::new();
    if tokens[0].eq_ignore_ascii_case("subgrid") {
        if tokens.len() > 2 {
            return None;
        }
        for token in &tokens[1..] {
            grid_line_names(token, 0, &mut names)?;
        }
        return Some((tracks, names, true));
    }
    if tokens.len() == 1 && tokens[0].eq_ignore_ascii_case("none") {
        return Some((tracks, names, false));
    }
    let mut line_names_since_track = false;
    for token in tokens {
        if token.starts_with('[') && token.ends_with(']') {
            if line_names_since_track {
                return None;
            }
            grid_line_names(token, tracks.len(), &mut names)?;
            line_names_since_track = true;
            continue;
        }
        if let Some(args) = grid_function_args(token, "repeat") {
            let args = comma_components(args, 2)?;
            if args.len() != 2 || grid_repeat_function_present(args[1]) {
                return None;
            }
            let count = args[0]
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|value| *value > 0 && *value <= MAX_GRID_TRACKS)?;
            let (repeated_tracks, repeated_names, subgrid) =
                grid_template_parts(args[1], context, depth + 1)?;
            if subgrid
                || repeated_tracks.is_empty()
                || tracks.len() + repeated_tracks.len() * count > MAX_GRID_TRACKS
            {
                return None;
            }
            for _ in 0..count {
                let offset = tracks.len();
                for line in &repeated_names {
                    if names.len() == 256 {
                        return None;
                    }
                    names.push(GridNamedLine {
                        name: line.name.clone(),
                        line: offset + line.line,
                    });
                }
                tracks.extend_from_slice(&repeated_tracks);
            }
            // A repeat() is a track-list section in the source grammar. Any
            // line-name blocks it contributes are merged with adjacent outer
            // line-name blocks after expansion, so they do not make a
            // following source-level block invalid.
            line_names_since_track = false;
            continue;
        }
        let parsed = grid_tracks(token, context)?;
        if parsed.is_empty() || tracks.len() + parsed.len() > MAX_GRID_TRACKS {
            return None;
        }
        tracks.extend_from_slice(&parsed);
        line_names_since_track = false;
    }
    Some((tracks, names, false))
}

fn grid_is_fixed_size(track: GridTrack) -> bool {
    let fixed_breadth = |breadth| {
        matches!(
            breadth,
            GridBreadth::Pixels(_) | GridBreadth::Percentage(_) | GridBreadth::Length(_, _)
        )
    };
    match track {
        GridTrack::Pixels(_) | GridTrack::Percentage(_) | GridTrack::Length(_, _) => true,
        GridTrack::MinMax(min, max) => fixed_breadth(min) || fixed_breadth(max),
        _ => false,
    }
}

fn grid_auto_repeat(raw: &str, context: LengthContext) -> Option<GridAutoRepeat> {
    let tokens = grid_components(raw)?;
    let mut auto_index = None;
    let mut auto_fit = false;
    let mut repeated_tracks = Vec::new();
    let mut repeated_names = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        let Some(args) = grid_function_args(token, "repeat") else {
            continue;
        };
        let args = comma_components(args, 2)?;
        if args.len() != 2 {
            return None;
        }
        let fit = if args[0].trim().eq_ignore_ascii_case("auto-fit") {
            true
        } else if args[0].trim().eq_ignore_ascii_case("auto-fill") {
            false
        } else {
            continue;
        };
        if auto_index.replace(index).is_some() {
            return None;
        }
        if grid_repeat_function_present(args[1]) {
            return None;
        }
        let (tracks, names, subgrid) = grid_template_parts(args[1], context, 0)?;
        if subgrid || tracks.is_empty() {
            return None;
        }
        auto_fit = fit;
        repeated_tracks = tracks;
        repeated_names = names;
    }
    let auto_index = auto_index?;
    let parse_side = |tokens: &[&str]| -> Option<(Vec<GridTrack>, Vec<GridNamedLine>)> {
        if tokens.is_empty() {
            return Some((Vec::new(), Vec::new()));
        }
        let raw = tokens.join(" ");
        let (tracks, names, subgrid) = grid_template_parts(&raw, context, 0)?;
        if subgrid || tracks.iter().any(|track| !grid_is_fixed_size(*track)) {
            return None;
        }
        Some((tracks, names))
    };
    let (prefix_tracks, prefix_names) = parse_side(&tokens[..auto_index])?;
    let (suffix_tracks, suffix_names) = parse_side(&tokens[auto_index + 1..])?;
    if prefix_tracks.len() + suffix_tracks.len() > MAX_GRID_TRACKS {
        return None;
    }
    Some(GridAutoRepeat {
        fit: auto_fit,
        tracks: repeated_tracks.into(),
        prefix_tracks: prefix_tracks.into(),
        suffix_tracks: suffix_tracks.into(),
        prefix_names: prefix_names.into(),
        repeat_names: repeated_names.into(),
        suffix_names: suffix_names.into(),
    })
}

fn grid_template(
    raw: &str,
    context: LengthContext,
) -> Option<(Arc<[GridTrack]>, Option<Arc<[GridNamedLine]>>, bool)> {
    let (tracks, names, subgrid) = grid_template_parts(raw.trim(), context, 0)?;
    if tracks.is_empty() && !subgrid && !raw.trim().eq_ignore_ascii_case("none") {
        return None;
    }
    Some((
        tracks.into(),
        (!names.is_empty()).then(|| names.into()),
        subgrid,
    ))
}

fn grid_words(raw: &str) -> Option<Vec<&str>> {
    grid_word_slices(raw, 3)
}

fn grid_line_segments(raw: &str, max_segments: usize) -> Option<Vec<&str>> {
    if raw.len() > MAX_VARIABLE_BYTES {
        return None;
    }
    let bytes = raw.as_bytes();
    let mut segments = Vec::new();
    let (mut position, mut start, mut parentheses) = (0usize, 0usize, 0usize);
    while position < bytes.len() {
        match bytes[position] {
            b'\\' => {
                selector_escape(raw, &mut position)?;
                continue;
            }
            b'(' => parentheses += 1,
            b')' => parentheses = parentheses.checked_sub(1)?,
            b'/' if parentheses == 0 => {
                segments.push(raw[start..position].trim());
                if segments.len() >= max_segments {
                    return None;
                }
                start = position + 1;
            }
            _ => {}
        }
        position += 1;
    }
    if parentheses != 0 {
        return None;
    }
    segments.push(raw[start..].trim());
    (segments.len() <= max_segments).then_some(segments)
}

#[derive(Clone, Copy)]
struct GridInteger {
    negative: bool,
    nonzero: bool,
}

fn grid_integer(raw: &str) -> Option<GridInteger> {
    let negative = raw.starts_with('-');
    let digits = raw
        .strip_prefix('+')
        .or_else(|| raw.strip_prefix('-'))
        .unwrap_or(raw);
    (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())).then(|| GridInteger {
        negative,
        nonzero: digits.bytes().any(|byte| byte != b'0'),
    })
}

fn grid_line_spec(raw: &str, max_segments: usize) -> bool {
    let Some(segments) = grid_line_segments(raw, max_segments) else {
        return false;
    };
    segments.into_iter().all(|segment| {
        if segment.eq_ignore_ascii_case("auto") {
            return true;
        }
        let Some(words) = grid_words(segment) else {
            return false;
        };
        if words.is_empty() {
            return false;
        }
        let (mut integer, mut name, mut span) = (None, false, false);
        for word in words {
            if word.eq_ignore_ascii_case("span") {
                if span {
                    return false;
                }
                span = true;
            } else if let Some(value) = grid_integer(word) {
                if integer.is_some() || !value.nonzero || span && value.negative {
                    return false;
                }
                integer = Some(value);
            } else if grid_identifier(word) {
                if name {
                    return false;
                }
                name = true;
            } else {
                return false;
            }
        }
        // The span arm allows a positive integer, a custom identifier, or
        // both. Check the sign after collecting every component because the
        // `&&` syntax permits `span` to occur on either side of the integer.
        (!span
            || ((integer.is_some() || name)
                && integer.is_none_or(|value| !value.negative && value.nonzero)))
            && (span || integer.is_some() || name)
    })
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
                        if horizontal {
                            area.columns
                        } else {
                            area.rows
                        }
                    } else {
                        0
                    }
            } else {
                explicit.checked_add(count.max(1) as usize)?
            }
        } else if count > 0 {
            count as usize - 1
        } else {
            (explicit + 1).saturating_sub((-count) as usize)
        };
        Some((Some(line.min(MAX_GRID_TRACKS)), 1, false))
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
                start: Some(end.saturating_sub(span)),
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
    // CSS Grid §7.6: lines and spans beyond the implementation limit clamp to
    // it instead of invalidating the placement.
    let mut placement = placement;
    placement.span = placement.span.clamp(1, MAX_GRID_TRACKS);
    if let Some(start) = placement.start.as_mut() {
        *start = (*start).min(MAX_GRID_TRACKS - 1);
        placement.span = placement.span.min(MAX_GRID_TRACKS - *start);
    }
    Some(placement)
}

fn grid_areas(raw: &str) -> Option<Arc<[GridArea]>> {
    if raw.trim().eq_ignore_ascii_case("none") {
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
    let lowered = ascii_lower(raw);
    let raw = lowered.as_ref();
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
            if !math_function(raw.trim()) && (length.pixels < 0.0 || length.percent < 0.0) {
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
    if raw.eq_ignore_ascii_case("none") {
        return Some(tracks.into());
    }
    for part in components(raw)? {
        if tracks.len() == MAX_GRID_TRACKS {
            return None;
        }
        // repeat() belongs to grid-template's track-list grammar, where it
        // is parsed together with named lines. grid-auto-{rows,columns} and
        // the track-size subgrammar do not admit it.
        if grid_function_args(part, "repeat").is_some() {
            return None;
        }
        tracks.push(if let Some(args) = grid_function_args(part, "minmax") {
            let args = comma_components(args, 2)?;
            if args.len() != 2 {
                return None;
            }
            let min = grid_breadth(args[0], context)?;
            if matches!(min, GridBreadth::Fraction(_)) {
                return None;
            }
            GridTrack::MinMax(min, grid_breadth(args[1], context)?)
        } else if let Some(args) = grid_function_args(part, "fit-content") {
            let length = transform_length(args, context)?;
            if !math_function(args.trim()) && (length.pixels < 0.0 || length.percent < 0.0) {
                return None;
            }
            if length.percent == 0.0 {
                GridTrack::FitContent(length.pixels)
            } else {
                GridTrack::FitContentLength(length)
            }
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
        });
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

fn grid_area_values(raw: &str) -> Option<Vec<Value>> {
    let segments = grid_line_segments(raw, 4)?;
    if segments.is_empty() || segments.iter().any(|line| !grid_line_spec(line, 1)) {
        return None;
    }
    let named_area = if segments.len() == 1 {
        let words = grid_words(segments[0])?;
        (words.len() == 1 && grid_identifier(words[0])).then(|| Arc::from(words[0]))
    } else {
        None
    };
    let row_start = segments[0];
    let column_start = segments.get(1).copied().unwrap_or(row_start);
    let row_end = segments.get(2).copied().unwrap_or(row_start);
    let column_end = segments.get(3).copied().unwrap_or(column_start);
    Some(alloc::vec![
        Value::GridArea(named_area),
        Value::GridRowSpec(Arc::from(alloc::format!("{row_start} / {row_end}"))),
        Value::GridColumnSpec(Arc::from(alloc::format!("{column_start} / {column_end}"))),
    ])
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

#[derive(Clone, Copy, Debug, PartialEq)]
struct LengthContext {
    font: f32,
    root_font: f32,
    ex: f32,
    ch: f32,
    viewport: MediaEnvironment,
    percent: Option<f32>,
}

fn font_length_context(
    text: Option<&dyn TextShaper>,
    font_size: f32,
    root_font_size: f32,
    font: &FontSpec,
    viewport: MediaEnvironment,
    percent: Option<f32>,
) -> LengthContext {
    let metrics = text.map_or(
        FontRelativeMetrics {
            ex: font_size * 0.5,
            ch: font_size * 0.5,
        },
        |text| text.font_relative_metrics_styled(font_size, font),
    );
    LengthContext {
        font: font_size,
        root_font: root_font_size,
        ex: metrics.ex,
        ch: metrics.ch,
        viewport,
        percent,
    }
}

fn style_length_context(
    text: Option<&dyn TextShaper>,
    style: &Style,
    viewport: MediaEnvironment,
    percent: Option<f32>,
) -> LengthContext {
    font_length_context(
        text,
        style.font_size,
        style.root_font_size,
        style.font_spec(),
        viewport,
        percent,
    )
}

#[derive(Clone, Debug, PartialEq)]
struct RelativeExpression {
    slot: usize,
    raw: Arc<str>,
    context: LengthContext,
    nonnegative: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SvgPaint {
    None,
    Color(Rgba),
    CurrentColor,
    /// An SVG paint server (for example `url(#gradient)`) is retained in the
    /// computed style so the shared renderer can report the unsupported
    /// capability instead of silently treating it as a solid color.
    Unsupported(Option<Rgba>),
    /// A decoded local URL reference and its CSS fallback, when supplied.
    Reference(Arc<str>, Option<Rgba>),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SvgFillRule {
    #[default]
    NonZero,
    EvenOdd,
}

/// Generated pseudo-element whose box is attached to an originating element.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PseudoElement {
    Before,
    After,
    Marker,
}

/// Computed `content` value. Item payloads remain typed so layout can resolve
/// attributes, counters, quotes, and images with their actual tree context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GeneratedContent {
    Normal,
    None,
    Items(Arc<[GeneratedContentItem]>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GeneratedContentItem {
    String(Arc<str>),
    Attribute {
        name: Arc<str>,
        fallback: Option<Arc<str>>,
    },
    Url(Arc<str>),
    Counter {
        name: Arc<str>,
        style: Arc<str>,
    },
    Counters {
        name: Arc<str>,
        separator: Arc<str>,
        style: Arc<str>,
    },
    OpenQuote,
    CloseQuote,
    NoOpenQuote,
    NoCloseQuote,
    /// Retains a recognized generated-content function that the layout
    /// backend has not implemented. It is never converted to text.
    UnsupportedFunction {
        name: Arc<str>,
        arguments: Arc<str>,
    },
    /// Alternative content after the `/` separator is metadata, not ink.
    AlternativeText(Arc<[GeneratedContentItem]>),
}

/// Computed quote-mark selection for CSS-generated quotation content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Quotes {
    Auto,
    None,
    MatchParent,
    Pairs(Arc<[(Arc<str>, Arc<str>)]>),
}

/// A parsed counter operation from `counter-reset`, `counter-increment`, or
/// `counter-set`. Reversed resets are retained so layout can report that the
/// currently supported counter subset does not cover them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CounterDirective {
    pub name: Arc<str>,
    pub value: i64,
    pub reversed: bool,
    /// Whether the source supplied an integer. This only affects reversed
    /// resets, whose omitted integer means automatic initialization.
    pub explicit_value: bool,
    /// A validated constant `calc()` value retained for specified CSSOM
    /// serialization. Computed serialization uses `value` instead.
    pub math_expression: Option<Arc<str>>,
}

/// Counter declaration whose grammar supplies the directive default value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CounterProperty {
    Reset,
    Increment,
    Set,
}

impl CounterProperty {
    const fn default_value(self) -> i64 {
        match self {
            Self::Reset | Self::Set => 0,
            Self::Increment => 1,
        }
    }

    const fn allows_reversed(self) -> bool {
        matches!(self, Self::Reset)
    }
}

/// This implementation's CSS counter bound, selected to keep integer
/// arithmetic deterministic across host targets while covering large WPT
/// overflow/underflow cases.
pub const MAX_COUNTER_VALUE: i64 = 2_100_000_000;

/// A resolved pseudo-element style plus its generated content items.
#[derive(Clone, Debug)]
pub struct GeneratedStyle {
    pub pseudo: PseudoElement,
    pub style: Style,
    pub content: GeneratedContent,
}

pub const MAX_GENERATED_CONTENT_ITEMS: usize = 64;
pub const MAX_GENERATED_CONTENT_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct StyleExtras {
    /// Computed animation longhands in canonical property order. Values stay
    /// serialized so the HTML animation adapter can apply CSS list repetition.
    pub animation: [Option<Arc<str>>; 9],
    pub font: FontSpec,
    /// The initial `auto` value is distinct from an explicitly specified zero.
    pub min_width_auto: bool,
    pub min_height_auto: bool,
    /// `flex-basis: content` differs from `auto` when the main size is set.
    pub flex_basis_content: bool,
    /// Intrinsic sizing keyword retained until flex layout has the available
    /// main size and can compute the corresponding intrinsic contribution.
    pub flex_basis_intrinsic: Option<IntrinsicSizing>,
    /// A computed value used element sibling position and must be recascaded
    /// after the child list changes.
    pub sibling_position_dependent: bool,
    pub gap_specified: bool,
    pub writing_mode: WritingMode,
    pub contain_paint: bool,
    pub contain_layout: bool,
    pub contain_size: bool,
    pub visibility_visible: bool,
    pub pointer_events_auto: bool,
    /// Inherited SVG paint properties used by the shared vector renderer.
    pub svg_fill: SvgPaint,
    pub svg_stroke: SvgPaint,
    pub svg_stroke_width: f32,
    pub svg_fill_rule: SvgFillRule,
    pub svg_clip_rule: SvgFillRule,
    pub svg_clip_path: Option<Arc<str>>,
    /// CSS geometry values for SVG shapes, retained until the current user
    /// viewport is available to the SVG geometry resolver.
    pub svg_geometry: [Option<Arc<str>>; 9],
    pub svg_stop_color: Rgba,
    pub svg_stop_opacity: f32,
    /// `content` is evaluated against the pseudo-element's originating node
    /// by layout; it is deliberately not inherited.
    pub generated_content: Option<Arc<[GeneratedContentItem]>>,
    pub content_none: bool,
    /// Computed `color-scheme` support serialized in canonical token order.
    /// `None` represents the initial `normal` value.
    pub color_scheme: Option<Arc<str>>,
    /// Parsed transition time lists. `None` preserves the initial `0s` values
    /// without allocating extras for styles that do not declare transitions.
    pub transition_duration: Option<Arc<[typed_numeric::NumericValue]>>,
    pub transition_delay: Option<Arc<[typed_numeric::NumericValue]>>,
    pub quotes: Quotes,
    pub counter_reset: Option<Arc<[CounterDirective]>>,
    pub counter_increment: Option<Arc<[CounterDirective]>>,
    pub counter_set: Option<Arc<[CounterDirective]>>,
    pub list_style_type: ListStyleType,
    pub list_style_position: ListStylePosition,
    /// Inherited marker image source. `None` is the initial CSS `none` value.
    pub list_style_image: Option<Arc<str>>,
    pub empty_cells_hide: bool,
    pub caption_bottom: bool,
    pub border_collapse: bool,
    pub logical_border_width: [Option<f32>; 4],
    pub logical_border_color: [Option<Rgba>; 4],
    pub logical_border_style: [Option<BorderStyle>; 4],
    pub border_width_sides: [Option<f32>; 4],
    pub border_color_sides: [Option<Rgba>; 4],
    pub border_solid_sides: [Option<bool>; 4],
    pub border_style_sides: [Option<BorderStyle>; 4],
    /// Per-corner elliptical radii. `None` preserves the scalar fast path.
    pub border_corner_radii: Option<[BorderRadiusCorner; 4]>,
    pub height_intrinsic: Option<IntrinsicSizing>,
    pub min_height_intrinsic: Option<IntrinsicSizing>,
    pub max_height_intrinsic: Option<IntrinsicSizing>,
    pub logical_inline_size: Option<f32>,
    pub logical_block_size: Option<f32>,
    pub logical_min_size: [Option<f32>; 2],
    pub logical_max_size: [Option<f32>; 2],
    pub logical_offsets: [Option<Option<f32>>; 4],
    pub logical_padding_inline: [Option<f32>; 2],
    pub logical_padding_block: [Option<f32>; 2],
    pub logical_margin_inline: [Option<f32>; 2],
    pub logical_margin_block: [Option<Option<f32>>; 2],
    pub column_count: Option<usize>,
    pub column_gap: Option<f32>,
    pub row_gap_fraction: f32,
    pub column_gap_fraction: f32,
    pub column_fill_auto: bool,
    pub column_rule_width: f32,
    pub column_rule_color: Option<Rgba>,
    pub column_rule_visible: bool,
    pub column_rule_pattern: Option<BorderPattern>,
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
    pub grid_columns_auto: Option<GridAutoRepeat>,
    pub grid_rows_auto: Option<GridAutoRepeat>,
    pub transforms: Option<Arc<[Transform]>>,
    pub transform_origin: [TransformLength; 2],
    pub border_pattern: Option<BorderPattern>,
    pub shadows: Option<Arc<[BoxShadow]>>,
    pub background_images: Option<Arc<[BackgroundImage]>>,
    pub background_position: Option<Arc<[[LengthPercentage; 2]]>>,
    pub background_size: Option<Arc<[BackgroundSize]>>,
    pub background_repeat: Option<Arc<[[BackgroundRepeat; 2]]>>,
    pub background_clip: Option<Arc<[BackgroundBox]>>,
    pub background_origin: Option<Arc<[BackgroundBox]>>,
    pub background_attachment: Option<Arc<[BackgroundAttachment]>>,
    pub overflow_x: Overflow,
    pub overflow_y: Overflow,
    pub white_space: WhiteSpace,
    pub text_align: TextAlign,
    /// First-line indentation, retained as a percentage of block width.
    pub text_indent: LengthPercentage,
    /// `None` preserves the computed CSS value `normal`; a length is the
    /// additional advance between typographic character units.
    pub letter_spacing: Option<f32>,
    /// Additional spacing after word separators. Percentages are retained as
    /// a fraction of each separator's shaped advance until layout.
    pub word_spacing: LengthPercentage,
    pub vertical_align: VerticalAlign,
    pub direction: Direction,
    pub text_decoration: u8,
    /// Extra content offset used when aligning a table cell's contents in its row.
    pub table_cell_content_offset: f32,
    pub root_font_size: f32,
    pub min_height: f32,
    pub max_height: Option<f32>,
    pub aspect_ratio: Option<f32>,
    pub z_index: Option<i32>,
    relative_lengths: Vec<(usize, RelativeLength)>,
    relative_expressions: Vec<RelativeExpression>,
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
    pub grid_columns: Option<Arc<[GridTrack]>>,
    pub grid_rows: Option<Arc<[GridTrack]>>,
}

static INITIAL_EXTRAS: StyleExtras = StyleExtras {
    animation: [const { None }; 9],
    font: FontSpec {
        families: None,
        weight: 400,
        style: FontStyle::Normal,
        stretch: 100.0,
        size_adjust: None,
    },
    min_width_auto: true,
    min_height_auto: true,
    flex_basis_content: false,
    flex_basis_intrinsic: None,
    sibling_position_dependent: false,
    gap_specified: false,
    writing_mode: WritingMode::HorizontalTb,
    contain_paint: false,
    contain_layout: false,
    contain_size: false,
    visibility_visible: true,
    pointer_events_auto: true,
    svg_fill: SvgPaint::Color(Rgba {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    }),
    svg_stroke: SvgPaint::None,
    svg_stroke_width: 1.0,
    svg_fill_rule: SvgFillRule::NonZero,
    svg_clip_rule: SvgFillRule::NonZero,
    svg_clip_path: None,
    svg_geometry: [const { None }; 9],
    svg_stop_color: Rgba {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    },
    svg_stop_opacity: 1.0,
    generated_content: None,
    content_none: false,
    color_scheme: None,
    transition_duration: None,
    transition_delay: None,
    quotes: Quotes::Auto,
    counter_reset: None,
    counter_increment: None,
    counter_set: None,
    list_style_type: ListStyleType::Disc,
    list_style_position: ListStylePosition::Outside,
    list_style_image: None,
    empty_cells_hide: false,
    caption_bottom: false,
    border_collapse: false,
    logical_border_width: [None; 4],
    logical_border_color: [None; 4],
    logical_border_style: [None; 4],
    border_width_sides: [None; 4],
    border_color_sides: [None; 4],
    border_solid_sides: [None; 4],
    border_style_sides: [None; 4],
    border_corner_radii: None,
    height_intrinsic: None,
    min_height_intrinsic: None,
    max_height_intrinsic: None,
    logical_inline_size: None,
    logical_block_size: None,
    logical_min_size: [None; 2],
    logical_max_size: [None; 2],
    logical_offsets: [None; 4],
    logical_padding_inline: [None; 2],
    logical_padding_block: [None; 2],
    logical_margin_inline: [None; 2],
    logical_margin_block: [None; 2],
    column_count: None,
    column_gap: None,
    row_gap_fraction: 0.0,
    column_gap_fraction: 0.0,
    column_fill_auto: false,
    column_rule_width: 0.0,
    column_rule_color: None,
    column_rule_visible: false,
    column_rule_pattern: None,
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
    grid_columns_auto: None,
    grid_rows_auto: None,
    transforms: None,
    transform_origin: [TransformLength {
        pixels: 0.0,
        percent: 50.0,
    }; 2],
    border_pattern: None,
    shadows: None,
    background_images: None,
    background_position: None,
    background_size: None,
    background_repeat: None,
    background_clip: None,
    background_origin: None,
    background_attachment: None,
    overflow_x: Overflow::Visible,
    overflow_y: Overflow::Visible,
    white_space: WhiteSpace::Normal,
    text_align: TextAlign::Start,
    text_indent: LengthPercentage {
        pixels: 0.0,
        fraction: 0.0,
    },
    letter_spacing: None,
    word_spacing: LengthPercentage {
        pixels: 0.0,
        fraction: 0.0,
    },
    vertical_align: VerticalAlign::Baseline,
    direction: Direction::Ltr,
    text_decoration: 0,
    table_cell_content_offset: 0.0,
    root_font_size: 16.0,
    min_height: 0.0,
    max_height: None,
    aspect_ratio: None,
    z_index: None,
    relative_lengths: Vec::new(),
    relative_expressions: Vec::new(),
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
    grid_columns: None,
    grid_rows: None,
};

#[derive(Clone, Debug)]
pub struct Style {
    extras: Option<Arc<StyleExtras>>,
    custom: Option<Arc<[(String, Option<String>)]>>,
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
    pub border_style: BorderStyle,
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
    /// Horizontal and vertical table border spacing.
    pub border_spacing: [f32; 2],
    pub grid_column: GridPlacement,
    pub grid_row: GridPlacement,
}

fn arcs_equal<T: ?Sized + PartialEq>(a: &Option<Arc<T>>, b: &Option<Arc<T>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b) || a == b,
        _ => false,
    }
}

impl PartialEq for Style {
    fn eq(&self, other: &Self) -> bool {
        let Style {
            extras,
            custom,
            display,
            opacity,
            color,
            background,
            width,
            min_width,
            max_width,
            height,
            margin,
            padding,
            font_size,
            border_radius,
            border_width,
            border_color,
            border_solid,
            border_style,
            overflow_clip,
            line_height,
            flex_direction,
            flex_wrap,
            justify_content,
            align_items,
            gap,
            flex_grow,
            flex_shrink,
            flex_basis,
            box_sizing,
            table_fixed,
            border_spacing,
            grid_column,
            grid_row,
        } = self;
        *display == other.display
            && *opacity == other.opacity
            && *color == other.color
            && *background == other.background
            && *width == other.width
            && *min_width == other.min_width
            && *max_width == other.max_width
            && *height == other.height
            && *margin == other.margin
            && *padding == other.padding
            && *font_size == other.font_size
            && *border_radius == other.border_radius
            && *border_width == other.border_width
            && *border_color == other.border_color
            && *border_solid == other.border_solid
            && *border_style == other.border_style
            && *overflow_clip == other.overflow_clip
            && *line_height == other.line_height
            && *flex_direction == other.flex_direction
            && *flex_wrap == other.flex_wrap
            && *justify_content == other.justify_content
            && *align_items == other.align_items
            && *gap == other.gap
            && *flex_grow == other.flex_grow
            && *flex_shrink == other.flex_shrink
            && *flex_basis == other.flex_basis
            && *box_sizing == other.box_sizing
            && *table_fixed == other.table_fixed
            && *border_spacing == other.border_spacing
            && *grid_column == other.grid_column
            && *grid_row == other.grid_row
            && arcs_equal(custom, &other.custom)
            && arcs_equal(extras, &other.extras)
    }
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
    /// Build the inherited style context for children of a `display: contents`
    /// element. The element has no principal box, so its box, positioning,
    /// background, and layout declarations do not participate in child layout.
    pub(crate) fn display_contents_child_style(&self) -> Self {
        let mut child = Self::initial();
        child.display = Display::Block;
        child.color = self.color;
        child.font_size = self.font_size;
        child.line_height = self.line_height;
        child.custom = self.custom.clone();

        let source = self.extras.as_deref().unwrap_or(&INITIAL_EXTRAS);
        let target = Arc::make_mut(
            child
                .extras
                .get_or_insert_with(|| Arc::new(INITIAL_EXTRAS.clone())),
        );
        target.font = source.font.clone();
        target.root_font_size = source.root_font_size;
        target.writing_mode = source.writing_mode;
        target.visibility_visible = source.visibility_visible;
        target.pointer_events_auto = source.pointer_events_auto;
        target.svg_fill = source.svg_fill.clone();
        target.svg_stroke = source.svg_stroke.clone();
        target.svg_stroke_width = source.svg_stroke_width;
        target.svg_fill_rule = source.svg_fill_rule;
        target.svg_clip_rule = source.svg_clip_rule;
        target.color_scheme = source.color_scheme.clone();
        target.quotes = source.quotes.clone();
        target.white_space = source.white_space;
        target.text_align = source.text_align;
        target.text_indent = source.text_indent;
        target.letter_spacing = source.letter_spacing;
        target.word_spacing = source.word_spacing;
        target.direction = source.direction;
        target.list_style_type = source.list_style_type.clone();
        target.list_style_position = source.list_style_position;
        target.list_style_image = source.list_style_image.clone();
        target.text_decoration = source.text_decoration;
        child
    }

    /// The resolved font selection used by layout and shared text shaping.
    pub fn font_spec(&self) -> &FontSpec {
        &self.extras.as_deref().unwrap_or(&INITIAL_EXTRAS).font
    }

    pub fn generated_content(&self) -> GeneratedContent {
        let extras = self.extras.as_deref().unwrap_or(&INITIAL_EXTRAS);
        if extras.content_none {
            GeneratedContent::None
        } else if let Some(items) = &extras.generated_content {
            GeneratedContent::Items(items.clone())
        } else {
            GeneratedContent::Normal
        }
    }

    pub fn quotes(&self) -> &Quotes {
        &self.quotes
    }

    /// Parsed directives for a counter property, or `None` for its initial
    /// `none` value. The property default is already applied to each entry.
    pub fn counter_directives(&self, property: CounterProperty) -> Option<&[CounterDirective]> {
        match property {
            CounterProperty::Reset => self.counter_reset.as_deref(),
            CounterProperty::Increment => self.counter_increment.as_deref(),
            CounterProperty::Set => self.counter_set.as_deref(),
        }
    }

    /// Physical padding values in top, right, bottom, left order.
    pub fn padding_sides(&self) -> [f32; 4] {
        self.extras
            .as_deref()
            .unwrap_or(&INITIAL_EXTRAS)
            .padding_sides
    }

    /// Physical border widths in top, right, bottom, left order.
    pub fn border_width_sides(&self) -> [f32; 4] {
        self.extras
            .as_deref()
            .unwrap_or(&INITIAL_EXTRAS)
            .border_width_sides
            .map(|width| width.unwrap_or(self.border_width))
    }

    /// Computed border styles in top, right, bottom, left order.
    pub fn border_styles(&self) -> [BorderStyle; 4] {
        self.extras
            .as_deref()
            .unwrap_or(&INITIAL_EXTRAS)
            .border_style_sides
            .map(|style| style.unwrap_or(self.border_style))
    }

    /// Resolve the four elliptical corner radii against the element's own
    /// border box, then apply the CSS common overlap reduction.
    ///
    /// The returned corners are top-left, top-right, bottom-right,
    /// bottom-left; each pair is horizontal then vertical.
    pub fn corner_radii(&self, border_box_width: f32, border_box_height: f32) -> [[f32; 2]; 4] {
        let width = if border_box_width.is_finite() {
            border_box_width.max(0.0)
        } else {
            0.0
        };
        let height = if border_box_height.is_finite() {
            border_box_height.max(0.0)
        } else {
            0.0
        };
        let resolve = |length: &BorderRadiusLength, basis: f32| {
            let value = if let Some(expression) = &length.expression {
                contextual_length(
                    expression,
                    Some(LengthContext {
                        percent: Some(basis),
                        ..length.context
                    }),
                )
                .unwrap_or(0.0)
            } else {
                length.value.resolve(basis)
            };
            if value.is_finite() {
                value.max(0.0)
            } else {
                0.0
            }
        };
        let mut result = if let Some(corners) = &self.border_corner_radii {
            core::array::from_fn(|index| {
                [
                    resolve(&corners[index].horizontal, width),
                    resolve(&corners[index].vertical, height),
                ]
            })
        } else {
            let radius = if self.border_radius.is_finite() {
                self.border_radius.max(0.0)
            } else {
                0.0
            };
            [[radius, radius]; 4]
        };

        let mut scale = 1.0f32;
        for (first, second, axis, basis) in [
            (0, 1, 0, width),
            (3, 2, 0, width),
            (0, 3, 1, height),
            (1, 2, 1, height),
        ] {
            let sum = result[first][axis] + result[second][axis];
            if sum > 0.0 {
                scale = scale.min(basis / sum);
            }
        }
        if scale.is_finite() {
            scale = scale.clamp(0.0, 1.0);
            for corner in &mut result {
                corner[0] *= scale;
                corner[1] *= scale;
            }
        }
        result
    }

    /// Serialize the computed shorthand while preserving percentages and
    /// elliptical radii for CSSOM.
    pub fn border_radius_css_value(&self) -> String {
        serialize_border_radius(self)
    }

    /// Serialize one computed physical corner longhand, retaining percentages
    /// and non-affine math until the element's border box is known.
    pub fn border_radius_corner_css_value(&self, index: usize) -> String {
        let corners = border_radius_corners(self);
        let corner = corners.get(index).unwrap_or(&corners[0]);
        let horizontal = serialize_radius_length(&corner.horizontal);
        let vertical = serialize_radius_length(&corner.vertical);
        if horizontal == vertical {
            horizontal
        } else {
            alloc::format!("{horizontal} {vertical}")
        }
    }

    pub fn text_align(&self) -> TextAlign {
        self.extras.as_deref().unwrap_or(&INITIAL_EXTRAS).text_align
    }

    pub fn direction(&self) -> Direction {
        self.extras.as_deref().unwrap_or(&INITIAL_EXTRAS).direction
    }

    pub fn writing_mode(&self) -> WritingMode {
        self.extras
            .as_deref()
            .unwrap_or(&INITIAL_EXTRAS)
            .writing_mode
    }

    pub fn white_space(&self) -> WhiteSpace {
        self.extras
            .as_deref()
            .unwrap_or(&INITIAL_EXTRAS)
            .white_space
    }

    pub fn custom_properties(&self) -> &[(String, Option<String>)] {
        self.custom.as_deref().unwrap_or(&[])
    }
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
            let local = transform.matrix(rect.width, rect.height);
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
        if let Some(expression) = self
            .relative_expressions
            .iter()
            .find(|expression| expression.slot == 20)
        {
            return basis
                .and_then(|basis| {
                    contextual_length(
                        &expression.raw,
                        Some(LengthContext {
                            percent: Some(basis),
                            ..expression.context
                        }),
                    )
                })
                .map(|value| value.max(0.0));
        }
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
                result.min_height_auto = false;
            } else if slot == 52 {
                result.max_height = None;
            }
        }
        if !self.relative_lengths.is_empty() {
            result.relative_lengths.clear();
        }
        for expression in &self.relative_expressions {
            if expression.slot == 20 {
                result.flex_basis = None;
                continue;
            }
            let basis = if matches!(expression.slot, 4 | 35 | 37 | 51 | 52) {
                height
            } else {
                Some(width)
            };
            if let Some(value) = basis.and_then(|basis| {
                contextual_length(
                    &expression.raw,
                    Some(LengthContext {
                        percent: Some(basis),
                        ..expression.context
                    }),
                )
            }) {
                if let Some(value) = length_value(
                    expression.slot,
                    if expression.nonnegative {
                        value.max(0.0)
                    } else {
                        value
                    },
                ) {
                    value.apply(&mut result);
                }
            } else if expression.slot == 4 {
                result.height = None;
            } else if expression.slot == 51 {
                result.min_height = 0.0;
                result.min_height_auto = false;
            } else if expression.slot == 52 {
                result.max_height = None;
            }
        }
        if !self.relative_expressions.is_empty() {
            result.relative_expressions.clear();
        }
        result
    }
    pub fn initial() -> Self {
        Self {
            extras: None,
            custom: None,
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
            border_style: BorderStyle::None,
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
            border_spacing: [0.0, 0.0],
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
    form_state: crate::forms::FormStateSet,
    interaction_state: crate::interaction::InteractionSet,
    top_layer_state: u8,
    /// 0 means no `:dir()` constraint, 1/2 mean LTR/RTL, and 3 means
    /// contradictory repeated constraints.
    directionality_mask: u8,
    structural: Vec<StructuralPseudo>,
    logical: Vec<LogicalPseudo>,
    root: bool,
    scope: bool,
    universal: bool,
    /// `:host` — matches the shadow host of the rule's scope.
    host: bool,
    /// `::slotted(compound)` — matches light children assigned to a slot.
    slotted: bool,
    /// `::part(name)` — matches shadow elements exposing the named part.
    part: Option<String>,
    /// Present only while parsing CSS nesting selector lists. The adapter
    /// retains source text and consumes this marker as syntax metadata; normal
    /// stylesheet selectors are never left unresolved with a nesting marker.
    nesting_selector: bool,
    /// Terminal generated pseudo-element attached to the matched element.
    pseudo_element: Option<PseudoElement>,
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    specificity: (u16, u16, u16),
    ancestor: Option<(Relation, Box<Selector>)>,
    attributes: Vec<AttributeSelector>,
}

#[derive(Clone, Debug)]
enum LogicalPseudo {
    Not(Vec<Selector>),
    Is(Vec<Selector>),
    Where(Vec<Selector>),
    Has(Vec<RelativeSelector>),
}

impl LogicalPseudo {
    fn selectors(&self) -> &[Selector] {
        match self {
            Self::Not(selectors) | Self::Is(selectors) | Self::Where(selectors) => selectors,
            Self::Has(_) => &[],
        }
    }
}

#[derive(Clone, Debug)]
struct RelativeSelector {
    /// The implicit descendant or explicit combinator from the :has anchor to
    /// the leftmost compound in this relative selector.
    relation: Relation,
    selector: Selector,
}

fn adjust_specificity(
    current: (u16, u16, u16),
    old: (u16, u16, u16),
    new: (u16, u16, u16),
) -> (u16, u16, u16) {
    (
        current.0.saturating_sub(old.0).saturating_add(new.0),
        current.1.saturating_sub(old.1).saturating_add(new.1),
        current.2.saturating_sub(old.2).saturating_add(new.2),
    )
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

fn directionality_argument(input: &str, offset: usize) -> Result<u8, CssError> {
    let mut position = 0;
    skip_css_space_comments(input, &mut position)
        .ok_or_else(|| selector_error(offset + position, "unterminated :dir() comment"))?;
    let value = consume_selector_identifier(input, &mut position)
        .ok_or_else(|| selector_error(offset + position, ":dir() requires ltr or rtl"))?;
    skip_css_space_comments(input, &mut position)
        .ok_or_else(|| selector_error(offset + position, "unterminated :dir() comment"))?;
    if position != input.len() {
        return Err(selector_error(
            offset + position,
            ":dir() takes one direction",
        ));
    }
    if value.eq_ignore_ascii_case("ltr") {
        Ok(1)
    } else if value.eq_ignore_ascii_case("rtl") {
        Ok(2)
    } else {
        Err(selector_error(offset, ":dir() requires ltr or rtl"))
    }
}

fn combine_directionality_constraints(left: u8, right: u8) -> u8 {
    match (left, right) {
        (0, value) | (value, 0) => value,
        (3, _) | (_, 3) => 3,
        (left, right) if left == right => left,
        _ => 3,
    }
}

#[derive(Clone, Debug)]
enum StructuralPseudo {
    Empty,
    Valid,
    Invalid,
    Nth {
        a: i32,
        b: i32,
        of_type: bool,
        reverse: bool,
        of: Option<Vec<Selector>>,
    },
    Last(bool),
    Only(bool),
}

/// Split the optional `of <complex-real-selector-list>` part from an An+B
/// expression. `of` is a keyword only when it starts at the top level after
/// whitespace; the same letters inside an attribute, string, escape, or
/// nested pseudo-class belong to the selector argument.
fn nth_of_separator(input: &str, offset: usize) -> Result<Option<(usize, usize)>, CssError> {
    let bytes = input.as_bytes();
    let (mut pos, mut brackets, mut parentheses, mut quote, mut separated) =
        (0usize, 0usize, 0usize, 0u8, false);
    while pos < bytes.len() {
        let byte = bytes[pos];
        if byte == b'\\' {
            selector_escape(input, &mut pos)
                .ok_or_else(|| selector_error(offset + pos, "invalid selector escape"))?;
            separated = false;
            continue;
        }
        if quote != 0 {
            if byte == quote {
                quote = 0;
            }
            pos += input[pos..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        if byte == b'/' && bytes.get(pos + 1) == Some(&b'*') {
            let Some(end) = input[pos + 2..].find("*/") else {
                return Err(selector_error(
                    offset + pos,
                    "unterminated selector comment",
                ));
            };
            // Comments are omitted by CSS tokenization. Preserve whether
            // whitespace already separated the formula from a following
            // `of` token, while allowing comments after the keyword itself.
            pos += end + 4;
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            quote = byte;
            separated = false;
            pos += 1;
            continue;
        }
        match byte {
            b'[' => {
                brackets += 1;
                separated = false;
            }
            b']' => {
                brackets = brackets.saturating_sub(1);
                separated = false;
            }
            b'(' => {
                parentheses += 1;
                separated = false;
            }
            b')' => {
                parentheses = parentheses.saturating_sub(1);
                separated = false;
            }
            _ if brackets == 0 && parentheses == 0 && is_css_whitespace(byte) => {
                separated = true;
                pos += 1;
                continue;
            }
            _ => {
                if brackets == 0
                    && parentheses == 0
                    && separated
                    && input[pos..]
                        .get(..2)
                        .is_some_and(|token| token.eq_ignore_ascii_case("of"))
                {
                    let end = pos + 2;
                    let continues_ident = bytes.get(end).is_some_and(|next| {
                        next.is_ascii_alphanumeric()
                            || matches!(next, b'-' | b'_' | b'\\')
                            || *next >= 0x80
                    });
                    if !continues_ident {
                        return Ok(Some((pos, end)));
                    }
                }
                separated = false;
            }
        }
        pos += input[pos..].chars().next().map_or(1, char::len_utf8);
    }
    Ok(None)
}

/// Remove CSS comments while keeping escaped delimiters and quoted attribute
/// values intact. Selector parsing works on component values, where comments
/// are consumed before selector tokens are interpreted.
fn strip_selector_comments(input: &str, offset: usize) -> Result<String, CssError> {
    let bytes = input.as_bytes();
    let mut result = String::with_capacity(input.len());
    let (mut pos, mut quote) = (0usize, 0u8);
    while pos < bytes.len() {
        let byte = bytes[pos];
        if byte == b'\\' {
            let start = pos;
            selector_escape(input, &mut pos)
                .ok_or_else(|| selector_error(offset + pos, "invalid selector escape"))?;
            result.push_str(&input[start..pos]);
        } else if quote != 0 {
            if byte == quote {
                quote = 0;
            }
            let character = input[pos..]
                .chars()
                .next()
                .ok_or_else(|| selector_error(offset + pos, "invalid selector character"))?;
            result.push(character);
            pos += character.len_utf8();
        } else if matches!(byte, b'\'' | b'"') {
            quote = byte;
            result.push(byte as char);
            pos += 1;
        } else if byte == b'/' && bytes.get(pos + 1) == Some(&b'*') {
            let Some(end) = input[pos + 2..].find("*/") else {
                return Err(selector_error(
                    offset + pos,
                    "unterminated selector comment",
                ));
            };
            pos += end + 4;
        } else {
            let character = input[pos..]
                .chars()
                .next()
                .ok_or_else(|| selector_error(offset + pos, "invalid selector character"))?;
            result.push(character);
            pos += character.len_utf8();
        }
    }
    Ok(result)
}

fn parse_nth_argument(
    input: &str,
    offset: usize,
    depth: usize,
    allow_of: bool,
    allow_nesting: bool,
) -> Result<((i32, i32), Option<Vec<Selector>>), CssError> {
    let separator = nth_of_separator(input, offset)?;
    let (formula, selector_list, selector_offset) = match separator {
        Some((start, end)) if allow_of => (
            &input[..start],
            Some(&input[end..]),
            offset.saturating_add(end),
        ),
        Some(_) => {
            return Err(selector_error(
                offset,
                "of selector lists are only valid with nth-child() and nth-last-child()",
            ));
        }
        None => (input, None, offset),
    };
    let formula = strip_selector_comments(formula, offset)?;
    let expression = nth_expression(&formula)
        .ok_or_else(|| selector_error(offset, "invalid nth pseudo-class"))?;
    let selectors = selector_list
        .map(|input| {
            let input = strip_selector_comments(input, selector_offset)?;
            let selectors = parse_selector_list_depth(
                &input,
                selector_offset,
                depth.saturating_add(1),
                false,
                allow_nesting,
            )?;
            if selectors.iter().any(Selector::has_pseudo_element) {
                return Err(selector_error(
                    selector_offset,
                    "pseudo-elements are not valid in an nth-child selector argument",
                ));
            }
            Ok(selectors)
        })
        .transpose()?;
    Ok((expression, selectors))
}

fn nth_expression(input: &str) -> Option<(i32, i32)> {
    let input = input.trim_ascii();
    if input.eq_ignore_ascii_case("odd") {
        return Some((2, 1));
    }
    if input.eq_ignore_ascii_case("even") {
        return Some((2, 0));
    }

    let bytes = input.as_bytes();
    let n = bytes
        .iter()
        .position(|byte| byte.eq_ignore_ascii_case(&b'n'));
    let Some(n) = n else {
        let integer = input
            .strip_prefix('+')
            .or_else(|| input.strip_prefix('-'))
            .unwrap_or(input);
        if integer.is_empty() || !integer.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        return Some((0, input.parse().ok()?));
    };

    // The coefficient is one CSS dimension token: signs and digits must be
    // adjacent to `n` (`+n`, `-2n`, `12n`), while whitespace before the whole
    // expression has already been trimmed.
    let coefficient = &input[..n];
    if coefficient.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return None;
    }
    let a = match coefficient {
        "" | "+" => 1,
        "-" => -1,
        _ => coefficient.parse().ok()?,
    };

    let suffix = input[n + 1..].trim_ascii_start();
    if suffix.is_empty() {
        return Some((a, 0));
    }
    let (sign, digits) = match suffix.as_bytes().first()? {
        b'+' => (1i64, &suffix[1..]),
        b'-' => (-1i64, &suffix[1..]),
        _ => return None,
    };
    let digits = digits.trim_ascii_start();
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let magnitude = digits.parse::<i64>().ok()?;
    let b = i32::try_from(sign.checked_mul(magnitude)?).ok()?;
    Some((a, b))
}

fn is_part_identifier(input: &str) -> bool {
    let mut chars = input.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let name_char =
        |ch: char| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') || !ch.is_ascii();
    if first == '-' {
        let Some(second) = chars.next() else {
            return false;
        };
        if second == '-' {
            return chars.all(name_char);
        }
        if second.is_ascii_alphabetic() || second == '_' || !second.is_ascii() {
            return chars.all(name_char);
        }
        return false;
    }
    if first.is_ascii_alphabetic() || first == '_' || !first.is_ascii() {
        chars.all(name_char)
    } else {
        false
    }
}

#[derive(Clone, Debug)]
struct AttributeSelector {
    name: String,
    namespace: AttributeNamespace,
    operator: AttributeOperator,
    value: Option<String>,
    case_sensitivity: AttributeCaseSensitivity,
    html_default_ascii_insensitive: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttributeNamespace {
    /// An unprefixed attribute selector matches attributes in no namespace.
    None,
    /// A `*|name` selector matches the local name in any namespace.
    Any,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttributeOperator {
    Exists,
    Equals,
    Includes,
    DashMatch,
    Prefix,
    Suffix,
    Substring,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttributeCaseSensitivity {
    Default,
    AsciiInsensitive,
    Sensitive,
}

impl AttributeSelector {
    fn matches_value(&self, actual: &str, html_default_ascii_insensitive: bool) -> bool {
        let Some(expected) = self.value.as_deref() else {
            return self.operator == AttributeOperator::Exists;
        };
        let ascii_insensitive = match self.case_sensitivity {
            AttributeCaseSensitivity::Default => html_default_ascii_insensitive,
            AttributeCaseSensitivity::AsciiInsensitive => true,
            AttributeCaseSensitivity::Sensitive => false,
        };
        let equals = |left: &str, right: &str| match self.case_sensitivity {
            AttributeCaseSensitivity::AsciiInsensitive => left.eq_ignore_ascii_case(right),
            AttributeCaseSensitivity::Default | AttributeCaseSensitivity::Sensitive
                if ascii_insensitive =>
            {
                left.eq_ignore_ascii_case(right)
            }
            AttributeCaseSensitivity::Default | AttributeCaseSensitivity::Sensitive => {
                left == right
            }
        };
        match self.operator {
            AttributeOperator::Exists => true,
            AttributeOperator::Equals => equals(actual, expected),
            AttributeOperator::Includes => {
                !expected.is_empty()
                    && actual
                        .split_ascii_whitespace()
                        .any(|token| equals(token, expected))
            }
            AttributeOperator::DashMatch => {
                equals(actual, expected)
                    || actual
                        .get(..expected.len())
                        .filter(|prefix| equals(prefix, expected))
                        .is_some_and(|_| actual.as_bytes().get(expected.len()) == Some(&b'-'))
            }
            AttributeOperator::Prefix => {
                !expected.is_empty()
                    && actual
                        .get(..expected.len())
                        .is_some_and(|prefix| equals(prefix, expected))
            }
            AttributeOperator::Suffix => {
                !expected.is_empty()
                    && actual
                        .len()
                        .checked_sub(expected.len())
                        .and_then(|start| actual.get(start..))
                        .is_some_and(|suffix| equals(suffix, expected))
            }
            AttributeOperator::Substring => {
                if expected.is_empty() {
                    false
                } else if ascii_insensitive {
                    let needle = expected.as_bytes();
                    actual.as_bytes().len() >= needle.len()
                        && actual
                            .as_bytes()
                            .windows(needle.len())
                            .any(|window| window.eq_ignore_ascii_case(needle))
                } else {
                    actual.contains(expected)
                }
            }
        }
    }
}

const HTML_ASCII_INSENSITIVE_ATTRIBUTE_VALUES: &[&str] = &[
    "accept",
    "accept-charset",
    "align",
    "alink",
    "axis",
    "bgcolor",
    "charset",
    "checked",
    "clear",
    "codetype",
    "color",
    "compact",
    "declare",
    "defer",
    "dir",
    "direction",
    "disabled",
    "enctype",
    "face",
    "frame",
    "hreflang",
    "http-equiv",
    "lang",
    "language",
    "link",
    "media",
    "method",
    "multiple",
    "nohref",
    "noresize",
    "noshade",
    "nowrap",
    "readonly",
    "rel",
    "rev",
    "rules",
    "scope",
    "scrolling",
    "selected",
    "shape",
    "target",
    "text",
    "type",
    "valign",
    "valuetype",
    "vlink",
];

fn html_default_ascii_insensitive_attribute_value(name: &str) -> bool {
    HTML_ASCII_INSENSITIVE_ATTRIBUTE_VALUES
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Relation {
    Descendant,
    Child,
    Adjacent,
    Following,
}

#[derive(Clone, Copy, Debug)]
enum LogicalBorderComponent {
    Width(f32),
    Color(Rgba),
    CurrentColor,
    Style(BorderStyle),
}

#[derive(Clone, Debug)]
enum Value {
    Animation(usize, Arc<str>),
    TransitionTimes(usize, Arc<[typed_numeric::NumericValue]>),
    ColorRaw(usize, Arc<str>),
    Transforms(Option<Arc<[Transform]>>),
    TransformRaw(String),
    TransformOrigin([TransformLength; 2]),
    TransformOriginRaw(String),
    BorderPattern(BorderPattern),
    BorderCurrentColor,
    BackgroundCurrentColor,
    Shadows(Option<Arc<[BoxShadow]>>),
    ShadowsRaw(String),
    WhiteSpace(WhiteSpace),
    TextAlign(TextAlign),
    TextIndent(LengthPercentage),
    TextIndentRaw(String),
    LetterSpacing(Option<f32>),
    WordSpacing(LengthPercentage),
    Direction(Direction),
    TextDecoration(u8),
    BackgroundImageNone,
    BackgroundImageRaw(String),
    BackgroundImages(Option<Arc<[BackgroundImage]>>),
    BackgroundPositionRaw(String),
    BackgroundPosition(Option<Arc<[[LengthPercentage; 2]]>>),
    BackgroundSizeRaw(String),
    BackgroundSizes(Option<Arc<[BackgroundSize]>>),
    BackgroundRepeats(Arc<[[BackgroundRepeat; 2]]>),
    BackgroundClip(Arc<[BackgroundBox]>),
    BackgroundOrigin(Arc<[BackgroundBox]>),

    ContextLength(usize, Box<str>, bool),
    MinHeight(f32),
    MinHeightAuto,
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
    Custom(Box<str>, Box<str>),
    Deferred(Box<str>, Box<str>),
    GeneratedContent(GeneratedContent),
    Quotes(Quotes),
    CounterReset(Option<Arc<[CounterDirective]>>),
    CounterIncrement(Option<Arc<[CounterDirective]>>),
    CounterSet(Option<Arc<[CounterDirective]>>),
    ListStyleType(ListStyleType),
    ListStylePosition(ListStylePosition),
    ListStyleImage(Option<Arc<str>>),
    ColorScheme(Option<Arc<str>>),
    Default(usize, bool),
    RevertLayer(usize),
    Revert(usize),
    Display(Display),
    Opacity(f32),
    Color(Rgba),
    Background(Rgba),
    Width(f32),
    MinWidth(f32),
    MinWidthAuto,
    MaxWidth(Option<f32>),
    Height(f32),
    Margin(f32),
    Padding(f32),
    FontSize(f32),
    FontFamily(Arc<[Arc<str>]>),
    FontWeight(i16),
    FontStyle(FontStyle),
    FontStretch(f32),
    FontSizeAdjust(Option<FontSizeAdjust>),
    SvgFill(SvgPaint),
    SvgStroke(SvgPaint),
    SvgStrokeWidth(f32),
    SvgFillRule(SvgFillRule),
    SvgClipRule(SvgFillRule),
    SvgClipPath(Option<Arc<str>>),
    SvgGeometry(usize, Option<Arc<str>>),
    SvgStopColor(Option<Rgba>),
    SvgStopOpacity(f32),
    BorderRadius(f32),
    BorderRadiusRaw(Arc<str>),
    BorderRadii([BorderRadiusCorner; 4]),
    BorderRadiusCornerRaw(usize, Arc<str>),
    BorderRadiusShorthandCornerRaw(usize, Arc<str>),
    BorderRadiusCorner(usize, BorderRadiusCorner),
    BorderWidth(f32),
    BorderColor(Rgba),
    BorderSolid(bool),
    BorderStyle(BorderStyle),
    OverflowAxis(usize, Overflow),
    BackgroundAttachment(Arc<[BackgroundAttachment]>),
    LineHeight(LineHeight),
    FlexDirection(FlexDirection),
    FlexWrap(bool),
    JustifyContent(JustifyContent),
    AlignItems(AlignItems),
    Gap(f32),
    GapLength(usize, LengthPercentage),
    GapRaw(usize, Arc<str>),
    FlexGrow(f32),
    FlexShrink(f32),
    FlexBasis(Option<f32>),
    FlexBasisContent,
    FlexBasisIntrinsic(IntrinsicSizing),
    BoxSizing(BoxSizing),
    TableFixed(bool),
    BorderSpacing([f32; 2]),
    GridColumns(Arc<[GridTrack]>, Option<Arc<[GridNamedLine]>>, bool),
    GridRows(Arc<[GridTrack]>, Option<Arc<[GridNamedLine]>>, bool),
    GridColumnsAuto(GridAutoRepeat),
    GridRowsAuto(GridAutoRepeat),
    AspectRatio(f32),
    ZIndex(Option<i32>),
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
    ColumnCount(Option<usize>),
    ColumnGap(Option<f32>),
    ColumnFillAuto(bool),
    ColumnRuleWidth(f32),
    ColumnRuleColor(Option<Rgba>),
    ColumnRuleStyle(bool, Option<BorderPattern>),
    LogicalEdge(usize, f32),
    LogicalSize(usize, f32),
    LogicalConstraint(usize, Option<f32>),
    LogicalOffset(usize, Option<f32>),
    LogicalMargin(usize, Option<f32>),
    WritingMode(WritingMode),
    Containment {
        paint: bool,
        layout: bool,
        size: bool,
    },
    Visibility(bool),
    PointerEvents(bool),
    IntrinsicHeight(usize, IntrinsicSizing),
    EmptyCellsHide(bool),
    CaptionBottom(bool),
    BorderCollapse(bool),
    VerticalAlign(VerticalAlign),
    LogicalBorder(usize, LogicalBorderComponent),
}

impl Value {
    fn slot(&self) -> usize {
        match self {
            Self::Animation(slot, _) => *slot,
            Self::TransitionTimes(slot, _) => *slot,
            Self::ColorRaw(slot, _) => *slot,
            Self::GridRaw(slot, _) => *slot,
            Self::GridColumnsAuto(_) => 27,
            Self::GridRowsAuto(_) => 28,
            Self::AspectRatio(_) => 73,
            Self::ZIndex(_) => 74,
            Self::GridAutoColumns(_) => 61,
            Self::GridAutoRows(_) => 62,
            Self::GridAutoFlow(_) => 63,
            Self::JustifyItems(_) => 64,
            Self::JustifySelf(_) => 65,
            Self::GridAreas(_) => 66,
            Self::GridArea(_) => 67,
            Self::ColumnCount(_) => 75,
            Self::ColumnGap(_) => 76,
            Self::ColumnFillAuto(_) => 77,
            Self::ColumnRuleWidth(_) => 78,
            Self::ColumnRuleColor(_) => 79,
            Self::ColumnRuleStyle(..) => 80,
            Self::LogicalEdge(slot, _) | Self::LogicalSize(slot, _) => *slot,
            Self::LogicalConstraint(slot, _) => *slot,
            Self::LogicalOffset(slot, _) => *slot,
            Self::LogicalMargin(slot, _) => *slot,
            Self::WritingMode(_) => 89,
            Self::Containment { .. } => 90,
            Self::Visibility(_) => 101,
            Self::PointerEvents(_) => 177,
            Self::IntrinsicHeight(slot, _) => *slot,
            Self::EmptyCellsHide(_) => 102,
            Self::CaptionBottom(_) => 127,
            Self::BorderCollapse(_) => 128,
            Self::VerticalAlign(_) => 138,
            Self::LogicalBorder(slot, _) => *slot,
            Self::Transforms(_) | Self::TransformRaw(_) => 59,
            Self::TransformOrigin(_) | Self::TransformOriginRaw(_) => 60,
            Self::BorderPattern(_) => 11,
            Self::BorderStyle(_) => 11,
            Self::BorderCurrentColor => 10,
            Self::BackgroundCurrentColor => 2,
            Self::Shadows(_) | Self::ShadowsRaw(_) => 58,
            Self::WhiteSpace(_) => 54,
            Self::TextAlign(_) => 55,
            Self::TextIndent(_) => 181,
            Self::TextIndentRaw(_) => 181,
            Self::LetterSpacing(_) => 179,
            Self::WordSpacing(_) => 180,
            Self::Direction(_) => 56,
            Self::TextDecoration(_) => 57,
            Self::BackgroundImageNone | Self::BackgroundImageRaw(_) | Self::BackgroundImages(_) => {
                53
            }
            Self::BackgroundPositionRaw(_) | Self::BackgroundPosition(_) => 68,
            Self::BackgroundSizeRaw(_) | Self::BackgroundSizes(_) => 69,
            Self::BackgroundRepeats(_) => 70,
            Self::BackgroundClip(_) => 71,
            Self::BackgroundOrigin(_) => 72,

            Self::ContextLength(slot, _, _) => *slot,
            Self::MinHeight(_) | Self::MinHeightAuto => 51,
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
            Self::GeneratedContent(_) => 158,
            Self::Quotes(_) => 159,
            Self::CounterReset(_) => 160,
            Self::CounterIncrement(_) => 161,
            Self::CounterSet(_) => 162,
            Self::ListStyleType(_) => 172,
            Self::ListStylePosition(_) => 173,
            Self::ColorScheme(_) => 174,
            Self::ListStyleImage(_) => 178,
            Self::Display(_) => 0,
            Self::Opacity(_) => 31,
            Self::Color(_) => 1,
            Self::Background(_) => 2,
            Self::Width(_) => 3,
            Self::MinWidth(_) | Self::MinWidthAuto => 23,
            Self::MaxWidth(_) => 24,
            Self::Height(_) => 4,
            Self::Margin(_) => 5,
            Self::Padding(_) => 6,
            Self::FontSize(_) => 7,
            Self::FontFamily(_) => 129,
            Self::FontWeight(_) => 130,
            Self::FontStyle(_) => 131,
            Self::FontStretch(_) => 139,
            Self::FontSizeAdjust(_) => 140,
            Self::SvgFill(_) => 141,
            Self::SvgStroke(_) => 142,
            Self::SvgStrokeWidth(_) => 143,
            Self::SvgFillRule(_) => 144,
            Self::SvgGeometry(index, _) => 145 + *index,
            Self::SvgClipPath(_) => 154,
            Self::SvgClipRule(_) => 155,
            Self::SvgStopColor(_) => 156,
            Self::SvgStopOpacity(_) => 157,
            Self::BorderRadius(_) => 8,
            Self::BorderRadiusRaw(_) | Self::BorderRadii(_) => 8,
            Self::BorderRadiusCornerRaw(index, _)
            | Self::BorderRadiusShorthandCornerRaw(index, _)
            | Self::BorderRadiusCorner(index, _) => 134 + *index,
            Self::BorderWidth(_) => 9,
            Self::BorderColor(_) => 10,
            Self::BorderSolid(_) => 11,
            Self::OverflowAxis(slot, _) => *slot,
            Self::BackgroundAttachment(_) => 133,
            Self::LineHeight(_) => 13,
            Self::FlexDirection(_) => 14,
            Self::FlexWrap(_) => 22,
            Self::JustifyContent(_) => 15,
            Self::AlignItems(_) => 16,
            Self::Gap(_) => 17,
            Self::GapLength(slot, _) | Self::GapRaw(slot, _) => *slot,
            Self::FlexGrow(_) => 18,
            Self::FlexShrink(_) => 19,
            Self::FlexBasis(_) | Self::FlexBasisContent | Self::FlexBasisIntrinsic(_) => 20,
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
            Self::Animation(slot, value) => style.animation[slot - 163] = Some(value.clone()),
            Self::TransitionTimes(slot, values) => match slot {
                175 => style.transition_duration = Some(values.clone()),
                176 => style.transition_delay = Some(values.clone()),
                _ => {}
            },
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
                style.border_style = BorderStyle::from_pattern(*v);
                style.border_solid = true;
                style.border_pattern = Some(*v);
            }
            Self::BorderCurrentColor => style.border_color = style.color,
            Self::BackgroundCurrentColor => style.background = style.color,
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
            Self::TextIndent(v) => style.text_indent = *v,
            Self::TextIndentRaw(_) => unreachable!(),
            Self::LetterSpacing(v) => style.letter_spacing = *v,
            Self::WordSpacing(v) => style.word_spacing = *v,
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
            Self::BackgroundImageNone => {
                if style.background_images.is_some() {
                    style.background_images = None;
                }
            }
            Self::BackgroundImages(v) => style.background_images = v.clone(),
            Self::BackgroundPosition(v) => style.background_position = v.clone(),
            Self::BackgroundSizes(v) => style.background_size = v.clone(),
            Self::BackgroundImageRaw(_)
            | Self::BackgroundPositionRaw(_)
            | Self::BackgroundSizeRaw(_) => unreachable!(),
            Self::BackgroundRepeats(v) => {
                // Storing the initial value as `None` keeps the lazy-extras fast path.
                style.background_repeat = if v
                    .iter()
                    .all(|repeat| *repeat == [BackgroundRepeat::Repeat; 2])
                {
                    None
                } else {
                    Some(v.clone())
                };
            }
            Self::BackgroundClip(v) => {
                style.background_clip = if v
                    .iter()
                    .all(|box_value| *box_value == BackgroundBox::Border)
                {
                    None
                } else {
                    Some(v.clone())
                };
            }
            Self::BackgroundOrigin(v) => {
                style.background_origin = if v
                    .iter()
                    .all(|box_value| *box_value == BackgroundBox::Padding)
                {
                    None
                } else {
                    Some(v.clone())
                };
            }

            Self::ContextLength(_, _, _)
            | Self::ColorRaw(_, _)
            | Self::BorderRadiusRaw(_)
            | Self::BorderRadiusCornerRaw(_, _)
            | Self::BorderRadiusShorthandCornerRaw(_, _) => unreachable!(),
            Self::MinHeight(v) => {
                style.min_height = *v;
                style.min_height_auto = false;
            }
            Self::MinHeightAuto => {
                style.min_height = 0.0;
                style.min_height_auto = true;
                style.min_height_intrinsic = None;
            }
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
            Self::MinWidth(v) => {
                style.min_width = *v;
                style.min_width_auto = false;
            }
            Self::MinWidthAuto => {
                style.min_width = 0.0;
                style.min_width_auto = true;
            }
            Self::MaxWidth(v) => style.max_width = *v,
            Self::Height(v) => style.height = Some(*v),
            Self::Margin(v) => style.margin = *v,
            Self::Padding(v) => style.padding = *v,
            Self::FontSize(v) => style.font_size = *v,
            Self::FontFamily(v) => style.font.families = Some(v.clone()),
            Self::FontWeight(v) => style.font.weight = *v as u16,
            Self::FontStyle(v) => style.font.style = *v,
            Self::FontStretch(v) => style.font.stretch = *v,
            Self::FontSizeAdjust(v) => style.font.size_adjust = *v,
            Self::SvgFill(value) => {
                style.svg_fill = match value {
                    SvgPaint::CurrentColor => SvgPaint::Color(style.color),
                    value => value.clone(),
                }
            }
            Self::SvgStroke(value) => {
                style.svg_stroke = match value {
                    SvgPaint::CurrentColor => SvgPaint::Color(style.color),
                    value => value.clone(),
                }
            }
            Self::SvgStrokeWidth(value) => style.svg_stroke_width = *value,
            Self::SvgFillRule(value) => style.svg_fill_rule = *value,
            Self::SvgClipRule(value) => style.svg_clip_rule = *value,
            Self::SvgClipPath(value) => style.svg_clip_path = value.clone(),
            Self::SvgGeometry(index, value) => style.svg_geometry[*index] = value.clone(),
            Self::SvgStopColor(value) => style.svg_stop_color = value.unwrap_or(style.color),
            Self::SvgStopOpacity(value) => style.svg_stop_opacity = *value,
            Self::GeneratedContent(value) => match value {
                GeneratedContent::Normal => {
                    style.generated_content = None;
                    style.content_none = false;
                }
                GeneratedContent::None => {
                    style.generated_content = None;
                    style.content_none = true;
                }
                GeneratedContent::Items(items) => {
                    style.generated_content = Some(items.clone());
                    style.content_none = false;
                }
            },
            Self::Quotes(value) => style.quotes = value.clone(),
            Self::CounterReset(value) => style.counter_reset = value.clone(),
            Self::CounterIncrement(value) => style.counter_increment = value.clone(),
            Self::CounterSet(value) => style.counter_set = value.clone(),
            Self::ListStyleType(value) => style.list_style_type = value.clone(),
            Self::ListStylePosition(value) => style.list_style_position = *value,
            Self::ListStyleImage(value) => style.list_style_image = value.clone(),
            Self::ColorScheme(value) => style.color_scheme = value.clone(),
            Self::BorderRadius(v) => style.border_radius = *v,
            Self::BorderRadii(v) => apply_border_radii(style, v.clone()),
            Self::BorderRadiusCorner(index, value) => {
                apply_border_radius_corner(style, *index, value.clone());
            }
            Self::BorderWidth(v) => style.border_width = *v,
            Self::BorderColor(v) => style.border_color = *v,
            Self::BorderSolid(v) => {
                style.border_solid = *v;
                style.border_style = if *v {
                    BorderStyle::Solid
                } else {
                    BorderStyle::None
                };
                if style.border_pattern.is_some() {
                    style.border_pattern = None;
                }
            }
            Self::BorderStyle(value) => {
                style.border_style = *value;
                style.border_solid = value.paints();
                style.border_pattern = value.pattern();
            }
            Self::OverflowAxis(slot, v) => {
                if *slot == 12 {
                    style.overflow_x = *v;
                } else {
                    style.overflow_y = *v;
                }
            }
            Self::BackgroundAttachment(v) => {
                style.background_attachment =
                    if v.iter().all(|v| *v == BackgroundAttachment::Scroll) {
                        None
                    } else {
                        Some(v.clone())
                    };
            }
            Self::LineHeight(v) => style.line_height = *v,
            Self::FlexDirection(v) => style.flex_direction = *v,
            Self::FlexWrap(v) => style.flex_wrap = *v,
            Self::JustifyContent(v) => style.justify_content = *v,
            Self::AlignItems(v) => style.align_items = *v,
            Self::Gap(v) => {
                style.gap = *v;
                style.row_gap_fraction = 0.0;
                style.gap_specified = true;
            }
            Self::GapLength(slot, value) => {
                if *slot == 17 {
                    style.gap = value.pixels;
                    style.row_gap_fraction = value.fraction;
                    style.gap_specified = true;
                } else {
                    style.column_gap = Some(value.pixels);
                    style.column_gap_fraction = value.fraction;
                }
            }
            Self::GapRaw(..) => unreachable!(),
            Self::FlexGrow(v) => style.flex_grow = *v,
            Self::FlexShrink(v) => style.flex_shrink = *v,
            Self::FlexBasis(v) => {
                style.flex_basis = *v;
                style.flex_basis_content = false;
                style.flex_basis_intrinsic = None;
            }
            Self::FlexBasisContent => {
                style.flex_basis = None;
                style.flex_basis_content = true;
                style.flex_basis_intrinsic = None;
            }
            Self::FlexBasisIntrinsic(value) => {
                style.flex_basis = None;
                style.flex_basis_content = false;
                style.flex_basis_intrinsic = Some(*value);
            }
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
            Self::ColumnCount(v) => style.column_count = *v,
            Self::ColumnGap(v) => {
                style.column_gap = *v;
                style.column_gap_fraction = 0.0;
            }
            Self::ColumnFillAuto(v) => style.column_fill_auto = *v,
            Self::ColumnRuleWidth(v) => style.column_rule_width = *v,
            Self::ColumnRuleColor(v) => style.column_rule_color = *v,
            Self::ColumnRuleStyle(visible, pattern) => {
                style.column_rule_visible = *visible;
                style.column_rule_pattern = *pattern;
            }
            Self::LogicalEdge(slot, value) => match slot {
                81 | 82 => style.logical_padding_inline[slot - 81] = Some(*value),
                83 | 84 => style.logical_padding_block[slot - 83] = Some(*value),
                85 | 86 => style.logical_margin_inline[slot - 85] = Some(*value),
                _ => {}
            },
            Self::LogicalSize(slot, value) => match slot {
                87 => style.logical_inline_size = Some(*value),
                88 => style.logical_block_size = Some(*value),
                _ => {}
            },
            Self::LogicalConstraint(slot, value) => match slot {
                91 | 93 => {
                    style.logical_min_size[if *slot == 91 { 0 } else { 1 }] =
                        Some(value.unwrap_or(0.0));
                    if *slot == 91 {
                        style.min_width_auto = false;
                    } else {
                        style.min_height_auto = false;
                    }
                }
                92 | 94 => style.logical_max_size[if *slot == 92 { 0 } else { 1 }] = *value,
                _ => {}
            },
            Self::LogicalOffset(slot, value) => {
                if (95..=98).contains(slot) {
                    style.logical_offsets[slot - 95] = Some(*value);
                }
            }
            Self::LogicalMargin(slot, value) => {
                if (99..=100).contains(slot) {
                    style.logical_margin_block[slot - 99] = Some(*value);
                }
            }
            Self::WritingMode(value) => style.writing_mode = *value,
            Self::Containment {
                paint,
                layout,
                size,
            } => {
                style.contain_paint = *paint;
                style.contain_layout = *layout;
                style.contain_size = *size;
            }
            Self::Visibility(value) => style.visibility_visible = *value,
            Self::PointerEvents(value) => style.pointer_events_auto = *value,
            Self::IntrinsicHeight(slot, value) => match slot {
                4 => style.height_intrinsic = Some(*value),
                51 => {
                    style.min_height_intrinsic = Some(*value);
                    style.min_height_auto = false;
                }
                52 => style.max_height_intrinsic = Some(*value),
                _ => {}
            },
            Self::EmptyCellsHide(value) => style.empty_cells_hide = *value,
            Self::CaptionBottom(value) => style.caption_bottom = *value,
            Self::BorderCollapse(value) => style.border_collapse = *value,
            Self::VerticalAlign(value) => style.vertical_align = *value,
            Self::LogicalBorder(slot, component) => match slot {
                103..=106 => {
                    if let LogicalBorderComponent::Width(value) = component {
                        style.logical_border_width[slot - 103] = Some(*value);
                    }
                }
                115..=118 => {
                    if let LogicalBorderComponent::Width(value) = component {
                        let resolved = (*value != style.border_width).then_some(*value);
                        if style.border_width_sides[slot - 115] != resolved {
                            style.border_width_sides[slot - 115] = resolved;
                        }
                    }
                }
                107..=110 => {
                    if let LogicalBorderComponent::Color(value) = component {
                        style.logical_border_color[slot - 107] = Some(*value);
                    } else if matches!(component, LogicalBorderComponent::CurrentColor) {
                        style.logical_border_color[slot - 107] = Some(style.color);
                    }
                }
                119..=122 => {
                    if let LogicalBorderComponent::Color(value) = component {
                        let resolved = (*value != style.border_color).then_some(*value);
                        if style.border_color_sides[slot - 119] != resolved {
                            style.border_color_sides[slot - 119] = resolved;
                        }
                    } else if matches!(component, LogicalBorderComponent::CurrentColor) {
                        let resolved = (style.color != style.border_color).then_some(style.color);
                        if style.border_color_sides[slot - 119] != resolved {
                            style.border_color_sides[slot - 119] = resolved;
                        }
                    }
                }
                111..=114 => {
                    if let LogicalBorderComponent::Style(value) = component {
                        style.logical_border_style[slot - 111] = Some(*value);
                    }
                }
                123..=126 => {
                    if let LogicalBorderComponent::Style(value) = component {
                        let side = slot - 123;
                        let resolved = (*value != style.border_style).then_some(*value);
                        if style.border_style_sides[side] != resolved {
                            style.border_style_sides[side] = resolved;
                        }
                        let solid = value.paints();
                        let resolved_solid = (solid != style.border_solid).then_some(solid);
                        if style.border_solid_sides[side] != resolved_solid {
                            style.border_solid_sides[side] = resolved_solid;
                        }
                    }
                }
                _ => {}
            },
            Self::GridColumns(v, names, subgrid) => {
                style.grid_columns = Some(v.clone());
                style.grid_column_names = names.clone();
                style.grid_columns_subgrid = *subgrid;
                style.grid_columns_auto = None;
            }
            Self::GridColumnsAuto(auto) => {
                style.grid_columns = None;
                style.grid_column_names = None;
                style.grid_columns_subgrid = false;
                style.grid_columns_auto = Some(auto.clone());
            }
            Self::AspectRatio(v) => style.aspect_ratio = Some(*v),
            Self::ZIndex(v) => style.z_index = *v,
            Self::GridRowsAuto(auto) => {
                style.grid_rows = None;
                style.grid_row_names = None;
                style.grid_rows_subgrid = false;
                style.grid_rows_auto = Some(auto.clone());
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

impl Rule {
    /// Tags a parsed rule with the shadow root its `<style>` lives in.
    pub(crate) fn scoped(mut self, scope: Option<NodeId>) -> Self {
        self.scope = scope;
        self
    }
}

#[derive(Clone, Debug)]
pub struct Rule {
    selector: Selector,
    /// Shadow root whose `<style>` produced this rule; `None` = document scope.
    pub(crate) scope: Option<NodeId>,
    declarations: Arc<[Declaration]>,
    /// SVG presentation properties that the current renderer cannot honor.
    /// Keeping the names alongside their selector lets the host's capability
    /// query test only rules that apply to the actual SVG node.
    unsupported_svg_properties: Arc<[String]>,
    pub(crate) media: Arc<[Arc<str>]>,
    /// Conditions inherited from `@supports` and conditional imports.
    pub(crate) supports: Arc<[Arc<str>]>,
    /// URL of the stylesheet that declared this rule, when available.
    pub(crate) source_url: Option<Arc<str>>,
    layer: Option<usize>,
    layers: Arc<[String]>,
    layer_path: Option<[usize; 8]>,
}

impl Rule {
    /// Whether this rule may apply at all given the node's tree position.
    fn scope_allows(&self, context: Option<(&Document, NodeId)>, root: NodeId) -> bool {
        match self.scope {
            None => match context {
                None => true,
                Some((document, node)) => {
                    // Document rules do not reach into shadow trees, except
                    // through `::part()` which crosses the boundary.
                    self.selector.part.is_some() || document.root_node(node, false) == Ok(root)
                }
            },
            Some(scope) => {
                match context {
                    None => false,
                    Some((document, node)) => {
                        document.root_node(node, false) == Ok(scope)
                            || (self.selector.host && document.shadow_host(scope) == Ok(Some(node)))
                            || (self.selector.slotted
                                && document.assigned_slot(node).ok().flatten().is_some_and(
                                    |slot| document.root_node(slot, false) == Ok(scope),
                                ))
                    }
                }
            }
        }
    }
    /// Matches a rule against a node with the rule's scope semantics applied.
    pub(crate) fn matches_at(&self, document: &Document, node: NodeId) -> bool {
        self.matches_at_with_validity(document, node, &crate::forms::NoValidityOverrides)
    }

    pub(crate) fn matches_at_with_validity(
        &self,
        document: &Document,
        node: NodeId,
        validity: &dyn crate::forms::ValidityStateView,
    ) -> bool {
        match self.scope {
            None => {
                if self.selector.part.is_some() {
                    return self.selector.matches_part_with_interaction(document, node)
                        && (!self.selector.scope
                            || crate::selector::document_element(document) == Some(node));
                }
                self.selector.matches_node_in_scope_with_validity(
                    document,
                    node,
                    crate::selector::document_element(document),
                    validity,
                )
            }
            Some(root) => self
                .selector
                .matches_shadow_with_validity(document, node, root, validity),
        }
    }
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
    /// Any parsed rule declares `z-index`; gates the stacking pre-pass.
    pub(crate) has_z_index: bool,
    /// Content declarations on `::before`/`::after` may create virtual boxes.
    has_generated_content: bool,
    /// Quote functions in pseudo content need one ordered tree-context pass.
    has_quote_content: bool,
    /// Counter declarations or generated counter functions need one bounded
    /// composed-tree evaluation before visual layout.
    has_counter_data: bool,
    /// CSS list-item display or marker rules may need the implicit list-item
    /// counter even when no author counter declaration is present.
    has_list_item_data: bool,
    /// Live form-state changes only invalidate retained styles when a selector
    /// can observe validity through a direct or nested pseudo-class.
    has_validity_data: bool,
    rules: Vec<Rule>,
    tags: Vec<usize>,
    ids: Vec<usize>,
    classes: Vec<usize>,
    universal: Vec<usize>,
    /// Per rule: bloom bits its ancestor chain requires (tag/id/class of every
    /// compound reached through descendant/child combinators).
    ancestor_masks: Vec<u64>,
    uses_ancestor_bloom: bool,
    /// No rule depends on anything but the element itself, its ancestors and
    /// its attributes, so siblings with equal elements compute equal styles.
    pub(crate) siblings_share: bool,
    rules_siblings_share: bool,
    document_base_url: Option<Arc<str>>,
    /// Web Animations effect declarations are kept outside the DOM/cascade
    /// source and applied at the animation cascade origin for their target.
    animation_declarations: Vec<(NodeId, Vec<Declaration>)>,
    pub(crate) keyframes: Vec<KeyframesRuleText>,
}

pub(crate) const MAX_RULES: usize = 4096;
const MAX_DECLARATIONS: usize = 128;
const MAX_SELECTOR_BYTES: usize = 256;
const MAX_SELECTOR_NESTING: usize = 32;
const MAX_SELECTOR_COMPOUNDS: usize = 64;
const MAX_SELECTOR_MATCH_DEPTH: usize = 128;
const MAX_CSS_BYTES: usize = 1024 * 1024;
const MAX_CSS_GRAPH_SHEETS: usize = 256;
const MAX_CSS_GRAPH_IMPORTS: usize = 512;
const MAX_CSS_GRAPH_DEPTH: usize = 32;

#[derive(Default)]
struct LayerCanonicalizer {
    names: Vec<String>,
    sheets: usize,
}

fn generated_items_have_counters(items: &[GeneratedContentItem]) -> bool {
    items.iter().any(|item| match item {
        GeneratedContentItem::Counter { .. } | GeneratedContentItem::Counters { .. } => true,
        GeneratedContentItem::AlternativeText(items) => generated_items_have_counters(items),
        _ => false,
    })
}

impl LayerCanonicalizer {
    fn add_sheet(&mut self, names: &[String]) -> usize {
        self.sheets += 1;
        let sheet = self.sheets;
        for name in names {
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
                if !self.names.contains(&prefix) {
                    self.names.push(prefix.clone());
                }
            }
        }
        sheet
    }

    fn path(&self, names: &[String], layer: Option<usize>, sheet: usize) -> Option<[usize; 8]> {
        let name = names.get(layer?)?;
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
            if depth < path.len() {
                path[depth] = self.names.iter().position(|value| value == &prefix)? + 1;
            }
        }
        Some(path)
    }
}

impl StyleIndex {
    pub fn new(rules: Vec<Rule>) -> Self {
        Self::new_with_document_base_url(rules, None)
    }

    pub fn new_with_document_base_url(
        mut rules: Vec<Rule>,
        document_base_url: Option<Arc<str>>,
    ) -> Self {
        let has_z_index = rules.iter().any(|rule| {
            rule.declarations
                .iter()
                .any(|declaration| matches!(declaration.value, Value::ZIndex(_)))
        });
        let has_generated_content = rules.iter().any(|rule| {
            rule.selector.pseudo_element.is_some()
                && rule.declarations.iter().any(|declaration| {
                    matches!(&declaration.value, Value::GeneratedContent(_))
                        || matches!(
                            &declaration.value,
                            Value::Deferred(name, _) if matches!(&**name, "content" | "all")
                        )
                        || matches!(
                            &declaration.value,
                            Value::Default(158, _) | Value::Revert(158) | Value::RevertLayer(158)
                        )
                })
        });
        let has_quote_content = rules.iter().any(|rule| {
            rule.selector.pseudo_element.is_some()
                && rule
                    .declarations
                    .iter()
                    .any(|declaration| match &declaration.value {
                        Value::GeneratedContent(GeneratedContent::Items(items)) => {
                            items.iter().any(|item| {
                                matches!(
                                    item,
                                    GeneratedContentItem::OpenQuote
                                        | GeneratedContentItem::CloseQuote
                                        | GeneratedContentItem::NoOpenQuote
                                        | GeneratedContentItem::NoCloseQuote
                                )
                            })
                        }
                        Value::Deferred(name, _) => matches!(&**name, "content" | "all"),
                        Value::Default(158, _) | Value::Revert(158) | Value::RevertLayer(158) => {
                            true
                        }
                        _ => false,
                    })
        });
        let has_counter_data = rules.iter().any(|rule| {
            rule.declarations
                .iter()
                .any(|declaration| match &declaration.value {
                    Value::CounterReset(value)
                    | Value::CounterIncrement(value)
                    | Value::CounterSet(value) => value.is_some(),
                    Value::GeneratedContent(GeneratedContent::Items(items)) => {
                        generated_items_have_counters(items)
                    }
                    Value::Deferred(name, _) => matches!(&**name, "content" | "all"),
                    Value::Default(158, _) | Value::Revert(158) | Value::RevertLayer(158) => true,
                    _ => false,
                })
        });
        let has_list_item_data = rules.iter().any(|rule| {
            rule.selector.pseudo_element == Some(PseudoElement::Marker)
                || rule
                    .declarations
                    .iter()
                    .any(|declaration| match &declaration.value {
                        Value::Display(Display::ListItem)
                        | Value::ListStyleType(_)
                        | Value::ListStylePosition(_)
                        | Value::ListStyleImage(_) => true,
                        Value::Deferred(name, _) => {
                            matches!(
                                &**name,
                                "display" | "list-style" | "list-style-type" | "list-style-image"
                            )
                        }
                        _ => false,
                    })
        });
        let has_validity_data = rules
            .iter()
            .any(|rule| rule.selector.has_validity_dependency());
        let mut layer_order = LayerCanonicalizer::default();
        let mut previous: Option<Arc<[String]>> = None;
        let mut sheet = 0usize;
        for rule in &mut rules {
            if previous
                .as_ref()
                .is_none_or(|previous| !Arc::ptr_eq(previous, &rule.layers))
            {
                sheet = layer_order.add_sheet(&rule.layers);
                previous = Some(rule.layers.clone());
            }
            rule.layer_path = layer_order.path(&rule.layers, rule.layer, sheet);
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
        tags.sort_unstable_by_key(|&index| {
            rules[index]
                .selector
                .tag
                .as_deref()
                .unwrap()
                .to_ascii_lowercase()
        });
        ids.sort_unstable_by(|&a, &b| rules[a].selector.id.cmp(&rules[b].selector.id));
        classes.sort_unstable_by(|&a, &b| {
            rules[a].selector.classes[0].cmp(&rules[b].selector.classes[0])
        });
        let ancestor_masks: Vec<u64> = rules
            .iter()
            .map(|rule| rule.selector.ancestor_mask())
            .collect();
        let uses_ancestor_bloom = ancestor_masks.iter().any(|mask| *mask != 0);
        let siblings_share = rules.iter().all(|rule| {
            rule.scope.is_none()
                && !rule.selector.position_dependent()
                && !rule.selector.has_validity_dependency()
                && !rule
                    .declarations
                    .iter()
                    .any(|declaration| value_depends_on_sibling_position(&declaration.value))
        });
        Self {
            environment: MediaEnvironment::default(),
            has_z_index,
            has_generated_content,
            has_quote_content,
            has_counter_data,
            has_list_item_data,
            has_validity_data,
            rules,
            tags,
            ids,
            classes,
            universal,
            ancestor_masks,
            uses_ancestor_bloom,
            siblings_share,
            rules_siblings_share: siblings_share,
            document_base_url,
            animation_declarations: Vec::new(),
            keyframes: Vec::new(),
        }
    }

    pub fn document_base_url(&self) -> Option<&str> {
        self.document_base_url.as_deref()
    }

    pub(crate) fn has_quote_content(&self) -> bool {
        self.has_quote_content
    }

    pub(crate) fn has_counter_data(&self) -> bool {
        self.has_counter_data
    }

    pub(crate) fn has_list_item_data(&self) -> bool {
        self.has_list_item_data
    }

    pub(crate) fn has_validity_data(&self) -> bool {
        self.has_validity_data
    }

    /// Compute a generated `::before` or `::after` child for an element.
    /// Ordinary element declarations and author `style` attributes are not
    /// applied to the pseudo-element; inherited values come from `origin_style`.
    /// `None` is returned when the computed content is `normal` or `none`.
    pub fn compute_pseudo(
        &self,
        document: &Document,
        origin: NodeId,
        origin_style: &Style,
        pseudo: PseudoElement,
        text: Option<&dyn TextShaper>,
    ) -> Result<Option<GeneratedStyle>, CssError> {
        let kind = document.kind(origin).map_err(|_| CssError {
            offset: 0,
            message: "invalid pseudo-element origin",
        })?;
        if !matches!(kind, NodeKind::Element { .. }) {
            return Ok(None);
        }
        if pseudo == PseudoElement::Marker && origin_style.display != Display::ListItem {
            return Ok(None);
        }
        let html_q = matches!(
            kind,
            NodeKind::Element {
                name,
                namespace: Namespace::Html,
                ..
            } if crate::svg::local_name(name) == "q"
        );
        if !self.has_generated_content && !html_q && pseudo != PseudoElement::Marker {
            return Ok(None);
        }
        let style = compute_for(
            kind,
            Some(origin_style),
            self,
            Some((document, origin)),
            u64::MAX,
            text,
            Some(pseudo),
        )?;
        let marker_uses_default_content = pseudo == PseudoElement::Marker
            && matches!(style.generated_content(), GeneratedContent::Normal);
        let content = match style.generated_content() {
            GeneratedContent::Items(content) => GeneratedContent::Items(content),
            GeneratedContent::Normal if pseudo == PseudoElement::Marker => {
                default_marker_content(&style.list_style_type)
            }
            GeneratedContent::Normal => GeneratedContent::Normal,
            GeneratedContent::None => GeneratedContent::None,
        };
        match content {
            GeneratedContent::Items(content) => Ok(Some(GeneratedStyle {
                pseudo,
                style,
                content: GeneratedContent::Items(content),
            })),
            GeneratedContent::Normal | GeneratedContent::None => {
                if marker_uses_default_content && style.list_style_image.is_some() {
                    let fallback = default_marker_content(&style.list_style_type);
                    let fallback = match fallback {
                        GeneratedContent::Items(items) => GeneratedContent::Items(items),
                        GeneratedContent::Normal | GeneratedContent::None => {
                            GeneratedContent::Items(Arc::from([]))
                        }
                    };
                    Ok(Some(GeneratedStyle {
                        pseudo,
                        style,
                        content: fallback,
                    }))
                } else {
                    Ok(None)
                }
            }
        }
    }

    pub(crate) fn unsupported_svg_properties_for_node(
        &self,
        document: &Document,
        node: NodeId,
    ) -> Vec<String> {
        self.rules
            .iter()
            .filter(|rule| {
                rule.media
                    .iter()
                    .all(|query| media_matches(query, self.environment))
                    && rule.matches_at(document, node)
            })
            .flat_map(|rule| rule.unsupported_svg_properties.iter().cloned())
            .collect()
    }

    /// Replace the animated declarations for one node. Values are parsed by
    /// the same CSS declaration parser used for author stylesheets; invalid
    /// properties/values are ignored like invalid keyframe declarations.
    pub fn set_animation_declarations(
        &mut self,
        node: NodeId,
        pairs: &[(String, String)],
    ) -> Result<(), CssError> {
        let mut parsed = Vec::new();
        for (name, value) in pairs {
            let source = alloc::format!("{name}:{value}");
            if let Ok(mut parsed_pair) = declarations(&source, 0) {
                for declaration in &mut parsed_pair {
                    // `!important` is ignored in keyframe blocks.
                    declaration.important = false;
                }
                parsed.extend(parsed_pair);
            }
        }
        self.animation_declarations
            .retain(|(target, _)| *target != node);
        if !parsed.is_empty() {
            self.animation_declarations.push((node, parsed));
        }
        // Animation effects are node-specific. Reusing a sibling's computed
        // style would incorrectly copy the first sibling's animation sample.
        self.siblings_share = self.rules_siblings_share && self.animation_declarations.is_empty();
        Ok(())
    }

    /// Append another stylesheet's parsed rules after the existing sheets and
    /// rebuild the selector index. Used by adopted/constructable sheets so
    /// they follow the same cascade and selector implementation as DOM sheets.
    pub fn append_rules(&mut self, rules: Vec<Rule>) {
        let environment = self.environment;
        let document_base_url = self.document_base_url.clone();
        let mut combined = core::mem::take(&mut self.rules);
        combined.extend(rules);
        *self = Self::new_with_document_base_url(combined, document_base_url);
        self.environment = environment;
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

    fn matches_tag<'a>(&'a self, key: &str) -> &'a [usize] {
        let key = key.to_ascii_lowercase();
        let tag = |index: usize| {
            self.rules[index]
                .selector
                .tag
                .as_deref()
                .unwrap()
                .to_ascii_lowercase()
        };
        let start = self.tags.partition_point(|&index| tag(index) < key);
        let end = start + self.tags[start..].partition_point(|&index| tag(index) == key);
        &self.tags[start..end]
    }
}

fn default_marker_content(style: &ListStyleType) -> GeneratedContent {
    let items: Arc<[GeneratedContentItem]> = match style {
        ListStyleType::Disc => Arc::from([GeneratedContentItem::String(Arc::from("•"))]),
        ListStyleType::Circle => Arc::from([GeneratedContentItem::String(Arc::from("◦"))]),
        ListStyleType::Square => Arc::from([GeneratedContentItem::String(Arc::from("▪"))]),
        ListStyleType::Decimal
        | ListStyleType::DecimalLeadingZero
        | ListStyleType::LowerRoman
        | ListStyleType::UpperRoman
        | ListStyleType::LowerAlpha
        | ListStyleType::UpperAlpha => {
            let counter_style = match style {
                ListStyleType::Decimal => "decimal",
                ListStyleType::DecimalLeadingZero => "decimal-leading-zero",
                ListStyleType::LowerRoman => "lower-roman",
                ListStyleType::UpperRoman => "upper-roman",
                ListStyleType::LowerAlpha => "lower-alpha",
                ListStyleType::UpperAlpha => "upper-alpha",
                _ => unreachable!(),
            };
            Arc::from([
                GeneratedContentItem::Counter {
                    name: Arc::from("list-item"),
                    style: Arc::from(counter_style),
                },
                GeneratedContentItem::String(Arc::from(".")),
            ])
        }
        ListStyleType::None => return GeneratedContent::None,
        ListStyleType::String(value) => Arc::from([GeneratedContentItem::String(value.clone())]),
        ListStyleType::Unsupported(name) => {
            Arc::from([GeneratedContentItem::UnsupportedFunction {
                name: Arc::from("list-style-type"),
                arguments: name.clone(),
            }])
        }
    };
    GeneratedContent::Items(items)
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

/// Merges a `:host(...)`/`::slotted(...)` compound argument into the outer
/// selector's compound. Only a plain compound is allowed.
fn merge_compound(outer: &mut Selector, inner: Selector) -> Result<(), CssError> {
    if inner.ancestor.is_some()
        || inner.host
        || inner.slotted
        || inner.part.is_some()
        || inner.pseudo_element.is_some()
    {
        return Err(CssError {
            offset: 0,
            message: "compound argument required",
        });
    }
    if outer.tag.is_some() && inner.tag.is_some() {
        return Err(CssError {
            offset: 0,
            message: "duplicate tag in compound",
        });
    }
    if outer.tag.is_none() {
        outer.tag = inner.tag;
    }
    if outer.id.is_none() {
        outer.id = inner.id;
    }
    outer.classes.extend(inner.classes);
    outer.attributes.extend(inner.attributes);
    outer.languages.extend(inner.languages);
    outer.form_state = outer.form_state.union(inner.form_state);
    outer.interaction_state = outer.interaction_state.union(inner.interaction_state);
    outer.top_layer_state |= inner.top_layer_state;
    outer.directionality_mask =
        combine_directionality_constraints(outer.directionality_mask, inner.directionality_mask);
    outer.structural.extend(inner.structural);
    outer.logical.extend(inner.logical);
    outer.root |= inner.root;
    outer.scope |= inner.scope;
    outer.universal |= inner.universal;
    outer.nesting_selector |= inner.nesting_selector;
    outer.specificity.0 += inner.specificity.0;
    outer.specificity.1 += inner.specificity.1;
    outer.specificity.2 += inner.specificity.2;
    Ok(())
}

pub(crate) fn parse_selector_list(input: &str, offset: usize) -> Result<Vec<Selector>, CssError> {
    parse_selector_list_depth(input, offset, 0, false, false)
}

/// Validates a nested CSS selector list against its containing style-rule
/// selector context. The adapter keeps and serializes the authored selector
/// text; this helper only provides the context-sensitive syntax check.
///
/// `parent_context` contains style-rule selector lists from outermost to
/// innermost. Its first entry is an ordinary selector list; later entries may
/// use CSS nesting syntax. The input may use `&` or a leading relative
/// combinator. A caller parsing a stylesheet may ignore this error for an
/// invalid nested rule, while CSSOM `insertRule()` should propagate it.
pub fn validate_nested_selector_list(
    input: &str,
    parent_context: &[&str],
) -> Result<(), CssError> {
    if parent_context.is_empty() || parent_context.len() > 32 {
        return Err(selector_error(0, "nested selector requires a bounded parent context"));
    }
    let mut total_parent_bytes = 0usize;
    for (index, parent) in parent_context.iter().enumerate() {
        total_parent_bytes = total_parent_bytes.saturating_add(parent.len());
        if total_parent_bytes > MAX_CSS_BYTES {
            return Err(selector_error(0, "nested selector context too large"));
        }
        let parsed = if index == 0 {
            parse_selector_list_depth(parent, 0, 0, false, false)?
        } else {
            parse_nested_selector_list_depth(parent, 0, 0, false)?
        };
        if parsed.is_empty() {
            return Err(selector_error(0, "empty nested selector parent context"));
        }
    }
    let parsed = parse_nested_selector_list_depth(input, 0, 0, false)?;
    if parsed.is_empty() {
        return Err(selector_error(0, "empty nested selector list"));
    }
    Ok(())
}

fn parse_nested_selector_list_depth(
    input: &str,
    offset: usize,
    depth: usize,
    forgiving: bool,
) -> Result<Vec<Selector>, CssError> {
    if depth > MAX_SELECTOR_NESTING {
        return Err(selector_error(offset, "selector nesting limit exceeded"));
    }
    if input.len() > MAX_SELECTOR_BYTES {
        return Err(selector_error(offset, "selector too large"));
    }
    let mut result = Vec::new();
    for (start, end) in selector_list_spans(input, offset, forgiving)? {
        let item = &input[start..end];
        let mut leading = 0;
        skip_css_space_comments(item, &mut leading)
            .ok_or_else(|| selector_error(offset + start + leading, "unterminated selector comment"))?;
        let parsed = if matches!(item.as_bytes().get(leading), Some(b'>' | b'+' | b'~')) {
            parse_relative_selector_list_depth(item, offset + start, depth, true)
                .map(|relative| relative.into_iter().map(|relative| relative.selector).collect())
        } else {
            parse_selector_depth(item, offset + start, depth, true).map(|selector| vec![selector])
        };
        match parsed {
            Ok(mut selectors) => result.append(&mut selectors),
            Err(error) if forgiving && !selector_limit_error(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(result)
}

fn selector_error(offset: usize, message: &'static str) -> CssError {
    CssError { offset, message }
}

fn selector_name_start(character: char) -> bool {
    character == '_' || character.is_ascii_alphabetic() || !character.is_ascii()
}

fn selector_name_character(character: char) -> bool {
    selector_name_start(character) || character.is_ascii_digit() || character == '-'
}

fn consume_selector_identifier(input: &str, position: &mut usize) -> Option<String> {
    let mut value = String::new();
    let first = if input.as_bytes().get(*position) == Some(&b'\\') {
        selector_escape(input, position)?
    } else {
        let character = input.get(*position..)?.chars().next()?;
        if character == '-' {
            *position += 1;
            let next = if input.as_bytes().get(*position) == Some(&b'\\') {
                selector_escape(input, position)?
            } else {
                let next = input.get(*position..)?.chars().next()?;
                if next != '-' && !selector_name_start(next) {
                    return None;
                }
                *position += next.len_utf8();
                next
            };
            value.push('-');
            value.push(next);
            loop {
                let Some(byte) = input.as_bytes().get(*position) else {
                    break;
                };
                if *byte == b'\\' {
                    value.push(selector_escape(input, position)?);
                    continue;
                }
                let character = input.get(*position..)?.chars().next()?;
                if !selector_name_character(character) {
                    break;
                }
                value.push(character);
                *position += character.len_utf8();
            }
            return Some(value);
        }
        if !selector_name_start(character) {
            return None;
        }
        *position += character.len_utf8();
        character
    };
    value.push(first);
    loop {
        let Some(byte) = input.as_bytes().get(*position) else {
            break;
        };
        if *byte == b'\\' {
            value.push(selector_escape(input, position)?);
            continue;
        }
        let character = input.get(*position..)?.chars().next()?;
        if !selector_name_character(character) {
            break;
        }
        value.push(character);
        *position += character.len_utf8();
    }
    Some(value)
}

fn parse_attribute_selector(input: &str, offset: usize) -> Result<AttributeSelector, CssError> {
    let mut position = 0;
    skip_css_space_comments(input, &mut position)
        .ok_or_else(|| selector_error(offset + position, "unterminated attribute comment"))?;
    let (namespace, name) = match input.as_bytes().get(position).copied() {
        Some(b'|') => {
            position += 1;
            let name = consume_selector_identifier(input, &mut position).ok_or_else(|| {
                selector_error(offset + position, "invalid attribute selector name")
            })?;
            (AttributeNamespace::None, name)
        }
        Some(b'*') if input.as_bytes().get(position + 1) == Some(&b'|') => {
            position += 2;
            let name = consume_selector_identifier(input, &mut position).ok_or_else(|| {
                selector_error(offset + position, "invalid attribute selector name")
            })?;
            (AttributeNamespace::Any, name)
        }
        _ => {
            let name = consume_selector_identifier(input, &mut position).ok_or_else(|| {
                selector_error(offset + position, "invalid attribute selector name")
            })?;
            if input.as_bytes().get(position) == Some(&b'|')
                && input.as_bytes().get(position + 1) != Some(&b'=')
            {
                return Err(selector_error(
                    offset + position,
                    "unresolved attribute namespace prefix",
                ));
            }
            (AttributeNamespace::None, name)
        }
    };
    let html_default_ascii_insensitive = html_default_ascii_insensitive_attribute_value(&name);
    skip_css_space_comments(input, &mut position)
        .ok_or_else(|| selector_error(offset + position, "unterminated attribute comment"))?;
    if position == input.len() {
        return Ok(AttributeSelector {
            name,
            namespace,
            operator: AttributeOperator::Exists,
            value: None,
            case_sensitivity: AttributeCaseSensitivity::Default,
            html_default_ascii_insensitive,
        });
    }

    let (operator, operator_len) = [
        ("~=", AttributeOperator::Includes),
        ("|=", AttributeOperator::DashMatch),
        ("^=", AttributeOperator::Prefix),
        ("$=", AttributeOperator::Suffix),
        ("*=", AttributeOperator::Substring),
        ("=", AttributeOperator::Equals),
    ]
    .into_iter()
    .find_map(|(spelling, operator)| {
        input[position..]
            .starts_with(spelling)
            .then_some((operator, spelling.len()))
    })
    .ok_or_else(|| selector_error(offset + position, "invalid attribute selector operator"))?;
    position += operator_len;
    skip_css_space_comments(input, &mut position)
        .ok_or_else(|| selector_error(offset + position, "unterminated attribute comment"))?;

    let value = if input
        .as_bytes()
        .get(position)
        .is_some_and(|byte| matches!(byte, b'\'' | b'"'))
    {
        let end = quoted_css_end(input, position).ok_or_else(|| {
            selector_error(offset + position, "invalid attribute selector string")
        })?;
        let value = css_string(&input[position..end]).ok_or_else(|| {
            selector_error(offset + position, "invalid attribute selector string")
        })?;
        position = end;
        value
    } else {
        consume_selector_identifier(input, &mut position)
            .ok_or_else(|| selector_error(offset + position, "invalid attribute selector value"))?
    };
    skip_css_space_comments(input, &mut position)
        .ok_or_else(|| selector_error(offset + position, "unterminated attribute comment"))?;

    let case_sensitivity = if position == input.len() {
        AttributeCaseSensitivity::Default
    } else {
        let flag = consume_selector_identifier(input, &mut position)
            .ok_or_else(|| selector_error(offset + position, "invalid attribute case flag"))?;
        let case_sensitivity = if flag.eq_ignore_ascii_case("i") {
            AttributeCaseSensitivity::AsciiInsensitive
        } else if flag.eq_ignore_ascii_case("s") {
            AttributeCaseSensitivity::Sensitive
        } else {
            return Err(selector_error(
                offset + position,
                "invalid attribute case flag",
            ));
        };
        skip_css_space_comments(input, &mut position)
            .ok_or_else(|| selector_error(offset + position, "unterminated attribute comment"))?;
        case_sensitivity
    };
    if position != input.len() {
        return Err(selector_error(
            offset + position,
            "trailing attribute selector input",
        ));
    }
    Ok(AttributeSelector {
        name,
        namespace,
        operator,
        value: Some(value),
        case_sensitivity,
        html_default_ascii_insensitive,
    })
}

fn selector_limit_error(error: &CssError) -> bool {
    matches!(
        error.message,
        "selector too large"
            | "selector nesting limit exceeded"
            | "selector compound limit exceeded"
    )
}

fn selector_list_spans(
    input: &str,
    offset: usize,
    forgiving: bool,
) -> Result<Vec<(usize, usize)>, CssError> {
    let (mut start, mut pos, mut brackets, mut parentheses, mut quote) =
        (0, 0, 0usize, 0usize, 0u8);
    let bytes = input.as_bytes();
    let mut spans = Vec::new();
    while pos < bytes.len() {
        let byte = bytes[pos];
        if byte == b'\\' {
            if selector_escape(input, &mut pos).is_none() {
                if !forgiving {
                    return Err(selector_error(offset + pos, "invalid selector escape"));
                }
                pos += 1;
            }
            continue;
        }
        if quote != 0 {
            if byte == quote {
                quote = 0;
            }
        } else if matches!(byte, b'\'' | b'"') {
            quote = byte;
        } else if byte == b'/' && bytes.get(pos + 1) == Some(&b'*') {
            let Some(end) = input[pos + 2..].find("*/") else {
                return Err(selector_error(
                    offset + pos,
                    "unterminated selector comment",
                ));
            };
            pos += end + 4;
            continue;
        } else if byte == b'[' {
            brackets += 1;
        } else if byte == b']' {
            if brackets == 0 {
                if forgiving {
                    pos += 1;
                    continue;
                }
                return Err(selector_error(offset + pos, "unmatched selector bracket"));
            }
            brackets -= 1;
        } else if byte == b'(' {
            parentheses += 1;
        } else if byte == b')' {
            if parentheses == 0 {
                return Err(selector_error(
                    offset + pos,
                    "unmatched selector parenthesis",
                ));
            }
            parentheses -= 1;
        } else if byte == b',' && brackets == 0 && parentheses == 0 {
            spans.push((start, pos));
            start = pos + 1;
        }
        pos += 1;
    }
    if quote != 0 || brackets != 0 || parentheses != 0 {
        return Err(selector_error(
            offset + input.len(),
            "unterminated selector component",
        ));
    }
    spans.push((start, input.len()));
    Ok(spans)
}

fn parse_selector_list_depth(
    input: &str,
    offset: usize,
    depth: usize,
    forgiving: bool,
    allow_nesting: bool,
) -> Result<Vec<Selector>, CssError> {
    if depth > MAX_SELECTOR_NESTING {
        return Err(selector_error(offset, "selector nesting limit exceeded"));
    }
    if input.len() > MAX_SELECTOR_BYTES {
        return Err(selector_error(offset, "selector too large"));
    }
    let mut result = Vec::new();
    for (start, end) in selector_list_spans(input, offset, forgiving)? {
        let parsed = parse_selector_depth(&input[start..end], offset + start, depth, allow_nesting);
        match parsed {
            Ok(selector) => result.push(selector),
            Err(error) if forgiving && !selector_limit_error(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(result)
}

fn parse_relative_selector_list_depth(
    input: &str,
    offset: usize,
    depth: usize,
    allow_nesting: bool,
) -> Result<Vec<RelativeSelector>, CssError> {
    if depth > MAX_SELECTOR_NESTING {
        return Err(selector_error(offset, "selector nesting limit exceeded"));
    }
    if input.len() > MAX_SELECTOR_BYTES {
        return Err(selector_error(offset, "selector too large"));
    }
    let mut result = Vec::new();
    for (start, end) in selector_list_spans(input, offset, false)? {
        let relative = &input[start..end];
        let mut position = 0;
        skip_css_space_comments(relative, &mut position).ok_or_else(|| {
            selector_error(offset + start + position, "unterminated selector comment")
        })?;
        let leading = match relative.as_bytes().get(position).copied() {
            Some(b'>') => Some(Relation::Child),
            Some(b'+') => Some(Relation::Adjacent),
            Some(b'~') => Some(Relation::Following),
            _ => None,
        };
        let relation = leading.unwrap_or(Relation::Descendant);
        if leading.is_some() {
            position += 1;
            skip_css_space_comments(relative, &mut position).ok_or_else(|| {
                selector_error(offset + start + position, "unterminated selector comment")
            })?;
        }
        if position == relative.len() {
            return Err(selector_error(
                offset + start + position,
                "relative selector requires a compound selector",
            ));
        }
        let selector = parse_selector_depth(
            &relative[position..],
            offset + start + position,
            depth,
            allow_nesting,
        )?;
        result.push(RelativeSelector { relation, selector });
    }
    Ok(result)
}

pub(crate) fn parse_selector(input: &str, offset: usize) -> Result<Selector, CssError> {
    parse_selector_depth(input, offset, 0, false)
}

fn parse_selector_depth(
    input: &str,
    offset: usize,
    depth: usize,
    allow_nesting: bool,
) -> Result<Selector, CssError> {
    if depth > MAX_SELECTOR_NESTING {
        return Err(selector_error(offset, "selector nesting limit exceeded"));
    }
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
        return Err(selector_error(offset, "selector too large"));
    }
    let bytes = input.as_bytes();
    let mut pos = 0;
    let mut previous = None;
    let mut relation = Relation::Descendant;
    let mut compounds = 0usize;
    loop {
        compounds += 1;
        if compounds > MAX_SELECTOR_COMPOUNDS {
            return Err(selector_error(
                offset + pos,
                "selector compound limit exceeded",
            ));
        }
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
            } else if byte == b'/' && bytes.get(pos + 1) == Some(&b'*') {
                let Some(end) = input[pos + 2..].find("*/") else {
                    return Err(selector_error(
                        offset + pos,
                        "unterminated selector comment",
                    ));
                };
                pos += end + 4;
                continue;
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
        let mut current = parse_simple_selector_depth(
            &input[start..pos],
            offset + start,
            depth,
            allow_nesting,
        )?;
        if let Some(previous) = previous {
            let previous: Selector = previous;
            if previous.pseudo_element.is_some() {
                return Err(selector_error(
                    offset + start,
                    "pseudo-element must end the selector",
                ));
            }
            current.specificity.0 += previous.specificity.0;
            current.specificity.1 += previous.specificity.1;
            current.specificity.2 += previous.specificity.2;
            current.ancestor = Some((relation, Box::new(previous)));
        }
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if current.pseudo_element.is_some() && pos < bytes.len() {
            return Err(selector_error(
                offset + start,
                "pseudo-element must end the selector",
            ));
        }
        if pos == bytes.len() {
            let mut part_count = 0usize;
            let mut slotted_count = 0usize;
            let mut chain = Some(&current);
            while let Some(selector) = chain {
                if selector.part.is_some() {
                    part_count += 1;
                }
                if selector.slotted {
                    slotted_count += 1;
                }
                chain = selector
                    .ancestor
                    .as_ref()
                    .map(|(_, ancestor)| ancestor.as_ref());
            }
            if part_count > 1 || slotted_count > 1 {
                return Err(CssError {
                    offset,
                    message: "part and slotted pseudo-elements cannot repeat in one selector",
                });
            }
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
    parse_simple_selector_depth(input, offset, 0, false)
}

fn selector_function_end(input: &str, open: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut pos = open.checked_add(1)?;
    let mut parentheses = 1usize;
    let mut brackets = 0usize;
    let mut quote = 0u8;
    while pos < bytes.len() {
        let byte = bytes[pos];
        if byte == b'\\' {
            if selector_escape(input, &mut pos).is_none() {
                pos += 1;
            }
            continue;
        }
        if quote != 0 {
            if byte == quote {
                quote = 0;
            }
        } else if matches!(byte, b'\'' | b'"') {
            quote = byte;
        } else if byte == b'/' && bytes.get(pos + 1) == Some(&b'*') {
            let end = input[pos + 2..].find("*/")?;
            pos += end + 4;
            continue;
        } else if byte == b'[' {
            brackets += 1;
        } else if byte == b']' {
            brackets = brackets.saturating_sub(1);
        } else if brackets == 0 && byte == b'(' {
            parentheses += 1;
        } else if brackets == 0 && byte == b')' {
            parentheses -= 1;
            if parentheses == 0 {
                return Some(pos);
            }
        }
        pos += 1;
    }
    None
}

fn parse_simple_selector_depth(
    input: &str,
    offset: usize,
    depth: usize,
    allow_nesting: bool,
) -> Result<Selector, CssError> {
    if depth > MAX_SELECTOR_NESTING {
        return Err(selector_error(offset, "selector nesting limit exceeded"));
    }
    if input.len() > MAX_SELECTOR_BYTES {
        return Err(selector_error(offset, "selector too large"));
    }
    // A wildcard namespace with a wildcard local name is equivalent to the
    // universal selector for matching. The selector representation does not
    // need a namespace constraint for this form: it matches every element in
    // every namespace.
    if matches!(input, "*" | "*|*") {
        return Ok(Selector {
            languages: Vec::new(),
            form_state: crate::forms::FormStateSet::EMPTY,
            interaction_state: crate::interaction::InteractionSet::EMPTY,
            top_layer_state: 0,
            directionality_mask: 0,
            structural: Vec::new(),
            logical: Vec::new(),
            root: false,
            scope: false,
            universal: true,
            host: false,
            slotted: false,
            part: None,
            nesting_selector: false,
            pseudo_element: None,
            tag: None,
            id: None,
            classes: Vec::new(),
            specificity: (0, 0, 0),
            ancestor: None,
            attributes: Vec::new(),
        });
    }
    let wildcard_namespace_prefix = input.starts_with("*|");
    let start = if wildcard_namespace_prefix {
        match input.as_bytes().get(2).copied() {
            // A wildcard local name contributes no type selector, but may
            // still be followed by class, ID, attribute, or pseudo selectors.
            Some(b'*') => 3,
            // `*|name` is a type selector in any namespace. Require its local
            // name before parsing the rest of the compound.
            Some(b'\\' | b'-' | b'_') => 2,
            Some(byte) if byte.is_ascii_alphabetic() || !byte.is_ascii() => 2,
            _ => {
                return Err(selector_error(
                    offset,
                    "invalid wildcard namespace selector",
                ));
            }
        }
    } else {
        usize::from(input.starts_with('*'))
    };
    let mut selector = Selector {
        languages: Vec::new(),
        form_state: crate::forms::FormStateSet::EMPTY,
        interaction_state: crate::interaction::InteractionSet::EMPTY,
        top_layer_state: 0,
        directionality_mask: 0,
        structural: Vec::new(),
        logical: Vec::new(),
        root: false,
        scope: false,
        universal: input.starts_with('*'),
        host: false,
        slotted: false,
        part: None,
        nesting_selector: false,
        pseudo_element: None,
        tag: None,
        id: None,
        classes: Vec::new(),
        specificity: (0, 0, 0),
        ancestor: None,
        attributes: Vec::new(),
    };
    let mut start = start;
    let bytes = input.as_bytes();
    while start < bytes.len() {
        if selector.slotted {
            return Err(CssError {
                offset: offset + start,
                message: "::slotted must end its compound selector",
            });
        }
        if selector.pseudo_element.is_some() {
            return Err(selector_error(
                offset + start,
                "pseudo-element must end its compound selector",
            ));
        }
        let kind = bytes[start];
        if kind == b'&' {
            if !allow_nesting {
                return Err(selector_error(
                    offset + start,
                    "nesting selector is only valid inside a nested style rule",
                ));
            }
            selector.nesting_selector = true;
            start += 1;
            continue;
        }
        if kind == b':' {
            let rest = &input[start..];
            let pseudo_element = rest.starts_with("::") && !rest.starts_with(":::");
            let name_start = if pseudo_element { 2 } else { 1 };
            let (name, args, end, args_offset) = if let Some(open) =
                rest.find('(').filter(|&open| {
                    rest[..open]
                        .bytes()
                        .all(|v| v == b':' || v == b'-' || v.is_ascii_alphabetic())
                }) {
                let close = selector_function_end(rest, open)
                    .ok_or_else(|| selector_error(offset + start, "unterminated pseudo-class"))?;
                (
                    &rest[name_start..open],
                    Some(&rest[open + 1..close]),
                    close + 1,
                    open + 1,
                )
            } else {
                let end = name_start
                    + rest[name_start..]
                        .bytes()
                        .take_while(|v| v.is_ascii_alphabetic() || *v == b'-')
                        .count();
                (&rest[name_start..end], None, end, end)
            };
            let name = name.to_ascii_lowercase();
            let name = name.as_str();
            if matches!(name, "before" | "after" | "marker") {
                if args.is_some() {
                    return Err(selector_error(offset + start, "invalid pseudo-element"));
                }
                selector.pseudo_element = Some(match name {
                    "before" => PseudoElement::Before,
                    "after" => PseudoElement::After,
                    _ => PseudoElement::Marker,
                });
                selector.specificity.2 = selector.specificity.2.saturating_add(1);
                start += end;
                continue;
            }
            if name == "has" {
                if pseudo_element {
                    return Err(selector_error(
                        offset + start,
                        ":has must use pseudo-class syntax",
                    ));
                }
                let args = args.ok_or_else(|| {
                    selector_error(offset + start, ":has requires a relative selector list")
                })?;
                let mut relative = parse_relative_selector_list_depth(
                    args,
                    offset + start + args_offset,
                    depth + 1,
                    allow_nesting,
                )?;
                if relative.iter_mut().any(|item| {
                    !item.selector.drop_nested_has_from_forgiving_lists()
                        || item.selector.has_pseudo_element()
                }) {
                    return Err(selector_error(
                        offset + start,
                        "nested :has() and pseudo-elements are not valid in :has()",
                    ));
                }
                let specificity = relative
                    .iter()
                    .map(|item| item.selector.specificity)
                    .max()
                    .unwrap_or((0, 0, 0));
                selector.logical.push(LogicalPseudo::Has(relative));
                selector.specificity.0 += specificity.0;
                selector.specificity.1 += specificity.1;
                selector.specificity.2 += specificity.2;
                start += end;
                continue;
            }
            if matches!(name, "not" | "is" | "where") {
                if pseudo_element {
                    return Err(selector_error(
                        offset + start,
                        "logical pseudo-classes cannot use pseudo-element syntax",
                    ));
                }
                let args = args.ok_or_else(|| {
                    selector_error(offset + start, "logical pseudo-class requires an argument")
                })?;
                let forgiving = !name.eq("not");
                let selectors = parse_selector_list_depth(
                    args,
                    offset + start + args_offset,
                    depth + 1,
                    forgiving,
                    allow_nesting,
                )?;
                if selectors.iter().any(Selector::has_pseudo_element) {
                    if forgiving {
                        // Pseudo-elements are not valid real selectors in an
                        // :is() or :where() argument list. Drop those branches.
                        let selectors = selectors
                            .into_iter()
                            .filter(|selector| !selector.has_pseudo_element())
                            .collect::<Vec<_>>();
                        let specificity = selectors
                            .iter()
                            .map(|selector| selector.specificity)
                            .max()
                            .unwrap_or((0, 0, 0));
                        match name {
                            "is" => {
                                selector.logical.push(LogicalPseudo::Is(selectors));
                                selector.specificity.0 += specificity.0;
                                selector.specificity.1 += specificity.1;
                                selector.specificity.2 += specificity.2;
                            }
                            "where" => selector.logical.push(LogicalPseudo::Where(selectors)),
                            _ => unreachable!(),
                        }
                        start += end;
                        continue;
                    }
                    return Err(selector_error(
                        offset + start,
                        "pseudo-elements are not valid in :not()",
                    ));
                }
                let specificity = selectors
                    .iter()
                    .map(|selector| selector.specificity)
                    .max()
                    .unwrap_or((0, 0, 0));
                match name {
                    "not" => {
                        selector.logical.push(LogicalPseudo::Not(selectors));
                        selector.specificity.0 += specificity.0;
                        selector.specificity.1 += specificity.1;
                        selector.specificity.2 += specificity.2;
                    }
                    "is" => {
                        selector.logical.push(LogicalPseudo::Is(selectors));
                        selector.specificity.0 += specificity.0;
                        selector.specificity.1 += specificity.1;
                        selector.specificity.2 += specificity.2;
                    }
                    "where" => selector.logical.push(LogicalPseudo::Where(selectors)),
                    _ => unreachable!(),
                }
                start += end;
                continue;
            }
            if pseudo_element && !matches!(name, "slotted" | "part") {
                return Err(selector_error(
                    offset + start,
                    "pseudo-class must use single-colon syntax",
                ));
            }
            if name == "dir" {
                if pseudo_element {
                    return Err(selector_error(
                        offset + start,
                        ":dir must use pseudo-class syntax",
                    ));
                }
                let args =
                    args.ok_or_else(|| selector_error(offset + start, ":dir requires ltr or rtl"))?;
                let argument_offset = offset + start + args_offset;
                let required = directionality_argument(args, argument_offset)?;
                selector.directionality_mask =
                    combine_directionality_constraints(selector.directionality_mask, required);
                selector.specificity.1 = selector.specificity.1.saturating_add(1);
                start += end;
                continue;
            }
            let form_state = match name {
                "disabled" if args.is_none() => Some(crate::forms::FormStatePseudo::Disabled),
                "enabled" if args.is_none() => Some(crate::forms::FormStatePseudo::Enabled),
                "checked" if args.is_none() => Some(crate::forms::FormStatePseudo::Checked),
                "required" if args.is_none() => Some(crate::forms::FormStatePseudo::Required),
                "optional" if args.is_none() => Some(crate::forms::FormStatePseudo::Optional),
                "read-only" if args.is_none() => Some(crate::forms::FormStatePseudo::ReadOnly),
                "read-write" if args.is_none() => Some(crate::forms::FormStatePseudo::ReadWrite),
                "default" if args.is_none() => Some(crate::forms::FormStatePseudo::Default),
                "user-valid" if args.is_none() => Some(crate::forms::FormStatePseudo::UserValid),
                "user-invalid" if args.is_none() => {
                    Some(crate::forms::FormStatePseudo::UserInvalid)
                }
                "placeholder-shown" if args.is_none() => {
                    Some(crate::forms::FormStatePseudo::PlaceholderShown)
                }
                "disabled" | "enabled" | "checked" | "required" | "optional" | "read-only"
                | "read-write" | "default" | "user-valid" | "user-invalid"
                | "placeholder-shown" => {
                    return Err(selector_error(
                        offset + start,
                        "form-state pseudo takes no arguments",
                    ));
                }
                _ => None,
            };
            if let Some(pseudo) = form_state {
                selector.form_state.insert(pseudo);
                selector.specificity.1 += 1;
                start += end;
                continue;
            }
            let top_layer_state = match name {
                "open" if args.is_none() => Some(crate::top_layer::SelectorPseudo::Open),
                "modal" if args.is_none() => Some(crate::top_layer::SelectorPseudo::Modal),
                "popover-open" if args.is_none() => {
                    Some(crate::top_layer::SelectorPseudo::PopoverOpen)
                }
                "open" | "modal" | "popover-open" => {
                    return Err(selector_error(
                        offset + start,
                        "top-layer state pseudo takes no arguments",
                    ));
                }
                _ => None,
            };
            if let Some(pseudo) = top_layer_state {
                if pseudo_element {
                    return Err(selector_error(
                        offset + start,
                        "top-layer state pseudo must use single-colon syntax",
                    ));
                }
                selector.top_layer_state |= pseudo.bit();
                selector.specificity.1 += 1;
                start += end;
                continue;
            }
            let interaction_state = match name {
                "focus" if args.is_none() => Some(crate::interaction::InteractionPseudo::Focus),
                "focus-within" if args.is_none() => {
                    Some(crate::interaction::InteractionPseudo::FocusWithin)
                }
                "focus-visible" if args.is_none() => {
                    Some(crate::interaction::InteractionPseudo::FocusVisible)
                }
                "hover" if args.is_none() => Some(crate::interaction::InteractionPseudo::Hover),
                "active" if args.is_none() => Some(crate::interaction::InteractionPseudo::Active),
                "focus" | "focus-within" | "focus-visible" | "hover" | "active" => {
                    return Err(selector_error(
                        offset + start,
                        "interaction pseudo takes no arguments",
                    ));
                }
                _ => None,
            };
            if let Some(pseudo) = interaction_state {
                if pseudo_element {
                    return Err(selector_error(
                        offset + start,
                        "interaction pseudo must use single-colon syntax",
                    ));
                }
                selector.interaction_state.insert(pseudo);
                selector.specificity.1 += 1;
                start += end;
                continue;
            }
            let pseudo = match name {
                "scope" if args.is_none() => {
                    if pseudo_element {
                        return Err(selector_error(
                            offset + start,
                            ":scope must use pseudo-class syntax",
                        ));
                    }
                    selector.scope = true;
                    selector.specificity.1 += 1;
                    start += end;
                    continue;
                }
                "host" => {
                    selector.host = true;
                    selector.specificity.1 += 1;
                    if let Some(args) = args {
                        let inner = parse_simple_selector_depth(
                            args,
                            offset + start,
                            depth + 1,
                            allow_nesting,
                        )?;
                        merge_compound(&mut selector, inner)?;
                    }
                    start += end;
                    continue;
                }
                "slotted" => {
                    if !pseudo_element {
                        return Err(CssError {
                            offset: offset + start,
                            message: "slotted must use pseudo-element syntax",
                        });
                    }
                    let args = args.ok_or(CssError {
                        offset: offset + start,
                        message: "::slotted requires a compound argument",
                    })?;
                    let inner = parse_simple_selector_depth(
                        args,
                        offset + start,
                        depth + 1,
                        allow_nesting,
                    )?;
                    merge_compound(&mut selector, inner)?;
                    selector.slotted = true;
                    selector.specificity.1 += 1;
                    start += end;
                    continue;
                }
                "part" => {
                    if !pseudo_element {
                        return Err(CssError {
                            offset: offset + start,
                            message: "part must use pseudo-element syntax",
                        });
                    }
                    let args = args.ok_or(CssError {
                        offset: offset + start,
                        message: "::part requires a name",
                    })?;
                    let names: Vec<_> = args.split_ascii_whitespace().collect();
                    if names.is_empty()
                        || names.iter().any(|name| !is_part_identifier(name))
                        || selector.part.is_some()
                    {
                        return Err(CssError {
                            offset: offset + start,
                            message: "invalid part name",
                        });
                    }
                    selector.part = Some(names.join(" "));
                    selector.specificity.1 += 1;
                    start += end;
                    continue;
                }
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
                "empty" if args.is_none() => Some(StructuralPseudo::Empty),
                "valid" if args.is_none() => Some(StructuralPseudo::Valid),
                "invalid" if args.is_none() => Some(StructuralPseudo::Invalid),
                "first-child" if args.is_none() => Some(StructuralPseudo::Nth {
                    a: 0,
                    b: 1,
                    of_type: false,
                    reverse: false,
                    of: None,
                }),
                "first-of-type" if args.is_none() => Some(StructuralPseudo::Nth {
                    a: 0,
                    b: 1,
                    of_type: true,
                    reverse: false,
                    of: None,
                }),
                "last-child" if args.is_none() => Some(StructuralPseudo::Last(false)),
                "last-of-type" if args.is_none() => Some(StructuralPseudo::Last(true)),
                "only-child" if args.is_none() => Some(StructuralPseudo::Only(false)),
                "only-of-type" if args.is_none() => Some(StructuralPseudo::Only(true)),
                "nth-child" | "nth-of-type" | "nth-last-child" | "nth-last-of-type" => {
                    let args = args.ok_or_else(|| {
                        selector_error(offset + start, "nth pseudo-class requires an argument")
                    })?;
                    let argument_offset = offset + start + args_offset;
                    let ((a, b), of) = parse_nth_argument(
                        args,
                        argument_offset,
                        depth,
                        matches!(name, "nth-child" | "nth-last-child"),
                        allow_nesting,
                    )?;
                    if let Some(specificity) = of.as_ref().and_then(|selectors| {
                        selectors.iter().map(|selector| selector.specificity).max()
                    }) {
                        selector.specificity.0 += specificity.0;
                        selector.specificity.1 += specificity.1;
                        selector.specificity.2 += specificity.2;
                    }
                    Some(StructuralPseudo::Nth {
                        a,
                        b,
                        of_type: name.ends_with("of-type"),
                        reverse: name.starts_with("nth-last"),
                        of,
                    })
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
        if input
            .get(start..start + 5)
            .is_some_and(|pseudo| pseudo.eq_ignore_ascii_case(":root"))
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
            while end < bytes.len() {
                let byte = bytes[end];
                if byte == b'\\' {
                    selector_escape(input, &mut end).ok_or_else(|| {
                        selector_error(offset + end, "invalid attribute selector escape")
                    })?;
                    continue;
                }
                if matches!(byte, b'\'' | b'"') {
                    end = quoted_css_end(input, end).ok_or_else(|| {
                        selector_error(offset + end, "unterminated attribute selector string")
                    })?;
                    continue;
                }
                if byte == b'/' && bytes.get(end + 1) == Some(&b'*') {
                    let comment_end = input[end + 2..].find("*/").ok_or_else(|| {
                        selector_error(offset + end, "unterminated attribute selector comment")
                    })?;
                    end += comment_end + 4;
                    continue;
                }
                if byte == b']' {
                    break;
                }
                end += input[end..]
                    .chars()
                    .next()
                    .ok_or_else(|| selector_error(offset + end, "invalid selector character"))?
                    .len_utf8();
            }
            if end == bytes.len() {
                return Err(CssError {
                    offset: offset + start,
                    message: "unterminated attribute selector",
                });
            }
            selector.attributes.push(parse_attribute_selector(
                &input[start + 1..end],
                offset + start + 1,
            )?);
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
                selector.tag = Some(name);
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
    if selector.specificity == (0, 0, 0)
        && selector.logical.is_empty()
        && !selector.nesting_selector
    {
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

fn math_function(input: &str) -> bool {
    ["calc(", "min(", "max(", "clamp(", "sign("]
        .iter()
        .any(|name| {
            input
                .get(..name.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(name))
        })
}

fn comparison_function(input: &str) -> bool {
    [b"min(".as_slice(), b"max(".as_slice(), b"clamp(".as_slice()]
        .iter()
        .any(|name| {
            input
                .as_bytes()
                .windows(name.len())
                .any(|part| part.eq_ignore_ascii_case(name))
        })
}

fn nonnegative_length(input: &str) -> Option<f32> {
    let value = length(input)?;
    if math_function(input.trim()) {
        Some(value.max(0.0))
    } else {
        (value >= 0.0).then_some(value)
    }
}

fn border_spacing_value(input: &str) -> Option<[f32; 2]> {
    let parts = components(input)?;
    match parts.as_slice() {
        [horizontal] => {
            let spacing = nonnegative_length(horizontal)?;
            Some([spacing, spacing])
        }
        [horizontal, vertical] => Some([
            nonnegative_length(horizontal)?,
            nonnegative_length(vertical)?,
        ]),
        _ => None,
    }
}

fn contextual_length(input: &str, context: Option<LengthContext>) -> Option<f32> {
    let input = input.trim();
    let calculated = math_function(input);
    if input.len() > typed_numeric::MAX_NUMERIC_EXPRESSION_BYTES {
        return None;
    }
    let mut resolver = LengthValueBuilder {
        context,
        scalar: false,
        sign_input_depth: 0,
        context_dependent: false,
    };
    let (value, dimension) = typed_numeric::parse_numeric_expression_with(input, &mut resolver)?;
    (dimension || (!calculated && value == 0.0)).then_some(value)
}

/// Parse a CSS length for text APIs that resolve font-relative units against
/// the active font. `ex` and `ch` bases come from that font's metrics.
pub fn parse_text_length(input: &str, font: f32, root_font: f32, ex: f32, ch: f32) -> Option<f32> {
    if ![font, root_font, ex, ch].into_iter().all(f32::is_finite)
        || input.len() > typed_numeric::MAX_NUMERIC_EXPRESSION_BYTES
    {
        return None;
    }
    contextual_length(
        input,
        Some(LengthContext {
            font,
            root_font,
            ex,
            ch,
            viewport: MediaEnvironment::default(),
            percent: None,
        }),
    )
}

struct LengthValueBuilder {
    context: Option<LengthContext>,
    scalar: bool,
    sign_input_depth: usize,
    context_dependent: bool,
}

impl typed_numeric::NumericExpressionBuilder for LengthValueBuilder {
    type Expr = (f32, bool);
    type Accumulator = (f32, bool);

    fn value(&mut self, value: typed_numeric::NumericValue) -> Option<Self::Expr> {
        use typed_numeric::NumericUnit as Unit;
        let scalar = self.scalar && self.sign_input_depth == 0;
        if scalar && !matches!(value.unit, Unit::Number | Unit::Percent) {
            return None;
        }
        if !matches!(
            value.unit,
            Unit::Number
                | Unit::Percent
                | Unit::Px
                | Unit::In
                | Unit::Cm
                | Unit::Mm
                | Unit::Q
                | Unit::Pt
                | Unit::Pc
        ) {
            self.context_dependent = true;
        }
        let scale = match value.unit {
            Unit::Number | Unit::Px => 1.0,
            Unit::In => 96.0,
            Unit::Cm => 96.0 / 2.54,
            Unit::Mm => 96.0 / 25.4,
            Unit::Q => 96.0 / 101.6,
            Unit::Pt => 96.0 / 72.0,
            Unit::Pc => 16.0,
            Unit::Em => self.context?.font,
            Unit::Ex => self.context?.ex,
            Unit::Ch => self.context?.ch,
            Unit::Rem => self.context?.root_font,
            Unit::Vw => self.context?.viewport.width / 100.0,
            Unit::Vh => self.context?.viewport.height / 100.0,
            Unit::Vmin => {
                self.context?
                    .viewport
                    .width
                    .min(self.context?.viewport.height)
                    / 100.0
            }
            Unit::Vmax => {
                self.context?
                    .viewport
                    .width
                    .max(self.context?.viewport.height)
                    / 100.0
            }
            // No size-query container API is available in this renderer yet.
            // CSS uses the small viewport fallback when no query container exists.
            Unit::Cqw | Unit::Cqi => self.context?.viewport.width / 100.0,
            Unit::Cqh | Unit::Cqb => self.context?.viewport.height / 100.0,
            Unit::Cqmin => {
                self.context?
                    .viewport
                    .width
                    .min(self.context?.viewport.height)
                    / 100.0
            }
            Unit::Cqmax => {
                self.context?
                    .viewport
                    .width
                    .max(self.context?.viewport.height)
                    / 100.0
            }
            Unit::Percent => self.context?.percent? / 100.0,
            _ => return None,
        };
        let resolved = value.value as f32 * scale;
        resolved
            .is_finite()
            .then_some((resolved, value.unit != Unit::Number))
    }

    fn begin_sum(&mut self, first: Self::Expr) -> Option<Self::Accumulator> {
        Some(first)
    }

    fn push_sum(
        &mut self,
        values: &mut Self::Accumulator,
        mut next: Self::Expr,
        subtract: bool,
    ) -> Option<()> {
        if values.1 != next.1 {
            return None;
        }
        if subtract {
            next.0 = -next.0;
        }
        values.0 += next.0;
        values.0.is_finite().then_some(())
    }

    fn finish_sum(&mut self, values: Self::Accumulator, _operated: bool) -> Option<Self::Expr> {
        Some(values)
    }

    fn begin_product(&mut self, first: Self::Expr) -> Option<Self::Accumulator> {
        Some(first)
    }

    fn push_product(
        &mut self,
        values: &mut Self::Accumulator,
        next: Self::Expr,
        divide: bool,
    ) -> Option<()> {
        let next = if divide { self.invert(next)? } else { next };
        if values.1 && next.1 {
            return None;
        }
        values.0 *= next.0;
        values.1 |= next.1;
        values.0.is_finite().then_some(())
    }

    fn finish_product(&mut self, values: Self::Accumulator, _operated: bool) -> Option<Self::Expr> {
        Some(values)
    }

    fn begin_min(&mut self, first: Self::Expr) -> Option<Self::Accumulator> {
        Some(first)
    }

    fn push_min(&mut self, values: &mut Self::Accumulator, next: Self::Expr) -> Option<()> {
        if values.1 != next.1 {
            return None;
        }
        values.0 = values.0.min(next.0);
        Some(())
    }

    fn finish_min(&mut self, values: Self::Accumulator) -> Option<Self::Expr> {
        Some(values)
    }

    fn begin_max(&mut self, first: Self::Expr) -> Option<Self::Accumulator> {
        Some(first)
    }

    fn push_max(&mut self, values: &mut Self::Accumulator, next: Self::Expr) -> Option<()> {
        if values.1 != next.1 {
            return None;
        }
        values.0 = values.0.max(next.0);
        Some(())
    }

    fn finish_max(&mut self, values: Self::Accumulator) -> Option<Self::Expr> {
        Some(values)
    }

    fn calc(&mut self, value: Self::Expr) -> Option<Self::Expr> {
        Some(value)
    }

    fn clamp(
        &mut self,
        lower: Self::Expr,
        value: Self::Expr,
        upper: Self::Expr,
    ) -> Option<Self::Expr> {
        if lower.1 != value.1 || value.1 != upper.1 {
            return None;
        }
        Some((lower.0.max(value.0.min(upper.0)), lower.1))
    }

    fn negate(&mut self, mut value: Self::Expr) -> Option<Self::Expr> {
        value.0 = -value.0;
        value.0.is_finite().then_some(value)
    }

    fn invert(&mut self, value: Self::Expr) -> Option<Self::Expr> {
        if value.1 || value.0 == 0.0 {
            return None;
        }
        let inverted = 1.0 / value.0;
        inverted.is_finite().then_some((inverted, false))
    }

    fn begin_sign_input(&mut self) {
        self.sign_input_depth += 1;
    }

    fn end_sign_input(&mut self) {
        self.sign_input_depth -= 1;
    }

    fn sign(&mut self, value: Self::Expr) -> Option<Self::Expr> {
        let sign = if value.0 == 0.0 {
            // CSS sign() returns zero unchanged, including its sign bit.
            value.0
        } else if value.0.is_sign_negative() {
            -1.0
        } else {
            1.0
        };
        Some((sign, false))
    }
}

fn css_scalar(input: &str, percentages: bool) -> Option<f32> {
    css_scalar_with_context(input, percentages).map(|(value, _)| value)
}

fn css_scalar_with_context(input: &str, percentages: bool) -> Option<(f32, bool)> {
    let input = input.trim();
    if input.len() > typed_numeric::MAX_NUMERIC_EXPRESSION_BYTES
        || input.starts_with('(')
        || (!percentages && input.contains('%'))
    {
        return None;
    }
    let mut resolver = LengthValueBuilder {
        context: Some(LengthContext {
            percent: Some(1.0),
            ..static_length_context()
        }),
        scalar: true,
        sign_input_depth: 0,
        context_dependent: false,
    };
    let (value, _) = typed_numeric::parse_numeric_expression_with(input, &mut resolver)?;
    Some((value, resolver.context_dependent))
}

fn ascii_lower(value: &str) -> alloc::borrow::Cow<'_, str> {
    if value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        alloc::borrow::Cow::Owned(value.to_ascii_lowercase())
    } else {
        alloc::borrow::Cow::Borrowed(value)
    }
}

fn color(input: &str) -> Option<Rgba> {
    color_depth(input, 0)
}

fn parse_svg_paint(input: &str) -> Option<SvgPaint> {
    let input = input.trim();
    if input.eq_ignore_ascii_case("none") {
        Some(SvgPaint::None)
    } else if input.eq_ignore_ascii_case("currentcolor") {
        Some(SvgPaint::CurrentColor)
    } else if input
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("url("))
    {
        let end = input.find(')')?;
        let reference = background_url(&input[..=end])?;
        let fallback = input[end + 1..].trim();
        let fallback = if fallback.is_empty() {
            None
        } else {
            Some(color(fallback)?)
        };
        if reference.starts_with('#') && reference.len() > 1 {
            Some(SvgPaint::Reference(reference, fallback))
        } else {
            Some(SvgPaint::Unsupported(fallback))
        }
    } else {
        color(input).map(SvgPaint::Color)
    }
}

fn svg_geometry_property_index(property: &str) -> Option<usize> {
    match property {
        "x" => Some(0),
        "y" => Some(1),
        "rx" => Some(4),
        "ry" => Some(5),
        "cx" => Some(6),
        "cy" => Some(7),
        "r" => Some(8),
        _ => None,
    }
}

fn svg_geometry_value(property: &str, input: &str) -> Option<Value> {
    let index = svg_geometry_property_index(property)?;
    crate::svg::supports_length(input).then(|| Value::SvgGeometry(index, Some(Arc::from(input))))
}

fn svg_local_fragment(input: &str) -> Option<Arc<str>> {
    let reference = background_url(input.trim())?;
    (reference.starts_with('#') && reference.len() > 1).then_some(reference)
}

const MAX_CONTEXT_COLOR_BYTES: usize = 4096;

/// Replace the CSS Values sibling-context functions with the supplied
/// element-only sibling coordinates. Their arguments are empty by definition;
/// text, strings and comments are copied without interpreting their contents.
fn sibling_context_colors(input: &str, index: usize, count: usize) -> Option<(String, bool)> {
    if input.len() > MAX_CONTEXT_COLOR_BYTES {
        return None;
    }
    let bytes = input.as_bytes();
    let mut output = String::new();
    output.try_reserve(input.len()).ok()?;
    let mut pos = 0;
    let mut copied = 0;
    let mut quote = 0u8;
    let mut replaced = false;
    while pos < bytes.len() {
        let byte = bytes[pos];
        if quote != 0 {
            if byte == b'\\' {
                pos = (pos + 2).min(bytes.len());
                continue;
            }
            if byte == quote {
                quote = 0;
            }
            pos += 1;
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            quote = byte;
            pos += 1;
            continue;
        }
        if bytes.get(pos..pos + 2) == Some(b"/*") {
            pos += 2;
            while pos + 1 < bytes.len() && bytes.get(pos..pos + 2) != Some(b"*/") {
                pos += 1;
            }
            pos = (pos + 2).min(bytes.len());
            continue;
        }
        let previous_is_ident = pos > 0
            && (bytes[pos - 1].is_ascii_alphanumeric()
                || matches!(bytes[pos - 1], b'_' | b'-')
                || bytes[pos - 1] >= 0x80);
        let candidate = if !previous_is_ident {
            [
                (b"sibling-index".as_slice(), index),
                (b"sibling-count".as_slice(), count),
            ]
            .iter()
            .find_map(|(name, value)| {
                let end = pos.checked_add(name.len())?;
                bytes
                    .get(pos..end)
                    .filter(|part| part.eq_ignore_ascii_case(name))?;
                if bytes.get(end) != Some(&b'(') {
                    return None;
                }
                let mut close = end + 1;
                while bytes.get(close).is_some_and(u8::is_ascii_whitespace) {
                    close += 1;
                }
                (bytes.get(close) == Some(&b')')).then_some((close + 1, *value))
            })
        } else {
            None
        };
        if let Some((end, value)) = candidate {
            output.push_str(&input[copied..pos]);
            output.push_str(&value.to_string());
            pos = end;
            copied = end;
            replaced = true;
        } else {
            pos += input[pos..].chars().next()?.len_utf8();
        }
    }
    if !replaced {
        return Some((input.to_string(), false));
    }
    output.push_str(&input[copied..]);
    Some((output, true))
}

fn value_depends_on_sibling_position(value: &Value) -> bool {
    match value {
        Value::ColorRaw(_, raw) => {
            sibling_context_colors(raw, 1, 1).is_some_and(|(_, replaced)| replaced)
        }
        _ => false,
    }
}

fn element_sibling_position(document: &Document, node: NodeId) -> (usize, usize) {
    let Some(parent) = document.parent(node).ok().flatten() else {
        return (1, 1);
    };
    let mut current = document.first_child(parent).ok().flatten();
    let (mut index, mut count) = (0, 0);
    while let Some(sibling) = current {
        if matches!(document.kind(sibling), Ok(NodeKind::Element { .. })) {
            count += 1;
            if sibling == node {
                index = count;
            }
        }
        current = document.next_sibling(sibling).ok().flatten();
    }
    if index == 0 {
        (1, 1)
    } else {
        (index, count.max(1))
    }
}

fn color_with_context(input: &str, current: Rgba) -> Option<Rgba> {
    let lowered = input.to_ascii_lowercase();
    if !lowered.contains("currentcolor") {
        return color(input);
    }
    color(&lowered.replace(
        "currentcolor",
        &alloc::format!(
            "rgba({},{},{},{})",
            current.r,
            current.g,
            current.b,
            current.a as f32 / 255.0
        ),
    ))
}

/// Parse a CSS color using the renderer's complete color grammar, resolving
/// `currentColor` against the supplied computed foreground color.
pub fn parse_animation_color(input: &str, current: Rgba) -> Option<Rgba> {
    color_with_context(input, current)
}

fn color_value(slot: usize, input: &str) -> Option<Value> {
    let (validation_input, sibling_dependent) = sibling_context_colors(input, 1, 1)?;
    let parsed = color_with_context(&validation_input, Style::initial().color)?;
    if input.to_ascii_lowercase().contains("currentcolor") || sibling_dependent {
        return Some(Value::ColorRaw(slot, Arc::from(input)));
    }
    color_at_slot(slot, parsed)
}

fn color_at_slot(slot: usize, color: Rgba) -> Option<Value> {
    Some(match slot {
        1 => Value::Color(color),
        2 => Value::Background(color),
        10 => Value::BorderColor(color),
        79 => Value::ColumnRuleColor(Some(color)),
        107..=110 | 119..=122 => Value::LogicalBorder(slot, LogicalBorderComponent::Color(color)),
        _ => return None,
    })
}

fn color_depth(input: &str, depth: u8) -> Option<Rgba> {
    if depth >= 8 {
        return None;
    }
    let value = input.trim();
    let lowered;
    let value = if value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        lowered = value.to_ascii_lowercase();
        lowered.as_str()
    } else {
        value
    };
    if value.starts_with("color-mix(") {
        return parse_srgb_color_mix(value, depth + 1);
    }
    if let Some(args) =
        function_args(value, &["hwb"]).filter(|args| args.trim_start().starts_with("from "))
    {
        return parse_relative_color(args, 9, depth + 1);
    }
    if let Some(args) = function_args(value, &["alpha"]) {
        let origin = args.trim().strip_prefix("from ")?.trim();
        let parts = top_level_split(origin, b'/', 2)?;
        let mut source = color_depth(parts[0], depth + 1)?;
        if let Some(alpha) = parts.get(1) {
            source.a = relative_alpha(alpha, source.a)?;
        }
        return Some(source);
    }
    if let Some(args) = function_args(value, &["light-dark"]) {
        let parts = top_level_split(args, b',', 2)?;
        if parts.len() != 2 {
            return None;
        }
        let light = color_depth(parts[0], depth + 1)?;
        color_depth(parts[1], depth + 1)?;
        // This renderer's current application profile uses a light color scheme.
        return Some(light);
    }
    if let Some(args) = function_args(value, &["contrast-color"]) {
        let source = color_depth(args, depth + 1)?;
        let luma = 0.2126 * srgb_to_linear(source.r as f32 / 255.0)
            + 0.7152 * srgb_to_linear(source.g as f32 / 255.0)
            + 0.0722 * srgb_to_linear(source.b as f32 / 255.0);
        let channel = if luma > 0.179 { 0 } else { 255 };
        return Some(Rgba {
            r: channel,
            g: channel,
            b: channel,
            a: 255,
        });
    }
    if let Ok(index) =
        crate::named_colors::NAMED_COLORS.binary_search_by_key(&value, |(name, _)| *name)
    {
        let packed = crate::named_colors::NAMED_COLORS[index].1;
        return Some(Rgba {
            r: (packed >> 16) as u8,
            g: (packed >> 8) as u8,
            b: packed as u8,
            a: 255,
        });
    }
    let (r, g, b, a) = match value {
        "canvas" | "field" => (255, 255, 255, 255),
        "canvastext" | "buttontext" | "fieldtext" | "highlighttext" | "selecteditemtext" => {
            (0, 0, 0, 255)
        }
        "linktext" => (0, 0, 238, 255),
        "visitedtext" => (85, 26, 139, 255),
        "activetext" => (255, 0, 0, 255),
        "buttonface" => (239, 239, 239, 255),
        "graytext" => (128, 128, 128, 255),
        "highlight" => (180, 215, 254, 255),
        "selecteditem" => (180, 215, 255, 255),
        "accentcolor" => (0, 117, 255, 255),
        "accentcolortext" => (255, 255, 255, 255),
        "buttonborder" => (0, 0, 0, 255),
        "mark" => (255, 255, 0, 255),
        "marktext" => (0, 0, 0, 255),
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
            } else if let Some(args) = function_args(value, &["rgb", "rgba"]) {
                if args.trim_start().starts_with("from ") {
                    let parsed = parse_relative_color(args, 0, depth + 1)?;
                    (parsed.r, parsed.g, parsed.b, parsed.a)
                } else {
                    let parts = color_components(args)?;
                    if parts.len() < 3 || parts.len() > 4 {
                        return None;
                    }
                    if top_level_split(args, b',', 4)?.len() > 1
                        && (parts[..3]
                            .iter()
                            .any(|part| part.contains('%') != parts[0].contains('%'))
                            || parts.iter().any(|part| part.eq_ignore_ascii_case("none")))
                    {
                        return None;
                    }
                    let component = |part: &str| -> Option<u8> {
                        let part = part.trim();
                        if part.eq_ignore_ascii_case("none") {
                            return Some(0);
                        }
                        let value =
                            css_scalar(part, true)? * if part.contains('%') { 255.0 } else { 1.0 };
                        Some((value.clamp(0.0, 255.0) + 0.5) as u8)
                    };
                    let alpha = parts.get(3).map_or(Some(255), |part| parse_alpha(part))?;
                    (
                        component(parts[0])?,
                        component(parts[1])?,
                        component(parts[2])?,
                        alpha,
                    )
                }
            } else if let Some(args) = function_args(value, &["hsl", "hsla"]) {
                if args.trim_start().starts_with("from ") {
                    let parsed = parse_relative_color(args, 2, depth + 1)?;
                    (parsed.r, parsed.g, parsed.b, parsed.a)
                } else {
                    let parts = color_components(args)?;
                    if parts.len() < 3 || parts.len() > 4 {
                        return None;
                    }
                    let hue = parse_hue(parts[0])?;
                    let saturation = parse_percent(parts[1])?;
                    let lightness = parse_percent(parts[2])?;
                    let (r, g, b) = hsl_to_rgb(hue, saturation, lightness);
                    (
                        r,
                        g,
                        b,
                        parts.get(3).map_or(Some(255), |part| parse_alpha(part))?,
                    )
                }
            } else if let Some(args) = function_args(value, &["hwb"]) {
                let parts = color_components(args)?;
                if parts.len() < 3 || parts.len() > 4 {
                    return None;
                }
                let hue = parse_hue(parts[0])?;
                let mut whiteness = parse_percent(parts[1])?;
                let mut blackness = parse_percent(parts[2])?;
                let sum = whiteness + blackness;
                if sum > 1.0 {
                    whiteness /= sum;
                    blackness /= sum;
                }
                let (pr, pg, pb) = hsl_to_rgb(hue, 1.0, 0.5);
                let factor = 1.0 - whiteness - blackness;
                let channel =
                    |value: u8| ((value as f32 / 255.0 * factor + whiteness) * 255.0 + 0.5) as u8;
                (
                    channel(pr),
                    channel(pg),
                    channel(pb),
                    parts.get(3).map_or(Some(255), |part| parse_alpha(part))?,
                )
            } else if let Some(args) = function_args(value, &["lab", "lch", "oklab", "oklch"]) {
                let name = value.split_once('(')?.0;
                let space = match name {
                    "lab" => 3,
                    "lch" => 4,
                    "oklab" => 5,
                    _ => 6,
                };
                if args.trim_start().starts_with("from ") {
                    let parsed = parse_relative_color(args, space, depth + 1)?;
                    (parsed.r, parsed.g, parsed.b, parsed.a)
                } else {
                    let (r, g, b) = parse_lab_color(name, args)?;
                    let parts = color_components(args)?;
                    (
                        r,
                        g,
                        b,
                        parts.get(3).map_or(Some(255), |part| parse_alpha(part))?,
                    )
                }
            } else if let Some(args) = function_args(value, &["xyz"]) {
                if args.trim_start().starts_with("from ") {
                    let parsed = parse_relative_color(args, 7, depth + 1)?;
                    (parsed.r, parsed.g, parsed.b, parsed.a)
                } else {
                    let parts = color_components(args)?;
                    if parts.len() < 3 || parts.len() > 4 {
                        return None;
                    }
                    let mut coords = [0.0; 3];
                    for index in 0..3 {
                        coords[index] = parse_color_number(parts[index], 1.0)?;
                    }
                    let (r, g, b) = space_to_rgb(coords, 7);
                    (
                        r,
                        g,
                        b,
                        parts.get(3).map_or(Some(255), |part| parse_alpha(part))?,
                    )
                }
            } else if let Some(args) = function_args(value, &["color"]) {
                if args.trim_start().starts_with("from ") {
                    let parsed = parse_relative_css_color(args, depth + 1)?;
                    (parsed.r, parsed.g, parsed.b, parsed.a)
                } else {
                    let tokens = components(args)?;
                    let mut parts = tokens.into_iter();
                    let space = parts.next()?;
                    let mut channels = Vec::new();
                    let mut alpha = None;
                    let mut slash = false;
                    for part in parts {
                        if part == "/" {
                            if slash || channels.len() != 3 {
                                return None;
                            }
                            slash = true;
                        } else if let Some(value) = part.strip_prefix('/') {
                            if slash || channels.len() != 3 || value.is_empty() {
                                return None;
                            }
                            slash = true;
                            alpha = Some(parse_alpha(value)?);
                        } else if slash {
                            if alpha.is_some() {
                                return None;
                            }
                            alpha = Some(parse_alpha(part)?);
                        } else {
                            if channels.len() == 3 {
                                return None;
                            }
                            channels.push(parse_color_number(part, 1.0)?);
                        }
                    }
                    if channels.len() != 3 {
                        return None;
                    }
                    let (r, g, b) = match space {
                        "srgb" => (
                            (channels[0].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
                            (channels[1].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
                            (channels[2].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
                        ),
                        "srgb-linear" => space_to_rgb([channels[0], channels[1], channels[2]], 1),
                        "xyz" | "xyz-d65" => {
                            space_to_rgb([channels[0], channels[1], channels[2]], 7)
                        }
                        "xyz-d50" => space_to_rgb([channels[0], channels[1], channels[2]], 8),
                        "display-p3" => wide_color_to_rgb(space, channels),
                        "a98-rgb" => wide_color_to_rgb(space, channels),
                        "prophoto-rgb" => wide_color_to_rgb(space, channels),
                        "rec2020" => wide_color_to_rgb(space, channels),
                        _ => return None,
                    };
                    (r, g, b, alpha.unwrap_or(255))
                }
            } else {
                return None;
            }
        }
    };
    Some(Rgba { r, g, b, a })
}

fn color_space_id(name: &str) -> Option<u8> {
    Some(match name {
        "srgb" => 10,
        "srgb-linear" => 11,
        "display-p3" => 12,
        "a98-rgb" => 13,
        "prophoto-rgb" => 14,
        "rec2020" => 15,
        "xyz" | "xyz-d65" => 7,
        "xyz-d50" => 8,
        _ => return None,
    })
}

fn parse_relative_css_color(args: &str, depth: u8) -> Option<Rgba> {
    if depth >= 8 {
        return None;
    }
    let rest = args.trim().strip_prefix("from ")?.trim_start();
    let mut nesting = 0u32;
    let mut source_end = None;
    for (index, ch) in rest.char_indices() {
        match ch {
            '(' => nesting = nesting.checked_add(1)?,
            ')' => nesting = nesting.checked_sub(1)?,
            ch if ch.is_ascii_whitespace() && nesting == 0 => {
                source_end = Some(index);
                break;
            }
            _ => {}
        }
    }
    let source_end = source_end?;
    let source = rest[..source_end].trim();
    let channels = rest[source_end..].trim_start();
    let space_end = channels.find(|ch: char| ch.is_ascii_whitespace())?;
    let space = color_space_id(&channels[..space_end])?;
    let channel_values = channels[space_end..].trim_start();
    let mut relative = String::new();
    relative
        .try_reserve(source.len() + channel_values.len() + 6)
        .ok()?;
    relative.push_str("from ");
    relative.push_str(source);
    relative.push(' ');
    relative.push_str(channel_values);
    parse_relative_color(&relative, space, depth)
}

fn function_args<'a>(value: &'a str, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|name| {
        value
            .strip_prefix(name)?
            .strip_prefix('(')?
            .strip_suffix(')')
    })
}

fn color_components(input: &str) -> Option<Vec<&str>> {
    let comma_parts = top_level_split(input, b',', 4)?;
    if comma_parts.len() > 1 {
        return Some(comma_parts);
    }
    let channels = top_level_split(input, b'/', 2)?;
    let mut parts = components(channels[0])?;
    if let Some(alpha) = channels.get(1) {
        let alpha = alpha.trim();
        if alpha.is_empty() {
            return None;
        }
        parts.push(alpha);
    }
    Some(parts)
}

fn parse_alpha(input: &str) -> Option<u8> {
    let input = input.trim();
    if input.eq_ignore_ascii_case("none") {
        return Some(0);
    }
    Some((css_scalar(input, true)?.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
}

fn parse_percent(input: &str) -> Option<f32> {
    if input.trim().eq_ignore_ascii_case("none") {
        return Some(0.0);
    }
    Some(
        input
            .trim()
            .strip_suffix('%')?
            .parse::<f32>()
            .ok()?
            .clamp(0.0, 100.0)
            / 100.0,
    )
}

fn parse_hue(input: &str) -> Option<f32> {
    let input = input.trim();
    if input.eq_ignore_ascii_case("none") {
        return Some(0.0);
    }
    let (value, scale) = if let Some(v) = input.strip_suffix("turn") {
        (v, 360.0)
    } else if let Some(v) = input.strip_suffix("grad") {
        (v, 0.9)
    } else if let Some(v) = input.strip_suffix("rad") {
        (v, 180.0 / core::f32::consts::PI)
    } else if let Some(v) = input.strip_suffix("deg") {
        (v, 1.0)
    } else {
        (input, 1.0)
    };
    let hue = value.parse::<f32>().ok()? * scale;
    hue.is_finite().then_some(hue.rem_euclid(360.0) / 360.0)
}

fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> (u8, u8, u8) {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let sector = hue * 6.0;
    let x = chroma * (1.0 - (sector.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match sector as u8 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let m = lightness - chroma / 2.0;
    let byte = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    (byte(r), byte(g), byte(b))
}

fn parse_relative_color(args: &str, space: u8, depth: u8) -> Option<Rgba> {
    if depth >= 8 {
        return None;
    }
    let rest = args.trim().strip_prefix("from ")?.trim_start();
    let mut nesting = 0u32;
    let mut origin_end = None;
    for (index, ch) in rest.char_indices() {
        match ch {
            '(' => nesting = nesting.checked_add(1)?,
            ')' => nesting = nesting.checked_sub(1)?,
            ch if ch.is_ascii_whitespace() && nesting == 0 => {
                origin_end = Some(index);
                break;
            }
            _ => {}
        }
    }
    let origin_end = origin_end?;
    let source = color_depth(rest[..origin_end].trim(), depth)?;
    let source_values = match space {
        0 => [source.r as f32, source.g as f32, source.b as f32],
        10..=15 => color_to_css_space(source, space),
        2 => {
            let (h, s, l) = rgb_to_hsl(source);
            [h * 360.0, s * 100.0, l * 100.0]
        }
        _ => {
            let mut values = color_to_space(source, space);
            if space == 9 {
                values[0] *= 360.0;
                values[1] *= 100.0;
                values[2] *= 100.0;
            } else if matches!(space, 4 | 6) {
                values[2] *= 360.0;
            }
            values
        }
    };
    let channels = rest[origin_end..].trim();
    let mut values = Vec::new();
    let mut alpha = None;
    let mut after_slash = false;
    for token in components(channels)? {
        if token == "/" {
            if after_slash || values.len() != 3 {
                return None;
            }
            after_slash = true;
        } else if let Some(token) = token.strip_prefix('/') {
            if after_slash || values.len() != 3 || token.is_empty() {
                return None;
            }
            after_slash = true;
            alpha = Some(relative_alpha(token, source.a)?);
        } else if after_slash {
            if alpha.is_some() {
                return None;
            }
            alpha = Some(relative_alpha(token, source.a)?);
        } else {
            if values.len() == 3 {
                return None;
            }
            values.push(relative_color_channel(
                token,
                source_values,
                space,
                values.len(),
            )?);
        }
    }
    if values.len() != 3 {
        return None;
    }
    if matches!(space, 2 | 9) {
        values[0] /= 360.0;
        values[1] /= 100.0;
        values[2] /= 100.0;
    } else if matches!(space, 4 | 6) {
        values[2] /= 360.0;
    }
    let (r, g, b) = match space {
        0 => (
            (values[0].clamp(0.0, 255.0) + 0.5) as u8,
            (values[1].clamp(0.0, 255.0) + 0.5) as u8,
            (values[2].clamp(0.0, 255.0) + 0.5) as u8,
        ),
        2 => hsl_to_rgb(values[0].rem_euclid(1.0), values[1], values[2]),
        10 => (
            (values[0].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
            (values[1].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
            (values[2].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
        ),
        11 => space_to_rgb([values[0], values[1], values[2]], 1),
        12 => wide_color_to_rgb("display-p3", values.clone()),
        13 => wide_color_to_rgb("a98-rgb", values.clone()),
        14 => wide_color_to_rgb("prophoto-rgb", values.clone()),
        15 => wide_color_to_rgb("rec2020", values.clone()),
        _ => space_to_rgb([values[0], values[1], values[2]], space),
    };
    Some(Rgba {
        r,
        g,
        b,
        a: alpha.unwrap_or(source.a),
    })
}

fn relative_color_channel(value: &str, source: [f32; 3], space: u8, index: usize) -> Option<f32> {
    let value = value.trim();
    let names = match space {
        0 => ["r", "g", "b"],
        10..=15 => ["r", "g", "b"],
        2 => ["h", "s", "l"],
        3 | 5 => ["l", "a", "b"],
        4 | 6 => ["l", "c", "h"],
        9 => ["h", "w", "b"],
        _ => ["x", "y", "z"],
    };
    for index in 0..3 {
        if value.eq_ignore_ascii_case(names[index]) {
            return Some(source[index]);
        }
    }
    if value.eq_ignore_ascii_case("none") {
        return Some(0.0);
    }
    if let Some(expression) = value
        .strip_prefix("calc(")
        .and_then(|s| s.strip_suffix(')'))
    {
        for operator in ['*', '/', '+', '-'] {
            if let Some((left, right)) = expression.split_once(operator) {
                let left = relative_color_channel(left.trim(), source, space, index)?;
                let right = right.trim().parse::<f32>().ok()?;
                let value = match operator {
                    '*' => left * right,
                    '/' if right != 0.0 => left / right,
                    '+' => left + right,
                    '-' => left - right,
                    _ => return None,
                };
                return value.is_finite().then_some(value);
            }
        }
        return None;
    }
    if space == 0 {
        return parse_color_number(value, 255.0);
    }
    if (10..=15).contains(&space) {
        return parse_color_number(value, 1.0);
    }
    if (matches!(space, 2 | 9) && index == 0) || (matches!(space, 4 | 6) && index == 2) {
        return parse_hue(value).map(|hue| hue * 360.0);
    }
    let percent_scale = match space {
        2 | 9 => 100.0,
        3 if index == 0 => 100.0,
        3 => 125.0,
        4 if index == 0 => 100.0,
        4 => 150.0,
        5 | 6 if index != 0 => 0.4,
        5 | 6 => 1.0,
        _ => 1.0,
    };
    parse_color_number(value, percent_scale)
}

fn relative_alpha(value: &str, source_alpha: u8) -> Option<u8> {
    let value = value.trim();
    match value {
        "a" | "alpha" => Some(source_alpha),
        "none" => Some(0),
        _ if value.starts_with("calc(") => {
            parse_alpha(&value.replace("alpha", &alloc::format!("{}", source_alpha as f32 / 255.0)))
        }
        _ => parse_alpha(value),
    }
}

fn parse_srgb_color_mix(value: &str, depth: u8) -> Option<Rgba> {
    let args = value.strip_prefix("color-mix(")?.strip_suffix(')')?;
    let parts = top_level_split(args, b',', 3)?;
    let (prelude, left, right) = match parts.as_slice() {
        [left, right] => ("in oklab", *left, *right),
        [prelude, left, right] => (*prelude, *left, *right),
        _ => return None,
    };
    let tokens: Vec<&str> = prelude.split_ascii_whitespace().collect();
    let (space, hue_method) = match tokens.as_slice() {
        ["in", "srgb"] => (0, 0),
        ["in", "srgb-linear"] => (1, 0),
        ["in", "hsl"] => (2, 0),
        ["in", "hsl", "shorter", "hue"] => (2, 0),
        ["in", "hsl", "longer", "hue"] => (2, 1),
        ["in", "hsl", "increasing", "hue"] => (2, 2),
        ["in", "hsl", "decreasing", "hue"] => (2, 3),
        ["in", "lab"] => (3, 0),
        ["in", "lch"] => (4, 0),
        ["in", "lch", "shorter", "hue"] => (4, 0),
        ["in", "lch", "longer", "hue"] => (4, 1),
        ["in", "lch", "increasing", "hue"] => (4, 2),
        ["in", "lch", "decreasing", "hue"] => (4, 3),
        ["in", "oklab"] => (5, 0),
        ["in", "oklch"] => (6, 0),
        ["in", "oklch", "shorter", "hue"] => (6, 0),
        ["in", "oklch", "longer", "hue"] => (6, 1),
        ["in", "oklch", "increasing", "hue"] => (6, 2),
        ["in", "oklch", "decreasing", "hue"] => (6, 3),
        ["in", "xyz"] | ["in", "xyz-d65"] => (7, 0),
        ["in", "xyz-d50"] => (8, 0),
        ["in", "hwb"] => (9, 0),
        _ => return None,
    };
    let left = parse_mix_component(left.trim(), depth)?;
    let right = parse_mix_component(right.trim(), depth)?;
    let (left_color, left_weight) = left;
    let (right_color, right_weight) = right;
    let (left_weight, right_weight) = match (left_weight, right_weight) {
        (None, None) => (50.0, 50.0),
        (Some(left), None) => (left, 100.0 - left),
        (None, Some(right)) => (100.0 - right, right),
        (Some(left), Some(right)) => (left, right),
    };
    if left_weight < 0.0 || right_weight < 0.0 {
        return None;
    }
    let sum = left_weight + right_weight;
    if !sum.is_finite() || sum == 0.0 {
        return Some(Rgba {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        });
    }
    let lw = left_weight / sum;
    let rw = right_weight / sum;
    let alpha_scale = (sum / 100.0).min(1.0);
    let alpha_l = left_color.a as f32 / 255.0;
    let alpha_r = right_color.a as f32 / 255.0;
    let alpha = (alpha_l * lw + alpha_r * rw) * alpha_scale;
    if alpha == 0.0 {
        return Some(Rgba {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        });
    }
    if matches!(space, 2 | 4 | 6 | 9) {
        let (left, right, hue_index) = match space {
            2 => {
                let left = rgb_to_hsl(left_color);
                let right = rgb_to_hsl(right_color);
                (
                    [left.0, left.1, left.2],
                    [right.0, right.1, right.2],
                    0usize,
                )
            }
            _ => (
                color_to_space(left_color, space),
                color_to_space(right_color, space),
                if space == 9 { 0usize } else { 2usize },
            ),
        };
        let mut hue_a = left[hue_index];
        let hue_b = right[hue_index];
        let mut delta = (hue_b - hue_a).rem_euclid(1.0);
        match hue_method {
            0 if delta > 0.5 => delta -= 1.0,
            1 if delta < 0.5 => delta -= 1.0,
            3 if delta > 0.0 => delta -= 1.0,
            _ => {}
        }
        hue_a = (hue_a + delta * rw).rem_euclid(1.0);
        let mut mixed = [0.0; 3];
        for index in 0..3 {
            if index == hue_index {
                mixed[index] = hue_a;
            } else {
                mixed[index] = (left[index] * alpha_l * lw + right[index] * alpha_r * rw)
                    / (alpha_l * lw + alpha_r * rw);
            }
        }
        let (r, g, b) = if space == 2 {
            hsl_to_rgb(mixed[0], mixed[1], mixed[2])
        } else {
            space_to_rgb(mixed, space)
        };
        return Some(Rgba {
            r,
            g,
            b,
            a: (alpha.clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
        });
    }
    if space >= 3 {
        let left = color_to_space(left_color, space);
        let right = color_to_space(right_color, space);
        let mut mixed = [0.0; 3];
        for index in 0..3 {
            mixed[index] = (left[index] * alpha_l * lw + right[index] * alpha_r * rw)
                / (alpha_l * lw + alpha_r * rw);
        }
        let (r, g, b) = space_to_rgb(mixed, space);
        return Some(Rgba {
            r,
            g,
            b,
            a: (alpha.clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
        });
    }
    let channel = |a: u8, b: u8| {
        let a = a as f32 / 255.0;
        let b = b as f32 / 255.0;
        let (a, b) = if space == 1 {
            (srgb_to_linear(a), srgb_to_linear(b))
        } else {
            (a, b)
        };
        let mixed = (a * alpha_l * lw + b * alpha_r * rw) / (alpha_l * lw + alpha_r * rw);
        let mixed = if space == 1 {
            linear_to_srgb(mixed)
        } else {
            mixed
        };
        (mixed.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
    };
    Some(Rgba {
        r: channel(left_color.r, right_color.r),
        g: channel(left_color.g, right_color.g),
        b: channel(left_color.b, right_color.b),
        a: (alpha.clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
    })
}

fn parse_mix_component(input: &str, depth: u8) -> Option<(Rgba, Option<f32>)> {
    let input = input.trim();
    let split = input.rfind(char::is_whitespace);
    if let Some(index) = split {
        let (color_text, weight) = input.split_at(index);
        let weight = weight.trim();
        if let Some(percent) = weight.strip_suffix('%') {
            let weight = percent.parse::<f32>().ok()?;
            if !weight.is_finite() || weight < 0.0 {
                return None;
            }
            return Some((color_depth(color_text.trim(), depth)?, Some(weight)));
        }
    }
    Some((color_depth(input, depth)?, None))
}

fn srgb_to_linear(value: f32) -> f32 {
    if value.abs() <= 0.04045 {
        value / 12.92
    } else {
        value.signum() * libm::powf((value.abs() + 0.055) / 1.055, 2.4)
    }
}

fn linear_to_srgb(value: f32) -> f32 {
    if value.abs() <= 0.0031308 {
        value * 12.92
    } else {
        value.signum() * (1.055 * libm::powf(value.abs(), 1.0 / 2.4) - 0.055)
    }
}

fn rgb_to_hsl(color: Rgba) -> (f32, f32, f32) {
    let r = color.r as f32 / 255.0;
    let g = color.g as f32 / 255.0;
    let b = color.b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let lightness = (max + min) * 0.5;
    if delta == 0.0 {
        return (0.0, 0.0, lightness);
    }
    let saturation = delta / (1.0 - (2.0 * lightness - 1.0).abs());
    let hue = (if max == r {
        ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    }) / 6.0;
    (hue.rem_euclid(1.0), saturation, lightness)
}

fn color_to_space(color: Rgba, space: u8) -> [f32; 3] {
    let rgb = [
        srgb_to_linear(color.r as f32 / 255.0),
        srgb_to_linear(color.g as f32 / 255.0),
        srgb_to_linear(color.b as f32 / 255.0),
    ];
    match space {
        3 | 4 => {
            let xyz = linear_rgb_to_xyz(rgb);
            let lab = xyz_d65_to_lab(xyz);
            if space == 3 {
                lab
            } else {
                let chroma = libm::sqrtf(lab[1] * lab[1] + lab[2] * lab[2]);
                let hue = libm::atan2f(lab[2], lab[1]).rem_euclid(core::f32::consts::TAU)
                    / core::f32::consts::TAU;
                [lab[0], chroma, hue]
            }
        }
        5 | 6 => {
            let lab = linear_rgb_to_oklab(rgb);
            if space == 5 {
                lab
            } else {
                let chroma = libm::sqrtf(lab[1] * lab[1] + lab[2] * lab[2]);
                let hue = libm::atan2f(lab[2], lab[1]).rem_euclid(core::f32::consts::TAU)
                    / core::f32::consts::TAU;
                [lab[0], chroma, hue]
            }
        }
        9 => {
            let (hue, _, _) = rgb_to_hsl(color);
            let channels = [color.r, color.g, color.b];
            let whiteness = channels.iter().copied().min().unwrap_or(0) as f32 / 255.0;
            let blackness = 1.0 - channels.iter().copied().max().unwrap_or(255) as f32 / 255.0;
            [hue, whiteness, blackness]
        }
        7 => linear_rgb_to_xyz(rgb),
        8 => xyz_d65_to_d50(linear_rgb_to_xyz(rgb)),
        _ => [
            color.r as f32 / 255.0,
            color.g as f32 / 255.0,
            color.b as f32 / 255.0,
        ],
    }
}

fn space_to_rgb(mut value: [f32; 3], space: u8) -> (u8, u8, u8) {
    if space == 9 {
        let sum = value[1] + value[2];
        if sum >= 1.0 {
            let gray = ((value[1] / sum).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            return (gray, gray, gray);
        }
        let (r, g, b) = hsl_to_rgb(value[0], 1.0, 0.5);
        let factor = 1.0 - sum;
        let channel = |channel: u8| {
            ((channel as f32 / 255.0 * factor + value[1]).clamp(0.0, 1.0) * 255.0 + 0.5) as u8
        };
        return (channel(r), channel(g), channel(b));
    }
    let linear = match space {
        3 => xyz_d50_to_d65(lab_to_xyz_d50(value)),
        4 => {
            let angle = value[2] * core::f32::consts::TAU;
            value = [
                value[0],
                value[1] * libm::cosf(angle),
                value[1] * libm::sinf(angle),
            ];
            xyz_d50_to_d65(lab_to_xyz_d50(value))
        }
        5 => oklab_to_linear_rgb(value),
        6 => {
            let angle = value[2] * core::f32::consts::TAU;
            value = [
                value[0],
                value[1] * libm::cosf(angle),
                value[1] * libm::sinf(angle),
            ];
            oklab_to_linear_rgb(value)
        }
        7 => xyz_to_linear_rgb(value),
        8 => xyz_to_linear_rgb(xyz_d50_to_d65(value)),
        _ => value,
    };
    let encode = |component: f32| (linear_to_srgb(component).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    (encode(linear[0]), encode(linear[1]), encode(linear[2]))
}

fn color_to_css_space(color: Rgba, space: u8) -> [f32; 3] {
    let encoded = [
        color.r as f32 / 255.0,
        color.g as f32 / 255.0,
        color.b as f32 / 255.0,
    ];
    if space == 10 {
        return encoded;
    }
    let xyz = linear_rgb_to_xyz(encoded.map(srgb_to_linear));
    let linear = match space {
        11 => encoded.map(srgb_to_linear),
        12 => [
            2.4934969 * xyz[0] - 0.9313836 * xyz[1] - 0.4027108 * xyz[2],
            -0.8294890 * xyz[0] + 1.7626641 * xyz[1] + 0.0236247 * xyz[2],
            0.0358458 * xyz[0] - 0.0761724 * xyz[1] + 0.9568845 * xyz[2],
        ],
        13 => [
            2.0413690 * xyz[0] - 0.5649464 * xyz[1] - 0.3446944 * xyz[2],
            -0.9692660 * xyz[0] + 1.8760108 * xyz[1] + 0.0415560 * xyz[2],
            0.0134474 * xyz[0] - 0.1183897 * xyz[1] + 1.0154096 * xyz[2],
        ],
        14 => {
            let d50 = xyz_d65_to_d50(xyz);
            [
                1.3459433 * d50[0] - 0.2556075 * d50[1] - 0.0511118 * d50[2],
                -0.5445989 * d50[0] + 1.5081673 * d50[1] + 0.0205351 * d50[2],
                1.2118128 * d50[2],
            ]
        }
        15 => [
            1.7166512 * xyz[0] - 0.3556708 * xyz[1] - 0.2533663 * xyz[2],
            -0.6666844 * xyz[0] + 1.6164812 * xyz[1] + 0.0157685 * xyz[2],
            0.0176399 * xyz[0] - 0.0427706 * xyz[1] + 0.9421031 * xyz[2],
        ],
        _ => return [0.0; 3],
    };
    match space {
        11 | 12 => linear.map(linear_to_srgb),
        13 => linear.map(|v| v.signum() * libm::powf(v.abs(), 256.0 / 563.0)),
        14 => linear.map(|v| {
            if v.abs() <= 1.0 / 512.0 {
                v * 16.0
            } else {
                v.signum() * libm::powf(v.abs(), 1.0 / 1.8)
            }
        }),
        15 => linear.map(|v| {
            if v.abs() < 0.0180539685 {
                v * 4.5
            } else {
                v.signum() * (1.0992968268 * libm::powf(v.abs(), 0.45) - 0.0992968268)
            }
        }),
        _ => unreachable!(),
    }
}

fn linear_rgb_to_xyz(rgb: [f32; 3]) -> [f32; 3] {
    [
        0.4123908 * rgb[0] + 0.3575843 * rgb[1] + 0.1804808 * rgb[2],
        0.2126390 * rgb[0] + 0.7151687 * rgb[1] + 0.0721923 * rgb[2],
        0.0193308 * rgb[0] + 0.1191948 * rgb[1] + 0.9505322 * rgb[2],
    ]
}

fn xyz_to_linear_rgb(xyz: [f32; 3]) -> [f32; 3] {
    [
        3.2409699 * xyz[0] - 1.5373832 * xyz[1] - 0.4986108 * xyz[2],
        -0.9692436 * xyz[0] + 1.8759675 * xyz[1] + 0.0415551 * xyz[2],
        0.0556301 * xyz[0] - 0.2039770 * xyz[1] + 1.0569715 * xyz[2],
    ]
}

fn wide_color_to_rgb(space: &str, encoded: Vec<f32>) -> (u8, u8, u8) {
    let encoded = [encoded[0], encoded[1], encoded[2]];
    let linear = match space {
        "display-p3" => encoded.map(srgb_to_linear),
        "a98-rgb" => encoded.map(|v| v.signum() * libm::powf(v.abs(), 563.0 / 256.0)),
        "prophoto-rgb" => encoded.map(|v| {
            if v.abs() <= 1.0 / 32.0 {
                v / 16.0
            } else {
                v.signum() * libm::powf(v.abs(), 1.8)
            }
        }),
        "rec2020" => encoded.map(|v| {
            if v.abs() < 0.0812428583 {
                v / 4.5
            } else {
                v.signum() * libm::powf((v.abs() + 0.0992968268) / 1.0992968268, 1.0 / 0.45)
            }
        }),
        _ => unreachable!(),
    };
    let xyz = match space {
        "display-p3" => [
            0.48657095 * linear[0] + 0.26566769 * linear[1] + 0.19821729 * linear[2],
            0.22897456 * linear[0] + 0.69173852 * linear[1] + 0.07928691 * linear[2],
            0.0 * linear[0] + 0.04511338 * linear[1] + 1.04394437 * linear[2],
        ],
        "a98-rgb" => [
            0.5767309 * linear[0] + 0.1855540 * linear[1] + 0.1881852 * linear[2],
            0.2973769 * linear[0] + 0.6273491 * linear[1] + 0.0752741 * linear[2],
            0.0270343 * linear[0] + 0.0706872 * linear[1] + 0.9911085 * linear[2],
        ],
        "prophoto-rgb" => xyz_d50_to_d65([
            0.79776664 * linear[0] + 0.13518130 * linear[1] + 0.03134773 * linear[2],
            0.28807483 * linear[0] + 0.71183523 * linear[1] + 0.00008994 * linear[2],
            0.0 * linear[0] + 0.0 * linear[1] + 0.82510460 * linear[2],
        ]),
        "rec2020" => [
            0.63695805 * linear[0] + 0.14461690 * linear[1] + 0.16888098 * linear[2],
            0.26270021 * linear[0] + 0.67799807 * linear[1] + 0.05930172 * linear[2],
            0.0 * linear[0] + 0.02807269 * linear[1] + 1.06098506 * linear[2],
        ],
        _ => unreachable!(),
    };
    let rgb = xyz_to_linear_rgb(xyz);
    let encode = |component: f32| (linear_to_srgb(component).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    (encode(rgb[0]), encode(rgb[1]), encode(rgb[2]))
}

fn xyz_d65_to_d50(xyz: [f32; 3]) -> [f32; 3] {
    [
        1.0478112 * xyz[0] + 0.0228866 * xyz[1] - 0.0501270 * xyz[2],
        0.0295424 * xyz[0] + 0.9904844 * xyz[1] - 0.0170491 * xyz[2],
        -0.0092345 * xyz[0] - 0.0150436 * xyz[1] + 0.7521316 * xyz[2],
    ]
}

fn xyz_d50_to_d65(xyz: [f32; 3]) -> [f32; 3] {
    [
        0.9555766 * xyz[0] - 0.0230393 * xyz[1] + 0.0631636 * xyz[2],
        -0.0282895 * xyz[0] + 1.0099416 * xyz[1] + 0.0210077 * xyz[2],
        0.0122982 * xyz[0] - 0.0204830 * xyz[1] + 1.3299098 * xyz[2],
    ]
}

fn lab_f(value: f32) -> f32 {
    if value > 216.0 / 24389.0 {
        libm::cbrtf(value)
    } else {
        value * (24389.0 / (27.0 * 116.0)) + 16.0 / 116.0
    }
}

fn lab_f_inv(value: f32) -> f32 {
    if value > 6.0 / 29.0 {
        value * value * value
    } else {
        3.0 * (6.0f32 / 29.0).powi(2) * (value - 4.0 / 29.0)
    }
}

fn xyz_d65_to_lab(xyz: [f32; 3]) -> [f32; 3] {
    let xyz = xyz_d65_to_d50(xyz);
    let x = lab_f(xyz[0] / 0.96422);
    let y = lab_f(xyz[1]);
    let z = lab_f(xyz[2] / 0.82521);
    [116.0 * y - 16.0, 500.0 * (x - y), 200.0 * (y - z)]
}

fn lab_to_xyz_d50(lab: [f32; 3]) -> [f32; 3] {
    let fy = (lab[0] + 16.0) / 116.0;
    let fx = fy + lab[1] / 500.0;
    let fz = fy - lab[2] / 200.0;
    [
        0.96422 * lab_f_inv(fx),
        lab_f_inv(fy),
        0.82521 * lab_f_inv(fz),
    ]
}

fn parse_color_number(input: &str, percent_scale: f32) -> Option<f32> {
    let input = input.trim();
    if input.eq_ignore_ascii_case("none") {
        return Some(0.0);
    }
    let value = css_scalar(input, true)?;
    Some(
        value
            * if input.contains('%') {
                percent_scale
            } else {
                1.0
            },
    )
}

fn parse_lab_color(name: &str, args: &str) -> Option<(u8, u8, u8)> {
    let parts = color_components(args)?;
    if parts.len() < 3 || parts.len() > 4 {
        return None;
    }
    let oklab = name.starts_with("ok");
    let polar = name == "lch" || name == "oklch";
    let l_scale = if oklab { 1.0 } else { 100.0 };
    let first = parse_color_number(parts[0], l_scale)?;
    let second_scale = if oklab {
        0.004
    } else if polar {
        1.5
    } else {
        1.25
    };
    let second = parse_color_number(parts[1], second_scale)?;
    let third = if polar {
        parse_hue(parts[2])?
    } else {
        parse_color_number(parts[2], if oklab { 0.004 } else { 1.25 })?
    };
    let coords = [first, second, third];
    let (r, g, b) = space_to_rgb(
        coords,
        match (oklab, polar) {
            (false, false) => 3,
            (false, true) => 4,
            (true, false) => 5,
            (true, true) => 6,
        },
    );
    Some((r, g, b))
}

fn linear_rgb_to_oklab(rgb: [f32; 3]) -> [f32; 3] {
    let l = libm::cbrtf(0.4122214708 * rgb[0] + 0.5363325363 * rgb[1] + 0.0514459929 * rgb[2]);
    let m = libm::cbrtf(0.2119034982 * rgb[0] + 0.6806995451 * rgb[1] + 0.1073969566 * rgb[2]);
    let s = libm::cbrtf(0.0883024619 * rgb[0] + 0.2817188376 * rgb[1] + 0.6299787005 * rgb[2]);
    [
        0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s,
        1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s,
        0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s,
    ]
}

fn oklab_to_linear_rgb(lab: [f32; 3]) -> [f32; 3] {
    let l = (lab[0] + 0.3963377774 * lab[1] + 0.2158037573 * lab[2]).powi(3);
    let m = (lab[0] - 0.1055613458 * lab[1] - 0.0638541728 * lab[2]).powi(3);
    let s = (lab[0] - 0.0894841775 * lab[1] - 1.2914855480 * lab[2]).powi(3);
    [
        4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
        -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
        -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s,
    ]
}

fn declaration_spans(input: &str) -> Result<Vec<(usize, usize)>, CssError> {
    declaration_spans_with_recovery(input, false)
}

fn declaration_spans_with_recovery(
    input: &str,
    recover: bool,
) -> Result<Vec<(usize, usize)>, CssError> {
    if input.len() > MAX_CSS_BYTES {
        return Err(CssError {
            offset: 0,
            message: "CSS input too large",
        });
    }
    let bytes = input.as_bytes();
    let (mut start, mut pos, mut depth, mut quote) = (0, 0, 0usize, 0u8);
    let mut end_of_input = input.len();
    let mut invalid = false;
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
            if let Some(end) = input[pos + 2..].find("*/") {
                pos += end + 3;
            } else if recover {
                // Comments continue to EOF. Keep complete declarations and
                // the value preceding the comment instead of losing the rule.
                end_of_input = pos;
                break;
            } else {
                return Err(CssError {
                    offset: pos,
                    message: "unterminated comment",
                });
            }
        } else if matches!(byte, b'(' | b'[' | b'{') {
            depth += 1;
        } else if matches!(byte, b')' | b']' | b'}') {
            if depth == 0 && !recover {
                return Err(CssError {
                    offset: pos,
                    message: "unbalanced declaration",
                });
            }
            invalid |= depth == 0;
            depth = depth.saturating_sub(1);
        } else if byte == b';' && depth == 0 {
            if !invalid {
                spans.push((start, pos));
            }
            start = pos + 1;
            invalid = false;
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
        if recover {
            // An incomplete component makes this declaration invalid; prior
            // declarations still participate in the cascade.
            end_of_input = start;
        } else {
            return Err(CssError {
                offset: start,
                message: "unterminated declaration",
            });
        }
    }
    if !invalid && start < end_of_input {
        spans.push((start, end_of_input));
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
    let mut end = raw.len();
    // Comments are whitespace between the ! delimiter and identifier.
    let trim_end = |mut end: usize| loop {
        end = raw[..end].trim_end().len();
        if raw[..end].ends_with("*/") {
            if let Some(start) = raw[..end - 2].rfind("/*") {
                end = start;
                continue;
            }
        }
        return end;
    };
    end = trim_end(end);
    if end >= 9 && raw.as_bytes()[end - 9..end].eq_ignore_ascii_case(b"important") {
        let bang_end = trim_end(end - 9);
        if bang_end > 0 && raw.as_bytes()[bang_end - 1] == b'!' {
            let preceding_escapes = raw.as_bytes()[..bang_end - 1]
                .iter()
                .rev()
                .take_while(|&&byte| byte == b'\\')
                .count();
            if preceding_escapes % 2 == 0 {
                return (raw[..bang_end - 1].trim_end(), true);
            }
        }
    }
    (raw, false)
}

fn property_matches(left: &str, right: &str) -> bool {
    if left.starts_with("--") || right.starts_with("--") {
        let left = decoded_custom_property_name(left).unwrap_or_else(|| left.to_string());
        let right = decoded_custom_property_name(right).unwrap_or_else(|| right.to_string());
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

const SVG_UNSUPPORTED_CSS_PROPERTIES: &[&str] = &[
    "alignment-baseline",
    "baseline-shift",
    "dominant-baseline",
    "fill-opacity",
    "filter",
    "gradienttransform",
    "lengthadjust",
    "marker-end",
    "marker-mid",
    "marker-start",
    "mask",
    "paint-order",
    "shape-rendering",
    "stroke-dasharray",
    "stroke-dashoffset",
    "stroke-linecap",
    "stroke-linejoin",
    "stroke-miterlimit",
    "stroke-opacity",
    "text-anchor",
    "textlength",
    "text-rendering",
    "transform",
    "transform-box",
    "transform-origin",
    "vector-effect",
    "width",
    "height",
];

/// Return SVG declarations whose computed rendering behavior is not yet
/// implemented. This uses the shared declaration boundary/escape handling;
/// it is used for inline styles and retained on stylesheet rules.
pub(crate) fn unsupported_svg_style_properties(input: &str) -> Vec<String> {
    let Ok(spans) = declaration_spans(input) else {
        return Vec::new();
    };
    let mut unsupported = Vec::new();
    for (start, end) in spans {
        let Some((property, _)) = declaration_pair(&input[start..end]) else {
            continue;
        };
        let lowered = ascii_lower(property.trim());
        if SVG_UNSUPPORTED_CSS_PROPERTIES.contains(&lowered.as_ref())
            && !unsupported
                .iter()
                .any(|present| present == lowered.as_ref())
        {
            unsupported.push(lowered.into_owned());
        }
    }
    unsupported
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

/// Declared property/descriptor names in source order, using the same token
/// boundaries as declaration lookup (including strings and nested functions).
pub fn cssom_declaration_names(input: &str) -> Result<Vec<String>, CssError> {
    let mut names = Vec::new();
    for (start, end) in declaration_spans(input)? {
        if let Some((name, _)) = declaration_pair(&input[start..end]) {
            let name = name.trim();
            let name = if name.starts_with("--") {
                decoded_custom_property_name(name).unwrap_or_else(|| name.to_string())
            } else {
                name.to_ascii_lowercase()
            };
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    Ok(names)
}

/// Whether a single value parses for a property in this renderer's registry.
/// This uses the author declaration grammar, without accepting an embedded
/// priority or a second declaration from CSSOM's value argument.
pub fn supports_declaration(name: &str, value: &str) -> bool {
    if name != name.trim() || value.trim().is_empty() || important_value(value).1 {
        return false;
    }
    let Ok(raw) = set_declaration("", name, value, false) else {
        return false;
    };
    declarations(&raw, 0).is_ok_and(|values| !values.is_empty())
        && expand_variables_mode(value, &[], &mut Vec::new(), true).is_some()
}

/// Whether a property/value pair is supported by the renderer. Custom
/// properties accept arbitrary component values, including an empty value;
/// ordinary properties still use their registered value grammar.
pub fn supports_property_value(name: &str, value: &str) -> bool {
    if name.starts_with("--") {
        if !valid_custom_property_name(name) || important_value(value).1 {
            return false;
        }
        if value.trim().is_empty() {
            let declaration = alloc::format!("{name}:");
            return declarations(&declaration, 0).is_ok_and(|values| !values.is_empty());
        }
    }
    supports_declaration(name, value)
}

/// Parse and evaluate CSS Conditional Rules' supports-condition grammar.
/// This function accepts the condition syntax used by `@supports`; the
/// `CSS.supports(conditionText)` wrapper below additionally permits the
/// single-declaration shorthand.
fn parse_supports_condition(input: &str, nesting: usize) -> Option<bool> {
    const MAX_NESTING: usize = 32;
    const MAX_TERMS: usize = 256;
    if nesting >= MAX_NESTING || input.len() > MAX_CSS_BYTES {
        return None;
    }
    let mut position = 0;
    skip_css_space_comments(input, &mut position)?;
    if position == input.len() {
        return None;
    }

    if let Some(keyword_end) = condition_keyword(input, position, "not") {
        let mut operand = keyword_end;
        skip_css_space_comments(input, &mut operand)?;
        if operand == keyword_end {
            return None;
        }
        let (value, end) = parse_supports_in_parens(input, operand, nesting + 1)?;
        let mut trailing = end;
        skip_css_space_comments(input, &mut trailing)?;
        return (trailing == input.len()).then_some(!value);
    }

    let (first, end) = parse_supports_in_parens(input, position, nesting + 1)?;
    let mut result = first;
    let mut position = end;
    skip_css_space_comments(input, &mut position)?;
    if position == input.len() {
        return Some(result);
    }

    let (operator, mut operand) = if let Some(end) = condition_keyword(input, position, "and") {
        (false, end)
    } else if let Some(end) = condition_keyword(input, position, "or") {
        (true, end)
    } else {
        return None;
    };
    skip_css_space_comments(input, &mut operand)?;
    if operand == position {
        return None;
    }

    let mut terms = 1usize;
    loop {
        let (value, end) = parse_supports_in_parens(input, operand, nesting + 1)?;
        terms += 1;
        if terms > MAX_TERMS {
            return None;
        }
        result = if operator {
            result || value
        } else {
            result && value
        };
        position = end;
        skip_css_space_comments(input, &mut position)?;
        if position == input.len() {
            return Some(result);
        }
        let (next_operator, end) = if let Some(end) = condition_keyword(input, position, "and") {
            (false, end)
        } else if let Some(end) = condition_keyword(input, position, "or") {
            (true, end)
        } else {
            return None;
        };
        if next_operator != operator {
            return None;
        }
        operand = end;
        skip_css_space_comments(input, &mut operand)?;
        if operand == end {
            return None;
        }
    }
}

/// Evaluate the one-argument `CSS.supports(conditionText)` overload. The
/// CSSOM algorithm first tries the supports-condition grammar as written, then
/// retries with parentheses around the complete input for declaration
/// shorthand such as `color: red`.
pub fn supports_condition(input: &str) -> bool {
    if input.len() > MAX_CSS_BYTES {
        return false;
    }
    if parse_supports_condition(input, 0) == Some(true) {
        return true;
    }
    let mut wrapped = String::with_capacity(input.len().saturating_add(2));
    wrapped.push('(');
    wrapped.push_str(input);
    wrapped.push(')');
    parse_supports_condition(&wrapped, 0) == Some(true)
}

fn supports_condition_declaration(input: &str) -> Option<bool> {
    let spans = declaration_spans(input).ok()?;
    if spans.len() != 1 || spans[0].1 != input.len() {
        return None;
    }
    let (start, end) = spans[0];
    let part = input[start..end].trim();
    let (name, value) = declaration_pair(part)?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    Some(supports_property_value(name, value.trim()))
}

fn parse_supports_in_parens(input: &str, start: usize, nesting: usize) -> Option<(bool, usize)> {
    if nesting >= 32 || start >= input.len() {
        return None;
    }
    if input.as_bytes()[start] == b'(' {
        let end = matching_css_block(input, start)?;
        let inner = &input[start + 1..end - 1];
        if let Some(value) = parse_supports_condition(inner, nesting + 1) {
            return Some((value, end));
        }
        if let Some(value) = supports_condition_declaration(inner) {
            return Some((value, end));
        }
        // A balanced but unknown parenthesized component value is a
        // <general-enclosed> and evaluates false, preserving forward
        // compatibility without accepting malformed delimiters.
        return Some((false, end));
    }

    let (name, open) = condition_function(input, start)?;
    let end = matching_css_block(input, open)?;
    let argument = &input[open + 1..end - 1];
    let value = if name.eq_ignore_ascii_case("selector") {
        // Conditional Rules Level 4: only selectors accepted by the same
        // parser used for stylesheet rules count as supported. That parser
        // accepts a single complex selector and rejects unsupported syntax.
        parse_selector(argument, open + 1).is_ok()
    } else {
        false
    };
    Some((value, end))
}

fn valid_custom_property_name(name: &str) -> bool {
    if !name.starts_with("--") || name.len() <= 2 || name.trim() != name {
        return false;
    }
    let mut position = 2;
    while position < name.len() {
        if name.as_bytes()[position] == b'\\' {
            if selector_escape(name, &mut position).is_none() {
                return false;
            }
            continue;
        }
        let Some(character) = name[position..].chars().next() else {
            return false;
        };
        if !(character.is_ascii_alphanumeric()
            || character == '_'
            || character == '-'
            || !character.is_ascii())
        {
            return false;
        }
        position += character.len_utf8();
    }
    true
}

/// Decode one CSS custom-property identifier to its canonical name. The input
/// remains CSS source text here, so escapes are validated before resolving the
/// identifier's CSS escape sequences.
fn decoded_custom_property_name(raw: &str) -> Option<String> {
    if !valid_custom_property_name(raw) {
        return None;
    }
    let mut position = 0;
    let name = consume_selector_identifier(raw, &mut position)?;
    (position == raw.len() && name.starts_with("--") && name.len() > 2).then_some(name)
}

/// Parse the CSS Color Adjustment `color-scheme` grammar and return its
/// normalized computed representation. `None` means invalid input; the inner
/// `None` represents the initial `normal` value.
pub fn parse_color_scheme(raw: &str) -> Option<Option<Arc<str>>> {
    const MAX_SCHEMES: usize = 64;
    if raw.len() > MAX_GENERATED_CONTENT_BYTES {
        return None;
    }

    let mut position = 0;
    skip_css_space_comments(raw, &mut position)?;
    if position == raw.len() {
        return None;
    }

    let mut schemes = Vec::new();
    let mut only = false;
    let mut only_precedes_schemes = false;
    loop {
        let identifier = consume_selector_identifier(raw, &mut position)?;
        if identifier.eq_ignore_ascii_case("normal") {
            skip_css_space_comments(raw, &mut position)?;
            return (schemes.is_empty() && !only && position == raw.len()).then_some(None);
        }
        if identifier.eq_ignore_ascii_case("only") {
            if only {
                return None;
            }
            only = true;
            only_precedes_schemes = schemes.is_empty();
        } else {
            // The `&&` grammar permits `only` on either side of the complete
            // scheme list, but not between two members of that list.
            if only && !only_precedes_schemes {
                return None;
            }
            let lower = ascii_lower(&identifier);
            if matches!(
                &*lower,
                "initial" | "inherit" | "unset" | "revert" | "revert-layer" | "default"
            ) {
                return None;
            }
            if schemes.len() >= MAX_SCHEMES {
                return None;
            }
            let serialized = if matches!(&*lower, "light" | "dark") {
                lower.into_owned()
            } else {
                serialize_css_identifier(&identifier)
            };
            schemes.try_reserve(1).ok()?;
            schemes.push(serialized);
        }

        skip_css_space_comments(raw, &mut position)?;
        if position == raw.len() {
            break;
        }
    }
    if schemes.is_empty() {
        return None;
    }

    let mut serialized = String::new();
    for token in schemes {
        if !serialized.is_empty() {
            append_counter_serialized(&mut serialized, " ")?;
        }
        append_counter_serialized(&mut serialized, &token)?;
    }
    if only {
        append_counter_serialized(&mut serialized, " only")?;
    }
    Some(Some(Arc::from(serialized)))
}

/// Canonically serialize a valid `color-scheme` declaration.
pub fn serialize_color_scheme_declaration(raw: &str) -> Option<String> {
    parse_color_scheme(raw)
        .map(|value| value.map_or_else(|| String::from("normal"), |value| value.to_string()))
}

/// Parse one `var()` function body. The caller supplies the byte index of its
/// opening parenthesis; the returned end is exclusive and includes `)`.
fn variable_reference<'a>(input: &'a str, open: usize) -> Option<(usize, String, Option<&'a str>)> {
    let end = matching_css_block(input, open)?;
    let body = input.get(open + 1..end.checked_sub(1)?)?;
    let mut position = 0;
    skip_css_space_comments(body, &mut position)?;
    let name = consume_selector_identifier(body, &mut position)?;
    if !name.starts_with("--") || name.len() <= 2 {
        return None;
    }
    // CSS custom-property identifiers are case-sensitive, and escapes are
    // decoded by the same tokenizer used for selector identifiers.
    skip_css_space_comments(body, &mut position)?;
    let fallback = match body.as_bytes().get(position) {
        Some(b',') => Some(&body[position + 1..]),
        None => None,
        _ => return None,
    };
    Some((end, name, fallback))
}

/// Whether `name` is a valid CSS custom property name after CSSOM decoding.
pub fn is_valid_custom_property_name(name: &str) -> bool {
    valid_custom_property_name(name)
}

fn condition_function(input: &str, start: usize) -> Option<(String, usize)> {
    let bytes = input.as_bytes();
    let mut position = start;
    let first = input.get(position..)?.chars().next()?;
    let name_start = |character: char| {
        character == '_' || character.is_ascii_alphabetic() || !character.is_ascii()
    };
    if first == '-' {
        position += 1;
        let next = input.get(position..)?.chars().next()?;
        if next == '-' {
            position += 1;
        } else if name_start(next) {
            position += next.len_utf8();
        } else if next == '\\' {
            selector_escape(input, &mut position)?;
        } else {
            return None;
        }
    } else if name_start(first) {
        position += first.len_utf8();
    } else if first == '\\' {
        selector_escape(input, &mut position)?;
    } else {
        return None;
    }

    let mut name = String::new();
    let mut scan = start;
    while scan < position {
        let byte = bytes[scan];
        if byte == b'\\' {
            name.push(selector_escape(input, &mut scan)?);
        } else {
            let character = input[scan..].chars().next()?;
            name.push(character);
            scan += character.len_utf8();
        }
    }
    while position < input.len() {
        let byte = bytes[position];
        if byte == b'\\' {
            name.push(selector_escape(input, &mut position)?);
        } else {
            let character = input[position..].chars().next()?;
            if !(character.is_ascii_alphanumeric()
                || character == '_'
                || character == '-'
                || !character.is_ascii())
            {
                break;
            }
            name.push(character);
            position += character.len_utf8();
        }
    }
    (bytes.get(position) == Some(&b'(')).then_some((name, position))
}

fn condition_keyword(input: &str, position: usize, keyword: &str) -> Option<usize> {
    let end = position.checked_add(keyword.len())?;
    if !input.get(position..end)?.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let following = input.as_bytes().get(end).copied()?;
    (is_css_whitespace(following) || input[end..].starts_with("/*")).then_some(end)
}

fn is_css_whitespace(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

fn skip_css_space_comments(input: &str, position: &mut usize) -> Option<()> {
    loop {
        while input
            .as_bytes()
            .get(*position)
            .copied()
            .is_some_and(is_css_whitespace)
        {
            *position += 1;
        }
        if input[*position..].starts_with("/*") {
            let end = input[*position + 2..].find("*/")?;
            *position += end + 4;
        } else {
            return Some(());
        }
    }
}

/// Skip a quoted component without interpreting its contents as rule delimiters.
fn quoted_css_end(input: &str, start: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    let quote = *bytes.get(start)?;
    if !matches!(quote, b'\'' | b'"') {
        return None;
    }
    let mut position = start + 1;
    while position < bytes.len() {
        if bytes[position] == b'\\' {
            position += 1;
            position += input.get(position..)?.chars().next()?.len_utf8();
        } else if bytes[position] == quote {
            return Some(position + 1);
        } else {
            position += input[position..].chars().next()?.len_utf8();
        }
    }
    None
}

/// Return the byte after a balanced CSS block, rejecting mismatched nested
/// blocks, malformed comments/strings/escapes, and excessive nesting.
fn matching_css_block(input: &str, open: usize) -> Option<usize> {
    const MAX_BLOCK_DEPTH: usize = 32;
    let bytes = input.as_bytes();
    let first = *bytes.get(open)?;
    if !matches!(first, b'(' | b'[' | b'{') {
        return None;
    }
    let mut stack = alloc::vec![match first {
        b'(' => b')',
        b'[' => b']',
        _ => b'}',
    }];
    let mut position = open + 1;
    while position < bytes.len() {
        let byte = bytes[position];
        if byte == b'\\' {
            selector_escape(input, &mut position)?;
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            position = quoted_css_end(input, position)?;
            continue;
        }
        if byte == b'/' && bytes.get(position + 1) == Some(&b'*') {
            let end = input[position + 2..].find("*/")?;
            position += end + 4;
            continue;
        }
        if matches!(byte, b'(' | b'[' | b'{') {
            stack.push(match byte {
                b'(' => b')',
                b'[' => b']',
                _ => b'}',
            });
            if stack.len() > MAX_BLOCK_DEPTH {
                return None;
            }
        } else if matches!(byte, b')' | b']' | b'}') {
            if stack.pop()? != byte {
                return None;
            }
            if stack.is_empty() {
                return Some(position + 1);
            }
        }
        position += input[position..].chars().next()?.len_utf8();
    }
    None
}

/// Recognize a font-face prelude with optional trailing CSS space and comments.
pub fn is_font_face_prelude(input: &str) -> bool {
    let input = input.trim();
    if !input
        .get(..10)
        .is_some_and(|name| name.eq_ignore_ascii_case("@font-face"))
    {
        return false;
    }
    let mut position = 10;
    skip_css_space_comments(input, &mut position).is_some() && position == input.len()
}

/// CSSOM font descriptors share comment, variable and value parsing with the
/// renderer's font-face declarations.
pub fn cssom_font_face_declaration_text(input: &str) -> String {
    let spans = match declaration_spans(input) {
        Ok(spans) => spans,
        Err(error) => {
            declaration_spans(&input[..error.offset.min(input.len())]).unwrap_or_default()
        }
    };
    let mut result = String::new();
    for (start, end) in spans {
        let Some((name, raw)) = declaration_pair(&input[start..end]) else {
            continue;
        };
        let mut name = name.trim().to_ascii_lowercase();
        if name == "font-stretch" {
            name = String::from("font-width");
        }
        if !matches!(
            name.as_str(),
            "font-family"
                | "src"
                | "font-style"
                | "font-weight"
                | "font-stretch"
                | "font-width"
                | "unicode-range"
                | "font-display"
                | "font-feature-settings"
                | "font-variation-settings"
                | "size-adjust"
                | "ascent-override"
                | "descent-override"
                | "line-gap-override"
        ) {
            continue;
        }
        let budget = if name == "src" {
            MAX_CSS_BYTES
        } else {
            MAX_VARIABLE_BYTES
        };
        let Some(cleaned) = expand_variables_bounded(raw, &[], &mut Vec::new(), false, budget)
        else {
            continue;
        };
        let (value, important) = important_value(cleaned.trim());
        let mut descriptors = FontFaceDescriptors::default();
        let parsed = if name == "font-family" {
            descriptors.set_css_family(value)
        } else {
            descriptors.set(&name, value)
        };
        if important || parsed.is_err() {
            continue;
        }
        if let Some(value) = descriptors.get(&name) {
            if let Ok(updated) = set_declaration(&result, &name, &value, false) {
                result = updated;
            }
        }
    }
    result
}

/// Parse CSSOM declaration text, ignoring invalid and unsupported declarations.
pub fn cssom_declaration_text(input: &str) -> String {
    let spans = match declaration_spans(input) {
        Ok(spans) => spans,
        Err(error) => {
            declaration_spans(&input[..error.offset.min(input.len())]).unwrap_or_default()
        }
    };
    let mut result = String::new();
    for (start, end) in spans {
        let Some((name, raw)) = declaration_pair(&input[start..end]) else {
            continue;
        };
        let name = name.trim();
        let (value, important) = important_value(raw);
        if !supports_declaration(name, value) {
            continue;
        }
        if !important
            && declaration_value(&result, name)
                .ok()
                .flatten()
                .is_some_and(|(_, priority)| priority)
        {
            continue;
        }
        if let Ok(updated) = set_declaration(&result, name, value, important) {
            result = updated;
        }
    }
    result
}

/// Serialize an already parsed declaration block for CSSOM reflection.
/// Keep the compact storage form separate from CSSOM's property/value spacing;
/// declaration boundaries still come from the shared CSS tokenizer.
pub fn serialize_cssom_declaration_block(input: &str) -> Result<String, CssError> {
    let spans = declaration_spans(input)?;
    let mut output = String::new();
    for (start, end) in spans {
        let Some((name, raw)) = declaration_pair(&input[start..end]) else {
            continue;
        };
        let (value, important) = important_value(raw);
        let name = name.trim();
        let value = value.trim();
        let extra = name
            .len()
            .checked_add(value.len())
            .and_then(|bytes| bytes.checked_add(if important { 15 } else { 4 }))
            .ok_or(CssError {
                offset: start,
                message: "CSS serialization exceeds limit",
            })?;
        if output
            .len()
            .checked_add(extra)
            .is_none_or(|bytes| bytes > MAX_CSS_BYTES)
            || output.try_reserve(extra).is_err()
        {
            return Err(CssError {
                offset: start,
                message: "CSS serialization exceeds limit",
            });
        }
        if !output.is_empty() {
            output.push(' ');
        }
        output.push_str(name);
        output.push_str(": ");
        output.push_str(value);
        if important {
            output.push_str(" !important");
        }
        output.push(';');
    }
    Ok(output)
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
        179 => Value::LetterSpacing(Some(value)),
        180 => Value::WordSpacing(LengthPercentage {
            pixels: value,
            fraction: 0.0,
        }),
        17 => Value::Gap(value),
        20 => Value::FlexBasis(Some(value)),
        23 => Value::MinWidth(value),
        24 => Value::MaxWidth(Some(value)),
        26 => Value::BorderSpacing([value; 2]),
        35..=38 => Value::Offset(slot - 35, Some(value)),
        39..=42 => Value::MarginSide(slot - 39, value),
        43..=46 => Value::PaddingSide(slot - 43, value),
        51 => Value::MinHeight(value),
        52 => Value::MaxHeight(Some(value)),
        115..=118 => Value::LogicalBorder(slot, LogicalBorderComponent::Width(value)),
        _ => return None,
    })
}

fn parse_context_length(slot: usize, raw: &str, nonnegative: bool) -> Option<Value> {
    let contextual_basis = slot == 20 && has_contextual_length_unit(raw);
    if !contextual_basis {
        if let Some(value) = if nonnegative {
            nonnegative_length(raw)
        } else {
            length(raw)
        } {
            return length_value(slot, value);
        }
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
        ex: 8.0,
        ch: 8.0,
        viewport: MediaEnvironment::default(),
        percent: Some(100.0),
    };
    let value = contextual_length(raw, Some(context))?;
    if nonnegative && value < 0.0 && !math_function(raw) {
        return None;
    }
    length_value(slot, 0.0)?;
    Some(Value::ContextLength(slot, raw.into(), nonnegative))
}

fn has_contextual_length_unit(input: &str) -> bool {
    if input.contains('%') {
        return true;
    }
    let bytes = input.as_bytes();
    let mut pos = 0;
    while pos < bytes.len() {
        if !bytes[pos].is_ascii_alphabetic() {
            pos += input[pos..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        let start = pos;
        while bytes.get(pos).is_some_and(u8::is_ascii_alphabetic) {
            pos += 1;
        }
        if [
            "em", "ex", "rem", "vw", "vh", "vmin", "vmax", "cqw", "cqh", "cqi", "cqb", "cqmin",
            "cqmax",
        ]
        .iter()
        .any(|unit| input[start..pos].eq_ignore_ascii_case(unit))
        {
            return true;
        }
    }
    false
}

#[derive(Clone, Copy)]
struct PropertyRegistration {
    name: &'static str,
    ids: &'static [usize],
    inherited: bool,
    in_all: bool,
}

const PROPERTY_COUNT: usize = 182;

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

/// Registered properties whose computed value only changes painted output,
/// never box geometry. Unregistered, custom and unlisted names are not.
pub(crate) fn paint_only_property(name: &str) -> bool {
    const PAINT_ONLY: &[&str] = &[
        "opacity",
        "transform",
        "color",
        "background-color",
        "border-color",
        "border-top-color",
        "border-right-color",
        "border-bottom-color",
        "border-left-color",
        "outline-color",
        "text-decoration-color",
        "filter",
        "backdrop-filter",
    ];
    let name = name.trim().to_ascii_lowercase();
    PAINT_ONLY.contains(&name.as_str()) && !slots(&name).is_empty()
}

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
    "animation-name" => [163], false, true;
    "animation-duration" => [164], false, true;
    "animation-delay" => [165], false, true;
    "animation-timing-function" => [166], false, true;
    "animation-iteration-count" => [167], false, true;
    "animation-direction" => [168], false, true;
    "animation-fill-mode" => [169], false, true;
    "animation-play-state" => [170], false, true;
    "animation-composition" => [171], false, true;
    "animation" => [163,164,165,166,167,168,169,170], false, true;
    "transition-duration" => [175], false, true;
    "transition-delay" => [176], false, true;
    "transform" => [59], false, true;
    "transform-origin" => [60], false, true;
    "order" => [48], false, true;
    "align-content" => [49], false, true;
    "align-self" => [47], false, true;
    "display" => [0], false, true;
    "color" => [1], true, true;
    "background" => [2,53,68,69,70,71,72,133], false, true;
    "background-color" => [2], false, true;
    "background-image" => [53], false, true;
    "background-position" => [68], false, true;
    "background-size" => [69], false, true;
    "background-repeat" => [70], false, true;
    "background-clip" => [71], false, true;
    "background-origin" => [72], false, true;
    "aspect-ratio" => [73], false, true;
    "z-index" => [74], false, true;
    "margin-inline" => [85, 86], false, true;
    "margin-block" => [99, 100], false, true;
    "margin-block-start" => [99], false, true;
    "margin-block-end" => [100], false, true;
    "padding-inline" => [81, 82], false, true;
    "padding-block" => [83, 84], false, true;
    "inset-inline" => [95, 96], false, true;
    "inset-block" => [97, 98], false, true;
    "inset-inline-start" => [95], false, true;
    "inset-inline-end" => [96], false, true;
    "inset-block-start" => [97], false, true;
    "inset-block-end" => [98], false, true;
    "box-shadow" => [58], false, true;
    "white-space" => [54], true, true;
    "text-align" => [55], true, true;
    "text-indent" => [181], true, true;
    "letter-spacing" => [179], true, true;
    "word-spacing" => [180], true, true;
    "direction" => [56], true, false;
    "text-decoration" => [57], false, true;
    "text-decoration-line" => [57], false, true;
    "width" => [3, 147], false, true;
    "height" => [4, 148], false, true;
    "margin" => [5, 39, 40, 41, 42], false, true;
    "padding" => [6, 43, 44, 45, 46], false, true;
    "font-size" => [7], true, true;
    "font-family" => [129], true, true;
    "font-weight" => [130], true, true;
    "font-style" => [131], true, true;
    "font-stretch" => [139], true, true;
    "font-size-adjust" => [140], true, true;
    "font" => [131,130,139,140,7,13,129], true, true;
    "fill" => [141], true, true;
    "stroke" => [142], true, true;
    "stroke-width" => [143], true, true;
    "fill-rule" => [144], true, true;
    "x" => [145], false, true;
    "y" => [146], false, true;
    "rx" => [149], false, true;
    "ry" => [150], false, true;
    "cx" => [151], false, true;
    "cy" => [152], false, true;
    "r" => [153], false, true;
    "clip-path" => [154], false, true;
    "clip-rule" => [155], true, true;
    "stop-color" => [156], false, true;
    "stop-opacity" => [157], false, true;
    "border-radius" => [8,134,135,136,137], false, true;
    "border-top-left-radius" => [134], false, true;
    "border-top-right-radius" => [135], false, true;
    "border-bottom-right-radius" => [136], false, true;
    "border-bottom-left-radius" => [137], false, true;
    "border-width" => [9,115,116,117,118], false, true;
    "border-color" => [10,119,120,121,122], false, true;
    "border-style" => [11,123,124,125,126], false, true;
    "border" => [9,10,11,115,116,117,118,119,120,121,122,123,124,125,126], false, true;
    "overflow" => [12,132], false, true;
    "overflow-x" => [12], false, true;
    "overflow-y" => [132], false, true;
    "background-attachment" => [133], false, true;
    "inset" => [35,36,37,38], false, true;
    "line-height" => [13], true, true;
    "flex-direction" => [14], false, true;
    "flex-flow" => [14,22,50], false, true;
    "justify-content" => [15], false, true;
    "align-items" => [16], false, true;
    "gap" => [17,76], false, true;
    "row-gap" => [17], false, true;
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
    "grid" => [27,28,66,61,62,63], false, true;
    "grid-template" => [27,28,66], false, true;
    "grid-template-columns" => [27], false, true;
    "grid-auto-columns" => [61], false, true;
    "grid-auto-rows" => [62], false, true;
    "grid-auto-flow" => [63], false, true;
    "justify-items" => [64], false, true;
    "justify-self" => [65], false, true;
    "grid-template-areas" => [66], false, true;
    // The grid-area shorthand sets the optional named-area value and both
    // physical-axis placements.
    "grid-area" => [67,29,30], false, true;
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
    "column-count" => [75], false, true;
    "column-gap" => [76], false, true;
    "column-fill" => [77], false, true;
    "column-rule-width" => [78], false, true;
    "column-rule-color" => [79], false, true;
    "column-rule-style" => [80], false, true;
    "column-rule" => [78,79,80], false, true;
    "padding-inline-start" => [81], false, true;
    "padding-inline-end" => [82], false, true;
    "padding-block-start" => [83], false, true;
    "padding-block-end" => [84], false, true;
    "margin-inline-start" => [85], false, true;
    "margin-inline-end" => [86], false, true;
    "inline-size" => [87], false, true;
    "block-size" => [88], false, true;
    "writing-mode" => [89], true, true;
    "contain" => [90], false, true;
    "visibility" => [101], true, true;
    "pointer-events" => [177], true, true;
    "empty-cells" => [102], true, true;
    "caption-side" => [127], false, true;
    "border-collapse" => [128], true, true;
    "border-inline-start-width" => [103], false, true;
    "border-inline-end-width" => [104], false, true;
    "border-block-start-width" => [105], false, true;
    "border-block-end-width" => [106], false, true;
    "border-inline-start-color" => [107], false, true;
    "border-inline-end-color" => [108], false, true;
    "border-block-start-color" => [109], false, true;
    "border-block-end-color" => [110], false, true;
    "border-inline-start-style" => [111], false, true;
    "border-inline-end-style" => [112], false, true;
    "border-block-start-style" => [113], false, true;
    "border-block-end-style" => [114], false, true;
    "min-inline-size" => [91], false, true;
    "max-inline-size" => [92], false, true;
    "min-block-size" => [93], false, true;
    "max-block-size" => [94], false, true;
    "border-inline" => [103,104,107,108,111,112], false, true;
    "border-block" => [105,106,109,110,113,114], false, true;
    "border-inline-start" => [103,107,111], false, true;
    "border-inline-end" => [104,108,112], false, true;
    "border-block-start" => [105,109,113], false, true;
    "border-block-end" => [106,110,114], false, true;
    "border-top-width" => [115], false, true;
    "border-right-width" => [116], false, true;
    "border-bottom-width" => [117], false, true;
    "border-left-width" => [118], false, true;
    "border-top-color" => [119], false, true;
    "border-right-color" => [120], false, true;
    "border-bottom-color" => [121], false, true;
    "border-left-color" => [122], false, true;
    "border-top-style" => [123], false, true;
    "border-right-style" => [124], false, true;
    "border-bottom-style" => [125], false, true;
    "border-left-style" => [126], false, true;
    "border-top" => [115,119,123], false, true;
    "border-right" => [116,120,124], false, true;
    "border-bottom" => [117,121,125], false, true;
    "border-left" => [118,122,126], false, true;
    "vertical-align" => [138], false, true;
    "content" => [158], false, true;
    "quotes" => [159], true, true;
    "counter-reset" => [160], false, true;
    "counter-increment" => [161], false, true;
    "counter-set" => [162], false, true;
    "list-style-type" => [172], true, true;
    "list-style-position" => [173], true, true;
    "list-style-image" => [178], true, true;
    "list-style" => [172, 173, 178], true, true;
    "color-scheme" => [174], true, true;
}

/// CSS property names in the shared declaration registry, used to install the
/// corresponding camel-case CSSStyleDeclaration aliases in the DOM adapter.
pub fn cssom_property_names() -> Vec<&'static str> {
    PROPERTIES.iter().map(|property| property.name).collect()
}

fn copy_slot(slot: usize, from: &Style, to: &mut Style) {
    if let Some((_, length)) = from.relative_lengths.iter().find(|(key, _)| *key == slot) {
        to.relative_lengths.push((slot, *length));
    }
    if let Some(expression) = from
        .relative_expressions
        .iter()
        .find(|expression| expression.slot == slot)
    {
        to.relative_expressions.push(expression.clone());
    }
    match slot {
        163..=171 => to.animation[slot - 163] = from.animation[slot - 163].clone(),
        175 => to.transition_duration = from.transition_duration.clone(),
        176 => to.transition_delay = from.transition_delay.clone(),
        172 => to.list_style_type = from.list_style_type.clone(),
        173 => to.list_style_position = from.list_style_position,
        174 => to.color_scheme = from.color_scheme.clone(),
        178 => to.list_style_image = from.list_style_image.clone(),
        0 => to.display = from.display,
        1 => to.color = from.color,
        2 => to.background = from.background,
        3 => to.width = from.width,
        4 => {
            to.height = from.height;
            to.height_intrinsic = from.height_intrinsic;
        }
        5 => to.margin = from.margin,
        6 => to.padding = from.padding,
        7 => to.font_size = from.font_size,
        8 => {
            to.border_radius = from.border_radius;
            to.border_corner_radii = from.border_corner_radii.clone();
        }
        134..=137 => {
            let index = slot - 134;
            apply_border_radius_corner(to, index, border_radius_corners(from)[index].clone());
        }
        9 => to.border_width = from.border_width,
        10 => to.border_color = from.border_color,
        11 => {
            Value::BorderStyle(from.border_style).apply(to);
        }
        12 => to.overflow_x = from.overflow_x,
        132 => to.overflow_y = from.overflow_y,
        133 => to.background_attachment = from.background_attachment.clone(),
        13 => to.line_height = from.line_height,
        14 => to.flex_direction = from.flex_direction,
        15 => to.justify_content = from.justify_content,
        16 => to.align_items = from.align_items,
        17 => {
            to.gap = from.gap;
            to.gap_specified = from.gap_specified;
            to.row_gap_fraction = from.row_gap_fraction;
        }
        18 => to.flex_grow = from.flex_grow,
        19 => to.flex_shrink = from.flex_shrink,
        20 => {
            to.flex_basis = from.flex_basis;
            to.flex_basis_content = from.flex_basis_content;
            to.flex_basis_intrinsic = from.flex_basis_intrinsic;
        }
        21 => to.box_sizing = from.box_sizing,
        22 => to.flex_wrap = from.flex_wrap,
        23 => {
            to.min_width = from.min_width;
            to.min_width_auto = from.min_width_auto;
        }
        24 => to.max_width = from.max_width,
        25 => to.table_fixed = from.table_fixed,
        26 => to.border_spacing = from.border_spacing,
        27 => {
            to.grid_columns = from.grid_columns.clone();
            to.grid_column_names = from.grid_column_names.clone();
            to.grid_columns_subgrid = from.grid_columns_subgrid;
            to.grid_columns_auto = from.grid_columns_auto.clone();
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
            to.grid_rows_auto = from.grid_rows_auto.clone();
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
        51 => {
            to.min_height = from.min_height;
            to.min_height_intrinsic = from.min_height_intrinsic;
            to.min_height_auto = from.min_height_auto;
        }
        52 => {
            to.max_height = from.max_height;
            to.max_height_intrinsic = from.max_height_intrinsic;
        }
        53 => {
            if to.background_images != from.background_images {
                to.background_images = from.background_images.clone();
            }
        }
        68 => to.background_position = from.background_position.clone(),
        69 => to.background_size = from.background_size.clone(),
        70 => to.background_repeat = from.background_repeat.clone(),
        71 => to.background_clip = from.background_clip.clone(),
        72 => to.background_origin = from.background_origin.clone(),
        73 => to.aspect_ratio = from.aspect_ratio,
        74 => to.z_index = from.z_index,
        75 => to.column_count = from.column_count,
        76 => {
            to.column_gap = from.column_gap;
            to.column_gap_fraction = from.column_gap_fraction;
        }
        77 => to.column_fill_auto = from.column_fill_auto,
        78 => to.column_rule_width = from.column_rule_width,
        79 => to.column_rule_color = from.column_rule_color,
        80 => {
            to.column_rule_visible = from.column_rule_visible;
            to.column_rule_pattern = from.column_rule_pattern;
        }
        81 | 82 => to.logical_padding_inline[slot - 81] = from.logical_padding_inline[slot - 81],
        83 | 84 => to.logical_padding_block[slot - 83] = from.logical_padding_block[slot - 83],
        85 | 86 => to.logical_margin_inline[slot - 85] = from.logical_margin_inline[slot - 85],
        87 => to.logical_inline_size = from.logical_inline_size,
        88 => to.logical_block_size = from.logical_block_size,
        89 => to.writing_mode = from.writing_mode,
        90 => {
            to.contain_paint = from.contain_paint;
            to.contain_layout = from.contain_layout;
            to.contain_size = from.contain_size;
        }
        101 => to.visibility_visible = from.visibility_visible,
        177 => to.pointer_events_auto = from.pointer_events_auto,
        102 => to.empty_cells_hide = from.empty_cells_hide,
        127 => to.caption_bottom = from.caption_bottom,
        128 => to.border_collapse = from.border_collapse,
        129 => {
            if to.font.families != from.font.families {
                to.font.families = from.font.families.clone();
            }
        }
        130 => {
            if to.font.weight != from.font.weight {
                to.font.weight = from.font.weight;
            }
        }
        139 => to.font.stretch = from.font.stretch,
        131 => {
            if to.font.style != from.font.style {
                to.font.style = from.font.style;
            }
        }
        138 => to.vertical_align = from.vertical_align,
        140 => to.font.size_adjust = from.font.size_adjust,
        141 => to.svg_fill = from.svg_fill.clone(),
        142 => to.svg_stroke = from.svg_stroke.clone(),
        143 => to.svg_stroke_width = from.svg_stroke_width,
        144 => to.svg_fill_rule = from.svg_fill_rule,
        145..=153 => to.svg_geometry[slot - 145] = from.svg_geometry[slot - 145].clone(),
        154 => to.svg_clip_path = from.svg_clip_path.clone(),
        155 => to.svg_clip_rule = from.svg_clip_rule,
        156 => to.svg_stop_color = from.svg_stop_color,
        157 => to.svg_stop_opacity = from.svg_stop_opacity,
        158 => {
            to.generated_content = from.generated_content.clone();
            to.content_none = from.content_none;
        }
        159 => to.quotes = from.quotes.clone(),
        160 => to.counter_reset = from.counter_reset.clone(),
        161 => to.counter_increment = from.counter_increment.clone(),
        162 => to.counter_set = from.counter_set.clone(),
        103..=106 => to.logical_border_width[slot - 103] = from.logical_border_width[slot - 103],
        107..=110 => to.logical_border_color[slot - 107] = from.logical_border_color[slot - 107],
        111..=114 => to.logical_border_style[slot - 111] = from.logical_border_style[slot - 111],
        115..=118 => Value::LogicalBorder(
            slot,
            LogicalBorderComponent::Width(
                from.border_width_sides[slot - 115].unwrap_or(from.border_width),
            ),
        )
        .apply(to),
        119..=122 => Value::LogicalBorder(
            slot,
            LogicalBorderComponent::Color(
                from.border_color_sides[slot - 119].unwrap_or(from.border_color),
            ),
        )
        .apply(to),
        123..=126 => Value::LogicalBorder(
            slot,
            LogicalBorderComponent::Style(
                from.border_style_sides[slot - 123].unwrap_or(from.border_style),
            ),
        )
        .apply(to),
        91 | 93 => {
            let axis = if slot == 91 { 0 } else { 1 };
            to.logical_min_size[axis] = from.logical_min_size[axis];
        }
        92 | 94 => {
            let axis = if slot == 92 { 0 } else { 1 };
            to.logical_max_size[axis] = from.logical_max_size[axis];
        }
        95..=98 => to.logical_offsets[slot - 95] = from.logical_offsets[slot - 95],
        99..=100 => to.logical_margin_block[slot - 99] = from.logical_margin_block[slot - 99],
        54 => Value::WhiteSpace(from.white_space).apply(to),
        55 => Value::TextAlign(from.text_align).apply(to),
        181 => Value::TextIndent(from.text_indent).apply(to),
        179 => Value::LetterSpacing(from.letter_spacing).apply(to),
        180 => Value::WordSpacing(from.word_spacing).apply(to),
        56 => Value::Direction(from.direction).apply(to),
        57 => Value::TextDecoration(from.text_decoration).apply(to),
        58 => Value::Shadows(from.shadows.clone()).apply(to),
        59 => Value::Transforms(from.transforms.clone()).apply(to),
        60 => Value::TransformOrigin(from.transform_origin).apply(to),
        _ => {}
    }
}

/// Parses an `aspect-ratio` value: `<number>` or `<number> / <number>` with
/// positive components (CSS Sizing §6.1).
fn ratio(raw: &str) -> Option<f32> {
    let (numerator, denominator) = match raw.split_once('/') {
        Some((numerator, denominator)) => (numerator.trim(), denominator.trim()),
        None => (raw.trim(), "1"),
    };
    let numerator = numerator.parse::<f32>().ok()?;
    let denominator = denominator.parse::<f32>().ok()?;
    if numerator <= 0.0 || denominator <= 0.0 {
        return None;
    }
    Some(numerator / denominator)
}

fn intrinsic_sizing(value: &str) -> Option<IntrinsicSizing> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("min-content") {
        Some(IntrinsicSizing::MinContent)
    } else if value.eq_ignore_ascii_case("max-content") {
        Some(IntrinsicSizing::MaxContent)
    } else if value.eq_ignore_ascii_case("fit-content") {
        Some(IntrinsicSizing::FitContent)
    } else {
        None
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

fn border_radius_length(raw: &str, context: LengthContext) -> Option<BorderRadiusLength> {
    let raw = raw.trim();
    let value = transform_length(raw, context)?;
    if !math_function(raw) && (value.pixels < 0.0 || value.percent < 0.0) {
        return None;
    }
    Some(BorderRadiusLength {
        value,
        expression: comparison_function(raw).then(|| Arc::from(raw)),
        context,
    })
}

fn expand_radius_axis(values: &[BorderRadiusLength]) -> Option<[BorderRadiusLength; 4]> {
    let first = values.first()?.clone();
    let second = values.get(1).unwrap_or(&first).clone();
    let third = values.get(2).unwrap_or(&first).clone();
    let fourth = values
        .get(3)
        .unwrap_or_else(|| values.get(1).unwrap_or(&first))
        .clone();
    Some([first, second, third, fourth])
}

fn border_radius_values(raw: &str, context: LengthContext) -> Option<[BorderRadiusCorner; 4]> {
    let axes = top_level_split(raw, b'/', 3)?;
    if !(1..=2).contains(&axes.len()) {
        return None;
    }
    let horizontal_tokens = components(axes[0]).filter(|tokens| (1..=4).contains(&tokens.len()))?;
    let horizontal = expand_radius_axis(
        &horizontal_tokens
            .into_iter()
            .map(|token| border_radius_length(token, context))
            .collect::<Option<Vec<_>>>()?,
    )?;
    let vertical = if let Some(axis) = axes.get(1) {
        let tokens = components(axis).filter(|tokens| (1..=4).contains(&tokens.len()))?;
        expand_radius_axis(
            &tokens
                .into_iter()
                .map(|token| border_radius_length(token, context))
                .collect::<Option<Vec<_>>>()?,
        )?
    } else {
        horizontal.clone()
    };
    Some(core::array::from_fn(|index| BorderRadiusCorner {
        horizontal: horizontal[index].clone(),
        vertical: vertical[index].clone(),
    }))
}

fn border_radius_corner_index(name: &str) -> Option<usize> {
    match name {
        "border-top-left-radius" => Some(0),
        "border-top-right-radius" => Some(1),
        "border-bottom-right-radius" => Some(2),
        "border-bottom-left-radius" => Some(3),
        _ => None,
    }
}

fn border_radius_corner_value(raw: &str, context: LengthContext) -> Option<BorderRadiusCorner> {
    let axes = top_level_split(raw, b'/', 2)?;
    if axes.len() != 1 {
        return None;
    }
    let values = components(axes[0]).filter(|values| (1..=2).contains(&values.len()))?;
    let horizontal = border_radius_length(values[0], context)?;
    let vertical = border_radius_length(values.get(1).copied().unwrap_or(values[0]), context)?;
    Some(BorderRadiusCorner {
        horizontal,
        vertical,
    })
}

fn border_radius_corners(style: &Style) -> [BorderRadiusCorner; 4] {
    if let Some(corners) = &style.border_corner_radii {
        return corners.clone();
    }
    let radius = if style.border_radius.is_finite() {
        style.border_radius.max(0.0)
    } else {
        0.0
    };
    let length = BorderRadiusLength {
        value: TransformLength {
            pixels: radius,
            percent: 0.0,
        },
        expression: None,
        context: static_length_context(),
    };
    core::array::from_fn(|_| BorderRadiusCorner {
        horizontal: length.clone(),
        vertical: length.clone(),
    })
}

fn uniform_absolute_radius(corners: &[BorderRadiusCorner; 4]) -> Option<f32> {
    let first = &corners[0].horizontal;
    if first.expression.is_some() || first.value.percent != 0.0 {
        return None;
    }
    if corners.iter().any(|corner| {
        [&corner.horizontal, &corner.vertical].iter().any(|value| {
            value.expression.is_some() || value.value.percent != 0.0 || value.value != first.value
        })
    }) {
        return None;
    }
    Some(first.value.pixels.max(0.0))
}

fn apply_border_radii(style: &mut Style, corners: [BorderRadiusCorner; 4]) {
    if let Some(radius) = uniform_absolute_radius(&corners) {
        style.border_radius = radius;
        style.border_corner_radii = None;
        return;
    }
    style.border_radius = corners
        .iter()
        .flat_map(|corner| [&corner.horizontal, &corner.vertical])
        .map(|value| value.value.pixels.max(0.0))
        .fold(0.0, f32::max);
    style.border_corner_radii = Some(corners);
}

fn apply_border_radius_corner(style: &mut Style, index: usize, corner: BorderRadiusCorner) {
    if index >= 4 {
        return;
    }
    let mut corners = border_radius_corners(style);
    corners[index] = corner;
    apply_border_radii(style, corners);
}

fn radius_number(value: f32) -> String {
    if value == 0.0 {
        return "0".into();
    }
    alloc::format!("{value}")
}

fn serialize_radius_length(value: &BorderRadiusLength) -> String {
    if let Some(expression) = &value.expression {
        return expression.to_string();
    }
    let pixels = value.value.pixels;
    let percent = value.value.percent;
    if percent == 0.0 {
        return alloc::format!("{}px", radius_number(pixels.max(0.0)));
    }
    if pixels == 0.0 {
        return alloc::format!("{}%", radius_number(percent));
    }
    if pixels > 0.0 {
        alloc::format!(
            "calc({}% + {}px)",
            radius_number(percent),
            radius_number(pixels)
        )
    } else {
        alloc::format!(
            "calc({}% - {}px)",
            radius_number(percent),
            radius_number(-pixels)
        )
    }
}

fn compact_radius_axis(values: &[String; 4]) -> String {
    let count = if values[0] == values[1] && values[0] == values[2] && values[0] == values[3] {
        1
    } else if values[0] == values[2] && values[1] == values[3] {
        2
    } else if values[1] == values[3] {
        3
    } else {
        4
    };
    values[..count].join(" ")
}

fn serialize_border_radius(style: &Style) -> String {
    let Some(corners) = &style.border_corner_radii else {
        return alloc::format!("{}px", radius_number(style.border_radius));
    };
    let horizontal =
        core::array::from_fn(|index| serialize_radius_length(&corners[index].horizontal));
    let vertical = core::array::from_fn(|index| serialize_radius_length(&corners[index].vertical));
    let horizontal = compact_radius_axis(&horizontal);
    if horizontal == compact_radius_axis(&vertical) {
        horizontal
    } else {
        alloc::format!("{horizontal} / {}", compact_radius_axis(&vertical))
    }
}

fn angle(raw: &str) -> Option<f32> {
    let raw = ascii_lower(raw);
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

/// Parse animation transforms with the same units and limits as stylesheet CSS.
pub fn parse_animation_transforms(
    input: &str,
    style: &Style,
    viewport: MediaEnvironment,
    text: Option<&dyn TextShaper>,
) -> Option<Arc<[Transform]>> {
    if input.trim().eq_ignore_ascii_case("none") {
        return Some(Vec::new().into());
    }
    transform_list(input, style_length_context(text, style, viewport, None))
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
        let name = ascii_lower(rest[..open].trim());
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
        let transform = match &*name {
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
                } else if function.eq_ignore_ascii_case("conic-gradient")
                    || function.eq_ignore_ascii_case("repeating-conic-gradient")
                {
                    conic_gradient(layer, current_color, context)?
                } else {
                    linear_gradient(layer, current_color, context)?
                },
            );
        }
    }
    Some(layers.into())
}

fn comma_components(raw: &str, max: usize) -> Option<Vec<&str>> {
    let layers = top_level_split(raw, b',', max)?;
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
        let (mut lengths, mut count, mut shadow_color, mut inset, mut lengths_finished) =
            ([0.0; 4], 0usize, None, false, false);
        for token in components(shadow)? {
            if token.eq_ignore_ascii_case("inset") {
                if inset {
                    return None;
                }
                inset = true;
                lengths_finished |= count != 0;
            } else if token.eq_ignore_ascii_case("currentcolor") {
                if shadow_color.is_some() {
                    return None;
                }
                shadow_color = Some(current_color);
                lengths_finished |= count != 0;
            } else if let Some(color) = color(token) {
                if shadow_color.is_some() {
                    return None;
                }
                shadow_color = Some(color);
                lengths_finished |= count != 0;
            } else {
                if count == 4 || lengths_finished {
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
                match &*ascii_lower(direction) {
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
            let angle_value = ascii_lower(token);
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
    if stops.len() == 1 {
        stops.push(stops[0]);
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

fn gap_value(slot: usize, raw: &str) -> Option<Value> {
    if raw == "normal" {
        return Some(if slot == 17 {
            Value::Gap(0.0)
        } else {
            Value::ColumnGap(None)
        });
    }
    let value = background_length(raw, static_length_context())?;
    if !raw.starts_with("calc(") && (value.pixels < 0.0 || value.fraction < 0.0) {
        return None;
    }
    Some(if length_independent(raw) {
        Value::GapLength(slot, value)
    } else {
        Value::GapRaw(slot, raw.into())
    })
}

fn radial_gradient(
    raw: &str,
    current_color: Rgba,
    context: Option<LengthContext>,
) -> Option<Arc<Gradient>> {
    let context = context.unwrap_or(static_length_context());
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
            } else if let Some(keyword) = match &*ascii_lower(token) {
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

/// Parses `conic-gradient([from <angle>]? [at <position>]? , <stops>)`
/// (CSS Images §3.4). Angular stop positions are stored as fractions of a
/// full turn so the shared stop solver works unchanged.
fn conic_gradient(
    raw: &str,
    current_color: Rgba,
    context: Option<LengthContext>,
) -> Option<Arc<Gradient>> {
    let context = context.unwrap_or(static_length_context());
    let raw = raw.trim();
    let (repeating, raw) = if raw
        .get(..10)
        .is_some_and(|v| v.eq_ignore_ascii_case("repeating-"))
    {
        (true, &raw[10..])
    } else {
        (false, raw)
    };
    if raw.len() > MAX_VARIABLE_BYTES || !raw.get(..15)?.eq_ignore_ascii_case("conic-gradient(") {
        return None;
    }
    let body = raw.get(15..)?.strip_suffix(')')?;
    let mut parts: Vec<&str> = Vec::new();
    let (mut start, mut depth) = (0, 0usize);
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
            if parts.len() > 33 {
                return None;
            }
        }
    }
    if depth != 0 {
        return None;
    }
    parts.push(body[start..].trim());
    let (mut from, mut center, mut first_stop) = (0.0f32, None, 0usize);
    let tokens = components(parts[0])?;
    if !tokens.is_empty() {
        let mut index = 0;
        if tokens
            .first()
            .is_some_and(|v| v.eq_ignore_ascii_case("from"))
        {
            let angle = ascii_lower(tokens.get(1)?);
            from = if let Some(v) = angle.strip_suffix("deg") {
                v.parse::<f32>().ok()?
            } else if let Some(v) = angle.strip_suffix("grad") {
                v.parse::<f32>().ok()? * 0.9
            } else if let Some(v) = angle.strip_suffix("rad") {
                v.parse::<f32>().ok()? * 180.0 / core::f32::consts::PI
            } else if let Some(v) = angle.strip_suffix("turn") {
                v.parse::<f32>().ok()? * 360.0
            } else {
                return None;
            };
            if !from.is_finite() {
                return None;
            }
            index = 2;
        }
        if tokens
            .get(index)
            .is_some_and(|v| v.eq_ignore_ascii_case("at"))
        {
            center = Some(background_position(&tokens[index + 1..], context)?);
            index = tokens.len();
        }
        // Directives were present only if `from`/`at` matched; otherwise the
        // first part is itself a color stop.
        if index > 0 {
            if index != tokens.len() {
                return None;
            }
            first_stop = 1;
        }
    }
    Some(Arc::new(Gradient {
        kind: GradientKind::Conic {
            from,
            center: center.unwrap_or(
                [LengthPercentage {
                    pixels: 0.0,
                    fraction: 0.5,
                }; 2],
            ),
        },
        repeating,
        stops: conic_stops(&parts[first_stop..], current_color, context)?,
    }))
}

/// Like [`gradient_stops`] but angle positions (`deg`/`grad`/`rad`/`turn`)
/// normalize to fractions of a full turn; lengths are rejected.
fn conic_stops(
    parts: &[&str],
    current_color: Rgba,
    _context: LengthContext,
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
                } else {
                    let angle = ascii_lower(token);
                    let degrees = if let Some(v) = angle.strip_suffix("deg") {
                        v.parse::<f32>().ok()?
                    } else if let Some(v) = angle.strip_suffix("grad") {
                        v.parse::<f32>().ok()? * 0.9
                    } else if let Some(v) = angle.strip_suffix("rad") {
                        v.parse::<f32>().ok()? * 180.0 / core::f32::consts::PI
                    } else if let Some(v) = angle.strip_suffix("turn") {
                        v.parse::<f32>().ok()? * 360.0
                    } else {
                        return None;
                    };
                    if !degrees.is_finite() {
                        return None;
                    }
                    GradientPosition::Fraction(degrees / 360.0)
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
    if stops.len() == 1 {
        stops.push(stops[0]);
    }
    if stops.len() < 2 {
        return None;
    }
    Some(stops.into())
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
        let token = ascii_lower(tokens[index]);
        let (axis, far) = match &*token {
            "left" => (0, false),
            "right" => (0, true),
            "top" => (1, false),
            "bottom" => (1, true),
            "center" => {
                let next_axis =
                    tokens
                        .get(index + 1)
                        .and_then(|token| match &*ascii_lower(token) {
                            "left" | "right" => Some(0),
                            "top" | "bottom" => Some(1),
                            _ => None,
                        });
                let axis = next_axis
                    .map_or_else(|| if result[0].is_none() { 0 } else { 1 }, |axis| 1 - axis);
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
                &*ascii_lower(tokens[index]),
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

/// Splits `raw` on a single top-level separator byte, ignoring separators inside
/// parentheses or quoted strings. `max` bounds the number of pieces (the limit
/// is rejected, not truncated, mirroring `comma_components`).
fn top_level_split(raw: &str, separator: u8, max: usize) -> Option<Vec<&str>> {
    top_level_split_bounded(raw, separator, max, MAX_VARIABLE_BYTES)
}

fn top_level_split_bounded(
    raw: &str,
    separator: u8,
    max: usize,
    max_bytes: usize,
) -> Option<Vec<&str>> {
    if raw.len() > max_bytes {
        return None;
    }
    let (mut start, mut depth, mut quote, mut escaped, mut pieces) =
        (0, 0usize, 0u8, false, Vec::new());
    for (pos, byte) in raw.bytes().enumerate() {
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if quote != 0 {
            if byte == quote {
                quote = 0;
            }
        } else if matches!(byte, b'\'' | b'"') {
            quote = byte;
        } else if byte == b'(' {
            depth += 1;
            if depth > 32 {
                return None;
            }
        } else if byte == b')' {
            depth = depth.checked_sub(1)?;
        } else if byte == separator && depth == 0 {
            pieces.push(raw[start..pos].trim());
            if pieces.len() >= max {
                return None;
            }
            start = pos + 1;
        }
    }
    if depth != 0 || quote != 0 {
        return None;
    }
    pieces.push(raw[start..].trim());
    Some(pieces)
}

/// Parses a `url(...)` background image reference. Quoted and unquoted forms
/// are accepted; CSS escape sequences are not decoded.
fn background_url(part: &str) -> Option<Arc<str>> {
    let (function, inner) = part.split_once('(')?;
    if !function.eq_ignore_ascii_case("url") {
        return None;
    }
    let inner = inner.strip_suffix(')')?.trim();
    if inner.starts_with(['\'', '"']) {
        return css_string(inner).map(Arc::from);
    }
    let mut url = String::new();
    let mut offset = 0;
    while offset < inner.len() {
        let character = inner[offset..].chars().next()?;
        if matches!(character, '(' | ')' | '\'' | '"')
            || character.is_ascii_whitespace()
            || character.is_ascii_control()
        {
            return None;
        }
        if character == '\\' {
            if inner
                .as_bytes()
                .get(offset + 1)
                .is_some_and(|byte| matches!(byte, b'\n' | b'\r' | b'\x0c'))
            {
                return None;
            }
            url.push(selector_escape(inner, &mut offset)?);
        } else {
            url.push(character);
            offset += character.len_utf8();
        }
    }
    Some(url.into())
}

fn css_string(raw: &str) -> Option<String> {
    let quote = *raw.as_bytes().first()?;
    if !matches!(quote, b'\'' | b'"') || raw.len() < 2 || !raw.ends_with(quote as char) {
        return None;
    }
    let value = &raw[1..raw.len() - 1];
    let mut result = String::new();
    let mut offset = 0;
    while offset < value.len() {
        let character = value[offset..].chars().next()?;
        if character as u32 == quote as u32 || matches!(character, '\n' | '\r' | '\x0c') {
            return None;
        }
        if character == '\\' {
            if value
                .as_bytes()
                .get(offset + 1)
                .is_some_and(|byte| matches!(byte, b'\n' | b'\r' | b'\x0c'))
            {
                offset += 2;
                if value.as_bytes().get(offset - 1) == Some(&b'\r')
                    && value.as_bytes().get(offset) == Some(&b'\n')
                {
                    offset += 1;
                }
                continue;
            }
            result.push(selector_escape(value, &mut offset)?);
        } else {
            result.push(if character == '\0' {
                '\u{fffd}'
            } else {
                character
            });
            offset += character.len_utf8();
        }
    }
    Some(result)
}

/// Bounded CSS Images 4 candidate negotiation. The host image profile supports
/// PNG; MIME candidates for other decoders are filtered before duplicate
/// resolutions, as required by the specification. Resolution choice is the
/// smallest density at least the media dppx, otherwise the largest density.
fn background_image_set(
    raw: &str,
    current_color: Rgba,
    context: Option<LengthContext>,
) -> Option<BackgroundImage> {
    let (name, arguments) = raw.split_once('(')?;
    if !name.eq_ignore_ascii_case("image-set") && !name.eq_ignore_ascii_case("-webkit-image-set") {
        return None;
    }
    let arguments = arguments.strip_suffix(')')?;
    let target = context.map_or(1.0, |context| context.viewport.resolution);
    let mut candidates: Vec<(f32, BackgroundImage)> = Vec::new();
    for candidate in top_level_split(arguments, b',', 16)? {
        let tokens = components(candidate)?;
        if tokens.is_empty() || tokens.len() > 3 {
            return None;
        }
        let image = tokens[0];
        let mut density = None;
        let mut image_type = None;
        for descriptor in &tokens[1..] {
            if let Some((name, value)) = descriptor.split_once('(') {
                if !name.eq_ignore_ascii_case("type") || image_type.is_some() {
                    return None;
                }
                let value = value.strip_suffix(')')?.trim();
                image_type = Some(css_string(value)?);
            } else {
                if density.is_some() {
                    return None;
                }
                let descriptor = ascii_lower(descriptor);
                let (number, multiplier) = if let Some(value) = descriptor
                    .strip_suffix("dppx")
                    .or_else(|| descriptor.strip_suffix('x'))
                {
                    (value, 1.0)
                } else if let Some(value) = descriptor.strip_suffix("dpi") {
                    (value, 1.0 / 96.0)
                } else if let Some(value) = descriptor.strip_suffix("dpcm") {
                    (value, 2.54 / 96.0)
                } else {
                    return None;
                };
                let value: f32 = number.parse().ok()?;
                if !value.is_finite() || value < 0.0 {
                    return None;
                }
                density = Some(value * multiplier);
            }
        }
        let density = density.unwrap_or(1.0);
        // Validate even options later removed by type or resolution filtering.
        let url = background_url(image).or_else(|| css_string(image).map(Arc::from));
        let resolved = if let Some(url) = url {
            if density == 0.0 {
                BackgroundImage::None
            } else {
                BackgroundImage::UrlResolution { url, density }
            }
        } else {
            BackgroundImage::Gradient(
                gradient_layers(image, current_color, context)?
                    .first()?
                    .clone(),
            )
        };
        if image_type.is_some_and(|image_type| !image_type.eq_ignore_ascii_case("image/png"))
            || candidates.iter().any(|(previous, _)| *previous == density)
        {
            continue;
        }
        candidates.push((density, resolved));
    }
    let selected = candidates
        .iter()
        .enumerate()
        .filter(|(_, (density, _))| *density >= target)
        .min_by(|(_, a), (_, b)| a.0.total_cmp(&b.0))
        .or_else(|| {
            candidates
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.0.total_cmp(&b.0))
        })
        .map(|(index, _)| index);
    Some(selected.map_or(BackgroundImage::None, |index| {
        candidates.swap_remove(index).1
    }))
}

/// Validate every image recursively before publishing a declaration.
fn valid_background_images(raw: &str) -> bool {
    background_images(raw, Style::initial().color, Some(static_length_context())).is_some()
}

/// Resolves a validated `background-image` value once when it cannot depend on
/// the cascade, otherwise defers it to compute time.
fn background_image_value(raw: &str) -> Value {
    if raw.to_ascii_lowercase().contains("image-set(") {
        return Value::BackgroundImageRaw(raw.into());
    }
    let context = static_length_context();
    match color_independent(raw, |color| {
        background_images(raw, color, Some(context)).filter(|images| !images.is_empty())
    }) {
        Some(images) => Value::BackgroundImages(Some(images)),
        None => Value::BackgroundImageRaw(raw.into()),
    }
}

fn background_position_value(raw: &str) -> Option<Value> {
    let context = static_length_context();
    for part in top_level_split(raw, b',', MAX_BACKGROUND_LAYERS)? {
        background_position(&components(part)?, context)?;
    }
    Some(if length_independent(raw) {
        Value::BackgroundPosition(background_positions(raw, None))
    } else {
        Value::BackgroundPositionRaw(raw.into())
    })
}

fn background_size_value(raw: &str) -> Option<Value> {
    let context = static_length_context();
    for part in top_level_split(raw, b',', MAX_BACKGROUND_LAYERS)? {
        parse_background_size(part, context)?;
    }
    Some(if length_independent(raw) {
        Value::BackgroundSizes(background_sizes(raw, None))
    } else {
        Value::BackgroundSizeRaw(raw.into())
    })
}

/// True when resolving `raw` cannot depend on font size, root font size or
/// viewport: every number carries an absolute unit, `%`, or none.
fn length_independent(raw: &str) -> bool {
    const UNITS: [&str; 17] = [
        "px", "cm", "mm", "q", "in", "pt", "pc", "deg", "rad", "turn", "grad", "s", "ms", "fr",
        "x", "dpi", "dppx",
    ];
    let bytes = raw.as_bytes();
    let mut pos = 0;
    while pos < bytes.len() {
        let byte = bytes[pos];
        if matches!(byte, b'\'' | b'"') {
            pos += 1;
            while pos < bytes.len() && bytes[pos] != byte {
                pos += usize::from(bytes[pos] == b'\\') + 1;
            }
            pos += 1;
            continue;
        }
        if byte == b'#' {
            pos += 1;
            while bytes.get(pos).is_some_and(u8::is_ascii_alphanumeric) {
                pos += 1;
            }
            continue;
        }
        let number = byte.is_ascii_digit()
            || (byte == b'.' && bytes.get(pos + 1).is_some_and(u8::is_ascii_digit));
        let inside_identifier = pos > 0
            && (bytes[pos - 1].is_ascii_alphanumeric()
                || bytes[pos - 1] == b'_'
                || (bytes[pos - 1] == b'-'
                    && pos > 1
                    && (bytes[pos - 2].is_ascii_alphanumeric() || bytes[pos - 2] == b'_')));
        if !number || inside_identifier {
            pos += 1;
            continue;
        }
        while bytes
            .get(pos)
            .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'.')
        {
            pos += 1;
        }
        let unit_start = pos;
        while bytes.get(pos).is_some_and(u8::is_ascii_alphabetic) {
            pos += 1;
        }
        let unit = &raw[unit_start..pos];
        let exponent = unit.starts_with(['e', 'E'])
            && bytes
                .get(pos)
                .is_some_and(|byte| byte.is_ascii_digit() || matches!(*byte, b'+' | b'-'));
        if exponent || (!unit.is_empty() && !UNITS.iter().any(|v| unit.eq_ignore_ascii_case(v))) {
            return false;
        }
    }
    true
}

/// Resolves `raw` once when the result depends on neither the length context
/// nor the current color; `resolve` must return `None` for invalid values.
fn color_independent<T: PartialEq>(raw: &str, resolve: impl Fn(Rgba) -> Option<T>) -> Option<T> {
    if !length_independent(raw) {
        return None;
    }
    let first = resolve(Rgba {
        r: 0x11,
        g: 0x22,
        b: 0x33,
        a: 0xff,
    })?;
    let second = resolve(Rgba {
        r: 0xee,
        g: 0xdd,
        b: 0xcc,
        a: 0xff,
    })?;
    (first == second).then_some(first)
}

/// Resolves a validated `background-image` raw value with the computed
/// current color and font context.
fn image_function_args<'a>(raw: &'a str, name: &str) -> Option<&'a str> {
    raw.get(..name.len())?
        .eq_ignore_ascii_case(name)
        .then_some(())?;
    raw.get(name.len()..)?.strip_suffix(')')
}

fn background_image_part(
    raw: &str,
    current_color: Rgba,
    context: Option<LengthContext>,
    depth: usize,
    allow_color: bool,
) -> Option<BackgroundImage> {
    if depth > 16 {
        return None;
    }
    if raw.eq_ignore_ascii_case("none") {
        return Some(BackgroundImage::None);
    }
    if let Some(url) = background_url(raw) {
        return Some(BackgroundImage::Url(url));
    }
    if let Some(image) = background_image_set(raw, current_color, context) {
        return Some(image);
    }
    if let Some(args) = image_function_args(raw, "image(") {
        return color_with_context(args.trim(), current_color).map(BackgroundImage::Solid);
    }
    if let Some(args) = image_function_args(raw, "light-dark(") {
        let parts = top_level_split(args, b',', 2)?;
        if parts.len() != 2 {
            return None;
        }
        let light = background_image_part(parts[0], current_color, context, depth + 1, false)?;
        let _dark = background_image_part(parts[1], current_color, context, depth + 1, false)?;
        // The renderer's current used color scheme is light.
        return Some(light);
    }
    if let Some(args) = image_function_args(raw, "cross-fade(") {
        let mut items = Vec::new();
        let mut specified = 0.0f32;
        let mut missing = 0usize;
        for part in top_level_split(args, b',', 16)? {
            let tokens = components(part)?;
            let mut image = None;
            let mut weight = None;
            for token in tokens {
                if let Some(value) = css_scalar(token, true).filter(|_| token.contains('%')) {
                    if weight.is_some() || !(0.0..=1.0).contains(&value) {
                        return None;
                    }
                    weight = Some(value);
                } else {
                    if image.is_some() {
                        return None;
                    }
                    image = Some(background_image_part(
                        token,
                        current_color,
                        context,
                        depth + 1,
                        true,
                    )?);
                }
            }
            let image = image?;
            if matches!(image, BackgroundImage::None) {
                return None;
            }
            if let Some(w) = weight {
                specified += w;
            } else {
                missing += 1;
            }
            items.push((image, weight));
        }
        if items.is_empty() {
            return None;
        }
        let omitted = if missing == 0 {
            0.0
        } else {
            (1.0 - specified).max(0.0) / missing as f32
        };
        let total = specified + omitted * missing as f32;
        let divisor = total.max(1.0);
        return Some(BackgroundImage::CrossFade(
            items
                .into_iter()
                .map(|(i, w)| (i, w.unwrap_or(omitted) / divisor))
                .collect::<Vec<_>>()
                .into(),
        ));
    }
    if allow_color {
        if let Some(color) = color_with_context(raw, current_color) {
            return Some(BackgroundImage::Solid(color));
        }
    }
    let gradient = gradient_layers(raw, current_color, context)?;
    Some(BackgroundImage::Gradient(gradient.first()?.clone()))
}

fn background_images(
    raw: &str,
    current_color: Rgba,
    context: Option<LengthContext>,
) -> Option<Arc<[BackgroundImage]>> {
    top_level_split(raw, b',', MAX_BACKGROUND_LAYERS)?
        .into_iter()
        .map(|part| background_image_part(part, current_color, context, 0, false))
        .collect::<Option<Vec<_>>>()
        .map(Into::into)
}

fn resolve_css_url(reference: &str, base: Option<&str>) -> Option<Arc<str>> {
    match base {
        Some(base) => lumen_common::url::parse(reference, Some(base))
            .ok()
            .map(|url| Arc::from(url.href())),
        None => Some(Arc::from(reference)),
    }
}

fn resolve_background_image(image: &BackgroundImage, base: Option<&str>) -> BackgroundImage {
    match image {
        BackgroundImage::Url(reference) => resolve_css_url(reference, base)
            .map(BackgroundImage::Url)
            .unwrap_or(BackgroundImage::None),
        BackgroundImage::UrlResolution { url, density } => resolve_css_url(url, base)
            .map(|url| BackgroundImage::UrlResolution {
                url,
                density: *density,
            })
            .unwrap_or(BackgroundImage::None),
        BackgroundImage::CrossFade(images) => BackgroundImage::CrossFade(
            images
                .iter()
                .map(|(image, weight)| (resolve_background_image(image, base), *weight))
                .collect::<Vec<_>>()
                .into(),
        ),
        _ => image.clone(),
    }
}

fn resolve_background_images(
    images: &Arc<[BackgroundImage]>,
    base: Option<&str>,
) -> Arc<[BackgroundImage]> {
    images
        .iter()
        .map(|image| resolve_background_image(image, base))
        .collect::<Vec<_>>()
        .into()
}

fn resolve_generated_content_items(
    items: &Arc<[GeneratedContentItem]>,
    base: Option<&str>,
) -> Arc<[GeneratedContentItem]> {
    items
        .iter()
        .map(|item| match item {
            GeneratedContentItem::Url(reference) => GeneratedContentItem::Url(
                resolve_css_url(reference, base).unwrap_or_else(|| reference.clone()),
            ),
            GeneratedContentItem::AlternativeText(alternative) => {
                GeneratedContentItem::AlternativeText(resolve_generated_content_items(
                    alternative,
                    base,
                ))
            }
            item => item.clone(),
        })
        .collect::<Vec<_>>()
        .into()
}

fn background_positions(
    raw: &str,
    context: Option<LengthContext>,
) -> Option<Arc<[[LengthPercentage; 2]]>> {
    let context = context.unwrap_or(static_length_context());
    let mut positions = Vec::new();
    for part in top_level_split(raw, b',', MAX_BACKGROUND_LAYERS)? {
        positions.push(background_position(&components(part)?, context)?);
    }
    if positions
        .iter()
        .all(|v| *v == [LengthPercentage::default(); 2])
    {
        return None;
    }
    Some(positions.into())
}

fn parse_background_size(part: &str, context: LengthContext) -> Option<BackgroundSize> {
    let tokens = components(part)?;
    let value = |token: &str| -> Option<Option<LengthPercentage>> {
        if token.eq_ignore_ascii_case("auto") {
            Some(None)
        } else {
            let value = background_length(token, context)?;
            (value.pixels >= 0.0 && value.fraction >= 0.0).then_some(Some(value))
        }
    };
    match tokens.len() {
        1 if tokens[0].eq_ignore_ascii_case("cover") => Some(BackgroundSize {
            kind: BackgroundSizeKind::Cover,
            width: None,
            height: None,
        }),
        1 if tokens[0].eq_ignore_ascii_case("contain") => Some(BackgroundSize {
            kind: BackgroundSizeKind::Contain,
            width: None,
            height: None,
        }),
        1 => Some(BackgroundSize {
            kind: BackgroundSizeKind::Explicit,
            width: value(tokens[0])?,
            height: None,
        }),
        2 => Some(BackgroundSize {
            kind: BackgroundSizeKind::Explicit,
            width: value(tokens[0])?,
            height: value(tokens[1])?,
        }),
        _ => None,
    }
}

fn background_sizes(raw: &str, context: Option<LengthContext>) -> Option<Arc<[BackgroundSize]>> {
    let context = context.unwrap_or(static_length_context());
    let mut sizes = Vec::new();
    for part in top_level_split(raw, b',', MAX_BACKGROUND_LAYERS)? {
        sizes.push(parse_background_size(part, context)?);
    }
    if sizes
        .iter()
        .all(|v| v.kind == BackgroundSizeKind::Explicit && v.width.is_none() && v.height.is_none())
    {
        return None;
    }
    Some(sizes.into())
}

fn parse_background_repeat_axis(token: &str) -> Option<BackgroundRepeat> {
    match &*ascii_lower(token) {
        "repeat" => Some(BackgroundRepeat::Repeat),
        "no-repeat" => Some(BackgroundRepeat::NoRepeat),
        "space" => Some(BackgroundRepeat::Space),
        "round" => Some(BackgroundRepeat::Round),
        _ => None,
    }
}

fn background_repeats(raw: &str) -> Option<Arc<[[BackgroundRepeat; 2]]>> {
    let mut repeats = Vec::new();
    for part in top_level_split(raw, b',', MAX_BACKGROUND_LAYERS)? {
        repeats.push(background_repeat_pair(&components(part)?)?);
    }
    Some(repeats.into())
}

fn background_repeat_pair(tokens: &[&str]) -> Option<[BackgroundRepeat; 2]> {
    match tokens {
        [token] if token.eq_ignore_ascii_case("repeat-x") => {
            Some([BackgroundRepeat::Repeat, BackgroundRepeat::NoRepeat])
        }
        [token] if token.eq_ignore_ascii_case("repeat-y") => {
            Some([BackgroundRepeat::NoRepeat, BackgroundRepeat::Repeat])
        }
        [token] => {
            let axis = parse_background_repeat_axis(token)?;
            Some([axis, axis])
        }
        [x, y] => Some([
            parse_background_repeat_axis(x)?,
            parse_background_repeat_axis(y)?,
        ]),
        _ => None,
    }
}

fn parse_background_box(token: &str) -> Option<BackgroundBox> {
    match &*ascii_lower(token) {
        "border-box" => Some(BackgroundBox::Border),
        "padding-box" => Some(BackgroundBox::Padding),
        "content-box" => Some(BackgroundBox::Content),
        _ => None,
    }
}

fn background_boxes(raw: &str) -> Option<Arc<[BackgroundBox]>> {
    let mut boxes = Vec::new();
    for part in top_level_split(raw, b',', MAX_BACKGROUND_LAYERS)? {
        let tokens = components(part)?;
        boxes.push(match tokens.len() {
            1 => parse_background_box(tokens[0])?,
            _ => return None,
        });
    }
    Some(boxes.into())
}

fn background_clip_boxes(raw: &str) -> Option<Arc<[BackgroundBox]>> {
    let mut boxes = Vec::new();
    for part in top_level_split(raw, b',', MAX_BACKGROUND_LAYERS)? {
        let tokens = components(part)?;
        let value = match tokens.as_slice() {
            [token] if token.eq_ignore_ascii_case("text") => BackgroundBox::Text,
            [token] if token.eq_ignore_ascii_case("border-area") => BackgroundBox::BorderArea,
            [a, b]
                if (a.eq_ignore_ascii_case("text") && b.eq_ignore_ascii_case("border-area"))
                    || (b.eq_ignore_ascii_case("text")
                        && a.eq_ignore_ascii_case("border-area")) =>
            {
                BackgroundBox::BorderAreaText
            }
            [token] => parse_background_box(token)?,
            _ => return None,
        };
        boxes.push(value);
    }
    Some(boxes.into())
}

/// A parsed `background` shorthand: comma-joined image/position/size layer
/// text (resolved against the computed font later when it has relative
/// lengths), typed repeat/box lists and the optional final-layer color.
struct BackgroundShorthand {
    images: String,
    all_images_none: bool,
    positions: String,
    sizes: String,
    repeats: Option<Vec<[BackgroundRepeat; 2]>>,
    clips: Vec<BackgroundBox>,
    origins: Vec<BackgroundBox>,
    attachments: Vec<BackgroundAttachment>,
    color: Option<Rgba>,
}

fn static_length_context() -> LengthContext {
    LengthContext {
        font: 16.0,
        root_font: 16.0,
        ex: 8.0,
        ch: 8.0,
        viewport: MediaEnvironment::default(),
        percent: None,
    }
}

fn is_background_position_token(token: &str) -> bool {
    matches!(
        &*ascii_lower(token),
        "left" | "right" | "top" | "bottom" | "center"
    ) || background_length(token, static_length_context()).is_some()
}

fn is_background_function(token: &str) -> bool {
    let function = token.split_once('(').map_or("", |(name, _)| name);
    matches!(
        &*ascii_lower(function),
        "linear-gradient"
            | "repeating-linear-gradient"
            | "radial-gradient"
            | "repeating-radial-gradient"
            | "conic-gradient"
            | "repeating-conic-gradient"
            | "image-set"
            | "-webkit-image-set"
            | "image"
            | "light-dark"
            | "cross-fade"
    )
}

/// Parses the full `background` shorthand per layer. Unsupported components
/// reject the whole value so the declaration is
/// dropped rather than silently ignored.
fn background_shorthand(raw: &str) -> Option<BackgroundShorthand> {
    let layers = top_level_split(raw, b',', MAX_BACKGROUND_LAYERS)?;
    let mut shorthand = BackgroundShorthand {
        images: String::new(),
        all_images_none: true,
        positions: String::new(),
        sizes: String::new(),
        repeats: Some(Vec::with_capacity(layers.len())),
        clips: Vec::with_capacity(layers.len()),
        origins: Vec::with_capacity(layers.len()),
        attachments: Vec::with_capacity(layers.len()),
        color: None,
    };
    let last = layers.len() - 1;
    let mut size = String::new();
    for (index, layer) in layers.iter().enumerate() {
        if index > 0 {
            shorthand.images.push(',');
            shorthand.positions.push(',');
            shorthand.sizes.push(',');
        }
        let mut image = "none";
        let position_start = shorthand.positions.len();
        size.clear();
        let mut repeat = [""; 3];
        let mut repeat_count = 0;
        let mut origin = BackgroundBox::Padding;
        let mut clip = BackgroundBox::Border;
        let mut visual_boxes = 0;
        let mut special_clip = false;
        let mut layer_color = None;
        let mut attachment = None;
        // `position[/size]` is one contiguous run inside the layer; other
        // components may follow the size.
        let mut in_size = false;
        let mut size_done = false;
        let mut position_started = false;
        let mut position_finished = false;
        let push_position = |positions: &mut String, token: &str| {
            if positions.len() > position_start {
                positions.push(' ');
            }
            positions.push_str(token);
        };
        for token in components(layer)? {
            let slash_parts = top_level_split(token, b'/', 2)?;
            if in_size {
                let is_size = token.eq_ignore_ascii_case("auto")
                    || token.eq_ignore_ascii_case("cover")
                    || token.eq_ignore_ascii_case("contain")
                    || background_length(token, static_length_context()).is_some();
                if is_size && !size_done {
                    if size.is_empty() {
                        size.push_str(token);
                    } else {
                        size.push(' ');
                        size.push_str(token);
                        size_done = true;
                    }
                    continue;
                }
                in_size = false;
                size_done = true;
            }
            if !size_done && slash_parts.len() == 2 {
                if position_finished {
                    return None;
                }
                let (pre, post) = (slash_parts[0], slash_parts[1]);
                if !pre.is_empty() {
                    if !is_background_position_token(pre) {
                        return None;
                    }
                    push_position(&mut shorthand.positions, pre);
                }
                if pre.is_empty() && !position_started {
                    return None;
                }
                position_started = true;
                if !post.is_empty() {
                    size.clear();
                    size.push_str(post);
                    size_done = true;
                } else {
                    in_size = true;
                }
                continue;
            }
            let position_token = is_background_position_token(token);
            if position_token {
                if position_finished || size_done {
                    return None;
                }
                position_started = true;
            } else if position_started {
                position_finished = true;
            }
            if token.eq_ignore_ascii_case("none") || background_url(token).is_some() {
                if image != "none" {
                    return None;
                }
                image = token;
            } else if matches!(
                &*ascii_lower(token),
                "repeat" | "space" | "round" | "no-repeat" | "repeat-x" | "repeat-y"
            ) {
                if let Some(slot) = repeat.get_mut(repeat_count) {
                    *slot = token;
                }
                repeat_count += 1;
            } else if token.eq_ignore_ascii_case("text")
                || token.eq_ignore_ascii_case("border-area")
            {
                if visual_boxes > 1 {
                    return None;
                }
                let value = if token.eq_ignore_ascii_case("text") {
                    BackgroundBox::Text
                } else {
                    BackgroundBox::BorderArea
                };
                if special_clip {
                    clip = match (clip, value) {
                        (BackgroundBox::Text, BackgroundBox::BorderArea)
                        | (BackgroundBox::BorderArea, BackgroundBox::Text) => {
                            BackgroundBox::BorderAreaText
                        }
                        _ => return None,
                    };
                } else {
                    clip = value;
                    special_clip = true;
                }
            } else if let Some(box_value) = parse_background_box(token) {
                visual_boxes += 1;
                if visual_boxes > 2 || special_clip && visual_boxes > 1 {
                    return None;
                }
                if visual_boxes == 1 {
                    origin = box_value;
                    if !special_clip {
                        clip = box_value;
                    }
                } else {
                    clip = box_value;
                }
            } else if matches!(&*ascii_lower(token), "scroll" | "fixed" | "local") {
                if attachment.is_some() {
                    return None;
                }
                attachment = Some(match &*ascii_lower(token) {
                    "fixed" => BackgroundAttachment::Fixed,
                    "local" => BackgroundAttachment::Local,
                    _ => BackgroundAttachment::Scroll,
                });
            } else if let Some(color) = if token.eq_ignore_ascii_case("currentcolor") {
                Some(Style::initial().color)
            } else {
                color(token)
            } {
                if index != last || layer_color.is_some() {
                    return None;
                }
                layer_color = Some(color);
            } else if slash_parts.len() == 2 {
                // A second size separator has no grammar position here.
                return None;
            } else if token.contains('(') && !position_token {
                // Not a color, so any remaining function must be a gradient.
                if !is_background_function(token) || image != "none" {
                    return None;
                }
                image = token;
            } else if is_background_position_token(token) {
                push_position(&mut shorthand.positions, token);
            } else {
                return None;
            }
        }
        if size.is_empty() {
            shorthand.sizes.push_str("auto");
        } else {
            parse_background_size(&size, static_length_context())?;
            shorthand.sizes.push_str(&size);
        }
        shorthand.images.push_str(image);
        shorthand.all_images_none &= image == "none";
        if shorthand.positions.len() == position_start {
            shorthand.positions.push_str("0% 0%");
        }
        let pair = if repeat_count == 0 {
            Some([BackgroundRepeat::Repeat; 2])
        } else if repeat_count > repeat.len() {
            None
        } else {
            background_repeat_pair(&repeat[..repeat_count])
        };
        match (pair, &mut shorthand.repeats) {
            (Some(pair), Some(repeats)) => repeats.push(pair),
            (_, repeats) => *repeats = None,
        }
        shorthand.origins.push(origin);
        shorthand.attachments.push(attachment.unwrap_or_default());
        shorthand.clips.push(clip);
        if layer_color.is_some() {
            shorthand.color = layer_color;
        }
    }
    Some(shorthand)
}

fn components(raw: &str) -> Option<Vec<&str>> {
    components_bounded(raw, MAX_VARIABLE_BYTES)
}

fn components_bounded(raw: &str, max_bytes: usize) -> Option<Vec<&str>> {
    if raw.len() > max_bytes {
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

fn generated_content_function(token: &str) -> Option<(&str, &str)> {
    let open = token.find('(')?;
    let name = token[..open].trim();
    if !is_part_identifier(name) {
        return None;
    }
    let end = matching_css_block(token, open)?;
    (end == token.len()).then_some((&token[..open], &token[open + 1..end - 1]))
}

fn parse_counter_style(raw: &str) -> Option<Arc<str>> {
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("none") {
        return Some(Arc::from("none"));
    }
    if let Some((function, arguments)) = generated_content_function(raw) {
        if !function.eq_ignore_ascii_case("symbols") {
            return None;
        }
        let parts = components_bounded(arguments, MAX_GENERATED_CONTENT_BYTES)?;
        if parts.is_empty() || parts.len() > MAX_GENERATED_CONTENT_ITEMS + 1 {
            return None;
        }
        let start = parts.first().is_some_and(|part| {
            ["cyclic", "numeric", "alphabetic", "symbolic", "fixed"]
                .iter()
                .any(|system| part.eq_ignore_ascii_case(system))
                || part.eq_ignore_ascii_case("extends")
        });
        let symbols = if start { &parts[1..] } else { &parts[..] };
        if symbols.is_empty()
            || symbols
                .iter()
                .any(|part| css_string(part).is_none() && parse_counter_ident(part).is_none())
        {
            return None;
        }
        return Some(Arc::from(alloc::format!("symbols({})", arguments.trim())));
    }
    parse_counter_ident(raw)
}

fn generated_content_item(token: &str) -> Option<GeneratedContentItem> {
    if token.starts_with(['\'', '"']) {
        return css_string(token).map(|value| GeneratedContentItem::String(Arc::from(value)));
    }
    if let Some(url) = background_url(token) {
        return Some(GeneratedContentItem::Url(url));
    }
    match token.to_ascii_lowercase().as_str() {
        "open-quote" => return Some(GeneratedContentItem::OpenQuote),
        "close-quote" => return Some(GeneratedContentItem::CloseQuote),
        "no-open-quote" => return Some(GeneratedContentItem::NoOpenQuote),
        "no-close-quote" => return Some(GeneratedContentItem::NoCloseQuote),
        _ => {}
    }
    let (name, arguments) = generated_content_function(token)?;
    let name = name.to_ascii_lowercase();
    match name.as_str() {
        "attr" => {
            let parts = top_level_split_bounded(arguments, b',', 2, MAX_GENERATED_CONTENT_BYTES)?;
            if !(1..=2).contains(&parts.len()) || !is_part_identifier(parts[0]) {
                return Some(GeneratedContentItem::UnsupportedFunction {
                    name: name.into(),
                    arguments: arguments.into(),
                });
            }
            let fallback = match parts.get(1) {
                None => None,
                Some(raw) => match css_string(raw) {
                    Some(value) => Some(Arc::from(value)),
                    None => {
                        return Some(GeneratedContentItem::UnsupportedFunction {
                            name: name.into(),
                            arguments: arguments.into(),
                        });
                    }
                },
            };
            Some(GeneratedContentItem::Attribute {
                name: parts[0].into(),
                fallback,
            })
        }
        "counter" => {
            let parts = top_level_split_bounded(arguments, b',', 3, MAX_GENERATED_CONTENT_BYTES)?;
            if !(1..=2).contains(&parts.len()) {
                return None;
            }
            let name = parse_counter_ident(parts[0].trim())?;
            let style = parts.get(1).copied().unwrap_or("decimal").trim();
            Some(GeneratedContentItem::Counter {
                name,
                style: parse_counter_style(style)?,
            })
        }
        "counters" => {
            let parts = top_level_split_bounded(arguments, b',', 4, MAX_GENERATED_CONTENT_BYTES)?;
            if !(2..=3).contains(&parts.len()) {
                return None;
            }
            let name = parse_counter_ident(parts[0].trim())?;
            let Some(separator) = css_string(parts[1]) else {
                return None;
            };
            let style = parts.get(2).copied().unwrap_or("decimal").trim();
            Some(GeneratedContentItem::Counters {
                name,
                separator: separator.into(),
                style: parse_counter_style(style)?,
            })
        }
        // Preserve functions which can contribute generated images or text;
        // the renderer can report them as unsupported without inventing ink.
        "image" | "image-set" | "cross-fade" | "element" | "leader" | "target-counter"
        | "target-counters" | "target-text" => Some(GeneratedContentItem::UnsupportedFunction {
            name: name.into(),
            arguments: arguments.into(),
        }),
        _ => Some(GeneratedContentItem::UnsupportedFunction {
            name: name.into(),
            arguments: arguments.into(),
        }),
    }
}

fn generated_content_items(raw: &str) -> Option<Vec<GeneratedContentItem>> {
    let tokens = components_bounded(raw, MAX_GENERATED_CONTENT_BYTES)?;
    if tokens.is_empty() || tokens.len() > MAX_GENERATED_CONTENT_ITEMS {
        return None;
    }
    tokens
        .into_iter()
        .map(generated_content_item)
        .collect::<Option<Vec<_>>>()
}

fn parse_generated_content(raw: &str) -> Option<GeneratedContent> {
    if raw.len() > MAX_GENERATED_CONTENT_BYTES {
        return None;
    }
    let raw = strip_selector_comments(raw, 0).ok()?;
    let sections = top_level_split_bounded(&raw, b'/', 2, MAX_GENERATED_CONTENT_BYTES)?;
    if sections.len() == 1 {
        let value = sections[0].trim();
        if value.eq_ignore_ascii_case("normal") {
            return Some(GeneratedContent::Normal);
        }
        if value.eq_ignore_ascii_case("none") {
            return Some(GeneratedContent::None);
        }
    }
    let mut items = generated_content_items(sections[0])?;
    if sections.len() == 2 {
        let alternative = generated_content_items(sections[1])?;
        if alternative.iter().any(|item| {
            !matches!(
                item,
                GeneratedContentItem::String(_)
                    | GeneratedContentItem::Attribute { .. }
                    | GeneratedContentItem::Counter { .. }
                    | GeneratedContentItem::Counters { .. }
            )
        }) {
            return None;
        }
        if items.len().saturating_add(alternative.len()) + 1 > MAX_GENERATED_CONTENT_ITEMS {
            return None;
        }
        items.push(GeneratedContentItem::AlternativeText(alternative.into()));
    }
    Some(GeneratedContent::Items(items.into()))
}

const MAX_COUNTER_DIRECTIVES: usize = 64;
const MAX_COUNTER_NAME_BYTES: usize = 256;

fn parse_counter_ident(raw: &str) -> Option<Arc<str>> {
    let mut position = 0;
    let name = consume_selector_identifier(raw, &mut position)?;
    if position != raw.len()
        || name.is_empty()
        || name.len() > MAX_COUNTER_NAME_BYTES
        || [
            "none",
            "default",
            "initial",
            "inherit",
            "unset",
            "revert",
            "revert-layer",
        ]
        .iter()
        .any(|keyword| name.eq_ignore_ascii_case(keyword))
    {
        return None;
    }
    Some(Arc::from(name))
}

struct ParsedCounterInteger {
    value: i64,
    math_expression: Option<Arc<str>>,
}

fn parse_counter_integer(raw: &str) -> Option<ParsedCounterInteger> {
    if raw
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("calc("))
    {
        // Reuse the CSS numeric parser, but do not freeze viewport/container
        // or font-relative expressions before their computed-value context.
        let (value, context_dependent) = css_scalar_with_context(raw, false)?;
        if context_dependent {
            return None;
        }
        if !value.is_finite() {
            return None;
        }
        // CSS integer-valued math rounds to the nearest integer, with ties
        // toward positive infinity (`-2.5` therefore becomes `-2`).
        let rounded = (f64::from(value) + 0.5).floor();
        let inner = raw.get(5..)?.strip_suffix(')')?.trim();
        return Some(ParsedCounterInteger {
            value: rounded.clamp(-(MAX_COUNTER_VALUE as f64), MAX_COUNTER_VALUE as f64) as i64,
            math_expression: Some(Arc::from(alloc::format!("calc({inner})"))),
        });
    }

    let bytes = raw.as_bytes();
    let (negative, digits) = match bytes.first()? {
        b'+' => (false, &bytes[1..]),
        b'-' => (true, &bytes[1..]),
        _ => (false, bytes),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let magnitude = digits.iter().fold(0i64, |value, digit| {
        value
            .saturating_mul(10)
            .saturating_add((*digit - b'0') as i64)
            .min(MAX_COUNTER_VALUE)
    });
    Some(ParsedCounterInteger {
        value: if negative { -magnitude } else { magnitude },
        math_expression: None,
    })
}

/// Parse the counter property value grammar. `None` means the CSS keyword;
/// `Some(list)` contains typed, bounded directives.
fn parse_counter_directives(
    raw: &str,
    allow_reversed: bool,
    default_value: i64,
) -> Option<Option<Arc<[CounterDirective]>>> {
    if raw.len() > MAX_GENERATED_CONTENT_BYTES {
        return None;
    }
    let raw = strip_selector_comments(raw, 0).ok()?;
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("none") {
        return Some(None);
    }
    let parts = components_bounded(raw, MAX_GENERATED_CONTENT_BYTES)?;
    if parts.is_empty() || parts.len() > MAX_COUNTER_DIRECTIVES * 2 {
        return None;
    }
    let mut directives = Vec::new();
    directives.try_reserve_exact(parts.len()).ok()?;
    let mut index = 0;
    while index < parts.len() {
        let token = parts[index];
        let (name, reversed) = if allow_reversed {
            if let Some((function, arguments)) = generated_content_function(token) {
                if function.eq_ignore_ascii_case("reversed") {
                    (parse_counter_ident(arguments.trim())?, true)
                } else {
                    return None;
                }
            } else {
                (parse_counter_ident(token)?, false)
            }
        } else {
            (parse_counter_ident(token)?, false)
        };
        let parsed_value = parts
            .get(index + 1)
            .and_then(|value| parse_counter_integer(value));
        let explicit_value = parsed_value.is_some();
        let value = parsed_value
            .as_ref()
            .map_or(default_value, |parsed| parsed.value);
        let math_expression = parsed_value.and_then(|parsed| parsed.math_expression);
        directives.push(CounterDirective {
            name,
            value,
            reversed,
            explicit_value,
            math_expression,
        });
        index += 1 + usize::from(explicit_value);
    }
    if directives.is_empty() || directives.len() > MAX_COUNTER_DIRECTIVES {
        return None;
    }
    Some(Some(directives.into()))
}

/// Parse a `counter-reset`, `counter-increment`, or `counter-set` declaration.
/// The nested option distinguishes an invalid value (`None`) from the valid
/// `none` keyword (`Some(None)`).
pub fn parse_counter_declaration(
    property: CounterProperty,
    raw: &str,
) -> Option<Option<Arc<[CounterDirective]>>> {
    parse_counter_directives(raw, property.allows_reversed(), property.default_value())
}

const MAX_QUOTES_PAIRS: usize = 16;

fn parse_quotes(raw: &str) -> Option<Quotes> {
    if raw.len() > MAX_GENERATED_CONTENT_BYTES {
        return None;
    }
    let raw = strip_selector_comments(raw, 0).ok()?;
    if raw.eq_ignore_ascii_case("auto") {
        return Some(Quotes::Auto);
    }
    if raw.eq_ignore_ascii_case("none") {
        return Some(Quotes::None);
    }
    if raw.eq_ignore_ascii_case("match-parent") {
        return Some(Quotes::MatchParent);
    }
    let parts = components_bounded(&raw, MAX_GENERATED_CONTENT_BYTES)?;
    if parts.is_empty() || parts.len() > MAX_QUOTES_PAIRS * 2 || parts.len() % 2 != 0 {
        return None;
    }
    let mut pairs = Vec::new();
    pairs.try_reserve_exact(parts.len() / 2).ok()?;
    for pair in parts.chunks_exact(2) {
        pairs.push((
            Arc::from(css_string(pair[0])?),
            Arc::from(css_string(pair[1])?),
        ));
    }
    Some(Quotes::Pairs(pairs.into()))
}

fn serialize_css_identifier(value: &str) -> String {
    let mut serialized = String::new();
    for (index, character) in value.chars().enumerate() {
        let leading_digit = index == 0 && character.is_ascii_digit();
        let second_digit_after_hyphen =
            index == 1 && value.starts_with('-') && character.is_ascii_digit();
        if character == '\0' {
            serialized.push('\u{fffd}');
        } else if character.is_ascii_control()
            || character == '\u{7f}'
            || leading_digit
            || second_digit_after_hyphen
        {
            serialized.push_str(&alloc::format!("\\{:x} ", character as u32));
        } else if character.is_ascii_alphanumeric()
            || matches!(character, '-' | '_')
            || !character.is_ascii()
        {
            serialized.push(character);
        } else {
            serialized.push('\\');
            serialized.push(character);
        }
    }
    serialized
}

fn append_counter_serialized(output: &mut String, value: &str) -> Option<()> {
    let end = output.len().checked_add(value.len())?;
    if end > MAX_GENERATED_CONTENT_BYTES {
        return None;
    }
    output.try_reserve(value.len()).ok()?;
    output.push_str(value);
    Some(())
}

/// Serialize parsed counter directives with CSSOM's canonical identifier and
/// integer forms. Ordinary directives always serialize their integer; a
/// reversed reset omits its integer only when it was omitted in the source.
/// Absent lists serialize as `none`.
pub fn serialize_counter_directives(
    property: CounterProperty,
    directives: Option<&[CounterDirective]>,
) -> Option<String> {
    serialize_counter_directives_with_math(property, directives, false)
}

fn serialize_counter_directives_with_math(
    property: CounterProperty,
    directives: Option<&[CounterDirective]>,
    preserve_math: bool,
) -> Option<String> {
    let Some(directives) = directives.filter(|directives| !directives.is_empty()) else {
        return Some(String::from("none"));
    };
    if directives.len() > MAX_COUNTER_DIRECTIVES {
        return None;
    }

    let mut serialized = String::new();
    for (index, directive) in directives.iter().enumerate() {
        if directive.reversed && !property.allows_reversed() {
            return None;
        }
        if directive.name.is_empty() || directive.name.len() > MAX_COUNTER_NAME_BYTES {
            return None;
        }
        let name = serialize_css_identifier(&directive.name);
        // CounterDirective is public, so validate externally constructed
        // values as well as values produced by the declaration parser.
        if parse_counter_ident(&name).as_deref() != Some(directive.name.as_ref()) {
            return None;
        }

        if index != 0 {
            append_counter_serialized(&mut serialized, " ")?;
        }
        if directive.reversed {
            append_counter_serialized(&mut serialized, "reversed(")?;
            append_counter_serialized(&mut serialized, &name)?;
            append_counter_serialized(&mut serialized, ")")?;
        } else {
            append_counter_serialized(&mut serialized, &name)?;
        }

        if preserve_math {
            if let Some(expression) = &directive.math_expression {
                if !directive.explicit_value {
                    return None;
                }
                append_counter_serialized(&mut serialized, " ")?;
                append_counter_serialized(&mut serialized, expression)?;
                continue;
            }
        }

        if !directive.reversed || directive.explicit_value {
            let value = directive.value.clamp(-MAX_COUNTER_VALUE, MAX_COUNTER_VALUE);
            append_counter_serialized(&mut serialized, " ")?;
            append_counter_serialized(&mut serialized, &alloc::format!("{value}"))?;
        }
    }
    Some(serialized)
}

/// Parse and canonically serialize one counter declaration for CSSOM.
pub fn serialize_counter_declaration(property: CounterProperty, raw: &str) -> Option<String> {
    let directives = parse_counter_declaration(property, raw)?;
    serialize_counter_directives_with_math(property, directives.as_deref(), true)
}

fn serialize_counter_style(value: &str) -> String {
    if let Some((function, arguments)) = generated_content_function(value) {
        if function.eq_ignore_ascii_case("symbols") {
            return alloc::format!("symbols({})", arguments.trim());
        }
    }
    match value.to_ascii_lowercase().as_str() {
        "decimal"
        | "decimal-leading-zero"
        | "lower-roman"
        | "upper-roman"
        | "lower-alpha"
        | "upper-alpha"
        | "lower-latin"
        | "upper-latin" => value.to_ascii_lowercase(),
        _ => serialize_css_identifier(value),
    }
}

fn append_serialized_generated_content_items(
    serialized: &mut String,
    items: &[GeneratedContentItem],
) {
    let mut section_has_item = false;
    for item in items {
        if let GeneratedContentItem::AlternativeText(alternative) = item {
            serialized.push_str(" / ");
            append_serialized_generated_content_items(serialized, alternative);
            section_has_item = false;
            continue;
        }
        if section_has_item {
            serialized.push(' ');
        }
        match item {
            GeneratedContentItem::String(value) => {
                serialized.push_str(&serialize_css_string(value));
            }
            GeneratedContentItem::Attribute { name, fallback } => {
                serialized.push_str("attr(");
                serialized.push_str(&serialize_css_identifier(name));
                if let Some(fallback) = fallback {
                    serialized.push_str(", ");
                    serialized.push_str(&serialize_css_string(fallback));
                }
                serialized.push(')');
            }
            GeneratedContentItem::Url(value) => {
                serialized.push_str("url(");
                serialized.push_str(&serialize_css_string(value));
                serialized.push(')');
            }
            GeneratedContentItem::Counter { name, style } => {
                serialized.push_str("counter(");
                serialized.push_str(&serialize_css_identifier(name));
                if !style.eq_ignore_ascii_case("decimal") {
                    serialized.push_str(", ");
                    serialized.push_str(&serialize_counter_style(style));
                }
                serialized.push(')');
            }
            GeneratedContentItem::Counters {
                name,
                separator,
                style,
            } => {
                serialized.push_str("counters(");
                serialized.push_str(&serialize_css_identifier(name));
                serialized.push_str(", ");
                serialized.push_str(&serialize_css_string(separator));
                if !style.eq_ignore_ascii_case("decimal") {
                    serialized.push_str(", ");
                    serialized.push_str(&serialize_counter_style(style));
                }
                serialized.push(')');
            }
            GeneratedContentItem::OpenQuote => serialized.push_str("open-quote"),
            GeneratedContentItem::CloseQuote => serialized.push_str("close-quote"),
            GeneratedContentItem::NoOpenQuote => serialized.push_str("no-open-quote"),
            GeneratedContentItem::NoCloseQuote => serialized.push_str("no-close-quote"),
            GeneratedContentItem::UnsupportedFunction { name, arguments } => {
                serialized.push_str(&serialize_css_identifier(name));
                serialized.push('(');
                serialized.push_str(arguments.trim());
                serialized.push(')');
            }
            GeneratedContentItem::AlternativeText(_) => unreachable!(),
        }
        section_has_item = true;
    }
}

fn serialize_generated_content_items(items: &[GeneratedContentItem]) -> String {
    let mut serialized = String::new();
    append_serialized_generated_content_items(&mut serialized, items);
    serialized
}

/// Canonically serialize a parsed/computed `content` value for CSSOM.
pub fn serialize_generated_content(value: &GeneratedContent) -> String {
    match value {
        GeneratedContent::Normal => String::from("normal"),
        GeneratedContent::None => String::from("none"),
        GeneratedContent::Items(items) => serialize_generated_content_items(items),
    }
}

/// Parse and canonically serialize a CSS `content` declaration value.
pub fn serialize_content_declaration(raw: &str) -> Option<String> {
    parse_generated_content(raw).map(|value| serialize_generated_content(&value))
}

/// Canonically serialize a parsed/computed `quotes` value for CSSOM.
pub fn serialize_quotes(value: &Quotes) -> String {
    match value {
        Quotes::Auto => String::from("auto"),
        Quotes::None => String::from("none"),
        Quotes::MatchParent => String::from("match-parent"),
        Quotes::Pairs(pairs) => {
            let mut serialized = String::new();
            for (open, close) in pairs.iter() {
                if !serialized.is_empty() {
                    serialized.push(' ');
                }
                serialized.push_str(&serialize_css_string(open));
                serialized.push(' ');
                serialized.push_str(&serialize_css_string(close));
            }
            serialized
        }
    }
}

/// Parse and canonically serialize a CSS `quotes` declaration value.
pub fn serialize_quotes_declaration(raw: &str) -> Option<String> {
    parse_quotes(raw).map(|value| serialize_quotes(&value))
}

/// Canonically serialize CSSOM property values whose parsed representation is
/// shared between inline and stylesheet declarations.
pub fn serialize_cssom_property_value(name: &str, value: &str) -> Option<String> {
    if name.eq_ignore_ascii_case("content") {
        serialize_content_declaration(value)
    } else if name.eq_ignore_ascii_case("quotes") {
        serialize_quotes_declaration(value)
    } else if name.eq_ignore_ascii_case("counter-reset") {
        serialize_counter_declaration(CounterProperty::Reset, value)
    } else if name.eq_ignore_ascii_case("counter-increment") {
        serialize_counter_declaration(CounterProperty::Increment, value)
    } else if name.eq_ignore_ascii_case("counter-set") {
        serialize_counter_declaration(CounterProperty::Set, value)
    } else if name.eq_ignore_ascii_case("color-scheme") {
        serialize_color_scheme_declaration(value)
    } else if name.eq_ignore_ascii_case("transition-duration")
        || name.eq_ignore_ascii_case("transition-delay")
    {
        serialize_transition_time_declaration(name, value)
    } else if name.eq_ignore_ascii_case("opacity") {
        value
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .map(|value| value.to_string())
    } else {
        None
    }
}

fn font_weight(raw: &str) -> Option<i16> {
    match raw {
        "normal" => Some(400),
        "bold" => Some(700),
        "bolder" => Some(-1),
        "lighter" => Some(-2),
        _ => raw.parse::<i16>().ok().filter(|v| (1..=1000).contains(v)),
    }
}

fn relative_font_weight(value: i16, inherited: u16) -> u16 {
    match value {
        -1 => {
            if inherited < 350 {
                400
            } else if inherited < 550 {
                700
            } else {
                900
            }
        }
        -2 => {
            if inherited < 550 {
                100
            } else if inherited < 750 {
                400
            } else {
                700
            }
        }
        _ => value as u16,
    }
}

fn font_families(raw: &str) -> Option<Arc<[Arc<str>]>> {
    let mut families = Vec::new();
    for part in comma_components(raw, 16)? {
        let part = part.trim();
        let quote = part.as_bytes().first().copied()?;
        let quoted = matches!(quote, b'\'' | b'"');
        let value = if quoted {
            if part.len() < 2 || part.as_bytes().last() != Some(&quote) {
                return None;
            }
            &part[1..part.len() - 1]
        } else {
            part
        };
        let mut decoded = String::new();
        let mut pos = 0;
        let mut word_start = true;
        while pos < value.len() {
            let ch = value[pos..].chars().next()?;
            if ch == '\\' {
                if quoted
                    && value
                        .as_bytes()
                        .get(pos + 1)
                        .is_some_and(|b| matches!(b, b'\n' | b'\r' | b'\x0c'))
                {
                    pos += 2;
                    if value.as_bytes().get(pos - 1) == Some(&b'\r')
                        && value.as_bytes().get(pos) == Some(&b'\n')
                    {
                        pos += 1;
                    }
                    continue;
                }
                decoded.push(selector_escape(value, &mut pos)?);
                word_start = false;
                continue;
            }
            if matches!(ch, '\n' | '\r' | '\u{c}' | '\0') {
                return None;
            }
            if !quoted {
                if ch.is_ascii_whitespace() {
                    if !decoded.ends_with(' ') {
                        decoded.push(' ');
                    }
                    word_start = true;
                    pos += ch.len_utf8();
                    continue;
                }
                if !(ch.is_ascii_alphabetic()
                    || ch == '-'
                    || ch == '_'
                    || !ch.is_ascii()
                    || (!word_start && ch.is_ascii_digit()))
                {
                    return None;
                }
                if word_start && ch == '-' {
                    let next = value[pos + 1..].chars().next()?;
                    if !(next.is_ascii_alphabetic()
                        || matches!(next, '-' | '_' | '\\')
                        || !next.is_ascii())
                    {
                        return None;
                    }
                }
                word_start = false;
            } else if ch as u32 == u32::from(quote) {
                return None;
            }
            decoded.push(ch);
            pos += ch.len_utf8();
        }
        let decoded = if quoted {
            decoded.as_str()
        } else {
            decoded.trim()
        };
        if decoded.is_empty() && !quoted
            || decoded.len() > 256
            || (!quoted
                && [
                    "inherit",
                    "initial",
                    "unset",
                    "revert",
                    "revert-layer",
                    "default",
                ]
                .iter()
                .any(|word| decoded.eq_ignore_ascii_case(word)))
        {
            return None;
        }
        families.push(Arc::<str>::from(decoded));
    }
    (!families.is_empty()).then(|| families.into())
}

fn is_generic_font_family(family: &str) -> bool {
    matches!(
        &*ascii_lower(family),
        "serif"
            | "sans-serif"
            | "monospace"
            | "cursive"
            | "fantasy"
            | "system-ui"
            | "ui-serif"
            | "ui-sans-serif"
            | "ui-monospace"
            | "ui-rounded"
            | "emoji"
            | "math"
            | "fangsong"
            | "inherit"
            | "initial"
            | "unset"
            | "revert"
            | "revert-layer"
    )
}

fn can_serialize_unquoted_font_family(family: &str) -> bool {
    if is_generic_font_family(family)
        || family.trim() != family
        || family
            .split_ascii_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            != family
    {
        return false;
    }
    parse_font_family_list(family).is_some_and(|families| {
        families.len() == 1
            && families
                .first()
                .is_some_and(|parsed| parsed.as_ref() == family)
    })
}

/// CSS font width keywords and positive percentage values.
fn font_stretch(raw: &str) -> Option<f32> {
    Some(match &*ascii_lower(raw) {
        "ultra-condensed" => 50.0,
        "extra-condensed" => 62.5,
        "condensed" => 75.0,
        "semi-condensed" => 87.5,
        "normal" => 100.0,
        "semi-expanded" => 112.5,
        "expanded" => 125.0,
        "extra-expanded" => 150.0,
        "ultra-expanded" => 200.0,
        _ => raw
            .strip_suffix('%')?
            .parse::<f32>()
            .ok()
            .filter(|v| v.is_finite() && *v >= 0.0)?,
    })
}

fn font_stretch_keyword(raw: &str) -> Option<&'static str> {
    Some(match &*ascii_lower(raw) {
        "ultra-condensed" => "ultra-condensed",
        "extra-condensed" => "extra-condensed",
        "condensed" => "condensed",
        "semi-condensed" => "semi-condensed",
        "normal" => "normal",
        "semi-expanded" => "semi-expanded",
        "expanded" => "expanded",
        "extra-expanded" => "extra-expanded",
        "ultra-expanded" => "ultra-expanded",
        _ => return None,
    })
}

fn font_size_adjust(raw: &str) -> Option<Option<FontSizeAdjust>> {
    if raw.trim().eq_ignore_ascii_case("none") {
        return Some(None);
    }
    let parts = components(raw)?;
    if !(1..=2).contains(&parts.len()) {
        return None;
    }
    let metric = if parts.len() == 1 {
        FontMetric::ExHeight
    } else {
        match &*ascii_lower(parts[0]) {
            "ex-height" => FontMetric::ExHeight,
            "cap-height" => FontMetric::CapHeight,
            "ch-width" => FontMetric::ChWidth,
            "ic-width" => FontMetric::IcWidth,
            "ic-height" => FontMetric::IcHeight,
            _ => return None,
        }
    };
    let value = parts.last()?;
    let value = if value.eq_ignore_ascii_case("from-font") {
        FontSizeAdjustValue::FromFont
    } else {
        let number = css_scalar(value, false)?;
        if !number.is_finite() || (number < 0.0 && !math_function(value)) {
            return None;
        }
        FontSizeAdjustValue::Number(number.max(0.0))
    };
    Some(Some(FontSizeAdjust { metric, value }))
}

fn font_shorthand(raw: &str) -> Option<Vec<Value>> {
    let tokens = components(raw)?;
    let (mut style, mut weight, mut index) = (FontStyle::Normal, 400, 0);
    let (mut seen_style, mut seen_weight, mut seen_stretch) = (false, false, false);
    let mut stretch = 100.0;
    while let Some(&token) = tokens.get(index) {
        let keyword = ascii_lower(token);
        match &*keyword {
            "normal" => {
                index += 1;
                if index > 3 {
                    return None;
                }
            }
            "italic" | "oblique" if !seen_style => {
                style = if keyword == "italic" {
                    FontStyle::Italic
                } else {
                    FontStyle::Oblique
                };
                seen_style = true;
                index += 1;
            }
            _ if !seen_stretch && !token.ends_with('%') && font_stretch(token).is_some() => {
                stretch = font_stretch(token)?;
                seen_stretch = true;
                index += 1;
            }
            _ if !seen_weight && font_weight(&keyword).is_some() => {
                weight = font_weight(&keyword)?;
                seen_weight = true;
                index += 1;
            }
            _ => break,
        }
    }
    let size_and_height = top_level_split(*tokens.get(index)?, b'/', 2)?;
    let size = *size_and_height.first()?;
    let mut height = size_and_height.get(1).copied();
    index += 1;
    if height == Some("") {
        height = Some(*tokens.get(index)?);
        index += 1;
    } else if height.is_none()
        && tokens
            .get(index)
            .is_some_and(|token| token.starts_with('/'))
    {
        let token = tokens[index];
        index += 1;
        height = Some(if token == "/" {
            let h = *tokens.get(index)?;
            index += 1;
            h
        } else {
            &token[1..]
        });
    }
    let family_start = tokens.get(index)?.as_ptr() as usize - raw.as_ptr() as usize;
    let family = font_families(&raw[family_start..])?;
    // Reuse the longhand grammar/context handling, including relative sizes.
    let size = declarations(&alloc::format!("font-size:{size}"), 0).ok()?;
    let height = declarations(
        &alloc::format!("line-height:{}", height.unwrap_or("normal")),
        0,
    )
    .ok()?;
    if size.len() != 1 || height.len() != 1 {
        return None;
    }
    let mut out = alloc::vec![
        Value::FontStyle(style),
        Value::FontWeight(weight),
        Value::FontStretch(stretch),
        Value::FontSizeAdjust(None),
    ];
    out.extend(size.into_iter().map(|v| v.value));
    out.extend(height.into_iter().map(|v| v.value));
    out.push(Value::FontFamily(family));
    Some(out)
}

/// Parse a complete CSS `font` shorthand for APIs that need its family and
/// matching descriptors. Relative size and line-height components are still
/// validated by the ordinary declaration parser; only descriptor fields are
/// returned here.
pub fn parse_font_shorthand(raw: &str) -> Option<FontSpec> {
    let input = raw.trim();
    if input.is_empty()
        || ["inherit", "initial", "unset", "revert", "revert-layer"]
            .iter()
            .any(|keyword| input.eq_ignore_ascii_case(keyword))
        || [
            "caption",
            "icon",
            "menu",
            "message-box",
            "small-caption",
            "status-bar",
        ]
        .iter()
        .any(|keyword| input.eq_ignore_ascii_case(keyword))
    {
        return None;
    }
    let mut spec = FontSpec::default();
    for value in font_shorthand(input)? {
        match value {
            Value::FontStyle(style) => spec.style = style,
            Value::FontWeight(weight) => spec.weight = relative_font_weight(weight, 400),
            Value::FontStretch(stretch) => spec.stretch = stretch,
            Value::FontFamily(families) => spec.families = Some(families),
            Value::FontSizeAdjust(adjust) => spec.size_adjust = adjust,
            Value::Deferred(..) | Value::Custom(..) => return None,
            _ => {}
        }
    }
    spec.families.as_ref()?;
    Some(spec)
}

/// Parse the family-list grammar used by CSS `font-family` and font-loading
/// APIs, preserving quoted family whitespace and CSS escapes.
pub fn parse_font_family_list(raw: &str) -> Option<Arc<[Arc<str>]>> {
    font_families(raw)
}

fn border_side_value(value: &Value, side: usize) -> Option<Value> {
    Some(match value {
        Value::BorderWidth(width) => {
            Value::LogicalBorder(115 + side, LogicalBorderComponent::Width(*width))
        }
        Value::ContextLength(9, raw, nonnegative) => {
            Value::ContextLength(115 + side, raw.clone(), *nonnegative)
        }
        Value::BorderColor(color) => {
            Value::LogicalBorder(119 + side, LogicalBorderComponent::Color(*color))
        }
        Value::BorderCurrentColor => {
            Value::LogicalBorder(119 + side, LogicalBorderComponent::CurrentColor)
        }
        Value::BorderSolid(solid) => Value::LogicalBorder(
            123 + side,
            LogicalBorderComponent::Style(if *solid {
                BorderStyle::Solid
            } else {
                BorderStyle::None
            }),
        ),
        Value::BorderPattern(pattern) => Value::LogicalBorder(
            123 + side,
            LogicalBorderComponent::Style(BorderStyle::from_pattern(*pattern)),
        ),
        Value::BorderStyle(style) => {
            Value::LogicalBorder(123 + side, LogicalBorderComponent::Style(*style))
        }
        _ => return None,
    })
}

fn declarations(input: &str, offset: usize) -> Result<Vec<Declaration>, CssError> {
    declarations_with_variables(input, offset, true)
}

fn declarations_with_variables(
    input: &str,
    offset: usize,
    defer_variables: bool,
) -> Result<Vec<Declaration>, CssError> {
    let mut out = Vec::new();
    for (start, end) in declaration_spans_with_recovery(input, true)? {
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
            if let Some(name) = decoded_custom_property_name(name) {
                out.push(Declaration {
                    value: Value::Custom(name.into(), raw.trim().into()),
                    important,
                });
            }
            continue;
        }
        let name = ascii_lower(name);
        let name = &*name;
        if defer_variables
            && raw
                .as_bytes()
                .windows(4)
                .any(|v| v.eq_ignore_ascii_case(b"var("))
        {
            if !slots(name).is_empty() {
                out.push(Declaration {
                    value: Value::Deferred(name.into(), raw.into()),
                    important,
                });
            }
            continue;
        }
        if (name.starts_with("animation-") || name == "animation")
            && !["inherit", "initial", "unset", "revert", "revert-layer"]
                .iter()
                .any(|keyword| raw.eq_ignore_ascii_case(keyword))
        {
            let Some(values) = animation_values(name, raw) else {
                continue;
            };
            if out.len() + values.len() > MAX_DECLARATIONS {
                return Err(CssError {
                    offset: offset + start,
                    message: "too many declarations",
                });
            }
            out.extend(values.into_iter().map(|(slot, value)| Declaration {
                value: Value::Animation(slot, Arc::from(value)),
                important,
            }));
            continue;
        }
        if matches!(name, "transition-duration" | "transition-delay")
            && !["inherit", "initial", "unset", "revert", "revert-layer"]
                .iter()
                .any(|keyword| raw.eq_ignore_ascii_case(keyword))
        {
            let (slot, nonnegative) = if name == "transition-duration" {
                (175, true)
            } else {
                (176, false)
            };
            if let Some(values) = transition_time_values(raw, nonnegative) {
                out.push(Declaration {
                    value: Value::TransitionTimes(slot, values),
                    important,
                });
            }
            continue;
        }
        let wide_keyword = ["inherit", "initial", "unset", "revert", "revert-layer"]
            .into_iter()
            .find(|keyword| raw.eq_ignore_ascii_case(keyword));
        if let Some(wide_keyword) = wide_keyword {
            for &slot in slots(name) {
                let inherit = wide_keyword == "inherit"
                    || (matches!(wide_keyword, "unset" | "revert") && inherited_property(slot));
                out.push(Declaration {
                    value: if wide_keyword == "revert-layer" {
                        Value::RevertLayer(slot)
                    } else if wide_keyword == "revert" {
                        Value::Revert(slot)
                    } else {
                        Value::Default(slot, inherit)
                    },
                    important,
                });
            }
            continue;
        }
        let counter_value = match name {
            "counter-reset" => {
                parse_counter_declaration(CounterProperty::Reset, raw).map(Value::CounterReset)
            }
            "counter-increment" => parse_counter_declaration(CounterProperty::Increment, raw)
                .map(Value::CounterIncrement),
            "counter-set" => {
                parse_counter_declaration(CounterProperty::Set, raw).map(Value::CounterSet)
            }
            _ => None,
        };
        if let Some(value) = counter_value {
            out.push(Declaration { value, important });
            continue;
        }
        if matches!(name, "counter-reset" | "counter-increment" | "counter-set") {
            continue;
        }
        if name == "quotes" {
            if let Some(value) = parse_quotes(raw) {
                out.push(Declaration {
                    value: Value::Quotes(value),
                    important,
                });
            }
            continue;
        }
        if name == "content" {
            if let Some(value) = parse_generated_content(raw) {
                out.push(Declaration {
                    value: Value::GeneratedContent(value),
                    important,
                });
            }
            continue;
        }
        if name == "font" {
            if let Some(values) = font_shorthand(raw) {
                for value in values {
                    out.push(Declaration { value, important });
                }
            }
            continue;
        }
        if name == "border-style" {
            let Some(parts) = components(raw).filter(|parts| (1..=4).contains(&parts.len())) else {
                continue;
            };
            let styles: Option<Vec<_>> = parts
                .iter()
                .map(|token| BorderStyle::parse(token))
                .collect();
            let Some(styles) = styles else { continue };
            let sides = [
                styles[0],
                *styles.get(1).unwrap_or(&styles[0]),
                *styles.get(2).unwrap_or(&styles[0]),
                *styles.get(3).unwrap_or(styles.get(1).unwrap_or(&styles[0])),
            ];
            out.push(Declaration {
                value: Value::BorderStyle(sides[0]),
                important,
            });
            out.extend(
                sides
                    .into_iter()
                    .enumerate()
                    .map(|(side, style)| Declaration {
                        value: Value::LogicalBorder(
                            123 + side,
                            LogicalBorderComponent::Style(style),
                        ),
                        important,
                    }),
            );
            continue;
        }
        if matches!(name, "border-width" | "border-color") {
            let Some(parts) = components(raw).filter(|parts| (1..=4).contains(&parts.len())) else {
                continue;
            };
            let sides = [
                parts[0],
                *parts.get(1).unwrap_or(&parts[0]),
                *parts.get(2).unwrap_or(&parts[0]),
                *parts.get(3).unwrap_or(parts.get(1).unwrap_or(&parts[0])),
            ];
            let mut parsed = Vec::new();
            for (side, token) in sides.into_iter().enumerate() {
                let value = if name == "border-width" {
                    match token {
                        "thin" => Some(Value::LogicalBorder(
                            115 + side,
                            LogicalBorderComponent::Width(1.0),
                        )),
                        "medium" => Some(Value::LogicalBorder(
                            115 + side,
                            LogicalBorderComponent::Width(3.0),
                        )),
                        "thick" => Some(Value::LogicalBorder(
                            115 + side,
                            LogicalBorderComponent::Width(5.0),
                        )),
                        _ => parse_context_length(115 + side, token, true),
                    }
                } else if token.eq_ignore_ascii_case("currentcolor") {
                    Some(Value::LogicalBorder(
                        119 + side,
                        LogicalBorderComponent::CurrentColor,
                    ))
                } else {
                    color_value(119 + side, token)
                };
                let Some(value) = value else {
                    parsed.clear();
                    break;
                };
                parsed.push(value);
            }
            if parsed.len() == 4 {
                // Keep scalar fast paths and computed CSSOM values coherent.
                let scalar = if name == "border-width" {
                    match parts[0] {
                        "thin" => Some(Value::BorderWidth(1.0)),
                        "medium" => Some(Value::BorderWidth(3.0)),
                        "thick" => Some(Value::BorderWidth(5.0)),
                        _ => parse_context_length(9, parts[0], true),
                    }
                } else if parts[0].eq_ignore_ascii_case("currentcolor") {
                    Some(Value::BorderCurrentColor)
                } else {
                    color_value(10, parts[0])
                };
                if let Some(value) = scalar {
                    out.push(Declaration { value, important });
                }
                out.extend(
                    parsed
                        .into_iter()
                        .map(|value| Declaration { value, important }),
                );
            }
            continue;
        }
        if name == "gap" {
            if let Some(parts) = components(raw).filter(|parts| (1..=2).contains(&parts.len())) {
                if let (Some(row), Some(column)) = (
                    gap_value(17, parts[0]),
                    gap_value(76, parts.get(1).copied().unwrap_or(parts[0])),
                ) {
                    out.push(Declaration {
                        value: row,
                        important,
                    });
                    out.push(Declaration {
                        value: column,
                        important,
                    });
                }
            }
            continue;
        }
        if name == "flex-flow" {
            if let Some(tokens) = components(raw).filter(|tokens| (1..=2).contains(&tokens.len())) {
                let mut direction = None;
                let mut wrap = None;
                let mut valid = true;
                for token in tokens {
                    match token {
                        "row" | "row-reverse" | "column" | "column-reverse"
                            if direction.is_none() =>
                        {
                            direction = Some(token)
                        }
                        "nowrap" | "wrap" | "wrap-reverse" if wrap.is_none() => wrap = Some(token),
                        _ => valid = false,
                    }
                }
                if valid {
                    let direction = match direction.unwrap_or("row") {
                        "row-reverse" => FlexDirection::RowReverse,
                        "column" => FlexDirection::Column,
                        "column-reverse" => FlexDirection::ColumnReverse,
                        _ => FlexDirection::Row,
                    };
                    let wrap = wrap.unwrap_or("nowrap");
                    for value in [
                        Value::FlexDirection(direction),
                        Value::FlexWrap(wrap != "nowrap"),
                        Value::FlexWrapReverse(wrap == "wrap-reverse"),
                    ] {
                        out.push(Declaration { value, important });
                    }
                }
            }
            continue;
        }
        if matches!(name, "transform" | "transform-origin") {
            let context = static_length_context();
            let independent = length_independent(raw);
            let value = if name == "transform" {
                if raw == "none" {
                    Some(Value::Transforms(None))
                } else {
                    transform_list(raw, context).map(|list| {
                        if independent {
                            Value::Transforms(Some(list))
                        } else {
                            Value::TransformRaw(raw.into())
                        }
                    })
                }
            } else {
                transform_origin(raw, context).map(|origin| {
                    if independent {
                        Value::TransformOrigin(origin)
                    } else {
                        Value::TransformOriginRaw(raw.into())
                    }
                })
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
                let context = static_length_context();
                if let Some(shadows) =
                    color_independent(raw, |color| box_shadows(raw, color, Some(context)))
                {
                    out.push(Declaration {
                        value: Value::Shadows(Some(shadows)),
                        important,
                    });
                } else if box_shadows(raw, Style::initial().color, Some(context)).is_some() {
                    out.push(Declaration {
                        value: Value::ShadowsRaw(raw.into()),
                        important,
                    });
                }
            }
            continue;
        }
        if name == "inset" {
            let mut buffer = [""; 5];
            let mut count = 0;
            for token in raw.split_ascii_whitespace().take(buffer.len()) {
                buffer[count] = token;
                count += 1;
            }
            let [top, right, bottom, left] = match &buffer[..count] {
                [a] => [*a, *a, *a, *a],
                [a, b] => [*a, *b, *a, *b],
                [a, b, c] => [*a, *b, *c, *b],
                [a, b, c, d] => [*a, *b, *c, *d],
                _ => {
                    continue;
                }
            };
            let sides = [top, right, bottom, left];
            let parsed: Option<[Value; 4]> = (|| {
                let mut values = [
                    Value::Offset(0, None),
                    Value::Offset(1, None),
                    Value::Offset(2, None),
                    Value::Offset(3, None),
                ];
                for (side, token) in sides.iter().enumerate() {
                    if *token != "auto" {
                        values[side] = Value::Offset(side, Some(length(token)?));
                    }
                }
                Some(values)
            })();
            if let Some(values) = parsed {
                for (side, value) in values.into_iter().enumerate() {
                    debug_assert_eq!(value.slot(), 35 + side);
                    out.push(Declaration { value, important });
                }
            }
            continue;
        }
        // Logical shorthands retain their axes until the computed writing
        // mode is known, then resolve to physical sides.
        if matches!(
            name,
            "margin-inline"
                | "margin-block"
                | "padding-inline"
                | "padding-block"
                | "inset-inline"
                | "inset-block"
        ) {
            let (start_side, end_side) =
                if matches!(name, "inset-inline" | "margin-inline" | "padding-inline") {
                    match name {
                        "margin-inline" => (85, 86),
                        "padding-inline" => (81, 82),
                        _ => (95, 96),
                    }
                } else if name == "padding-block" {
                    (83, 84)
                } else if name == "margin-block" {
                    (99, 100)
                } else {
                    (97, 98)
                };
            let mut values = raw.split_ascii_whitespace();
            let pair = match (values.next(), values.next(), values.next()) {
                (Some(start), end, None) => Some((start, end.unwrap_or(start))),
                _ => None,
            };
            if let Some((start, end)) = pair {
                let mut valid = true;
                let mut push = |side: usize, token: &str, out: &mut Vec<Declaration>| {
                    let value = if matches!(name, "inset-inline" | "inset-block") {
                        match token {
                            "auto" => Some(Value::LogicalOffset(side, None)),
                            _ => length(token).map(|v| Value::LogicalOffset(side, Some(v))),
                        }
                    } else if matches!(name, "margin-inline" | "margin-block") {
                        match token {
                            "auto" if name == "margin-block" => {
                                Some(Value::LogicalMargin(side, None))
                            }
                            "auto" => None,
                            _ if name == "margin-inline" => {
                                length(token).map(|v| Value::LogicalEdge(side, v))
                            }
                            _ => length(token).map(|v| Value::LogicalMargin(side, Some(v))),
                        }
                    } else {
                        match token {
                            "auto" => None,
                            _ if matches!(name, "padding-inline" | "padding-block") => {
                                nonnegative_length(token).map(|v| Value::LogicalEdge(side, v))
                            }
                            _ => nonnegative_length(token).map(|v| Value::PaddingSide(side, v)),
                        }
                    };
                    match value {
                        Some(value) => out.push(Declaration { value, important }),
                        None => valid = false,
                    }
                };
                let mut expanded = Vec::new();
                push(start_side, start, &mut expanded);
                push(end_side, end, &mut expanded);
                if valid {
                    out.extend(expanded);
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
                let style = BorderStyle::parse(token).map(Value::BorderStyle);
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
                    border_style.unwrap_or(Value::BorderStyle(BorderStyle::None)),
                ] {
                    out.push(Declaration {
                        value: value.clone(),
                        important,
                    });
                    for side in 0..4 {
                        if let Some(value) = border_side_value(&value, side) {
                            out.push(Declaration { value, important });
                        }
                    }
                }
            }
            continue;
        }
        if matches!(
            name,
            "border-inline"
                | "border-block"
                | "border-inline-start"
                | "border-inline-end"
                | "border-block-start"
                | "border-block-end"
                | "border-top"
                | "border-right"
                | "border-bottom"
                | "border-left"
        ) {
            let Some(tokens) = components(raw) else {
                continue;
            };
            let edges: &[usize] = match name {
                "border-inline" => &[0, 1],
                "border-inline-start" => &[0],
                "border-inline-end" => &[1],
                "border-block" => &[2, 3],
                "border-block-start" => &[2],
                "border-block-end" => &[3],
                "border-top" => &[4],
                "border-right" => &[5],
                "border-bottom" => &[6],
                _ => &[7],
            };
            let (mut width, mut paint, mut border_style, mut valid) = (None, None, None, true);
            for token in tokens {
                let parsed_style = BorderStyle::parse(token);
                if let Some(value) = parsed_style {
                    if border_style.replace(value).is_some() {
                        valid = false;
                    }
                } else if token.eq_ignore_ascii_case("currentcolor") {
                    if paint.replace(None).is_some() {
                        valid = false;
                    }
                } else if let Some(value) = color(token) {
                    if paint.replace(Some(value)).is_some() {
                        valid = false;
                    }
                } else {
                    let parsed = match token {
                        "thin" => Some(1.0),
                        "medium" => Some(3.0),
                        "thick" => Some(5.0),
                        _ => nonnegative_length(token),
                    };
                    if width.replace(parsed).is_some() || parsed.is_none() {
                        valid = false;
                    }
                }
            }
            if !valid || raw.is_empty() {
                continue;
            }
            for edge in edges {
                let (width_slot, color_slot, style_slot) = if *edge < 4 {
                    (103 + edge, 107 + edge, 111 + edge)
                } else {
                    let side = edge - 4;
                    (115 + side, 119 + side, 123 + side)
                };
                for (slot, component) in [
                    (
                        width_slot,
                        LogicalBorderComponent::Width(width.flatten().unwrap_or(3.0)),
                    ),
                    (
                        color_slot,
                        paint.flatten().map_or(
                            LogicalBorderComponent::CurrentColor,
                            LogicalBorderComponent::Color,
                        ),
                    ),
                    (
                        style_slot,
                        LogicalBorderComponent::Style(border_style.unwrap_or(BorderStyle::None)),
                    ),
                ] {
                    out.push(Declaration {
                        value: Value::LogicalBorder(slot, component),
                        important,
                    });
                }
            }
            continue;
        }
        if name == "column-rule" {
            let Some(tokens) = components(raw) else {
                continue;
            };
            let (mut width, mut paint, mut rule_style, mut valid) = (None, None, None, true);
            for token in tokens {
                let style = match token {
                    "solid" => Some((true, None)),
                    "none" | "hidden" => Some((false, None)),
                    "double" => Some((true, Some(BorderPattern::Double))),
                    "dashed" => Some((true, Some(BorderPattern::Dashed))),
                    "dotted" => Some((true, Some(BorderPattern::Dotted))),
                    _ => None,
                };
                if let Some(style) = style {
                    if rule_style.replace(style).is_some() {
                        valid = false;
                    }
                } else if token.eq_ignore_ascii_case("currentcolor") {
                    if paint.replace(None).is_some() {
                        valid = false;
                    }
                } else if let Some(color) = color(token) {
                    if paint.replace(Some(color)).is_some() {
                        valid = false;
                    }
                } else {
                    let parsed = match token {
                        "thin" => Some(1.0),
                        "medium" => Some(3.0),
                        "thick" => Some(5.0),
                        _ => nonnegative_length(token),
                    };
                    if width.replace(parsed).is_some() || parsed.is_none() {
                        valid = false;
                    }
                }
            }
            if valid && !raw.is_empty() {
                let (visible, pattern) = rule_style.unwrap_or((false, None));
                for value in [
                    Value::ColumnRuleWidth(width.flatten().unwrap_or(3.0)),
                    Value::ColumnRuleColor(paint.flatten()),
                    Value::ColumnRuleStyle(visible, pattern),
                ] {
                    out.push(Declaration { value, important });
                }
            }
            continue;
        }
        if matches!(name, "background" | "background-image") {
            if name == "background-image" {
                if raw == "none" {
                    out.push(Declaration {
                        value: Value::BackgroundImageNone,
                        important,
                    });
                } else if valid_background_images(raw) {
                    out.push(Declaration {
                        value: background_image_value(raw),
                        important,
                    });
                }
                continue;
            }
            if let Some(parts) = background_shorthand(raw) {
                if !valid_background_images(&parts.images) {
                    continue;
                }
                let Some(position) = background_position_value(&parts.positions) else {
                    continue;
                };
                let Some(size) = background_size_value(&parts.sizes) else {
                    continue;
                };
                // The shorthand resets every omitted component to its initial value.
                out.push(Declaration {
                    value: Value::Background(parts.color.unwrap_or(Style::initial().background)),
                    important,
                });
                out.push(Declaration {
                    value: if parts.all_images_none {
                        Value::BackgroundImageNone
                    } else {
                        background_image_value(&parts.images)
                    },
                    important,
                });
                out.push(Declaration {
                    value: position,
                    important,
                });
                out.push(Declaration {
                    value: size,
                    important,
                });
                if let Some(repeats) = parts.repeats {
                    out.push(Declaration {
                        value: Value::BackgroundRepeats(repeats.into()),
                        important,
                    });
                }
                out.push(Declaration {
                    value: Value::BackgroundOrigin(parts.origins.into()),
                    important,
                });
                out.push(Declaration {
                    value: Value::BackgroundClip(parts.clips.into()),
                    important,
                });
                out.push(Declaration {
                    value: Value::BackgroundAttachment(parts.attachments.into()),
                    important,
                });
            }
            continue;
        }
        if name == "overflow" {
            let values = raw.split_whitespace().collect::<Vec<_>>();
            if (1..=2).contains(&values.len()) {
                if let (Some(x), Some(y)) = (
                    Overflow::parse(values[0]),
                    Overflow::parse(values.get(1).copied().unwrap_or(values[0])),
                ) {
                    out.push(Declaration {
                        value: Value::OverflowAxis(12, x),
                        important,
                    });
                    out.push(Declaration {
                        value: Value::OverflowAxis(132, y),
                        important,
                    });
                }
            }
            continue;
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
        if name != "border-radius" {
            if let Some(&slot) = slots(name).first() {
                if length(raw).is_none() || (slot == 20 && has_contextual_length_unit(raw)) {
                    if let Some(value) =
                        parse_context_length(slot, raw, !matches!(slot, 5 | 35..=42))
                    {
                        out.push(Declaration { value, important });
                        continue;
                    }
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
                    let parsed = typed_declarations("flex", &converted, offset + start)?;
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
        let mut parsed = typed_declarations(name, raw, offset + start)?;
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
            if name == "border-style" {
                for side in 0..4 {
                    if let Some(value) = border_side_value(&declaration.value, side) {
                        out.push(Declaration { value, important });
                    }
                }
            }
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

fn animation_values(name: &str, raw: &str) -> Option<Vec<(usize, String)>> {
    let names = [
        "animation-name",
        "animation-duration",
        "animation-delay",
        "animation-timing-function",
        "animation-iteration-count",
        "animation-direction",
        "animation-fill-mode",
        "animation-play-state",
        "animation-composition",
    ];
    if name == "animation" {
        let mut columns: [Vec<String>; 8] = core::array::from_fn(|_| Vec::new());
        for item in top_level_split(raw, b',', 64)? {
            let mut values = ["none", "0s", "0s", "ease", "1", "normal", "none", "running"];
            let mut seen_time = 0;
            let mut seen = [false; 8];
            for token in grid_components(item)? {
                let time = css_time_ms(token);
                if let Some(ms) = time {
                    let index = if seen_time == 0 { 1 } else { 2 };
                    if seen[index] || (index == 1 && ms < 0.0) {
                        return None;
                    }
                    values[index] = token;
                    seen[index] = true;
                    seen_time += 1;
                } else if crate::animation::ease(token, 0.5).is_some() {
                    if seen[3] {
                        return None;
                    }
                    values[3] = token;
                    seen[3] = true;
                } else if token == "infinite"
                    || token
                        .parse::<f64>()
                        .is_ok_and(|v| v.is_finite() && v >= 0.0)
                {
                    if seen[4] {
                        return None;
                    }
                    values[4] = token;
                    seen[4] = true;
                } else if matches!(
                    token,
                    "normal" | "reverse" | "alternate" | "alternate-reverse"
                ) {
                    if seen[5] {
                        return None;
                    }
                    values[5] = token;
                    seen[5] = true;
                } else if matches!(token, "none" | "forwards" | "backwards" | "both") {
                    if seen[6] {
                        return None;
                    }
                    values[6] = token;
                    seen[6] = true;
                } else if matches!(token, "running" | "paused") {
                    if seen[7] {
                        return None;
                    }
                    values[7] = token;
                    seen[7] = true;
                } else {
                    if seen[0] || !valid_animation_name(token) {
                        return None;
                    }
                    values[0] = token;
                    seen[0] = true;
                }
            }
            if seen_time > 2 {
                return None;
            }
            for index in 0..8 {
                columns[index].push(values[index].to_owned());
            }
        }
        return Some(
            columns
                .into_iter()
                .enumerate()
                .map(|(i, values)| (163 + i, values.join(", ")))
                .collect(),
        );
    }
    let index = names.iter().position(|candidate| *candidate == name)?;
    let valid = top_level_split(raw, b',', 64)?.into_iter().all(|item| {
        let item = item.trim();
        match index {
            0 => valid_animation_name(item),
            1 | 2 => css_time_ms(item).is_some_and(|ms| index == 1 && ms >= 0.0 || index == 2),
            3 => crate::animation::ease(item, 0.5).is_some(),
            4 => {
                item == "infinite"
                    || item
                        .parse::<f64>()
                        .is_ok_and(|value| value.is_finite() && value >= 0.0)
            }
            5 => matches!(
                item,
                "normal" | "reverse" | "alternate" | "alternate-reverse"
            ),
            6 => matches!(item, "none" | "forwards" | "backwards" | "both"),
            7 => matches!(item, "running" | "paused"),
            8 => matches!(item, "replace" | "add" | "accumulate"),
            _ => false,
        }
    });
    valid.then(|| alloc::vec![(163 + index, raw.trim().to_owned())])
}

fn transition_time_values(
    raw: &str,
    nonnegative: bool,
) -> Option<Arc<[typed_numeric::NumericValue]>> {
    let parts = top_level_split(raw, b',', 64)?;
    if parts.is_empty() {
        return None;
    }
    let mut values = Vec::new();
    values.try_reserve_exact(parts.len()).ok()?;
    for part in parts {
        values.push(transition_time_value(part, nonnegative)?);
    }
    Some(values.into())
}

#[derive(Clone, Copy)]
struct TransitionTimeExpression {
    value_seconds: f64,
    numeric_type: typed_numeric::NumericType,
}

/// Fold the shared bounded numeric grammar directly into a scalar time value.
/// This is used by transition computed values so calc() does not allocate a
/// temporary Typed OM expression tree on the renderer path.
struct TransitionTimeBuilder;

impl typed_numeric::NumericExpressionBuilder for TransitionTimeBuilder {
    type Expr = TransitionTimeExpression;
    type Accumulator = TransitionTimeExpression;

    fn value(&mut self, value: typed_numeric::NumericValue) -> Option<Self::Expr> {
        use typed_numeric::{NumericDimension, NumericType};

        let dimension = value.unit.dimension();
        if !matches!(dimension, NumericDimension::Number | NumericDimension::Time) {
            return None;
        }
        let (unit, factor) = value.unit.canonical_unit_and_factor()?;
        if unit.dimension() != dimension {
            return None;
        }
        let value_seconds = value.value * factor;
        value_seconds
            .is_finite()
            .then_some(TransitionTimeExpression {
                value_seconds,
                numeric_type: NumericType::from_unit(unit),
            })
    }

    fn begin_sum(&mut self, first: Self::Expr) -> Option<Self::Accumulator> {
        Some(first)
    }

    fn push_sum(
        &mut self,
        values: &mut Self::Accumulator,
        next: Self::Expr,
        subtract: bool,
    ) -> Option<()> {
        values.numeric_type = values.numeric_type.add(next.numeric_type)?;
        values.value_seconds += if subtract {
            -next.value_seconds
        } else {
            next.value_seconds
        };
        values.value_seconds.is_finite().then_some(())
    }

    fn finish_sum(&mut self, values: Self::Accumulator, _operated: bool) -> Option<Self::Expr> {
        Some(values)
    }

    fn begin_product(&mut self, first: Self::Expr) -> Option<Self::Accumulator> {
        Some(first)
    }

    fn push_product(
        &mut self,
        values: &mut Self::Accumulator,
        next: Self::Expr,
        divide: bool,
    ) -> Option<()> {
        let next = if divide { self.invert(next)? } else { next };
        values.numeric_type = values.numeric_type.multiply(next.numeric_type)?;
        values.value_seconds *= next.value_seconds;
        values.value_seconds.is_finite().then_some(())
    }

    fn finish_product(&mut self, values: Self::Accumulator, _operated: bool) -> Option<Self::Expr> {
        Some(values)
    }

    fn begin_min(&mut self, first: Self::Expr) -> Option<Self::Accumulator> {
        Some(first)
    }

    fn push_min(&mut self, values: &mut Self::Accumulator, next: Self::Expr) -> Option<()> {
        values.numeric_type = values.numeric_type.add(next.numeric_type)?;
        values.value_seconds = values.value_seconds.min(next.value_seconds);
        Some(())
    }

    fn finish_min(&mut self, values: Self::Accumulator) -> Option<Self::Expr> {
        Some(values)
    }

    fn begin_max(&mut self, first: Self::Expr) -> Option<Self::Accumulator> {
        Some(first)
    }

    fn push_max(&mut self, values: &mut Self::Accumulator, next: Self::Expr) -> Option<()> {
        values.numeric_type = values.numeric_type.add(next.numeric_type)?;
        values.value_seconds = values.value_seconds.max(next.value_seconds);
        Some(())
    }

    fn finish_max(&mut self, values: Self::Accumulator) -> Option<Self::Expr> {
        Some(values)
    }

    fn calc(&mut self, value: Self::Expr) -> Option<Self::Expr> {
        Some(value)
    }

    fn clamp(
        &mut self,
        lower: Self::Expr,
        value: Self::Expr,
        upper: Self::Expr,
    ) -> Option<Self::Expr> {
        let numeric_type = lower
            .numeric_type
            .add(value.numeric_type)?
            .add(upper.numeric_type)?;
        Some(TransitionTimeExpression {
            value_seconds: lower
                .value_seconds
                .max(value.value_seconds.min(upper.value_seconds)),
            numeric_type,
        })
    }

    fn negate(&mut self, mut value: Self::Expr) -> Option<Self::Expr> {
        value.value_seconds = -value.value_seconds;
        value.value_seconds.is_finite().then_some(value)
    }

    fn invert(&mut self, mut value: Self::Expr) -> Option<Self::Expr> {
        if value.value_seconds == 0.0 {
            return None;
        }
        value.value_seconds = 1.0 / value.value_seconds;
        value.numeric_type = value.numeric_type.invert();
        value.value_seconds.is_finite().then_some(value)
    }

    fn sign(&mut self, value: Self::Expr) -> Option<Self::Expr> {
        let value_seconds = if value.value_seconds == 0.0 {
            value.value_seconds
        } else if value.value_seconds.is_sign_negative() {
            -1.0
        } else {
            1.0
        };
        Some(TransitionTimeExpression {
            value_seconds,
            numeric_type: typed_numeric::NumericType::default(),
        })
    }
}

fn transition_time_value(input: &str, nonnegative: bool) -> Option<typed_numeric::NumericValue> {
    use typed_numeric::{NumericDimension, NumericType, NumericUnit, NumericValue};

    let input = input.trim();
    // Keep an author-specified primitive in its parsed unit for CSSStyleValue
    // and specified-value serialization.
    if let Some(value) = typed_numeric::parse_numeric_value(input) {
        if value.unit.dimension() == NumericDimension::Time && (!nonnegative || value.value >= 0.0)
        {
            return Some(value);
        }
        return None;
    }

    let mut builder = TransitionTimeBuilder;
    let value = typed_numeric::parse_numeric_expression_with(input, &mut builder)?;
    if value.numeric_type != NumericType::from_unit(NumericUnit::S) {
        return None;
    }
    Some(NumericValue {
        // CSS range checking for a calculated duration happens at computed
        // value time. Keep a negative primitive invalid above, but clamp a
        // valid calc() result to the property's nonnegative range here.
        value: if nonnegative {
            value.value_seconds.max(0.0)
        } else {
            value.value_seconds
        },
        unit: NumericUnit::S,
    })
}

pub fn serialize_transition_time_values(values: Option<&[typed_numeric::NumericValue]>) -> String {
    serialize_transition_time_list(values, true)
}

fn serialize_transition_time_values_as_specified(
    values: Option<&[typed_numeric::NumericValue]>,
) -> String {
    serialize_transition_time_list(values, false)
}

fn serialize_transition_time_list(
    values: Option<&[typed_numeric::NumericValue]>,
    seconds: bool,
) -> String {
    let Some(values) = values else {
        return String::from("0s");
    };
    let mut output = String::new();
    for (index, value) in values.iter().enumerate() {
        if index != 0 {
            output.push_str(", ");
        }
        let (number, unit) = if seconds && value.unit == typed_numeric::NumericUnit::Ms {
            (value.value / 1000.0, typed_numeric::NumericUnit::S)
        } else {
            (value.value, value.unit)
        };
        output.push_str(&typed_numeric::serialize_numeric_value(number, unit));
    }
    output
}

/// Canonically serialize a transition duration or delay list.
pub fn serialize_transition_time_declaration(name: &str, raw: &str) -> Option<String> {
    let nonnegative = if name.eq_ignore_ascii_case("transition-duration") {
        true
    } else if name.eq_ignore_ascii_case("transition-delay") {
        false
    } else {
        return None;
    };
    let values = transition_time_values(raw, nonnegative)?;
    Some(serialize_transition_time_values_as_specified(Some(
        values.as_ref(),
    )))
}

fn css_time_ms(input: &str) -> Option<f64> {
    let input = input.trim();
    let (number, scale) = if let Some(number) = input.strip_suffix("ms") {
        (number, 1.0)
    } else if let Some(number) = input.strip_suffix('s') {
        (number, 1000.0)
    } else {
        return None;
    };
    let value = number.trim().parse::<f64>().ok()? * scale;
    value.is_finite().then_some(value)
}

fn valid_animation_name(input: &str) -> bool {
    let input = input.trim();
    if input == "none" {
        return true;
    }
    if (input.starts_with('"') && input.ends_with('"'))
        || (input.starts_with('\'') && input.ends_with('\''))
    {
        return css_string(input).is_some();
    }
    if matches!(
        input.to_ascii_lowercase().as_str(),
        "initial" | "inherit" | "unset" | "revert" | "revert-layer"
    ) {
        return false;
    }
    let mut position = 0;
    consume_selector_identifier(input, &mut position).is_some() && position == input.len()
}

/// Decode one CSS animation-name item, preserving its case-sensitive value.
pub fn normalize_animation_name(input: &str) -> Option<String> {
    let input = input.trim();
    if input.eq_ignore_ascii_case("none") {
        return Some(String::from("none"));
    }
    if (input.starts_with('"') && input.ends_with('"'))
        || (input.starts_with('\'') && input.ends_with('\''))
    {
        return css_string(input);
    }
    let mut position = 0;
    let value = consume_selector_identifier(input, &mut position)?;
    (position == input.len()).then_some(value)
}

fn grid_components(raw: &str) -> Option<Vec<&str>> {
    if raw.len() > MAX_VARIABLE_BYTES {
        return None;
    }
    let (mut start, mut parentheses, mut brackets, mut quote, mut escaped) =
        (0usize, 0usize, 0usize, 0u8, false);
    let mut result = Vec::new();
    for (index, byte) in raw.bytes().enumerate() {
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
        match byte {
            b'\'' | b'"' => quote = byte,
            b'(' => {
                parentheses += 1;
                if parentheses > 32 {
                    return None;
                }
            }
            b')' => {
                parentheses = parentheses.checked_sub(1)?;
                // Component values may be adjacent without whitespace. When
                // one top-level grid function closes directly before another
                // function token, keep them as separate track-list items.
                // This matters for e.g. `repeat(2,5px)[outer]repeat(auto-fill,20px)repeat(2,5px)`.
                if parentheses == 0 && brackets == 0 && grid_function_token_starts(raw, index + 1) {
                    result.push(&raw[start..index + 1]);
                    if result.len() > 64 {
                        return None;
                    }
                    start = index + 1;
                }
            }
            b'[' if parentheses == 0 => {
                // Grid line-name blocks are separate grammar components even
                // when authors omit whitespace around them, e.g.
                // `repeat(2, 5px)[before]repeat(auto-fill, [cell] 20px)`.
                // Keep the track/function slices intact while allowing the
                // adjacent bracket block to be parsed independently.
                if brackets == 0 && start < index {
                    result.push(&raw[start..index]);
                    if result.len() > 64 {
                        return None;
                    }
                }
                brackets += 1;
                if brackets == 1 {
                    start = index;
                }
            }
            b']' if parentheses == 0 => {
                brackets = brackets.checked_sub(1)?;
                if brackets == 0 {
                    let end = index + 1;
                    result.push(&raw[start..end]);
                    if result.len() > 64 {
                        return None;
                    }
                    start = end;
                }
            }
            _ if byte.is_ascii_whitespace() && parentheses == 0 && brackets == 0 => {
                if start < index {
                    result.push(&raw[start..index]);
                    if result.len() > 64 {
                        return None;
                    }
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    if parentheses != 0 || brackets != 0 || quote != 0 || escaped {
        return None;
    }
    if start < raw.len() {
        result.push(&raw[start..]);
    }
    (result.len() <= 64).then_some(result)
}

fn grid_function_token_starts(raw: &str, start: usize) -> bool {
    let Some(rest) = raw.get(start..) else {
        return false;
    };
    let bytes = rest.as_bytes();
    if !bytes
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
    {
        return false;
    }
    let mut index = 1;
    while let Some(byte) = bytes.get(index) {
        if *byte == b'(' {
            return true;
        }
        if !(byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_')) {
            return false;
        }
        index += 1;
    }
    false
}

fn grid_resolve(slot: usize, raw: &str, context: LengthContext) -> Option<Value> {
    match slot {
        27 | 28 => {
            if let Some(auto) = grid_auto_repeat(raw, context) {
                Some(if slot == 27 {
                    Value::GridColumnsAuto(auto)
                } else {
                    Value::GridRowsAuto(auto)
                })
            } else {
                grid_template(raw, context).map(|(tracks, names, subgrid)| {
                    if slot == 27 {
                        Value::GridColumns(tracks, names, subgrid)
                    } else {
                        Value::GridRows(tracks, names, subgrid)
                    }
                })
            }
        }
        61 | 62 => grid_tracks(raw, context).map(|tracks| {
            if slot == 61 {
                Value::GridAutoColumns(tracks)
            } else {
                Value::GridAutoRows(tracks)
            }
        }),
        _ => None,
    }
}

/// A grid track list resolved once when it holds no font or viewport relative
/// lengths, otherwise kept as text for the computed-style pass.
fn grid_value(slot: usize, raw: &str) -> Value {
    if length_independent(raw) {
        if let Some(value) = grid_resolve(slot, raw, static_length_context()) {
            return value;
        }
    }
    Value::GridRaw(slot, Arc::from(raw))
}

fn grid_template_shorthand(raw: &str, context: LengthContext) -> Option<Vec<Value>> {
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("none") {
        return Some(alloc::vec![
            grid_value(28, "none"),
            grid_value(27, "none"),
            Value::GridAreas(Arc::from([])),
        ]);
    }

    let parts = top_level_split(raw, b'/', 3)?;
    if parts.len() > 2 {
        return None;
    }
    let row_side = parts[0];
    let area_tokens = grid_components(row_side)?;
    let has_areas = area_tokens
        .iter()
        .any(|token| token.len() >= 2 && matches!(token.as_bytes()[0], b'\'' | b'"'));
    if has_areas {
        let mut area_text = String::new();
        let mut row_text = String::new();
        let mut index = 0;
        let mut row_count = 0usize;
        while index < area_tokens.len() {
            let is_line_names = |token: &str| token.starts_with('[') && token.ends_with(']');
            let is_area_row = |token: &str| {
                token.len() >= 2
                    && matches!(token.as_bytes()[0], b'\'' | b'"')
                    && token.as_bytes()[0] == token.as_bytes()[token.len() - 1]
            };
            let token = area_tokens[index];
            if is_line_names(token) {
                grid_append_line_names(&mut row_text, token)?;
                index += 1;
                if area_tokens
                    .get(index)
                    .is_some_and(|token| is_line_names(token))
                {
                    return None;
                }
            }
            let token = *area_tokens.get(index)?;
            if !is_area_row(token) {
                return None;
            }
            if !area_text.is_empty() {
                area_text.push(' ');
            }
            area_text.push_str(token);
            index += 1;
            let size = if let Some(candidate) = area_tokens.get(index).copied() {
                if is_line_names(candidate) || is_area_row(candidate) {
                    "auto"
                } else {
                    if grid_function_args(candidate, "repeat").is_some()
                        || grid_tracks(candidate, context).is_none_or(|tracks| tracks.len() != 1)
                    {
                        return None;
                    }
                    index += 1;
                    candidate
                }
            } else {
                "auto"
            };
            if !row_text.is_empty() {
                row_text.push(' ');
            }
            row_text.push_str(size);
            if area_tokens
                .get(index)
                .is_some_and(|token| is_line_names(token))
            {
                grid_append_line_names(&mut row_text, area_tokens[index])?;
                index += 1;
            }
            row_count += 1;
        }
        let row_tracks = grid_template(&row_text, context)?;
        if row_count == 0
            || grid_areas(&area_text).is_none()
            || row_tracks.2
            || row_tracks.0.is_empty()
        {
            return None;
        }
        let columns = if parts.len() == 2 { parts[1] } else { "none" };
        if parts.len() == 2 {
            let (column_tracks, _, subgrid) = grid_template(columns, context)?;
            if subgrid || column_tracks.is_empty() {
                return None;
            }
        }
        return Some(alloc::vec![
            grid_value(28, &row_text),
            grid_value(27, columns),
            Value::GridAreas(grid_areas(&area_text)?),
        ]);
    }

    if parts.len() != 2 {
        return None;
    }
    let valid_rows =
        grid_auto_repeat(parts[0], context).is_some() || grid_template(parts[0], context).is_some();
    let valid_columns =
        grid_auto_repeat(parts[1], context).is_some() || grid_template(parts[1], context).is_some();
    (valid_rows && valid_columns).then(|| {
        alloc::vec![
            grid_value(28, parts[0]),
            grid_value(27, parts[1]),
            Value::GridAreas(Arc::from([])),
        ]
    })
}

/// Append a line-name block from a grid-template area row, merging adjacent
/// blocks which name the same boundary line. The shorthand permits the block
/// after one row and the block before the next row to be adjacent in source.
fn grid_append_line_names(list: &mut String, token: &str) -> Option<()> {
    let names = token.strip_prefix('[')?.strip_suffix(']')?;
    if list.ends_with(']') {
        list.pop();
        if !names.is_empty() && !list.ends_with('[') {
            list.push(' ');
        }
        list.push_str(names);
        list.push(']');
    } else {
        if !list.is_empty() {
            list.push(' ');
        }
        list.push_str(token);
    }
    Some(())
}

fn grid_shorthand(raw: &str, context: LengthContext) -> Option<Vec<Value>> {
    let mut template = grid_template_shorthand(raw, context);
    if let Some(values) = template.as_mut() {
        values.extend([
            Value::GridAutoFlow(GridAutoFlow {
                column: false,
                dense: false,
            }),
            Value::GridAutoRows(grid_tracks("auto", context)?),
            Value::GridAutoColumns(grid_tracks("auto", context)?),
        ]);
        return template;
    }

    let sides = top_level_split(raw, b'/', 3)?;
    if sides.len() != 2 {
        return None;
    }
    let before = grid_components(sides[0])?;
    let after = grid_components(sides[1])?;
    let contains_flow = |values: &[&str]| {
        values
            .iter()
            .any(|token| token.eq_ignore_ascii_case("auto-flow"))
    };
    let flow_before = contains_flow(&before);
    let flow_after = contains_flow(&after);
    if flow_before == flow_after {
        return None;
    }
    let (flow_side, template_side) = if flow_before {
        (&before, sides[1])
    } else {
        (&after, sides[0])
    };
    let (mut dense, mut saw_flow, mut tracks) = (false, false, String::new());
    for token in flow_side {
        if token.eq_ignore_ascii_case("auto-flow") {
            if saw_flow {
                return None;
            }
            saw_flow = true;
        } else if token.eq_ignore_ascii_case("dense") {
            if dense {
                return None;
            }
            dense = true;
        } else {
            if !tracks.is_empty() {
                tracks.push(' ');
            }
            tracks.push_str(token);
        }
    }
    if !saw_flow {
        return None;
    }
    if tracks.is_empty() {
        tracks.push_str("auto");
    }
    let auto_tracks = grid_tracks(&tracks, context)?;
    let template_auto = grid_auto_repeat(template_side, context);
    if template_auto.is_none() && grid_template(template_side, context).is_none() {
        return None;
    }
    let flow = GridAutoFlow {
        column: !flow_before,
        dense,
    };
    Some(if flow_before {
        alloc::vec![
            grid_value(28, "none"),
            grid_value(27, template_side),
            Value::GridAreas(Arc::from([])),
            Value::GridAutoFlow(flow),
            Value::GridAutoRows(auto_tracks),
            Value::GridAutoColumns(grid_tracks("auto", context)?),
        ]
    } else {
        alloc::vec![
            grid_value(28, template_side),
            grid_value(27, "none"),
            Value::GridAreas(Arc::from([])),
            Value::GridAutoFlow(flow),
            Value::GridAutoRows(grid_tracks("auto", context)?),
            Value::GridAutoColumns(auto_tracks),
        ]
    })
}

fn typed_declarations(name: &str, raw: &str, offset: usize) -> Result<Vec<Declaration>, CssError> {
    let mut out = Vec::new();
    {
        let raw = raw.split("/*").next().unwrap().trim();
        let (raw, important) = important_value(raw);
        if name == "list-style" {
            let Some((marker, position, image)) = list_style_shorthand(raw) else {
                return Ok(out);
            };
            out.try_reserve_exact(3).map_err(|_| CssError {
                offset,
                message: "too many declarations",
            })?;
            out.push(Declaration {
                value: Value::ListStyleType(marker),
                important,
            });
            out.push(Declaration {
                value: Value::ListStylePosition(position),
                important,
            });
            out.push(Declaration {
                value: Value::ListStyleImage(image),
                important,
            });
            return Ok(out);
        }
        if name == "border-radius" {
            if border_radius_values(raw, static_length_context()).is_some() {
                if out.len() + 5 > MAX_DECLARATIONS {
                    return Err(CssError {
                        offset,
                        message: "too many declarations",
                    });
                }
                let raw: Arc<str> = Arc::from(raw);
                out.push(Declaration {
                    value: Value::BorderRadiusRaw(raw.clone()),
                    important,
                });
                for index in 0..4 {
                    out.push(Declaration {
                        value: Value::BorderRadiusShorthandCornerRaw(index, raw.clone()),
                        important,
                    });
                }
            }
            return Ok(out);
        }
        if let Some(index) = border_radius_corner_index(name) {
            if border_radius_corner_value(raw, static_length_context()).is_some() {
                out.push(Declaration {
                    value: Value::BorderRadiusCornerRaw(index, Arc::from(raw)),
                    important,
                });
            }
            return Ok(out);
        }
        if name == "flex" {
            let normalized = ascii_lower(raw);
            let values = match &*normalized {
                "none" => Some((0.0, 0.0, Value::FlexBasis(None))),
                "auto" => Some((1.0, 1.0, Value::FlexBasis(None))),
                "initial" => Some((0.0, 1.0, Value::FlexBasis(Some(0.0)))),
                _ => {
                    let (mut grow, mut shrink, mut basis, mut valid) = (None, None, None, true);
                    for token in components(raw).unwrap_or_default() {
                        if let Some(number) = css_scalar(token, false) {
                            if number < 0.0 && !math_function(token) {
                                valid = false;
                                break;
                            }
                            let number = number.max(0.0);
                            if grow.is_none() {
                                grow = Some(number);
                            } else if shrink.is_none() {
                                shrink = Some(number);
                            } else if basis.is_none() && number == 0.0 && !math_function(token) {
                                basis = Some(Value::FlexBasis(Some(0.0)));
                            } else {
                                valid = false;
                                break;
                            }
                        } else if basis.is_none() {
                            if token.eq_ignore_ascii_case("auto") {
                                basis = Some(Value::FlexBasis(None));
                            } else if token.eq_ignore_ascii_case("content") {
                                basis = Some(Value::FlexBasisContent);
                            } else if let Some(value) = intrinsic_sizing(token) {
                                basis = Some(Value::FlexBasisIntrinsic(value));
                            } else if let Some(value) = nonnegative_length(token) {
                                basis = Some(Value::FlexBasis(Some(value)));
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
                            basis.unwrap_or(Value::FlexBasis(Some(0.0))),
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
                for value in [Value::FlexGrow(grow), Value::FlexShrink(shrink), basis] {
                    out.push(Declaration { value, important });
                }
            }
            return Ok(out);
        }
        if name == "grid" {
            let context = static_length_context();
            if let Some(values) = grid_shorthand(raw, context) {
                if out.len() + values.len() > MAX_DECLARATIONS {
                    return Err(CssError {
                        offset,
                        message: "too many declarations",
                    });
                }
                out.extend(
                    values
                        .into_iter()
                        .map(|value| Declaration { value, important }),
                );
            }
            return Ok(out);
        }
        if name == "grid-template" {
            let context = static_length_context();
            if let Some(values) = grid_template_shorthand(raw, context) {
                if out.len() + values.len() > MAX_DECLARATIONS {
                    return Err(CssError {
                        offset,
                        message: "too many declarations",
                    });
                }
                out.extend(
                    values
                        .into_iter()
                        .map(|value| Declaration { value, important }),
                );
            }
            return Ok(out);
        }
        if name == "grid-area" {
            if let Some(values) = grid_area_values(raw) {
                if out.len() + values.len() > MAX_DECLARATIONS {
                    return Err(CssError {
                        offset,
                        message: "too many declarations",
                    });
                }
                out.extend(
                    values
                        .into_iter()
                        .map(|value| Declaration { value, important }),
                );
            }
            return Ok(out);
        }
        if matches!(
            name,
            "grid-template-columns" | "grid-template-rows" | "grid-auto-columns" | "grid-auto-rows"
        ) {
            let context = static_length_context();
            let valid = if name.starts_with("grid-auto-") {
                grid_tracks(raw, context).is_some_and(|v| !v.is_empty())
            } else {
                grid_auto_repeat(raw, context).is_some() || grid_template(raw, context).is_some()
            };
            if valid {
                out.push(Declaration {
                    value: grid_value(slots(&name)[0], raw),
                    important,
                });
            }
            return Ok(out);
        }
        let value = match name {
            "white-space" => match raw {
                "normal" => Some(Value::WhiteSpace(WhiteSpace::Normal)),
                "nowrap" => Some(Value::WhiteSpace(WhiteSpace::NoWrap)),
                "pre" => Some(Value::WhiteSpace(WhiteSpace::Pre)),
                "pre-wrap" => Some(Value::WhiteSpace(WhiteSpace::PreWrap)),
                "pre-line" => Some(Value::WhiteSpace(WhiteSpace::PreLine)),
                "break-spaces" => Some(Value::WhiteSpace(WhiteSpace::BreakSpaces)),
                _ => None,
            },
            "letter-spacing" => {
                if raw.eq_ignore_ascii_case("normal") {
                    Some(Value::LetterSpacing(None))
                } else {
                    parse_context_length(179, raw, false)
                }
            }
            "text-indent" => background_length(raw, static_length_context())
                .map(|_| Value::TextIndentRaw(raw.into())),
            "word-spacing" => {
                if raw.eq_ignore_ascii_case("normal") {
                    Some(Value::WordSpacing(LengthPercentage::default()))
                } else if let Some(percent) = raw.strip_suffix('%') {
                    percent
                        .trim()
                        .parse::<f32>()
                        .ok()
                        .filter(|value| value.is_finite())
                        .map(|value| {
                            Value::WordSpacing(LengthPercentage {
                                pixels: 0.0,
                                fraction: value / 100.0,
                            })
                        })
                } else {
                    parse_context_length(180, raw, false)
                }
            }
            "text-align" => match &*ascii_lower(raw) {
                "start" => Some(Value::TextAlign(TextAlign::Start)),
                "end" => Some(Value::TextAlign(TextAlign::End)),
                "left" => Some(Value::TextAlign(TextAlign::Left)),
                "right" => Some(Value::TextAlign(TextAlign::Right)),
                "center" => Some(Value::TextAlign(TextAlign::Center)),
                "justify" => Some(Value::TextAlign(TextAlign::Justify)),
                "match-parent" => Some(Value::TextAlign(TextAlign::MatchParent)),
                "justify-all" => Some(Value::TextAlign(TextAlign::JustifyAll)),
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
                "baseline" => Some(Value::AlignSelf(Some(AlignItems::Baseline))),
                _ => None,
            },
            "position" => match raw {
                "static" => Some(Value::Position(Position::Static)),
                "relative" => Some(Value::Position(Position::Relative)),
                "sticky" => Some(Value::Position(Position::Sticky)),
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
                "flow-root" => Some(Value::Display(Display::FlowRoot)),
                "inline" => Some(Value::Display(Display::Inline)),
                "inline-block" => Some(Value::Display(Display::InlineBlock)),
                "list-item" => Some(Value::Display(Display::ListItem)),
                "flex" => Some(Value::Display(Display::Flex)),
                "grid" => Some(Value::Display(Display::Grid)),
                "table" | "inline-table" => Some(Value::Display(Display::Table)),
                "table-caption" => Some(Value::Display(Display::TableCaption)),
                "table-row-group" | "table-header-group" | "table-footer-group" => {
                    Some(Value::Display(Display::TableRowGroup))
                }
                "table-row" => Some(Value::Display(Display::TableRow)),
                "table-cell" => Some(Value::Display(Display::TableCell)),
                "contents" => Some(Value::Display(Display::Contents)),
                "none" => Some(Value::Display(Display::None)),
                _ => None,
            },
            "list-style-type" => list_style_type(raw).map(Value::ListStyleType),
            "list-style-position" => list_style_position(raw).map(Value::ListStylePosition),
            "list-style-image" => list_style_image(raw).map(Value::ListStyleImage),
            "color" => {
                if raw.eq_ignore_ascii_case("currentcolor") {
                    Some(Value::Default(1, true))
                } else {
                    color_value(1, raw)
                }
            }
            "background" | "background-color" => {
                if raw.eq_ignore_ascii_case("currentcolor") {
                    Some(Value::BackgroundCurrentColor)
                } else {
                    color_value(2, raw)
                }
            }
            "column-count" => {
                if raw == "auto" {
                    Some(Value::ColumnCount(None))
                } else {
                    raw.parse::<usize>()
                        .ok()
                        .filter(|count| (1..=64).contains(count))
                        .map(|count| Value::ColumnCount(Some(count)))
                }
            }
            "column-gap" => gap_value(76, raw),
            "column-fill" => match raw {
                "balance" => Some(Value::ColumnFillAuto(false)),
                "auto" => Some(Value::ColumnFillAuto(true)),
                _ => None,
            },
            "column-rule-width" => match raw {
                "thin" => Some(Value::ColumnRuleWidth(1.0)),
                "medium" => Some(Value::ColumnRuleWidth(3.0)),
                "thick" => Some(Value::ColumnRuleWidth(5.0)),
                _ => nonnegative_length(raw).map(Value::ColumnRuleWidth),
            },
            "column-rule-color" => {
                if raw.eq_ignore_ascii_case("currentcolor") {
                    Some(Value::ColumnRuleColor(None))
                } else {
                    color(raw).map(|value| Value::ColumnRuleColor(Some(value)))
                }
            }
            "column-rule-style" => match raw {
                "none" | "hidden" => Some(Value::ColumnRuleStyle(false, None)),
                "solid" => Some(Value::ColumnRuleStyle(true, None)),
                "double" => Some(Value::ColumnRuleStyle(true, Some(BorderPattern::Double))),
                "dashed" => Some(Value::ColumnRuleStyle(true, Some(BorderPattern::Dashed))),
                "dotted" => Some(Value::ColumnRuleStyle(true, Some(BorderPattern::Dotted))),
                _ => None,
            },
            "border-top-width"
            | "border-right-width"
            | "border-bottom-width"
            | "border-left-width" => {
                let index = [
                    "border-top-width",
                    "border-right-width",
                    "border-bottom-width",
                    "border-left-width",
                ]
                .iter()
                .position(|property| *property == name)
                .unwrap_or(0);
                nonnegative_length(raw).map(|value| {
                    Value::LogicalBorder(115 + index, LogicalBorderComponent::Width(value))
                })
            }
            "border-top-color"
            | "border-right-color"
            | "border-bottom-color"
            | "border-left-color" => {
                let index = [
                    "border-top-color",
                    "border-right-color",
                    "border-bottom-color",
                    "border-left-color",
                ]
                .iter()
                .position(|property| *property == name)
                .unwrap_or(0);
                if raw.eq_ignore_ascii_case("currentcolor") {
                    Some(Value::LogicalBorder(
                        119 + index,
                        LogicalBorderComponent::CurrentColor,
                    ))
                } else {
                    color(raw).map(|value| {
                        Value::LogicalBorder(119 + index, LogicalBorderComponent::Color(value))
                    })
                }
            }
            "border-top-style"
            | "border-right-style"
            | "border-bottom-style"
            | "border-left-style" => {
                let index = [
                    "border-top-style",
                    "border-right-style",
                    "border-bottom-style",
                    "border-left-style",
                ]
                .iter()
                .position(|property| *property == name)
                .unwrap_or(0);
                BorderStyle::parse(raw).map(|style| {
                    Value::LogicalBorder(123 + index, LogicalBorderComponent::Style(style))
                })
            }
            "padding-inline-start" => nonnegative_length(raw).map(|v| Value::LogicalEdge(81, v)),
            "padding-inline-end" => nonnegative_length(raw).map(|v| Value::LogicalEdge(82, v)),
            "padding-block-start" => nonnegative_length(raw).map(|v| Value::LogicalEdge(83, v)),
            "padding-block-end" => nonnegative_length(raw).map(|v| Value::LogicalEdge(84, v)),
            "margin-inline-start" => length(raw).map(|v| Value::LogicalEdge(85, v)),
            "margin-inline-end" => length(raw).map(|v| Value::LogicalEdge(86, v)),
            "margin-block-start" => match raw {
                "auto" => Some(Value::LogicalMargin(99, None)),
                _ => length(raw).map(|v| Value::LogicalMargin(99, Some(v))),
            },
            "margin-block-end" => match raw {
                "auto" => Some(Value::LogicalMargin(100, None)),
                _ => length(raw).map(|v| Value::LogicalMargin(100, Some(v))),
            },
            "inset-inline-start" => match raw {
                "auto" => Some(Value::LogicalOffset(95, None)),
                _ => length(raw).map(|v| Value::LogicalOffset(95, Some(v))),
            },
            "inset-inline-end" => match raw {
                "auto" => Some(Value::LogicalOffset(96, None)),
                _ => length(raw).map(|v| Value::LogicalOffset(96, Some(v))),
            },
            "inset-block-start" => match raw {
                "auto" => Some(Value::LogicalOffset(97, None)),
                _ => length(raw).map(|v| Value::LogicalOffset(97, Some(v))),
            },
            "inset-block-end" => match raw {
                "auto" => Some(Value::LogicalOffset(98, None)),
                _ => length(raw).map(|v| Value::LogicalOffset(98, Some(v))),
            },
            "inline-size" => nonnegative_length(raw).map(|v| Value::LogicalSize(87, v)),
            "block-size" => nonnegative_length(raw).map(|v| Value::LogicalSize(88, v)),
            "min-inline-size" => {
                nonnegative_length(raw).map(|v| Value::LogicalConstraint(91, Some(v)))
            }
            "max-inline-size" => {
                if raw == "none" {
                    Some(Value::LogicalConstraint(92, None))
                } else {
                    nonnegative_length(raw).map(|v| Value::LogicalConstraint(92, Some(v)))
                }
            }
            "min-block-size" => {
                nonnegative_length(raw).map(|v| Value::LogicalConstraint(93, Some(v)))
            }
            "max-block-size" => {
                if raw == "none" {
                    Some(Value::LogicalConstraint(94, None))
                } else {
                    nonnegative_length(raw).map(|v| Value::LogicalConstraint(94, Some(v)))
                }
            }
            "writing-mode" => match raw {
                "horizontal-tb" => Some(Value::WritingMode(WritingMode::HorizontalTb)),
                "vertical-rl" => Some(Value::WritingMode(WritingMode::VerticalRl)),
                "vertical-lr" => Some(Value::WritingMode(WritingMode::VerticalLr)),
                _ => None,
            },
            "background-position" => background_position_value(raw),
            "background-size" => background_size_value(raw),
            "background-repeat" => background_repeats(raw).map(Value::BackgroundRepeats),
            "background-clip" => background_clip_boxes(raw).map(Value::BackgroundClip),
            "background-origin" => background_boxes(raw).map(Value::BackgroundOrigin),
            "width" => {
                if raw == "auto" {
                    Some(Value::Default(3, false))
                } else {
                    nonnegative_length(raw).map(Value::Width)
                }
            }
            "min-width" => {
                if raw == "auto" {
                    Some(Value::MinWidthAuto)
                } else {
                    nonnegative_length(raw).map(Value::MinWidth)
                }
            }
            "min-height" => {
                if raw == "auto" {
                    Some(Value::MinHeightAuto)
                } else {
                    nonnegative_length(raw).map(Value::MinHeight).or_else(|| {
                        intrinsic_sizing(raw).map(|value| Value::IntrinsicHeight(51, value))
                    })
                }
            }
            "max-height" => {
                if raw == "none" {
                    Some(Value::MaxHeight(None))
                } else {
                    nonnegative_length(raw)
                        .map(|v| Value::MaxHeight(Some(v)))
                        .or_else(|| {
                            intrinsic_sizing(raw).map(|value| Value::IntrinsicHeight(52, value))
                        })
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
                    nonnegative_length(raw).map(Value::Height).or_else(|| {
                        intrinsic_sizing(raw).map(|value| Value::IntrinsicHeight(4, value))
                    })
                }
            }
            "aspect-ratio" => {
                if raw == "auto" {
                    Some(Value::Default(73, false))
                } else {
                    ratio(raw).map(Value::AspectRatio)
                }
            }
            "z-index" => {
                if raw == "auto" {
                    Some(Value::Default(74, false))
                } else {
                    raw.parse::<i32>().ok().map(|v| Value::ZIndex(Some(v)))
                }
            }
            "font-size" => length(raw).filter(|v| *v > 0.0).map(Value::FontSize),
            "font-family" => font_families(raw).map(Value::FontFamily),
            "font-weight" => font_weight(raw).map(Value::FontWeight),
            "font-stretch" => font_stretch(&ascii_lower(raw)).map(Value::FontStretch),
            "font-size-adjust" => font_size_adjust(raw).map(Value::FontSizeAdjust),
            "font-style" => match raw {
                "normal" => Some(Value::FontStyle(FontStyle::Normal)),
                "italic" => Some(Value::FontStyle(FontStyle::Italic)),
                "oblique" => Some(Value::FontStyle(FontStyle::Oblique)),
                _ => None,
            },
            "color-scheme" => parse_color_scheme(raw).map(Value::ColorScheme),
            "fill" => parse_svg_paint(raw).map(Value::SvgFill),
            "stroke" => parse_svg_paint(raw).map(Value::SvgStroke),
            "stroke-width" => nonnegative_length(raw).map(Value::SvgStrokeWidth),
            "clip-path" => {
                if raw.eq_ignore_ascii_case("none") {
                    Some(Value::SvgClipPath(None))
                } else {
                    svg_local_fragment(raw).map(|reference| Value::SvgClipPath(Some(reference)))
                }
            }
            "stop-color" => {
                if raw.eq_ignore_ascii_case("currentcolor") {
                    Some(Value::SvgStopColor(None))
                } else {
                    color(raw).map(|color| Value::SvgStopColor(Some(color)))
                }
            }
            "stop-opacity" => css_scalar(raw, true)
                .filter(|value| value.is_finite())
                .map(|value| Value::SvgStopOpacity(value.clamp(0.0, 1.0))),
            "clip-rule" => {
                if raw.eq_ignore_ascii_case("nonzero") {
                    Some(Value::SvgClipRule(SvgFillRule::NonZero))
                } else if raw.eq_ignore_ascii_case("evenodd") {
                    Some(Value::SvgClipRule(SvgFillRule::EvenOdd))
                } else {
                    None
                }
            }
            "x" | "y" | "rx" | "ry" | "cx" | "cy" | "r" => svg_geometry_value(name, raw),
            "fill-rule" => {
                if raw.eq_ignore_ascii_case("nonzero") {
                    Some(Value::SvgFillRule(SvgFillRule::NonZero))
                } else if raw.eq_ignore_ascii_case("evenodd") {
                    Some(Value::SvgFillRule(SvgFillRule::EvenOdd))
                } else {
                    None
                }
            }
            "margin" => length(raw).map(Value::Margin),
            "padding" => nonnegative_length(raw).map(Value::Padding),
            "border-radius" => border_radius_values(raw, static_length_context())
                .map(|_| Value::BorderRadiusRaw(Arc::from(raw))),
            "border-width" => nonnegative_length(raw).map(Value::BorderWidth),
            "border-inline-start-width"
            | "border-inline-end-width"
            | "border-block-start-width"
            | "border-block-end-width" => {
                let index = [
                    "border-inline-start-width",
                    "border-inline-end-width",
                    "border-block-start-width",
                    "border-block-end-width",
                ]
                .iter()
                .position(|property| *property == name)
                .unwrap_or(0);
                nonnegative_length(raw).map(|value| {
                    Value::LogicalBorder(103 + index, LogicalBorderComponent::Width(value))
                })
            }
            "border-color" => {
                if raw.eq_ignore_ascii_case("currentcolor") {
                    Some(Value::BorderCurrentColor)
                } else {
                    color_value(10, raw)
                }
            }
            "border-inline-start-color"
            | "border-inline-end-color"
            | "border-block-start-color"
            | "border-block-end-color" => {
                let index = [
                    "border-inline-start-color",
                    "border-inline-end-color",
                    "border-block-start-color",
                    "border-block-end-color",
                ]
                .iter()
                .position(|property| *property == name)
                .unwrap_or(0);
                if raw.eq_ignore_ascii_case("currentcolor") {
                    Some(Value::LogicalBorder(
                        107 + index,
                        LogicalBorderComponent::CurrentColor,
                    ))
                } else {
                    color_value(107 + index, raw)
                }
            }
            "border-style" => BorderStyle::parse(raw).map(Value::BorderStyle),
            "border-inline-start-style"
            | "border-inline-end-style"
            | "border-block-start-style"
            | "border-block-end-style" => {
                let index = [
                    "border-inline-start-style",
                    "border-inline-end-style",
                    "border-block-start-style",
                    "border-block-end-style",
                ]
                .iter()
                .position(|property| *property == name)
                .unwrap_or(0);
                BorderStyle::parse(raw).map(|style| {
                    Value::LogicalBorder(111 + index, LogicalBorderComponent::Style(style))
                })
            }
            "overflow-x" => Overflow::parse(raw).map(|v| Value::OverflowAxis(12, v)),
            "overflow-y" => Overflow::parse(raw).map(|v| Value::OverflowAxis(132, v)),
            "background-attachment" => background_attachments(raw).map(Value::BackgroundAttachment),
            "visibility" => match raw {
                "visible" => Some(Value::Visibility(true)),
                "hidden" | "collapse" => Some(Value::Visibility(false)),
                _ => None,
            },
            "pointer-events" => match raw {
                "auto" => Some(Value::PointerEvents(true)),
                "none" => Some(Value::PointerEvents(false)),
                _ => None,
            },
            "empty-cells" => match raw {
                "show" => Some(Value::EmptyCellsHide(false)),
                "hide" => Some(Value::EmptyCellsHide(true)),
                _ => None,
            },
            "caption-side" => match raw {
                "top" => Some(Value::CaptionBottom(false)),
                "bottom" => Some(Value::CaptionBottom(true)),
                _ => None,
            },
            "vertical-align" => {
                let normalized = ascii_lower(raw);
                let value = match normalized.as_ref() {
                    "baseline" => Some(VerticalAlign::Baseline),
                    "sub" => Some(VerticalAlign::Sub),
                    "super" => Some(VerticalAlign::Super),
                    "text-top" => Some(VerticalAlign::TextTop),
                    "middle" => Some(VerticalAlign::Middle),
                    "top" => Some(VerticalAlign::Top),
                    "bottom" => Some(VerticalAlign::Bottom),
                    "text-bottom" => Some(VerticalAlign::TextBottom),
                    _ => background_length(raw, static_length_context()).map(VerticalAlign::Length),
                };
                value.map(Value::VerticalAlign)
            }
            "border-collapse" => match raw {
                "separate" => Some(Value::BorderCollapse(false)),
                "collapse" => Some(Value::BorderCollapse(true)),
                _ => None,
            },
            "contain" => {
                let tokens: Vec<_> = raw.split_ascii_whitespace().collect();
                match tokens.as_slice() {
                    ["none"] => Some(Value::Containment {
                        paint: false,
                        layout: false,
                        size: false,
                    }),
                    ["strict"] => Some(Value::Containment {
                        paint: true,
                        layout: true,
                        size: true,
                    }),
                    ["content"] => Some(Value::Containment {
                        paint: true,
                        layout: true,
                        size: false,
                    }),
                    values
                        if values.iter().all(|value| {
                            matches!(*value, "size" | "layout" | "style" | "paint")
                        }) =>
                    {
                        Some(Value::Containment {
                            paint: values.contains(&&"paint"),
                            layout: values.contains(&&"layout"),
                            size: values.contains(&&"size"),
                        })
                    }
                    _ => None,
                }
            }
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
                "baseline" => Some(Value::AlignItems(AlignItems::Baseline)),
                _ => None,
            },
            "row-gap" => gap_value(17, raw),
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
                .or_else(|| grid_line_spec(raw, 2).then(|| Value::GridColumnSpec(Arc::from(raw)))),
            "grid-row" => grid_placement(raw)
                .map(Value::GridRow)
                .or_else(|| grid_line_spec(raw, 2).then(|| Value::GridRowSpec(Arc::from(raw)))),
            "opacity" => css_scalar(raw, true).map(|v| Value::Opacity(v.clamp(0.0, 1.0))),
            "flex-grow" => css_scalar(raw, false)
                .filter(|value| *value >= 0.0 || math_function(raw))
                .map(|value| value.max(0.0))
                .map(Value::FlexGrow),
            "flex-shrink" => css_scalar(raw, false)
                .filter(|value| *value >= 0.0 || math_function(raw))
                .map(|value| value.max(0.0))
                .map(Value::FlexShrink),
            "table-layout" => match raw {
                "fixed" => Some(Value::TableFixed(true)),
                "auto" => Some(Value::TableFixed(false)),
                _ => None,
            },
            "border-spacing" => border_spacing_value(raw).map(Value::BorderSpacing),
            "flex-basis" => {
                if raw.eq_ignore_ascii_case("auto") {
                    Some(Value::FlexBasis(None))
                } else if raw.eq_ignore_ascii_case("content") {
                    Some(Value::FlexBasisContent)
                } else if let Some(value) = intrinsic_sizing(raw) {
                    Some(Value::FlexBasisIntrinsic(value))
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

/// Parses a stylesheet belonging to a shadow tree; its rules are scoped so
/// they apply only within that tree (plus `:host`/`::slotted` semantics).
pub fn parse_scoped(input: &str, scope: Option<NodeId>) -> Result<Vec<Rule>, CssError> {
    Ok(parse_scoped_stylesheet(input, scope)?.rules)
}

pub fn parse_scoped_stylesheet(
    input: &str,
    scope: Option<NodeId>,
) -> Result<ParsedStylesheets, CssError> {
    let mut parsed = parse_stylesheet(input)?;
    for rule in &mut parsed.rules {
        rule.scope = scope;
    }
    for keyframes in &mut parsed.keyframes {
        keyframes.scope = scope;
    }
    Ok(parsed)
}

pub fn parse(input: &str) -> Result<Vec<Rule>, CssError> {
    Ok(parse_stylesheet(input)?.rules)
}

/// Parse rules, font faces and cascade-layer declarations in one pass. The
/// returned layer table is shared by every rule and face from this sheet.
pub fn parse_stylesheet(input: &str) -> Result<ParsedStylesheets, CssError> {
    if input.len() > MAX_CSS_BYTES {
        return Err(CssError {
            offset: 0,
            message: "CSS input too large",
        });
    }
    let mut rules = Vec::new();
    let mut font_faces = Vec::new();
    let mut keyframes = Vec::new();
    let mut layers = Vec::new();
    parse_rules(
        input,
        &mut rules,
        &Arc::from([]),
        &Arc::from([]),
        None,
        &mut layers,
        &mut font_faces,
        &mut keyframes,
        0,
        &Arc::from([]),
        0,
    )?;
    let revision = css_source_revision(input);
    for face in &mut font_faces {
        face.source_revision = revision;
    }
    if rules.is_empty() && !layers.is_empty() {
        rules.push(Rule {
            scope: None,
            selector: parse_simple_selector("*", 0)?,
            declarations: Arc::from([]),
            unsupported_svg_properties: Arc::from([]),
            media: Arc::from([]),
            supports: Arc::from([]),
            source_url: None,
            layer: None,
            layers: Arc::from([]),
            layer_path: None,
        });
    }
    let layers: Arc<[String]> = layers.into();
    for rule in &mut rules {
        rule.layers = layers.clone();
    }
    for face in &mut font_faces {
        face.layers = layers.clone();
    }
    let mut parsed = ParsedStylesheets {
        rules,
        font_faces,
        keyframes,
        layers,
    };
    canonicalize_font_faces(core::slice::from_mut(&mut parsed));
    Ok(parsed)
}

/// Stable content key used to distinguish replacement stylesheet sources in
/// the shared face identity records.
pub fn stylesheet_source_revision(input: &str) -> u64 {
    css_source_revision(input)
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum FontFaceSource {
    Url(Arc<str>),
    Local(Arc<str>),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FontDisplay {
    #[default]
    Auto,
    Block,
    Swap,
    Fallback,
    Optional,
}

/// Validated CSS Font Loading descriptors shared by stylesheet parsing and
/// the native `FontFace` adapter. Values are stored in their matching form;
/// `get` returns CSSOM serialization rather than the original spelling.
#[derive(Clone, Debug, PartialEq)]
pub struct FontFaceDescriptors {
    pub family: Arc<str>,
    pub family_quoted: bool,
    pub sources: Arc<[FontFaceSource]>,
    pub weight: u16,
    pub weight_range: [u16; 2],
    pub weight_keyword: Option<Arc<str>>,
    pub stretch: f32,
    pub stretch_range: [f32; 2],
    pub stretch_keyword: Option<Arc<str>>,
    pub style: crate::paint::FontStyle,
    pub oblique_range: Option<[f32; 2]>,
    pub unicode_range: Option<Arc<str>>,
    pub display: FontDisplay,
    pub feature_settings: Arc<str>,
    pub variation_settings: Arc<str>,
    pub size_adjust: f32,
    pub ascent_override: Option<f32>,
    pub descent_override: Option<f32>,
    pub line_gap_override: Option<f32>,
}

impl Default for FontFaceDescriptors {
    fn default() -> Self {
        Self {
            family: Arc::from(""),
            family_quoted: false,
            sources: Arc::from([]),
            weight: 400,
            weight_range: [400, 400],
            weight_keyword: Some(Arc::from("normal")),
            stretch: 100.0,
            stretch_range: [100.0, 100.0],
            stretch_keyword: Some(Arc::from("normal")),
            style: crate::paint::FontStyle::Normal,
            oblique_range: None,
            unicode_range: None,
            display: FontDisplay::Auto,
            feature_settings: Arc::from("normal"),
            variation_settings: Arc::from("normal"),
            size_adjust: 100.0,
            ascent_override: None,
            descent_override: None,
            line_gap_override: None,
        }
    }
}

impl FontFaceDescriptors {
    /// Parse a required family and a list of validated FontFace descriptors.
    /// Unknown or invalid descriptor names are errors for the JS API; the CSS
    /// parser uses `set` one declaration at a time and discards invalid values.
    pub fn parse(family: &str, entries: &[(&str, &str)]) -> Result<Self, CssError> {
        let mut descriptors = Self::default();
        descriptors.set_family_argument(family)?;
        for (name, value) in entries {
            descriptors.set(name, value)?;
        }
        Ok(descriptors)
    }

    /// Return the canonical serialization of one supported descriptor.
    pub fn get(&self, name: &str) -> Option<String> {
        Some(match normalize_font_descriptor_name(name)?.as_str() {
            "family" => {
                if self.family_quoted {
                    serialize_css_string(&self.family)
                } else {
                    self.family.to_string()
                }
            }
            "src" => serialize_font_sources(&self.sources),
            "weight" if self.weight_range[0] == self.weight_range[1] => self
                .weight_keyword
                .as_deref()
                .map(String::from)
                .unwrap_or_else(|| serialize_weight_range(self.weight_range)),
            "weight" => serialize_weight_range(self.weight_range),
            "stretch" if self.stretch_range[0] == self.stretch_range[1] => self
                .stretch_keyword
                .as_deref()
                .map(String::from)
                .unwrap_or_else(|| serialize_stretch_range(self.stretch_range)),
            "stretch" => serialize_stretch_range(self.stretch_range),
            "style" => serialize_font_face_style(self.style, self.oblique_range),
            "unicode-range" => self
                .unicode_range
                .as_deref()
                .map(serialize_unicode_ranges)
                .unwrap_or_else(|| String::from("U+0-10FFFF")),
            "display" => String::from(match self.display {
                FontDisplay::Auto => "auto",
                FontDisplay::Block => "block",
                FontDisplay::Swap => "swap",
                FontDisplay::Fallback => "fallback",
                FontDisplay::Optional => "optional",
            }),
            "feature-settings" => self.feature_settings.to_string(),
            "variation-settings" => self.variation_settings.to_string(),
            "size-adjust" => serialize_percent(self.size_adjust),
            "ascent-override" => serialize_metric_override(self.ascent_override),
            "descent-override" => serialize_metric_override(self.descent_override),
            "line-gap-override" => serialize_metric_override(self.line_gap_override),
            _ => return None,
        })
    }

    /// Validate and store one descriptor. Both CSS descriptor names and
    /// FontFace dictionary names are accepted so callers share one grammar.
    pub fn set(&mut self, name: &str, value: &str) -> Result<(), CssError> {
        let Some(name) = normalize_font_descriptor_name(name) else {
            return Err(font_descriptor_error());
        };
        let value = value.trim();
        let invalid = || font_descriptor_error();
        match name.as_str() {
            "family" => {
                self.set_family_argument(value)?;
            }
            "src" => {
                let Some(sources) = parse_font_face_sources(value) else {
                    return Err(invalid());
                };
                self.sources = sources.into();
            }
            "weight" => {
                let Some(range) = parse_font_weight_range(value) else {
                    return Err(invalid());
                };
                self.weight_range = range;
                self.weight = range[0];
                let lowered = ascii_lower(value);
                self.weight_keyword = (range[0] == range[1]
                    && matches!(&*lowered, "normal" | "bold"))
                .then(|| Arc::from(lowered.as_ref()));
            }
            "stretch" => {
                let Some(range) = parse_font_stretch_range(value) else {
                    return Err(invalid());
                };
                self.stretch_range = range;
                self.stretch = range[0];
                let lowered = ascii_lower(value);
                self.stretch_keyword = (range[0] == range[1]
                    && font_stretch_keyword(&lowered).is_some())
                .then(|| Arc::from(lowered.as_ref()));
            }
            "style" => {
                let Some((style, range)) = parse_font_face_style(value) else {
                    return Err(invalid());
                };
                self.style = style;
                self.oblique_range = range;
            }
            "unicode-range" => {
                let Some(ranges) = parse_unicode_ranges(value) else {
                    return Err(invalid());
                };
                self.unicode_range = Some(serialize_unicode_range_values(&ranges).into());
            }
            "display" => {
                self.display = match &*ascii_lower(value) {
                    "auto" => FontDisplay::Auto,
                    "block" => FontDisplay::Block,
                    "swap" => FontDisplay::Swap,
                    "fallback" => FontDisplay::Fallback,
                    "optional" => FontDisplay::Optional,
                    _ => return Err(invalid()),
                };
            }
            "feature-settings" => {
                self.feature_settings = parse_font_settings(value, false)
                    .ok_or_else(invalid)?
                    .into();
            }
            "variation-settings" => {
                self.variation_settings =
                    parse_font_settings(value, true).ok_or_else(invalid)?.into();
            }
            "size-adjust" => {
                let Some(percent) = value
                    .strip_suffix('%')
                    .and_then(|part| part.trim().parse::<f32>().ok())
                    .filter(|number| number.is_finite() && *number >= 0.0)
                else {
                    return Err(invalid());
                };
                self.size_adjust = percent;
            }
            "ascent-override" | "descent-override" | "line-gap-override" => {
                let parsed = if value.eq_ignore_ascii_case("normal") {
                    None
                } else {
                    let Some(percent) = value
                        .strip_suffix('%')
                        .and_then(|part| part.trim().parse::<f32>().ok())
                        .filter(|number| number.is_finite() && *number >= 0.0)
                    else {
                        return Err(invalid());
                    };
                    Some(percent / 100.0)
                };
                match name.as_str() {
                    "ascent-override" => self.ascent_override = parsed,
                    "descent-override" => self.descent_override = parsed,
                    _ => self.line_gap_override = parsed,
                }
            }
            _ => return Err(invalid()),
        }
        Ok(())
    }

    /// FontFace.family accepts arbitrary strings and CSSOM quotes values that
    /// are not one ordinary non-generic family-name. CSS @font-face parsing
    /// uses the stricter CSS descriptor grammar instead.
    pub fn set_family_argument(&mut self, value: &str) -> Result<(), CssError> {
        if value.len() > 256 {
            return Err(font_descriptor_error());
        }
        let parsed = parse_font_family_list(value).filter(|families| families.len() == 1);
        let ordinary = parsed
            .as_ref()
            .and_then(|families| families.first())
            .is_some_and(|family| {
                family.as_ref() == value && can_serialize_unquoted_font_family(family)
            });
        self.family = if ordinary {
            parsed.unwrap()[0].clone()
        } else {
            Arc::from(value)
        };
        self.family_quoted = !ordinary;
        Ok(())
    }

    fn set_css_family(&mut self, value: &str) -> Result<(), CssError> {
        let families = parse_font_family_list(value).filter(|families| families.len() == 1);
        let Some(family) = families.and_then(|values| values.first().cloned()) else {
            return Err(font_descriptor_error());
        };
        let quoted = value.trim_start().starts_with(['\'', '"']);
        if is_generic_font_family(&family) && !quoted {
            return Err(font_descriptor_error());
        }
        self.family_quoted = !can_serialize_unquoted_font_family(&family);
        self.family = family;
        Ok(())
    }

    /// Convert descriptors into the same rule record used by CSS faces and
    /// font matching. A BufferSource-backed manual face deliberately has no
    /// URL/local sources; its bytes stay in the DOM provider record.
    pub fn to_rule(&self, source_url: Option<Arc<str>>) -> FontFaceRule {
        FontFaceRule {
            descriptors: self.clone(),
            identity: None,
            rule_start: 0,
            import_path: Arc::from([]),
            source_revision: 0,
            media: Arc::from([]),
            supports: Arc::from([]),
            source_url,
            source_order: 0,
            layer: None,
            layers: Arc::from([]),
            layer_path: None,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum FontFaceOwnerId {
    Element(NodeId),
    Adopted {
        scope: Option<NodeId>,
        stylesheet: u64,
        duplicate_index: usize,
    },
}

/// Stable address for a parsed CSS font-face rule within its live stylesheet
/// owner. The source signature changes when a stylesheet's source is replaced.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct FontFaceRuleId {
    pub owner: FontFaceOwnerId,
    pub sheet_revision: u64,
    pub source_url: Option<Arc<str>>,
    pub import_path: Arc<[usize]>,
    pub rule_start: usize,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum FontFaceIdentity {
    Css(FontFaceRuleId),
    Manual(u64),
}

/// CSS Fonts unicode-range grammar. Return a normalized union of inclusive
/// codepoint ranges; malformed descriptors are ignored by the caller.
pub fn parse_unicode_ranges(raw: &str) -> Option<Arc<[(u32, u32)]>> {
    let mut ranges = Vec::new();
    let hex = |text: &str| {
        if text.is_empty() || text.len() > 6 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(text, 16)
            .ok()
            .filter(|value| *value <= 0x10ffff)
    };
    for part in raw.split(',') {
        if ranges.len() == 256 {
            return None;
        }
        let part = part.trim();
        if !part.get(..2)?.eq_ignore_ascii_case("u+") {
            return None;
        }
        let value = &part[2..];
        let range = if let Some((first, last)) = value.split_once('-') {
            (hex(first)?, hex(last)?)
        } else if let Some(at) = value.find('?') {
            if value.len() > 6 || !value[at..].bytes().all(|byte| byte == b'?') {
                return None;
            }
            let first = value.replace('?', "0");
            let last = value.replace('?', "f");
            (hex(&first)?, hex(&last)?)
        } else {
            let value = hex(value)?;
            (value, value)
        };
        if range.0 > range.1 {
            return None;
        }
        ranges.push(range);
    }
    ranges.sort_unstable();
    let mut union: Vec<(u32, u32)> = Vec::new();
    for (first, last) in ranges {
        if let Some(previous) = union.last_mut().filter(|previous| first <= previous.1 + 1) {
            previous.1 = previous.1.max(last);
        } else {
            union.push((first, last));
        }
    }
    Some(union.into())
}

fn font_descriptor_error() -> CssError {
    CssError {
        offset: 0,
        message: "invalid font-face descriptor",
    }
}

fn css_source_revision(input: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in input.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn normalize_font_descriptor_name(name: &str) -> Option<String> {
    if name.is_empty() || name.len() > 64 {
        return None;
    }
    let mut normalized = String::with_capacity(name.len() + 8);
    for ch in name.chars() {
        if ch.is_ascii_uppercase() {
            normalized.push('-');
            normalized.push(ch.to_ascii_lowercase());
        } else if ch.is_ascii_alphanumeric() || ch == '-' {
            normalized.push(ch.to_ascii_lowercase());
        } else {
            return None;
        }
    }
    let normalized = normalized.strip_prefix("font-").unwrap_or(&normalized);
    Some(match normalized {
        "font-family" | "family" => String::from("family"),
        "src" => String::from("src"),
        "font-weight" | "weight" => String::from("weight"),
        "font-stretch" | "font-width" | "stretch" | "width" => String::from("stretch"),
        "font-style" | "style" => String::from("style"),
        "unicode-range" | "unicoderange" => String::from("unicode-range"),
        "font-display" | "display" => String::from("display"),
        "font-feature-settings" | "feature-settings" | "featuresettings" => {
            String::from("feature-settings")
        }
        "font-variation-settings" | "variation-settings" | "variationsettings" => {
            String::from("variation-settings")
        }
        "size-adjust" | "sizeadjust" => String::from("size-adjust"),
        "ascent-override" | "ascentoverride" => String::from("ascent-override"),
        "descent-override" | "descentoverride" => String::from("descent-override"),
        "line-gap-override" | "linegap-override" | "linegapoverride" => {
            String::from("line-gap-override")
        }
        _ => return None,
    })
}

fn serialize_css_string(value: &str) -> String {
    let mut serialized = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' | '"' => {
                serialized.push('\\');
                serialized.push(ch);
            }
            '\n' => serialized.push_str("\\a "),
            '\r' => serialized.push_str("\\d "),
            '\u{c}' => serialized.push_str("\\c "),
            ch if (ch as u32) < 0x20 || ch == '\u{7f}' => {
                serialized.push_str(&alloc::format!("\\{:x} ", ch as u32));
            }
            ch => serialized.push(ch),
        }
    }
    serialized.push('"');
    serialized
}

fn serialize_font_sources(sources: &[FontFaceSource]) -> String {
    sources
        .iter()
        .map(|source| match source {
            FontFaceSource::Url(url) => alloc::format!("url({})", serialize_css_string(url)),
            FontFaceSource::Local(name) => {
                alloc::format!("local({})", serialize_css_string(name))
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_font_face_sources(raw: &str) -> Option<Vec<FontFaceSource>> {
    let parts = top_level_split_bounded(raw, b',', 64, MAX_CSS_BYTES)?;
    let mut sources = Vec::new();
    for part in parts {
        let tokens = components_bounded(part, MAX_CSS_BYTES)?;
        let token = *tokens.first()?;
        if let Some(url) = background_url(token) {
            sources.push(FontFaceSource::Url(url));
            continue;
        }
        let Some((function, contents)) = token.split_once('(') else {
            continue;
        };
        if !function.eq_ignore_ascii_case("local") {
            continue;
        }
        let Some(contents) = contents.strip_suffix(')') else {
            continue;
        };
        let names = font_families(contents)?;
        if names.len() == 1 {
            sources.push(FontFaceSource::Local(names[0].clone()));
        }
    }
    (!sources.is_empty()).then_some(sources)
}

fn parse_font_weight_range(raw: &str) -> Option<[u16; 2]> {
    let values = components(raw)?;
    if !(1..=2).contains(&values.len()) {
        return None;
    }
    let parse = |value: &str| {
        let weight = font_weight(&ascii_lower(value))?;
        (weight > 0).then_some(weight as u16)
    };
    let first = parse(values[0])?;
    let last = values.get(1).map_or(Some(first), |value| parse(value))?;
    (first <= last).then_some([first, last])
}

fn parse_font_stretch_range(raw: &str) -> Option<[f32; 2]> {
    let values = components(raw)?;
    if !(1..=2).contains(&values.len()) {
        return None;
    }
    let first = font_stretch(values[0])?;
    let last = values
        .get(1)
        .map_or(Some(first), |value| font_stretch(value))?;
    (first <= last).then_some([first, last])
}

fn parse_angle_degrees(raw: &str) -> Option<f32> {
    let raw = raw.trim();
    let (number, unit) = ["deg", "grad", "rad", "turn"]
        .into_iter()
        .find_map(|unit| raw.strip_suffix(unit).map(|number| (number.trim(), unit)))?;
    let value = number.parse::<f32>().ok()?;
    if !value.is_finite() {
        return None;
    }
    let degrees = match unit {
        "deg" => value,
        "grad" => value * 0.9,
        "rad" => value.to_degrees(),
        "turn" => value * 360.0,
        _ => return None,
    };
    (degrees.is_finite() && (-90.0..=90.0).contains(&degrees)).then_some(degrees)
}

fn parse_font_face_style(raw: &str) -> Option<(crate::paint::FontStyle, Option<[f32; 2]>)> {
    let values = components(raw)?;
    match values.as_slice() {
        [value] if value.eq_ignore_ascii_case("normal") => {
            Some((crate::paint::FontStyle::Normal, None))
        }
        [value] if value.eq_ignore_ascii_case("italic") => {
            Some((crate::paint::FontStyle::Italic, None))
        }
        [value] if value.eq_ignore_ascii_case("oblique") => {
            Some((crate::paint::FontStyle::Oblique, None))
        }
        [kind, start] if kind.eq_ignore_ascii_case("oblique") => {
            let start = parse_angle_degrees(start)?;
            Some((crate::paint::FontStyle::Oblique, Some([start, start])))
        }
        [kind, start, end] if kind.eq_ignore_ascii_case("oblique") => {
            let range = [parse_angle_degrees(start)?, parse_angle_degrees(end)?];
            (range[0] <= range[1]).then_some((crate::paint::FontStyle::Oblique, Some(range)))
        }
        _ => None,
    }
}

fn serialize_weight_range(range: [u16; 2]) -> String {
    if range[0] == range[1] {
        range[0].to_string()
    } else {
        alloc::format!("{} {}", range[0], range[1])
    }
}

fn serialize_stretch_range(range: [f32; 2]) -> String {
    if range[0] == range[1] {
        serialize_percent(range[0])
    } else {
        alloc::format!(
            "{} {}",
            serialize_percent(range[0]),
            serialize_percent(range[1])
        )
    }
}

fn serialize_font_face_style(
    style: crate::paint::FontStyle,
    oblique_range: Option<[f32; 2]>,
) -> String {
    match style {
        crate::paint::FontStyle::Normal => String::from("normal"),
        crate::paint::FontStyle::Italic => String::from("italic"),
        crate::paint::FontStyle::Oblique => match oblique_range {
            None => String::from("oblique"),
            Some([start, end]) if start == end => alloc::format!("oblique {start}deg"),
            Some([start, end]) => alloc::format!("oblique {start}deg {end}deg"),
        },
    }
}

fn serialize_percent(value: f32) -> String {
    alloc::format!("{value}%")
}

fn serialize_metric_override(value: Option<f32>) -> String {
    value.map_or_else(
        || String::from("normal"),
        |value| {
            // Metric overrides are stored as ratios; a decimal percentage such
            // as 30% becomes the nearest f32 to 0.3. Rounding the displayed
            // percentage avoids leaking that binary representation as
            // 30.000002% in CSSOM serialization.
            let percent = ((f64::from(value) * 100.0) * 100_000.0).round() / 100_000.0;
            alloc::format!("{percent}%")
        },
    )
}

fn serialize_unicode_range_values(ranges: &[(u32, u32)]) -> String {
    ranges
        .iter()
        .map(|(first, last)| {
            if first == last {
                alloc::format!("U+{first:X}")
            } else {
                alloc::format!("U+{first:X}-{last:X}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn serialize_unicode_ranges(raw: &str) -> String {
    parse_unicode_ranges(raw)
        .map(|ranges| serialize_unicode_range_values(&ranges))
        .unwrap_or_else(|| raw.to_string())
}

fn parse_font_settings(raw: &str, variation: bool) -> Option<String> {
    if raw.eq_ignore_ascii_case("normal") {
        return Some(String::from("normal"));
    }
    let parts = comma_components(raw, 64)?;
    let mut settings = Vec::with_capacity(parts.len());
    for part in parts {
        let tokens = components(part)?;
        if tokens.is_empty() || tokens.len() > 2 || (variation && tokens.len() != 2) {
            return None;
        }
        let tag = css_string(tokens[0])?;
        if tag.len() != 4 || !tag.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
            return None;
        }
        let serialized_value = if variation {
            let value = tokens[1].parse::<f32>().ok()?;
            value.is_finite().then(|| alloc::format!("{value}"))?
        } else {
            let value = tokens.get(1).copied().unwrap_or("1");
            match &*ascii_lower(value) {
                "on" => String::from("1"),
                "off" => String::from("0"),
                _ => {
                    let value = value.parse::<u32>().ok()?;
                    (value <= u16::MAX as u32).then(|| value.to_string())?
                }
            }
        };
        let tag = serialize_css_string(&tag);
        if !variation && serialized_value == "1" {
            settings.push(tag);
        } else {
            settings.push(alloc::format!("{tag} {serialized_value}"));
        }
    }
    (!settings.is_empty()).then(|| settings.join(", "))
}

/// Font descriptors use the same stylesheet scanner, CSS strings and URL
/// escaping as ordinary author rules. The host resolves the sources; parsing
/// never performs filesystem or network operations.
#[derive(Clone, Debug, PartialEq)]
pub struct FontFaceRule {
    pub descriptors: FontFaceDescriptors,
    pub identity: Option<FontFaceIdentity>,
    pub rule_start: usize,
    pub import_path: Arc<[usize]>,
    pub source_revision: u64,
    pub media: Arc<[Arc<str>]>,
    pub supports: Arc<[Arc<str>]>,
    pub source_url: Option<Arc<str>>,
    pub source_order: usize,
    pub layer: Option<usize>,
    pub layers: Arc<[String]>,
    pub layer_path: Option<[usize; 8]>,
}

impl FontFaceRule {
    pub fn applies(&self, environment: MediaEnvironment) -> bool {
        self.media
            .iter()
            .all(|query| media_query_matches(query, environment))
            && self
                .supports
                .iter()
                .all(|condition| supports_condition(condition))
    }
}

impl core::ops::Deref for FontFaceRule {
    type Target = FontFaceDescriptors;

    fn deref(&self) -> &Self::Target {
        &self.descriptors
    }
}

impl core::ops::DerefMut for FontFaceRule {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.descriptors
    }
}

/// Return the CSS `@font-face` records that participate in a font request,
/// ordered by family preference, descriptor match, normal layer order, and
/// source order. Unicode-range filtering is performed before source loading,
/// so font-loading APIs do not fetch faces that cannot cover the requested
/// text.
pub fn matching_font_faces(spec: &FontSpec, faces: &[FontFaceRule], text: &str) -> Vec<usize> {
    if text.is_empty() {
        return Vec::new();
    }
    let Some(families) = spec.families.as_deref() else {
        return Vec::new();
    };
    let mut selected = Vec::new();
    for family in families.iter() {
        let mut candidates: Vec<(FontMatchRank, [usize; 8], usize, usize, u16, f32)> = faces
            .iter()
            .enumerate()
            .filter(|(_, face)| face.family.eq_ignore_ascii_case(family))
            .filter_map(|(index, face)| {
                let (rank, matched_weight, matched_stretch) = font_face_match_rank(spec, face)?;
                let layer = face.layer_path.unwrap_or([usize::MAX - 1; 8]);
                Some((
                    rank,
                    layer,
                    face.source_order,
                    index,
                    matched_weight,
                    matched_stretch,
                ))
            })
            .collect();
        candidates.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                // Later normal layers and then later source rules win ties.
                .then_with(|| right.1.cmp(&left.1))
                .then_with(|| right.2.cmp(&left.2))
                .then_with(|| right.3.cmp(&left.3))
        });
        let Some((best_rank, _, _, best_index, best_weight, best_stretch)) =
            candidates.first().copied()
        else {
            continue;
        };
        for (rank, _, _, index, matched_weight, matched_stretch) in candidates {
            let face = &faces[index];
            if rank == best_rank
                && matched_weight == best_weight
                && matched_stretch == best_stretch
                && face.style == faces[best_index].style
                && font_face_covers_text(face, text)
                && !selected.contains(&index)
            {
                selected.push(index);
            }
        }
    }
    selected
}

/// Select faces demanded by rendered text, in first-use order.
///
/// `available` observes source/display failure and decoded glyph coverage for
/// one Unicode scalar. An unresolved source remains available: its loaded
/// fallback may render while it loads, without demanding another family.
/// The callback must not start requests; only the returned faces are demanded.
pub fn rendering_font_faces(
    spec: &FontSpec,
    faces: &[FontFaceRule],
    text: &str,
    mut available: impl FnMut(&FontFaceRule, &str) -> bool,
) -> Vec<usize> {
    let mut selected = Vec::new();
    for (start, character) in text.char_indices() {
        let piece = &text[start..start + character.len_utf8()];
        for index in matching_font_faces(spec, faces, piece) {
            if !available(&faces[index], piece) {
                continue;
            }
            if !selected.contains(&index) {
                selected.push(index);
            }
            break;
        }
    }
    selected
}

fn font_face_match_rank(spec: &FontSpec, face: &FontFaceRule) -> Option<(FontMatchRank, u16, f32)> {
    crate::paint::font_match_range_rank(spec, face.style, face.weight_range, face.stretch_range)
}

fn font_face_covers_text(face: &FontFaceRule, text: &str) -> bool {
    let Some(raw) = &face.unicode_range else {
        return true;
    };
    let Some(ranges) = parse_unicode_ranges(raw) else {
        return false;
    };
    text.chars().any(|ch| {
        let codepoint = ch as u32;
        ranges
            .iter()
            .any(|&(first, last)| first <= codepoint && codepoint <= last)
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImportLayer {
    Anonymous,
    Named(Arc<str>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportRule {
    pub span: Range<usize>,
    /// Decoded URL string, before resolution against the declaring sheet.
    pub url: Arc<str>,
    pub layer: Option<ImportLayer>,
    pub supports: Option<Arc<str>>,
    pub media: Option<Arc<str>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StylesheetSource {
    pub url: Arc<str>,
    pub text: Arc<str>,
    pub imports: Vec<LoadedImport>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoadedImport {
    pub rule: ImportRule,
    /// `None` represents a failed load or a cycle edge; the import rule still
    /// contributes its conditional layer declaration when applicable.
    pub source: Option<Box<StylesheetSource>>,
}

#[derive(Clone, Debug)]
pub struct ParsedStylesheets {
    pub rules: Vec<Rule>,
    pub font_faces: Vec<FontFaceRule>,
    pub keyframes: Vec<KeyframesRuleText>,
    pub layers: Arc<[String]>,
}

/// Resolve an import URL with the shared URL implementation.
pub fn resolve_import_url(rule: &ImportRule, source_url: &str) -> Option<Arc<str>> {
    lumen_common::url::parse(&rule.url, Some(source_url))
        .ok()
        .map(|url| Arc::from(url.href()))
}

/// Whether an import's supports and media conditions permit fetching it in
/// the current environment. Parsed graph rules retain their media condition
/// for reevaluation when the environment changes.
pub fn import_conditions_match(rule: &ImportRule, environment: MediaEnvironment) -> bool {
    rule.supports.as_deref().is_none_or(supports_condition)
        && rule
            .media
            .as_deref()
            .is_none_or(|media| media_query_matches(media, environment))
}

fn import_keyword_end(input: &str, position: usize, keyword: &str) -> Option<usize> {
    let end = position.checked_add(keyword.len())?;
    if !input.get(position..end)?.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let following = input.as_bytes().get(end).copied();
    if following.is_some_and(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-') || byte >= 0x80
    }) {
        None
    } else {
        Some(end)
    }
}

fn valid_import_layer_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('.').all(|part| !part.is_empty())
        && !name.bytes().any(|byte| {
            is_css_whitespace(byte) || matches!(byte, b'(' | b')' | b',' | b'/' | b'"' | b'\'')
        })
        && !matches!(
            name.to_ascii_lowercase().as_str(),
            "initial" | "inherit" | "unset" | "revert" | "revert-layer"
        )
}

fn valid_import_supports(condition: &str) -> bool {
    parse_supports_condition(condition, 0).is_some()
        || supports_condition_declaration(condition).is_some()
}

fn parse_import_prelude(prelude: &str, span: Range<usize>) -> Option<ImportRule> {
    let mut position = 0;
    skip_css_space_comments(prelude, &mut position)?;
    if prelude.as_bytes().get(position) != Some(&b'@') {
        return None;
    }
    position += 1;
    position = import_keyword_end(prelude, position, "import")?;
    skip_css_space_comments(prelude, &mut position)?;

    let url = match prelude.as_bytes().get(position).copied()? {
        b'\'' | b'"' => {
            let end = quoted_css_end(prelude, position)?;
            let value = css_string(&prelude[position..end])?;
            position = end;
            Arc::from(value)
        }
        _ => {
            let end = import_keyword_end(prelude, position, "url")?;
            if prelude.as_bytes().get(end) != Some(&b'(') {
                return None;
            }
            let close = matching_css_block(prelude, end)?;
            let value = background_url(&prelude[position..close])?;
            position = close;
            value
        }
    };

    skip_css_space_comments(prelude, &mut position)?;
    let mut layer = None;
    if let Some(end) = import_keyword_end(prelude, position, "layer") {
        if prelude.as_bytes().get(end) == Some(&b'(') {
            let close = matching_css_block(prelude, end)?;
            let name = prelude[end + 1..close - 1].trim();
            layer = Some(if name.is_empty() {
                ImportLayer::Anonymous
            } else {
                if !valid_import_layer_name(name) {
                    return None;
                }
                ImportLayer::Named(Arc::from(name))
            });
            position = close;
        } else {
            layer = Some(ImportLayer::Anonymous);
            position = end;
        }
        skip_css_space_comments(prelude, &mut position)?;
    }

    let mut supports = None;
    if let Some(end) = import_keyword_end(prelude, position, "supports") {
        if prelude.as_bytes().get(end) == Some(&b'(') {
            let close = matching_css_block(prelude, end)?;
            let condition = prelude[end + 1..close - 1].trim();
            if !valid_import_supports(condition) {
                return None;
            }
            supports = Some(Arc::from(condition));
            position = close;
            skip_css_space_comments(prelude, &mut position)?;
        }
    }

    let media = prelude[position..].trim();
    Some(ImportRule {
        span,
        url,
        layer,
        supports,
        media: (!media.is_empty()).then(|| Arc::from(media)),
    })
}

fn is_at_rule(prelude: &str, name: &str) -> bool {
    let prelude = prelude.trim_start();
    prelude
        .strip_prefix('@')
        .and_then(|rest| import_keyword_end(rest, 0, name))
        .is_some()
}

fn is_layer_statement(prelude: &str) -> bool {
    let prelude = prelude.trim_start();
    let Some(rest) = prelude.strip_prefix('@') else {
        return false;
    };
    import_keyword_end(rest, 0, "layer").is_some_and(|end| {
        rest[end..].starts_with(char::is_whitespace) || rest[end..].starts_with("/*")
    })
}

/// Extract valid top-level imports in their original source order. The spans
/// include each `@import` statement and its semicolon so graph construction
/// can validate loader records without rebuilding or rewriting source text.
pub fn imports(input: &str) -> Result<Vec<ImportRule>, CssError> {
    if input.len() > MAX_CSS_BYTES {
        return Err(CssError {
            offset: 0,
            message: "CSS input too large",
        });
    }
    let mut found = Vec::new();
    let (mut position, mut imports_open, mut saw_import) = (0, true, false);
    while position < input.len() {
        if skip_css_space_comments(input, &mut position).is_none() {
            break;
        }
        let rest = &input[position..];
        if rest.trim().is_empty() {
            break;
        }
        if rest.starts_with("<!--") || rest.starts_with("-->") {
            position += if rest.starts_with("<!--") { 4 } else { 3 };
            continue;
        }
        match rule_boundary(rest) {
            Some(RuleBoundary::Statement(end)) => {
                let prelude = rest[..end].trim();
                if is_at_rule(prelude, "import") {
                    if imports_open {
                        let span = position..position + end + 1;
                        if let Some(rule) = parse_import_prelude(prelude, span) {
                            found.push(rule);
                            if found.len() > MAX_CSS_GRAPH_IMPORTS {
                                return Err(CssError {
                                    offset: position,
                                    message: "too many CSS imports",
                                });
                            }
                        } else {
                            imports_open = false;
                        }
                    }
                    saw_import = true;
                } else if is_at_rule(prelude, "charset") && !saw_import {
                    // @charset may precede @import.
                } else if is_layer_statement(prelude) && !saw_import {
                    // Cascade Level 5 permits empty layer statements before imports.
                } else {
                    imports_open = false;
                }
                position += end + 1;
            }
            Some(RuleBoundary::Block(open)) => {
                imports_open = false;
                if let Some(end) = matching_css_block(rest, open) {
                    position += end;
                } else {
                    break;
                }
            }
            Some(RuleBoundary::Discard(end)) => {
                imports_open = false;
                position += end;
            }
            None => break,
        }
    }
    Ok(found)
}

/// Split a media query list using CSS component boundaries.
pub fn media_query_list_items(input: &str) -> Vec<String> {
    if input.trim().is_empty() {
        return Vec::new();
    }
    top_level_split(input, b',', 256)
        .unwrap_or_default()
        .into_iter()
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(String::from)
        .collect()
}

/// Split a CSS comma-list while respecting strings and nested functions.
pub fn css_list_items(input: &str) -> Vec<String> {
    if input.trim().is_empty() {
        return Vec::new();
    }
    top_level_split(input, b',', 64)
        .unwrap_or_default()
        .into_iter()
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(String::from)
        .collect()
}

/// Serialize a parsed import using its existing condition and layer data.
pub fn serialize_import_rule(rule: &ImportRule) -> String {
    let mut output = alloc::format!("@import {}", serialize_css_string(&rule.url));
    if let Some(layer) = &rule.layer {
        match layer {
            ImportLayer::Anonymous => output.push_str(" layer"),
            ImportLayer::Named(name) => {
                output.push_str(" layer(");
                output.push_str(
                    &name
                        .split('.')
                        .map(serialize_css_identifier)
                        .collect::<Vec<_>>()
                        .join("."),
                );
                output.push(')');
            }
        }
    }
    if let Some(supports) = &rule.supports {
        output.push_str(" supports(");
        output.push_str(supports);
        output.push(')');
    }
    if let Some(media) = &rule.media {
        if !media.is_empty() {
            output.push(' ');
            output.push_str(media);
        }
    }
    output.push(';');
    output
}

/// One parsed CSS keyframe block, with the selectors and declaration text
/// serialized through the shared CSS parser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyframeRuleText {
    pub key_text: String,
    pub css_text: String,
    pub style: String,
}

/// Parsed `@keyframes` rule used by CSSOM. The normal cascade parser currently
/// ignores keyframes, but CSSOM and stylesheet editing share this grammar.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyframesRuleText {
    pub name: String,
    pub name_text: String,
    pub css_text: String,
    pub rules: Vec<KeyframeRuleText>,
    pub source_order: usize,
    pub source_url: Option<Arc<str>>,
    pub media: Arc<[Arc<str>]>,
    pub supports: Arc<[Arc<str>]>,
    pub scope: Option<NodeId>,
}

/// Serialize keyframe data using the parsed name token and declarations.
pub fn serialize_keyframes_rule(name_text: &str, rules: &[KeyframeRuleText]) -> String {
    let body = rules
        .iter()
        .map(|rule| rule.css_text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    alloc::format!("@keyframes {name_text} {{ {body} }}")
}

fn parse_keyframe_key_text(input: &str) -> Option<String> {
    let mut selectors = Vec::new();
    for selector in input.split(',') {
        let selector = selector.trim();
        if selector.eq_ignore_ascii_case("from") {
            selectors.push("from".to_owned());
        } else if selector.eq_ignore_ascii_case("to") {
            selectors.push("to".to_owned());
        } else {
            let percent = selector.strip_suffix('%')?.trim().parse::<f32>().ok()?;
            if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
                return None;
            }
            selectors.push(alloc::format!("{}%", radius_number(percent)));
        }
    }
    (!selectors.is_empty()).then(|| selectors.join(", "))
}

/// Resolve a serialized keyframe selector list to normalized offsets.
pub fn keyframe_offsets(input: &str) -> Option<Vec<f64>> {
    let canonical = parse_keyframe_key_text(input)?;
    canonical
        .split(',')
        .map(|selector| match selector.trim() {
            "from" => Some(0.0),
            "to" => Some(1.0),
            value => Some(value.strip_suffix('%')?.parse::<f64>().ok()? / 100.0),
        })
        .collect()
}

/// Parse keyframe declarations with the normal CSS declaration grammar.
pub fn parse_keyframe_declarations(input: &str) -> Result<Vec<(String, String)>, CssError> {
    let mut output = Vec::new();
    for (start, end) in declaration_spans_with_recovery(input, true)? {
        let Some((name, value)) = declaration_pair(&input[start..end]) else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let (value, _) = important_value(value);
        if name.starts_with("animation-") || name == "animation" {
            continue;
        }
        if !declarations(&alloc::format!("{name}:{value}"), start)?.is_empty() {
            output.push((name, value.trim().to_owned()));
        }
    }
    Ok(output)
}

/// Parse a single keyframe rule using the common rule/declaration grammar.
pub fn parse_keyframe_rule(input: &str) -> Result<KeyframeRuleText, CssError> {
    let input = input.trim();
    let error = |message| CssError { offset: 0, message };
    let Some(RuleBoundary::Block(open)) = rule_boundary(input) else {
        return Err(error("expected keyframe rule block"));
    };
    let Some(close) = matching_css_block(input, open) else {
        return Err(error("unterminated keyframe rule"));
    };
    if !input[close..].trim().is_empty() {
        return Err(error("extra input after keyframe rule"));
    }
    let key_text = parse_keyframe_key_text(&input[..open])
        .ok_or_else(|| error("invalid keyframe selector"))?;
    let style = cssom_declaration_text(&input[open + 1..close - 1]);
    Ok(KeyframeRuleText {
        css_text: alloc::format!("{key_text} {{ {style} }}"),
        key_text,
        style,
    })
}

/// Parse one `@keyframes` rule. Other rule types return `None`.
pub fn parse_keyframes_rule(input: &str) -> Result<Option<KeyframesRuleText>, CssError> {
    let input = input.trim();
    let Some(RuleBoundary::Block(open)) = rule_boundary(input) else {
        return Ok(None);
    };
    let prelude = input[..open].trim();
    let Some(prefix) = prelude.get(..10) else {
        return Ok(None);
    };
    if !prefix.eq_ignore_ascii_case("@keyframes") {
        return Ok(None);
    }
    let raw_name = prelude.get(10..).unwrap_or("").trim();
    if !prelude
        .as_bytes()
        .get(10)
        .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b'/')
    {
        return Err(CssError {
            offset: 10,
            message: "missing keyframes name separator",
        });
    }
    let name = if (raw_name.starts_with('\'') && raw_name.ends_with('\''))
        || (raw_name.starts_with('"') && raw_name.ends_with('"'))
    {
        css_string(raw_name).ok_or_else(|| CssError {
            offset: 0,
            message: "invalid keyframes name",
        })?
    } else {
        let mut position = 0;
        let name =
            consume_selector_identifier(raw_name, &mut position).ok_or_else(|| CssError {
                offset: 0,
                message: "invalid keyframes name",
            })?;
        if position != raw_name.len() {
            return Err(CssError {
                offset: 0,
                message: "invalid keyframes name",
            });
        }
        name
    };
    let Some(close) = matching_css_block(input, open) else {
        return Err(CssError {
            offset: open,
            message: "unterminated keyframes rule",
        });
    };
    if !input[close..].trim().is_empty() {
        return Err(CssError {
            offset: close,
            message: "extra input after keyframes rule",
        });
    }
    let body = &input[open + 1..close - 1];
    let mut rules = Vec::new();
    let mut position = 0;
    while position < body.len() {
        if skip_css_space_comments(body, &mut position).is_none() || position >= body.len() {
            break;
        }
        let Some(RuleBoundary::Block(frame_open)) = rule_boundary(&body[position..]) else {
            return Err(CssError {
                offset: open + 1 + position,
                message: "expected keyframe rule",
            });
        };
        let Some(frame_end) = matching_css_block(&body[position..], frame_open) else {
            return Err(CssError {
                offset: open + 1 + position,
                message: "unterminated keyframe rule",
            });
        };
        let frame = parse_keyframe_rule(&body[position..position + frame_end])?;
        rules.push(frame);
        position += frame_end;
    }
    Ok(Some(KeyframesRuleText {
        css_text: input.to_owned(),
        name,
        name_text: raw_name.to_owned(),
        rules,
        source_order: 0,
        source_url: None,
        media: Arc::from([]),
        supports: Arc::from([]),
        scope: None,
    }))
}

struct StylesheetGraphParser {
    rules: Vec<Rule>,
    font_faces: Vec<FontFaceRule>,
    keyframes: Vec<KeyframesRuleText>,
    layers: Vec<String>,
    active_urls: Vec<Arc<str>>,
    bytes: usize,
    sheets: usize,
    import_count: usize,
}

impl StylesheetGraphParser {
    fn parse_source(
        &mut self,
        source: &StylesheetSource,
        media: &Arc<[Arc<str>]>,
        supports: &Arc<[Arc<str>]>,
        parent_layer: Option<usize>,
        import_path: &[usize],
        depth: usize,
    ) -> Result<(), CssError> {
        if depth >= MAX_CSS_GRAPH_DEPTH {
            return Err(CssError {
                offset: 0,
                message: "CSS import depth limit",
            });
        }
        if self.active_urls.iter().any(|url| url == &source.url) {
            return Ok(());
        }
        self.sheets += 1;
        self.bytes = self.bytes.saturating_add(source.text.len());
        if self.sheets > MAX_CSS_GRAPH_SHEETS || self.bytes > MAX_CSS_BYTES {
            return Err(CssError {
                offset: 0,
                message: "CSS import graph too large",
            });
        }

        let expected = imports(&source.text)?;
        self.import_count = self.import_count.saturating_add(expected.len());
        if self.import_count > MAX_CSS_GRAPH_IMPORTS || expected.len() != source.imports.len() {
            return Err(CssError {
                offset: 0,
                message: "invalid CSS import graph records",
            });
        }
        if expected
            .iter()
            .zip(&source.imports)
            .any(|(expected, loaded)| expected != &loaded.rule)
        {
            return Err(CssError {
                offset: 0,
                message: "CSS import graph span mismatch",
            });
        }

        self.active_urls.push(source.url.clone());
        let mut position = 0;
        for loaded in &source.imports {
            let rule = &loaded.rule;
            if rule.span.start < position
                || rule.span.end > source.text.len()
                || !source.text.is_char_boundary(rule.span.start)
                || !source.text.is_char_boundary(rule.span.end)
            {
                return Err(CssError {
                    offset: rule.span.start,
                    message: "invalid CSS import span",
                });
            }
            self.parse_segment(
                &source.text[position..rule.span.start],
                &source.url,
                media,
                supports,
                parent_layer,
                position,
                import_path,
                css_source_revision(&source.text),
            )?;
            position = rule.span.end;

            // Supports conditions describe the declarations this engine can
            // implement, so a false condition cannot become active when the
            // viewport changes. Media conditions are different: a loaded
            // sheet remains part of the stylesheet graph and its rules are
            // reevaluated by StyleIndex whenever the media environment
            // changes. Do not discard an already-loaded child merely because
            // its media query is false right now.
            if rule
                .supports
                .as_deref()
                .is_some_and(|condition| !supports_condition(condition))
            {
                continue;
            }
            let child_media: Arc<[Arc<str>]> = match &rule.media {
                Some(condition) => media.iter().cloned().chain([condition.clone()]).collect(),
                None => media.clone(),
            };
            let child_supports: Arc<[Arc<str>]> = match &rule.supports {
                Some(condition) => supports
                    .iter()
                    .cloned()
                    .chain([condition.clone()])
                    .collect(),
                None => supports.clone(),
            };
            let child_layer = match &rule.layer {
                None => parent_layer,
                Some(layer) => Some(self.import_layer(layer, parent_layer, rule.span.start)?),
            };
            if let Some(child) = loaded.source.as_deref() {
                let mut child_path = import_path.to_vec();
                child_path.push(rule.span.start);
                self.parse_source(
                    child,
                    &child_media,
                    &child_supports,
                    child_layer,
                    &child_path,
                    depth + 1,
                )?;
            }
        }
        self.parse_segment(
            &source.text[position..],
            &source.url,
            media,
            supports,
            parent_layer,
            position,
            import_path,
            css_source_revision(&source.text),
        )?;
        self.active_urls.pop();
        Ok(())
    }

    fn parse_segment(
        &mut self,
        input: &str,
        source_url: &Arc<str>,
        media: &Arc<[Arc<str>]>,
        supports: &Arc<[Arc<str>]>,
        parent_layer: Option<usize>,
        base_offset: usize,
        import_path: &[usize],
        source_revision: u64,
    ) -> Result<(), CssError> {
        let rule_start = self.rules.len();
        let face_start = self.font_faces.len();
        let keyframe_start = self.keyframes.len();
        parse_rules(
            input,
            &mut self.rules,
            media,
            supports,
            parent_layer,
            &mut self.layers,
            &mut self.font_faces,
            &mut self.keyframes,
            base_offset,
            &Arc::from(import_path),
            0,
        )?;
        for rule in &mut self.rules[rule_start..] {
            rule.source_url = Some(source_url.clone());
        }
        for face in &mut self.font_faces[face_start..] {
            face.source_url = Some(source_url.clone());
            face.source_revision = source_revision;
            face.import_path = Arc::from(import_path);
        }
        for keyframes in &mut self.keyframes[keyframe_start..] {
            keyframes.source_url = Some(source_url.clone());
        }
        Ok(())
    }

    fn import_layer(
        &mut self,
        layer: &ImportLayer,
        parent: Option<usize>,
        offset: usize,
    ) -> Result<usize, CssError> {
        let name = match layer {
            ImportLayer::Anonymous => alloc::format!("#{}", self.layers.len()),
            ImportLayer::Named(name) => name.to_string(),
        };
        let name = if let Some(parent) = parent {
            alloc::format!("{}.{name}", self.layers[parent])
        } else {
            name
        };
        if name.split('.').count() >= 8 || self.layers.len() >= 128 {
            return Err(CssError {
                offset,
                message: "too many cascade layers",
            });
        }
        if let Some(index) = self.layers.iter().position(|existing| existing == &name) {
            Ok(index)
        } else {
            self.layers.push(name);
            Ok(self.layers.len() - 1)
        }
    }
}

/// Parse a validated tree of stylesheet sources. Import media/supports
/// conditions are accumulated onto descendant rules and font faces. The
/// loader supplies one record for every valid top-level import; missing child
/// sources retain conditional layer declarations without contributing rules.
pub fn parse_graph(
    root: &StylesheetSource,
    _environment: MediaEnvironment,
) -> Result<ParsedStylesheets, CssError> {
    let mut parser = StylesheetGraphParser {
        rules: Vec::new(),
        font_faces: Vec::new(),
        keyframes: Vec::new(),
        layers: Vec::new(),
        active_urls: Vec::new(),
        bytes: 0,
        sheets: 0,
        import_count: 0,
    };
    parser.parse_source(root, &Arc::from([]), &Arc::from([]), None, &[], 0)?;
    if parser.rules.is_empty() && !parser.layers.is_empty() {
        parser.rules.push(Rule {
            scope: None,
            selector: parse_simple_selector("*", 0)?,
            declarations: Arc::from([]),
            unsupported_svg_properties: Arc::from([]),
            media: Arc::from([]),
            supports: Arc::from([]),
            source_url: Some(root.url.clone()),
            layer: None,
            layers: Arc::from([]),
            layer_path: None,
        });
    }
    let layers: Arc<[String]> = parser.layers.into();
    for rule in &mut parser.rules {
        rule.layers = layers.clone();
    }
    for face in &mut parser.font_faces {
        face.layers = layers.clone();
    }
    let mut parsed = ParsedStylesheets {
        rules: parser.rules,
        font_faces: parser.font_faces,
        keyframes: parser.keyframes,
        layers,
    };
    canonicalize_font_faces(core::slice::from_mut(&mut parsed));
    Ok(parsed)
}

/// Apply the same named and anonymous layer ordering used by `StyleIndex` to
/// the effective font faces in stylesheet owner order.
pub fn canonicalize_font_faces(groups: &mut [ParsedStylesheets]) -> Vec<FontFaceRule> {
    let mut layers = LayerCanonicalizer::default();
    let mut faces = Vec::new();
    let mut source_order = 0usize;
    for group in groups {
        let sheet = layers.add_sheet(&group.layers);
        for face in &mut group.font_faces {
            face.layer_path = layers.path(&face.layers, face.layer, sheet);
            face.source_order = source_order;
            source_order = source_order.saturating_add(1);
            faces.push(face.clone());
        }
    }
    // FontSet resolves equal descriptor matches in reverse registration order.
    // Keep the font-face cascade in weak-to-strong order so later/stronger
    // layers, then later source declarations, are examined first.
    faces.sort_by_key(|face| {
        (
            face.layer_path.unwrap_or([usize::MAX - 1; 8]),
            face.source_order,
        )
    });
    faces
}

pub fn parse_font_faces(input: &str) -> Result<Vec<FontFaceRule>, CssError> {
    Ok(parse_stylesheet(input)?.font_faces)
}

fn font_face_rule(
    input: &str,
    media: &Arc<[Arc<str>]>,
    supports: &Arc<[Arc<str>]>,
    layer: Option<usize>,
) -> Result<Option<FontFaceRule>, CssError> {
    let mut descriptors = FontFaceDescriptors::default();
    for (start, end) in declaration_spans_with_recovery(input, true)? {
        let Some((name, raw)) = declaration_pair(&input[start..end]) else {
            continue;
        };
        // Reuse ordinary CSS value expansion for comment removal and quoted
        // strings. Variables are invalid in font descriptors.
        let budget = if name.trim().eq_ignore_ascii_case("src") {
            MAX_CSS_BYTES
        } else {
            MAX_VARIABLE_BYTES
        };
        let Some(cleaned) = expand_variables_bounded(raw, &[], &mut Vec::new(), false, budget)
        else {
            continue;
        };
        let raw = cleaned.trim();
        if important_value(raw).1 {
            continue;
        }
        // Invalid CSS descriptors are ignored and leave a previous valid
        // declaration intact; this is the ordinary CSS descriptor recovery
        // behavior. The JS FontFace API uses the same validator but surfaces
        // its error to the caller.
        if normalize_font_descriptor_name(name.trim()).as_deref() == Some("family") {
            let _ = descriptors.set_css_family(raw);
        } else {
            let _ = descriptors.set(name.trim(), raw);
        }
    }
    if descriptors.family.is_empty() || descriptors.sources.is_empty() {
        return Ok(None);
    }
    let mut rule = descriptors.to_rule(None);
    rule.media = media.clone();
    rule.supports = supports.clone();
    rule.layer = layer;
    Ok(Some(rule))
}

enum RuleBoundary {
    Block(usize),
    Statement(usize),
    Discard(usize),
}

fn valid_css_escape_at_start(input: &str) -> bool {
    let Some(rest) = input.strip_prefix('\\') else {
        return false;
    };
    rest.chars()
        .next()
        .is_some_and(|next| !matches!(next, '\n' | '\r' | '\u{c}'))
}

/// CSS Syntax's would-start-an-identifier check, without allocating or
/// consuming the sequence. This is used to decide whether `@` begins an
/// at-keyword token before the rule scanner decides how semicolons terminate.
fn would_start_css_identifier(input: &str) -> bool {
    let mut chars = input.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first == '-' {
        match chars.next() {
            Some('-') => true,
            Some('\u{5c}') => valid_css_escape_at_start(&input[1..]),
            Some(character) => selector_name_start(character),
            None => false,
        }
    } else if first == '\u{5c}' {
        valid_css_escape_at_start(input)
    } else {
        selector_name_start(first)
    }
}

/// Locate the end of a rule prelude using CSS component boundaries. In
/// particular, strings, comments, functions and attribute selectors may
/// contain semicolons or braces without ending the prelude.
fn rule_boundary(input: &str) -> Option<RuleBoundary> {
    let bytes = input.as_bytes();
    let at_rule = input
        .strip_prefix('@')
        .is_some_and(would_start_css_identifier);
    let mut position = 0;
    while position < bytes.len() {
        match bytes[position] {
            b'\\' => {
                selector_escape(input, &mut position)?;
                continue;
            }
            b'\'' | b'"' => {
                position = quoted_css_end(input, position)?;
                continue;
            }
            b'/' if bytes.get(position + 1) == Some(&b'*') => {
                position += input[position + 2..].find("*/")? + 4;
                continue;
            }
            b'(' | b'[' => {
                position = matching_css_block(input, position)?;
                continue;
            }
            b'{' => return Some(RuleBoundary::Block(position)),
            b';' if at_rule => return Some(RuleBoundary::Statement(position)),
            b'}' => return Some(RuleBoundary::Discard(position + 1)),
            _ => {}
        }
        position += input[position..].chars().next()?.len_utf8();
    }
    // An unfinished prelude has no rule to contribute. Previously parsed
    // rules remain valid; CSS syntax errors do not invalidate the stylesheet.
    None
}

fn parse_rules(
    input: &str,
    rules: &mut Vec<Rule>,
    media: &Arc<[Arc<str>]>,
    supports: &Arc<[Arc<str>]>,
    layer: Option<usize>,
    layers: &mut Vec<String>,
    font_faces: &mut Vec<FontFaceRule>,
    keyframes: &mut Vec<KeyframesRuleText>,
    base_offset: usize,
    import_path: &Arc<[usize]>,
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
        if skip_css_space_comments(input, &mut pos).is_none() {
            break;
        }
        let rest = &input[pos..];
        if rest.trim().is_empty() {
            break;
        }
        if nesting == 0 && (rest.starts_with("<!--") || rest.starts_with("-->")) {
            pos += if rest.starts_with("<!--") { 4 } else { 3 };
            continue;
        }
        let open = match rule_boundary(rest) {
            Some(RuleBoundary::Block(open)) => open,
            Some(RuleBoundary::Statement(end)) => {
                let prelude = rest[..end].trim();
                if prelude
                    .get(..6)
                    .is_some_and(|name| name.eq_ignore_ascii_case("@layer"))
                    && prelude
                        .get(6..)
                        .is_some_and(|names| names.starts_with(char::is_whitespace))
                {
                    for name in prelude[6..].split(',').map(str::trim) {
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
                }
                pos += end + 1;
                continue;
            }
            Some(RuleBoundary::Discard(end)) => {
                pos += end;
                continue;
            }
            None => break,
        };
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
                if let Some(end) = rest[close + 2..].find("*/") {
                    close += end + 3;
                } else {
                    close = bytes.len();
                    break;
                }
            } else if byte == b'{' {
                depth += 1;
            } else if byte == b'}' {
                depth -= 1;
            }
            close += 1;
        }
        if depth == 0 {
            close -= 1;
        } else {
            close = close.min(bytes.len());
        }
        let prelude = rest[..open].trim();
        if is_font_face_prelude(prelude) {
            if let Some(mut face) = font_face_rule(&rest[open + 1..close], media, supports, layer)?
            {
                if font_faces.len() >= 64 {
                    return Err(CssError {
                        offset: pos,
                        message: "too many font faces",
                    });
                }
                face.source_order = font_faces.len();
                face.rule_start = base_offset.saturating_add(pos);
                face.import_path = import_path.clone();
                font_faces.push(face);
            }
            pos += close + 1;
            continue;
        }
        if let Some(query) = prelude
            .get(..6)
            .filter(|name| name.eq_ignore_ascii_case("@media"))
            .and_then(|_| prelude.get(6..))
            .filter(|query| query.starts_with(char::is_whitespace) || query.starts_with('('))
        {
            let nested: Arc<[Arc<str>]> = media
                .iter()
                .cloned()
                .chain([Arc::from(query.trim())])
                .collect();
            parse_rules(
                &rest[open + 1..close],
                rules,
                &nested,
                supports,
                layer,
                layers,
                font_faces,
                keyframes,
                base_offset.saturating_add(pos + open + 1),
                import_path,
                nesting + 1,
            )?;
            pos += close + 1;
            continue;
        }
        if let Some(condition) = prelude
            .get(..9)
            .filter(|name| name.eq_ignore_ascii_case("@supports"))
            .and_then(|_| prelude.get(9..))
            .filter(|condition| {
                condition.starts_with(char::is_whitespace)
                    || condition.starts_with('(')
                    || condition.starts_with("/*")
            })
        {
            if parse_supports_condition(condition, 0) == Some(true) {
                let nested: Arc<[Arc<str>]> = supports
                    .iter()
                    .cloned()
                    .chain([Arc::from(condition.trim())])
                    .collect();
                parse_rules(
                    &rest[open + 1..close],
                    rules,
                    media,
                    &nested,
                    layer,
                    layers,
                    font_faces,
                    keyframes,
                    base_offset.saturating_add(pos + open + 1),
                    import_path,
                    nesting + 1,
                )?;
            }
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
                supports,
                Some(rank),
                layers,
                font_faces,
                keyframes,
                base_offset.saturating_add(pos + open + 1),
                import_path,
                nesting + 1,
            )?;
            pos += close + 1;
            continue;
        }
        if prelude.starts_with('@') {
            // EOF implicitly ends an unfinished block; there is no closing
            // brace byte to include in that case.
            let rule_end = if depth == 0 { close + 1 } else { close };
            if let Some(mut parsed) = parse_keyframes_rule(&rest[..rule_end])? {
                parsed.source_order = keyframes.len();
                parsed.media = media.clone();
                parsed.supports = supports.clone();
                keyframes.push(parsed);
            }
            pos += close + 1;
            continue;
        }
        let declarations: Arc<[Declaration]> =
            declarations(&rest[open + 1..close], pos + open + 1)?.into();
        let unsupported_properties = unsupported_svg_style_properties(&rest[open + 1..close]);
        if let Ok(selectors) = parse_selector_list(&rest[..open], pos) {
            for selector in selectors {
                if rules.len() >= MAX_RULES {
                    return Err(CssError {
                        offset: pos,
                        message: "too many rules",
                    });
                }
                rules.push(Rule {
                    scope: None,
                    selector,
                    declarations: declarations.clone(),
                    unsupported_svg_properties: unsupported_properties.clone().into(),
                    media: media.clone(),
                    supports: supports.clone(),
                    source_url: None,
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

fn bloom_bits(kind: u8, name: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64 ^ u64::from(kind);
    for byte in name.bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    (1u64 << (hash & 63)) | (1u64 << ((hash >> 20) & 63))
}

/// Bloom bits for the tag, id and classes of an element.
fn element_bloom(kind: &NodeKind) -> u64 {
    let NodeKind::Element {
        name, attributes, ..
    } = kind
    else {
        return 0;
    };
    let mut bits = bloom_bits(0, crate::svg::local_name(name));
    for (key, value) in attributes {
        match key.as_str() {
            "id" => bits |= bloom_bits(1, value),
            "class" => {
                for class in value.split_ascii_whitespace() {
                    bits |= bloom_bits(2, class);
                }
            }
            _ => {}
        }
    }
    bits
}

impl RelativeSelector {
    fn matches_from(
        &self,
        document: &Document,
        anchor: NodeId,
        shadow_root: Option<NodeId>,
        scope_root: Option<NodeId>,
        depth: usize,
        validity: &dyn crate::forms::ValidityStateView,
    ) -> bool {
        if depth > MAX_SELECTOR_MATCH_DEPTH {
            return false;
        }
        let matches_candidate = |node| {
            matches!(document.kind(node), Ok(NodeKind::Element { .. }))
                && self.selector.matches_in_context_with_anchor_and_validity(
                    document,
                    node,
                    shadow_root,
                    scope_root,
                    depth + 1,
                    Some((anchor, self.relation)),
                    validity,
                )
        };
        let matches_subtree = |root| {
            let mut current = document.first_child(root).ok().flatten();
            while let Some(node) = current {
                if matches_candidate(node) {
                    return true;
                }
                current = crate::selector::next_descendant(document, root, node)
                    .ok()
                    .flatten();
            }
            false
        };
        let has_further_combinator = self.selector.ancestor.is_some();
        match self.relation {
            Relation::Descendant => matches_subtree(anchor),
            Relation::Child if !has_further_combinator => {
                let mut current = document.first_child(anchor).ok().flatten();
                while let Some(node) = current {
                    if matches_candidate(node) {
                        return true;
                    }
                    current = document.next_sibling(node).ok().flatten();
                }
                false
            }
            // With further combinators, the subject can be deeper than the
            // first child compound, so inspect descendants and let the anchor
            // relation constrain the leftmost compound.
            Relation::Child => matches_subtree(anchor),
            Relation::Adjacent | Relation::Following => {
                let mut current = document.next_sibling(anchor).ok().flatten();
                while let Some(node) = current {
                    if matches!(document.kind(node), Ok(NodeKind::Element { .. }))
                        && (matches_candidate(node)
                            || (has_further_combinator && matches_subtree(node)))
                    {
                        return true;
                    }
                    if self.relation == Relation::Adjacent
                        && matches!(document.kind(node), Ok(NodeKind::Element { .. }))
                    {
                        return false;
                    }
                    current = document.next_sibling(node).ok().flatten();
                }
                false
            }
        }
    }
}

fn validity_pseudo_matches(
    document: &Document,
    node: NodeId,
    expected_valid: bool,
    view: &dyn crate::forms::ValidityStateView,
) -> bool {
    let invalid_candidate = |candidate| {
        crate::forms::will_validate(document, candidate)
            && !document
                .validity_state(candidate)
                .unwrap_or_else(|| crate::forms::validity_with_view(document, candidate, view))
                .valid()
    };
    let Ok(kind) = document.kind(node) else {
        return false;
    };
    let NodeKind::Element {
        namespace: Namespace::Html,
        name,
        ..
    } = &kind
    else {
        return false;
    };
    let tag = crate::svg::local_name(name);
    let has_invalid_candidate = match tag {
        "form" => {
            let mut invalid = false;
            let _ = crate::forms::for_each_form_control(document, node, |candidate| {
                if invalid_candidate(candidate) {
                    invalid = true;
                    false
                } else {
                    true
                }
            });
            invalid
        }
        "fieldset" => {
            let mut current = document.first_child(node).ok().flatten();
            let mut invalid = false;
            while let Some(candidate) = current {
                if invalid_candidate(candidate) {
                    invalid = true;
                    break;
                }
                current = crate::selector::next_descendant(document, node, candidate)
                    .ok()
                    .flatten();
            }
            invalid
        }
        _ if crate::forms::will_validate(document, node) => {
            return document
                .validity_state(node)
                .unwrap_or_else(|| crate::forms::validity_with_view(document, node, view))
                .valid()
                == expected_valid;
        }
        _ => return false,
    };
    !has_invalid_candidate == expected_valid
}

fn relative_relation_matches(
    document: &Document,
    node: NodeId,
    anchor: NodeId,
    relation: Relation,
) -> bool {
    match relation {
        Relation::Child => document.parent(node).ok().flatten() == Some(anchor),
        Relation::Descendant => {
            let mut parent = document.parent(node).ok().flatten();
            while let Some(current) = parent {
                if current == anchor {
                    return true;
                }
                parent = document.parent(current).ok().flatten();
            }
            false
        }
        Relation::Adjacent | Relation::Following => {
            let mut previous = document.previous_sibling(node).ok().flatten();
            while let Some(current) = previous {
                if matches!(document.kind(current), Ok(NodeKind::Element { .. })) {
                    if current == anchor {
                        return true;
                    }
                    if relation == Relation::Adjacent {
                        return false;
                    }
                }
                previous = document.previous_sibling(current).ok().flatten();
            }
            false
        }
    }
}

impl Selector {
    fn has_validity_dependency(&self) -> bool {
        self.directionality_mask != 0
            || !self.interaction_state.is_empty()
            || self
                .form_state
                .contains(crate::forms::FormStatePseudo::Checked)
            || self
                .form_state
                .contains(crate::forms::FormStatePseudo::UserValid)
            || self
                .form_state
                .contains(crate::forms::FormStatePseudo::UserInvalid)
            || self
                .form_state
                .contains(crate::forms::FormStatePseudo::PlaceholderShown)
            || self.structural.iter().any(|pseudo| match pseudo {
                StructuralPseudo::Valid | StructuralPseudo::Invalid => true,
                StructuralPseudo::Nth { of, .. } => of.as_ref().is_some_and(|selectors| {
                    selectors.iter().any(Selector::has_validity_dependency)
                }),
                StructuralPseudo::Empty | StructuralPseudo::Last(_) | StructuralPseudo::Only(_) => {
                    false
                }
            })
            || self.logical.iter().any(|pseudo| match pseudo {
                LogicalPseudo::Has(relative) => relative
                    .iter()
                    .any(|relative| relative.selector.has_validity_dependency()),
                LogicalPseudo::Not(selectors)
                | LogicalPseudo::Is(selectors)
                | LogicalPseudo::Where(selectors) => {
                    selectors.iter().any(Selector::has_validity_dependency)
                }
            })
            || self
                .ancestor
                .as_ref()
                .is_some_and(|(_, ancestor)| ancestor.has_validity_dependency())
    }

    /// Whether the selector (or an ancestor/sibling compound of it) matches on
    /// something other than the element, its attributes and its ancestors.
    fn position_dependent(&self) -> bool {
        self.host
            || self.slotted
            || self.part.is_some()
            || self.root
            || self.scope
            || !self.form_state.is_empty()
            || !self.interaction_state.is_empty()
            || self.directionality_mask != 0
            || !self.structural.is_empty()
            || self.logical.iter().any(|pseudo| match pseudo {
                LogicalPseudo::Has(_) => true,
                LogicalPseudo::Not(selectors)
                | LogicalPseudo::Is(selectors)
                | LogicalPseudo::Where(selectors) => {
                    selectors.iter().any(Selector::position_dependent)
                }
            })
            || self.ancestor.as_ref().is_some_and(|(relation, ancestor)| {
                matches!(relation, Relation::Adjacent | Relation::Following)
                    || ancestor.position_dependent()
            })
    }

    fn ancestor_mask(&self) -> u64 {
        // `::part()` and `:host`/`::slotted()` matching ignores the ancestor chain.
        // Logical branches are alternatives, so their ancestor masks cannot be
        // unioned here without rejecting a node that matches just one branch.
        if self.part.is_some() || self.host || self.slotted {
            return 0;
        }
        let mut mask = 0;
        let mut current = self;
        while let Some((Relation::Child | Relation::Descendant, ancestor)) = &current.ancestor {
            if !(ancestor.host || ancestor.slotted || ancestor.part.is_some()) {
                if let Some(tag) = &ancestor.tag {
                    mask |= bloom_bits(0, tag);
                }
                if let Some(id) = &ancestor.id {
                    mask |= bloom_bits(1, id);
                }
                for class in &ancestor.classes {
                    mask |= bloom_bits(2, class);
                }
            }
            current = &**ancestor;
        }
        mask
    }

    pub(crate) fn matches(&self, kind: &NodeKind) -> bool {
        self.matches_context_free(kind).unwrap_or(false)
    }

    fn matches_context_free(&self, kind: &NodeKind) -> Option<bool> {
        if !self.matches_kind(None, None, kind) {
            return Some(false);
        }
        if self.host
            || self.slotted
            || self.part.is_some()
            || self.root
            || self.scope
            || !self.languages.is_empty()
            || !self.form_state.is_empty()
            || !self.interaction_state.is_empty()
            || self.directionality_mask != 0
            || !self.structural.is_empty()
            || self.ancestor.is_some()
        {
            return None;
        }
        for pseudo in &self.logical {
            if matches!(pseudo, LogicalPseudo::Has(_)) {
                // A relational pseudo-class depends on the surrounding tree,
                // which is unavailable to this context-free fast path.
                return None;
            }
            let results = pseudo
                .selectors()
                .iter()
                .map(|selector| selector.matches_context_free(kind));
            let matched = match pseudo {
                LogicalPseudo::Not(_) => {
                    let mut unknown = false;
                    for result in results {
                        match result {
                            Some(true) => return Some(false),
                            Some(false) => {}
                            None => unknown = true,
                        }
                    }
                    if unknown {
                        return None;
                    }
                    true
                }
                LogicalPseudo::Is(_) | LogicalPseudo::Where(_) => {
                    let mut unknown = false;
                    let mut matched = false;
                    for result in results {
                        match result {
                            Some(true) => {
                                matched = true;
                                break;
                            }
                            Some(false) => {}
                            None => unknown = true,
                        }
                    }
                    if !matched && unknown {
                        return None;
                    }
                    matched
                }
                LogicalPseudo::Has(_) => return None,
            };
            if !matched {
                return Some(false);
            }
        }
        Some(true)
    }

    /// Matches inside a scoped (shadow) rule context: `:host` matches the
    /// host, `::slotted()` matches assigned light children, `::part()` matches
    /// exposed parts, and plain selectors use the shadow tree structure.
    pub(crate) fn matches_shadow(&self, document: &Document, node: NodeId, root: NodeId) -> bool {
        self.matches_in_context(document, node, Some(root), Some(root), 0)
    }

    pub(crate) fn matches_shadow_with_validity(
        &self,
        document: &Document,
        node: NodeId,
        root: NodeId,
        validity: &dyn crate::forms::ValidityStateView,
    ) -> bool {
        self.matches_in_context_with_validity(document, node, Some(root), Some(root), 0, validity)
    }

    fn matches_in_context(
        &self,
        document: &Document,
        node: NodeId,
        shadow_root: Option<NodeId>,
        scope_root: Option<NodeId>,
        depth: usize,
    ) -> bool {
        self.matches_in_context_with_validity(
            document,
            node,
            shadow_root,
            scope_root,
            depth,
            &crate::forms::NoValidityOverrides,
        )
    }

    fn matches_in_context_with_validity(
        &self,
        document: &Document,
        node: NodeId,
        shadow_root: Option<NodeId>,
        scope_root: Option<NodeId>,
        depth: usize,
        validity: &dyn crate::forms::ValidityStateView,
    ) -> bool {
        self.matches_in_context_with_anchor_and_validity(
            document,
            node,
            shadow_root,
            scope_root,
            depth,
            None,
            validity,
        )
    }

    fn matches_in_context_with_anchor(
        &self,
        document: &Document,
        node: NodeId,
        shadow_root: Option<NodeId>,
        scope_root: Option<NodeId>,
        depth: usize,
        relative_anchor: Option<(NodeId, Relation)>,
    ) -> bool {
        self.matches_in_context_with_anchor_and_validity(
            document,
            node,
            shadow_root,
            scope_root,
            depth,
            relative_anchor,
            &crate::forms::NoValidityOverrides,
        )
    }

    fn matches_in_context_with_anchor_and_validity(
        &self,
        document: &Document,
        node: NodeId,
        shadow_root: Option<NodeId>,
        scope_root: Option<NodeId>,
        depth: usize,
        relative_anchor: Option<(NodeId, Relation)>,
        validity: &dyn crate::forms::ValidityStateView,
    ) -> bool {
        if depth > MAX_SELECTOR_MATCH_DEPTH {
            return false;
        }
        if self.host {
            return shadow_root.is_some_and(|root| document.shadow_host(root) == Ok(Some(node)))
                && self.ancestor.is_none()
                && self.matches_flat_depth(
                    document,
                    node,
                    shadow_root,
                    scope_root,
                    depth,
                    relative_anchor,
                    validity,
                );
        }
        if self.slotted {
            let Some(root) = shadow_root else {
                return false;
            };
            let slot_assigned = document
                .assigned_slot(node)
                .ok()
                .flatten()
                .is_some_and(|slot| document.root_node(slot, false) == Ok(root));
            return slot_assigned
                && self.ancestor.is_none()
                && self.matches_flat_depth(
                    document,
                    node,
                    shadow_root,
                    scope_root,
                    depth,
                    relative_anchor,
                    validity,
                );
        }
        if self.part.is_some() {
            return shadow_root.is_some()
                && self.matches_part_with_interaction(document, node)
                && (!self.scope || scope_root == Some(node))
                && relative_anchor.is_none_or(|(anchor, relation)| {
                    relative_relation_matches(document, node, anchor, relation)
                });
        }
        if shadow_root.is_none() && (self.host || self.slotted || self.part.is_some()) {
            return false;
        }
        self.matches_flat_depth(
            document,
            node,
            shadow_root,
            scope_root,
            depth,
            relative_anchor,
            validity,
        )
    }

    /// `::part(name)` matches a shadow element whose `part` attribute lists it.
    pub(crate) fn matches_part(&self, document: &Document, node: NodeId) -> bool {
        let Some(part) = &self.part else { return false };
        let Ok(NodeKind::Element { attributes, .. }) = document.kind(node) else {
            return false;
        };
        attributes
            .iter()
            .find(|(name, _)| name == "part")
            .is_some_and(|(_, value)| {
                part.split_ascii_whitespace()
                    .any(|part| value.split_ascii_whitespace().any(|token| token == part))
            })
    }

    fn matches_part_with_interaction(&self, document: &Document, node: NodeId) -> bool {
        self.matches_part(document, node)
            && crate::interaction::InteractionPseudo::ALL
                .into_iter()
                .all(|pseudo| {
                    !self.interaction_state.contains(pseudo)
                        || crate::interaction::matches(document, node, pseudo)
                })
    }

    /// Matching with no selector scope. DOM query APIs use
    /// `matches_node_in_scope` to supply their ParentNode/Element root.
    pub(crate) fn matches_node(&self, document: &Document, node: NodeId) -> bool {
        self.matches_node_in_scope(document, node, None)
    }

    pub(crate) fn matches_node_in_scope(
        &self,
        document: &Document,
        node: NodeId,
        scope_root: Option<NodeId>,
    ) -> bool {
        self.matches_node_in_scope_with_validity(
            document,
            node,
            scope_root,
            &crate::forms::NoValidityOverrides,
        )
    }

    pub(crate) fn matches_node_in_scope_with_validity(
        &self,
        document: &Document,
        node: NodeId,
        scope_root: Option<NodeId>,
        validity: &dyn crate::forms::ValidityStateView,
    ) -> bool {
        self.matches_in_context_with_validity(document, node, None, scope_root, 0, validity)
    }

    fn matches_flat_depth(
        &self,
        document: &Document,
        node: NodeId,
        shadow_root: Option<NodeId>,
        scope_root: Option<NodeId>,
        depth: usize,
        relative_anchor: Option<(NodeId, Relation)>,
        validity: &dyn crate::forms::ValidityStateView,
    ) -> bool {
        if depth > MAX_SELECTOR_MATCH_DEPTH {
            return false;
        }
        let Ok(kind) = document.kind(node) else {
            return false;
        };
        if !matches!(kind, NodeKind::Element { .. }) {
            return scope_root == Some(node)
                && self.matches_virtual_scope()
                && relative_anchor.is_none_or(|(anchor, relation)| {
                    relative_relation_matches(document, node, anchor, relation)
                });
        }
        if !self.matches_kind(Some(document), Some(node), kind)
            || (self.scope && scope_root != Some(node))
        {
            return false;
        }
        if !self.form_state.is_empty() {
            for pseudo in crate::forms::FormStatePseudo::ALL {
                if self.form_state.contains(pseudo)
                    && !crate::forms::matches_form_state_pseudo(document, node, pseudo, validity)
                {
                    return false;
                }
            }
        }
        for pseudo in crate::top_layer::SelectorPseudo::ALL {
            if self.top_layer_state & pseudo.bit() != 0 && !pseudo.matches(document, node) {
                return false;
            }
        }
        if self.directionality_mask != 0 {
            if self.directionality_mask == 3 {
                return false;
            }
            let direction = crate::directionality::resolved_element_directionality(document, node);
            let bit = match direction {
                crate::directionality::Direction::Ltr => 1,
                crate::directionality::Direction::Rtl => 2,
            };
            if self.directionality_mask != bit {
                return false;
            }
        }
        for pseudo in crate::interaction::InteractionPseudo::ALL {
            if self.interaction_state.contains(pseudo)
                && !crate::interaction::matches(document, node, pseudo)
            {
                return false;
            }
        }
        for pseudo in &self.logical {
            let any = match pseudo {
                LogicalPseudo::Has(relative) => relative.iter().any(|selector| {
                    selector.matches_from(
                        document,
                        node,
                        shadow_root,
                        scope_root,
                        depth + 1,
                        validity,
                    )
                }),
                _ => pseudo.selectors().iter().any(|selector| {
                    selector.matches_in_context_with_validity(
                        document,
                        node,
                        shadow_root,
                        scope_root,
                        depth + 1,
                        validity,
                    )
                }),
            };
            let matched = match pseudo {
                LogicalPseudo::Not(_) => !any,
                LogicalPseudo::Is(_) | LogicalPseudo::Where(_) => any,
                LogicalPseudo::Has(_) => any,
            };
            if !matched {
                return false;
            }
        }
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
        for pseudo in &self.structural {
            match pseudo {
                StructuralPseudo::Valid => {
                    if !validity_pseudo_matches(document, node, true, validity) {
                        return false;
                    }
                    continue;
                }
                StructuralPseudo::Invalid => {
                    if !validity_pseudo_matches(document, node, false, validity) {
                        return false;
                    }
                    continue;
                }
                _ => {}
            }
            if matches!(pseudo, StructuralPseudo::Empty) {
                // Selectors 3: comments and zero-length text do not affect emptiness.
                let mut child = document.first_child(node).ok().flatten();
                while let Some(id) = child {
                    if document.kind(id).is_ok_and(|kind| {
                        matches!(kind, NodeKind::Element { .. })
                            || matches!(kind, NodeKind::Text(text) | NodeKind::CData(text) if !text.is_empty())
                    }) {
                        return false;
                    }
                    child = document.next_sibling(id).ok().flatten();
                }
                continue;
            }
            let of_type = match pseudo {
                StructuralPseudo::Empty | StructuralPseudo::Valid | StructuralPseudo::Invalid => {
                    unreachable!()
                }
                StructuralPseudo::Nth { of_type, .. }
                | StructuralPseudo::Last(of_type)
                | StructuralPseudo::Only(of_type) => *of_type,
            };
            let expected_type = match document.kind(node) {
                Ok(NodeKind::Element {
                    namespace, name, ..
                }) if of_type => Some((namespace, name)),
                Ok(NodeKind::Element { .. }) => None,
                _ => return false,
            };
            let Some(parent) = document.parent(node).ok().flatten() else {
                return false;
            };
            let matches_sibling = |sibling| {
                let Ok(NodeKind::Element {
                    namespace, name, ..
                }) = document.kind(sibling)
                else {
                    return false;
                };
                let same_type = expected_type.is_none_or(|(expected_namespace, expected_name)| {
                    namespace == expected_namespace && name == expected_name
                });
                let in_filter = match pseudo {
                    StructuralPseudo::Nth {
                        of: Some(selectors),
                        ..
                    } => selectors.iter().any(|selector| {
                        selector.matches_in_context_with_validity(
                            document,
                            sibling,
                            shadow_root,
                            scope_root,
                            depth.saturating_add(1),
                            validity,
                        )
                    }),
                    _ => true,
                };
                same_type && in_filter
            };
            if let StructuralPseudo::Nth { a, b, reverse, .. } = pseudo {
                if !matches_sibling(node) {
                    return false;
                }
                let mut index = 1i64;
                let mut current = if *reverse {
                    document.next_sibling(node).ok().flatten()
                } else {
                    document.first_child(parent).ok().flatten()
                };
                while let Some(sibling) = current {
                    if !*reverse && sibling == node {
                        break;
                    }
                    if matches_sibling(sibling) {
                        index += 1;
                    }
                    current = document.next_sibling(sibling).ok().flatten();
                }
                let (a, b) = (*a as i64, *b as i64);
                let matches = if a == 0 {
                    index == b
                } else {
                    (index - b) % a == 0 && (index - b) / a >= 0
                };
                if !matches {
                    return false;
                }
                continue;
            }

            let mut current = document.first_child(parent).ok().flatten();
            let (mut index, mut count) = (0i64, 0i64);
            while let Some(sibling) = current {
                if matches_sibling(sibling) {
                    count += 1;
                    if sibling == node {
                        index = count;
                    }
                }
                current = document.next_sibling(sibling).ok().flatten();
            }
            let matches = match pseudo {
                StructuralPseudo::Empty | StructuralPseudo::Valid | StructuralPseudo::Invalid => {
                    unreachable!()
                }
                StructuralPseudo::Last(_) => index == count,
                StructuralPseudo::Only(_) => count == 1,
                StructuralPseudo::Nth { .. } => unreachable!(),
            };
            if index == 0 || !matches {
                return false;
            }
        }
        let Some((relation, ancestor)) = &self.ancestor else {
            return relative_anchor.is_none_or(|(anchor, relation)| {
                relative_relation_matches(document, node, anchor, relation)
            });
        };
        match relation {
            Relation::Child => document.parent(node).ok().flatten().is_some_and(|parent| {
                ancestor.matches_in_context_with_anchor_and_validity(
                    document,
                    parent,
                    shadow_root,
                    scope_root,
                    depth + 1,
                    relative_anchor,
                    validity,
                )
            }),
            Relation::Descendant => {
                let mut current = document.parent(node).ok().flatten();
                while let Some(id) = current {
                    if ancestor.matches_in_context_with_anchor_and_validity(
                        document,
                        id,
                        shadow_root,
                        scope_root,
                        depth + 1,
                        relative_anchor,
                        validity,
                    ) {
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
                        if ancestor.matches_in_context_with_anchor_and_validity(
                            document,
                            id,
                            shadow_root,
                            scope_root,
                            depth + 1,
                            relative_anchor,
                            validity,
                        ) {
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

    fn matches_kind(
        &self,
        document: Option<&Document>,
        node: Option<NodeId>,
        kind: &NodeKind,
    ) -> bool {
        let NodeKind::Element {
            namespace,
            name,
            attributes,
            ..
        } = kind
        else {
            return false;
        };
        let local_name = crate::svg::local_name(name);
        if self.tag.as_deref().is_some_and(|tag| {
            if namespace == &Namespace::Html {
                !tag.eq_ignore_ascii_case(local_name)
            } else {
                tag != local_name
            }
        }) {
            return false;
        }
        let html_attribute_names =
            namespace == &Namespace::Html && document.is_some_and(Document::is_html_document);
        let attribute_namespace = |index| {
            document
                .zip(node)
                .and_then(|(document, node)| document.attribute_namespace_uri_at(node, index))
        };
        if self.id.as_ref().is_some_and(|id| {
            !attributes.iter().enumerate().any(|(index, (key, value))| {
                attribute_namespace(index).is_none()
                    && (if html_attribute_names {
                        key.as_str().eq_ignore_ascii_case("id")
                    } else {
                        key.as_str() == "id"
                    })
                    && value == id
            })
        }) {
            return false;
        }
        if self.classes.iter().any(|class| {
            !attributes.iter().enumerate().any(|(index, (key, value))| {
                attribute_namespace(index).is_none()
                    && (if html_attribute_names {
                        key.as_str().eq_ignore_ascii_case("class")
                    } else {
                        key.as_str() == "class"
                    })
                    && value.split_ascii_whitespace().any(|token| token == class)
            })
        }) {
            return false;
        }
        if self.attributes.iter().any(|selector| {
            !attributes.iter().enumerate().any(|(index, (name, value))| {
                let namespace_uri = attribute_namespace(index);
                let namespace_matches = match selector.namespace {
                    AttributeNamespace::None => namespace_uri.is_none(),
                    AttributeNamespace::Any => true,
                };
                let local_name = if namespace_uri.is_some() {
                    name.as_str()
                        .rsplit_once(':')
                        .map_or(name.as_str(), |(_, local)| local)
                } else {
                    name.as_str()
                };
                let html_name = html_attribute_names && namespace_uri.is_none();
                let name_matches = if html_name {
                    local_name.eq_ignore_ascii_case(selector.name.as_str())
                } else {
                    local_name == selector.name.as_str()
                };
                namespace_matches
                    && name_matches
                    && selector
                        .matches_value(value, html_name && selector.html_default_ascii_insensitive)
            })
        }) {
            return false;
        }
        true
    }

    fn matches_virtual_scope(&self) -> bool {
        self.scope
            && !self.universal
            && !self.root
            && !self.host
            && !self.slotted
            && self.part.is_none()
            && self.pseudo_element.is_none()
            && self.tag.is_none()
            && self.id.is_none()
            && self.classes.is_empty()
            && self.attributes.is_empty()
            && self.languages.is_empty()
            && self.structural.is_empty()
            && self.logical.is_empty()
            && self.ancestor.is_none()
    }

    fn has_pseudo_element(&self) -> bool {
        self.pseudo_element.is_some()
            || self.slotted
            || self.part.is_some()
            || self.structural.iter().any(|pseudo| match pseudo {
                StructuralPseudo::Nth { of, .. } => of
                    .as_ref()
                    .is_some_and(|selectors| selectors.iter().any(Selector::has_pseudo_element)),
                _ => false,
            })
            || self
                .ancestor
                .as_ref()
                .is_some_and(|(_, ancestor)| ancestor.has_pseudo_element())
            || self.logical.iter().any(|pseudo| match pseudo {
                LogicalPseudo::Has(relative) => relative
                    .iter()
                    .any(|item| item.selector.has_pseudo_element()),
                _ => pseudo.selectors().iter().any(Selector::has_pseudo_element),
            })
    }

    /// Nested `:has()` is invalid in a strict selector context, but a nested
    /// occurrence inside the forgiving argument list of `:is()` or `:where()`
    /// invalidates only that branch. Normalize those lists before validating
    /// the enclosing `:has()` argument and keep specificity in sync with the
    /// surviving branches.
    fn drop_nested_has_from_forgiving_lists(&mut self) -> bool {
        let mut specificity = self.specificity;
        for pseudo in &mut self.logical {
            match pseudo {
                LogicalPseudo::Has(_) => return false,
                LogicalPseudo::Is(selectors) => {
                    let old = selectors
                        .iter()
                        .map(|selector| selector.specificity)
                        .max()
                        .unwrap_or((0, 0, 0));
                    selectors.retain_mut(Selector::drop_nested_has_from_forgiving_lists);
                    let new = selectors
                        .iter()
                        .map(|selector| selector.specificity)
                        .max()
                        .unwrap_or((0, 0, 0));
                    specificity = adjust_specificity(specificity, old, new);
                }
                LogicalPseudo::Where(selectors) => {
                    selectors.retain_mut(Selector::drop_nested_has_from_forgiving_lists);
                }
                LogicalPseudo::Not(selectors) => {
                    let old = selectors
                        .iter()
                        .map(|selector| selector.specificity)
                        .max()
                        .unwrap_or((0, 0, 0));
                    if !selectors
                        .iter_mut()
                        .all(Selector::drop_nested_has_from_forgiving_lists)
                    {
                        return false;
                    }
                    let new = selectors
                        .iter()
                        .map(|selector| selector.specificity)
                        .max()
                        .unwrap_or((0, 0, 0));
                    specificity = adjust_specificity(specificity, old, new);
                }
            }
        }
        for pseudo in &mut self.structural {
            let StructuralPseudo::Nth {
                of: Some(selectors),
                ..
            } = pseudo
            else {
                continue;
            };
            let old = selectors
                .iter()
                .map(|selector| selector.specificity)
                .max()
                .unwrap_or((0, 0, 0));
            for selector in selectors.iter_mut() {
                if !selector.drop_nested_has_from_forgiving_lists() {
                    return false;
                }
            }
            let new = selectors
                .iter()
                .map(|selector| selector.specificity)
                .max()
                .unwrap_or((0, 0, 0));
            specificity = adjust_specificity(specificity, old, new);
        }
        if let Some((_, ancestor)) = &mut self.ancestor {
            let old = ancestor.specificity;
            if !ancestor.drop_nested_has_from_forgiving_lists() {
                return false;
            }
            specificity = adjust_specificity(specificity, old, ancestor.specificity);
        }
        self.specificity = specificity;
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

/// Matches a media query against the browser's current media environment.
/// The same parser is used when the cascade evaluates `@media` rules.
pub fn media_query_matches(query: &str, environment: MediaEnvironment) -> bool {
    media_matches(query, environment)
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
    compute_for(kind, parent, index, None, u64::MAX, None, None)
}

const MAX_CUSTOM_PROPERTIES: usize = 128;
const MAX_VARIABLE_DEPTH: usize = 32;
const MAX_VARIABLE_BYTES: usize = 8192;

fn expand_variables(
    raw: &str,
    properties: &[(String, Option<String>)],
    stack: &mut Vec<String>,
) -> Option<String> {
    expand_variables_mode(raw, properties, stack, false)
}

// CSSOM checks var() grammar before substitution; unresolved names are valid.
fn expand_variables_mode(
    raw: &str,
    properties: &[(String, Option<String>)],
    stack: &mut Vec<String>,
    syntax_only: bool,
) -> Option<String> {
    expand_variables_bounded(raw, properties, stack, syntax_only, MAX_VARIABLE_BYTES)
}

fn expand_variables_bounded(
    raw: &str,
    properties: &[(String, Option<String>)],
    stack: &mut Vec<String>,
    syntax_only: bool,
    max_bytes: usize,
) -> Option<String> {
    if stack.len() >= MAX_VARIABLE_DEPTH || raw.len() > max_bytes {
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
            .get(pos..pos + 3)
            .is_some_and(|v| v.eq_ignore_ascii_case(b"var"))
            && bytes.get(pos + 3) == Some(&b'(')
            && (pos == 0
                || !matches!(bytes[pos-1], b'a'..=b'z'|b'A'..=b'Z'|b'0'..=b'9'|b'_'|b'-'|128..=255))
        {
            let (end, name, fallback) = variable_reference(raw, pos + 3)?;
            if syntax_only {
                if let Some(fallback) = fallback {
                    stack.push(name);
                    let valid = expand_variables_mode(fallback, properties, stack, true);
                    stack.pop();
                    valid?;
                }
                out.push('0');
                pos = end;
                continue;
            }
            let value = if stack.iter().any(|v| v == &name) {
                None
            } else {
                properties
                    .iter()
                    .find(|(key, _)| key == &name)
                    .and_then(|(_, value)| value.as_ref())
                    .and_then(|value| {
                        stack.push(name.into());
                        let result = expand_variables(value, properties, stack);
                        stack.pop();
                        result
                    })
            };
            let value = value.or_else(|| {
                fallback.and_then(|fallback| expand_variables(fallback, properties, stack))
            })?;
            if out.len() + value.len() > max_bytes {
                return None;
            }
            out.push_str(&value);
            pos = end;
        } else {
            let ch = raw[pos..].chars().next()?;
            if out.len() + ch.len_utf8() > max_bytes {
                return None;
            }
            out.push(ch);
            pos += ch.len_utf8();
        }
    }
    Some(out)
}

/// Values without `var(` or comments expand to themselves and cannot cycle.
fn needs_expansion(raw: &str) -> bool {
    raw.len() > MAX_VARIABLE_BYTES
        || raw.contains("/*")
        || raw
            .as_bytes()
            .windows(4)
            .any(|window| window.eq_ignore_ascii_case(b"var("))
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
    let mut pos = 0;
    let mut quote = 0u8;
    while pos < raw.len() {
        let byte = raw.as_bytes()[pos];
        if quote != 0 {
            if byte == b'\\' {
                pos += 1;
                if pos < raw.len() {
                    pos += raw[pos..].chars().next().map_or(1, char::len_utf8);
                }
                continue;
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
            .get(pos..pos + 3)
            .is_some_and(|v| v.eq_ignore_ascii_case(b"var"))
            && raw.as_bytes().get(pos + 3) == Some(&b'(')
            && (pos == 0
                || !matches!(raw.as_bytes()[pos-1], b'a'..=b'z'|b'A'..=b'Z'|b'0'..=b'9'|b'_'|b'-'|128..=255))
        {
            let Some((end, dependency, fallback)) = variable_reference(raw, pos + 3) else {
                stack.pop();
                return true;
            };
            if custom_cycle(&dependency, properties, stack, budget) {
                stack.pop();
                return true;
            }
            if fallback
                .is_some_and(|fallback| custom_value_cycle(fallback, properties, stack, budget))
            {
                stack.pop();
                return true;
            }
            pos = end;
            continue;
        }
        if byte == b'\\' {
            let Some(end) = selector_escape(raw, &mut pos) else {
                stack.pop();
                return true;
            };
            let _ = end;
        } else {
            pos += raw[pos..].chars().next().map_or(1, char::len_utf8);
        }
    }
    stack.pop();
    false
}

fn custom_value_cycle(
    raw: &str,
    properties: &[(String, Option<String>)],
    stack: &mut Vec<String>,
    budget: &mut usize,
) -> bool {
    if *budget == 0 {
        return true;
    }
    *budget -= 1;
    let (mut position, mut quote) = (0, 0u8);
    while position < raw.len() {
        let byte = raw.as_bytes()[position];
        if quote != 0 {
            if byte == b'\\' {
                position += 1;
                if position < raw.len() {
                    position += raw[position..].chars().next().map_or(1, char::len_utf8);
                }
                continue;
            }
            if byte == quote {
                quote = 0;
            }
        } else if matches!(byte, b'\'' | b'"') {
            quote = byte;
        } else if raw[position..].starts_with("/*") {
            let Some(comment_end) = raw[position + 2..].find("*/") else {
                return true;
            };
            position += comment_end + 4;
            continue;
        } else if raw
            .as_bytes()
            .get(position..position + 3)
            .is_some_and(|v| v.eq_ignore_ascii_case(b"var"))
            && raw.as_bytes().get(position + 3) == Some(&b'(')
            && (position == 0
                || !matches!(raw.as_bytes()[position - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | 128..=255))
        {
            let Some((end, dependency, fallback)) = variable_reference(raw, position + 3) else {
                return true;
            };
            if custom_cycle(&dependency, properties, stack, budget)
                || fallback
                    .is_some_and(|fallback| custom_value_cycle(fallback, properties, stack, budget))
            {
                return true;
            }
            position = end;
            continue;
        }
        if byte == b'\\' {
            if selector_escape(raw, &mut position).is_none() {
                return true;
            }
        } else {
            position += raw[position..].chars().next().map_or(1, char::len_utf8);
        }
    }
    false
}

type Priority = (bool, [usize; 8], (u16, u16, u16, u16), usize);
type Candidate<'a> = (
    Priority,
    alloc::borrow::Cow<'a, Declaration>,
    Option<Arc<str>>,
);

fn cascade_custom_properties(
    parent: Option<&Style>,
    candidates: &[Candidate<'_>],
) -> Result<Option<Arc<[(String, Option<String>)]>>, CssError> {
    if !candidates
        .iter()
        .any(|(_, declaration, _)| matches!(declaration.value, Value::Custom(..)))
    {
        return Ok(parent.and_then(|v| v.custom.clone()));
    }
    let mut custom_properties = parent
        .map(|v| v.custom_properties().to_vec())
        .unwrap_or_default();
    let mut custom_layer = None;
    let has_custom_revert_layer = candidates.iter().any(
        |(_, declaration, _)| matches!(&declaration.value, Value::Custom(_, raw) if &**raw == "revert-layer"),
    );
    let mut custom_baseline = Vec::new();
    for (priority, declaration, _) in candidates {
        if has_custom_revert_layer && custom_layer != Some((priority.0, priority.1)) {
            custom_layer = Some((priority.0, priority.1));
            custom_baseline = custom_properties.clone();
        }
        if let Value::Custom(name, raw) = &declaration.value {
            let value = match &**raw {
                "initial" => None,
                "inherit" | "unset" | "revert" => parent
                    .and_then(|v| {
                        v.custom_properties()
                            .iter()
                            .find(|(key, _)| key.as_str() == &**name)
                    })
                    .and_then(|(_, v)| v.clone()),
                "revert-layer" => custom_baseline
                    .iter()
                    .find(|(key, _)| key.as_str() == &**name)
                    .and_then(|(_, v)| v.clone()),
                _ => Some(raw.to_string()),
            };
            if let Some((_, existing)) = custom_properties
                .iter_mut()
                .find(|(key, _)| key.as_str() == &**name)
            {
                *existing = value;
            } else if custom_properties.len() < MAX_CUSTOM_PROPERTIES {
                custom_properties.push((name.to_string(), value));
            } else {
                return Err(CssError {
                    offset: 0,
                    message: "too many custom properties",
                });
            }
        }
    }
    let needs_work = |value: &Option<String>| value.as_deref().is_some_and(needs_expansion);
    if custom_properties.iter().any(|(_, value)| needs_work(value)) {
        let cyclic: Vec<bool> = custom_properties
            .iter()
            .map(|(name, value)| {
                needs_work(value)
                    && custom_cycle(name, &custom_properties, &mut Vec::new(), &mut 4096)
            })
            .collect();
        for ((_, value), cyclic) in custom_properties.iter_mut().zip(cyclic) {
            if cyclic {
                *value = None;
            }
        }
        let expanded: Vec<Option<Option<String>>> = custom_properties
            .iter()
            .map(|(_, value)| match value.as_deref() {
                Some(value) if needs_expansion(value) => {
                    Some(expand_variables(value, &custom_properties, &mut Vec::new()))
                }
                _ => None,
            })
            .collect();
        for ((_, value), expanded) in custom_properties.iter_mut().zip(expanded) {
            if let Some(expanded) = expanded {
                *value = expanded;
            }
        }
    }
    if custom_properties.is_empty() {
        return Ok(None);
    }
    if let Some(inherited) =
        parent.filter(|v| v.custom_properties() == custom_properties.as_slice())
    {
        return Ok(inherited.custom.clone());
    }
    Ok(Some(Arc::from(custom_properties)))
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
    compute_for(
        kind,
        parent,
        index,
        Some((document, node)),
        u64::MAX,
        None,
        None,
    )
}

const SHARED_RECENT: usize = 16;
const PARENT_STYLES_RECENT: usize = 32;
const MAX_PARENT_STYLES: usize = 1 << 16;

struct SharedStyle {
    node: NodeId,
    dom_parent: NodeId,
    parent_token: u32,
    style: Arc<Style>,
}

/// Shared computed style objects. The owner invalidates the cache when cascade
/// inputs change; unchanged text updates can retain these objects.
#[derive(Default)]
pub(crate) struct StyleCache {
    /// Per node index: (parent style token, computed style).
    nodes: Vec<Option<(NodeId, u32, Arc<Style>)>>,
    /// Distinct parent styles seen; token `n + 1` is `parents[n]`, 0 is no parent.
    parents: Vec<Arc<Style>>,
    last_parent: usize,
    shared: Vec<SharedStyle>,
    next_shared: usize,
    /// Per node index: bloom of its ancestors' tags, ids and classes; 0 = unset.
    blooms: Vec<u64>,
    pending: Vec<NodeId>,
    text_generation: Option<u64>,
    computed: usize,
    hits: usize,
    shared_hits: usize,
    intern_hits: usize,
    interned: Vec<Arc<Style>>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StyleCacheStats {
    pub styled_nodes: usize,
    pub unique_styles: usize,
    pub computed_styles: usize,
    pub cache_hits: usize,
    pub shared_hits: usize,
    pub intern_hits: usize,
}

impl StyleCache {
    /// Only for a cache whose owner has already validated all cascade inputs.
    pub(crate) fn node_style(&self, node: NodeId) -> Option<Arc<Style>> {
        self.nodes
            .get(node.index())?
            .as_ref()
            .filter(|(cached, _, _)| *cached == node)
            .map(|(_, _, style)| style.clone())
    }
    pub(crate) fn remember_snapshot_style(
        &mut self,
        document: &Document,
        node: NodeId,
        style: Style,
    ) -> Arc<Style> {
        if let Some(style) = self.node_style(node) {
            return style;
        }
        let style = Arc::new(style);
        self.store(document, node, 0, &style);
        style
    }
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }
    /// Drop one node's own cascade result. Descendants are keyed by their
    /// parent's style value, so a changed parent style misses them naturally.
    pub(crate) fn invalidate_node(&mut self, node: NodeId) {
        if let Some(slot) = self.nodes.get_mut(node.index()) {
            *slot = None;
        }
        self.shared.retain(|entry| entry.node != node);
        self.next_shared = self.next_shared.min(self.shared.len());
    }
    pub(crate) fn begin_frame(&mut self) {
        self.computed = 0;
        self.hits = 0;
        self.shared_hits = 0;
        self.intern_hits = 0;
    }
    pub(crate) fn stats(&self) -> StyleCacheStats {
        let mut pointers = self
            .nodes
            .iter()
            .filter_map(|node| {
                node.as_ref()
                    .map(|(_, _, style)| Arc::as_ptr(style) as usize)
            })
            .collect::<Vec<_>>();
        let styled_nodes = pointers.len();
        pointers.sort_unstable();
        pointers.dedup();
        StyleCacheStats {
            styled_nodes,
            unique_styles: pointers.len(),
            computed_styles: self.computed,
            cache_hits: self.hits,
            shared_hits: self.shared_hits,
            intern_hits: self.intern_hits,
        }
    }

    fn parent_token(&mut self, parent: Option<&Style>) -> Option<u32> {
        let Some(parent) = parent else {
            return Some(0);
        };
        if self
            .parents
            .get(self.last_parent)
            .is_some_and(|candidate| candidate.as_ref() == parent)
        {
            return Some(self.last_parent as u32 + 1);
        }
        let found = self
            .parents
            .iter()
            .enumerate()
            .rev()
            .take(PARENT_STYLES_RECENT)
            .find(|(_, candidate)| candidate.as_ref() == parent)
            .map(|(position, _)| position);
        let position = match found {
            Some(position) => position,
            None if self.parents.len() < MAX_PARENT_STYLES => {
                let shared = self
                    .interned
                    .iter()
                    .rev()
                    .take(PARENT_STYLES_RECENT)
                    .find(|style| style.as_ref() == parent)
                    .cloned()
                    .unwrap_or_else(|| Arc::new(parent.clone()));
                self.parents.push(shared);
                self.parents.len() - 1
            }
            None => return None,
        };
        self.last_parent = position;
        Some(position as u32 + 1)
    }

    fn ancestor_bloom(&mut self, document: &Document, node: NodeId) -> u64 {
        self.pending.clear();
        let mut bloom = 0;
        let mut current = document.parent(node).ok().flatten();
        while let Some(id) = current {
            match self.blooms.get(id.index()).copied() {
                Some(known) if known != 0 => {
                    bloom = known;
                    break;
                }
                _ => {}
            }
            self.pending.push(id);
            current = document.parent(id).ok().flatten();
        }
        while let Some(id) = self.pending.pop() {
            bloom |= document.kind(id).map_or(0, element_bloom);
            if self.blooms.len() <= id.index() {
                self.blooms
                    .resize(document.node_count().max(id.index() + 1), 0);
            }
            self.blooms[id.index()] = bloom;
        }
        bloom
    }

    fn store(&mut self, document: &Document, node: NodeId, token: u32, style: &Arc<Style>) {
        let slot = node.index();
        if self.nodes.len() <= slot {
            self.nodes
                .resize_with(document.node_count().max(slot + 1), || None);
        }
        self.nodes[slot] = Some((node, token, style.clone()));
    }
}

fn same_element(a: &NodeKind, b: &NodeKind) -> bool {
    match (a, b) {
        (
            NodeKind::Element {
                namespace: namespace_a,
                name: name_a,
                attributes: attributes_a,
            },
            NodeKind::Element {
                namespace: namespace_b,
                name: name_b,
                attributes: attributes_b,
            },
        ) => namespace_a == namespace_b && name_a == name_b && attributes_a == attributes_b,
        _ => false,
    }
}

fn control_count<N: AsRef<str>>(
    attributes: &[(N, String)],
    name: &str,
    default: u16,
    max: u16,
) -> u16 {
    attributes
        .iter()
        .find(|(key, _)| key.as_ref() == name)
        .and_then(|(_, value)| value.trim().parse::<u16>().ok())
        .filter(|value| *value != 0)
        .unwrap_or(default)
        .min(max)
}

pub(crate) fn input_type_is_nontext(input_type: &str) -> bool {
    matches!(
        &*ascii_lower(input_type),
        "button"
            | "checkbox"
            | "color"
            | "date"
            | "datetime-local"
            | "file"
            | "hidden"
            | "image"
            | "month"
            | "radio"
            | "range"
            | "reset"
            | "submit"
            | "time"
            | "week"
    )
}

/// `compute_node` through a shared cache: a node is cascaded once per parent
/// style, and siblings with identical elements share one cascade.
pub(crate) fn compute_node_cached(
    document: &Document,
    node: NodeId,
    parent: Option<&Style>,
    index: &StyleIndex,
    cache: &mut StyleCache,
) -> Result<Style, CssError> {
    compute_node_cached_impl(document, node, parent, index, cache, None)
}

pub(crate) fn compute_node_cached_with_text(
    document: &Document,
    node: NodeId,
    parent: Option<&Style>,
    index: &StyleIndex,
    cache: &mut StyleCache,
    text: &dyn TextShaper,
) -> Result<Style, CssError> {
    compute_node_cached_impl(document, node, parent, index, cache, Some(text))
}

fn compute_node_cached_impl(
    document: &Document,
    node: NodeId,
    parent: Option<&Style>,
    index: &StyleIndex,
    cache: &mut StyleCache,
    text: Option<&dyn TextShaper>,
) -> Result<Style, CssError> {
    let generation = text.map_or(0, |text| text.generation());
    if cache.text_generation != Some(generation) {
        if cache.text_generation.is_some() {
            cache.clear();
        }
        cache.text_generation = Some(generation);
    }
    let kind = document.kind(node).map_err(|_| CssError {
        offset: 0,
        message: "invalid style node",
    })?;
    let Some(token) = cache.parent_token(parent) else {
        return compute_for(
            kind,
            parent,
            index,
            Some((document, node)),
            u64::MAX,
            text,
            None,
        );
    };
    if let Some(Some((cached_node, cached_token, style))) = cache.nodes.get(node.index()) {
        if *cached_node == node && *cached_token == token && !style.sibling_position_dependent {
            cache.hits += 1;
            return Ok((**style).clone());
        }
    }
    let is_element = matches!(kind, NodeKind::Element { .. });
    let dom_parent = if is_element && index.siblings_share {
        document.parent(node).ok().flatten()
    } else {
        None
    };
    if let Some(dom_parent) = dom_parent {
        let shared = cache.shared.iter().find(|entry| {
            entry.dom_parent == dom_parent
                && entry.parent_token == token
                && !entry.style.sibling_position_dependent
                && document
                    .kind(entry.node)
                    .is_ok_and(|other| same_element(kind, other))
        });
        if let Some(entry) = shared {
            let style = entry.style.clone();
            cache.shared_hits += 1;
            cache.store(document, node, token, &style);
            return Ok((*style).clone());
        }
    }
    let bloom = if index.uses_ancestor_bloom {
        cache.ancestor_bloom(document, node)
    } else {
        u64::MAX
    };
    let computed = compute_for(
        kind,
        parent,
        index,
        Some((document, node)),
        bloom,
        text,
        None,
    )?;
    cache.computed += 1;
    // Value interning is safe even for contextual selectors: the full cascade
    // has already run for this node before equality is checked.
    let style = if let Some(style) = cache
        .interned
        .iter()
        .rev()
        .take(SHARED_RECENT)
        .find(|style| style.as_ref() == &computed)
    {
        cache.intern_hits += 1;
        style.clone()
    } else {
        let style = Arc::new(computed);
        if cache.interned.len() < MAX_PARENT_STYLES {
            cache.interned.push(style.clone());
        }
        style
    };
    if !style.sibling_position_dependent {
        cache.store(document, node, token, &style);
    }
    if let Some(dom_parent) = dom_parent.filter(|_| !style.sibling_position_dependent) {
        let entry = SharedStyle {
            node,
            dom_parent,
            parent_token: token,
            style: style.clone(),
        };
        if cache.shared.len() < SHARED_RECENT {
            cache.shared.push(entry);
        } else {
            cache.shared[cache.next_shared] = entry;
            cache.next_shared = (cache.next_shared + 1) % SHARED_RECENT;
        }
    }
    Ok((*style).clone())
}

fn compute_for(
    kind: &NodeKind,
    parent: Option<&Style>,
    index: &StyleIndex,
    context: Option<(&Document, NodeId)>,
    ancestor_bloom: u64,
    text: Option<&dyn TextShaper>,
    pseudo_target: Option<PseudoElement>,
) -> Result<Style, CssError> {
    let mut style = Style::initial();
    if pseudo_target.is_some() {
        style.display = Display::Inline;
    }
    if let (
        Some(pseudo),
        NodeKind::Element {
            name,
            namespace: Namespace::Html,
            ..
        },
    ) = (pseudo_target, kind)
    {
        if crate::svg::local_name(name) == "q" {
            let item = match pseudo {
                PseudoElement::Before => Some(GeneratedContentItem::OpenQuote),
                PseudoElement::After => Some(GeneratedContentItem::CloseQuote),
                PseudoElement::Marker => None,
            };
            if let Some(item) = item {
                style.generated_content = Some(alloc::vec![item].into());
            }
        }
    }
    let initial_font = FontSpec::default();
    if pseudo_target.is_none() {
        if let NodeKind::Element {
            name, namespace, ..
        } = kind
        {
            let tag = crate::svg::local_name(name);
            if *namespace == Namespace::Svg && crate::svg::local_name(name) == "svg" {
                style.display = Display::Inline;
            } else if *namespace == Namespace::Html
                && matches!(
                    tag,
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
                        | "noscript"
                        | "q"
                        | "s"
                        | "small"
                        | "span"
                        | "strong"
                        | "sub"
                        | "sup"
                        | "time"
                        | "u"
                )
            {
                style.display = Display::Inline;
            }
        }
    }
    if let Some(parent) = parent {
        style.color = parent.color;
        style.font_size = parent.font_size;
        if style.font != parent.font {
            style.font = parent.font.clone();
        }
        style.svg_fill = parent.svg_fill.clone();
        style.svg_stroke = parent.svg_stroke.clone();
        style.svg_stroke_width = parent.svg_stroke_width;
        style.svg_fill_rule = parent.svg_fill_rule;
        style.svg_clip_rule = parent.svg_clip_rule;
        style.line_height = parent.line_height;
        style.border_spacing = parent.border_spacing;
        style.quotes = parent.quotes.clone();
        style.color_scheme = parent.color_scheme.clone();
        style.list_style_type = parent.list_style_type.clone();
        style.list_style_position = parent.list_style_position;
        style.list_style_image = parent.list_style_image.clone();
        if style.writing_mode != parent.writing_mode {
            style.writing_mode = parent.writing_mode;
        }
        if style.visibility_visible != parent.visibility_visible {
            style.visibility_visible = parent.visibility_visible;
        }
        if style.pointer_events_auto != parent.pointer_events_auto {
            style.pointer_events_auto = parent.pointer_events_auto;
        }
        if style.empty_cells_hide != parent.empty_cells_hide {
            style.empty_cells_hide = parent.empty_cells_hide;
        }
        if style.border_collapse != parent.border_collapse {
            style.border_collapse = parent.border_collapse;
        }
        Value::WhiteSpace(parent.white_space).apply(&mut style);
        Value::TextAlign(parent.text_align).apply(&mut style);
        Value::TextIndent(parent.text_indent).apply(&mut style);
        Value::LetterSpacing(parent.letter_spacing).apply(&mut style);
        Value::WordSpacing(parent.word_spacing).apply(&mut style);
        Value::Direction(parent.direction).apply(&mut style);
    }
    if pseudo_target.is_none() {
        if let (
            Some((document, node)),
            NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            },
        ) = (context, kind)
        {
            let tag = crate::svg::local_name(name);
            let has_dir_attribute = document
                .get_attribute_ns_ref(node, None, "dir")
                .ok()
                .flatten()
                .is_some();
            let is_telephone_input =
                tag == "input" && crate::forms::input_type_state(document, node) == "tel";
            if has_dir_attribute || tag == "bdi" || is_telephone_input {
                // The HTML UA sheet initializes CSS `direction` from HTML
                // directionality. This is a cascade starting value, so
                // author declarations can still override it below.
                style.direction =
                    match crate::directionality::resolved_element_directionality(document, node) {
                        crate::directionality::Direction::Ltr => Direction::Ltr,
                        crate::directionality::Direction::Rtl => Direction::Rtl,
                    };
            }
        }
    }
    if pseudo_target.is_none() {
        if let NodeKind::Element {
            name,
            namespace,
            attributes,
            ..
        } = kind
        {
            let tag = crate::svg::local_name(name);
            if *namespace == Namespace::Html {
                match tag {
                    "body" => style.margin_sides = [8.0; 4],
                    "li" => style.display = Display::ListItem,
                    "ol" | "ul" | "menu" => {
                        style.margin_sides[0] = style.font_size;
                        style.margin_sides[2] = style.font_size;
                        match style.direction {
                            Direction::Ltr => style.padding_sides[3] = 40.0,
                            Direction::Rtl => style.padding_sides[1] = 40.0,
                        }
                        let reversed =
                            tag == "ol" && crate::svg::attribute(attributes, "reversed").is_some();
                        let start = crate::svg::attribute(attributes, "start")
                            .and_then(|value| value.trim().parse::<i64>().ok())
                            .unwrap_or(1);
                        style.counter_reset = Some(Arc::from([CounterDirective {
                            name: Arc::from("list-item"),
                            value: clamp_css_counter(start.saturating_sub(1)),
                            reversed,
                            explicit_value: crate::svg::attribute(attributes, "start").is_some(),
                            math_expression: None,
                        }]));
                        style.list_style_type = if tag == "ol" {
                            ListStyleType::Decimal
                        } else {
                            ListStyleType::Disc
                        };
                    }
                    "b" | "strong" | "th" => {
                        style.font.weight = relative_font_weight(-1, style.font.weight)
                    }
                    "i" | "em" | "cite" | "var" | "address" => style.font.style = FontStyle::Italic,
                    "pre" => Value::WhiteSpace(WhiteSpace::Pre).apply(&mut style),
                    "nobr" => Value::WhiteSpace(WhiteSpace::NoWrap).apply(&mut style),
                    "input" => {
                        let input_type = crate::svg::attribute(attributes, "type")
                            .map_or_else(|| "text".into(), ascii_lower);
                        style.display = Display::InlineBlock;
                        style.font_size = 13.333_333;
                        style.font = FontSpec {
                            families: Some(alloc::vec![Arc::<str>::from("Arial")].into()),
                            weight: 400,
                            style: FontStyle::Normal,
                            stretch: 100.0,
                            size_adjust: None,
                        };
                        style.line_height = LineHeight::Normal;
                        style.border_solid = true;
                        style.border_style = BorderStyle::Solid;
                        style.border_width = 1.0;
                        style.border_color = Rgba {
                            r: 118,
                            g: 118,
                            b: 118,
                            a: 255,
                        };
                        style.background = Rgba {
                            r: 255,
                            g: 255,
                            b: 255,
                            a: 255,
                        };
                        if !input_type_is_nontext(&input_type) {
                            let size = control_count(attributes, "size", 20, 256);
                            style.width = Some(size as f32 * style.font_size * 0.52);
                            style.height = Some(style.font_size * 1.2);
                            style.padding_sides = [2.0; 4];
                        } else if input_type != "hidden" {
                            // Compact native controls remain visible and never paint
                            // their `value` attribute as text.
                            style.width = Some(13.0);
                            style.height = Some(13.0);
                        }
                    }
                    "textarea" => {
                        style.display = Display::InlineBlock;
                        style.font_size = 13.333_333;
                        style.font = FontSpec {
                            families: Some(alloc::vec![Arc::<str>::from("monospace")].into()),
                            weight: 400,
                            style: FontStyle::Normal,
                            stretch: 100.0,
                            size_adjust: None,
                        };
                        style.line_height = LineHeight::Normal;
                        Value::WhiteSpace(WhiteSpace::PreWrap).apply(&mut style);
                        style.border_solid = true;
                        style.border_style = BorderStyle::Solid;
                        style.border_width = 1.0;
                        style.border_color = Rgba {
                            r: 118,
                            g: 118,
                            b: 118,
                            a: 255,
                        };
                        style.background = Rgba {
                            r: 255,
                            g: 255,
                            b: 255,
                            a: 255,
                        };
                        let cols = control_count(attributes, "cols", 20, 256);
                        let rows = control_count(attributes, "rows", 2, 256);
                        style.width = Some(cols as f32 * style.font_size * 0.6);
                        style.height = Some(rows as f32 * style.font_size * 1.2);
                        style.padding_sides = [2.0; 4];
                    }
                    _ => {}
                }
                if tag == "li" {
                    if let Some(value) = crate::svg::attribute(attributes, "value")
                        .and_then(|value| value.trim().parse::<i64>().ok())
                    {
                        style.counter_set = Some(Arc::from([CounterDirective {
                            name: Arc::from("list-item"),
                            value: clamp_css_counter(value),
                            reversed: false,
                            explicit_value: true,
                            math_expression: None,
                        }]));
                    }
                }
                if matches!(tag, "li" | "ol" | "ul" | "menu") {
                    let type_attribute = crate::svg::attribute(attributes, "type");
                    if let Some(value) = type_attribute.and_then(html_list_style_type) {
                        style.list_style_type = value;
                    }
                }
                style.display = match tag {
                    "table" => Display::Table,
                    "caption" => Display::TableCaption,
                    "thead" | "tbody" | "tfoot" => Display::TableRowGroup,
                    "tr" => Display::TableRow,
                    "td" | "th" => Display::TableCell,
                    _ => style.display,
                };
                if tag == "dialog"
                    && context.is_some_and(|(document, node)| {
                        !crate::top_layer::dialog_is_open(document, node)
                    })
                {
                    style.display = Display::None;
                }
                if context.is_some_and(|(document, node)| {
                    crate::top_layer::popover_mode(document, node).is_some()
                        && !crate::top_layer::matches_popover_open(document, node)
                        && !(tag == "dialog" && crate::top_layer::dialog_is_open(document, node))
                }) {
                    style.display = Display::None;
                }
            }
            // HTML rendering defaults remain lower priority than author CSS.
            if attributes.iter().any(|(key, value)| {
                crate::svg::local_name(key) == "hidden"
                    && !value.eq_ignore_ascii_case("until-found")
            }) && tag != "embed"
            {
                style.display = Display::None;
            }
        }
    }
    let mut candidates: Vec<Candidate<'_>> = Vec::new();
    if pseudo_target.is_none()
        && context.is_some_and(|(document, _)| {
            document.is_html_document() && document.scripting_enabled()
        })
        && matches!(
            kind,
            NodeKind::Element {
                name,
                namespace: Namespace::Html,
                ..
            } if crate::svg::local_name(name) == "noscript"
        )
    {
        // HTML's user-agent sheet has
        // `@media (scripting) { noscript { display: none !important } }`.
        // The current cascade has no explicit origin axis; use its reserved
        // origin slot above stylesheet, inline, and animation candidates so
        // UA-important display cannot be overridden by author-important CSS.
        let priority = (true, [usize::MAX; 8], (3, 0, 0, 0), usize::MAX);
        candidates.push((
            priority,
            alloc::borrow::Cow::Owned(Declaration {
                value: Value::Display(Display::None),
                important: true,
            }),
            index.document_base_url.clone(),
        ));
    }
    if pseudo_target.is_none() {
        if let NodeKind::Element {
            name,
            namespace: Namespace::Svg,
            attributes,
            ..
        } = kind
        {
            let tag = crate::svg::local_name(name);
            // SVG presentation attributes participate at author origin with zero
            // specificity, below every stylesheet declaration.
            for (name, raw) in attributes {
                let local = crate::xml::split_qname(name.as_str())
                    .map(|(_, local)| local)
                    .unwrap_or_else(|| name.as_str());
                let geometry = match tag {
                    "rect" => matches!(local, "x" | "y" | "width" | "height" | "rx" | "ry"),
                    "circle" => matches!(local, "cx" | "cy" | "r"),
                    "ellipse" => matches!(local, "cx" | "cy" | "rx" | "ry"),
                    _ => false,
                };
                if !matches!(
                    local,
                    "fill"
                        | "stroke"
                        | "stroke-width"
                        | "fill-rule"
                        | "clip-path"
                        | "clip-rule"
                        | "stop-color"
                        | "stop-opacity"
                ) && !geometry
                {
                    continue;
                }
                for mut declaration in typed_declarations(local, raw, 0)? {
                    declaration.important = false;
                    candidates.push((
                        (false, [0; 8], (0, 0, 0, 0), 0),
                        alloc::borrow::Cow::Owned(declaration),
                        index.document_base_url.clone(),
                    ));
                }
            }
        }
    }
    let mut in_document: Option<bool> = None;
    let mut apply = |order: usize| {
        let mask = index.ancestor_masks[order];
        if ancestor_bloom & mask != mask {
            return;
        }
        let rule = &index.rules[order];
        if rule.selector.pseudo_element != pseudo_target {
            return;
        }
        if !rule
            .media
            .iter()
            .all(|query| media_matches(query, index.environment))
        {
            return;
        }
        match context {
            Some((document, node)) => {
                // Document rules do not reach into shadow trees except through
                // `::part()`; scoped rules apply only within their own tree.
                if rule.scope.is_none()
                    && rule.selector.part.is_none()
                    && !*in_document.get_or_insert_with(|| {
                        document.root_node(node, false) == Ok(document.root())
                    })
                {
                    return;
                }
                if !rule.matches_at(document, node) {
                    return;
                }
            }
            None => {
                if rule.scope.is_some() || !rule.selector.matches(kind) {
                    return;
                }
            }
        }
        let (ids, classes, tags) = rule.selector.specificity;
        let normal_layer = rule.layer_path.unwrap_or([usize::MAX - 1; 8]);
        let important_layer = rule
            .layer_path
            .map(|path| path.map(|v| usize::MAX - 1 - v))
            .unwrap_or([0; 8]);
        for declaration in rule.declarations.iter() {
            let layer = if declaration.important {
                important_layer
            } else {
                normal_layer
            };
            let priority = (
                declaration.important,
                layer,
                (0, ids, classes, tags),
                order + 1,
            );
            let source_url = rule
                .source_url
                .clone()
                .or_else(|| index.document_base_url.clone());
            candidates.push((
                priority,
                alloc::borrow::Cow::Borrowed(declaration),
                source_url,
            ));
        }
    };
    if let NodeKind::Element {
        namespace,
        name,
        attributes,
        ..
    } = kind
    {
        for &order in &index.universal {
            apply(order);
        }
        let local_name = crate::svg::local_name(name);
        for &order in index.matches_tag(local_name) {
            if namespace == &Namespace::Html
                || index.rules[order].selector.tag.as_deref() == Some(local_name)
            {
                apply(order);
            }
        }
        let html_attribute_names = namespace == &Namespace::Html
            && context.is_some_and(|(document, _)| document.is_html_document());
        for (position, (key, value)) in attributes.iter().enumerate() {
            if context.is_some_and(|(document, node)| {
                document
                    .attribute_namespace_uri_at(node, position)
                    .is_some()
            }) {
                continue;
            }
            let id_name = if html_attribute_names {
                key.as_str().eq_ignore_ascii_case("id")
            } else {
                key.as_str() == "id"
            };
            if id_name {
                for &order in index.matches(&index.ids, value, 1) {
                    apply(order);
                }
            }
            let class_name = if html_attribute_names {
                key.as_str().eq_ignore_ascii_case("class")
            } else {
                key.as_str() == "class"
            };
            if class_name {
                for class in value.split_ascii_whitespace() {
                    for &order in index.matches(&index.classes, class, 2) {
                        apply(order);
                    }
                }
            }
        }
    }
    if pseudo_target.is_none() {
        if matches!(kind, NodeKind::Element { .. }) {
            let inline = if let Some((document, node)) = context {
                document
                    .get_attribute_ns_ref(node, None, "style")
                    .map_err(|_| CssError {
                        offset: 0,
                        message: "invalid inline style node",
                    })?
            } else {
                match kind {
                    NodeKind::Element { attributes, .. } => attributes
                        .iter()
                        .find(|(name, _)| name.as_str() == "style")
                        .map(|(_, value)| value.as_str()),
                    _ => None,
                }
            };
            if let Some(inline) = inline {
                for declaration in declarations(inline, 0)? {
                    let priority = (
                        declaration.important,
                        [usize::MAX; 8],
                        (1, 0, 0, 0),
                        usize::MAX,
                    );
                    candidates.push((
                        priority,
                        alloc::borrow::Cow::Owned(declaration),
                        index.document_base_url.clone(),
                    ));
                }
            }
        }
    }
    if pseudo_target.is_none() {
        if let Some((_, node)) = context {
            if let Some((_, declarations)) = index
                .animation_declarations
                .iter()
                .find(|(target, _)| *target == node)
            {
                // CSS animations outrank all normal author declarations (including
                // inline styles) but remain below every !important declaration.
                for declaration in declarations {
                    let priority = (false, [usize::MAX; 8], (2, 0, 0, 0), usize::MAX - 1);
                    candidates.push((
                        priority,
                        alloc::borrow::Cow::Borrowed(declaration),
                        index.document_base_url.clone(),
                    ));
                }
            }
        }
    }
    candidates.sort_by_key(|(priority, _, _)| *priority);
    style.custom = cascade_custom_properties(parent, &candidates)?;
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
    let needs_expansion_pass = candidates.iter().any(|(_, declaration, _)| {
        matches!(declaration.value, Value::Custom(..) | Value::Deferred(..))
    });
    let expanded = if !needs_expansion_pass {
        candidates
    } else {
        let mut expanded: Vec<Candidate<'_>> = Vec::new();
        for (priority, declaration, source_url) in candidates {
            let values: Vec<Declaration> = match &declaration.value {
                Value::Custom(_, _) => continue,
                Value::Deferred(name, raw) => {
                    let resolved =
                        expand_variables(raw, style.custom_properties(), &mut Vec::new());
                    let parsed = resolved
                        // Substitution has already processed variable functions.
                        // A quoted literal containing `var(` must now be parsed
                        // as a normal value, rather than deferred a second time.
                        .and_then(|raw| {
                            declarations_with_variables(&alloc::format!("{name}:{raw}"), 0, false)
                                .ok()
                        })
                        .unwrap_or_default();
                    if parsed.is_empty() {
                        slots(name)
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
                _ => {
                    expanded.push((priority, declaration, source_url));
                    continue;
                }
            };
            for declaration in values {
                expanded.push((
                    priority,
                    alloc::borrow::Cow::Owned(declaration),
                    source_url.clone(),
                ));
            }
        }
        expanded
    };
    let has_revert_layer = expanded
        .iter()
        .any(|(_, declaration, _)| matches!(declaration.value, Value::RevertLayer(_)));
    let reverted = expanded
        .iter()
        .any(|(_, declaration, _)| matches!(declaration.value, Value::Revert(_)))
        .then(|| style.clone());
    let mut initial = None;
    for pass in 0..4 {
        let font_pass = pass == 0;
        if !font_pass && root && style.font_size != style.root_font_size {
            style.root_font_size = style.font_size;
        }
        let mut layer_key = None;
        let mut layer_baseline = has_revert_layer.then(|| style.clone());
        for (priority, declaration, source_url) in &expanded {
            let slot = declaration.value.slot();
            let color_dependent = matches!(slot, 10 | 53 | 58 | 59 | 60 | 68 | 69 | 107..=110 | 119..=122 | 141 | 142);
            if (pass == 0 && slot != 7)
                || (pass == 1 && !matches!(slot, 1 | 140))
                || (pass == 2 && (matches!(slot, 1 | 7 | 140) || color_dependent))
                || (pass == 3 && !color_dependent)
            {
                continue;
            }
            if layer_key != Some((priority.0, priority.1)) {
                layer_key = Some((priority.0, priority.1));
                if has_revert_layer {
                    layer_baseline = Some(style.clone());
                }
            }
            if *priority >= winners[slot] {
                winners[slot] = *priority;
                if style.relative_lengths.iter().any(|(key, _)| *key == slot) {
                    style.relative_lengths.retain(|(key, _)| *key != slot);
                }
                if style
                    .relative_expressions
                    .iter()
                    .any(|expression| expression.slot == slot)
                {
                    style
                        .relative_expressions
                        .retain(|expression| expression.slot != slot);
                }
                if let Value::RevertLayer(_) = declaration.value {
                    if let Some(baseline) = &layer_baseline {
                        copy_slot(slot, baseline, &mut style);
                    }
                } else if let Value::Revert(_) = declaration.value {
                    if let Some(reverted) = &reverted {
                        copy_slot(slot, reverted, &mut style);
                    }
                } else if let Value::Default(_, inherit) = declaration.value {
                    let initial: &Style = initial.get_or_insert_with(|| {
                        let mut initial = Style::initial();
                        initial.display = Display::Inline;
                        initial
                    });
                    copy_slot(
                        slot,
                        if inherit {
                            parent.unwrap_or(initial)
                        } else {
                            initial
                        },
                        &mut style,
                    );
                    if slot == 10 && !inherit {
                        style.border_color = style.color;
                    }
                } else if let Value::GridRaw(slot, raw) = &declaration.value {
                    let context = style_length_context(text, &style, index.environment, None);
                    if let Some(value) = grid_resolve(*slot, raw, context) {
                        value.apply(&mut style);
                    }
                } else if matches!(
                    declaration.value,
                    Value::TransformRaw(_) | Value::TransformOriginRaw(_)
                ) {
                    let context = style_length_context(text, &style, index.environment, None);
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
                    let context = style_length_context(text, &style, index.environment, None);
                    style.shadows = box_shadows(raw, style.color, Some(context));
                } else if let Value::BackgroundImages(images) = &declaration.value {
                    style.background_images = images
                        .as_ref()
                        .map(|images| resolve_background_images(images, source_url.as_deref()));
                } else if let Value::GeneratedContent(GeneratedContent::Items(items)) =
                    &declaration.value
                {
                    style.generated_content = Some(resolve_generated_content_items(
                        items,
                        source_url.as_deref(),
                    ));
                    style.content_none = false;
                } else if let Value::BackgroundImageRaw(raw) = &declaration.value {
                    let context = style_length_context(text, &style, index.environment, None);
                    style.background_images = background_images(raw, style.color, Some(context))
                        .filter(|images| !images.is_empty())
                        .map(|images| resolve_background_images(&images, source_url.as_deref()));
                } else if let Value::BackgroundPositionRaw(raw) = &declaration.value {
                    let context = style_length_context(text, &style, index.environment, None);
                    style.background_position = background_positions(raw, Some(context));
                } else if let Value::BackgroundSizeRaw(raw) = &declaration.value {
                    let context = style_length_context(text, &style, index.environment, None);
                    style.background_size = background_sizes(raw, Some(context));
                } else if let Value::ColorRaw(color_slot, raw) = &declaration.value {
                    let current = if *color_slot == 1 {
                        parent.map_or(Style::initial().color, |parent| parent.color)
                    } else {
                        style.color
                    };
                    let (sibling_index, sibling_count) = context
                        .map(|(document, node)| element_sibling_position(document, node))
                        .unwrap_or((1, 1));
                    let value = sibling_context_colors(raw, sibling_index, sibling_count)
                        .and_then(|(resolved, _)| color_with_context(&resolved, current));
                    if let Some(value) = value.and_then(|color| color_at_slot(*color_slot, color)) {
                        value.apply(&mut style);
                        style.sibling_position_dependent |=
                            sibling_context_colors(raw, 1, 1).is_some_and(|(_, replaced)| replaced);
                    }
                } else if let Value::BorderRadiusRaw(raw) = &declaration.value {
                    let context = style_length_context(text, &style, index.environment, None);
                    if let Some(corners) = border_radius_values(raw, context) {
                        Value::BorderRadii(corners).apply(&mut style);
                    }
                } else if let Value::BorderRadiusShorthandCornerRaw(corner, raw) =
                    &declaration.value
                {
                    let context = style_length_context(text, &style, index.environment, None);
                    if let Some(value) = border_radius_values(raw, context)
                        .and_then(|corners| corners.get(*corner).cloned())
                    {
                        Value::BorderRadiusCorner(*corner, value).apply(&mut style);
                    }
                } else if let Value::BorderRadiusCornerRaw(corner, raw) = &declaration.value {
                    let context = style_length_context(text, &style, index.environment, None);
                    if let Some(value) = border_radius_corner_value(raw, context) {
                        Value::BorderRadiusCorner(*corner, value).apply(&mut style);
                    }
                } else if let Value::TextIndentRaw(raw) = &declaration.value {
                    let context = style_length_context(text, &style, index.environment, None);
                    if let Some(value) = background_length(raw, context) {
                        Value::TextIndent(value).apply(&mut style);
                    }
                } else if let Value::ContextLength(_, raw, nonnegative) = &declaration.value {
                    let context_font_size = if font_pass {
                        parent.map(|value| value.font_size).unwrap_or(16.0)
                    } else {
                        style.font_size
                    };
                    let context_root_font = if font_pass {
                        root_font
                    } else {
                        style.root_font_size
                    };
                    let context_font = if font_pass {
                        parent.map_or(&initial_font, |value| value.font_spec())
                    } else {
                        style.font_spec()
                    };
                    let percent = Some(if font_pass {
                        context_font_size
                    } else if slot == 13 {
                        style.font_size
                    } else {
                        0.0
                    });
                    let context = font_length_context(
                        text,
                        context_font_size,
                        context_root_font,
                        context_font,
                        index.environment,
                        percent,
                    );
                    let Some(pixels) = contextual_length(raw, Some(context)) else {
                        continue;
                    };
                    if raw.contains('%') && !font_pass && slot != 13 {
                        if comparison_function(raw) {
                            style.relative_expressions.push(RelativeExpression {
                                slot,
                                raw: Arc::from(raw.as_ref()),
                                context,
                                nonnegative: *nonnegative,
                            });
                        } else {
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
                } else if let Value::GapRaw(slot, raw) = &declaration.value {
                    let context = style_length_context(text, &style, index.environment, None);
                    if let Some(value) = background_length(raw, context) {
                        Value::GapLength(*slot, value).apply(&mut style);
                    }
                } else if let Value::FontWeight(weight) = declaration.value {
                    let inherited = parent.map_or(400, |parent| parent.font.weight);
                    style.font.weight = relative_font_weight(weight, inherited);
                } else {
                    declaration.value.apply(&mut style);
                }
            }
        }
    }
    // The HTML hidden input rule is an important UA declaration: author
    // display declarations cannot turn a hidden form control into a box.
    if pseudo_target.is_none() {
        if let NodeKind::Element {
            name, attributes, ..
        } = kind
        {
            if name == "input"
                && attributes
                    .iter()
                    .any(|(key, value)| key == "type" && value.eq_ignore_ascii_case("hidden"))
            {
                style.display = Display::None;
            }
        }
    }
    let vertical = matches!(
        style.writing_mode,
        WritingMode::VerticalRl | WritingMode::VerticalLr
    );
    let (inline_start, inline_end, block_start, block_end) = if vertical {
        let inline = if style.direction == Direction::Rtl {
            (2, 0)
        } else {
            (0, 2)
        };
        let block = if style.writing_mode == WritingMode::VerticalRl {
            (1, 3)
        } else {
            (3, 1)
        };
        (inline.0, inline.1, block.0, block.1)
    } else {
        let inline = if style.direction == Direction::Rtl {
            (1, 3)
        } else {
            (3, 1)
        };
        (inline.0, inline.1, 0, 2)
    };
    for (logical, physical) in [
        (0, inline_start),
        (1, inline_end),
        (2, block_start),
        (3, block_end),
    ] {
        if let Some(value) = style.logical_offsets[logical] {
            match physical {
                0 => style.top = value,
                1 => style.right = value,
                2 => style.bottom = value,
                _ => style.left = value,
            }
        }
    }
    if vertical {
        if let Some(inline) = style.logical_inline_size {
            style.height = Some(inline);
        }
        if let Some(block) = style.logical_block_size {
            style.width = Some(block);
        }
        if let Some(value) = style.logical_min_size[0] {
            style.min_height = value;
            style.min_height_auto = false;
        }
        if let Some(value) = style.logical_max_size[0] {
            style.max_height = Some(value);
        }
        if let Some(value) = style.logical_min_size[1] {
            style.min_width = value;
            style.min_width_auto = false;
        }
        if let Some(value) = style.logical_max_size[1] {
            style.max_width = Some(value);
        }
        if let Some(value) = style.logical_padding_inline[0] {
            style.padding_sides[inline_start] = value;
        }
        if let Some(value) = style.logical_padding_inline[1] {
            style.padding_sides[inline_end] = value;
        }
        if let Some(value) = style.logical_padding_block[0] {
            style.padding_sides[block_start] = value;
        }
        if let Some(value) = style.logical_padding_block[1] {
            style.padding_sides[block_end] = value;
        }
        if let Some(value) = style.logical_margin_inline[0] {
            style.margin_sides[inline_start] = value;
        }
        if let Some(value) = style.logical_margin_inline[1] {
            style.margin_sides[inline_end] = value;
        }
        for (index, side) in [(0, block_start), (1, block_end)] {
            if let Some(value) = style.logical_margin_block[index] {
                if let Some(value) = value {
                    style.margin_sides[side] = value;
                    style.margin_auto[side] = false;
                } else {
                    style.margin_sides[side] = 0.0;
                    style.margin_auto[side] = true;
                }
            }
        }
    } else {
        if let Some(inline) = style.logical_inline_size {
            style.width = Some(inline);
        }
        if let Some(block) = style.logical_block_size {
            style.height = Some(block);
        }
        if let Some(value) = style.logical_min_size[0] {
            style.min_width = value;
            style.min_width_auto = false;
        }
        if let Some(value) = style.logical_max_size[0] {
            style.max_width = Some(value);
        }
        if let Some(value) = style.logical_min_size[1] {
            style.min_height = value;
            style.min_height_auto = false;
        }
        if let Some(value) = style.logical_max_size[1] {
            style.max_height = Some(value);
        }
        if let Some(value) = style.logical_padding_inline[0] {
            style.padding_sides[inline_start] = value;
        }
        if let Some(value) = style.logical_padding_inline[1] {
            style.padding_sides[inline_end] = value;
        }
        if let Some(value) = style.logical_padding_block[0] {
            style.padding_sides[0] = value;
        }
        if let Some(value) = style.logical_padding_block[1] {
            style.padding_sides[2] = value;
        }
        if let Some(value) = style.logical_margin_inline[0] {
            style.margin_sides[inline_start] = value;
        }
        if let Some(value) = style.logical_margin_inline[1] {
            style.margin_sides[inline_end] = value;
        }
        for (index, side) in [(0, block_start), (1, block_end)] {
            if let Some(value) = style.logical_margin_block[index] {
                if let Some(value) = value {
                    style.margin_sides[side] = value;
                    style.margin_auto[side] = false;
                } else {
                    style.margin_sides[side] = 0.0;
                    style.margin_auto[side] = true;
                }
            }
        }
    }
    for (logical, physical) in [
        (0, inline_start),
        (1, inline_end),
        (2, block_start),
        (3, block_end),
    ] {
        if let Some(value) = style.logical_border_width[logical] {
            style.border_width_sides[physical] = Some(value);
        }
        if let Some(value) = style.logical_border_color[logical] {
            style.border_color_sides[physical] = Some(value);
        }
        if let Some(value) = style.logical_border_style[logical] {
            style.border_style_sides[physical] = (value != style.border_style).then_some(value);
            let paints = value.paints();
            style.border_solid_sides[physical] = (paints != style.border_solid).then_some(paints);
        }
    }
    // Preserve the existing fast uniform-border path when all four logical
    // edges resolve to the same border. Layout can then use this without
    // changing the box metrics used by the common all-around-border case.
    if let (
        [Some(top), Some(right), Some(bottom), Some(left)],
        [Some(top_color), Some(right_color), Some(bottom_color), Some(left_color)],
        [Some(top_solid), Some(right_solid), Some(bottom_solid), Some(left_solid)],
    ) = (
        style.border_width_sides,
        style.border_color_sides,
        style.border_solid_sides,
    ) {
        if top == right
            && top == bottom
            && top == left
            && top_color == right_color
            && top_color == bottom_color
            && top_color == left_color
            && top_solid == right_solid
            && top_solid == bottom_solid
            && top_solid == left_solid
        {
            style.border_width = top;
            style.border_color = top_color;
            style.border_solid = top_solid;
        }
    }
    if winners[10] == (false, [0; 8], (0, 0, 0, 0), 0)
        && style.border_color_sides.iter().all(Option::is_none)
    {
        style.border_color = style.color;
    }
    // Visible/clip compute to auto/hidden if the other axis creates a scroll container.
    let (x, y) = (style.overflow_x, style.overflow_y);
    let normalize = |axis: Overflow, other: Overflow| {
        if other.scroll_container() {
            match axis {
                Overflow::Visible => Overflow::Auto,
                Overflow::Clip => Overflow::Hidden,
                _ => axis,
            }
        } else {
            axis
        }
    };
    if normalize(x, y) != x {
        style.overflow_x = normalize(x, y);
    }
    if normalize(y, x) != y {
        style.overflow_y = normalize(y, x);
    }
    style.overflow_clip = style.overflow_x.clips() || style.overflow_y.clips();
    if style.text_align == TextAlign::MatchParent {
        style.text_align = parent.map_or(TextAlign::Start, |parent| match parent.text_align {
            TextAlign::Start if parent.direction == Direction::Rtl => TextAlign::Right,
            TextAlign::Start => TextAlign::Left,
            TextAlign::End if parent.direction == Direction::Rtl => TextAlign::Left,
            TextAlign::End => TextAlign::Right,
            inherited => inherited,
        });
    }
    if style
        .extras
        .as_deref()
        .is_some_and(|extras| extras == &INITIAL_EXTRAS)
    {
        style.extras = None;
    }
    // The UA's list-item increment is the initial declaration for every
    // list-item box. An authored counter-increment (including `none`) replaces
    // it through the ordinary cascade.
    if style.display == Display::ListItem && winners[161].3 == 0 {
        style.counter_increment = Some(Arc::from([CounterDirective {
            name: Arc::from("list-item"),
            value: 1,
            reversed: false,
            explicit_value: false,
            math_expression: None,
        }]));
    }
    Ok(style)
}

#[cfg(test)]
mod tests {
    #[test]
    fn unknown_at_rules_at_eof_preserve_preceding_rules() {
        for suffix in [
            "@unknown {",
            "@unknown { color: red",
            "@unknown { /* unfinished",
        ] {
            let parsed =
                super::parse_stylesheet(&alloc::format!("div {{ color: red }} {suffix}")).unwrap();
            assert_eq!(parsed.rules.len(), 1);
        }
        assert!(super::parse_stylesheet("@keyframes fade { from { opacity: 0 }").is_err());
    }
    #[test]
    fn omitted_font_feature_setting_value_defaults_to_one() {
        assert_eq!(
            super::parse_font_settings("'liga'", false),
            Some(String::from("\"liga\""))
        );
        assert_eq!(
            super::parse_font_settings("'liga' 1", false),
            Some(String::from("\"liga\""))
        );
        assert_eq!(
            super::parse_font_settings("'liga' off", false),
            Some(String::from("\"liga\" 0"))
        );
        assert_eq!(super::parse_font_settings("'liga' invalid", false), None);
        assert_eq!(super::parse_font_settings("'liga'", true), None);
    }
    #[test]
    fn import_extractor_decodes_urls_and_enforces_top_level_placement() {
        let source = r#"
          @charset "utf-8";
          @layer base;
          @import /* before url */ "theme\2e css" layer(theme.colors)
            supports(display: grid) screen and (min-width: 200px);
          @layer after-import;
          @import "late.css";
          @media screen { @import "nested.css"; }
          div { color: red }
        "#;
        let found = imports(source).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(&*found[0].url, "theme.css");
        assert_eq!(
            found[0].layer,
            Some(ImportLayer::Named(Arc::from("theme.colors")))
        );
        assert_eq!(found[0].supports.as_deref(), Some("display: grid"));
        assert_eq!(
            found[0].media.as_deref(),
            Some("screen and (min-width: 200px)")
        );
        assert!(import_conditions_match(
            &found[0],
            MediaEnvironment::default()
        ));
        assert!(!import_conditions_match(
            &found[0],
            MediaEnvironment {
                width: 100.0,
                ..MediaEnvironment::default()
            }
        ));
        assert!(source[found[0].span.clone()].starts_with("@import"));
        assert!(source[found[0].span.clone()].ends_with(';'));
        assert_eq!(
            resolve_import_url(&found[0], "https://example.test/css/main.css").as_deref(),
            Some("https://example.test/css/theme.css")
        );
        assert_eq!(
            imports("@import 'anonymous.css' layer();").unwrap()[0].layer,
            Some(ImportLayer::Anonymous)
        );
    }

    #[test]
    fn stylesheet_graph_preserves_source_media_fonts_and_layer_order() {
        let root_url: Arc<str> = "https://example.test/css/main.css".into();
        let theme_url: Arc<str> = "https://example.test/css/theme.css".into();
        let nested_url: Arc<str> = "https://example.test/css/nested.css".into();
        let root_text: Arc<str> = concat!(
            "@import 'theme.css' layer(theme) supports(display: grid) screen;",
            "@import 'missing.css' layer(missing);",
            "div { color: black }"
        )
        .into();
        let theme_text: Arc<str> = concat!(
            "@import 'nested.css' layer(sub) screen;",
            "@font-face { font-family: ThemeFace; src: url('../fonts/theme.woff2') }",
            "div { --tile: url('../img/tile.png'); background-image: var(--tile) }"
        )
        .into();
        let nested_text: Arc<str> =
            "@font-face { font-family: NestedFace; src: url('../fonts/nested.woff2') }".into();

        let nested = StylesheetSource {
            url: nested_url,
            imports: Vec::new(),
            text: nested_text,
        };
        let mut theme_imports = imports(&theme_text).unwrap();
        assert_eq!(theme_imports.len(), 1);
        let theme = StylesheetSource {
            url: theme_url,
            text: theme_text,
            imports: alloc::vec![LoadedImport {
                rule: theme_imports.remove(0),
                source: Some(Box::new(nested)),
            }],
        };
        let mut root_imports = imports(&root_text).unwrap();
        assert_eq!(root_imports.len(), 2);
        let root = StylesheetSource {
            url: root_url,
            text: root_text,
            imports: alloc::vec![
                LoadedImport {
                    rule: root_imports.remove(0),
                    source: Some(Box::new(theme)),
                },
                LoadedImport {
                    rule: root_imports.remove(0),
                    source: None,
                },
            ],
        };

        let mut parsed = parse_graph(&root, MediaEnvironment::default()).unwrap();
        assert_eq!(parsed.layers.as_ref(), &["theme", "theme.sub", "missing"]);
        assert_eq!(parsed.rules.len(), 2);
        let imported_rule = parsed
            .rules
            .iter()
            .find(|rule| rule.source_url.as_deref() == Some("https://example.test/css/theme.css"))
            .unwrap();
        assert_eq!(imported_rule.media.as_ref(), &[Arc::<str>::from("screen")]);
        assert_eq!(imported_rule.layer, Some(0));

        assert_eq!(parsed.font_faces.len(), 2);
        let theme_face = parsed
            .font_faces
            .iter()
            .find(|face| face.family.as_ref() == "ThemeFace")
            .unwrap();
        assert_eq!(
            theme_face.source_url.as_deref(),
            Some("https://example.test/css/theme.css")
        );
        assert_eq!(
            theme_face.supports.as_ref(),
            &[Arc::<str>::from("display: grid")]
        );
        let nested_face = parsed
            .font_faces
            .iter()
            .find(|face| face.family.as_ref() == "NestedFace")
            .unwrap();
        assert_eq!(
            nested_face.source_url.as_deref(),
            Some("https://example.test/css/nested.css")
        );
        assert_eq!(nested_face.layer, Some(1));

        canonicalize_font_faces(core::slice::from_mut(&mut parsed));
        let nested_face = parsed
            .font_faces
            .iter()
            .find(|face| face.family.as_ref() == "NestedFace")
            .unwrap();
        assert_eq!(nested_face.layer_path.unwrap()[..2], [1, 2]);

        let index = StyleIndex::new(parsed.rules.clone());
        let style = compute(&element(""), None, &index).unwrap();
        assert!(matches!(
            style.background_images.as_deref().and_then(|images| images.first()),
            Some(BackgroundImage::Url(url)) if url.as_ref() == "https://example.test/img/tile.png"
        ));
    }

    #[test]
    fn loaded_import_media_rules_survive_environment_changes() {
        let root_text: Arc<str> =
            "@import 'responsive.css' layer(responsive) screen and (min-width: 600px);".into();
        let child_text: Arc<str> = "div { color: red }".into();
        let mut root_imports = imports(&root_text).unwrap();
        let child = StylesheetSource {
            url: "https://example.test/css/responsive.css".into(),
            text: child_text,
            imports: Vec::new(),
        };
        let root = StylesheetSource {
            url: "https://example.test/css/main.css".into(),
            text: root_text,
            imports: alloc::vec![LoadedImport {
                rule: root_imports.remove(0),
                source: Some(Box::new(child)),
            }],
        };

        let parsed = parse_graph(
            &root,
            MediaEnvironment {
                width: 400.0,
                ..MediaEnvironment::default()
            },
        )
        .unwrap();
        assert_eq!(parsed.rules.len(), 1);
        assert_eq!(
            parsed.rules[0].media.as_ref(),
            &[Arc::<str>::from("screen and (min-width: 600px)")]
        );

        let mut index = StyleIndex::new(parsed.rules);
        index.environment.width = 400.0;
        let narrow = compute(&element(""), None, &index).unwrap();
        index.environment.width = 800.0;
        let wide = compute(&element(""), None, &index).unwrap();
        assert_ne!(narrow.color, wide.color);
        assert_eq!(
            wide.color,
            crate::paint::Rgba {
                r: 255,
                g: 0,
                b: 0,
                a: 255,
            }
        );
    }

    #[test]
    fn stylesheet_graph_stops_cycles_and_enforces_aggregate_budget() {
        let root_url: Arc<str> = "https://example.test/css/root.css".into();
        let child_url: Arc<str> = "https://example.test/css/child.css".into();
        let root_text: Arc<str> = "@import 'child.css' layer(framework);".into();
        let child_text: Arc<str> = "@import 'root.css' layer(return); .child { color: red }".into();
        let root_edge = imports(&root_text).unwrap().remove(0);
        let child_edge = imports(&child_text).unwrap().remove(0);
        let cycle = StylesheetSource {
            url: root_url.clone(),
            text: root_text.clone(),
            imports: Vec::new(),
        };
        let child = StylesheetSource {
            url: child_url,
            text: child_text,
            imports: alloc::vec![LoadedImport {
                rule: child_edge.clone(),
                source: Some(Box::new(cycle)),
            }],
        };
        let root = StylesheetSource {
            url: root_url,
            text: root_text,
            imports: alloc::vec![LoadedImport {
                rule: root_edge.clone(),
                source: Some(Box::new(child)),
            }],
        };
        let parsed = parse_graph(&root, MediaEnvironment::default()).unwrap();
        assert_eq!(parsed.rules.len(), 1);
        assert_eq!(parsed.layers.as_ref(), &["framework", "framework.return"]);

        let large_root_text: Arc<str> = "@import 'large.css';".into();
        let large_rule = imports(&large_root_text).unwrap().remove(0);
        let large = StylesheetSource {
            url: "https://example.test/css/large.css".into(),
            text: " ".repeat(super::MAX_CSS_BYTES).into(),
            imports: Vec::new(),
        };
        let large_root = StylesheetSource {
            url: "https://example.test/css/root.css".into(),
            text: large_root_text,
            imports: alloc::vec![LoadedImport {
                rule: large_rule,
                source: Some(Box::new(large)),
            }],
        };
        assert_eq!(
            parse_graph(&large_root, MediaEnvironment::default())
                .unwrap_err()
                .message,
            "CSS import graph too large"
        );
    }

    #[test]
    fn font_face_layer_order_matches_style_layers_without_suppressing_faces() {
        let mut groups = [
            parse_stylesheet(
                "@layer base, override; @layer override { @font-face { font-family: Shared; src: url('override.woff'); font-weight: 700; unicode-range: U+0042 } }",
            )
            .unwrap(),
            parse_stylesheet(
                "@layer base { @font-face { font-family: Shared; src: url('base.woff'); unicode-range: U+0041 } }",
            )
            .unwrap(),
            parse_stylesheet(
                "@font-face { font-family: Unlayered; src: url('plain.woff') }",
            )
            .unwrap(),
        ];
        let faces = canonicalize_font_faces(&mut groups);
        assert_eq!(faces.len(), 3);
        assert_eq!(faces[0].family.as_ref(), "Shared");
        assert_eq!(faces[0].unicode_range.as_deref(), Some("U+41"));
        assert_eq!(faces[0].layer_path.unwrap()[0], 1);
        assert_eq!(faces[1].family.as_ref(), "Shared");
        assert_eq!(faces[1].weight, 700);
        assert_eq!(faces[1].unicode_range.as_deref(), Some("U+42"));
        assert_eq!(faces[1].layer_path.unwrap()[0], 2);
        assert_eq!(faces[2].family.as_ref(), "Unlayered");
        assert_eq!(faces[2].layer_path, None);
        assert_eq!(
            faces
                .iter()
                .map(|face| face.source_order)
                .collect::<Vec<_>>(),
            [1, 0, 2]
        );
    }

    #[test]
    fn font_face_descriptors_share_validation_and_cssom_serialization() {
        let descriptors = FontFaceDescriptors::parse(
            "Lato",
            &[
                ("weight", "bold"),
                ("width", "ultra-expanded"),
                ("style", "oblique 40deg"),
                ("featureSettings", "\"smcp\" off"),
                ("variationSettings", "\"wght\" 500"),
                ("display", "fallback"),
                ("sizeAdjust", "200%"),
                ("ascentOverride", "50%"),
                ("lineGapOverride", "10%"),
                ("unicodeRange", "U+4??"),
            ],
        )
        .unwrap();
        assert_eq!(descriptors.get("family").as_deref(), Some("Lato"));
        assert_eq!(descriptors.get("font-weight").as_deref(), Some("bold"));
        assert_eq!(
            descriptors.get("stretch").as_deref(),
            Some("ultra-expanded")
        );
        assert_eq!(
            descriptors.get("font-style").as_deref(),
            Some("oblique 40deg")
        );
        assert_eq!(
            descriptors.get("featureSettings").as_deref(),
            Some("\"smcp\" 0")
        );
        assert_eq!(
            descriptors.get("variationSettings").as_deref(),
            Some("\"wght\" 500")
        );
        assert_eq!(descriptors.get("display").as_deref(), Some("fallback"));
        assert_eq!(descriptors.get("sizeAdjust").as_deref(), Some("200%"));
        assert_eq!(descriptors.get("ascentOverride").as_deref(), Some("50%"));
        assert_eq!(descriptors.get("lineGapOverride").as_deref(), Some("10%"));
        assert_eq!(
            descriptors.get("unicodeRange").as_deref(),
            Some("U+400-4FF")
        );

        let mut canonical = FontFaceDescriptors::default();
        canonical.set("featureSettings", "\"liga\" 1").unwrap();
        canonical.set("descentOverride", "30%").unwrap();
        assert_eq!(
            canonical.get("featureSettings").as_deref(),
            Some("\"liga\"")
        );
        assert_eq!(canonical.get("descentOverride").as_deref(), Some("30%"));

        let invalid_family = FontFaceDescriptors::parse("a 1", &[]).unwrap();
        assert_eq!(invalid_family.get("family").as_deref(), Some("\"a 1\""));
        let rule = invalid_family.to_rule(None);
        assert!(rule.sources.is_empty());
        assert!(rule.identity.is_none());
        assert!(FontFaceDescriptors::parse("Lato", &[("weight", "1000 300")]).is_err());
        assert!(
            FontFaceDescriptors::parse("Lato", &[("variationSettings", "\"wght\" NaN")]).is_err()
        );
    }

    #[test]
    fn font_face_source_records_retain_rule_address_and_match_ranges() {
        let stylesheet = concat!(
            "@media screen {",
            "@font-face { font-family: Shared; src: url(font.woff); ",
            "font-weight: 300 700; font-stretch: 75% 125%; }",
            "}"
        );
        let parsed = parse_stylesheet(stylesheet).unwrap();
        let face = &parsed.font_faces[0];
        assert_eq!(face.rule_start, stylesheet.find("@font-face").unwrap());
        assert_eq!(face.source_revision, stylesheet_source_revision(stylesheet));
        assert_eq!(face.weight_range, [300, 700]);
        assert_eq!(face.stretch_range, [75.0, 125.0]);
        assert_eq!(face.get("weight").as_deref(), Some("300 700"));
        assert_eq!(face.media.as_ref(), &[Arc::<str>::from("screen")]);

        let faces = parse_font_faces(
            "@font-face { font-family: Shared; src: url(font.woff); \
             font-weight: 300 700; font-stretch: 75% 125%; }",
        )
        .unwrap();
        let spec = parse_font_shorthand("500 16px/1 Shared").unwrap();
        assert_eq!(matching_font_faces(&spec, &faces, "A"), [0]);
    }

    #[test]
    fn font_shorthand_accepts_an_empty_quoted_family_for_font_loading_queries() {
        let spec = parse_font_shorthand("1px \"\"").unwrap();
        assert_eq!(spec.families.as_deref().unwrap().len(), 1);
        assert_eq!(spec.families.as_deref().unwrap()[0].as_ref(), "");
        assert!(parse_font_family_list("empty").is_some());
        assert!(parse_font_family_list("\"\"").is_some());
    }

    #[test]
    fn font_size_adjust_cascades_and_resets_with_the_font_shorthand() {
        assert_eq!(
            font_size_adjust("CAP-HEIGHT FROM-FONT"),
            Some(Some(FontSizeAdjust {
                metric: FontMetric::CapHeight,
                value: FontSizeAdjustValue::FromFont,
            }))
        );
        assert_eq!(
            font_size_adjust("0.5"),
            Some(Some(FontSizeAdjust {
                metric: FontMetric::ExHeight,
                value: FontSizeAdjustValue::Number(0.5),
            }))
        );
        assert_eq!(
            font_size_adjust("cap-height calc(0.5 + 1)"),
            Some(Some(FontSizeAdjust {
                metric: FontMetric::CapHeight,
                value: FontSizeAdjustValue::Number(1.5),
            }))
        );
        assert_eq!(
            font_size_adjust("calc(-0.5)"),
            Some(Some(FontSizeAdjust {
                metric: FontMetric::ExHeight,
                value: FontSizeAdjustValue::Number(0.0),
            }))
        );
        assert_eq!(font_size_adjust("none"), Some(None));
        for invalid in ["-0.1", "NaN", "ex-height 0.5 extra", "not-a-metric 0.5"] {
            assert_eq!(font_size_adjust(invalid), None, "{invalid}");
        }

        let index = StyleIndex::new(Vec::new());
        let parent = compute(
            &element("font-size:20px;font-size-adjust:ch-width 0.55;line-height:1"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(parent.font_size, 20.0);
        assert_eq!(parent.line_height, LineHeight::Number(1.0));
        assert_eq!(
            parent.font_spec().size_adjust,
            Some(FontSizeAdjust {
                metric: FontMetric::ChWidth,
                value: FontSizeAdjustValue::Number(0.55),
            })
        );
        let inherited = compute(&element(""), Some(&parent), &index).unwrap();
        assert_eq!(
            inherited.font_spec().size_adjust,
            parent.font_spec().size_adjust
        );

        let reset = compute(
            &element("font:ITALIC BOLDER CONDENSED 16px serif"),
            Some(&parent),
            &index,
        )
        .unwrap();
        assert_eq!(reset.font.weight, 700);
        assert_eq!(reset.font.stretch, 75.0);
        assert_eq!(reset.font.style, FontStyle::Italic);
        assert_eq!(reset.font_spec().size_adjust, None);
        assert_eq!(
            parse_font_shorthand("lighter 16px serif").unwrap().weight,
            100
        );
        assert!(parse_font_shorthand("16px/var(--line-height) serif").is_none());
    }

    #[test]
    fn font_face_matching_uses_descriptor_rank_before_unicode_ranges() {
        fn source(faces: &[FontFaceRule], index: usize) -> &str {
            match &faces[index].sources[0] {
                FontFaceSource::Url(source) => source,
                FontFaceSource::Local(_) => "local",
            }
        }

        let mut groups = [parse_stylesheet(
            "@layer base, override; \
             @layer base { @font-face { font-family: Shared; src: url(base.woff); unicode-range: U+0041-0042 } } \
             @layer override { @font-face { font-family: Shared; src: url(override.woff); unicode-range: U+0042 } } \
             @font-face { font-family: Shared; src: url(bold.woff); font-weight: 700; unicode-range: U+0042 }",
        )
        .unwrap()];
        let faces = canonicalize_font_faces(&mut groups);
        let spec = parse_font_shorthand("normal 16px Shared").unwrap();

        let a = matching_font_faces(&spec, &faces, "A");
        assert_eq!(a.len(), 1);
        assert_eq!(source(&faces, a[0]), "base.woff");

        let b = matching_font_faces(&spec, &faces, "B");
        assert_eq!(
            b.iter()
                .map(|&index| source(&faces, index))
                .collect::<Vec<_>>(),
            ["override.woff", "base.woff"]
        );
        assert!(!b.iter().any(|&index| source(&faces, index) == "bold.woff"));
        assert!(matching_font_faces(&spec, &faces, "").is_empty());
    }

    #[test]
    fn rendering_font_faces_preserve_pending_primary_and_observed_coverage() {
        let mut groups = [parse_stylesheet(
            "@font-face { font-family: Primary; src: url(primary.woff); unicode-range: U+0041 } \
             @font-face { font-family: Secondary; src: url(secondary.woff); unicode-range: U+0041, U+1F600 } \
             @font-face { font-family: Unused; src: url(unused.woff) }",
        )
        .unwrap()];
        let faces = canonicalize_font_faces(&mut groups);
        let spec = parse_font_shorthand("16px Primary, Secondary, Unused").unwrap();
        let mut observed = Vec::new();
        let selected = rendering_font_faces(&spec, &faces, "AA😀", |face, piece| {
            observed.push((face.family.to_string(), piece.to_owned()));
            true // A pending primary is not a reason to demand its fallback.
        });
        assert_eq!(selected, [0, 1]);
        assert_eq!(
            observed,
            [
                ("Primary".into(), "A".into()),
                ("Primary".into(), "A".into()),
                ("Secondary".into(), "😀".into()),
            ]
        );

        let selected = rendering_font_faces(&spec, &faces, "A", |face, _| {
            face.family.as_ref() != "Primary" // Failed source or missing glyph.
        });
        assert_eq!(selected, [1]);
        assert!(rendering_font_faces(&spec, &faces, "", |_, _| {
            panic!("empty text must not query sources")
        })
        .is_empty());
        assert!(rendering_font_faces(&spec, &faces, "A", |_, _| false).is_empty());
    }

    #[test]
    fn inline_background_urls_use_document_base_url() {
        let index = StyleIndex::new_with_document_base_url(
            Vec::new(),
            Some("https://example.test/pages/current.html".into()),
        );
        let style = compute(
            &element("background-image:url('img/tile.png')"),
            None,
            &index,
        )
        .unwrap();
        assert!(matches!(
            style.background_images.as_deref().and_then(|images| images.first()),
            Some(BackgroundImage::Url(url)) if url.as_ref() == "https://example.test/pages/img/tile.png"
        ));
    }

    #[test]
    fn stylesheet_statement_rules_preserve_following_cascade_and_fonts() {
        let source = r#"
          @charset "utf-8";
          @namespace "urn:semi;brace{namespace";
          @unknown url("ignored;{resource.css");
          @LAYER low, high;
          @layer low { div { color: red } }
          @layer high { div { color: blue } }
          @font-face { font-family: "StatementFont"; src: url("font;{name.ttf") }
          unfinished prelude
        "#;
        let index = StyleIndex::new(parse(source).unwrap());
        assert_eq!(
            compute(&element(""), None, &index).unwrap().color,
            color("blue").unwrap()
        );
        let faces = parse_font_faces(source).unwrap();
        assert_eq!(faces.len(), 1);
        assert_eq!(&*faces[0].family, "StatementFont");
        assert_eq!(
            faces[0].sources.as_ref(),
            &[FontFaceSource::Url("font;{name.ttf".into())]
        );
    }

    #[test]
    fn stylesheet_eof_recovery_keeps_complete_values_and_font_descriptors() {
        for source in [
            "div { color: blue",
            "div { color: blue; width: calc(12px +",
            "div { color: blue /* unfinished comment",
            "<!-- div { color: blue } --> /* unfinished comment",
            "} div { color: blue } unfinished prelude",
            "div { --tone:blue; --tone:); color:var(--tone) }",
            "div { color: blue; content: \"trailing escape\\",
        ] {
            let index = StyleIndex::new(parse(source).unwrap());
            assert_eq!(
                compute(&element(""), None, &index).unwrap().color,
                color("blue").unwrap(),
                "{source}"
            );
        }
        let faces = parse_font_faces(
            "@media screen { @font-face { font-family: EofFont; src: url(eof.ttf); /* EOF",
        )
        .unwrap();
        assert_eq!(faces.len(), 1);
        assert_eq!(&*faces[0].family, "EofFont");
        assert_eq!(faces[0].media[0].as_ref(), "screen");
        // Strict single-value CSSOM validation keeps its own rejection contract.
        assert!(set_declaration("", "width", "calc(", false).is_err());
    }

    #[test]
    fn stylesheet_recovery_keeps_resource_limits() {
        let excessive_faces = "@font-face{font-family:A;src:url(a.ttf)}".repeat(65);
        assert_eq!(
            parse_font_faces(&excessive_faces).unwrap_err().message,
            "too many font faces"
        );
        let nested = alloc::format!(
            "{}div{{color:blue}}{}",
            "@media screen{".repeat(33),
            "}".repeat(33)
        );
        assert_eq!(
            parse(&nested).unwrap_err().message,
            "CSS rule nesting limit"
        );
    }

    #[test]
    fn font_stretch_cascades_inherits_and_resets_with_shorthand() {
        let index = StyleIndex::new(Vec::new());
        let parent = compute(&element("font-stretch:semi-condensed"), None, &index).unwrap();
        assert_eq!(parent.font.stretch, 87.5);
        assert_eq!(
            compute(&element("font-stretch:CONDENSED"), None, &index)
                .unwrap()
                .font
                .stretch,
            75.0
        );
        let child = compute(
            &element("font-stretch:0%;font-stretch:-10%"),
            Some(&parent),
            &index,
        )
        .unwrap();
        assert_eq!(child.font.stretch, 0.0);
        let child = compute(&element("font-stretch:123.5%"), Some(&parent), &index).unwrap();
        assert_eq!(child.font.stretch, 123.5);
        let reset = compute(&element("font:italic 16px serif"), Some(&parent), &index).unwrap();
        assert_eq!(reset.font.stretch, 100.0);
        let expanded = compute(&element("font:expanded bold 16px serif"), None, &index).unwrap();
        assert_eq!(expanded.font.stretch, 125.0);
        let faces = parse_font_faces(r#"@MEDIA screen { @FONT-FACE { font-family:"A" /* family */;src:url("a/*literal*/.woff");font-weight:700/*weight*/;font-stretch:CONDENSED;ascent-override:80%;descent-override:20%;ascent-override:-10%; } }"#).unwrap();
        assert_eq!(faces.len(), 1);
        assert_eq!(&*faces[0].family, "A");
        assert_eq!(faces[0].weight, 700);
        assert_eq!(faces[0].stretch, 75.0);
        assert_eq!(faces[0].ascent_override, Some(0.8));
        assert_eq!(faces[0].descent_override, Some(0.2));
        assert_eq!(
            faces[0].sources.as_ref(),
            &[FontFaceSource::Url("a/*literal*/.woff".into())]
        );
        assert_eq!(&*faces[0].media[0], "screen");
    }

    #[test]
    fn font_unicode_ranges_validate_and_normalize_css_grammar() {
        assert_eq!(
            super::parse_unicode_ranges("U+4??, u+400-500, U+A5")
                .unwrap()
                .as_ref(),
            &[(0xa5, 0xa5), (0x400, 0x500)]
        );
        assert_eq!(
            super::parse_unicode_ranges("U+???").unwrap().as_ref(),
            &[(0, 0xfff)]
        );
        for invalid in [
            "",
            "U+??????",
            "U+1?????",
            "U+10FFFF-110000",
            "U+500-400",
            "U+1?2",
            "U+10FFFF,",
            "U+0 1",
        ] {
            assert!(super::parse_unicode_ranges(invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn font_face_sources_keep_css_strings_order_and_media() {
        let faces = super::parse_font_faces(
            r#"
          @media screen and (min-width: 1px) {
            @font-face { font-family: "Ahem"; font-weight: bold; font-style: italic;
              src: local("Missing, Face"), url("../fonts/Ah\65 m.ttf") format("truetype");
              unicode-range: U+0-7F;
            }
          }
          @font-face { font-family: serif; src: url(ignore.ttf); }
          @font-face { font-family: "serif"; src: url(quoted.ttf); }
          @font-face { font-family: MissingSource; }
        "#,
        )
        .unwrap();
        assert_eq!(faces.len(), 2);
        assert_eq!(&*faces[0].family, "Ahem");
        assert_eq!(faces[0].weight, 700);
        assert_eq!(faces[0].style, super::FontStyle::Italic);
        assert_eq!(
            faces[0].sources.as_ref(),
            &[
                super::FontFaceSource::Local("Missing, Face".into()),
                super::FontFaceSource::Url("../fonts/Ahem.ttf".into()),
            ]
        );
        assert_eq!(faces[0].unicode_range.as_deref(), Some("U+0-7F"));
        assert_eq!(faces[0].media[0].as_ref(), "screen and (min-width: 1px)");
        assert_eq!(&*faces[1].family, "serif");
    }
    use super::*;

    #[test]
    fn supports_conditions_evaluate_declarations_logic_and_selectors() {
        assert!(supports_condition("color: red"));
        assert!(!supports_condition("color: no-such-color"));
        assert!(supports_condition(
            "(display: grid) and (width: calc(50% - 2px))"
        ));
        assert!(!supports_condition("(display: grid) and (unknown: value)"));
        assert!(supports_condition("not (unknown: value)"));
        assert!(supports_condition("selector(div > .item)"));
        assert!(!supports_condition("selector(div, .item)"));
        assert!(!supports_condition("(future(condition))"));
        assert!(supports_condition("not (future(condition))"));
        assert!(!supports_condition(
            "(display: grid) and (color: red) or (width: 1px)"
        ));
    }

    #[test]
    fn supports_rules_enter_the_cascade_only_when_their_condition_is_true() {
        let rules = parse(
            "@supports (display: grid) and (selector(div)) { div { color: red } }\
             @supports (display: invalid-value) { div { color: blue } }\
             @supports (future(condition)) { div { color: green } }",
        )
        .unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].declarations.len(), 1);
        assert!(matches!(
            &rules[0].declarations[0].value,
            Value::Color(value) if (value.r, value.g, value.b, value.a) == (255, 0, 0, 255)
        ));
    }

    #[test]
    fn overflow_axes_cascade_normalize_and_clip_without_scroll_containment() {
        let index = StyleIndex::new(Vec::new());
        let style = compute(&element("overflow:clip visible"), None, &index).unwrap();
        assert_eq!(
            (style.overflow_x, style.overflow_y),
            (Overflow::Clip, Overflow::Visible)
        );
        assert!(!style.overflow_x.scroll_container());
        let style = compute(&element("overflow:hidden;overflow-x:clip"), None, &index).unwrap();
        assert_eq!(
            (style.overflow_x, style.overflow_y),
            (Overflow::Hidden, Overflow::Hidden)
        );
        let style = compute(&element("overflow-y:auto"), None, &index).unwrap();
        assert_eq!(
            (style.overflow_x, style.overflow_y),
            (Overflow::Auto, Overflow::Auto)
        );
        let style = compute(
            &element("overflow:clip visible!important;overflow-x:hidden"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            (style.overflow_x, style.overflow_y),
            (Overflow::Clip, Overflow::Visible)
        );
        let style = compute(
            &element("overflow:hidden;overflow:clip bogus"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            (style.overflow_x, style.overflow_y),
            (Overflow::Hidden, Overflow::Hidden)
        );
    }

    #[test]
    fn background_attachments_cycle_and_shorthand_reset() {
        let index = StyleIndex::new(Vec::new());
        let style = compute(
            &element("background-attachment:fixed,local,scroll"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            style.background_attachment.as_ref().unwrap().as_ref(),
            &[
                BackgroundAttachment::Fixed,
                BackgroundAttachment::Local,
                BackgroundAttachment::Scroll
            ]
        );
        let style = compute(
            &element("background:fixed url(a.png),local linear-gradient(red,blue)"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            style.background_attachment.as_ref().unwrap().as_ref(),
            &[BackgroundAttachment::Fixed, BackgroundAttachment::Local]
        );
        let style = compute(
            &element("background-attachment:fixed;background:red"),
            None,
            &index,
        )
        .unwrap();
        assert!(style.background_attachment.is_none());
        let style = compute(
            &element("background-attachment:fixed;background:local fixed blue"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            style.background_attachment.as_ref().unwrap()[0],
            BackgroundAttachment::Fixed
        );
    }

    #[test]
    fn image_sets_select_resolution_and_filter_types_before_duplicates() {
        let mut index = StyleIndex::new(Vec::new());
        let kind = element(
            "background-image:image-set('low.png' 1x, url(high.png) 192dpi, 'huge.png' 3dppx)",
        );
        for (resolution, expected, density) in [
            (1.0, "low.png", 1.0),
            (1.5, "high.png", 2.0),
            (2.5, "huge.png", 3.0),
            (4.0, "huge.png", 3.0),
        ] {
            index.environment.resolution = resolution;
            let style = compute(&kind, None, &index).unwrap();
            assert!(
                matches!(&style.background_images.as_ref().unwrap()[0], BackgroundImage::UrlResolution { url, density: actual } if &**url == expected && *actual == density)
            );
        }
        let style = compute(&element("background:image-set('a.avif' type('image/avif'), 'first.png' type('image/png'), 'ignored.png') no-repeat"), None, &index).unwrap();
        assert!(
            matches!(&style.background_images.as_ref().unwrap()[0], BackgroundImage::UrlResolution { url, density: 1.0 } if &**url == "first.png")
        );
        let style = compute(
            &element("background-image:image-set('a.avif' type('image/avif'))"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            style.background_images.as_ref().unwrap()[0],
            BackgroundImage::None
        );
        for value in [
            "image-set()",
            "image-set('a.png' -1x)",
            "image-set('a.png' 1x 2x)",
            "image-set(image-set('a.png'))",
        ] {
            assert!(!valid_background_images(value), "{value}");
        }
        assert!(valid_background_images(
            "image-set(linear-gradient(red,blue) 1x, 'a.png' 2x)"
        ));
    }

    #[test]
    fn gap_shorthand_and_axis_longhands_cascade_independently() {
        let index = StyleIndex::new(Vec::new());
        let style = compute(&element("gap:10px 20px;row-gap:3px"), None, &index).unwrap();
        assert_eq!(style.gap, 3.0);
        assert_eq!(style.column_gap, Some(20.0));
        let style = compute(&element("gap:7px;column-gap:normal"), None, &index).unwrap();
        assert_eq!(style.gap, 7.0);
        assert_eq!(style.column_gap, None);
        let style = compute(&element("gap:9px;gap:-1px 2px"), None, &index).unwrap();
        assert_eq!((style.gap, style.column_gap), (9.0, Some(9.0)));
        let style = compute(&element("gap:9px;gap:initial"), None, &index).unwrap();
        assert_eq!((style.gap, style.column_gap), (0.0, None));
        let style = compute(
            &element("font-size:20px;row-gap:calc(1em + 10%);column-gap:25%"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            (style.gap, style.column_gap, style.column_gap_fraction),
            (20.0, Some(0.0), 0.25)
        );
        assert!((style.row_gap_fraction - 0.1).abs() < 1e-6);
    }

    #[test]
    fn flex_flow_expands_both_axes_and_resets_omitted_components() {
        let index = StyleIndex::new(Vec::new());
        let style = compute(&element("flex-flow:wrap-reverse column"), None, &index).unwrap();
        assert_eq!(style.flex_direction, FlexDirection::Column);
        assert!(style.flex_wrap && style.flex_wrap_reverse);
        let style = compute(
            &element("flex-flow:column wrap-reverse;flex-flow:row"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(style.flex_direction, FlexDirection::Row);
        assert!(!style.flex_wrap && !style.flex_wrap_reverse);
        let style = compute(
            &element("flex-flow:column wrap;flex-flow:row column"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(style.flex_direction, FlexDirection::Column);
        assert!(style.flex_wrap);
    }

    #[test]
    fn sharing_cache_matches_uncached_cascade_for_contextual_selectors_and_parent_styles() {
        let document = crate::html::parse("<main class=one><span class=item></span><span class=item></span><span class=item></span></main><main class=two lang=fr><span class=item></span><span class=item></span></main>", 64).unwrap();
        for stylesheet in [
            "main.one{--tone:red;font:italic 600 12px sans-serif}main.two{--tone:blue;font:400 20px serif}.item{color:var(--tone);font-weight:bolder}",
            ":root{color:purple}.one>.item:nth-child(2){color:red}.item+.item{background:lime}.two .item:lang(fr){font-size:18px}.item:last-child{color:blue}",
        ] {
            let index = StyleIndex::new(parse(stylesheet).unwrap());
            let mut cache = StyleCache::default();
            let mut pending = alloc::vec![(document.root(), None)];
            while let Some((node, inherited)) = pending.pop() {
                let style = if matches!(document.kind(node).unwrap(), NodeKind::Element { .. }) {
                    let expected =
                        compute_node(&document, node, inherited.as_ref(), &index).unwrap();
                    let actual = compute_node_cached(
                        &document,
                        node,
                        inherited.as_ref(),
                        &index,
                        &mut cache,
                    )
                    .unwrap();
                    assert_eq!(actual, expected, "{stylesheet}: {node:?}");
                    // Repeat reads exercise the per-node cache, while the two
                    // sibling groups exercise parent-style and element sharing.
                    assert_eq!(
                        compute_node_cached(
                            &document,
                            node,
                            inherited.as_ref(),
                            &index,
                            &mut cache
                        )
                        .unwrap(),
                        expected
                    );
                    Some(actual)
                } else {
                    inherited
                };
                let mut child = document.first_child(node).unwrap();
                let mut children = Vec::new();
                while let Some(id) = child {
                    children.push((id, style.clone()));
                    child = document.next_sibling(id).unwrap();
                }
                pending.extend(children.into_iter().rev());
            }
        }
    }

    #[test]
    fn named_colors_include_wpt_palette_and_aliases() {
        assert_eq!(
            color("lime"),
            Some(Rgba {
                r: 0,
                g: 255,
                b: 0,
                a: 255
            })
        );
        assert_eq!(
            color("LightPink"),
            Some(Rgba {
                r: 255,
                g: 182,
                b: 193,
                a: 255
            })
        );
        assert_eq!(
            color("rebeccapurple"),
            Some(Rgba {
                r: 102,
                g: 51,
                b: 153,
                a: 255
            })
        );
        assert_eq!(color("darkgrey"), color("darkgray"));
        assert_eq!(color("fuchsia"), color("magenta"));
        assert_eq!(
            color("transparent"),
            Some(Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 0
            })
        );
    }

    #[test]
    fn html_hidden_rendering_defaults_and_important_input_rule() {
        let index = StyleIndex::new(Vec::new());
        let hidden = NodeKind::Element {
            name: "div".into(),
            namespace: crate::Namespace::Html,
            attributes: alloc::vec![("hidden".into(), "".into())],
        };
        assert_eq!(
            compute(&hidden, None, &index).unwrap().display,
            Display::None
        );
        let mut shown = hidden.clone();
        if let NodeKind::Element { attributes, .. } = &mut shown {
            attributes.push(("style".into(), "display:block".into()));
        }
        assert_eq!(
            compute(&shown, None, &index).unwrap().display,
            Display::Block
        );
        let input = NodeKind::Element {
            name: "input".into(),
            namespace: crate::Namespace::Html,
            attributes: alloc::vec![
                ("type".into(), "HiDdEn".into()),
                ("style".into(), "display:block!important".into())
            ],
        };
        assert_eq!(
            compute(&input, None, &index).unwrap().display,
            Display::None
        );
    }

    #[test]
    fn computed_fonts_inherit_expand_and_resolve_relative_weights_from_parent() {
        let index = StyleIndex::new(Vec::new());
        let parent = compute(
            &element("font:italic 600 20px/1.25 'Face One', Face\\20 Two, sans-serif"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(parent.font_size, 20.0);
        assert_eq!(parent.line_height, LineHeight::Number(1.25));
        assert_eq!(parent.font.style, FontStyle::Italic);
        assert_eq!(parent.font.weight, 600);
        assert_eq!(
            parent
                .font
                .families
                .as_ref()
                .unwrap()
                .iter()
                .map(|name| &**name)
                .collect::<Vec<_>>(),
            ["Face One", "Face Two", "sans-serif"]
        );
        let inherited = compute(&element(""), Some(&parent), &index).unwrap();
        assert_eq!(inherited.font, parent.font);
        let child = compute(
            &element("font-weight:100;font-weight:bolder;font-size:0.5em"),
            Some(&parent),
            &index,
        )
        .unwrap();
        assert_eq!(child.font.weight, 900);
        assert_eq!(child.font_size, 10.0);
        let reset = compute(&element("font:12px / 18px serif"), Some(&parent), &index).unwrap();
        assert_eq!(reset.font.weight, 400);
        assert_eq!(reset.font.style, FontStyle::Normal);
        assert_eq!(reset.line_height, LineHeight::Pixels(18.0));
        let initial = compute(&element("all:initial"), Some(&parent), &index).unwrap();
        assert_eq!(initial.font, FontSpec::default());
        for invalid in [
            "font-family:Face,",
            "font-family:-3",
            "font-family:INHERIT, sans-serif",
            "font:italic italic 12px serif",
            "font:12px",
        ] {
            assert!(declarations(invalid, 0).unwrap().is_empty(), "{invalid}");
        }
    }
    use crate::Namespace;

    fn element(style: &str) -> NodeKind {
        NodeKind::Element {
            namespace: Namespace::Html,
            name: "div".into(),
            attributes: alloc::vec![("style".into(), style.into())],
        }
    }

    #[test]
    fn text_align_match_parent_computes_against_parent_direction() {
        let index = StyleIndex::new(Vec::new());
        let parent = compute(&element("direction:rtl;text-align:start"), None, &index).unwrap();
        let child = compute(
            &element("direction:ltr;text-align:match-parent"),
            Some(&parent),
            &index,
        )
        .unwrap();
        assert_eq!(child.text_align, TextAlign::Right);
        let root = compute(&element("text-align:match-parent"), None, &index).unwrap();
        assert_eq!(root.text_align, TextAlign::Start);
        let all = compute(&element("text-align:justify-all"), None, &index).unwrap();
        assert!(all.text_align.justifies());
        assert_eq!(all.text_align, TextAlign::JustifyAll);
    }

    #[test]
    fn flex_intrinsic_basis_keywords_survive_cascade_and_shorthand() {
        let index = StyleIndex::new(Vec::new());
        for (property, expected) in [
            ("flex-basis:fit-content", IntrinsicSizing::FitContent),
            ("flex-basis:min-content", IntrinsicSizing::MinContent),
            ("flex-basis:max-content", IntrinsicSizing::MaxContent),
            ("flex:min-content", IntrinsicSizing::MinContent),
        ] {
            let style = compute(&element(property), None, &index).unwrap();
            assert_eq!(style.flex_basis_intrinsic, Some(expected), "{property}");
            assert_eq!(style.flex_basis, None);
            assert!(!style.flex_basis_content);
        }
        let parent = compute(&element("flex-basis:max-content"), None, &index).unwrap();
        let inherited = compute(&element("flex-basis:inherit"), Some(&parent), &index).unwrap();
        assert_eq!(
            inherited.flex_basis_intrinsic,
            Some(IntrinsicSizing::MaxContent)
        );
        let reset = compute(&element("flex-basis:unset"), Some(&parent), &index).unwrap();
        assert_eq!(reset.flex_basis_intrinsic, None);
        let math = compute(
            &element(
                "flex:calc(10 + (sign(20cqw - 10px) * 5)) calc(10 + (sign(20cqw - 10px) * 5)) 1px",
            ),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(math.flex_grow, 15.0);
        assert_eq!(math.flex_shrink, 15.0);
        assert_eq!(math.flex_basis, Some(1.0));
    }

    #[test]
    fn sibling_color_functions_recompute_from_element_siblings() {
        let rules =
            parse("div { color: alpha(from green / calc(sibling-index() / sibling-count())) }")
                .unwrap();
        let index = StyleIndex::new(rules);
        assert!(!index.siblings_share);
        let mut document = Document::new(16);
        let parent = document
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: "section".into(),
                attributes: Vec::new(),
            })
            .unwrap();
        let first = document.create(element("")).unwrap();
        let second = document.create(element("")).unwrap();
        let third = document.create(element("")).unwrap();
        document.append(document.root(), parent).unwrap();
        document.append(parent, first).unwrap();
        document.append(parent, second).unwrap();
        let mut cache = StyleCache::default();
        let first_style = compute_node_cached(&document, first, None, &index, &mut cache).unwrap();
        let second_style =
            compute_node_cached(&document, second, None, &index, &mut cache).unwrap();
        assert_eq!(first_style.color.a, 128);
        assert_eq!(second_style.color.a, 255);
        assert!(second_style.sibling_position_dependent);
        document.append(parent, third).unwrap();
        let updated = compute_node_cached(&document, second, None, &index, &mut cache).unwrap();
        assert_eq!(updated.color.a, 170);
    }

    #[test]
    fn relative_wide_color_and_sign_math_parse() {
        assert!(color_value(
            1,
            "color(from alpha(from currentcolor / 0.5) display-p3 r g b / alpha)"
        )
        .is_some());
        assert_eq!(
            css_scalar("calc(10 + (sign(20cqw - 10px) * 5))", false),
            Some(15.0)
        );
        assert_eq!(
            contextual_length(
                "calc(10px + (sign(20cqw - 10px) * 5px))",
                Some(static_length_context()),
            ),
            Some(15.0)
        );
    }

    #[test]
    fn border_radius_shorthand_expands_elliptical_values_and_resolves_font_units() {
        let index = StyleIndex::new(Vec::new());
        let style = compute(
            &element("font-size:20px;border-radius:1em 2em 3em / 1em 2em"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            style.corner_radii(200.0, 100.0),
            [[20.0, 20.0], [40.0, 40.0], [60.0, 20.0], [40.0, 40.0]]
        );
        assert!(style.border_corner_radii.is_some());

        let uniform = compute(&element("border-radius:5px"), None, &index).unwrap();
        assert_eq!(uniform.border_radius, 5.0);
        assert!(uniform.border_corner_radii.is_none());
        assert_eq!(uniform.corner_radii(30.0, 20.0), [[5.0, 5.0]; 4]);
    }

    #[test]
    fn border_radius_percentages_use_own_box_and_common_overlap_reduction() {
        let index = StyleIndex::new(Vec::new());
        let style = compute(&element("border-radius:80% / 60%"), None, &index).unwrap();
        assert_eq!(style.corner_radii(100.0, 40.0), [[50.0, 15.0]; 4]);

        let asymmetric =
            compute(&element("border-radius:50% 25% / 25% 50%"), None, &index).unwrap();
        assert_eq!(
            asymmetric.corner_radii(200.0, 80.0),
            [[100.0, 20.0], [50.0, 40.0], [100.0, 20.0], [50.0, 40.0]]
        );
    }

    #[test]
    fn border_radius_invalid_shorthands_are_rejected_as_a_whole() {
        let index = StyleIndex::new(Vec::new());
        for invalid in [
            "auto",
            "1px 2px 3px 4px 5px",
            "-1px",
            "1px -2px",
            "1em /",
            "1px / 2px / 3px",
            "4 / 5",
        ] {
            assert!(
                declarations(&alloc::format!("border-radius:{invalid}"), 0)
                    .unwrap()
                    .is_empty(),
                "{invalid}"
            );
        }
        let style = compute(
            &element("border-radius:7px;border-radius:1px -2px"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(style.border_radius, 7.0);
        assert!(style.border_corner_radii.is_none());
    }

    #[test]
    fn border_radius_corner_longhands_cascade_copy_reset_and_resolve_context() {
        let index = StyleIndex::new(Vec::new());
        let parent = compute(
            &element("border-radius:1px 2px 3px 4px / 5px 6px 7px 8px"),
            None,
            &index,
        )
        .unwrap();
        let inherited = compute(
            &element(
                "border-top-left-radius:inherit;border-bottom-left-radius:inherit;\
                 border-top-right-radius:unset;border-bottom-right-radius:initial",
            ),
            Some(&parent),
            &index,
        )
        .unwrap();
        assert_eq!(
            inherited.corner_radii(100.0, 100.0),
            [[1.0, 5.0], [0.0, 0.0], [0.0, 0.0], [4.0, 8.0]]
        );

        let root = compute(&element("font-size:16px"), None, &index).unwrap();
        let resolved = compute(
            &element(
                "--horizontal:calc(2em + 10%);--vertical:3rem;font-size:20px;\
                 border-top-left-radius:var(--horizontal) var(--vertical);\
                 border-bottom-right-radius:calc(10px + 5%)",
            ),
            Some(&root),
            &index,
        )
        .unwrap();
        assert_eq!(
            resolved.corner_radii(200.0, 100.0),
            [[60.0, 48.0], [0.0, 0.0], [20.0, 15.0], [0.0, 0.0]]
        );
    }

    #[test]
    fn border_radius_corner_longhands_follow_shorthand_priority_and_all_resets() {
        let index = StyleIndex::new(Vec::new());
        let later_shorthand = compute(
            &element("border-top-left-radius:9px;border-radius:1px 2px 3px 4px"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            later_shorthand.corner_radii(100.0, 100.0),
            [[1.0, 1.0], [2.0, 2.0], [3.0, 3.0], [4.0, 4.0]]
        );
        let later_longhand = compute(
            &element("border-radius:1px 2px 3px 4px;border-top-left-radius:9px 10px"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            later_longhand.corner_radii(100.0, 100.0),
            [[9.0, 10.0], [2.0, 2.0], [3.0, 3.0], [4.0, 4.0]]
        );

        let important_shorthand = compute(
            &element("border-radius:5px!important;border-top-left-radius:9px"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            important_shorthand.corner_radii(100.0, 100.0),
            [[5.0, 5.0]; 4]
        );
        let important_longhand = compute(
            &element("border-radius:5px;border-top-left-radius:9px!important"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            important_longhand.corner_radii(100.0, 100.0),
            [[9.0, 9.0], [5.0, 5.0], [5.0, 5.0], [5.0, 5.0]]
        );

        let reset = compute(&element("border-radius:7px;all:initial"), None, &index).unwrap();
        assert_eq!(reset.corner_radii(100.0, 100.0), [[0.0, 0.0]; 4]);
        let reapply = compute(
            &element("border-radius:7px;all:initial;border-bottom-right-radius:3px"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            reapply.corner_radii(100.0, 100.0),
            [[0.0, 0.0], [0.0, 0.0], [3.0, 3.0], [0.0, 0.0]]
        );
    }

    #[test]
    fn border_radius_corner_longhands_reject_invalid_values_atomically() {
        let index = StyleIndex::new(Vec::new());
        for invalid in ["auto", "1px 2px 3px", "1px / 2px", "-1px", "1px -2px"] {
            assert!(
                declarations(&alloc::format!("border-top-left-radius:{invalid}"), 0)
                    .unwrap()
                    .is_empty(),
                "{invalid}"
            );
        }
        let style = compute(
            &element(
                "border-top-left-radius:5px;border-top-left-radius:1px 2px 3px;\
                 border-bottom-left-radius:8px;border-bottom-left-radius:1px / 2px",
            ),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            style.corner_radii(100.0, 100.0),
            [[5.0, 5.0], [0.0, 0.0], [0.0, 0.0], [8.0, 8.0]]
        );
    }

    #[test]
    fn computed_border_radius_serialization_keeps_percentages_and_ellipses() {
        let index = StyleIndex::new(Vec::new());
        let style = compute(
            &element("font-size:16px;border-radius:calc(10px + 25%) 1em 25% 25px / calc(20px + 25%) 1em 25% 25px"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            style.border_radius_css_value(),
            "calc(25% + 10px) 16px 25% 25px / calc(25% + 20px) 16px 25% 25px"
        );
        let compact = compute(
            &element("border-radius:1px 1px 1px 2% / 1px 2% 1px 2%"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(compact.border_radius_css_value(), "1px 1px 1px 2% / 1px 2%");
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
                        ex: 8.0,
                        ch: 8.0,
                        viewport: MediaEnvironment::default(),
                        percent: None
                    }
                )
                .is_none(),
                "{invalid}"
            );
        }
        assert!(compute(
            &element("transform:none;transform-origin:50% 50%"),
            None,
            &index
        )
        .unwrap()
        .extras
        .is_none());
        assert!(core::mem::size_of::<Style>() <= 200);
    }

    #[test]
    fn variable_substitution_preserves_quoted_function_text() {
        let index = StyleIndex::new(Vec::new());
        for text in [
            r#"content: "var(--number)""#,
            r#"--literal: "var(--number)"; content: var(--literal)"#,
        ] {
            let style = compute(&element(text), None, &index).unwrap();
            assert_eq!(
                style.generated_content(),
                GeneratedContent::Items(Arc::from([GeneratedContentItem::String(Arc::from(
                    "var(--number)"
                )),]))
            );
        }
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
                .custom_properties()
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
    fn color_scheme_parses_inherits_and_custom_property_escapes_resolve() {
        for (value, expected) in [
            ("normal", Some("normal")),
            ("dark light", Some("dark light")),
            ("only light", Some("light only")),
            ("only light dark", Some("light dark only")),
            ("dark AcmeDisplay only", Some("dark AcmeDisplay only")),
            ("light dark light", Some("light dark light")),
        ] {
            assert_eq!(
                serialize_color_scheme_declaration(value).as_deref(),
                expected,
                "{value}"
            );
            assert!(supports_declaration("color-scheme", value), "{value}");
        }
        for value in [
            "",
            "only",
            "normal light",
            "light normal",
            "only only light",
            "light only dark",
            "light default",
            "light initial",
            "light / dark",
        ] {
            assert!(!supports_declaration("color-scheme", value), "{value}");
        }

        let index = StyleIndex::new(Vec::new());
        let parent = compute(&element("color-scheme: dark only"), None, &index).unwrap();
        let inherited = compute(&element(""), Some(&parent), &index).unwrap();
        assert_eq!(inherited.color_scheme.as_deref(), Some("dark only"));
        assert_eq!(
            compute(&element("color-scheme: initial"), Some(&parent), &index)
                .unwrap()
                .color_scheme
                .as_deref(),
            None
        );

        let escaped = compute(
            &element(r"--a\,fail: pass; --unparsed: var(--a\,fail)"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            escaped
                .custom_properties()
                .iter()
                .find(|(name, _)| name == "--a,fail")
                .and_then(|(_, value)| value.as_deref()),
            Some("pass")
        );
        assert_eq!(
            escaped
                .custom_properties()
                .iter()
                .find(|(name, _)| name == "--unparsed")
                .and_then(|(_, value)| value.as_deref()),
            Some("pass")
        );
        let escaped_value = compute(
            &element(r"--a\,fail:5px;width:var(--a\,fail)"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(escaped_value.width, Some(5.0));
    }

    #[test]
    fn transition_time_lists_validate_cascade_and_resolve_variables() {
        for (property, value, expected) in [
            ("transition-duration", "1s, 250ms, 0s", "1s, 250ms, 0s"),
            ("transition-delay", "-25ms, 0s, 1.5s", "-25ms, 0s, 1.5s"),
            ("transition-duration", "1S, 250MS", "1s, 250ms"),
            ("transition-duration", "calc(2 * 3s)", "6s"),
            ("transition-delay", "calc(-2 * 25ms)", "-0.05s"),
            ("transition-duration", "calc(2 * 3s + 250ms)", "6.25s"),
            ("transition-duration", "calc(2 * -3s)", "0s"),
        ] {
            assert!(supports_declaration(property, value), "{property}: {value}");
            assert_eq!(
                serialize_transition_time_declaration(property, value).as_deref(),
                Some(expected),
                "{property}: {value}"
            );
        }
        for (property, value) in [
            ("transition-duration", "-1ms"),
            ("transition-duration", "0"),
            ("transition-duration", "1px"),
            ("transition-duration", "1e999s"),
            ("transition-duration", "1s,,2s"),
            ("transition-delay", "1s, , 2s"),
            ("transition-delay", "1s, 2px"),
            ("transition-duration", "calc(1s * 2s)"),
            ("transition-delay", "2s + 3s"),
        ] {
            assert!(
                !supports_declaration(property, value),
                "accepted {property}: {value}"
            );
        }
        let sixty_four = alloc::vec!["0s"; 64].join(", ");
        let sixty_five = alloc::vec!["0s"; 65].join(", ");
        assert!(supports_declaration("transition-duration", &sixty_four));
        assert!(!supports_declaration("transition-duration", &sixty_five));

        let index = StyleIndex::new(Vec::new());
        let parent = compute(
            &element("transition-duration:250ms;transition-delay:-50ms"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            serialize_transition_time_values(parent.transition_duration.as_deref()),
            "0.25s"
        );
        assert_eq!(
            serialize_transition_time_values(parent.transition_delay.as_deref()),
            "-0.05s"
        );

        let child = compute(&element(""), Some(&parent), &index).unwrap();
        assert_eq!(
            serialize_transition_time_values(child.transition_duration.as_deref()),
            "0s"
        );
        assert_eq!(
            serialize_transition_time_values(child.transition_delay.as_deref()),
            "0s"
        );
        let inherited = compute(
            &element("transition-duration:inherit;transition-delay:inherit"),
            Some(&parent),
            &index,
        )
        .unwrap();
        assert_eq!(
            serialize_transition_time_values(inherited.transition_duration.as_deref()),
            "0.25s"
        );
        assert_eq!(
            serialize_transition_time_values(inherited.transition_delay.as_deref()),
            "-0.05s"
        );

        let substituted = compute(
            &element(
                "--duration:350ms;--delay:-25ms;transition-duration:var(--duration);transition-delay:var(--delay)",
            ),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            serialize_transition_time_values(substituted.transition_duration.as_deref()),
            "0.35s"
        );
        assert_eq!(
            serialize_transition_time_values(substituted.transition_delay.as_deref()),
            "-0.025s"
        );
        let invalid_substitution = compute(
            &element("--duration:-1ms;transition-duration:var(--duration)"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            serialize_transition_time_values(invalid_substitution.transition_duration.as_deref()),
            "0s"
        );

        let calculated = compute(
            &element("transition-duration:calc(2 * 3s);transition-delay:calc(-2 * 25ms)"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            serialize_transition_time_values(calculated.transition_duration.as_deref()),
            "6s"
        );
        assert_eq!(
            serialize_transition_time_values(calculated.transition_delay.as_deref()),
            "-0.05s"
        );
        let clamped_duration =
            compute(&element("transition-duration:calc(2 * -3s)"), None, &index).unwrap();
        assert_eq!(
            serialize_transition_time_values(clamped_duration.transition_duration.as_deref()),
            "0s"
        );
    }

    #[test]
    fn optional_style_state_is_lazy_and_flex_controls_cascade() {
        let index = StyleIndex::new(Vec::new());
        assert!(compute(&element("color:red;width:5px"), None, &index)
            .unwrap()
            .extras
            .is_none());
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
    fn display_contents_is_preserved_as_a_computed_value() {
        let parsed = parse_stylesheet("div { display: contents }").unwrap();
        let style = compute(&element(""), None, &StyleIndex::new(parsed.rules)).unwrap();
        assert_eq!(style.display, Display::Contents);
    }

    #[test]
    fn flex_auto_minimum_and_content_basis_survive_css_wide_cascade() {
        let index = StyleIndex::new(Vec::new());
        let explicit = compute(
            &element("min-width:0;min-height:0;flex-basis:content"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!((explicit.min_width, explicit.min_height), (0.0, 0.0));
        assert!(!explicit.min_width_auto && !explicit.min_height_auto);
        assert!(explicit.flex_basis_content);

        let inherited = compute(
            &element("min-width:inherit;min-height:inherit;flex-basis:inherit"),
            Some(&explicit),
            &index,
        )
        .unwrap();
        assert!(!inherited.min_width_auto && !inherited.min_height_auto);
        assert!(inherited.flex_basis_content);

        let reset = compute(
            &element("min-width:unset;min-height:unset;flex-basis:unset"),
            Some(&explicit),
            &index,
        )
        .unwrap();
        assert!(reset.min_width_auto && reset.min_height_auto);
        assert!(!reset.flex_basis_content && reset.flex_basis.is_none());

        let logical = compute(&element("min-inline-size:0"), None, &index).unwrap();
        assert_eq!(logical.min_width, 0.0);
        assert!(!logical.min_width_auto);
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
        assert!(compute(&element("color:red;width:5px"), None, &index)
            .unwrap()
            .extras
            .is_none());
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
    fn nth_child_of_selector_lists_filter_forward_and_reverse_sibling_counts() {
        let document = crate::html::parse(
            r#"<main><ol id="parent"><li id="first" class="pick"><em></em></li><li id="unselected"></li><li id="middle" class="pick"><i></i></li><b id="selected" class="pick"></b><li id="last" class="pick"></li></ol></main>"#,
            64,
        )
        .unwrap();
        let matches = |id: &str, selector: &str| {
            let node = crate::selector::query_selector(
                &document,
                document.root(),
                &alloc::format!("#{id}"),
            )
            .unwrap()
            .expect("fixture node");
            crate::selector::matches(&document, node, selector).unwrap()
        };

        assert!(matches("middle", "#middle:nth-child(2 of .pick)"));
        assert!(matches("middle", "#middle:nth-child(2n of .pick)"));
        assert!(matches("selected", "#selected:nth-child(3 of .pick)"));
        assert!(matches("middle", "#middle:nth-last-child(3 of .pick)"));
        assert!(matches("selected", "#selected:nth-last-child(2n of .pick)"));
        assert!(matches("last", "#last:nth-last-child(1 of .pick)"));
        assert!(!matches("unselected", "#unselected:nth-child(2 of .pick)"));
        assert!(matches(
            "selected",
            "#selected:nth-child(3 of #parent > .pick)"
        ));
        assert!(matches(
            "middle",
            "#middle:nth-child(2 of li.pick:has(i), li.pick:has(em))"
        ));
    }

    #[test]
    fn nth_child_of_wildcard_namespace_counts_elements_from_all_namespaces() {
        let document = crate::html::parse(
            r#"<main><i id="first"></i><svg id="second"></svg><b id="third"></b></main>"#,
            32,
        )
        .unwrap();
        let second = crate::selector::query_selector(&document, document.root(), "#second")
            .unwrap()
            .expect("the SVG child is present");

        assert!(crate::selector::matches(&document, second, "*:nth-child(2 of *|*)").unwrap());
        assert!(parse_selector("*:nth-child(even of *|*)", 0).is_ok());
    }

    #[test]
    fn form_state_pseudos_use_the_shared_selector_matcher() {
        let document = crate::html::parse(
            "<form><fieldset disabled><legend><input id='legend'></legend><input id='blocked'></fieldset><input id='check' type='checkbox' checked required><input id='text' type='text' checked><div id='plain' disabled></div></form>",
            64,
        )
        .unwrap();
        let id = |name: &str| {
            crate::selector::get_element_by_id(&document, document.root(), name)
                .unwrap()
                .unwrap()
        };
        let matches = |name: &str, source: &str| {
            parse_selector(source, 0)
                .unwrap()
                .matches_node(&document, id(name))
        };

        assert!(matches("legend", "input:enabled"));
        assert!(matches("blocked", "input:disabled"));
        assert!(matches("check", "input:checked:required"));
        assert!(!matches("text", "input:checked"));
        assert!(matches("text", "input:not(:checked)"));
        assert!(!matches("plain", ":disabled"));
        assert!(!matches("plain", ":enabled"));
        assert!(parse_selector(":checked:required", 0).is_ok());
        assert!(parse_selector(":checked(true)", 0).is_err());
        assert_eq!(
            parse_selector(":checked:checked", 0).unwrap().specificity,
            (0, 2, 0)
        );
    }

    #[test]
    fn interaction_pseudos_parse_and_use_the_shared_selector_matcher() {
        let mut document =
            crate::html::parse("<main><button id='target'></button></main>", 16).unwrap();
        let target = crate::selector::get_element_by_id(&document, document.root(), "target")
            .unwrap()
            .unwrap();
        let matches = |document: &Document, selector: &str| {
            parse_selector(selector, 0)
                .unwrap()
                .matches_node(document, target)
        };

        assert!(!matches(&document, "button:focus"));
        document.set_interaction_state(crate::interaction::InteractionState {
            focused: Some(target),
            focus_visible: Some(target),
            hover: Some(target),
            active: [Some(target), None],
        });
        for pseudo in ["focus", "focus-within", "focus-visible", "hover", "active"] {
            assert!(matches(&document, &alloc::format!("button:{pseudo}")));
        }
        assert!(matches(&document, "button:focus:hover:active"));
        assert!(!matches(&document, "button:not(:hover)"));
        assert_eq!(parse_selector(":focus", 0).unwrap().specificity, (0, 1, 0));
        assert_eq!(parse_selector(":HOVER", 0).unwrap().specificity, (0, 1, 0));
        assert!(parse_selector(":focus(true)", 0).is_err());
        assert!(parse_selector("::hover", 0).is_err());
    }

    #[test]
    fn interaction_pseudos_also_match_exposed_shadow_parts() {
        let mut document = crate::html::parse("<x-host></x-host>", 8).unwrap();
        let host = crate::selector::query_selector(&document, document.root(), "x-host")
            .unwrap()
            .unwrap();
        let shadow = document
            .attach_shadow(host, crate::shadow::ShadowMode::Open)
            .unwrap();
        let part = document
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: "button".into(),
                attributes: alloc::vec![("part".into(), "control".into())],
            })
            .unwrap();
        document.append(shadow, part).unwrap();

        let mut document_rule = parse_stylesheet("x-host::part(control):hover { color: red }")
            .unwrap()
            .rules
            .into_iter()
            .next()
            .unwrap();
        assert!(!document_rule.matches_at(&document, part));
        document.set_interaction_state(crate::interaction::InteractionState {
            hover: Some(part),
            ..Default::default()
        });
        assert!(document_rule.matches_at(&document, part));

        document_rule.scope = Some(shadow);
        assert!(document_rule.matches_at(&document, part));
        document.set_interaction_state(crate::interaction::InteractionState::default());
        assert!(!document_rule.matches_at(&document, part));
    }

    #[test]
    fn direction_pseudo_uses_shared_html_directionality() {
        let document = crate::html::parse(
            concat!(
                "<body dir='rtl'>",
                "<div id='inherited'></div>",
                "<div id='explicit' dir='ltr'></div>",
                "<div id='auto' dir='auto'> 123 مرحبا</div>",
                "<div id='empty-auto' dir='auto'> 123 !</div>",
                "<bdi id='bdi'>123 עברית</bdi>",
                "<input id='input' dir='auto' value='עברית'>",
                "<input id='tel' type='tel'>",
                "</body>"
            ),
            64,
        )
        .unwrap();
        let matches = |id: &str, selector: &str| {
            let node = crate::selector::get_element_by_id(&document, document.root(), id)
                .unwrap()
                .unwrap();
            parse_selector(selector, 0)
                .unwrap()
                .matches_node(&document, node)
        };

        assert!(matches("inherited", ":dir(RTL)"));
        assert!(matches("explicit", ":dir(ltr)"));
        assert!(matches("auto", "div:dir(rtl)"));
        assert!(matches("empty-auto", ":dir(ltr)"));
        assert!(matches("bdi", ":dir(rtl)"));
        assert!(matches("input", ":dir(rtl)"));
        assert!(matches("tel", ":dir(ltr)"));
        assert!(matches("inherited", ":not(:dir(ltr))"));
        assert!(!matches("explicit", ":dir(rtl)"));
        assert!(!matches("inherited", ":dir(ltr):dir(rtl)"));
        assert_eq!(
            parse_selector(":dir(rtl)", 0).unwrap().specificity,
            (0, 1, 0)
        );
        assert!(parse_selector(":dir()", 0).is_err());
        assert!(parse_selector(":dir(auto)", 0).is_err());
        assert!(parse_selector(":dir(ltr, rtl)", 0).is_err());
        assert!(parse_selector("::dir(rtl)", 0).is_err());
    }

    #[test]
    fn html_direction_ua_starting_value_precedes_author_cascade() {
        let document = crate::html::parse(
            concat!(
                "<body dir='rtl'>",
                "<div id='ua'></div>",
                "<div id='author' dir='rtl'><span id='descendant'></span></div>",
                "<bdi id='bdi'>مرحبا</bdi>",
                "<input id='telephone' type='TEL'>",
                "</body>"
            ),
            64,
        )
        .unwrap();
        let index =
            StyleIndex::new(parse("#author { direction: ltr } #bdi { direction: ltr }").unwrap());
        let node = |name: &str| {
            crate::selector::get_element_by_id(&document, document.root(), name)
                .unwrap()
                .unwrap()
        };
        let html = crate::selector::query_selector(&document, document.root(), "html")
            .unwrap()
            .unwrap();
        let body = crate::selector::query_selector(&document, document.root(), "body")
            .unwrap()
            .unwrap();
        let html_style = compute_node(&document, html, None, &index).unwrap();
        let body_style = compute_node(&document, body, Some(&html_style), &index).unwrap();
        let ua_style = compute_node(&document, node("ua"), Some(&body_style), &index).unwrap();
        let author_style =
            compute_node(&document, node("author"), Some(&body_style), &index).unwrap();
        let descendant_style =
            compute_node(&document, node("descendant"), Some(&author_style), &index).unwrap();
        let bdi_style = compute_node(&document, node("bdi"), Some(&body_style), &index).unwrap();
        let telephone_style =
            compute_node(&document, node("telephone"), Some(&body_style), &index).unwrap();

        assert_eq!(body_style.direction(), Direction::Rtl);
        assert_eq!(ua_style.direction(), Direction::Rtl);
        assert_eq!(author_style.direction(), Direction::Ltr);
        assert_eq!(descendant_style.direction(), Direction::Ltr);
        assert_eq!(bdi_style.direction(), Direction::Ltr);
        assert_eq!(telephone_style.direction(), Direction::Ltr);
        let author = node("author");
        assert!(parse_selector("#author:dir(rtl)", 0)
            .unwrap()
            .matches_node(&document, author));
    }

    #[test]
    fn nth_child_of_lists_preserve_scope_strictness_specificity_and_limits() {
        let document = crate::html::parse(
            r#"<main id="scope"><i id="first" class="item" data-note="a,)"></i><i id="second" class="item"></i></main>"#,
            32,
        )
        .unwrap();
        let scope = crate::selector::query_selector(&document, document.root(), "#scope")
            .unwrap()
            .unwrap();
        let matches = crate::selector::query_selector_all(
            &document,
            scope,
            ":scope > .item:nth-child(2 of :scope > .item)",
        )
        .unwrap();
        assert_eq!(matches.len(), 1, "the filter must receive the query scope");
        assert_eq!(
            parse_selector(":nth-child(2n of .item, #important)", 0)
                .unwrap()
                .specificity,
            (1, 1, 0)
        );
        assert_eq!(
            parse_selector(":nth-last-child(odd of :where(#ignored), .item)", 0)
                .unwrap()
                .specificity,
            (0, 2, 0)
        );
        assert!(parse_selector(r#":nth-child(1 of [data-note="a,)"])"#, 0).is_ok());
        assert!(parse_selector(":nth-child(1 of/* comment */.item)", 0).is_ok());
        assert!(parse_selector(r#":nth-child(3 of/* my comment */target)"#, 0).is_ok());
        assert!(parse_selector(r#":nth-child(3 of/* comment ) , */target)"#, 0).is_ok());
        assert!(parse_selector(r#":nth-child(2n/* formula comment */ + 1 of .item)"#, 0).is_ok());
        assert!(parse_selector(":nth-child(1 of :is(:unsupported, .item))", 0).is_ok());
        assert!(parse_selector(":nth-child(1 of :is(::part(icon), .item))", 0).is_ok());

        for invalid in [
            ":nth-child(1 of)",
            ":nth-child(1 of, .item)",
            ":nth-child(1 of .item, :unsupported)",
            ":nth-child(1 of ::before)",
            ":nth-last-child(1 of ::part(icon))",
            ":nth-of-type(1 of .item)",
            ":nth-last-of-type(even of *)",
            ":nth-child(n + 1of .item)",
            r#":nth-child(1 of "text")"#,
        ] {
            assert!(parse_selector(invalid, 0).is_err(), "{invalid}");
        }
        let too_deep = alloc::format!(
            ":nth-child(1 of {}.item{})",
            ":not(".repeat(MAX_SELECTOR_NESTING + 1),
            ")".repeat(MAX_SELECTOR_NESTING + 1)
        );
        assert!(parse_selector(&too_deep, 0).is_err());
    }

    #[test]
    fn nth_child_of_is_live_for_sibling_mutations_and_nested_has_rules() {
        let mut document = crate::html::parse(
            "<ol><li id='first' class='pick'></li><li id='second' class='pick'></li><li id='third' class='pick'></li></ol>",
            32,
        )
        .unwrap();
        let find = |document: &Document, id: &str| {
            crate::selector::query_selector(document, document.root(), &alloc::format!("#{id}"))
                .unwrap()
                .unwrap()
        };
        let first = find(&document, "first");
        let second = find(&document, "second");
        let third = find(&document, "third");
        let selector = "li:nth-child(2 of .pick)";
        assert!(crate::selector::matches(&document, second, selector).unwrap());
        assert!(!crate::selector::matches(&document, third, selector).unwrap());

        let index = StyleIndex::new(
            parse("li { color:blue } li:nth-child(2 of .pick) { color:red }").unwrap(),
        );
        assert!(!index.siblings_share);
        assert_eq!(
            compute_node(&document, second, None, &index).unwrap().color,
            color("red").unwrap()
        );

        document.set_attribute(first, "class", "").unwrap();
        assert!(!crate::selector::matches(&document, second, selector).unwrap());
        assert!(crate::selector::matches(&document, third, selector).unwrap());
        assert_eq!(
            compute_node(&document, second, None, &index).unwrap().color,
            color("blue").unwrap()
        );

        assert!(parse_selector(":has(> li:nth-child(1 of :has(span)))", 0).is_err());
        assert!(parse_selector(":has(> li:nth-child(1 of :not(:has(span))))", 0).is_err());
        let forgiving_nested =
            parse_selector(":has(> li:nth-child(1 of :is(:has(span), .pick)))", 0).unwrap();
        assert_eq!(forgiving_nested.specificity, (0, 2, 1));
    }

    #[test]
    fn logical_pseudos_match_complex_lists_with_selectors4_specificity() {
        let document = crate::html::parse(
            r#"<main><section class="card"><p id="target" class="target a)" data-x="),"></p></section><section class="skip"><p id="skipped"></p></section></main>"#,
            64,
        )
        .unwrap();
        let find = |selector: &str| {
            crate::selector::query_selector_all(&document, document.root(), selector)
                .unwrap()
                .into_iter()
                .map(|node| match document.kind(node).unwrap() {
                    NodeKind::Element { attributes, .. } => attributes
                        .iter()
                        .find(|(name, _)| name == "id")
                        .map(|(_, value)| value.clone())
                        .unwrap_or_default(),
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(
            find(":is(main > section.card > p.target, #fallback)"),
            ["target"]
        );
        assert_eq!(find("p:not(main .skip *)"), ["target"]);
        assert_eq!(find(":is([data-x=\"),\"], .fallback)"), ["target"]);
        assert_eq!(find(r":is(.a\), .fallback)"), ["target"]);
        assert_eq!(find("p:is(.target, :not(.skip))"), ["target", "skipped"]);
        assert_eq!(find("p:not(:is(main .skip p, .excluded))"), ["target"]);
        assert_eq!(find(":is(::part(icon), .target)"), ["target"]);
        assert_eq!(find(":is(.target, :unsupported, [broken=])"), ["target"]);
        assert_eq!(find(":where(, .target, :unsupported,)"), ["target"]);
        assert_eq!(find(":IS(.target)"), ["target"]);
        assert!(parse_selector(":not(.target, :unsupported)", 0).is_err());
        assert!(parse_selector(":not(::part(icon))", 0).is_err());
        assert!(parse_selector(":not()", 0).is_err());
        assert!(find(":is(:unsupported)").is_empty());

        assert_eq!(
            parse_selector(":is(.a, #b)", 0).unwrap().specificity,
            (1, 0, 0)
        );
        assert_eq!(
            parse_selector(":not(.a, #b)", 0).unwrap().specificity,
            (1, 0, 0)
        );
        assert_eq!(
            parse_selector(":where(#b, .a)", 0).unwrap().specificity,
            (0, 0, 0)
        );
        assert_eq!(
            parse_selector(":is(.a, #b) span", 0).unwrap().specificity,
            (1, 0, 1)
        );
        assert_eq!(
            parse_selector(":where(*)", 0).unwrap().specificity,
            (0, 0, 0)
        );
        assert_eq!(
            parse_selector(":WHERE(*)", 0).unwrap().specificity,
            (0, 0, 0)
        );
        assert!(parse_selector(":is(:first-child)", 0)
            .unwrap()
            .position_dependent());
        assert!(!StyleIndex::new(parse(":is(:first-child) { color:red }").unwrap()).siblings_share);

        let target = crate::selector::query_selector(&document, document.root(), "#target")
            .unwrap()
            .expect("target element");
        let index = StyleIndex::new(
            parse("#target { color:red } :is(.target, #missing) { color:blue }").unwrap(),
        );
        assert_eq!(
            compute_node(&document, target, None, &index).unwrap().color,
            color("blue").unwrap()
        );
        let index =
            StyleIndex::new(parse("#target { color:red } :where(#target) { color:blue }").unwrap());
        assert_eq!(
            compute_node(&document, target, None, &index).unwrap().color,
            color("red").unwrap()
        );
    }

    #[test]
    fn relational_has_matches_anchored_paths_and_live_mutations() {
        let mut document = crate::html::parse(
            r#"<div class="outside"><main id="anchor"><section class="row"><i id="leaf" data-note="a,)"></i></section></main><ol><li id="before"></li><li id="subject"></li><li id="tail"><b class="needle"></b></li></ol></div>"#,
            64,
        )
        .unwrap();
        let find = |selector: &str| {
            crate::selector::query_selector(&document, document.root(), selector)
                .unwrap()
                .expect("fixture element")
        };
        let anchor = find("#anchor");
        assert!(
            crate::selector::matches(&document, anchor, "main:has(.row .needle, .row > i)")
                .unwrap()
        );
        assert!(crate::selector::matches(&document, anchor, "main:has(> section.row)").unwrap());
        assert!(!crate::selector::matches(&document, anchor, "main:has(> i)").unwrap());
        assert!(crate::selector::matches(
            &document,
            anchor,
            r#"main:has(> section.row > i[data-note="a,)"]:not(:is(.cold, .disabled)))"#
        )
        .unwrap());
        // The leftmost compound must stay inside the relative anchor; an
        // ancestor outside it cannot satisfy the relative selector.
        assert!(!crate::selector::matches(&document, anchor, "main:has(.outside .row i)").unwrap());

        let subject = find("#subject");
        let before = find("#before");
        assert!(crate::selector::matches(&document, subject, ":has(+ li#tail)").unwrap());
        assert!(!crate::selector::matches(&document, subject, ":has(+ b.needle)").unwrap());
        assert!(
            crate::selector::matches(&document, subject, ":has(+ li#tail > b.needle)").unwrap()
        );
        assert!(crate::selector::matches(&document, before, ":has(~ li#tail)").unwrap());
        assert!(crate::selector::matches(&document, before, ":has(~ li#tail > b.needle)").unwrap());
        assert!(!crate::selector::matches(&document, subject, ":has(~ li#before)").unwrap());

        let leaf = find("#leaf");
        document.set_attribute(leaf, "class", "cold").unwrap();
        assert!(!crate::selector::matches(
            &document,
            anchor,
            r#"main:has(> section.row > i[data-note="a,)"]:not(:is(.cold, .disabled)))"#
        )
        .unwrap());
        document.set_attribute(leaf, "class", "leaf").unwrap();
        assert!(crate::selector::matches(
            &document,
            anchor,
            r#"main:has(> section.row > i[data-note="a,)"]:not(:is(.cold, .disabled)))"#
        )
        .unwrap());
    }

    #[test]
    fn has_is_strict_specificity_aware_and_bounded() {
        for invalid in [
            ":has()",
            ":has(.valid, :unsupported)",
            ":has(::part(icon))",
            ":has(:has(.nested))",
            ":has(:not(:has(.nested)))",
        ] {
            assert!(parse_selector(invalid, 0).is_err(), "{invalid}");
        }
        for forgiving in [":has(:is(:has(*)))", ":has(:where(:has(*)))"] {
            assert!(parse_selector(forgiving, 0).is_ok(), "{forgiving}");
        }
        assert_eq!(
            parse_selector(":has(:is(:has(#removed), .needle))", 0)
                .unwrap()
                .specificity,
            (0, 1, 0),
            "discarded forgiving branches must not contribute specificity"
        );
        assert_eq!(
            parse_selector(".subject:has(> #needle, .minor)", 0)
                .unwrap()
                .specificity,
            (1, 1, 0)
        );
        let nested = alloc::format!(
            ":has({}*{})",
            ":not(".repeat(MAX_SELECTOR_NESTING + 1),
            ")".repeat(MAX_SELECTOR_NESTING + 1)
        );
        assert!(parse_selector(&nested, 0).is_err());

        let document = crate::html::parse(
            "<div id='target' class='subject'><i id='needle'></i></div>",
            16,
        )
        .unwrap();
        let target = crate::selector::query_selector(&document, document.root(), "#target")
            .unwrap()
            .unwrap();
        assert!(
            crate::selector::matches(&document, target, ":has(:is(:has(*), #needle))").unwrap()
        );
        assert!(
            crate::selector::matches(&document, target, ":has(:where(:has(*)))")
                .is_ok_and(|matched| !matched)
        );
        let index = StyleIndex::new(
            parse("#target { color:blue } .subject:has(> #needle) { color:red }").unwrap(),
        );
        assert!(!index.siblings_share);
        assert_eq!(
            compute_node(&document, target, None, &index).unwrap().color,
            color("red").unwrap()
        );
    }

    #[test]
    fn stylesheet_scope_defaults_to_document_element() {
        assert_eq!(parse_selector(":scope", 0).unwrap().specificity, (0, 1, 0));
        let document = crate::html::parse("<main><p>text</p></main>", 16).unwrap();
        let root = crate::selector::document_element(&document).unwrap();
        let body = crate::selector::query_selector(&document, document.root(), "body")
            .unwrap()
            .unwrap();
        let index = StyleIndex::new(parse(":scope { display:none }").unwrap());
        assert!(!index.siblings_share);
        assert_eq!(
            compute_node(&document, root, None, &index).unwrap().display,
            Display::None
        );
        assert_eq!(
            compute_node(&document, body, None, &index).unwrap().display,
            Display::Block
        );
    }

    #[test]
    fn logical_selector_recursion_and_compound_counts_are_bounded() {
        let depth = MAX_SELECTOR_NESTING + 1;
        let nested = alloc::format!("{}*{}", ":not(".repeat(depth), ")".repeat(depth));
        assert!(parse_selector(&nested, 0).is_err());
        let forgiving_nested = alloc::format!("{}*{}", ":is(".repeat(depth), ")".repeat(depth));
        assert!(parse_selector(&forgiving_nested, 0).is_err());

        let compounds = alloc::format!("{}target", "a ".repeat(MAX_SELECTOR_COMPOUNDS));
        assert!(parse_selector(&compounds, 0).is_err());
        assert!(parse_selector(&alloc::format!(":is({compounds})"), 0).is_err());
    }

    #[test]
    fn gradient_directions_stops_and_computed_colors_are_bounded() {
        let index = StyleIndex::new(Vec::new());
        let style = compute(&element("background-image:linear-gradient(to right bottom, currentColor 1em 25%, rgba(0, 0, 255, 0.5), red 120%);font-size:10px;color:green"),None,&index).unwrap();
        let BackgroundImage::Gradient(gradient) = &style.background_images.as_ref().unwrap()[0]
        else {
            panic!("expected gradient background image");
        };
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
            .background_images
            .as_ref()
            .unwrap()
            .first()
            .map(|image| match image {
                BackgroundImage::Gradient(gradient) => gradient.kind.clone(),
                _ => panic!("expected gradient background image"),
            })
            .unwrap(),
            GradientKind::Linear {
                angle: 90.0,
                corner: None
            }
        );
        assert!(compute(
            &element("background:linear-gradient(red,blue);background:red"),
            None,
            &index
        )
        .unwrap()
        .background_images
        .is_none());
        assert!(compute(&element("background:red;width:5px"), None, &index)
            .unwrap()
            .extras
            .is_none());
        for raw in [
            "linear-gradient(to left right,red,blue)",
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
        let layers = style.background_images.as_ref().unwrap();
        assert_eq!(layers.len(), 2);
        let [first, second] = layers.as_ref() else {
            panic!("expected two background layers")
        };
        let [BackgroundImage::Gradient(first), BackgroundImage::Gradient(second)] = [first, second]
        else {
            panic!("expected gradient background images");
        };
        assert_eq!(first.stops[0].color, color("green").unwrap());
        assert_eq!(
            second.kind,
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
    fn background_geometry_properties_parse_and_layers_cycle() {
        let index = StyleIndex::new(Vec::new());
        let style = compute(
            &element(
                "background-image:url(a.png),url(b.png);\
                 background-position:left 5px top 10px,25%;\
                 background-size:50% auto,cover;\
                 background-repeat:repeat-x,space round;\
                 background-clip:content-box;\
                 background-origin:border-box,padding-box",
            ),
            None,
            &index,
        )
        .unwrap();
        let images = style.background_images.as_ref().unwrap();
        assert_eq!(images.len(), 2);
        assert!(matches!(
            &images[0],
            BackgroundImage::Url(url) if url.as_ref() == "a.png"
        ));
        assert_eq!(
            style.background_position.as_ref().unwrap().as_ref(),
            &[
                [
                    LengthPercentage {
                        pixels: 5.0,
                        fraction: 0.0
                    },
                    LengthPercentage {
                        pixels: 10.0,
                        fraction: 0.0
                    }
                ],
                [
                    LengthPercentage {
                        pixels: 0.0,
                        fraction: 0.25
                    },
                    LengthPercentage {
                        pixels: 0.0,
                        fraction: 0.5
                    }
                ],
            ]
        );
        let sizes = style.background_size.as_ref().unwrap();
        assert_eq!(
            sizes[0].width,
            Some(LengthPercentage {
                pixels: 0.0,
                fraction: 0.5
            })
        );
        assert_eq!(sizes[0].height, None);
        assert_eq!(sizes[1].kind, BackgroundSizeKind::Cover);
        let repeats = style.background_repeat.as_ref().unwrap();
        assert_eq!(
            repeats[0],
            [BackgroundRepeat::Repeat, BackgroundRepeat::NoRepeat]
        );
        assert_eq!(
            repeats[1],
            [BackgroundRepeat::Space, BackgroundRepeat::Round]
        );
        assert_eq!(
            style.background_clip.as_ref().unwrap().as_ref(),
            &[BackgroundBox::Content]
        );
        assert_eq!(
            style.background_origin.as_ref().unwrap().as_ref(),
            &[BackgroundBox::Border, BackgroundBox::Padding]
        );
        // Per-layer lists are stored raw and cycled against the layer count.
        let style = compute(
            &element("background-image:url(a.png),url(b.png);background-position:5px"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(style.background_position.as_ref().unwrap().len(), 1);
        // Initial-equivalent values stay on the lazy-extras fast path.
        let style = compute(
            &element(
                "background-position:0% 0%;background-size:auto;background-repeat:repeat;\
                 background-clip:border-box;background-origin:padding-box",
            ),
            None,
            &index,
        )
        .unwrap();
        assert!(style.background_position.is_none());
        assert!(style.background_size.is_none());
        assert!(style.background_repeat.is_none());
        assert!(style.background_clip.is_none());
        assert!(style.background_origin.is_none());
    }

    #[test]
    fn background_mask_shorthands_keep_origin_and_clip_independent_and_atomic() {
        let index = StyleIndex::new(Vec::new());
        for (raw, origin, clip) in [
            (
                "background:red padding-box text",
                BackgroundBox::Padding,
                BackgroundBox::Text,
            ),
            (
                "background:red text padding-box border-area",
                BackgroundBox::Padding,
                BackgroundBox::BorderAreaText,
            ),
            (
                "background:red padding-box content-box",
                BackgroundBox::Padding,
                BackgroundBox::Content,
            ),
        ] {
            let style = compute(&element(raw), None, &index).unwrap();
            assert_eq!(style.background.r, 255, "{raw}");
            assert_eq!(
                style
                    .background_origin
                    .as_deref()
                    .map(|v| v[0])
                    .unwrap_or(BackgroundBox::Padding),
                origin,
                "{raw}"
            );
            assert_eq!(
                style
                    .background_clip
                    .as_deref()
                    .map(|v| v[0])
                    .unwrap_or(BackgroundBox::Border),
                clip,
                "{raw}"
            );
        }
        for raw in [
            "background:blue;background:red border-box padding-box content-box",
            "background:blue;background:red cross-fade(red -1%,blue)",
            "background:blue;background:red text text",
        ] {
            let style = compute(&element(raw), None, &index).unwrap();
            assert_eq!(
                style.background,
                Rgba {
                    r: 0,
                    g: 0,
                    b: 255,
                    a: 255
                },
                "{raw}"
            );
        }
        assert!(compute(&element("background-origin:text"), None, &index)
            .unwrap()
            .background_origin
            .is_none());
    }

    #[test]
    fn conic_gradient_parses_layers_and_shorthand() {
        let parse = |raw: &str| {
            gradient_layers(raw, Style::initial().color, None)
                .and_then(|layers| (!layers.is_empty()).then(|| layers[0].clone()))
        };
        let gradient =
            parse("conic-gradient(#ff0000, #00ff00, #0000ff, #ff0000)").expect("plain conic stops");
        assert!(matches!(
            gradient.kind,
            GradientKind::Conic { from: 0.0, .. }
        ));
        assert_eq!(gradient.stops.len(), 4);
        let gradient =
            parse("conic-gradient(from 90deg at 30% 70%, red 0deg, red 90deg, blue 180deg)")
                .expect("from/at conic");
        match gradient.kind {
            GradientKind::Conic { from, center } => {
                assert_eq!(from, 90.0);
                assert!((center[0].fraction - 0.3).abs() < 1e-6);
                assert!((center[1].fraction - 0.7).abs() < 1e-6);
            }
            other => panic!("{other:?}"),
        }
        let stops = gradient.stops.as_ref();
        assert!(matches!(
            stops[1].position,
            Some(GradientPosition::Fraction(v)) if (v - 0.25).abs() < 1e-6
        ));
        let repeating =
            parse("repeating-conic-gradient(from 45deg, red 0deg, blue 90deg)").unwrap();
        assert!(repeating.repeating);
        assert!(matches!(
            repeating.kind,
            GradientKind::Conic { from: 45.0, .. }
        ));
        // The background shorthand accepts conic layers too.
        let background = background_shorthand("conic-gradient(red, blue) no-repeat");
        assert!(background.is_some(), "shorthand conic layer");
        for raw in [
            "conic-gradient()",
            "conic-gradient(from abc at 50%, red, blue)",
            "conic-gradient(red 10px, blue)",
        ] {
            assert!(parse(raw).is_none(), "{raw}");
        }
    }

    #[test]
    fn logical_shorthands_expand_to_physical_sides() {
        let index = StyleIndex::new(Vec::new());
        let style = compute(&element("margin-inline:12px 4px"), None, &index).unwrap();
        assert_eq!(style.margin_sides, [0.0, 4.0, 0.0, 12.0]);
        let style = compute(&element("margin-inline:8px"), None, &index).unwrap();
        assert_eq!(style.margin_sides, [0.0, 8.0, 0.0, 8.0]);
        let style = compute(&element("padding-block:10px 2px"), None, &index).unwrap();
        assert_eq!(style.padding_sides, [10.0, 0.0, 2.0, 0.0]);
        let style = compute(&element("inset-inline:24px 8px"), None, &index).unwrap();
        assert_eq!(style.left, Some(24.0));
        assert_eq!(style.right, Some(8.0));
        let style = compute(&element("inset-block:auto 6px"), None, &index).unwrap();
        assert_eq!(style.top, None);
        assert_eq!(style.bottom, Some(6.0));
        // Invalid components drop the whole declaration.
        let style = compute(&element("padding-inline:1px auto"), None, &index).unwrap();
        assert_eq!(style.padding_sides, [0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn aspect_ratio_parses_into_style() {
        let index = StyleIndex::new(Vec::new());
        let style = compute(&element("aspect-ratio:2/1"), None, &index).unwrap();
        assert_eq!(style.aspect_ratio, Some(2.0));
        let style = compute(&element("aspect-ratio:0.75"), None, &index).unwrap();
        assert_eq!(style.aspect_ratio, Some(0.75));
        let style = compute(&element("aspect-ratio:auto"), None, &index).unwrap();
        assert_eq!(style.aspect_ratio, None);
        for raw in [
            "aspect-ratio:0/1",
            "aspect-ratio:2/-1",
            "aspect-ratio:auto 2/1",
        ] {
            assert_eq!(
                compute(&element(raw), None, &index).unwrap().aspect_ratio,
                None,
                "{raw}"
            );
        }
    }

    #[test]
    fn grid_auto_repeat_parses_into_layout_resolved_auto_tracks() {
        let index = StyleIndex::new(Vec::new());
        let style = compute(
            &element("grid-template-columns:repeat(auto-fit,minmax(30px,1fr))"),
            None,
            &index,
        )
        .unwrap();
        assert!(style.grid_columns.is_none());
        let auto = style.grid_columns_auto.as_ref().unwrap();
        assert!(auto.fit);
        assert_eq!(
            auto.tracks.as_ref(),
            &[GridTrack::MinMax(
                GridBreadth::Pixels(30.0),
                GridBreadth::Fraction(1.0)
            )]
        );
        let style = compute(
            &element("grid-template-rows:repeat(auto-fill,20px)"),
            None,
            &index,
        )
        .unwrap();
        assert!(style.grid_rows.is_none());
        assert_eq!(
            style.grid_rows_auto.as_ref().unwrap().tracks.as_ref(),
            &[GridTrack::Pixels(20.0)]
        );
        let style = compute(
            &element(
                "grid-template-columns:10px [outer] repeat(auto-fill,[inner-start] 20px [inner-end]) 30px [after]",
            ),
            None,
            &index,
        )
        .unwrap();
        let auto = style.grid_columns_auto.as_ref().unwrap();
        assert_eq!(auto.prefix_tracks.as_ref(), &[GridTrack::Pixels(10.0)]);
        assert_eq!(auto.tracks.as_ref(), &[GridTrack::Pixels(20.0)]);
        assert_eq!(auto.suffix_tracks.as_ref(), &[GridTrack::Pixels(30.0)]);
        assert_eq!(auto.prefix_names.len(), 1);
        assert_eq!(auto.prefix_names[0].name.as_ref(), "outer");
        assert_eq!(auto.prefix_names[0].line, 1);
        assert_eq!(auto.repeat_names.len(), 2);
        assert_eq!(auto.repeat_names[0].name.as_ref(), "inner-start");
        assert_eq!(auto.repeat_names[0].line, 0);
        assert_eq!(auto.repeat_names[1].name.as_ref(), "inner-end");
        assert_eq!(auto.repeat_names[1].line, 1);
        assert_eq!(auto.suffix_names.len(), 1);
        assert_eq!(auto.suffix_names[0].name.as_ref(), "after");
        assert_eq!(auto.suffix_names[0].line, 1);

        // Line-name blocks may touch track functions and sizes without
        // whitespace; the grammar boundaries come from the brackets, not
        // from token spacing.
        let compact_components = grid_components(
            "repeat(2,5px)[outer]repeat(auto-fill,[auto-line]20px[auto-end])repeat(2,5px)[last]",
        )
        .unwrap();
        assert_eq!(
            compact_components,
            [
                "repeat(2,5px)",
                "[outer]",
                "repeat(auto-fill,[auto-line]20px[auto-end])",
                "repeat(2,5px)",
                "[last]",
            ]
        );
        let compact = compute(
            &element(
                "grid-template-columns:repeat(2,5px)[outer]repeat(auto-fill,[auto-line]20px[auto-end])repeat(2,5px)[last]",
            ),
            None,
            &index,
        )
        .unwrap();
        let auto = compact.grid_columns_auto.as_ref().unwrap();
        assert_eq!(auto.prefix_tracks.len(), 2);
        assert_eq!(auto.suffix_tracks.len(), 2);
        assert_eq!(auto.prefix_names[0].name.as_ref(), "outer");
        assert_eq!(auto.repeat_names[0].name.as_ref(), "auto-line");
        assert_eq!(auto.repeat_names[1].name.as_ref(), "auto-end");
        assert_eq!(auto.suffix_names[0].name.as_ref(), "last");

        // Intrinsic and flexible track sizes are accepted within the
        // auto-repeat itself, while nested auto-repeat remains invalid.
        for raw in [
            "grid-template-columns:repeat(auto-fit,auto 100px auto)",
            "grid-template-columns:repeat(auto-fill,[a] 20px [b])",
            "grid-template-columns:repeat(auto-fill,20px 30px)",
        ] {
            let style = compute(&element(raw), None, &index).unwrap();
            assert!(
                style.grid_columns_auto.is_some(),
                "expected accepted auto-repeat: {raw}"
            );
        }
        for raw in [
            "grid-template-columns:auto repeat(auto-fill,10px)",
            "grid-template-columns:repeat(auto-fill,repeat(auto-fill,20px))",
            "grid-template-columns:repeat(auto-fill,Repeat(auto-fill,20px))",
        ] {
            let style = compute(&element(raw), None, &index).unwrap();
            assert!(
                style.grid_columns.is_none() && style.grid_columns_auto.is_none(),
                "{raw}"
            );
        }
    }

    #[test]
    fn grid_value_grammars_accept_named_lines_and_reject_malformed_forms() {
        for value in [
            "auto / auto",
            "auto / auto / auto",
            "auto / auto / auto / auto",
            "-zπ",
            "+90 -a-",
            "span 2 i",
            "i 2 SpAn",
            "2 j / span 3 k",
            "first span 1 / last",
            "3 first / 2 span last",
            "2 span first / last",
            "5 nav / last span 7",
            "\\31st / \\31 st",
        ] {
            assert!(
                supports_declaration("grid-area", value),
                "expected valid grid-area {value:?}"
            );
        }
        for (property, value) in [
            ("grid-row", "auto / inherit"),
            ("grid-column", "auto / unset"),
            ("grid-column", "-1 / -2 span"),
            ("grid-row", "-3 span / -4"),
            ("grid-row", "last -2 span / 1 nav"),
            ("grid-area", "auto / auto / auto / inherit"),
            ("grid-area", "1 / 2 / 3 / 4 / 5"),
            ("grid-auto-columns", "repeat(2, 10px)"),
            ("grid-template-columns", "repeat(2, repeat(2, 10px))"),
            (
                "grid-template-columns",
                "repeat(auto-fill, repeat(2, 10px))",
            ),
        ] {
            assert!(
                !supports_declaration(property, value),
                "expected invalid {property} {value:?}"
            );
        }
        for value in [
            "[one] repeat(2, minmax(10px, auto)) [two] 30px [three] repeat(auto-fill, 10px) 40px [four five] repeat(2, minmax(10px, auto)) [six]",
            "repeat(auto-fit, fit-content(20%) 100px fit-content(20%))",
        ] {
            assert!(
                supports_declaration("grid-template-columns", value),
                "expected valid grid-template-columns {value:?}"
            );
        }
        assert!(supports_declaration(
            "grid-template",
            "\"a\" calc(100% - 10px) / calc(10px)"
        ));
        for value in [
            "\"a\" [] [] \"b\"",
            "\"a\" [a] [b] \"b\"",
            "\"a\" [a] [a] \"b\" 10px",
            "\"a\" [a] [] \"b\" 10px",
            "\"a\" \"a\" [a] [a] \"b\" / auto",
        ] {
            assert!(
                supports_declaration("grid-template", value),
                "expected valid grid-template {value:?}"
            );
            assert!(
                supports_declaration("grid", value),
                "expected valid grid {value:?}"
            );
        }
        for value in [
            "[] [] \"a\"",
            "\"a\" 10px [a] [a]",
            "\"a\" [a] [a]",
            "[a] \"a\" [a] [a]",
            "\"a\" \"a\" [a] [a]",
        ] {
            assert!(
                !supports_declaration("grid-template", value),
                "expected invalid grid-template {value:?}"
            );
            assert!(
                !supports_declaration("grid", value),
                "expected invalid grid {value:?}"
            );
        }
    }

    #[test]
    fn background_shorthand_resets_components_and_parses_layers() {
        let index = StyleIndex::new(Vec::new());
        let style = compute(
            &element(
                "background-position:9px 9px;background-repeat:no-repeat;\
                 background:url(b.png) center / cover no-repeat content-box red",
            ),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(style.background, color("red").unwrap());
        let images = style.background_images.as_ref().unwrap();
        assert!(matches!(
            &images[0],
            BackgroundImage::Url(url) if url.as_ref() == "b.png"
        ));
        assert_eq!(
            style.background_position.as_ref().unwrap()[0],
            [
                LengthPercentage {
                    pixels: 0.0,
                    fraction: 0.5
                },
                LengthPercentage {
                    pixels: 0.0,
                    fraction: 0.5
                }
            ]
        );
        assert_eq!(
            style.background_size.as_ref().unwrap()[0].kind,
            BackgroundSizeKind::Cover
        );
        assert_eq!(
            style.background_repeat.as_ref().unwrap()[0],
            [BackgroundRepeat::NoRepeat; 2]
        );
        assert_eq!(
            style.background_origin.as_ref().unwrap()[0],
            BackgroundBox::Content
        );
        // One box value sets both origin and clip in the shorthand.
        assert_eq!(
            style.background_clip.as_ref().unwrap()[0],
            BackgroundBox::Content
        );
        // The plain color shorthand still resets images and stays lazy.
        let style = compute(&element("background:red"), None, &index).unwrap();
        assert_eq!(style.background, color("red").unwrap());
        assert!(style.background_images.is_none());
        assert!(style.background_position.is_none());
        assert!(style.background_repeat.is_none());
        // Duplicate attachments reject the whole shorthand instead of leaking.
        assert!(declarations("background:url(a.png) fixed local", 0)
            .map(|parsed| parsed.is_empty())
            .unwrap_or(true));
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
    fn letter_and_word_spacing_parse_and_inherit() {
        let index = StyleIndex::new(Vec::new());
        let parent = compute(
            &element("letter-spacing:2px;word-spacing:10%"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(parent.letter_spacing, Some(2.0));
        assert_eq!(parent.word_spacing.pixels, 0.0);
        assert_eq!(parent.word_spacing.fraction, 0.1);
        assert!(inherited_property(179));
        assert!(inherited_property(180));

        let child = compute(&element(""), Some(&parent), &index).unwrap();
        assert_eq!(child.letter_spacing, parent.letter_spacing);
        assert_eq!(child.word_spacing, parent.word_spacing);

        let reset = compute(
            &element("letter-spacing:normal;word-spacing:normal"),
            Some(&parent),
            &index,
        )
        .unwrap();
        assert_eq!(reset.letter_spacing, None);
        assert_eq!(reset.word_spacing, LengthPercentage::default());
    }

    #[test]
    fn text_indent_parses_inherited_lengths_and_percentages() {
        let index = StyleIndex::new(Vec::new());
        let parent = compute(
            &element("text-indent:calc(4px + 10%)"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(parent.text_indent.pixels, 4.0);
        assert!((parent.text_indent.fraction - 0.1).abs() < 1e-6);
        assert!(inherited_property(181));
        assert!(slots("text-indent").contains(&181));

        let child = compute(&element(""), Some(&parent), &index).unwrap();
        assert_eq!(child.text_indent, parent.text_indent);
        let em = compute(&element("font-size:10px;text-indent:2em"), None, &index).unwrap();
        assert_eq!(em.text_indent.pixels, 20.0);
        let reset = compute(&element("text-indent:initial"), Some(&parent), &index).unwrap();
        assert_eq!(reset.text_indent, LengthPercentage::default());
        assert!(compute(&element("text-indent:bogus"), None, &index)
            .unwrap()
            .text_indent
            == LengthPercentage::default());
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
        assert!(compute(
            &element("box-shadow:none;background:red;width:5px"),
            None,
            &index
        )
        .unwrap()
        .extras
        .is_none());
        assert!(core::mem::size_of::<Style>() <= 200);
    }

    #[test]
    fn registry_drives_aliases_shorthands_and_wide_defaulting() {
        let samples = [
            "0s",
            "linear",
            "running",
            "block",
            "red",
            "1px",
            "0",
            "solid",
            "auto",
            "normal",
            "start",
            "none",
            "repeat",
            "1px / 1px",
            "row",
            "stretch",
            "wrap",
            "ltr",
            "static",
            "fixed",
            "border-box",
            "horizontal-tb",
            "visible",
            "balance",
            "show",
            "top",
            "separate",
            "italic 700 16px/1.5 'Test Face', sans-serif",
            "nonzero",
            "replace",
            "outside",
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
        assert!(compute(&element("border:1px solid red"), None, &index)
            .unwrap()
            .extras
            .is_none());
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
    fn scalar_math_resolves_percentages_and_preserves_signed_zero_in_sign() {
        let (percentage, _) = css_scalar_with_context("calc(20% * 2)", true).unwrap();
        assert!((percentage - 0.4).abs() < 0.0001);
        assert_eq!(css_scalar_with_context("calc(1 + 20%)", true), None);

        let (positive_zero, _) = css_scalar_with_context("sign(0)", true).unwrap();
        let (negative_zero, _) = css_scalar_with_context("sign(-0)", true).unwrap();
        assert_eq!(positive_zero, 0.0);
        assert!(!positive_zero.is_sign_negative());
        assert_eq!(negative_zero, 0.0);
        assert!(negative_zero.is_sign_negative());
        assert_eq!(css_scalar_with_context("sign(2)", true).unwrap().0, 1.0);
        assert_eq!(css_scalar_with_context("sign(-2)", true).unwrap().0, -1.0);
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
    fn comparison_lengths_keep_nonlinear_percentages_until_layout() {
        let kind = NodeKind::Element { namespace: Namespace::Html, name: "div".into(),
            attributes: alloc::vec![("style".into(), "width:min(50%,80px);height:clamp(10px,50%,60px);flex-basis:max(25%,20px);opacity:clamp(50%,80%,70%)".into())] };
        let style = compute(&kind, None, &StyleIndex::new(Vec::new())).unwrap();
        assert_eq!(
            style.resolve_percentages(100.0, Some(100.0)).width,
            Some(50.0)
        );
        assert_eq!(
            style.resolve_percentages(300.0, Some(300.0)).width,
            Some(80.0)
        );
        assert_eq!(
            style.resolve_percentages(100.0, Some(100.0)).height,
            Some(50.0)
        );
        assert_eq!(
            style.resolve_percentages(300.0, Some(300.0)).height,
            Some(60.0)
        );
        assert_eq!(style.resolve_flex_basis_percentage(Some(200.0)), Some(50.0));
        assert_eq!(style.resolve_percentages(100.0, None).height, None);
        assert!((style.opacity - 0.7).abs() < 0.0001);
        for invalid in ["1px", "1.", "calc(1% + 1)", "clamp(0, 1)"] {
            assert!(!supports_declaration("opacity", invalid), "{invalid}");
        }
    }

    #[test]
    fn border_shorthands_expand_atomically_and_reset_previous_sides() {
        let make = |raw: &str| NodeKind::Element {
            namespace: Namespace::Html,
            name: "div".into(),
            attributes: alloc::vec![("style".into(), raw.into())],
        };
        let index = StyleIndex::new(Vec::new());
        let style = compute(&make("border-style:solid;border-width:2px thin medium thick;border-color:red green blue currentcolor;color:white"), None, &index).unwrap();
        assert_eq!(style.border_width, 2.0);
        assert_eq!(
            style.border_width_sides,
            [None, Some(1.0), Some(3.0), Some(5.0)]
        );
        assert_eq!(style.border_color_sides[2].unwrap().b, 255);
        assert_eq!(style.border_color_sides[3], Some(style.color));
        let reset = compute(
            &make("border-left-width:8px;border-top-color:red;border:1px solid blue"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(reset.border_width_sides, [None; 4]);
        assert_eq!(reset.border_color_sides, [None; 4]);
        let parent = compute(&make("border:5px solid red"), None, &index).unwrap();
        let child = compute(
            &make("border-left-width:inherit;border-left-color:inherit"),
            Some(&parent),
            &index,
        )
        .unwrap();
        assert_eq!(child.border_width_sides[3], Some(5.0));
        assert_eq!(child.border_color_sides[3], Some(parent.border_color));
        for invalid in ["1px 2px bad", "1px 2px 3px 4px 5px"] {
            assert!(!supports_declaration("border-width", invalid));
        }
        assert!(!supports_declaration("box-shadow", "2px red 4px"));
        assert!(!supports_declaration("background-position", "left right"));
        assert!(!supports_declaration("background-size", "cover auto"));
    }

    #[test]
    fn typed_border_styles_keep_hidden_distinct_and_cascade_per_side() {
        let make = |raw: &str| NodeKind::Element {
            namespace: Namespace::Html,
            name: "div".into(),
            attributes: alloc::vec![("style".into(), raw.into())],
        };
        let index = StyleIndex::new(Vec::new());
        let styles = [
            ("none", BorderStyle::None),
            ("hidden", BorderStyle::Hidden),
            ("solid", BorderStyle::Solid),
            ("double", BorderStyle::Double),
            ("dotted", BorderStyle::Dotted),
            ("dashed", BorderStyle::Dashed),
            ("groove", BorderStyle::Groove),
            ("ridge", BorderStyle::Ridge),
            ("inset", BorderStyle::Inset),
            ("outset", BorderStyle::Outset),
        ];
        for (raw, expected) in styles {
            let style =
                compute(&make(&alloc::format!("border-style:{raw}")), None, &index).unwrap();
            assert_eq!(style.border_style, expected, "{raw}");
            assert_eq!(style.border_styles(), [expected; 4], "{raw}");
            assert_eq!(style.border_solid, expected.paints(), "{raw}");
        }

        let shorthand = compute(
            &make("border-style:double groove none hidden"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            shorthand.border_styles(),
            [
                BorderStyle::Double,
                BorderStyle::Groove,
                BorderStyle::None,
                BorderStyle::Hidden
            ]
        );
        assert_eq!(shorthand.border_pattern, Some(BorderPattern::Double));
        assert!(!shorthand.border_solid_sides[2].unwrap_or(true));
        assert!(!shorthand.border_solid_sides[3].unwrap_or(true));

        let physical = compute(
            &make("border-style:solid;border-left-style:hidden;border-top:4px groove currentcolor"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(
            physical.border_styles(),
            [
                BorderStyle::Groove,
                BorderStyle::Solid,
                BorderStyle::Solid,
                BorderStyle::Hidden
            ]
        );
        assert!(!physical.border_solid_sides[3].unwrap_or(true));

        let logical = compute(&make("border-inline-start-style:hidden"), None, &index).unwrap();
        assert_eq!(logical.border_styles()[3], BorderStyle::Hidden);

        let parent = compute(
            &make("border-style:double hidden ridge outset"),
            None,
            &index,
        )
        .unwrap();
        let inherited = compute(&make("border-style:inherit"), Some(&parent), &index).unwrap();
        assert_eq!(inherited.border_style, parent.border_style);
        assert_eq!(inherited.border_styles(), parent.border_styles());
        let initial = compute(&make("border-style:initial"), Some(&parent), &index).unwrap();
        assert_eq!(initial.border_style, BorderStyle::None);
        assert_eq!(initial.border_styles(), [BorderStyle::None; 4]);
        let side_initial = compute(
            &make("border-style:solid;border-top-style:initial"),
            Some(&parent),
            &index,
        )
        .unwrap();
        assert_eq!(
            side_initial.border_styles(),
            [
                BorderStyle::None,
                BorderStyle::Solid,
                BorderStyle::Solid,
                BorderStyle::Solid
            ]
        );

        let invalid = compute(
            &make("border-style:double;border-style:solid hidden invalid"),
            None,
            &index,
        )
        .unwrap();
        assert_eq!(invalid.border_styles(), [BorderStyle::Double; 4]);
        assert!(!supports_declaration(
            "border-style",
            "solid hidden invalid"
        ));
        assert!(compute(&make("border:1px hidden red"), None, &index)
            .unwrap()
            .border_styles()
            .into_iter()
            .all(|style| style == BorderStyle::Hidden));

        let uniform = compute(&make("border:1px solid red"), None, &index).unwrap();
        assert!(
            uniform.extras.is_none(),
            "uniform border stays on the compact style path"
        );
    }

    #[test]
    fn border_spacing_parses_and_inherits_both_axes() {
        let make = |raw: &str| NodeKind::Element {
            namespace: Namespace::Html,
            name: "table".into(),
            attributes: alloc::vec![("style".into(), raw.into())],
        };
        let index = StyleIndex::new(Vec::new());
        assert_eq!(Style::initial().border_spacing, [0.0, 0.0]);
        assert_eq!(
            compute(&make("border-spacing:8px 6px"), None, &index)
                .unwrap()
                .border_spacing,
            [8.0, 6.0]
        );
        assert_eq!(
            compute(&make("border-spacing:4px"), None, &index)
                .unwrap()
                .border_spacing,
            [4.0, 4.0]
        );
        assert_eq!(
            compute(
                &make("border-spacing:calc(2px + 6px) calc(2px + 4px)"),
                None,
                &index,
            )
            .unwrap()
            .border_spacing,
            [8.0, 6.0]
        );
        assert!(!supports_declaration("border-spacing", "1px 2px 3px"));

        let parent = compute(&make("border-spacing:8px 6px"), None, &index).unwrap();
        assert_eq!(
            compute(&make("border-spacing:inherit"), Some(&parent), &index)
                .unwrap()
                .border_spacing,
            [8.0, 6.0]
        );
        assert_eq!(
            compute(&make("border-spacing:initial"), Some(&parent), &index)
                .unwrap()
                .border_spacing,
            [0.0, 0.0]
        );
    }

    #[test]
    fn vertical_align_parses_and_uses_its_noninherited_initial_value() {
        let make = |raw: &str| NodeKind::Element {
            namespace: Namespace::Html,
            name: "td".into(),
            attributes: alloc::vec![("style".into(), raw.into())],
        };
        let index = StyleIndex::new(Vec::new());
        assert_eq!(Style::initial().vertical_align, VerticalAlign::Baseline);
        let parent = compute(&make("vertical-align:middle"), None, &index).unwrap();
        assert_eq!(parent.vertical_align, VerticalAlign::Middle);
        assert_eq!(
            compute(&make(""), Some(&parent), &index)
                .unwrap()
                .vertical_align,
            VerticalAlign::Baseline,
            "vertical-align is not inherited"
        );
        assert_eq!(
            compute(&make("vertical-align:inherit"), Some(&parent), &index)
                .unwrap()
                .vertical_align,
            VerticalAlign::Middle
        );
        let VerticalAlign::Length(value) =
            compute(&make("vertical-align:calc(2px + 10%)"), None, &index)
                .unwrap()
                .vertical_align
        else {
            panic!("expected a length-percentage");
        };
        assert_eq!(value.pixels, 2.0);
        // Percentage arithmetic may round by one f32 ULP in calc().
        assert!((value.fraction - 0.1).abs() <= f32::EPSILON);
        for keyword in [
            "baseline",
            "sub",
            "super",
            "text-top",
            "top",
            "bottom",
            "text-bottom",
        ] {
            assert!(supports_declaration("vertical-align", keyword), "{keyword}");
        }
        assert!(supports_declaration("vertical-align", "-2px"));
        assert!(!supports_declaration("vertical-align", "sideways"));
    }

    #[test]
    fn cascade_specificity_and_inline_priority() {
        let rules = StyleIndex::new(parse("p { color: blue; background: #f00 } #title { color: red } p.title { color: green !important }").unwrap());
        let kind = NodeKind::Element {
            namespace: Namespace::Html,
            name: "p".into(),
            attributes: alloc::vec![
                ("id".into(), "title".to_string()),
                ("class".into(), "title".to_string()),
                ("style".into(), "color: white".to_string())
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
    fn type_selectors_are_html_ascii_insensitive_and_svg_case_sensitive() {
        let mut document = Document::new(32);
        let wrapper = document
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: "div".into(),
                attributes: Vec::new(),
            })
            .unwrap();
        let html = document
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: "DIV".into(),
                attributes: Vec::new(),
            })
            .unwrap();
        let svg_root = document
            .create(NodeKind::Element {
                namespace: Namespace::Svg,
                name: "svg".into(),
                attributes: Vec::new(),
            })
            .unwrap();
        let svg = document
            .create(NodeKind::Element {
                namespace: Namespace::Svg,
                name: "linearGradient".into(),
                attributes: Vec::new(),
            })
            .unwrap();
        document.set_attribute(html, "id", "html").unwrap();
        document.set_attribute(svg, "id", "svg").unwrap();
        let root = document.root();
        document.append(root, wrapper).unwrap();
        document.append(wrapper, html).unwrap();
        document.append(wrapper, svg_root).unwrap();
        document.append(svg_root, svg).unwrap();
        let html = crate::selector::query_selector(&document, document.root(), "#html")
            .unwrap()
            .unwrap();
        let svg = crate::selector::query_selector(&document, document.root(), "#svg")
            .unwrap()
            .unwrap();
        assert!(
            matches!(document.kind(svg), Ok(NodeKind::Element { namespace: Namespace::Svg, name, .. }) if name.as_str() == "linearGradient")
        );
        let index = StyleIndex::new(
            parse(
                "DIV { width: 3px } linearGradient { height: 7px } lineargradient { width: 99px }",
            )
            .unwrap(),
        );
        assert_eq!(
            compute_node(&document, html, None, &index).unwrap().width,
            Some(3.0)
        );
        let svg_style = compute_node(&document, svg, None, &index).unwrap();
        assert_eq!(svg_style.height, Some(7.0));
        assert_eq!(svg_style.width, None);
    }

    #[test]
    fn selector_lists_invalidate_the_whole_rule() {
        for selector in ["p, :unknown", "p,", ",p", "p, .1bad"] {
            assert!(parse(&alloc::format!("{selector} {{ color:red }}"))
                .unwrap()
                .is_empty());
        }
        assert_eq!(parse("[title='a,b'], p {color:red}").unwrap().len(), 2);
        assert_eq!(parse(r".a\,b, p {color:red}").unwrap().len(), 2);
    }

    #[test]
    fn attribute_operators_use_the_shared_matcher_in_the_cascade() {
        let document =
            crate::html::parse("<main><i id='target' data-token='one two'></i></main>", 16)
                .unwrap();
        let target = crate::selector::query_selector(&document, document.root(), "#target")
            .unwrap()
            .unwrap();
        let index = StyleIndex::new(
            parse("[data-token~=two] { color: red } [data-token^=one] { width: 9px }").unwrap(),
        );
        let style = compute_node(&document, target, None, &index).unwrap();
        assert_eq!(style.color.r, 255);
        assert_eq!(style.width, Some(9.0));
    }

    #[test]
    fn nested_selector_context_accepts_ampersands_relative_and_logical_arguments() {
        let parents = [".root, #app", "& > .section"];
        for nested in [
            "& .item",
            "&.active > .item",
            "> .item, + .sibling",
            ":is(&, .fallback) > [data-mark='&']",
        ] {
            assert!(
                validate_nested_selector_list(nested, &parents).is_ok(),
                "nested selector rejected: {nested}"
            );
        }
        assert!(parse_selector_list("& .item", 0).is_err());
    }

    #[test]
    fn nested_selector_context_rejects_invalid_parent_and_child_syntax() {
        assert!(validate_nested_selector_list("& .item", &[]).is_err());
        assert!(validate_nested_selector_list("& .item", &[".root,"]).is_err());
        assert!(validate_nested_selector_list("& >", &[".root"]).is_err());
        assert!(validate_nested_selector_list(".item]", &[".root"]).is_err());
    }

    #[test]
    fn attribute_selectors_respect_expanded_names_and_id_class_namespaces() {
        let mut document = crate::html::parse(
            "<main><a id='namespaced'></a><a id='plain' href='plain'></a><i></i></main>",
            16,
        )
        .unwrap();
        let root = document.root();
        let namespaced = crate::selector::query_selector(&document, root, "#namespaced")
            .unwrap()
            .unwrap();
        let plain = crate::selector::query_selector(&document, root, "#plain")
            .unwrap()
            .unwrap();
        let identity_only = crate::selector::query_selector(&document, root, "i")
            .unwrap()
            .unwrap();

        // The same local and qualified name may exist in both namespaces.
        // An unprefixed selector is specifically the no-namespace form.
        document
            .set_attribute_ns(namespaced, Some("urn:custom"), "href", "namespaced")
            .unwrap();
        document
            .set_attribute_ns(namespaced, Some("urn:custom"), "id", "decoy")
            .unwrap();
        document
            .set_attribute_ns(namespaced, Some("urn:custom"), "class", "decoy")
            .unwrap();
        document
            .set_attribute_ns(identity_only, Some("urn:custom"), "id", "ghost")
            .unwrap();
        document
            .set_attribute_ns(identity_only, Some("urn:custom"), "class", "ghost")
            .unwrap();
        document
            .set_attribute_ns(identity_only, Some("urn:custom"), "xlink:href", "prefixed")
            .unwrap();
        document
            .set_attribute_ns(identity_only, None, "id", "id-real")
            .unwrap();
        document
            .set_attribute_ns(identity_only, None, "class", "class-real")
            .unwrap();

        assert_eq!(
            crate::selector::query_selector(&document, root, "[href]").unwrap(),
            Some(plain)
        );
        assert_eq!(
            crate::selector::query_selector(&document, root, "[|href]").unwrap(),
            Some(plain)
        );
        assert_eq!(
            crate::selector::query_selector(&document, root, "[*|href]").unwrap(),
            Some(namespaced)
        );
        assert_eq!(
            crate::selector::query_selector_all(&document, root, "[*|href]").unwrap(),
            [namespaced, plain, identity_only]
        );
        assert!(crate::selector::query_selector(&document, root, "#ghost")
            .unwrap()
            .is_none());
        assert!(crate::selector::query_selector(&document, root, ".ghost")
            .unwrap()
            .is_none());
        assert_eq!(
            crate::selector::query_selector(&document, root, "#id-real").unwrap(),
            Some(identity_only)
        );
        assert_eq!(
            crate::selector::query_selector(&document, root, ".class-real").unwrap(),
            Some(identity_only)
        );
        assert_eq!(
            crate::selector::query_selector(&document, root, "[*|id]").unwrap(),
            Some(namespaced)
        );
        assert!(crate::selector::query_selector(&document, root, "[custom|href]").is_err());
        assert!(
            crate::selector::query_selector(&document, root, ":is([custom|href])")
                .unwrap()
                .is_none()
        );

        let index =
            StyleIndex::new(parse("[href] { width: 2px } [*|href] { height: 3px }").unwrap());
        let namespaced_style = compute_node(&document, namespaced, None, &index).unwrap();
        let plain_style = compute_node(&document, plain, None, &index).unwrap();
        assert_eq!(namespaced_style.width, None);
        assert_eq!(namespaced_style.height, Some(3.0));
        assert_eq!(plain_style.width, Some(2.0));
        assert_eq!(plain_style.height, Some(3.0));
        let identity_index = StyleIndex::new(
            parse("#id-real { margin-left: 4px } .class-real { margin-right: 5px }").unwrap(),
        );
        let identity_style = compute_node(&document, identity_only, None, &identity_index).unwrap();
        assert_eq!(identity_style.margin_sides[3], 4.0);
        assert_eq!(identity_style.margin_sides[1], 5.0);
    }

    #[test]
    fn inline_style_reads_only_the_null_namespace_attribute() {
        let mut document = crate::html::parse("<main><div id='target'></div></main>", 16).unwrap();
        let target = crate::selector::query_selector(&document, document.root(), "#target")
            .unwrap()
            .unwrap();
        document
            .set_attribute_ns(target, Some("urn:custom"), "style", "color: red")
            .unwrap();
        let index = StyleIndex::new(Vec::new());
        let style = compute_node(&document, target, None, &index).unwrap();
        assert_eq!(style.color, Style::initial().color);

        document
            .set_attribute_ns(target, None, "style", "color: blue")
            .unwrap();
        assert_eq!(
            compute_node(&document, target, None, &index).unwrap().color,
            color("blue").unwrap()
        );
    }

    #[test]
    fn universal_namespace_type_selectors_preserve_xml_local_name_case() {
        let document = crate::xml::parse(
            r#"<?xml version="1.0"?><cp:coreProperties xmlns:cp="urn:core" xmlns:dc="urn:title" id="target"><dc:title/></cp:coreProperties>"#,
            16,
        )
        .unwrap();
        let root = document.root();
        let target = crate::selector::query_selector(&document, root, "coreProperties")
            .unwrap()
            .unwrap();
        assert_eq!(
            crate::selector::query_selector(&document, root, "*|coreProperties").unwrap(),
            Some(target)
        );
        assert!(
            crate::selector::query_selector(&document, root, "*|CoreProperties")
                .unwrap()
                .is_none()
        );
        let title = crate::selector::query_selector(&document, root, "*|coreProperties > *|title")
            .unwrap()
            .unwrap();
        assert!(
            matches!(document.kind(title), Ok(NodeKind::Element { name, .. }) if name.as_str() == "dc:title")
        );
        assert!(crate::selector::query_selector(&document, root, "cp|coreProperties").is_err());
        assert!(
            crate::selector::query_selector(&document, root, ":is(cp|coreProperties)")
                .unwrap()
                .is_none()
        );

        let index = StyleIndex::new(parse("*|coreProperties { width: 12px }").unwrap());
        assert_eq!(
            compute_node(&document, target, None, &index).unwrap().width,
            Some(12.0)
        );
    }

    #[test]
    fn content_none_and_replacement_values_reach_their_apply_arm() {
        let document =
            crate::html::parse("<main><select id='target'></select></main>", 16).unwrap();
        let target = crate::selector::query_selector(&document, document.root(), "#target")
            .unwrap()
            .unwrap();
        let none = StyleIndex::new(parse("#target { content: none }").unwrap());
        assert_eq!(
            compute_node(&document, target, None, &none)
                .unwrap()
                .generated_content(),
            GeneratedContent::None
        );

        let replacement =
            StyleIndex::new(parse("#target { content: url('resources/rect.svg') }").unwrap());
        assert!(matches!(
            compute_node(&document, target, None, &replacement)
                .unwrap()
                .generated_content(),
            GeneratedContent::Items(items)
                if matches!(items.first(), Some(GeneratedContentItem::Url(url)) if &**url == "resources/rect.svg")
        ));
    }

    #[test]
    fn html_default_attribute_value_insensitivity_reaches_the_cascade() {
        let document = crate::html::parse(
            "<main><input id='target' type='TEXT' rel='StyleSheet' data-label='MiXeD'></main>",
            16,
        )
        .unwrap();
        let target = crate::selector::query_selector(&document, document.root(), "#target")
            .unwrap()
            .unwrap();
        let index = StyleIndex::new(
            parse(
                "[type=text] { color: red } [rel~=stylesheet] { width: 9px } [data-label=mixed] { opacity: 0.25 }",
            )
            .unwrap(),
        );
        let style = compute_node(&document, target, None, &index).unwrap();
        assert_eq!(style.color.r, 255);
        assert_eq!(style.width, Some(9.0));
        assert_eq!(style.opacity, 1.0);
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
        assert_eq!(
            color("rgba(100%, 0%, 0%, 0.5)").unwrap(),
            Rgba {
                r: 255,
                g: 0,
                b: 0,
                a: 128
            }
        );
        assert_eq!(color("rgb(16, 32, 48)").unwrap().b, 48);
        assert_eq!(
            color("RGB(100% 0% 0% / 50%)").unwrap(),
            Rgba {
                r: 255,
                g: 0,
                b: 0,
                a: 128
            }
        );
        assert_eq!(
            color("hsl(0.5turn 100% 50%)").unwrap(),
            Rgba {
                r: 0,
                g: 255,
                b: 255,
                a: 255
            }
        );
        assert_eq!(
            color("hwb(120 20% 10%)").unwrap(),
            Rgba {
                r: 51,
                g: 230,
                b: 51,
                a: 255
            }
        );
        assert_eq!(
            color("color(srgb 1 0 0)").unwrap(),
            Rgba {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            }
        );
        assert_eq!(color("color(srgb-linear 0 1 0)").unwrap().g, 255);
        assert!(color("lab(52.2345 40.1645 59.9971)").is_some());
        assert!(color("lch(52.2345 72.2 56.2)").is_some());
        assert!(color("oklab(0.65125 -0.0320 0.1274)").is_some());
        assert!(color("oklch(0.452 0.313 264.1)").is_some());
        assert_eq!(
            background_shorthand("lab(52.2345 40.1645 59.9971)")
                .unwrap()
                .color,
            color("lab(52.2345 40.1645 59.9971)")
        );
        assert!(color("color(display-p3 0.4 0.2 0.9)").is_some());
        assert!(color("color(a98-rgb 0.44091 0.49971 0.37408)").is_some());
        assert!(color("color(prophoto-rgb 0.36589 0.41717 0.31333)").is_some());
        assert!(color("color(rec2020 0.6295 0.9657 0.3633)").is_some());
        assert!(background_shorthand("oklab(0.65125 -0.0320 0.1274)").is_some());
        assert_eq!(
            color("rebeccapurple").unwrap(),
            Rgba {
                r: 102,
                g: 51,
                b: 153,
                a: 255
            }
        );
        assert_eq!(
            color("CanvasText").unwrap(),
            Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 255
            }
        );
        assert_eq!(
            color("color-mix(in srgb, red, blue)").unwrap(),
            Rgba {
                r: 128,
                g: 0,
                b: 128,
                a: 255
            }
        );
        assert_eq!(
            color("color-mix(in srgb, red 75%, blue 75%)").unwrap(),
            Rgba {
                r: 128,
                g: 0,
                b: 128,
                a: 255
            }
        );
        assert_eq!(
            color("color-mix(in hsl shorter hue, hsl(120 100% 50%), hsl(240 100% 50%))").unwrap(),
            Rgba {
                r: 0,
                g: 255,
                b: 255,
                a: 255
            }
        );
        assert_eq!(
            color("color-mix(in hsl longer hue, hsl(120 100% 50%), hsl(240 100% 50%))").unwrap(),
            Rgba {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            }
        );
        assert!(color("color-mix(in oklab, white, black)").is_some());
        assert!(color("color-mix(in lch longer hue, lch(50 60 40), lch(50 60 280))").is_some());
        assert!(color("color-mix(in hwb, red, yellow)").is_some());
        assert_eq!(
            color("rgb(from red r g b)").unwrap(),
            Rgba {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            }
        );
        assert_eq!(
            color("rgb(from rgb(255 128 0) calc(r/2) g b)").unwrap(),
            Rgba {
                r: 128,
                g: 128,
                b: 0,
                a: 255
            }
        );
    }

    #[test]
    fn flex_shorthand_preserves_math_components_and_clamps_calculated_factors() {
        let kind = |raw: &str| NodeKind::Element {
            namespace: Namespace::Html,
            name: "div".into(),
            attributes: alloc::vec![("style".into(), raw.into())],
        };
        let index = StyleIndex::new(Vec::new());
        let style = compute(&kind("flex:calc(1) calc(2 + 1) calc(3px)"), None, &index).unwrap();
        assert_eq!(
            (style.flex_grow, style.flex_shrink, style.flex_basis),
            (1.0, 3.0, Some(3.0))
        );
        let style = compute(&kind("flex:calc(-1) calc(-1) 0"), None, &index).unwrap();
        assert_eq!(
            (style.flex_grow, style.flex_shrink, style.flex_basis),
            (0.0, 0.0, Some(0.0))
        );
        assert!(!supports_declaration("flex", "-1 1 0"));
        assert!(!supports_declaration("flex-grow", "-1"));
        assert!(supports_declaration("flex-grow", "calc(-1)"));
    }

    #[test]
    fn background_position_axes_and_size_runs_follow_author_grammar() {
        let position =
            background_position(&["center", "right", "7%"], static_length_context()).unwrap();
        assert!((position[0].fraction - 0.93).abs() < 0.001);
        assert_eq!(position[1].fraction, 0.5);
        assert!(background_shorthand("black 0 url(https://example.invalid/) / cover").is_none());
        assert!(background_shorthand("black url(https://example.invalid/) 0 / cover").is_some());
        assert!(background_shorthand("calc(10px + 5%) center / cover black").is_some());
    }

    #[test]
    fn contextual_and_relative_colors_resolve_after_inherited_color() {
        let parent = Style {
            color: Rgba {
                r: 255,
                g: 0,
                b: 0,
                a: 128,
            },
            ..Style::initial()
        };
        let kind = NodeKind::Element {
            namespace: Namespace::Html, name: "div".into(),
            attributes: alloc::vec![("style".into(),
                "color:rgb(from currentcolor calc(r / 2) g b / alpha);background-color:alpha(from currentcolor / 25%);border-color:currentcolor blue".into())],
        };
        let style = compute(&kind, Some(&parent), &StyleIndex::new(Vec::new())).unwrap();
        assert_eq!(
            style.color,
            Rgba {
                r: 128,
                g: 0,
                b: 0,
                a: 128
            }
        );
        assert_eq!(
            style.background,
            Rgba {
                r: 128,
                g: 0,
                b: 0,
                a: 64
            }
        );
        assert_eq!(style.border_color, style.color);
        assert_eq!(color("hsl(from red 120 s l)"), color("lime"));
        assert_eq!(color("alpha(from blue)"), color("blue"));
        assert_eq!(color("light-dark(red, blue)"), color("red"));
        assert_eq!(color("contrast-color(white)"), color("black"));
        assert_eq!(color("hwb(from red 120 w b / alpha)"), color("lime"));
        assert_eq!(
            color("color(srgb calc(1 / 2) 50% 0 / 25%)"),
            Some(Rgba {
                r: 128,
                g: 128,
                b: 0,
                a: 64
            })
        );
        assert_eq!(
            color("color-mix(red, blue)"),
            color("color-mix(in oklab, red, blue)")
        );
        assert!(color("contrast-color(color-mix(blue, green))").is_some());
        assert_eq!(color("color(srgb NaN 0 0)"), None);
        assert_eq!(color("color(srgb 0px 0 0)"), None);
        for value in [-2.0, -0.5, -0.01, 0.0, 0.01, 0.5, 2.0] {
            assert!((linear_to_srgb(srgb_to_linear(value)) - value).abs() < 0.00001);
        }
        assert_eq!(color("color(display-p3 -1 -1 -1)"), color("black"));
        assert_eq!(color("color(a98-rgb -1 -1 -1)"), color("black"));
        for invalid in [
            "rgb(10%, 20, 30)",
            "rgb(NaN 0 0)",
            "lab(inf 0 0)",
            "hsl(from red 50% s l)",
            "alpha(from red / NaN)",
        ] {
            assert_eq!(color(invalid), None, "{invalid}");
        }
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
    fn native_text_control_defaults_are_bounded_and_hidden_inputs_stay_hidden() {
        let index = StyleIndex::new(Vec::new());
        let input = |size: &str| NodeKind::Element {
            namespace: Namespace::Html,
            name: "input".into(),
            attributes: alloc::vec![("size".into(), size.into())],
        };
        let default = compute(&input("invalid"), None, &index).unwrap();
        assert_eq!(default.display, Display::InlineBlock);
        assert!((default.width.unwrap() - 20.0 * 13.333_333 * 0.52).abs() < 0.001);
        assert_eq!(
            default.font.families.as_deref().unwrap()[0].as_ref(),
            "Arial"
        );
        let bounded = compute(&input("999"), None, &index).unwrap();
        assert!((bounded.width.unwrap() - 256.0 * 13.333_333 * 0.52).abs() < 0.01);

        let textarea = NodeKind::Element {
            namespace: Namespace::Html,
            name: "textarea".into(),
            attributes: alloc::vec![("cols".into(), "999".into()), ("rows".into(), "999".into())],
        };
        let textarea = compute(&textarea, None, &index).unwrap();
        assert_eq!(textarea.display, Display::InlineBlock);
        assert_eq!(textarea.white_space, WhiteSpace::PreWrap);
        assert!((textarea.width.unwrap() - 256.0 * 13.333_333 * 0.6).abs() < 0.01);
        assert!((textarea.height.unwrap() - 256.0 * 13.333_333 * 1.2).abs() < 0.01);

        let hidden = NodeKind::Element {
            namespace: Namespace::Html,
            name: "input".into(),
            attributes: alloc::vec![("type".into(), "hidden".into())],
        };
        assert_eq!(
            compute(&hidden, None, &index).unwrap().display,
            Display::None
        );
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
    fn generated_content_parser_keeps_typed_tokens_and_limits_input() {
        let parsed = parse_generated_content(
            r#""before " attr(data-label, "fallback") url(../icon.svg) counter(step, upper-roman) counters(section, ".", decimal) open-quote / "spoken""#,
        )
        .unwrap();
        let GeneratedContent::Items(items) = parsed else {
            panic!("expected generated items");
        };
        assert_eq!(items.len(), 7);
        assert_eq!(items[0], GeneratedContentItem::String(Arc::from("before ")));
        assert_eq!(
            items[1],
            GeneratedContentItem::Attribute {
                name: Arc::from("data-label"),
                fallback: Some(Arc::from("fallback")),
            }
        );
        assert_eq!(
            items[2],
            GeneratedContentItem::Url(Arc::from("../icon.svg"))
        );
        assert_eq!(
            items[3],
            GeneratedContentItem::Counter {
                name: Arc::from("step"),
                style: Arc::from("upper-roman"),
            }
        );
        assert_eq!(
            items[4],
            GeneratedContentItem::Counters {
                name: Arc::from("section"),
                separator: Arc::from("."),
                style: Arc::from("decimal"),
            }
        );
        assert_eq!(items[5], GeneratedContentItem::OpenQuote);
        assert_eq!(
            items[6],
            GeneratedContentItem::AlternativeText(
                alloc::vec![GeneratedContentItem::String(Arc::from("spoken"))].into()
            )
        );
        assert_eq!(
            parse_generated_content("normal"),
            Some(GeneratedContent::Normal)
        );
        assert_eq!(
            parse_generated_content("none"),
            Some(GeneratedContent::None)
        );
        assert!(parse_generated_content("'unterminated").is_none());
        for invalid in [
            "open-quote / no-open-quote",
            "close-quote / no-close-quote",
            "'hello' / 'hi' no-close-quote",
            "url(icon.svg) / url(alt.svg)",
            "'hello' / url(https://example.test/picture.svg)",
        ] {
            assert!(
                parse_generated_content(invalid).is_none(),
                "invalid alternative text was accepted: {invalid}"
            );
        }
        assert!(parse_counter_declaration(CounterProperty::Set, "default 2").is_none());
        assert!(parse_generated_content(&alloc::format!(
            "\"{}\"",
            "x".repeat(MAX_GENERATED_CONTENT_BYTES)
        ))
        .is_none());
        assert!(matches!(
            parse_generated_content("image-set(url(a.png) 1x)").unwrap(),
            GeneratedContent::Items(items)
                if matches!(items.first(), Some(GeneratedContentItem::UnsupportedFunction { name, .. }) if &**name == "image-set")
        ));
    }

    #[test]
    fn counter_declarations_are_typed_bounded_and_cssom_serialization_is_canonical() {
        assert_eq!(
            parse_counter_directives("chapter 2 reversed(section)", true, 0),
            Some(Some(
                alloc::vec![
                    CounterDirective {
                        name: Arc::from("chapter"),
                        value: 2,
                        reversed: false,
                        explicit_value: true,
                        math_expression: None,
                    },
                    CounterDirective {
                        name: Arc::from("section"),
                        value: 0,
                        reversed: true,
                        explicit_value: false,
                        math_expression: None,
                    },
                ]
                .into()
            ))
        );
        assert_eq!(
            parse_counter_directives("section -3 section 4", false, 1),
            Some(Some(
                alloc::vec![
                    CounterDirective {
                        name: Arc::from("section"),
                        value: -3,
                        reversed: false,
                        explicit_value: true,
                        math_expression: None,
                    },
                    CounterDirective {
                        name: Arc::from("section"),
                        value: 4,
                        reversed: false,
                        explicit_value: true,
                        math_expression: None,
                    },
                ]
                .into()
            ))
        );
        assert_eq!(parse_counter_directives("none", false, 1), Some(None));
        assert!(parse_counter_directives("none step", false, 1).is_none());
        assert!(parse_counter_directives("initial 2", false, 1).is_none());
        assert!(parse_counter_directives("x 2.5", false, 1).is_none());

        assert_eq!(
            serialize_counter_declaration(CounterProperty::Reset, "chapter"),
            Some("chapter 0".into())
        );
        assert_eq!(
            serialize_counter_declaration(CounterProperty::Increment, "myCounter"),
            Some("myCounter 1".into())
        );
        assert_eq!(
            serialize_counter_declaration(CounterProperty::Set, "chapter"),
            Some("chapter 0".into())
        );
        assert_eq!(
            serialize_counter_declaration(
                CounterProperty::Reset,
                "chapter reversed(chapter) reversed(section) 0"
            ),
            Some("chapter 0 reversed(chapter) reversed(section) 0".into())
        );
        assert_eq!(
            serialize_counter_declaration(CounterProperty::Reset, "reversed(chapter)"),
            Some("reversed(chapter)".into())
        );
        assert_eq!(
            serialize_counter_declaration(CounterProperty::Reset, r"a\ 8 9"),
            Some(r"a\ 8 9".into())
        );
        assert_eq!(
            serialize_counter_declaration(CounterProperty::Increment, "section +001"),
            Some("section 1".into())
        );
        assert_eq!(
            serialize_counter_declaration(CounterProperty::Set, "section -0"),
            Some("section 0".into())
        );
        assert_eq!(
            serialize_counter_declaration(CounterProperty::Reset, "section calc(1)"),
            Some("section calc(1)".into())
        );
        assert_eq!(
            serialize_counter_declaration(CounterProperty::Reset, "section calc(-2.5)"),
            Some("section calc(-2.5)".into())
        );
        assert_eq!(
            serialize_counter_declaration(CounterProperty::Reset, "section calc(2.5)"),
            Some("section calc(2.5)".into())
        );
        assert_eq!(
            serialize_counter_declaration(CounterProperty::Increment, "section calc(1 + sign(2))"),
            Some("section calc(1 + sign(2))".into())
        );
        let parsed_calc = parse_counter_declaration(CounterProperty::Reset, "section calc(-2.5)")
            .unwrap()
            .unwrap();
        assert_eq!(
            serialize_counter_directives(CounterProperty::Reset, Some(&parsed_calc)),
            Some("section -2".into())
        );
        for (source, expected) in [
            ("counter calc(999999999999)", "counter 2100000000"),
            ("counter calc(-999999999999)", "counter -2100000000"),
        ] {
            let parsed = parse_counter_declaration(CounterProperty::Set, source)
                .unwrap()
                .unwrap();
            assert_eq!(
                serialize_counter_directives(CounterProperty::Set, Some(&parsed)),
                Some(expected.into())
            );
        }
        assert!(serialize_counter_declaration(
            CounterProperty::Increment,
            "section reversed(section)"
        )
        .is_none());
        assert!(serialize_counter_declaration(
            CounterProperty::Set,
            "section calc(10 + (5 * sign(2cqw - 10px)))"
        )
        .is_none());
        assert_eq!(
            serialize_counter_directives(CounterProperty::Set, None),
            Some("none".into())
        );
        assert_eq!(
            serialize_counter_declaration(CounterProperty::Reset, "none"),
            Some("none".into())
        );

        assert_eq!(
            serialize_content_declaration("open-quote 'hello' counter(chapter, DECIMAL) / 'alt'"),
            Some("open-quote \"hello\" counter(chapter) / \"alt\"".into())
        );
        assert_eq!(
            serialize_content_declaration("counter(chapter, NONE) counters(section, '.', DECIMAL)"),
            Some("counter(chapter, none) counters(section, \".\")".into())
        );
        assert_eq!(
            serialize_quotes_declaration("'left' \"right\" 'inner' 'end'"),
            Some("\"left\" \"right\" \"inner\" \"end\"".into())
        );
    }

    #[test]
    fn quotes_parser_keeps_bounded_pairs_and_keywords() {
        assert_eq!(parse_quotes("auto"), Some(Quotes::Auto));
        assert_eq!(parse_quotes("none"), Some(Quotes::None));
        assert_eq!(parse_quotes("match-parent"), Some(Quotes::MatchParent));
        assert_eq!(
            parse_quotes(r#""outer \" left" "right" "inner" "end""#),
            Some(Quotes::Pairs(
                alloc::vec![
                    (Arc::from("outer \" left"), Arc::from("right")),
                    (Arc::from("inner"), Arc::from("end")),
                ]
                .into()
            ))
        );
        assert!(parse_quotes("\"unpaired\"").is_none());
        assert!(parse_quotes("\"one\" two").is_none());
        let too_many = alloc::vec!["\"a\" \"b\""; MAX_QUOTES_PAIRS + 1].join(" ");
        assert!(parse_quotes(&too_many).is_none());
    }

    #[test]
    fn q_default_pseudos_use_open_and_close_quote_content() {
        let document = crate::html::parse("<main><q id='target'>quoted</q></main>", 64).unwrap();
        let target = crate::selector::query_selector(&document, document.root(), "#target")
            .unwrap()
            .unwrap();
        let index = StyleIndex::new(Vec::new());
        let origin = compute_node(&document, target, None, &index).unwrap();
        let before = index
            .compute_pseudo(&document, target, &origin, PseudoElement::Before, None)
            .unwrap()
            .unwrap();
        let after = index
            .compute_pseudo(&document, target, &origin, PseudoElement::After, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            before.content,
            GeneratedContent::Items(alloc::vec![GeneratedContentItem::OpenQuote].into())
        );
        assert_eq!(
            after.content,
            GeneratedContent::Items(alloc::vec![GeneratedContentItem::CloseQuote].into())
        );

        let overrides = StyleIndex::new(
            parse("q::before { content: normal; quotes: none } q::after { quotes: '<' '>' }")
                .unwrap(),
        );
        assert!(overrides
            .compute_pseudo(&document, target, &origin, PseudoElement::Before, None)
            .unwrap()
            .is_none());
        assert_eq!(
            overrides
                .compute_pseudo(&document, target, &origin, PseudoElement::After, None)
                .unwrap()
                .unwrap()
                .style
                .quotes(),
            &Quotes::Pairs(alloc::vec![(Arc::from("<"), Arc::from(">"))].into())
        );
    }

    #[test]
    fn list_item_defaults_inherit_style_and_build_the_marker_counter() {
        let document = crate::html::parse(
            "<ol id='items' start='7'><li id='first'></li><li id='second' value='12'></li></ol>",
            32,
        )
        .unwrap();
        let list = crate::selector::query_selector(&document, document.root(), "#items")
            .unwrap()
            .unwrap();
        let first = crate::selector::query_selector(&document, document.root(), "#first")
            .unwrap()
            .unwrap();
        let second = crate::selector::query_selector(&document, document.root(), "#second")
            .unwrap()
            .unwrap();
        let index = StyleIndex::new(
            parse(
                "#items { list-style-type: upper-alpha } #second { list-style-position: inside } #second::marker { content: counter(list-item, upper-alpha) '>' }",
            )
            .unwrap(),
        );

        let list_style = compute_node(&document, list, None, &index).unwrap();
        assert_eq!(list_style.list_style_type, ListStyleType::UpperAlpha);
        assert_eq!(list_style.counter_reset.as_deref().unwrap()[0].value, 6);
        let first_style = compute_node(&document, first, Some(&list_style), &index).unwrap();
        let second_style = compute_node(&document, second, Some(&list_style), &index).unwrap();
        assert_eq!(first_style.display, Display::ListItem);
        assert_eq!(first_style.list_style_type, ListStyleType::UpperAlpha);
        assert_eq!(second_style.list_style_position, ListStylePosition::Inside);
        assert_eq!(second_style.counter_set.as_deref().unwrap()[0].value, 12);
        let marker = index
            .compute_pseudo(
                &document,
                second,
                &second_style,
                PseudoElement::Marker,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            marker.content,
            GeneratedContent::Items(
                alloc::vec![
                    GeneratedContentItem::Counter {
                        name: Arc::from("list-item"),
                        style: Arc::from("upper-alpha"),
                    },
                    GeneratedContentItem::String(Arc::from(">")),
                ]
                .into()
            )
        );
    }

    #[test]
    fn list_style_shorthand_assigns_ambiguous_none_components() {
        let document = crate::html::parse(
            "<div id='single'></div><div id='pair'></div><div id='none-type'></div><div id='type-none'></div>",
            64,
        )
        .unwrap();
        let index = StyleIndex::new(
            parse(
                "#single { display:list-item; list-style:none } #pair { display:list-item; list-style:none none } #none-type { display:list-item; list-style:none square } #type-none { display:list-item; list-style:square none }",
            )
            .unwrap(),
        );

        for id in ["single", "pair"] {
            let node = crate::selector::query_selector(
                &document,
                document.root(),
                &alloc::format!("#{id}"),
            )
            .unwrap()
            .unwrap();
            let style = compute_node(&document, node, None, &index).unwrap();
            assert_eq!(style.list_style_type, ListStyleType::None, "#{id}");
            assert_eq!(style.list_style_position, ListStylePosition::Outside);
            assert!(index
                .compute_pseudo(&document, node, &style, PseudoElement::Marker, None)
                .unwrap()
                .is_none());
        }

        for id in ["none-type", "type-none"] {
            let node = crate::selector::query_selector(
                &document,
                document.root(),
                &alloc::format!("#{id}"),
            )
            .unwrap()
            .unwrap();
            let style = compute_node(&document, node, None, &index).unwrap();
            assert_eq!(style.list_style_type, ListStyleType::Square, "#{id}");
            let marker = index
                .compute_pseudo(&document, node, &style, PseudoElement::Marker, None)
                .unwrap()
                .unwrap();
            assert_eq!(
                marker.content,
                GeneratedContent::Items(
                    alloc::vec![GeneratedContentItem::String(Arc::from("▪"))].into()
                )
            );
        }
        assert!(
            typed_declarations("list-style", "url(marker.png) square", 0)
                .unwrap()
                .iter()
                .any(|declaration| matches!(
                    &declaration.value,
                    Value::ListStyleImage(Some(url)) if url.as_ref() == "marker.png"
                )),
            "image shorthand must retain its image component"
        );
    }

    #[test]
    fn list_style_image_is_inherited_and_kept_in_the_shorthand() {
        let document = crate::html::parse(
            "<ul id='list'><li id='item'></li></ul><div id='none'></div>",
            32,
        )
        .unwrap();
        let index = StyleIndex::new(
            parse(
                "#list { list-style-image: url(marker.png); list-style-type: none } #none { display:list-item; list-style: none url(other.png) inside }",
            )
            .unwrap(),
        );
        let list = crate::selector::query_selector(&document, document.root(), "#list")
            .unwrap()
            .unwrap();
        let item = crate::selector::query_selector(&document, document.root(), "#item")
            .unwrap()
            .unwrap();
        let none = crate::selector::query_selector(&document, document.root(), "#none")
            .unwrap()
            .unwrap();
        let list_style = compute_node(&document, list, None, &index).unwrap();
        let item_style = compute_node(&document, item, Some(&list_style), &index).unwrap();
        assert_eq!(
            item_style.list_style_image.as_deref(),
            Some("marker.png"),
            "the source URL remains shared and inherits through list items"
        );
        let marker = index
            .compute_pseudo(&document, item, &item_style, PseudoElement::Marker, None)
            .unwrap()
            .unwrap();
        assert!(matches!(marker.content, GeneratedContent::Items(items) if items.is_empty()));

        let shorthand_style = compute_node(&document, none, None, &index).unwrap();
        assert_eq!(shorthand_style.list_style_type, ListStyleType::None);
        assert_eq!(shorthand_style.list_style_position, ListStylePosition::Inside);
        assert_eq!(
            shorthand_style.list_style_image.as_deref(),
            Some("other.png")
        );
        assert_eq!(
            typed_declarations("list-style", "square none", 0)
                .unwrap()
                .len(),
            3,
            "the shorthand resets all three longhands"
        );
    }

    #[test]
    fn malformed_at_keyword_preludes_discard_one_rule_and_keep_following_rules() {
        assert!(matches!(
            rule_boundary("@ import url(a); div{}"),
            Some(RuleBoundary::Block(_))
        ));
        assert!(matches!(rule_boundary("@1import; div{}"), Some(RuleBoundary::Block(_))));
        assert!(matches!(rule_boundary("@-1import; div{}"), Some(RuleBoundary::Block(_))));
        assert!(matches!(
            rule_boundary("@import url(a);"),
            Some(RuleBoundary::Statement(_))
        ));

        let document = crate::html::parse("<div id='target'></div>", 16).unwrap();
        for malformed in ["@ import bogus; div", "@1import bogus; div", "@-1import bogus; div"] {
            let rules = parse(&alloc::format!(
                "{malformed} {{ color:red }} * {{ color:green }}"
            ))
            .unwrap();
            let target = crate::selector::query_selector(&document, document.root(), "#target")
                .unwrap()
                .unwrap();
            let style = compute_node(&document, target, None, &StyleIndex::new(rules)).unwrap();
            assert_eq!(
                style.color,
                color("green").unwrap(),
                "a malformed at-keyword must not swallow the later qualified rule: {malformed}"
            );
        }
    }

    #[test]
    fn html_caption_default_display_is_table_caption() {
        let document =
            crate::html::parse("<table><caption id='caption'>x</caption></table>", 32).unwrap();
        let caption = crate::selector::query_selector(&document, document.root(), "#caption")
            .unwrap()
            .unwrap();
        let style = compute_node(&document, caption, None, &StyleIndex::new(Vec::new())).unwrap();
        assert_eq!(style.display, Display::TableCaption);
    }

    #[test]
    fn before_after_cascade_is_separate_and_inherits_from_origin() {
        let mut document = Document::new(16);
        let html = document
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: "html".into(),
                attributes: Vec::new(),
            })
            .unwrap();
        let body = document
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: "body".into(),
                attributes: Vec::new(),
            })
            .unwrap();
        let target = document
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: "div".into(),
                attributes: alloc::vec![
                    ("id".into(), "target".into()),
                    ("data-label".into(), "card".into()),
                ],
            })
            .unwrap();
        let other = document
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: "div".into(),
                attributes: alloc::vec![("id".into(), "other".into())],
            })
            .unwrap();
        document.append(document.root(), html).unwrap();
        document.append(html, body).unwrap();
        document.append(body, target).unwrap();
        document.append(body, other).unwrap();
        let index = StyleIndex::new(
            parse(
                r#"#target { color: blue; font-size: 20px; content: "origin" }
                   #target::before {
                       content: "before " attr(data-label, "fallback") counter(step, upper-roman) / "spoken";
                       color: red;
                   }
                   #target::after { content: inherit; color: green }
                   #other { content: "other" }
                   #other::before { color: red }"#,
            )
            .unwrap(),
        );
        let origin_style = compute_node(&document, target, None, &index).unwrap();
        let element_style = compute_node(&document, other, None, &index).unwrap();
        assert_eq!(element_style.color.r, 0);
        assert_eq!(element_style.display, Display::Block);
        let before = index
            .compute_pseudo(
                &document,
                target,
                &origin_style,
                PseudoElement::Before,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(before.style.display, Display::Inline);
        assert_eq!(before.style.color.r, 255);
        assert_eq!(before.style.font_size, 20.0);
        let GeneratedContent::Items(items) = &before.content else {
            panic!("before should have typed content");
        };
        assert!(matches!(
            items.get(1),
            Some(GeneratedContentItem::Attribute { name, fallback: Some(fallback) })
                if &**name == "data-label" && &**fallback == "fallback"
        ));
        assert!(matches!(
            items.get(2),
            Some(GeneratedContentItem::Counter { .. })
        ));

        let after = index
            .compute_pseudo(&document, target, &origin_style, PseudoElement::After, None)
            .unwrap()
            .unwrap();
        assert_eq!(after.style.color.g, 128);
        assert_eq!(
            after.content,
            GeneratedContent::Items(
                alloc::vec![GeneratedContentItem::String(Arc::from("origin"))].into()
            )
        );
        assert!(index
            .compute_pseudo(
                &document,
                other,
                &element_style,
                PseudoElement::Before,
                None,
            )
            .unwrap()
            .is_none());
        // Pseudo content defaults to Normal and does not inherit the origin's
        // computed `content` declaration.
        assert_eq!(
            element_style.generated_content(),
            GeneratedContent::Items(
                alloc::vec![GeneratedContentItem::String(Arc::from("other"))].into()
            )
        );

        document
            .set_attribute(target, "data-label", "changed")
            .unwrap();
        let refreshed = index
            .compute_pseudo(
                &document,
                target,
                &origin_style,
                PseudoElement::Before,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(refreshed.content, before.content);
        assert!(
            matches!(document.kind(target), Ok(NodeKind::Element { attributes, .. })
            if attributes.iter().any(|(name, value)| name == "data-label" && value == "changed"))
        );
    }

    #[test]
    fn generated_pseudo_selectors_are_terminal_and_count_as_type_specificity() {
        let before = parse_selector_list("div::before", 0).unwrap().remove(0);
        assert_eq!(before.pseudo_element, Some(PseudoElement::Before));
        assert_eq!(before.specificity, (0, 0, 2));
        let legacy_after = parse_selector_list(".note:after", 0).unwrap().remove(0);
        assert_eq!(legacy_after.pseudo_element, Some(PseudoElement::After));
        assert_eq!(legacy_after.specificity, (0, 1, 1));
        for invalid in [
            "div::before span",
            "div::before:hover",
            "div::before::after",
            "div::host",
            "div::first-child",
            ":not(::before)",
            "div:has(::after)",
            "li:nth-child(2 of ::before)",
        ] {
            assert!(parse_selector_list(invalid, 0).is_err(), "{invalid}");
        }
    }

    #[test]
    fn pseudo_content_inherit_survives_the_generated_content_fast_path() {
        let document = crate::html::parse("<div id='origin'></div>", 64).unwrap();
        let target = crate::selector::query_selector(&document, document.root(), "#origin")
            .unwrap()
            .unwrap();
        let index = StyleIndex::new(
            parse("#origin { content: 'inherited' } #origin::before { content: inherit }").unwrap(),
        );
        let origin_style = compute_node(&document, target, None, &index).unwrap();
        let generated = index
            .compute_pseudo(
                &document,
                target,
                &origin_style,
                PseudoElement::Before,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            generated.content,
            GeneratedContent::Items(
                alloc::vec![GeneratedContentItem::String(Arc::from("inherited"))].into()
            )
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

    #[test]
    fn computed_animation_lists_and_keyframes_survive_shared_parse() {
        let document = crate::html::parse(
            "<div id='target' style='animation: 100ms ease-in 20ms 3 alternate both paused pulse'></div>",
            16,
        )
        .unwrap();
        let target = crate::selector::query_selector(&document, document.root(), "#target")
            .unwrap()
            .unwrap();
        let style = compute_node(&document, target, None, &StyleIndex::new(Vec::new())).unwrap();
        assert_eq!(style.animation[0].as_deref(), Some("pulse"));
        assert_eq!(style.animation[1].as_deref(), Some("100ms"));
        assert_eq!(style.animation[2].as_deref(), Some("20ms"));
        assert_eq!(style.animation[4].as_deref(), Some("3"));
        assert_eq!(style.animation[5].as_deref(), Some("alternate"));
        assert_eq!(style.animation[6].as_deref(), Some("both"));
        assert_eq!(style.animation[7].as_deref(), Some("paused"));

        let parsed = parse_stylesheet(
            "@media (min-width: 1px) { @keyframes fade { from { opacity: 0 } to { opacity: 1 } } }",
        )
        .unwrap();
        assert_eq!(parsed.keyframes.len(), 1);
        assert_eq!(parsed.keyframes[0].name, "fade");
        assert_eq!(parsed.keyframes[0].media[0].as_ref(), "(min-width: 1px)");
        assert_eq!(
            keyframe_offsets("from, 50%, to").as_deref(),
            Some(&[0.0, 0.5, 1.0][..])
        );
        assert_eq!(
            parse_keyframe_declarations("opacity: .5; color: red !important")
                .unwrap()
                .len(),
            2
        );
    }
}
