//! Block formatting into a typed display list in CSS pixels.
use crate::{
    Document, Namespace, NodeId, NodeKind,
    css::{
        self, AlignItems, BoxSizing, Clear, Direction, Display, FlexDirection, Float,
        JustifyContent, LineHeight, Position, Style, StyleIndex, TextAlign, WhiteSpace,
    },
    paint::{Command, DisplayList, ImageData, Rect, Rgba, TextShaper},
};
use alloc::borrow::Cow;
use alloc::sync::Arc;
use alloc::vec::Vec;

pub trait ImageResolver {
    fn resolve(&self, source: &str) -> ImageState;
    fn generation(&self) -> u64 {
        0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ImageState {
    Ready(Arc<ImageData>),
    Pending,
    Failed,
}

impl<F: Fn(&str) -> ImageState> ImageResolver for F {
    fn resolve(&self, source: &str) -> ImageState {
        self(source)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LayoutError {
    Css(css::CssError),
    InvalidTree,
    DepthLimit,
    Text,
    CommandLimit,
    ImagePending,
    ImageFailed,
    GridLimit,
}

const MAX_DISPLAY_COMMANDS: usize = 16 * 1024;

fn collapsed_text(value: &str) -> Cow<'_, str> {
    let mut previous_space = false;
    let needs_collapse = value.chars().any(|ch| {
        let space = matches!(ch, ' ' | '\t' | '\n' | '\r' | '\x0c');
        let changed = space && (previous_space || ch != ' ');
        previous_space = space;
        changed
    });
    if !needs_collapse {
        return Cow::Borrowed(value);
    }
    let mut out = alloc::string::String::with_capacity(value.len());
    previous_space = false;
    for ch in value.chars() {
        let space = matches!(ch, ' ' | '\t' | '\n' | '\r' | '\x0c');
        if !space || !previous_space {
            out.push(if space { ' ' } else { ch });
        }
        previous_space = space;
    }
    Cow::Owned(out)
}

fn formatted_text(value: &str, whitespace: WhiteSpace) -> Cow<'_, str> {
    if matches!(
        whitespace,
        WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::BreakSpaces
    ) {
        return Cow::Borrowed(value);
    }
    if whitespace != WhiteSpace::PreLine {
        return collapsed_text(value);
    }
    if !value.contains(['\t', '\r', '\x0c']) && !value.contains("  ") {
        return Cow::Borrowed(value);
    }
    let mut out = alloc::string::String::with_capacity(value.len());
    let mut space = false;
    for ch in value.chars() {
        if ch == '\n' {
            while out.ends_with(' ') {
                out.pop();
            }
            out.push(ch);
            space = true;
        } else if matches!(ch, ' ' | '\t' | '\r' | '\x0c') {
            if !space {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(ch);
            space = false;
        }
    }
    Cow::Owned(out)
}

pub(crate) fn stylesheets(document: &Document) -> Result<StyleIndex, LayoutError> {
    let mut rules = Vec::new();
    let mut pending = alloc::vec![document.root()];
    while let Some(id) = pending.pop() {
        if let NodeKind::Element {
            name, namespace, ..
        } = document.kind(id).map_err(|_| LayoutError::InvalidTree)?
        {
            if *namespace != Namespace::Html || name == "template" {
                continue;
            }
            if name == "style" {
                if let Some(child) = document
                    .first_child(id)
                    .map_err(|_| LayoutError::InvalidTree)?
                {
                    if document
                        .next_sibling(child)
                        .map_err(|_| LayoutError::InvalidTree)?
                        .is_none()
                    {
                        if let NodeKind::Text(value) =
                            document.kind(child).map_err(|_| LayoutError::InvalidTree)?
                        {
                            let parsed = css::parse(value).map_err(LayoutError::Css)?;
                            if rules.len() + parsed.len() > css::MAX_RULES {
                                return Err(LayoutError::Css(css::CssError {
                                    offset: 0,
                                    message: "too many rules",
                                }));
                            }
                            rules.extend(parsed);
                            continue;
                        }
                    }
                }
                let mut text = alloc::string::String::new();
                let mut child = document
                    .first_child(id)
                    .map_err(|_| LayoutError::InvalidTree)?;
                while let Some(current) = child {
                    if let NodeKind::Text(value) = document
                        .kind(current)
                        .map_err(|_| LayoutError::InvalidTree)?
                    {
                        text.push_str(value);
                    }
                    child = document
                        .next_sibling(current)
                        .map_err(|_| LayoutError::InvalidTree)?;
                }
                let parsed = css::parse(&text).map_err(LayoutError::Css)?;
                if rules.len() + parsed.len() > css::MAX_RULES {
                    return Err(LayoutError::Css(css::CssError {
                        offset: 0,
                        message: "too many rules",
                    }));
                }
                rules.extend(parsed);
                continue;
            }
        }
        let mut child = document
            .last_child(id)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(current) = child {
            pending.push(current);
            child = document
                .previous_sibling(current)
                .map_err(|_| LayoutError::InvalidTree)?;
        }
    }
    Ok(StyleIndex::new(rules))
}

struct Layout<'a> {
    document: &'a Document,
    text: &'a dyn TextShaper,
    rules: &'a StyleIndex,
    images: Option<&'a dyn ImageResolver>,
    commands: Vec<Command>,
    body_background_on_canvas: bool,
    viewport: Rect,
    geometry: Option<&'a mut LayoutGeometry>,
    scrolls: &'a [ScrollOffset],
    containing_block: Option<Rect>,
    fixed_containing_block: Option<Rect>,
    transform_depth: usize,
    parent_height: Option<f32>,
    decorations: Vec<TextDecoration>,
}

#[derive(Clone, Copy)]
struct TextDecoration {
    lines: u8,
    color: Rgba,
    size: f32,
}

#[derive(Clone, Copy)]
pub(crate) struct ScrollOffset {
    pub node: NodeId,
    pub x: f32,
    pub y: f32,
}

#[derive(Default)]
pub(crate) struct LayoutGeometry {
    pub transforms: Vec<HitTransform>,
    pub hits: Vec<HitRegion>,
    pub rounded_clips: Vec<HitClip>,
    pub scroll_extents: Vec<ScrollOffset>,
}

pub(crate) struct HitClip {
    pub first_transform: usize,
    pub hits: core::ops::Range<usize>,
    pub rect: Rect,
    pub radius: f32,
}

pub(crate) struct HitTransform {
    pub hits: core::ops::Range<usize>,
    pub matrix: crate::paint::Affine,
}

impl LayoutGeometry {
    pub(crate) fn contains_hit(&self, index: usize, x: f32, y: f32) -> bool {
        let (mut local_x, mut local_y) = (x, y);
        for transform in self
            .transforms
            .iter()
            .rev()
            .filter(|v| v.hits.contains(&index))
        {
            let Some(inverse) = transform.matrix.inverse() else {
                return false;
            };
            (local_x, local_y) = inverse.apply(local_x, local_y);
        }
        let Some(hit) = self.hits.get(index) else {
            return false;
        };
        if !hit.rect.contains_rounded(local_x, local_y, 0.0) {
            return false;
        }
        self.rounded_clips
            .iter()
            .filter(|clip| clip.hits.contains(&index))
            .all(|clip| {
                let (mut clip_x, mut clip_y) = (x, y);
                for transform in self.transforms[clip.first_transform..]
                    .iter()
                    .rev()
                    .filter(|v| v.hits.start <= clip.hits.start && clip.hits.end <= v.hits.end)
                {
                    let Some(inverse) = transform.matrix.inverse() else {
                        return false;
                    };
                    (clip_x, clip_y) = inverse.apply(clip_x, clip_y);
                }
                clip.rect.contains_rounded(clip_x, clip_y, clip.radius)
            })
    }
    fn move_hits(&mut self, range: core::ops::Range<usize>, x: f32, y: f32) {
        for transform in &mut self.transforms {
            if range.start <= transform.hits.start && transform.hits.end <= range.end {
                transform.matrix = transform.matrix.translated_space(x, y);
            }
        }
        for hit in &mut self.hits[range.clone()] {
            hit.rect.x += x;
            hit.rect.y += y;
        }
        for clip in &mut self.rounded_clips {
            if range.start <= clip.hits.start && clip.hits.end <= range.end {
                clip.rect.x += x;
                clip.rect.y += y;
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct HitRegion {
    pub node: NodeId,
    pub rect: Rect,
}

fn background_color(rect: Rect, style: &Style) -> Command {
    if style.border_radius > 0.0 {
        let inset = background_bleed_inset(style);
        let radius = style.border_radius.min(rect.width.min(rect.height) * 0.5);
        let rect = Rect {
            x: rect.x + inset,
            y: rect.y + inset,
            width: (rect.width - 2.0 * inset).max(0.0),
            height: (rect.height - 2.0 * inset).max(0.0),
        };
        Command::FillRoundedRect {
            rect,
            radius: (radius - inset).max(0.0),
            color: style.background,
        }
    } else {
        Command::FillRect {
            rect,
            color: style.background,
        }
    }
}

fn background_bleed_inset(style: &Style) -> f32 {
    // Blink's shrink-background path hides the fill edge behind an opaque solid border.
    if style.border_radius > 0.0
        && style.border_solid
        && style.border_pattern.is_none()
        && style.border_color.a == 255
    {
        style.border_width * 0.5
    } else {
        0.0
    }
}

fn grid_track_sizes(
    definitions: &[css::GridTrack],
    count: usize,
    available: Option<f32>,
    gap: f32,
    stretch: bool,
    contributions: impl Iterator<Item = (usize, usize, f32, f32)>,
) -> [f32; css::MAX_GRID_TRACKS] {
    use css::{GridBreadth as B, GridTrack as T, MAX_GRID_TRACKS};
    let mut sizes = [0.0f32; MAX_GRID_TRACKS];
    let mut limits = [f32::INFINITY; MAX_GRID_TRACKS];
    let fixed = |breadth| match breadth {
        B::Pixels(v) => Some(v),
        B::Percentage(v) => available.map(|size| size * v / 100.0),
        B::Length(px, pct) => available.map(|size| px + size * pct / 100.0),
        _ => None,
    };
    for i in 0..count {
        let track = definitions.get(i).copied().unwrap_or(T::Auto);
        sizes[i] = fixed(track.minimum()).unwrap_or(0.0);
        limits[i] = fixed(track.maximum())
            .unwrap_or(f32::INFINITY)
            .max(sizes[i]);
    }
    for (start, span, min, max) in contributions {
        for maximum in [false, true] {
            let eligible = |i: usize| {
                let track = definitions.get(i).copied().unwrap_or(T::Auto);
                let breadth = if maximum {
                    track.maximum()
                } else {
                    track.minimum()
                };
                if maximum {
                    matches!(breadth, B::MaxContent | B::Auto | B::MinContent)
                        && !matches!(track, T::MinMax(_, B::Fraction(_)))
                } else {
                    matches!(breadth, B::Auto | B::MinContent | B::MaxContent)
                        || matches!(breadth, B::Percentage(_)) && available.is_none()
                }
            };
            let current = sizes[start..start + span].iter().sum::<f32>() + gap * (span - 1) as f32;
            let breadth = if maximum {
                definitions.get(start).copied().unwrap_or(T::Auto).maximum()
            } else {
                definitions.get(start).copied().unwrap_or(T::Auto).minimum()
            };
            let target = if span == 1 && matches!(breadth, B::MinContent) {
                min
            } else if maximum || span == 1 && matches!(breadth, B::MaxContent) {
                max
            } else {
                min
            };
            let mut extra = (target - current).max(0.0);
            for _ in 0..span {
                let eligible_count = (start..start + span)
                    .filter(|i| eligible(*i) && sizes[*i] < limits[*i])
                    .count();
                if eligible_count == 0 || extra <= 0.0 {
                    break;
                }
                let share = extra / eligible_count as f32;
                for i in start..start + span {
                    if eligible(i) && sizes[i] < limits[i] {
                        let added = share.min(limits[i] - sizes[i]);
                        sizes[i] += added;
                        extra -= added;
                    }
                }
            }
        }
        for i in start..start + span {
            if let Some(T::FitContent(limit)) = definitions.get(i) {
                sizes[i] = sizes[i].min(limit.max(min / span as f32));
            }
        }
    }
    if let Some(available) = available {
        let mut extra =
            (available - sizes[..count].iter().sum::<f32>() - gap * count.saturating_sub(1) as f32)
                .max(0.0);
        for _ in 0..count {
            let growing = (0..count)
                .filter(|i| limits[*i].is_finite() && sizes[*i] < limits[*i])
                .count();
            if growing == 0 || extra <= 0.0 {
                break;
            }
            let share = extra / growing as f32;
            for i in 0..count {
                if limits[i].is_finite() && sizes[i] < limits[i] {
                    let added = share.min(limits[i] - sizes[i]);
                    sizes[i] += added;
                    extra -= added;
                }
            }
        }
        let mut frozen = [false; MAX_GRID_TRACKS];
        for _ in 0..=count {
            let mut used = gap * count.saturating_sub(1) as f32;
            let mut fractions = 0.0;
            for i in 0..count {
                match definitions.get(i).copied().unwrap_or(T::Auto).maximum() {
                    B::Fraction(v) if !frozen[i] => fractions += v,
                    _ => used += sizes[i],
                }
            }
            if fractions == 0.0 {
                break;
            }
            let unit = ((available - used).max(0.0) / fractions.max(1.0)).max(0.0);
            let mut changed = false;
            for i in 0..count {
                if let B::Fraction(v) = definitions.get(i).copied().unwrap_or(T::Auto).maximum() {
                    if !frozen[i] && sizes[i] > unit * v {
                        frozen[i] = true;
                        changed = true;
                    }
                }
            }
            if !changed {
                for i in 0..count {
                    if let B::Fraction(v) = definitions.get(i).copied().unwrap_or(T::Auto).maximum()
                    {
                        if !frozen[i] {
                            sizes[i] = unit * v;
                        }
                    }
                }
                break;
            }
        }
        if stretch {
            let extra = (available
                - sizes[..count].iter().sum::<f32>()
                - gap * count.saturating_sub(1) as f32)
                .max(0.0);
            let autos = (0..count)
                .filter(|i| {
                    matches!(
                        definitions.get(*i).copied().unwrap_or(T::Auto),
                        T::Auto | T::MinMax(_, B::Auto)
                    )
                })
                .count();
            if autos > 0 {
                for i in 0..count {
                    if matches!(
                        definitions.get(i).copied().unwrap_or(T::Auto),
                        T::Auto | T::MinMax(_, B::Auto)
                    ) {
                        sizes[i] += extra / autos as f32;
                    }
                }
            }
        }
    } else {
        let unit = (0..count)
            .filter_map(
                |i| match definitions.get(i).copied().unwrap_or(T::Auto).maximum() {
                    B::Fraction(v) if v > 0.0 => Some(sizes[i] / v.max(1.0)),
                    _ => None,
                },
            )
            .fold(0.0f32, f32::max);
        for i in 0..count {
            if let B::Fraction(v) = definitions.get(i).copied().unwrap_or(T::Auto).maximum() {
                sizes[i] = sizes[i].max(unit * v);
            }
        }
    }
    sizes
}

fn has_background(style: &Style) -> bool {
    style.background.a != 0 || style.gradients.is_some() || style.shadows.is_some()
}

fn background_paints(style: &Style) -> usize {
    usize::from(style.background.a != 0 || style.gradients.is_none())
        + style.gradients.as_ref().map_or(0, |v| v.len())
        + style.shadows.as_ref().map_or(0, |v| v.len())
}

fn border(rect: Rect, style: &Style) -> Command {
    if let Some(pattern) = style.border_pattern {
        return Command::StrokePatternBorder {
            rect,
            radius: style.border_radius,
            width: style.border_width,
            color: style.border_color,
            pattern,
        };
    }
    Command::StrokeBorder {
        rect,
        radius: style.border_radius,
        width: style.border_width,
        color: style.border_color,
    }
}

fn content_dimension(style: &Style, specified: f32) -> f32 {
    let inset = if style.box_sizing == BoxSizing::BorderBox {
        style.padding_sides[1]
            + style.padding_sides[3]
            + if style.border_solid {
                2.0 * style.border_width
            } else {
                0.0
            }
    } else {
        0.0
    };
    (specified - inset).max(0.0)
}

fn content_height_dimension(style: &Style, specified: f32) -> f32 {
    (specified
        - if style.box_sizing == BoxSizing::BorderBox {
            style.padding_sides[0]
                + style.padding_sides[2]
                + if style.border_solid {
                    2.0 * style.border_width
                } else {
                    0.0
                }
        } else {
            0.0
        })
    .max(0.0)
}

fn box_edges(style: &Style, horizontal: bool) -> f32 {
    let side = usize::from(horizontal);
    style.padding_sides[side]
        + style.padding_sides[side + 2]
        + style.margin_sides[side]
        + style.margin_sides[side + 2]
        + if style.border_solid {
            2.0 * style.border_width
        } else {
            0.0
        }
}

fn specified_height(style: &Style, content: f32) -> f32 {
    content
        + if style.box_sizing == BoxSizing::BorderBox {
            style.padding_sides[0]
                + style.padding_sides[2]
                + if style.border_solid {
                    2.0 * style.border_width
                } else {
                    0.0
                }
        } else {
            0.0
        }
}

fn specified_dimension(style: &Style, content: f32) -> f32 {
    content
        + if style.box_sizing == BoxSizing::BorderBox {
            style.padding_sides[1]
                + style.padding_sides[3]
                + if style.border_solid {
                    2.0 * style.border_width
                } else {
                    0.0
                }
        } else {
            0.0
        }
}

fn constrained_width(style: &Style, content: f32) -> f32 {
    content
        .min(
            style
                .max_width
                .map_or(f32::INFINITY, |value| content_dimension(style, value)),
        )
        .max(content_dimension(style, style.min_width))
}

fn constrained_height(style: &Style, content: f32) -> f32 {
    content
        .min(style.max_height.map_or(f32::INFINITY, |value| {
            content_height_dimension(style, value)
        }))
        .max(content_height_dimension(style, style.min_height))
}

fn move_command(command: &mut Command, dx: f32, dy: f32) {
    match command {
        Command::PushTransform(matrix) => *matrix = matrix.translated_space(dx, dy),
        Command::FillBackground {
            rect,
            positioning_rect,
            image_rect,
            ..
        } => {
            for rect in [rect, positioning_rect, image_rect] {
                rect.x += dx;
                rect.y += dy;
            }
        }
        Command::PushClip(rect)
        | Command::PushLayer { rect, .. }
        | Command::FillRect { rect, .. }
        | Command::FillRoundedRect { rect, .. }
        | Command::FillGradient { rect, .. }
        | Command::BoxShadow { rect, .. }
        | Command::StrokeBorder { rect, .. }
        | Command::StrokePatternBorder { rect, .. }
        | Command::Image { rect, .. } => {
            rect.x += dx;
            rect.y += dy;
        }
        Command::GlyphRun {
            origin_x,
            baseline_y,
            ..
        } => {
            *origin_x += dx;
            *baseline_y += dy;
        }
        Command::PopClip | Command::PopLayer | Command::PopTransform => {}
    }
}

fn float_edges(floats: &[(Rect, Float)], left: f32, width: f32, y: f32, height: f32) -> (f32, f32) {
    let mut start = left;
    let mut end = left + width;
    for (rect, side) in floats {
        if rect.y < y + height && rect.y + rect.height > y {
            if *side == Float::Left {
                start = start.max(rect.x + rect.width);
            } else {
                end = end.min(rect.x);
            }
        }
    }
    (start, (end - start).max(0.0))
}

impl Layout<'_> {
    fn decorate_text(
        &mut self,
        x: f32,
        baseline: f32,
        width: f32,
        style: &Style,
    ) -> Result<(), LayoutError> {
        if width <= 0.0 {
            return Ok(());
        }
        let count = self
            .decorations
            .len()
            .max(usize::from(style.text_decoration != 0));
        for index in 0..count {
            let source = self
                .decorations
                .get(index)
                .copied()
                .unwrap_or(TextDecoration {
                    lines: style.text_decoration,
                    color: style.color,
                    size: style.font_size,
                });
            let (underline, thickness) = self.text.underline_metrics(source.size);
            let (strike, strike_thickness) = self.text.strike_metrics(source.size);
            for (bit, offset, thickness) in [
                (1, underline, thickness),
                (2, -self.text.ascent(source.size), thickness),
                (4, strike, strike_thickness),
            ] {
                if source.lines & bit != 0 {
                    self.push_command(Command::FillRect {
                        rect: Rect {
                            x,
                            y: baseline + offset,
                            width,
                            height: thickness.max(1.0),
                        },
                        color: source.color,
                    })?;
                }
            }
        }
        Ok(())
    }

    fn begin_decoration(&mut self, style: &Style) -> Result<(), LayoutError> {
        if style.text_decoration != 0 {
            if self.decorations.len() >= 512 {
                return Err(LayoutError::DepthLimit);
            }
            self.decorations
                .try_reserve(1)
                .map_err(|_| LayoutError::CommandLimit)?;
            self.decorations.push(TextDecoration {
                lines: style.text_decoration,
                color: style.color,
                size: style.font_size,
            });
        }
        Ok(())
    }
    fn push_background(&mut self, rect: Rect, style: &Style) -> Result<(), LayoutError> {
        if let Some(shadows) = &style.shadows {
            for shadow in shadows.iter().rev().filter(|shadow| !shadow.inset) {
                self.push_command(Command::BoxShadow {
                    rect,
                    radius: style.border_radius,
                    shadow: *shadow,
                })?;
            }
        }
        if style.background.a != 0 || style.gradients.is_none() {
            self.push_command(background_color(rect, style))?;
        }
        if let Some(gradients) = &style.gradients {
            let border = if style.border_solid {
                style.border_width
            } else {
                0.0
            };
            let rect = Rect {
                x: rect.x + border,
                y: rect.y + border,
                width: (rect.width - 2.0 * border).max(0.0),
                height: (rect.height - 2.0 * border).max(0.0),
            };
            for gradient in gradients.iter().rev() {
                self.push_command(Command::FillGradient {
                    rect,
                    radius: (style.border_radius - border).max(0.0),
                    gradient: gradient.clone(),
                })?;
            }
        }
        if let Some(shadows) = &style.shadows {
            let border = if style.border_solid {
                style.border_width
            } else {
                0.0
            };
            let rect = Rect {
                x: rect.x + border,
                y: rect.y + border,
                width: (rect.width - 2.0 * border).max(0.0),
                height: (rect.height - 2.0 * border).max(0.0),
            };
            for shadow in shadows.iter().rev().filter(|shadow| shadow.inset) {
                self.push_command(Command::BoxShadow {
                    rect,
                    radius: (style.border_radius - border).max(0.0),
                    shadow: *shadow,
                })?;
            }
        }
        Ok(())
    }

    fn finish_background(&mut self, index: usize, rect: Rect, style: &Style) {
        let border = if style.border_solid {
            style.border_width
        } else {
            0.0
        };
        let padding_rect = Rect {
            x: rect.x + border,
            y: rect.y + border,
            width: (rect.width - 2.0 * border).max(0.0),
            height: (rect.height - 2.0 * border).max(0.0),
        };
        let first = index + 1 - background_paints(style);
        for command in &mut self.commands[first..=index] {
            match command {
                Command::FillGradient { rect, .. } => *rect = padding_rect,
                Command::BoxShadow {
                    rect: target,
                    shadow,
                    ..
                } => *target = if shadow.inset { padding_rect } else { rect },
                Command::FillRect { .. } | Command::FillRoundedRect { .. } => {
                    *command = background_color(rect, style)
                }
                _ => {}
            }
        }
    }
    fn table_rows(
        &self,
        id: NodeId,
        style: &Style,
        rows: &mut Vec<(NodeId, Style)>,
        depth: usize,
    ) -> Result<(), LayoutError> {
        if depth > 512 {
            return Err(LayoutError::DepthLimit);
        }
        let mut child = self
            .document
            .first_child(id)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(node) = child {
            if matches!(self.document.kind(node), Ok(NodeKind::Element { .. })) {
                let computed = css::compute_node(self.document, node, Some(style), self.rules)
                    .map_err(LayoutError::Css)?;
                if computed.display == Display::TableRow {
                    if rows.len() == 4096 {
                        return Err(LayoutError::CommandLimit);
                    }
                    rows.push((node, computed));
                } else if computed.display == Display::TableRowGroup {
                    self.table_rows(node, &computed, rows, depth + 1)?;
                }
            }
            child = self
                .document
                .next_sibling(node)
                .map_err(|_| LayoutError::InvalidTree)?;
        }
        Ok(())
    }

    fn table(
        &mut self,
        id: NodeId,
        style: &Style,
        x: f32,
        y: f32,
        available: f32,
        depth: usize,
    ) -> Result<f32, LayoutError> {
        struct Cell {
            node: NodeId,
            style: Style,
            row: usize,
            col: usize,
            cols: usize,
            rows: usize,
            height: f32,
        }
        let mut rows = Vec::new();
        self.table_rows(id, style, &mut rows, depth)?;
        let mut occupied = [0usize; 256];
        let mut widths = [0f32; 256];
        let mut cells = Vec::new();
        let mut columns = 0;
        for (row, (node, parent)) in rows.iter().enumerate() {
            let mut col = 0;
            let mut child = self
                .document
                .first_child(*node)
                .map_err(|_| LayoutError::InvalidTree)?;
            while let Some(node) = child {
                if let Ok(NodeKind::Element { attributes, .. }) = self.document.kind(node) {
                    let computed = css::compute_node(self.document, node, Some(parent), self.rules)
                        .map_err(LayoutError::Css)?;
                    if computed.display == Display::TableCell {
                        let span = |key: &str, limit: usize| {
                            attributes
                                .iter()
                                .find(|(name, _)| name == key)
                                .and_then(|(_, v)| v.parse::<usize>().ok())
                                .unwrap_or(1)
                                .max(1)
                                .min(limit)
                        };
                        let cols = span("colspan", 256);
                        while col + cols <= 256
                            && occupied[col..col + cols].iter().any(|end| *end > row)
                        {
                            col += 1;
                        }
                        if col + cols > 256 || cells.len() == 4096 {
                            return Err(LayoutError::CommandLimit);
                        }
                        let rowspan = if attributes
                            .iter()
                            .any(|(name, v)| name == "rowspan" && v == "0")
                        {
                            rows.len() - row
                        } else {
                            span("rowspan", rows.len() - row)
                        };
                        for slot in &mut occupied[col..col + cols] {
                            *slot = row + rowspan;
                        }
                        let (width, height) = self.intrinsic_size(node, &computed, depth + 1)?;
                        if !style.table_fixed || style.width.is_none() || row == 0 {
                            let width = if style.table_fixed && style.width.is_some() {
                                computed.width.unwrap_or(0.0)
                            } else {
                                width
                            } / cols as f32;
                            for slot in &mut widths[col..col + cols] {
                                *slot = slot.max(width);
                            }
                        }
                        cells.push(Cell {
                            node,
                            style: computed,
                            row,
                            col,
                            cols,
                            rows: rowspan,
                            height,
                        });
                        col += cols;
                        columns = columns.max(col);
                    }
                }
                child = self
                    .document
                    .next_sibling(node)
                    .map_err(|_| LayoutError::InvalidTree)?;
            }
        }
        let spacing = style.border_spacing.max(0.0);
        let inset = style.padding
            + if style.border_solid {
                style.border_width
            } else {
                0.0
            };
        let natural: f32 = widths[..columns].iter().sum();
        let gaps = spacing * (columns + 1) as f32;
        let width = style
            .width
            .map(|w| content_dimension(style, w))
            .unwrap_or((natural + gaps).min(available))
            .max(natural + gaps);
        let remaining = (width - gaps - natural).max(0.0);
        let unspecified = widths[..columns].iter().filter(|w| **w == 0.0).count();
        for slot in &mut widths[..columns] {
            if style.table_fixed && style.width.is_some() && unspecified > 0 {
                if *slot == 0.0 {
                    *slot = remaining / unspecified as f32;
                }
            } else {
                *slot += remaining / columns.max(1) as f32;
            }
        }
        let mut heights = alloc::vec![0f32; rows.len()];
        for cell in &cells {
            if cell.rows != 1 {
                continue;
            }
            heights[cell.row] = heights[cell.row].max(cell.height);
        }
        for cell in &cells {
            if cell.rows == 1 {
                continue;
            }
            let present = heights[cell.row..cell.row + cell.rows].iter().sum::<f32>()
                + spacing * (cell.rows - 1) as f32;
            let extra = (cell.height - present).max(0.0) / cell.rows as f32;
            for slot in &mut heights[cell.row..cell.row + cell.rows] {
                *slot += extra;
            }
        }
        let ox = x + style.margin;
        let oy = y + style.margin;
        let height = heights.iter().sum::<f32>() + spacing * (rows.len() + 1) as f32;
        let rect = Rect {
            x: ox,
            y: oy,
            width: width + inset * 2.0,
            height: height + inset * 2.0,
        };
        let hit = self.begin_hit(id)?;
        self.finish_hit(hit, rect);
        if has_background(style) {
            self.push_background(rect, style)?;
        }
        if style.border_solid {
            self.push_command(border(rect, style))?;
        }
        let mut xs = [0f32; 257];
        for col in 0..columns {
            xs[col + 1] = xs[col] + widths[col] + spacing;
        }
        let mut ys = Vec::with_capacity(heights.len() + 1);
        ys.push(0.0);
        for height in heights {
            ys.push(ys.last().copied().unwrap_or(0.0) + height + spacing);
        }
        for (row, (node, computed)) in rows.iter().enumerate() {
            let rect = Rect {
                x: ox + inset + spacing,
                y: oy + inset + spacing + ys[row],
                width: (width - 2.0 * spacing).max(0.0),
                height: ys[row + 1] - ys[row] - spacing,
            };
            let hit = self.begin_hit(*node)?;
            self.finish_hit(hit, rect);
            if has_background(computed) {
                self.push_background(rect, computed)?;
            }
        }
        for cell in cells {
            let cy = oy + inset + spacing + ys[cell.row];
            let cw = xs[cell.col + cell.cols] - xs[cell.col] - spacing;
            let cx = ox
                + inset
                + if style.direction == Direction::Rtl {
                    width - spacing - xs[cell.col] - cw
                } else {
                    spacing + xs[cell.col]
                };
            let ch = ys[cell.row + cell.rows] - ys[cell.row] - spacing;
            let mut computed = cell.style;
            computed.display = Display::Block;
            computed.margin = 0.0;
            computed.box_sizing = BoxSizing::BorderBox;
            computed.width = Some(cw);
            computed.height = Some(ch);
            self.box_for(cell.node, style, Some(computed), cx, cy, cw, depth + 1)?;
        }
        Ok(rect.height + 2.0 * style.margin)
    }

    fn intrinsic_size(
        &self,
        id: NodeId,
        style: &Style,
        depth: usize,
    ) -> Result<(f32, f32), LayoutError> {
        self.intrinsic_size_mode(id, style, depth, false)
    }

    fn intrinsic_size_mode(
        &self,
        id: NodeId,
        style: &Style,
        depth: usize,
        minimum: bool,
    ) -> Result<(f32, f32), LayoutError> {
        if depth > 512 {
            return Err(LayoutError::DepthLimit);
        }
        let kind = self
            .document
            .kind(id)
            .map_err(|_| LayoutError::InvalidTree)?;
        if let NodeKind::Text(value) = kind {
            let text = collapsed_text(value);
            if minimum
                && !matches!(
                    style.white_space,
                    css::WhiteSpace::NoWrap | css::WhiteSpace::Pre
                )
            {
                let mut width = 0.0f32;
                for word in text.split_ascii_whitespace() {
                    width = width.max(
                        self.text
                            .measure(word, style.font_size)
                            .map_err(|_| LayoutError::Text)?,
                    );
                }
                return Ok((width, self.line_height(style)));
            }
            return Ok((
                self.text
                    .measure(text.trim_matches(' '), style.font_size)
                    .map_err(|_| LayoutError::Text)?,
                self.line_height(style),
            ));
        }
        let NodeKind::Element {
            name,
            namespace: Namespace::Html,
            attributes,
        } = kind
        else {
            return Ok((0.0, 0.0));
        };
        if style.display == Display::None
            || matches!(
                name.as_str(),
                "head" | "style" | "script" | "template" | "meta" | "link"
            )
        {
            return Ok((0.0, 0.0));
        }
        let edges = box_edges(style, true);
        let vertical_edges = box_edges(style, false);
        if let (Some(width), Some(height)) = (style.width, style.height) {
            return Ok((
                constrained_width(style, content_dimension(style, width)) + edges,
                content_height_dimension(style, height) + vertical_edges,
            ));
        }
        if name == "img" {
            let dimension = |key: &str| {
                attributes
                    .iter()
                    .find(|(name, _)| name == key)
                    .and_then(|(_, value)| value.parse::<f32>().ok())
                    .filter(|value| value.is_finite() && *value >= 0.0)
            };
            let mut width = style
                .width
                .or_else(|| dimension("width"))
                .map(|value| content_dimension(style, value));
            let mut height = style
                .height
                .or_else(|| dimension("height"))
                .map(|value| content_height_dimension(style, value));
            if width.is_none() || height.is_none() {
                if let Some((_, source)) = attributes.iter().find(|(name, _)| name == "src") {
                    let image = match self.images.map(|images| images.resolve(source)) {
                        Some(ImageState::Ready(image)) if image.is_valid() => image,
                        Some(ImageState::Pending) => return Err(LayoutError::ImagePending),
                        _ => return Err(LayoutError::ImageFailed),
                    };
                    match (width, height) {
                        (Some(w), None) => {
                            height = Some(w * image.height as f32 / image.width as f32)
                        }
                        (None, Some(h)) => {
                            width = Some(h * image.width as f32 / image.height as f32)
                        }
                        _ => {
                            width = Some(image.width as f32);
                            height = Some(image.height as f32);
                        }
                    }
                }
            }
            return Ok((
                constrained_width(style, width.unwrap_or(0.0)) + edges,
                height.unwrap_or(0.0) + vertical_edges,
            ));
        }
        let (mut width, mut height, mut inline_width, mut inline_height) =
            (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        let mut child = self
            .document
            .first_child(id)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(node) = child {
            let child_kind = self
                .document
                .kind(node)
                .map_err(|_| LayoutError::InvalidTree)?;
            let child_style = if matches!(child_kind, NodeKind::Element { .. }) {
                css::compute_node(self.document, node, Some(style), self.rules)
                    .map_err(LayoutError::Css)?
            } else {
                style.clone()
            };
            let (w, h) = self.intrinsic_size_mode(node, &child_style, depth + 1, minimum)?;
            if matches!(child_kind, NodeKind::Text(_)) || child_style.display == Display::Inline {
                inline_width += w;
                inline_height = inline_height.max(h);
            } else {
                width = width.max(inline_width).max(w);
                height += inline_height + h;
                inline_width = 0.0;
                inline_height = 0.0;
            }
            child = self
                .document
                .next_sibling(node)
                .map_err(|_| LayoutError::InvalidTree)?;
        }
        Ok((
            constrained_width(
                style,
                style
                    .width
                    .map(|value| content_dimension(style, value))
                    .unwrap_or(width.max(inline_width)),
            ) + edges,
            style
                .height
                .map(|value| content_height_dimension(style, value))
                .unwrap_or(height + inline_height)
                + vertical_edges,
        ))
    }

    fn grid_children(
        &mut self,
        parent: NodeId,
        style: &Style,
        x: f32,
        y: f32,
        width: f32,
        depth: usize,
    ) -> Result<(f32, f32), LayoutError> {
        use css::{GridTrack, MAX_GRID_TRACKS};
        struct Item {
            node: NodeId,
            style: Style,
            col: usize,
            row: usize,
            cols: usize,
            rows: usize,
            natural: (f32, f32),
            minimum: f32,
            commands: core::ops::Range<usize>,
            hits: core::ops::Range<usize>,
        }
        let mut items = Vec::new();
        let column_tracks = style.grid_columns.as_deref().unwrap_or(&[]);
        let row_tracks = style.grid_rows.as_deref().unwrap_or(&[]);
        let mut columns = column_tracks.len().max(1);
        let mut rows = row_tracks.len().max(1);
        if let Some(areas) = &style.grid_areas {
            for area in areas.iter() {
                columns = columns.max(area.column + area.columns);
                rows = rows.max(area.row + area.rows);
            }
        }
        let mut child = self
            .document
            .first_child(parent)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(node) = child {
            let kind = self
                .document
                .kind(node)
                .map_err(|_| LayoutError::InvalidTree)?;
            if matches!(kind, NodeKind::Element { .. }) {
                let mut computed = css::compute_node(self.document, node, Some(style), self.rules)
                    .map_err(LayoutError::Css)?;
                if computed.display != Display::None {
                    if matches!(computed.position, Position::Absolute | Position::Fixed) {
                        self.box_for(node, style, Some(computed), x, y, width, depth + 1)?;
                        child = self
                            .document
                            .next_sibling(node)
                            .map_err(|_| LayoutError::InvalidTree)?;
                        continue;
                    }
                    if let Some(area) = computed.grid_area.as_ref().and_then(|name| {
                        style
                            .grid_areas
                            .as_ref()?
                            .iter()
                            .find(|area| area.name == *name)
                    }) {
                        computed.grid_column = css::GridPlacement {
                            start: Some(area.column),
                            span: area.columns,
                        };
                        computed.grid_row = css::GridPlacement {
                            start: Some(area.row),
                            span: area.rows,
                        };
                    }
                    computed.grid_column = css::resolve_grid_placement(
                        computed.grid_column,
                        computed.grid_column_spec.as_deref(),
                        style.grid_column_names.as_deref().unwrap_or(&[]),
                        column_tracks.len(),
                        style.grid_areas.as_deref().unwrap_or(&[]),
                        true,
                    )
                    .ok_or(LayoutError::GridLimit)?;
                    computed.grid_row = css::resolve_grid_placement(
                        computed.grid_row,
                        computed.grid_row_spec.as_deref(),
                        style.grid_row_names.as_deref().unwrap_or(&[]),
                        row_tracks.len(),
                        style.grid_areas.as_deref().unwrap_or(&[]),
                        false,
                    )
                    .ok_or(LayoutError::GridLimit)?;
                    if items.len() == MAX_GRID_TRACKS * MAX_GRID_TRACKS {
                        return Err(LayoutError::GridLimit);
                    }
                    columns = columns
                        .max(computed.grid_column.start.unwrap_or(0) + computed.grid_column.span);
                    rows = rows.max(computed.grid_row.start.unwrap_or(0) + computed.grid_row.span);
                    if columns > MAX_GRID_TRACKS || rows > MAX_GRID_TRACKS {
                        return Err(LayoutError::GridLimit);
                    }
                    let sizing_style = computed.resolve_percentages(width, self.parent_height);
                    let natural = self.intrinsic_size(node, &sizing_style, depth + 1)?;
                    let minimum = self
                        .intrinsic_size_mode(node, &sizing_style, depth + 1, true)?
                        .0;
                    items.push(Item {
                        node,
                        style: computed,
                        col: 0,
                        row: 0,
                        cols: 1,
                        rows: 1,
                        natural,
                        minimum,
                        commands: 0..0,
                        hits: 0..0,
                    });
                }
            }
            child = self
                .document
                .next_sibling(node)
                .map_err(|_| LayoutError::InvalidTree)?;
        }
        items.sort_by_key(|item| item.style.order);
        let mut occupied = [0u64; MAX_GRID_TRACKS];
        let mut cursor = 0;
        let column_flow = style.grid_auto_flow.column;
        let flow_rows = rows;
        let flow_columns = columns;
        // Reserve definite areas, then major-axis locked items, then remaining auto items.
        for phase in 0..3 {
            for item in &mut items {
                let cp = item.style.grid_column;
                let rp = item.style.grid_row;
                let definite = cp.start.is_some() && rp.start.is_some();
                let locked = if column_flow {
                    cp.start.is_some()
                } else {
                    rp.start.is_some()
                };
                if (if definite {
                    0
                } else if locked {
                    1
                } else {
                    2
                }) != phase
                {
                    continue;
                }
                item.cols = cp.span;
                item.rows = rp.span;
                let mut found = None;
                for index in if phase < 2 || style.grid_auto_flow.dense {
                    0
                } else {
                    cursor
                }
                    ..MAX_GRID_TRACKS * if column_flow { flow_rows } else { flow_columns }
                {
                    let mut col = cp.start.unwrap_or(if column_flow {
                        index / flow_rows
                    } else {
                        index % flow_columns
                    });
                    let mut row = rp.start.unwrap_or(if column_flow {
                        index % flow_rows
                    } else {
                        index / flow_columns
                    });
                    if phase == 2 && !style.grid_auto_flow.dense {
                        if !column_flow
                            && cp.start.is_some_and(|start| start < index % flow_columns)
                        {
                            row += 1;
                        }
                        if column_flow && rp.start.is_some_and(|start| start < index % flow_rows) {
                            col += 1;
                        }
                    }
                    if col + item.cols
                        > if column_flow {
                            MAX_GRID_TRACKS
                        } else {
                            flow_columns
                        }
                        || row + item.rows
                            > if column_flow {
                                flow_rows
                            } else {
                                MAX_GRID_TRACKS
                            }
                    {
                        continue;
                    }
                    let mask = (u64::MAX >> (64 - item.cols)) << col;
                    if definite || occupied[row..row + item.rows].iter().all(|v| v & mask == 0) {
                        found = Some((col, row, mask));
                        break;
                    }
                }
                let (col, row, mask) = found.ok_or(LayoutError::GridLimit)?;
                for value in &mut occupied[row..row + item.rows] {
                    *value |= mask;
                }
                item.col = col;
                item.row = row;
                rows = rows.max(row + item.rows);
                columns = columns.max(col + item.cols);
                if phase == 2 {
                    cursor = if column_flow {
                        col * flow_rows + row + item.rows
                    } else {
                        row * flow_columns + col + item.cols
                    };
                }
            }
        }
        let mut column_definitions = [GridTrack::Auto; MAX_GRID_TRACKS];
        let mut row_definitions = [GridTrack::Auto; MAX_GRID_TRACKS];
        for (definitions, explicit, implicit, count) in [
            (
                &mut column_definitions,
                column_tracks,
                style.grid_auto_columns.as_deref().unwrap_or(&[]),
                columns,
            ),
            (
                &mut row_definitions,
                row_tracks,
                style.grid_auto_rows.as_deref().unwrap_or(&[]),
                rows,
            ),
        ] {
            for i in 0..count {
                definitions[i] = explicit.get(i).copied().unwrap_or_else(|| {
                    if implicit.is_empty() {
                        GridTrack::Auto
                    } else {
                        implicit[(i - explicit.len()) % implicit.len()]
                    }
                });
            }
        }
        let gap = style.gap;
        let tracks = |items: &[Item],
                      definitions: &[GridTrack],
                      count: usize,
                      available: Option<f32>,
                      horizontal: bool| {
            grid_track_sizes(
                definitions,
                count,
                available,
                gap,
                if horizontal {
                    style.justify_content == JustifyContent::Stretch
                } else {
                    style.align_content.is_none()
                },
                items.iter().map(|item| {
                    if horizontal {
                        (item.col, item.cols, item.minimum, item.natural.0)
                    } else {
                        (item.row, item.rows, item.natural.1, item.natural.1)
                    }
                }),
            )
        };
        let widths = tracks(&items, &column_definitions, columns, Some(width), true);
        let distribute = |count: usize, free: f32, alignment: JustifyContent| match alignment {
            JustifyContent::End => (free, 0.0),
            JustifyContent::Center => (free * 0.5, 0.0),
            JustifyContent::SpaceBetween if count > 1 => (0.0, free / (count - 1) as f32),
            JustifyContent::SpaceAround if count > 0 => {
                (free / count as f32 * 0.5, free / count as f32)
            }
            JustifyContent::SpaceEvenly => (free / (count + 1) as f32, free / (count + 1) as f32),
            _ => (0.0, 0.0),
        };
        let (column_offset, column_extra) = distribute(
            columns,
            (width
                - widths[..columns].iter().sum::<f32>()
                - gap * columns.saturating_sub(1) as f32)
                .max(0.0),
            style.justify_content,
        );
        let preliminary_rows = tracks(
            &items,
            &row_definitions,
            rows,
            style.height.map(|v| content_height_dimension(style, v)),
            false,
        );
        // Lay out once at the final column width; retain commands and move them after row sizing.
        for item in &mut items {
            let mut cx = x
                + column_offset
                + widths[..item.col].iter().sum::<f32>()
                + (gap + column_extra) * item.col as f32;
            let cw = widths[item.col..item.col + item.cols].iter().sum::<f32>()
                + (gap + column_extra) * (item.cols - 1) as f32;
            if style.direction == css::Direction::Rtl {
                cx = x + width - (cx - x) - cw;
            }
            let definite_height = style.height.is_some()
                || row_definitions[item.row..item.row + item.rows]
                    .iter()
                    .all(|track| matches!(track, GridTrack::Pixels(_)));
            let area_height = preliminary_rows[item.row..item.row + item.rows]
                .iter()
                .sum::<f32>()
                + gap * (item.rows - 1) as f32;
            let mut computed = item
                .style
                .resolve_percentages(cw, definite_height.then_some(area_height));
            if computed.grid_columns_subgrid || computed.grid_rows_subgrid {
                let subgap = if computed.gap_specified {
                    computed.gap
                } else {
                    gap
                };
                let difference = subgap - gap;
                let border = if computed.border_solid {
                    computed.border_width
                } else {
                    0.0
                };
                if computed.grid_columns_subgrid {
                    let mut shared: Vec<_> = widths[item.col..item.col + item.cols]
                        .iter()
                        .copied()
                        .collect();
                    for (index, size) in shared.iter_mut().enumerate() {
                        let start = if index == 0 {
                            computed.padding_sides[3] + border
                        } else {
                            difference * 0.5
                        };
                        let end = if index + 1 == item.cols {
                            computed.padding_sides[1] + border
                        } else {
                            difference * 0.5
                        };
                        *size = (*size - start - end).max(0.0);
                    }
                    computed.grid_columns = Some(
                        shared
                            .into_iter()
                            .map(GridTrack::Pixels)
                            .collect::<Vec<_>>()
                            .into(),
                    );
                    computed.grid_column_names = style.grid_column_names.as_ref().map(|names| {
                        names
                            .iter()
                            .filter(|line| {
                                line.line >= item.col && line.line <= item.col + item.cols
                            })
                            .map(|line| css::GridNamedLine {
                                name: line.name.clone(),
                                line: line.line - item.col,
                            })
                            .collect::<Vec<_>>()
                            .into()
                    });
                }
                if computed.grid_rows_subgrid {
                    let mut shared: Vec<_> = preliminary_rows[item.row..item.row + item.rows]
                        .iter()
                        .copied()
                        .collect();
                    for (index, size) in shared.iter_mut().enumerate() {
                        let start = if index == 0 {
                            computed.padding_sides[0] + border
                        } else {
                            difference * 0.5
                        };
                        let end = if index + 1 == item.rows {
                            computed.padding_sides[2] + border
                        } else {
                            difference * 0.5
                        };
                        *size = (*size - start - end).max(0.0);
                    }
                    computed.grid_rows = Some(
                        shared
                            .into_iter()
                            .map(GridTrack::Pixels)
                            .collect::<Vec<_>>()
                            .into(),
                    );
                    computed.grid_row_names = style.grid_row_names.as_ref().map(|names| {
                        names
                            .iter()
                            .filter(|line| {
                                line.line >= item.row && line.line <= item.row + item.rows
                            })
                            .map(|line| css::GridNamedLine {
                                name: line.name.clone(),
                                line: line.line - item.row,
                            })
                            .collect::<Vec<_>>()
                            .into()
                    });
                    if computed.height.is_none() {
                        computed.height = Some(specified_height(
                            &computed,
                            (area_height - box_edges(&computed, false)).max(0.0),
                        ));
                    }
                }
                computed.gap = subgap;
            }
            if computed.display == Display::Inline {
                computed.display = Display::Block;
            }
            let margin = computed.margin_sides[1] + computed.margin_sides[3];
            let alignment = computed.justify_self.unwrap_or(style.justify_items);
            if computed.width.is_none() {
                computed.width = Some(specified_dimension(
                    &computed,
                    ((if alignment == AlignItems::Stretch {
                        cw
                    } else {
                        item.natural.0.min(cw).max(item.minimum)
                    }) - margin
                        - box_edges(&computed, true))
                    .max(0.0),
                ));
            }
            let free = (cw
                - margin
                - content_dimension(&computed, computed.width.unwrap_or(0.0))
                - box_edges(&computed, true))
            .max(0.0);
            cx += if computed.margin_auto[3] {
                if computed.margin_auto[1] {
                    free * 0.5
                } else {
                    free
                }
            } else if computed.margin_auto[1] {
                0.0
            } else {
                match alignment {
                    AlignItems::End => {
                        if style.direction == css::Direction::Rtl {
                            0.0
                        } else {
                            free
                        }
                    }
                    AlignItems::Start | AlignItems::Stretch => {
                        if style.direction == css::Direction::Rtl {
                            free
                        } else {
                            0.0
                        }
                    }
                    AlignItems::Center => free * 0.5,
                }
            };
            item.commands.start = self.commands.len();
            item.hits.start = self.geometry.as_ref().map_or(0, |g| g.hits.len());
            item.natural.1 =
                self.box_for(item.node, style, Some(computed), cx, y, cw, depth + 1)?;
            item.commands.end = self.commands.len();
            item.hits.end = self.geometry.as_ref().map_or(0, |g| g.hits.len());
        }
        let heights = tracks(
            &items,
            &row_definitions,
            rows,
            style.height.map(|v| content_height_dimension(style, v)),
            false,
        );
        let used_height = heights[..rows].iter().sum::<f32>() + gap * rows.saturating_sub(1) as f32;
        let (row_offset, row_extra) = distribute(
            rows,
            (style
                .height
                .map_or(used_height, |v| content_height_dimension(style, v))
                - used_height)
                .max(0.0),
            style.align_content.unwrap_or(JustifyContent::Stretch),
        );
        for item in &items {
            let ch = heights[item.row..item.row + item.rows].iter().sum::<f32>()
                + (gap + row_extra) * (item.rows - 1) as f32;
            let free = (ch - item.natural.1).max(0.0);
            let alignment = item.style.align_self.unwrap_or(style.align_items);
            let align = if item.style.margin_auto[0] {
                if item.style.margin_auto[2] {
                    free * 0.5
                } else {
                    free
                }
            } else if item.style.margin_auto[2] {
                0.0
            } else {
                match alignment {
                    AlignItems::End => free,
                    AlignItems::Center => free * 0.5,
                    _ => 0.0,
                }
            };
            let dy = row_offset
                + heights[..item.row].iter().sum::<f32>()
                + (gap + row_extra) * item.row as f32
                + align;
            if item.style.height.is_none()
                && alignment == AlignItems::Stretch
                && !item.style.margin_auto[0]
                && !item.style.margin_auto[2]
            {
                let height =
                    (ch - item.style.margin_sides[0] - item.style.margin_sides[2]).max(0.0);
                let paints = if has_background(&item.style) {
                    background_paints(&item.style)
                } else {
                    0
                } + usize::from(
                    item.style.border_solid
                        && item.style.border_width > 0.0
                        && item.style.border_color.a != 0,
                );
                let mut remaining = paints;
                for command in self.commands[item.commands.clone()].iter_mut().take(
                    paints
                        + usize::from(item.style.opacity < 1.0)
                        + usize::from(item.style.transforms.is_some()),
                ) {
                    match command {
                        Command::PushLayer { .. } | Command::PushTransform(_) => {}
                        Command::FillRect { rect, .. }
                        | Command::StrokeBorder { rect, .. }
                        | Command::StrokePatternBorder { rect, .. }
                            if remaining > 0 =>
                        {
                            rect.height = height;
                            remaining -= 1;
                        }
                        Command::FillRoundedRect { rect, .. } if remaining > 0 => {
                            let inset = background_bleed_inset(&item.style);
                            let outer = Rect {
                                x: rect.x - inset,
                                y: rect.y - inset,
                                width: rect.width + 2.0 * inset,
                                height,
                            };
                            *command = background_color(outer, &item.style);
                            remaining -= 1;
                        }
                        Command::FillGradient { rect, .. } if remaining > 0 => {
                            rect.height = (height
                                - if item.style.border_solid {
                                    2.0 * item.style.border_width
                                } else {
                                    0.0
                                })
                            .max(0.0);
                            remaining -= 1;
                        }
                        Command::BoxShadow { rect, shadow, .. } if remaining > 0 => {
                            rect.height = (height
                                - if shadow.inset && item.style.border_solid {
                                    2.0 * item.style.border_width
                                } else {
                                    0.0
                                })
                            .max(0.0);
                            remaining -= 1;
                        }
                        _ => break,
                    }
                }
                if let Some(geometry) = self.geometry.as_mut() {
                    if let Some(hit) = geometry.hits.get_mut(item.hits.start) {
                        if hit.node == item.node {
                            hit.rect.height = height;
                        }
                    }
                }
            }
            self.refresh_transform(&item.style, item.commands.clone(), item.hits.clone())?;
            for command in &mut self.commands[item.commands.clone()] {
                move_command(command, 0.0, dy);
            }
            if let Some(geometry) = self.geometry.as_mut() {
                geometry.move_hits(item.hits.clone(), 0.0, dy);
            }
        }
        Ok((
            widths[..columns].iter().sum::<f32>() + gap * columns.saturating_sub(1) as f32,
            heights[..rows].iter().sum::<f32>() + gap * rows.saturating_sub(1) as f32,
        ))
    }

    fn flex_children(
        &mut self,
        parent: NodeId,
        style: &Style,
        x: f32,
        y: f32,
        width: f32,
        depth: usize,
    ) -> Result<(f32, f32), LayoutError> {
        struct Item {
            node: NodeId,
            style: Style,
            main: f32,
            source_order: usize,
            cross: f32,
            edges: f32,
            cross_edges: f32,
            frozen: bool,
            first_command: usize,
            last_command: usize,
            first_hit: usize,
            last_hit: usize,
        }
        let row = matches!(
            style.flex_direction,
            FlexDirection::Row | FlexDirection::RowReverse
        );
        let reverse = matches!(
            style.flex_direction,
            FlexDirection::RowReverse | FlexDirection::ColumnReverse
        ) ^ (row && style.direction == Direction::Rtl);
        let cross_reverse = style.flex_wrap_reverse ^ (!row && style.direction == Direction::Rtl);
        let mut items = Vec::new();
        let mut positioned = Vec::new();
        let mut child = self
            .document
            .first_child(parent)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(node) = child {
            let kind = self
                .document
                .kind(node)
                .map_err(|_| LayoutError::InvalidTree)?;
            let mut item_style = if matches!(kind, NodeKind::Element { .. }) {
                css::compute_node(self.document, node, Some(style), self.rules)
                    .map_err(LayoutError::Css)?
            } else {
                style.clone()
            };
            let basis = item_style.resolve_flex_basis_percentage(if row {
                Some(width)
            } else {
                self.parent_height
            });
            item_style = item_style.resolve_percentages(width, self.parent_height);
            item_style.flex_basis = basis;
            if !matches!(kind, NodeKind::Text(value) if collapsed_text(value).trim_matches(' ').is_empty())
                && (matches!(kind, NodeKind::Text(_))
                    || matches!(kind, NodeKind::Element { name, namespace: Namespace::Html, .. }
                    if !matches!(name.as_str(), "head" | "style" | "script" | "meta" | "link" | "template")))
                && item_style.display != Display::None
            {
                let (w, h) = self.intrinsic_size(node, &item_style, depth + 1)?;
                if matches!(item_style.position, Position::Absolute | Position::Fixed) {
                    if positioned.len() >= MAX_DISPLAY_COMMANDS {
                        return Err(LayoutError::CommandLimit);
                    }
                    positioned
                        .try_reserve(1)
                        .map_err(|_| LayoutError::CommandLimit)?;
                    positioned.push((node, item_style, w, h));
                    child = self
                        .document
                        .next_sibling(node)
                        .map_err(|_| LayoutError::InvalidTree)?;
                    continue;
                }
                let edges = box_edges(&item_style, row);
                let cross_edges = box_edges(&item_style, !row);
                let main = item_style
                    .flex_basis
                    .map_or(if row { w } else { h }, |basis| {
                        if row {
                            content_dimension(&item_style, basis) + edges
                        } else {
                            content_height_dimension(&item_style, basis) + edges
                        }
                    });
                let main = if row {
                    constrained_width(&item_style, (main - edges).max(0.0)) + edges
                } else {
                    constrained_height(&item_style, (main - edges).max(0.0)) + edges
                };
                if items.len() >= MAX_DISPLAY_COMMANDS {
                    return Err(LayoutError::CommandLimit);
                }
                items
                    .try_reserve(1)
                    .map_err(|_| LayoutError::CommandLimit)?;
                items.push(Item {
                    node,
                    style: item_style,
                    main,
                    source_order: items.len(),
                    cross: if row { h } else { w },
                    edges,
                    cross_edges,
                    frozen: false,
                    first_command: 0,
                    last_command: 0,
                    first_hit: 0,
                    last_hit: 0,
                });
            }
            child = self
                .document
                .next_sibling(node)
                .map_err(|_| LayoutError::InvalidTree)?;
        }
        for (node, item_style, w, h) in positioned {
            let main_extent = if row {
                width
            } else {
                style
                    .height
                    .map_or(0.0, |height| content_height_dimension(style, height))
            };
            let cross_extent = if row {
                style
                    .height
                    .map_or(0.0, |height| content_height_dimension(style, height))
            } else {
                width
            };
            let main_size = if row { w } else { h };
            let cross_size = if row { h } else { w };
            let free = main_extent - main_size;
            let main = match style.justify_content {
                JustifyContent::End => {
                    if reverse {
                        0.0
                    } else {
                        free
                    }
                }
                JustifyContent::Center
                | JustifyContent::SpaceAround
                | JustifyContent::SpaceEvenly => free * 0.5,
                _ => {
                    if reverse {
                        free
                    } else {
                        0.0
                    }
                }
            };
            let cross = match item_style.align_self.unwrap_or(style.align_items) {
                AlignItems::Center => (cross_extent - cross_size) * 0.5,
                AlignItems::End => cross_extent - cross_size,
                _ => 0.0,
            };
            self.box_for(
                node,
                style,
                Some(item_style),
                x + if row { main } else { cross },
                y + if row { cross } else { main },
                width,
                depth + 1,
            )?;
        }
        if items.is_empty() {
            return Ok((0.0, 0.0));
        }
        items.sort_unstable_by_key(|item| (item.style.order, item.source_order));
        let main_limit = if row {
            Some(width)
        } else {
            style
                .height
                .map(|height| content_height_dimension(style, height))
        };
        let mut line_start = 0;
        let mut lines = Vec::new();
        let mut natural_cross = 0.0f32;
        while line_start < items.len() {
            let mut line_end = line_start + 1;
            let mut line_main = items[line_start].main;
            while line_end < items.len() {
                let next = line_main + style.gap + items[line_end].main;
                if style.flex_wrap && main_limit.is_some_and(|limit| next > limit) {
                    break;
                }
                line_main = next;
                line_end += 1;
            }
            let cross = items[line_start..line_end]
                .iter()
                .map(|item| item.cross)
                .fold(0.0f32, f32::max);
            natural_cross += cross;
            lines
                .try_reserve(1)
                .map_err(|_| LayoutError::CommandLimit)?;
            lines.push((line_start, line_end, cross));
            line_start = line_end;
        }
        natural_cross += style.gap * lines.len().saturating_sub(1) as f32;
        let cross_limit = if row {
            style
                .height
                .map(|height| content_height_dimension(style, height))
        } else {
            Some(width)
        };
        let cross_free = cross_limit.map_or(0.0, |limit| (limit - natural_cross).max(0.0));
        let line_stretch = if style.align_content.is_none() {
            cross_free / lines.len() as f32
        } else {
            0.0
        };
        let count = lines.len() as f32;
        let (mut cross_offset, line_gap) = match style.align_content {
            Some(JustifyContent::End) => (cross_free, 0.0),
            Some(JustifyContent::Center) => (cross_free * 0.5, 0.0),
            Some(JustifyContent::SpaceBetween) if lines.len() > 1 => {
                (0.0, cross_free / (count - 1.0))
            }
            Some(JustifyContent::SpaceAround) => (cross_free / count * 0.5, cross_free / count),
            Some(JustifyContent::SpaceEvenly) => {
                (cross_free / (count + 1.0), cross_free / (count + 1.0))
            }
            _ => (0.0, 0.0),
        };
        let mut total_main = 0.0f32;
        for (line_start, line_end, line_cross) in lines {
            let items = &mut items[line_start..line_end];
            let line_offset = if cross_reverse {
                cross_limit.unwrap_or(natural_cross) - cross_offset - line_cross - line_stretch
            } else {
                cross_offset
            };
            let (x, y) = if row {
                (x, y + line_offset)
            } else {
                (x + line_offset, y)
            };
            let gaps = style.gap * items.len().saturating_sub(1) as f32;
            let basis: f32 = items.iter().map(|item| item.main).sum();
            let available = if row {
                width
            } else {
                style
                    .height
                    .map(|height| content_height_dimension(style, height))
                    .unwrap_or(basis + gaps)
            };
            // honey: at most one freezing pass per item; optimize if very large flex lines recur.
            loop {
                let free = available - items.iter().map(|item| item.main).sum::<f32>() - gaps;
                let factor: f32 = items
                    .iter()
                    .filter(|item| !item.frozen)
                    .map(|item| {
                        if free >= 0.0 {
                            item.style.flex_grow
                        } else {
                            item.style.flex_shrink * item.main
                        }
                    })
                    .sum();
                if factor <= 0.0 {
                    break;
                }
                let mut froze = false;
                for item in items.iter_mut() {
                    if item.frozen {
                        continue;
                    }
                    let weight = if free >= 0.0 {
                        item.style.flex_grow
                    } else {
                        item.style.flex_shrink * item.main
                    };
                    let proposed = (item.main
                        + free * weight / if free >= 0.0 { factor.max(1.0) } else { factor })
                    .max(item.edges);
                    item.main = if row {
                        constrained_width(&item.style, proposed - item.edges) + item.edges
                    } else {
                        constrained_height(&item.style, proposed - item.edges) + item.edges
                    };
                    if item.main != proposed {
                        item.frozen = true;
                        froze = true;
                    }
                }
                if !froze {
                    break;
                }
            }
            let remaining =
                (available - gaps - items.iter().map(|item| item.main).sum::<f32>()).max(0.0);
            let (main_start, main_end) = match (row, reverse) {
                (true, false) => (3, 1),
                (true, true) => (1, 3),
                (false, false) => (0, 2),
                (false, true) => (2, 0),
            };
            let auto_count = items
                .iter()
                .map(|item| {
                    usize::from(item.style.margin_auto[main_start])
                        + usize::from(item.style.margin_auto[main_end])
                })
                .sum::<usize>();
            let auto_margin = if auto_count == 0 {
                0.0
            } else {
                remaining / auto_count as f32
            };
            let remaining = if auto_count == 0 { remaining } else { 0.0 };
            let count = items.len() as f32;
            let (start, extra_gap) = match style.justify_content {
                JustifyContent::Start | JustifyContent::Stretch => (0.0, 0.0),
                JustifyContent::End => (remaining, 0.0),
                JustifyContent::Center => (remaining * 0.5, 0.0),
                JustifyContent::SpaceBetween if items.len() > 1 => (0.0, remaining / (count - 1.0)),
                JustifyContent::SpaceAround => (remaining / count * 0.5, remaining / count),
                JustifyContent::SpaceEvenly => {
                    (remaining / (count + 1.0), remaining / (count + 1.0))
                }
                _ => (0.0, 0.0),
            };
            let cross_extent = if style.flex_wrap {
                line_cross + line_stretch
            } else if row {
                style
                    .height
                    .map(|height| content_height_dimension(style, height))
                    .unwrap_or(items.iter().map(|item| item.cross).fold(0.0f32, f32::max))
            } else {
                width
            };
            let mut cursor = start;
            let mut actual_cross = 0.0f32;
            for item in items.iter_mut() {
                cursor += if item.style.margin_auto[main_start] {
                    auto_margin
                } else {
                    0.0
                };
                let main_pos = if reverse {
                    available - cursor - item.main
                } else {
                    cursor
                };
                let mut computed = item.style.clone();
                if row {
                    computed.width = Some(specified_dimension(
                        &computed,
                        (item.main - item.edges).max(0.0),
                    ));
                } else {
                    computed.height = Some(specified_height(
                        &computed,
                        (item.main - item.edges).max(0.0),
                    ));
                }
                if computed.display == Display::Inline {
                    computed.display = Display::Block;
                }
                let cross_auto = if row {
                    computed.margin_auto[0] || computed.margin_auto[2]
                } else {
                    computed.margin_auto[1] || computed.margin_auto[3]
                };
                if !cross_auto
                    && computed.align_self.unwrap_or(style.align_items) == AlignItems::Stretch
                {
                    if row
                        && (style.height.is_some() || style.flex_wrap)
                        && computed.height.is_none()
                    {
                        computed.height = Some(specified_height(
                            &computed,
                            (cross_extent - item.cross_edges).max(0.0),
                        ));
                    }
                    if !row && computed.width.is_none() {
                        computed.width = Some(specified_dimension(
                            &computed,
                            (cross_extent - item.cross_edges).max(0.0),
                        ));
                    }
                }
                item.first_command = self.commands.len();
                item.first_hit = self
                    .geometry
                    .as_ref()
                    .map_or(0, |geometry| geometry.hits.len());
                let (child_x, child_y) = if row {
                    (x + main_pos, y)
                } else {
                    (x, y + main_pos)
                };
                let cross = if let NodeKind::Text(value) = self
                    .document
                    .kind(item.node)
                    .map_err(|_| LayoutError::InvalidTree)?
                {
                    let (mut text_y, mut advance, mut height) = (child_y, 0.0, 0.0);
                    let mut first = self.commands.len();
                    let mut trailing = 0.0;
                    self.text_flow(
                        value,
                        style,
                        child_x,
                        if row { item.main } else { width },
                        &mut text_y,
                        &mut advance,
                        &mut height,
                        &[],
                        &mut first,
                        &mut trailing,
                    )?;
                    self.align_line(
                        first,
                        child_x,
                        if row { item.main } else { width },
                        advance - trailing,
                        style,
                        true,
                    );
                    if row {
                        text_y - child_y + height
                    } else {
                        advance
                    }
                } else {
                    let height = self.box_for(
                        item.node,
                        style,
                        Some(computed.clone()),
                        child_x,
                        child_y,
                        if row { item.main } else { width },
                        depth + 1,
                    )?;
                    if row {
                        height
                    } else {
                        computed
                            .width
                            .map(|value| {
                                constrained_width(&computed, content_dimension(&computed, value))
                            })
                            .unwrap_or_else(|| {
                                constrained_width(&computed, width - item.cross_edges)
                            })
                            .max(0.0)
                            + item.cross_edges
                    }
                };
                item.cross = cross;
                actual_cross = actual_cross.max(cross);
                item.last_command = self.commands.len();
                item.last_hit = self
                    .geometry
                    .as_ref()
                    .map_or(0, |geometry| geometry.hits.len());
                cursor += item.main
                    + style.gap
                    + extra_gap
                    + if item.style.margin_auto[main_end] {
                        auto_margin
                    } else {
                        0.0
                    };
            }
            let align_extent = if style.flex_wrap {
                actual_cross.max(cross_extent)
            } else if row {
                style
                    .height
                    .filter(|_| !style.flex_wrap)
                    .map(|height| content_height_dimension(style, height))
                    .unwrap_or(actual_cross)
            } else {
                width
            };
            for item in items.iter() {
                let align = item.style.align_self.unwrap_or(style.align_items);
                if row
                    && align == AlignItems::Stretch
                    && item.style.height.is_none()
                    && !item.style.margin_auto[0]
                    && !item.style.margin_auto[2]
                {
                    let root_height =
                        (align_extent - item.style.margin_sides[0] - item.style.margin_sides[2])
                            .max(0.0);
                    let paints = if has_background(&item.style) {
                        background_paints(&item.style)
                    } else {
                        0
                    } + usize::from(
                        item.style.border_solid
                            && item.style.border_width > 0.0
                            && item.style.border_color.a != 0,
                    );
                    for command in self.commands[item.first_command..item.last_command]
                        .iter_mut()
                        .take(
                            paints
                                + usize::from(item.style.opacity < 1.0)
                                + usize::from(item.style.transforms.is_some()),
                        )
                    {
                        match command {
                            Command::FillRect { rect, .. }
                            | Command::StrokeBorder { rect, .. }
                            | Command::StrokePatternBorder { rect, .. } => {
                                rect.height = root_height
                            }
                            Command::FillRoundedRect { rect, .. } => {
                                let inset = background_bleed_inset(&item.style);
                                let outer = Rect {
                                    x: rect.x - inset,
                                    y: rect.y - inset,
                                    width: rect.width + 2.0 * inset,
                                    height: root_height,
                                };
                                *command = background_color(outer, &item.style);
                            }
                            Command::FillGradient { rect, .. } => {
                                rect.height = (root_height
                                    - if item.style.border_solid {
                                        2.0 * item.style.border_width
                                    } else {
                                        0.0
                                    })
                                .max(0.0)
                            }
                            Command::BoxShadow { rect, shadow, .. } => {
                                rect.height = (root_height
                                    - if shadow.inset && item.style.border_solid {
                                        2.0 * item.style.border_width
                                    } else {
                                        0.0
                                    })
                                .max(0.0)
                            }
                            _ => {}
                        }
                    }
                    if let Some(geometry) = self.geometry.as_mut() {
                        if let Some(hit) = geometry.hits.get_mut(item.first_hit) {
                            if hit.node == item.node {
                                hit.rect.height = root_height;
                            }
                        }
                    }
                }
                self.refresh_transform(
                    &item.style,
                    item.first_command..item.last_command,
                    item.first_hit..item.last_hit,
                )?;
                let free = (align_extent - item.cross).max(0.0);
                let (cross_start, cross_end) = if row { (0, 2) } else { (3, 1) };
                let offset = if item.style.margin_auto[cross_start] {
                    if item.style.margin_auto[cross_end] {
                        free * 0.5
                    } else {
                        free
                    }
                } else if item.style.margin_auto[cross_end] {
                    0.0
                } else {
                    match align {
                        AlignItems::End => free,
                        AlignItems::Center => free * 0.5,
                        _ => 0.0,
                    }
                };
                let offset = if cross_reverse
                    && !item.style.margin_auto[cross_start]
                    && !item.style.margin_auto[cross_end]
                {
                    free - offset
                } else {
                    offset
                };
                let (dx, dy) = if row { (0.0, offset) } else { (offset, 0.0) };
                for command in &mut self.commands[item.first_command..item.last_command] {
                    move_command(command, dx, dy);
                }
                if let Some(geometry) = self.geometry.as_mut() {
                    geometry.move_hits(item.first_hit..item.last_hit, dx, dy);
                }
            }
            cross_offset += align_extent + style.gap + line_gap;
            total_main = total_main.max(available);
        }
        cross_offset = (cross_offset - style.gap - line_gap).max(0.0);
        if style.flex_wrap && cross_limit.is_some() {
            cross_offset = cross_limit.unwrap();
        }
        Ok(if row {
            (total_main, cross_offset)
        } else {
            (cross_offset, total_main)
        })
    }
    fn line_height(&self, style: &Style) -> f32 {
        match style.line_height {
            LineHeight::Normal => self.text.line_height(style.font_size),
            LineHeight::Number(value) => value * style.font_size,
            LineHeight::Pixels(value) => value,
        }
    }

    fn baseline(&self, style: &Style) -> f32 {
        self.text.ascent(style.font_size)
            + (self.line_height(style) - self.text.line_height(style.font_size)) * 0.5
    }
    fn begin_hit(&mut self, node: NodeId) -> Result<Option<usize>, LayoutError> {
        let Some(geometry) = self.geometry.as_mut() else {
            return Ok(None);
        };
        let hits = &mut geometry.hits;
        if hits.len() >= MAX_DISPLAY_COMMANDS {
            return Err(LayoutError::CommandLimit);
        }
        hits.try_reserve(1).map_err(|_| LayoutError::CommandLimit)?;
        let index = hits.len();
        hits.push(HitRegion {
            node,
            rect: Rect {
                x: 0.0,
                y: 0.0,
                width: 0.0,
                height: 0.0,
            },
        });
        Ok(Some(index))
    }

    fn finish_hit(&mut self, index: Option<usize>, rect: Rect) {
        if let (Some(geometry), Some(index)) = (self.geometry.as_mut(), index) {
            geometry.hits[index].rect = rect;
        }
    }
    fn push_command(&mut self, command: Command) -> Result<(), LayoutError> {
        if self.commands.len() >= MAX_DISPLAY_COMMANDS {
            return Err(LayoutError::CommandLimit);
        }
        self.commands
            .try_reserve(1)
            .map_err(|_| LayoutError::CommandLimit)?;
        self.commands.push(command);
        Ok(())
    }

    fn text_flow(
        &mut self,
        value: &str,
        style: &Style,
        left: f32,
        width: f32,
        cursor: &mut f32,
        advance: &mut f32,
        line_height: &mut f32,
        floats: &[(Rect, Float)],
        first_command: &mut usize,
        trailing: &mut f32,
    ) -> Result<(), LayoutError> {
        let collapse = matches!(
            style.white_space,
            WhiteSpace::Normal | WhiteSpace::NoWrap | WhiteSpace::PreLine
        );
        let wrap = !matches!(style.white_space, WhiteSpace::NoWrap | WhiteSpace::Pre);
        let normalized = formatted_text(value, style.white_space);
        let value = if (*advance == 0.0 || *trailing > 0.0) && collapse {
            normalized.trim_start_matches(' ')
        } else {
            &normalized
        };
        if value.is_empty() {
            return Ok(());
        }
        let size = style.font_size;
        let height = self.line_height(style);
        if *cursor >= self.viewport.height || left >= self.viewport.width {
            *line_height = (*line_height).max(height);
            return Ok(());
        }
        if floats.is_empty()
            && style.text_align != TextAlign::Justify
            && !(*advance == 0.0 && value.starts_with(' '))
            && !value
                .bytes()
                .any(|byte| matches!(byte, b'\n' | b'\r' | b'\t'))
        {
            let shaped = self
                .text
                .shape_directional(value, size, style.direction == Direction::Rtl)
                .map_err(|_| LayoutError::Text)?;
            if !wrap || *advance + shaped.width <= width {
                let tail = if collapse {
                    value.len() - value.trim_end_matches(' ').len()
                } else {
                    0
                };
                *trailing = if tail == 0 {
                    0.0
                } else {
                    self.text
                        .measure(&value[value.len() - tail..], size)
                        .map_err(|_| LayoutError::Text)?
                };
                if left + *advance < self.viewport.width && *cursor < self.viewport.height {
                    self.push_command(Command::GlyphRun {
                        origin_x: left + *advance,
                        baseline_y: *cursor + self.baseline(style),
                        size,
                        color: style.color,
                        glyphs: shaped.glyphs,
                    })?;
                    self.decorate_text(
                        left + *advance,
                        *cursor + self.baseline(style),
                        shaped.width,
                        style,
                    )?;
                }
                *advance += shaped.width;
                *line_height = (*line_height).max(height);
                return Ok(());
            }
        }
        let chunks = lumen_common::ucd::line_breaks(value).scan(0, |start, (end, _)| {
            let part = &value[*start..end];
            *start = end;
            Some(part)
        });
        for part in chunks
            .flat_map(|part| part.split_inclusive(' '))
            .flat_map(|part| part.split_inclusive('\t'))
        {
            if collapse && part.trim_matches(' ').is_empty() && *advance == 0.0 {
                continue;
            }
            let newline = part.ends_with('\n');
            let tab = part.ends_with('\t');
            let part = part.trim_end_matches(['\n', '\t']);
            let shaped = self
                .text
                .shape_directional(part, size, style.direction == Direction::Rtl)
                .map_err(|_| LayoutError::Text)?;
            let (_, space) = float_edges(floats, left, width, *cursor, height);
            let hang = collapse || style.white_space == WhiteSpace::PreWrap;
            let visible_width = if hang && part.ends_with(' ') {
                self.text
                    .measure(part.trim_end_matches(' '), size)
                    .map_err(|_| LayoutError::Text)?
            } else {
                shaped.width
            };
            if wrap && visible_width > 0.0 && *advance > 0.0 && *advance + visible_width > space {
                self.align_line(
                    *first_command,
                    left,
                    space,
                    *advance - *trailing,
                    style,
                    false,
                );
                *cursor += *line_height;
                *advance = 0.0;
                *line_height = 0.0;
                *first_command = self.commands.len();
                *trailing = 0.0;
                if collapse && part.trim_matches(' ').is_empty() {
                    continue;
                }
            }
            let (mut line_left, mut space) = float_edges(floats, left, width, *cursor, height);
            while wrap && *advance == 0.0 && space < visible_width && !floats.is_empty() {
                let next = floats
                    .iter()
                    .map(|(rect, _)| rect.y + rect.height)
                    .filter(|end| *end > *cursor)
                    .min_by(f32::total_cmp);
                let Some(next) = next else {
                    break;
                };
                *cursor = next;
                (line_left, space) = float_edges(floats, left, width, *cursor, height);
            }
            if !part.is_empty()
                && line_left + *advance < self.viewport.width
                && *cursor < self.viewport.height
            {
                self.push_command(Command::GlyphRun {
                    origin_x: line_left + *advance,
                    baseline_y: *cursor + self.baseline(style),
                    size,
                    color: style.color,
                    glyphs: shaped.glyphs,
                })?;
                self.decorate_text(
                    line_left + *advance,
                    *cursor + self.baseline(style),
                    visible_width,
                    style,
                )?;
            }
            *advance += shaped.width;
            if tab {
                let tab_width = (self
                    .text
                    .measure(" ", size)
                    .map_err(|_| LayoutError::Text)?
                    * 8.0)
                    .max(1.0);
                *advance = ((*advance / tab_width).floor() + 1.0) * tab_width;
            }
            *trailing = if hang {
                shaped.width - visible_width
            } else {
                0.0
            };
            *line_height = (*line_height).max(height);
            if newline {
                self.align_line(
                    *first_command,
                    line_left,
                    space,
                    *advance - *trailing,
                    style,
                    true,
                );
                *cursor += *line_height;
                *advance = 0.0;
                *line_height = 0.0;
                *trailing = 0.0;
                *first_command = self.commands.len();
            }
        }
        Ok(())
    }

    fn align_line(
        &mut self,
        first: usize,
        _left: f32,
        available: f32,
        used: f32,
        style: &Style,
        last: bool,
    ) {
        self.align_line_until(first, self.commands.len(), available, used, style, last);
    }

    fn align_line_until(
        &mut self,
        first: usize,
        end: usize,
        available: f32,
        used: f32,
        style: &Style,
        last: bool,
    ) {
        if used <= 0.0 {
            return;
        }
        let free = (available - used).max(0.0);
        let right = match style.text_align {
            TextAlign::Right => true,
            TextAlign::Start => style.direction == Direction::Rtl,
            TextAlign::End => style.direction == Direction::Ltr,
            TextAlign::Justify => last && style.direction == Direction::Rtl,
            _ => false,
        };
        let offset = if right {
            free
        } else if style.text_align == TextAlign::Center {
            free * 0.5
        } else {
            0.0
        };
        let count = if style.text_align == TextAlign::Justify && !last {
            self.commands[first..end]
                .iter()
                .filter(|c| matches!(c, Command::GlyphRun { .. }))
                .count()
                .saturating_sub(1)
        } else {
            0
        };
        let mut word = 0usize;
        let mut seen = false;
        let line_y = self.commands[first..end]
            .iter()
            .find_map(|command| match command {
                Command::GlyphRun { baseline_y, .. } => Some(*baseline_y - self.baseline(style)),
                _ => None,
            });
        for command in &mut self.commands[first..end] {
            if matches!(command, Command::GlyphRun { .. }) {
                if seen {
                    word += 1;
                }
                seen = true;
            }
            move_command(
                command,
                offset
                    + if count != 0 {
                        free * word.min(count) as f32 / count as f32
                    } else {
                        0.0
                    },
                0.0,
            );
        }
        let height = self.line_height(style);
        if count == 0 && offset != 0.0 {
            if let (Some(geometry), Some(y)) = (&mut self.geometry, line_y) {
                for hit in &mut geometry.hits {
                    if hit.rect.y >= y && hit.rect.y + hit.rect.height <= y + height {
                        hit.rect.x += offset;
                    }
                }
            }
        }
    }

    fn inline_for(
        &mut self,
        id: NodeId,
        parent_style: &Style,
        computed: Option<Style>,
        x: f32,
        y: f32,
        available: f32,
        depth: usize,
    ) -> Result<(f32, f32), LayoutError> {
        let computed = self
            .opacity_style(id, parent_style, computed)?
            .map(|style| style.resolve_percentages(available, self.parent_height));
        if computed
            .as_ref()
            .is_some_and(|style| matches!(style.position, Position::Absolute | Position::Fixed))
        {
            self.box_for(id, parent_style, computed, x, y, available, depth)?;
            return Ok((0.0, 0.0));
        }
        if matches!(self.document.kind(id), Ok(NodeKind::Element { name, .. }) if name == "img") {
            let first_hit = self
                .geometry
                .as_ref()
                .map_or(0, |geometry| geometry.hits.len());
            let first_command = self.commands.len();
            let margins = computed
                .as_ref()
                .map_or([0.0; 4], |style| style.margin_sides);
            let edges = computed
                .as_ref()
                .map_or(0.0, |style| box_edges(style, true));
            let height = self.box_for(id, parent_style, computed, x, y, available, depth)?;
            let width = self
                .geometry
                .as_ref()
                .and_then(|geometry| geometry.hits.get(first_hit))
                .map(|hit| hit.rect.width)
                .or_else(|| {
                    self.commands[first_command..]
                        .iter()
                        .find_map(|command| match command {
                            Command::Image { rect, .. } => Some(rect.width + edges),
                            _ => None,
                        })
                })
                .unwrap_or(0.0);
            return Ok((width + margins[1] + margins[3], height));
        }
        let decorations = self.decorations.len();
        if let Some(style) = &computed {
            self.begin_decoration(style)?;
        }
        let offset = computed
            .as_ref()
            .filter(|style| {
                style.position == Position::Relative && style.display == Display::Inline
            })
            .map_or((0.0, 0.0), |style| {
                (
                    style.left.unwrap_or_else(|| -style.right.unwrap_or(0.0)),
                    style.top.unwrap_or_else(|| -style.bottom.unwrap_or(0.0)),
                )
            });
        let layer = self.begin_opacity(computed.as_ref().filter(|style| {
            !matches!(
                style.display,
                Display::Block | Display::Flex | Display::Grid
            )
        }))?;
        let result = self.inline_content(
            id,
            parent_style,
            computed,
            x + offset.0,
            y + offset.1,
            available,
            depth,
        );
        self.decorations.truncate(decorations);
        let result = result?;
        self.end_opacity(layer)?;
        Ok(result)
    }

    fn inline_content(
        &mut self,
        id: NodeId,
        parent_style: &Style,
        computed: Option<Style>,
        x: f32,
        y: f32,
        available: f32,
        depth: usize,
    ) -> Result<(f32, f32), LayoutError> {
        if depth > 512 {
            return Err(LayoutError::DepthLimit);
        }
        let kind = self
            .document
            .kind(id)
            .map_err(|_| LayoutError::InvalidTree)?;
        if let NodeKind::Text(value) = kind {
            let normalized = formatted_text(value, parent_style.white_space);
            let value = normalized.as_ref();
            if value.is_empty() {
                return Ok((0.0, 0.0));
            }
            let size = parent_style.font_size;
            let height = self.line_height(parent_style);
            if self.transform_depth == 0 && (x >= self.viewport.width || y >= self.viewport.height)
            {
                return Ok((0.0, height));
            }
            let shaped = self
                .text
                .shape_directional(value, size, parent_style.direction == Direction::Rtl)
                .map_err(|_| LayoutError::Text)?;
            let width = shaped.width;
            self.push_command(Command::GlyphRun {
                origin_x: x,
                baseline_y: y + self.baseline(parent_style),
                size,
                color: parent_style.color,
                glyphs: shaped.glyphs,
            })?;
            self.decorate_text(x, y + self.baseline(parent_style), width, parent_style)?;
            return Ok((width, height));
        }
        let NodeKind::Element {
            name, namespace, ..
        } = kind
        else {
            return Ok((0.0, 0.0));
        };
        if *namespace != Namespace::Html {
            return Ok((0.0, 0.0));
        }
        if matches!(
            name.as_str(),
            "head" | "style" | "script" | "meta" | "link" | "template"
        ) {
            return Ok((0.0, 0.0));
        }
        let style = match computed {
            Some(style) => style,
            None => css::compute_node(self.document, id, Some(parent_style), self.rules)
                .map_err(LayoutError::Css)?,
        };
        if style.display == Display::None {
            return Ok((0.0, 0.0));
        }
        if matches!(
            style.display,
            Display::Block | Display::Flex | Display::Grid
        ) {
            let height = self.box_for(id, parent_style, Some(style), x, y, available, depth)?;
            return Ok((available, height));
        }
        let hit = self.begin_hit(id)?;
        let margin = style.margin.max(0.0);
        let padding = style.padding.max(0.0);
        let border_width = if style.border_solid {
            style.border_width
        } else {
            0.0
        };
        let outer_x = x + margin;
        let outer_y = y;
        let inset = padding + border_width;
        let background_index = if has_background(&style)
            && (self.transform_depth > 0
                || (outer_x < self.viewport.width && y < self.viewport.height))
        {
            let index = self.commands.len() + background_paints(&style) - 1;
            self.push_background(
                Rect {
                    x: outer_x,
                    y: outer_y,
                    width: 0.0,
                    height: 0.0,
                },
                &style,
            )?;
            Some(index)
        } else {
            None
        };
        let border_index = if border_width > 0.0
            && style.border_color.a > 0
            && (self.transform_depth > 0
                || (outer_x < self.viewport.width && y < self.viewport.height))
        {
            let index = self.commands.len();
            self.push_command(border(
                Rect {
                    x: outer_x,
                    y: outer_y,
                    width: 0.0,
                    height: 0.0,
                },
                &style,
            ))?;
            Some(index)
        } else {
            None
        };
        let mut advance = 0.0;
        let mut height = self.line_height(&style);
        let mut child = self
            .document
            .first_child(id)
            .map_err(|_| LayoutError::InvalidTree)?;
        while let Some(current) = child {
            let (width, child_height) = self.inline_for(
                current,
                &style,
                None,
                outer_x + inset + advance,
                outer_y + inset,
                available,
                depth + 1,
            )?;
            advance += width;
            height = height.max(child_height);
            child = self
                .document
                .next_sibling(current)
                .map_err(|_| LayoutError::InvalidTree)?;
        }
        let box_width = advance + 2.0 * inset;
        let box_height = height + 2.0 * inset;
        let rect = Rect {
            x: outer_x,
            y: outer_y,
            width: box_width,
            height: box_height,
        };
        self.finish_hit(hit, rect);
        if let Some(index) = background_index {
            self.finish_background(index, rect, &style);
        }
        if let Some(index) = border_index {
            self.commands[index] = border(rect, &style);
        }
        Ok((box_width + 2.0 * margin, box_height))
    }

    fn opacity_style(
        &self,
        id: NodeId,
        parent: &Style,
        computed: Option<Style>,
    ) -> Result<Option<Style>, LayoutError> {
        if computed.is_some() {
            return Ok(computed);
        }
        if matches!(self.document.kind(id), Ok(NodeKind::Element { .. })) {
            Ok(Some(
                css::compute_node(self.document, id, Some(parent), self.rules)
                    .map_err(LayoutError::Css)?,
            ))
        } else {
            Ok(None)
        }
    }

    fn begin_opacity(&mut self, style: Option<&Style>) -> Result<Option<usize>, LayoutError> {
        if let Some(style) = style.filter(|style| style.opacity < 1.0) {
            let index = self.commands.len();
            self.push_command(Command::PushLayer {
                rect: self.viewport,
                radius: 0.0,
                opacity: style.opacity,
                clip: false,
            })?;
            Ok(Some(index))
        } else {
            Ok(None)
        }
    }

    fn refresh_transform(
        &mut self,
        style: &Style,
        commands: core::ops::Range<usize>,
        hits: core::ops::Range<usize>,
    ) -> Result<(), LayoutError> {
        if style.transforms.is_none() {
            return Ok(());
        }
        let rect = self
            .geometry
            .as_ref()
            .and_then(|geometry| geometry.hits.get(hits.start))
            .map(|hit| hit.rect)
            .or_else(|| {
                self.commands[commands.clone()]
                    .iter()
                    .find_map(|command| match command {
                        Command::FillRect { rect, .. }
                        | Command::StrokeBorder { rect, .. }
                        | Command::StrokePatternBorder { rect, .. }
                        | Command::Image { rect, .. } => Some(*rect),
                        Command::FillRoundedRect { rect, .. } => {
                            let inset = background_bleed_inset(style);
                            Some(Rect {
                                x: rect.x - inset,
                                y: rect.y - inset,
                                width: rect.width + 2.0 * inset,
                                height: rect.height + 2.0 * inset,
                            })
                        }
                        Command::FillGradient { rect, .. } => {
                            let border = if style.border_solid {
                                style.border_width
                            } else {
                                0.0
                            };
                            Some(Rect {
                                x: rect.x - border,
                                y: rect.y - border,
                                width: rect.width + 2.0 * border,
                                height: rect.height + 2.0 * border,
                            })
                        }
                        Command::BoxShadow { rect, shadow, .. } if !shadow.inset => Some(*rect),
                        _ => None,
                    })
            });
        if let Some(rect) = rect {
            let matrix = style
                .transform_matrix(rect)
                .ok_or(LayoutError::Css(css::CssError {
                    offset: 0,
                    message: "invalid computed transform",
                }))?;
            if let Some(Command::PushTransform(value)) = self.commands.get_mut(commands.start) {
                *value = matrix;
            }
            if let Some(geometry) = self.geometry.as_mut() {
                if let Some(group) = geometry
                    .transforms
                    .iter_mut()
                    .rev()
                    .find(|group| group.hits == hits)
                {
                    group.matrix = matrix;
                }
            }
        }
        Ok(())
    }

    fn end_opacity(&mut self, layer: Option<usize>) -> Result<(), LayoutError> {
        if let Some(index) = layer {
            if self.commands.len() == index + 1 {
                self.commands.pop();
            } else {
                self.push_command(Command::PopLayer)?;
            }
        }
        Ok(())
    }

    fn box_for(
        &mut self,
        id: NodeId,
        parent_style: &Style,
        computed: Option<Style>,
        x: f32,
        y: f32,
        available: f32,
        depth: usize,
    ) -> Result<f32, LayoutError> {
        let mut computed = self
            .opacity_style(id, parent_style, computed)?
            .map(|style| {
                let containing = if style.position == Position::Fixed {
                    self.fixed_containing_block.or(Some(self.viewport))
                } else if style.position == Position::Absolute {
                    self.containing_block.or(Some(self.viewport))
                } else {
                    None
                };
                style.resolve_percentages(
                    containing.map_or(available, |rect| rect.width),
                    containing.map_or(self.parent_height, |rect| Some(rect.height)),
                )
            });
        let saved = self.containing_block;
        let saved_fixed = self.fixed_containing_block;
        let saved_height = self.parent_height;
        let saved_decorations = self.decorations.len();
        let mut origin = (x, y);
        let mut available = available;
        let mut out_of_flow = false;
        let mut trailing = (None, None);
        if let Some(style) = computed.as_mut() {
            self.begin_decoration(style)?;
            out_of_flow = matches!(style.position, Position::Absolute | Position::Fixed);
            if !out_of_flow
                && style.float == Float::None
                && !matches!(parent_style.display, Display::Flex | Display::Grid)
                && matches!(
                    style.display,
                    Display::Block | Display::Flex | Display::Grid
                )
            {
                let left_auto = style.margin_auto[3];
                let right_auto = style.margin_auto[1];
                let left = if left_auto {
                    0.0
                } else {
                    style.margin_sides[3]
                };
                let right = if right_auto {
                    0.0
                } else {
                    style.margin_sides[1]
                };
                let edges = style.padding_sides[1]
                    + style.padding_sides[3]
                    + if style.border_solid {
                        2.0 * style.border_width
                    } else {
                        0.0
                    };
                let content = constrained_width(
                    style,
                    style
                        .width
                        .map_or((available - left - right - edges).max(0.0), |width| {
                            content_dimension(style, width)
                        }),
                );
                let free = available - edges - content - left - right;
                let (used_left, used_right) = if free >= 0.0 && (left_auto || right_auto) {
                    match (left_auto, right_auto) {
                        (true, true) => (free * 0.5, free * 0.5),
                        (true, false) => (free, right),
                        _ => (left, free),
                    }
                } else if parent_style.direction == Direction::Rtl {
                    (left + free, right)
                } else {
                    (left, right)
                };
                if style.margin_sides[3] != used_left {
                    style.margin_sides[3] = used_left;
                }
                if style.margin_sides[1] != used_right {
                    style.margin_sides[1] = used_right;
                }
            }
            if out_of_flow {
                let containing = if style.position == Position::Fixed {
                    saved_fixed.unwrap_or(self.viewport)
                } else {
                    saved.unwrap_or(self.viewport)
                };
                available = containing.width;
                if style.width.is_none() {
                    if let (Some(left), Some(right)) = (style.left, style.right) {
                        style.width = Some(specified_dimension(
                            style,
                            (containing.width - left - right - box_edges(style, true)).max(0.0),
                        ));
                    } else {
                        let (intrinsic, _) = self.intrinsic_size(id, style, depth + 1)?;
                        style.width = Some(specified_dimension(
                            style,
                            (intrinsic.min(containing.width) - box_edges(style, true)).max(0.0),
                        ));
                    }
                }
                if style.height.is_none() {
                    if let (Some(top), Some(bottom)) = (style.top, style.bottom) {
                        style.height = Some(specified_height(
                            style,
                            (containing.height - top - bottom - box_edges(style, false)).max(0.0),
                        ));
                    }
                }
                origin.0 = style.left.map_or(x, |left| containing.x + left);
                origin.1 = style.top.map_or(y, |top| containing.y + top);
                if style.left.is_none() {
                    trailing.0 = style
                        .right
                        .map(|right| containing.x + containing.width - right);
                    if let Some(edge) = trailing.0 {
                        origin.0 = edge
                            - content_dimension(style, style.width.unwrap_or(0.0))
                            - box_edges(style, true);
                    }
                }
                if style.top.is_none() {
                    trailing.1 = style
                        .bottom
                        .map(|bottom| containing.y + containing.height - bottom);
                    if let Some(edge) = trailing.1 {
                        let height = if let Some(height) = style.height {
                            content_height_dimension(style, height) + box_edges(style, false)
                        } else {
                            self.intrinsic_size(id, style, depth + 1)?.1
                        };
                        origin.1 = edge - height;
                    }
                }
            } else if style.position == Position::Relative {
                origin.0 += style.left.unwrap_or_else(|| -style.right.unwrap_or(0.0));
                origin.1 += style.top.unwrap_or_else(|| -style.bottom.unwrap_or(0.0));
            }
            if style.position != Position::Static || style.transforms.is_some() {
                let border = if style.border_solid {
                    style.border_width
                } else {
                    0.0
                };
                self.containing_block = Some(Rect {
                    x: origin.0 + style.margin_sides[3] + border,
                    y: origin.1 + style.margin_sides[0] + border,
                    width: style.width.map_or(
                        available - style.margin_sides[1] - style.margin_sides[3] - 2.0 * border,
                        |width| {
                            content_dimension(style, width)
                                + style.padding_sides[1]
                                + style.padding_sides[3]
                        },
                    ),
                    height: style
                        .height
                        .map_or(0.0, |height| content_height_dimension(style, height))
                        + style.padding_sides[0]
                        + style.padding_sides[2],
                });
                if style.transforms.is_some() {
                    self.fixed_containing_block = self.containing_block;
                }
            }
            self.parent_height = style
                .height
                .map(|height| constrained_height(style, content_height_dimension(style, height)));
        }
        let first_command = self.commands.len();
        let first_hit = self
            .geometry
            .as_ref()
            .map_or(0, |geometry| geometry.hits.len());
        let transform_style = computed
            .as_ref()
            .filter(|style| style.transforms.is_some())
            .cloned();
        let transform_command = if transform_style.is_some() {
            let index = self.commands.len();
            self.push_command(Command::PushTransform(crate::paint::Affine::IDENTITY))?;
            self.transform_depth += 1;
            Some(index)
        } else {
            None
        };
        let layer = self.begin_opacity(computed.as_ref())?;
        let width = computed.as_ref().map_or(available, |style| {
            let border = if style.border_solid {
                2.0 * style.border_width
            } else {
                0.0
            };
            style.width.map_or(available, |width| {
                content_dimension(style, width)
                    + style.padding_sides[1]
                    + style.padding_sides[3]
                    + border
                    + style.margin_sides[1]
                    + style.margin_sides[3]
            })
        });
        let result = self.box_content(
            id,
            parent_style,
            computed,
            origin.0,
            origin.1,
            available,
            depth,
        );
        self.containing_block = saved;
        self.fixed_containing_block = saved_fixed;
        self.parent_height = saved_height;
        self.decorations.truncate(saved_decorations);
        if transform_command.is_some() {
            self.transform_depth -= 1;
        }
        let result = result?;
        self.end_opacity(layer)?;
        if let (Some(index), Some(style)) = (transform_command, transform_style.as_ref()) {
            let fallback = Rect {
                x: origin.0 + style.margin_sides[3],
                y: origin.1 + style.margin_sides[0],
                width: (width - style.margin_sides[1] - style.margin_sides[3]).max(0.0),
                height: (result - style.margin_sides[0] - style.margin_sides[2]).max(0.0),
            };
            let rect = self
                .geometry
                .as_ref()
                .and_then(|g| g.hits.get(first_hit))
                .filter(|hit| hit.node == id)
                .map_or(fallback, |hit| hit.rect);
            let matrix = style
                .transform_matrix(rect)
                .ok_or(LayoutError::Css(css::CssError {
                    offset: 0,
                    message: "invalid computed transform",
                }))?;
            self.commands[index] = Command::PushTransform(matrix);
            if self.commands.len() == index + 1 {
                self.commands.pop();
            } else {
                self.push_command(Command::PopTransform)?;
            }
            if let Some(geometry) = self.geometry.as_mut() {
                if geometry.transforms.len() == MAX_DISPLAY_COMMANDS {
                    return Err(LayoutError::CommandLimit);
                }
                geometry
                    .transforms
                    .try_reserve(1)
                    .map_err(|_| LayoutError::CommandLimit)?;
                geometry.transforms.push(HitTransform {
                    hits: first_hit..geometry.hits.len(),
                    matrix,
                });
            }
        }
        let dx = trailing.0.map_or(0.0, |edge| edge - origin.0 - width);
        let dy = trailing.1.map_or(0.0, |edge| edge - origin.1 - result);
        if dx != 0.0 || dy != 0.0 {
            for command in &mut self.commands[first_command..] {
                move_command(command, dx, dy);
            }
            if let Some(geometry) = self.geometry.as_mut() {
                geometry.move_hits(first_hit..geometry.hits.len(), dx, dy);
            }
        }
        Ok(if out_of_flow { 0.0 } else { result })
    }

    fn box_content(
        &mut self,
        id: NodeId,
        parent_style: &Style,
        computed: Option<Style>,
        x: f32,
        y: f32,
        available: f32,
        depth: usize,
    ) -> Result<f32, LayoutError> {
        if depth > 512 {
            return Err(LayoutError::DepthLimit);
        }
        let kind = self
            .document
            .kind(id)
            .map_err(|_| LayoutError::InvalidTree)?;
        if let NodeKind::Text(value) = kind {
            if value.is_empty() {
                return Ok(0.0);
            }
            let size = parent_style.font_size;
            let line_height = self.line_height(parent_style);
            if self.transform_depth == 0
                && (x >= self.viewport.width
                    || y >= self.viewport.height
                    || (line_height > 0.0 && y + line_height <= 0.0))
            {
                return Ok(line_height);
            }
            let shaped = self
                .text
                .shape_directional(value, size, parent_style.direction == Direction::Rtl)
                .map_err(|_| LayoutError::Text)?;
            self.push_command(Command::GlyphRun {
                origin_x: x,
                baseline_y: y + self.baseline(parent_style),
                size,
                color: parent_style.color,
                glyphs: shaped.glyphs,
            })?;
            return Ok(line_height);
        }
        let NodeKind::Element {
            name,
            attributes,
            namespace,
        } = kind
        else {
            return Ok(0.0);
        };
        if *namespace != Namespace::Html {
            return Ok(0.0);
        }
        if matches!(
            name.as_str(),
            "head" | "style" | "script" | "meta" | "link" | "template"
        ) {
            return Ok(0.0);
        }
        let style = match computed {
            Some(style) => style,
            None => css::compute_node(self.document, id, Some(parent_style), &self.rules)
                .map_err(LayoutError::Css)?,
        };
        if style.display == Display::None {
            return Ok(0.0);
        }
        if style.display == Display::Table {
            return self.table(id, &style, x, y, available, depth);
        }
        let hit = self.begin_hit(id)?;
        let border_width = if style.border_solid {
            style.border_width
        } else {
            0.0
        };
        let [margin_top, margin_right, margin_bottom, margin_left] = style.margin_sides;
        let [padding_top, padding_right, padding_bottom, padding_left] = style.padding_sides;
        let outer_x = x + margin_left;
        let outer_y = y + margin_top;
        let content_width = constrained_width(
            &style,
            style
                .width
                .map(|width| content_dimension(&style, width))
                .unwrap_or(
                    (available
                        - margin_left
                        - margin_right
                        - padding_left
                        - padding_right
                        - 2.0 * border_width)
                        .max(0.0),
                )
                .max(0.0),
        );
        let box_width = content_width + padding_left + padding_right + 2.0 * border_width;
        if name == "img" {
            let dimension = |key: &str| {
                attributes
                    .iter()
                    .find(|(name, _)| name == key)
                    .and_then(|(_, value)| value.parse::<f32>().ok())
                    .filter(|value| value.is_finite() && *value >= 0.0)
            };
            let specified_width = style
                .width
                .or_else(|| dimension("width"))
                .map(|width| content_dimension(&style, width));
            let specified_height = style
                .height
                .or_else(|| dimension("height"))
                .map(|height| content_height_dimension(&style, height));
            if self.transform_depth == 0 && outer_y >= self.viewport.height {
                if let Some(height) = specified_height {
                    return Ok(height
                        + padding_top
                        + padding_bottom
                        + 2.0 * border_width
                        + margin_top
                        + margin_bottom);
                }
            }
            let source = attributes
                .iter()
                .find(|(name, _)| name == "src")
                .map(|(_, source)| source.as_str());
            let image = match (source, self.images) {
                (Some(source), Some(images)) => match images.resolve(source) {
                    ImageState::Ready(image) if image.is_valid() => Some(image),
                    ImageState::Ready(_) => return Err(LayoutError::ImageFailed),
                    ImageState::Pending => return Err(LayoutError::ImagePending),
                    ImageState::Failed => return Err(LayoutError::ImageFailed),
                },
                (Some(_), None) => return Err(LayoutError::ImageFailed),
                (None, _) => None,
            };
            if let Some(image) = image {
                let (image_width, image_height) = match (specified_width, specified_height) {
                    (Some(width), Some(height)) => (width, height),
                    (Some(width), None) => {
                        (width, width * image.height as f32 / image.width as f32)
                    }
                    (None, Some(height)) => {
                        (height * image.width as f32 / image.height as f32, height)
                    }
                    (None, None) => (image.width as f32, image.height as f32),
                };
                let rect = Rect {
                    x: outer_x + border_width + padding_left,
                    y: outer_y + border_width + padding_top,
                    width: image_width,
                    height: image_height,
                };
                let outer_rect = Rect {
                    x: outer_x,
                    y: outer_y,
                    width: image_width + padding_left + padding_right + 2.0 * border_width,
                    height: image_height + padding_top + padding_bottom + 2.0 * border_width,
                };
                self.finish_hit(hit, outer_rect);
                if has_background(&style)
                    && (self.transform_depth > 0
                        || outer_rect.intersection(self.viewport).is_some())
                {
                    self.push_background(outer_rect, &style)?;
                }
                if border_width > 0.0
                    && (self.transform_depth > 0
                        || outer_rect.intersection(self.viewport).is_some())
                {
                    self.push_command(border(outer_rect, &style))?;
                }
                if self.transform_depth > 0 || rect.intersection(self.viewport).is_some() {
                    self.push_command(Command::Image { rect, image })?;
                }
                return Ok(image_height
                    + padding_top
                    + padding_bottom
                    + 2.0 * border_width
                    + margin_top
                    + margin_bottom);
            }
        }
        let background_index = if !has_background(&style)
            || (self.transform_depth == 0
                && (outer_y >= self.viewport.height || outer_x >= self.viewport.width))
            || (name == "body" && self.body_background_on_canvas)
        {
            None
        } else {
            let index = self.commands.len() + background_paints(&style) - 1;
            self.push_background(
                Rect {
                    x: outer_x,
                    y: outer_y,
                    width: box_width,
                    height: 0.0,
                },
                &style,
            )?;
            Some(index)
        };
        let border_index = if border_width > 0.0
            && style.border_color.a > 0
            && (self.transform_depth > 0
                || (outer_y < self.viewport.height && outer_x < self.viewport.width))
        {
            let index = self.commands.len();
            self.push_command(border(
                Rect {
                    x: outer_x,
                    y: outer_y,
                    width: box_width,
                    height: 0.0,
                },
                &style,
            ))?;
            Some(index)
        } else {
            None
        };
        let scroll = if style.overflow_clip {
            self.scrolls
                .iter()
                .find(|scroll| scroll.node == id)
                .copied()
        } else {
            None
        };
        let (scroll_x, scroll_y) = scroll.map_or((0.0, 0.0), |scroll| (scroll.x, scroll.y));
        let clip_index = if style.overflow_clip {
            let index = self.commands.len();
            self.push_command(Command::PushClip(Rect {
                x: outer_x + border_width,
                y: outer_y + border_width,
                width: content_width + padding_left + padding_right,
                height: 0.0,
            }))?;
            Some(index)
        } else {
            None
        };
        let first_child_hit = self
            .geometry
            .as_ref()
            .map_or(0, |geometry| geometry.hits.len());
        let content_x = outer_x + border_width + padding_left - scroll_x;
        let mut cursor = outer_y + border_width + padding_top - scroll_y;
        let mut content_extent_width = content_width;
        let mut inline_width = 0.0;
        let mut inline_height: f32 = 0.0;
        let mut line_start = self.commands.len();
        let mut trailing_space = 0.0;
        let mut floats: Vec<(Rect, Float)> = Vec::new();
        let mut positioned: Vec<(NodeId, Style, f32, f32)> = Vec::new();
        let mut previous_margin: f32 = 0.0;
        let mut child = if style.display == Display::Grid {
            let (extent_width, height) =
                self.grid_children(id, &style, content_x, cursor, content_width, depth)?;
            cursor += height;
            content_extent_width = content_extent_width.max(extent_width);
            None
        } else if style.display == Display::Flex {
            let (extent_width, height) =
                self.flex_children(id, &style, content_x, cursor, content_width, depth)?;
            cursor += height;
            content_extent_width = content_extent_width.max(extent_width);
            None
        } else {
            self.document
                .first_child(id)
                .map_err(|_| LayoutError::InvalidTree)?
        };
        while let Some(current) = child {
            let kind = self
                .document
                .kind(current)
                .map_err(|_| LayoutError::InvalidTree)?;
            let computed = if matches!(kind, NodeKind::Element { .. }) {
                Some(
                    css::compute_node(self.document, current, Some(&style), self.rules)
                        .map_err(LayoutError::Css)
                        .map(|child| {
                            if matches!(child.position, Position::Absolute | Position::Fixed) {
                                child
                            } else {
                                child.resolve_percentages(content_width, self.parent_height)
                            }
                        })?,
                )
            } else {
                None
            };
            if let Some(child_style) = &computed {
                if child_style.display != Display::None {
                    content_extent_width = content_extent_width.max(
                        child_style
                            .width
                            .map(|width| content_dimension(child_style, width))
                            .unwrap_or(0.0)
                            + 2.0
                                * (child_style.padding
                                    + child_style.margin
                                    + if child_style.border_solid {
                                        child_style.border_width
                                    } else {
                                        0.0
                                    }),
                    );
                }
            }
            if computed
                .as_ref()
                .is_some_and(|style| style.display == Display::None)
                || matches!(
                    kind,
                    NodeKind::Comment(_) | NodeKind::ProcessingInstruction { .. }
                )
                || matches!(kind, NodeKind::Element { namespace, .. } if *namespace != Namespace::Html)
                || matches!(kind, NodeKind::Element { name, .. } if matches!(name.as_str(), "head" | "style" | "script" | "meta" | "link" | "template" | "title" | "base"))
            {
            } else if computed
                .as_ref()
                .is_some_and(|s| matches!(s.position, Position::Absolute | Position::Fixed))
            {
                if positioned.len() == 4096 {
                    return Err(LayoutError::CommandLimit);
                }
                positioned
                    .try_reserve(1)
                    .map_err(|_| LayoutError::CommandLimit)?;
                positioned.push((current, computed.unwrap(), content_x + inline_width, cursor));
            } else if computed.as_ref().is_some_and(|s| s.float != Float::None) {
                let mut floated = computed.unwrap();
                let (intrinsic, _) = self.intrinsic_size(current, &floated, depth + 1)?;
                let width = floated.width.unwrap_or_else(|| {
                    specified_dimension(
                        &floated,
                        (intrinsic.min(content_width) - box_edges(&floated, true)).max(0.0),
                    )
                });
                floated.width = Some(width);
                let outer_width = content_dimension(&floated, width) + box_edges(&floated, true);
                let mut float_y = cursor;
                let (mut left, mut space) =
                    float_edges(&floats, content_x, content_width, float_y, 1.0);
                while outer_width > space && !floats.is_empty() {
                    let next = floats
                        .iter()
                        .map(|(r, _)| r.y + r.height)
                        .filter(|end| *end > float_y)
                        .min_by(f32::total_cmp);
                    let Some(next) = next else {
                        break;
                    };
                    float_y = next;
                    (left, space) = float_edges(&floats, content_x, content_width, float_y, 1.0);
                }
                let side = floated.float;
                let float_x = if side == Float::Right {
                    left + space - outer_width
                } else {
                    left
                };
                let height = self.box_for(
                    current,
                    &style,
                    Some(floated),
                    float_x,
                    float_y,
                    outer_width,
                    depth + 1,
                )?;
                if floats.len() == 4096 {
                    return Err(LayoutError::CommandLimit);
                }
                floats
                    .try_reserve(1)
                    .map_err(|_| LayoutError::CommandLimit)?;
                floats.push((
                    Rect {
                        x: float_x,
                        y: float_y,
                        width: outer_width,
                        height,
                    },
                    side,
                ));
                if inline_width == 0.0 {
                    line_start = self.commands.len();
                }
            } else if matches!(kind, NodeKind::Element { name, namespace: Namespace::Html, .. } if name == "br")
                && computed
                    .as_ref()
                    .is_some_and(|style| style.display != Display::None)
            {
                self.align_line(
                    line_start,
                    content_x,
                    content_width,
                    inline_width - trailing_space,
                    &style,
                    true,
                );
                cursor += inline_height.max(self.line_height(&style));
                inline_width = 0.0;
                inline_height = 0.0;
                line_start = self.commands.len();
                trailing_space = 0.0;
            } else if let NodeKind::Text(value) = kind {
                self.text_flow(
                    value,
                    &style,
                    content_x,
                    content_width,
                    &mut cursor,
                    &mut inline_width,
                    &mut inline_height,
                    &floats,
                    &mut line_start,
                    &mut trailing_space,
                )?;
            } else if computed
                .as_ref()
                .is_some_and(|style| style.display == Display::Inline)
            {
                if !matches!(style.white_space, WhiteSpace::NoWrap | WhiteSpace::Pre)
                    && inline_width > 0.0
                    && inline_width >= content_width
                {
                    self.align_line(
                        line_start,
                        content_x,
                        content_width,
                        inline_width - trailing_space,
                        &style,
                        false,
                    );
                    cursor += inline_height;
                    inline_width = 0.0;
                    inline_height = 0.0;
                    line_start = self.commands.len();
                    trailing_space = 0.0;
                }
                let first_command = self.commands.len();
                let first_hit = self
                    .geometry
                    .as_ref()
                    .map_or(0, |geometry| geometry.hits.len());
                let (width, height) = self.inline_for(
                    current,
                    &style,
                    computed,
                    content_x + inline_width,
                    cursor,
                    content_width,
                    depth + 1,
                )?;
                if !matches!(style.white_space, WhiteSpace::NoWrap | WhiteSpace::Pre)
                    && inline_width > 0.0
                    && inline_width + width > content_width
                {
                    self.align_line_until(
                        line_start,
                        first_command,
                        content_width,
                        inline_width - trailing_space,
                        &style,
                        false,
                    );
                    cursor += inline_height;
                    for command in &mut self.commands[first_command..] {
                        move_command(command, -inline_width, inline_height);
                    }
                    if let Some(geometry) = self.geometry.as_mut() {
                        geometry.move_hits(
                            first_hit..geometry.hits.len(),
                            -inline_width,
                            inline_height,
                        );
                    }
                    inline_width = 0.0;
                    inline_height = 0.0;
                    line_start = first_command;
                    trailing_space = 0.0;
                }
                inline_width += width;
                inline_height = inline_height.max(height);
            } else {
                self.align_line(
                    line_start,
                    content_x,
                    content_width,
                    inline_width - trailing_space,
                    &style,
                    true,
                );
                cursor += inline_height;
                inline_width = 0.0;
                inline_height = 0.0;
                trailing_space = 0.0;
                if let Some(child_style) = &computed {
                    if child_style.clear != Clear::None {
                        for (rect, side) in &floats {
                            if child_style.clear == Clear::Both
                                || child_style.clear == Clear::Left && *side == Float::Left
                                || child_style.clear == Clear::Right && *side == Float::Right
                            {
                                cursor = cursor.max(rect.y + rect.height);
                            }
                        }
                        previous_margin = 0.0;
                    }
                    let margin = child_style.margin_sides[0];
                    if previous_margin != 0.0 {
                        let collapsed = previous_margin.max(margin).max(0.0)
                            + previous_margin.min(margin).min(0.0);
                        cursor += collapsed - previous_margin - margin;
                    }
                    previous_margin = child_style.margin_sides[2];
                }
                cursor += self.box_for(
                    current,
                    &style,
                    computed,
                    content_x,
                    cursor,
                    content_width,
                    depth + 1,
                )?;
                line_start = self.commands.len();
            }
            content_extent_width = content_extent_width.max(inline_width);
            child = self
                .document
                .next_sibling(current)
                .map_err(|_| LayoutError::InvalidTree)?;
        }
        self.align_line(
            line_start,
            content_x,
            content_width,
            inline_width - trailing_space,
            &style,
            true,
        );
        cursor += inline_height;
        if style.overflow_clip
            || style.float != Float::None
            || matches!(style.position, Position::Absolute | Position::Fixed)
        {
            for (rect, _) in &floats {
                cursor = cursor.max(rect.y + rect.height);
            }
        }
        let content_extent_height =
            (cursor + scroll_y - outer_y - border_width - padding_top).max(0.0);
        let content_height = constrained_height(
            &style,
            style
                .height
                .map(|height| content_height_dimension(&style, height))
                .unwrap_or(content_extent_height),
        );
        let box_height = content_height + padding_top + padding_bottom + 2.0 * border_width;
        if style.position != Position::Static || style.transforms.is_some() {
            self.containing_block = Some(Rect {
                x: outer_x + border_width,
                y: outer_y + border_width,
                width: box_width - 2.0 * border_width,
                height: box_height - 2.0 * border_width,
            });
            if style.transforms.is_some() {
                self.fixed_containing_block = self.containing_block;
            }
        }
        for (node, child_style, x, y) in positioned {
            self.box_for(
                node,
                &style,
                Some(child_style),
                x,
                y,
                content_width,
                depth + 1,
            )?;
        }
        if let Some(index) = clip_index {
            let clip = Rect {
                x: outer_x + border_width,
                y: outer_y + border_width,
                width: content_width + padding_left + padding_right,
                height: content_height + padding_top + padding_bottom,
            };
            if style.border_radius > border_width {
                self.commands[index] = Command::PushLayer {
                    rect: clip,
                    radius: style.border_radius - border_width,
                    opacity: 1.0,
                    clip: true,
                };
                self.push_command(Command::PopLayer)?;
            } else {
                self.commands[index] = Command::PushClip(clip);
                self.push_command(Command::PopClip)?;
            }
            if let Some(geometry) = self.geometry.as_mut() {
                if geometry.rounded_clips.len() == MAX_DISPLAY_COMMANDS {
                    return Err(LayoutError::CommandLimit);
                }
                geometry
                    .rounded_clips
                    .try_reserve(1)
                    .map_err(|_| LayoutError::CommandLimit)?;
                geometry.rounded_clips.push(HitClip {
                    first_transform: geometry.transforms.len(),
                    hits: first_child_hit..geometry.hits.len(),
                    rect: clip,
                    radius: (style.border_radius - border_width).max(0.0),
                });
                geometry
                    .scroll_extents
                    .try_reserve(1)
                    .map_err(|_| LayoutError::CommandLimit)?;
                geometry.scroll_extents.push(ScrollOffset {
                    node: id,
                    x: (content_extent_width - content_width).max(0.0),
                    y: (content_extent_height - content_height).max(0.0),
                });
            }
        }
        self.finish_hit(
            hit,
            Rect {
                x: outer_x,
                y: outer_y,
                width: box_width,
                height: box_height,
            },
        );
        if let Some(index) = background_index {
            self.finish_background(
                index,
                Rect {
                    x: outer_x,
                    y: outer_y,
                    width: box_width,
                    height: box_height,
                },
                &style,
            );
        }
        if let Some(index) = border_index {
            self.commands[index] = border(
                Rect {
                    x: outer_x,
                    y: outer_y,
                    width: box_width,
                    height: box_height,
                },
                &style,
            );
        }
        Ok(box_height + margin_top + margin_bottom)
    }
}

pub fn display_list(
    document: &Document,
    width: u32,
    height: u32,
    text: &dyn TextShaper,
) -> Result<DisplayList, LayoutError> {
    let mut rules = stylesheets(document)?;
    rules.environment = css::MediaEnvironment {
        width: width as f32,
        height: height as f32,
        ..Default::default()
    };
    display_list_with_styles(document, width, height, text, &rules, None, None, &[])
}

pub fn display_list_with_images(
    document: &Document,
    width: u32,
    height: u32,
    text: &dyn TextShaper,
    images: &dyn ImageResolver,
) -> Result<DisplayList, LayoutError> {
    let mut rules = stylesheets(document)?;
    rules.environment = css::MediaEnvironment {
        width: width as f32,
        height: height as f32,
        ..Default::default()
    };
    display_list_with_styles(
        document,
        width,
        height,
        text,
        &rules,
        Some(images),
        None,
        &[],
    )
}

pub(crate) fn display_list_with_styles(
    document: &Document,
    width: u32,
    height: u32,
    text: &dyn TextShaper,
    rules: &StyleIndex,
    images: Option<&dyn ImageResolver>,
    geometry: Option<&mut LayoutGeometry>,
    scrolls: &[ScrollOffset],
) -> Result<DisplayList, LayoutError> {
    let root = document.root();
    let mut html = None;
    let mut child = document
        .first_child(root)
        .map_err(|_| LayoutError::InvalidTree)?;
    while let Some(current) = child {
        if matches!(document.kind(current), Ok(NodeKind::Element { name, .. }) if name == "html") {
            html = Some(current);
            break;
        }
        child = document
            .next_sibling(current)
            .map_err(|_| LayoutError::InvalidTree)?;
    }
    let html = html.ok_or(LayoutError::InvalidTree)?;
    let mut layout = Layout {
        document,
        text,
        rules,
        images,
        commands: Vec::new(),
        body_background_on_canvas: false,
        viewport: Rect {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
        },
        geometry,
        scrolls,
        containing_block: None,
        fixed_containing_block: None,
        transform_depth: 0,
        parent_height: None,
        decorations: Vec::new(),
    };
    layout.push_command(Command::FillRect {
        rect: Rect {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
        },
        color: Rgba {
            r: 255,
            g: 255,
            b: 255,
            a: 255,
        },
    })?;
    let initial =
        css::compute_node(document, html, None, &layout.rules).map_err(LayoutError::Css)?;
    if initial.display == Display::None {
        return Ok(DisplayList(layout.commands));
    }
    let root_hit = layout.begin_hit(html)?;
    layout.finish_hit(root_hit, layout.viewport);
    if has_background(&initial) {
        layout.push_background(layout.viewport, &initial)?;
    }
    let mut child = document
        .first_child(html)
        .map_err(|_| LayoutError::InvalidTree)?;
    while let Some(current) = child {
        if matches!(document.kind(current), Ok(NodeKind::Element { name, .. }) if name == "body") {
            let style = css::compute_node(document, current, Some(&initial), &layout.rules)
                .map_err(LayoutError::Css)?;
            if style.display != Display::None && has_background(&style) && !has_background(&initial)
            {
                layout.push_background(
                    Rect {
                        x: 0.0,
                        y: 0.0,
                        width: width as f32,
                        height: height as f32,
                    },
                    &style,
                )?;
                layout.body_background_on_canvas = true;
            }
            layout.box_for(current, &initial, None, 0.0, 0.0, width as f32, 0)?;
        }
        child = document
            .next_sibling(current)
            .map_err(|_| LayoutError::InvalidTree)?;
    }
    Ok(DisplayList(layout.commands))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paint::ShapedRun;

    struct NoShape;

    impl TextShaper for NoShape {
        fn shape(&self, _: &str, _: f32) -> Result<ShapedRun, ()> {
            panic!("offscreen text was shaped")
        }
        fn ascent(&self, _: f32) -> f32 {
            10.0
        }
        fn line_height(&self, _: f32) -> f32 {
            20.0
        }
    }

    #[test]
    fn offscreen_text_skips_shaping_and_paint() {
        let document =
            crate::html::parse("<div style='height:100px'></div><p>below viewport</p>", 64)
                .unwrap();
        let list = display_list(&document, 20, 10, &NoShape).unwrap();
        assert!(
            !list
                .0
                .iter()
                .any(|command| matches!(command, Command::GlyphRun { .. }))
        );
    }

    #[test]
    fn offscreen_sized_image_skips_loading() {
        let document = crate::html::parse(
            "<div style='height:100px'></div><img src='large.png' width='20' height='10'>",
            64,
        )
        .unwrap();
        let images = |_: &str| -> ImageState { panic!("offscreen image was loaded") };
        display_list_with_images(&document, 20, 10, &NoShape, &images).unwrap();
    }

    struct FixedText;

    #[test]
    fn table_fixed_cells_share_columns_and_spans() {
        let boxes = colored_boxes(
            "<table style='width:40px;table-layout:fixed;border-spacing:2px'><tr><td rowspan='2' style='height:10px;background:red'></td><td style='height:4px;background:green'></td></tr><tr><td style='height:4px;background:red'></td></tr><tr><td colspan='2' style='height:6px;background:green'></td></tr></table>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [
                (2.0, 2.0, 17.0, 10.0),
                (21.0, 2.0, 17.0, 4.0),
                (21.0, 8.0, 17.0, 4.0),
                (2.0, 14.0, 36.0, 6.0)
            ]
        );
    }

    #[test]
    fn css_table_roles_use_same_geometry() {
        let boxes = colored_boxes(
            "<div style='display:table;width:30px;table-layout:fixed;border-spacing:0px'><div style='display:table-row'><div style='display:table-cell;height:6px;background:red'></div><div style='display:table-cell;height:10px;background:green'></div></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [(0.0, 0.0, 15.0, 10.0), (15.0, 0.0, 15.0, 10.0)]
        );
    }

    #[test]
    fn table_column_limit_rejects_overflow() {
        let document = crate::html::parse(
            "<table><tr><td colspan='256'></td><td></td></tr></table>",
            32,
        )
        .unwrap();
        assert_eq!(
            display_list(&document, 100, 100, &FixedText),
            Err(LayoutError::CommandLimit)
        );
    }

    fn colored_boxes(markup: &str) -> Vec<Rect> {
        let document = crate::html::parse(markup, 32).unwrap();
        display_list(&document, 100, 100, &FixedText)
            .unwrap()
            .0
            .into_iter()
            .filter_map(|command| match command {
                Command::FillRect { rect, color } if color.a == 255 && color.b == 0 => Some(rect),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn rtl_blocks_resolve_overflow_and_physical_auto_margins() {
        let boxes = colored_boxes(
            "<style>body{margin:0}main{direction:rtl;width:80px}div{height:5px;background:red}</style><main><div style='width:30px;margin-left:20px'></div><div style='width:120px'></div><div style='width:30px;margin:auto'></div><div style='width:30px;margin-right:auto'></div></main>",
        );
        assert_eq!(
            boxes.iter().map(|rect| rect.x).collect::<Vec<_>>(),
            [50.0, -40.0, 25.0, 0.0]
        );
    }

    #[test]
    fn rtl_flex_row_reverse_and_column_start_follow_inline_direction() {
        for (direction, expected) in [
            ("row", [60.0, 40.0]),
            ("row-reverse", [0.0, 20.0]),
            ("column", [60.0, 60.0]),
        ] {
            let markup = alloc::format!(
                "<style>body{{margin:0}}main{{display:flex;width:80px;direction:rtl;flex-direction:{direction};align-items:start}}div{{width:20px;height:5px;background:red}}</style><main><div></div><div></div></main>"
            );
            let boxes = colored_boxes(&markup);
            assert_eq!(
                boxes.iter().map(|rect| rect.x).collect::<Vec<_>>(),
                expected,
                "{direction}"
            );
        }
    }

    #[test]
    fn rtl_table_columns_and_cell_block_content_start_on_the_right() {
        let boxes = colored_boxes(
            "<style>body{margin:0}table{direction:rtl;width:80px;border-spacing:0}td{padding:0}div{width:20px;height:5px;background:red}</style><table><tr><td><div></div></td><td><div></div></td></tr></table>",
        );
        assert_eq!(
            boxes.iter().map(|rect| rect.x).collect::<Vec<_>>(),
            [60.0, 20.0]
        );
    }

    #[test]
    fn width_constraints_apply_to_auto_explicit_and_border_box_widths() {
        let boxes = colored_boxes(
            "<div style='width:10px;min-width:20px;max-width:5px;height:4px;background:red'></div><div style='max-width:12px;height:4px;background:green'></div><div style='box-sizing:border-box;min-width:20px;width:8px;padding:2px;height:8px;background:red'></div>",
        );
        assert_eq!(
            boxes.iter().map(|r| r.width).collect::<Vec<_>>(),
            [20.0, 12.0, 20.0]
        );
    }

    #[test]
    fn grid_fraction_tracks_gaps_spans_and_sparse_auto_placement() {
        let boxes = colored_boxes(
            "<div style='display:grid;width:100px;grid-template-columns:20px 1fr 2fr;grid-template-rows:10px 20px;gap:5px'><div style='grid-column:2 / span 2;background:red'></div><div style='background:green'></div><div style='background:red'></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [
                (25.0, 0.0, 75.0, 10.0),
                (0.0, 15.0, 20.0, 20.0),
                (25.0, 15.0, 70.0 / 3.0, 20.0)
            ]
        );
    }

    #[test]
    fn grid_browser_fixture_has_chrome_integral_geometry() {
        let boxes = colored_boxes(include_str!("../tests/grid-browser.html"));
        assert_eq!(
            boxes
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [
                (25.0, 0.0, 65.0, 10.0),
                (0.0, 15.0, 20.0, 20.0),
                (25.0, 15.0, 20.0, 20.0)
            ]
        );
    }

    #[test]
    fn grid_bounded_dense_document_metrics() {
        extern crate std;
        let columns = "1fr ".repeat(32);
        let markup = alloc::format!(
            "<div style='display:grid;width:320px;grid-template-columns:{columns}'>{}</div>",
            "<div style='height:1px;background:red'></div>".repeat(1024)
        );
        let document = crate::html::parse(&markup, 1100).unwrap();
        let started = std::time::Instant::now();
        let list = display_list(&document, 320, 64, &FixedText).unwrap();
        assert_eq!(list.0.iter().filter(|command| matches!(command, Command::FillRect { color, .. } if color.r == 255 && color.g == 0 && color.b == 0 && color.a == 255)).count(), 1024);
        assert!(core::mem::size_of::<Style>() <= 256);
        std::println!(
            "grid: 1024 items, {} commands, {} style bytes, 512 occupancy bytes, {:?} layout",
            list.0.len(),
            core::mem::size_of::<Style>(),
            started.elapsed()
        );
    }

    #[test]
    fn grid_auto_rows_measure_wrapped_content_at_final_column_width() {
        let boxes = colored_boxes(
            "<div style='display:grid;width:8px;grid-template-columns:8px'><div style='background:red;font-size:10px;line-height:10px'>aa aa</div><div style='background:green;height:3px'></div></div>",
        );
        assert_eq!(
            boxes.iter().map(|r| (r.y, r.height)).collect::<Vec<_>>(),
            [(0.0, 20.0), (20.0, 3.0)]
        );
    }

    #[test]
    fn grid_indefinite_fraction_rows_share_one_fraction_size() {
        let boxes = colored_boxes(
            "<div style='display:grid;width:10px;grid-template-rows:1fr 2fr'><div style='height:10px;background:red'></div><div style='height:6px;background:green'></div></div>",
        );
        assert_eq!(
            boxes.iter().map(|r| (r.y, r.height)).collect::<Vec<_>>(),
            [(0.0, 10.0), (10.0, 6.0)]
        );
    }

    #[test]
    fn grid_definite_items_reserve_space_before_auto_items() {
        let boxes = colored_boxes(
            "<div style='display:grid;width:40px;grid-template-columns:20px 20px;grid-template-rows:10px 10px'><div style='background:red'></div><div style='grid-column:1;grid-row:1;background:green'></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [(20.0, 0.0, 20.0, 10.0), (0.0, 0.0, 20.0, 10.0)]
        );
    }

    #[test]
    fn grid_auto_tracks_and_implicit_rows_use_item_intrinsics() {
        let boxes = colored_boxes(
            "<div style='display:grid;width:50px;grid-template-columns:auto 20px;gap:2px'><div style='height:8px;background:red'></div><div style='height:4px;background:green'></div><div style='height:6px;background:red'></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [
                (0.0, 0.0, 28.0, 8.0),
                (30.0, 0.0, 20.0, 4.0),
                (0.0, 10.0, 28.0, 6.0)
            ]
        );
    }

    #[test]
    fn grid_rejects_placement_beyond_bounded_tracks() {
        let document = crate::html::parse(
            "<div style='display:grid'><div style='grid-column:64 / span 2'></div></div>",
            8,
        )
        .unwrap();
        assert_eq!(
            display_list(&document, 100, 100, &FixedText),
            Err(LayoutError::GridLimit)
        );
    }

    #[test]
    fn flex_width_constraints_freeze_and_redistribute_free_space() {
        let grow = colored_boxes(
            "<div style='display:flex;width:50px'><div style='width:10px;max-width:15px;flex-grow:1;height:4px;background:red'></div><div style='width:10px;flex-grow:1;height:4px;background:green'></div></div>",
        );
        assert_eq!(
            grow.iter().map(|r| (r.x, r.width)).collect::<Vec<_>>(),
            [(0.0, 15.0), (15.0, 35.0)]
        );
        let shrink = colored_boxes(
            "<div style='display:flex;width:40px'><div style='width:30px;min-width:25px;height:4px;background:red'></div><div style='width:30px;height:4px;background:green'></div></div>",
        );
        assert_eq!(
            shrink.iter().map(|r| (r.x, r.width)).collect::<Vec<_>>(),
            [(0.0, 25.0), (25.0, 15.0)]
        );
    }

    #[test]
    fn flex_wrap_forms_lines_before_grow_and_aligns_each_line() {
        let boxes = colored_boxes(
            "<div style='display:flex;flex-wrap:wrap;width:24px;gap:2px;align-items:center'><div style='width:10px;height:4px;flex-grow:1;background:red'></div><div style='width:10px;height:8px;flex-grow:1;background:green'></div><div style='width:10px;height:6px;flex-grow:1;background:red'></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [
                (0.0, 2.0, 11.0, 4.0),
                (13.0, 0.0, 11.0, 8.0),
                (0.0, 10.0, 24.0, 6.0)
            ]
        );
    }

    #[test]
    fn flex_wrap_stretches_lines_in_definite_cross_size() {
        let boxes = colored_boxes(
            "<div style='display:flex;flex-wrap:wrap;width:20px;height:30px;gap:2px'><div style='width:12px;height:4px;background:red'></div><div style='width:12px;background:green'></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [(0.0, 0.0, 12.0, 4.0), (0.0, 18.0, 12.0, 12.0)]
        );
    }

    #[test]
    fn flex_column_wrap_and_oversized_item() {
        let boxes = colored_boxes(
            "<div style='display:flex;flex-direction:column;flex-wrap:wrap;width:30px;height:10px;gap:2px;align-items:start'><div style='width:4px;height:6px;background:red'></div><div style='width:8px;height:6px;background:green'></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [(0.0, 0.0, 4.0, 6.0), (14.0, 0.0, 8.0, 6.0)]
        );
        let boxes = colored_boxes(
            "<div style='display:flex;flex-wrap:wrap;width:10px'><div style='width:20px;height:4px;background:red'></div><div style='width:8px;height:6px;background:green'></div></div>",
        );
        assert_eq!(
            boxes.iter().map(|r| (r.y, r.width)).collect::<Vec<_>>(),
            [(0.0, 10.0), (4.0, 8.0)]
        );
    }

    #[test]
    fn flex_row_gap_justify_and_cross_alignment() {
        let boxes = colored_boxes(
            "<div style='display:flex;width:40px;height:20px;gap:4px;justify-content:space-between;align-items:center'><div style='width:8px;height:6px;background:red'></div><div style='width:8px;height:10px;background:green'></div></div>",
        );
        assert_eq!(
            boxes,
            [
                Rect {
                    x: 0.0,
                    y: 7.0,
                    width: 8.0,
                    height: 6.0
                },
                Rect {
                    x: 32.0,
                    y: 5.0,
                    width: 8.0,
                    height: 10.0
                }
            ]
        );
    }

    #[test]
    fn flex_grow_and_weighted_shrink_distribute_main_size() {
        let grow = colored_boxes(
            "<div style='display:flex;width:40px'><div style='width:10px;height:4px;flex-grow:1;background:red'></div><div style='width:10px;height:4px;flex-grow:3;background:green'></div></div>",
        );
        assert_eq!(
            grow.iter()
                .map(|rect| (rect.x, rect.width))
                .collect::<Vec<_>>(),
            [(0.0, 15.0), (15.0, 25.0)]
        );
        let shrink = colored_boxes(
            "<div style='display:flex;width:10px'><div style='width:8px;height:4px;background:red'></div><div style='width:8px;height:4px;background:green'></div></div>",
        );
        assert_eq!(
            shrink
                .iter()
                .map(|rect| (rect.x, rect.width))
                .collect::<Vec<_>>(),
            [(0.0, 5.0), (5.0, 5.0)]
        );
    }

    #[test]
    fn flex_column_reverse_and_auto_height_stretch() {
        let column = colored_boxes(
            "<div style='display:flex;flex-direction:column-reverse;width:20px;height:20px;gap:2px;align-items:center'><div style='width:4px;height:4px;background:red'></div><div style='width:8px;height:6px;background:green'></div></div>",
        );
        assert_eq!(
            column,
            [
                Rect {
                    x: 8.0,
                    y: 16.0,
                    width: 4.0,
                    height: 4.0
                },
                Rect {
                    x: 6.0,
                    y: 8.0,
                    width: 8.0,
                    height: 6.0
                }
            ]
        );
        let stretched = colored_boxes(
            "<div style='display:flex;width:20px'><div style='width:5px;height:4px;background:red'></div><div style='width:5px;background:green'></div></div>",
        );
        assert_eq!(
            stretched.iter().map(|rect| rect.height).collect::<Vec<_>>(),
            [4.0, 4.0]
        );
    }

    #[test]
    fn border_box_sizes_include_padding_border_and_flex_allocations() {
        let boxes = colored_boxes(
            "<div style='box-sizing:border-box;width:8px;height:8px;padding:2px;border:1px solid blue;background:red'></div>",
        );
        assert_eq!(
            boxes,
            [Rect {
                x: 0.0,
                y: 0.0,
                width: 8.0,
                height: 8.0
            }]
        );
        let flex = colored_boxes(
            "<div style='display:flex;width:40px'><div style='box-sizing:border-box;width:20px;height:8px;padding:2px;border:1px solid blue;background:red'></div><div style='box-sizing:border-box;width:20px;height:8px;padding:2px;border:1px solid blue;background:green'></div></div>",
        );
        assert_eq!(
            flex.iter()
                .map(|rect| (rect.x, rect.width, rect.height))
                .collect::<Vec<_>>(),
            [(0.0, 20.0, 8.0), (20.0, 20.0, 8.0)]
        );
    }

    #[test]
    fn normal_whitespace_collapses_but_nbsp_does_not_break() {
        let plain = crate::html::parse("<p>a b</p>", 16).unwrap();
        let spaced = crate::html::parse("<p> \t a\n\r  b</p>", 16).unwrap();
        assert_eq!(
            display_list(&plain, 100, 30, &FixedText).unwrap(),
            display_list(&spaced, 100, 30, &FixedText).unwrap()
        );
        let nbsp = crate::html::parse("<p style='width:6px'>a&nbsp;b</p>", 16).unwrap();
        let list = display_list(&nbsp, 100, 30, &FixedText).unwrap();
        assert_eq!(
            list.0
                .iter()
                .filter(|c| matches!(c, Command::GlyphRun { .. }))
                .count(),
            1
        );
        assert!(matches!(collapsed_text("unchanged"), Cow::Borrowed(_)));
    }

    #[test]
    fn explicit_break_starts_next_line_and_hidden_break_does_not() {
        for (markup, expected) in [
            ("<p>a<br>b</p>", [(0.0, 2.0), (0.0, 5.0)]),
            (
                "<p>a<br style='display:none'>b</p>",
                [(0.0, 2.0), (2.0, 2.0)],
            ),
        ] {
            let document = crate::html::parse(markup, 16).unwrap();
            let list = display_list(&document, 100, 30, &FixedText).unwrap();
            let runs: Vec<_> = list
                .0
                .iter()
                .filter_map(|command| match command {
                    Command::GlyphRun {
                        origin_x,
                        baseline_y,
                        ..
                    } => Some((*origin_x, *baseline_y)),
                    _ => None,
                })
                .collect();
            assert_eq!(runs, expected);
        }
    }

    #[test]
    fn unitless_line_height_inherits_and_centers_glyphs() {
        let document = crate::html::parse(
            "<div style='line-height:2'><p style='font-size:4px'>a<br>b</p></div>",
            16,
        )
        .unwrap();
        let list = display_list(&document, 100, 30, &FixedText).unwrap();
        let baselines: Vec<_> = list
            .0
            .iter()
            .filter_map(|command| match command {
                Command::GlyphRun { baseline_y, .. } => Some(*baseline_y),
                _ => None,
            })
            .collect();
        assert_eq!(baselines, [4.5, 12.5]);
    }

    #[test]
    fn template_subtrees_are_inert_for_style_paint_and_inline_flow() {
        let document = crate::html::parse(
            "<p>a<template><style>p{background:red}</style><b>hidden</b></template>b</p>",
            24,
        )
        .unwrap();
        let list = display_list(&document, 100, 30, &FixedText).unwrap();
        let runs: Vec<_> = list
            .0
            .iter()
            .filter_map(|command| match command {
                Command::GlyphRun {
                    origin_x,
                    baseline_y,
                    ..
                } => Some((*origin_x, *baseline_y)),
                _ => None,
            })
            .collect();
        assert_eq!(runs, [(0.0, 2.0), (2.0, 2.0)]);
        assert!(!list.0.iter().any(|command| matches!(command, Command::FillRect { color, .. } if color.r == 255 && color.g == 0)));
    }

    #[test]
    fn foreign_namespace_elements_are_inert() {
        let mut document = crate::html::parse("<body></body>", 16).unwrap();
        let body = crate::selector::query_selector(&document, document.root(), "body")
            .unwrap()
            .unwrap();
        let foreign = document
            .create(NodeKind::Element {
                namespace: Namespace::Svg,
                name: "div".into(),
                attributes: alloc::vec![(
                    "style".into(),
                    "background:red;width:10px;height:10px".into()
                )],
            })
            .unwrap();
        document.append(body, foreign).unwrap();
        let text = document.create(NodeKind::Text("inert".into())).unwrap();
        document.append(foreign, text).unwrap();
        let list = display_list(&document, 20, 20, &NoShape).unwrap();
        assert_eq!(list.0.len(), 1);
    }

    impl TextShaper for FixedText {
        fn shape(&self, text: &str, _: f32) -> Result<ShapedRun, ()> {
            Ok(ShapedRun {
                glyphs: Vec::new(),
                width: text.len() as f32 * 2.0,
            })
        }
        fn ascent(&self, _: f32) -> f32 {
            2.0
        }
        fn line_height(&self, _: f32) -> f32 {
            3.0
        }
    }

    #[test]
    fn inline_text_and_elements_share_a_line() {
        let document =
            crate::html::parse("<p>Hello <strong>world</strong>!</p><div>next</div>", 16).unwrap();
        let list = display_list(&document, 100, 30, &FixedText).unwrap();
        let runs: Vec<_> = list
            .0
            .iter()
            .filter_map(|command| match command {
                Command::GlyphRun {
                    origin_x,
                    baseline_y,
                    ..
                } => Some((*origin_x, *baseline_y)),
                _ => None,
            })
            .collect();
        assert_eq!(runs, [(0.0, 2.0), (12.0, 2.0), (22.0, 2.0), (0.0, 5.0)]);
    }

    #[test]
    fn text_and_inline_elements_wrap_at_box_width() {
        let document = crate::html::parse(
            "<p style='width:10px'>aaa <strong>bbbb</strong> cccc</p>",
            12,
        )
        .unwrap();
        let list = display_list(&document, 40, 30, &FixedText).unwrap();
        let runs: Vec<_> = list
            .0
            .iter()
            .filter_map(|command| match command {
                Command::GlyphRun {
                    origin_x,
                    baseline_y,
                    ..
                } => Some((*origin_x, *baseline_y)),
                _ => None,
            })
            .collect();
        assert_eq!(runs, [(0.0, 2.0), (0.0, 5.0), (8.0, 5.0), (0.0, 8.0)]);
    }

    #[test]
    fn rounded_background_bleed_inset_requires_opaque_continuous_border() {
        for (border, inset) in [
            ("2px solid #0080ff", 1.0),
            ("2px solid rgba(0,128,255,0.5)", 0.0),
            ("2px dashed #0080ff", 0.0),
            ("2px dotted #0080ff", 0.0),
        ] {
            let html = alloc::format!(
                "<div style='width:20px;border:{border};border-radius:6px;background:#ff8000'><div style='height:16px'></div></div>"
            );
            let document = crate::html::parse(&html, 16).unwrap();
            let list = display_list(&document, 40, 40, &FixedText).unwrap();
            let (rect, radius) = list
                .0
                .iter()
                .find_map(|command| match command {
                    Command::FillRoundedRect {
                        rect,
                        radius,
                        color,
                    } if color.r == 255 && color.g == 128 => Some((*rect, *radius)),
                    _ => None,
                })
                .unwrap();
            assert_eq!(
                rect,
                Rect {
                    x: inset,
                    y: inset,
                    width: 24.0 - 2.0 * inset,
                    height: 20.0 - 2.0 * inset
                },
                "{border}"
            );
            assert_eq!(radius, 6.0 - inset, "{border}");
            assert!(
                !list
                    .0
                    .iter()
                    .any(|command| matches!(command, Command::PushLayer { .. }))
            );
        }
    }

    #[test]
    fn grid_repeat_minmax_percent_implicit_flow_and_alignment() {
        let boxes = colored_boxes(
            "<div style='display:grid;width:100px;grid-template-columns:repeat(2,minmax(0,1fr));grid-auto-rows:10px;gap:4px'><div style='background:red'></div><div style='background:green'></div><div style='background:#ff8000'></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|rect| (rect.x, rect.y, rect.width, rect.height))
                .collect::<Vec<_>>(),
            [
                (0.0, 0.0, 48.0, 10.0),
                (52.0, 0.0, 48.0, 10.0),
                (0.0, 14.0, 48.0, 10.0)
            ]
        );
        let boxes = colored_boxes(
            "<div style='display:grid;width:100px;grid-template-columns:25% 1fr;grid-template-rows:10px 10px;grid-auto-flow:column'><div style='background:red'></div><div style='background:green'></div><div style='background:#ff8000'></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|rect| (rect.x, rect.y, rect.width))
                .collect::<Vec<_>>(),
            [(0.0, 0.0, 25.0), (0.0, 10.0, 25.0), (25.0, 0.0, 75.0)]
        );
        let boxes = colored_boxes(
            "<div style='display:grid;width:100px;height:40px;grid-template-columns:20px 20px;grid-template-rows:10px;justify-content:center;align-content:end;justify-items:center;align-items:center'><div style='width:10px;height:4px;background:red'></div></div>",
        );
        assert_eq!(
            (boxes[0].x, boxes[0].y, boxes[0].width, boxes[0].height),
            (35.0, 33.0, 10.0, 4.0)
        );
    }

    #[test]
    fn grid_named_lines_areas_rtl_and_subgrid_inherit_tracks() {
        let boxes = colored_boxes(
            "<div style='display:grid;width:100px;grid-template-columns:[a] 30px [b] 70px [c];grid-template-rows:10px'><div style='grid-column:b / c;background:red'></div></div>",
        );
        assert_eq!((boxes[0].x, boxes[0].width), (30.0, 70.0));
        let boxes = colored_boxes(
            "<div style='display:grid;width:100px;grid-template-columns:30px 70px;grid-template-rows:10px 20px;grid-template-areas:\"top top\" \"left right\"'><div style='grid-area:right;background:red'></div></div>",
        );
        assert_eq!(
            (boxes[0].x, boxes[0].y, boxes[0].width, boxes[0].height),
            (30.0, 10.0, 70.0, 20.0)
        );
        let boxes = colored_boxes(
            "<div style='display:grid;direction:rtl;width:100px;grid-template-columns:30px 70px;grid-template-rows:10px'><div style='background:red'></div><div style='background:green'></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|rect| (rect.x, rect.width))
                .collect::<Vec<_>>(),
            [(70.0, 30.0), (0.0, 70.0)]
        );
        let boxes = colored_boxes(
            "<div style='display:grid;width:100px;grid-template-columns:30px 70px;grid-template-rows:10px'><div style='display:grid;grid-column:1 / 3;grid-template-columns:subgrid;grid-template-rows:subgrid'><div style='background:red'></div><div style='background:green'></div></div></div>",
        );
        assert_eq!(
            boxes
                .iter()
                .map(|rect| (rect.x, rect.width, rect.height))
                .collect::<Vec<_>>(),
            [(0.0, 30.0, 10.0), (30.0, 70.0, 10.0)]
        );
    }
}
