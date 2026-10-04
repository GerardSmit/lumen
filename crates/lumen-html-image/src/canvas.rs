//! Shared bitmap drawing core for DOM and offscreen canvases.
//! Paths, coverage, clipping and compositing use tiny-skia's rasterizer.
use super::{ImageError, Rgba8Image, MAX_IMAGE_BYTES};
use lumen_html::paint::{FontSpec, ShapedRun};
use lumen_html_text::{CanvasTextOptions, FontProvider, GlyphOutlineCommand};
use std::{cell::RefCell, rc::Rc};
use tiny_skia::{
    BlendMode, Color, FillRule, FilterQuality, GradientStop, LinearGradient, Mask, Paint, Path,
    PathBuilder, Pattern, Pixmap, PixmapPaint, Point, RadialGradient, Rect, Shader, SpreadMode,
    Stroke, Transform,
};

const MAX_SVG_PATH_BYTES: usize = 1024 * 1024;
const MAX_SVG_PATH_SEGMENTS: usize = 65_536;

#[derive(Clone)]
pub struct ParsedSvgPath {
    pub path: Option<Path>,
    pub current: (f32, f32),
    pub subpath_start: (f32, f32),
}

/// Parses SVG path data into the shared vector path representation used by
/// Canvas and the HTML image rasterizer.
pub fn parse_svg_path(data: &str) -> Result<ParsedSvgPath, &'static str> {
    if data.len() > MAX_SVG_PATH_BYTES {
        return Err("SVG path data is too large");
    }
    SvgPathParser::new(data).parse()
}

/// Resize a straight-alpha raster image using tiny-skia's shared sampling
/// implementation. The result remains straight-alpha for ImageData and
/// image-source consumers.
pub fn resize_image(
    image: &Rgba8Image,
    width: u32,
    height: u32,
    quality: FilterQuality,
) -> Result<Rgba8Image, ImageError> {
    if image.width == 0 || image.height == 0 || width == 0 || height == 0 {
        return Err(ImageError::InvalidViewport);
    }
    let source = premultiplied_pixmap(image)?;
    let mut destination = Pixmap::new(width, height).ok_or(ImageError::TooLarge)?;
    let paint = PixmapPaint {
        quality,
        ..PixmapPaint::default()
    };
    destination.draw_pixmap(
        0,
        0,
        source.as_ref(),
        &paint,
        Transform::from_scale(
            width as f32 / image.width as f32,
            height as f32 / image.height as f32,
        ),
        None,
    );
    let mut pixels = Vec::with_capacity(pixel_bytes(width, height)?);
    for pixel in destination.pixels() {
        let straight = pixel.demultiply();
        pixels.extend_from_slice(&[
            straight.red(),
            straight.green(),
            straight.blue(),
            straight.alpha(),
        ]);
    }
    Ok(Rgba8Image {
        width,
        height,
        pixels,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageResizeQuality {
    Pixelated,
    Low,
    Medium,
    High,
}

pub fn resize_image_with_quality(
    image: &Rgba8Image,
    width: u32,
    height: u32,
    quality: ImageResizeQuality,
) -> Result<Rgba8Image, ImageError> {
    match quality {
        ImageResizeQuality::Low => resize_image(image, width, height, FilterQuality::Nearest),
        ImageResizeQuality::Medium => resize_image(image, width, height, FilterQuality::Bilinear),
        ImageResizeQuality::High => resize_image(image, width, height, FilterQuality::Bicubic),
        ImageResizeQuality::Pixelated => {
            let multiple = |target: u32, source: u32| -> Result<u32, ImageError> {
                let count = ((f64::from(target) / f64::from(source)).round() as u32).max(1);
                source.checked_mul(count).ok_or(ImageError::TooLarge)
            };
            let intermediate_width = multiple(width, image.width)?;
            let intermediate_height = multiple(height, image.height)?;
            let enlarged = resize_image(
                image,
                intermediate_width,
                intermediate_height,
                FilterQuality::Nearest,
            )?;
            if (intermediate_width, intermediate_height) == (width, height) {
                Ok(enlarged)
            } else {
                resize_image(&enlarged, width, height, FilterQuality::Bilinear)
            }
        }
    }
}

/// Resize RGBA channels independently for image sources whose alpha must stay
/// unpremultiplied during resampling. Normal Canvas resizes should use
/// `resize_image_with_quality`, which filters premultiplied colors.
pub fn resize_image_straight_with_quality(
    image: &Rgba8Image,
    width: u32,
    height: u32,
    quality: ImageResizeQuality,
) -> Result<Rgba8Image, ImageError> {
    if image.width == 0 || image.height == 0 || width == 0 || height == 0 {
        return Err(ImageError::InvalidViewport);
    }
    if image.pixels.len() != pixel_bytes(image.width, image.height)? {
        return Err(ImageError::InvalidViewport);
    }
    let source = image::RgbaImage::from_raw(image.width, image.height, image.pixels.clone())
        .ok_or(ImageError::InvalidViewport)?;
    let filter = match quality {
        ImageResizeQuality::Low | ImageResizeQuality::Pixelated => {
            image::imageops::FilterType::Nearest
        }
        ImageResizeQuality::Medium => image::imageops::FilterType::Triangle,
        ImageResizeQuality::High => image::imageops::FilterType::Lanczos3,
    };
    let output = image::imageops::resize(&source, width, height, filter);
    Ok(Rgba8Image {
        width,
        height,
        pixels: output.into_raw(),
    })
}

struct SvgPathParser<'a> {
    data: &'a [u8],
    offset: usize,
    builder: PathBuilder,
    current: (f32, f32),
    subpath: (f32, f32),
    previous_command: u8,
    cubic_control: (f32, f32),
    quad_control: (f32, f32),
    segments: usize,
    last_was_number: bool,
}

impl<'a> SvgPathParser<'a> {
    fn new(data: &'a str) -> Self {
        Self {
            data: data.as_bytes(),
            offset: 0,
            builder: PathBuilder::new(),
            current: (0.0, 0.0),
            subpath: (0.0, 0.0),
            previous_command: 0,
            cubic_control: (0.0, 0.0),
            quad_control: (0.0, 0.0),
            segments: 0,
            last_was_number: false,
        }
    }

    fn parse(mut self) -> Result<ParsedSvgPath, &'static str> {
        let mut command = 0u8;
        let mut had_move = false;
        while self.has_more() {
            self.skip_whitespace();
            if self.offset >= self.data.len() {
                break;
            }
            if self.data[self.offset].is_ascii_alphabetic() {
                command = self.data[self.offset];
                self.offset += 1;
                self.last_was_number = false;
                if !matches!(
                    command.to_ascii_uppercase(),
                    b'M' | b'Z' | b'L' | b'H' | b'V' | b'C' | b'S' | b'Q' | b'T' | b'A'
                ) {
                    return Err("unsupported SVG path command");
                }
                if command.to_ascii_uppercase() == b'Z' {
                    if !had_move {
                        return Err("SVG path close has no subpath");
                    }
                    self.builder.close();
                    self.current = self.subpath;
                    self.previous_command = command;
                    command = 0;
                    self.segment()?;
                    continue;
                }
            } else if command == 0 {
                return Err("SVG path data must begin with a command");
            }

            let relative = command.is_ascii_lowercase();
            let upper = command.to_ascii_uppercase();
            if !had_move && upper != b'M' {
                return Err("SVG path must begin with moveto");
            }
            match upper {
                b'M' => {
                    let (x, y) = self.point(relative)?;
                    self.builder.move_to(x, y);
                    self.current = (x, y);
                    self.subpath = (x, y);
                    had_move = true;
                    self.previous_command = command;
                    self.segment()?;
                    // Subsequent pairs after moveto are implicit lineto.
                    command = if relative { b'l' } else { b'L' };
                }
                b'L' => {
                    let (x, y) = self.point(relative)?;
                    self.builder.line_to(x, y);
                    self.current = (x, y);
                    self.previous_command = command;
                    self.segment()?;
                }
                b'H' => {
                    let value = self.number()?;
                    let x = if relative {
                        self.current.0 + value
                    } else {
                        value
                    };
                    self.builder.line_to(x, self.current.1);
                    self.current.0 = x;
                    self.previous_command = command;
                    self.segment()?;
                }
                b'V' => {
                    let value = self.number()?;
                    let y = if relative {
                        self.current.1 + value
                    } else {
                        value
                    };
                    self.builder.line_to(self.current.0, y);
                    self.current.1 = y;
                    self.previous_command = command;
                    self.segment()?;
                }
                b'C' => {
                    let c1 = self.point(relative)?;
                    let c2 = self.point(relative)?;
                    let end = self.point(relative)?;
                    self.builder.cubic_to(c1.0, c1.1, c2.0, c2.1, end.0, end.1);
                    self.current = end;
                    self.cubic_control = c2;
                    self.previous_command = command;
                    self.segment()?;
                }
                b'S' => {
                    let c1 = if matches!(self.previous_command.to_ascii_uppercase(), b'C' | b'S') {
                        (
                            2.0 * self.current.0 - self.cubic_control.0,
                            2.0 * self.current.1 - self.cubic_control.1,
                        )
                    } else {
                        self.current
                    };
                    let c2 = self.point(relative)?;
                    let end = self.point(relative)?;
                    self.builder.cubic_to(c1.0, c1.1, c2.0, c2.1, end.0, end.1);
                    self.current = end;
                    self.cubic_control = c2;
                    self.previous_command = command;
                    self.segment()?;
                }
                b'Q' => {
                    let control = self.point(relative)?;
                    let end = self.point(relative)?;
                    self.builder.quad_to(control.0, control.1, end.0, end.1);
                    self.current = end;
                    self.quad_control = control;
                    self.previous_command = command;
                    self.segment()?;
                }
                b'T' => {
                    let control =
                        if matches!(self.previous_command.to_ascii_uppercase(), b'Q' | b'T') {
                            (
                                2.0 * self.current.0 - self.quad_control.0,
                                2.0 * self.current.1 - self.quad_control.1,
                            )
                        } else {
                            self.current
                        };
                    let end = self.point(relative)?;
                    self.builder.quad_to(control.0, control.1, end.0, end.1);
                    self.current = end;
                    self.quad_control = control;
                    self.previous_command = command;
                    self.segment()?;
                }
                b'A' => {
                    let rx = self.number()?.abs();
                    let ry = self.number()?.abs();
                    let rotation = self.number()?;
                    let large_arc = self.flag()?;
                    let sweep = self.flag()?;
                    let end = self.point(relative)?;
                    append_svg_arc(
                        &mut self.builder,
                        self.current,
                        end,
                        rx,
                        ry,
                        rotation,
                        large_arc,
                        sweep,
                    )?;
                    self.current = end;
                    self.previous_command = command;
                    self.segment()?;
                }
                _ => return Err("invalid SVG path command"),
            }
        }
        Ok(ParsedSvgPath {
            path: self.builder.finish(),
            current: self.current,
            subpath_start: self.subpath,
        })
    }

    fn has_more(&self) -> bool {
        self.offset < self.data.len()
    }
    fn skip_whitespace(&mut self) {
        while self
            .data
            .get(self.offset)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.offset += 1;
        }
    }
    fn number(&mut self) -> Result<f32, &'static str> {
        self.skip_whitespace();
        if self.data.get(self.offset) == Some(&b',') {
            if !self.last_was_number {
                return Err("SVG path comma is misplaced");
            }
            self.offset += 1;
            self.skip_whitespace();
            if self.offset >= self.data.len()
                || self.data[self.offset] == b','
                || self.data[self.offset].is_ascii_alphabetic()
            {
                return Err("SVG path comma has no following number");
            }
        }
        let start = self.offset;
        if self
            .data
            .get(self.offset)
            .is_some_and(|byte| *byte == b'+' || *byte == b'-')
        {
            self.offset += 1;
        }
        let mut digits = 0;
        while self.data.get(self.offset).is_some_and(u8::is_ascii_digit) {
            self.offset += 1;
            digits += 1;
        }
        if self.data.get(self.offset) == Some(&b'.') {
            self.offset += 1;
            while self.data.get(self.offset).is_some_and(u8::is_ascii_digit) {
                self.offset += 1;
                digits += 1;
            }
        }
        if digits == 0 {
            return Err("invalid SVG path number");
        }
        if self
            .data
            .get(self.offset)
            .is_some_and(|byte| *byte == b'e' || *byte == b'E')
        {
            self.offset += 1;
            if self
                .data
                .get(self.offset)
                .is_some_and(|byte| *byte == b'+' || *byte == b'-')
            {
                self.offset += 1;
            }
            let exponent_start = self.offset;
            while self.data.get(self.offset).is_some_and(u8::is_ascii_digit) {
                self.offset += 1;
            }
            if self.offset == exponent_start {
                return Err("invalid SVG path exponent");
            }
        }
        let text = core::str::from_utf8(&self.data[start..self.offset])
            .map_err(|_| "invalid SVG path number")?;
        let value = text.parse::<f32>().map_err(|_| "invalid SVG path number")?;
        if !value.is_finite() {
            return Err("SVG path number must be finite");
        }
        self.last_was_number = true;
        Ok(value)
    }
    fn flag(&mut self) -> Result<bool, &'static str> {
        self.skip_whitespace();
        if self.data.get(self.offset) == Some(&b',') {
            if !self.last_was_number {
                return Err("SVG arc flag separator is misplaced");
            }
            self.offset += 1;
            self.skip_whitespace();
        }
        let value = match self.data.get(self.offset) {
            Some(b'0') => false,
            Some(b'1') => true,
            _ => return Err("invalid SVG arc flag"),
        };
        self.offset += 1;
        self.last_was_number = true;
        Ok(value)
    }
    fn point(&mut self, relative: bool) -> Result<(f32, f32), &'static str> {
        let x = self.number()?;
        let y = self.number()?;
        let point = if relative {
            (self.current.0 + x, self.current.1 + y)
        } else {
            (x, y)
        };
        if point.0.is_finite() && point.1.is_finite() {
            Ok(point)
        } else {
            Err("SVG path coordinate is out of range")
        }
    }
    fn segment(&mut self) -> Result<(), &'static str> {
        self.segments += 1;
        if self.segments > MAX_SVG_PATH_SEGMENTS {
            Err("SVG path has too many segments")
        } else {
            Ok(())
        }
    }
}

fn append_svg_arc(
    builder: &mut PathBuilder,
    start: (f32, f32),
    end: (f32, f32),
    rx: f32,
    ry: f32,
    rotation_degrees: f32,
    large_arc: bool,
    sweep: bool,
) -> Result<(), &'static str> {
    if start == end {
        return Ok(());
    }
    if rx == 0.0 || ry == 0.0 {
        builder.line_to(end.0, end.1);
        return Ok(());
    }
    let phi = rotation_degrees
        .to_radians()
        .rem_euclid(core::f32::consts::TAU);
    let (sin_phi, cos_phi) = phi.sin_cos();
    let dx = (start.0 - end.0) * 0.5;
    let dy = (start.1 - end.1) * 0.5;
    let x1p = cos_phi * dx + sin_phi * dy;
    let y1p = -sin_phi * dx + cos_phi * dy;
    let mut rx = rx.abs();
    let mut ry = ry.abs();
    let lambda = x1p * x1p / (rx * rx) + y1p * y1p / (ry * ry);
    if lambda > 1.0 {
        let scale = lambda.sqrt();
        rx *= scale;
        ry *= scale;
    }
    let numerator = (rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p).max(0.0);
    let denominator = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
    if denominator == 0.0 || !denominator.is_finite() {
        return Err("invalid SVG arc geometry");
    }
    let sign = if large_arc == sweep { -1.0 } else { 1.0 };
    let coefficient = sign * (numerator / denominator).sqrt();
    let cxp = coefficient * rx * y1p / ry;
    let cyp = -coefficient * ry * x1p / rx;
    let center = (
        cos_phi * cxp - sin_phi * cyp + (start.0 + end.0) * 0.5,
        sin_phi * cxp + cos_phi * cyp + (start.1 + end.1) * 0.5,
    );
    let angle = |ux: f32, uy: f32, vx: f32, vy: f32| (ux * vy - uy * vx).atan2(ux * vx + uy * vy);
    let ux = (x1p - cxp) / rx;
    let uy = (y1p - cyp) / ry;
    let vx = (-x1p - cxp) / rx;
    let vy = (-y1p - cyp) / ry;
    let theta = uy.atan2(ux);
    let mut delta = angle(ux, uy, vx, vy);
    if !sweep && delta > 0.0 {
        delta -= core::f32::consts::TAU;
    }
    if sweep && delta < 0.0 {
        delta += core::f32::consts::TAU;
    }
    if large_arc && delta.abs() < core::f32::consts::PI {
        delta += if sweep {
            core::f32::consts::TAU
        } else {
            -core::f32::consts::TAU
        };
    }
    if !large_arc && delta.abs() > core::f32::consts::PI {
        delta += if sweep {
            -core::f32::consts::TAU
        } else {
            core::f32::consts::TAU
        };
    }
    let count = (delta.abs() / core::f32::consts::FRAC_PI_2).ceil().max(1.0) as usize;
    let step = delta / count as f32;
    for segment in 0..count {
        let a0 = theta + step * segment as f32;
        let a1 = a0 + step;
        let (s0, c0) = a0.sin_cos();
        let (s1, c1) = a1.sin_cos();
        let point = |c: f32, s: f32| {
            (
                center.0 + cos_phi * rx * c - sin_phi * ry * s,
                center.1 + sin_phi * rx * c + cos_phi * ry * s,
            )
        };
        let derivative = |c: f32, s: f32| {
            (
                -cos_phi * rx * s - sin_phi * ry * c,
                -sin_phi * rx * s + cos_phi * ry * c,
            )
        };
        let p0 = point(c0, s0);
        let p1 = point(c1, s1);
        let d0 = derivative(c0, s0);
        let d1 = derivative(c1, s1);
        let tangent = (4.0 / 3.0) * (step / 4.0).tan();
        builder.cubic_to(
            p0.0 + tangent * d0.0,
            p0.1 + tangent * d0.1,
            p1.0 - tangent * d1.0,
            p1.1 - tangent * d1.1,
            p1.0,
            p1.1,
        );
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub enum CanvasGradientKind {
    Linear {
        start: [f32; 2],
        end: [f32; 2],
        transform: Transform,
    },
    Radial {
        start: [f32; 2],
        end: [f32; 2],
        start_radius: f32,
        end_radius: f32,
        transform: Transform,
    },
}

#[derive(Clone, Copy)]
pub struct CanvasGradientStop {
    pub offset: f32,
    pub color: [u8; 4],
}

#[derive(Clone)]
pub struct CanvasGradient {
    pub kind: CanvasGradientKind,
    stops: Rc<RefCell<Vec<CanvasGradientStop>>>,
}

#[derive(Clone)]
pub struct CanvasPattern {
    pixmap: Rc<Pixmap>,
    transform: Rc<RefCell<Transform>>,
    repetition: CanvasPatternRepetition,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CanvasPatternRepetition {
    Repeat,
    RepeatX,
    RepeatY,
    NoRepeat,
}

impl CanvasPattern {
    pub fn new(
        image: &Rgba8Image,
        repetition: CanvasPatternRepetition,
    ) -> Result<Self, ImageError> {
        Ok(Self {
            pixmap: Rc::new(premultiplied_pixmap(image)?),
            transform: Rc::new(RefCell::new(Transform::identity())),
            repetition,
        })
    }
    pub fn set_transform(&self, transform: Transform) {
        *self.transform.borrow_mut() = transform;
    }
    pub fn transform(&self) -> Transform {
        *self.transform.borrow()
    }
    pub fn repetition(&self) -> CanvasPatternRepetition {
        self.repetition
    }
}

impl CanvasGradient {
    pub fn new(kind: CanvasGradientKind) -> Self {
        Self {
            kind,
            stops: Rc::new(RefCell::new(Vec::new())),
        }
    }
    pub fn add_color_stop(&self, offset: f32, color: [u8; 4]) -> Result<(), ()> {
        if !offset.is_finite() || !(0.0..=1.0).contains(&offset) {
            return Err(());
        }
        let mut stops = self.stops.borrow_mut();
        let index = stops.partition_point(|stop| stop.offset <= offset);
        stops.insert(index, CanvasGradientStop { offset, color });
        Ok(())
    }
    fn shader(&self, alpha: f32) -> Option<Shader<'static>> {
        let original_stops = self.stops.borrow().clone();
        let make_stops = |stops: Vec<CanvasGradientStop>| {
            stops
                .into_iter()
                .map(|stop| {
                    let [r, g, b, a] = stop.color;
                    GradientStop::new(
                        stop.offset,
                        Color::from_rgba8(r, g, b, (f32::from(a) * alpha).round() as u8),
                    )
                })
                .collect::<Vec<_>>()
        };
        if original_stops.is_empty() {
            return Some(Shader::SolidColor(Color::from_rgba8(0, 0, 0, 0)));
        }
        match self.kind {
            CanvasGradientKind::Linear {
                start,
                end,
                transform,
            } => LinearGradient::new(
                Point::from_xy(start[0], start[1]),
                Point::from_xy(end[0], end[1]),
                make_stops(original_stops),
                SpreadMode::Pad,
                transform,
            ),
            CanvasGradientKind::Radial {
                start,
                end,
                start_radius,
                end_radius,
                transform,
            } => {
                let (start, end, radius, mapped) = if start_radius == 0.0 {
                    (start, end, end_radius, original_stops)
                } else if end_radius > start_radius {
                    let delta = end_radius - start_radius;
                    let focus = [
                        start[0] - start_radius / delta * (end[0] - start[0]),
                        start[1] - start_radius / delta * (end[1] - start[1]),
                    ];
                    let mut stops = original_stops
                        .into_iter()
                        .map(|mut stop| {
                            stop.offset = (start_radius + stop.offset * delta) / end_radius;
                            stop
                        })
                        .collect::<Vec<_>>();
                    stops.sort_by(|left, right| left.offset.total_cmp(&right.offset));
                    (focus, end, end_radius, stops)
                } else if start_radius > end_radius {
                    let delta = start_radius - end_radius;
                    let focus = [
                        end[0] - end_radius / delta * (start[0] - end[0]),
                        end[1] - end_radius / delta * (start[1] - end[1]),
                    ];
                    let mut stops = original_stops
                        .into_iter()
                        .map(|mut stop| {
                            stop.offset = (end_radius + (1.0 - stop.offset) * delta) / start_radius;
                            stop
                        })
                        .collect::<Vec<_>>();
                    stops.sort_by(|left, right| left.offset.total_cmp(&right.offset));
                    (focus, start, start_radius, stops)
                } else {
                    return None;
                };
                RadialGradient::new(
                    Point::from_xy(start[0], start[1]),
                    Point::from_xy(end[0], end[1]),
                    radius,
                    make_stops(mapped),
                    SpreadMode::Pad,
                    transform,
                )
            }
        }
    }

    fn equal_radial_bitmap(&self, width: u32, height: u32) -> Option<Pixmap> {
        let CanvasGradientKind::Radial {
            start,
            end,
            start_radius,
            end_radius,
            transform,
        } = &self.kind
        else {
            return None;
        };
        if start_radius != end_radius {
            return None;
        }
        let inverse = transform.invert()?;
        let stops = self.stops.borrow().clone();
        if stops.is_empty() {
            return Some(Pixmap::new(width, height)?);
        }
        let gradient_stops = stops
            .into_iter()
            .map(|stop| {
                let [r, g, b, a] = stop.color;
                GradientStop::new(stop.offset, Color::from_rgba8(r, g, b, a))
            })
            .collect::<Vec<_>>();
        let lookup_width = 4096u32;
        let shader = LinearGradient::new(
            Point::from_xy(0.0, 0.0),
            Point::from_xy(lookup_width as f32, 0.0),
            gradient_stops,
            SpreadMode::Pad,
            Transform::identity(),
        )?;
        let mut lookup = Pixmap::new(lookup_width, 1)?;
        let paint = Paint {
            shader,
            ..Paint::default()
        };
        let rect = Rect::from_xywh(0.0, 0.0, lookup_width as f32, 1.0)?;
        lookup.fill_rect(rect, &paint, Transform::identity(), None);
        let mut image = Pixmap::new(width, height)?;
        let dx = end[0] - start[0];
        let dy = end[1] - start[1];
        let a = dx * dx + dy * dy;
        if *start_radius == 0.0 || a <= f32::EPSILON {
            return Some(image);
        }
        let radius_squared = *start_radius * *start_radius;
        for y in 0..height {
            for x in 0..width {
                let mut point = Point::from_xy(x as f32 + 0.5, y as f32 + 0.5);
                inverse.map_point(&mut point);
                let qx = point.x - start[0];
                let qy = point.y - start[1];
                let b = -2.0 * (qx * dx + qy * dy);
                let c = qx * qx + qy * qy - radius_squared;
                let discriminant = b * b - 4.0 * a * c;
                if !discriminant.is_finite() || discriminant < 0.0 {
                    continue;
                }
                let t = (-b + discriminant.sqrt()) / (2.0 * a);
                if !t.is_finite() || t < 0.0 {
                    continue;
                }
                let index = (t.clamp(0.0, 1.0) * (lookup_width - 1) as f32).round() as u32;
                let source = lookup.pixels()[index as usize];
                image.pixels_mut()[y as usize * width as usize + x as usize] = source;
            }
        }
        Some(image)
    }
}

#[derive(Clone)]
pub struct DrawingState {
    pub transform: Transform,
    pub fill: [u8; 4],
    pub stroke: [u8; 4],
    pub alpha: f32,
    pub blend: BlendMode,
    pub line: Stroke,
    pub fill_gradient: Option<CanvasGradient>,
    pub stroke_gradient: Option<CanvasGradient>,
    pub fill_pattern: Option<CanvasPattern>,
    pub stroke_pattern: Option<CanvasPattern>,
    clip: Option<Mask>,
}

impl Default for DrawingState {
    fn default() -> Self {
        Self {
            transform: Transform::identity(),
            fill: [0, 0, 0, 255],
            stroke: [0, 0, 0, 255],
            alpha: 1.0,
            blend: BlendMode::SourceOver,
            line: Stroke {
                width: 1.0,
                ..Stroke::default()
            },
            fill_gradient: None,
            stroke_gradient: None,
            fill_pattern: None,
            stroke_pattern: None,
            clip: None,
        }
    }
}

pub struct CanvasSurface {
    width: u32,
    height: u32,
    bitmap: Option<Pixmap>,
    state: DrawingState,
    saved: Vec<DrawingState>,
    generation: u64,
}

impl CanvasSurface {
    pub fn new(width: u32, height: u32) -> Result<Self, ImageError> {
        Ok(Self {
            width,
            height,
            bitmap: allocate(width, height)?,
            state: DrawingState::default(),
            saved: Vec::new(),
            generation: 0,
        })
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn state(&self) -> &DrawingState {
        &self.state
    }
    pub fn state_mut(&mut self) -> &mut DrawingState {
        &mut self.state
    }

    /// Setting either DOM bitmap dimension resets pixels and all drawing state,
    /// including when the new dimensions equal the previous ones.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), ImageError> {
        let bitmap = allocate(width, height)?;
        self.width = width;
        self.height = height;
        self.bitmap = bitmap;
        self.state = DrawingState::default();
        self.saved.clear();
        self.changed();
        Ok(())
    }

    pub fn save(&mut self) {
        self.saved.push(self.state.clone());
    }
    pub fn restore(&mut self) {
        if let Some(state) = self.saved.pop() {
            self.state = state;
        }
    }

    pub fn fill_rect(&mut self, x: f32, y: f32, width: f32, height: f32) {
        self.rect(x, y, width, height, false);
    }
    pub fn clear_rect(&mut self, x: f32, y: f32, width: f32, height: f32) {
        self.rect(x, y, width, height, true);
    }

    /// Draw an RGBA image into the destination rectangle using the current
    /// canvas transform, clip, alpha, and composite operation.
    pub fn draw_image(
        &mut self,
        image: &Rgba8Image,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    ) -> Result<(), ImageError> {
        self.draw_image_legacy_impl(image, x, y, width, height)
    }

    /// Draw an image source whose bitmap was created with a specified alpha
    /// preprocessing mode. `premultiply_alpha=false` filters straight RGBA
    /// channels before the normal Canvas compositing conversion.
    pub fn draw_image_with_alpha_behavior(
        &mut self,
        image: &Rgba8Image,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        premultiply_alpha: bool,
    ) -> Result<(), ImageError> {
        if premultiply_alpha {
            return self.draw_image_legacy_impl(image, x, y, width, height);
        }
        let transform = self.state.transform;
        let scale_x = transform.sx.hypot(transform.ky);
        let scale_y = transform.kx.hypot(transform.sy);
        let target_width = (width.abs() * scale_x).ceil();
        let target_height = (height.abs() * scale_y).ceil();
        if !target_width.is_finite()
            || !target_height.is_finite()
            || target_width > u32::MAX as f32
            || target_height > u32::MAX as f32
        {
            return Err(ImageError::TooLarge);
        }
        let target_width = (target_width as u32).max(1);
        let target_height = (target_height as u32).max(1);
        let filtered = if target_width == image.width && target_height == image.height {
            image.clone()
        } else {
            resize_image_straight_with_quality(
                image,
                target_width,
                target_height,
                ImageResizeQuality::Medium,
            )?
        };
        self.draw_image_legacy_impl(&filtered, x, y, width, height)
    }

    pub fn draw_image_crop_with_alpha_behavior(
        &mut self,
        image: &Rgba8Image,
        sx: f32,
        sy: f32,
        sw: f32,
        sh: f32,
        dx: f32,
        dy: f32,
        dw: f32,
        dh: f32,
        premultiply_alpha: bool,
    ) -> Result<(), ImageError> {
        if premultiply_alpha {
            return self.draw_image_crop(image, sx, sy, sw, sh, dx, dy, dw, dh);
        }
        if ![sx, sy, sw, sh, dx, dy, dw, dh]
            .iter()
            .all(|value| value.is_finite())
        {
            return Ok(());
        }
        if sw == 0.0 || sh == 0.0 || dw == 0.0 || dh == 0.0 {
            return Ok(());
        }
        let left = sx.min(sx + sw).max(0.0);
        let top = sy.min(sy + sh).max(0.0);
        let right = sx.max(sx + sw).min(image.width as f32);
        let bottom = sy.max(sy + sh).min(image.height as f32);
        if right <= left || bottom <= top {
            return Ok(());
        }
        let x0 = left.floor() as u32;
        let y0 = top.floor() as u32;
        let x1 = right.ceil().min(image.width as f32) as u32;
        let y1 = bottom.ceil().min(image.height as f32) as u32;
        let crop_width = x1 - x0;
        let crop_height = y1 - y0;
        let mut crop = Rgba8Image {
            width: crop_width,
            height: crop_height,
            pixels: vec![0; pixel_bytes(crop_width, crop_height)?],
        };
        for row in 0..crop_height {
            let source = ((y0 + row) as usize * image.width as usize + x0 as usize) * 4;
            let destination = row as usize * crop_width as usize * 4;
            let bytes = crop_width as usize * 4;
            crop.pixels[destination..destination + bytes]
                .copy_from_slice(&image.pixels[source..source + bytes]);
        }
        let source_width = sw.abs();
        let source_height = sh.abs();
        let left_ratio = ((left - sx.min(sx + sw)) / source_width).clamp(0.0, 1.0);
        let top_ratio = ((top - sy.min(sy + sh)) / source_height).clamp(0.0, 1.0);
        let right_ratio = ((right - sx.min(sx + sw)) / source_width).clamp(0.0, 1.0);
        let bottom_ratio = ((bottom - sy.min(sy + sh)) / source_height).clamp(0.0, 1.0);
        self.draw_image_with_alpha_behavior(
            &crop,
            dx + dw * left_ratio,
            dy + dh * top_ratio,
            dw * (right_ratio - left_ratio),
            dh * (bottom_ratio - top_ratio),
            false,
        )
    }

    fn draw_image_legacy_impl(
        &mut self,
        image: &Rgba8Image,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    ) -> Result<(), ImageError> {
        if ![x, y, width, height].iter().all(|v| v.is_finite()) || width == 0.0 || height == 0.0 {
            return Ok(());
        }
        if image.pixels.len() != pixel_bytes(image.width, image.height)? {
            return Err(ImageError::InvalidViewport);
        }
        if image.width == 0 || image.height == 0 {
            return Ok(());
        }
        let (x, width) = if width < 0.0 {
            (x + width, -width)
        } else {
            (x, width)
        };
        let (y, height) = if height < 0.0 {
            (y + height, -height)
        } else {
            (y, height)
        };
        let source = premultiplied_pixmap(image)?;
        let sx = width / image.width as f32;
        let sy = height / image.height as f32;
        let transform = self.state.transform;
        let destination = Transform::from_row(
            transform.sx * sx,
            transform.kx * sx,
            transform.ky * sy,
            transform.sy * sy,
            transform.sx * x + transform.ky * y + transform.tx,
            transform.kx * x + transform.sy * y + transform.ty,
        );
        let paint = PixmapPaint {
            opacity: self.state.alpha,
            blend_mode: self.state.blend,
            quality: FilterQuality::Bilinear,
        };
        if let Some(bitmap) = &mut self.bitmap {
            bitmap.draw_pixmap(
                0,
                0,
                source.as_ref(),
                &paint,
                destination,
                self.state.clip.as_ref(),
            );
            self.changed();
        }
        Ok(())
    }

    /// Draws a source crop, clipping source coordinates to the image before
    /// scaling. Negative crop dimensions extend toward the preceding edge
    /// without flipping the sampled pixels.
    pub fn draw_image_crop(
        &mut self,
        image: &Rgba8Image,
        sx: f32,
        sy: f32,
        sw: f32,
        sh: f32,
        dx: f32,
        dy: f32,
        dw: f32,
        dh: f32,
    ) -> Result<(), ImageError> {
        if ![sx, sy, sw, sh, dx, dy, dw, dh]
            .iter()
            .all(|v| v.is_finite())
        {
            return Ok(());
        }
        if sw == 0.0 || sh == 0.0 || dw == 0.0 || dh == 0.0 {
            return Ok(());
        }
        if image.pixels.len() != pixel_bytes(image.width, image.height)? {
            return Err(ImageError::InvalidViewport);
        }
        let left = sx.min(sx + sw).max(0.0);
        let top = sy.min(sy + sh).max(0.0);
        let right = sx.max(sx + sw).min(image.width as f32);
        let bottom = sy.max(sy + sh).min(image.height as f32);
        if right <= left || bottom <= top {
            return Ok(());
        }
        let x0 = left.floor() as u32;
        let y0 = top.floor() as u32;
        let x1 = right.ceil().min(image.width as f32) as u32;
        let y1 = bottom.ceil().min(image.height as f32) as u32;
        let crop_width = x1 - x0;
        let crop_height = y1 - y0;
        let mut crop = Rgba8Image {
            width: crop_width,
            height: crop_height,
            pixels: vec![0; pixel_bytes(crop_width, crop_height)?],
        };
        for row in 0..crop_height {
            let source = ((y0 + row) as usize * image.width as usize + x0 as usize) * 4;
            let destination = row as usize * crop_width as usize * 4;
            let bytes = crop_width as usize * 4;
            crop.pixels[destination..destination + bytes]
                .copy_from_slice(&image.pixels[source..source + bytes]);
        }
        let source_width = sw.abs();
        let source_height = sh.abs();
        let left_ratio = ((left - sx.min(sx + sw)) / source_width).clamp(0.0, 1.0);
        let top_ratio = ((top - sy.min(sy + sh)) / source_height).clamp(0.0, 1.0);
        let right_ratio = ((right - sx.min(sx + sw)) / source_width).clamp(0.0, 1.0);
        let bottom_ratio = ((bottom - sy.min(sy + sh)) / source_height).clamp(0.0, 1.0);
        self.draw_image(
            &crop,
            dx + dw * left_ratio,
            dy + dh * top_ratio,
            dw * (right_ratio - left_ratio),
            dh * (bottom_ratio - top_ratio),
        )
    }

    /// Shapes and rasterizes one Canvas text run through the shared font
    /// provider. Glyph coverage is rasterized by `lumen-html-text`; tiny-skia
    /// applies the Canvas transform, clip, paint, and compositing state.
    pub fn fill_text(
        &mut self,
        font: &dyn FontProvider,
        text: &str,
        size: f32,
        font_spec: &FontSpec,
        rtl: bool,
        x: f32,
        scale_anchor_x: f32,
        baseline_y: f32,
        max_width: Option<f32>,
        options: &CanvasTextOptions,
    ) -> Result<f32, ImageError> {
        if !x.is_finite() || !baseline_y.is_finite() || !size.is_finite() || size <= 0.0 {
            return Ok(0.0);
        }
        let run = font
            .shape_canvas_text(text, size, rtl, font_spec, options)
            .map_err(|_| ImageError::Font("text shaping failed"))?;
        if run.glyphs.is_empty() || self.bitmap.is_none() {
            return Ok(run.width);
        }
        let horizontal_scale = max_width
            .filter(|width| width.is_finite() && *width >= 0.0 && run.width > *width)
            .map_or(1.0, |width| width / run.width);
        if horizontal_scale <= 0.0 {
            return Ok(run.width);
        }
        let Some(mut glyphs) = Pixmap::new(self.width, self.height) else {
            return Err(ImageError::TooLarge);
        };
        let width_transform = Transform::from_row(
            horizontal_scale,
            0.0,
            0.0,
            1.0,
            scale_anchor_x * (1.0 - horizontal_scale),
            0.0,
        );
        let text_transform = self.state.transform.pre_concat(width_transform);
        let pixmap_paint = PixmapPaint {
            blend_mode: BlendMode::SourceOver,
            quality: FilterQuality::Bilinear,
            ..PixmapPaint::default()
        };
        for glyph in run.glyphs.iter() {
            let coverage = font
                .rasterize_glyph(glyph.face, glyph.id, size * glyph.size_scale)
                .map_err(ImageError::Font)?;
            if coverage.width == 0 || coverage.height == 0 {
                continue;
            }
            let mut pixels = Vec::with_capacity(coverage.alpha.len().saturating_mul(4));
            for alpha in coverage.alpha {
                pixels.extend_from_slice(&[alpha, alpha, alpha, alpha]);
            }
            let glyph_size =
                tiny_skia::IntSize::from_wh(coverage.width as u32, coverage.height as u32)
                    .ok_or(ImageError::TooLarge)?;
            let source = Pixmap::from_vec(pixels, glyph_size).ok_or(ImageError::TooLarge)?;
            let left = ((x + glyph.x) as f32).round() as i32 + coverage.x_min;
            let top =
                (baseline_y - glyph.y).round() as i32 - coverage.y_min - coverage.height as i32;
            glyphs.draw_pixmap(
                left,
                top,
                source.as_ref(),
                &pixmap_paint,
                text_transform,
                None,
            );
        }
        let mut mask = Mask::from_pixmap(glyphs.as_ref(), tiny_skia::MaskType::Alpha);
        let gradient_image = self
            .state
            .fill_gradient
            .as_ref()
            .and_then(|gradient| gradient.equal_radial_bitmap(self.width, self.height));
        let paint = Self::paint(
            self.state.blend,
            self.state.alpha,
            self.state.fill,
            self.state.fill_gradient.as_ref(),
            self.state.fill_pattern.as_ref(),
            self.state.transform,
            gradient_image.as_ref(),
        );
        let pattern_clip = self.pattern_clip(self.state.fill_pattern.as_ref());
        if let Some(clip) = pattern_clip.as_ref().or(self.state.clip.as_ref()) {
            for (coverage, clip_coverage) in mask.data_mut().iter_mut().zip(clip.data()) {
                *coverage = ((*coverage as u16 * *clip_coverage as u16 + 127) / 255) as u8;
            }
        }
        if let Some(bitmap) = &mut self.bitmap {
            if let Some(rect) = Rect::from_xywh(0.0, 0.0, self.width as f32, self.height as f32) {
                bitmap.fill_rect(rect, &paint, Transform::identity(), Some(&mask));
                self.changed();
            }
        }
        Ok(run.width)
    }

    /// Strokes shaped glyph outlines with the current canvas stroke style.
    /// The font provider supplies native vector contours; no bitmap dilation
    /// or synthetic outline approximation is used.
    pub fn stroke_text(
        &mut self,
        font: &dyn FontProvider,
        text: &str,
        size: f32,
        font_spec: &FontSpec,
        rtl: bool,
        origin_x: f32,
        scale_anchor_x: f32,
        baseline_y: f32,
        max_width: Option<f32>,
        options: &CanvasTextOptions,
    ) -> Result<f32, ImageError> {
        if !origin_x.is_finite() || !baseline_y.is_finite() || !size.is_finite() || size <= 0.0 {
            return Ok(0.0);
        }
        let run = font
            .shape_canvas_text(text, size, rtl, font_spec, options)
            .map_err(|_| ImageError::Font("text shaping failed"))?;
        if run.glyphs.is_empty() || self.bitmap.is_none() {
            return Ok(run.width);
        }
        let horizontal_scale = max_width
            .filter(|width| width.is_finite() && *width >= 0.0 && run.width > *width)
            .map_or(1.0, |width| width / run.width);
        if horizontal_scale <= 0.0 {
            return Ok(run.width);
        }
        let mut builder = PathBuilder::new();
        for glyph in run.glyphs.iter() {
            let outline = font
                .outline_glyph(glyph.face, glyph.id, size * glyph.size_scale)
                .map_err(ImageError::Font)?;
            for command in outline.commands {
                let point = |glyph_x: f32, glyph_y: f32| {
                    (origin_x + glyph.x + glyph_x, baseline_y - glyph.y - glyph_y)
                };
                match command {
                    GlyphOutlineCommand::MoveTo(x, y) => {
                        let (x, y) = point(x, y);
                        builder.move_to(x, y);
                    }
                    GlyphOutlineCommand::LineTo(x, y) => {
                        let (x, y) = point(x, y);
                        builder.line_to(x, y);
                    }
                    GlyphOutlineCommand::QuadTo(x1, y1, x, y) => {
                        let (x1, y1) = point(x1, y1);
                        let (x, y) = point(x, y);
                        builder.quad_to(x1, y1, x, y);
                    }
                    GlyphOutlineCommand::CurveTo(x1, y1, x2, y2, x, y) => {
                        let (x1, y1) = point(x1, y1);
                        let (x2, y2) = point(x2, y2);
                        let (x, y) = point(x, y);
                        builder.cubic_to(x1, y1, x2, y2, x, y);
                    }
                    GlyphOutlineCommand::Close => builder.close(),
                }
            }
        }
        let Some(path) = builder.finish() else {
            return Ok(run.width);
        };
        let width_transform = Transform::from_row(
            horizontal_scale,
            0.0,
            0.0,
            1.0,
            scale_anchor_x * (1.0 - horizontal_scale),
            0.0,
        );
        let transform = self.state.transform.pre_concat(width_transform);
        let gradient_image = self
            .state
            .stroke_gradient
            .as_ref()
            .and_then(|gradient| gradient.equal_radial_bitmap(self.width, self.height));
        let paint = Self::paint(
            self.state.blend,
            self.state.alpha,
            self.state.stroke,
            self.state.stroke_gradient.as_ref(),
            self.state.stroke_pattern.as_ref(),
            transform,
            gradient_image.as_ref(),
        );
        let pattern_clip = self.pattern_clip(self.state.stroke_pattern.as_ref());
        if let Some(bitmap) = &mut self.bitmap {
            bitmap.stroke_path(
                &path,
                &paint,
                &self.state.line,
                transform,
                pattern_clip.as_ref().or(self.state.clip.as_ref()),
            );
            self.changed();
        }
        Ok(run.width)
    }

    pub fn measure_text(
        &self,
        font: &dyn FontProvider,
        text: &str,
        size: f32,
        font_spec: &FontSpec,
        rtl: bool,
        options: &CanvasTextOptions,
    ) -> Result<ShapedRun, ImageError> {
        font.shape_canvas_text(text, size, rtl, font_spec, options)
            .map_err(|_| ImageError::Font("text shaping failed"))
    }
    fn rect(&mut self, x: f32, y: f32, width: f32, height: f32, clear: bool) {
        let Some(rect) = rectangle(x, y, width, height) else {
            return;
        };
        let gradient_image = if clear {
            None
        } else {
            self.state
                .fill_gradient
                .as_ref()
                .and_then(|gradient| gradient.equal_radial_bitmap(self.width, self.height))
        };
        let paint = if clear {
            // DestinationOut with an opaque source applies the clip coverage
            // to destination alpha, including partially covered edge pixels.
            Paint {
                blend_mode: BlendMode::DestinationOut,
                ..Paint::default()
            }
        } else {
            Self::paint(
                self.state.blend,
                self.state.alpha,
                self.state.fill,
                self.state.fill_gradient.as_ref(),
                self.state.fill_pattern.as_ref(),
                self.state.transform,
                gradient_image.as_ref(),
            )
        };
        let pattern_clip = if clear {
            None
        } else {
            self.pattern_clip(self.state.fill_pattern.as_ref())
        };
        if let Some(bitmap) = &mut self.bitmap {
            bitmap.fill_rect(
                rect,
                &paint,
                self.state.transform,
                pattern_clip.as_ref().or(self.state.clip.as_ref()),
            );
            self.changed();
        }
    }

    pub fn fill_path(&mut self, path: &Path, rule: FillRule) {
        let gradient_image = self
            .state
            .fill_gradient
            .as_ref()
            .and_then(|gradient| gradient.equal_radial_bitmap(self.width, self.height));
        let paint = Self::paint(
            self.state.blend,
            self.state.alpha,
            self.state.fill,
            self.state.fill_gradient.as_ref(),
            self.state.fill_pattern.as_ref(),
            self.state.transform,
            gradient_image.as_ref(),
        );
        let pattern_clip = self.pattern_clip(self.state.fill_pattern.as_ref());
        if let Some(bitmap) = &mut self.bitmap {
            bitmap.fill_path(
                path,
                &paint,
                rule,
                self.state.transform,
                pattern_clip.as_ref().or(self.state.clip.as_ref()),
            );
            self.changed();
        }
    }
    pub fn stroke_path(&mut self, path: &Path) {
        let gradient_image = self
            .state
            .stroke_gradient
            .as_ref()
            .and_then(|gradient| gradient.equal_radial_bitmap(self.width, self.height));
        let paint = Self::paint(
            self.state.blend,
            self.state.alpha,
            self.state.stroke,
            self.state.stroke_gradient.as_ref(),
            self.state.stroke_pattern.as_ref(),
            self.state.transform,
            gradient_image.as_ref(),
        );
        let pattern_clip = self.pattern_clip(self.state.stroke_pattern.as_ref());
        if let Some(bitmap) = &mut self.bitmap {
            bitmap.stroke_path(
                path,
                &paint,
                &self.state.line,
                self.state.transform,
                pattern_clip.as_ref().or(self.state.clip.as_ref()),
            );
            self.changed();
        }
    }
    pub fn clip_path(&mut self, path: &Path, rule: FillRule) {
        if let Some(clip) = &mut self.state.clip {
            clip.intersect_path(path, rule, true, self.state.transform);
        } else if let Some(mut clip) = Mask::new(self.width, self.height) {
            clip.fill_path(path, rule, true, self.state.transform);
            self.state.clip = Some(clip);
        }
    }

    /// Intersect the current clip with the union of the supplied transformed
    /// paths. SVG clipPath children contribute a logical OR before ancestor
    /// clip regions are intersected.
    pub fn clip_paths_union(
        &mut self,
        paths: &[(&Path, FillRule, Transform)],
    ) -> Result<(), ImageError> {
        let mut pixmap = Pixmap::new(self.width, self.height).ok_or(ImageError::TooLarge)?;
        let mut paint = Paint::default();
        paint.set_color(Color::WHITE);
        for (path, rule, transform) in paths {
            pixmap.fill_path(path, &paint, *rule, *transform, None);
        }
        let union = Mask::from_pixmap(pixmap.as_ref(), tiny_skia::MaskType::Alpha);
        let combined = if let Some(existing) = self.state.clip.as_ref() {
            let data = existing
                .data()
                .iter()
                .zip(union.data())
                .map(|(left, right)| ((u16::from(*left) * u16::from(*right) + 127) / 255) as u8)
                .collect();
            Mask::from_vec(
                data,
                tiny_skia::IntSize::from_wh(self.width, self.height)
                    .ok_or(ImageError::InvalidViewport)?,
            )
            .ok_or(ImageError::InvalidViewport)?
        } else {
            union
        };
        self.state.clip = Some(combined);
        Ok(())
    }

    /// Snapshot in the straight-alpha format used by ImageData and HTML images.
    pub fn snapshot(&self) -> Rgba8Image {
        let mut pixels = Vec::new();
        if let Some(bitmap) = &self.bitmap {
            pixels.reserve(bitmap.data().len());
            for pixel in bitmap.pixels() {
                let c = pixel.demultiply();
                pixels.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
            }
        }
        Rgba8Image {
            width: self.width,
            height: self.height,
            pixels,
        }
    }

    /// Clears the bitmap for `transferToImageBitmap` without resetting the
    /// context's drawing state.
    pub fn clear_bitmap(&mut self) {
        if let Some(bitmap) = &mut self.bitmap {
            bitmap.fill(Color::TRANSPARENT);
            self.changed();
        }
    }

    /// Pixel reads bypass the transform and clip. Samples outside the bitmap
    /// are transparent black, as required by ImageData readback.
    pub fn read_pixels(
        &self,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Result<Rgba8Image, ImageError> {
        let bytes = pixel_bytes(width, height)?;
        let mut output = Rgba8Image {
            width,
            height,
            pixels: vec![0; bytes],
        };
        if let Some(bitmap) = &self.bitmap {
            for row in 0..height {
                for col in 0..width {
                    let sx = i64::from(x) + i64::from(col);
                    let sy = i64::from(y) + i64::from(row);
                    if sx < 0
                        || sy < 0
                        || sx >= i64::from(self.width)
                        || sy >= i64::from(self.height)
                    {
                        continue;
                    }
                    let c = bitmap.pixels()[sy as usize * self.width as usize + sx as usize]
                        .demultiply();
                    let offset = (row as usize * width as usize + col as usize) * 4;
                    output.pixels[offset..offset + 4].copy_from_slice(&[
                        c.red(),
                        c.green(),
                        c.blue(),
                        c.alpha(),
                    ]);
                }
            }
        }
        Ok(output)
    }

    /// Direct pixel replacement bypasses alpha, compositing, transform and clip.
    pub fn write_pixels(&mut self, image: &Rgba8Image, x: i32, y: i32) -> Result<(), ImageError> {
        if image.pixels.len() != pixel_bytes(image.width, image.height)? {
            return Err(ImageError::InvalidViewport);
        }
        if let Some(bitmap) = &mut self.bitmap {
            for row in 0..image.height {
                for col in 0..image.width {
                    let dx = i64::from(x) + i64::from(col);
                    let dy = i64::from(y) + i64::from(row);
                    if dx < 0
                        || dy < 0
                        || dx >= i64::from(self.width)
                        || dy >= i64::from(self.height)
                    {
                        continue;
                    }
                    let offset = (row as usize * image.width as usize + col as usize) * 4;
                    let c = &image.pixels[offset..offset + 4];
                    bitmap.pixels_mut()[dy as usize * self.width as usize + dx as usize] =
                        tiny_skia::ColorU8::from_rgba(c[0], c[1], c[2], c[3]).premultiply();
                }
            }
            self.changed();
        }
        Ok(())
    }

    fn paint<'a>(
        blend: BlendMode,
        alpha: f32,
        color: [u8; 4],
        gradient: Option<&CanvasGradient>,
        pattern: Option<&'a CanvasPattern>,
        canvas_transform: Transform,
        custom_gradient: Option<&'a Pixmap>,
    ) -> Paint<'a> {
        let mut paint = Paint {
            blend_mode: blend,
            ..Paint::default()
        };
        if let Some(gradient) = custom_gradient {
            paint.shader = Pattern::new(
                gradient.as_ref(),
                SpreadMode::Pad,
                FilterQuality::Nearest,
                alpha,
                Transform::identity(),
            );
        } else if let Some(gradient) = gradient {
            paint.shader = gradient
                .shader(alpha)
                .unwrap_or_else(|| Shader::SolidColor(Color::from_rgba8(0, 0, 0, 0)));
        } else if let Some(pattern) = pattern {
            paint.shader = Pattern::new(
                pattern.pixmap.as_ref().as_ref(),
                SpreadMode::Repeat,
                FilterQuality::Bilinear,
                alpha,
                canvas_transform.pre_concat(pattern.transform()),
            );
        } else {
            paint.set_color_rgba8(
                color[0],
                color[1],
                color[2],
                (f32::from(color[3]) * alpha).round() as u8,
            );
        }
        paint
    }

    fn pattern_clip(&self, pattern: Option<&CanvasPattern>) -> Option<Mask> {
        let pattern = pattern?;
        if pattern.repetition == CanvasPatternRepetition::Repeat {
            return None;
        }
        let transform = self.state.transform.pre_concat(pattern.transform());
        let inverse = transform.invert()?;
        let mut viewport = [
            Point::from_xy(0.0, 0.0),
            Point::from_xy(self.width as f32, 0.0),
            Point::from_xy(self.width as f32, self.height as f32),
            Point::from_xy(0.0, self.height as f32),
        ];
        inverse.map_points(&mut viewport);
        let min_x = viewport
            .iter()
            .map(|point| point.x)
            .fold(f32::INFINITY, f32::min);
        let max_x = viewport
            .iter()
            .map(|point| point.x)
            .fold(f32::NEG_INFINITY, f32::max);
        let min_y = viewport
            .iter()
            .map(|point| point.y)
            .fold(f32::INFINITY, f32::min);
        let max_y = viewport
            .iter()
            .map(|point| point.y)
            .fold(f32::NEG_INFINITY, f32::max);
        if ![min_x, max_x, min_y, max_y]
            .iter()
            .all(|value| value.is_finite())
        {
            return None;
        }
        let (left, top, right, bottom) = match pattern.repetition {
            CanvasPatternRepetition::Repeat => unreachable!(),
            CanvasPatternRepetition::RepeatX => (min_x, 0.0, max_x, pattern.pixmap.height() as f32),
            CanvasPatternRepetition::RepeatY => (0.0, min_y, pattern.pixmap.width() as f32, max_y),
            CanvasPatternRepetition::NoRepeat => (
                0.0,
                0.0,
                pattern.pixmap.width() as f32,
                pattern.pixmap.height() as f32,
            ),
        };
        if right <= left || bottom <= top {
            return None;
        }
        let mut builder = PathBuilder::new();
        builder.move_to(left, top);
        builder.line_to(right, top);
        builder.line_to(right, bottom);
        builder.line_to(left, bottom);
        builder.close();
        let path = builder.finish()?;
        let size = tiny_skia::IntSize::from_wh(self.width, self.height)?;
        let mut mask = if let Some(clip) = &self.state.clip {
            Mask::from_vec(clip.data().to_vec(), size)?
        } else {
            Mask::from_vec(
                vec![u8::MAX; self.width as usize * self.height as usize],
                size,
            )?
        };
        mask.intersect_path(&path, FillRule::Winding, true, transform);
        Some(mask)
    }
    fn changed(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }
}

fn premultiplied_pixmap(image: &Rgba8Image) -> Result<Pixmap, ImageError> {
    if image.pixels.len() != pixel_bytes(image.width, image.height)? {
        return Err(ImageError::InvalidViewport);
    }
    let mut premultiplied = image.pixels.clone();
    for pixel in premultiplied.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        pixel[0] = ((u16::from(pixel[0]) * alpha + 127) / 255) as u8;
        pixel[1] = ((u16::from(pixel[1]) * alpha + 127) / 255) as u8;
        pixel[2] = ((u16::from(pixel[2]) * alpha + 127) / 255) as u8;
    }
    let size =
        tiny_skia::IntSize::from_wh(image.width, image.height).ok_or(ImageError::TooLarge)?;
    Pixmap::from_vec(premultiplied, size).ok_or(ImageError::TooLarge)
}

fn allocate(width: u32, height: u32) -> Result<Option<Pixmap>, ImageError> {
    pixel_bytes(width, height)?;
    if width == 0 || height == 0 {
        return Ok(None);
    }
    Pixmap::new(width, height)
        .map(Some)
        .ok_or(ImageError::TooLarge)
}

fn pixel_bytes(width: u32, height: u32) -> Result<usize, ImageError> {
    let bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(ImageError::TooLarge)?;
    if bytes > MAX_IMAGE_BYTES as u64 {
        return Err(ImageError::TooLarge);
    }
    Ok(bytes as usize)
}

fn rectangle(x: f32, y: f32, width: f32, height: f32) -> Option<Rect> {
    if ![x, y, width, height].iter().all(|v| v.is_finite()) {
        return None;
    }
    Rect::from_xywh(
        x.min(x + width),
        y.min(y + height),
        width.abs(),
        height.abs(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_skia::PathBuilder;

    #[test]
    fn state_clip_transform_clear_and_resize_affect_real_pixels() {
        let mut canvas = CanvasSurface::new(4, 2).unwrap();
        canvas.state_mut().fill = [255, 0, 0, 255];
        canvas.fill_rect(0.0, 0.0, 4.0, 2.0);
        canvas.save();
        let path = PathBuilder::from_rect(Rect::from_xywh(1.0, 0.0, 1.0, 2.0).unwrap());
        canvas.clip_path(&path, FillRule::Winding);
        canvas.state_mut().transform = Transform::from_translate(1.0, 0.0);
        canvas.clear_rect(0.0, 0.0, 2.0, 2.0);
        assert_eq!(&canvas.snapshot().pixels[4..8], &[0, 0, 0, 0]);
        assert_eq!(&canvas.snapshot().pixels[8..12], &[255, 0, 0, 255]);
        canvas.restore();
        canvas.fill_rect(2.0, 1.0, -1.0, -1.0);
        assert_eq!(&canvas.snapshot().pixels[4..8], &[255, 0, 0, 255]);
        canvas.resize(4, 2).unwrap();
        assert!(canvas.snapshot().pixels.iter().all(|byte| *byte == 0));
        assert_eq!(canvas.state().fill, [0, 0, 0, 255]);
    }

    #[test]
    fn pixel_operations_ignore_drawing_state_and_pad_outside_bounds() {
        let mut canvas = CanvasSurface::new(2, 1).unwrap();
        canvas.state_mut().alpha = 0.0;
        canvas.state_mut().transform = Transform::from_translate(100.0, 100.0);
        canvas
            .write_pixels(
                &Rgba8Image {
                    width: 2,
                    height: 1,
                    pixels: vec![255, 0, 0, 255, 0, 0, 255, 255],
                },
                -1,
                0,
            )
            .unwrap();
        assert_eq!(
            canvas.read_pixels(-1, 0, 3, 1).unwrap().pixels,
            [0, 0, 0, 0, 0, 0, 255, 255, 0, 0, 0, 0]
        );
    }

    #[test]
    fn source_over_uses_premultiplied_alpha_and_exports_straight_pixels() {
        let mut canvas = CanvasSurface::new(1, 1).unwrap();
        canvas.state_mut().fill = [255, 0, 0, 128];
        canvas.fill_rect(0.0, 0.0, 1.0, 1.0);
        assert_eq!(canvas.snapshot().pixels, [255, 0, 0, 128]);
        canvas.state_mut().fill = [0, 0, 255, 128];
        canvas.fill_rect(0.0, 0.0, 1.0, 1.0);
        let pixel = canvas.snapshot().pixels;
        assert!((i32::from(pixel[0]) - 85).abs() <= 1);
        assert!((i32::from(pixel[2]) - 170).abs() <= 1);
        assert!((i32::from(pixel[3]) - 192).abs() <= 1);
        assert!(CanvasSurface::new(u32::MAX, u32::MAX).is_err());
    }

    #[test]
    fn image_blits_scale_and_use_canvas_compositing_state() {
        let mut canvas = CanvasSurface::new(2, 1).unwrap();
        canvas
            .draw_image(
                &Rgba8Image {
                    width: 1,
                    height: 1,
                    pixels: vec![0, 0, 255, 255],
                },
                1.0,
                0.0,
                1.0,
                1.0,
            )
            .unwrap();
        assert_eq!(canvas.snapshot().pixels, [0, 0, 0, 0, 0, 0, 255, 255]);
        canvas.state_mut().alpha = 0.5;
        canvas
            .draw_image(
                &Rgba8Image {
                    width: 1,
                    height: 1,
                    pixels: vec![255, 0, 0, 255],
                },
                0.0,
                0.0,
                2.0,
                1.0,
            )
            .unwrap();
        assert_eq!(&canvas.snapshot().pixels[0..4], &[255, 0, 0, 128]);
    }

    #[test]
    fn gradient_shaders_use_ordered_stops_and_are_saved_with_drawing_state() {
        let gradient = CanvasGradient::new(CanvasGradientKind::Linear {
            start: [0.0, 0.0],
            end: [4.0, 0.0],
            transform: Transform::identity(),
        });
        gradient.add_color_stop(1.0, [0, 0, 255, 255]).unwrap();
        gradient.add_color_stop(0.0, [255, 0, 0, 255]).unwrap();
        let mut canvas = CanvasSurface::new(4, 1).unwrap();
        canvas.state_mut().fill_gradient = Some(gradient);
        canvas.fill_rect(0.0, 0.0, 4.0, 1.0);
        let pixels = canvas.snapshot().pixels;
        assert!(pixels[0] > pixels[2]);
        assert!(pixels[12] < pixels[14]);
        canvas.save();
        canvas.state_mut().fill_gradient = None;
        canvas.state_mut().fill = [0, 255, 0, 255];
        canvas.restore();
        assert!(canvas.state().fill_gradient.is_some());
    }

    #[test]
    fn svg_path_parser_handles_relative_smooth_and_elliptical_commands() {
        let parsed = parse_svg_path(
            "M2 3 l8 0 h2 v4 C12 9 12 10 11 11 s-2 2 -3 1 Q6 11 5 12 t-2 1 A3 2 25 0 1 2 3 z",
        )
        .unwrap();
        assert!(parsed.path.is_some());
        assert_eq!(parsed.current, (2.0, 3.0));
        assert_eq!(parsed.subpath_start, (2.0, 3.0));
        assert!(parse_svg_path("M0,0 L1,,2").is_err());
        assert!(parse_svg_path("L0 0").is_err());
        assert!(parse_svg_path("M0 0 A1 1 0 2 0 4 4").is_err());
        let move_only = parse_svg_path("M12 14").unwrap();
        assert!(move_only.path.is_none());
        assert_eq!(move_only.current, (12.0, 14.0));
    }
}
