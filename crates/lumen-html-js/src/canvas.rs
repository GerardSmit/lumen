//! Canvas DOM bindings backed by lumen-html-image's shared raster surface.
use super::*;
use lumen::embed::{Deferred, JsFunction, TaKind};
use lumen_html::paint::{FontSpec, FontStyle, Rgba, TextShaper};
use lumen_html_image::{
    canvas::{
        CanvasGradient, CanvasGradientKind, CanvasPattern, CanvasPatternRepetition, CanvasSurface,
        ParsedSvgPath, CanvasPublication, OffscreenCanvasTransfer,
    },
    Rgba8Image,
};
use lumen_html_text::{
    CanvasFontKerning, CanvasFontVariantCaps, CanvasTextOptions, CanvasTextRendering, FontFace,
    FontProvider, FontSet, RegisteredFont, DEFAULT_FONT_BYTES, TEST_FONT_BOLD_BYTES,
    TEST_FONT_BYTES,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    cell::RefCell,
    collections::HashMap,
    ops::Deref,
    rc::Rc,
    sync::{Arc, OnceLock},
};
use tiny_skia::{BlendMode, FillRule, LineCap, LineJoin, Path, PathBuilder, Rect, Transform};

const DEFAULT_WIDTH: u32 = 300;
const DEFAULT_HEIGHT: u32 = 150;
const MAX_IMAGE_DATA_BYTES: usize = 64 * 1024 * 1024;
const WEBIDL_UINT32_MODULUS: f64 = 4_294_967_296.0;
const WEBIDL_UINT64_LIMIT: f64 = 18_446_744_073_709_551_616.0;
const OFFSCREEN_CANVAS_OWNER_SLOT: &str = "#lumen_offscreen_canvas_owner\u{1}canvas";
static NEXT_GRADIENT_ID: AtomicU64 = AtomicU64::new(1);
static CANVAS_FONTS: OnceLock<FontSet> = OnceLock::new();

pub(crate) enum CanvasFontSource {
    Static(&'static FontSet),
    Realm(Rc<FontSet>, Option<font_loading::WeakFontRealm>,RefCell<Rc<FontSet>>,RenderFontFallback),
}

impl Deref for CanvasFontSource {
    type Target = FontSet;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Static(fonts) => fonts,
            Self::Realm(fonts, _,_,_) => fonts,
        }
    }
}

pub(crate) enum RenderFontFallback {
    Static(&'static FontSet),
    Owned(Arc<FontSet>),
}
impl Deref for RenderFontFallback {
    type Target=FontSet;
    fn deref(&self)->&FontSet {match self {Self::Static(fonts)=>fonts,Self::Owned(fonts)=>fonts}}
}

impl CanvasFontSource {
    fn realm(fonts:Rc<FontSet>,owner:font_loading::WeakFontRealm,fallback:RenderFontFallback)->Self {
        owner.attach_display_owner();
        Self::Realm(fonts.clone(),Some(owner),RefCell::new(fonts),fallback)
    }

    fn refresh_fonts(&self)->Result<(), &'static str> {
        if let Self::Realm(_,Some(owner),current,fallback)=self {
            if let Some(fonts)=owner.refresh_render_font_set(fallback)? {
                if !Rc::ptr_eq(&fonts,&current.borrow()) {*current.borrow_mut()=fonts;}
            }
        }
        Ok(())
    }
    fn visit_fonts<R>(&self,visit:impl FnOnce(&FontSet)->R)->R {
        match self {Self::Static(fonts)=>visit(fonts),Self::Realm(_,_,current,_)=>visit(&current.borrow())}
    }
    fn with_fonts<R>(&self,visit:impl FnOnce(&FontSet)->R)->R {
        let _=self.refresh_fonts();self.visit_fonts(visit)
    }
    fn with_fonts_checked<R>(&self,visit:impl FnOnce(&FontSet)->R)->Result<R,&'static str> {
        self.refresh_fonts()?;Ok(self.visit_fonts(visit))
    }

    fn request_metric_font(&self,spec:&FontSpec) {
        if let Self::Realm(_,Some(owner),_,_)=self {owner.request_metric_font(spec);}
    }

    fn queue_demand_task(&self,ctx:&mut Ctx)->OpResult<()> {
        if let Self::Realm(_,Some(owner),_,_)=self {owner.queue_demand_task(ctx)?;}
        Ok(())
    }

    fn request_rendered_font(&self,text:&str,size:f32,spec:&FontSpec)->Result<(), &'static str> {
        if size.is_finite() && size>0.0 {
            if let Self::Realm(_,Some(owner),_,_)=self {owner.request_rendered_font(spec,text)?;}
        }
        Ok(())
    }

}

impl TextShaper for CanvasFontSource {
    fn first_available_font_metric(&self, spec: &FontSpec, metric: lumen_html::paint::FontMetric) -> Option<Option<f32>> {
        self.request_metric_font(spec);
        self.with_fonts(|fonts| fonts.first_available_font_metric(spec, metric))
    }
    fn generation(&self)->u64 {self.with_fonts(|fonts|fonts.generation())}
    fn glyph_ink_bounds(&self,glyph:&lumen_html::paint::Glyph,size:f32)->Option<lumen_html::paint::Rect> {self.with_fonts(|fonts|fonts.glyph_ink_bounds(glyph,size))}
    fn shape(&self,text:&str,size:f32)->Result<lumen_html::paint::ShapedRun,()> {self.request_rendered_font(text,size,&FontSpec::default()).map_err(|_|())?;self.with_fonts_checked(|fonts|fonts.shape(text,size)).map_err(|_|())?}
    fn shape_directional(&self,text:&str,size:f32,rtl:bool)->Result<lumen_html::paint::ShapedRun,()> {self.request_rendered_font(text,size,&FontSpec::default()).map_err(|_|())?;self.with_fonts_checked(|fonts|fonts.shape_directional(text,size,rtl)).map_err(|_|())?}
    fn shape_styled(&self,text:&str,size:f32,rtl:bool,spec:&FontSpec)->Result<lumen_html::paint::ShapedRun,()> {self.request_rendered_font(text,size,spec).map_err(|_|())?;self.with_fonts_checked(|fonts|fonts.shape_styled(text,size,rtl,spec)).map_err(|_|())?}
    fn shape_resolved(&self,text:&str,size:f32,rtl:bool,spec:&FontSpec)->Result<lumen_html::paint::ShapedRun,()> {self.request_rendered_font(text,size,spec).map_err(|_|())?;self.with_fonts_checked(|fonts|fonts.shape_resolved(text,size,rtl,spec)).map_err(|_|())?}
    fn shape_styled_with_cluster_advances(&self,text:&str,size:f32,rtl:bool,spec:&FontSpec)->Result<Option<lumen_html::paint::ShapedRunWithClusterAdvances>,()> {self.request_rendered_font(text,size,spec).map_err(|_|())?;self.with_fonts_checked(|fonts|fonts.shape_styled_with_cluster_advances(text,size,rtl,spec)).map_err(|_|())?}
    fn shape_resolved_with_cluster_advances(&self,text:&str,size:f32,rtl:bool,spec:&FontSpec)->Result<Option<lumen_html::paint::ShapedRunWithClusterAdvances>,()> {self.request_rendered_font(text,size,spec).map_err(|_|())?;self.with_fonts_checked(|fonts|fonts.shape_resolved_with_cluster_advances(text,size,rtl,spec)).map_err(|_|())?}
    fn shape_styled_context(&self,text:&str,size:f32,rtl:bool,spec:&FontSpec,language:Option<&str>)->Result<lumen_html::paint::ShapedRun,()> {self.request_rendered_font(text,size,spec).map_err(|_|())?;self.with_fonts_checked(|fonts|fonts.shape_styled_context(text,size,rtl,spec,language)).map_err(|_|())?}
    fn shape_resolved_context(&self,text:&str,size:f32,rtl:bool,spec:&FontSpec,language:Option<&str>)->Result<lumen_html::paint::ShapedRun,()> {self.request_rendered_font(text,size,spec).map_err(|_|())?;self.with_fonts_checked(|fonts|fonts.shape_resolved_context(text,size,rtl,spec,language)).map_err(|_|())?}
    fn shape_resolved_context_with_cluster_advances(&self,text:&str,size:f32,rtl:bool,spec:&FontSpec,language:Option<&str>)->Result<Option<lumen_html::paint::ShapedRunWithClusterAdvances>,()> {self.request_rendered_font(text,size,spec).map_err(|_|())?;self.with_fonts_checked(|fonts|fonts.shape_resolved_context_with_cluster_advances(text,size,rtl,spec,language)).map_err(|_|())?}
    fn shape_resolved_segment_with_cluster_advances(&self,text:&str,source:std::ops::Range<usize>,size:f32,rtl:bool,spec:&FontSpec,language:Option<&str>)->Result<Option<lumen_html::paint::ShapedRunWithClusterAdvances>,()> {let piece=text.get(source.clone()).ok_or(())?;self.request_rendered_font(piece,size,spec).map_err(|_|())?;self.with_fonts_checked(|fonts|fonts.shape_resolved_segment_with_cluster_advances(text,source,size,rtl,spec,language)).map_err(|_|())?}
    fn ascent(&self,size:f32)->f32 {self.with_fonts(|fonts|fonts.ascent(size))}
    fn line_height(&self,size:f32)->f32 {self.with_fonts(|fonts|fonts.line_height(size))}
    fn underline_metrics(&self,size:f32)->(f32,f32) {self.with_fonts(|fonts|fonts.underline_metrics(size))}
    fn strike_metrics(&self,size:f32)->(f32,f32) {self.with_fonts(|fonts|fonts.strike_metrics(size))}
    fn ascent_styled(&self,size:f32,spec:&FontSpec)->f32 {self.request_metric_font(spec);self.with_fonts(|fonts|fonts.ascent_styled(size,spec))}
    fn line_height_styled(&self,size:f32,spec:&FontSpec)->f32 {self.request_metric_font(spec);self.with_fonts(|fonts|fonts.line_height_styled(size,spec))}
    fn underline_metrics_styled(&self,size:f32,spec:&FontSpec)->(f32,f32) {self.request_metric_font(spec);self.with_fonts(|fonts|fonts.underline_metrics_styled(size,spec))}
    fn strike_metrics_styled(&self,size:f32,spec:&FontSpec)->(f32,f32) {self.request_metric_font(spec);self.with_fonts(|fonts|fonts.strike_metrics_styled(size,spec))}
    fn primary_character_widths_styled(&self,size:f32,spec:&FontSpec)->Option<lumen_html::paint::PrimaryCharacterWidths> {self.request_metric_font(spec);self.with_fonts(|fonts|fonts.primary_character_widths_styled(size,spec))}
    fn font_unit_metrics_styled(&self,size:f32,spec:&FontSpec,vertical:bool,upright_zero:bool)->lumen_html::paint::FontUnitMetrics {
        self.request_metric_font(spec);
        self.with_fonts(|fonts|fonts.font_unit_metrics_styled(size,spec,vertical,upright_zero))
    }
    fn font_relative_metrics_styled(&self,size:f32,spec:&FontSpec)->lumen_html::paint::FontRelativeMetrics {self.request_metric_font(spec);self.with_fonts(|fonts|fonts.font_relative_metrics_styled(size,spec))}
}

impl FontProvider for CanvasFontSource {
    fn first_available_metric(&self,font:&FontSpec,metric:lumen_html::paint::FontMetric)->Option<f32> {
        self.request_metric_font(font);self.with_fonts(|fonts|fonts.first_available_metric(font,metric))
    }
    fn registrations(&self)->Option<Vec<lumen_html_text::FontRegistration>> {self.with_fonts(|fonts|fonts.registrations())}
    fn rasterize_glyph(&self,face:u64,id:u16,size:f32)->Result<lumen_html_text::GlyphCoverage, &'static str> {self.with_fonts_checked(|fonts|fonts.rasterize_glyph(face,id,size)).and_then(|result|result)}
    fn outline_glyph(&self,face:u64,id:u16,size:f32)->Result<lumen_html_text::GlyphOutline, &'static str> {self.with_fonts_checked(|fonts|fonts.outline_glyph(face,id,size)).and_then(|result|result)}
    fn face_key(&self,face:u64)->Result<u64, &'static str> {self.with_fonts_checked(|fonts|fonts.face_key(face)).and_then(|result|result)}
    fn shape_cache_stats(&self)->lumen_html_text::ShapeCacheStats {self.with_fonts(|fonts|fonts.shape_cache_stats())}
    fn shape_canvas_text(&self,text:&str,size:f32,rtl:bool,spec:&FontSpec,options:&CanvasTextOptions)->Result<lumen_html::paint::ShapedRun, &'static str> {
        self.request_rendered_font(text,size,spec)?;
        self.with_fonts_checked(|fonts|fonts.shape_canvas_text(text,size,rtl,spec,options)).and_then(|result|result)
    }
}

#[derive(Clone)]
struct CanvasTextState {
    font: String,
    size: f32,
    spec: FontSpec,
    align: String,
    baseline: String,
    direction: String,
    letter_spacing: f32,
    word_spacing: f32,
    font_kerning: CanvasFontKerning,
    font_variant_caps: CanvasFontVariantCaps,
    text_rendering: CanvasTextRendering,
}

impl CanvasTextState {
    fn shaping_options(&self) -> CanvasTextOptions {
        CanvasTextOptions {
            letter_spacing: self.letter_spacing,
            word_spacing: self.word_spacing,
            font_kerning: self.font_kerning,
            font_variant_caps: self.font_variant_caps,
            text_rendering: self.text_rendering,
        }
    }
}

fn text_anchor(state: &CanvasTextState) -> f32 {
    match state.align.as_str() {
        "center" => 0.5,
        "right" => 1.0,
        "end" if state.direction == "rtl" => 0.0,
        "start" if state.direction == "rtl" => 1.0,
        "end" => 1.0,
        _ => 0.0,
    }
}

fn text_baseline(state: &CanvasTextState, y: f32, ascent: f32, descent: f32) -> f32 {
    match state.baseline.as_str() {
        "top" | "hanging" => y + ascent,
        "middle" => y + (ascent - descent) * 0.5,
        "bottom" | "ideographic" => y - descent,
        _ => y,
    }
}

impl Default for CanvasTextState {
    fn default() -> Self {
        Self {
            font: "10px sans-serif".into(),
            size: 10.0,
            spec: FontSpec {
                families: Some(Arc::from([lumen_html::paint::FontFamily::Generic(lumen_html::paint::GenericFontFamily::SansSerif)])),
                stretch: 100.0,
                ..FontSpec::default()
            },
            align: "start".into(),
            baseline: "alphabetic".into(),
            direction: "inherit".into(),
            letter_spacing: 0.0,
            word_spacing: 0.0,
            font_kerning: CanvasFontKerning::Auto,
            font_variant_caps: CanvasFontVariantCaps::Normal,
            text_rendering: CanvasTextRendering::Auto,
        }
    }
}

struct GradientHandle {
    gradient: CanvasGradient,
}
struct PatternHandle {
    pattern: CanvasPattern,
    origin_clean: bool,
}
thread_local! {
    static GRADIENTS: RefCell<HashMap<u64, std::rc::Weak<GradientHandle>>> = RefCell::new(HashMap::new());
    static PATTERNS: RefCell<HashMap<u64, std::rc::Weak<PatternHandle>>> = RefCell::new(HashMap::new());
}

/// Drop this thread's dead gradient and pattern entries: a dead `Weak` still pins the handle's
/// allocation until the entry is removed.
pub fn prune_dead_thread_handles() {
    let _ = GRADIENTS.try_with(|gradients| {
        let mut gradients = gradients.borrow_mut();
        gradients.retain(|_, weak| weak.strong_count() > 0);
        gradients.shrink_to_fit();
    });
    let _ = PATTERNS.try_with(|patterns| {
        let mut patterns = patterns.borrow_mut();
        patterns.retain(|_, weak| weak.strong_count() > 0);
        patterns.shrink_to_fit();
    });
}

#[derive(Default)]
pub struct CanvasRegistry {
    surfaces: RefCell<HashMap<NodeId, Rc<RefCell<CanvasData>>>>,
    pending_publish: RefCell<Vec<NodeId>>,
    pending_detach: RefCell<Vec<NodeId>>,
    paint_requests: RefCell<Vec<NodeId>>,
    drawable_epochs: RefCell<HashMap<NodeId,u64>>,
    drawable_discovery_version: Cell<Option<u64>>,
    drawable_snapshots: RefCell<Vec<(NodeId, NodeId, Rgba8Image)>>,
    snapshot_provider: RefCell<Option<Rc<dyn Fn(&mut lumen_html::session::RenderSession, NodeId, u32, u32) -> Result<Rgba8Image, String>>>>,

}

pub struct CanvasData {
    surface: CanvasSurface,
    logical_width: u64,
    logical_height: u64,
    bitmap_available: bool,
    context_mode: Option<&'static str>,
    transferred: bool,
    inherited_direction_rtl: bool,
    bitmap_output: Option<Rgba8Image>,
    gpu_generation: u64,
    gpu_snapshot_revision: u64,
    snapshot_synchronizer: Option<Rc<dyn Fn() -> Result<(), String>>>,
    bitmap_alpha: bool,
    alpha: bool,
    color_space: CanvasColorSpace,
    desynchronized: bool,
    will_read_frequently: bool,
    current_path: CanvasPath,
    context_wrapper: Option<WeakValue>,
    suppress_dimension_mutation: bool,
    pending_error: Option<String>,
    fill_gradient_value: Option<Value>,
    stroke_gradient_value: Option<Value>,
    fill_pattern_value: Option<Value>,
    stroke_pattern_value: Option<Value>,
    fill_pattern_origin_clean: bool,
    stroke_pattern_origin_clean: bool,
    saved_fill_styles: Vec<(Option<Value>, Option<Value>, bool)>,
    saved_stroke_styles: Vec<(Option<Value>, Option<Value>, bool)>,
    origin_clean: bool,
    text: CanvasTextState,
    saved_text: Vec<CanvasTextState>,
    remote_inbound: Option<Arc<std::sync::Mutex<CanvasPublication>>>,
    remote_outbound: Option<Arc<std::sync::Mutex<CanvasPublication>>>,
    remote_generation: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CanvasColorSpace {
    #[default]
    Srgb,
    DisplayP3,
}

impl CanvasColorSpace {
    fn as_str(self) -> &'static str {
        match self {
            Self::Srgb => "srgb",
            Self::DisplayP3 => "display-p3",
        }
    }
}

#[derive(Clone, Copy)]
struct Canvas2dSettings {
    alpha: bool,
    color_space: CanvasColorSpace,
    desynchronized: bool,
    will_read_frequently: bool,
}

impl Default for Canvas2dSettings {
    fn default() -> Self {
        Self {
            alpha: true,
            color_space: CanvasColorSpace::Srgb,
            desynchronized: false,
            will_read_frequently: false,
        }
    }
}

fn parse_canvas_2d_settings(ctx: &mut Ctx, options: Option<&Value>) -> OpResult<Canvas2dSettings> {
    let Some(options) = options.filter(|value| !matches!(value, Value::Null | Value::Undefined))
    else {
        return Ok(Canvas2dSettings::default());
    };
    let mut settings = Canvas2dSettings::default();
    let alpha = ctx.member_get(options, "alpha").map_err(OpError::thrown)?;
    if !matches!(alpha, Value::Undefined) {
        settings.alpha = ctx.to_boolean(&alpha);
    }
    let color_space = ctx
        .member_get(options, "colorSpace")
        .map_err(OpError::thrown)?;
    if !matches!(color_space, Value::Undefined) {
        let color_space = ctx.coerce_string(&color_space).map_err(OpError::thrown)?;
        settings.color_space = match color_space.as_ref() {
            "srgb" => CanvasColorSpace::Srgb,
            "display-p3" => CanvasColorSpace::DisplayP3,
            _ => {
                return Err(OpError::new(
                    "TypeError",
                    "CanvasRenderingContext2D colorSpace is invalid",
                ));
            }
        };
    }
    let desynchronized = ctx
        .member_get(options, "desynchronized")
        .map_err(OpError::thrown)?;
    if !matches!(desynchronized, Value::Undefined) {
        settings.desynchronized = ctx.to_boolean(&desynchronized);
    }
    let will_read_frequently = ctx
        .member_get(options, "willReadFrequently")
        .map_err(OpError::thrown)?;
    if !matches!(will_read_frequently, Value::Undefined) {
        settings.will_read_frequently = ctx.to_boolean(&will_read_frequently);
    }
    Ok(settings)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ImageDataPixelFormat {
    #[default]
    RgbaUnorm8,
    RgbaFloat16,
}

impl ImageDataPixelFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::RgbaUnorm8 => "rgba-unorm8",
            Self::RgbaFloat16 => "rgba-float16",
        }
    }

    fn typed_array_kind(self) -> TaKind {
        match self {
            Self::RgbaUnorm8 => TaKind::U8Clamped,
            Self::RgbaFloat16 => TaKind::F16,
        }
    }

    fn bytes_per_component(self) -> usize {
        match self {
            Self::RgbaUnorm8 => 1,
            Self::RgbaFloat16 => 2,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct ImageDataSettings {
    color_space: CanvasColorSpace,
    pixel_format: ImageDataPixelFormat,
}

fn parse_image_data_settings(
    ctx: &mut Ctx,
    options: Option<&Value>,
) -> OpResult<ImageDataSettings> {
    parse_image_data_settings_with_default(ctx, options, CanvasColorSpace::Srgb)
}

fn parse_image_data_settings_with_default(
    ctx: &mut Ctx,
    options: Option<&Value>,
    default_color_space: CanvasColorSpace,
) -> OpResult<ImageDataSettings> {
    let mut settings = ImageDataSettings {
        color_space: default_color_space,
        pixel_format: ImageDataPixelFormat::RgbaUnorm8,
    };
    let Some(options) = options.filter(|value| !matches!(value, Value::Null | Value::Undefined))
    else {
        return Ok(settings);
    };
    if !matches!(options, Value::Obj(_)) {
        return Err(OpError::new(
            "TypeError",
            "ImageData settings must be an object",
        ));
    }
    let color_space = ctx
        .member_get(options, "colorSpace")
        .map_err(OpError::thrown)?;
    if !matches!(color_space, Value::Undefined) {
        let color_space = ctx.coerce_string(&color_space).map_err(OpError::thrown)?;
        settings.color_space = match color_space.as_ref() {
            "srgb" => CanvasColorSpace::Srgb,
            "display-p3" => CanvasColorSpace::DisplayP3,
            _ => {
                return Err(OpError::new("TypeError", "ImageData colorSpace is invalid"));
            }
        };
    }
    let pixel_format = ctx
        .member_get(options, "pixelFormat")
        .map_err(OpError::thrown)?;
    if !matches!(pixel_format, Value::Undefined) {
        let pixel_format = ctx.coerce_string(&pixel_format).map_err(OpError::thrown)?;
        settings.pixel_format = match pixel_format.as_ref() {
            "rgba-unorm8" => ImageDataPixelFormat::RgbaUnorm8,
            "rgba-float16" => ImageDataPixelFormat::RgbaFloat16,
            _ => {
                return Err(OpError::new(
                    "TypeError",
                    "ImageData pixelFormat is invalid",
                ));
            }
        };
    }
    Ok(settings)
}

/// Opaque backing supplied to the embedder when a canvas requests WebGPU.
/// The callbacks stay in the HTML realm so GPU code cannot bypass canvas
/// ownership, dimensions, or retained-paint publication.
#[derive(Clone)]
pub struct CanvasGpuTarget {
    pub canvas: Value,
    pub dimensions: Rc<dyn Fn() -> (u32, u32)>,
    pub generation: Rc<dyn Fn() -> u64>,
    pub invalidate: Rc<dyn Fn()>,
    pub reserve_snapshot_revision: Rc<dyn Fn() -> u64>,
    pub publish_rgba: Rc<dyn Fn(u64, u64, u32, u32, Vec<u8>) -> Result<(), String>>,
    /// Backends install a weakly owned callback to finish drawing before bitmap reads.
    pub set_snapshot_synchronizer: Rc<dyn Fn(Rc<dyn Fn() -> Result<(), String>>)>,
}

type CanvasGpuContextFactory = Rc<dyn Fn(&mut Ctx, CanvasGpuTarget) -> OpResult<Value>>;

struct CanvasGpuContextFactoryHost(CanvasGpuContextFactory);

type CanvasWebGlContextFactory =
    Rc<dyn Fn(&mut Ctx, CanvasGpuTarget, &str, Option<Value>) -> OpResult<Value>>;

struct CanvasWebGlContextFactoryHost(CanvasWebGlContextFactory);

/// Install the runtime's typed WebGPU canvas-context factory in this realm.
pub fn set_gpu_canvas_context_factory(
    ctx: &mut Ctx,
    factory: Rc<dyn Fn(&mut Ctx, CanvasGpuTarget) -> OpResult<Value>>,
) {
    ctx.op_state().put(CanvasGpuContextFactoryHost(factory));
}

/// Install the runtime's typed WebGL canvas-context factory in this realm.
pub fn set_webgl_canvas_context_factory(
    ctx: &mut Ctx,
    factory: Rc<dyn Fn(&mut Ctx, CanvasGpuTarget, &str, Option<Value>) -> OpResult<Value>>,
) {
    ctx.op_state().put(CanvasWebGlContextFactoryHost(factory));
}

fn create_gpu_canvas_context(ctx: &mut Ctx, target: CanvasGpuTarget) -> OpResult<Value> {
    let factory = ctx
        .host_mut::<CanvasGpuContextFactoryHost>()
        .map(|host| host.0.clone())
        .ok_or_else(|| {
            OpError::new(
                "NotSupportedError",
                "WebGPU canvas contexts are unavailable in this host",
            )
        })?;
    factory(ctx, target)
}

fn create_webgl_canvas_context(
    ctx: &mut Ctx,
    target: CanvasGpuTarget,
    context_id: &str,
    options: Option<Value>,
) -> OpResult<Value> {
    let factory = ctx
        .host_mut::<CanvasWebGlContextFactoryHost>()
        .map(|host| host.0.clone())
        .ok_or_else(|| OpError::new("NotSupportedError", "WebGL canvas factory is unavailable"))?;
    factory(ctx, target, context_id, options)
}

impl CanvasData {
    /// Read the current drawing buffer without holding a borrow across backend work.
    pub fn snapshot_for_read(data: &Rc<RefCell<Self>>) -> OpResult<Rgba8Image> {
        data.borrow().ensure_bitmap_available()?;
        let synchronize = data.borrow().snapshot_synchronizer.clone();
        if let Some(synchronize) = synchronize {
            synchronize().map_err(|message| OpError::new("OperationError", message))?;
        }
        Ok(data.borrow().snapshot())
    }

    fn new(width: u32, height: u32) -> OpResult<Self> {
        check_dimensions(width, height)?;
        let surface = CanvasSurface::new(width, height).map_err(|_| {
            OpError::new("IndexSizeError", "canvas bitmap dimensions are too large")
        })?;
        Ok(Self {
            surface,
            logical_width: u64::from(width),
            logical_height: u64::from(height),
            bitmap_available: true,
            context_mode: None,
            transferred: false,
            inherited_direction_rtl: false,
            bitmap_output: None,
            gpu_generation: 0,
            gpu_snapshot_revision: 0,
            snapshot_synchronizer: None,
            bitmap_alpha: true,
            alpha: true,
            color_space: CanvasColorSpace::Srgb,
            desynchronized: false,
            will_read_frequently: false,
            current_path: CanvasPath::default(),
            context_wrapper: None,
            suppress_dimension_mutation: false,
            pending_error: None,
            fill_gradient_value: None,
            stroke_gradient_value: None,
            fill_pattern_value: None,
            stroke_pattern_value: None,
            fill_pattern_origin_clean: true,
            stroke_pattern_origin_clean: true,
            origin_clean: true,
            saved_fill_styles: Vec::new(),
            saved_stroke_styles: Vec::new(),
            text: CanvasTextState::default(),
            saved_text: Vec::new(),
            remote_inbound: None,
            remote_outbound: None,
            remote_generation: 0,
        })
    }

    fn new_offscreen(width: u64, height: u64) -> OpResult<Self> {
        let (surface, bitmap_available) = offscreen_surface(width, height)?;
        let mut data = Self::new(0, 0)?;
        data.surface = surface;
        data.logical_width = width;
        data.logical_height = height;
        data.bitmap_available = bitmap_available;
        Ok(data)
    }

    fn resize(&mut self, width: u32, height: u32) -> OpResult<()> {
        check_dimensions(width, height)?;
        self.surface.resize(width, height).map_err(|_| {
            OpError::new("IndexSizeError", "canvas bitmap dimensions are too large")
        })?;
        self.logical_width = u64::from(width);
        self.logical_height = u64::from(height);
        self.bitmap_available = true;
        self.reset_after_resize();
        Ok(())
    }

    fn resize_offscreen(&mut self, width: u64, height: u64) -> OpResult<()> {
        let (surface, bitmap_available) = offscreen_surface(width, height)?;
        self.surface = surface;
        self.logical_width = width;
        self.logical_height = height;
        self.bitmap_available = bitmap_available;
        self.reset_after_resize();
        Ok(())
    }

    fn reset_after_resize(&mut self) {
        if matches!(self.context_mode, Some("webgpu" | "webgl" | "webgl2")) {
            self.bitmap_output = None;
        }
        self.gpu_generation = self.gpu_generation.wrapping_add(1);
        self.current_path = CanvasPath::default();
        self.fill_gradient_value = None;
        self.stroke_gradient_value = None;
        self.fill_pattern_value = None;
        self.stroke_pattern_value = None;
        self.fill_pattern_origin_clean = true;
        self.stroke_pattern_origin_clean = true;
        if self.bitmap_output.is_none() {
            self.origin_clean = true;
        }
        self.saved_fill_styles.clear();
        self.saved_stroke_styles.clear();
        self.text = CanvasTextState::default();
        self.saved_text.clear();
    }

    fn ensure_bitmap_available(&self) -> OpResult<()> {
        if self.bitmap_available {
            Ok(())
        } else {
            Err(OpError::new(
                "InvalidStateError",
                "canvas bitmap is unavailable because its dimensions exceed the host resource limit",
            ))
        }
    }

    fn snapshot(&self) -> Rgba8Image {
        let mut image = self
            .bitmap_output
            .clone()
            .unwrap_or_else(|| self.surface.snapshot());
        if self.context_mode == Some("bitmaprenderer") && !self.bitmap_alpha {
            make_image_opaque(&mut image.pixels);
        }
        if self.context_mode == Some("2d") && !self.alpha {
            make_image_opaque(&mut image.pixels);
        }
        image
    }
}

impl CanvasRegistry {
    pub fn set_snapshot_provider(&self, provider: Rc<dyn Fn(&mut lumen_html::session::RenderSession,NodeId,u32,u32)->Result<Rgba8Image,String>>) {
        *self.snapshot_provider.borrow_mut() = Some(provider);
    }
    pub fn paint_pending(&self, realm: &DomRealm) -> bool {
        let version = realm.session.borrow().document().version();
        (self.snapshot_provider.borrow().is_some() && self.drawable_discovery_version.get()!=Some(version)) || !self.paint_requests.borrow().is_empty()
            || self.drawable_epochs.borrow().values().any(|epoch|*epoch!=version)
    }

    fn request_paint(&self, node: NodeId) -> OpResult<()> {
        let mut requests = self.paint_requests.borrow_mut();
        if !requests.contains(&node) {
            if requests.len() >= 128 { return Err(OpError::new("QuotaExceededError", "Canvas paint request limit")); }
            requests.push(node);
        }
        Ok(())
    }

    pub fn update_drawable_paint(&self, ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
        let mut requests = core::mem::take(&mut *self.paint_requests.borrow_mut());
        let forced_requests = requests.clone();
        let version = realm.session.borrow().document().version();
        if self.snapshot_provider.borrow().is_some() && self.drawable_discovery_version.get()!=Some(version) {
            let session=realm.session.borrow();
            let document=session.document();
            let mut stack=vec![document.root()];
            let mut canvases=0usize;
            while let Some(node)=stack.pop() {
                if matches!(document.kind(node),Ok(lumen_html::NodeKind::Element{name,..}) if name=="canvas")
                    && document.get_attribute_ns(node,None,"content").ok().flatten().as_deref()==Some("drawable") {
                    canvases+=1;
                    if canvases>128 {return Err(OpError::new("QuotaExceededError","Drawable canvas count limit"));}
                    if !self.drawable_epochs.borrow().contains_key(&node) && !requests.contains(&node) {requests.push(node);}
                }
                stack.extend(document.composed_children(node).map_err(dom_error)?.into_iter().rev());
            }
            self.drawable_discovery_version.set(Some(version));
        }
        self.drawable_epochs.borrow_mut().retain(|node,_|drawable_connected(realm.session.borrow().document(),*node));
        self.drawable_snapshots.borrow_mut().retain(|(owner,node,_)|drawable_connected(realm.session.borrow().document(),*owner) && drawable_connected(realm.session.borrow().document(),*node));
        for (&canvas,&epoch) in self.drawable_epochs.borrow().iter() {
            if epoch!=version && !requests.contains(&canvas) {requests.push(canvas);}
        }
        if requests.is_empty() { return Ok(()); }
        // Author request order is unrelated to rendering order. Walk the composed
        // tree once, bounded by the document, then paint children before ancestors.
        {
            let session=realm.session.borrow();
            let document=session.document();
            let mut ordered=Vec::with_capacity(requests.len());
            let mut stack=vec![document.root()];
            while let Some(node)=stack.pop() {
                if requests.contains(&node) {ordered.push(node);}
                let children=document.composed_children(node).map_err(dom_error)?;
                stack.extend(children.into_iter().rev());
            }
            requests=ordered;
        }
        let provider = self.snapshot_provider.borrow().clone().ok_or_else(|| OpError::new("NotSupportedError", "Host does not provide drawable rendering"))?;
        // Complete a child canvas's actual snapshot/event before its ancestor consumes its pixels.
        for canvas in requests.into_iter().rev() {
            let (width, height, nodes) = {
                let session = realm.session.borrow();
                let document = session.document();
                if !matches!(document.kind(canvas), Ok(lumen_html::NodeKind::Element {name, ..}) if name == "canvas") { continue; }
                let drawable=document.get_attribute_ns(canvas,None,"content").ok().flatten().as_deref()==Some("drawable");
                let width = lumen_html::layout::canvas_dimension(document.get_attribute_ns(canvas,None,"width").ok().flatten().as_deref(),DEFAULT_WIDTH);
                let height = lumen_html::layout::canvas_dimension(document.get_attribute_ns(canvas,None,"height").ok().flatten().as_deref(),DEFAULT_HEIGHT);
                let mut nodes = Vec::new();
                let mut stack = if drawable {document.composed_children(canvas).map_err(dom_error)?} else {Vec::new()};
                stack.reverse();
                while let Some(node) = stack.pop() {
                    let nested_canvas=matches!(document.kind(node),Ok(lumen_html::NodeKind::Element{name,..}) if name == "canvas");
                    if nested_canvas {
                        if self.surfaces.borrow().get(&node).is_some_and(|data|!data.borrow().origin_clean) {
                            return Err(OpError::new("SecurityError","Drawable snapshot contains unreadable canvas pixels"));
                        }
                    }
                    if document.get_attribute_ns(node,None,"drawable").ok().flatten().is_some() {
                        if nodes.len() >= 128 { return Err(OpError::new("QuotaExceededError", "Drawable snapshot count limit")); }
                        nodes.push(node);
                    }
                    // A drawable nested canvas contributes its real bitmap,
                    // while its fallback subtree belongs to its own generation.
                    if !nested_canvas {
                        stack.extend(document.composed_children(node).map_err(dom_error)?.into_iter().rev());
                    }
                }
                (width,height,nodes)
            };
            let mut changed_nodes = Vec::new();
            let mut fresh = Vec::with_capacity(nodes.len());
            let mut bytes = 0usize;
            // Paint events also belong to ordinary canvases. Complete an actual
            // host-rendered canvas snapshot before their author callback; only
            // content=drawable canvases publish drawable descendant resources.
            if nodes.is_empty() {
                let painted_canvas=provider(&mut realm.session.borrow_mut(),canvas,width,height)
                    .map_err(|error|OpError::new("OperationError",error))?;
                if painted_canvas.pixels.len()>MAX_IMAGE_DATA_BYTES {return Err(OpError::new("QuotaExceededError","Canvas paint bitmap budget"));}
            }
            for node in nodes {
                let image = provider(&mut realm.session.borrow_mut(),node,width,height).map_err(|error| OpError::new("OperationError",error))?;
                bytes = bytes.checked_add(image.pixels.len()).filter(|bytes| *bytes <= MAX_IMAGE_DATA_BYTES).ok_or_else(|| OpError::new("QuotaExceededError", "Drawable snapshot bitmap budget"))?;
                let changed = self.drawable_snapshots.borrow().iter().find(|(owner,element,_)|*owner==canvas && *element==node)
                    .is_none_or(|(_,_,old)|old.width!=image.width || old.height!=image.height || old.pixels!=image.pixels);
                if changed {changed_nodes.push(node);}
                fresh.push((canvas,node,image));
            }
            {
                let mut retained = self.drawable_snapshots.borrow_mut();
                retained.retain(|(owner,_,_)| *owner != canvas);
                let total = retained.iter().try_fold(bytes, |bytes,(_,_,image)|bytes.checked_add(image.pixels.len())).filter(|bytes|*bytes<=MAX_IMAGE_DATA_BYTES).ok_or_else(||OpError::new("QuotaExceededError","Drawable snapshot bitmap budget"))?;
                let _ = total;
                if retained.len()+fresh.len()>128 {return Err(OpError::new("QuotaExceededError","Drawable snapshot count limit"));}
                retained.extend(fresh);
            }
            // All resources are retained before author callbacks run. DOM mutations in
            // the callback therefore cannot alter this published rendering generation.
            if self.drawable_epochs.borrow().len()>=128 && !self.drawable_epochs.borrow().contains_key(&canvas) {return Err(OpError::new("QuotaExceededError","Drawable canvas count limit"));}
            self.drawable_epochs.borrow_mut().insert(canvas,realm.session.borrow().document().version());
            if changed_nodes.is_empty() && !forced_requests.contains(&canvas) {continue;}
            let elements=changed_nodes.into_iter().map(|node|realm.wrap(ctx,node)).collect::<Vec<_>>();
            let array=<lumen::embed::JsHost as lumen_bind::Host>::from_list(ctx,elements);
            realm.dispatch_user_agent(ctx,canvas,"paint",false,false,&[("changedElements",array)])?;
        }
        Ok(())
    }

    pub(crate) fn adopt_nodes_into(
        &self,
        ctx: &mut Ctx,
        target: &CanvasRegistry,
        target_realm: &Rc<DomRealm>,
        mapping: &[(NodeId, NodeId)],
    ) -> OpResult<()> {
        for &(old, new) in mapping {
            if let Some(surface) = self.surfaces.borrow_mut().remove(&old) {
                if let Some(wrapper) = surface
                    .borrow()
                    .context_wrapper
                    .as_ref()
                    .and_then(WeakValue::upgrade)
                {
                    if surface.borrow().context_mode == Some("bitmaprenderer") {
                        ctx.with_instance_mut::<DomImageBitmapRenderingContext, _>(
                            &wrapper,
                            |context| {
                                context.realm = Some(target_realm.clone());
                                context.node = Some(new);
                            },
                        )?;
                    } else {
                        ctx.with_instance_mut::<DomCanvasRenderingContext2D, _>(
                            &wrapper,
                            |context| {
                                context.realm = Some(target_realm.clone());
                                context.node = Some(new);
                            },
                        )?;
                    }
                }
                target.surfaces.borrow_mut().insert(new, surface);
            }
            let mut pending = self.pending_publish.borrow_mut();
            if let Some(index) = pending.iter().position(|node| *node == old) {
                pending.remove(index);
                target.queue_publish(new);
            }
            self.pending_detach.borrow_mut().retain(|node| *node != old);
        }
        Ok(())
    }

    pub fn data_for(&self, realm: &DomRealm, node: NodeId) -> OpResult<Rc<RefCell<CanvasData>>> {
        if let Some(state) = self.surfaces.borrow().get(&node).cloned() {
            return Ok(state);
        }
        let (width, height) = {
            let session = realm.session.borrow();
            canvas_dimensions(session.document(), node)
        };
        let state = Rc::new(RefCell::new(CanvasData::new(width, height)?));
        self.surfaces.borrow_mut().insert(node, state.clone());
        Ok(state)
    }

    /// Called from the permanent DOM mutation router. This updates the surface
    /// but only queues publication: the router still holds the session borrow.
    pub fn on_mutation(
        &self,
        document: &lumen_html::Document,
        mutation: &lumen_html::observe::ObservedMutation,
    ) {
        match &mutation.kind {
            lumen_html::observe::ObservedKind::Attribute {
                name,
                namespace_uri,
                ..
            } if namespace_uri.is_none() => {
                self.reset_for_dimension_attribute(document, mutation.target, name);
            }
            lumen_html::observe::ObservedKind::Attribute { .. } => {}
            lumen_html::observe::ObservedKind::ChildList { added, removed, .. } => {
                if let Some(node) = removed {
                    self.pending_detach.borrow_mut().push(*node);
                }
                if let Some(node) = added {
                    self.queue_attached_subtree(document, *node);
                }
            }
            lumen_html::observe::ObservedKind::ChildListMany { added, removed } => {
                self.pending_detach
                    .borrow_mut()
                    .extend(removed.iter().copied());
                for node in added {
                    self.queue_attached_subtree(document, *node);
                }
            }
            lumen_html::observe::ObservedKind::CharacterData { .. }
            | lumen_html::observe::ObservedKind::ChildListReplacement { .. }
            | lumen_html::observe::ObservedKind::TextSplit { .. }
            | lumen_html::observe::ObservedKind::TextMerge { .. }
            | lumen_html::observe::ObservedKind::SlotAssignment => {}
        }
    }

    fn reset_for_dimension_attribute(
        &self,
        document: &lumen_html::Document,
        target: NodeId,
        name: &str,
    ) {
        if !name.eq_ignore_ascii_case("width") && !name.eq_ignore_ascii_case("height") {
            return;
        }
        let Some(state) = self.surfaces.borrow().get(&target).cloned() else {
            return;
        };
        let mut data = state.borrow_mut();
        if data.transferred {
            return;
        }
        if data.suppress_dimension_mutation {
            data.suppress_dimension_mutation = false;
            return;
        }
        let (width, height) = canvas_dimensions(document, target);
        match data.resize(width, height) {
            Ok(()) => data.pending_error = None,
            Err(error) => {
                data.pending_error = Some(error.to_string());
                let _ = data.resize(0, 0);
            }
        }
        drop(data);
        self.queue_publish(target);
    }

    fn queue_publish(&self, node: NodeId) {
        let mut pending = self.pending_publish.borrow_mut();
        if !pending.contains(&node) {
            pending.push(node);
        }
    }

    fn queue_attached_subtree(&self, document: &lumen_html::Document, root: NodeId) {
        let surfaces = self.surfaces.borrow();
        for node in surfaces.keys().copied() {
            if is_in_subtree(document, root, node) {
                self.queue_publish(node);
            }
        }
    }

    /// Publish mutation-driven resizes once the session's mutation callback has
    /// returned. Embedders call this before building or exposing a new frame.
    pub fn sync(&self, realm: &DomRealm) -> OpResult<()> {
        let mut pending = core::mem::take(&mut *self.pending_publish.borrow_mut());
        // Worker publication crosses only a bounded latest-bitmap mailbox. The
        // document owner adopts it at its normal rendering boundary.
        for (&node, state) in self.surfaces.borrow().iter() {
            let mut data = state.borrow_mut();
            let Some(mailbox) = data.remote_inbound.clone() else { continue; };
            let publication = mailbox.lock().map_err(|_| OpError::new("OperationError", "Canvas publication unavailable"))?;
            if publication.generation != data.remote_generation {
                data.remote_generation = publication.generation;
                data.bitmap_output = publication.image.clone();
                if !pending.contains(&node) { pending.push(node); }
            }
        }
        let detached = core::mem::take(&mut *self.pending_detach.borrow_mut());
        if pending.is_empty() && detached.is_empty() {
            return Ok(());
        }
        let mut session = realm.session.borrow_mut();
        let mut updates = Vec::with_capacity(pending.len());
        let mut errors = Vec::new();
        {
            let document = session.document();
            let mut removed_nodes = Vec::new();
            for root in detached {
                collect_subtree(document, root, &mut removed_nodes);
            }
            let mut surfaces = self.surfaces.borrow_mut();
            for node in removed_nodes {
                if !is_attached(document, node)
                    && surfaces
                        .get(&node)
                        .is_some_and(|state| Rc::strong_count(state) == 1)
                {
                    surfaces.remove(&node);
                    self.pending_publish
                        .borrow_mut()
                        .retain(|pending| *pending != node);
                }
            }
            for node in pending {
                if !is_attached(document, node) {
                    continue;
                }
                let Some(state) = surfaces.get(&node) else {
                    continue;
                };
                let data = state.borrow();
                if let Some(error) = &data.pending_error {
                    errors.push(error.clone());
                }
                updates.push((node, data.snapshot(), data.transferred));
            }
        }
        for (node, image, transferred) in updates {
            if transferred {
                session.document_mut().set_attribute(node, "width", &image.width.to_string()).map_err(super::dom_error)?;
                session.document_mut().set_attribute(node, "height", &image.height.to_string()).map_err(super::dom_error)?;
            }
            session
                .set_node_bitmap(node, image_data(image))
                .map_err(|error| {
                    OpError::new(
                        "InvalidStateError",
                        format!("canvas publication failed: {error:?}"),
                    )
                })?;
        }
        if let Some(error) = errors.into_iter().next() {
            return Err(OpError::new(
                "IndexSizeError",
                format!("canvas resize failed: {error}"),
            ));
        }
        Ok(())
    }
}

fn canvas_dimensions(document: &lumen_html::Document, node: NodeId) -> (u32, u32) {
    let Ok(NodeKind::Element { attributes, .. }) = document.kind(node) else {
        return (DEFAULT_WIDTH, DEFAULT_HEIGHT);
    };
    let attribute = |wanted: &str| {
        attributes
            .iter()
            .find(|(name, _)| name == wanted)
            .map(|(_, value)| value.as_str())
    };
    (
        lumen_html::layout::canvas_dimension(attribute("width"), DEFAULT_WIDTH),
        lumen_html::layout::canvas_dimension(attribute("height"), DEFAULT_HEIGHT),
    )
}

fn drawable_connected(document:&lumen_html::Document,mut node:NodeId)->bool {
    for _ in 0..512 {
        if node==document.root() {return true;}
        let Ok(Some(parent))=document.composed_parent(node) else {return false;};
        node=parent;
    }
    false
}

fn is_attached(document: &lumen_html::Document, mut node: NodeId) -> bool {
    let root = document.root();
    loop {
        if node == root {
            return true;
        }
        let Ok(Some(parent)) = document.parent(node) else {
            return false;
        };
        node = parent;
    }
}

fn is_in_subtree(document: &lumen_html::Document, root: NodeId, target: NodeId) -> bool {
    let mut current = target;
    loop {
        if current == root {
            return true;
        }
        let Ok(Some(parent)) = document.parent(current) else {
            return false;
        };
        current = parent;
    }
}

fn collect_subtree(document: &lumen_html::Document, root: NodeId, output: &mut Vec<NodeId>) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        output.push(node);
        let Ok(mut child) = document.first_child(node) else {
            continue;
        };
        while let Some(id) = child {
            stack.push(id);
            child = document.next_sibling(id).ok().flatten();
        }
    }
}

fn check_dimensions(width: u32, height: u32) -> OpResult<()> {
    // Empty bitmaps carry dimensions but allocate no pixels. For non-empty
    // surfaces, bound the actual pixel storage rather than imposing an
    // independent per-axis limit.
    if width == 0 || height == 0 {
        return Ok(());
    }
    let bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| OpError::new("IndexSizeError", "canvas bitmap dimensions are too large"))?;
    if bytes > MAX_IMAGE_DATA_BYTES as u64 {
        return Err(OpError::new(
            "IndexSizeError",
            "canvas bitmap dimensions are too large",
        ));
    }
    Ok(())
}

fn offscreen_surface(width: u64, height: u64) -> OpResult<(CanvasSurface, bool)> {
    let empty_surface = || {
        CanvasSurface::new(0, 0)
            .map_err(|_| OpError::new("IndexSizeError", "canvas bitmap dimensions are too large"))
    };
    let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
        return Ok((empty_surface()?, false));
    };
    if check_dimensions(width, height).is_err() {
        return Ok((empty_surface()?, false));
    }
    match CanvasSurface::new(width, height) {
        Ok(surface) => Ok((surface, true)),
        Err(_) => Ok((empty_surface()?, false)),
    }
}

fn make_image_opaque(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        for channel in 0..3 {
            pixel[channel] = ((u16::from(pixel[channel]) * u16::from(pixel[3]) + 127) / 255) as u8;
        }
        pixel[3] = 255;
    }
}

pub(crate) fn webidl_unsigned_long(ctx: &mut Ctx, value: &Value) -> OpResult<u32> {
    let number = ctx.coerce_number(value).map_err(OpError::thrown)?;
    if !number.is_finite() || number == 0.0 {
        return Ok(0);
    }
    Ok(number.trunc().rem_euclid(WEBIDL_UINT32_MODULUS) as u32)
}

fn webidl_enforce_range_unsigned_long_long(ctx: &mut Ctx, value: &Value) -> OpResult<u64> {
    let number = ctx.coerce_number(value).map_err(OpError::thrown)?;
    let integer = number.trunc();
    if !integer.is_finite() || integer < 0.0 || integer >= WEBIDL_UINT64_LIMIT {
        return Err(OpError::new(
            "TypeError",
            "canvas dimension is outside the unsigned 64-bit range",
        ));
    }
    Ok(integer as u64)
}

fn canvas_long(ctx: &mut Ctx, value: &Value) -> OpResult<i32> {
    let number = ctx.coerce_number(value).map_err(OpError::thrown)?;
    let integer = number.trunc();
    if !integer.is_finite() || integer < i32::MIN as f64 || integer > i32::MAX as f64 {
        return Err(OpError::new(
            "TypeError",
            "canvas image-data dimension is outside the signed 32-bit range",
        ));
    }
    Ok(integer as i32)
}

fn normalized_dirty_rect(
    image_width: u32,
    image_height: u32,
    dirty_x: i32,
    dirty_y: i32,
    dirty_width: i32,
    dirty_height: i32,
) -> Option<(u32, u32, u32, u32)> {
    let (mut x, mut y, mut width, mut height) = (
        i64::from(dirty_x),
        i64::from(dirty_y),
        i64::from(dirty_width),
        i64::from(dirty_height),
    );
    if width < 0 {
        x += width;
        width = -width;
    }
    if height < 0 {
        y += height;
        height = -height;
    }
    if x < 0 {
        width += x;
        x = 0;
    }
    if y < 0 {
        height += y;
        y = 0;
    }
    if x + width > i64::from(image_width) {
        width = i64::from(image_width) - x;
    }
    if y + height > i64::from(image_height) {
        height = i64::from(image_height) - y;
    }
    if width <= 0 || height <= 0 {
        return None;
    }
    Some((
        u32::try_from(x).ok()?,
        u32::try_from(y).ok()?,
        u32::try_from(width).ok()?,
        u32::try_from(height).ok()?,
    ))
}

fn zeroed_bytes(length: usize, message: &'static str) -> OpResult<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| OpError::new("IndexSizeError", message))?;
    bytes.resize(length, 0);
    Ok(bytes)
}

fn copy_bytes(source: &[u8], message: &'static str) -> OpResult<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(source.len())
        .map_err(|_| OpError::new("IndexSizeError", message))?;
    bytes.extend_from_slice(source);
    Ok(bytes)
}

fn clone_rgba_image(image: &Rgba8Image) -> OpResult<Rgba8Image> {
    Ok(Rgba8Image {
        width: image.width,
        height: image.height,
        pixels: copy_bytes(&image.pixels, "ImageData bitmap allocation failed")?,
    })
}

fn image_data(image: Rgba8Image) -> Option<Arc<lumen_html::paint::ImageData>> {
    (image.width != 0 && image.height != 0).then(|| {
        Arc::new(lumen_html::paint::ImageData {
            width: image.width,
            height: image.height,
            pixels: image.pixels,
        })
    })
}

fn publish(realm: Option<&Rc<DomRealm>>, node: Option<NodeId>, data: &CanvasData) -> OpResult<()> {
    if let Some(mailbox) = &data.remote_outbound {
        let image = data.snapshot();
        let mut publication = mailbox.lock().map_err(|_| OpError::new("OperationError", "Canvas publication unavailable"))?;
        publication.image = Some(image);
        publication.generation = publication.generation.wrapping_add(1);
    }
    let (Some(realm), Some(node)) = (realm, node) else {
        return Ok(());
    };
    if data.transferred {
        realm.canvases.queue_publish(node);
        return Ok(());
    }
    let attached = {
        let session = realm.session.borrow();
        is_attached(session.document(), node)
    };
    if !attached {
        return Ok(());
    }
    let image = image_data(data.snapshot());
    realm
        .session
        .borrow_mut()
        .set_node_bitmap(node, image)
        .map_err(|error| {
            OpError::new(
                "InvalidStateError",
                format!("canvas publication failed: {error:?}"),
            )
        })
}

#[derive(Clone, Default)]
struct CanvasPath {
    commands: Vec<PathCommand>,
}

#[derive(Clone)]
enum PathCommand {
    Move(f32, f32),
    Line(f32, f32),
    Quad(f32, f32, f32, f32),
    Cubic(f32, f32, f32, f32, f32, f32),
    Rect(f32, f32, f32, f32),
    Arc {
        cx: f32,
        cy: f32,
        rx: f32,
        ry: f32,
        rotation: f32,
        start: f32,
        end: f32,
        counterclockwise: bool,
    },
    SharedSvg(ParsedSvgPath),
    Close,
}

impl CanvasPath {
    fn build(&self, transform: Transform) -> Option<Path> {
        self.build_parsed(transform).path
    }
    fn build_parsed(&self, transform: Transform) -> ParsedSvgPath {
        let mut builder = PathBuilder::new();
        let (mut has_subpath, mut current, mut subpath_start) =
            (false, (0.0f32, 0.0f32), (0.0f32, 0.0f32));
        for command in &self.commands {
            match *command {
                PathCommand::Move(x, y) => {
                    builder.move_to(x, y);
                    has_subpath = true;
                    current = (x, y);
                    subpath_start = current;
                }
                PathCommand::Line(x, y) => {
                    if !has_subpath {
                        has_subpath = true;
                        current = (0.0, 0.0);
                        subpath_start = current;
                    }
                    builder.line_to(x, y);
                    current = (x, y);
                }
                PathCommand::Quad(x1, y1, x, y) => {
                    if !has_subpath {
                        has_subpath = true;
                        current = (0.0, 0.0);
                        subpath_start = current;
                    }
                    builder.quad_to(x1, y1, x, y);
                    current = (x, y);
                }
                PathCommand::Cubic(x1, y1, x2, y2, x, y) => {
                    if !has_subpath {
                        has_subpath = true;
                        current = (0.0, 0.0);
                        subpath_start = current;
                    }
                    builder.cubic_to(x1, y1, x2, y2, x, y);
                    current = (x, y);
                }
                PathCommand::Rect(x, y, width, height) => {
                    if let Some(rect) = Rect::from_xywh(x, y, width, height) {
                        builder.push_rect(rect);
                        has_subpath = true;
                        current = (x, y);
                        subpath_start = current;
                    }
                }
                PathCommand::Arc {
                    cx,
                    cy,
                    rx,
                    ry,
                    rotation,
                    start,
                    end,
                    counterclockwise,
                } => {
                    current = append_ellipse_arc(
                        &mut builder,
                        has_subpath,
                        current,
                        (cx, cy),
                        (rx, ry),
                        rotation,
                        start,
                        end,
                        counterclockwise,
                    );
                    if !has_subpath {
                        subpath_start = ellipse_point((cx, cy), (rx, ry), rotation, start);
                    }
                    has_subpath = true;
                }
                PathCommand::SharedSvg(ref parsed) => {
                    if let Some(path) = &parsed.path {
                        builder.push_path(path);
                    } else {
                        builder.move_to(parsed.current.0, parsed.current.1);
                    }
                    has_subpath = true;
                    current = parsed.current;
                    subpath_start = parsed.subpath_start;
                }
                PathCommand::Close => {
                    builder.close();
                    if has_subpath {
                        current = subpath_start;
                    }
                }
            }
        }
        let mut current = tiny_skia::Point::from_xy(current.0, current.1);
        let mut subpath = tiny_skia::Point::from_xy(subpath_start.0, subpath_start.1);
        transform.map_point(&mut current);
        transform.map_point(&mut subpath);
        ParsedSvgPath {
            path: builder.finish().and_then(|path| path.transform(transform)),
            current: (current.x, current.y),
            subpath_start: (subpath.x, subpath.y),
        }
    }
}

#[lumen_bind::class(name = "Path2D", hint(js(webidl)))]
struct DomPath2D {
    path: RefCell<CanvasPath>,
}

#[lumen_bind::methods]
impl DomPath2D {
    #[constructor]
    fn new(ctx: &mut Ctx, source: Option<Value>) -> OpResult<Self> {
        let path = match source {
            None | Some(Value::Undefined) => CanvasPath::default(),
            Some(Value::Str(data)) => {
                let parsed = lumen_html_image::canvas::parse_svg_path(data.as_str())
                    .map_err(|error| OpError::new("SyntaxError", error))?;
                CanvasPath {
                    commands: vec![PathCommand::SharedSvg(parsed)],
                }
            }
            Some(source) => ctx
                .with_instance::<DomPath2D, _>(&source, |path| path.path.borrow().clone())
                .map_err(|_| OpError::new("TypeError", "Path2D source must be a Path2D"))?,
        };
        Ok(Self {
            path: RefCell::new(path),
        })
    }
    fn close_path(&self) {
        self.path.borrow_mut().commands.push(PathCommand::Close);
    }
    fn move_to(&self, x: f64, y: f64) {
        self.path
            .borrow_mut()
            .commands
            .push(PathCommand::Move(x as f32, y as f32));
    }
    fn line_to(&self, x: f64, y: f64) {
        self.path
            .borrow_mut()
            .commands
            .push(PathCommand::Line(x as f32, y as f32));
    }
    fn quadratic_curve_to(&self, cpx: f64, cpy: f64, x: f64, y: f64) {
        self.path.borrow_mut().commands.push(PathCommand::Quad(
            cpx as f32, cpy as f32, x as f32, y as f32,
        ));
    }
    fn bezier_curve_to(&self, cp1x: f64, cp1y: f64, cp2x: f64, cp2y: f64, x: f64, y: f64) {
        self.path.borrow_mut().commands.push(PathCommand::Cubic(
            cp1x as f32,
            cp1y as f32,
            cp2x as f32,
            cp2y as f32,
            x as f32,
            y as f32,
        ));
    }
    fn rect(&self, x: f64, y: f64, width: f64, height: f64) {
        self.path.borrow_mut().commands.push(PathCommand::Rect(
            x as f32,
            y as f32,
            width as f32,
            height as f32,
        ));
    }
    fn arc(
        &self,
        x: f64,
        y: f64,
        radius: f64,
        start_angle: f64,
        end_angle: f64,
        counterclockwise: Option<bool>,
    ) -> OpResult<()> {
        if radius < 0.0 {
            return Err(OpError::new(
                "IndexSizeError",
                "arc radius must be non-negative",
            ));
        }
        let values = [x, y, radius, start_angle, end_angle];
        if !values.iter().all(|value| value.is_finite())
            || values.iter().any(|value| value.abs() > f64::from(f32::MAX))
        {
            return Ok(());
        }
        self.path.borrow_mut().commands.push(PathCommand::Arc {
            cx: x as f32,
            cy: y as f32,
            rx: radius as f32,
            ry: radius as f32,
            rotation: 0.0,
            start: start_angle as f32,
            end: end_angle as f32,
            counterclockwise: counterclockwise.unwrap_or(false),
        });
        Ok(())
    }
    fn ellipse(
        &self,
        x: f64,
        y: f64,
        radius_x: f64,
        radius_y: f64,
        rotation: f64,
        start_angle: f64,
        end_angle: f64,
        counterclockwise: Option<bool>,
    ) -> OpResult<()> {
        if radius_x < 0.0 || radius_y < 0.0 {
            return Err(OpError::new(
                "IndexSizeError",
                "ellipse radii must be non-negative",
            ));
        }
        let values = [x, y, radius_x, radius_y, rotation, start_angle, end_angle];
        if !values.iter().all(|value| value.is_finite())
            || values.iter().any(|value| value.abs() > f64::from(f32::MAX))
        {
            return Ok(());
        }
        self.path.borrow_mut().commands.push(PathCommand::Arc {
            cx: x as f32,
            cy: y as f32,
            rx: radius_x as f32,
            ry: radius_y as f32,
            rotation: rotation as f32,
            start: start_angle as f32,
            end: end_angle as f32,
            counterclockwise: counterclockwise.unwrap_or(false),
        });
        Ok(())
    }
    #[method(name = "addPath")]
    fn add_path(&self, ctx: &mut Ctx, source: Value, transform: Option<Value>) -> OpResult<()> {
        let source = ctx
            .with_instance::<DomPath2D, _>(&source, |path| path.path.borrow().clone())
            .map_err(|_| OpError::new("TypeError", "addPath expects a Path2D"))?;
        if source.commands.is_empty() {
            return Ok(());
        }
        let matrix = match transform {
            Some(transform) => parse_dom_matrix(ctx, &transform)?,
            None => Transform::identity(),
        };
        let parsed = source.build_parsed(matrix);
        self.path
            .borrow_mut()
            .commands
            .push(PathCommand::SharedSvg(parsed));
        Ok(())
    }
}

fn ellipse_point(center: (f32, f32), radii: (f32, f32), rotation: f32, angle: f32) -> (f32, f32) {
    let (sin_rotation, cos_rotation) = rotation.sin_cos();
    let (sin_angle, cos_angle) = angle.sin_cos();
    (
        center.0 + radii.0 * cos_angle * cos_rotation - radii.1 * sin_angle * sin_rotation,
        center.1 + radii.0 * cos_angle * sin_rotation + radii.1 * sin_angle * cos_rotation,
    )
}

fn arc_sweep(start: f32, end: f32, counterclockwise: bool) -> f32 {
    let tau = core::f64::consts::TAU;
    let raw = f64::from(end) - f64::from(start);
    let sweep = if counterclockwise {
        if raw <= -tau {
            -tau
        } else if raw > 0.0 {
            let remainder = raw.rem_euclid(tau);
            if remainder == 0.0 {
                0.0
            } else {
                remainder - tau
            }
        } else {
            raw
        }
    } else if raw >= tau {
        tau
    } else if raw <= 0.0 {
        raw.rem_euclid(tau)
    } else {
        raw
    };
    sweep as f32
}

fn append_ellipse_arc(
    builder: &mut PathBuilder,
    has_subpath: bool,
    current: (f32, f32),
    center: (f32, f32),
    radii: (f32, f32),
    rotation: f32,
    start: f32,
    end: f32,
    counterclockwise: bool,
) -> (f32, f32) {
    let sweep = arc_sweep(start, end, counterclockwise);
    let start_point = ellipse_point(center, radii, rotation, start);
    if has_subpath {
        if (current.0 - start_point.0).abs() > f32::EPSILON
            || (current.1 - start_point.1).abs() > f32::EPSILON
        {
            builder.line_to(start_point.0, start_point.1);
        }
    } else {
        builder.move_to(start_point.0, start_point.1);
    }
    if sweep == 0.0 {
        return start_point;
    }
    let count = (sweep.abs() / core::f32::consts::FRAC_PI_2).ceil() as usize;
    let step = sweep / count as f32;
    let (sin_rotation, cos_rotation) = rotation.sin_cos();
    for segment in 0..count {
        let a0 = start + step * segment as f32;
        let a1 = a0 + step;
        let (sin0, cos0) = a0.sin_cos();
        let (sin1, cos1) = a1.sin_cos();
        let p0 = ellipse_point(center, radii, rotation, a0);
        let p1 = ellipse_point(center, radii, rotation, a1);
        let d0 = (
            -radii.0 * sin0 * cos_rotation - radii.1 * cos0 * sin_rotation,
            -radii.0 * sin0 * sin_rotation + radii.1 * cos0 * cos_rotation,
        );
        let d1 = (
            -radii.0 * sin1 * cos_rotation - radii.1 * cos1 * sin_rotation,
            -radii.0 * sin1 * sin_rotation + radii.1 * cos1 * cos_rotation,
        );
        let tangent = (4.0 / 3.0) * (step / 4.0).tan();
        builder.cubic_to(
            p0.0 + d0.0 * tangent,
            p0.1 + d0.1 * tangent,
            p1.0 - d1.0 * tangent,
            p1.1 - d1.1 * tangent,
            p1.0,
            p1.1,
        );
    }
    ellipse_point(center, radii, rotation, start + sweep)
}

#[lumen_bind::class(name = "CanvasPaintEvent", extends = super::events::DomEvent, hint(js(webidl)))]
pub(crate) struct DomCanvasPaintEvent { base: super::events::DomEvent, elements_slot: String }
struct CanvasPaintEventConstructor { event: DomCanvasPaintEvent, elements: Vec<Value> }
impl lumen_bind::CtorRet<lumen::embed::JsHost,DomCanvasPaintEvent> for CanvasPaintEventConstructor {
    fn into_ctor(self, cx: &<lumen::embed::JsHost as lumen_bind::Host>::Cx<'_>) -> Result<Value,Value> {
        let slot=self.event.elements_slot.clone();
        let instance = <lumen::embed::JsHost as lumen_bind::Host>::construct(cx,self.event)?;
        <lumen::embed::JsHost as lumen_bind::Host>::with_ctx(cx,|ctx:&mut Ctx| {
            let array = <lumen::embed::JsHost as lumen_bind::Host>::from_list(ctx,self.elements);
            ctx.freeze_native_object(&array);
            ctx.define_native_private_value_slot(&instance,&slot,array)?;
            Ok(())
        })?;
        Ok(instance)
    }
}
#[lumen_bind::methods]
impl DomCanvasPaintEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<CanvasPaintEventConstructor> {
        let base = super::events::DomEvent::new(ctx,kind,options.clone())?;
        let mut elements=Vec::new();
        if let Some(options)=options.filter(|value|matches!(value,Value::Obj(_))) {
            let value=ctx.member_get(&options,"changedElements").map_err(OpError::thrown)?;
            if !matches!(value,Value::Undefined|Value::Null) {
                if !ctx.is_array_value(&value).map_err(OpError::thrown)? {return Err(OpError::type_error("changedElements must be a sequence"));}
                let length=ctx.member_get(&value,"length").map_err(OpError::thrown)?;
                let Value::Num(length)=length else {return Err(OpError::type_error("Invalid changedElements length"));};
                if length<0.0 || length>128.0 || !length.is_finite() {return Err(OpError::new("QuotaExceededError","changedElements limit"));}
                for index in 0..length as usize {
                    let element=ctx.member_get(&value,&index.to_string()).map_err(OpError::thrown)?;
                    ctx.with_instance::<super::DomElement,_>(&element,|_|())?;
                    elements.push(element);
                }
            }
        }
        let elements_slot=ctx.allocate_native_private_slot_name();
        Ok(CanvasPaintEventConstructor{event:Self{base,elements_slot},elements})
    }
    #[getter(name="changedElements")]
    fn changed_elements(&self,ctx:&mut Ctx,this:lumen_bind::This<Value>)->OpResult<Value> {
        Ok(ctx.native_private_value_slot(&this.0,&self.elements_slot).unwrap_or(Value::Undefined))
    }
}

pub(crate) fn paint_event(ctx:&mut Ctx,elements:Vec<Value>)->OpResult<Value> {
    let base=super::events::DomEvent::new(ctx,"paint",None)?;
    let elements_slot=ctx.allocate_native_private_slot_name();
    let slot=elements_slot.clone();
    let event=ctx.new_instance(DomCanvasPaintEvent{base,elements_slot});
    let array=<lumen::embed::JsHost as lumen_bind::Host>::from_list(ctx,elements);
    ctx.freeze_native_object(&array);
    ctx.define_native_private_value_slot(&event,&slot,array).map_err(OpError::thrown)?;
    Ok(event)
}

#[lumen_bind::class(name = "HTMLCanvasElement", extends = super::DomHtmlElement, hint(js(webidl)))]
pub struct DomCanvasElement {
    base: super::DomHtmlElement,
}

impl DomCanvasElement {
    pub fn from_node(base: super::DomHtmlElement) -> Self {
        Self { base }
    }
    fn data(&self) -> OpResult<Rc<RefCell<CanvasData>>> {
        self.base
            .base
            .base
            .realm
            .canvases
            .data_for(&self.base.base.base.realm, self.base.base.base.id)
    }
}

#[lumen_bind::methods]
impl DomCanvasElement {
    #[constructor]
    fn new(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<crate::custom_elements::HtmlElementCtor> {
        crate::custom_elements::construct_customized_class::<Self>(ctx, this.0)
    }
    #[getter]
    fn content(&self) -> String {
        let realm=&self.base.base.base.realm;
        if realm.session.borrow().document().get_attribute_ns(self.base.base.base.id,None,"content").ok().flatten().as_deref()==Some("drawable") {"drawable".into()} else {"fallback".into()}
    }

    #[setter]
    fn set_content(&self, value: String) -> OpResult<()> {
        self.base.base.base.set_attribute_core("content",&value)?;
        if value=="drawable" {self.request_paint()?;}
        Ok(())
    }

    #[method(name = "requestPaint")]
    fn request_paint(&self) -> OpResult<()> {
        self.base.base.base.realm.canvases.request_paint(self.base.base.base.id)
    }

    #[method(name = "captureElementImage")]
    fn capture_element_image(&self, ctx: &mut Ctx, target: Value) -> OpResult<Value> {
        let (realm,node)=ctx.with_instance::<super::DomElement,_>(&target,|element|(element.base.realm.clone(),element.base.id))?;
        let owner=&self.base.base.base;
        if !Rc::ptr_eq(&realm,&owner.realm) {return Err(OpError::new("InvalidStateError","Element belongs to another document"));}
        let image=realm.canvases.drawable_snapshots.borrow().iter()
            .find(|(canvas,element,_)|*canvas==owner.id && *element==node)
            .map(|(_,_,image)|image.clone())
            .ok_or_else(||OpError::new("InvalidStateError","Element has no published drawable snapshot for this canvas"))?;
        Ok(ctx.new_instance(DomElementImage {image:RefCell::new(Some(element_image_pixels(image)?)),origin_clean:true}))
    }

    #[getter(name = "onpaint")]
    fn onpaint(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        super::event_content_handlers::element_handler_value(ctx, &self.base.base.base.realm, self.base.base.base.id, &this.0, "paint")
    }

    #[setter(name = "onpaint")]
    fn set_onpaint(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>, handler: super::events::EventHandler) -> OpResult<()> {
        super::event_content_handlers::set_element_handler(ctx, &self.base.base.base.realm, self.base.base.base.id, &this.0, "paint", handler);
        self.request_paint()
    }

    #[getter]
    fn width(&self) -> u32 {
        canvas_idl_dimension(&self.base.base.base, "width", DEFAULT_WIDTH)
    }

    #[setter]
    fn set_width(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> {
        let value = webidl_unsigned_long(ctx, &value)?;
        let data = self.data()?;
        if data.borrow().transferred { return Err(OpError::new("InvalidStateError", "canvas control was transferred")); }
        set_dom_dimension(ctx, &self.base.base.base, &data, "width", value)
    }

    #[getter]
    fn height(&self) -> u32 {
        canvas_idl_dimension(&self.base.base.base, "height", DEFAULT_HEIGHT)
    }

    fn capture_stream(&self, ctx: &mut Ctx, frame_rate: Option<f64>) -> OpResult<Value> {
        let data = self.data()?;
        if !data.borrow().origin_clean {
            return Err(OpError::new(
                "SecurityError",
                "a canvas with cross-origin pixels cannot be captured",
            ));
        }
        let source = Rc::new(move || {
            ensure_origin_clean(&data.borrow())?;
            let image = CanvasData::snapshot_for_read(&data)?;
            Ok(super::VideoFrameSnapshot {
                presentation_time_micros: 0,
                width: image.width,
                height: image.height,
                rgba: Arc::new(image.pixels),
            })
        });
        crate::media_capture::canvas_capture_stream(
            ctx,
            source,
            frame_rate,
            &self.base.base.base.realm,
        )
    }

    #[setter]
    fn set_height(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> {
        let value = webidl_unsigned_long(ctx, &value)?;
        let data = self.data()?;
        if data.borrow().transferred { return Err(OpError::new("InvalidStateError", "canvas control was transferred")); }
        set_dom_dimension(ctx, &self.base.base.base, &data, "height", value)
    }

    fn transfer_control_to_offscreen(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let data = self.data()?;
        let node = &self.base.base.base;
        let inherited_direction_rtl = canvas_inherited_direction(Some(&node.realm), Some(node.id));
        {
            let mut state = data.borrow_mut();
            if state.transferred || state.context_mode.is_some() {
                return Err(OpError::new("InvalidStateError", "canvas already has a context or transferred control"));
            }
            state.transferred = true;
            state.inherited_direction_rtl = inherited_direction_rtl;
        }
        Ok(ctx.new_instance(DomOffscreenCanvas { data, realm:Some(node.realm.clone()), placeholder:Some(node.id), detached: Cell::new(false) }))
    }

    fn get_context(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        context_id: &str,
        options: Option<Value>,
    ) -> OpResult<Value> {
        if context_id != "2d"
            && context_id != "bitmaprenderer"
            && context_id != "webgpu"
            && context_id != "webgl"
            && context_id != "webgl2"
        {
            return Ok(Value::Null);
        }
        let data = self.data()?;
        if data.borrow().transferred { return Err(OpError::new("InvalidStateError", "canvas control was transferred")); }
        context_value(
            ctx,
            data,
            Some(self.base.base.base.realm.clone()),
            Some(self.base.base.base.id),
            Some(this.0),
            context_id,
            options,
        )
    }

    #[method(name = "toDataURL")]
    fn to_data_url(
        &self,
        ctx: &mut Ctx,
        image_type: Option<&str>,
        quality: Option<Value>,
    ) -> OpResult<String> {
        let data = self.data()?;
        ensure_origin_clean(&data.borrow())?;
        let image = CanvasData::snapshot_for_read(&data)?;
        if image.width == 0 || image.height == 0 {
            return Ok("data:,".to_owned());
        }
        let quality = canvas_export_quality(ctx, quality)?;
        let (bytes, mime) = lumen_html_image::encode_canvas_image(&image, image_type, quality)
            .map_err(|_| OpError::new("EncodingError", "canvas image encoding failed"))?;
        let encoded = lumen_common::codec::base64_encode(&bytes, false, true);
        Ok(format!("data:{mime};base64,{encoded}"))
    }

    #[method(name = "toBlob")]
    fn to_blob(
        &self,
        ctx: &mut Ctx,
        callback: Value,
        image_type: Option<&str>,
        quality: Option<Value>,
    ) -> OpResult<Value> {
        if JsFunction::from_value(callback.clone()).is_none() {
            return Err(OpError::new(
                "TypeError",
                "toBlob callback must be callable",
            ));
        }
        let data = self.data()?;
        ensure_origin_clean(&data.borrow())?;
        let image = CanvasData::snapshot_for_read(&data)?;
        if image.width == 0 || image.height == 0 {
            queue_canvas_blob_callback(ctx, callback, Value::Null)?;
            return Ok(Value::Undefined);
        }
        let quality = canvas_export_quality(ctx, quality)?;
        let blob = canvas_blob(ctx, &image, image_type, quality)?;
        queue_canvas_blob_callback(ctx, callback, blob)?;
        Ok(Value::Undefined)
    }
}

fn queue_canvas_blob_callback(ctx: &mut Ctx, callback: Value, blob: Value) -> OpResult<()> {
    super::scheduling::queue_task(ctx, move |ctx| {
        let callback = JsFunction::from_value(callback)
            .ok_or_else(|| OpError::new("TypeError", "toBlob callback must be callable"))?;
        callback.call(ctx, Value::Undefined, &[blob])?;
        Ok(())
    })
}

fn canvas_export_quality(ctx: &mut Ctx, quality: Option<Value>) -> OpResult<Option<f64>> {
    let Some(quality) = quality.filter(|value| !matches!(value, Value::Undefined)) else {
        return Ok(None);
    };
    let quality = ctx.coerce_number(&quality).map_err(OpError::thrown)?;
    Ok((quality.is_finite() && (0.0..=1.0).contains(&quality)).then_some(quality))
}

fn canvas_blob(
    ctx: &mut Ctx,
    image: &Rgba8Image,
    image_type: Option<&str>,
    quality: Option<f64>,
) -> OpResult<Value> {
    let (bytes, mime) = lumen_html_image::encode_canvas_image(image, image_type, quality)
        .map_err(|_| OpError::new("EncodingError", "canvas image encoding failed"))?;
    Ok(lumen_host::blob::new_blob(ctx, bytes, &mime))
}

fn canvas_idl_dimension(node: &DomNode, name: &str, fallback: u32) -> u32 {
    canvas_dimension_at(&node.realm, node.id, name, fallback)
}

fn canvas_dimension_at(realm: &Rc<DomRealm>, id: NodeId, name: &str, fallback: u32) -> u32 {
    let session = realm.session.borrow();
    let raw = match session.document().kind(id) {
        Ok(NodeKind::Element { attributes, .. }) => attributes
            .iter()
            .find(|(attribute, _)| attribute == name)
            .map(|(_, value)| value.as_str()),
        _ => None,
    };
    lumen_html::layout::canvas_dimension(raw, fallback)
}

fn set_dom_dimension(
    ctx: &mut Ctx,
    node: &DomNode,
    data: &Rc<RefCell<CanvasData>>,
    name: &str,
    value: u32,
) -> OpResult<()> {
    let width = if name == "width" {
        value
    } else {
        node_width(node)
    };
    let height = if name == "height" {
        value
    } else {
        node_height(node)
    };
    check_dimensions(width, height)?;
    // Allocate before mutating the reflected attribute so allocation failures
    // do not leave the DOM value changed with a stale bitmap.
    data.borrow_mut().resize(width, height)?;
    {
        data.borrow_mut().suppress_dimension_mutation = true;
    }
    let attr_result = node.set_attribute(ctx, name, &value.to_string());
    data.borrow_mut().suppress_dimension_mutation = false;
    attr_result?;
    publish(Some(&node.realm), Some(node.id), &data.borrow())
}

fn node_width(node: &DomNode) -> u32 {
    canvas_idl_dimension(node, "width", DEFAULT_WIDTH)
}
fn node_height(node: &DomNode) -> u32 {
    canvas_idl_dimension(node, "height", DEFAULT_HEIGHT)
}

#[lumen_bind::class(name = "OffscreenCanvas", hint(js(webidl)))]
pub struct DomOffscreenCanvas {
    data: Rc<RefCell<CanvasData>>,
    realm: Option<Rc<DomRealm>>,
    placeholder: Option<NodeId>,
    detached: Cell<bool>,
}

impl DomOffscreenCanvas {
    fn ensure_attached(&self) -> OpResult<()> {
        if self.detached.get() { Err(OpError::new("InvalidStateError", "OffscreenCanvas is detached")) } else { Ok(()) }
    }
}

#[lumen_bind::methods]
impl DomOffscreenCanvas {
    #[constructor]
    fn new(ctx: &mut Ctx, width: Value, height: Value) -> OpResult<Self> {
        let width = webidl_enforce_range_unsigned_long_long(ctx, &width)?;
        let height = webidl_enforce_range_unsigned_long_long(ctx, &height)?;
        let realm = window_globals::current_dom_realm(ctx);
        let mut data = CanvasData::new_offscreen(width, height)?;
        data.inherited_direction_rtl = canvas_inherited_direction(realm.as_ref(), None);
        Ok(Self {
            data: Rc::new(RefCell::new(data)),
            realm,
            placeholder: None,
            detached: Cell::new(false),
        })
    }

    #[getter]
    fn width(&self) -> f64 {
        if self.detached.get() { 0.0 } else { self.data.borrow().logical_width as f64 }
    }
    #[setter]
    fn set_width(&self, ctx: &mut Ctx, width: Value) -> OpResult<()> {
        let width = webidl_enforce_range_unsigned_long_long(ctx, &width)?;
        self.ensure_attached()?;
        let height = self.data.borrow().logical_height;
        resize_offscreen(&self.data, width, height)?;
        publish(self.realm.as_ref(), self.placeholder, &self.data.borrow())
    }
    #[getter]
    fn height(&self) -> f64 {
        if self.detached.get() { 0.0 } else { self.data.borrow().logical_height as f64 }
    }
    #[setter]
    fn set_height(&self, ctx: &mut Ctx, height: Value) -> OpResult<()> {
        let height = webidl_enforce_range_unsigned_long_long(ctx, &height)?;
        self.ensure_attached()?;
        let width = self.data.borrow().logical_width;
        resize_offscreen(&self.data, width, height)?;
        publish(self.realm.as_ref(), self.placeholder, &self.data.borrow())
    }

    fn get_context(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        context_id: &str,
        options: Option<Value>,
    ) -> OpResult<Value> {
        self.ensure_attached()?;
        if context_id != "2d"
            && context_id != "bitmaprenderer"
            && context_id != "webgpu"
            && context_id != "webgl"
            && context_id != "webgl2"
        {
            return Err(OpError::new(
                "TypeError",
                "OffscreenCanvas context identifier is unsupported",
            ));
        }
        let wrapper = this.0;
        context_value(
            ctx,
            self.data.clone(),
            self.realm.clone(),
            self.placeholder,
            Some(wrapper),
            context_id,
            options,
        )
    }

    #[method(name = "transferToImageBitmap")]
    fn transfer_to_image_bitmap(&self, ctx: &mut Ctx) -> OpResult<Value> {
        self.ensure_attached()?;
        let image = CanvasData::snapshot_for_read(&self.data)?;
        let (image, origin_clean) = {
            let mut data = self.data.borrow_mut();
            data.bitmap_output = None;
            data.surface.clear_bitmap();
            data.gpu_generation = data.gpu_generation.wrapping_add(1);
            let origin_clean = data.origin_clean;
            data.origin_clean = true;
            (image, origin_clean)
        };
        new_image_bitmap_with_origin(ctx, image, true, origin_clean)
    }

    #[method(name = "convertToBlob")]
    fn convert_to_blob(&self, ctx: &mut Ctx, options: Option<Value>) -> OpResult<Value> {
        let deferred = Deferred::new(ctx);
        let promise = deferred.promise();
        if let Err(error) = self.ensure_attached().and_then(|_| ensure_origin_clean(&self.data.borrow())) {
            deferred.reject(ctx, error);
            return Ok(promise);
        }
        let image_type = options
            .as_ref()
            .and_then(|value| ctx.get_member(value, "type").ok())
            .and_then(|value| match value {
                Value::Str(value) => Some(value.to_string()),
                _ => None,
            });
        let image = CanvasData::snapshot_for_read(&self.data)?;
        if image.width == 0 || image.height == 0 {
            return Err(OpError::new(
                "IndexSizeError",
                "canvas bitmap has zero dimensions",
            ));
        }
        let quality = options
            .as_ref()
            .map(|value| ctx.get_member(value, "quality"))
            .transpose()
            .map_err(|_| OpError::new("TypeError", "convertToBlob quality getter failed"))?;
        let quality = canvas_export_quality(ctx, quality)?;
        let blob = canvas_blob(ctx, &image, image_type.as_deref(), quality)?;
        let promise = ctx
            .get_member(&ctx.global_object(), "Promise")
            .map_err(|_| OpError::new("Error", "Promise is unavailable"))?;
        let resolve = ctx
            .get_member(&promise, "resolve")
            .map_err(|_| OpError::new("Error", "Promise.resolve is unavailable"))?;
        JsFunction::from_value(resolve)
            .ok_or_else(|| OpError::new("Error", "Promise.resolve is unavailable"))?
            .call(ctx, promise, &[blob])
    }
}

fn resize_offscreen(data: &Rc<RefCell<CanvasData>>, width: u64, height: u64) -> OpResult<()> {
    data.borrow_mut().resize_offscreen(width, height)
}

fn context_value(
    ctx: &mut Ctx,
    data: Rc<RefCell<CanvasData>>,
    realm: Option<Rc<DomRealm>>,
    node: Option<NodeId>,
    canvas_wrapper: Option<Value>,
    context_id: &str,
    options: Option<Value>,
) -> OpResult<Value> {
    if data
        .borrow()
        .context_mode
        .is_some_and(|mode| mode != context_id)
    {
        return Ok(Value::Null);
    }
    if !data.borrow().bitmap_available && context_id != "2d" {
        return Err(OpError::new(
            "NotSupportedError",
            "canvas bitmap exceeds the available backend resource limit",
        ));
    }
    if let Some(value) = data
        .borrow()
        .context_wrapper
        .as_ref()
        .and_then(WeakValue::upgrade)
    {
        return Ok(value);
    }
    let options = options.filter(|value| matches!(value, Value::Obj(_)));
    let context_settings = if context_id == "2d" {
        Some(parse_canvas_2d_settings(ctx, options.as_ref())?)
    } else {
        None
    };
    if context_id == "webgpu" {
        let target = gpu_canvas_target(
            data.clone(),
            realm.clone(),
            node,
            canvas_wrapper.unwrap_or(Value::Null),
        );
        let value = create_gpu_canvas_context(ctx, target)?;
        data.borrow_mut().context_mode = Some("webgpu");
        data.borrow_mut().context_wrapper = ctx.weak_value(&value);
        return Ok(value);
    }
    if matches!(context_id, "webgl" | "webgl2") {
        let target = gpu_canvas_target(
            data.clone(),
            realm.clone(),
            node,
            canvas_wrapper.unwrap_or(Value::Null),
        );
        let value = create_webgl_canvas_context(ctx, target, context_id, options)?;
        if matches!(value, Value::Null) {
            return Ok(Value::Null);
        }
        data.borrow_mut().context_mode = Some(if context_id == "webgl2" {
            "webgl2"
        } else {
            "webgl"
        });
        data.borrow_mut().context_wrapper = ctx.weak_value(&value);
        return Ok(value);
    }
    let is_offscreen = node.is_none() || data.borrow().transferred;
    let value = if context_id == "bitmaprenderer" {
        if data.borrow().context_mode.is_none() {
            let alpha =
                match options.filter(|value| !matches!(value, Value::Null | Value::Undefined)) {
                    Some(options) => {
                        let alpha = ctx.member_get(&options, "alpha")?;
                        matches!(alpha, Value::Undefined) || ctx.to_boolean(&alpha)
                    }
                    None => true,
                };
            data.borrow_mut().bitmap_alpha = alpha;
        }
        let value = ctx.new_instance(DomImageBitmapRenderingContext {
            data: data.clone(),
            realm,
            node,
            canvas_in_private_slot: is_offscreen,
        });
        if is_offscreen {
            install_offscreen_canvas_owner(ctx, &value, canvas_wrapper)?;
        }
        data.borrow_mut().context_mode = Some("bitmaprenderer");
        value
    } else {
        let value = if is_offscreen {
            let value = ctx.new_instance(DomOffscreenCanvasRenderingContext2D {
                data: data.clone(),
                realm,
                node,
                canvas_in_private_slot: true,
            });
            install_offscreen_canvas_owner(ctx, &value, canvas_wrapper)?;
            value
        } else {
            ctx.new_instance(DomCanvasRenderingContext2D {
                data: data.clone(),
                realm,
                node,
                canvas_in_private_slot: false,
            })
        };
        if let Some(settings) = context_settings {
            let mut data = data.borrow_mut();
            data.alpha = settings.alpha;
            data.color_space = settings.color_space;
            data.desynchronized = settings.desynchronized;
            data.will_read_frequently = settings.will_read_frequently;
        }
        data.borrow_mut().context_mode = Some("2d");
        value
    };
    data.borrow_mut().context_wrapper = ctx.weak_value(&value);
    Ok(value)
}

fn install_offscreen_canvas_owner(
    ctx: &mut Ctx,
    context: &Value,
    canvas: Option<Value>,
) -> OpResult<()> {
    let Some(canvas) = canvas else {
        return Ok(());
    };
    ctx.define_native_private_value_slot(context, OFFSCREEN_CANVAS_OWNER_SLOT, canvas)
        .map_err(OpError::thrown)
}

fn canvas_owner_value(
    ctx: &mut Ctx,
    realm: &Option<Rc<DomRealm>>,
    node: Option<NodeId>,
    canvas_in_private_slot: bool,
    this: &Value,
) -> Value {
    if canvas_in_private_slot {
        return ctx
            .native_private_value_slot(this, OFFSCREEN_CANVAS_OWNER_SLOT)
            .unwrap_or(Value::Null);
    }
    match (realm, node) {
        (Some(realm), Some(node)) => realm.wrap(ctx, node),
        _ => Value::Null,
    }
}

fn gpu_canvas_target(
    data: Rc<RefCell<CanvasData>>,
    realm: Option<Rc<DomRealm>>,
    node: Option<NodeId>,
    canvas: Value,
) -> CanvasGpuTarget {
    let dimensions = {
        let realm = realm.clone();
        let data = data.clone();
        Rc::new(move || match (&realm, node) {
            (Some(realm), Some(node)) => (
                canvas_dimension_at(realm, node, "width", DEFAULT_WIDTH),
                canvas_dimension_at(realm, node, "height", DEFAULT_HEIGHT),
            ),
            _ => data.borrow().surface.dimensions(),
        }) as Rc<dyn Fn() -> (u32, u32)>
    };
    let generation = {
        let data = data.clone();
        Rc::new(move || data.borrow().gpu_generation) as Rc<dyn Fn() -> u64>
    };
    let invalidate = {
        let data = data.clone();
        Rc::new(move || {
            let mut data = data.borrow_mut();
            data.gpu_generation = data.gpu_generation.wrapping_add(1);
            data.bitmap_output = None;
        }) as Rc<dyn Fn()>
    };
    let publish_rgba = {
        let data = data.clone();
        let realm = realm.clone();
        Rc::new(
            move |epoch: u64, revision: u64, width: u32, height: u32, pixels: Vec<u8>| {
                let (current_width, current_height) = match (&realm, node) {
                    (Some(realm), Some(node)) => (
                        canvas_dimension_at(realm, node, "width", DEFAULT_WIDTH),
                        canvas_dimension_at(realm, node, "height", DEFAULT_HEIGHT),
                    ),
                    _ => data.borrow().surface.dimensions(),
                };
                let expected = usize::try_from(width)
                    .ok()
                    .and_then(|width| {
                        usize::try_from(height)
                            .ok()
                            .and_then(|height| width.checked_mul(height))
                    })
                    .and_then(|pixels| pixels.checked_mul(4));
                if epoch != data.borrow().gpu_generation
                    || revision != data.borrow().gpu_snapshot_revision
                    || width != current_width
                    || height != current_height
                    || expected != Some(pixels.len())
                {
                    return Err("GPU canvas readback dimensions are stale or invalid".to_owned());
                }
                data.borrow_mut().bitmap_output = Some(Rgba8Image {
                    width,
                    height,
                    pixels,
                });
                data.borrow_mut().origin_clean = true;
                publish(realm.as_ref(), node, &data.borrow())
                    .map_err(|_| "GPU canvas bitmap publication failed".to_owned())
            },
        ) as Rc<dyn Fn(u64, u64, u32, u32, Vec<u8>) -> Result<(), String>>
    };
    CanvasGpuTarget {
        canvas,
        dimensions,
        generation,
        invalidate,
        publish_rgba,
        reserve_snapshot_revision: {
            let data = Rc::downgrade(&data);
            Rc::new(move || {
                let Some(data) = data.upgrade() else {
                    return 0;
                };
                let mut data = data.borrow_mut();
                data.gpu_snapshot_revision = data.gpu_snapshot_revision.wrapping_add(1);
                data.gpu_snapshot_revision
            })
        },
        set_snapshot_synchronizer: {
            let data = Rc::downgrade(&data);
            Rc::new(move |synchronize| {
                if let Some(data) = data.upgrade() {
                    data.borrow_mut().snapshot_synchronizer = Some(synchronize);
                }
            })
        },
    }
}

#[lumen_bind::class(name = "ImageBitmapRenderingContext", hint(js(webidl)))]
pub struct DomImageBitmapRenderingContext {
    data: Rc<RefCell<CanvasData>>,
    realm: Option<Rc<DomRealm>>,
    node: Option<NodeId>,
    canvas_in_private_slot: bool,
}

#[lumen_bind::methods]
impl DomImageBitmapRenderingContext {
    #[constructor]
    fn new() -> OpResult<Self> {
        Err(OpError::new(
            "TypeError",
            "ImageBitmapRenderingContext cannot be constructed directly",
        ))
    }

    #[getter]
    fn canvas(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        canvas_owner_value(
            ctx,
            &self.realm,
            self.node,
            self.canvas_in_private_slot,
            &this.0,
        )
    }

    fn transfer_from_image_bitmap(&self, ctx: &mut Ctx, bitmap: Value) -> OpResult<()> {
        self.data.borrow().ensure_bitmap_available()?;
        let (image, clean) = if matches!(bitmap, Value::Null) {
            (None, true)
        } else {
            let bitmap = ctx
                .instance_data::<DomImageBitmap>(&bitmap)
                .ok_or_else(|| OpError::new("TypeError", "expected an ImageBitmap or null"))?;
            let bitmap = bitmap.borrow();
            let image = bitmap
                .image
                .borrow_mut()
                .take()
                .ok_or_else(|| OpError::new("InvalidStateError", "ImageBitmap is detached"))?;
            (Some(image), bitmap.origin_clean)
        };
        let mut data = self.data.borrow_mut();
        data.bitmap_output = image;
        data.origin_clean = clean;
        if data.bitmap_output.is_none() {
            data.surface.clear_bitmap();
        }
        publish(self.realm.as_ref(), self.node, &data)
    }
}

#[lumen_bind::class(name = "CanvasRenderingContext2D", hint(js(webidl)))]
pub struct DomCanvasRenderingContext2D {
    data: Rc<RefCell<CanvasData>>,
    realm: Option<Rc<DomRealm>>,
    node: Option<NodeId>,
    canvas_in_private_slot: bool,
}

#[lumen_bind::class(name = "OffscreenCanvasRenderingContext2D", hint(js(webidl)))]
pub struct DomOffscreenCanvasRenderingContext2D {
    data: Rc<RefCell<CanvasData>>,
    realm: Option<Rc<DomRealm>>,
    node: Option<NodeId>,
    canvas_in_private_slot: bool,
}

#[lumen_bind::class(name = "PaintRenderingContext2D", hint(js(webidl)))]
pub(crate) struct DomPaintRenderingContext2D {
    data: Rc<RefCell<CanvasData>>,
    realm: Option<Rc<DomRealm>>,
    node: Option<NodeId>,
    canvas_in_private_slot: bool,
    _reservation: std::sync::Arc<lumen_common::limits::ByteLease>,
}

/// Install the independent Paint interface using the shared typed Canvas2D
/// operations. Paint scopes exclude Canvas ownership, text and pixel-read APIs.
pub(crate) fn install_paint_context(ctx: &mut Ctx) -> OpResult<()> {
    let constructor=ctx.class_constructor::<DomPaintRenderingContext2D>();
    let prototype=ctx.member_get(&constructor,"prototype").map_err(OpError::thrown)?;
    let global=ctx.global_object();
    let reflect=ctx.member_get(&global,"Reflect").map_err(OpError::thrown)?;
    let delete=ctx.member_get(&reflect,"deleteProperty").map_err(OpError::thrown)?;
    for name in ["canvas","font","textAlign","textBaseline","direction","fontKerning",
        "fontStretch","fontVariantCaps","letterSpacing","wordSpacing","textRendering",
        "fillText","strokeText","measureText","getImageData","putImageData","createImageData",
        "drawImage","drawElementImage","reset","isContextLost","beginLayer","endLayer","isPointInPath","isPointInStroke"] {
        ctx.call(delete.clone(),reflect.clone(),&[prototype.clone(),Value::from_string(name.into())])
            .map_err(|error|OpError::thrown(lumen::embed::abrupt_value(error)))?;
    }
    crate::install_interface(ctx,&global,"PaintRenderingContext2D",constructor).map_err(OpError::thrown)
}

pub(crate) fn paint_context(ctx: &mut Ctx, width: u32, height: u32, alpha: bool,
    budget: &std::sync::Arc<lumen_common::limits::ByteBudget>) -> OpResult<Value> {
    let bytes=(width as usize).checked_mul(height as usize).and_then(|pixels|pixels.checked_mul(4))
        .ok_or_else(||OpError::new("QuotaExceededError","paint surface dimensions exceed budget"))?;
    let reservation=budget.reserve(bytes).ok_or_else(||OpError::new("QuotaExceededError","paint surface budget exhausted"))?;
    let mut data=CanvasData::new(width,height)?;
    data.context_mode=Some("2d");data.alpha=alpha;
    if !alpha {data.surface.clear_bitmap();}
    Ok(ctx.new_instance(DomPaintRenderingContext2D {data:Rc::new(RefCell::new(data)),realm:None,
        node:None,canvas_in_private_slot:false,_reservation:std::sync::Arc::new(reservation)}))
}

pub(crate) fn paint_context_snapshot(ctx: &mut Ctx, value: &Value,
    budget: &std::sync::Arc<lumen_common::limits::ByteBudget>) -> OpResult<std::sync::Arc<lumen_html::render_capture::ReservedImageData>> {
    let data=ctx.with_instance::<DomPaintRenderingContext2D,_>(value,|context|context.data.clone())?;
    let bytes=(data.borrow().logical_width as usize).checked_mul(data.borrow().logical_height as usize)
        .and_then(|pixels|pixels.checked_mul(4)).ok_or_else(||OpError::new("QuotaExceededError","paint snapshot dimensions exceed budget"))?;
    let reservation=budget.reserve(bytes).ok_or_else(||OpError::new("QuotaExceededError","paint snapshot budget exhausted"))?;
    let image=data.borrow().snapshot();
    lumen_html::render_capture::ReservedImageData::new(lumen_html::paint::ImageData {
        width:image.width,height:image.height,pixels:image.pixels},reservation)
        .ok_or_else(||OpError::new("QuotaExceededError","paint snapshot exceeded reservation"))
}

macro_rules! impl_canvas_2d_methods {
    ($context:ident) => {
        #[lumen_bind::methods]
        impl $context {
            #[getter]
            fn canvas(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
                Ok(canvas_owner_value(
                    ctx,
                    &self.realm,
                    self.node,
                    self.canvas_in_private_slot,
                    &this.0,
                ))
            }

            #[getter]
            fn global_alpha(&self) -> f32 {
                self.data.borrow().surface.state().alpha
            }
            #[setter]
            fn set_global_alpha(&self, value: f32) {
                if value.is_finite() && (0.0..=1.0).contains(&value) {
                    self.data.borrow_mut().surface.state_mut().alpha = value;
                }
            }
            #[getter]
            fn fill_style(&self) -> Value {
                let data = self.data.borrow();
                data.fill_gradient_value
                    .clone()
                    .or_else(|| data.fill_pattern_value.clone())
                    .unwrap_or_else(|| Value::Str(color_string(data.surface.state().fill).into()))
            }
            #[setter]
            fn set_fill_style(&self, ctx: &mut Ctx, value: Value) {
                if let Some(color) =
                    parse_style_color(&value, current_canvas_color(&self.realm, self.node))
                {
                    let mut data = self.data.borrow_mut();
                    data.surface.state_mut().fill = color;
                    data.surface.state_mut().fill_gradient = None;
                    data.surface.state_mut().fill_pattern = None;
                    data.fill_gradient_value = None;
                    data.fill_pattern_value = None;
                    data.fill_pattern_origin_clean = true;
                } else if let Some((gradient, value)) = resolve_gradient(ctx, &value) {
                    let mut data = self.data.borrow_mut();
                    data.surface.state_mut().fill_gradient = Some(gradient);
                    data.surface.state_mut().fill_pattern = None;
                    data.fill_gradient_value = Some(value);
                    data.fill_pattern_value = None;
                    data.fill_pattern_origin_clean = true;
                } else if let Some((pattern, value)) = resolve_pattern(ctx, &value) {
                    let clean = pattern_origin_clean(ctx, &value).unwrap_or(true);
                    let mut data = self.data.borrow_mut();
                    data.surface.state_mut().fill_gradient = None;
                    data.surface.state_mut().fill_pattern = Some(pattern);
                    data.fill_gradient_value = None;
                    data.fill_pattern_value = Some(value);
                    data.fill_pattern_origin_clean = clean;
                }
            }
            #[getter]
            fn stroke_style(&self) -> Value {
                let data = self.data.borrow();
                data.stroke_gradient_value
                    .clone()
                    .or_else(|| data.stroke_pattern_value.clone())
                    .unwrap_or_else(|| Value::Str(color_string(data.surface.state().stroke).into()))
            }
            #[setter]
            fn set_stroke_style(&self, ctx: &mut Ctx, value: Value) {
                if let Some(color) =
                    parse_style_color(&value, current_canvas_color(&self.realm, self.node))
                {
                    let mut data = self.data.borrow_mut();
                    data.surface.state_mut().stroke = color;
                    data.surface.state_mut().stroke_gradient = None;
                    data.surface.state_mut().stroke_pattern = None;
                    data.stroke_gradient_value = None;
                    data.stroke_pattern_value = None;
                    data.stroke_pattern_origin_clean = true;
                } else if let Some((gradient, value)) = resolve_gradient(ctx, &value) {
                    let mut data = self.data.borrow_mut();
                    data.surface.state_mut().stroke_gradient = Some(gradient);
                    data.surface.state_mut().stroke_pattern = None;
                    data.stroke_gradient_value = Some(value);
                    data.stroke_pattern_value = None;
                    data.stroke_pattern_origin_clean = true;
                } else if let Some((pattern, value)) = resolve_pattern(ctx, &value) {
                    let clean = pattern_origin_clean(ctx, &value).unwrap_or(true);
                    let mut data = self.data.borrow_mut();
                    data.surface.state_mut().stroke_gradient = None;
                    data.surface.state_mut().stroke_pattern = Some(pattern);
                    data.stroke_gradient_value = None;
                    data.stroke_pattern_value = Some(value);
                    data.stroke_pattern_origin_clean = clean;
                }
            }
            #[getter]
            fn line_width(&self) -> f32 {
                self.data.borrow().surface.state().line.width
            }
            #[setter]
            fn set_line_width(&self, width: f32) {
                if width.is_finite() && width > 0.0 {
                    self.data.borrow_mut().surface.state_mut().line.width = width;
                }
            }
            #[getter]
            fn line_cap(&self) -> &'static str {
                match self.data.borrow().surface.state().line.line_cap {
                    LineCap::Butt => "butt",
                    LineCap::Round => "round",
                    LineCap::Square => "square",
                }
            }
            #[setter]
            fn set_line_cap(&self, value: &str) {
                let line_cap = match value {
                    "butt" => LineCap::Butt,
                    "round" => LineCap::Round,
                    "square" => LineCap::Square,
                    _ => return,
                };
                self.data.borrow_mut().surface.state_mut().line.line_cap = line_cap;
            }
            #[getter]
            fn line_join(&self) -> &'static str {
                match self.data.borrow().surface.state().line.line_join {
                    LineJoin::Miter | LineJoin::MiterClip => "miter",
                    LineJoin::Round => "round",
                    LineJoin::Bevel => "bevel",
                }
            }
            #[setter]
            fn set_line_join(&self, value: &str) {
                let line_join = match value {
                    "miter" => LineJoin::Miter,
                    "round" => LineJoin::Round,
                    "bevel" => LineJoin::Bevel,
                    _ => return,
                };
                self.data.borrow_mut().surface.state_mut().line.line_join = line_join;
            }
            #[getter]
            fn miter_limit(&self) -> f64 {
                self.data.borrow().surface.state().miter_limit
            }
            #[setter]
            fn set_miter_limit(&self, value: f64) {
                if value.is_finite() && value > 0.0 {
                    let mut data = self.data.borrow_mut();
                    data.surface.state_mut().miter_limit = value;
                    data.surface.state_mut().line.miter_limit =
                        value.min(f64::from(f32::MAX)) as f32;
                }
            }
            #[getter]
            fn shadow_color(&self) -> String {
                color_string(self.data.borrow().surface.state().shadow_color)
            }
            #[setter]
            fn set_shadow_color(&self, value: &str) {
                if let Some(color) = lumen_html::css::parse_animation_color(
                    value,
                    current_canvas_color(&self.realm, self.node),
                ) {
                    self.data.borrow_mut().surface.state_mut().shadow_color =
                        [color.r, color.g, color.b, color.a];
                }
            }
            #[getter]
            fn shadow_blur(&self) -> f64 {
                self.data.borrow().surface.state().shadow_blur
            }
            #[setter]
            fn set_shadow_blur(&self, value: f64) {
                if value.is_finite() && value >= 0.0 {
                    self.data.borrow_mut().surface.state_mut().shadow_blur = value;
                }
            }
            #[getter]
            fn shadow_offset_x(&self) -> f64 {
                self.data.borrow().surface.state().shadow_offset_x
            }
            #[setter]
            fn set_shadow_offset_x(&self, value: f64) {
                if value.is_finite() {
                    self.data.borrow_mut().surface.state_mut().shadow_offset_x = value;
                }
            }
            #[getter]
            fn shadow_offset_y(&self) -> f64 {
                self.data.borrow().surface.state().shadow_offset_y
            }
            #[setter]
            fn set_shadow_offset_y(&self, value: f64) {
                if value.is_finite() {
                    self.data.borrow_mut().surface.state_mut().shadow_offset_y = value;
                }
            }
            #[getter]
            fn global_composite_operation(&self) -> String {
                composite_name(self.data.borrow().surface.state().blend).into()
            }
            #[setter]
            fn set_global_composite_operation(&self, value: &str) {
                if let Some(mode) = composite_mode(value) {
                    self.data.borrow_mut().surface.state_mut().blend = mode;
                }
            }

            #[getter]
            fn font(&self) -> String {
                self.data.borrow().text.font.clone()
            }
            #[setter]
            fn set_font(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
                let (font_owner,font_node)=match (self.realm.as_ref(),self.node) {
                    (Some(realm),Some(node))=>{let (owner,node)=realm.resolve_adopted_node(node);(Some(owner),Some(node))},
                    _=>(self.realm.clone(),self.node),
                };
                let font = lumen_html::css::resolve_font_shorthand_with_requirements(value, |requirements| {
                    if let (Some(realm), Some(node)) = (font_owner.as_ref(), font_node) {
                        if requirements.query_container { realm.flush_layout()?; }
                        else {
                            let pending = realm.with_session(|session| {
                                let mut current = Some(node);
                                for _ in 0..512 {
                                    let Some(id) = current else { break; };
                                    if matches!(session.document().kind(id), Ok(NodeKind::Element { .. }))
                                        && session.computed_style(id).is_ok_and(|style| style.font_query_context_pending()) { return true; }
                                    current = session.document().composed_parent(id).ok().flatten();
                                }
                                false
                            });
                            if pending { realm.flush_layout()?; }
                        }
                    }
                    let inherited_node = if self.data.borrow().transferred { None } else { font_node };
                    let (inherited_size, inherited_spec) = canvas_inherited_font(font_owner.as_ref(), inherited_node);
                    let fonts = canvas_node_font_source(ctx,font_owner.as_ref(),font_node)?;
                    let metrics = fonts.font_relative_metrics_styled(inherited_size, &inherited_spec);
                    let (root_size,root_units)=canvas_root_font_units(font_owner.as_ref(),&fonts)?;
                    fonts.queue_demand_task(ctx)?;
                    Ok::<_, OpError>(lumen_html::css::FontShorthandContext {
                        font_size: inherited_size, root_font_size: root_size,
                        ex: metrics.ex, ch: metrics.ch,
                        units: Some([font_owner.as_ref().and_then(|realm| inherited_node.and_then(|node| realm.with_session(|session|
                            session.computed_style_with_text(node,Some(&fonts)).ok().map(|style| style.font_unit_context(&fonts)[0]))))
                            .unwrap_or_else(|| lumen_html::css::font_unit_bases(Some(&fonts),inherited_size,&inherited_spec,lumen_html::css::LineHeight::Normal,false,false)),root_units]),
                        weight: inherited_spec.weight,
                        viewport: font_owner.as_ref().map(|realm| realm.with_session(|session| session.media_environment())),
                        query: if requirements.query_container {
                            font_owner.as_ref().map(|realm| realm.with_session(|session| {
                                if let Some(node) = font_node {
                                    session.query_container_context(node).map_err(|error| OpError::new("InvalidStateError", format!("font query context unavailable: {error:?}")))
                                } else {
                                    session.query_container_context(session.document().root()).map_err(|error| OpError::new("InvalidStateError", format!("font query context unavailable: {error:?}")))
                                }
                            })).transpose()?
                        } else { None },
                    })
                })?;
                if let Some(mut font) = font.filter(|font| font.size <= 512.0) {
                    if let (Some(owner),Some(node))=(font_owner.as_ref(),font_node) {
                        font.spec.family_scope=owner.with_session(|session|session.font_reference_scope(node))
                            .map_err(|error|OpError::new("InvalidStateError",format!("canvas font reference scope: {error:?}")))?;
                    }
                    let mut data = self.data.borrow_mut();
                    data.text.font = font.serialized;
                    data.text.size = font.size;
                    data.text.font_variant_caps = font.spec.caps;
                    data.text.spec = font.spec;
                }
                Ok(())
            }
            #[getter]
            fn font_stretch(&self) -> String {
                stretch_value(self.data.borrow().text.spec.stretch)
            }
            #[setter]
            fn set_font_stretch(&self, value: &str) {
                if let Some(stretch) = parse_stretch(value) {
                    self.data.borrow_mut().text.spec.stretch = stretch;
                }
            }
            #[getter]
            fn font_kerning(&self) -> &'static str {
                match self.data.borrow().text.font_kerning {
                    CanvasFontKerning::Auto => "auto",
                    CanvasFontKerning::Normal => "normal",
                    CanvasFontKerning::None => "none",
                }
            }
            #[setter]
            fn set_font_kerning(&self, value: &str) {
                let value = match value {
                    "auto" => CanvasFontKerning::Auto,
                    "normal" => CanvasFontKerning::Normal,
                    "none" => CanvasFontKerning::None,
                    _ => return,
                };
                self.data.borrow_mut().text.font_kerning = value;
            }
            #[getter]
            fn font_variant_caps(&self) -> &'static str {
                self.data.borrow().text.font_variant_caps.as_str()
            }
            #[setter]
            fn set_font_variant_caps(&self, value: &str) {
                let Some(value) = CanvasFontVariantCaps::parse(value) else { return; };
                let mut data = self.data.borrow_mut();
                data.text.font_variant_caps = value;
                data.text.spec.caps = value;
            }
            #[getter]
            fn text_rendering(&self) -> &'static str {
                match self.data.borrow().text.text_rendering {
                    CanvasTextRendering::Auto => "auto",
                    CanvasTextRendering::OptimizeSpeed => "optimizeSpeed",
                    CanvasTextRendering::OptimizeLegibility => "optimizeLegibility",
                    CanvasTextRendering::GeometricPrecision => "geometricPrecision",
                }
            }
            #[setter]
            fn set_text_rendering(&self, value: &str) {
                let value = match value {
                    "auto" => CanvasTextRendering::Auto,
                    "optimizeSpeed" => CanvasTextRendering::OptimizeSpeed,
                    "optimizeLegibility" => CanvasTextRendering::OptimizeLegibility,
                    "geometricPrecision" => CanvasTextRendering::GeometricPrecision,
                    _ => return,
                };
                self.data.borrow_mut().text.text_rendering = value;
            }
            #[getter]
            fn letter_spacing(&self) -> String {
                css_spacing_string(self.data.borrow().text.letter_spacing)
            }
            #[setter]
            fn set_letter_spacing(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
                let text = self.data.borrow().text.clone();
                let fonts = canvas_node_font_source(ctx,self.realm.as_ref(),self.node)?;
                let owner = self.realm.as_ref().map(|realm| self.node.map_or_else(||realm.clone(),|node|realm.resolve_adopted_node(node).0));
                let own = lumen_html::css::font_unit_bases(Some(&fonts),text.size,&text.spec,lumen_html::css::LineHeight::Normal,false,false);
                let (root_size, root) = canvas_root_font_units(owner.as_ref(),&fonts)?;
                if let Some(value) = parse_css_spacing(value,text.size,root_size,[own,root]) {
                    self.data.borrow_mut().text.letter_spacing = value;
                }
                fonts.queue_demand_task(ctx)?;
                Ok(())
            }
            #[getter]
            fn word_spacing(&self) -> String {
                css_spacing_string(self.data.borrow().text.word_spacing)
            }
            #[setter]
            fn set_word_spacing(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
                let text = self.data.borrow().text.clone();
                let fonts = canvas_node_font_source(ctx,self.realm.as_ref(),self.node)?;
                let owner = self.realm.as_ref().map(|realm| self.node.map_or_else(||realm.clone(),|node|realm.resolve_adopted_node(node).0));
                let own = lumen_html::css::font_unit_bases(Some(&fonts),text.size,&text.spec,lumen_html::css::LineHeight::Normal,false,false);
                let (root_size, root) = canvas_root_font_units(owner.as_ref(),&fonts)?;
                if let Some(value) = parse_css_spacing(value,text.size,root_size,[own,root]) {
                    self.data.borrow_mut().text.word_spacing = value;
                }
                fonts.queue_demand_task(ctx)?;
                Ok(())
            }
            #[getter]
            fn text_align(&self) -> String {
                self.data.borrow().text.align.clone()
            }
            #[setter]
            fn set_text_align(&self, value: &str) {
                if matches!(value, "start" | "end" | "left" | "right" | "center") {
                    self.data.borrow_mut().text.align = value.into();
                }
            }
            #[getter]
            fn text_baseline(&self) -> String {
                self.data.borrow().text.baseline.clone()
            }
            #[setter]
            fn set_text_baseline(&self, value: &str) {
                if matches!(
                    value,
                    "top" | "hanging" | "middle" | "alphabetic" | "ideographic" | "bottom"
                ) {
                    self.data.borrow_mut().text.baseline = value.into();
                }
            }
            #[getter]
            fn direction(&self) -> String {
                self.data.borrow().text.direction.clone()
            }
            #[setter]
            fn set_direction(&self, value: &str) {
                if matches!(value, "inherit" | "ltr" | "rtl") {
                    self.data.borrow_mut().text.direction = value.into();
                }
            }
            #[method(name = "measureText")]
            fn measure_text(&self, ctx: &mut Ctx, text: &str) -> OpResult<Value> {
                let (font_size, spec, rtl, options) = {
                    let data = self.data.borrow();
                    (
                        data.text.size,
                        data.text.spec.clone(),
                        resolved_canvas_direction(&data, self.realm.as_ref(), self.node),
                        data.text.shaping_options(),
                    )
                };
                let fonts = canvas_node_font_source(ctx,self.realm.as_ref(),self.node)?;
                let run = self
                    .data
                    .borrow()
                    .surface
                    .measure_text(&fonts, text, font_size, &spec, rtl, &options)
                    .map_err(|error| {
                        OpError::new(
                            "InvalidStateError",
                            format!("text shaping failed: {error:?}"),
                        )
                    })?;
                let metrics = text_metrics(&fonts, &run, font_size, &spec)?;
                fonts.queue_demand_task(ctx)?;
                let value = ctx.new_instance(DomTextMetrics { metrics });
                Ok(value)
            }
            #[method(name = "fillText")]
            fn fill_text(
                &self,
                ctx: &mut Ctx,
                text: &str,
                x: f64,
                y: f64,
                max_width: Option<f64>,
            ) -> OpResult<()> {
                if !x.is_finite()
                    || !y.is_finite()
                    || max_width.is_some_and(|value| value.is_nan() || value <= 0.0)
                {
                    return Ok(());
                }
                let fonts = canvas_node_font_source(ctx,self.realm.as_ref(),self.node)?;
                let rtl = resolved_canvas_direction(&self.data.borrow(), self.realm.as_ref(), self.node);
                let mut data = self.data.borrow_mut();
                if !data.fill_pattern_origin_clean {
                    data.origin_clean = false;
                }
                let mut text_state = data.text.clone();
                text_state.direction = if rtl { "rtl" } else { "ltr" }.into();
                let options = text_state.shaping_options();
                let (width, ascent, descent) = {
                    let run = data
                        .surface
                        .measure_text(
                            &fonts,
                            text,
                            text_state.size,
                            &text_state.spec,
                            text_state.direction == "rtl",
                            &options,
                        )
                        .map_err(|error| {
                            OpError::new(
                                "InvalidStateError",
                                format!("text shaping failed: {error:?}"),
                            )
                        })?;
                    (
                        run.width,
                        fonts.ascent_styled(text_state.size, &text_state.spec),
                        (fonts.line_height_styled(text_state.size, &text_state.spec)
                            - fonts.ascent_styled(text_state.size, &text_state.spec))
                        .max(0.0),
                    )
                };
                let origin_x = x as f32 - width * text_anchor(&text_state);
                let baseline_y = text_baseline(&text_state, y as f32, ascent, descent);
                data.surface
                    .fill_text(
                        &fonts,
                        text,
                        text_state.size,
                        &text_state.spec,
                        text_state.direction == "rtl",
                        origin_x,
                        x as f32,
                        baseline_y,
                        max_width.map(|value| value as f32),
                        &options,
                    )
                    .map_err(|error| {
                        OpError::new(
                            "InvalidStateError",
                            format!("text drawing failed: {error:?}"),
                        )
                    })?;
                fonts.queue_demand_task(ctx)?;
                self.publish_data(&data)
            }
            #[method(name = "strokeText")]
            fn stroke_text(
                &self,
                ctx: &mut Ctx,
                text: &str,
                x: f64,
                y: f64,
                max_width: Option<f64>,
            ) -> OpResult<()> {
                if !x.is_finite()
                    || !y.is_finite()
                    || max_width.is_some_and(|value| value.is_nan() || value <= 0.0)
                {
                    return Ok(());
                }
                let fonts = canvas_node_font_source(ctx,self.realm.as_ref(),self.node)?;
                let rtl = resolved_canvas_direction(&self.data.borrow(), self.realm.as_ref(), self.node);
                let mut data = self.data.borrow_mut();
                if !data.stroke_pattern_origin_clean {
                    data.origin_clean = false;
                }
                let mut text_state = data.text.clone();
                text_state.direction = if rtl { "rtl" } else { "ltr" }.into();
                let options = text_state.shaping_options();
                let (width, ascent, descent) = {
                    let run = data
                        .surface
                        .measure_text(
                            &fonts,
                            text,
                            text_state.size,
                            &text_state.spec,
                            text_state.direction == "rtl",
                            &options,
                        )
                        .map_err(|error| {
                            OpError::new(
                                "InvalidStateError",
                                format!("text shaping failed: {error:?}"),
                            )
                        })?;
                    (
                        run.width,
                        fonts.ascent_styled(text_state.size, &text_state.spec),
                        (fonts.line_height_styled(text_state.size, &text_state.spec)
                            - fonts.ascent_styled(text_state.size, &text_state.spec))
                        .max(0.0),
                    )
                };
                let anchor = text_anchor(&text_state);
                let origin_x = x as f32 - width * anchor;
                let baseline_y = text_baseline(&text_state, y as f32, ascent, descent);
                data.surface
                    .stroke_text(
                        &fonts,
                        text,
                        text_state.size,
                        &text_state.spec,
                        text_state.direction == "rtl",
                        origin_x,
                        x as f32,
                        baseline_y,
                        max_width.map(|value| value as f32),
                        &options,
                    )
                    .map_err(|error| {
                        OpError::new(
                            "InvalidStateError",
                            format!("text drawing failed: {error:?}"),
                        )
                    })?;
                fonts.queue_demand_task(ctx)?;
                self.publish_data(&data)
            }

            #[method(name = "createLinearGradient")]
            fn create_linear_gradient(
                &self,
                ctx: &mut Ctx,
                x0: f64,
                y0: f64,
                x1: f64,
                y1: f64,
            ) -> OpResult<Value> {
                if ![x0, y0, x1, y1]
                    .iter()
                    .all(|v| v.is_finite() && v.abs() <= f64::from(f32::MAX))
                    || (x0 == x1 && y0 == y1)
                {
                    return Err(OpError::new(
                        "IndexSizeError",
                        "linear gradient endpoints must be finite and distinct",
                    ));
                }
                let transform = self.data.borrow().surface.state().transform;
                new_gradient(
                    ctx,
                    CanvasGradient::new(CanvasGradientKind::Linear {
                        start: [x0 as f32, y0 as f32],
                        end: [x1 as f32, y1 as f32],
                        transform,
                    }),
                )
            }

            #[method(name = "createRadialGradient")]
            fn create_radial_gradient(
                &self,
                ctx: &mut Ctx,
                x0: f64,
                y0: f64,
                r0: f64,
                x1: f64,
                y1: f64,
                r1: f64,
            ) -> OpResult<Value> {
                if ![x0, y0, r0, x1, y1, r1]
                    .iter()
                    .all(|v| v.is_finite() && v.abs() <= f64::from(f32::MAX))
                    || r0 < 0.0
                    || r1 < 0.0
                {
                    return Err(OpError::new(
                        "IndexSizeError",
                        "radial gradient circles are invalid",
                    ));
                }
                let transform = self.data.borrow().surface.state().transform;
                new_gradient(
                    ctx,
                    CanvasGradient::new(CanvasGradientKind::Radial {
                        start: [x0 as f32, y0 as f32],
                        end: [x1 as f32, y1 as f32],
                        start_radius: r0 as f32,
                        end_radius: r1 as f32,
                        transform,
                    }),
                )
            }

            #[method(name = "createPattern")]
            fn create_pattern(
                &self,
                ctx: &mut Ctx,
                source: Value,
                repetition: Option<&str>,
            ) -> OpResult<Value> {
                let source_image = match canvas_source_image(ctx, source.clone())? {
                    Some(image) => image,
                    None => return Ok(Value::Null),
                };
                let source_clean = canvas_source_origin_clean(ctx, &source);
                let image=source_image.pixels;
                let repetition = match repetition.unwrap_or("repeat") {
                    "repeat" => CanvasPatternRepetition::Repeat,
                    "repeat-x" => CanvasPatternRepetition::RepeatX,
                    "repeat-y" => CanvasPatternRepetition::RepeatY,
                    "no-repeat" => CanvasPatternRepetition::NoRepeat,
                    _ => {
                        return Err(OpError::new(
                            "SyntaxError",
                            "invalid CanvasPattern repetition",
                        ));
                    }
                };
                if image.width == 0 || image.height == 0 || source_image.natural.0==0.0 || source_image.natural.1==0.0 {
                    return Ok(Value::Null);
                }
                let pattern = CanvasPattern::with_natural_size(&image, repetition,source_image.natural.0,source_image.natural.1).map_err(|_| {
                    OpError::new(
                        "InvalidStateError",
                        "pattern source could not be rasterized",
                    )
                })?;
                new_pattern(ctx, pattern, source_clean)
            }

            fn save(&self) {
                let mut data = self.data.borrow_mut();
                data.surface.save();
                let fill = (
                    data.fill_gradient_value.clone(),
                    data.fill_pattern_value.clone(),
                    data.fill_pattern_origin_clean,
                );
                let stroke = (
                    data.stroke_gradient_value.clone(),
                    data.stroke_pattern_value.clone(),
                    data.stroke_pattern_origin_clean,
                );
                data.saved_fill_styles.push(fill);
                data.saved_stroke_styles.push(stroke);
                let text = data.text.clone();
                data.saved_text.push(text);
            }
            fn restore(&self) {
                let mut data = self.data.borrow_mut();
                if !data.surface.restore() { return; }
                if let Some((gradient, pattern, pattern_clean)) = data.saved_fill_styles.pop() {
                    data.fill_gradient_value = gradient;
                    data.fill_pattern_value = pattern;
                    data.fill_pattern_origin_clean = pattern_clean;
                }
                if let Some((gradient, pattern, pattern_clean)) = data.saved_stroke_styles.pop() {
                    data.stroke_gradient_value = gradient;
                    data.stroke_pattern_value = pattern;
                    data.stroke_pattern_origin_clean = pattern_clean;
                }
                if let Some(text) = data.saved_text.pop() {
                    data.text = text;
                }
            }
            fn begin_layer(&self) -> OpResult<()> {
                // The entry save holds native state and traced JS style values.
                self.save();
                let result = self.data.borrow_mut().surface.begin_layer();
                if result.is_err() {
                    self.restore();
                    return Err(OpError::new("QuotaExceededError", "Canvas layer budget exceeded"));
                }
                Ok(())
            }
            fn end_layer(&self) -> OpResult<()> {
                let mut data = self.data.borrow_mut();
                let depth = data.surface.end_layer().ok_or_else(|| {
                    OpError::new("InvalidStateError", "No Canvas layer is open")
                })?;
                data.surface.restore();
                while data.saved_fill_styles.len() >= depth {
                    if let Some((gradient, pattern, clean)) = data.saved_fill_styles.pop() {
                        data.fill_gradient_value = gradient;
                        data.fill_pattern_value = pattern;
                        data.fill_pattern_origin_clean = clean;
                    }
                    if let Some((gradient, pattern, clean)) = data.saved_stroke_styles.pop() {
                        data.stroke_gradient_value = gradient;
                        data.stroke_pattern_value = pattern;
                        data.stroke_pattern_origin_clean = clean;
                    }
                    if let Some(text) = data.saved_text.pop() { data.text = text; }
                }
                self.publish_data(&data)
            }
            fn fill_rect(&self, x: f64, y: f64, width: f64, height: f64) -> OpResult<()> {
                let mut data = self.data.borrow_mut();
                if !data.fill_pattern_origin_clean {
                    data.origin_clean = false;
                }
                data.surface
                    .fill_rect(x as f32, y as f32, width as f32, height as f32)
                    .map_err(|error| {
                        OpError::new(
                            "InvalidStateError",
                            format!("canvas shadow rendering failed: {error:?}"),
                        )
                    })?;
                self.publish_data(&data)
            }
            fn clear_rect(&self, x: f64, y: f64, width: f64, height: f64) -> OpResult<()> {
                let mut data = self.data.borrow_mut();
                data.surface
                    .clear_rect(x as f32, y as f32, width as f32, height as f32);
                self.publish_data(&data)
            }
            fn stroke_rect(&self, x: f64, y: f64, width: f64, height: f64) -> OpResult<()> {
                if ![x, y, width, height].iter().all(|value| value.is_finite())
                    || width == 0.0 && height == 0.0
                {
                    return Ok(());
                }
                let mut data = self.data.borrow_mut();
                if !data.stroke_pattern_origin_clean {
                    data.origin_clean = false;
                }
                let rectangle = CanvasPath {
                    commands: vec![PathCommand::Rect(
                        x as f32,
                        y as f32,
                        width as f32,
                        height as f32,
                    )],
                };
                if let Some(path) = rectangle.build(Transform::identity()) {
                    data.surface.stroke_path(&path).map_err(|error| {
                        OpError::new(
                            "InvalidStateError",
                            format!("canvas shadow rendering failed: {error:?}"),
                        )
                    })?;
                }
                self.publish_data(&data)
            }
            fn begin_path(&self) {
                self.data.borrow_mut().current_path = CanvasPath::default();
            }
            fn close_path(&self) {
                self.data
                    .borrow_mut()
                    .current_path
                    .commands
                    .push(PathCommand::Close);
            }
            fn move_to(&self, x: f64, y: f64) {
                self.data
                    .borrow_mut()
                    .current_path
                    .commands
                    .push(PathCommand::Move(x as f32, y as f32));
            }
            fn line_to(&self, x: f64, y: f64) {
                self.data
                    .borrow_mut()
                    .current_path
                    .commands
                    .push(PathCommand::Line(x as f32, y as f32));
            }
            fn quadratic_curve_to(&self, cpx: f64, cpy: f64, x: f64, y: f64) {
                self.data
                    .borrow_mut()
                    .current_path
                    .commands
                    .push(PathCommand::Quad(
                        cpx as f32, cpy as f32, x as f32, y as f32,
                    ));
            }
            fn bezier_curve_to(&self, cp1x: f64, cp1y: f64, cp2x: f64, cp2y: f64, x: f64, y: f64) {
                self.data
                    .borrow_mut()
                    .current_path
                    .commands
                    .push(PathCommand::Cubic(
                        cp1x as f32,
                        cp1y as f32,
                        cp2x as f32,
                        cp2y as f32,
                        x as f32,
                        y as f32,
                    ));
            }
            #[method(name = "arc")]
            fn arc(
                &self,
                x: f64,
                y: f64,
                radius: f64,
                start_angle: f64,
                end_angle: f64,
                counterclockwise: Option<bool>,
            ) -> OpResult<()> {
                if radius < 0.0 {
                    return Err(OpError::new(
                        "IndexSizeError",
                        "arc radius must be non-negative",
                    ));
                }
                let values = [x, y, radius, start_angle, end_angle];
                if !values.iter().all(|value| value.is_finite())
                    || values.iter().any(|value| value.abs() > f64::from(f32::MAX))
                {
                    return Ok(());
                }
                self.data
                    .borrow_mut()
                    .current_path
                    .commands
                    .push(PathCommand::Arc {
                        cx: x as f32,
                        cy: y as f32,
                        rx: radius as f32,
                        ry: radius as f32,
                        rotation: 0.0,
                        start: start_angle as f32,
                        end: end_angle as f32,
                        counterclockwise: counterclockwise.unwrap_or(false),
                    });
                Ok(())
            }
            #[method(name = "ellipse")]
            fn ellipse(
                &self,
                x: f64,
                y: f64,
                radius_x: f64,
                radius_y: f64,
                rotation: f64,
                start_angle: f64,
                end_angle: f64,
                counterclockwise: Option<bool>,
            ) -> OpResult<()> {
                if radius_x < 0.0 || radius_y < 0.0 {
                    return Err(OpError::new(
                        "IndexSizeError",
                        "ellipse radii must be non-negative",
                    ));
                }
                let values = [x, y, radius_x, radius_y, rotation, start_angle, end_angle];
                if !values.iter().all(|value| value.is_finite())
                    || values.iter().any(|value| value.abs() > f64::from(f32::MAX))
                {
                    return Ok(());
                }
                self.data
                    .borrow_mut()
                    .current_path
                    .commands
                    .push(PathCommand::Arc {
                        cx: x as f32,
                        cy: y as f32,
                        rx: radius_x as f32,
                        ry: radius_y as f32,
                        rotation: rotation as f32,
                        start: start_angle as f32,
                        end: end_angle as f32,
                        counterclockwise: counterclockwise.unwrap_or(false),
                    });
                Ok(())
            }
            fn rect(&self, x: f64, y: f64, width: f64, height: f64) {
                self.data
                    .borrow_mut()
                    .current_path
                    .commands
                    .push(PathCommand::Rect(
                        x as f32,
                        y as f32,
                        width as f32,
                        height as f32,
                    ));
            }
            fn fill(
                &self,
                ctx: &mut Ctx,
                path_or_rule: Option<Value>,
                fill_rule: Option<&str>,
            ) -> OpResult<()> {
                let mut data = self.data.borrow_mut();
                if !data.fill_pattern_origin_clean {
                    data.origin_clean = false;
                }
                let (path_data, rule_text) = match path_or_rule {
                    Some(Value::Str(rule)) => (None, Some(rule.as_str().to_owned())),
                    Some(path_value) => (
                        Some(clone_path2d(ctx, &path_value)?),
                        fill_rule.map(str::to_owned),
                    ),
                    None => (None, fill_rule.map(str::to_owned)),
                };
                let rule = parse_fill_rule(rule_text.as_deref())?;
                let path = path_data
                    .as_ref()
                    .unwrap_or(&data.current_path)
                    .build(Transform::identity());
                if let Some(path) = path {
                    data.surface.fill_path(&path, rule).map_err(|error| {
                        OpError::new(
                            "InvalidStateError",
                            format!("canvas shadow rendering failed: {error:?}"),
                        )
                    })?;
                }
                self.publish_data(&data)
            }
            fn stroke(&self, ctx: &mut Ctx, path_data: Option<Value>) -> OpResult<()> {
                let mut data = self.data.borrow_mut();
                if !data.stroke_pattern_origin_clean {
                    data.origin_clean = false;
                }
                let path_data = path_data
                    .map(|value| clone_path2d(ctx, &value))
                    .transpose()?;
                let path = path_data
                    .as_ref()
                    .unwrap_or(&data.current_path)
                    .build(Transform::identity());
                if let Some(path) = path {
                    data.surface.stroke_path(&path).map_err(|error| {
                        OpError::new(
                            "InvalidStateError",
                            format!("canvas shadow rendering failed: {error:?}"),
                        )
                    })?;
                }
                self.publish_data(&data)
            }
            fn clip(
                &self,
                ctx: &mut Ctx,
                path_or_rule: Option<Value>,
                fill_rule: Option<&str>,
            ) -> OpResult<()> {
                let mut data = self.data.borrow_mut();
                let (path_data, rule_text) = match path_or_rule {
                    Some(Value::Str(rule)) => (None, Some(rule.as_str().to_owned())),
                    Some(path_value) => (
                        Some(clone_path2d(ctx, &path_value)?),
                        fill_rule.map(str::to_owned),
                    ),
                    None => (None, fill_rule.map(str::to_owned)),
                };
                let rule = parse_fill_rule(rule_text.as_deref())?;
                let path = path_data
                    .as_ref()
                    .unwrap_or(&data.current_path)
                    .build(Transform::identity());
                if let Some(path) = path {
                    data.surface.clip_path(&path, rule);
                }
                Ok(())
            }

            fn translate(&self, x: f64, y: f64) {
                self.transform(Transform::from_translate(x as f32, y as f32));
            }
            fn scale(&self, x: f64, y: f64) {
                self.transform(Transform::from_scale(x as f32, y as f32));
            }
            fn rotate(&self, angle: f64) {
                if angle.is_finite() {
                    let (s, c) = (angle.sin() as f32, angle.cos() as f32);
                    self.transform(Transform::from_row(c, s, -s, c, 0.0, 0.0));
                }
            }
            fn set_transform(&self, a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) {
                if [a, b, c, d, e, f].iter().all(|n| n.is_finite()) {
                    self.data.borrow_mut().surface.state_mut().transform = Transform::from_row(
                        a as f32, b as f32, c as f32, d as f32, e as f32, f as f32,
                    );
                }
            }
            fn reset_transform(&self) {
                self.data.borrow_mut().surface.state_mut().transform = Transform::identity();
            }

            fn get_image_data(
                &self,
                ctx: &mut Ctx,
                sx: Value,
                sy: Value,
                sw: Value,
                sh: Value,
                settings: Option<Value>,
            ) -> OpResult<Value> {
                let sx = canvas_long(ctx, &sx)?;
                let sy = canvas_long(ctx, &sy)?;
                let sw = canvas_long(ctx, &sw)?;
                let sh = canvas_long(ctx, &sh)?;
                let default_color_space = self.data.borrow().color_space;
                let settings = parse_image_data_settings_with_default(
                    ctx,
                    settings.as_ref(),
                    default_color_space,
                )?;
                ensure_origin_clean(&self.data.borrow())?;
                self.data.borrow().ensure_bitmap_available()?;
                if sw == 0 || sh == 0 {
                    return Err(OpError::new(
                        "IndexSizeError",
                        "image data dimensions cannot be zero",
                    ));
                }
                let width = sw.unsigned_abs();
                let height = sh.unsigned_abs();
                image_data_storage(width, height, settings.pixel_format)?;
                let mut image = self
                    .data
                    .borrow()
                    .surface
                    .read_pixels(
                        if sw < 0 {
                            i64::from(sx) + i64::from(sw)
                        } else {
                            i64::from(sx)
                        },
                        if sh < 0 {
                            i64::from(sy) + i64::from(sh)
                        } else {
                            i64::from(sy)
                        },
                        width,
                        height,
                    )
                    .map_err(|_| OpError::new("IndexSizeError", "image data is too large"))?;
                if !self.data.borrow().alpha {
                    make_image_opaque(&mut image.pixels);
                }
                Ok(ctx.new_instance(DomImageData::from_image_settings(image, settings)))
            }

            fn create_image_data(
                &self,
                ctx: &mut Ctx,
                source_or_width: Value,
                height: Option<Value>,
                settings: Option<Value>,
            ) -> OpResult<Value> {
                if let Some(source) = ctx.instance_data::<DomImageData>(&source_or_width) {
                    let source = source.borrow();
                    let settings = ImageDataSettings {
                        color_space: source.color_space,
                        pixel_format: source.pixel_format,
                    };
                    let image = match source.pixel_format {
                        ImageDataPixelFormat::RgbaUnorm8 => Some(Rgba8Image {
                            width: source.width,
                            height: source.height,
                            pixels: zeroed_bytes(
                                image_data_bytes(source.width, source.height)?,
                                "ImageData allocation failed",
                            )?,
                        }),
                        ImageDataPixelFormat::RgbaFloat16 => None,
                    };
                    let copy = DomImageData {
                        image,
                        width: source.width,
                        height: source.height,
                        color_space: settings.color_space,
                        pixel_format: settings.pixel_format,
                        pixels: RefCell::new(None),
                    };
                    return Ok(ctx.new_instance(copy));
                }
                if matches!(source_or_width, Value::Obj(_)) {
                    return Err(OpError::new(
                        "TypeError",
                        "createImageData expects an ImageData object or numeric dimensions",
                    ));
                }
                let height = height.ok_or_else(|| {
                    OpError::new("TypeError", "createImageData requires two dimensions")
                })?;
                let sw = canvas_long(ctx, &source_or_width)?;
                let sh = canvas_long(ctx, &height)?;
                let default_color_space = self.data.borrow().color_space;
                let settings = parse_image_data_settings_with_default(
                    ctx,
                    settings.as_ref(),
                    default_color_space,
                )?;
                let width = sw.unsigned_abs();
                let height = sh.unsigned_abs();
                let (_, byte_len) = image_data_storage(width, height, settings.pixel_format)?;
                let image = match settings.pixel_format {
                    ImageDataPixelFormat::RgbaUnorm8 => Some(Rgba8Image {
                        width,
                        height,
                        pixels: zeroed_bytes(byte_len, "ImageData allocation failed")?,
                    }),
                    ImageDataPixelFormat::RgbaFloat16 => None,
                };
                Ok(ctx.new_instance(DomImageData {
                    image,
                    width,
                    height,
                    color_space: settings.color_space,
                    pixel_format: settings.pixel_format,
                    pixels: RefCell::new(None),
                }))
            }
            fn put_image_data(
                &self,
                ctx: &mut Ctx,
                image: &DomImageData,
                dx: Value,
                dy: Value,
                dirty_x: lumen_bind::Passed<Value>,
                dirty_y: lumen_bind::Passed<Value>,
                dirty_width: lumen_bind::Passed<Value>,
                dirty_height: lumen_bind::Passed<Value>,
            ) -> OpResult<()> {
                let dx = canvas_long(ctx, &dx)?;
                let dy = canvas_long(ctx, &dy)?;
                let dirty = match (dirty_x.0, dirty_y.0, dirty_width.0, dirty_height.0) {
                    (Some(x), Some(y), Some(width), Some(height)) => {
                        let x = canvas_long(ctx, &x)?;
                        let y = canvas_long(ctx, &y)?;
                        let width = canvas_long(ctx, &width)?;
                        let height = canvas_long(ctx, &height)?;
                        normalized_dirty_rect(image.width, image.height, x, y, width, height)
                    }
                    // The overload set has three- and seven-argument forms.
                    // Arguments beyond the shorter overload are ignored when
                    // the seven-argument overload is not selected.
                    _ => Some((0, 0, image.width, image.height)),
                };
                let Some((source_x, source_y, width, height)) = dirty else {
                    return Ok(());
                };
                self.data.borrow().ensure_bitmap_available()?;
                let pixels = image.read_pixels(ctx)?;
                let mut data = self.data.borrow_mut();
                data.surface
                    .write_pixels_region(
                        &pixels,
                        i64::from(dx) + i64::from(source_x),
                        i64::from(dy) + i64::from(source_y),
                        source_x,
                        source_y,
                        width,
                        height,
                    )
                    .map_err(|_| OpError::new("IndexSizeError", "invalid image data"))?;
                self.publish_data(&data)
            }

            #[method(name = "drawElementImage")]
            fn draw_element_image(
                &self,
                ctx: &mut Ctx,
                source: Value,
                a: f64,
                b: f64,
                c: Option<f64>,
                d: Option<f64>,
                e: Option<f64>,
                f: Option<f64>,
                g: Option<f64>,
                h: Option<f64>,
            ) -> OpResult<()> {
                // Native canvas sources expose their current bitmap, including
                // draws performed after the last element paint generation.
                // Reuse the ordinary image path for taint and numeric geometry.
                if ctx.with_instance::<DomCanvasElement,_>(&source,|_|()).is_ok()
                    || ctx.with_instance::<DomElementImage,_>(&source,|_|()).is_ok() {
                    return self.draw_image(ctx,source,a,b,c,d,e,f,g,h);
                }
                let (source_realm,node) = ctx.with_instance::<super::DomElement,_>(&source, |element| (element.base.realm.clone(),element.base.id))?;
                let realm = self.realm.as_ref().ok_or_else(|| OpError::new("InvalidStateError","Element drawing requires the owning document"))?;
                if !Rc::ptr_eq(realm,&source_realm) {return Err(OpError::new("InvalidStateError","Element belongs to another document"));}
                let canvas = self.node.ok_or_else(||OpError::new("InvalidStateError","Element drawing requires the owning canvas"))?;
                let image = realm.canvases.drawable_snapshots.borrow().iter()
                    .find(|(owner,element,_)| *owner==canvas && *element==node).map(|(_,_,image)|image.clone())
                    .ok_or_else(||OpError::new("InvalidStateError","Element has no published drawable snapshot for this canvas"))?;
                let premultiply_alpha = true;
                let mut data = self.data.borrow_mut();
                match (c, d, e, f, g, h) {
                    (None, None, None, None, None, None) => {
                        data.surface.draw_image_with_alpha_behavior(
                            &image,
                            a as f32,
                            b as f32,
                            image.width as f32,
                            image.height as f32,
                            premultiply_alpha,
                        )
                    }
                    (Some(width), Some(height), None, None, None, None) => {
                        data.surface.draw_image_with_alpha_behavior(
                            &image,
                            a as f32,
                            b as f32,
                            width as f32,
                            height as f32,
                            premultiply_alpha,
                        )
                    }
                    (Some(sw), Some(sh), Some(dx), Some(dy), None, None) => {
                        data.surface.draw_image_crop_with_alpha_behavior(&image,a as f32,b as f32,sw as f32,sh as f32,dx as f32,dy as f32,sw as f32,sh as f32,premultiply_alpha)
                    }
                    (Some(sw), Some(sh), Some(dx), Some(dy), Some(dw), Some(dh)) => {
                        data.surface.draw_image_crop_with_alpha_behavior(
                            &image,
                            a as f32,
                            b as f32,
                            sw as f32,
                            sh as f32,
                            dx as f32,
                            dy as f32,
                            dw as f32,
                            dh as f32,
                            premultiply_alpha,
                        )
                    }
                    _ => {
                        return Err(OpError::new(
                            "TypeError",
                            "drawImage expects 3, 5, or 9 arguments",
                        ));
                    }
                }
                .map_err(|_| {
                    OpError::new(
                        "InvalidStateError",
                        "drawImage could not rasterize the source",
                    )
                })?;
                self.publish_data(&data)
            }

            #[method(name = "drawImage")]
            fn draw_image(
                &self,
                ctx: &mut Ctx,
                source: Value,
                a: f64,
                b: f64,
                c: Option<f64>,
                d: Option<f64>,
                e: Option<f64>,
                f: Option<f64>,
                g: Option<f64>,
                h: Option<f64>,
            ) -> OpResult<()> {
                if [Some(a),Some(b),c,d,e,f,g,h].into_iter().flatten().any(|value|!value.is_finite()){return Ok(());}
                let premultiply_alpha = ctx
                    .instance_data::<DomImageBitmap>(&source)
                    .map_or(true, |bitmap| bitmap.as_ref().borrow().premultiply_alpha);
                let Some(source_image) = canvas_source_image(ctx, source.clone())? else {
                    return Ok(());
                };
                let source_origin_clean = canvas_source_origin_clean(ctx, &source);
                let image=source_image.pixels;
                let natural=source_image.natural;
                if natural.0<=0.0 || natural.1<=0.0 || !natural.0.is_finite() || !natural.1.is_finite(){return Ok(());}
                if matches!((c,d,e,f,g,h),(Some(sw),Some(sh),Some(_),Some(_),Some(_),Some(_)) if sw==0.0 || sh==0.0){return Ok(());}
                let mut data = self.data.borrow_mut();
                if !source_origin_clean {
                    data.origin_clean = false;
                }
                match (c, d, e, f, g, h) {
                    (None, None, None, None, None, None) => {
                        data.surface.draw_image_with_alpha_behavior(
                            &image,
                            a as f32,
                            b as f32,
                            natural.0 as f32,
                            natural.1 as f32,
                            premultiply_alpha,
                        )
                    }
                    (Some(width), Some(height), None, None, None, None) => {
                        data.surface.draw_image_with_alpha_behavior(
                            &image,
                            a as f32,
                            b as f32,
                            width as f32,
                            height as f32,
                            premultiply_alpha,
                        )
                    }
                    (Some(sw), Some(sh), Some(dx), Some(dy), Some(dw), Some(dh)) => {
                        data.surface.draw_image_crop_with_alpha_behavior(
                            &image,
                            (a*f64::from(image.width)/natural.0) as f32,
                            (b*f64::from(image.height)/natural.1) as f32,
                            (sw*f64::from(image.width)/natural.0) as f32,
                            (sh*f64::from(image.height)/natural.1) as f32,
                            dx as f32,
                            dy as f32,
                            dw as f32,
                            dh as f32,
                            premultiply_alpha,
                        )
                    }
                    _ => {
                        return Err(OpError::new(
                            "TypeError",
                            "drawImage expects 3, 5, or 9 arguments",
                        ));
                    }
                }
                .map_err(|_| {
                    OpError::new(
                        "InvalidStateError",
                        "drawImage could not rasterize the source",
                    )
                })?;
                self.publish_data(&data)
            }
        }
    };
}

impl_canvas_2d_methods!(DomCanvasRenderingContext2D);
impl_canvas_2d_methods!(DomOffscreenCanvasRenderingContext2D);
impl_canvas_2d_methods!(DomPaintRenderingContext2D);

fn new_gradient(ctx: &mut Ctx, gradient: CanvasGradient) -> OpResult<Value> {
    let id = NEXT_GRADIENT_ID.fetch_add(1, Ordering::Relaxed);
    let handle = Rc::new(GradientHandle { gradient });
    GRADIENTS.with(|gradients| {
        let mut gradients = gradients.borrow_mut();
        gradients.retain(|_, weak| weak.strong_count() > 0);
        gradients.insert(id, Rc::downgrade(&handle));
    });
    let value = ctx.new_instance(DomCanvasGradient { handle });
    ctx.set_member(&value, "__lumenCanvasGradientId", Value::Num(id as f64))
        .map_err(|_| OpError::new("TypeError", "could not initialize CanvasGradient"))?;
    Ok(value)
}

fn resolve_gradient(ctx: &mut Ctx, value: &Value) -> Option<(CanvasGradient, Value)> {
    let Value::Num(id) = ctx.get_member(value, "__lumenCanvasGradientId").ok()? else {
        return None;
    };
    if !id.is_finite() || id < 1.0 || id.fract() != 0.0 || id > u64::MAX as f64 {
        return None;
    }
    let handle = GRADIENTS.with(|gradients| {
        gradients
            .borrow()
            .get(&(id as u64))
            .and_then(std::rc::Weak::upgrade)
    })?;
    Some((handle.gradient.clone(), value.clone()))
}

fn new_pattern(ctx: &mut Ctx, pattern: CanvasPattern, origin_clean: bool) -> OpResult<Value> {
    let id = NEXT_GRADIENT_ID.fetch_add(1, Ordering::Relaxed);
    let handle = Rc::new(PatternHandle {
        pattern,
        origin_clean,
    });
    PATTERNS.with(|patterns| {
        let mut patterns = patterns.borrow_mut();
        patterns.retain(|_, weak| weak.strong_count() > 0);
        patterns.insert(id, Rc::downgrade(&handle));
    });
    let value = ctx.new_instance(DomCanvasPattern { handle });
    ctx.set_member(&value, "__lumenCanvasPatternId", Value::Num(id as f64))
        .map_err(|_| OpError::new("TypeError", "could not initialize CanvasPattern"))?;
    Ok(value)
}

fn resolve_pattern(ctx: &mut Ctx, value: &Value) -> Option<(CanvasPattern, Value)> {
    let Value::Num(id) = ctx.get_member(value, "__lumenCanvasPatternId").ok()? else {
        return None;
    };
    if !id.is_finite() || id < 1.0 || id.fract() != 0.0 || id > u64::MAX as f64 {
        return None;
    }
    let handle = PATTERNS.with(|patterns| {
        patterns
            .borrow()
            .get(&(id as u64))
            .and_then(std::rc::Weak::upgrade)
    })?;
    Some((handle.pattern.clone(), value.clone()))
}

#[lumen_bind::class(name = "CanvasGradient", hint(js(webidl)))]
struct DomCanvasGradient {
    handle: Rc<GradientHandle>,
}

#[lumen_bind::methods]
impl DomCanvasGradient {
    #[constructor]
    fn new() -> OpResult<Self> {
        Err(OpError::new(
            "TypeError",
            "CanvasGradient cannot be constructed directly",
        ))
    }
    #[method(name = "addColorStop")]
    fn add_color_stop(&self, offset: f64, color: &str) -> OpResult<()> {
        let value = Value::Str(color.into());
        // Gradients are canvas-neutral, so there is no element color to inherit.
        let color = parse_style_color(
            &value,
            Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            },
        )
        .ok_or_else(|| OpError::new("SyntaxError", "invalid gradient color"))?;
        self.handle
            .gradient
            .add_color_stop(offset as f32, color)
            .map_err(|_| {
                OpError::new(
                    "IndexSizeError",
                    "gradient stop offset must be between zero and one",
                )
            })
    }
}

#[lumen_bind::class(name = "CanvasPattern", hint(js(webidl)))]
struct DomCanvasPattern {
    handle: Rc<PatternHandle>,
}

#[lumen_bind::class(name = "ImageBitmap", hint(js(webidl)))]
struct DomImageBitmap {
    image: RefCell<Option<Rgba8Image>>,
    premultiply_alpha: bool,
    origin_clean: bool,
}

#[lumen_bind::methods]
impl DomImageBitmap {
    #[constructor]
    fn new() -> OpResult<Self> {
        Err(OpError::new(
            "TypeError",
            "ImageBitmap cannot be constructed directly",
        ))
    }
    #[getter]
    fn width(&self) -> u32 {
        self.image.borrow().as_ref().map_or(0, |image| image.width)
    }
    #[getter]
    fn height(&self) -> u32 {
        self.image.borrow().as_ref().map_or(0, |image| image.height)
    }
    #[getter]
    fn closed(&self) -> bool {
        self.image.borrow().is_none()
    }
    fn close(&self) {
        self.image.borrow_mut().take();
    }
}

fn new_image_bitmap(ctx: &mut Ctx, image: Rgba8Image, premultiply_alpha: bool) -> OpResult<Value> {
    new_image_bitmap_with_origin(ctx, image, premultiply_alpha, true)
}

fn new_image_bitmap_with_origin(
    ctx: &mut Ctx,
    image: Rgba8Image,
    premultiply_alpha: bool,
    origin_clean: bool,
) -> OpResult<Value> {
    if image.width == 0
        || image.height == 0
        || image.pixels.len() != image_data_bytes(image.width, image.height)?
    {
        return Err(OpError::new(
            "InvalidStateError",
            "ImageBitmap source is empty or invalid",
        ));
    }
    Ok(ctx.new_instance(DomImageBitmap {
        image: RefCell::new(Some(image)),
        premultiply_alpha,
        origin_clean,
    }))
}

#[lumen_bind::op(name = "createImageBitmap")]
fn create_image_bitmap(
    ctx: &mut Ctx,
    source: Value,
    sx: Option<Value>,
    sy: Option<Value>,
    sw: Option<Value>,
    sh: Option<Value>,
    options: Option<Value>,
) -> OpResult<Value> {
    let (sx, sy, sw, sh, options) = match sx {
        Some(value @ Value::Obj(_))
            if sy.is_none() && sw.is_none() && sh.is_none() && options.is_none() =>
        {
            (None, None, None, None, Some(value))
        }
        Some(Value::Obj(_)) => {
            return Err(OpError::new(
                "TypeError",
                "ImageBitmapOptions cannot be combined with a crop rectangle",
            ));
        }
        value => (
            bitmap_number(ctx, value)?,
            bitmap_number(ctx, sy)?,
            bitmap_number(ctx, sw)?,
            bitmap_number(ctx, sh)?,
            options,
        ),
    };
    let crop = parse_bitmap_crop(sx, sy, sw, sh)?;
    let options = parse_bitmap_options(ctx, options, crop)?;
    let array_buffer = ctx.get_member(&source, "arrayBuffer").ok();
    if let Some(array_buffer) = array_buffer.and_then(JsFunction::from_value) {
        let bytes_promise = array_buffer.call(ctx, source, &[])?;
        let then = ctx
            .get_member(&bytes_promise, "then")
            .map_err(|_| OpError::new("TypeError", "Blob.arrayBuffer did not return a promise"))?;
        let then = JsFunction::from_value(then).ok_or_else(|| {
            OpError::new("TypeError", "Blob.arrayBuffer did not return a promise")
        })?;
        let job = ctx.new_instance(DomBitmapDecodeJob { options });
        let decode = ctx
            .get_member(&job, "decode")
            .map_err(|_| OpError::new("Error", "ImageBitmap decoder is unavailable"))?;
        let decode = ctx
            .bind_function_this(decode, job)
            .map_err(OpError::thrown)?;
        return then.call(ctx, bytes_promise, &[decode]);
    }

    let origin_clean = canvas_source_origin_clean(ctx, &source);
    let image = if let Some(data) = ctx.instance_data::<DomImageData>(&source) {
        data.borrow().read_pixels(ctx)?
    } else {
        match canvas_source_image(ctx, source) {
            Ok(Some(image)) => image.pixels,
            Ok(None) => {
                return Ok(rejected_image_bitmap(
                    ctx,
                    "Image source is not fully decodable",
                ));
            }
            Err(error) if error.class() == "InvalidStateError" => {
                return Ok(rejected_image_bitmap(ctx, error.message()));
            }
            Err(error) => return Err(error),
        }
    };
    let bitmap = new_image_bitmap_with_origin(
        ctx,
        apply_bitmap_options(image, options)?,
        options.premultiply_alpha,
        origin_clean,
    )?;
    let promise = ctx
        .get_member(&ctx.global_object(), "Promise")
        .map_err(|_| OpError::new("Error", "Promise is unavailable"))?;
    let resolve = ctx
        .get_member(&promise, "resolve")
        .map_err(|_| OpError::new("Error", "Promise.resolve is unavailable"))?;
    JsFunction::from_value(resolve)
        .ok_or_else(|| OpError::new("Error", "Promise.resolve is unavailable"))?
        .call(ctx, promise, &[bitmap])
}

fn bitmap_number(ctx: &mut Ctx, value: Option<Value>) -> OpResult<Option<f64>> {
    let Some(value) = value.filter(|value| !matches!(value, Value::Undefined)) else {
        return Ok(None);
    };
    ctx.coerce_number(&value).map(Some).map_err(OpError::thrown)
}

#[derive(Clone, Copy)]
struct BitmapOptions {
    crop: Option<(i64, i64, i64, i64)>,
    resize_width: Option<u32>,
    resize_height: Option<u32>,
    quality: lumen_html_image::canvas::ImageResizeQuality,
    flip_y: bool,
    from_image_orientation: bool,
    premultiply_alpha: bool,
    convert_color_space: bool,
}

fn parse_bitmap_options(
    ctx: &mut Ctx,
    options: Option<Value>,
    crop: Option<(i64, i64, i64, i64)>,
) -> OpResult<BitmapOptions> {
    let mut parsed = BitmapOptions {
        crop,
        resize_width: None,
        resize_height: None,
        quality: lumen_html_image::canvas::ImageResizeQuality::Low,
        flip_y: false,
        from_image_orientation: true,
        premultiply_alpha: true,
        convert_color_space: true,
    };
    let Some(options) = options.filter(|value| !matches!(value, Value::Null | Value::Undefined))
    else {
        return Ok(parsed);
    };
    let read_dimension = |ctx: &mut Ctx, key: &str| -> OpResult<Option<u32>> {
        let value = ctx.get_member(&options, key).map_err(|_| {
            OpError::new(
                "TypeError",
                format!("ImageBitmapOptions.{key} getter failed"),
            )
        })?;
        if matches!(value, Value::Undefined) {
            return Ok(None);
        }
        let value = ctx.coerce_number(&value).map_err(OpError::thrown)?;
        if !value.is_finite() || value < 0.0 || value > u32::MAX as f64 {
            return Err(OpError::new(
                "RangeError",
                format!("{key} must be a valid image dimension"),
            ));
        }
        let value = value.trunc() as u32;
        if value == 0 {
            return Err(OpError::new(
                "InvalidStateError",
                format!("{key} must be greater than zero"),
            ));
        }
        Ok(Some(value))
    };
    parsed.resize_width = read_dimension(ctx, "resizeWidth")?;
    parsed.resize_height = read_dimension(ctx, "resizeHeight")?;
    let quality = ctx.get_member(&options, "resizeQuality").map_err(|_| {
        OpError::new(
            "TypeError",
            "ImageBitmapOptions.resizeQuality getter failed",
        )
    })?;
    parsed.quality = match quality {
        Value::Undefined => lumen_html_image::canvas::ImageResizeQuality::Low,
        Value::Str(value) if value.as_str() == "pixelated" => {
            lumen_html_image::canvas::ImageResizeQuality::Pixelated
        }
        Value::Str(value) if value.as_str() == "low" => {
            lumen_html_image::canvas::ImageResizeQuality::Low
        }
        Value::Str(value) if value.as_str() == "medium" => {
            lumen_html_image::canvas::ImageResizeQuality::Medium
        }
        Value::Str(value) if value.as_str() == "high" => {
            lumen_html_image::canvas::ImageResizeQuality::High
        }
        _ => {
            return Err(OpError::new(
                "TypeError",
                "resizeQuality must be pixelated, low, medium or high",
            ));
        }
    };
    let orientation = ctx.get_member(&options, "imageOrientation").map_err(|_| {
        OpError::new(
            "TypeError",
            "ImageBitmapOptions.imageOrientation getter failed",
        )
    })?;
    match orientation {
        Value::Undefined => {}
        Value::Str(value) if value.as_str() == "from-image" => {
            parsed.from_image_orientation = true;
        }
        Value::Str(value) if value.as_str() == "flipY" => {
            parsed.flip_y = true;
            parsed.from_image_orientation = false;
        }
        _ => {
            return Err(OpError::new(
                "TypeError",
                "imageOrientation must be from-image or flipY",
            ));
        }
    }
    for key in ["premultiplyAlpha", "colorSpaceConversion"] {
        let value = ctx.get_member(&options, key).map_err(|_| {
            OpError::new(
                "TypeError",
                format!("ImageBitmapOptions.{key} getter failed"),
            )
        })?;
        let allowed: &[&str] = if key == "premultiplyAlpha" {
            &["default", "none", "premultiply"]
        } else {
            &["default", "none"]
        };
        match (key, value) {
            (_, Value::Undefined) => {}
            (_, Value::Str(value)) if value.as_str() == "default" => {}
            ("premultiplyAlpha", Value::Str(value)) if value.as_str() == "premultiply" => {
                parsed.premultiply_alpha = true;
            }
            ("premultiplyAlpha", Value::Str(value)) if value.as_str() == "none" => {
                parsed.premultiply_alpha = false;
            }
            ("colorSpaceConversion", Value::Str(value)) if value.as_str() == "none" => {
                parsed.convert_color_space = false;
            }
            (key, Value::Str(value)) if allowed.contains(&value.as_str()) => {
                return Err(OpError::new(
                    "NotSupportedError",
                    format!("ImageBitmapOptions.{key} transformation is unavailable"),
                ));
            }
            _ => {
                return Err(OpError::new(
                    "TypeError",
                    format!("invalid ImageBitmapOptions.{key}"),
                ));
            }
        }
    }
    Ok(parsed)
}

fn apply_bitmap_options(mut image: Rgba8Image, options: BitmapOptions) -> OpResult<Rgba8Image> {
    image = apply_bitmap_crop(image, options.crop)?;
    let resize = match (options.resize_width, options.resize_height) {
        (Some(width), Some(height)) => Some((width, height)),
        (Some(width), None) => Some((
            width,
            ((f64::from(image.height) * f64::from(width) / f64::from(image.width))
                .ceil()
                .max(1.0)) as u32,
        )),
        (None, Some(height)) => Some((
            ((f64::from(image.width) * f64::from(height) / f64::from(image.height))
                .ceil()
                .max(1.0)) as u32,
            height,
        )),
        (None, None) => None,
    };
    if let Some((width, height)) = resize {
        image = if options.premultiply_alpha {
            lumen_html_image::canvas::resize_image_with_quality(
                &image,
                width,
                height,
                options.quality,
            )
        } else {
            lumen_html_image::canvas::resize_image_straight_with_quality(
                &image,
                width,
                height,
                options.quality,
            )
        }
        .map_err(|_| OpError::new("InvalidStateError", "ImageBitmap resize failed"))?;
    }
    if options.flip_y {
        let row_bytes = image.width as usize * 4;
        for y in 0..image.height as usize / 2 {
            let opposite = image.height as usize - 1 - y;
            let (head, tail) = image.pixels.split_at_mut(opposite * row_bytes);
            head[y * row_bytes..(y + 1) * row_bytes].swap_with_slice(&mut tail[..row_bytes]);
        }
    }
    Ok(image)
}

fn parse_bitmap_crop(
    sx: Option<f64>,
    sy: Option<f64>,
    sw: Option<f64>,
    sh: Option<f64>,
) -> OpResult<Option<(i64, i64, i64, i64)>> {
    match (sx, sy, sw, sh) {
        (None, None, None, None) => Ok(None),
        (Some(sx), Some(sy), Some(sw), Some(sh)) => {
            let values = [sx, sy, sw, sh];
            if !values.iter().all(|value| value.is_finite()) {
                return Err(OpError::new("TypeError", "crop rectangle must be finite"));
            }
            let (mut sx, mut sy, mut sw, mut sh) = (
                sx.trunc() as i64,
                sy.trunc() as i64,
                sw.trunc() as i64,
                sh.trunc() as i64,
            );
            if sw < 0 {
                sx = sx.saturating_add(sw);
                sw = sw.saturating_neg();
            }
            if sh < 0 {
                sy = sy.saturating_add(sh);
                sh = sh.saturating_neg();
            }
            if sw == 0 || sh == 0 {
                return Err(OpError::new("InvalidStateError", "crop rectangle is empty"));
            }
            Ok(Some((sx, sy, sw, sh)))
        }
        _ => Err(OpError::new(
            "TypeError",
            "createImageBitmap crop requires sx, sy, sw and sh",
        )),
    }
}

fn apply_bitmap_crop(
    image: Rgba8Image,
    crop: Option<(i64, i64, i64, i64)>,
) -> OpResult<Rgba8Image> {
    let Some((sx, sy, sw, sh)) = crop else {
        return Ok(image);
    };
    let width = u32::try_from(sw)
        .map_err(|_| OpError::new("InvalidStateError", "crop width is too large"))?;
    let height = u32::try_from(sh)
        .map_err(|_| OpError::new("InvalidStateError", "crop height is too large"))?;
    let bytes = image_data_bytes(width, height)?;
    let mut pixels = vec![0; bytes];
    let left = sx.max(0).min(i64::from(image.width));
    let top = sy.max(0).min(i64::from(image.height));
    let right = sx.saturating_add(sw).max(0).min(i64::from(image.width));
    let bottom = sy.saturating_add(sh).max(0).min(i64::from(image.height));
    if right > left && bottom > top {
        let copy_width = (right - left) as usize;
        let copy_height = (bottom - top) as usize;
        let destination_x = left.saturating_sub(sx) as usize;
        let destination_y = top.saturating_sub(sy) as usize;
        for row in 0..copy_height {
            let src = ((top as usize + row) * image.width as usize + left as usize) * 4;
            let dst = ((destination_y + row) * width as usize + destination_x) * 4;
            let len = copy_width * 4;
            pixels[dst..dst + len].copy_from_slice(&image.pixels[src..src + len]);
        }
    }
    Ok(Rgba8Image {
        width,
        height,
        pixels,
    })
}

fn decode_image_bitmap_inner(
    ctx: &mut Ctx,
    array_buffer: Value,
    options: BitmapOptions,
) -> OpResult<Value> {
    let bytes = ctx
        .buffer_source_bytes(&array_buffer)
        .ok_or_else(|| OpError::new("TypeError", "Blob data is not an ArrayBuffer"))?;
    let icc_profile = if options.convert_color_space {
        lumen_html_image::raster_image_icc_profile(&bytes).map_err(|_| {
            error_reporting::dom_exception(
                ctx,
                "InvalidStateError",
                "createImageBitmap could not read the raster color profile",
            )
        })?
    } else {
        None
    };
    let mut image = lumen_html_image::decode_raster_image_with_orientation(
        &bytes,
        options.from_image_orientation,
    )
    .map_err(|_| {
        error_reporting::dom_exception(
            ctx,
            "InvalidStateError",
            "createImageBitmap could not decode the raster image",
        )
    })?;
    if let Some(icc_profile) = icc_profile {
        lumen_html_image::raster_image_convert_to_srgb(&mut image, &icc_profile).map_err(|_| {
            OpError::new(
                "NotSupportedError",
                "embedded raster color profile cannot be converted to sRGB",
            )
        })?;
    }
    new_image_bitmap(
        ctx,
        apply_bitmap_options(image, options)?,
        options.premultiply_alpha,
    )
}

#[lumen_bind::class(name = "__LumenBitmapDecodeJob")]
struct DomBitmapDecodeJob {
    options: BitmapOptions,
}

#[lumen_bind::methods]
impl DomBitmapDecodeJob {
    #[constructor]
    fn new() -> OpResult<Self> {
        Err(OpError::new(
            "TypeError",
            "ImageBitmap decoder jobs are internal",
        ))
    }

    #[method(name = "decode")]
    fn decode(&self, ctx: &mut Ctx, array_buffer: Value) -> OpResult<Value> {
        let deferred = Deferred::new(ctx);
        let promise = deferred.promise();
        let options = self.options;
        // Blob reading completes through Promise jobs, but bitmap creation and
        // rejection settle in a genuine host task, retaining its owner realm.
        scheduling::queue_task(ctx, move |ctx| {
            match decode_image_bitmap_inner(ctx, array_buffer, options) {
                Ok(bitmap) => deferred.resolve(ctx, bitmap),
                Err(error) => deferred.reject(ctx, error),
            }
            Ok(())
        })?;
        Ok(promise)
    }
}

#[lumen_bind::methods]
impl DomCanvasPattern {
    #[constructor]
    fn new() -> OpResult<Self> {
        Err(OpError::new(
            "TypeError",
            "CanvasPattern cannot be constructed directly",
        ))
    }
    #[method(name = "setTransform")]
    fn set_transform(&self, ctx: &mut Ctx, transform: Option<Value>) -> OpResult<()> {
        let transform = match transform {
            Some(value) => parse_dom_matrix(ctx, &value)?,
            None => Transform::identity(),
        };
        self.handle.pattern.set_transform(transform);
        Ok(())
    }

    #[getter]
    fn repetition(&self) -> &'static str {
        match self.handle.pattern.repetition() {
            CanvasPatternRepetition::Repeat => "repeat",
            CanvasPatternRepetition::RepeatX => "repeat-x",
            CanvasPatternRepetition::RepeatY => "repeat-y",
            CanvasPatternRepetition::NoRepeat => "no-repeat",
        }
    }
}

fn rejected_image_bitmap(ctx: &mut Ctx, message: &str) -> Value {
    let deferred = Deferred::new(ctx);
    let promise = deferred.promise();
    deferred.reject(ctx, OpError::new("InvalidStateError", message.to_owned()));
    promise
}

/// Original decoded pixels and their coordinate-space dimensions are kept
/// together only for this consumer operation, never as a second image cache.
struct CanvasSourceImage {pixels:Rgba8Image,natural:(f64,f64)}
fn canvas_source_image(ctx:&mut Ctx,source:Value)->OpResult<Option<CanvasSourceImage>> {
    let natural=ctx.with_instance::<DomHtmlImageElement,_>(&source,|image|image.image_snapshot().natural_size).ok().flatten();
    Ok(canvas_source_pixels(ctx,source)?.map(|pixels|{
        let natural=natural.unwrap_or((f64::from(pixels.width),f64::from(pixels.height)));
        CanvasSourceImage{pixels,natural}
    }))
}

fn canvas_source_pixels(ctx: &mut Ctx, source: Value) -> OpResult<Option<Rgba8Image>> {
    if let Ok(image)=ctx.with_instance::<DomElementImage,_>(&source,|image|image.image.borrow().clone()) {
        return image.map(|image|Some(image.image.clone())).ok_or_else(||OpError::new("InvalidStateError","ElementImage is detached"));
    }
    if let Ok(image) = ctx.with_instance::<DomCanvasElement, _>(&source, |canvas| {
        canvas
            .data()
            .and_then(|data| CanvasData::snapshot_for_read(&data))
    }) {
        return nonempty_canvas_snapshot(image?);
    }
    if let Ok(image) = ctx.with_instance::<DomOffscreenCanvas, _>(&source, |canvas| {
        canvas.ensure_attached()?;
        CanvasData::snapshot_for_read(&canvas.data)
    }) {
        return nonempty_canvas_snapshot(image?);
    }
    if let Some(bitmap) = ctx.instance_data::<DomImageBitmap>(&source) {
        return bitmap
            .borrow()
            .image
            .borrow()
            .clone()
            .map(Some)
            .ok_or_else(|| OpError::new("InvalidStateError", "ImageBitmap is closed"));
    }
    if let Ok((realm, node)) = ctx.with_instance::<DomHtmlImageElement, _>(&source, |image| {
        let node = image.image_node();
        (node.realm.clone(), node.id)
    }) {
        return match realm.images.canvas_image_state(node) {
            image_loading::CanvasImageState::Available(image, _) => Ok(Some(Rgba8Image {
                width: image.width,
                height: image.height,
                pixels: image.pixels.clone(),
            })),
            image_loading::CanvasImageState::Unavailable => Ok(None),
            image_loading::CanvasImageState::Broken => Err(OpError::new(
                "InvalidStateError",
                "image source has a broken current request",
            )),
        };
    }
    if let Ok((realm, node)) = ctx
        .with_instance::<crate::media::DomHtmlVideoElement, _>(&source, |video| {
            (video.node().realm.clone(), video.node().id)
        })
    {
        let frame = realm
            .media_video_frame(node)
            .ok_or_else(|| OpError::new("InvalidStateError", "video has no current frame"))?;
        return Ok(Some(Rgba8Image {
            width: frame.width,
            height: frame.height,
            pixels: frame.rgba.as_ref().clone(),
        }));
    }
    let get = |ctx: &mut Ctx, object: &Value, name: &str| {
        ctx.get_member(object, name)
            .map_err(|_| OpError::new("TypeError", "drawImage source is unavailable"))
    };
    let width = match get(ctx, &source, "width")? {
        Value::Num(value) if value.is_finite() && value >= 0.0 && value <= u32::MAX as f64 => {
            value as u32
        }
        _ => {
            return Err(OpError::new(
                "TypeError",
                "drawImage source must be a canvas",
            ));
        }
    };
    let height = match get(ctx, &source, "height")? {
        Value::Num(value) if value.is_finite() && value >= 0.0 && value <= u32::MAX as f64 => {
            value as u32
        }
        _ => {
            return Err(OpError::new(
                "TypeError",
                "drawImage source must be a canvas",
            ));
        }
    };
    if width == 0 || height == 0 {
        return Err(OpError::new(
            "InvalidStateError",
            "canvas image source has zero dimensions",
        ));
    }
    let get_context = get(ctx, &source, "getContext")?;
    let get_context = JsFunction::from_value(get_context)
        .ok_or_else(|| OpError::new("TypeError", "drawImage source must be a canvas"))?;
    let context = get_context.call(ctx, source.clone(), &[Value::Str("2d".into())])?;
    let get_pixels = get(ctx, &context, "getImageData")?;
    let get_pixels = JsFunction::from_value(get_pixels)
        .ok_or_else(|| OpError::new("InvalidStateError", "canvas source has no 2D context"))?;
    let pixels = get_pixels.call(
        ctx,
        context,
        &[
            Value::Num(0.0),
            Value::Num(0.0),
            Value::Num(width as f64),
            Value::Num(height as f64),
        ],
    )?;
    let data = get(ctx, &pixels, "data")?;
    let bytes = ctx
        .typed_array_bytes(&data)
        .ok_or_else(|| OpError::new("InvalidStateError", "canvas source pixels are unavailable"))?;
    if bytes.len() != width as usize * height as usize * 4 {
        return Err(OpError::new(
            "InvalidStateError",
            "canvas source pixels have an invalid length",
        ));
    }
    Ok(Some(Rgba8Image {
        width,
        height,
        pixels: bytes,
    }))
}

fn nonempty_canvas_snapshot(image: Rgba8Image) -> OpResult<Option<Rgba8Image>> {
    if image.width == 0 || image.height == 0 {
        Err(OpError::new(
            "InvalidStateError",
            "canvas image source has zero dimensions",
        ))
    } else {
        Ok(Some(image))
    }
}

fn ensure_origin_clean(data: &CanvasData) -> OpResult<()> {
    if data.origin_clean {
        Ok(())
    } else {
        Err(OpError::new("SecurityError", "canvas is not origin-clean"))
    }
}

fn canvas_source_origin_clean(ctx: &mut Ctx, source: &Value) -> bool {
    if let Ok(clean)=ctx.with_instance::<DomElementImage,_>(source,|image|image.origin_clean) {return clean;}
    if let Some(bitmap) = ctx.instance_data::<DomImageBitmap>(source) {
        return bitmap.borrow().origin_clean;
    }
    if let Ok((realm, node)) = ctx.with_instance::<DomHtmlImageElement, _>(source, |image| {
        let node = image.image_node();
        (node.realm.clone(), node.id)
    }) {
        return matches!(
            realm.images.canvas_image_state(node),
            image_loading::CanvasImageState::Available(_, true)
        );
    }
    if let Ok((realm, node)) = ctx
        .with_instance::<crate::media::DomHtmlVideoElement, _>(source, |video| {
            (video.node().realm.clone(), video.node().id)
        })
    {
        return realm.media_snapshot(node).origin_clean;
    }
    if let Ok(Some(clean)) = ctx.with_instance::<DomCanvasElement, _>(source, |canvas| {
        canvas.data().ok().map(|data| data.borrow().origin_clean)
    }) {
        return clean;
    }
    if let Ok(clean) = ctx
        .with_instance::<DomOffscreenCanvas, _>(source, |canvas| canvas.data.borrow().origin_clean)
    {
        return clean;
    }
    if let Some(data) = ctx.instance_data::<DomImageData>(source) {
        let _ = data;
        return true;
    }
    true
}

fn pattern_origin_clean(ctx: &mut Ctx, pattern: &Value) -> Option<bool> {
    ctx.instance_data::<DomCanvasPattern>(pattern)
        .map(|pattern| pattern.borrow().handle.origin_clean)
}

macro_rules! impl_canvas_2d_helpers {
    ($context:ident) => {
        impl $context {
            fn publish_data(&self, data: &CanvasData) -> OpResult<()> {
                data.ensure_bitmap_available()?;
                publish(self.realm.as_ref(), self.node, data)
            }

            fn transform(&self, transform: Transform) {
                let mut data = self.data.borrow_mut();
                let current = data.surface.state().transform;
                data.surface.state_mut().transform = current.pre_concat(transform);
            }
        }
    };
}

impl_canvas_2d_helpers!(DomCanvasRenderingContext2D);
impl_canvas_2d_helpers!(DomOffscreenCanvasRenderingContext2D);
impl_canvas_2d_helpers!(DomPaintRenderingContext2D);

#[derive(Clone, Copy, Default)]
struct CanvasTextMetricsData {
    width: f64,
    actual_left: f64,
    actual_right: f64,
    actual_ascent: f64,
    actual_descent: f64,
    font_ascent: f64,
    font_descent: f64,
}

#[lumen_bind::class(name = "TextMetrics", hint(js(webidl)))]
struct DomTextMetrics {
    metrics: CanvasTextMetricsData,
}

#[lumen_bind::methods]
impl DomTextMetrics {
    fn new() -> OpResult<Self> {
        Err(OpError::new(
            "TypeError",
            "TextMetrics cannot be constructed directly",
        ))
    }
    #[getter]
    fn width(&self) -> f64 {
        self.metrics.width
    }
    #[getter]
    fn actual_bounding_box_left(&self) -> f64 {
        self.metrics.actual_left
    }
    #[getter]
    fn actual_bounding_box_right(&self) -> f64 {
        self.metrics.actual_right
    }
    #[getter]
    fn actual_bounding_box_ascent(&self) -> f64 {
        self.metrics.actual_ascent
    }
    #[getter]
    fn actual_bounding_box_descent(&self) -> f64 {
        self.metrics.actual_descent
    }
    #[getter]
    fn font_bounding_box_ascent(&self) -> f64 {
        self.metrics.font_ascent
    }
    #[getter]
    fn font_bounding_box_descent(&self) -> f64 {
        self.metrics.font_descent
    }
    #[getter]
    fn em_height_ascent(&self) -> f64 {
        self.metrics.font_ascent
    }
    #[getter]
    fn em_height_descent(&self) -> f64 {
        self.metrics.font_descent
    }
}

fn canvas_node_font_source(ctx:&mut Ctx,realm:Option<&Rc<DomRealm>>,node:Option<NodeId>)->OpResult<CanvasFontSource> {
    if let (Some(realm),Some(node))=(realm,node) {
        let (owner,_)=realm.resolve_adopted_node(node);
        canvas_font_source(ctx,Some(&owner))
    } else {canvas_font_source(ctx,realm)}
}

fn canvas_font_source(ctx: &mut Ctx, realm: Option<&Rc<DomRealm>>) -> OpResult<CanvasFontSource> {
    let Some(realm) = realm else {
        if let Some(worker) = ctx.op_state().get::<Rc<font_loading::WorkerFontContext>>().cloned() {
            let fonts = worker.canvas_font_set(canvas_fallback_fonts())
                .map_err(|error| OpError::new("InvalidStateError", error))?;
            return Ok(CanvasFontSource::realm(fonts,font_loading::WeakFontRealm::Worker(Rc::downgrade(&worker)),RenderFontFallback::Static(canvas_fallback_fonts())));
        }
        return Ok(CanvasFontSource::Static(canvas_fallback_fonts()));
    };
    realm_font_source(realm)
}

pub(crate) fn realm_font_source(realm: &Rc<DomRealm>) -> OpResult<CanvasFontSource> {
    if !realm
        .font_loading
        .has_canvas_font_source()
        .map_err(|error| OpError::new("InvalidStateError", error))?
    {
        let source=CanvasFontSource::Static(canvas_fallback_fonts());
        realm.font_loading.canvas_font_source_initialized.set(true);
        return Ok(source);
    }
    let key = {
        let mut session = realm.session.borrow_mut();
        let generation = session.font_face_generation().map_err(|error| {
            OpError::new("InvalidStateError", format!("font stylesheet: {error:?}"))
        })?;
        (generation, session.media_environment())
    };
    if realm.font_loading.canvas_css_key.get() != Some(key) {
        let css_faces = {
            let mut session = realm.session.borrow_mut();
            let environment = session.media_environment();
            let mut faces = session.font_faces().map_err(|error| {
                OpError::new("InvalidStateError", format!("font stylesheet: {error:?}"))
            })?;
            faces.retain(|face| face.applies(environment));
            faces
        };
        realm.font_loading.replace_document_css_faces(&css_faces)?;
        realm.font_loading.canvas_css_key.set(Some(key));
    }
    let document_base = realm.base_url();
    let descriptor_query = {
        let mut session = realm.session.borrow_mut();
        let root = session.document().root();
        session.query_container_context(root).map_err(|error| OpError::new("InvalidStateError", format!("font descriptor query context: {error:?}")))?
    };
    let installed=realm.font_loading.render_fallback.borrow().clone();
    let fallback=match installed.as_deref() {
        Some(fonts) => fonts,
        None => canvas_fallback_fonts(),
    };
    let fonts = realm
        .font_loading
        .canvas_font_set_with_query(fallback, &document_base, descriptor_query)
        .map_err(|error| OpError::new("InvalidStateError", error))?;
    realm.font_loading.canvas_font_source_initialized.set(true);
    Ok(CanvasFontSource::realm(fonts,font_loading::WeakFontRealm::Document(Rc::downgrade(realm)),
        installed.map_or_else(||RenderFontFallback::Static(canvas_fallback_fonts()),RenderFontFallback::Owned)))
}

/// Host renderers may supply their existing platform registry and an already
/// captured document view without re-entering the Session during shaping.
pub(crate) fn render_font_source_from_snapshot(realm:&Rc<DomRealm>,fallback:Arc<FontSet>,
    rules:&[lumen_html::css::FontFaceRule],document_base:&str,query:lumen_html::css::ContainerUnitContext)->OpResult<CanvasFontSource> {
    realm.font_loading.replace_document_css_faces(rules)?;
    *realm.font_loading.render_fallback.borrow_mut()=Some(fallback.clone());
    let fonts=realm.font_loading.canvas_font_set_with_query(&fallback,document_base,query)
        .map_err(|error|OpError::new("InvalidStateError",error))?;
    realm.font_loading.canvas_font_source_initialized.set(true);
    Ok(CanvasFontSource::realm(fonts,font_loading::WeakFontRealm::Document(Rc::downgrade(realm)),
        RenderFontFallback::Owned(fallback)))
}

pub(crate) fn initialized_realm_font_source(realm:&Rc<DomRealm>)->OpResult<Option<CanvasFontSource>> {
    if realm.font_loading.canvas_font_source_initialized.get(){realm_font_source(realm).map(Some)}
    else{Ok(None)}
}

pub(crate) fn canvas_fallback_fonts() -> &'static FontSet {
    CANVAS_FONTS.get_or_init(|| {
        let mono = Arc::new(
            FontFace::new(Arc::from(DEFAULT_FONT_BYTES))
                .expect("bundled Inconsolata font is valid"),
        );
        let sans = Arc::new(
            FontFace::new(Arc::from(TEST_FONT_BYTES))
                .expect("bundled Liberation Sans font is valid"),
        );
        let sans_bold = Arc::new(
            FontFace::new(Arc::from(TEST_FONT_BOLD_BYTES))
                .expect("bundled Liberation Sans bold font is valid"),
        );
        let mut faces = Vec::new();
        for family in ["monospace", "Inconsolata"] {
            faces.push(RegisteredFont {
                family: Arc::from(family),
                weight: 400,
                stretch: 100.0,
                style: FontStyle::Normal,
                face: mono.clone(),
            });
        }
        for family in ["sans-serif", "serif", "Liberation Sans"] {
            faces.push(RegisteredFont {
                family: Arc::from(family),
                weight: 400,
                stretch: 100.0,
                style: FontStyle::Normal,
                face: sans.clone(),
            });
            faces.push(RegisteredFont {
                family: Arc::from(family),
                weight: 700,
                stretch: 100.0,
                style: FontStyle::Normal,
                face: sans_bold.clone(),
            });
        }
        FontSet::new(faces).expect("bundled canvas fonts form a valid font set")
    })
}

fn canvas_inherited_direction(realm: Option<&Rc<DomRealm>>, node: Option<NodeId>) -> bool {
    let Some(realm) = realm else { return false; };
    realm.with_session(|session| {
        let node = node.or_else(|| selector::query_selector(session.document(), session.document().root(), "html").ok().flatten());
        node.and_then(|node| session.computed_style(node).ok())
            .is_some_and(|style| style.direction == lumen_html::css::Direction::Rtl)
    })
}

fn resolved_canvas_direction(data: &CanvasData, realm: Option<&Rc<DomRealm>>, node: Option<NodeId>) -> bool {
    match data.text.direction.as_str() {
        "rtl" => true,
        "ltr" => false,
        _ if data.transferred || node.is_none() => data.inherited_direction_rtl,
        _ => canvas_inherited_direction(realm, node),
    }
}

fn canvas_inherited_font(realm: Option<&Rc<DomRealm>>, node: Option<NodeId>) -> (f32, FontSpec) {
    if let (Some(realm), Some(node)) = (realm, node) {
        if realm.has_browsing_context {
            let style = realm.with_session(|session| {
                if !script_loading::is_connected(session.document(), node) { return None; }
                let mut current = node;
                let source = session.computed_style(node).ok()?;
                loop {
                    if current == session.document().root() { return Some(source); }
                    if matches!(session.document().kind(current), Ok(NodeKind::Element { .. }))
                        && (session.computed_style(current).ok()?.display == lumen_html::css::Display::None
                            || current == node && source.display == lumen_html::css::Display::Contents)
                    { return None; }
                    current = session.document().composed_parent(current).ok()??;
                }
            });
            if let Some(style) = style { return (style.font_size, style.font.clone()); }
        }
    }
    // Disconnected/hidden elements and OffscreenCanvas use the initial font,
    // even if a previous assignment selected a larger canvas font.
    (10.0, FontSpec { families: Some(Arc::from([lumen_html::paint::FontFamily::Generic(lumen_html::paint::GenericFontFamily::SansSerif)])), ..FontSpec::default() })
}

fn parse_stretch(value: &str) -> Option<f32> {
    let named = match value {
        "ultra-condensed" => Some(50.0),
        "extra-condensed" => Some(62.5),
        "condensed" => Some(75.0),
        "semi-condensed" => Some(87.5),
        "normal" => Some(100.0),
        "semi-expanded" => Some(112.5),
        "expanded" => Some(125.0),
        "extra-expanded" => Some(150.0),
        "ultra-expanded" => Some(200.0),
        _ => None,
    };
    if let Some(value) = named {
        return Some(value);
    }
    let value = value.strip_suffix('%')?.parse::<f32>().ok()?;
    (value.is_finite() && (0.0..=1000.0).contains(&value)).then_some(value)
}

fn stretch_value(stretch: f32) -> String {
    match stretch {
        50.0 => "ultra-condensed".into(),
        62.5 => "extra-condensed".into(),
        75.0 => "condensed".into(),
        87.5 => "semi-condensed".into(),
        100.0 => "normal".into(),
        112.5 => "semi-expanded".into(),
        125.0 => "expanded".into(),
        150.0 => "extra-expanded".into(),
        200.0 => "ultra-expanded".into(),
        _ => format!("{}%", stretch),
    }
}

fn parse_css_spacing(value: &str, size: f32, root_font: f32, units: [lumen_html::css::FontUnitBases;2]) -> Option<f32> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("normal") { return Some(0.0); }
    let value = lumen_html::css::parse_text_length_with_font_units(value,size,root_font,units)?;
    (value.is_finite() && value.abs() <= 32_768.0).then_some(value)
}

fn canvas_root_font_units(realm: Option<&Rc<DomRealm>>, fonts: &dyn TextShaper) -> OpResult<(f32,lumen_html::css::FontUnitBases)> {
    let initial = || (16.0,lumen_html::css::font_unit_bases(Some(fonts),16.0,&FontSpec::default(),lumen_html::css::LineHeight::Normal,false,false));
    let Some(realm) = realm else { return Ok(initial()); };
    realm.with_session(|session| {
        let root=session.document().document_element_at(session.document().root()).map_err(dom_error)?;
        let Some(root)=root else { return Ok(initial()); };
        let style=session.computed_style_with_text(root,Some(fonts)).map_err(|error|OpError::new("InvalidStateError",format!("root font units unavailable: {error:?}")))?;
        Ok((style.font_size,style.font_unit_context(fonts)[0]))
    })
}

fn css_spacing_string(value: f32) -> String {
    if value == 0.0 {
        "0px".into()
    } else {
        format!("{value}px")
    }
}

fn text_metrics(
    font: &dyn FontProvider,
    run: &lumen_html::paint::ShapedRun,
    size: f32,
    spec: &FontSpec,
) -> OpResult<CanvasTextMetricsData> {
    let mut metrics = CanvasTextMetricsData {
        width: f64::from(run.width),
        font_ascent: f64::from(font.ascent_styled(size, spec)),
        font_descent: f64::from(
            (font.line_height_styled(size, spec) - font.ascent_styled(size, spec)).max(0.0),
        ),
        ..CanvasTextMetricsData::default()
    };
    let (mut left, mut right, mut top, mut bottom) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for glyph in run.glyphs.iter() {
        let coverage = font
            .rasterize_glyph(glyph.face, glyph.id, size * glyph.size_scale)
            .map_err(|error| {
                OpError::new(
                    "InvalidStateError",
                    format!("font rasterization failed: {error}"),
                )
            })?;
        if coverage.width == 0 || coverage.height == 0 {
            continue;
        }
        let glyph_left = glyph.x + coverage.x_min as f32;
        let glyph_right = glyph_left + coverage.width as f32;
        let glyph_top = -glyph.y - coverage.y_min as f32 - coverage.height as f32;
        let glyph_bottom = glyph_top + coverage.height as f32;
        left = left.min(glyph_left);
        right = right.max(glyph_right);
        top = top.min(glyph_top);
        bottom = bottom.max(glyph_bottom);
    }
    metrics.actual_left = f64::from((-left).max(0.0));
    metrics.actual_right = f64::from(right.max(0.0));
    metrics.actual_ascent = f64::from((-top).max(0.0));
    metrics.actual_descent = f64::from(bottom.max(0.0));
    Ok(metrics)
}

fn current_canvas_color(realm: &Option<Rc<DomRealm>>, node: Option<NodeId>) -> Rgba {
    const BLACK: Rgba = Rgba {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    };
    let (Some(realm), Some(node)) = (realm, node) else {
        return BLACK;
    };
    realm.with_session(|session| {
        session
            .computed_style(node)
            .ok()
            .map_or(BLACK, |style| style.color)
    })
}

fn parse_style_color(value: &Value, current: Rgba) -> Option<[u8; 4]> {
    let Value::Str(value) = value else {
        return None;
    };
    let color = lumen_html::css::parse_animation_color(value.as_str(), current)?;
    Some([color.r, color.g, color.b, color.a])
}

fn color_string(color: [u8; 4]) -> String {
    if color[3] == 255 {
        format!("rgb({}, {}, {})", color[0], color[1], color[2])
    } else {
        format!(
            "rgba({}, {}, {}, {})",
            color[0],
            color[1],
            color[2],
            color[3] as f32 / 255.0
        )
    }
}

fn composite_name(mode: BlendMode) -> &'static str {
    match mode {
        BlendMode::Clear => "copy",
        BlendMode::Source => "copy",
        BlendMode::Destination => "destination-over",
        BlendMode::DestinationOver => "destination-over",
        BlendMode::SourceIn => "source-in",
        BlendMode::DestinationIn => "destination-in",
        BlendMode::SourceOut => "source-out",
        BlendMode::DestinationOut => "destination-out",
        BlendMode::SourceAtop => "source-atop",
        BlendMode::DestinationAtop => "destination-atop",
        BlendMode::Xor => "xor",
        BlendMode::Plus => "lighter",
        BlendMode::Multiply => "multiply",
        BlendMode::Screen => "screen",
        BlendMode::Overlay => "overlay",
        BlendMode::Darken => "darken",
        BlendMode::Lighten => "lighten",
        BlendMode::ColorDodge => "color-dodge",
        BlendMode::ColorBurn => "color-burn",
        BlendMode::HardLight => "hard-light",
        BlendMode::SoftLight => "soft-light",
        BlendMode::Difference => "difference",
        BlendMode::Exclusion => "exclusion",
        BlendMode::Hue => "hue",
        BlendMode::Saturation => "saturation",
        BlendMode::Color => "color",
        BlendMode::Luminosity => "luminosity",
        _ => "source-over",
    }
}

fn composite_mode(name: &str) -> Option<BlendMode> {
    Some(match name {
        "source-over" => BlendMode::SourceOver,
        "copy" => BlendMode::Source,
        "destination-over" => BlendMode::DestinationOver,
        "source-in" => BlendMode::SourceIn,
        "destination-in" => BlendMode::DestinationIn,
        "source-out" => BlendMode::SourceOut,
        "destination-out" => BlendMode::DestinationOut,
        "source-atop" => BlendMode::SourceAtop,
        "destination-atop" => BlendMode::DestinationAtop,
        "xor" => BlendMode::Xor,
        "lighter" => BlendMode::Plus,
        "multiply" => BlendMode::Multiply,
        "screen" => BlendMode::Screen,
        "overlay" => BlendMode::Overlay,
        "darken" => BlendMode::Darken,
        "lighten" => BlendMode::Lighten,
        "color-dodge" => BlendMode::ColorDodge,
        "color-burn" => BlendMode::ColorBurn,
        "hard-light" => BlendMode::HardLight,
        "soft-light" => BlendMode::SoftLight,
        "difference" => BlendMode::Difference,
        "exclusion" => BlendMode::Exclusion,
        "hue" => BlendMode::Hue,
        "saturation" => BlendMode::Saturation,
        "color" => BlendMode::Color,
        "luminosity" => BlendMode::Luminosity,
        _ => return None,
    })
}

fn parse_fill_rule(rule: Option<&str>) -> OpResult<FillRule> {
    match rule.unwrap_or("nonzero") {
        "nonzero" => Ok(FillRule::Winding),
        "evenodd" => Ok(FillRule::EvenOdd),
        _ => Err(OpError::new(
            "TypeError",
            "fill rule must be 'nonzero' or 'evenodd'",
        )),
    }
}

#[lumen_bind::class(name = "ImageData", hint(js(webidl)))]
pub struct DomImageData {
    image: Option<Rgba8Image>,
    width: u32,
    height: u32,
    color_space: CanvasColorSpace,
    pixel_format: ImageDataPixelFormat,
    pixels: RefCell<Option<Value>>,
}

impl DomImageData {
    fn from_image_settings(image: Rgba8Image, settings: ImageDataSettings) -> Self {
        Self {
            width: image.width,
            height: image.height,
            image: Some(image),
            color_space: settings.color_space,
            pixel_format: settings.pixel_format,
            pixels: RefCell::new(None),
        }
    }

    fn read_pixels(&self, ctx: &mut Ctx) -> OpResult<Rgba8Image> {
        let Some(data) = self.pixels.borrow().clone() else {
            if let Some(image) = self.image.as_ref() {
                return clone_rgba_image(image);
            }
            let length = image_data_bytes(self.width, self.height)?;
            return Ok(Rgba8Image {
                width: self.width,
                height: self.height,
                pixels: zeroed_bytes(length, "ImageData bitmap allocation failed")?,
            });
        };
        let expected = image_data_storage(self.width, self.height, self.pixel_format)?.1;
        let actual = ctx
            .typed_array_byte_len(&data)
            .ok_or_else(|| OpError::new("TypeError", "ImageData data is detached"))?;
        if actual != expected {
            return Err(OpError::new(
                "InvalidStateError",
                "ImageData data no longer matches its dimensions",
            ));
        }
        let bytes = ctx
            .typed_array_bytes(&data)
            .ok_or_else(|| OpError::new("TypeError", "ImageData data is detached"))?;
        match self.pixel_format {
            ImageDataPixelFormat::RgbaUnorm8 => Ok(Rgba8Image {
                width: self.width,
                height: self.height,
                pixels: bytes,
            }),
            ImageDataPixelFormat::RgbaFloat16 => {
                let output_len = image_data_bytes(self.width, self.height)?;
                let mut pixels = Vec::new();
                pixels.try_reserve_exact(output_len).map_err(|_| {
                    OpError::new("IndexSizeError", "ImageData bitmap allocation failed")
                })?;
                for rgba in bytes.chunks_exact(8) {
                    for component in 0..4 {
                        let bits =
                            u16::from_ne_bytes([rgba[component * 2], rgba[component * 2 + 1]]);
                        let number = lumen_common::float16::f16_bits_to_f64(bits);
                        let channel = if number.is_nan() {
                            0
                        } else {
                            (number.clamp(0.0, 1.0) * 255.0).round() as u8
                        };
                        pixels.push(channel);
                    }
                }
                Ok(Rgba8Image {
                    width: self.width,
                    height: self.height,
                    pixels,
                })
            }
        }
    }
}

#[lumen_bind::methods]
impl DomImageData {
    #[constructor]
    fn new(
        ctx: &mut Ctx,
        data_or_width: Value,
        height: Option<Value>,
        data_height_or_settings: Option<Value>,
        settings: Option<Value>,
    ) -> OpResult<Self> {
        match ctx.typed_array_kind(&data_or_width) {
            Some(TaKind::U8Clamped | TaKind::F16) => {
                let width = height.as_ref().ok_or_else(|| {
                    OpError::new("TypeError", "ImageData requires width and height")
                })?;
                let height = data_height_or_settings.as_ref().ok_or_else(|| {
                    OpError::new("TypeError", "ImageData requires width and height")
                })?;
                let width = webidl_unsigned_long(ctx, width)?;
                let height = webidl_unsigned_long(ctx, height)?;
                let parsed = parse_image_data_settings(ctx, settings.as_ref())?;
                let (_, byte_len) = image_data_storage(width, height, parsed.pixel_format)?;
                let actual_kind = ctx.typed_array_kind(&data_or_width);
                if actual_kind != Some(parsed.pixel_format.typed_array_kind()) {
                    return Err(OpError::new(
                        "InvalidStateError",
                        "ImageData pixelFormat does not match its pixel array",
                    ));
                }
                if ctx.typed_array_byte_len(&data_or_width) != Some(byte_len) {
                    return Err(OpError::new(
                        "InvalidStateError",
                        "ImageData array is detached or its length does not match its dimensions",
                    ));
                }
                Ok(Self {
                    image: None,
                    width,
                    height,
                    color_space: parsed.color_space,
                    pixel_format: parsed.pixel_format,
                    pixels: RefCell::new(Some(data_or_width)),
                })
            }
            Some(_) => Err(OpError::new(
                "TypeError",
                "ImageData requires a Uint8ClampedArray or Float16Array",
            )),
            None => {
                if matches!(data_or_width, Value::Obj(_)) {
                    return Err(OpError::new(
                        "TypeError",
                        "ImageData expects an image-data array or numeric dimensions",
                    ));
                }
                let height = height.ok_or_else(|| {
                    OpError::new("TypeError", "ImageData expects width and height")
                })?;
                let width = webidl_unsigned_long(ctx, &data_or_width)?;
                let height = webidl_unsigned_long(ctx, &height)?;
                let parsed = parse_image_data_settings(ctx, data_height_or_settings.as_ref())?;
                let (_, byte_len) = image_data_storage(width, height, parsed.pixel_format)?;
                let image = match parsed.pixel_format {
                    ImageDataPixelFormat::RgbaUnorm8 => Some(Rgba8Image {
                        width,
                        height,
                        pixels: zeroed_bytes(byte_len, "ImageData allocation failed")?,
                    }),
                    ImageDataPixelFormat::RgbaFloat16 => None,
                };
                Ok(Self {
                    image,
                    width,
                    height,
                    color_space: parsed.color_space,
                    pixel_format: parsed.pixel_format,
                    pixels: RefCell::new(None),
                })
            }
        }
    }
    #[getter]
    fn width(&self) -> u32 {
        self.width
    }
    #[getter]
    fn height(&self) -> u32 {
        self.height
    }
    #[getter]
    fn color_space(&self) -> &'static str {
        self.color_space.as_str()
    }
    #[getter]
    fn pixel_format(&self) -> &'static str {
        self.pixel_format.as_str()
    }
    #[getter]
    fn data(&self, ctx: &mut Ctx) -> OpResult<Value> {
        if let Some(value) = self.pixels.borrow().clone() {
            return Ok(value);
        }
        let (element_count, byte_len) = image_data_storage(self.width, self.height, self.pixel_format)?;
        let buffer = ctx.make_array_buffer_from(zeroed_bytes(byte_len, "ImageData data allocation failed")?);
        let array = ctx.new_typed_array_view(self.pixel_format.typed_array_kind(), &buffer, 0, element_count)
            .map_err(OpError::thrown)?;
        if let Some(image) = self.image.as_ref() {
            match self.pixel_format {
                ImageDataPixelFormat::RgbaUnorm8 => {
                    if !ctx.typed_array_set_bytes(&array, &image.pixels) {
                        return Err(OpError::new("TypeError", "could not initialize image data"));
                    }
                }
                ImageDataPixelFormat::RgbaFloat16 => {
                    let mut bytes = Vec::new();
                    let (_, byte_len) = image_data_storage(
                        self.width,
                        self.height,
                        ImageDataPixelFormat::RgbaFloat16,
                    )?;
                    bytes.try_reserve_exact(byte_len).map_err(|_| {
                        OpError::new("IndexSizeError", "ImageData data allocation failed")
                    })?;
                    for component in &image.pixels {
                        let number = f64::from(*component) / 255.0;
                        let bits = lumen_common::float16::f64_to_f16_bits(number).unwrap_or(0);
                        bytes.extend_from_slice(&bits.to_ne_bytes());
                    }
                    if !ctx.typed_array_set_bytes(&array, &bytes) {
                        return Err(OpError::new("TypeError", "could not initialize image data"));
                    }
                }
            }
        }
        *self.pixels.borrow_mut() = Some(array.clone());
        Ok(array)
    }
}

fn image_data_bytes(width: u32, height: u32) -> OpResult<usize> {
    image_data_storage(width, height, ImageDataPixelFormat::RgbaUnorm8).map(|(_, bytes)| bytes)
}

fn image_data_storage(
    width: u32,
    height: u32,
    pixel_format: ImageDataPixelFormat,
) -> OpResult<(usize, usize)> {
    if width == 0 || height == 0 {
        return Err(OpError::new(
            "IndexSizeError",
            "image data dimensions cannot be zero",
        ));
    }
    let elements = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|elements| usize::try_from(elements).ok())
        .ok_or_else(|| OpError::new("IndexSizeError", "image data is too large"))?;
    let bytes = elements
        .checked_mul(pixel_format.bytes_per_component())
        .ok_or_else(|| OpError::new("IndexSizeError", "image data is too large"))?;
    if bytes > MAX_IMAGE_DATA_BYTES {
        return Err(OpError::new("IndexSizeError", "image data is too large"));
    }
    Ok((elements, bytes))
}

fn clone_path2d(ctx: &mut Ctx, value: &Value) -> OpResult<CanvasPath> {
    ctx.with_instance::<DomPath2D, _>(value, |path| path.path.borrow().clone())
        .map_err(|_| OpError::new("TypeError", "argument must be a Path2D"))
}

fn parse_dom_matrix(ctx: &mut Ctx, value: &Value) -> OpResult<Transform> {
    let component = |ctx: &mut Ctx, name: &str, default: f64| -> OpResult<f32> {
        let value = ctx
            .get_member(value, name)
            .map_err(|_| OpError::new("TypeError", "Path2D transform is unreadable"))?;
        let value = match value {
            Value::Undefined => default,
            Value::Num(value) if value.is_finite() => value,
            _ => {
                return Err(OpError::new(
                    "TypeError",
                    "Path2D transform must be a finite matrix",
                ));
            }
        };
        let value = value as f32;
        if value.is_finite() {
            Ok(value)
        } else {
            Err(OpError::new(
                "TypeError",
                "Path2D transform is out of range",
            ))
        }
    };
    Ok(Transform::from_row(
        component(ctx, "a", 1.0)?,
        component(ctx, "b", 0.0)?,
        component(ctx, "c", 0.0)?,
        component(ctx, "d", 1.0)?,
        component(ctx, "e", 0.0)?,
        component(ctx, "f", 0.0)?,
    ))
}

#[lumen_bind::class(name = "ElementImage", hint(js(webidl)))]
pub struct DomElementImage {
    image: RefCell<Option<Arc<ElementImagePixels>>>,
    origin_clean: bool,
}

struct ElementImagePixels {
    image: Rgba8Image,
    _reservation: lumen_common::limits::ByteLease,
}

fn element_image_pixels(image:Rgba8Image) -> OpResult<Arc<ElementImagePixels>> {
    static BUDGET:std::sync::OnceLock<Arc<lumen_common::limits::ByteBudget>>=std::sync::OnceLock::new();
    if !image.is_valid() {return Err(OpError::new("InvalidStateError","ElementImage has invalid pixels"));}
    let budget=BUDGET.get_or_init(||lumen_common::limits::ByteBudget::new(64*1024*1024));
    let reservation=budget.reserve(image.pixels.len()).ok_or_else(||OpError::new("QuotaExceededError","ElementImage pixel budget exhausted"))?;
    Ok(Arc::new(ElementImagePixels {image,_reservation:reservation}))
}

#[lumen_bind::methods]
impl DomElementImage {
    #[constructor]
    fn new() -> OpResult<Self> {Err(OpError::new("TypeError","Illegal constructor"))}
    #[getter]
    fn width(&self) -> u32 {self.image.borrow().as_ref().map_or(0,|image|image.image.width)}
    #[getter]
    fn height(&self) -> u32 {self.image.borrow().as_ref().map_or(0,|image|image.image.height)}
}

fn element_image_transfer_codec() -> lumen_host::clone_transfer::NativeTransferCodec {
    lumen_host::clone_transfer::NativeTransferCodec {
        kind:"ElementImage",
        matches:|ctx,value|ctx.with_instance::<DomElementImage,_>(value,|_|()).is_ok(),
        validate:|ctx,value|ctx.with_instance::<DomElementImage,_>(value,|image| {
            if image.image.borrow().is_none() {Err(OpError::new("DataCloneError","ElementImage is detached"))} else {Ok(())}
        })?,
        export:|ctx,value|ctx.with_instance::<DomElementImage,_>(value,|image| {
            Box::new((image.image.borrow().as_ref().expect("validated ElementImage").clone(),image.origin_clean)) as Box<dyn std::any::Any+Send>
        }),
        detach:|ctx,value,_| {let _=ctx.with_instance::<DomElementImage,_>(value,|image|{image.image.borrow_mut().take();});},
        import:|ctx,payload| {
            let payload=payload.downcast::<(Arc<ElementImagePixels>,bool)>().map_err(|_|OpError::new("DataCloneError","Invalid ElementImage attachment"))?;
            let (image,origin_clean)=*payload;
            Ok(ctx.new_instance(DomElementImage {image:RefCell::new(Some(image)),origin_clean}))
        },
    }
}

fn offscreen_transfer_codec() -> lumen_host::clone_transfer::NativeTransferCodec {
    use lumen_host::clone_transfer::NativeTransferCodec;
    NativeTransferCodec {
        kind: "OffscreenCanvas",
        matches: |ctx, value| ctx.with_instance::<DomOffscreenCanvas, _>(value, |_| ()).is_ok(),
        validate: |ctx, value| ctx.with_instance::<DomOffscreenCanvas, _>(value, |canvas| {
            if canvas.detached.get() || canvas.data.borrow().context_mode.is_some() {
                Err(OpError::new("DataCloneError", "OffscreenCanvas is detached or has a rendering context"))
            } else { Ok(()) }
        })?,
        export: |ctx, value| ctx.with_instance::<DomOffscreenCanvas, _>(value, |canvas| {
            let data = canvas.data.borrow();
            let publication = data.remote_outbound.clone().or_else(|| canvas.placeholder.map(|_| Arc::new(std::sync::Mutex::new(CanvasPublication::default()))));
            Box::new(OffscreenCanvasTransfer {
                width: data.logical_width, height: data.logical_height,
                rtl: data.inherited_direction_rtl, origin_clean: data.origin_clean, publication,
            }) as Box<dyn std::any::Any + Send>
        }),
        detach: |ctx, value, payload| {
            let payload = payload.downcast_ref::<OffscreenCanvasTransfer>().expect("native Canvas attachment type");
            let _ = ctx.with_instance::<DomOffscreenCanvas, _>(value, |canvas| {
                canvas.detached.set(true);
                if canvas.placeholder.is_some() {
                    canvas.data.borrow_mut().remote_inbound = payload.publication.clone();
                }
            });
        },
        import: |ctx, payload| {
            let payload = payload.downcast::<OffscreenCanvasTransfer>().map_err(|_| OpError::new("DataCloneError", "Invalid Canvas attachment"))?;
            let mut data = CanvasData::new_offscreen(payload.width, payload.height)?;
            data.inherited_direction_rtl = payload.rtl;
            data.origin_clean = payload.origin_clean;
            data.remote_outbound = payload.publication;
            let realm = window_globals::current_dom_realm(ctx);
            Ok(ctx.new_instance(DomOffscreenCanvas {
                data: Rc::new(RefCell::new(data)), realm,
                placeholder: None, detached: Cell::new(false),
            }))
        },
    }
}

fn image_data_value_codec() -> lumen_host::clone_transfer::NativeGraphValueCodec {
    use lumen_host::clone_transfer::NativeGraphValueCodec;
    NativeGraphValueCodec {
        kind: "ImageData", max_bytes: 10, max_children: 1,
        matches: |ctx, value| ctx.with_instance::<DomImageData, _>(value, |_| ()).is_ok(),
        serialize: |ctx, value| {
            let image = ctx.instance_data::<DomImageData>(value).ok_or_else(|| OpError::new("DataCloneError", "Invalid ImageData"))?;
            let image = image.borrow();
            let pixels = image.data(ctx)?;
            let expected = image_data_storage(image.width, image.height, image.pixel_format)?.1;
            if ctx.typed_array_byte_len(&pixels) != Some(expected) {
                return Err(OpError::new("DataCloneError", "ImageData pixel array is detached"));
            }
            let mut bytes = Vec::with_capacity(10);
            bytes.extend_from_slice(&image.width.to_le_bytes());
            bytes.extend_from_slice(&image.height.to_le_bytes());
            bytes.push(match image.color_space { CanvasColorSpace::Srgb => 0, CanvasColorSpace::DisplayP3 => 1 });
            bytes.push(match image.pixel_format { ImageDataPixelFormat::RgbaUnorm8 => 0, ImageDataPixelFormat::RgbaFloat16 => 1 });
            Ok((bytes, vec![pixels]))
        },
        create: |ctx, bytes| {
            if bytes.len() != 10 { return Err(OpError::new("DataCloneError", "Invalid ImageData metadata")); }
            let width = u32::from_le_bytes(bytes[..4].try_into().unwrap());
            let height = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
            let color_space = match bytes[8] { 0 => CanvasColorSpace::Srgb, 1 => CanvasColorSpace::DisplayP3, _ => return Err(OpError::new("DataCloneError", "Invalid ImageData color space")) };
            let pixel_format = match bytes[9] { 0 => ImageDataPixelFormat::RgbaUnorm8, 1 => ImageDataPixelFormat::RgbaFloat16, _ => return Err(OpError::new("DataCloneError", "Invalid ImageData pixel format")) };
            image_data_storage(width, height, pixel_format).map_err(|_| OpError::new("DataCloneError", "Invalid ImageData dimensions"))?;
            Ok(ctx.new_instance(DomImageData { image: None, width, height, color_space, pixel_format, pixels: RefCell::new(None) }))
        },
        populate: |ctx, value, mut children| {
            if children.len() != 1 { return Err(OpError::new("DataCloneError", "Invalid ImageData pixel array")); }
            let pixels = children.pop().unwrap();
            let (width, height, format) = ctx.with_instance::<DomImageData, _>(value, |image| (image.width, image.height, image.pixel_format))?;
            let expected = image_data_storage(width, height, format)?.1;
            if ctx.typed_array_kind(&pixels) != Some(format.typed_array_kind()) || ctx.typed_array_byte_len(&pixels) != Some(expected) {
                return Err(OpError::new("DataCloneError", "Invalid ImageData pixel array"));
            }
            ctx.with_instance::<DomImageData, _>(value, |image| { *image.pixels.borrow_mut() = Some(pixels); })?;
            Ok(())
        },
    }
}

fn install_canvas_apis(ctx: &mut Ctx, include_window_context: bool) -> OpResult<()> {
    if include_window_context { let constructor=ctx.class_constructor::<DomCanvasPaintEvent>(); let global=ctx.global_object(); crate::install_interface(ctx,&global,"CanvasPaintEvent",constructor).map_err(OpError::thrown)?; }
    lumen_host::clone_transfer::register_native_codec(ctx, offscreen_transfer_codec());
    lumen_host::clone_transfer::register_native_codec(ctx, element_image_transfer_codec());
    lumen_host::clone_transfer::register_native_graph_value_codec(ctx, image_data_value_codec());
    let global = ctx.global_object();
    let _bitmap_decode_job_constructor = ctx.class_constructor::<DomBitmapDecodeJob>();
    for (name, constructor) in [
        ("ElementImage",ctx.class_constructor::<DomElementImage>()),
        (
            "OffscreenCanvas",
            ctx.class_constructor::<DomOffscreenCanvas>(),
        ),
        (
            "OffscreenCanvasRenderingContext2D",
            ctx.class_constructor::<DomOffscreenCanvasRenderingContext2D>(),
        ),
        (
            "ImageBitmapRenderingContext",
            ctx.class_constructor::<DomImageBitmapRenderingContext>(),
        ),
        ("ImageData", ctx.class_constructor::<DomImageData>()),
        (
            "CanvasGradient",
            ctx.class_constructor::<DomCanvasGradient>(),
        ),
        ("CanvasPattern", ctx.class_constructor::<DomCanvasPattern>()),
        ("TextMetrics", ctx.class_constructor::<DomTextMetrics>()),
        ("Path2D", ctx.class_constructor::<DomPath2D>()),
        ("ImageBitmap", ctx.class_constructor::<DomImageBitmap>()),
    ] {
        crate::install_interface(ctx, &global, name, constructor)
            .map_err(|_| OpError::new("Error", "canvas constructor installation failed"))?;
    }
    if include_window_context {
        let constructor = ctx.class_constructor::<DomCanvasRenderingContext2D>();
        crate::install_interface(ctx, &global, "CanvasRenderingContext2D", constructor)
            .map_err(|_| OpError::new("Error", "canvas constructor installation failed"))?;
    }
    let create_bitmap = ctx.bound_function(&lumen_bind::FnItem::of::<create_image_bitmap::Op>());
    ctx.set_member(&global, "createImageBitmap", create_bitmap)
        .map_err(|_| OpError::new("Error", "createImageBitmap installation failed"))?;
    Ok(())
}

pub(crate) fn install(ctx: &mut Ctx) -> OpResult<()> {
    install_canvas_apis(ctx, true)
}

pub(crate) fn install_worker(ctx: &mut Ctx) -> OpResult<()> {
    install_canvas_apis(ctx, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transferred_offscreen_text_captures_placeholder_direction() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<html dir='rtl'><body><canvas id='inherited' dir='rtl' width='100' height='40'></canvas><canvas id='explicit' width='100' height='40'></canvas><canvas id='ltr' width='100' height='40'></canvas></body></html>",128).unwrap();
        assert!(eval_bool(&mut engine,r#"
            var inherited=document.getElementById('inherited').transferControlToOffscreen();
            var explicit=document.getElementById('explicit').transferControlToOffscreen();
            var ltr=document.getElementById('ltr').transferControlToOffscreen();
            document.getElementById('inherited').dir='ltr';
            for(const [canvas,direction] of [[inherited,'inherit'],[explicit,'rtl'],[ltr,'ltr']]){
                const context=canvas.getContext('2d');context.font='20px sans-serif';context.direction=direction;context.fillText('ABC!',60,25);
            }
            inherited.getContext('2d').direction==='inherit'
        "#));
        let pixels=|name:&str|{let node=realm.with_session(|session|selector::query_selector(session.document(),session.document().root(),&format!("#{name}")).unwrap().unwrap());realm.canvases.data_for(&realm,node).unwrap().borrow().snapshot().pixels};
        assert_eq!(pixels("inherited"),pixels("explicit"));
        assert_ne!(pixels("inherited"),pixels("ltr"));
        assert!(eval_bool(&mut engine,"var standalone=new OffscreenCanvas(100,40);standalone.getContext('2d').fillText('ABC!',60,25);true"));
        let value=engine.eval_value("standalone").unwrap().unwrap_or_else(|_|panic!("standalone query threw"));
        assert!(engine.ctx().with_instance::<DomOffscreenCanvas,_>(&value,|canvas|canvas.data.borrow().inherited_direction_rtl).unwrap());
    }

    #[test]
    fn drawable_canvas_initial_render_publishes_without_author_paint_request() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<!doctype html><canvas id=c content=drawable width=8 height=6><div id=target drawable style='width:4px;height:3px;background:blue'></div></canvas>",64).unwrap();
        realm.set_canvas_snapshot_provider(Rc::new(|session,node,width,height| {
            let fonts=canvas_fallback_fonts();
            let (list,rect)=session.element_snapshot_display_list(node,width,height,fonts,None).map_err(|error|format!("{error:?}"))?;
            lumen_html_image::render_with_font(&list,rect.width.ceil() as u32,rect.height.ceil() as u32,1.0,false,fonts).map_err(|error|format!("{error:?}"))
        }));
        assert!(realm.canvas_paint_pending());
        realm.update_rendered_focus(engine.ctx()).unwrap();
        assert!(eval_bool(&mut engine,"document.getElementById('c').getContext('2d').drawElementImage(document.getElementById('target'),0,0);document.getElementById('c').getContext('2d').getImageData(0,0,1,1).data[2]===255"));
        assert!(!realm.canvas_paint_pending());
        assert!(eval_bool(&mut engine,"globalThis.retained=document.getElementById('c').captureElementImage(document.getElementById('target'));retained instanceof ElementImage && retained.width===4 && retained.height===3"));
        assert!(eval_bool(&mut engine,"document.getElementById('target').style.background='red';true"));
        realm.update_rendered_focus(engine.ctx()).unwrap();
        assert!(eval_bool(&mut engine,"document.getElementById('c').getContext('2d').drawElementImage(document.getElementById('target'),0,0);document.getElementById('c').getContext('2d').getImageData(0,0,1,1).data[0]===255"));
        assert!(eval_bool(&mut engine,"document.getElementById('c').getContext('2d').drawElementImage(retained,0,0);document.getElementById('c').getContext('2d').getImageData(0,0,1,1).data[2]===255"));
    }

    #[test]
    fn canvas_paint_renders_ordinary_nested_and_slotted_resources() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<!doctype html><canvas id=plain width=4 height=3></canvas><canvas id=outer width=8 height=6 content=drawable><canvas id=nested drawable width=8 height=6></canvas><div id=host><div id=slotted slot=s drawable style='width:4px;height:3px;background:yellow'></div></div>",128).unwrap();
        let rendered=Rc::new(RefCell::new(Vec::new()));
        let observed=rendered.clone();
        realm.set_canvas_snapshot_provider(Rc::new(move |session,node,width,height| {
            let fonts=canvas_fallback_fonts();
            let (list,rect)=session.element_snapshot_display_list(node,width,height,fonts,None).map_err(|error|format!("{error:?}"))?;
            let image=lumen_html_image::render_with_font(&list,rect.width.ceil() as u32,rect.height.ceil() as u32,1.0,false,fonts).map_err(|error|format!("{error:?}"))?;
            observed.borrow_mut().push((node,image.pixels.clone()));
            Ok(image)
        }));
        assert!(eval_bool(&mut engine,r#"
            globalThis.plain=document.getElementById('plain');globalThis.outer=document.getElementById('outer');
            globalThis.nested=document.getElementById('nested');globalThis.host=document.getElementById('host');
            globalThis.events=[];globalThis.plainValid=false;globalThis.px=plain.getContext('2d');globalThis.nx=nested.getContext('2d');globalThis.ox=outer.getContext('2d');
            px.fillStyle='blue';px.fillRect(0,0,4,3);nx.fillStyle='red';nx.fillRect(0,0,8,6);
            plain.onpaint=e=>{events.push('plain');plainValid=e instanceof CanvasPaintEvent && e.isTrusted && e.changedElements.length===0;};
            nested.onpaint=()=>{events.push('nested');nx.fillStyle='green';nx.fillRect(0,0,8,6)};
            outer.onpaint=e=>{events.push('outer');if(e.changedElements[0]!==nested)throw Error('nested publication');ox.drawElementImage(nested,0,0)};
            host.attachShadow({mode:'open'}).innerHTML='<canvas id="sc" width="8" height="6" content="drawable"><slot name="s"></slot></canvas>';
            globalThis.sc=host.shadowRoot.getElementById('sc');globalThis.sx=sc.getContext('2d');
            sc.onpaint=e=>{events.push('slot');if(e.changedElements[0]!==document.getElementById('slotted'))throw Error('composed publication');sx.drawElementImage(e.changedElements[0],0,0)};
            plain.requestPaint();outer.requestPaint();nested.requestPaint();sc.requestPaint();true
        "#));
        realm.update_rendered_focus(engine.ctx()).unwrap();
        assert!(eval_bool(&mut engine,"plainValid && events.includes('plain') && events.indexOf('nested')<events.indexOf('outer') && ox.getImageData(0,0,1,1).data[1]===128 && sx.getImageData(0,0,1,1).data[0]===255 && sx.getImageData(0,0,1,1).data[1]===255"));
        assert!(eval_bool(&mut engine,"nested.removeAttribute('drawable');nx.fillStyle='blue';nx.fillRect(0,0,8,6);ox.drawElementImage(nested,0,0,4,3);ox.getImageData(0,0,1,1).data[2]===255"),"native canvas source uses current pixels and numeric scaling without a drawable publication");
        let plain=realm.with_session(|session|selector::query_selector(session.document(),session.document().root(),"#plain").unwrap().unwrap());
        assert!(rendered.borrow().iter().any(|(node,pixels)|*node==plain && pixels.get(..4)==Some(&[0,0,255,255])));
        let nested=realm.with_session(|session|selector::query_selector(session.document(),session.document().root(),"#nested").unwrap().unwrap());
        realm.canvases.data_for(&realm,nested).unwrap().borrow_mut().origin_clean=false;
        assert!(eval_bool(&mut engine,"(()=>{ox.drawElementImage(nested,0,0);try{ox.getImageData(0,0,1,1);return false}catch(e){return e.name==='SecurityError'}})()"),"native canvas drawing propagates origin taint through the shared image path");
        assert!(eval_bool(&mut engine,"events=[];outer.requestPaint();true"));
        let error=realm.update_rendered_focus(engine.ctx()).expect_err("tainted nested canvas must not publish readable pixels");
        assert_eq!(error.class(),"SecurityError");
        assert!(eval_bool(&mut engine,"!events.includes('outer')"));
    }

    #[test]
    fn drawable_canvas_paint_publishes_real_pixels_and_retains_callback_generation() {
        let mut engine = Engine::new();
        let realm = super::super::install(engine.ctx(), "<!doctype html><canvas id=c width=30 height=20 content=drawable><div id=target drawable style='width:10px;height:5px;background:blue'></div></canvas>",128).unwrap();
        let captures = Rc::new(Cell::new(0));
        let observed = captures.clone();
        realm.set_canvas_snapshot_provider(Rc::new(move |session,node,width,height| {
            observed.set(observed.get()+1);
            let fonts = canvas_fallback_fonts();
            let (list,rect) = session.element_snapshot_display_list(node,width,height,fonts,None).map_err(|error|format!("{error:?}"))?;
            lumen_html_image::render_with_font(&list,rect.width.ceil() as u32,rect.height.ceil() as u32,1.0,false,fonts).map_err(|error|format!("{error:?}"))
        }));
        assert!(eval_bool(&mut engine, "devicePixelRatio===1 && window.devicePixelRatio===1"));
        realm.set_device_pixel_ratio(2.0).unwrap();
        assert!(eval_bool(&mut engine, "devicePixelRatio===2"));
        assert!(realm.set_device_pixel_ratio(f64::NAN).is_err());
        realm.set_device_pixel_ratio(1.0).unwrap();
        assert!(eval_bool(&mut engine, "var resized=document.getElementById('c');resized.width=30*devicePixelRatio;resized.height=20*devicePixelRatio;resized.width===30 && resized.height===20"));
        assert!(eval_bool(&mut engine, r#"globalThis.c=document.getElementById('c');globalThis.target=document.getElementById('target');globalThis.x=c.getContext('2d');globalThis.paints=0;globalThis.trusted=false;c.onpaint=e=>{paints++;trusted=e.isTrusted && e instanceof CanvasPaintEvent && Object.isFrozen(e.changedElements) && e.changedElements[0]===target;x.drawElementImage(target,2,3);target.style.background='red'};c.requestPaint();c.requestPaint();(()=>{try{x.drawElementImage(target,0,0);return false}catch(e){return e.name==='InvalidStateError'}})()"#));
        assert!(realm.canvas_paint_pending());
        if let Err(error)=realm.update_rendered_focus(engine.ctx()) {
            let value=error.to_value(engine.ctx());
            let message=engine.ctx().to_string(&value).unwrap_or_else(|_| "unprintable paint exception".into());
            panic!("real paint update failed: {message}");
        }
        assert_eq!(captures.get(),1);
        assert!(eval_bool(&mut engine,"paints===1 && trusted && x.getImageData(2,3,1,1).data[2]===255"));
        assert!(eval_bool(&mut engine,"x.clearRect(0,0,30,20);x.drawElementImage(target,0,0);x.getImageData(0,0,1,1).data[2]===255"));
        assert!(eval_bool(&mut engine,"c.requestPaint();true"));
        if let Err(error)=realm.update_rendered_focus(engine.ctx()) {
            let value=error.to_value(engine.ctx());
            let message=engine.ctx().to_string(&value).unwrap_or_else(|_| "unprintable paint exception".into());
            panic!("real paint update failed: {message}");
        }
        assert_eq!(captures.get(),2);
        assert!(eval_bool(&mut engine,"paints===2 && x.getImageData(2,3,1,1).data[0]===255"));
    }

    #[test]
    fn image_data_native_storage_clone_preserves_pixels_aliases_and_intrinsics() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<main></main>", 64).unwrap();
        let source = engine.eval_value(r#"var image=new ImageData(2,1,{colorSpace:'display-p3'});image.data[0]=137;var pixels=image.data;Uint8ClampedArray=function(){throw Error('author constructor called')};({image,pixels,again:image})"#).unwrap().unwrap_or_else(|_| panic!("canvas JavaScript evaluation threw"));
        let bytes = lumen_host::structured_clone::serialize_for_storage(engine.ctx(), &source, 1024).unwrap();
        let cloned = lumen_host::structured_clone::deserialize_for_storage(engine.ctx(), &bytes).unwrap();
        let global=engine.ctx().global_object();
        engine.ctx().create_data_property(&global,"cloned",cloned).unwrap_or_else(|_| panic!("clone publication threw"));
        assert!(eval_bool(&mut engine,"cloned.image instanceof ImageData && cloned.image===cloned.again && cloned.image.data===cloned.pixels && cloned.image.data!==pixels && cloned.image.data[0]===137 && cloned.image.colorSpace==='display-p3'"));
        engine.ctx().collect_garbage();
        assert!(eval_bool(&mut engine,"cloned.image.data[0]===137"));
    }

    #[test]
    fn element_image_native_transfer_preserves_snapshot_and_detaches_sender() {
        let mut engine=Engine::new();
        install_worker(engine.ctx()).unwrap();
        let image=engine.ctx().new_instance(DomElementImage {image:RefCell::new(Some(element_image_pixels(Rgba8Image {
            width:2,height:1,pixels:vec![0,0,255,255,255,0,0,255],
        }).unwrap())),origin_clean:true});
        let global=engine.ctx().global_object();
        engine.ctx().create_data_property(&global,"image",image.clone()).unwrap_or_else(|_|panic!("image publication"));
        assert!(lumen_host::structured_clone::serialize(engine.ctx(),&image,&[],true,&Value::Null).is_err());
        let bad=engine.eval_value("({image,uncloneable(){}})").unwrap().unwrap_or_else(|_|panic!("evaluation"));
        assert!(lumen_host::structured_clone::serialize(engine.ctx(),&bad,&[image.clone()],true,&Value::Null).is_err());
        assert!(eval_bool(&mut engine,"image.width===2 && image.height===1"));
        let bytes=lumen_host::structured_clone::serialize(engine.ctx(),&image,&[image.clone()],true,&Value::Null).unwrap();
        let message=lumen_host::clone_transfer::take_message(engine.ctx(),bytes);
        assert!(eval_bool(&mut engine,"image.width===0 && image.height===0"));
        assert!(lumen_host::structured_clone::serialize(engine.ctx(),&image,&[image.clone()],true,&Value::Null).is_err());
        std::thread::spawn(move|| {
            let mut worker=Engine::new();install_worker(worker.ctx()).unwrap();
            let bytes=lumen_host::clone_transfer::install_message(worker.ctx(),message);
            let image=lumen_host::structured_clone::deserialize(worker.ctx(),&bytes,&Value::Null).unwrap();
            let global=worker.ctx().global_object();worker.ctx().create_data_property(&global,"image",image).unwrap_or_else(|_|panic!("worker publication"));
            assert!(eval_bool(&mut worker,"image instanceof ElementImage && image.width===2 && (()=>{const x=new OffscreenCanvas(4,2).getContext('2d');x.drawElementImage(image,0,0,4,2);const p=x.getImageData(0,0,4,2).data;return p[2]===255 && p[12]===255 && p[14]===0})()"));
        }).join().unwrap();
    }

    #[test]
    fn offscreen_native_transfer_detaches_sender_and_publishes_worker_pixels() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<canvas id='placeholder' width='2' height='1'></canvas>", 64).unwrap();
        let source = engine.eval_value("var transferred=document.getElementById('placeholder').transferControlToOffscreen();transferred").unwrap().unwrap_or_else(|_| panic!("canvas JavaScript evaluation threw"));
        let failure = lumen_host::structured_clone::serialize(engine.ctx(), &source, &[], true, &Value::Null);
        assert!(failure.is_err(), "OffscreenCanvas requires its transfer-list entry");
        assert!(lumen_host::structured_clone::serialize(engine.ctx(), &source, &[source.clone(),source.clone()], true, &Value::Null).is_err());
        let bad = engine.eval_value("({canvas:transferred, notCloneable(){}})").unwrap().unwrap_or_else(|_| panic!("canvas JavaScript evaluation threw"));
        assert!(lumen_host::structured_clone::serialize(engine.ctx(), &bad, &[source.clone()], true, &Value::Null).is_err());
        assert!(eval_bool(&mut engine, "transferred.width===2"));
        let message_value = engine.eval_value("({canvas:transferred,get mutate(){transferred.width=3;return true}})").unwrap().unwrap_or_else(|_| panic!("canvas JavaScript evaluation threw"));
        let bytes = lumen_host::structured_clone::serialize(engine.ctx(), &message_value, &[source.clone()], true, &Value::Null).unwrap();
        let message = lumen_host::clone_transfer::take_message(engine.ctx(), bytes);
        assert!(eval_bool(&mut engine, r#"(()=>{
            if(transferred.width!==0 || transferred.height!==0) return false;
            for(const operation of [()=>transferred.getContext('2d'),()=>{transferred.width=5},()=>transferred.transferToImageBitmap()]) {
                let denied=false;try{operation()}catch(e){denied=e.name==='InvalidStateError'};
                if(!denied)return false;
            }
            return true;
        })()"#));
        assert!(lumen_host::structured_clone::serialize(engine.ctx(), &source, &[source.clone()], true, &Value::Null).is_err());
        std::thread::spawn(move || {
            let mut worker = Engine::new();
            install_worker(worker.ctx()).unwrap();
            let bytes = lumen_host::clone_transfer::install_message(worker.ctx(), message);
            let value = lumen_host::structured_clone::deserialize(worker.ctx(), &bytes, &Value::Null).unwrap();
            let canvas = worker.ctx().member_get(&value, "canvas").unwrap_or_else(|_| panic!("worker canvas lookup threw"));
            let global = worker.ctx().global_object();
            worker.ctx().create_data_property(&global, "canvas", canvas).unwrap_or_else(|_| panic!("worker canvas publication threw"));
            assert!(eval_bool(&mut worker, r#"(()=>{
                if(canvas.width!==3 || canvas.height!==1)return false;
                canvas.width=canvas.height=200;
                const x=canvas.getContext('2d');x.fillStyle='purple';x.fillRect(60,60,75,50);
                x.beginLayer();x.fillStyle='red';x.fillRect(40,40,75,50);
                return true;
            })()"#));
        }).join().unwrap();
        realm.sync_canvas().unwrap();
        assert!(eval_bool(&mut engine,"document.getElementById('placeholder').width===200 && document.getElementById('placeholder').height===200"));
        let node = realm.with_session(|session|selector::query_selector(session.document(),session.document().root(),"#placeholder").unwrap().unwrap());
        let data = realm.canvases.data_for(&realm,node).unwrap();
        let image = data.borrow().snapshot();
        let pixel = |x:usize,y:usize| &image.pixels[(y*200+x)*4..(y*200+x)*4+4];
        assert_eq!(pixel(60,60),[128,0,128,255]);
        assert_eq!(pixel(40,40),[0,0,0,0],"unclosed worker layer stays isolated");
        let busy = engine.eval_value("var busy=new OffscreenCanvas(2,1);busy.getContext('2d');busy").unwrap().unwrap_or_else(|_| panic!("canvas JavaScript evaluation threw"));
        assert!(lumen_host::structured_clone::serialize(engine.ctx(), &busy, &[busy.clone()], true, &Value::Null).is_err());
        assert!(eval_bool(&mut engine,"busy.width===2 && busy.getContext('2d')!==null"));
    }

    #[test]
    fn transferred_offscreen_canvas_presents_real_pixels_at_frame_boundary() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<canvas id='placeholder' width='2' height='1'></canvas><canvas id='busy'></canvas>", 64).unwrap();
        let result = engine.eval_value(r#"(()=>{
            const check=(v,m)=>{if(!v)throw Error(m)};
            var placeholder=document.getElementById('placeholder');
            globalThis.off=placeholder.transferControlToOffscreen();
            const ctx=off.getContext('2d');
            check(ctx instanceof OffscreenCanvasRenderingContext2D && ctx.canvas===off,'typed context owner');
            off.width=3;off.height=2;
            ctx.fillStyle='green';ctx.fillRect(0,0,3,2);
            check(placeholder.width===2 && placeholder.height===1,'placeholder waits for rendering');
            for(const action of [()=>placeholder.transferControlToOffscreen(),()=>placeholder.getContext('2d'),()=>{placeholder.width=4}]) {
                let failed=false;try{action()}catch(e){failed=e.name==='InvalidStateError'};check(failed,'transfer state guard');
            }
            const busy=document.getElementById('busy');busy.getContext('2d');
            let failed=false;try{busy.transferControlToOffscreen()}catch(e){failed=e.name==='InvalidStateError'};check(failed,'existing context guard');
            return true;
        })()"#).unwrap().unwrap_or_else(|_|panic!("transfer setup threw"));
        assert!(matches!(result,Value::Bool(true)));
        realm.sync_canvas().unwrap();
        let node=realm.with_session(|session|selector::query_selector(session.document(),session.document().root(),"#placeholder").unwrap().unwrap());
        let data=realm.canvases.data_for(&realm,node).unwrap();
        let snapshot=data.borrow().snapshot();
        assert_eq!((snapshot.width,snapshot.height),(3,2));
        assert!(snapshot.pixels.chunks_exact(4).all(|pixel|pixel==[0,128,0,255]));
        assert!(eval_bool(&mut engine,"document.getElementById('placeholder').width===3 && document.getElementById('placeholder').height===2"));
        engine.ctx().collect_garbage();
        assert!(eval_bool(&mut engine,"off.getContext('2d').canvas===off"));
    }
    use lumen::embed::WeakValue;
    use lumen::Engine;

    fn eval_bool(engine: &mut Engine, source: &str) -> bool {
        let source = format!(
            "try {{ {source} }} catch (e) {{ '__JS_ERROR__' + String(e && (e.stack || e.message || e)) }}"
        );
        match engine.eval_value(&source) {
            Ok(Ok(Value::Bool(value))) => value,
            Ok(Ok(Value::Str(error))) => panic!("JavaScript exception: {}", error.as_str()),
            Ok(Err(_)) => panic!("JavaScript evaluation ended abruptly"),
            Err(_) => panic!("JavaScript evaluation could not start"),
            Ok(Ok(_)) => panic!("JavaScript assertion did not return a boolean"),
        }
    }

    #[test]
    fn offscreen_context_uses_its_own_brand_and_shared_2d_operations() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            r#"(()=>{
                const canvas = new OffscreenCanvas(2, 1);
                const context = canvas.getContext('2d');
                const domCanvas = document.createElement('canvas');
                const domContext = domCanvas.getContext('2d');
                context.fillStyle = 'red';
                context.fillRect(0, 0, 1, 1);
                const pixel = context.getImageData(0, 0, 1, 1).data;
                const ctor = OffscreenCanvasRenderingContext2D;
                return canvas instanceof OffscreenCanvas &&
                    context instanceof ctor &&
                    Object.getPrototypeOf(context) === ctor.prototype &&
                    Object.getPrototypeOf(ctor.prototype) === Object.prototype &&
                    !(context instanceof CanvasRenderingContext2D) &&
                    domContext instanceof CanvasRenderingContext2D &&
                    !(domContext instanceof ctor) &&
                    canvas.getContext('2d') === context && context.canvas === canvas &&
                    pixel[0] === 255 && pixel[1] === 0 && pixel[3] === 255;
            })()"#
        ));
    }

    #[test]
    fn stroke_rect_zero_area_is_a_noop_and_rectangles_preserve_the_current_path() {
        let mut engine = Engine::new();
        install_worker(engine.ctx()).unwrap();
        assert!(eval_bool(
            &mut engine,
            r#"(() => {
            const canvas=new OffscreenCanvas(20,20);
            const context=canvas.getContext('2d');
            context.beginPath();context.moveTo(2,2);context.lineTo(2,10);
            context.strokeStyle='red';context.lineWidth=250;
            context.lineCap='round';context.lineJoin='round';
            context.strokeRect(15,15,0,0);
            if(context.getImageData(15,15,1,1).data[3]!==0)return false;
            context.strokeRect(NaN,0,5,5);
            context.strokeStyle='#00ff00';context.lineWidth=2;context.lineCap='butt';
            context.strokeRect(12,12,5,5);
            context.stroke();
            const pixel=context.getImageData(2,5,1,1).data;
            return pixel[0]===0&&pixel[1]===255&&pixel[2]===0&&pixel[3]===255;
        })()"#
        ));
    }

    #[test]
    fn canvas_line_and_shadow_state_roundtrips_saves_renders_and_resets() {
        let mut engine = Engine::new();
        install_worker(engine.ctx()).unwrap();
        assert!(eval_bool(
            &mut engine,
            r#"(()=>{
                const canvas = new OffscreenCanvas(9, 4);
                const context = canvas.getContext('2d');
                const defaults = context.lineCap === 'butt' && context.lineJoin === 'miter' &&
                    context.miterLimit === 10 && context.shadowColor === 'rgba(0, 0, 0, 0)' &&
                    context.shadowBlur === 0 && context.shadowOffsetX === 0 &&
                    context.shadowOffsetY === 0;
                context.lineCap = 'round';
                context.lineCap = 'not-a-cap';
                context.lineJoin = 'bevel';
                context.miterLimit = 3.5;
                context.shadowColor = 'blue';
                context.shadowBlur = 2;
                context.shadowOffsetX = 3;
                context.shadowOffsetY = -1;
                context.save();
                context.lineCap = 'square';
                context.lineJoin = 'round';
                context.miterLimit = 7;
                context.shadowColor = 'red';
                context.shadowBlur = 8;
                context.shadowOffsetX = -5;
                context.shadowOffsetY = 6;
                context.restore();
                const restored = context.lineCap === 'round' && context.lineJoin === 'bevel' &&
                    context.miterLimit === 3.5 && context.shadowColor === 'rgb(0, 0, 255)' &&
                    context.shadowBlur === 2 && context.shadowOffsetX === 3 &&
                    context.shadowOffsetY === -1;
                context.shadowBlur = 0;
                context.shadowOffsetY = 0;
                context.fillStyle = 'red';
                context.fillRect(1, 2, 1, 1);
                const source = context.getImageData(1, 2, 1, 1).data;
                const shadow = context.getImageData(4, 2, 1, 1).data;
                canvas.width = 9;
                const reset = context.lineCap === 'butt' && context.lineJoin === 'miter' &&
                    context.miterLimit === 10 && context.shadowColor === 'rgba(0, 0, 0, 0)' &&
                    context.shadowBlur === 0 && context.shadowOffsetX === 0 &&
                    context.shadowOffsetY === 0;
                return defaults && restored && source[0] === 255 && source[3] === 255 &&
                    shadow[2] === 255 && shadow[3] === 255 && reset;
            })()"#
        ));
    }

    #[test]
    fn canvas_small_caps_reach_visible_pixels_and_share_font_reset_save_restore_state() {
        let mut engine = Engine::new();
        install_worker(engine.ctx()).unwrap();
        assert!(eval_bool(&mut engine, r#"(() => {
            const canvas=new OffscreenCanvas(240,64), ctx=canvas.getContext('2d');
            ctx.font='small-caps italic 400 32px/2 "Unknown Font", sans-serif';
            if(ctx.font!=='italic small-caps 32px "Unknown Font", sans-serif' || ctx.fontVariantCaps!=='small-caps') throw new Error('shared caps shorthand state');
            ctx.font='small-caps 32px sans-serif';
            const width=ctx.measureText('Hello World').width;
            ctx.fillStyle='red'; ctx.fillText('Hello World',4,40);
            const small=ctx.getImageData(0,0,240,64).data;
            let smallInk=0; for(let i=3;i<small.length;i+=4) if(small[i]>0) smallInk++;
            if(smallInk===0) throw new Error('small caps produced no visible ink');
            ctx.save(); ctx.fontVariantCaps='all-small-caps'; ctx.restore();
            if(ctx.fontVariantCaps!=='small-caps' || ctx.font!=='small-caps 32px sans-serif') throw new Error('caps save restore lost shared state');
            ctx.font='32px sans-serif';
            if(ctx.fontVariantCaps!=='normal') throw new Error('normal font did not reset caps');
            if(ctx.measureText('Hello World').width===width) throw new Error('small caps did not change actual advances');
            ctx.clearRect(0,0,240,64); ctx.fillText('Hello World',4,40);
            const normal=ctx.getImageData(0,0,240,64).data;
            let different=false, normalInk=0;
            for(let i=3;i<normal.length;i+=4) { if(normal[i]>0) normalInk++; if(normal[i]!==small[i]) different=true; }
            if(!different || normalInk===0) throw new Error('small caps pixels matched normal text');
            ctx.fontVariantCaps='small-caps'; const direct=ctx.measureText('Hello World').width;
            ctx.font='small-caps 32px sans-serif';
            if(ctx.measureText('Hello World').width!==direct) throw new Error('caps property and shorthand use different shaping');
            ctx.font='small-caps small-caps 60px serif';
            return ctx.font==='small-caps 32px sans-serif' && ctx.fontVariantCaps==='small-caps';
        })()"#));
    }

    #[test]
    fn canvas_font_style_source_uses_rendered_eligibility_and_recovers_after_mutation() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<div id='parent' style='font-size:24px'><canvas id='source'></canvas></div>", 64).unwrap();
        assert!(eval_bool(&mut engine, r#"(() => {
            const source = document.querySelector('#source');
            const parent = document.querySelector('#parent');
            const ctx = source.getContext('2d');
            ctx.font = '1em sans-serif';
            if (ctx.font !== '24px sans-serif') throw new Error('connected source lost CSS font');
            source.remove();
            ctx.font = '80px serif'; ctx.font = '1em sans-serif';
            if (ctx.font !== '10px sans-serif') throw new Error('detached source used stale computed CSS');
            const detachedWidth = ctx.measureText('iiii').width;
            parent.append(source);
            ctx.font = '1em sans-serif';
            if (ctx.font !== '24px sans-serif') throw new Error('reconnected source did not restore CSS');
            if (!(ctx.measureText('iiii').width > detachedWidth * 2)) throw new Error('reconnected CSS font did not reach shaping');
            parent.style.display = 'none';
            ctx.font = '1em sans-serif';
            if (ctx.font !== '10px sans-serif') throw new Error('hidden ancestor still supplied CSS font');
            parent.style.display = 'block'; source.style.display = 'none';
            ctx.font = '1em sans-serif';
            if (ctx.font !== '10px sans-serif') throw new Error('hidden source still supplied CSS font');
            source.style.display = 'block'; source.style.visibility = 'hidden';
            ctx.font = '1em sans-serif';
            if (ctx.font !== '24px sans-serif') throw new Error('visibility-hidden box incorrectly lost its computed font');
            const detached = document.createElement('canvas');
            detached.style.fontSize = '99px';
            const detachedCtx = detached.getContext('2d');
            detachedCtx.font = '2em sans-serif';
            if (detachedCtx.font !== '20px sans-serif') throw new Error('fresh detached canvas used its inline CSS font');
            return detachedCtx.measureText('iiii').width > detachedWidth * 1.5;
        })()"#));
    }

    #[test]
    fn canvas_query_font_assignment_flushes_valid_dependencies_and_tracks_container_mutations() {
        let mut engine = Engine::new();
        let realm = super::super::install(engine.ctx(),
            "<div id='container' style='container-type:size;width:200px;height:100px'><canvas id='canvas' style='font-size:24px'></canvas></div>", 128).unwrap();
        let calls = Rc::new(std::cell::Cell::new(0));
        let observed = calls.clone();
        realm.set_layout_flusher(Rc::new(move |session| {
            observed.set(observed.get() + 1);
            session.display_list(800, 600, canvas_fallback_fonts()).map(|_| ()).map_err(|error| format!("{error:?}"))
        }));
        assert!(eval_bool(&mut engine, r#"(() => {
            globalThis.context = document.querySelector('#canvas').getContext('2d');
            context.font = '16px serif';
            context.font = 'oblique 100deg 10cqw serif';
            return context.font === '16px serif';
        })()"#));
        assert_eq!(calls.get(), 0);
        assert!(eval_bool(&mut engine, r#"(() => {
            context.font = 'oblique calc(30deg + sign(20cqw - 10px)*5deg) 10cqi serif';
            if (context.font !== 'oblique 35deg 20px serif') throw new Error(context.font);
            document.querySelector('#container').style.width = '10px';
            context.font = 'oblique calc(30deg + sign(20cqw - 10px)*5deg) 10cqi serif';
            return context.font === 'oblique 25deg 1px serif';
        })()"#));
        assert_eq!(calls.get(), 2);
        assert!(eval_bool(&mut engine, r#"(() => {
            document.querySelector('#canvas').style.fontSize = '10cqw';
            context.font = '2em serif';
            return context.font === '2px serif';
        })()"#));
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn canvas_font_context_resolves_relative_calc_and_canonicalizes_atomically() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<canvas id='canvas' style='font-size:24px'></canvas>", 64).unwrap();
        assert!(eval_bool(&mut engine, r#"(() => {
            const off = new OffscreenCanvas(100, 40).getContext('2d');
            off.font = '100px serif';
            off.font = 'calc(2em + 5px)/2 \"Unknown Font\", SERIF';
            if (off.font !== '25px \"Unknown Font\", serif') throw new Error('offscreen font inherited the previous assignment');
            const before = off.measureText('iiii').width;
            off.font = '12px serif';
            if (!(off.measureText('iiii').width < before)) throw new Error('resolved font size did not reach shaping');
            off.font = '20PX   SERIF';
            if (off.font !== '20px serif') throw new Error('font shorthand was not canonicalized');
            off.font = 'var(--font)';
            if (off.font !== '20px serif') throw new Error('invalid font changed canvas state');
            const element = document.querySelector('#canvas').getContext('2d');
            element.font = '150% serif';
            if (element.font !== '36px serif') throw new Error('percentage did not use element CSS font');
            element.font = '2em serif';
            return element.font === '48px serif' && off instanceof OffscreenCanvasRenderingContext2D;
        })()"#));
    }

    #[test]
    fn worker_canvas_installer_exposes_only_worker_context_brand() {
        let mut engine = Engine::new();
        install_worker(engine.ctx()).unwrap();
        assert!(eval_bool(
            &mut engine,
            r#"(()=>{
                const canvas = new OffscreenCanvas(2, 1);
                const context = canvas.getContext('2d');
                context.fillStyle = 'blue';
                context.fillRect(1, 0, 1, 1);
                const pixel = context.getImageData(1, 0, 1, 1).data;
                const ctor = OffscreenCanvasRenderingContext2D;
                return typeof OffscreenCanvas === 'function' &&
                    typeof TextMetrics === 'function' && typeof ImageData === 'function' &&
                    typeof createImageBitmap === 'function' &&
                    typeof CanvasRenderingContext2D === 'undefined' &&
                    typeof document === 'undefined' && context instanceof ctor &&
                    Object.getPrototypeOf(context) === ctor.prototype &&
                    Object.getPrototypeOf(ctor.prototype) === Object.prototype &&
                    canvas.getContext('2d') === context && context.canvas === canvas &&
                    pixel[0] === 0 && pixel[2] === 255 && pixel[3] === 255;
            })()"#
        ));
    }

    #[test]
    fn offscreen_dimensions_enforce_webidl_unsigned_long_long_and_allow_large_empty_axes() {
        let mut engine = Engine::new();
        install_worker(engine.ctx()).unwrap();
        assert!(eval_bool(
            &mut engine,
            r#"(()=>{
                const canvas = new OffscreenCanvas('2.9', 3.9);
                let conversions = 0;
                canvas.width = { valueOf() { conversions++; return '5.9'; } };
                const coercedOnce = canvas.width === 5 && conversions === 1;
                canvas.height = '0';
                canvas.width = 4294967297;
                const beyondU32 = canvas.width === 4294967297;
                const hugeEmpty = new OffscreenCanvas(2 ** 53, 0);
                const invalid = (fn) => {
                    try { fn(); } catch (error) { return error instanceof TypeError; }
                    return false;
                };
                const unavailable = (fn) => {
                    try { fn(); } catch (error) { return error.name === 'InvalidStateError'; }
                    return false;
                };
                const preserved = new OffscreenCanvas(17, 0);
                const rejectInfinity = invalid(() => new OffscreenCanvas(Infinity, 0));
                const rejectNegative = invalid(() => new OffscreenCanvas(-1, 0));
                const rejectU64Limit = invalid(() => new OffscreenCanvas(2 ** 64, 0));
                const rejectSetter = invalid(() => { preserved.width = 2 ** 64; });
                const setterAtomic = preserved.width === 17;
                const oversized = new OffscreenCanvas(16385, 1025);
                const context = oversized.getContext('2d');
                const readFails = unavailable(() => context.getImageData(0, 0, 1, 1));
                const drawFails = unavailable(() => context.fillRect(0, 0, 1, 1));
                return canvas.height === 0 && beyondU32 && hugeEmpty.width === 2 ** 53 &&
                    hugeEmpty.height === 0 && coercedOnce && conversions === 1 &&
                    rejectInfinity && rejectNegative && rejectU64Limit && rejectSetter &&
                    setterAtomic && oversized.width === 16385 && oversized.height === 1025 &&
                    readFails && drawFails;
            })()"#
        ));
    }

    #[test]
    fn image_data_overloads_use_internal_typed_array_brands_and_bounded_storage() {
        let mut engine = Engine::new();
        install_worker(engine.ctx()).unwrap();
        assert!(eval_bool(
            &mut engine,
            r#"(()=>{
                const canvas = new OffscreenCanvas(2, 2);
                const context = canvas.getContext('2d');
                const bytes = new Uint8ClampedArray([1, 2, 3, 4]);
                const image = new ImageData(bytes, 1, 1);
                const blank = context.createImageData(image);
                const dimensions = context.createImageData(-2.9, 1.9);
                const halves = new Float16Array([1, 0.5, 0, 1]);
                const floatImage = new ImageData(halves, 1, 1, {
                    pixelFormat: 'rgba-float16'
                });
                const floatBlank = context.createImageData(1, 1, {
                    pixelFormat: 'rgba-float16'
                });
                const typeError = (fn) => {
                    try { fn(); } catch (error) { return error instanceof TypeError; }
                    return false;
                };
                const indexSize = (fn) => {
                    try { fn(); } catch (error) { return error.name === 'IndexSizeError'; }
                    return false;
                };
                const spoof = {
                    0: 0, 1: 0, 2: 0, 3: 0, length: 4,
                    [Symbol.toStringTag]: 'Uint8ClampedArray'
                };
                return image.data === bytes && image.width === 1 && image.height === 1 &&
                    blank !== image && blank.data !== bytes && Array.from(blank.data).every(v => v === 0) &&
                    dimensions.width === 2 && dimensions.height === 1 &&
                    floatImage.data === halves && floatImage.pixelFormat === 'rgba-float16' &&
                    floatBlank.data instanceof Float16Array && floatBlank.pixelFormat === 'rgba-float16' &&
                    typeError(() => new ImageData(new Uint8Array(4), 1, 1)) &&
                    typeError(() => new ImageData(spoof, 1, 1)) &&
                    typeError(() => new ImageData(Object.create(ImageData.prototype))) &&
                    typeError(() => context.createImageData(null)) &&
                    typeError(() => context.createImageData(NaN, 1)) &&
                    typeError(() => context.createImageData(2147483648, 1)) &&
                    typeError(() => new ImageData(1, 1, true)) &&
                    indexSize(() => context.createImageData(0, 1)) &&
                    indexSize(() => new ImageData(1, 0)) &&
                    indexSize(() => context.createImageData(2147483647, 0));
            })()"#
        ));
    }

    #[test]
    fn put_image_data_clips_dirty_rectangles_and_enforces_signed_long_arguments() {
        let mut engine = Engine::new();
        install_worker(engine.ctx()).unwrap();
        assert!(eval_bool(
            &mut engine,
            r#"(()=>{
                const source = new Uint8ClampedArray(3 * 2 * 4);
                for (let y = 0; y < 2; y++) for (let x = 0; x < 3; x++) {
                    source.set([x * 50, y * 60, x + y, 255], (y * 3 + x) * 4);
                }
                const image = new ImageData(source, 3, 2);
                const canvas = new OffscreenCanvas(5, 4);
                const context = canvas.getContext('2d');
                const pixel = (x, y) => Array.from(context.getImageData(x, y, 1, 1).data);
                const empty = pixel(0, 0)[3] === 0;
                context.putImageData(image, 0, 0, 1, 0, 2, 2);
                const initialDirty = pixel(1, 0).join(',') === '50,0,1,255' &&
                    pixel(2, 1).join(',') === '100,60,3,255' && pixel(0, 0)[3] === 0;
                context.putImageData(image, 3, 2, 3, 2, -2, -2);
                const negativeExtent = pixel(4, 2).join(',') === '50,0,1,255';
                context.putImageData(image, 0, 0, -1, 0, 3, 1);
                const clippedNegativeOrigin = pixel(0, 0).join(',') === '0,0,0,255' &&
                    pixel(1, 0).join(',') === '50,0,1,255';
                context.putImageData(image, 0, 0, 30, 30, 2, 1);
                const outsideNoop = pixel(0, 0).join(',') === '0,0,0,255';
                // With four to six arguments the short Web IDL overload is selected;
                // the extra arguments do not restrict the source rectangle.
                context.putImageData(image, 0, 3, 30);
                const shortOverload = pixel(2, 3).join(',') === '100,0,2,255';
                const typeError = (fn) => {
                    try { fn(); } catch (error) { return error instanceof TypeError; }
                    return false;
                };
                return empty && initialDirty && negativeExtent && clippedNegativeOrigin &&
                    outsideNoop && shortOverload &&
                    typeError(() => context.getImageData(NaN, 0, 1, 1)) &&
                    typeError(() => context.getImageData(0, 0, Infinity, 1)) &&
                    typeError(() => context.getImageData(0, 0, 2147483648, 1)) &&
                    typeError(() => context.putImageData(image, Infinity, 0)) &&
                    typeError(() => context.putImageData(image, 2147483648, 0)) &&
                    typeError(() => context.putImageData(image, 0, 0, 0, 0, -2147483649, 1));
            })()"#
        ));
    }

    #[test]
    fn canvas_colors_reuse_css_color_grammar_and_clamp_rgb_channels() {
        let mut engine = Engine::new();
        install_worker(engine.ctx()).unwrap();
        assert!(eval_bool(
            &mut engine,
            r#"(()=>{
                const context = new OffscreenCanvas(2, 1).getContext('2d');
                context.fillStyle = 'rgb(300 -10 12)';
                context.fillRect(0, 0, 1, 1);
                const pixel = context.getImageData(0, 0, 1, 1).data;
                return pixel[0] === 255 && pixel[1] === 0 && pixel[2] === 12 && pixel[3] === 255;
            })()"#
        ));

        let mut engine = Engine::new();
        super::super::install(
            engine.ctx(),
            "<style>canvas{color:rgb(20,40,60)}</style><canvas width='1' height='1'></canvas>",
            64,
        )
        .unwrap();
        assert!(eval_bool(
            &mut engine,
            r#"(()=>{
                const context = document.querySelector('canvas').getContext('2d');
                context.fillStyle = 'currentColor';
                context.fillRect(0, 0, 1, 1);
                const pixel = context.getImageData(0, 0, 1, 1).data;
                return pixel[0] === 20 && pixel[1] === 40 && pixel[2] === 60 && pixel[3] === 255;
            })()"#
        ));
    }

    #[test]
    fn canvas_and_image_data_settings_read_dictionary_members_in_order() {
        let mut engine = Engine::new();
        install_worker(engine.ctx()).unwrap();
        assert!(eval_bool(
            &mut engine,
            r#"(()=>{
                const contextOrder = [];
                const contextOptions = {
                    get alpha() { contextOrder.push('alpha'); return true; },
                    get colorSpace() {
                        contextOrder.push('colorSpace');
                        return { toString() { contextOrder.push('colorSpace toString'); return 'srgb'; } };
                    },
                    get desynchronized() { contextOrder.push('desynchronized'); return false; },
                    get willReadFrequently() { contextOrder.push('willReadFrequently'); return false; }
                };
                const canvas = new OffscreenCanvas(1, 1);
                const context = canvas.getContext('2d', contextOptions);
                const creationOrder = contextOrder.join('|');
                contextOrder.length = 0;
                const cached = canvas.getContext('2d', contextOptions) === context;
                const cachedUntouched = contextOrder.length === 0;

                const imageOrder = [];
                const image = new ImageData(1, 1, {
                    get colorSpace() {
                        imageOrder.push('colorSpace');
                        return { toString() { imageOrder.push('colorSpace toString'); return 'srgb'; } };
                    },
                    get pixelFormat() { imageOrder.push('pixelFormat'); return 'rgba-unorm8'; }
                });
                const imageCreationOrder = imageOrder.join('|');
                const invalidContext = (name) => {
                    try { canvas.getContext(name); } catch (error) { return error instanceof TypeError; }
                    return false;
                };
                const primitiveOptions = ['ignored', 42, false, true, null, undefined].every(options =>
                    new OffscreenCanvas(1, 1).getContext('2d', options) instanceof OffscreenCanvasRenderingContext2D
                );
                return creationOrder === 'alpha|colorSpace|colorSpace toString|desynchronized|willReadFrequently' &&
                    cached && cachedUntouched && imageCreationOrder === 'colorSpace|colorSpace toString|pixelFormat' &&
                    image.colorSpace === 'srgb' && image.pixelFormat === 'rgba-unorm8' &&
                    new OffscreenCanvas(1, 1).getContext('2d', true) instanceof OffscreenCanvasRenderingContext2D &&
                    new OffscreenCanvas(1, 1).getContext('2d', 'primitive', 'ignored') instanceof OffscreenCanvasRenderingContext2D &&
                    primitiveOptions &&
                    invalidContext('') && invalidContext('2D') && invalidContext('3d') &&
                    invalidContext(undefined);
            })()"#
        ));
    }

    #[test]
    fn offscreen_context_owner_edge_is_traced_and_collectible() {
        let mut engine = Engine::new();
        install_worker(engine.ctx()).unwrap();
        assert!(eval_bool(
            &mut engine,
            r#"(()=>{
                const canvas = new OffscreenCanvas(1, 1);
                canvas.ownerMarker = { value: 73 };
                const context = canvas.getContext('2d');
                globalThis.keptOffscreenContext = context;
                return context.canvas === canvas;
            })()"#
        ));

        let (canvas_weak, context_weak): (WeakValue, WeakValue) = {
            let ctx = engine.ctx();
            let global = ctx.global_object();
            let context_value = ctx
                .member_get(&global, "keptOffscreenContext")
                .ok()
                .expect("global context root exists");
            let canvas_value = ctx
                .member_get(&context_value, "canvas")
                .ok()
                .expect("context canvas getter succeeds");
            let canvas_weak = ctx
                .weak_value(&canvas_value)
                .expect("offscreen canvas wrapper is an object");
            let context_weak = ctx
                .weak_value(&context_value)
                .expect("offscreen context wrapper is an object");
            (canvas_weak, context_weak)
        };
        engine.collect_garbage();

        let (canvas, context) = {
            let ctx = engine.ctx();
            let context = context_weak
                .upgrade()
                .expect("the JavaScript global keeps the context alive");
            let canvas = canvas_weak
                .upgrade()
                .expect("the rooted context traces its original canvas");
            let current_canvas = ctx
                .member_get(&context, "canvas")
                .ok()
                .expect("context canvas getter still succeeds after collection");
            assert!(ctx.values_strict_equal(&canvas, &current_canvas));
            let marker = ctx
                .member_get(&canvas, "ownerMarker")
                .ok()
                .expect("canvas expando survives through the context owner edge");
            assert!(matches!(
                ctx.member_get(&marker, "value"),
                Ok(Value::Num(value)) if value == 73.0
            ));
            (canvas, context)
        };

        {
            let ctx = engine.ctx();
            assert!(ctx
                .set_member(&canvas, "contextCycle", context.clone())
                .is_ok());
            let global = ctx.global_object();
            assert!(ctx
                .delete_member(&global, "keptOffscreenContext")
                .unwrap_or(false));
        }
        drop(context);
        drop(canvas);
        engine.collect_garbage();
        assert!(
            context_weak.upgrade().is_none(),
            "unrooted context/canvas cycle is collectible"
        );
        assert!(
            canvas_weak.upgrade().is_none(),
            "unrooted canvas/context cycle is collectible"
        );
    }

    #[test]
    fn bitmaprenderer_publishes_transferred_bitmap_to_retained_display_list() {
        let mut engine = Engine::new();
        let realm =
            super::super::install(engine.ctx(), "<canvas width='2' height='1'></canvas>", 64)
                .unwrap();
        assert!(eval_bool(
            &mut engine,
            "(()=>{const source=new OffscreenCanvas(2,1),x=source.getContext('2d');x.fillStyle='red';x.fillRect(0,0,2,1);document.querySelector('canvas').getContext('bitmaprenderer').transferFromImageBitmap(source.transferToImageBitmap());return true})()"
        ));
        let mut session = realm.session.borrow_mut();
        let list = session.display_list(32, 32, canvas_fallback_fonts()).unwrap();
        assert!(list.0.iter().any(|command| matches!(command, lumen_html::paint::Command::Image { image, .. } if image.width == 2 && image.height == 1 && image.pixels == [255,0,0,255,255,0,0,255])));
    }

    #[test]
    fn bitmaprenderer_transfers_pixels_and_excludes_other_contexts() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            r#"(()=>{
            const source = new OffscreenCanvas(2,1), s = source.getContext('2d');
            s.fillStyle = 'red'; s.fillRect(0,0,2,1);
            const bitmap = source.transferToImageBitmap();
            const canvas = document.createElement('canvas'); canvas.width=4; canvas.height=3;
            const renderer = canvas.getContext('bitmaprenderer');
            const same = renderer === canvas.getContext('bitmaprenderer') && renderer.canvas === canvas;
            renderer.transferFromImageBitmap(bitmap);
            const detached = bitmap.width === 0 && bitmap.height === 0;
            let rejected = false; try { renderer.transferFromImageBitmap(bitmap); } catch(e) { rejected = e.name === 'InvalidStateError'; }
            canvas.width = 4;
            const out = new OffscreenCanvas(4,3), x = out.getContext('2d'); x.drawImage(canvas,0,0);
            const pixel = x.getImageData(1,0,1,1).data;
            const dimensions = canvas.width === 4 && canvas.height === 3;
            const exclusive = canvas.getContext('2d') === null && source.getContext('bitmaprenderer') === null;
            renderer.transferFromImageBitmap(null); x.clearRect(0,0,4,3); x.drawImage(canvas,0,0);
            return same && detached && rejected && dimensions && exclusive && pixel[0]===255 && pixel[3]===255 && x.getImageData(1,0,1,1).data[3]===0;
        })()"#
        ));
    }

    #[test]
    fn bitmaprenderer_opaque_output_and_offscreen_transfer_reset() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        let bitmap = new_image_bitmap(
            engine.ctx(),
            Rgba8Image {
                width: 1,
                height: 1,
                pixels: vec![200, 100, 50, 128],
            },
            true,
        )
        .unwrap();
        let global = engine.ctx().global_object();
        engine
            .ctx()
            .set_member(&global, "inputBitmap", bitmap)
            .unwrap_or_else(|_| panic!("bitmap test global installation failed"));
        assert!(eval_bool(
            &mut engine,
            r#"(()=>{
            const c = new OffscreenCanvas(2,2), renderer=c.getContext('bitmaprenderer',{alpha:false});
            renderer.transferFromImageBitmap(inputBitmap);
            const image = c.transferToImageBitmap(), target=new OffscreenCanvas(2,2), x=target.getContext('2d');
            x.drawImage(image,0,0); const p=x.getImageData(0,0,1,1).data;
            const dims = c.width===2 && c.height===2 && image.width===1 && image.height===1;
            const blank=c.transferToImageBitmap(); x.clearRect(0,0,2,2); x.drawImage(blank,0,0);
            const b=x.getImageData(1,1,1,1).data;
            return renderer.canvas===c && dims && p[0]===100 && p[1]===50 && p[3]===255 && b[0]===0 && b[3]===255;
        })()"#
        ));
    }

    #[test]
    fn bitmaprenderer_propagates_taint_and_null_restores_clean_bitmap() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        let bitmap = new_image_bitmap_with_origin(
            engine.ctx(),
            Rgba8Image {
                width: 1,
                height: 1,
                pixels: vec![255, 0, 0, 255],
            },
            true,
            false,
        )
        .unwrap();
        let global = engine.ctx().global_object();
        engine
            .ctx()
            .set_member(&global, "inputBitmap", bitmap)
            .unwrap_or_else(|_| panic!("bitmap test global installation failed"));
        assert!(eval_bool(
            &mut engine,
            r#"(()=>{
            const c=document.createElement('canvas'), r=c.getContext('bitmaprenderer');
            r.transferFromImageBitmap(inputBitmap);
            let denied=false; try{c.toDataURL()}catch(e){denied=e.name==='SecurityError'}
            c.width=c.width;
            let resizeDenied=false; try{c.toDataURL()}catch(e){resizeDenied=e.name==='SecurityError'}
            const target=new OffscreenCanvas(1,1), x=target.getContext('2d'); x.drawImage(c,0,0);
            let propagated=false; try{x.getImageData(0,0,1,1)}catch(e){propagated=e.name==='SecurityError'}
            r.transferFromImageBitmap(null);
            return denied && resizeDenied && propagated && c.toDataURL().startsWith('data:image/png');
        })()"#
        ));
    }

    #[test]
    fn canvas_layers_restore_native_and_js_styles_for_dom_and_offscreen() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<canvas></canvas>", 64).unwrap();
        assert!(eval_bool(&mut engine, r#"(() => {
            for (const c of [document.querySelector('canvas'), new OffscreenCanvas(2, 1)]) {
                c.width=2; c.height=1;
                const x=c.getContext('2d');
                x.fillStyle='purple'; x.font='12px serif'; x.globalAlpha=.5;
                const beforeStyle=x.fillStyle;
                x.beginLayer(); x.fillStyle='red'; x.font='24px serif';
                x.save(); x.fillRect(0,0,1,1);
                x.beginLayer(); x.fillStyle='lime'; x.fillRect(1,0,1,1); x.endLayer();
                x.endLayer();
                if (x.fillStyle !== beforeStyle || x.font !== '12px serif' || x.globalAlpha !== .5) throw Error('layer entry styles not restored');
                const pixels=x.getImageData(0,0,2,1).data;
                if(pixels[0]!==255 || pixels[3]!==128 || pixels[5]!==255 || pixels[7]!==128) throw Error('layer pixels '+Array.from(pixels));
                let denied=false; try{x.endLayer()}catch(e){denied=e.name==='InvalidStateError'}
                if(!denied) return false;
                x.beginLayer(); c.width=2;
                denied=false; try{x.endLayer()}catch(e){denied=e.name==='InvalidStateError'}
                if(!denied) return false;
            }
            return true;
        })()"#));
    }

    #[test]
    fn canvas_context_draws_real_pixels_and_same_value_resets_bitmap() {
        let mut engine = Engine::new();
        let realm = super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "(()=>{const c=document.createElement('canvas'); c.id='resetCanvas'; c.width=2; c.height=2; document.querySelector('main').append(c); const x=c.getContext('2d'); const same=x===c.getContext('2d') && c.getContext('2D')===null; x.fillStyle='red'; x.fillRect(0,0,2,2); const painted=x.getImageData(0,0,1,1).data[0]===255 && x.getImageData(0,0,1,1).data[3]===255; c.width=2; const idlReset=x.getImageData(0,0,1,1).data[3]===0; x.fillRect(0,0,1,1); return same && painted && idlReset && x.getImageData(0,0,1,1).data[3]===255})()"
        ));
        let surface = {
            let session = realm.session.borrow();
            let document = session.document();
            let canvas =
                lumen_html::selector::query_selector(document, document.root(), "#resetCanvas")
                    .unwrap()
                    .unwrap();
            let surface = realm
                .canvases
                .surfaces
                .borrow()
                .get(&canvas)
                .cloned()
                .expect("2D context creates a native canvas surface");
            surface
        };
        let before_repeat = surface.borrow().gpu_generation;
        assert!(eval_bool(
            &mut engine,
            "resetCanvas.setAttribute('width','2'); true"
        ));
        assert_eq!(
            surface.borrow().gpu_generation,
            before_repeat + 1,
            "same-value content attribute changes reset the bitmap once"
        );
        let after_repeat = surface.borrow().gpu_generation;
        assert!(eval_bool(
            &mut engine,
            "resetCanvas.setAttributeNS('urn:custom','width','9'); true"
        ));
        assert_eq!(
            surface.borrow().gpu_generation,
            after_repeat,
            "a width attribute in another namespace does not resize the canvas"
        );
    }

    #[test]
    fn offscreen_canvas_and_image_data_share_pixel_bytes() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "const c=new OffscreenCanvas(1,1), x=c.getContext('2d'), d=new ImageData(1,1); d.data[0]=12; d.data[1]=34; d.data[2]=56; d.data[3]=255; x.putImageData(d,0,0); const p=x.getImageData(0,0,1,1); p.data[0]===12 && p.data[1]===34 && p.data[2]===56 && p.data[3]===255 && p.data===p.data"
        ));
    }

    #[test]
    fn detached_canvas_context_remains_usable_without_publishing_stale_nodes() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "const c=document.createElement('canvas'), x=c.getContext('2d'); document.body.append(c); x.fillStyle='blue'; x.fillRect(0,0,1,1); c.remove(); x.fillRect(1,0,1,1); x.getImageData(1,0,1,1).data[2]===255"
        ));
    }

    #[test]
    fn draw_image_blits_canvas_sources_and_exports_png_data_urls() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "const src=document.createElement('canvas'); src.width=1; src.height=1; const sc=src.getContext('2d'); sc.fillStyle='#00ff00'; sc.fillRect(0,0,1,1); const dst=document.createElement('canvas'); dst.width=2; dst.height=1; const dc=dst.getContext('2d'); dc.drawImage(src,0,0,2,1); dc.getImageData(1,0,1,1).data[1]===255 && dst.toDataURL().startsWith('data:image/png;base64,')"
        ));
    }

    #[test]
    fn draw_image_uses_current_video_frame_and_preserves_origin_taint() {
        let mut engine = Engine::new();
        let realm = super::super::install(
            engine.ctx(),
            "<video id='clean'></video><video id='tainted'></video>",
            64,
        )
        .unwrap();
        for (id, origin_clean) in [("clean", true), ("tainted", false)] {
            let value = engine
                .eval_value(&format!("document.querySelector('#{id}')"))
                .unwrap_or_else(|_| panic!("video query evaluation failed"))
                .unwrap_or_else(|_| panic!("video query threw"));
            let node = engine
                .ctx()
                .with_instance::<crate::media::DomHtmlVideoElement, _>(&value, |video| {
                    video.node().id
                })
                .unwrap();
            let generation =
                realm.media_select_source(node, format!("https://media.test/{id}.mp4"));
            realm.media_video_loaded(node, generation, 1.0, 1, 1, origin_clean);
            realm
                .media_set_video_frame(
                    node,
                    generation,
                    Some(crate::VideoFrameSnapshot {
                        presentation_time_micros: 0,
                        width: 1,
                        height: 1,
                        rgba: Arc::new(vec![20, 40, 60, 255]),
                    }),
                )
                .unwrap();
        }
        assert!(eval_bool(
            &mut engine,
            "(()=>{const c=document.createElement('canvas'),x=c.getContext('2d');x.drawImage(document.querySelector('#clean'),0,0);const p=x.getImageData(0,0,1,1).data;return p[0]===20&&p[1]===40&&p[2]===60&&p[3]===255})()"
        ));
        assert!(eval_bool(
            &mut engine,
            "(()=>{const c=document.createElement('canvas'),x=c.getContext('2d');x.drawImage(document.querySelector('#tainted'),0,0);try{c.toDataURL();return false}catch(e){return e.name==='SecurityError'}})()"
        ));
    }

    #[test]
    fn tainted_canvas_rejects_pixel_readback_and_offscreen_blob_export() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        let data = Rc::new(RefCell::new(CanvasData::new(1, 1).unwrap()));
        data.borrow_mut().origin_clean = false;
        let context = DomCanvasRenderingContext2D {
            data: data.clone(),
            realm: None,
            node: None,
            canvas_in_private_slot: false,
        };
        let error = context
            .get_image_data(
                engine.ctx(),
                Value::Num(0.0),
                Value::Num(0.0),
                Value::Num(1.0),
                Value::Num(1.0),
                None,
            )
            .err()
            .expect("tainted canvas must reject readback");
        assert_eq!(error.class(), "SecurityError");
        let offscreen = DomOffscreenCanvas { data, realm: None, placeholder:None, detached: Cell::new(false) };
        assert!(offscreen.convert_to_blob(engine.ctx(), None).is_ok());
    }

    #[test]
    fn canvas_exports_jpeg_webp_png_fallback_and_jpeg_quality() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "(()=>{const c=document.createElement('canvas');c.width=24;c.height=24;const x=c.getContext('2d'),d=new ImageData(24,24);for(let y=0;y<24;y++)for(let z=0;z<24;z++){const i=(y*24+z)*4;d.data[i]=(z*37+y*17)%256;d.data[i+1]=(z*13+y*47)%256;d.data[i+2]=(z*23+y*31)%256;d.data[i+3]=255}x.putImageData(d,0,0);const low=c.toDataURL('image/jpeg',0.1),high=c.toDataURL('image/jpeg',0.95),webp=c.toDataURL('image/webp',0.5),fallback=c.toDataURL('image/unsupported');return low.startsWith('data:image/jpeg;base64,')&&high.startsWith('data:image/jpeg;base64,')&&low!==high&&webp.startsWith('data:image/webp;base64,')&&fallback.startsWith('data:image/png;base64,')})()"
        ));
    }

    #[test]
    fn to_blob_callback_runs_as_a_task_with_encoded_type_and_snapshot() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(lumen_host::lazy_globals::<lumen_host::blob::bindings::Module>(engine.ctx()).is_ok());
        assert!(eval_bool(
            &mut engine,
            "globalThis.__canvasBlobState='pending';(()=>{const c=document.createElement('canvas');c.width=2;c.height=2;const x=c.getContext('2d');x.fillStyle='red';x.fillRect(0,0,2,2);c.toBlob(blob=>globalThis.__canvasBlobState=blob.type+':'+blob.size,'image/jpeg',0.8)})();globalThis.__canvasBlobState==='pending'"
        ));
        assert!(super::scheduling::task_pending(engine.ctx()));
        let errors = super::scheduling::run_tasks(&mut engine, 8);
        assert!(
            errors.is_empty(),
            "toBlob task produced {} errors",
            errors.len()
        );
        assert!(eval_bool(
            &mut engine,
            "globalThis.__canvasBlobState.startsWith('image/jpeg:')&&Number(globalThis.__canvasBlobState.split(':')[1])>0"
        ));
    }

    #[test]
    fn linear_gradient_stops_drive_raster_pixels_and_restore_style_identity() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "(()=>{const c=new OffscreenCanvas(4,1),x=c.getContext('2d'),g=x.createLinearGradient(0,0,4,0);g.addColorStop(0,'red');g.addColorStop(1,'blue');x.save();x.fillStyle=g;const identity=x.fillStyle===g;x.fillRect(0,0,4,1);const p=x.getImageData(0,0,4,1).data;const gradient=p[0]>p[2]&&p[12]<p[14];x.restore();return identity&&gradient&&typeof x.fillStyle==='string'})()"
        ));
    }

    #[test]
    fn equal_radius_radial_gradient_rasterizes_distinct_stops() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "(()=>{const c=new OffscreenCanvas(10,3),x=c.getContext('2d'),g=x.createRadialGradient(0,1,3,8,1,3);g.addColorStop(0,'red');g.addColorStop(1,'blue');x.fillStyle=g;x.fillRect(0,0,10,3);const a=x.getImageData(1,1,1,1).data,b=x.getImageData(3,1,1,1).data;return a[0]>b[0]+20&&a[2]+20<b[2]&&a[3]===255&&b[3]===255})()"
        ));
    }

    #[test]
    fn repeating_canvas_pattern_rasterizes_and_retains_style_identity() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "(()=>{const s=new OffscreenCanvas(1,1),sc=s.getContext('2d');sc.fillStyle='green';sc.fillRect(0,0,1,1);const d=new OffscreenCanvas(3,1),c=d.getContext('2d'),p=c.createPattern(s,'repeat');c.fillStyle=p;c.fillRect(0,0,3,1);const bytes=c.getImageData(0,0,3,1).data;return c.fillStyle===p&&bytes[1]===128&&bytes[5]===128&&bytes[9]===128})()"
        ));
    }

    #[test]
    fn text_uses_shared_font_shaping_and_rasterizes_ink_pixels() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "(()=>{const c=new OffscreenCanvas(120,40),x=c.getContext('2d');x.font='20px sans-serif';x.fillStyle='white';const m=x.measureText('Canvas');x.fillText('Canvas',2,28);const pixels=x.getImageData(0,0,120,40).data;let ink=0;for(let i=3;i<pixels.length;i+=4)if(pixels[i])ink++;return m instanceof TextMetrics&&m.width>20&&m.actualBoundingBoxAscent>0&&ink>20&&x.font==='20px sans-serif'})()"
        ));
    }

    #[test]
    fn canvas_text_invalid_geometry_is_a_noop_and_infinite_width_is_unconstrained() {
        let mut engine = Engine::new();
        install_worker(engine.ctx()).unwrap();
        assert!(eval_bool(
            &mut engine,
            r#"(() => {
                const canvas = new OffscreenCanvas(80, 40);
                const context = canvas.getContext('2d');
                context.font = '20px sans-serif';
                for (const method of ['fillText', 'strokeText']) {
                    for (const width of [0, -1, NaN, -Infinity]) {
                        context[method]('Canvas', 2, 28, width);
                    }
                    context[method]('Canvas', NaN, 28);
                    context[method]('Canvas', 2, Infinity);
                }
                if (context.getImageData(0, 0, 80, 40).data.some(value => value !== 0)) return false;
                for (const method of ['fillText', 'strokeText']) {
                    context.clearRect(0, 0, 80, 40);
                    context[method]('Canvas', 2, 28);
                    const expected = context.getImageData(0, 0, 80, 40).data;
                    if (!expected.some(value => value !== 0)) return false;
                    context.clearRect(0, 0, 80, 40);
                    context[method]('Canvas', 2, 28, Infinity);
                    const actual = context.getImageData(0, 0, 80, 40).data;
                    if (!actual.every((value, index) => value === expected[index])) return false;
                }
                return true;
            })()"#
        ));
    }

    #[test]
    fn canvas_text_region_raster_preserves_transformed_pixels_on_large_canvases() {
        let mut engine = Engine::new();
        install_worker(engine.ctx()).unwrap();
        assert!(eval_bool(
            &mut engine,
            r#"(() => {
            const small = new OffscreenCanvas(160, 96).getContext('2d');
            const large = new OffscreenCanvas(2048, 2048).getContext('2d');
            for (const [context, offset] of [[small, 0], [large, 1000]]) {
                context.font = '20px sans-serif';
                context.fillStyle = '#336699';
                context.shadowColor = '#ff0000';
                context.shadowBlur = 2;
                context.shadowOffsetX = 3;
                context.setTransform(1, 0.25, 0.5, 1, 16 + offset, 8 + offset);
                context.fillText('Text', 2, 28, 36);
            }
            const expected = small.getImageData(0, 0, 160, 96).data;
            const actual = large.getImageData(1000, 1000, 160, 96).data;
            const bounds = data => {
                let left = 160, top = 96, right = -1, bottom = -1;
                for (let pixel = 0; pixel < data.length / 4; pixel++) {
                    if (data[pixel * 4 + 3] === 0) continue;
                    const x = pixel % 160, y = Math.floor(pixel / 160);
                    left = Math.min(left, x); top = Math.min(top, y);
                    right = Math.max(right, x); bottom = Math.max(bottom, y);
                }
                return [left, top, right, bottom];
            };
            for (let index = 0; index < actual.length; index++) {
                if (actual[index] !== expected[index]) {
                    throw new Error(JSON.stringify({index,
                        expected: expected[index], actual: actual[index],
                        expectedBounds: bounds(expected), actualBounds: bounds(actual)}));
                }
            }
            return expected.some(value => value !== 0) &&
                actual.every((value, index) => value === expected[index]);
        })()"#
        ));
    }

    #[test]
    fn stroke_text_rasterizes_outline_pixels_distinct_from_fill() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "(()=>{const c=new OffscreenCanvas(160,64),x=c.getContext('2d');x.font='bold 44px monospace';const m=x.measureText('I');x.fillText('I',10,55);x.strokeText('I',90,55);const p=x.getImageData(0,0,160,64).data;let filled=0,outlined=0,different=0;for(let y=0;y<64;y++)for(let z=0;z<70;z++){const a=p[(y*160+z)*4+3],b=p[(y*160+z+80)*4+3];if(a)filled++;if(b)outlined++;if(a!==b)different++}const cx=10+Math.floor(m.width/2),cy=55-Math.floor(m.actualBoundingBoxAscent/2);const fillInterior=p[(cy*160+cx)*4+3],strokeInterior=p[(cy*160+cx+80)*4+3];return filled>0&&outlined>0&&different>0&&fillInterior>0&&strokeInterior<fillInterior})()"
        ));
    }

    #[test]
    fn canvas_sans_bold_uses_the_registered_bold_face() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "(()=>{const x=new OffscreenCanvas(1,1).getContext('2d');x.font='40px sans-serif';const regular=x.measureText('ink').width;x.font='bold 40px sans-serif';const bold=x.measureText('ink').width;return bold>regular})()"
        ));
    }

    #[test]
    fn text_metrics_alignment_baseline_and_max_width_share_one_font_run() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        let source = "(()=>{const c=new OffscreenCanvas(100,40),x=c.getContext('2d');x.font='20px monospace';const m=x.measureText('MMMM');x.textAlign='center';x.fillText('MMMM',50,25);const p=x.getImageData(0,0,100,40).data;let minX=100,maxX=-1,minY=40,maxY=-1;for(let y=0;y<40;y++)for(let z=0;z<100;z++)if(p[(y*100+z)*4+3]){minX=Math.min(minX,z);maxX=Math.max(maxX,z);minY=Math.min(minY,y);maxY=Math.max(maxY,y)}const centered=minX<50&&maxX>50&&Math.abs((minX+maxX)/2-50)<3;const baseline=minY>=25-m.actualBoundingBoxAscent-2&&maxY<=25+m.actualBoundingBoxDescent+2;const d=new OffscreenCanvas(40,40),y=d.getContext('2d');y.font='20px monospace';y.fillText('MMMM',4,25,10);const q=y.getImageData(0,0,40,40).data;let right=-1;for(let yy=0;yy<40;yy++)for(let j=0;j<40;j++)if(q[(yy*40+j)*4+3])right=Math.max(right,j);const ok=m.width>10&&centered&&baseline&&right>=4&&right<=16;return ok?'ok':JSON.stringify({width:m.width,ascent:m.actualBoundingBoxAscent,descent:m.actualBoundingBoxDescent,minX,maxX,minY,maxY,centered,baseline,maxWidthRight:right})})()";
        let diagnostic = match engine.eval_value(source) {
            Ok(Ok(Value::Str(value))) => value.as_str().to_owned(),
            Ok(Ok(_)) => "unexpected JavaScript result type".to_owned(),
            Ok(Err(_)) => "JavaScript evaluation ended abruptly".to_owned(),
            Err(_) => "JavaScript evaluation could not start".to_owned(),
        };
        assert_eq!(diagnostic, "ok", "Canvas text metrics: {diagnostic}");
    }

    #[test]
    fn canvas_text_spacing_kerning_caps_stretch_and_rendering_controls_are_shaped() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        let diagnostic = match engine.eval_value(
            "(()=>{const c=new OffscreenCanvas(180,56),x=c.getContext('2d');x.font='20px monospace';const base=x.measureText('A B').width;x.letterSpacing='2px';x.wordSpacing='3px';const spaced=x.measureText('A B').width;x.fillText('A B',4,36);const p=x.getImageData(0,0,180,56).data;let max=-1;for(let y=0;y<56;y++)for(let z=0;z<180;z++)if(p[(y*180+z)*4+3])max=Math.max(max,z);x.letterSpacing='normal';x.wordSpacing='normal';x.font='48px sans-serif';x.fontKerning='none';const unkerned=x.measureText('AV').width;x.fontKerning='normal';const kerned=x.measureText('AV').width;x.fontKerning='auto';x.textRendering='optimizeSpeed';const fast=x.measureText('AV').width;x.textRendering='optimizeLegibility';const legible=x.measureText('AV').width;x.fontVariantCaps='small-caps';const caps=x.fontVariantCaps;x.fontStretch='condensed';const stretch=x.fontStretch;x.font='condensed 20px sans-serif';const shorthand=x.fontStretch;return [base,spaced,max,kerned,unkerned,fast,legible,caps,stretch,shorthand].join('|')})()",
        ) {
            Ok(Ok(Value::Str(value))) => value.as_str().to_owned(),
            Ok(Ok(_)) => "unexpected JavaScript result type".to_owned(),
            Ok(Err(_)) => "JavaScript evaluation ended abruptly".to_owned(),
            Err(_) => "JavaScript evaluation could not start".to_owned(),
        };
        let values = diagnostic.split('|').collect::<Vec<_>>();
        assert_eq!(
            values.len(),
            10,
            "Canvas text option diagnostic: {diagnostic}"
        );
        let number = |index: usize| values[index].parse::<f64>().unwrap();
        assert_eq!(number(1) - number(0), 7.0);
        assert!(number(2) > number(0) + 4.0);
        assert!(number(3) < number(4));
        assert_ne!(values[5], values[6]);
        assert_eq!(values[7], "small-caps");
        assert_eq!(values[8], "condensed");
        assert_eq!(values[9], "condensed");
    }

    #[test]
    fn specification_font_relative_units_canvas_use_unshaped_metrics_and_root_context() {
        let mut engine=Engine::new();
        let realm=super::super::install(engine.ctx(),"<!doctype html><style>html{font-size:20px;line-height:2}</style><canvas id=c></canvas>",64).unwrap();
        let diagnostic=engine.eval_value(r#"(() => {
            const context=document.getElementById('c').getContext('2d');
            context.font='10px monospace';
            context.letterSpacing='1rlh';
            if(context.letterSpacing!=='40px')throw new Error('canvas rlh did not use the actual document root');
            context.wordSpacing='calc(1cap + 2px)';
            const cap=parseFloat(context.wordSpacing);
            if(!(cap>2))throw new Error('canvas cap metric was rejected');
            document.documentElement.style.lineHeight='3';
            context.letterSpacing='1rlh';
            if(context.letterSpacing!=='60px')throw new Error('canvas root line-height mutation was stale');
            context.font='1rlh monospace';
            if(!context.font.includes('60px'))throw new Error('canvas font shorthand did not resolve genuine rlh');
            return true;
        })()"#).unwrap().unwrap_or_else(|error|panic!("font-unit canvas guard: {}",engine.ctx().coerce_string(&error).map(|text|text.to_string()).unwrap_or_default()));
        assert!(matches!(diagnostic,Value::Bool(true)));
        let _=realm;
    }

    #[test]
    fn canvas_spacing_resolves_css_font_relative_absolute_and_calc_lengths() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "(()=>{const x=new OffscreenCanvas(1,1).getContext('2d');x.font='20px monospace';x.letterSpacing='1em';const em=x.letterSpacing==='20px';x.letterSpacing='1rem';const rem=x.letterSpacing==='16px';x.letterSpacing='1ch';const ch=Math.abs(parseFloat(x.letterSpacing)-x.measureText('0').width)<0.01;x.letterSpacing='calc(1em + 2px)';const calc=x.letterSpacing==='22px';x.wordSpacing='1in';const abs=x.wordSpacing==='96px';x.wordSpacing='bad';return em&&rem&&ch&&calc&&abs&&x.wordSpacing==='96px'})()"
        ));
    }

    #[test]
    fn canvas_spacing_resolves_rem_from_document_root_font_size() {
        let mut engine = Engine::new();
        super::super::install(
            engine.ctx(),
            "<style>html { font-size: 32px }</style><canvas></canvas>",
            64,
        )
        .unwrap();
        assert!(eval_bool(
            &mut engine,
            "(()=>{const x=document.querySelector('canvas').getContext('2d');x.letterSpacing='1rem';return x.letterSpacing==='32px'})()"
        ));
    }

    #[test]
    fn arc_and_ellipse_paths_render_under_current_transform_and_validate_radii() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "(()=>{const c=new OffscreenCanvas(32,24),x=c.getContext('2d');x.translate(5,0);x.beginPath();x.arc(8,10,4,0,Math.PI*2);x.fillStyle='red';x.fill();const p=x.getImageData(13,10,1,1).data;let arcError=false,ellipseError=false;try{x.arc(2,2,-1,0,1)}catch(e){arcError=e.name==='IndexSizeError'}try{x.ellipse(2,2,1,-1,0,0,1)}catch(e){ellipseError=e.name==='IndexSizeError'}x.setTransform(1,0,0,1,0,0);x.beginPath();x.ellipse(23,10,5,3,Math.PI/4,0,Math.PI*2);x.fillStyle='blue';x.fill();const q=x.getImageData(23,10,1,1).data;return p[0]===255&&p[3]===255&&q[2]===255&&q[3]===255&&arcError&&ellipseError})()"
        ));
    }

    #[test]
    fn path2d_can_be_cloned_and_filled_with_current_transform() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "(()=>{const p=new Path2D();p.rect(1,1,4,4);const copy=new Path2D(p),svg=new Path2D('M2 2 h6 v6 h-6 z'),joined=new Path2D();joined.addPath(svg,{e:1,f:0});const c=new OffscreenCanvas(12,10),x=c.getContext('2d');x.translate(2,0);x.fillStyle='blue';x.fill(copy);const pixels=x.getImageData(4,3,1,1).data;x.fill(svg);const svgPixel=x.getImageData(9,7,1,1).data;x.fill(joined);const joinedPixel=x.getImageData(10,7,1,1).data;let invalid=false;try{new Path2D('M0,,0 L1 1')}catch(e){invalid=e.name==='SyntaxError'}return copy instanceof Path2D&&pixels[2]===255&&pixels[3]===255&&svgPixel[2]===255&&svgPixel[3]===255&&joinedPixel[2]===255&&joinedPixel[3]===255&&invalid&&svg instanceof Path2D})()"
        ));
    }

    #[test]
    fn transfer_to_image_bitmap_preserves_pixels_and_clears_only_the_offscreen_bitmap() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "(()=>{const c=new OffscreenCanvas(2,1),x=c.getContext('2d');x.fillStyle='red';x.fillRect(0,0,2,1);const b=c.transferToImageBitmap(),cleared=x.getImageData(0,0,2,1).data[3]===0,d=new OffscreenCanvas(2,1),y=d.getContext('2d');y.drawImage(b,0,0);const p=y.getImageData(1,0,1,1).data;const was=b instanceof ImageBitmap&&b.width===2&&b.height===1;b.close();return was&&cleared&&p[0]===255&&p[3]===255&&b.closed&&b.width===0})()"
        ));
    }

    #[test]
    fn create_image_bitmap_resize_and_flip_y_options_transform_asymmetric_pixels() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "globalThis.__imageBitmapOptionsState='pending';(()=>{const s=new OffscreenCanvas(1,2),x=s.getContext('2d');x.fillStyle='red';x.fillRect(0,0,1,1);x.fillStyle='blue';x.fillRect(0,1,1,1);createImageBitmap(s,{resizeWidth:2,imageOrientation:'flipY',resizeQuality:'pixelated'}).then(b=>{try{const d=new OffscreenCanvas(b.width,b.height),y=d.getContext('2d');y.drawImage(b,0,0);const top=y.getImageData(0,0,1,1).data,bottom=y.getImageData(0,b.height-1,1,1).data;globalThis.__imageBitmapOptionsState=`ready:${b.width}x${b.height}:${Array.from(top)}:${Array.from(bottom)}`}catch(e){globalThis.__imageBitmapOptionsState='callback-error:'+String(e&&e.stack||e)}}).catch(e=>globalThis.__imageBitmapOptionsState='rejected:'+String(e&&e.stack||e))})();true"
        ));
        engine.ctx().drain_microtasks_for_host();
        let state = match engine.eval_value("globalThis.__imageBitmapOptionsState") {
            Ok(Ok(Value::Str(value))) => value.as_str().to_owned(),
            Ok(Ok(_)) => "unexpected JavaScript result type".to_owned(),
            Ok(Err(_)) => "JavaScript evaluation ended abruptly".to_owned(),
            Err(_) => "JavaScript evaluation could not start".to_owned(),
        };
        assert_eq!(state, "ready:2x4:0,0,255,255:255,0,0,255");
    }

    #[test]
    fn create_image_bitmap_alpha_modes_change_translucent_resize_sampling() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "globalThis.__alphaModesReady='pending';(()=>{const d=new ImageData(2,1);d.data.set([255,0,0,0,0,0,255,255]);Promise.all([createImageBitmap(d,{resizeWidth:3,resizeQuality:'medium',premultiplyAlpha:'none'}),createImageBitmap(d,{resizeWidth:3,resizeQuality:'medium',premultiplyAlpha:'premultiply'})]).then(([straight,premul])=>{const a=new OffscreenCanvas(3,2),b=new OffscreenCanvas(3,2),ac=a.getContext('2d'),bc=b.getContext('2d');ac.drawImage(straight,0,0);bc.drawImage(premul,0,0);const p=ac.getImageData(1,0,1,1).data,q=bc.getImageData(1,0,1,1).data;globalThis.__alphaModesReady=`${Array.from(p)}:${Array.from(q)}`}).catch(e=>globalThis.__alphaModesReady=String(e&&e.stack||e))})();true"
        ));
        engine.ctx().drain_microtasks_for_host();
        let diagnostic = match engine.eval_value("globalThis.__alphaModesReady") {
            Ok(Ok(Value::Str(value))) => value.as_str().to_owned(),
            Ok(Ok(_)) => "unexpected JavaScript result type".to_owned(),
            Ok(Err(_)) => "JavaScript evaluation ended abruptly".to_owned(),
            Err(_) => "JavaScript evaluation could not start".to_owned(),
        };
        let (straight, premultiplied) = diagnostic
            .split_once(':')
            .map(|(straight, premultiplied)| (straight, premultiplied))
            .unwrap_or(("", ""));
        let channels = |value: &str| -> Vec<u8> {
            value
                .split(',')
                .filter_map(|channel| channel.parse().ok())
                .collect()
        };
        let straight = channels(straight);
        let premultiplied = channels(premultiplied);
        assert_ne!(diagnostic, "pending", "ImageBitmap promises did not settle");
        assert!(
            straight.len() == 4
                && premultiplied.len() == 4
                && straight[3] > 0
                && premultiplied[3] > 0
                && straight[0] > premultiplied[0],
            "straight and premultiplied filtering had unexpected readback: {diagnostic}"
        );
    }

    #[test]
    fn create_image_bitmap_accepts_color_space_modes_for_srgb_canvas_sources() {
        let mut engine = Engine::new();
        super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "globalThis.__colorModesReady=false;(()=>{const s=new OffscreenCanvas(1,1),x=s.getContext('2d');x.fillStyle='rgba(40,120,220,0.5)';x.fillRect(0,0,1,1);Promise.all([createImageBitmap(s,{colorSpaceConversion:'default'}),createImageBitmap(s,{colorSpaceConversion:'none'})]).then(([a,b])=>{const c=new OffscreenCanvas(1,1),d=new OffscreenCanvas(1,1),cx=c.getContext('2d'),dx=d.getContext('2d');cx.drawImage(a,0,0);dx.drawImage(b,0,0);const p=cx.getImageData(0,0,1,1).data,q=dx.getImageData(0,0,1,1).data;globalThis.__colorModesReady=Array.from(p).every((v,i)=>v===q[i])&&p[3]>0})})();true"
        ));
        engine.ctx().drain_microtasks_for_host();
        assert!(eval_bool(
            &mut engine,
            "globalThis.__colorModesReady===true"
        ));
    }
}
