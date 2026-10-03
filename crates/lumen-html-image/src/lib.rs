//! Deterministic RGBA8 replay and PNG output for the shared HTML display list.
use lumen_html::{
    layout::ImageState,
    paint::{Command, DisplayList, Glyph, ImageData, Rect, ReplayError, ReplaySink, Rgba},
};
use lumen_html_text::{FontFace, GlyphCoverage};
use std::collections::HashMap;
use std::{cell::RefCell, io::Read, path::PathBuf, sync::Arc};
mod background;
mod border;
mod coverage;
mod gradient;
mod shadow;
mod transform;

const GLYPH_CACHE_BYTES: usize = 2 * 1024 * 1024;
const GLYPH_CACHE_ENTRIES: usize = 4096;
const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_LAYER_BYTES: usize = 4 * 1024 * 1024;
const ASSET_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PNG_FILE_BYTES: u64 = 20 * 1024 * 1024;

pub struct FileImages {
    base: PathBuf,
    cache: RefCell<HashMap<String, Option<Arc<ImageData>>>>,
    cached_bytes: RefCell<usize>,
}

impl FileImages {
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self {
            base: base.into(),
            cache: RefCell::new(HashMap::new()),
            cached_bytes: RefCell::new(0),
        }
    }
}

impl lumen_html::layout::ImageResolver for FileImages {
    fn resolve(&self, source: &str) -> ImageState {
        if let Some(cached) = self.cache.borrow().get(source) {
            return cached.clone().map_or(ImageState::Failed, ImageState::Ready);
        }
        if self.cache.borrow().len() >= 256 {
            return ImageState::Failed;
        }
        let remaining_bytes = ASSET_CACHE_BYTES - *self.cached_bytes.borrow();
        if source.len() > remaining_bytes {
            return ImageState::Failed;
        }
        let remaining_bytes = remaining_bytes - source.len();
        let path = std::path::Path::new(source);
        let bytes = if let Some(encoded) = source.strip_prefix("data:image/png;base64,") {
            (encoded.len() as u64 <= (MAX_PNG_FILE_BYTES / 3 + 1) * 4)
                .then(|| lumen_common::codec::base64_decode_forgiving(encoded.as_bytes()))
                .flatten()
        } else if path.is_absolute()
            || !path
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
        {
            None
        } else {
            std::fs::File::open(self.base.join(path))
                .ok()
                .and_then(|file| {
                    let mut bytes = Vec::new();
                    file.take(MAX_PNG_FILE_BYTES + 1)
                        .read_to_end(&mut bytes)
                        .ok()?;
                    (bytes.len() as u64 <= MAX_PNG_FILE_BYTES).then_some(bytes)
                })
        };
        let decoded = bytes
            .and_then(|bytes| {
                if bytes.len() as u64 > MAX_PNG_FILE_BYTES {
                    return None;
                }
                let dimensions = bytes.get(16..24)?;
                let width = u32::from_be_bytes(dimensions[..4].try_into().ok()?) as usize;
                let height = u32::from_be_bytes(dimensions[4..].try_into().ok()?) as usize;
                lumen_common::limits::size::repeat(width, height, remaining_bytes / 4).ok()?;
                decode_png(&bytes).ok()
            })
            .map(|image| {
                Arc::new(ImageData {
                    width: image.width,
                    height: image.height,
                    pixels: image.pixels,
                })
            });
        let bytes = source.len() + decoded.as_ref().map_or(0, |image| image.pixels.len());
        let mut cached_bytes = self.cached_bytes.borrow_mut();
        if *cached_bytes + bytes > ASSET_CACHE_BYTES {
            return ImageState::Failed;
        }
        *cached_bytes += bytes;
        self.cache
            .borrow_mut()
            .insert(source.to_owned(), decoded.clone());
        decoded.map_or(ImageState::Failed, ImageState::Ready)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rgba8Image {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImageError {
    InvalidViewport,
    TooLarge,
    DisplayList(ReplayError),
    Html(lumen_html::html::ParseError),
    Layout(lumen_html::layout::LayoutError),
    Font(&'static str),
    Png(&'static str),
    SettleLimit,
}

#[derive(Clone, Copy, Debug)]
pub struct SettleOptions {
    pub max_passes: u32,
    pub timeout: std::time::Duration,
}

impl Default for SettleOptions {
    fn default() -> Self {
        Self {
            max_passes: 16,
            timeout: std::time::Duration::from_secs(5),
        }
    }
}

#[derive(Default)]
pub struct GlyphCache {
    glyphs: HashMap<(u64, u16, u32), GlyphCoverage>,
    bytes: usize,
}

impl GlyphCache {
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn clear(&mut self) {
        self.glyphs.clear();
        self.bytes = 0;
    }
}

struct Raster<'a> {
    image: &'a mut Rgba8Image,
    scale: f32,
    antialias: bool,
    clips: Vec<Rect>,
    font: Option<&'a FontFace>,
    cache: &'a mut GlyphCache,
    error: Option<ImageError>,
}

#[inline]
fn rounded_contains(x: f32, y: f32, left: f32, top: f32, right: f32, bottom: f32, r: f32) -> bool {
    if x < left || x >= right || y < top || y >= bottom {
        return false;
    }
    if r == 0.0 {
        return true;
    }
    let dx = x - x.clamp(left + r, right - r);
    let dy = y - y.clamp(top + r, bottom - r);
    dx * dx + dy * dy <= r * r
}

impl ReplaySink for Raster<'_> {
    fn push_clip(&mut self, rect: Rect) {
        let clip = self
            .clips
            .last()
            .copied()
            .and_then(|outer| outer.intersection(rect))
            .unwrap_or(Rect {
                x: 0.0,
                y: 0.0,
                width: 0.0,
                height: 0.0,
            });
        self.clips.push(clip);
    }

    fn pop_clip(&mut self) {
        self.clips.pop();
    }

    fn fill_rect(&mut self, rect: Rect, color: Rgba) {
        let Some(rect) = self
            .clips
            .last()
            .copied()
            .and_then(|clip| clip.intersection(rect))
        else {
            return;
        };
        let bounds = Rect {
            x: rect.x * self.scale,
            y: rect.y * self.scale,
            width: rect.width * self.scale,
            height: rect.height * self.scale,
        };
        let x0 = bounds.x.floor().max(0.0) as u32;
        let y0 = bounds.y.floor().max(0.0) as u32;
        let x1 = (bounds.x + bounds.width)
            .ceil()
            .min(self.image.width as f32) as u32;
        let y1 = (bounds.y + bounds.height)
            .ceil()
            .min(self.image.height as f32) as u32;
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        if color.a == 255
            && (!self.antialias
                || (bounds.x.fract() == 0.0
                    && bounds.y.fract() == 0.0
                    && bounds.width.fract() == 0.0
                    && bounds.height.fract() == 0.0))
        {
            let pixel = [color.r, color.g, color.b, 255];
            for y in y0..y1 {
                let start = ((y as usize * self.image.width as usize) + x0 as usize) * 4;
                let end = start + (x1 - x0) as usize * 4;
                for target in self.image.pixels[start..end].chunks_exact_mut(4) {
                    target.copy_from_slice(&pixel);
                }
            }
            return;
        }
        for y in y0..y1 {
            for x in x0..x1 {
                let offset = ((y as usize * self.image.width as usize) + x as usize) * 4;
                if color.a == 255
                    && (!self.antialias
                        || (bounds.x <= x as f32
                            && bounds.x + bounds.width >= x as f32 + 1.0
                            && bounds.y <= y as f32
                            && bounds.y + bounds.height >= y as f32 + 1.0))
                {
                    self.image.pixels[offset..offset + 4]
                        .copy_from_slice(&[color.r, color.g, color.b, 255]);
                    continue;
                }
                let coverage = if self.antialias {
                    let covered_x = ((bounds.x + bounds.width).min(x as f32 + 1.0)
                        - bounds.x.max(x as f32))
                    .clamp(0.0, 1.0);
                    let covered_y = ((bounds.y + bounds.height).min(y as f32 + 1.0)
                        - bounds.y.max(y as f32))
                    .clamp(0.0, 1.0);
                    covered_x * covered_y
                } else {
                    1.0
                };
                if coverage == 0.0 {
                    continue;
                }
                composite(&mut self.image.pixels[offset..offset + 4], color, coverage);
            }
        }
    }

    fn fill_rounded_rect(&mut self, rect: Rect, radius: f32, color: Rgba) {
        let radius = radius.min(rect.width * 0.5).min(rect.height * 0.5);
        if radius <= 0.0 {
            self.fill_rect(rect, color);
            return;
        }
        let Some(visible) = self
            .clips
            .last()
            .copied()
            .and_then(|clip| clip.intersection(rect))
        else {
            return;
        };
        let x0 = (visible.x * self.scale).floor().max(0.0) as u32;
        let y0 = (visible.y * self.scale).floor().max(0.0) as u32;
        let x1 = ((visible.x + visible.width) * self.scale)
            .ceil()
            .min(self.image.width as f32) as u32;
        let y1 = ((visible.y + visible.height) * self.scale)
            .ceil()
            .min(self.image.height as f32) as u32;
        let left = rect.x * self.scale;
        let top = rect.y * self.scale;
        let right = (rect.x + rect.width) * self.scale;
        let bottom = (rect.y + rect.height) * self.scale;
        let r = radius * self.scale;
        let r2 = r * r;
        for y in y0..y1 {
            for x in x0..x1 {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                let coverage = if self.antialias
                    && x as f32 >= left
                    && x as f32 + 1.0 <= right
                    && y as f32 >= top
                    && y as f32 + 1.0 <= bottom
                    && (x as f32 >= left + r && x as f32 + 1.0 <= right - r
                        || y as f32 >= top + r && y as f32 + 1.0 <= bottom - r)
                {
                    1.0
                } else if !self.antialias {
                    let cx = px.clamp(left + r, right - r);
                    let cy = py.clamp(top + r, bottom - r);
                    let dx = px - cx;
                    let dy = py - cy;
                    (px >= left
                        && px < right
                        && py >= top
                        && py < bottom
                        && dx * dx + dy * dy <= r2) as u8 as f32
                } else {
                    coverage::rounded(x as f32, y as f32, left, top, right, bottom, r)
                };
                if coverage == 0.0 {
                    continue;
                }
                let offset = ((y as usize * self.image.width as usize) + x as usize) * 4;
                if coverage == 1.0 && color.a == 255 {
                    self.image.pixels[offset..offset + 4]
                        .copy_from_slice(&[color.r, color.g, color.b, 255]);
                } else {
                    composite(&mut self.image.pixels[offset..offset + 4], color, coverage);
                }
            }
        }
    }

    fn fill_gradient(&mut self, rect: Rect, radius: f32, gradient: &lumen_html::paint::Gradient) {
        gradient::fill(self, rect, radius, gradient);
    }

    fn fill_background(
        &mut self,
        rect: Rect,
        radius: f32,
        positioning_rect: Rect,
        image_rect: Rect,
        repeat: [lumen_html::paint::BackgroundRepeat; 2],
        image: &lumen_html::paint::BackgroundPaint,
    ) {
        background::fill(
            self,
            rect,
            radius,
            positioning_rect,
            image_rect,
            repeat,
            image,
        );
    }

    fn draw_shadow(&mut self, rect: Rect, radius: f32, shadow: lumen_html::paint::BoxShadow) {
        if let Err(error) = shadow::draw(self, rect, radius, shadow) {
            self.error = Some(error);
        }
    }

    fn stroke_border(&mut self, rect: Rect, radius: f32, width: f32, color: Rgba) {
        border::draw(self, rect, radius, width, color, None);
    }

    fn stroke_pattern_border(
        &mut self,
        rect: Rect,
        radius: f32,
        width: f32,
        color: Rgba,
        pattern: lumen_html::paint::BorderPattern,
    ) {
        border::draw(self, rect, radius, width, color, Some(pattern));
    }

    fn draw_glyphs(
        &mut self,
        origin_x: f32,
        baseline_y: f32,
        size: f32,
        color: Rgba,
        glyphs: &[Glyph],
    ) {
        if color.a == 0 || glyphs.is_empty() {
            return;
        }
        let Some(font) = self.font else {
            self.error = Some(ImageError::Font("font required"));
            return;
        };
        let scaled_size = size * self.scale;
        for glyph in glyphs {
            let key = (font.id(), glyph.id, scaled_size.to_bits());
            let mut uncached = None;
            if !self.cache.glyphs.contains_key(&key) {
                let bitmap = match font.rasterize(glyph.id, scaled_size) {
                    Ok(bitmap) => bitmap,
                    Err(error) => {
                        self.error = Some(ImageError::Font(error));
                        return;
                    }
                };
                let bytes = bitmap.alpha.len();
                if bytes > GLYPH_CACHE_BYTES {
                    uncached = Some(bitmap);
                } else {
                    if self.cache.bytes + bytes > GLYPH_CACHE_BYTES
                        || self.cache.glyphs.len() >= GLYPH_CACHE_ENTRIES
                    {
                        self.cache.clear();
                    }
                    self.cache.bytes += bytes;
                    self.cache.glyphs.insert(key, bitmap);
                }
            }
            let coverage = uncached
                .as_ref()
                .or_else(|| self.cache.glyphs.get(&key))
                .unwrap();
            let left = ((origin_x + glyph.x) * self.scale).round() as i32 + coverage.x_min;
            let top = ((baseline_y - glyph.y) * self.scale).round() as i32
                - coverage.y_min
                - coverage.height as i32;
            for row in 0..coverage.height {
                let y = top + row as i32;
                if y < 0 || y >= self.image.height as i32 {
                    continue;
                }
                for col in 0..coverage.width {
                    let alpha = coverage.alpha[row * coverage.width + col];
                    if alpha == 0 {
                        continue;
                    }
                    let x = left + col as i32;
                    if x < 0 || x >= self.image.width as i32 {
                        continue;
                    }
                    let css_x = (x as f32 + 0.5) / self.scale;
                    let css_y = (y as f32 + 0.5) / self.scale;
                    if self.clips.last().is_none_or(|clip| {
                        css_x < clip.x
                            || css_y < clip.y
                            || css_x >= clip.x + clip.width
                            || css_y >= clip.y + clip.height
                    }) {
                        continue;
                    }
                    let offset = ((y as usize * self.image.width as usize) + x as usize) * 4;
                    if alpha == 255 && color.a == 255 {
                        self.image.pixels[offset..offset + 4]
                            .copy_from_slice(&[color.r, color.g, color.b, 255]);
                    } else {
                        composite(
                            &mut self.image.pixels[offset..offset + 4],
                            color,
                            alpha as f32 / 255.0,
                        );
                    }
                }
            }
        }
    }

    fn draw_image(&mut self, rect: Rect, image: &ImageData) {
        if rect.width <= 0.0 || rect.height <= 0.0 {
            return;
        }
        let Some(visible) = self
            .clips
            .last()
            .copied()
            .and_then(|clip| clip.intersection(rect))
        else {
            return;
        };
        let x0 = (visible.x * self.scale).floor().max(0.0) as u32;
        let y0 = (visible.y * self.scale).floor().max(0.0) as u32;
        let x1 = ((visible.x + visible.width) * self.scale)
            .ceil()
            .min(self.image.width as f32) as u32;
        let y1 = ((visible.y + visible.height) * self.scale)
            .ceil()
            .min(self.image.height as f32) as u32;
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        for y in y0..y1 {
            let center_y = (y as f32 + 0.5) / self.scale;
            let sy = ((((center_y - rect.y) / rect.height * image.height as f32).ceil() - 1.0)
                .max(0.0) as u32)
                .min(image.height - 1);
            for x in x0..x1 {
                let center_x = (x as f32 + 0.5) / self.scale;
                let sx = ((((center_x - rect.x) / rect.width * image.width as f32).ceil() - 1.0)
                    .max(0.0) as u32)
                    .min(image.width - 1);
                let coverage = if self.antialias {
                    let left = ((visible.x + visible.width).min((x as f32 + 1.0) / self.scale)
                        - visible.x.max(x as f32 / self.scale))
                    .clamp(0.0, 1.0 / self.scale)
                        * self.scale;
                    let top = ((visible.y + visible.height).min((y as f32 + 1.0) / self.scale)
                        - visible.y.max(y as f32 / self.scale))
                    .clamp(0.0, 1.0 / self.scale)
                        * self.scale;
                    left * top
                } else {
                    1.0
                };
                if coverage == 0.0 {
                    continue;
                }
                let source = ((sy as usize * image.width as usize) + sx as usize) * 4;
                let pixel = &image.pixels[source..source + 4];
                let target = ((y as usize * self.image.width as usize) + x as usize) * 4;
                if pixel[3] == 255 && coverage == 1.0 {
                    self.image.pixels[target..target + 4].copy_from_slice(pixel);
                } else {
                    composite(
                        &mut self.image.pixels[target..target + 4],
                        Rgba {
                            r: pixel[0],
                            g: pixel[1],
                            b: pixel[2],
                            a: pixel[3],
                        },
                        coverage,
                    );
                }
            }
        }
    }
}

fn composite(dst: &mut [u8], src: Rgba, coverage: f32) {
    let sa = src.a as f32 / 255.0 * coverage;
    if sa == 0.0 {
        return;
    }
    let da = dst[3] as f32 / 255.0;
    let out_a = sa + da * (1.0 - sa);
    if out_a == 0.0 {
        return;
    }
    let alpha = (out_a * 255.0).round();
    if alpha == 0.0 {
        return;
    }
    for (channel, source) in [src.r, src.g, src.b].into_iter().enumerate() {
        // RGBA8 surfaces quantize premultiplied color before source-over.
        let source = (source as f32 * sa).round();
        let destination = (dst[channel] as f32 * da).round();
        let color = source + (destination * (1.0 - sa)).round();
        dst[channel] = (color * 255.0 / alpha).round().min(255.0) as u8;
    }
    dst[3] = alpha as u8;
}

pub fn render(
    list: &DisplayList,
    width_css: u32,
    height_css: u32,
    scale: f32,
    antialias: bool,
) -> Result<Rgba8Image, ImageError> {
    render_internal(
        list,
        width_css,
        height_css,
        scale,
        antialias,
        None,
        &mut GlyphCache::default(),
    )
}

pub fn render_with_font(
    list: &DisplayList,
    width_css: u32,
    height_css: u32,
    scale: f32,
    antialias: bool,
    font: &FontFace,
) -> Result<Rgba8Image, ImageError> {
    render_with_font_cached(
        list,
        width_css,
        height_css,
        scale,
        antialias,
        font,
        &mut GlyphCache::default(),
    )
}

pub fn render_with_font_cached(
    list: &DisplayList,
    width_css: u32,
    height_css: u32,
    scale: f32,
    antialias: bool,
    font: &FontFace,
    cache: &mut GlyphCache,
) -> Result<Rgba8Image, ImageError> {
    render_internal(
        list,
        width_css,
        height_css,
        scale,
        antialias,
        Some(font),
        cache,
    )
}

/// Resolve composited subtrees to cropped sprites, with a bounded total allocation.
pub fn rasterize_layers(
    list: &DisplayList,
    scale: f32,
    font: &FontFace,
    cache: &mut GlyphCache,
) -> Result<DisplayList, ImageError> {
    list.validate().map_err(ImageError::DisplayList)?;
    if !scale.is_finite() || scale <= 0.0 {
        return Err(ImageError::InvalidViewport);
    }
    let mut bytes = 0;
    let mut resolved = resolve_layers(list, scale, Some(font), cache, &mut bytes)?;
    for command in &mut resolved.0 {
        let rect = match command {
            Command::FillGradient { rect, .. } | Command::FillBackground { rect, .. } => *rect,
            Command::BoxShadow { rect, shadow, .. } => shadow.bounds(*rect),
            Command::StrokePatternBorder { rect, .. } => *rect,
            _ => continue,
        };
        let left = (rect.x * scale).floor();
        let top = (rect.y * scale).floor();
        let width = ((rect.x + rect.width) * scale).ceil() - left;
        let height = ((rect.y + rect.height) * scale).ceil() - top;
        if width <= 0.0 || height <= 0.0 {
            continue;
        }
        if width > u32::MAX as f32 || height > u32::MAX as f32 {
            return Err(ImageError::TooLarge);
        }
        let allocation = lumen_common::limits::size::repeat(
            width as usize,
            height as usize,
            (MAX_LAYER_BYTES - bytes) / 4,
        )
        .map_err(|_| ImageError::TooLarge)?
            * 4;
        bytes += allocation;
        let (x, y) = (left / scale, top / scale);
        let mut translated = command.clone();
        translate_command(&mut translated, -x, -y);
        let raster = render_scaled_region(
            &DisplayList(vec![translated]),
            width as u32,
            height as u32,
            scale,
            Some(font),
            cache,
        )?;
        *command = Command::Image {
            rect: Rect {
                x,
                y,
                width: width / scale,
                height: height / scale,
            },
            image: Arc::new(ImageData {
                width: raster.width,
                height: raster.height,
                pixels: raster.pixels,
            }),
        };
    }
    Ok(resolved)
}

fn resolve_layers(
    list: &DisplayList,
    scale: f32,
    font: Option<&FontFace>,
    cache: &mut GlyphCache,
    bytes: &mut usize,
) -> Result<DisplayList, ImageError> {
    let mut output = DisplayList::default();
    let mut index = 0;
    while index < list.0.len() {
        if let Command::PushTransform(matrix) = list.0[index] {
            let start = index + 1;
            let mut depth = 1;
            index += 1;
            while depth != 0 {
                match list.0.get(index) {
                    Some(Command::PushTransform(_) | Command::PushLayer { .. }) => depth += 1,
                    Some(Command::PopTransform | Command::PopLayer) => depth -= 1,
                    Some(_) => {}
                    None => return Err(ImageError::DisplayList(ReplayError::UnbalancedClip)),
                }
                if depth != 0 {
                    index += 1;
                }
            }
            let end = index;
            index += 1;
            if matrix.inverse().is_none() {
                continue;
            }
            let mut child = DisplayList(list.0[start..end].to_vec());
            // Integral translations retain the original primitives and glyph cache.
            if matrix.a == 1.0
                && matrix.b == 0.0
                && matrix.c == 0.0
                && matrix.d == 1.0
                && (matrix.e * scale).fract() == 0.0
                && (matrix.f * scale).fract() == 0.0
            {
                for command in &mut child.0 {
                    translate_command(command, matrix.e, matrix.f);
                }
                output
                    .0
                    .extend(resolve_layers(&child, scale, font, cache, bytes)?.0);
                continue;
            }
            let mut child = resolve_layers(&child, scale, font, cache, bytes)?;
            if matrix.b == 0.0
                && matrix.c == 0.0
                && child.0.iter().all(|command| {
                    matches!(
                        command,
                        Command::FillRect { .. } | Command::PushClip(_) | Command::PopClip
                    )
                })
            {
                for command in &mut child.0 {
                    match command {
                        Command::FillRect { rect, .. } | Command::PushClip(rect) => {
                            *rect = matrix.bounds(*rect)
                        }
                        _ => {}
                    }
                }
                output.0.extend(child.0);
                continue;
            }
            let Some(crop) = paint_bounds(&child, scale, font, cache)? else {
                continue;
            };
            let left = (crop.x * scale).floor();
            let top = (crop.y * scale).floor();
            let width = ((crop.x + crop.width) * scale).ceil() - left;
            let height = ((crop.y + crop.height) * scale).ceil() - top;
            if !width.is_finite()
                || !height.is_finite()
                || width < 1.0
                || height < 1.0
                || width > u32::MAX as f32
                || height > u32::MAX as f32
            {
                return Err(ImageError::TooLarge);
            }
            let len = (width as usize)
                .checked_mul(height as usize)
                .and_then(|n| n.checked_mul(4))
                .ok_or(ImageError::TooLarge)?;
            if bytes.checked_add(len).is_none_or(|n| n > MAX_LAYER_BYTES) {
                return Err(ImageError::TooLarge);
            }
            let source_rect = Rect {
                x: left / scale,
                y: top / scale,
                width: width / scale,
                height: height / scale,
            };
            for command in &mut child.0 {
                translate_command(command, -source_rect.x, -source_rect.y);
            }
            let source =
                render_scaled_region(&child, width as u32, height as u32, scale, font, cache)?;
            if let Some((rect, image)) =
                transform::rasterize(source, source_rect, matrix, scale, MAX_LAYER_BYTES - *bytes)?
            {
                *bytes = bytes
                    .checked_add(image.pixels.len())
                    .ok_or(ImageError::TooLarge)?;
                output.0.push(Command::Image {
                    rect,
                    image: Arc::new(ImageData {
                        width: image.width,
                        height: image.height,
                        pixels: image.pixels,
                    }),
                });
            }
            continue;
        }
        let Command::PushLayer {
            rect,
            radius,
            opacity,
            clip,
        } = list.0[index]
        else {
            output.0.push(list.0[index].clone());
            index += 1;
            continue;
        };
        let start = index + 1;
        let mut depth = 1;
        index += 1;
        while depth != 0 {
            match list.0.get(index) {
                Some(Command::PushLayer { .. } | Command::PushTransform(_)) => depth += 1,
                Some(Command::PopLayer | Command::PopTransform) => depth -= 1,
                Some(_) => {}
                None => return Err(ImageError::DisplayList(ReplayError::UnbalancedClip)),
            }
            if depth != 0 {
                index += 1;
            }
        }
        let end = index;
        index += 1;
        if (clip && (rect.width == 0.0 || rect.height == 0.0)) || opacity == 0.0 {
            continue;
        }
        let child = DisplayList(list.0[start..end].to_vec());
        let mut child = resolve_layers(&child, scale, font, cache, bytes)?;
        let Some(crop) = paint_bounds(&child, scale, font, cache)?.and_then(|bounds| {
            if clip {
                bounds.intersection(rect)
            } else {
                Some(bounds)
            }
        }) else {
            continue;
        };
        let left = (crop.x * scale).floor();
        let top = (crop.y * scale).floor();
        let width = ((crop.x + crop.width) * scale).ceil() - left;
        let height = ((crop.y + crop.height) * scale).ceil() - top;
        if !width.is_finite()
            || !height.is_finite()
            || width < 1.0
            || height < 1.0
            || width > u32::MAX as f32
            || height > u32::MAX as f32
        {
            return Err(ImageError::TooLarge);
        }
        let len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or(ImageError::TooLarge)?;
        *bytes = bytes.checked_add(len).ok_or(ImageError::TooLarge)?;
        if *bytes > MAX_LAYER_BYTES {
            return Err(ImageError::TooLarge);
        }
        let x = left / scale;
        let y = top / scale;
        for command in &mut child.0 {
            translate_command(command, -x, -y);
        }
        let mut image =
            render_scaled_region(&child, width as u32, height as u32, scale, font, cache)?;
        let radius = radius.min(rect.width * 0.5).min(rect.height * 0.5) * scale;
        let clip_left = rect.x * scale - left;
        let clip_top = rect.y * scale - top;
        let right = clip_left + rect.width * scale;
        let bottom = clip_top + rect.height * scale;
        for py in 0..image.height {
            for px in 0..image.width {
                let coverage = if clip {
                    coverage::rounded(
                        px as f32, py as f32, clip_left, clip_top, right, bottom, radius,
                    )
                } else {
                    1.0
                };
                let alpha = ((py as usize * image.width as usize + px as usize) * 4) + 3;
                image.pixels[alpha] =
                    (image.pixels[alpha] as f32 * opacity * coverage).round() as u8;
            }
        }
        output.0.push(Command::Image {
            rect: Rect {
                x,
                y,
                width: width / scale,
                height: height / scale,
            },
            image: Arc::new(ImageData {
                width: image.width,
                height: image.height,
                pixels: image.pixels,
            }),
        });
    }
    Ok(output)
}

fn paint_bounds(
    list: &DisplayList,
    scale: f32,
    font: Option<&FontFace>,
    cache: &mut GlyphCache,
) -> Result<Option<Rect>, ImageError> {
    let mut bounds: Option<Rect> = None;
    let mut include = |rect: Rect| {
        if rect.width <= 0.0 || rect.height <= 0.0 {
            return;
        }
        bounds = Some(bounds.map_or(rect, |old| {
            let x = old.x.min(rect.x);
            let y = old.y.min(rect.y);
            Rect {
                x,
                y,
                width: (old.x + old.width).max(rect.x + rect.width) - x,
                height: (old.y + old.height).max(rect.y + rect.height) - y,
            }
        }));
    };
    for command in &list.0 {
        match command {
            Command::FillRect { rect, color }
            | Command::FillRoundedRect { rect, color, .. }
            | Command::StrokeBorder { rect, color, .. }
            | Command::StrokePatternBorder { rect, color, .. }
                if color.a != 0 =>
            {
                include(*rect)
            }
            Command::Image { rect, .. }
            | Command::FillGradient { rect, .. }
            | Command::FillBackground { rect, .. } => include(*rect),
            Command::BoxShadow { rect, shadow, .. } if shadow.color.a != 0 => {
                include(shadow.bounds(*rect))
            }
            Command::GlyphRun {
                origin_x,
                baseline_y,
                size,
                color,
                glyphs,
            } if color.a != 0 => {
                let font = font.ok_or(ImageError::Font("font required"))?;
                for glyph in glyphs {
                    let key = (font.id(), glyph.id, (size * scale).to_bits());
                    if !cache.glyphs.contains_key(&key) {
                        let coverage = font
                            .rasterize(glyph.id, size * scale)
                            .map_err(ImageError::Font)?;
                        if coverage.alpha.len() > GLYPH_CACHE_BYTES {
                            return Err(ImageError::TooLarge);
                        }
                        if cache.bytes + coverage.alpha.len() > GLYPH_CACHE_BYTES
                            || cache.glyphs.len() == GLYPH_CACHE_ENTRIES
                        {
                            cache.clear();
                        }
                        cache.bytes += coverage.alpha.len();
                        cache.glyphs.insert(key, coverage);
                    }
                    let coverage = &cache.glyphs[&key];
                    include(Rect {
                        x: (((origin_x + glyph.x) * scale).round() + coverage.x_min as f32) / scale,
                        y: (((baseline_y - glyph.y) * scale).round()
                            - coverage.y_min as f32
                            - coverage.height as f32)
                            / scale,
                        width: coverage.width as f32 / scale,
                        height: coverage.height as f32 / scale,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(bounds)
}

fn translate_command(command: &mut Command, x: f32, y: f32) {
    match command {
        Command::FillBackground {
            rect,
            positioning_rect,
            image_rect,
            ..
        } => {
            for rect in [rect, positioning_rect, image_rect] {
                rect.x += x;
                rect.y += y;
            }
        }
        Command::PushTransform(matrix) => *matrix = matrix.translated_space(x, y),
        Command::PushClip(rect)
        | Command::PushLayer { rect, .. }
        | Command::FillRect { rect, .. }
        | Command::FillRoundedRect { rect, .. }
        | Command::FillGradient { rect, .. }
        | Command::BoxShadow { rect, .. }
        | Command::StrokePatternBorder { rect, .. }
        | Command::StrokeBorder { rect, .. }
        | Command::Image { rect, .. } => {
            rect.x += x;
            rect.y += y;
        }
        Command::GlyphRun {
            origin_x,
            baseline_y,
            ..
        } => {
            *origin_x += x;
            *baseline_y += y;
        }
        Command::PopClip | Command::PopLayer | Command::PopTransform => {}
    }
}

fn render_scaled_region(
    list: &DisplayList,
    width: u32,
    height: u32,
    scale: f32,
    font: Option<&FontFace>,
    cache: &mut GlyphCache,
) -> Result<Rgba8Image, ImageError> {
    let mut image = Rgba8Image {
        width,
        height,
        pixels: vec![0; width as usize * height as usize * 4],
    };
    let mut sink = Raster {
        image: &mut image,
        scale,
        antialias: true,
        clips: vec![Rect {
            x: 0.0,
            y: 0.0,
            width: width as f32 / scale,
            height: height as f32 / scale,
        }],
        font,
        cache,
        error: None,
    };
    list.replay(&mut sink).map_err(ImageError::DisplayList)?;
    if let Some(error) = sink.error {
        return Err(error);
    }
    Ok(image)
}

fn render_internal(
    list: &DisplayList,
    width_css: u32,
    height_css: u32,
    scale: f32,
    antialias: bool,
    font: Option<&FontFace>,
    cache: &mut GlyphCache,
) -> Result<Rgba8Image, ImageError> {
    if !scale.is_finite() || scale <= 0.0 || width_css == 0 || height_css == 0 {
        return Err(ImageError::InvalidViewport);
    }
    let resolved;
    let list = if list.0.iter().any(|command| {
        matches!(
            command,
            Command::PushLayer { .. } | Command::PushTransform(_)
        )
    }) {
        list.validate().map_err(ImageError::DisplayList)?;
        resolved = resolve_layers(list, scale, font, cache, &mut 0)?;
        &resolved
    } else {
        list
    };
    let width = (width_css as f64 * scale as f64).round();
    let height = (height_css as f64 * scale as f64).round();
    if width < 1.0 || height < 1.0 || width > u32::MAX as f64 || height > u32::MAX as f64 {
        return Err(ImageError::InvalidViewport);
    }
    let (width, height) = (width as u32, height as u32);
    let pixels =
        lumen_common::limits::size::repeat(width as usize, height as usize, MAX_IMAGE_BYTES / 4)
            .map_err(|_| ImageError::TooLarge)?;
    let len = lumen_common::limits::size::repeat(pixels, 4, MAX_IMAGE_BYTES)
        .map_err(|_| ImageError::TooLarge)?;
    let mut data = Vec::new();
    data.try_reserve_exact(len)
        .map_err(|_| ImageError::TooLarge)?;
    data.resize(len, 0);
    let mut image = Rgba8Image {
        width,
        height,
        pixels: data,
    };
    let full = Rect {
        x: 0.0,
        y: 0.0,
        width: width_css as f32,
        height: height_css as f32,
    };
    let mut sink = Raster {
        image: &mut image,
        scale,
        antialias,
        clips: vec![full],
        font,
        cache,
        error: None,
    };
    list.replay(&mut sink).map_err(ImageError::DisplayList)?;
    if let Some(error) = sink.error {
        return Err(error);
    }
    Ok(image)
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    png.extend_from_slice(kind);
    png.extend_from_slice(data);
    let crc = lumen_common::compress::crc32_from(lumen_common::compress::crc32_from(0, kind), data);
    png.extend_from_slice(&crc.to_be_bytes());
}

pub fn render_png(
    list: &DisplayList,
    width_css: u32,
    height_css: u32,
    scale: f32,
    antialias: bool,
) -> Result<Vec<u8>, ImageError> {
    let image = render(list, width_css, height_css, scale, antialias)?;
    Ok(encode_png(&image))
}

pub fn encode_png(image: &Rgba8Image) -> Vec<u8> {
    let stride = image.width as usize * 4;
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&image.width.to_be_bytes());
    header.extend_from_slice(&image.height.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &header);
    let mut compressor = lumen_common::compress::ZStream::deflate(6, 15, 8, 0)
        .expect("zlib compressor initialization");
    let mut scanline = Vec::with_capacity(stride + 1);
    let mut compressed = [0u8; 16 * 1024];
    for row in image.pixels.chunks_exact(stride) {
        scanline.clear();
        scanline.push(0);
        scanline.extend_from_slice(row);
        let mut input = scanline.as_slice();
        while !input.is_empty() {
            let step = compressor.run(0, input, &mut compressed);
            assert_eq!(step.code, lumen_common::compress::Z_OK);
            assert!(step.consumed != 0 || step.produced != 0);
            input = &input[step.consumed..];
            if step.produced != 0 {
                chunk(&mut png, b"IDAT", &compressed[..step.produced]);
            }
        }
    }
    loop {
        let step = compressor.run(lumen_common::compress::Z_FINISH, &[], &mut compressed);
        assert!(
            step.code == lumen_common::compress::Z_OK
                || step.code == lumen_common::compress::Z_STREAM_END
        );
        if step.produced != 0 {
            chunk(&mut png, b"IDAT", &compressed[..step.produced]);
        }
        if step.code == lumen_common::compress::Z_STREAM_END {
            break;
        }
        assert_ne!(step.produced, 0);
    }
    chunk(&mut png, b"IEND", &[]);
    png
}

fn unfilter(row: &mut [u8], previous: Option<&[u8]>, channels: usize) -> Result<(), ImageError> {
    let (filter, bytes) = row
        .split_first_mut()
        .ok_or(ImageError::Png("empty scanline"))?;
    if *filter > 4 {
        return Err(ImageError::Png("unsupported PNG filter"));
    }
    for i in 0..bytes.len() {
        let left = if i >= channels {
            bytes[i - channels]
        } else {
            0
        };
        let previous_index = i / channels * 4 + i % channels;
        let above = previous.map_or(0, |row| row[previous_index]);
        let upper_left = if i >= channels {
            previous.map_or(0, |row| row[previous_index - 4])
        } else {
            0
        };
        let predictor = match *filter {
            0 => 0,
            1 => left,
            2 => above,
            3 => ((left as u16 + above as u16) / 2) as u8,
            _ => {
                let p = left as i32 + above as i32 - upper_left as i32;
                let (a, b, c) = (
                    (p - left as i32).abs(),
                    (p - above as i32).abs(),
                    (p - upper_left as i32).abs(),
                );
                if a <= b && a <= c {
                    left
                } else if b <= c {
                    above
                } else {
                    upper_left
                }
            }
        };
        bytes[i] = bytes[i].wrapping_add(predictor);
    }
    Ok(())
}

/// Decode a non-interlaced 8-bit RGB/RGBA PNG with bounded pixel storage.
pub fn decode_png(bytes: &[u8]) -> Result<Rgba8Image, ImageError> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(ImageError::Png("invalid PNG signature"));
    }
    let mut offset = 8usize;
    let mut image = None;
    let mut scanline = Vec::new();
    let mut inflater = None;
    let mut filled = 0usize;
    let mut rows = 0usize;
    let mut ended = false;
    let mut channels = 4;
    while offset < bytes.len() {
        let header_end = offset
            .checked_add(8)
            .ok_or(ImageError::Png("chunk too large"))?;
        let header = bytes
            .get(offset..header_end)
            .ok_or(ImageError::Png("truncated chunk"))?;
        let len = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        let end = offset
            .checked_add(len)
            .and_then(|n| n.checked_add(12))
            .ok_or(ImageError::Png("chunk too large"))?;
        if end > bytes.len() {
            return Err(ImageError::Png("truncated chunk"));
        }
        let kind = &bytes[offset + 4..offset + 8];
        let data = &bytes[offset + 8..offset + 8 + len];
        let expected_crc = u32::from_be_bytes(bytes[end - 4..end].try_into().unwrap());
        if lumen_common::compress::crc32_from(0, &bytes[offset + 4..end - 4]) != expected_crc {
            return Err(ImageError::Png("PNG CRC mismatch"));
        }
        offset = end;
        match kind {
            b"IHDR" if image.is_none() && len == 13 => {
                let width = u32::from_be_bytes(data[0..4].try_into().unwrap());
                let height = u32::from_be_bytes(data[4..8].try_into().unwrap());
                if width == 0
                    || height == 0
                    || data[8] != 8
                    || !matches!(data[9], 2 | 6)
                    || data[10..] != [0, 0, 0]
                {
                    return Err(ImageError::Png("unsupported PNG format"));
                }
                channels = if data[9] == 2 { 3 } else { 4 };
                let pixels = lumen_common::limits::size::repeat(
                    width as usize,
                    height as usize,
                    MAX_IMAGE_BYTES / 4,
                )
                .map_err(|_| ImageError::TooLarge)?;
                let len = lumen_common::limits::size::repeat(pixels, 4, MAX_IMAGE_BYTES)
                    .map_err(|_| ImageError::TooLarge)?;
                let mut data = Vec::new();
                data.try_reserve_exact(len)
                    .map_err(|_| ImageError::TooLarge)?;
                data.resize(len, 0);
                scanline
                    .try_reserve_exact(width as usize * channels + 1)
                    .map_err(|_| ImageError::TooLarge)?;
                scanline.resize(width as usize * channels + 1, 0);
                image = Some(Rgba8Image {
                    width,
                    height,
                    pixels: data,
                });
            }
            b"IDAT" => {
                let picture = image.as_mut().ok_or(ImageError::Png("IDAT before IHDR"))?;
                if inflater.is_none() {
                    inflater = Some(
                        lumen_common::compress::ZStream::inflate(15)
                            .map_err(|_| ImageError::Png("zlib inflater initialization failed"))?,
                    );
                }
                let stream = inflater.as_mut().unwrap();
                let mut input = data;
                while !input.is_empty() {
                    if ended {
                        return Err(ImageError::Png("extra compressed data"));
                    }
                    let step = stream.run(0, input, &mut scanline[filled..]);
                    if step.code != lumen_common::compress::Z_OK
                        && step.code != lumen_common::compress::Z_STREAM_END
                    {
                        return Err(ImageError::Png("invalid zlib stream"));
                    }
                    if step.consumed == 0 && step.produced == 0 {
                        return Err(ImageError::Png("stalled zlib stream"));
                    }
                    input = &input[step.consumed..];
                    filled += step.produced;
                    if filled == scanline.len() {
                        if rows >= picture.height as usize {
                            return Err(ImageError::Png("too many scanlines"));
                        }
                        let stride = picture.width as usize * 4;
                        let previous = if rows == 0 {
                            None
                        } else {
                            Some(&picture.pixels[(rows - 1) * stride..rows * stride])
                        };
                        unfilter(&mut scanline, previous, channels)?;
                        let destination = &mut picture.pixels[rows * stride..(rows + 1) * stride];
                        if channels == 4 {
                            destination.copy_from_slice(&scanline[1..]);
                        } else {
                            for (source, target) in scanline[1..]
                                .chunks_exact(3)
                                .zip(destination.chunks_exact_mut(4))
                            {
                                target[..3].copy_from_slice(source);
                                target[3] = 255;
                            }
                        }
                        rows += 1;
                        filled = 0;
                    }
                    if step.code == lumen_common::compress::Z_STREAM_END {
                        ended = true;
                    }
                }
            }
            b"IEND" if len == 0 => {
                let picture = image.ok_or(ImageError::Png("missing IHDR"))?;
                if !ended || filled != 0 || rows != picture.height as usize || offset != bytes.len()
                {
                    return Err(ImageError::Png("incomplete PNG data"));
                }
                return Ok(picture);
            }
            b"tRNS" => return Err(ImageError::Png("unsupported PNG transparency")),
            _ if kind[0] & 0x20 != 0 => {}
            _ => return Err(ImageError::Png("unsupported PNG chunk")),
        }
    }
    Err(ImageError::Png("missing IEND"))
}

pub fn render_html_png(
    html: &str,
    width_css: u32,
    height_css: u32,
    scale: f32,
) -> Result<Vec<u8>, ImageError> {
    Ok(encode_png(&render_html(
        html, width_css, height_css, scale,
    )?))
}

pub fn render_html(
    html: &str,
    width_css: u32,
    height_css: u32,
    scale: f32,
) -> Result<Rgba8Image, ImageError> {
    static FONT: std::sync::OnceLock<Result<FontFace, &'static str>> = std::sync::OnceLock::new();
    let font = FONT
        .get_or_init(|| FontFace::new(std::sync::Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)));
    render_html_with_font(
        html,
        width_css,
        height_css,
        scale,
        font.as_ref().map_err(|&error| ImageError::Font(error))?,
    )
}

pub fn render_html_with_font(
    html: &str,
    width_css: u32,
    height_css: u32,
    scale: f32,
    font: &FontFace,
) -> Result<Rgba8Image, ImageError> {
    let document = lumen_html::html::parse(html, 100_000).map_err(ImageError::Html)?;
    render_document(&document, width_css, height_css, scale, font)
}

pub fn render_html_with_images(
    html: &str,
    width_css: u32,
    height_css: u32,
    scale: f32,
    font: &FontFace,
    images: &dyn lumen_html::layout::ImageResolver,
) -> Result<Rgba8Image, ImageError> {
    let document = lumen_html::html::parse(html, 100_000).map_err(ImageError::Html)?;
    render_document_with_images(&document, width_css, height_css, scale, font, images)
}

pub fn render_document(
    document: &lumen_html::Document,
    width_css: u32,
    height_css: u32,
    scale: f32,
    font: &FontFace,
) -> Result<Rgba8Image, ImageError> {
    let list = lumen_html::layout::display_list(document, width_css, height_css, font)
        .map_err(ImageError::Layout)?;
    render_with_font(&list, width_css, height_css, scale, true, font)
}

pub fn render_document_with_images(
    document: &lumen_html::Document,
    width_css: u32,
    height_css: u32,
    scale: f32,
    font: &FontFace,
    images: &dyn lumen_html::layout::ImageResolver,
) -> Result<Rgba8Image, ImageError> {
    let list =
        lumen_html::layout::display_list_with_images(document, width_css, height_css, font, images)
            .map_err(ImageError::Layout)?;
    render_with_font(&list, width_css, height_css, scale, true, font)
}

/// Pump host jobs before each pass. The callback returns true while work remains.
/// Both pending images and queued jobs must settle before pixels are produced.
pub fn render_settled(
    session: &mut lumen_html::session::RenderSession,
    width_css: u32,
    height_css: u32,
    scale: f32,
    font: &FontFace,
    images: &dyn lumen_html::layout::ImageResolver,
    options: SettleOptions,
    mut pump: impl FnMut(&mut lumen_html::session::RenderSession) -> bool,
) -> Result<Rgba8Image, ImageError> {
    settle(options, || {
        let pending = pump(session);
        settle_pass(session, width_css, height_css, scale, font, images, pending)
    })
}

/// Pump realm jobs without holding the DOM session's RefCell borrow.
pub fn render_settled_shared(
    session: &std::rc::Rc<RefCell<lumen_html::session::RenderSession>>,
    width_css: u32,
    height_css: u32,
    scale: f32,
    font: &FontFace,
    images: &dyn lumen_html::layout::ImageResolver,
    options: SettleOptions,
    mut pump: impl FnMut() -> bool,
) -> Result<Rgba8Image, ImageError> {
    settle(options, || {
        let pending = pump();
        settle_pass(
            &mut session.borrow_mut(),
            width_css,
            height_css,
            scale,
            font,
            images,
            pending,
        )
    })
}

fn settle(
    options: SettleOptions,
    mut pass: impl FnMut() -> Result<Option<Rgba8Image>, ImageError>,
) -> Result<Rgba8Image, ImageError> {
    let started = std::time::Instant::now();
    for _ in 0..options.max_passes {
        if started.elapsed() >= options.timeout {
            return Err(ImageError::SettleLimit);
        }
        let image = pass()?;
        if started.elapsed() >= options.timeout {
            return Err(ImageError::SettleLimit);
        }
        if let Some(image) = image {
            return Ok(image);
        }
    }
    Err(ImageError::SettleLimit)
}

fn settle_pass(
    session: &mut lumen_html::session::RenderSession,
    width_css: u32,
    height_css: u32,
    scale: f32,
    font: &FontFace,
    images: &dyn lumen_html::layout::ImageResolver,
    pending: bool,
) -> Result<Option<Rgba8Image>, ImageError> {
    match session.display_list_with_images(width_css, height_css, font, images) {
        Err(lumen_html::layout::LayoutError::ImagePending) => Ok(None),
        Err(error) => Err(ImageError::Layout(error)),
        Ok(list) if !pending => {
            render_with_font(list, width_css, height_css, scale, true, font).map(Some)
        }
        Ok(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_html::paint::Command;

    #[test]
    fn source_over_quantizes_premultiplied_channels_and_preserves_transparency() {
        let source = Rgba {
            r: 23,
            g: 78,
            b: 166,
            a: 128,
        };
        let mut opaque = [232, 238, 248, 255];
        composite(&mut opaque, source, 1.0);
        assert_eq!(opaque, [128, 158, 207, 255]);
        let mut transparent = [255, 0, 0, 0];
        composite(&mut transparent, source, 1.0);
        assert_eq!(transparent, [24, 78, 165, 128]);
        let saved = transparent;
        composite(&mut transparent, Rgba { a: 0, ..source }, 1.0);
        assert_eq!(transparent, saved);
        composite(&mut transparent, Rgba { a: 255, ..source }, 1.0);
        assert_eq!(transparent, [23, 78, 166, 255]);
    }

    #[test]
    fn html_block_and_inline_opacity_are_grouped_once() {
        let font = FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for display in ["block", "inline"] {
            let html = format!(
                "<body style='margin:0'><div style='opacity:0.5;display:{display};background:red'><div style='width:4px;height:4px;background:blue'></div></div></body>"
            );
            let image = render_html_with_font(&html, 8, 8, 1.0, &font).unwrap();
            assert_eq!(&image.pixels[..4], &[127, 127, 255, 255]);
        }
    }

    #[test]
    fn layers_crop_and_apply_opacity_after_children_overlap() {
        let rect = Rect {
            x: 3.0,
            y: 2.0,
            width: 4.0,
            height: 4.0,
        };
        let red = Rgba {
            r: 255,
            g: 0,
            b: 0,
            a: 255,
        };
        let blue = Rgba {
            r: 0,
            g: 0,
            b: 255,
            a: 255,
        };
        let list = DisplayList(vec![
            Command::PushLayer {
                rect: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 1000.0,
                    height: 1000.0,
                },
                radius: 0.0,
                opacity: 0.5,
                clip: true,
            },
            Command::FillRect { rect, color: red },
            Command::FillRect { rect, color: blue },
            Command::PopLayer,
        ]);
        let font = FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for scale in [1.0, 2.0] {
            let resolved =
                rasterize_layers(&list, scale, &font, &mut GlyphCache::default()).unwrap();
            let Command::Image { rect: crop, image } = &resolved.0[0] else {
                panic!("layer sprite missing")
            };
            assert_eq!(*crop, rect);
            assert_eq!(
                (image.width, image.height),
                ((4.0 * scale) as u32, (4.0 * scale) as u32)
            );
            assert!(
                image
                    .pixels
                    .chunks_exact(4)
                    .all(|pixel| pixel == [0, 0, 255, 128])
            );
        }
    }

    #[test]
    fn axis_scale_keeps_rectangles_sharp_and_avoids_sprite_allocation() {
        let source = Rect {
            x: 1.0,
            y: 1.0,
            width: 3.0,
            height: 2.0,
        };
        let color = Rgba {
            r: 255,
            g: 0,
            b: 0,
            a: 255,
        };
        let matrix = lumen_html::paint::Affine {
            a: 2.0,
            d: 1.5,
            ..Default::default()
        };
        let transformed = DisplayList(vec![
            Command::PushTransform(matrix),
            Command::FillRect {
                rect: source,
                color,
            },
            Command::PopTransform,
        ]);
        let expected = DisplayList(vec![Command::FillRect {
            rect: matrix.bounds(source),
            color,
        }]);
        assert_eq!(
            render(&transformed, 12, 8, 1.0, true).unwrap(),
            render(&expected, 12, 8, 1.0, true).unwrap()
        );
        let mut bytes = 0;
        let resolved = resolve_layers(
            &transformed,
            1.0,
            None,
            &mut GlyphCache::default(),
            &mut bytes,
        )
        .unwrap();
        assert_eq!(resolved, expected);
        assert_eq!(bytes, 0);
    }

    #[test]
    fn transformed_opacity_does_not_clip_to_the_untransformed_viewport() {
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 4.0,
            height: 4.0,
        };
        let list = DisplayList(vec![
            Command::PushTransform(lumen_html::paint::Affine {
                e: -8.0,
                ..Default::default()
            }),
            Command::PushLayer {
                rect,
                radius: 0.0,
                opacity: 0.5,
                clip: false,
            },
            Command::FillRect {
                rect: Rect { x: 8.0, ..rect },
                color: Rgba {
                    r: 255,
                    g: 0,
                    b: 0,
                    a: 255,
                },
            },
            Command::PopLayer,
            Command::PopTransform,
        ]);
        let image = render(&list, 4, 4, 1.0, true).unwrap();
        assert!(
            image
                .pixels
                .chunks_exact(4)
                .all(|pixel| pixel == [255, 0, 0, 128])
        );
    }

    #[test]
    fn rounded_layer_clips_and_rejects_malformed_scopes_and_budget() {
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 8.0,
            height: 8.0,
        };
        let list = DisplayList(vec![
            Command::PushLayer {
                rect,
                radius: 4.0,
                opacity: 1.0,
                clip: true,
            },
            Command::FillRect {
                rect,
                color: Rgba {
                    r: 255,
                    g: 0,
                    b: 0,
                    a: 255,
                },
            },
            Command::PopLayer,
        ]);
        let image = render(&list, 8, 8, 1.0, true).unwrap();
        assert_eq!(image.pixels[3], 0);
        assert_eq!(image.pixels[(4 * 8 + 4) * 4 + 3], 255);
        let invalid = DisplayList(vec![
            Command::PushLayer {
                rect,
                radius: 0.0,
                opacity: 1.0,
                clip: true,
            },
            Command::PopClip,
        ]);
        assert_eq!(
            render(&invalid, 8, 8, 1.0, true),
            Err(ImageError::DisplayList(ReplayError::UnbalancedClip))
        );
        let huge = Rect {
            width: 2048.0,
            height: 2048.0,
            ..rect
        };
        let large = DisplayList(vec![
            Command::PushLayer {
                rect: huge,
                radius: 0.0,
                opacity: 0.5,
                clip: true,
            },
            Command::FillRect {
                rect: huge,
                color: Rgba {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 255,
                },
            },
            Command::PopLayer,
        ]);
        assert_eq!(render(&large, 8, 8, 1.0, true), Err(ImageError::TooLarge));
    }

    #[test]
    fn clips_blends_and_encodes_pixels() {
        let list = DisplayList(vec![
            Command::FillRect {
                rect: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 2.0,
                    height: 1.0,
                },
                color: Rgba {
                    r: 255,
                    g: 0,
                    b: 0,
                    a: 255,
                },
            },
            Command::PushClip(Rect {
                x: 1.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            }),
            Command::FillRect {
                rect: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 2.0,
                    height: 1.0,
                },
                color: Rgba {
                    r: 0,
                    g: 0,
                    b: 255,
                    a: 128,
                },
            },
            Command::PopClip,
        ]);
        let image = render(&list, 2, 1, 1.0, false).unwrap();
        assert_eq!(&image.pixels[..4], &[255, 0, 0, 255]);
        assert_eq!(&image.pixels[4..], &[127, 0, 128, 255]);
        let png = render_png(&list, 2, 1, 1.0, false).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(decode_png(&png).unwrap(), image);
        let mut corrupt = png.clone();
        corrupt[29] ^= 1;
        assert_eq!(
            decode_png(&corrupt),
            Err(ImageError::Png("PNG CRC mismatch"))
        );
        let mut offset = 8;
        let mut packed = Vec::new();
        while offset < png.len() {
            let len = u32::from_be_bytes(png[offset..offset + 4].try_into().unwrap()) as usize;
            let kind = &png[offset + 4..offset + 8];
            let data = &png[offset + 8..offset + 8 + len];
            let crc = lumen_common::compress::crc32_from(0, &png[offset + 4..offset + 8 + len]);
            assert_eq!(
                &png[offset + 8 + len..offset + 12 + len],
                &crc.to_be_bytes()
            );
            if kind == b"IDAT" {
                packed.extend_from_slice(data);
            }
            offset += len + 12;
        }
        assert_eq!(
            lumen_common::compress::zlib_decompress(&packed).unwrap(),
            [vec![0], image.pixels].concat()
        );
    }

    #[test]
    fn antialias_covers_fractional_edges_and_rejects_bad_commands() {
        let rect = Rect {
            x: 0.25,
            y: 0.0,
            width: 0.5,
            height: 1.0,
        };
        let color = Rgba {
            r: 255,
            g: 255,
            b: 255,
            a: 255,
        };
        let list = DisplayList(vec![Command::FillRect { rect, color }]);
        assert_eq!(
            render(&list, 1, 1, 1.0, true).unwrap().pixels,
            [255, 255, 255, 128]
        );
        let invalid = DisplayList(vec![Command::PopClip]);
        assert_eq!(
            render(&invalid, 1, 1, 1.0, true),
            Err(ImageError::DisplayList(ReplayError::UnbalancedClip))
        );
        let clip = Command::PushClip(Rect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        });
        let nested = DisplayList(vec![clip; 257]);
        assert_eq!(
            render(&nested, 1, 1, 1.0, true),
            Err(ImageError::DisplayList(ReplayError::ClipLimit))
        );
        let offscreen = DisplayList(vec![Command::FillRect {
            rect: Rect {
                x: 10.0,
                y: 0.0,
                width: 2.0,
                height: 1.0,
            },
            color,
        }]);
        assert_eq!(render(&offscreen, 1, 1, 1.0, true).unwrap().pixels, [0; 4]);
        assert_eq!(
            render(&offscreen, 5000, 5000, 1.0, true),
            Err(ImageError::TooLarge)
        );
    }

    #[test]
    fn html_css_renders_into_image() {
        let html = "<html><head><style>div { background: red; width: 2px; height: 1px }</style></head><body><div></div></body></html>";
        let image = render_html(html, 3, 1, 1.0).unwrap();
        assert_eq!(
            image.pixels,
            [255, 0, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255,]
        );
        let page = render_html(
            "<style>body{background:#203040}</style><p>x</p>",
            32,
            32,
            1.0,
        )
        .unwrap();
        assert_eq!(&page.pixels[page.pixels.len() - 4..], &[32, 48, 64, 255]);
    }

    #[test]
    fn canvas_background_obeys_root_and_hidden_body() {
        let image = render_html(
            "<style>html{background:red}body{background:blue;width:1px;height:1px}</style>",
            2,
            2,
            1.0,
        )
        .unwrap();
        assert_eq!(&image.pixels[..4], &[0, 0, 255, 255]);
        assert_eq!(&image.pixels[12..16], &[255, 0, 0, 255]);
        let hidden = render_html(
            "<style>body{display:none;background:red}</style>",
            1,
            1,
            1.0,
        )
        .unwrap();
        assert_eq!(hidden.pixels, [255, 255, 255, 255]);
        let inherited = render_html("<style>html{color:red}</style><p>x</p>", 30, 30, 1.0).unwrap();
        assert!(
            inherited
                .pixels
                .chunks_exact(4)
                .any(|p| p[0] == 255 && p[1] < 255)
        );
    }

    #[test]
    fn rounded_background_covers_center_and_softens_corners() {
        let image = render_html(
            "<div style='width:4px;height:4px;background:red;border-radius:2px'></div>",
            4,
            4,
            1.0,
        )
        .unwrap();
        assert_eq!(
            &image.pixels[4 * (4 * 2 + 1)..4 * (4 * 2 + 2)],
            &[255, 0, 0, 255]
        );
        assert!(image.pixels[1] > 0);
        assert!(image.pixels[1] < 255);
    }

    #[test]
    fn rounded_rect_antialiases_fractional_edges() {
        let list = DisplayList(vec![Command::FillRoundedRect {
            rect: Rect {
                x: 0.25,
                y: 0.25,
                width: 2.5,
                height: 2.5,
            },
            radius: 0.5,
            color: Rgba {
                r: 255,
                g: 0,
                b: 0,
                a: 255,
            },
        }]);
        let image = render(&list, 4, 4, 1.0, true).unwrap();
        assert!(image.pixels[4 * 4 + 3] > 0);
        assert!(image.pixels[4 * 4 + 3] < 255);
    }

    #[test]
    fn css_border_uses_content_box_and_rounded_ring() {
        let image = render_html(
            "<div style='width:3px;height:3px;border:1px solid red;border-radius:2px'></div>",
            5,
            5,
            1.0,
        )
        .unwrap();
        let pixel = |x: usize, y: usize| &image.pixels[(y * 5 + x) * 4..(y * 5 + x + 1) * 4];
        assert_eq!(pixel(2, 0), &[255, 0, 0, 255]);
        assert_eq!(pixel(2, 2), &[255, 255, 255, 255]);
        assert!(pixel(0, 0)[1] > 0);
    }

    #[test]
    fn html_text_renders_with_explicit_font() {
        let image = render_html("<p style='color: black'>Hello</p>", 100, 30, 1.0).unwrap();
        assert!(image.pixels.chunks_exact(4).any(|pixel| pixel[0] < 255));
        assert_eq!(&image.pixels[0..4], &[255, 255, 255, 255]);
    }

    #[test]
    fn png_streams_multiple_idat_chunks() {
        let mut state = 1u32;
        let pixels: Vec<u8> = (0..128 * 128 * 4)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        let image = Rgba8Image {
            width: 128,
            height: 128,
            pixels,
        };
        let png = encode_png(&image);
        assert_eq!(decode_png(&png).unwrap(), image);
        let mut offset = 8;
        let mut packed = Vec::new();
        let mut chunks = 0;
        while offset < png.len() {
            let len = u32::from_be_bytes(png[offset..offset + 4].try_into().unwrap()) as usize;
            if &png[offset + 4..offset + 8] == b"IDAT" {
                chunks += 1;
                packed.extend_from_slice(&png[offset + 8..offset + 8 + len]);
            }
            offset += len + 12;
        }
        let decoded = lumen_common::compress::zlib_decompress(&packed).unwrap();
        for (row, source) in decoded
            .chunks_exact(513)
            .zip(image.pixels.chunks_exact(512))
        {
            assert_eq!(row[0], 0);
            assert_eq!(&row[1..], source);
        }
        assert!(chunks > 1);
    }

    #[test]
    fn png_decoder_reconstructs_all_scanline_filters() {
        let first = [10, 20, 30, 40, 50, 60, 70, 80];
        let second = [20, 30, 40, 50, 60, 70, 80, 90];
        let expected = Rgba8Image {
            width: 2,
            height: 2,
            pixels: [first, second].concat(),
        };
        let cases = [
            (0, first, second),
            (
                1,
                [10, 20, 30, 40, 40, 40, 40, 40],
                [20, 30, 40, 50, 40, 40, 40, 40],
            ),
            (2, first, [10; 8]),
            (
                3,
                [10, 20, 30, 40, 45, 50, 55, 60],
                [15, 20, 25, 30, 25, 25, 25, 25],
            ),
            (4, [10, 20, 30, 40, 40, 40, 40, 40], [10; 8]),
        ];
        for (filter, top, bottom) in cases {
            let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
            chunk(&mut png, b"IHDR", &[0, 0, 0, 2, 0, 0, 0, 2, 8, 6, 0, 0, 0]);
            let raw = [vec![filter], top.to_vec(), vec![filter], bottom.to_vec()].concat();
            chunk(
                &mut png,
                b"IDAT",
                &lumen_common::compress::zlib_compress(&raw),
            );
            chunk(&mut png, b"IEND", &[]);
            assert_eq!(decode_png(&png).unwrap(), expected);
        }
    }

    #[test]
    fn rgb_png_filters_use_three_byte_pixels_and_expand_alpha() {
        let cases = [
            (0, [10, 20, 30, 40, 50, 60], [20, 30, 40, 50, 60, 70]),
            (1, [10, 20, 30, 30, 30, 30], [20, 30, 40, 30, 30, 30]),
            (2, [10, 20, 30, 40, 50, 60], [10, 10, 10, 10, 10, 10]),
            (3, [10, 20, 30, 35, 40, 45], [15, 20, 25, 20, 20, 20]),
            (4, [10, 20, 30, 30, 30, 30], [10, 10, 10, 10, 10, 10]),
        ];
        for (filter, first, second) in cases {
            let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
            chunk(&mut png, b"IHDR", &[0, 0, 0, 2, 0, 0, 0, 2, 8, 2, 0, 0, 0]);
            let raw = [vec![filter], first.to_vec(), vec![filter], second.to_vec()].concat();
            chunk(
                &mut png,
                b"IDAT",
                &lumen_common::compress::zlib_compress(&raw),
            );
            chunk(&mut png, b"IEND", &[]);
            assert_eq!(
                decode_png(&png).unwrap().pixels,
                [
                    10, 20, 30, 255, 40, 50, 60, 255, 20, 30, 40, 255, 50, 60, 70, 255
                ]
            );
        }
    }

    #[test]
    fn img_uses_intrinsic_size_and_shared_image_command() {
        let doc = lumen_html::html::parse("<img src='tile.png'>", 8).unwrap();
        let font =
            FontFace::new(std::sync::Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap();
        let tile = std::sync::Arc::new(ImageData {
            width: 2,
            height: 1,
            pixels: vec![255, 0, 0, 255, 0, 0, 255, 255],
        });
        let assets = |source: &str| {
            if source == "tile.png" {
                ImageState::Ready(tile.clone())
            } else {
                ImageState::Failed
            }
        };
        let image = render_document_with_images(&doc, 2, 1, 1.0, &font, &assets).unwrap();
        assert_eq!(image.pixels, [255, 0, 0, 255, 0, 0, 255, 255]);
        let scaled = lumen_html::html::parse("<img src='tile.png' width='4'>", 8).unwrap();
        let image = render_document_with_images(&scaled, 4, 2, 1.0, &font, &assets).unwrap();
        assert_eq!(&image.pixels[4 * 4..4 * 5], &[255, 0, 0, 255]);
        assert_eq!(&image.pixels[4 * 7..4 * 8], &[0, 0, 255, 255]);
        // Chrome's pixelated downsampling selects the lower texel at an exact tie.
        let downscaled =
            lumen_html::html::parse("<img src='tile.png' width='1' height='1'>", 8).unwrap();
        let image = render_document_with_images(&downscaled, 1, 1, 1.0, &font, &assets).unwrap();
        assert_eq!(image.pixels, [255, 0, 0, 255]);
    }

    #[test]
    fn data_images_decode_and_cache_bounded_png() {
        let assets = FileImages::new(".");
        let png = encode_png(&Rgba8Image {
            width: 1,
            height: 1,
            pixels: vec![9, 8, 7, 255],
        });
        let source = format!(
            "data:image/png;base64,{}",
            lumen_common::codec::base64_encode(&png, false, true)
        );
        let ImageState::Ready(first) = lumen_html::layout::ImageResolver::resolve(&assets, &source)
        else {
            panic!("data image did not load")
        };
        let ImageState::Ready(again) = lumen_html::layout::ImageResolver::resolve(&assets, &source)
        else {
            panic!("cached data image did not load")
        };
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(first.pixels, [9, 8, 7, 255]);
        assert_eq!(
            lumen_html::layout::ImageResolver::resolve(&assets, "data:image/png;base64,invalid!"),
            ImageState::Failed
        );
    }

    #[test]
    fn file_images_decode_and_cache_local_png() {
        let directory =
            std::env::temp_dir().join(format!("lumen-html-image-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("tile.png");
        let png = encode_png(&Rgba8Image {
            width: 1,
            height: 1,
            pixels: vec![9, 8, 7, 255],
        });
        std::fs::write(&path, png).unwrap();
        let assets = FileImages::new(&directory);
        let ImageState::Ready(first) =
            lumen_html::layout::ImageResolver::resolve(&assets, "tile.png")
        else {
            panic!("image did not load")
        };
        let ImageState::Ready(again) =
            lumen_html::layout::ImageResolver::resolve(&assets, "tile.png")
        else {
            panic!("cached image did not load")
        };
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(&first.pixels[..], &[9, 8, 7, 255]);
        let font = FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap();
        let rendered =
            render_html_with_images("<img src='tile.png'>", 1, 1, 1.0, &font, &assets).unwrap();
        assert_eq!(rendered.pixels, [9, 8, 7, 255]);
        assert_eq!(
            lumen_html::layout::ImageResolver::resolve(&assets, "../tile.png"),
            ImageState::Failed
        );
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn pending_images_settle_after_pump_and_fail_on_limit() {
        let document =
            lumen_html::html::parse("<img src='later.png' width='1' height='1'>", 8).unwrap();
        let mut session = lumen_html::session::RenderSession::new(document);
        let font = FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap();
        let ready = std::cell::Cell::new(false);
        let pixels = Arc::new(ImageData {
            width: 1,
            height: 1,
            pixels: vec![1, 2, 3, 255],
        });
        let images = |_: &str| {
            if ready.get() {
                ImageState::Ready(pixels.clone())
            } else {
                ImageState::Pending
            }
        };
        let limited = SettleOptions {
            max_passes: 1,
            timeout: std::time::Duration::from_secs(1),
        };
        assert_eq!(
            render_settled(&mut session, 1, 1, 1.0, &font, &images, limited, |_| false),
            Err(ImageError::SettleLimit)
        );
        let rendered = render_settled(
            &mut session,
            1,
            1,
            1.0,
            &font,
            &images,
            SettleOptions::default(),
            |_| {
                ready.set(true);
                false
            },
        )
        .unwrap();
        assert_eq!(rendered.pixels, [1, 2, 3, 255]);
        let failed = |_: &str| ImageState::Failed;
        assert_eq!(
            render_document_with_images(session.document(), 1, 1, 1.0, &font, &failed),
            Err(ImageError::Layout(
                lumen_html::layout::LayoutError::ImageFailed
            ))
        );
    }

    #[test]
    fn settled_render_waits_for_jobs_that_mutate_a_ready_document() {
        let document =
            lumen_html::html::parse("<body style='margin:0;background:red'></body>", 8).unwrap();
        let body = lumen_html::selector::query_selector(&document, document.root(), "body")
            .unwrap()
            .unwrap();
        let mut session = lumen_html::session::RenderSession::new(document);
        let font = FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let images = |_: &str| ImageState::Failed;
        let mut passes = 0;
        let image = render_settled(
            &mut session,
            1,
            1,
            1.0,
            &font,
            &images,
            SettleOptions::default(),
            |session| {
                passes += 1;
                if passes == 2 {
                    session
                        .document_mut()
                        .set_attribute(body, "style", "margin:0;background:blue")
                        .unwrap();
                }
                passes < 2
            },
        )
        .unwrap();
        assert_eq!(passes, 2);
        assert_eq!(image.pixels, [0, 0, 255, 255]);
        assert_eq!(
            render_settled(
                &mut session,
                1,
                1,
                1.0,
                &font,
                &images,
                SettleOptions {
                    max_passes: 1,
                    timeout: std::time::Duration::from_secs(1)
                },
                |_| true
            ),
            Err(ImageError::SettleLimit)
        );
    }

    #[test]
    fn shared_settling_releases_session_before_pumping_and_checks_limits() {
        let document =
            lumen_html::html::parse("<body style='margin:0;background:red'></body>", 8).unwrap();
        let body = lumen_html::selector::query_selector(&document, document.root(), "body")
            .unwrap()
            .unwrap();
        let session = std::rc::Rc::new(RefCell::new(lumen_html::session::RenderSession::new(
            document,
        )));
        let font = FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let images = |_: &str| ImageState::Failed;
        let mut passes = 0;
        let image = render_settled_shared(
            &session,
            1,
            1,
            1.0,
            &font,
            &images,
            SettleOptions::default(),
            || {
                passes += 1;
                session
                    .borrow_mut()
                    .document_mut()
                    .set_attribute(body, "style", "margin:0;background:blue")
                    .unwrap();
                passes < 2
            },
        )
        .unwrap();
        assert_eq!(passes, 2);
        assert_eq!(image.pixels, [0, 0, 255, 255]);
        for options in [
            SettleOptions {
                max_passes: 0,
                timeout: std::time::Duration::from_secs(1),
            },
            SettleOptions {
                max_passes: 1,
                timeout: std::time::Duration::ZERO,
            },
        ] {
            assert_eq!(
                render_settled_shared(&session, 1, 1, 1.0, &font, &images, options, || panic!(
                    "invalid limits must not pump"
                )),
                Err(ImageError::SettleLimit)
            );
        }
        assert_eq!(
            render_settled_shared(
                &session,
                1,
                1,
                1.0,
                &font,
                &images,
                SettleOptions {
                    max_passes: 1,
                    timeout: std::time::Duration::from_millis(1)
                },
                || {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    false
                }
            ),
            Err(ImageError::SettleLimit)
        );
    }

    #[test]
    fn glyph_cache_reuses_rasters_across_frames_and_separates_fonts() {
        let document = lumen_html::html::parse("<p>Hello</p>", 8).unwrap();
        let font = FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap();
        let list = lumen_html::layout::display_list(&document, 80, 30, &font).unwrap();
        let mut cache = GlyphCache::default();
        let first = render_with_font_cached(&list, 80, 30, 1.0, true, &font, &mut cache).unwrap();
        let entries = cache.glyphs.len();
        assert!(entries > 0 && cache.bytes() <= GLYPH_CACHE_BYTES);
        let second = render_with_font_cached(&list, 80, 30, 1.0, true, &font, &mut cache).unwrap();
        assert_eq!(first, second);
        assert_eq!(cache.glyphs.len(), entries);
        let other = FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap();
        render_with_font_cached(&list, 80, 30, 1.0, true, &other, &mut cache).unwrap();
        assert_eq!(cache.glyphs.len(), entries * 2);
    }
}
