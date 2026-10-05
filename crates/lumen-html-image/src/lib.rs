//! Deterministic RGBA8 replay and PNG output for the shared HTML display list.
use image::{ImageDecoder as _, ImageEncoder as _};
use lumen_html::{
    layout::ImageState,
    paint::{
        Affine, Command, DisplayList, Glyph, ImageData, Rect, ReplayError, ReplaySink, Rgba,
        SvgFillRule as PaintSvgFillRule,
    },
};
use lumen_html_text::{FontFace, FontProvider, GlyphCoverage};
use std::collections::HashMap;
use std::{
    cell::{Cell, RefCell},
    io::{Cursor, Read},
    path::PathBuf,
    sync::Arc,
};
mod background;
mod border;
pub mod canvas;
mod coverage;
mod gpui;
mod gradient;
mod shadow;
mod snap;
mod transform;

const GLYPH_CACHE_BYTES: usize = 2 * 1024 * 1024;
const GLYPH_CACHE_ENTRIES: usize = 4096;
const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_LAYER_BYTES: usize = 4 * 1024 * 1024;
const ASSET_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PNG_FILE_BYTES: u64 = 20 * 1024 * 1024;

/// Interpolate two premultiplied-alpha sRGB colors using the same shared
/// operation used by gradient replay.
pub fn interpolate_color(
    from: lumen_html::paint::Rgba,
    to: lumen_html::paint::Rgba,
    progress: f32,
) -> lumen_html::paint::Rgba {
    gradient::interpolate(from, to, progress.clamp(0.0, 1.0))
}

/// Geometry policies of the two image paths and the GPUI quad replay path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RasterizationMode {
    Integer,
    Antialiased,
    /// CSS box edges snap to the device grid; glyphs, curves and transforms
    /// retain their antialiased coverage. Raw display-list AA stays available.
    CssPixelSnapped,
    Gpui,
}

pub fn render_with_mode_cached(
    list: &DisplayList,
    width_css: u32,
    height_css: u32,
    scale: f32,
    mode: RasterizationMode,
    font: &dyn FontProvider,
    cache: &mut GlyphCache,
) -> Result<Rgba8Image, ImageError> {
    match mode {
        RasterizationMode::Integer | RasterizationMode::Antialiased => render_with_font_cached(
            list,
            width_css,
            height_css,
            scale,
            mode == RasterizationMode::Antialiased,
            font,
            cache,
        ),
        RasterizationMode::CssPixelSnapped => {
            if !scale.is_finite() || scale <= 0.0 {
                return Err(ImageError::InvalidViewport);
            }
            let mut snapped = list.clone();
            snap::boxes(&mut snapped, scale);
            render_with_font_cached(&snapped, width_css, height_css, scale, true, font, cache)
        }
        RasterizationMode::Gpui => {
            // Resolve the same sprite fallbacks as HtmlView before snapping
            // native quads; their internal geometry stays antialiased.
            let mut resolved = rasterize_for_gpui(list, scale, font, cache, false)?;
            gpui::snap(&mut resolved, scale);
            render_internal_mode(
                &resolved,
                width_css,
                height_css,
                scale,
                true,
                Some(font),
                cache,
                true,
            )
        }
    }
}

pub struct FileImages {
    base: PathBuf,
    root: Option<PathBuf>,
    cache: RefCell<HashMap<String, Result<Arc<ImageData>, ImageFailure>>>,
    cached_bytes: RefCell<usize>,
    decoded_bytes: Cell<u64>,
    generation: Cell<u64>,
}

/// Compact cached failures avoid retaining parser trees or capability diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageFailure {
    Resource,
    Decode,
    Limit,
    UnsupportedDrawing,
}

impl FileImages {
    /// Cumulative pixel bytes produced by image decodes; unchanged images are
    /// served from the cache and never add to it.
    pub fn decoded_bytes(&self) -> u64 {
        self.decoded_bytes.get()
    }

    pub fn failure(&self, source: &str) -> Option<ImageFailure> {
        self.cache
            .borrow()
            .get(source)
            .and_then(|entry| entry.as_ref().err().copied())
    }

    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self {
            base: base.into(),
            root: None,
            cache: RefCell::new(HashMap::new()),
            cached_bytes: RefCell::new(0),
            decoded_bytes: Cell::new(0),
            generation: Cell::new(0),
        }
    }

    /// Allow document-relative parent paths and URL-root paths while confining
    /// every canonical file (including symlinks) to an explicit resource root.
    pub fn with_root(base: impl Into<PathBuf>, root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let base = base.into().canonicalize()?;
        let root = root.into().canonicalize()?;
        if !base.starts_with(&root) || !base.is_dir() || !root.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "image document directory must be inside the resource root",
            ));
        }
        let mut images = Self::new(base);
        images.root = Some(root);
        Ok(images)
    }

    fn file_path(&self, source: &str) -> Option<PathBuf> {
        if let Some(root) = &self.root {
            let source = source.split(['?', '#']).next()?;
            let decoded = lumen_common::codec::percent_decode(source.as_bytes());
            let source = std::str::from_utf8(&decoded).ok()?;
            let path = if source.starts_with('/') {
                root.join(source.trim_start_matches('/'))
            } else {
                self.base.join(source)
            }
            .canonicalize()
            .ok()?;
            (path.starts_with(root) && path.is_file()).then_some(path)
        } else {
            let path = std::path::Path::new(source);
            (!path.is_absolute()
                && path
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_))))
            .then(|| self.base.join(path))
        }
    }
}

impl lumen_html::layout::ImageResolver for FileImages {
    fn generation(&self) -> u64 {
        self.generation.get()
    }

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
        let bytes = if let Some(data) = source
            .get(..5)
            .filter(|prefix| prefix.eq_ignore_ascii_case("data:"))
            .map(|_| &source[5..])
        {
            data.split_once(',').and_then(|(metadata, payload)| {
                let payload = lumen_common::codec::percent_decode(payload.as_bytes());
                if metadata
                    .split(';')
                    .any(|part| part.eq_ignore_ascii_case("base64"))
                {
                    lumen_common::codec::base64_decode_forgiving(&payload)
                } else {
                    Some(payload)
                }
            })
        } else {
            self.file_path(source)
                .and_then(|path| std::fs::File::open(path).ok())
                .and_then(|file| {
                    let mut bytes = Vec::new();
                    file.take(MAX_PNG_FILE_BYTES + 1)
                        .read_to_end(&mut bytes)
                        .ok()?;
                    (bytes.len() as u64 <= MAX_PNG_FILE_BYTES).then_some(bytes)
                })
        };
        let decoded = bytes
            .ok_or(ImageFailure::Resource)
            .and_then(|bytes| {
                if bytes.len() as u64 > MAX_PNG_FILE_BYTES {
                    return Err(ImageFailure::Limit);
                }
                decode_image_with_limit(&bytes, remaining_bytes).map_err(|error| match error {
                    ImageError::TooLarge => ImageFailure::Limit,
                    ImageError::UnsupportedSvg(_) => ImageFailure::UnsupportedDrawing,
                    _ => ImageFailure::Decode,
                })
            })
            .map(|image| {
                Arc::new(ImageData {
                    width: image.width,
                    height: image.height,
                    pixels: image.pixels,
                })
            });
        let bytes = source.len() + decoded.as_ref().map_or(0, |image| image.pixels.len());
        self.decoded_bytes.set(self.decoded_bytes.get().saturating_add(
            decoded.as_ref().map_or(0, |image| image.pixels.len() as u64),
        ));
        let mut cached_bytes = self.cached_bytes.borrow_mut();
        if *cached_bytes + bytes > ASSET_CACHE_BYTES {
            return ImageState::Failed;
        }
        *cached_bytes += bytes;
        self.cache
            .borrow_mut()
            .insert(source.to_owned(), decoded.clone());
        self.generation.set(self.generation.get().wrapping_add(1));
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
    Xml(lumen_html::xml::ParseError),
    Svg(&'static str),
    UnsupportedSvg(Vec<lumen_html::layout::SvgUnsupportedFeature>),
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
    glyphs: HashMap<(u64, u16, u32), (GlyphCoverage, u64)>,
    recency: std::collections::BTreeMap<u64, (u64, u16, u32)>,
    tick: u64,
    bytes: usize,
    hits: u64,
    misses: u64,
}

/// Cumulative glyph coverage cache counters; callers diff two reads for a frame.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GlyphCacheStats {
    pub entries: usize,
    pub bytes: usize,
    pub hits: u64,
    pub misses: u64,
}

impl GlyphCache {
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn stats(&self) -> GlyphCacheStats {
        GlyphCacheStats {
            entries: self.glyphs.len(),
            bytes: self.bytes,
            hits: self.hits,
            misses: self.misses,
        }
    }

    fn lookup(&mut self, key: &(u64, u16, u32)) -> Option<&GlyphCoverage> {
        let Some((coverage, used)) = self.glyphs.get_mut(key) else {
            self.misses += 1;
            return None;
        };
        self.hits += 1;
        self.tick += 1;
        self.recency.remove(used);
        *used = self.tick;
        self.recency.insert(self.tick, *key);
        Some(&*coverage)
    }

    pub fn clear(&mut self) {
        self.glyphs.clear();
        self.recency.clear();
        self.bytes = 0;
    }

    fn evict_oldest(&mut self) -> bool {
        let Some((_, key)) = self.recency.pop_first() else {
            return false;
        };
        if let Some((coverage, _)) = self.glyphs.remove(&key) {
            self.bytes = self.bytes.saturating_sub(coverage.alpha.len());
        }
        true
    }

    fn insert(&mut self, key: (u64, u16, u32), coverage: GlyphCoverage) {
        let bytes = coverage.alpha.len();
        if let Some((old, used)) = self.glyphs.remove(&key) {
            self.recency.remove(&used);
            self.bytes = self.bytes.saturating_sub(old.alpha.len());
        }
        while (self.bytes + bytes > GLYPH_CACHE_BYTES || self.glyphs.len() >= GLYPH_CACHE_ENTRIES)
            && self.evict_oldest()
        {}
        self.tick += 1;
        self.bytes += bytes;
        self.recency.insert(self.tick, key);
        self.glyphs.insert(key, (coverage, self.tick));
    }
}

fn blit_glyph(
    image: &mut Rgba8Image,
    scale: f32,
    clip: Rect,
    (x, y): (f32, f32),
    color: Rgba,
    coverage: &GlyphCoverage,
) {
    let left = (x * scale).round() as i32 + coverage.x_min;
    let top = (y * scale).round() as i32 - coverage.y_min - coverage.height as i32;
    let mut columns = 0..0;
    for col in 0..coverage.width {
        let x = left + col as i32;
        let css_x = (x as f32 + 0.5) / scale;
        let visible =
            x >= 0 && x < image.width as i32 && css_x >= clip.x && css_x < clip.x + clip.width;
        if visible {
            if columns.is_empty() {
                columns.start = col;
            }
            columns.end = col + 1;
        }
    }
    if columns.is_empty() {
        return;
    }
    for row in 0..coverage.height {
        let y = top + row as i32;
        if y < 0 || y >= image.height as i32 {
            continue;
        }
        let css_y = (y as f32 + 0.5) / scale;
        if css_y < clip.y || css_y >= clip.y + clip.height {
            continue;
        }
        let alphas = &coverage.alpha
            [row * coverage.width + columns.start..row * coverage.width + columns.end];
        let start =
            ((y as usize * image.width as usize) + (left + columns.start as i32) as usize) * 4;
        let targets = &mut image.pixels[start..start + alphas.len() * 4];
        for (target, &alpha) in targets.chunks_exact_mut(4).zip(alphas) {
            if alpha == 0 {
                continue;
            }
            if alpha == 255 && color.a == 255 {
                target.copy_from_slice(&[color.r, color.g, color.b, 255]);
            } else {
                composite(target, color, alpha as f32 / 255.0);
            }
        }
    }
}

struct Raster<'a> {
    image: &'a mut Rgba8Image,
    scale: f32,
    antialias: bool,
    gpui_coverage: bool,
    clips: Vec<Rect>,
    font: Option<&'a dyn FontProvider>,
    cache: &'a mut GlyphCache,
    error: Option<ImageError>,
}

impl Raster<'_> {
    /// Pixel rectangle covering `visible` (CSS units), clamped to the image.
    fn pixel_span(&self, visible: Rect) -> (u32, u32, u32, u32) {
        (
            (visible.x * self.scale).floor().max(0.0) as u32,
            (visible.y * self.scale).floor().max(0.0) as u32,
            ((visible.x + visible.width) * self.scale)
                .ceil()
                .min(self.image.width as f32) as u32,
            ((visible.y + visible.height) * self.scale)
                .ceil()
                .min(self.image.height as f32) as u32,
        )
    }
}

fn transformed_svg_path_bounds(
    path: &tiny_skia::Path,
    transform: Affine,
    stroke: bool,
    stroke_width: f32,
    scale: f32,
) -> Option<Rect> {
    let bounds = path.bounds();
    let bounds = transform.bounds(Rect {
        x: bounds.x(),
        y: bounds.y(),
        width: bounds.width(),
        height: bounds.height(),
    });
    let scale_x = transform.a.hypot(transform.b);
    let scale_y = transform.c.hypot(transform.d);
    let extent = if stroke {
        stroke_width * scale_x.max(scale_y) * 2.0 + 1.0 / scale.max(f32::MIN_POSITIVE)
    } else {
        1.0 / scale.max(f32::MIN_POSITIVE)
    };
    Some(Rect {
        x: bounds.x - extent,
        y: bounds.y - extent,
        width: bounds.width + 2.0 * extent,
        height: bounds.height + 2.0 * extent,
    })
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
        let solid_columns = (bounds.x.ceil().max(x0 as f32) as u32)
            ..((bounds.x + bounds.width).floor().min(x1 as f32) as u32);
        for y in y0..y1 {
            let mut edges = (x0..x1, 0..0);
            if color.a == 255
                && self.antialias
                && !solid_columns.is_empty()
                && bounds.y <= y as f32
                && bounds.y + bounds.height >= y as f32 + 1.0
            {
                let pixel = [color.r, color.g, color.b, 255];
                let row = (y as usize * self.image.width as usize) * 4;
                let span =
                    row + solid_columns.start as usize * 4..row + solid_columns.end as usize * 4;
                for target in self.image.pixels[span].chunks_exact_mut(4) {
                    target.copy_from_slice(&pixel);
                }
                edges = (x0..solid_columns.start, solid_columns.end..x1);
            }
            for x in edges.0.chain(edges.1) {
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

    fn fill_rounded_rect(
        &mut self,
        rect: Rect,
        radius: f32,
        corners: Option<&[[f32; 2]; 4]>,
        color: Rgba,
    ) {
        if let Some(corners) = corners {
            let Some(visible) = self.clips.last().and_then(|clip| clip.intersection(rect)) else {
                return;
            };
            let (x0, y0, x1, y1) = self.pixel_span(visible);
            let scaled = corners.map(|r| [r[0] * self.scale, r[1] * self.scale]);
            for y in y0..y1 {
                for x in x0..x1 {
                    let coverage = if self.antialias {
                        coverage::rounded_corners(
                            x as f32,
                            y as f32,
                            rect.x * self.scale,
                            rect.y * self.scale,
                            (rect.x + rect.width) * self.scale,
                            (rect.y + rect.height) * self.scale,
                            scaled,
                        )
                    } else {
                        rect.contains_corners(
                            (x as f32 + 0.5) / self.scale,
                            (y as f32 + 0.5) / self.scale,
                            corners,
                        ) as u8 as f32
                    };
                    let offset = (y as usize * self.image.width as usize + x as usize) * 4;
                    composite(&mut self.image.pixels[offset..offset + 4], color, coverage);
                }
            }
            return;
        }
        if self.gpui_coverage {
            gpui::draw_quad(self, rect, radius, None, color);
            return;
        }
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
        let (x0, y0, x1, y1) = self.pixel_span(visible);
        let left = rect.x * self.scale;
        let top = rect.y * self.scale;
        let right = (rect.x + rect.width) * self.scale;
        let bottom = (rect.y + rect.height) * self.scale;
        let r = radius * self.scale;
        let r2 = r * r;
        for y in y0..y1 {
            let mut edges = (x0..x1, 0..0);
            if self.antialias && y as f32 >= top && y as f32 + 1.0 <= bottom {
                let (span_left, span_right) = if y as f32 >= top + r && y as f32 + 1.0 <= bottom - r
                {
                    (left, right)
                } else {
                    (left + r, right - r)
                };
                let start = (span_left.ceil().max(x0 as f32) as u32).min(x1);
                let end = (span_right.floor().min(x1 as f32) as u32).max(start);
                if start < end {
                    let row = y as usize * self.image.width as usize;
                    let span = (row + start as usize) * 4..(row + end as usize) * 4;
                    for target in self.image.pixels[span].chunks_exact_mut(4) {
                        if color.a == 255 {
                            target.copy_from_slice(&[color.r, color.g, color.b, 255]);
                        } else {
                            composite(target, color, 1.0);
                        }
                    }
                    edges = (x0..start, end..x1);
                }
            }
            for x in edges.0.chain(edges.1) {
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
        corners: Option<&[[f32; 2]; 4]>,
        rect: Rect,
        radius: f32,
        positioning_rect: Rect,
        image_rect: Rect,
        repeat: [lumen_html::paint::BackgroundRepeat; 2],
        image: &lumen_html::paint::BackgroundPaint,
    ) {
        background::fill(
            self,
            corners,
            rect,
            radius,
            positioning_rect,
            image_rect,
            repeat,
            image,
        );
    }

    fn draw_shadow(
        &mut self,
        rect: Rect,
        radius: f32,
        corners: Option<&[[f32; 2]; 4]>,
        shadow: lumen_html::paint::BoxShadow,
    ) {
        if let Err(error) = shadow::draw(self, rect, radius, corners, shadow) {
            self.error = Some(error);
        }
    }

    fn stroke_border(&mut self, rect: Rect, radius: f32, width: f32, color: Rgba) {
        if self.gpui_coverage {
            gpui::draw_quad(self, rect, radius, Some(width), color);
            return;
        }
        border::draw(self, rect, radius, width, color, None);
    }

    fn stroke_box_border(&mut self, border: &lumen_html::paint::BoxBorder) {
        border::draw_box(self, border);
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
        for glyph in glyphs {
            let scaled_size = size * glyph.size_scale * self.scale;
            let key = (
                match font.face_key(glyph.face) {
                    Ok(id) => id,
                    Err(error) => {
                        self.error = Some(ImageError::Font(error));
                        return;
                    }
                },
                glyph.id,
                scaled_size.to_bits(),
            );
            let origin = (origin_x + glyph.x, baseline_y - glyph.y);
            if let Some(coverage) = self.cache.lookup(&key) {
                if let Some(clip) = self.clips.last().copied() {
                    blit_glyph(self.image, self.scale, clip, origin, color, coverage);
                }
                continue;
            }
            let bitmap = match font.rasterize_glyph(glyph.face, glyph.id, scaled_size) {
                Ok(bitmap) => bitmap,
                Err(error) => {
                    self.error = Some(ImageError::Font(error));
                    return;
                }
            };
            if let Some(clip) = self.clips.last().copied() {
                blit_glyph(self.image, self.scale, clip, origin, color, &bitmap);
            }
            let bytes = bitmap.alpha.len();
            if bytes <= GLYPH_CACHE_BYTES {
                self.cache.insert(key, bitmap);
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
        let (x0, y0, x1, y1) = self.pixel_span(visible);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let columns: Vec<(usize, f32)> = (x0..x1)
            .map(|x| {
                let center_x = (x as f32 + 0.5) / self.scale;
                let sx = ((((center_x - rect.x) / rect.width * image.width as f32).ceil() - 1.0)
                    .max(0.0) as u32)
                    .min(image.width - 1);
                let left = if self.antialias {
                    ((visible.x + visible.width).min((x as f32 + 1.0) / self.scale)
                        - visible.x.max(x as f32 / self.scale))
                    .clamp(0.0, 1.0 / self.scale)
                        * self.scale
                } else {
                    1.0
                };
                (sx as usize * 4, left)
            })
            .collect();
        for y in y0..y1 {
            let center_y = (y as f32 + 0.5) / self.scale;
            let sy = ((((center_y - rect.y) / rect.height * image.height as f32).ceil() - 1.0)
                .max(0.0) as u32)
                .min(image.height - 1);
            let top = if self.antialias {
                ((visible.y + visible.height).min((y as f32 + 1.0) / self.scale)
                    - visible.y.max(y as f32 / self.scale))
                .clamp(0.0, 1.0 / self.scale)
                    * self.scale
            } else {
                1.0
            };
            let source_row = &image.pixels[sy as usize * image.width as usize * 4
                ..(sy as usize + 1) * image.width as usize * 4];
            let start = (y as usize * self.image.width as usize + x0 as usize) * 4;
            let targets = &mut self.image.pixels[start..start + columns.len() * 4];
            for (target, &(sx, left)) in targets.chunks_exact_mut(4).zip(&columns) {
                let coverage = left * top;
                if coverage == 0.0 {
                    continue;
                }
                let pixel = &source_row[sx..sx + 4];
                if pixel[3] == 255 && coverage == 1.0 {
                    target.copy_from_slice(pixel);
                } else {
                    composite(
                        target,
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

    fn draw_svg_path(
        &mut self,
        bounds: Rect,
        data: &str,
        transform: Affine,
        fill: Option<&lumen_html::paint::SvgPaint>,
        stroke: Option<&lumen_html::paint::SvgPaint>,
        stroke_width: f32,
        fill_rule: PaintSvgFillRule,
        clips: &[lumen_html::paint::SvgClip],
    ) {
        if !lumen_html::paint::svg_paint_has_ink(fill)
            && (!lumen_html::paint::svg_paint_has_ink(stroke) || stroke_width == 0.0)
        {
            return;
        }
        let Ok(parsed) = canvas::parse_svg_path(data) else {
            // SVG ignores malformed path data; the shared Canvas parser is the
            // single bounded implementation used here and by Path2D.
            return;
        };
        let Some(path) = parsed.path else {
            return;
        };
        let Some(path_bounds) = transformed_svg_path_bounds(
            &path,
            transform,
            lumen_html::paint::svg_paint_has_ink(stroke),
            stroke_width,
            self.scale,
        ) else {
            return;
        };
        let Some(visible) = self
            .clips
            .last()
            .copied()
            .and_then(|clip| clip.intersection(bounds))
            .and_then(|visible| visible.intersection(path_bounds))
        else {
            return;
        };
        let (x0, y0, x1, y1) = self.pixel_span(visible);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let (width, height) = (x1 - x0, y1 - y0);
        let len = match (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
        {
            Some(len) => len,
            None => {
                self.error = Some(ImageError::TooLarge);
                return;
            }
        };
        if len > MAX_IMAGE_BYTES {
            self.error = Some(ImageError::TooLarge);
            return;
        }
        let mut surface = match canvas::CanvasSurface::new(width, height) {
            Ok(surface) => surface,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        let path_transform = tiny_skia::Transform::from_row(
            transform.a * self.scale,
            transform.b * self.scale,
            transform.c * self.scale,
            transform.d * self.scale,
            transform.e * self.scale - x0 as f32,
            transform.f * self.scale - y0 as f32,
        );
        surface.state_mut().transform = path_transform;
        surface.state_mut().line.width = stroke_width;

        let path_bounds = path.bounds();
        let object_box = lumen_html::paint::Affine {
            a: path_bounds.width(),
            b: 0.0,
            c: 0.0,
            d: path_bounds.height(),
            e: path_bounds.x(),
            f: path_bounds.y(),
        };
        let screen = lumen_html::paint::Affine {
            a: self.scale,
            b: 0.0,
            c: 0.0,
            d: self.scale,
            e: -(x0 as f32),
            f: -(y0 as f32),
        };
        for clip in clips {
            let unit_transform = match clip.units {
                lumen_html::paint::SvgGradientUnits::ObjectBoundingBox => object_box,
                lumen_html::paint::SvgGradientUnits::UserSpaceOnUse => {
                    lumen_html::paint::Affine::IDENTITY
                }
            };
            let mut parsed_shapes = Vec::with_capacity(clip.shapes.len());
            for shape in clip.shapes.iter() {
                let Ok(parsed_clip) = canvas::parse_svg_path(&shape.data) else {
                    continue;
                };
                let Some(clip_path) = parsed_clip.path else {
                    continue;
                };
                let clip_transform = screen
                    .then(transform)
                    .then(unit_transform)
                    .then(clip.transform)
                    .then(shape.transform);
                parsed_shapes.push((
                    clip_path,
                    match shape.fill_rule {
                        PaintSvgFillRule::NonZero => tiny_skia::FillRule::Winding,
                        PaintSvgFillRule::EvenOdd => tiny_skia::FillRule::EvenOdd,
                    },
                    tiny_skia::Transform::from_row(
                        clip_transform.a,
                        clip_transform.b,
                        clip_transform.c,
                        clip_transform.d,
                        clip_transform.e,
                        clip_transform.f,
                    ),
                ));
            }
            let clip_paths = parsed_shapes
                .iter()
                .map(|(path, rule, transform)| (path, *rule, *transform))
                .collect::<Vec<_>>();
            if let Err(error) = surface.clip_paths_union(&clip_paths) {
                self.error = Some(error);
                return;
            }
        }

        let make_gradient = |paint: Option<&lumen_html::paint::SvgPaint>| {
            let Some(lumen_html::paint::SvgPaint::Gradient(gradient)) = paint else {
                return None;
            };
            let unit_transform = match gradient.units {
                lumen_html::paint::SvgGradientUnits::ObjectBoundingBox => object_box,
                lumen_html::paint::SvgGradientUnits::UserSpaceOnUse => {
                    lumen_html::paint::Affine::IDENTITY
                }
            };
            let gradient_transform = screen
                .then(transform)
                .then(unit_transform)
                .then(gradient.transform);
            let gradient_transform = tiny_skia::Transform::from_row(
                gradient_transform.a,
                gradient_transform.b,
                gradient_transform.c,
                gradient_transform.d,
                gradient_transform.e,
                gradient_transform.f,
            );
            let kind = match gradient.kind {
                lumen_html::paint::SvgGradientKind::Linear { start, end } => {
                    canvas::CanvasGradientKind::Linear {
                        start,
                        end,
                        transform: gradient_transform,
                    }
                }
                lumen_html::paint::SvgGradientKind::Radial {
                    start,
                    end,
                    start_radius,
                    end_radius,
                } => canvas::CanvasGradientKind::Radial {
                    start,
                    end,
                    start_radius,
                    end_radius,
                    transform: gradient_transform,
                },
            };
            let resolved = canvas::CanvasGradient::new(kind);
            for stop in gradient.stops.iter() {
                if resolved
                    .add_color_stop(
                        stop.offset,
                        [stop.color.r, stop.color.g, stop.color.b, stop.color.a],
                    )
                    .is_err()
                {
                    return None;
                }
            }
            Some(resolved)
        };
        if let Some(lumen_html::paint::SvgPaint::Color(color)) = fill {
            surface.state_mut().fill = [color.r, color.g, color.b, color.a];
        }
        if let Some(lumen_html::paint::SvgPaint::Color(color)) = stroke {
            surface.state_mut().stroke = [color.r, color.g, color.b, color.a];
        }
        surface.state_mut().fill_gradient = make_gradient(fill);
        surface.state_mut().stroke_gradient = make_gradient(stroke);
        if lumen_html::paint::svg_paint_has_ink(fill) {
            if let Err(error) = surface.fill_path(
                &path,
                match fill_rule {
                    PaintSvgFillRule::NonZero => tiny_skia::FillRule::Winding,
                    PaintSvgFillRule::EvenOdd => tiny_skia::FillRule::EvenOdd,
                },
            ) {
                self.error = Some(error);
                return;
            }
        }
        if lumen_html::paint::svg_paint_has_ink(stroke) && stroke_width > 0.0 {
            if let Err(error) = surface.stroke_path(&path) {
                self.error = Some(error);
                return;
            }
        }
        let output = surface.snapshot();
        for row in 0..height as usize {
            let source = row * width as usize * 4;
            let target = ((y0 as usize + row) * self.image.width as usize + x0 as usize) * 4;
            for column in 0..width as usize {
                let source = source + column * 4;
                if output.pixels[source + 3] == 0 {
                    continue;
                }
                let target = target + column * 4;
                composite(
                    &mut self.image.pixels[target..target + 4],
                    Rgba {
                        r: output.pixels[source],
                        g: output.pixels[source + 1],
                        b: output.pixels[source + 2],
                        a: output.pixels[source + 3],
                    },
                    1.0,
                );
            }
        }
    }
}

fn composite(dst: &mut [u8], src: Rgba, coverage: f32) {
    composite_internal(dst, src, coverage, 1.0, true);
}

fn composite_native(dst: &mut [u8], src: Rgba, coverage: f32, rgb_scale: f32) {
    composite_internal(dst, src, coverage, rgb_scale, false);
}

fn composite_internal(
    dst: &mut [u8],
    src: Rgba,
    coverage: f32,
    rgb_scale: f32,
    quantize_sources: bool,
) {
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
        let source = source as f32 * sa * rgb_scale;
        let destination = dst[channel] as f32 * da;
        let color = if quantize_sources {
            source.round() + (destination.round() * (1.0 - sa)).round()
        } else {
            source + destination * (1.0 - sa)
        };
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
    font: &dyn FontProvider,
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
    font: &dyn FontProvider,
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
    font: &dyn FontProvider,
    cache: &mut GlyphCache,
) -> Result<DisplayList, ImageError> {
    rasterize_for_gpui(list, scale, font, cache, false)
}

/// Custom explicit font providers retain face identity through the sprite
/// route when a window's native text system has not registered those faces.
pub fn rasterize_for_gpui(
    list: &DisplayList,
    scale: f32,
    font: &dyn FontProvider,
    cache: &mut GlyphCache,
    rasterize_text: bool,
) -> Result<DisplayList, ImageError> {
    let _html_allocations = lumen_common::memcat::enter(lumen_common::memcat::CategoryTag::HTML);
    list.validate().map_err(ImageError::DisplayList)?;
    if !scale.is_finite() || scale <= 0.0 {
        return Err(ImageError::InvalidViewport);
    }
    let mut bytes = 0;
    let mut resolved = resolve_layers(&list.0, scale, Some(font), cache, &mut bytes)?;
    for command in &mut resolved.0 {
        let rect = match command {
            Command::FillGradient { rect, .. } => *rect,
            Command::MaskedBackground(mask) => mask.rect,
            Command::FillBackground(fill) => fill.rect,
            Command::BoxShadow { rect, shadow, .. } => shadow.bounds(*rect),
            Command::StrokePatternBorder { rect, .. } => *rect,
            Command::StrokeBoxBorder(border) => border.rect,
            Command::SvgPath { bounds, .. } => *bounds,
            Command::GlyphRun { .. } if rasterize_text => {
                let Some(bounds) =
                    paint_bounds(core::slice::from_ref(&*command), scale, Some(font), cache)?
                else {
                    continue;
                };
                bounds
            }
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
    list: &[Command],
    scale: f32,
    font: Option<&dyn FontProvider>,
    cache: &mut GlyphCache,
    bytes: &mut usize,
) -> Result<DisplayList, ImageError> {
    let mut output = DisplayList(Vec::with_capacity(list.len()));
    let mut index = 0;
    while index < list.len() {
        if let Command::MaskedBackground(mask) = &list[index] {
            index += 1;
            let left = (mask.rect.x * scale).floor();
            let top = (mask.rect.y * scale).floor();
            let width = ((mask.rect.x + mask.rect.width) * scale).ceil() - left;
            let height = ((mask.rect.y + mask.rect.height) * scale).ceil() - top;
            if width <= 0.0 || height <= 0.0 || mask.mask.0.is_empty() {
                continue;
            }
            if !width.is_finite()
                || !height.is_finite()
                || width > u32::MAX as f32
                || height > u32::MAX as f32
            {
                return Err(ImageError::TooLarge);
            }
            let size = (width as usize)
                .checked_mul(height as usize)
                .and_then(|n| n.checked_mul(8))
                .ok_or(ImageError::TooLarge)?;
            *bytes = bytes.checked_add(size).ok_or(ImageError::TooLarge)?;
            if *bytes > MAX_LAYER_BYTES {
                return Err(ImageError::TooLarge);
            }
            let x = left / scale;
            let y = top / scale;
            let mut paint = DisplayList(vec![(*mask.paint).clone()]);
            for c in &mut paint.0 {
                translate_command(c, -x, -y);
            }
            let mut image =
                render_scaled_region(&paint, width as u32, height as u32, scale, font, cache)?;
            let mut ink = mask.mask.0.clone();
            for c in &mut ink {
                translate_command(c, mask.offset[0] - x, mask.offset[1] - y);
            }
            let ink = resolve_layers(&ink, scale, font, cache, bytes)?;
            let coverage =
                render_scaled_region(&ink, width as u32, height as u32, scale, font, cache)?;
            for (pixel, ink) in image
                .pixels
                .chunks_exact_mut(4)
                .zip(coverage.pixels.chunks_exact(4))
            {
                pixel[3] = ((pixel[3] as u16 * ink[3] as u16 + 127) / 255) as u8;
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
            continue;
        }
        if let Command::PushTransform(matrix) = list[index] {
            let start = index + 1;
            let mut depth = 1;
            index += 1;
            while depth != 0 {
                match list.get(index) {
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
            // Integral translations retain the original primitives and glyph cache.
            if matrix.a == 1.0
                && matrix.b == 0.0
                && matrix.c == 0.0
                && matrix.d == 1.0
                && (matrix.e * scale).fract() == 0.0
                && (matrix.f * scale).fract() == 0.0
            {
                let mut child = list[start..end].to_vec();
                for command in &mut child {
                    translate_command(command, matrix.e, matrix.f);
                }
                output
                    .0
                    .extend(resolve_layers(&child, scale, font, cache, bytes)?.0);
                continue;
            }
            let mut child = resolve_layers(&list[start..end], scale, font, cache, bytes)?;
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
            let Some(crop) = paint_bounds(&child.0, scale, font, cache)? else {
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
            ref corners,
            rect,
            radius,
            opacity,
            clip,
        } = list[index]
        else {
            output.0.push(list[index].clone());
            index += 1;
            continue;
        };
        let start = index + 1;
        let mut depth = 1;
        index += 1;
        while depth != 0 {
            match list.get(index) {
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
        let mut child = resolve_layers(&list[start..end], scale, font, cache, bytes)?;
        let Some(crop) = paint_bounds(&child.0, scale, font, cache)?.and_then(|bounds| {
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
        let row_len = image.width as usize * 4;
        if clip {
            let inset_x = match corners {
                Some(corners) => corners
                    .iter()
                    .map(|r| r[0] * scale)
                    .fold(0.0f32, f32::max),
                None => radius
                    .max(0.0)
                    .min((right - clip_left) * 0.5)
                    .min((bottom - clip_top) * 0.5),
            };
            let interior_start = clip_left + inset_x;
            let interior_end = right - inset_x;
            for (py, row) in image.pixels.chunks_exact_mut(row_len).enumerate() {
                let row_inside = py as f32 >= clip_top && (py + 1) as f32 <= bottom;
                for (px, pixel) in row.chunks_exact_mut(4).enumerate() {
                    if row_inside
                        && px as f32 >= interior_start
                        && (px + 1) as f32 <= interior_end
                    {
                        if opacity != 1.0 {
                            pixel[3] = (pixel[3] as f32 * opacity).round() as u8;
                        }
                        continue;
                    }
                    let coverage = if let Some(corners) = corners {
                        coverage::rounded_corners(
                            px as f32,
                            py as f32,
                            clip_left,
                            clip_top,
                            right,
                            bottom,
                            corners.map(|r| [r[0] * scale, r[1] * scale]),
                        )
                    } else {
                        coverage::rounded(
                            px as f32, py as f32, clip_left, clip_top, right, bottom, radius,
                        )
                    };
                    pixel[3] = (pixel[3] as f32 * opacity * coverage).round() as u8;
                }
            }
        } else if opacity != 1.0 {
            for pixel in image.pixels.chunks_exact_mut(4) {
                pixel[3] = (pixel[3] as f32 * opacity).round() as u8;
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
    list: &[Command],
    scale: f32,
    font: Option<&dyn FontProvider>,
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
    for command in list {
        match command {
            Command::FillRect { rect, color }
            | Command::FillRoundedRect { rect, color, .. }
            | Command::StrokeBorder { rect, color, .. }
            | Command::StrokePatternBorder { rect, color, .. }
                if color.a != 0 =>
            {
                include(*rect)
            }
            Command::Image { rect, .. } | Command::FillGradient { rect, .. } => include(*rect),
            Command::SvgPath {
                bounds,
                data,
                transform,
                fill,
                stroke,
                stroke_width,
                ..
            } if lumen_html::paint::svg_paint_has_ink(fill.as_ref())
                || lumen_html::paint::svg_paint_has_ink(stroke.as_ref()) =>
            {
                if let Ok(parsed) = canvas::parse_svg_path(data) {
                    if let Some(path) = parsed.path {
                        let path_bounds = transformed_svg_path_bounds(
                            &path,
                            *transform,
                            lumen_html::paint::svg_paint_has_ink(stroke.as_ref()),
                            *stroke_width,
                            scale,
                        );
                        if let Some(path_bounds) =
                            path_bounds.and_then(|path_bounds| path_bounds.intersection(*bounds))
                        {
                            include(path_bounds);
                        }
                    }
                }
            }
            Command::MaskedBackground(mask) => include(mask.rect),
            Command::FillBackground(fill) => include(fill.rect),
            Command::StrokeBoxBorder(border) => include(border.rect),
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
                for glyph in glyphs.iter() {
                    let key = (
                        font.face_key(glyph.face).map_err(ImageError::Font)?,
                        glyph.id,
                        (size * glyph.size_scale * scale).to_bits(),
                    );
                    let (x_min, y_min, width, height) = if let Some(c) = cache.lookup(&key) {
                        (c.x_min, c.y_min, c.width, c.height)
                    } else {
                        let coverage = font
                            .rasterize_glyph(glyph.face, glyph.id, size * glyph.size_scale * scale)
                            .map_err(ImageError::Font)?;
                        if coverage.alpha.len() > GLYPH_CACHE_BYTES {
                            return Err(ImageError::TooLarge);
                        }
                        let metrics = (
                            coverage.x_min,
                            coverage.y_min,
                            coverage.width,
                            coverage.height,
                        );
                        cache.insert(key, coverage);
                        metrics
                    };
                    include(Rect {
                        x: (((origin_x + glyph.x) * scale).round() + x_min as f32) / scale,
                        y: (((baseline_y - glyph.y) * scale).round()
                            - y_min as f32
                            - height as f32)
                            / scale,
                        width: width as f32 / scale,
                        height: height as f32 / scale,
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
        Command::MaskedBackground(mask) => {
            mask.rect.x += x;
            mask.rect.y += y;
            mask.offset[0] += x;
            mask.offset[1] += y;
            translate_command(&mut mask.paint, x, y);
        }
        Command::StrokeBoxBorder(border) => {
            border.rect.x += x;
            border.rect.y += y;
        }
        Command::FillBackground(fill) => {
            let fill = &mut **fill;
            for rect in [
                &mut fill.rect,
                &mut fill.positioning_rect,
                &mut fill.image_rect,
            ] {
                rect.x += x;
                rect.y += y;
            }
        }
        Command::PushTransform(matrix) => *matrix = matrix.translated_space(x, y),
        Command::SvgPath {
            bounds, transform, ..
        } => {
            bounds.x += x;
            bounds.y += y;
            transform.e += x;
            transform.f += y;
        }
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
    font: Option<&dyn FontProvider>,
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
        gpui_coverage: false,
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
    font: Option<&dyn FontProvider>,
    cache: &mut GlyphCache,
) -> Result<Rgba8Image, ImageError> {
    render_internal_mode(
        list, width_css, height_css, scale, antialias, font, cache, false,
    )
}

fn render_internal_mode(
    list: &DisplayList,
    width_css: u32,
    height_css: u32,
    scale: f32,
    antialias: bool,
    font: Option<&dyn FontProvider>,
    cache: &mut GlyphCache,
    gpui_coverage: bool,
) -> Result<Rgba8Image, ImageError> {
    let _html_allocations = lumen_common::memcat::enter(lumen_common::memcat::CategoryTag::HTML);
    if !scale.is_finite() || scale <= 0.0 || width_css == 0 || height_css == 0 {
        return Err(ImageError::InvalidViewport);
    }
    let resolved;
    let list = if list.0.iter().any(|command| {
        matches!(
            command,
            Command::MaskedBackground(_) | Command::PushLayer { .. } | Command::PushTransform(_)
        )
    }) {
        list.validate().map_err(ImageError::DisplayList)?;
        resolved = resolve_layers(&list.0, scale, font, cache, &mut 0)?;
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
        gpui_coverage,
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

/// Encode a canvas bitmap using an implemented Canvas serialization format.
/// Unsupported requested MIME types fall back to PNG. JPEG composites
/// transparency against black as required for canvas serialization. WebP uses
/// the shared crate's lossless encoder because it does not expose lossy WebP.
pub fn encode_canvas_image(
    image: &Rgba8Image,
    requested_type: Option<&str>,
    quality: Option<f64>,
) -> Result<(Vec<u8>, &'static str), ImageError> {
    let mime = requested_type.unwrap_or("image/png").to_ascii_lowercase();
    let pixel_count = (image.width as usize)
        .checked_mul(image.height as usize)
        .ok_or(ImageError::TooLarge)?;
    let rgba_bytes = pixel_count.checked_mul(4).ok_or(ImageError::TooLarge)?;
    match mime.as_str() {
        "image/jpeg" => {
            if image.pixels.len() != rgba_bytes {
                return Err(ImageError::InvalidViewport);
            }
            let rgb_bytes = pixel_count.checked_mul(3).ok_or(ImageError::TooLarge)?;
            let mut rgb = Vec::with_capacity(rgb_bytes);
            for rgba in image.pixels.chunks_exact(4) {
                let alpha = u16::from(rgba[3]);
                rgb.extend(
                    rgba[..3]
                        .iter()
                        .map(|channel| ((u16::from(*channel) * alpha + 127) / 255) as u8),
                );
            }
            let quality = quality
                .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
                .unwrap_or(0.92);
            let quality = (quality * 100.0).round().clamp(1.0, 100.0) as u8;
            let mut output = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, quality)
                .write_image(
                    &rgb,
                    image.width,
                    image.height,
                    image::ExtendedColorType::Rgb8,
                )
                .map_err(|_| ImageError::Png("JPEG encoding failed"))?;
            Ok((output, "image/jpeg"))
        }
        "image/webp" => {
            if image.pixels.len() != rgba_bytes {
                return Err(ImageError::InvalidViewport);
            }
            let mut output = Vec::new();
            image::codecs::webp::WebPEncoder::new_lossless(&mut output)
                .write_image(
                    &image.pixels,
                    image.width,
                    image.height,
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(|_| ImageError::Png("WebP encoding failed"))?;
            Ok((output, "image/webp"))
        }
        _ => Ok((encode_png(image), "image/png")),
    }
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
    decode_raster_image_bounded(bytes, true, MAX_IMAGE_BYTES)
}

fn decode_png_basic(bytes: &[u8]) -> Result<Rgba8Image, ImageError> {
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

/// Decode one still raster image through the shared image crate. PNG keeps the
/// bounded decoder used by the existing HTML image path; JPEG, WebP, GIF and BMP
/// are decoded by their upstream image codecs. Animated formats yield their
/// first frame because this API returns a static raster.
pub fn decode_raster_image(bytes: &[u8]) -> Result<Rgba8Image, ImageError> {
    decode_raster_image_with_orientation(bytes, true)
}

/// Decode a still raster within the caller's remaining decoded-pixel budget.
/// The shared decoder rejects oversized dimensions before allocating pixels;
/// the global image ceiling also applies when a caller supplies a larger budget.
pub fn decode_raster_image_with_limit(
    bytes: &[u8],
    max_bytes: usize,
) -> Result<Rgba8Image, ImageError> {
    decode_raster_image_bounded(bytes, true, max_bytes.min(MAX_IMAGE_BYTES))
}

/// Decode a still image, using the shared XML/cascade/vector renderer for SVG.
/// The output allocation must fit both the caller's budget and the global ceiling.
pub fn decode_image_with_limit(bytes: &[u8], max_bytes: usize) -> Result<Rgba8Image, ImageError> {
    let prefix = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    if prefix
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        == Some(b'<')
    {
        decode_svg_image_with_limit(bytes, max_bytes)
    } else {
        decode_raster_image_with_limit(bytes, max_bytes)
    }
}

/// Rasterize an SVG image without an interactive document or external fetches.
/// Reuses the renderer's capability checks so unsupported drawing is never
/// silently accepted as a successfully decoded image.
pub fn decode_svg_image_with_limit(
    bytes: &[u8],
    max_bytes: usize,
) -> Result<Rgba8Image, ImageError> {
    let source = std::str::from_utf8(bytes).map_err(|_| ImageError::Svg("SVG is not UTF-8"))?;
    let document = lumen_html::xml::parse(source, 16_384).map_err(|error| match error.message {
        "XML input too large"
        | "XML node limit exceeded"
        | "XML nesting limit exceeded"
        | "XML entity output too large" => ImageError::TooLarge,
        _ => ImageError::Xml(error),
    })?;
    let mut current = document
        .first_child(document.root())
        .map_err(|_| ImageError::Svg("invalid SVG tree"))?;
    let root = loop {
        let node = current.ok_or(ImageError::Svg("SVG document has no root element"))?;
        if matches!(
            document.kind(node),
            Ok(lumen_html::NodeKind::Element { .. })
        ) {
            break node;
        }
        current = document
            .next_sibling(node)
            .map_err(|_| ImageError::Svg("invalid SVG tree"))?;
    };
    if !matches!(document.kind(root), Ok(lumen_html::NodeKind::Element { namespace: lumen_html::Namespace::Svg, name, .. }) if lumen_html::svg::local_name(name) == "svg")
    {
        return Err(ImageError::Svg("image root is not an SVG element"));
    }
    let mut session = lumen_html::session::RenderSession::new(document);
    session.set_canvas_background(None);
    let style = session.computed_style(root).map_err(ImageError::Layout)?;
    let attributes = match session.document().kind(root) {
        Ok(lumen_html::NodeKind::Element { attributes, .. }) => attributes,
        _ => return Err(ImageError::Svg("invalid SVG root")),
    };
    let (width, height) =
        lumen_html::svg::root_size(attributes, style.width, style.height, 300.0, Some(150.0));
    let (width, height) = (f64::from(width).ceil(), f64::from(height).ceil());
    if !width.is_finite() || !height.is_finite() || width < 1.0 || height < 1.0 {
        return Err(ImageError::InvalidViewport);
    }
    if width > u32::MAX as f64 || height > u32::MAX as f64 {
        return Err(ImageError::TooLarge);
    }
    let (width, height) = (width as u32, height as u32);
    let pixels = lumen_common::limits::size::repeat(
        width as usize,
        height as usize,
        max_bytes.min(MAX_IMAGE_BYTES) / 4,
    )
    .map_err(|_| ImageError::TooLarge)?;
    lumen_common::limits::size::repeat(pixels, 4, max_bytes.min(MAX_IMAGE_BYTES))
        .map_err(|_| ImageError::TooLarge)?;
    let unsupported = session
        .unsupported_svg_features()
        .map_err(ImageError::Layout)?;
    if !unsupported.is_empty() {
        return Err(ImageError::UnsupportedSvg(unsupported));
    }
    let font = default_font()?;
    let list = session
        .display_list(width, height, font)
        .map_err(ImageError::Layout)?;
    render_with_font(list, width, height, 1.0, true, font)
}

pub fn decode_raster_image_with_orientation(
    bytes: &[u8],
    apply_orientation: bool,
) -> Result<Rgba8Image, ImageError> {
    decode_raster_image_bounded(bytes, apply_orientation, MAX_IMAGE_BYTES)
}

/// Report whether the encoded raster carries an ICC profile. Decoders expose
/// profiles as metadata; they do not transform pixels into sRGB themselves.
pub fn raster_image_has_icc_profile(bytes: &[u8]) -> Result<bool, ImageError> {
    Ok(raster_image_icc_profile(bytes)?.is_some())
}

/// Extract a bounded embedded ICC profile from a supported raster image.
pub fn raster_image_icc_profile(bytes: &[u8]) -> Result<Option<Vec<u8>>, ImageError> {
    let format = image::guess_format(bytes).map_err(|_| ImageError::Png("unknown image format"))?;
    if format == image::ImageFormat::Png {
        return png_icc_profile(bytes);
    }
    if !matches!(
        format,
        image::ImageFormat::Jpeg | image::ImageFormat::WebP | image::ImageFormat::Gif
    ) {
        return Err(ImageError::Png("unsupported raster image format"));
    }
    let reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut decoder = reader
        .into_decoder()
        .map_err(|_| ImageError::Png("raster image decoder unavailable"))?;
    let profile = decoder
        .icc_profile()
        .map_err(|_| ImageError::Png("raster ICC profile read failed"))?;
    if profile
        .as_ref()
        .is_some_and(|profile| profile.len() > 4 * 1024 * 1024)
    {
        return Err(ImageError::TooLarge);
    }
    Ok(profile)
}

/// Convert straight RGBA samples from an embedded profile into sRGB. The
/// shared moxcms engine carries alpha through the transform unchanged.
pub fn raster_image_convert_to_srgb(
    image: &mut Rgba8Image,
    icc_profile: &[u8],
) -> Result<(), ImageError> {
    let source = moxcms::ColorProfile::new_from_slice(icc_profile)
        .map_err(|_| ImageError::Png("invalid embedded ICC profile"))?;
    raster_image_convert_to_srgb_profile(image, &source)
}

/// Transform image samples through a parsed ICC profile. This is exposed so
/// callers with an already parsed profile can reuse the same pixel contract.
pub fn raster_image_convert_to_srgb_profile(
    image: &mut Rgba8Image,
    source: &moxcms::ColorProfile,
) -> Result<(), ImageError> {
    let expected_len = (image.width as usize)
        .checked_mul(image.height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(ImageError::TooLarge)?;
    if image.pixels.len() != expected_len {
        return Err(ImageError::Png("invalid RGBA sample buffer"));
    }
    if source.color_space != moxcms::DataColorSpace::Rgb {
        return Err(ImageError::Png("unsupported non-RGB embedded ICC profile"));
    }
    let destination = moxcms::ColorProfile::new_srgb();
    let transform = source
        .create_transform_8bit(
            moxcms::Layout::Rgba,
            &destination,
            moxcms::Layout::Rgba,
            moxcms::TransformOptions::default(),
        )
        .map_err(|_| ImageError::Png("embedded ICC profile cannot be transformed"))?;
    let mut converted = vec![0; image.pixels.len()];
    transform
        .transform(&image.pixels, &mut converted)
        .map_err(|_| ImageError::Png("embedded ICC pixel conversion failed"))?;
    image.pixels = converted;
    Ok(())
}

fn png_icc_profile(bytes: &[u8]) -> Result<Option<Vec<u8>>, ImageError> {
    const MAX_ICC_PROFILE_BYTES: usize = 4 * 1024 * 1024;
    let mut offset = 8usize;
    while let Some(header) = bytes.get(offset..offset.saturating_add(8)) {
        let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        if &header[4..8] == b"iCCP" {
            let start = offset.checked_add(8).ok_or(ImageError::TooLarge)?;
            let end = start.checked_add(length).ok_or(ImageError::TooLarge)?;
            let payload = bytes
                .get(start..end)
                .ok_or(ImageError::Png("truncated PNG ICC chunk"))?;
            let name_end = payload
                .iter()
                .position(|byte| *byte == 0)
                .ok_or(ImageError::Png("invalid PNG ICC profile name"))?;
            if name_end + 2 > payload.len() || payload[name_end + 1] != 0 {
                return Err(ImageError::Png("invalid PNG ICC compression method"));
            }
            let profile = lumen_common::compress::zlib_decompress_limited(
                &payload[name_end + 2..],
                MAX_ICC_PROFILE_BYTES,
            )
            .map_err(|_| ImageError::Png("invalid or oversized PNG ICC profile"))?;
            return Ok(Some(profile));
        }
        let Some(next) = offset
            .checked_add(12)
            .and_then(|value| value.checked_add(length))
        else {
            return Err(ImageError::TooLarge);
        };
        if next > bytes.len() {
            return Err(ImageError::Png("truncated PNG chunk"));
        }
        if &header[4..8] == b"IEND" {
            return Ok(None);
        }
        offset = next;
    }
    Ok(None)
}

fn decode_raster_image_bounded(
    bytes: &[u8],
    apply_orientation: bool,
    max_bytes: usize,
) -> Result<Rgba8Image, ImageError> {
    let format = image::guess_format(bytes).map_err(|_| ImageError::Png("unknown image format"))?;
    if format == image::ImageFormat::Png {
        let dimensions = bytes
            .get(16..24)
            .ok_or(ImageError::Png("missing PNG dimensions"))?;
        let width = u32::from_be_bytes(dimensions[..4].try_into().unwrap()) as usize;
        let height = u32::from_be_bytes(dimensions[4..].try_into().unwrap()) as usize;
        lumen_common::limits::size::repeat(width, height, max_bytes / 4)
            .map_err(|_| ImageError::TooLarge)?;
        // Retain the streaming common-zlib path for its supported PNG subset.
        // Palette, packed/16-bit samples, transparency and Adam7 use the same
        // bounded maintained codec path as the other raster formats.
        if bytes.get(24..29).is_some_and(|header| {
            header[0] == 8 && matches!(header[1], 2 | 6) && header[2..] == [0, 0, 0]
        }) && !bytes.windows(4).any(|window| window == b"tRNS")
        {
            return decode_png_basic(bytes);
        }
    }
    if !matches!(
        format,
        image::ImageFormat::Png
            | image::ImageFormat::Jpeg
            | image::ImageFormat::WebP
            | image::ImageFormat::Gif
            | image::ImageFormat::Bmp
    ) {
        return Err(ImageError::Png("unsupported raster image format"));
    }
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(32_768);
    limits.max_image_height = Some(32_768);
    limits.max_alloc = Some(max_bytes as u64);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .map_err(|_| ImageError::Png("raster image decoder unavailable"))?;
    let (width, height) = decoder.dimensions();
    lumen_common::limits::size::repeat(width as usize, height as usize, max_bytes / 4)
        .map_err(|_| ImageError::TooLarge)?;
    let orientation = if apply_orientation {
        decoder
            .orientation()
            .unwrap_or(image::metadata::Orientation::NoTransforms)
    } else {
        image::metadata::Orientation::NoTransforms
    };
    let mut decoded = image::DynamicImage::from_decoder(decoder)
        .map_err(|_| ImageError::Png("raster image decode failed"))?;
    decoded.apply_orientation(orientation);
    let (width, height) = (decoded.width(), decoded.height());
    let pixels = decoded.to_rgba8().into_raw();
    if pixels.len() > max_bytes {
        return Err(ImageError::TooLarge);
    }
    Ok(Rgba8Image {
        width,
        height,
        pixels,
    })
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
    render_html_with_font(html, width_css, height_css, scale, default_font()?)
}

fn default_font() -> Result<&'static FontFace, ImageError> {
    static FONT: std::sync::OnceLock<Result<FontFace, &'static str>> = std::sync::OnceLock::new();
    FONT.get_or_init(|| FontFace::new(std::sync::Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)))
        .as_ref()
        .map_err(|&error| ImageError::Font(error))
}

pub fn render_html_with_font(
    html: &str,
    width_css: u32,
    height_css: u32,
    scale: f32,
    font: &dyn FontProvider,
) -> Result<Rgba8Image, ImageError> {
    let document = lumen_html::html::parse(html, 100_000).map_err(ImageError::Html)?;
    render_document(&document, width_css, height_css, scale, font)
}

pub fn render_html_with_images(
    html: &str,
    width_css: u32,
    height_css: u32,
    scale: f32,
    font: &dyn FontProvider,
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
    font: &dyn FontProvider,
) -> Result<Rgba8Image, ImageError> {
    let mut list = lumen_html::layout::display_list(document, width_css, height_css, font)
        .map_err(ImageError::Layout)?;
    if !scale.is_finite() || scale <= 0.0 {
        return Err(ImageError::InvalidViewport);
    }
    snap::boxes(&mut list, scale);
    render_with_font(&list, width_css, height_css, scale, true, font)
}

pub fn render_document_with_images(
    document: &lumen_html::Document,
    width_css: u32,
    height_css: u32,
    scale: f32,
    font: &dyn FontProvider,
    images: &dyn lumen_html::layout::ImageResolver,
) -> Result<Rgba8Image, ImageError> {
    let mut list =
        lumen_html::layout::display_list_with_images(document, width_css, height_css, font, images)
            .map_err(ImageError::Layout)?;
    if !scale.is_finite() || scale <= 0.0 {
        return Err(ImageError::InvalidViewport);
    }
    snap::boxes(&mut list, scale);
    render_with_font(&list, width_css, height_css, scale, true, font)
}

/// Pump host jobs before each pass. The callback returns true while work remains.
/// Both pending images and queued jobs must settle before pixels are produced.
pub fn render_settled(
    session: &mut lumen_html::session::RenderSession,
    width_css: u32,
    height_css: u32,
    scale: f32,
    font: &dyn FontProvider,
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
    font: &dyn FontProvider,
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
    font: &dyn FontProvider,
    images: &dyn lumen_html::layout::ImageResolver,
    pending: bool,
) -> Result<Option<Rgba8Image>, ImageError> {
    match session.display_list_with_images(width_css, height_css, font, images) {
        Err(lumen_html::layout::LayoutError::ImagePending) => Ok(None),
        Err(error) => Err(ImageError::Layout(error)),
        Ok(list) if !pending => render_with_mode_cached(
            list,
            width_css,
            height_css,
            scale,
            RasterizationMode::CssPixelSnapped,
            font,
            &mut GlyphCache::default(),
        )
        .map(Some),
        Ok(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_image_decode_reuses_cascade_viewbox_and_preserves_transparency() {
        let source = br#"<svg xmlns="http://www.w3.org/2000/svg" width="4" viewBox="0 0 2 1"><style><![CDATA[rect { fill: red }]]></style><rect width="1" height="1" fill="green"/><script>throw new Error('inert image script')</script></svg>"#;
        let decoded = decode_image_with_limit(source, 32).unwrap();
        assert_eq!((decoded.width, decoded.height), (4, 2));
        assert_eq!(&decoded.pixels[..4], &[255, 0, 0, 255]);
        assert_eq!(&decoded.pixels[12..16], &[0, 0, 0, 0]);
        let encoded = "data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='1'%20height='1'%3E%3Crect%20width='1'%20height='1'%20fill='green'/%3E%3C/svg%3E";
        let images = FileImages::new(".");
        let ImageState::Ready(first) = lumen_html::layout::ImageResolver::resolve(&images, encoded)
        else {
            panic!("SVG data URL should decode through the shared file resolver");
        };
        assert_eq!(first.pixels, [0, 128, 0, 255]);
        let ImageState::Ready(second) =
            lumen_html::layout::ImageResolver::resolve(&images, encoded)
        else {
            panic!("cached SVG data URL should remain ready");
        };
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(lumen_html::layout::ImageResolver::generation(&images), 1);
        let unsupported = "data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='1'%20height='1'%3E%3Crect%20width='1'%20height='1'%20filter='url(%23missing)'/%3E%3C/svg%3E";
        assert_eq!(
            lumen_html::layout::ImageResolver::resolve(&images, unsupported),
            ImageState::Failed
        );
        assert_eq!(
            images.failure(unsupported),
            Some(ImageFailure::UnsupportedDrawing)
        );
        assert_eq!(lumen_html::layout::ImageResolver::generation(&images), 2);
    }

    #[test]
    fn bmp_decode_preserves_pixel_order_and_rejects_insufficient_budget() {
        let mut encoded = Vec::new();
        image::codecs::bmp::BmpEncoder::new(&mut encoded)
            .encode(
                &[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255],
                2,
                2,
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        assert!(decode_raster_image_with_limit(&encoded, 15).is_err());
        let decoded = decode_image_with_limit(&encoded, 16).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 2));
        assert_eq!(
            decoded.pixels,
            vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255]
        );
        let mut oversized = encoded;
        oversized[18..22].copy_from_slice(&i32::MAX.to_le_bytes());
        assert!(decode_raster_image_with_limit(&oversized, 16).is_err());
    }

    #[test]
    fn svg_image_decode_checks_budget_and_reports_unimplemented_drawing() {
        let source = br#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"><rect width="2" height="2"/></svg>"#;
        assert!(matches!(
            decode_svg_image_with_limit(source, 15),
            Err(ImageError::TooLarge)
        ));
        assert!(decode_svg_image_with_limit(source, 16).is_ok());
        let oversized =
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="4294967295" height="4294967295"/>"#;
        assert!(matches!(
            decode_svg_image_with_limit(oversized, usize::MAX),
            Err(ImageError::TooLarge)
        ));
        let unsupported = br#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"><rect width="2" height="2" filter="url(#missing)"/></svg>"#;
        assert!(matches!(
            decode_svg_image_with_limit(unsupported, 16),
            Err(ImageError::UnsupportedSvg(_))
        ));
        assert!(matches!(
            decode_svg_image_with_limit(b"<svg/>", 64),
            Err(ImageError::Svg(_))
        ));
        assert!(matches!(
            decode_svg_image_with_limit(b"<svg", 64),
            Err(ImageError::Xml(_))
        ));
    }

    #[test]
    fn raster_decode_respects_remaining_pixel_budget_before_allocation() {
        let pixels = vec![17, 34, 51, 255].repeat(4);
        let encoded = encode_png(&Rgba8Image {
            width: 2,
            height: 2,
            pixels: pixels.clone(),
        });
        assert!(matches!(
            decode_raster_image_with_limit(&encoded, 15),
            Err(ImageError::TooLarge)
        ));
        assert_eq!(
            decode_raster_image_with_limit(&encoded, 16).unwrap().pixels,
            pixels
        );

        // Deliberately invalid CRC: the size guard must fire before decompression
        // or validation can allocate storage for the advertised dimensions.
        let mut oversized = encoded;
        oversized[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            decode_raster_image_with_limit(&oversized, usize::MAX),
            Err(ImageError::TooLarge)
        ));
    }

    #[test]
    fn html_inline_svg_maps_viewbox_and_rasterizes_primitive_fill() {
        let image = render_html(
            "<body style='margin:0;background:white'><svg width='4' height='4' viewBox='0 0 2 2'><rect width='1' height='2' fill='red'/></svg></body>",
            4,
            4,
            1.0,
        )
        .unwrap();
        let pixel = |x: usize, y: usize| {
            let offset = (y * image.width as usize + x) * 4;
            &image.pixels[offset..offset + 4]
        };
        assert_eq!(pixel(1, 1), &[255, 0, 0, 255]);
        assert_eq!(pixel(3, 1), &[255, 255, 255, 255]);
    }

    #[test]
    fn svg_linear_gradient_uses_stop_offsets_and_object_bounds() {
        let image = render_html(
            "<body style='margin:0;background:white'><svg width='10' height='2' viewBox='0 0 10 2'><defs><linearGradient id='paint'><stop offset='0%' stop-color='red'/><stop offset='100%' stop-color='blue'/></linearGradient></defs><rect width='10' height='2' fill='url(#paint)'/></svg></body>",
            10,
            2,
            1.0,
        )
        .unwrap();
        let pixel = |x: usize| {
            let offset = x * 4;
            &image.pixels[offset..offset + 4]
        };
        assert!(pixel(0)[0] > pixel(0)[2]);
        assert!(pixel(9)[2] > pixel(9)[0]);
    }

    #[test]
    fn svg_user_space_gradient_applies_gradient_transform() {
        let image = render_html(
            "<body style='margin:0;background:white'><svg width='8' height='2' viewBox='0 0 8 2'><defs><linearGradient id='paint' gradientUnits='userSpaceOnUse' x1='0' y1='0' x2='8' y2='0' gradientTransform='translate(2 0)'><stop offset='0' stop-color='red'/><stop offset='1' stop-color='blue'/></linearGradient></defs><rect width='8' height='2' fill='url(#paint)'/></svg></body>",
            8,
            2,
            1.0,
        )
        .unwrap();
        let pixel = |x: usize| {
            let offset = x * 4;
            &image.pixels[offset..offset + 4]
        };
        assert!(pixel(0)[0] > pixel(0)[2]);
        assert!(pixel(7)[2] > pixel(7)[0]);
    }

    #[test]
    fn invalid_singular_svg_gradient_uses_paint_fallback() {
        let image = render_html(
            "<body style='margin:0;background:white'><svg width='4' height='2' viewBox='0 0 4 2'><defs><linearGradient id='bad' gradientTransform='scale(0)'><stop offset='0' stop-color='red'/><stop offset='1' stop-color='blue'/></linearGradient></defs><rect width='4' height='2' fill='url(#bad) lime'/></svg></body>",
            4,
            2,
            1.0,
        )
        .unwrap();
        assert_eq!(&image.pixels[..4], &[0, 255, 0, 255]);
    }

    #[test]
    fn svg_use_resolves_a_local_group_and_applies_percentage_position() {
        let image = render_html(
            "<body style='margin:0;background:white'><svg width='8' height='4' viewBox='0 0 8 4'><defs><g id='shape' transform='translate(1 0)'><rect width='2' height='2' fill='red'/></g></defs><use href='#shape' x='25%' y='1'/></svg></body>",
            8,
            4,
            1.0,
        )
        .unwrap();
        let pixel = |x: usize, y: usize| {
            let offset = (y * image.width as usize + x) * 4;
            &image.pixels[offset..offset + 4]
        };
        assert_eq!(pixel(3, 1), &[255, 0, 0, 255]);
        assert_eq!(pixel(4, 2), &[255, 0, 0, 255]);
        assert_eq!(pixel(2, 1), &[255, 255, 255, 255]);
        assert_eq!(pixel(5, 1), &[255, 255, 255, 255]);
    }

    #[test]
    fn svg_user_space_clip_path_unions_children_and_respects_even_odd_rule() {
        let image = render_html(
            "<body style='margin:0;background:white'><svg width='8' height='4' viewBox='0 0 8 4'><defs><clipPath id='clip' transform='translate(1 0)'><rect width='1' height='4'/><rect x='6' width='1' height='4'/></clipPath><clipPath id='hole'><path clip-rule='evenodd' d='M0 0H4V4H0Z M1 1V3H3V1Z'/></clipPath></defs><rect width='8' height='4' fill='red' clip-path='url(#clip)'/><rect x='2' width='4' height='4' fill='blue' clip-path='url(#hole)'/></svg></body>",
            8,
            4,
            1.0,
        )
        .unwrap();
        let pixel = |x: usize, y: usize| {
            let offset = (y * image.width as usize + x) * 4;
            &image.pixels[offset..offset + 4]
        };
        assert_eq!(pixel(0, 1), &[255, 255, 255, 255]);
        assert_eq!(pixel(1, 1), &[255, 0, 0, 255]);
        assert_eq!(pixel(2, 1), &[255, 255, 255, 255]);
        assert_eq!(pixel(3, 1), &[0, 0, 255, 255]);
        assert_eq!(pixel(7, 1), &[255, 0, 0, 255]);
        assert_eq!(pixel(2, 0), &[0, 0, 255, 255]);
        assert_eq!(pixel(5, 1), &[255, 255, 255, 255]);
    }

    #[test]
    fn svg_object_bounding_box_clip_scales_to_target_shape() {
        let image = render_html(
            "<body style='margin:0;background:white'><svg width='8' height='4' viewBox='0 0 8 4'><defs><clipPath id='clip' clipPathUnits='objectBoundingBox'><rect width='.5' height='1'/></clipPath></defs><rect x='2' y='0' width='4' height='4' fill='red' clip-path='url(#clip)'/></svg></body>",
            8,
            4,
            1.0,
        )
        .unwrap();
        let pixel = |x: usize, y: usize| {
            let offset = (y * image.width as usize + x) * 4;
            &image.pixels[offset..offset + 4]
        };
        assert_eq!(pixel(2, 1), &[255, 0, 0, 255]);
        assert_eq!(pixel(3, 1), &[255, 0, 0, 255]);
        assert_eq!(pixel(4, 1), &[255, 255, 255, 255]);
        assert_eq!(pixel(5, 1), &[255, 255, 255, 255]);
    }

    #[test]
    fn svg_css_geometry_changes_rect_position_and_size() {
        let image = render_html(
            "<body style='margin:0;background:white'><style>svg rect { x:2px; y:1px; width:2px; height:2px; fill:green }</style><svg width='8' height='4' viewBox='0 0 8 4'><rect width='1' height='1'/></svg></body>",
            8,
            4,
            1.0,
        )
        .unwrap();
        let pixel = |x: usize, y: usize| {
            let offset = (y * image.width as usize + x) * 4;
            &image.pixels[offset..offset + 4]
        };
        assert_eq!(pixel(2, 1), &[0, 128, 0, 255]);
        assert_eq!(pixel(3, 2), &[0, 128, 0, 255]);
        assert_eq!(pixel(1, 1), &[255, 255, 255, 255]);
    }

    #[test]
    fn standalone_prefixed_svg_uses_local_tag_names_for_style_and_geometry() {
        let document = lumen_html::xml::parse(
            "<s:svg xmlns:s='http://www.w3.org/2000/svg' viewBox='0 0 2 2'><s:style>rect { fill: blue }</s:style><s:rect width='1' height='2'/></s:svg>",
            100,
        )
        .unwrap();
        let font = FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap();
        let image = render_document(&document, 4, 4, 1.0, &font).unwrap();
        let pixel = |x: usize, y: usize| {
            let offset = (y * image.width as usize + x) * 4;
            &image.pixels[offset..offset + 4]
        };
        assert_eq!(pixel(1, 1), &[0, 0, 255, 255]);
        assert_eq!(pixel(3, 1), &[255, 255, 255, 255]);
    }

    #[test]
    fn embedded_display_p3_profile_converts_rgb_but_preserves_alpha() {
        let mut image = Rgba8Image {
            width: 2,
            height: 1,
            pixels: vec![255, 0, 0, 0, 180, 70, 30, 96],
        };
        raster_image_convert_to_srgb_profile(&mut image, &moxcms::ColorProfile::new_display_p3())
            .unwrap();
        assert_ne!(&image.pixels[4..7], &[180, 70, 30]);
        assert_eq!(image.pixels[3], 0);
        assert_eq!(image.pixels[7], 96);
    }

    #[test]
    fn styled_font_fallback_retains_face_identity_through_cached_sprites() {
        use lumen_html::paint::FontStyle;
        use lumen_html_text::{FontSet, RegisteredFont, DEFAULT_FONT_BYTES, TEST_FONT_BYTES};
        let mono = Arc::new(FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap());
        let sans = Arc::new(FontFace::new(Arc::from(TEST_FONT_BYTES)).unwrap());
        let fonts = FontSet::new(vec![
            RegisteredFont {
                stretch: 100.0,
                family: "mono".into(),
                weight: 400,
                style: FontStyle::Normal,
                face: mono.clone(),
            },
            RegisteredFont {
                stretch: 100.0,
                family: "sans".into(),
                weight: 400,
                style: FontStyle::Normal,
                face: sans.clone(),
            },
        ])
        .unwrap();
        let document = lumen_html::html::parse("<body style='margin:0;background:white;font-size:16px'><div style='font-family:mono'>office שלום</div><div style='font-family:sans'>office</div></body>", 64).unwrap();
        let mut session = lumen_html::session::RenderSession::new(document);
        let list = session.display_list(200, 80, &fonts).unwrap();
        let faces: Vec<_> = list
            .0
            .iter()
            .flat_map(|command| match command {
                Command::GlyphRun { glyphs, .. } => {
                    glyphs.iter().map(|glyph| glyph.face).collect::<Vec<_>>()
                }
                _ => Vec::new(),
            })
            .collect();
        assert!(faces.contains(&mono.id()) && faces.contains(&sans.id()));
        let mut cache = GlyphCache::default();
        for scale in [1.0, 2.0] {
            let direct =
                render_with_font_cached(list, 200, 80, scale, true, &fonts, &mut cache).unwrap();
            let sprites = rasterize_for_gpui(list, scale, &fonts, &mut cache, true).unwrap();
            assert!(sprites
                .0
                .iter()
                .all(|command| !matches!(command, Command::GlyphRun { .. })));
            let replay =
                render_with_font_cached(&sprites, 200, 80, scale, true, &fonts, &mut cache)
                    .unwrap();
            assert_eq!(direct, replay);
        }
        assert!(cache.glyphs.keys().any(|key| key.0 == mono.id()));
        assert!(cache.glyphs.keys().any(|key| key.0 == sans.id()));
        // Switching the immutable provider invalidates retained shaping even
        // without a DOM mutation, and switching back restores fallback faces.
        let single = session.display_list(200, 80, mono.as_ref()).unwrap();
        assert!(single.0.iter().all(|command| {
            match command {
                Command::GlyphRun { glyphs, .. } => glyphs
                    .iter()
                    .all(|glyph| glyph.face == 0 || glyph.face == mono.id()),
                _ => true,
            }
        }));
        let restored = session.display_list(200, 80, &fonts).unwrap();
        assert!(restored.0.iter().any(|command| match command {
            Command::GlyphRun { glyphs, .. } => glyphs.iter().any(|glyph| glyph.face == sans.id()),
            _ => false,
        }));
    }
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
                corners: None,
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
            assert!(image
                .pixels
                .chunks_exact(4)
                .all(|pixel| pixel == [0, 0, 255, 128]));
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
            &transformed.0,
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
                corners: None,
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
        assert!(image
            .pixels
            .chunks_exact(4)
            .all(|pixel| pixel == [255, 0, 0, 128]));
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
                corners: None,
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
                corners: None,
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
                corners: None,
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
        let html = "<html><head><style>body { margin: 0 } div { background: red; width: 2px; height: 1px }</style></head><body><div></div></body></html>";
        let image = render_html(html, 3, 1, 1.0).unwrap();
        assert_eq!(
            image.pixels,
            [255, 0, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255,]
        );
        let page = render_html(
            "<style>body{margin:0;background:#203040}</style><p>x</p>",
            32,
            32,
            1.0,
        )
        .unwrap();
        assert_eq!(&page.pixels[page.pixels.len() - 4..], &[32, 48, 64, 255]);
    }

    #[test]
    fn background_clip_text_matches_colored_glyph_coverage() {
        for transform in ["none", "translate(4px,3px)", "rotate(5deg)"] {
            let clipped = format!(
                "<style>body{{margin:0;background:white}}</style><div style='font-size:24px;width:120px;height:40px;transform:{transform};transform-origin:0 0;background:red;background-clip:text;color:transparent'>Clip <b>ink</b></div>"
            );
            let reference = format!(
                "<style>body{{margin:0;background:white}}</style><div style='font-size:24px;width:120px;height:40px;transform:{transform};transform-origin:0 0;color:red'>Clip <b>ink</b></div>"
            );
            let clipped = render_html(&clipped, 150, 60, 1.0).unwrap();
            let reference = render_html(&reference, 150, 60, 1.0).unwrap();
            assert_eq!(clipped.pixels, reference.pixels, "{transform}");
        }
    }

    #[test]
    fn ancestor_text_clip_repaints_after_descendant_scroll() {
        let font = FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let mut sessions = Vec::new();
        for paint in [
            "background:red;background-clip:text;color:transparent",
            "color:red",
        ] {
            let fixture = format!(
                "<style>body{{margin:0;background:white}}</style><div style='font-size:20px;{paint}'><div id='scroll' style='width:90px;height:30px;overflow:auto'><div style='height:100px'>Scroll ink<br>second line</div></div></div>"
            );
            let document = lumen_html::html::parse(&fixture, 64).unwrap();
            let node = lumen_html::selector::query_selector(&document, document.root(), "#scroll")
                .unwrap()
                .unwrap();
            let mut session = lumen_html::session::RenderSession::new(document);
            session.display_list(100, 50, &font).unwrap();
            sessions.push((session, node));
        }
        for offset in [1.0, 8.0] {
            let mut images = Vec::new();
            for (session, node) in &mut sessions {
                assert!(session.set_scroll_offset(*node, 0.0, offset).unwrap());
                let list = session.display_list(100, 50, &font).unwrap();
                images.push(render_with_font(list, 100, 50, 1.0, true, &font).unwrap());
            }
            assert_eq!(images[0].pixels, images[1].pixels, "scroll offset {offset}");
        }
    }

    #[test]
    fn background_clip_border_area_follows_double_bands_ignoring_border_alpha() {
        let image=render_html("<style>body{margin:0;background:white}</style><div style='width:20px;height:10px;border:6px double transparent;background:red;background-clip:border-area'></div>",32,22,1.0).unwrap();
        for (y, expected) in [
            (0, [255, 0, 0, 255]),
            (1, [255, 0, 0, 255]),
            (2, [255, 255, 255, 255]),
            (3, [255, 255, 255, 255]),
            (4, [255, 0, 0, 255]),
            (5, [255, 0, 0, 255]),
            (8, [255, 255, 255, 255]),
        ] {
            let at = (y * 32 + 16) * 4;
            assert_eq!(&image.pixels[at..at + 4], &expected, "row {y}");
        }
    }

    #[test]
    fn cross_fade_blends_premultiplied_pixels_and_retains_missing_transparency() {
        for (expression, expected) in [
            ("cross-fade(red 50%, blue 50%)", [128, 0, 128, 255]),
            (
                "cross-fade(red 40%, rgb(0 255 0 / 50%) 20%, transparent 40%)",
                [229, 153, 127, 255],
            ),
            ("cross-fade(red 25%, blue 25%)", [191, 127, 191, 255]),
            ("cross-fade(red 100%, blue 100%)", [128, 0, 128, 255]),
            (
                "cross-fade(red 1%, cross-fade(red 2%, blue))",
                [8, 0, 247, 255],
            ),
        ] {
            let html = format!(
                "<style>body{{margin:0;background:white}}</style><div style='width:4px;height:4px;background-image:{expression}'></div>"
            );
            let image = render_html(&html, 4, 4, 1.0).unwrap();
            assert_eq!(&image.pixels[..4], &expected, "{expression}");
        }
    }

    #[test]
    fn elliptical_background_and_overflow_share_the_used_corner_shape() {
        for content in ["background:red", "overflow:hidden"] {
            let html = format!(
                "<style>body{{margin:0;background:white}}</style><div style='width:40px;height:20px;border-radius:50% / 25%;{content}'><div style='width:40px;height:20px;background:red'></div></div>"
            );
            let image = render_html(&html, 40, 20, 1.0).unwrap();
            // Without overflow, descendants intentionally cover the background.
            if content == "overflow:hidden" {
                assert_eq!(&image.pixels[..4], &[255, 255, 255, 255]);
                let center = (10 * 40 + 20) * 4;
                assert_eq!(&image.pixels[center..center + 4], &[255, 0, 0, 255]);
                let top_center = (2 * 40 + 20) * 4;
                assert_eq!(&image.pixels[top_center..top_center + 4], &[255, 0, 0, 255]);
            }
        }
        let image=render_html("<style>body{margin:0;background:white}</style><div style='width:40px;height:20px;border-radius:50% / 25%;background:red'></div>",40,20,1.0).unwrap();
        assert_eq!(&image.pixels[..4], &[255, 255, 255, 255]);
        let top_center = (2 * 40 + 20) * 4;
        assert_eq!(&image.pixels[top_center..top_center + 4], &[255, 0, 0, 255]);
    }

    #[test]
    fn canvas_background_obeys_root_and_hidden_body() {
        let image = render_html(
            "<style>html{background:red}body{margin:0;background:blue;width:1px;height:1px}</style>",
            2,
            2,
            1.0,
        )
        .unwrap();
        assert_eq!(&image.pixels[..4], &[0, 0, 255, 255]);
        assert_eq!(&image.pixels[12..16], &[255, 0, 0, 255]);
        let hidden = render_html(
            "<style>body{margin:0;display:none;background:red}</style>",
            1,
            1,
            1.0,
        )
        .unwrap();
        assert_eq!(hidden.pixels, [255, 255, 255, 255]);
        let inherited = render_html(
            "<style>body{margin:0}html{color:red}</style><p>x</p>",
            30,
            30,
            1.0,
        )
        .unwrap();
        assert!(inherited
            .pixels
            .chunks_exact(4)
            .any(|p| p[0] == 255 && p[1] < 255));
    }

    #[test]
    fn user_agent_body_margin_offsets_content_by_eight_pixels() {
        let image = render_html(
            "<div style='width:1px;height:1px;background:red'></div>",
            10,
            10,
            1.0,
        )
        .unwrap();
        let pixel = |x: usize, y: usize| &image.pixels[(y * 10 + x) * 4..(y * 10 + x + 1) * 4];
        assert_eq!(pixel(8, 8), &[255, 0, 0, 255]);
        assert_eq!(pixel(7, 8), &[255, 255, 255, 255]);
        assert_eq!(pixel(8, 7), &[255, 255, 255, 255]);
    }

    #[test]
    fn rounded_background_covers_center_and_softens_corners() {
        let image = render_html(
            "<body style='margin:0'><div style='width:4px;height:4px;background:red;border-radius:2px'></div></body>",
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
            corners: None,
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
            "<body style='margin:0'><div style='width:3px;height:3px;border:1px solid red;border-radius:2px'></div></body>",
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
                [10, 20, 30, 255, 40, 50, 60, 255, 20, 30, 40, 255, 50, 60, 70, 255]
            );
        }
    }

    fn sample_png(header: &[u8; 13], extra: &[(&[u8; 4], &[u8])], raw: &[u8]) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        chunk(&mut bytes, b"IHDR", header);
        for (kind, data) in extra {
            chunk(&mut bytes, kind, data);
        }
        chunk(
            &mut bytes,
            b"IDAT",
            &lumen_common::compress::zlib_compress(raw),
        );
        chunk(&mut bytes, b"IEND", &[]);
        bytes
    }

    #[test]
    fn png_palette_and_packed_grayscale_preserve_transparency() {
        let palette = sample_png(
            &[0, 0, 0, 2, 0, 0, 0, 1, 1, 3, 0, 0, 0],
            &[(b"PLTE", &[0, 128, 0, 255, 0, 0]), (b"tRNS", &[255, 0])],
            &[0, 0x40],
        );
        assert_eq!(
            decode_png(&palette).unwrap().pixels,
            [0, 128, 0, 255, 255, 0, 0, 0]
        );
        let grayscale = sample_png(
            &[0, 0, 0, 4, 0, 0, 0, 1, 2, 0, 0, 0, 0],
            &[(b"tRNS", &[0, 2])],
            &[0, 0x1b],
        );
        assert_eq!(
            decode_png(&grayscale).unwrap().pixels,
            [0, 0, 0, 255, 85, 85, 85, 255, 170, 170, 170, 0, 255, 255, 255, 255]
        );
        assert!(decode_png(&palette[..palette.len() / 2]).is_err());
    }

    #[test]
    fn png_sixteen_bit_samples_and_adam7_decode_to_rgba() {
        let sixteen = sample_png(
            &[0, 0, 0, 1, 0, 0, 0, 1, 16, 4, 0, 0, 0],
            &[],
            &[0, 0x80, 0x80, 0xff, 0xff],
        );
        assert_eq!(decode_png(&sixteen).unwrap().pixels, [128, 128, 128, 255]);
        let interlaced = sample_png(
            &[0, 0, 0, 2, 0, 0, 0, 2, 8, 6, 0, 0, 1],
            &[],
            &[
                0, 255, 0, 0, 255, 0, 0, 255, 0, 255, 0, 0, 0, 255, 255, 255, 255, 255, 255,
            ],
        );
        assert_eq!(
            decode_png(&interlaced).unwrap().pixels,
            [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255]
        );
        assert!(matches!(
            decode_raster_image_bounded(&interlaced, true, 15),
            Err(ImageError::TooLarge)
        ));
    }

    #[test]
    fn img_uses_intrinsic_size_and_shared_image_command() {
        let doc = lumen_html::html::parse("<body style='margin:0'><img src='tile.png'></body>", 8)
            .unwrap();
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
        let scaled = lumen_html::html::parse(
            "<body style='margin:0'><img src='tile.png' width='4'></body>",
            8,
        )
        .unwrap();
        let image = render_document_with_images(&scaled, 4, 2, 1.0, &font, &assets).unwrap();
        assert_eq!(&image.pixels[4 * 4..4 * 5], &[255, 0, 0, 255]);
        assert_eq!(&image.pixels[4 * 7..4 * 8], &[0, 0, 255, 255]);
        // Chrome's pixelated downsampling selects the lower texel at an exact tie.
        let downscaled = lumen_html::html::parse(
            "<body style='margin:0'><img src='tile.png' width='1' height='1'></body>",
            8,
        )
        .unwrap();
        let image = render_document_with_images(&downscaled, 1, 1, 1.0, &font, &assets).unwrap();
        assert_eq!(image.pixels, [255, 0, 0, 255]);
    }

    #[test]
    fn background_url_tiles_follow_position_size_and_repeat() {
        let font = FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap();
        let tile = Arc::new(ImageData {
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
        let white = [255, 255, 255, 255];
        // repeat-x tiles the strip across the width; the uncovered row shows
        // the canvas through the transparent background color.
        let doc = lumen_html::html::parse(
            "<body style='margin:0'><div style='width:4px;height:2px;background-image:url(tile.png);background-repeat:repeat-x'></div></body>",
            8,
        )
        .unwrap();
        let image = render_document_with_images(&doc, 4, 2, 1.0, &font, &assets).unwrap();
        assert_eq!(&image.pixels[0..4], &[255, 0, 0, 255]);
        assert_eq!(&image.pixels[4..8], &[0, 0, 255, 255]);
        assert_eq!(&image.pixels[8..12], &[255, 0, 0, 255]);
        assert_eq!(&image.pixels[12..16], &[0, 0, 255, 255]);
        assert_eq!(&image.pixels[16..20], &white);
        assert_eq!(&image.pixels[28..32], &white);
        // right top anchors the single tile at the right edge.
        let doc = lumen_html::html::parse(
            "<body style='margin:0'><div style='width:4px;height:1px;background-image:url(tile.png);background-repeat:no-repeat;background-position:right top'></div></body>",
            8,
        )
        .unwrap();
        let image = render_document_with_images(&doc, 4, 1, 1.0, &font, &assets).unwrap();
        assert_eq!(&image.pixels[0..4], &white);
        assert_eq!(&image.pixels[4..8], &white);
        assert_eq!(&image.pixels[8..12], &[255, 0, 0, 255]);
        assert_eq!(&image.pixels[12..16], &[0, 0, 255, 255]);
        // An explicit percentage size scales the tile to half width, full height.
        let doc = lumen_html::html::parse(
            "<body style='margin:0'><div style='width:4px;height:2px;background-image:url(tile.png);background-repeat:no-repeat;background-size:50% 100%'></div></body>",
            8,
        )
        .unwrap();
        let image = render_document_with_images(&doc, 4, 2, 1.0, &font, &assets).unwrap();
        // The 2x2 tile covers the left half; both rows sample the 1-row strip.
        assert_eq!(&image.pixels[0..4], &[255, 0, 0, 255]);
        assert_eq!(&image.pixels[4..8], &[0, 0, 255, 255]);
        assert_eq!(&image.pixels[8..12], &white);
        assert_eq!(&image.pixels[12..16], &white);
        assert_eq!(&image.pixels[16..20], &[255, 0, 0, 255]);
        assert_eq!(&image.pixels[20..24], &[0, 0, 255, 255]);
        assert_eq!(&image.pixels[24..28], &white);
        assert_eq!(&image.pixels[28..32], &white);
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
        let rendered = render_html_with_images(
            "<body style='margin:0'><img src='tile.png'></body>",
            1,
            1,
            1.0,
            &font,
            &assets,
        )
        .unwrap();
        assert_eq!(rendered.pixels, [9, 8, 7, 255]);
        assert_eq!(
            lumen_html::layout::ImageResolver::resolve(&assets, "../tile.png"),
            ImageState::Failed
        );
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn data_images_percent_decode_before_base64_and_sniff_encoded_format() {
        use lumen_html::layout::ImageResolver;
        let png = encode_png(&Rgba8Image {
            width: 1,
            height: 1,
            pixels: vec![12, 34, 56, 255],
        });
        let base64 = lumen_common::codec::base64_encode(&png, false, true).replace('=', "%3D");
        let assets = FileImages::new(".");
        for source in [
            format!("data:;base64,{base64}"),
            format!("DATA:text/plain;BASE64,{base64}"),
        ] {
            let ImageState::Ready(image) = assets.resolve(&source) else {
                panic!("data image failed");
            };
            assert_eq!(image.pixels, [12, 34, 56, 255]);
        }
    }

    #[test]
    fn file_images_use_codec_dimensions_and_confine_url_paths() {
        use lumen_html::layout::ImageResolver;
        let directory =
            std::env::temp_dir().join(format!("lumen-html-image-codecs-{}", std::process::id()));
        let base = directory.join("pages");
        let resources = directory.join("resources");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&resources).unwrap();
        let pixels = image::RgbaImage::from_pixel(2, 3, image::Rgba([20, 40, 60, 255]));
        for (extension, format) in [
            ("gif", image::ImageFormat::Gif),
            ("webp", image::ImageFormat::WebP),
        ] {
            let mut encoded = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(pixels.clone())
                .write_to(&mut encoded, format)
                .unwrap();
            std::fs::write(
                resources.join(format!("tile.{extension}")),
                encoded.get_ref(),
            )
            .unwrap();
        }
        let mut encoded = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut encoded)
            .encode(
                &[20, 40, 60].repeat(6),
                2,
                3,
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        std::fs::write(resources.join("tile.jpg"), &encoded).unwrap();
        assert!(decode_raster_image_bounded(&encoded, true, 23).is_err());
        let assets = FileImages::with_root(&base, &directory).unwrap();
        for source in [
            "../resources/tile.jpg",
            "/resources/tile.gif?v=1#crop",
            "../resources/tile%2Ewebp",
        ] {
            let ImageState::Ready(image) = assets.resolve(source) else {
                panic!("failed {source}");
            };
            assert_eq!((image.width, image.height, image.pixels.len()), (2, 3, 24));
        }
        assert_eq!(assets.resolve("../../outside.png"), ImageState::Failed);
        #[cfg(unix)]
        {
            let outside = directory.with_extension("outside.png");
            std::fs::write(
                &outside,
                encode_png(&Rgba8Image {
                    width: 1,
                    height: 1,
                    pixels: vec![0; 4],
                }),
            )
            .unwrap();
            let link = resources.join("escape.png");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert_eq!(assets.resolve("/resources/escape.png"), ImageState::Failed);
            std::fs::remove_file(link).unwrap();
            std::fs::remove_file(outside).unwrap();
        }
        for name in ["tile.jpg", "tile.gif", "tile.webp"] {
            std::fs::remove_file(resources.join(name)).unwrap();
        }
        std::fs::remove_dir(resources).unwrap();
        std::fs::remove_dir(base).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn pending_images_settle_after_pump_and_fail_on_limit() {
        let document = lumen_html::html::parse(
            "<body style='margin:0'><img src='later.png' width='1' height='1'></body>",
            8,
        )
        .unwrap();
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
