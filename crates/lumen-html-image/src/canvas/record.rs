//! Deferred recording for `CanvasSurface`.
//!
//! A recorded surface appends draw operations, each with a frozen copy of the drawing state, to a
//! display list and replays it through the immediate-mode raster functions only when pixels are
//! needed. Operations that provably overwrite the whole canvas drop the list instead, and a fresh
//! canvas whose first operation copies an image 1:1 just references that image.
use super::*;
use lumen_html::layout::LazyImageHandle;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;

pub const DEFAULT_MAX_RECORDED_OP_BYTES: usize = 2 * 1024 * 1024;
pub const DEFAULT_MAX_PINNED_IMAGE_BYTES: usize = 32 * 1024 * 1024;

static RECORDING_ENABLED: AtomicBool = AtomicBool::new(true);
static MAX_OP_BYTES: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_RECORDED_OP_BYTES);
static MAX_PINNED_BYTES: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_PINNED_IMAGE_BYTES);

/// Whether surfaces created by `CanvasSurface::new_recorded` defer their drawing. Existing
/// surfaces keep the mode they were created with.
pub fn set_canvas_recording_enabled(enabled: bool) {
    RECORDING_ENABLED.store(enabled, Relaxed);
}

/// Process-wide defaults: a recorded canvas rasterizes once its recorded operations exceed
/// `op_bytes`, or the distinct images they reference exceed `pinned_image_bytes` decoded.
pub fn set_canvas_recording_limits(op_bytes: usize, pinned_image_bytes: usize) {
    MAX_OP_BYTES.store(op_bytes, Relaxed);
    MAX_PINNED_BYTES.store(pinned_image_bytes, Relaxed);
}

macro_rules! counters {
    ($($field:ident),* $(,)?) => {
        #[derive(Default)]
        struct Counters {$($field: AtomicU64),*}
        static COUNTERS: Counters = Counters {$($field: AtomicU64::new(0)),*};
        /// Process-wide totals since start (or the last `reset_canvas_stats`).
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub struct CanvasStats {$(pub $field: u64),*}
        pub fn canvas_stats() -> CanvasStats {
            CanvasStats {$($field: COUNTERS.$field.load(Relaxed)),*}
        }
        pub fn reset_canvas_stats() {
            $(COUNTERS.$field.store(0, Relaxed);)*
        }
    };
}

counters!(
    recorded_ops,
    replayed_ops,
    canvases_rasterized,
    overdraw_drops,
    overdraw_dropped_ops,
    reset_drops,
    limit_flushes,
    cow_aliases,
    cow_materialized,
    cow_reads,
    pixmaps_allocated,
    snapshots_recycled,
);

fn count(counter: &AtomicU64, by: u64) {
    counter.fetch_add(by, Relaxed);
}

pub(super) fn count_materialized_alias() {
    count(&COUNTERS.cow_materialized, 1);
}

pub(super) fn count_alias_read() {
    count(&COUNTERS.cow_reads, 1);
}

pub(super) fn count_pixmap() {
    count(&COUNTERS.pixmaps_allocated, 1);
}

/// The pixels a draw reads. Immutable and cheap to hold: an `<img>` keeps its lazily decoded
/// resource undecoded until the draw is replayed.
#[derive(Clone)]
pub enum CanvasSource {
    Pixels(Arc<Rgba8Image>),
    Premultiplied(Arc<PremultipliedImage>),
    Lazy(LazyImageHandle),
    Transparent { width: u32, height: u32 },
}

/// A canvas snapshot kept as the premultiplied pixels the canvas stores, so drawing it onto
/// another canvas needs no conversion; the straight-alpha image is made once, on first request.
pub struct PremultipliedImage {
    pixmap: Option<Pixmap>,
    straight: std::sync::OnceLock<Arc<Rgba8Image>>,
}

const SNAPSHOT_POOL_BUFFERS: usize = 4;
const SNAPSHOT_POOL_BYTES: usize = 16 * 1024 * 1024;

thread_local! {
    static SNAPSHOT_POOL: std::cell::RefCell<Vec<Vec<u8>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A copy of `source` that reuses the buffer of a dropped snapshot of the same size when there is
/// one. A buffer enters the pool only when the last reference to its snapshot is dropped, so a
/// live reader never sees it rewritten.
pub fn pooled_copy(source: &Pixmap) -> Pixmap {
    let len = source.data().len();
    let recycled = SNAPSHOT_POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        let at = pool.iter().position(|buffer| buffer.len() == len)?;
        Some(pool.swap_remove(at))
    });
    if let Some(mut buffer) = recycled {
        buffer.copy_from_slice(source.data());
        if let Some(size) = tiny_skia::IntSize::from_wh(source.width(), source.height()) {
            if let Some(pixmap) = Pixmap::from_vec(buffer, size) {
                count(&COUNTERS.snapshots_recycled, 1);
                return pixmap;
            }
        }
    }
    source.clone()
}

fn recycle_buffer(buffer: Vec<u8>) {
    SNAPSHOT_POOL.with(|pool| {
        let Ok(mut pool) = pool.try_borrow_mut() else { return };
        let held: usize = pool.iter().map(Vec::len).sum();
        if pool.len() < SNAPSHOT_POOL_BUFFERS && held + buffer.len() <= SNAPSHOT_POOL_BYTES {
            pool.push(buffer);
        }
    });
}

#[cfg(test)]
pub(super) fn pooled_snapshot_buffers() -> usize {
    SNAPSHOT_POOL.with(|pool| pool.borrow().len())
}

impl Drop for PremultipliedImage {
    fn drop(&mut self) {
        if let Some(pixmap) = self.pixmap.take() {
            recycle_buffer(pixmap.take());
        }
    }
}

impl PremultipliedImage {
    pub fn new(pixmap: Pixmap) -> Self {
        Self { pixmap: Some(pixmap), straight: std::sync::OnceLock::new() }
    }

    pub fn pixmap(&self) -> &Pixmap {
        self.pixmap.as_ref().expect("pixmap lives until drop")
    }

    pub fn straight(&self) -> Arc<Rgba8Image> {
        self.straight
            .get_or_init(|| {
                let pixmap = self.pixmap();
                let mut pixels = Vec::with_capacity(pixmap.data().len());
                for pixel in pixmap.pixels() {
                    let c = pixel.demultiply();
                    pixels.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
                }
                Arc::new(Rgba8Image { width: pixmap.width(), height: pixmap.height(), pixels })
            })
            .clone()
    }
}

impl CanvasSource {
    pub fn width(&self) -> u32 {
        match self {
            Self::Pixels(image) => image.width,
            Self::Premultiplied(image) => image.pixmap().width(),
            Self::Lazy(lazy) => lazy.width(),
            Self::Transparent { width, .. } => *width,
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            Self::Pixels(image) => image.height,
            Self::Premultiplied(image) => image.pixmap().height(),
            Self::Lazy(lazy) => lazy.height(),
            Self::Transparent { height, .. } => *height,
        }
    }

    /// Decodes a lazy source; `None` when its body is corrupt.
    pub fn pixels(&self) -> Option<Arc<Rgba8Image>> {
        match self {
            Self::Pixels(image) => Some(image.clone()),
            Self::Premultiplied(image) => Some(image.straight()),
            Self::Lazy(lazy) => lazy.pixels(),
            Self::Transparent { width, height } => {
                let bytes = pixel_bytes(*width, *height).ok()?;
                Some(Arc::new(Rgba8Image { width: *width, height: *height, pixels: vec![0; bytes] }))
            }
        }
    }

    /// `Some(true)` only when every pixel is known to be opaque without decoding a lazy image.
    pub fn is_opaque(&self) -> Option<bool> {
        match self {
            Self::Pixels(image) => Some(image.pixels.chunks_exact(4).all(|pixel| pixel[3] == 255)),
            Self::Premultiplied(image) => Some(image.pixmap().pixels().iter().all(|pixel| pixel.alpha() == 255)),
            Self::Lazy(lazy) => lazy.is_opaque(),
            Self::Transparent { .. } => Some(false),
        }
    }

    fn identity(&self) -> usize {
        match self {
            Self::Pixels(image) => Arc::as_ptr(image) as usize,
            Self::Premultiplied(image) => Arc::as_ptr(image) as usize,
            Self::Lazy(lazy) => lazy.id(),
            Self::Transparent { .. } => 0,
        }
    }

    fn decoded_bytes(&self) -> usize {
        match self {
            Self::Transparent { .. } => 0,
            _ => (self.width() as usize).saturating_mul(self.height() as usize).saturating_mul(4),
        }
    }

    fn is_known_corrupt(&self) -> bool {
        matches!(self, Self::Lazy(lazy) if lazy.is_known_corrupt())
    }
}

/// Where a source lands, as the 3-, 5- and 9-argument forms of `drawImage` describe it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SourceGeometry {
    Scaled { x: f32, y: f32, width: f32, height: f32 },
    Crop { sx: f32, sy: f32, sw: f32, sh: f32, dx: f32, dy: f32, dw: f32, dh: f32 },
}

/// The fonts a text run draws with. A recorded run keeps its font set, so it draws later exactly
/// as it would have drawn immediately.
#[derive(Clone)]
pub enum TextSource {
    Static(&'static dyn FontProvider),
    Shared(Rc<dyn FontProvider>),
}

impl TextSource {
    pub fn provider(&self) -> &dyn FontProvider {
        match self {
            Self::Static(provider) => *provider,
            Self::Shared(provider) => provider.as_ref(),
        }
    }
}

pub(super) struct TextOp {
    font: TextSource,
    text: String,
    size: f32,
    spec: FontSpec,
    rtl: bool,
    x: f32,
    scale_anchor_x: f32,
    baseline_y: f32,
    max_width: Option<f32>,
    options: CanvasTextOptions,
    stroke: bool,
}

pub(super) struct PixelWrite {
    image: Arc<Rgba8Image>,
    destination_x: i64,
    destination_y: i64,
    source_x: u32,
    source_y: u32,
    width: u32,
    height: u32,
}

pub(super) enum OpKind {
    FillRect(Rect),
    ClearRect(Rect),
    FillPath(Path, FillRule),
    StrokePath(Path),
    Image { source: CanvasSource, geometry: SourceGeometry },
    Text(Box<TextOp>),
    Pixels(PixelWrite),
}

pub(super) struct Recorded {
    state: Rc<DrawingState>,
    kind: OpKind,
}

#[derive(Default)]
pub(super) struct Recorder {
    ops: Vec<Recorded>,
    op_bytes: usize,
    image_bytes: usize,
    pinned: HashSet<usize>,
    limits: Option<(usize, usize)>,
}

impl Recorder {
    fn clear(&mut self) -> usize {
        let dropped = self.ops.len();
        self.ops = Vec::new();
        self.op_bytes = 0;
        self.image_bytes = 0;
        self.pinned.clear();
        dropped
    }

    fn exceeded(&self) -> bool {
        let (op_limit, image_limit) =
            self.limits.unwrap_or_else(|| (MAX_OP_BYTES.load(Relaxed), MAX_PINNED_BYTES.load(Relaxed)));
        self.op_bytes > op_limit || self.image_bytes > image_limit
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Discard {
    Overdraw,
    Reset,
    Cleared,
}

pub(super) fn path_bytes(path: &Path) -> usize {
    path.points().len() * std::mem::size_of::<Point>() + path.verbs().len()
}

impl CanvasGradient {
    fn detached(&self) -> Self {
        Self { kind: self.kind, stops: Rc::new(RefCell::new(self.stops.borrow().clone())) }
    }
}

impl CanvasPattern {
    fn detached(&self) -> Self {
        Self {
            pixmap: self.pixmap.clone(),
            transform: Rc::new(RefCell::new(self.transform())),
            repetition: self.repetition,
            source_transform: self.source_transform,
        }
    }
}

impl DrawingState {
    fn has_shared_paint(&self) -> bool {
        self.fill_gradient.is_some()
            || self.stroke_gradient.is_some()
            || self.fill_pattern.is_some()
            || self.stroke_pattern.is_some()
    }

    /// A copy that later `addColorStop` or `setTransform` calls on the live gradient or pattern
    /// objects cannot reach.
    fn detached(&self) -> Self {
        let mut state = self.clone();
        state.fill_gradient = state.fill_gradient.as_deref().map(|gradient| Box::new(gradient.detached()));
        state.stroke_gradient = state.stroke_gradient.as_deref().map(|gradient| Box::new(gradient.detached()));
        state.fill_pattern = state.fill_pattern.as_deref().map(|pattern| Box::new(pattern.detached()));
        state.stroke_pattern = state.stroke_pattern.as_deref().map(|pattern| Box::new(pattern.detached()));
        state
    }

    fn shadow_possible(&self) -> bool {
        self.shadow_color[3] != 0
    }

    fn is_plain_source_over(&self) -> bool {
        self.alpha == 1.0 && self.blend == BlendMode::SourceOver && !self.shadow_possible()
    }
}

impl CanvasSurface {
    /// A surface that records its drawing and rasterizes on demand (see `set_canvas_recording_enabled`).
    pub fn new_recorded(width: u32, height: u32) -> Result<Self, ImageError> {
        let mut surface = Self::new(width, height)?;
        if RECORDING_ENABLED.load(Relaxed) {
            surface.recorder = Some(Recorder::default());
        }
        Ok(surface)
    }

    /// Overrides the process-wide recording limits for this surface.
    pub fn set_recording_limits(&mut self, op_bytes: usize, pinned_image_bytes: usize) {
        if let Some(recorder) = &mut self.recorder {
            recorder.limits = Some((op_bytes, pinned_image_bytes));
        }
    }

    /// Whether the canvas has an alpha channel. A canvas without one can only reference an image
    /// that is opaque.
    pub fn set_has_alpha(&mut self, has_alpha: bool) {
        self.has_alpha = has_alpha;
    }

    pub fn pending_operations(&self) -> usize {
        self.recorder.as_ref().map_or(0, |recorder| recorder.ops.len())
    }

    pub fn is_recorded(&self) -> bool {
        self.recorder.is_some()
    }

    /// Whether the canvas currently shows an image it references rather than pixels of its own.
    pub fn is_image_alias(&self) -> bool {
        self.alias.is_some() && self.bitmap.is_none()
    }

    pub fn has_pixmap(&self) -> bool {
        self.bitmap.is_some()
    }

    pub(super) fn recording(&self) -> bool {
        self.recorder.is_some()
            && !self.replaying
            && self.layers.is_empty()
            && self.width != 0
            && self.height != 0
    }

    /// Transparent with nothing recorded: every pixel is transparent black.
    pub(super) fn is_blank(&self) -> bool {
        self.lazy && self.alias.is_none() && self.pending_operations() == 0
    }

    /// Rasterizes the recorded operations into the pixmap.
    pub fn flush(&mut self) {
        let Some(recorder) = self.recorder.as_mut() else {
            return;
        };
        if recorder.ops.is_empty() {
            return;
        }
        let ops = std::mem::take(&mut recorder.ops);
        recorder.clear();
        count(&COUNTERS.canvases_rasterized, 1);
        count(&COUNTERS.replayed_ops, ops.len() as u64);
        let live = std::mem::take(&mut self.state);
        let generation = self.generation;
        self.replaying = true;
        for op in ops {
            self.state = (*op.state).clone();
            let _ = self.replay(op.kind);
        }
        self.replaying = false;
        self.generation = generation;
        self.state = live;
    }

    fn replay(&mut self, kind: OpKind) -> Result<(), ImageError> {
        match kind {
            OpKind::FillRect(rect) => self.rect(rect.x(), rect.y(), rect.width(), rect.height(), false),
            OpKind::ClearRect(rect) => self.rect(rect.x(), rect.y(), rect.width(), rect.height(), true),
            OpKind::FillPath(path, rule) => self.fill_path_now(&path, rule),
            OpKind::StrokePath(path) => self.stroke_path_now(&path),
            OpKind::Image { source, geometry } => {
                if let CanvasSource::Premultiplied(image) = &source {
                    return self.draw_pixmap_geometry(image.pixmap(), geometry);
                }
                let Some(image) = source.pixels() else {
                    return Ok(());
                };
                match geometry {
                    SourceGeometry::Scaled { x, y, width, height } => {
                        self.draw_image_legacy_impl(&image, x, y, width, height)
                    }
                    SourceGeometry::Crop { sx, sy, sw, sh, dx, dy, dw, dh } => {
                        self.draw_image_crop_now(&image, [sx, sy, sw, sh, dx, dy, dw, dh])
                    }
                }
            }
            OpKind::Text(op) => {
                let font = op.font.provider();
                let result = if op.stroke {
                    self.stroke_text(font, &op.text, op.size, &op.spec, op.rtl, op.x, op.scale_anchor_x, op.baseline_y, op.max_width, &op.options)
                } else {
                    self.fill_text(font, &op.text, op.size, &op.spec, op.rtl, op.x, op.scale_anchor_x, op.baseline_y, op.max_width, &op.options)
                };
                result.map(|_| ())
            }
            OpKind::Pixels(write) => self.write_pixels_region_now(
                &write.image,
                write.destination_x,
                write.destination_y,
                write.source_x,
                write.source_y,
                write.width,
                write.height,
            ),
        }
    }

    /// Drops everything recorded and, when `why` is an overdraw, the pixels beneath it too: the
    /// operation that follows covers them.
    pub(super) fn discard_recording(&mut self, why: Discard) {
        let had_pixels = self.bitmap.is_some() || self.alias.is_some();
        let dropped = self.recorder.as_mut().map_or(0, Recorder::clear);
        match why {
            Discard::Overdraw => {
                if dropped != 0 || had_pixels {
                    count(&COUNTERS.overdraw_drops, 1);
                    count(&COUNTERS.overdraw_dropped_ops, dropped as u64);
                }
                if self.layers.is_empty() {
                    self.bitmap = None;
                    self.alias = None;
                    self.lazy = self.width != 0 && self.height != 0;
                }
            }
            Discard::Reset => {
                if dropped != 0 {
                    count(&COUNTERS.reset_drops, 1);
                }
                self.alias = None;
            }
            Discard::Cleared => {}
        }
    }

    pub(super) fn push(&mut self, kind: OpKind, extra_bytes: usize) {
        let state = self.frozen_state();
        let Some(recorder) = self.recorder.as_mut() else {
            return;
        };
        if let OpKind::Image { source, .. } = &kind {
            if recorder.pinned.insert(source.identity()) {
                recorder.image_bytes = recorder.image_bytes.saturating_add(source.decoded_bytes());
            }
        }
        recorder.op_bytes = recorder.op_bytes.saturating_add(std::mem::size_of::<Recorded>() + extra_bytes);
        recorder.ops.push(Recorded { state, kind });
        let exceeded = recorder.exceeded();
        count(&COUNTERS.recorded_ops, 1);
        self.changed();
        if exceeded {
            count(&COUNTERS.limit_flushes, 1);
            self.flush();
        }
    }

    fn frozen_state(&mut self) -> Rc<DrawingState> {
        if let Some(frozen) = &self.frozen {
            return frozen.clone();
        }
        let frozen = Rc::new(self.state.detached());
        if !self.state.has_shared_paint() {
            self.frozen = Some(frozen.clone());
        }
        frozen
    }

    fn clip_is_open(&self) -> bool {
        self.state.clip.as_deref().is_none_or(|clip| clip.data().iter().all(|coverage| *coverage == u8::MAX))
    }

    fn rect_covers_canvas(&self, rect: Rect) -> bool {
        let transform = self.state.transform;
        if transform.has_skew() {
            return false;
        }
        rect.transform(transform).is_some_and(|bounds| {
            bounds.left() <= 0.0
                && bounds.top() <= 0.0
                && bounds.right() >= self.width as f32
                && bounds.bottom() >= self.height as f32
        })
    }

    pub(super) fn clear_covers_canvas(&self, rect: Rect) -> bool {
        self.rect_covers_canvas(rect) && self.clip_is_open()
    }

    pub(super) fn opaque_fill_covers_canvas(&self, rect: Rect) -> bool {
        let state = &self.state;
        state.is_plain_source_over()
            && state.fill[3] == u8::MAX
            && state.fill_gradient.is_none()
            && state.fill_pattern.is_none()
            && self.rect_covers_canvas(rect)
            && self.clip_is_open()
    }

    /// Draws an image source: recorded when the surface records, immediately otherwise.
    pub fn draw_source(
        &mut self,
        source: &CanvasSource,
        geometry: SourceGeometry,
        premultiply_alpha: bool,
    ) -> Result<(), ImageError> {
        if !premultiply_alpha || !self.recording() {
            if let (true, CanvasSource::Premultiplied(image)) = (premultiply_alpha, source) {
                self.flush();
                return self.draw_pixmap_geometry(image.pixmap(), geometry);
            }
            let image = source.pixels().ok_or(ImageError::InvalidViewport)?;
            return match geometry {
                SourceGeometry::Scaled { x, y, width, height } => {
                    self.draw_image_with_alpha_behavior(&image, x, y, width, height, premultiply_alpha)
                }
                SourceGeometry::Crop { sx, sy, sw, sh, dx, dy, dw, dh } => {
                    self.draw_image_crop_with_alpha_behavior(&image, sx, sy, sw, sh, dx, dy, dw, dh, premultiply_alpha)
                }
            };
        }
        if source.width() == 0 || source.height() == 0 || source.is_known_corrupt() {
            return Ok(());
        }
        let (destination, whole_source) = match geometry {
            SourceGeometry::Scaled { x, y, width, height } => {
                if ![x, y, width, height].iter().all(|value| value.is_finite()) || width == 0.0 || height == 0.0 {
                    return Ok(());
                }
                (normalized(x, y, width, height), true)
            }
            SourceGeometry::Crop { sx, sy, sw, sh, dx, dy, dw, dh } => {
                let Some(crop) = crop_geometry(source.width(), source.height(), [sx, sy, sw, sh, dx, dy, dw, dh]) else {
                    return Ok(());
                };
                let whole = crop.x0 == 0 && crop.y0 == 0 && crop.width == source.width() && crop.height == source.height();
                (normalized(crop.dx, crop.dy, crop.dw, crop.dh), whole)
            }
        };
        if matches!(source, CanvasSource::Transparent { .. }) && self.state.blend == BlendMode::SourceOver {
            self.changed();
            return Ok(());
        }
        if self.image_covers_canvas(source, destination) {
            self.discard_recording(Discard::Overdraw);
        }
        if whole_source && self.try_alias(source, destination) {
            return Ok(());
        }
        self.push(OpKind::Image { source: source.clone(), geometry }, 0);
        Ok(())
    }

    fn image_covers_canvas(&self, source: &CanvasSource, destination: (f32, f32, f32, f32)) -> bool {
        let (x, y, width, height) = destination;
        self.state.is_plain_source_over()
            && Rect::from_xywh(x, y, width, height).is_some_and(|rect| self.rect_covers_canvas(rect))
            && self.clip_is_open()
            && source.is_opaque() == Some(true)
    }

    /// A blank canvas whose first operation copies an image onto it one to one just shows that image;
    /// pixels of its own are made only when something draws over it.
    fn try_alias(&mut self, source: &CanvasSource, destination: (f32, f32, f32, f32)) -> bool {
        if !self.is_blank()
            || source.width() != self.width
            || source.height() != self.height
            || destination != (0.0, 0.0, self.width as f32, self.height as f32)
            || !self.state.transform.is_identity()
            || !self.state.is_plain_source_over()
            || !self.clip_is_open()
            || matches!(source, CanvasSource::Transparent { .. })
            || !(self.has_alpha || source.is_opaque() == Some(true))
        {
            return false;
        }
        self.alias = Some(source.clone());
        count(&COUNTERS.cow_aliases, 1);
        self.changed();
        true
    }

    /// The canvas contents as an image source for another canvas, pattern or bitmap. Rasterizes
    /// pending operations; an image alias is returned as the image itself, and an unchanged
    /// canvas hands out the same snapshot to every consumer.
    pub fn source(&mut self) -> CanvasSource {
        self.flush();
        if self.layers.is_empty() && self.lazy {
            return match &self.alias {
                Some(alias) => alias.clone(),
                None => CanvasSource::Transparent { width: self.width, height: self.height },
            };
        }
        if let Some((generation, cached)) = &self.source_cache {
            if *generation == self.generation {
                if let Some(image) = cached.upgrade() {
                    return CanvasSource::Premultiplied(image);
                }
            }
        }
        let published = self.layers.first().map_or(&self.bitmap, |layer| &layer.bitmap);
        let image = match published {
            Some(bitmap) => Arc::new(PremultipliedImage::new(pooled_copy(bitmap))),
            None => {
                let snapshot = Arc::new(self.snapshot());
                return CanvasSource::Pixels(snapshot);
            }
        };
        self.source_cache = Some((self.generation, Arc::downgrade(&image)));
        CanvasSource::Premultiplied(image)
    }

    /// `putImageData`: a direct pixel copy that ignores the drawing state.
    pub fn put_pixels(
        &mut self,
        image: Arc<Rgba8Image>,
        destination_x: i64,
        destination_y: i64,
        source_x: u32,
        source_y: u32,
        width: u32,
        height: u32,
    ) -> Result<(), ImageError> {
        if !self.recording() {
            return self.write_pixels_region(&image, destination_x, destination_y, source_x, source_y, width, height);
        }
        if image.pixels.len() != pixel_bytes(image.width, image.height)? {
            return Err(ImageError::InvalidViewport);
        }
        let right = source_x.checked_add(width).ok_or(ImageError::InvalidViewport)?;
        let bottom = source_y.checked_add(height).ok_or(ImageError::InvalidViewport)?;
        if right > image.width || bottom > image.height {
            return Err(ImageError::InvalidViewport);
        }
        if width == 0 || height == 0 {
            return Ok(());
        }
        let covers = destination_x <= 0
            && destination_y <= 0
            && destination_x.saturating_add(i64::from(width)) >= i64::from(self.width)
            && destination_y.saturating_add(i64::from(height)) >= i64::from(self.height);
        if covers {
            self.discard_recording(Discard::Overdraw);
        }
        let bytes = image.pixels.len();
        self.push(
            OpKind::Pixels(PixelWrite { image, destination_x, destination_y, source_x, source_y, width, height }),
            bytes,
        );
        Ok(())
    }

    pub fn fill_text_with(
        &mut self,
        font: &TextSource,
        text: &str,
        size: f32,
        spec: &FontSpec,
        rtl: bool,
        x: f32,
        scale_anchor_x: f32,
        baseline_y: f32,
        max_width: Option<f32>,
        options: &CanvasTextOptions,
    ) -> Result<(), ImageError> {
        self.text_with(false, font, text, size, spec, rtl, x, scale_anchor_x, baseline_y, max_width, options)
    }

    pub fn stroke_text_with(
        &mut self,
        font: &TextSource,
        text: &str,
        size: f32,
        spec: &FontSpec,
        rtl: bool,
        x: f32,
        scale_anchor_x: f32,
        baseline_y: f32,
        max_width: Option<f32>,
        options: &CanvasTextOptions,
    ) -> Result<(), ImageError> {
        self.text_with(true, font, text, size, spec, rtl, x, scale_anchor_x, baseline_y, max_width, options)
    }

    fn text_with(
        &mut self,
        stroke: bool,
        font: &TextSource,
        text: &str,
        size: f32,
        spec: &FontSpec,
        rtl: bool,
        x: f32,
        scale_anchor_x: f32,
        baseline_y: f32,
        max_width: Option<f32>,
        options: &CanvasTextOptions,
    ) -> Result<(), ImageError> {
        if !self.recording() {
            let provider = font.provider();
            let drawn = if stroke {
                self.stroke_text(provider, text, size, spec, rtl, x, scale_anchor_x, baseline_y, max_width, options)
            } else {
                self.fill_text(provider, text, size, spec, rtl, x, scale_anchor_x, baseline_y, max_width, options)
            };
            return drawn.map(|_| ());
        }
        if !x.is_finite() || !baseline_y.is_finite() || !size.is_finite() || size <= 0.0 {
            return Ok(());
        }
        let op = TextOp {
            font: font.clone(),
            text: text.to_owned(),
            size,
            spec: spec.clone(),
            rtl,
            x,
            scale_anchor_x,
            baseline_y,
            max_width,
            options: *options,
            stroke,
        };
        self.push(OpKind::Text(Box::new(op)), std::mem::size_of::<TextOp>() + text.len());
        Ok(())
    }
}

fn normalized(x: f32, y: f32, width: f32, height: f32) -> (f32, f32, f32, f32) {
    let (x, width) = if width < 0.0 { (x + width, -width) } else { (x, width) };
    let (y, height) = if height < 0.0 { (y + height, -height) } else { (y, height) };
    (x, y, width, height)
}
