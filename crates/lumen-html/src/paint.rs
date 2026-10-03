//! CSS-pixel display list shared by the image and window backends.
use alloc::{sync::Arc, vec::Vec};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Glyph {
    pub id: u16,
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShapedRun {
    pub glyphs: Vec<Glyph>,
    pub width: f32,
}

#[derive(Debug, PartialEq)]
pub struct ImageData {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl ImageData {
    pub fn is_valid(&self) -> bool {
        self.width != 0
            && self.height != 0
            && (self.width as usize)
                .checked_mul(self.height as usize)
                .and_then(|size| size.checked_mul(4))
                == Some(self.pixels.len())
    }
}

pub trait TextShaper {
    fn shape(&self, text: &str, size: f32) -> Result<ShapedRun, ()>;
    fn shape_directional(&self, text: &str, size: f32, _rtl: bool) -> Result<ShapedRun, ()> {
        self.shape(text, size)
    }
    fn measure(&self, text: &str, size: f32) -> Result<f32, ()> {
        self.shape(text, size).map(|run| run.width)
    }
    fn ascent(&self, size: f32) -> f32;
    fn line_height(&self, size: f32) -> f32;
    fn underline_metrics(&self, size: f32) -> (f32, f32) {
        (size * 0.1, size / 16.0)
    }
    fn strike_metrics(&self, size: f32) -> (f32, f32) {
        (-size * 0.3, size / 16.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn contains_rounded(self, x: f32, y: f32, radius: f32) -> bool {
        if x < self.x || y < self.y || x >= self.x + self.width || y >= self.y + self.height {
            return false;
        }
        let radius = radius.min(self.width * 0.5).min(self.height * 0.5);
        if radius <= 0.0 {
            return true;
        }
        let dx = x - x.clamp(self.x + radius, self.x + self.width - radius);
        let dy = y - y.clamp(self.y + radius, self.y + self.height - radius);
        dx * dx + dy * dy <= radius * radius
    }
    pub fn intersection(self, other: Self) -> Option<Self> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = (self.x + self.width).min(other.x + other.width);
        let bottom = (self.y + self.height).min(other.y + other.height);
        (right > x && bottom > y).then_some(Self {
            x,
            y,
            width: right - x,
            height: bottom - y,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Gradient {
    pub kind: GradientKind,
    pub repeating: bool,
    pub stops: Arc<[GradientStop]>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GradientKind {
    /// CSS degrees: zero points up; corner directions use box-dependent angles.
    Linear {
        angle: f32,
        corner: Option<(i8, i8)>,
    },
    Radial {
        shape: RadialShape,
        size: RadialSize,
        center: [LengthPercentage; 2],
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RadialShape {
    Circle,
    Ellipse,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RadialSize {
    ClosestSide,
    FarthestSide,
    ClosestCorner,
    FarthestCorner,
    Radii([LengthPercentage; 2]),
}
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LengthPercentage {
    pub pixels: f32,
    pub fraction: f32,
}
impl LengthPercentage {
    pub fn resolve(self, basis: f32) -> f32 {
        self.pixels + self.fraction * basis
    }
    fn is_finite(self) -> bool {
        self.pixels.is_finite() && self.fraction.is_finite()
    }
}
impl Gradient {
    pub fn is_valid(&self) -> bool {
        let valid_kind = match self.kind {
            GradientKind::Linear { angle, corner } => {
                angle.is_finite()
                    && !corner.is_some_and(|(x, y)| !matches!(x, -1 | 1) || !matches!(y, -1 | 1))
            }
            GradientKind::Radial {
                shape,
                size,
                center,
            } => {
                center.iter().all(|v| v.is_finite())
                    && match size {
                        RadialSize::Radii(radii) => {
                            radii
                                .iter()
                                .all(|v| v.is_finite() && (v.fraction != 0.0 || v.pixels >= 0.0))
                                && (shape != RadialShape::Circle
                                    || radii[0] == radii[1] && radii[0].fraction == 0.0)
                        }
                        _ => true,
                    }
            }
        };
        valid_kind
            && (2..=32).contains(&self.stops.len())
            && self.stops.iter().all(|stop| match stop.position {
                Some(GradientPosition::Fraction(v) | GradientPosition::Pixels(v)) => v.is_finite(),
                Some(GradientPosition::Mixed(v)) => v.is_finite(),
                None => true,
            })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackgroundRepeat {
    Repeat,
    NoRepeat,
    Space,
    Round,
}
#[derive(Clone, Debug, PartialEq)]
pub enum BackgroundPaint {
    Gradient(Arc<Gradient>),
    Image(Arc<ImageData>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GradientPosition {
    Fraction(f32),
    Pixels(f32),
    Mixed(LengthPercentage),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GradientStop {
    pub color: Rgba,
    pub position: Option<GradientPosition>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoxShadow {
    pub offset_x: f32,
    pub offset_y: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: Rgba,
    pub inset: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BorderPattern {
    Dashed,
    Dotted,
}

impl BoxShadow {
    pub fn bounds(self, rect: Rect) -> Rect {
        if self.inset {
            return rect;
        }
        let extent = self.spread + self.blur * 1.5 + 1.0;
        Rect {
            x: rect.x + self.offset_x - extent,
            y: rect.y + self.offset_y - extent,
            width: (rect.width + extent * 2.0).max(0.0),
            height: (rect.height + extent * 2.0).max(0.0),
        }
    }
}

/// CSS two-dimensional affine matrix, in column-vector order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}
impl Default for Affine {
    fn default() -> Self {
        Self::IDENTITY
    }
}
impl Affine {
    pub const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };
    pub fn apply(self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }
    pub fn then(self, rhs: Self) -> Self {
        Self {
            a: self.a * rhs.a + self.c * rhs.b,
            b: self.b * rhs.a + self.d * rhs.b,
            c: self.a * rhs.c + self.c * rhs.d,
            d: self.b * rhs.c + self.d * rhs.d,
            e: self.a * rhs.e + self.c * rhs.f + self.e,
            f: self.b * rhs.e + self.d * rhs.f + self.f,
        }
    }
    pub fn inverse(self) -> Option<Self> {
        let det = self.a * self.d - self.b * self.c;
        if det == 0.0 || !det.is_finite() {
            return None;
        }
        let result = Self {
            a: self.d / det,
            b: -self.b / det,
            c: -self.c / det,
            d: self.a / det,
            e: (self.c * self.f - self.d * self.e) / det,
            f: (self.b * self.e - self.a * self.f) / det,
        };
        result.is_finite().then_some(result)
    }
    pub fn is_finite(self) -> bool {
        [self.a, self.b, self.c, self.d, self.e, self.f]
            .iter()
            .all(|v| v.is_finite())
    }
    pub fn translated_space(self, x: f32, y: f32) -> Self {
        Self {
            e: self.e + x - self.a * x - self.c * y,
            f: self.f + y - self.b * x - self.d * y,
            ..self
        }
    }
    pub fn bounds(self, rect: Rect) -> Rect {
        let points = [
            self.apply(rect.x, rect.y),
            self.apply(rect.x + rect.width, rect.y),
            self.apply(rect.x, rect.y + rect.height),
            self.apply(rect.x + rect.width, rect.y + rect.height),
        ];
        let (mut x, mut y, mut right, mut bottom) =
            (points[0].0, points[0].1, points[0].0, points[0].1);
        for (px, py) in points {
            x = x.min(px);
            y = y.min(py);
            right = right.max(px);
            bottom = bottom.max(py);
        }
        Rect {
            x,
            y,
            width: right - x,
            height: bottom - y,
        }
    }
}

#[cfg(test)]
mod affine_tests {
    use super::*;
    #[test]
    fn inverse_and_coordinate_translation_preserve_geometry() {
        let matrix = Affine {
            a: 0.0,
            b: 2.0,
            c: -3.0,
            d: 0.0,
            e: 12.0,
            f: 4.0,
        };
        let point = matrix.apply(5.0, 7.0);
        assert_eq!(
            matrix.inverse().unwrap().apply(point.0, point.1),
            (5.0, 7.0)
        );
        let shifted = matrix.translated_space(20.0, -6.0).apply(25.0, 1.0);
        assert_eq!(shifted, (point.0 + 20.0, point.1 - 6.0));
        assert_eq!(matrix.then(matrix.inverse().unwrap()), Affine::IDENTITY);
        assert!(
            Affine {
                a: 0.0,
                b: 0.0,
                ..Affine::IDENTITY
            }
            .inverse()
            .is_none()
        );
    }
    #[test]
    fn transform_scope_requires_matching_pop() {
        assert_eq!(
            DisplayList(alloc::vec![
                Command::PushTransform(Affine::IDENTITY),
                Command::PopClip
            ])
            .validate(),
            Err(ReplayError::UnbalancedClip)
        );
        assert!(
            DisplayList(alloc::vec![
                Command::PushTransform(Affine::IDENTITY),
                Command::PushClip(Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 1.0,
                    height: 1.0
                }),
                Command::PopClip,
                Command::PopTransform
            ])
            .validate()
            .is_ok()
        );
        assert_eq!(
            DisplayList(alloc::vec![
                Command::PushTransform(Affine {
                    e: f32::NAN,
                    ..Affine::IDENTITY
                }),
                Command::PopTransform
            ])
            .validate(),
            Err(ReplayError::InvalidGeometry)
        );
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    PushTransform(Affine),
    PopTransform,
    PushClip(Rect),
    PopClip,
    PushLayer {
        rect: Rect,
        radius: f32,
        opacity: f32,
        clip: bool,
    },
    PopLayer,
    FillRect {
        rect: Rect,
        color: Rgba,
    },
    FillRoundedRect {
        rect: Rect,
        radius: f32,
        color: Rgba,
    },
    FillGradient {
        rect: Rect,
        radius: f32,
        gradient: Arc<Gradient>,
    },
    FillBackground {
        rect: Rect,
        radius: f32,
        positioning_rect: Rect,
        image_rect: Rect,
        repeat: [BackgroundRepeat; 2],
        image: BackgroundPaint,
    },
    BoxShadow {
        rect: Rect,
        radius: f32,
        shadow: BoxShadow,
    },
    StrokeBorder {
        rect: Rect,
        radius: f32,
        width: f32,
        color: Rgba,
    },
    StrokePatternBorder {
        rect: Rect,
        radius: f32,
        width: f32,
        color: Rgba,
        pattern: BorderPattern,
    },
    GlyphRun {
        origin_x: f32,
        baseline_y: f32,
        size: f32,
        color: Rgba,
        glyphs: Vec<Glyph>,
    },
    Image {
        rect: Rect,
        image: Arc<ImageData>,
    },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DisplayList(pub Vec<Command>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayError {
    UnbalancedClip,
    InvalidGeometry,
    ClipLimit,
    InvalidImage,
    UnsupportedLayer,
}

pub trait ReplaySink {
    fn push_clip(&mut self, rect: Rect);
    fn pop_clip(&mut self);
    fn fill_rect(&mut self, rect: Rect, color: Rgba);
    fn fill_rounded_rect(&mut self, rect: Rect, radius: f32, color: Rgba);
    fn fill_gradient(&mut self, rect: Rect, radius: f32, gradient: &Gradient);
    fn fill_background(
        &mut self,
        rect: Rect,
        radius: f32,
        positioning_rect: Rect,
        image_rect: Rect,
        repeat: [BackgroundRepeat; 2],
        image: &BackgroundPaint,
    );
    fn draw_shadow(&mut self, rect: Rect, radius: f32, shadow: BoxShadow);
    fn stroke_border(&mut self, rect: Rect, radius: f32, width: f32, color: Rgba);
    fn stroke_pattern_border(
        &mut self,
        rect: Rect,
        radius: f32,
        width: f32,
        color: Rgba,
        pattern: BorderPattern,
    );
    fn draw_glyphs(
        &mut self,
        origin_x: f32,
        baseline_y: f32,
        size: f32,
        color: Rgba,
        glyphs: &[Glyph],
    );
    fn draw_image(&mut self, rect: Rect, image: &ImageData);
}

impl DisplayList {
    /// Validate the complete list before forwarding paint to a backend.
    pub fn validate(&self) -> Result<(), ReplayError> {
        let mut depth = 0usize;
        let mut scopes = [0u8; 256];
        for command in &self.0 {
            match command {
                Command::PushTransform(matrix) => {
                    if !matrix.is_finite() {
                        return Err(ReplayError::InvalidGeometry);
                    }
                    if depth == scopes.len() {
                        return Err(ReplayError::ClipLimit);
                    }
                    scopes[depth] = 2;
                    depth += 1;
                }
                Command::PushClip(rect)
                | Command::PushLayer { rect, .. }
                | Command::FillRect { rect, .. }
                | Command::FillRoundedRect { rect, .. }
                | Command::FillGradient { rect, .. }
                | Command::FillBackground { rect, .. }
                | Command::BoxShadow { rect, .. }
                | Command::StrokeBorder { rect, .. }
                | Command::StrokePatternBorder { rect, .. }
                | Command::Image { rect, .. } => {
                    if ![rect.x, rect.y, rect.width, rect.height]
                        .iter()
                        .all(|n| n.is_finite())
                        || rect.width < 0.0
                        || rect.height < 0.0
                    {
                        return Err(ReplayError::InvalidGeometry);
                    }
                    if matches!(command, Command::PushClip(_) | Command::PushLayer { .. }) {
                        if depth == scopes.len() {
                            return Err(ReplayError::ClipLimit);
                        }
                        scopes[depth] = u8::from(matches!(command, Command::PushLayer { .. }));
                        depth += 1;
                        if depth > 256 {
                            return Err(ReplayError::ClipLimit);
                        }
                    }
                    if let Command::PushLayer {
                        radius, opacity, ..
                    } = command
                    {
                        if !radius.is_finite()
                            || *radius < 0.0
                            || !opacity.is_finite()
                            || !(0.0..=1.0).contains(opacity)
                        {
                            return Err(ReplayError::InvalidGeometry);
                        }
                    }
                    if let Command::Image { image, .. } = command {
                        if !image.is_valid() {
                            return Err(ReplayError::InvalidImage);
                        }
                    }
                    if let Command::FillRoundedRect { radius, .. } = command {
                        if !radius.is_finite() || *radius < 0.0 {
                            return Err(ReplayError::InvalidGeometry);
                        }
                    }
                    if let Command::FillGradient {
                        radius, gradient, ..
                    } = command
                    {
                        if !radius.is_finite() || *radius < 0.0 || !gradient.is_valid() {
                            return Err(ReplayError::InvalidGeometry);
                        }
                    }
                    if let Command::FillBackground {
                        radius,
                        positioning_rect,
                        image_rect,
                        image,
                        ..
                    } = command
                    {
                        if !radius.is_finite()
                            || *radius < 0.0
                            || [positioning_rect, image_rect].iter().any(|r| {
                                ![r.x, r.y, r.width, r.height].iter().all(|v| v.is_finite())
                                    || r.width < 0.0
                                    || r.height < 0.0
                            })
                        {
                            return Err(ReplayError::InvalidGeometry);
                        }
                        match image {
                            BackgroundPaint::Gradient(gradient) if !gradient.is_valid() => {
                                return Err(ReplayError::InvalidGeometry);
                            }
                            BackgroundPaint::Image(image) if !image.is_valid() => {
                                return Err(ReplayError::InvalidImage);
                            }
                            _ => {}
                        }
                    }
                    if let Command::StrokeBorder { radius, width, .. }
                    | Command::StrokePatternBorder { radius, width, .. } = command
                    {
                        if !radius.is_finite()
                            || *radius < 0.0
                            || !width.is_finite()
                            || *width < 0.0
                        {
                            return Err(ReplayError::InvalidGeometry);
                        }
                    }
                    if let Command::BoxShadow { radius, shadow, .. } = command {
                        if !radius.is_finite()
                            || *radius < 0.0
                            || !shadow.blur.is_finite()
                            || shadow.blur < 0.0
                            || ![shadow.offset_x, shadow.offset_y, shadow.spread]
                                .iter()
                                .all(|value| value.is_finite())
                        {
                            return Err(ReplayError::InvalidGeometry);
                        }
                    }
                }
                Command::PopClip | Command::PopLayer | Command::PopTransform => {
                    depth = depth.checked_sub(1).ok_or(ReplayError::UnbalancedClip)?;
                    let kind = match command {
                        Command::PopTransform => 2,
                        Command::PopLayer => 1,
                        _ => 0,
                    };
                    if scopes[depth] != kind {
                        return Err(ReplayError::UnbalancedClip);
                    }
                }
                Command::GlyphRun {
                    origin_x,
                    baseline_y,
                    size,
                    glyphs,
                    ..
                } => {
                    if !origin_x.is_finite()
                        || !baseline_y.is_finite()
                        || !size.is_finite()
                        || *size <= 0.0
                        || glyphs
                            .iter()
                            .any(|glyph| !glyph.x.is_finite() || !glyph.y.is_finite())
                    {
                        return Err(ReplayError::InvalidGeometry);
                    }
                }
            }
        }
        if depth != 0 {
            return Err(ReplayError::UnbalancedClip);
        }
        Ok(())
    }

    pub fn replay(&self, sink: &mut impl ReplaySink) -> Result<(), ReplayError> {
        self.validate()?;
        if self.0.iter().any(|command| {
            matches!(
                command,
                Command::PushLayer { .. } | Command::PushTransform(_)
            )
        }) {
            return Err(ReplayError::UnsupportedLayer);
        }
        for command in &self.0 {
            match *command {
                Command::PushClip(rect) => sink.push_clip(rect),
                Command::PopClip => sink.pop_clip(),
                Command::PushLayer { .. }
                | Command::PopLayer
                | Command::PushTransform(_)
                | Command::PopTransform => {
                    return Err(ReplayError::UnsupportedLayer);
                }
                Command::FillRect { rect, color }
                    if rect.width > 0.0 && rect.height > 0.0 && color.a > 0 =>
                {
                    sink.fill_rect(rect, color)
                }
                Command::FillRect { .. } => {}
                Command::FillRoundedRect {
                    rect,
                    radius,
                    color,
                } if rect.width > 0.0 && rect.height > 0.0 && color.a > 0 => {
                    sink.fill_rounded_rect(rect, radius, color)
                }
                Command::FillRoundedRect { .. } => {}
                Command::FillGradient {
                    rect,
                    radius,
                    ref gradient,
                } => sink.fill_gradient(rect, radius, gradient),
                Command::FillBackground {
                    rect,
                    radius,
                    positioning_rect,
                    image_rect,
                    repeat,
                    ref image,
                } => {
                    sink.fill_background(rect, radius, positioning_rect, image_rect, repeat, image)
                }
                Command::BoxShadow {
                    rect,
                    radius,
                    shadow,
                } => sink.draw_shadow(rect, radius, shadow),
                Command::StrokeBorder {
                    rect,
                    radius,
                    width,
                    color,
                } if rect.width > 0.0 && rect.height > 0.0 && width > 0.0 && color.a > 0 => {
                    sink.stroke_border(rect, radius, width, color)
                }
                Command::StrokeBorder { .. } => {}
                Command::StrokePatternBorder {
                    rect,
                    radius,
                    width,
                    color,
                    pattern,
                } => sink.stroke_pattern_border(rect, radius, width, color, pattern),
                Command::GlyphRun {
                    origin_x,
                    baseline_y,
                    size,
                    color,
                    ref glyphs,
                } => sink.draw_glyphs(origin_x, baseline_y, size, color, glyphs),
                Command::Image { rect, ref image } => sink.draw_image(rect, image),
            }
        }
        Ok(())
    }
}
