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
    gradient::interpolate(from, to, progress)
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
            let mut resolved = rasterize_for_gpui_in_region(list, scale, font, cache, false,Rect{
                x:0.0,y:0.0,width:width_css as f32,height:height_css as f32})?;
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

/// Retains admitted vector source bytes and weak viewport rasters. The ordinary
/// image resolver owns natural dimensions; resized views neither refetch nor
/// alter that identity. Dead raster keys are reclaimed on admission.
#[derive(Default)]
pub struct SvgViewportImages {
    sources:RefCell<HashMap<Arc<str>,SvgViewportSource>>,
    natural_sources:RefCell<HashMap<usize,Arc<str>>>,
    rasters:RefCell<HashMap<(u64,u32,u32,lumen_html::css::UsedColorScheme),std::sync::Weak<ImageData>>>,
    next_id:Cell<u64>,
}
struct SvgViewportSource {id:u64,bytes:lumen_common::bytes::Bytes,fragment:Option<Arc<str>>,natural:std::sync::Weak<ImageData>,last:Option<(lumen_html::css::UsedColorScheme,Arc<ImageData>)>,metadata:[Option<SvgImageMetadata>;2]}
#[derive(Clone,Copy)]
struct SvgImageMetadata {intrinsic:lumen_html::object::IntrinsicSize,coordinates:Option<(Option<lumen_html::svg::ViewBox>,lumen_html::svg::AspectRatio)>}
fn scheme_index(scheme:lumen_html::css::UsedColorScheme)->usize {usize::from(scheme==lumen_html::css::UsedColorScheme::Dark)}
impl SvgViewportImages {
    pub fn is_vector(bytes:&[u8])->bool {
        bytes.strip_prefix(&[0xef,0xbb,0xbf]).unwrap_or(bytes).iter().copied().find(|byte|!byte.is_ascii_whitespace())==Some(b'<')
    }
    pub fn live_raster_bytes(&self)->Option<usize> {
        let rasters=self.rasters.borrow();
        let pixels=rasters.values().filter_map(std::sync::Weak::upgrade).try_fold(0usize,|size,image|size.checked_add(image.pixels.len()))?;
        pixels.checked_add(rasters.capacity().checked_mul(std::mem::size_of::<((u64,u32,u32,lumen_html::css::UsedColorScheme),std::sync::Weak<ImageData>)>()+32)?)
    }
    pub fn source_cost(key:&str,bytes:usize,fragment:Option<&str>)->Option<usize> {key.len().checked_add(bytes).and_then(|size|size.checked_add(fragment.map_or(0,str::len))).and_then(|size|size.checked_add(4*(std::mem::size_of::<(Arc<str>,SvgViewportSource)>()+std::mem::size_of::<(usize,Arc<str>)>()+64)+128))}
    pub fn remember(&self,key:&str,bytes:lumen_common::bytes::Bytes,fragment:Option<&str>,budget:usize)->Result<usize,ImageError> {
        if !Self::is_vector(&bytes) || self.sources.borrow().contains_key(key) {return Ok(0);}
        let charge=Self::source_cost(key,bytes.len(),fragment).ok_or(ImageError::TooLarge)?;
        let budget=budget.checked_sub(self.live_raster_bytes().ok_or(ImageError::TooLarge)?).ok_or(ImageError::TooLarge)?;
        if charge>budget || key.len()>8192 || fragment.is_some_and(|fragment|fragment.len()>8192) || self.sources.borrow().len()>=256 {return Err(ImageError::TooLarge);}
        let mut sources=self.sources.borrow_mut();
        sources.try_reserve(1).map_err(|_|ImageError::TooLarge)?;
        self.natural_sources.borrow_mut().try_reserve(1).map_err(|_|ImageError::TooLarge)?;
        let id=self.next_id.get().checked_add(1).ok_or(ImageError::TooLarge)?;
        self.next_id.set(id);
        sources.insert(key.into(),SvgViewportSource{id,bytes,fragment:fragment.map(Arc::from),natural:std::sync::Weak::new(),last:None,metadata:[None;2]});
        Ok(charge)
    }
    pub fn attach_natural(&self,key:&str,image:&Arc<ImageData>) {
        self.attach_natural_with_intrinsic(key,image,None);
    }
    pub fn attach_natural_with_intrinsic(&self,key:&str,image:&Arc<ImageData>,intrinsic:Option<lumen_html::object::IntrinsicSize>) {
        let mut sources=self.sources.borrow_mut();
        let key=sources.get_key_value(key).map(|(key,_)|key.clone());
        if let Some(key)=key {
            if let Some(source)=sources.get_mut(key.as_ref()) {
                let mut index=self.natural_sources.borrow_mut();
                index.remove(&(source.natural.as_ptr() as usize));
                source.natural=Arc::downgrade(image);
                source.metadata[0]=intrinsic.filter(|size|size.is_valid()).map(|intrinsic|SvgImageMetadata{intrinsic,coordinates:source.metadata[0].and_then(|metadata|metadata.coordinates)});
                index.insert(Arc::as_ptr(image) as usize,key);
            }
        }
    }
    /// Metadata belongs to the exact decoded source/fragment image identity.
    fn metadata(&self,image:&Arc<ImageData>,scheme:lumen_html::css::UsedColorScheme)->Option<SvgImageMetadata> {
        let key=self.natural_sources.borrow().get(&(Arc::as_ptr(image) as usize))?.clone();
        let (id,bytes,fragment)={let sources=self.sources.borrow();let source=sources.get(key.as_ref())?;
            source.natural.upgrade().filter(|natural|Arc::ptr_eq(natural,image))?;
            if let Some(metadata)=source.metadata[scheme_index(scheme)].filter(|metadata|metadata.coordinates.is_some()) {return Some(metadata);}
            (source.id,source.bytes.clone(),source.fragment.clone())};
        // Reuse the image document preparation/metric authority; no raster allocation,
        // fetch or second parser. Never hold a source/raster RefCell borrow across it.
        let (_,intrinsic,coordinates)=prepare_svg_image(&bytes,fragment.as_deref(),scheme).ok()?;
        let metadata=SvgImageMetadata{intrinsic,coordinates:Some(coordinates)};
        let mut sources=self.sources.borrow_mut();let source=sources.get_mut(key.as_ref())?;
        if source.id!=id || !source.natural.upgrade().is_some_and(|natural|Arc::ptr_eq(&natural,image)) {return None;}
        source.metadata[scheme_index(scheme)]=Some(metadata);
        Some(metadata)
    }
    pub fn intrinsic_size(&self,image:&Arc<ImageData>,scheme:lumen_html::css::UsedColorScheme)->Option<lumen_html::object::IntrinsicSize> {
        self.metadata(image,scheme).map(|metadata|metadata.intrinsic)
    }
    pub fn coordinate_scale(&self,image:&Arc<ImageData>,width:f32,height:f32,scheme:lumen_html::css::UsedColorScheme)->Option<[f32;2]> {
        let (view_box,aspect)=self.metadata(image,scheme)?.coordinates?;
        let (transform,_)=lumen_html::svg::view_box_transform_with_aspect(view_box,aspect,0.0,0.0,width,height);
        Some([transform.a,transform.d])
    }
    pub fn render_image(&self,image:&Arc<ImageData>,width:f32,height:f32,scheme:lumen_html::css::UsedColorScheme,budget:usize)->Option<Result<Arc<ImageData>,ImageError>> {
        let key=self.natural_sources.borrow().get(&(Arc::as_ptr(image) as usize))?.clone();
        let matches=self.sources.borrow().get(key.as_ref()).and_then(|source|source.natural.upgrade()).is_some_and(|natural|Arc::ptr_eq(&natural,image));
        if !matches{return None;}
        self.render(&key,width,height,scheme,budget)
    }
    pub fn render(&self,key:&str,width:f32,height:f32,scheme:lumen_html::css::UsedColorScheme,budget:usize)->Option<Result<Arc<ImageData>,ImageError>> {
        let (id,bytes,fragment)={let mut sources=self.sources.borrow_mut();let source=sources.get_mut(key)?;
            if let Some((_,image))=source.last.as_ref().filter(|(sampled,image)|*sampled==scheme && image.width as f32==width.ceil() && image.height as f32==height.ceil()) {return Some(Ok(image.clone()));}
            source.last=None;
            (source.id,source.bytes.clone(),source.fragment.clone())};
        Some((|| {
            if !width.is_finite() || !height.is_finite() || width<=0.0 || height<=0.0 || f64::from(width.ceil())>f64::from(u32::MAX) || f64::from(height.ceil())>f64::from(u32::MAX) {return Err(ImageError::InvalidViewport);}
            let (width,height)=(width.ceil() as u32,height.ceil() as u32);
            let mut rasters=self.rasters.borrow_mut();
            if let Some(image)=rasters.get(&(id,width,height,scheme)).and_then(std::sync::Weak::upgrade) {return Ok(image);}
            rasters.retain(|_,image|image.strong_count()!=0);
            let live=rasters.values().filter_map(std::sync::Weak::upgrade).try_fold(0usize,|size,image|size.checked_add(image.pixels.len())).ok_or(ImageError::TooLarge)?;
            let workspace=rasters.capacity().max(rasters.len().checked_add(1).and_then(|count|count.checked_mul(2)).and_then(|count|count.checked_add(3)).ok_or(ImageError::TooLarge)?).checked_mul(std::mem::size_of::<((u64,u32,u32,lumen_html::css::UsedColorScheme),std::sync::Weak<ImageData>)>()+32).ok_or(ImageError::TooLarge)?;
            let remaining=budget.checked_sub(live).and_then(|remaining|remaining.checked_sub(workspace)).ok_or(ImageError::TooLarge)?;
            if rasters.len()>=256 {return Err(ImageError::TooLarge);}
            rasters.try_reserve(1).map_err(|_|ImageError::TooLarge)?;
            let (decoded,intrinsic,coordinates)=decode_svg_image_in_viewport_with_intrinsic(&bytes,fragment.as_deref(),Some((width,height)),scheme,remaining)?;
            let image=Arc::new(ImageData{width:decoded.width,height:decoded.height,pixels:decoded.pixels});
            rasters.insert((id,width,height,scheme),Arc::downgrade(&image));
            let mut sources=self.sources.borrow_mut();let source=sources.get_mut(key).ok_or(ImageError::TooLarge)?;
            source.last=Some((scheme,image.clone()));source.metadata[scheme_index(scheme)]=Some(SvgImageMetadata{intrinsic,coordinates:Some(coordinates)});
            Ok(image)
        })())
    }
}

pub struct FileImages {
    svg_viewports:SvgViewportImages,
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
            svg_viewports:SvgViewportImages::default(),
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
            let source=source.split(['?','#']).next()?;
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
    fn image_coordinate_scale(&self,image:&Arc<ImageData>,width:f32,height:f32,scheme:lumen_html::css::UsedColorScheme)->Option<[f32;2]> {self.svg_viewports.coordinate_scale(image,width,height,scheme)}
    fn image_intrinsic_size(&self,image:&Arc<ImageData>,scheme:lumen_html::css::UsedColorScheme)->Option<lumen_html::object::IntrinsicSize> {
        self.svg_viewports.intrinsic_size(image,scheme)
    }
    fn resolve_viewport(&self,_node:Option<lumen_html::NodeId>,_base:&str,_source:&str,natural:&Arc<lumen_html::paint::ImageData>,width:f32,height:f32,scheme:lumen_html::css::UsedColorScheme)->Option<ImageState> {
        self.svg_viewports.render_image(natural,width,height,scheme,ASSET_CACHE_BYTES.saturating_sub(*self.cached_bytes.borrow())).map(|result|result.map_or(ImageState::Failed,ImageState::Ready))
    }
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
            data.split('#').next().and_then(|data|data.split_once(',')).and_then(|(metadata, payload)| {
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
                let fragment=lumen_common::url::parse(source,Some("file:///")).ok().and_then(|url|url.fragment);
                let raster_budget=remaining_bytes.checked_sub(self.svg_viewports.live_raster_bytes().ok_or(ImageFailure::Limit)?).ok_or(ImageFailure::Limit)?;
                let (decoded,intrinsic)=decode_image_with_fragment_and_intrinsic_limit(&bytes,fragment.as_deref(),raster_budget).map_err(|error| match error {
                    ImageError::TooLarge => ImageFailure::Limit,
                    ImageError::UnsupportedSvg(_) => ImageFailure::UnsupportedDrawing,
                    _ => ImageFailure::Decode,
                })?;
                if SvgViewportImages::is_vector(&bytes) {
                    let budget=remaining_bytes.checked_sub(decoded.pixels.len()).ok_or(ImageFailure::Limit)?;
                    let _charge=SvgViewportImages::source_cost(source,bytes.len(),fragment.as_deref()).filter(|charge|self.svg_viewports.live_raster_bytes().and_then(|live|charge.checked_add(live)).is_some_and(|total|total<=budget)).ok_or(ImageFailure::Limit)?;
                    let actual=self.svg_viewports.remember(source,lumen_common::bytes::Bytes::owned(Arc::from(bytes)),fragment.as_deref(),budget).map_err(|_|ImageFailure::Limit)?;
                    *self.cached_bytes.borrow_mut()+=actual;
                }
                Ok((decoded,intrinsic))
            })
            .map(|(image,intrinsic)| {
                (Arc::new(ImageData {
                    width: image.width,
                    height: image.height,
                    pixels: image.pixels,
                }),intrinsic)
            });
        if let Ok((image,intrinsic))=decoded.as_ref(){self.svg_viewports.attach_natural_with_intrinsic(source,image,*intrinsic);}
        let decoded=decoded.map(|(image,_)|image);
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

pub use lumen_common::raster::Rgba8Image;

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
    device_origin:[i64;2],
    clip: Option<Rect>,
    (x, y): (f32, f32),
    color: Rgba,
    coverage: &GlyphCoverage,
) {
    let Some(left)=((x*scale).round() as i64).checked_sub(device_origin[0])
        .and_then(|left|left.checked_add(i64::from(coverage.x_min))) else{return;};
    let Some(top)=((y*scale).round() as i64).checked_sub(device_origin[1])
        .and_then(|top|top.checked_sub(i64::from(coverage.y_min)))
        .and_then(|top|i64::try_from(coverage.height).ok().and_then(|height|top.checked_sub(height))) else{return;};
    let (Ok(width),Ok(height))=(i64::try_from(coverage.width),i64::try_from(coverage.height)) else{return;};
    let (Some(right),Some(bottom))=(left.checked_add(width),top.checked_add(height)) else{return;};
    if right<=0 || bottom<=0 || left>=i64::from(image.width) || top>=i64::from(image.height) {return;}
    let first=left.saturating_neg().max(0) as usize;
    let end=i64::from(image.width).saturating_sub(left).min(width) as usize;
    let mut columns = 0..0;
    for col in first..end {
        let x = left + col as i64;
        let css_x = ((x.saturating_add(device_origin[0])) as f32 + 0.5) / scale;
        let visible =
            x >= 0 && x < i64::from(image.width) && clip.is_none_or(|clip|css_x>=clip.x && css_x<clip.x+clip.width);
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
    for row in top.saturating_neg().max(0) as usize..i64::from(image.height).saturating_sub(top).min(height) as usize {
        let y = top + row as i64;
        if y < 0 || y >= i64::from(image.height) {
            continue;
        }
        let css_y = ((y.saturating_add(device_origin[1])) as f32 + 0.5) / scale;
        if clip.is_some_and(|clip|css_y<clip.y || css_y>=clip.y+clip.height) {
            continue;
        }
        let alphas = &coverage.alpha
            [row * coverage.width + columns.start..row * coverage.width + columns.end];
        let start =
            ((y as usize * image.width as usize) + (left + columns.start as i64) as usize) * 4;
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
    device_origin: [i64; 2],
    source_window: bool,
    source_reserved: Option<usize>,
    clips: Vec<Rect>,
    font: Option<&'a dyn FontProvider>,
    cache: &'a mut GlyphCache,
    error: Option<ImageError>,
}

impl Raster<'_> {
    fn current_clip(&self)->Option<Rect> {
        if self.source_window && self.clips.len()==1 {None}else{self.clips.last().copied()}
    }
    fn visible_rect(&self,rect:Rect)->Option<Rect> {
        match self.current_clip(){Some(clip)=>clip.intersection(rect),None=>Some(rect)}
    }
    fn device_x(&self, value:f32)->f32 { if self.device_origin[0]==0 {value*self.scale}else{((value*self.scale) as f64-self.device_origin[0] as f64) as f32} }
    fn device_y(&self, value:f32)->f32 { if self.device_origin[1]==0 {value*self.scale}else{((value*self.scale) as f64-self.device_origin[1] as f64) as f32} }
    fn css_x(&self, pixel:f32)->f32 { if self.device_origin[0]==0 {pixel/self.scale}else{(f64::from(pixel)+self.device_origin[0] as f64) as f32/self.scale} }
    fn css_y(&self, pixel:f32)->f32 { if self.device_origin[1]==0 {pixel/self.scale}else{(f64::from(pixel)+self.device_origin[1] as f64) as f32/self.scale} }
    fn axis_coverage(&self,start:f32,end:f32,pixel:u32,axis:usize)->f32 {
        let pixel=i64::from(pixel).saturating_add(self.device_origin[axis]) as f32;
        (end.min((pixel+1.0)/self.scale)-start.max(pixel/self.scale)).clamp(0.0,1.0/self.scale)*self.scale
    }
    /// Pixel rectangle covering `visible` (CSS units), clamped to the image.
    fn pixel_span(&self, visible: Rect) -> (u32, u32, u32, u32) {
        (
            self.device_x(visible.x).floor().max(0.0) as u32,
            self.device_y(visible.y).floor().max(0.0) as u32,
            self.device_x(visible.x + visible.width)
                .ceil()
                .min(self.image.width as f32) as u32,
            self.device_y(visible.y + visible.height)
                .ceil()
                .min(self.image.height as f32) as u32,
        )
    }
}

fn apply_svg_clips(surface:&mut canvas::CanvasSurface,clips:&[lumen_html::paint::SvgClip],
    transform:Affine,object_box:Affine,screen:Affine,reserved:Option<usize>)->Result<(),ImageError> {
    for clip in clips {
        let unit_transform=match clip.units {
            lumen_html::paint::SvgGradientUnits::ObjectBoundingBox=>object_box,
            lumen_html::paint::SvgGradientUnits::UserSpaceOnUse=>Affine::IDENTITY,
        };
        let count=clip.shapes.len();
        let parsed_bytes=count.checked_mul(core::mem::size_of::<(tiny_skia::Path,tiny_skia::FillRule,tiny_skia::Transform)>()).ok_or(ImageError::TooLarge)?;
        let view_bytes=count.checked_mul(core::mem::size_of::<(&tiny_skia::Path,tiny_skia::FillRule,tiny_skia::Transform)>()).ok_or(ImageError::TooLarge)?;
        if reserved.is_some_and(|occupied|occupied.checked_add(parsed_bytes).and_then(|bytes|bytes.checked_add(view_bytes)).is_none_or(|bytes|bytes>MAX_LAYER_BYTES)) {return Err(ImageError::TooLarge);}
        let mut parsed_shapes=Vec::new();parsed_shapes.try_reserve_exact(count).map_err(|_|ImageError::TooLarge)?;
        let mut clip_paths=Vec::new();clip_paths.try_reserve_exact(count).map_err(|_|ImageError::TooLarge)?;
        let metadata=parsed_shapes.capacity().checked_mul(core::mem::size_of::<(tiny_skia::Path,tiny_skia::FillRule,tiny_skia::Transform)>())
            .and_then(|bytes|clip_paths.capacity().checked_mul(core::mem::size_of::<(&tiny_skia::Path,tiny_skia::FillRule,tiny_skia::Transform)>()).and_then(|more|bytes.checked_add(more))).ok_or(ImageError::TooLarge)?;
        let mut occupied=reserved.map(|bytes|bytes.checked_add(metadata).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)).transpose()?;
        for shape in clip.shapes.iter() {
            let parsed=if let Some(bytes)=occupied {
                match lumen_common::svg_path::parse_svg_path_bounded(&shape.data,MAX_LAYER_BYTES.checked_sub(bytes).ok_or(ImageError::TooLarge)?) {
                    Ok(parsed)=>parsed,
                    Err(lumen_common::svg_path::SvgPathError::Invalid(_))=>continue,
                    Err(lumen_common::svg_path::SvgPathError::BudgetExceeded)=>return Err(ImageError::TooLarge),
                }
            }else{let Ok(parsed)=canvas::parse_svg_path(&shape.data)else{continue;};parsed};
            let Some(path)=parsed.path else{continue;};
            occupied=occupied.map(|bytes|bytes.checked_add(path.allocated_bytes()).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)).transpose()?;
            let matrix=screen.then(transform).then(unit_transform).then(clip.transform).then(shape.transform);
            parsed_shapes.push((path,match shape.fill_rule {PaintSvgFillRule::NonZero=>tiny_skia::FillRule::Winding,PaintSvgFillRule::EvenOdd=>tiny_skia::FillRule::EvenOdd},
                tiny_skia::Transform::from_row(matrix.a,matrix.b,matrix.c,matrix.d,matrix.e,matrix.f)));
        }
        clip_paths.extend(parsed_shapes.iter().map(|(path,rule,matrix)|(path,*rule,*matrix)));
        if let Some(occupied)=occupied {
            surface.clip_paths_union_bounded(&clip_paths,MAX_LAYER_BYTES.checked_sub(occupied).ok_or(ImageError::TooLarge)?)?;
        }else{surface.clip_paths_union(&clip_paths)?;}
    }
    Ok(())
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
        let clip = self.current_clip().map_or(Some(rect),|outer|outer.intersection(rect)).unwrap_or(Rect {
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
        let Some(rect) = self.visible_rect(rect)
        else {
            return;
        };
        let bounds = Rect {
            x: self.device_x(rect.x),
            y: self.device_y(rect.y),
            width: self.device_x(rect.x + rect.width)-self.device_x(rect.x),
            height: self.device_y(rect.y + rect.height)-self.device_y(rect.y),
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
            let Some(visible) = self.visible_rect(rect) else {
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
                            self.device_x(rect.x),
                            self.device_y(rect.y),
                            self.device_x(rect.x + rect.width),
                            self.device_y(rect.y + rect.height),
                            scaled,
                        )
                    } else {
                        rect.contains_corners(
                            self.css_x(x as f32 + 0.5),
                            self.css_y(y as f32 + 0.5),
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
        let Some(visible) = self.visible_rect(rect)
        else {
            return;
        };
        let (x0, y0, x1, y1) = self.pixel_span(visible);
        let left = self.device_x(rect.x);
        let top = self.device_y(rect.y);
        let right = self.device_x(rect.x + rect.width);
        let bottom = self.device_y(rect.y + rect.height);
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
        let clip=self.current_clip();
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
                blit_glyph(self.image, self.scale, self.device_origin, clip, origin, color, coverage);
                continue;
            }
            let bitmap = match font.rasterize_glyph(glyph.face, glyph.id, scaled_size) {
                Ok(bitmap) => bitmap,
                Err(error) => {
                    self.error = Some(ImageError::Font(error));
                    return;
                }
            };
            blit_glyph(self.image, self.scale, self.device_origin, clip, origin, color, &bitmap);
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
        let Some(visible) = self.visible_rect(rect)
        else {
            return;
        };
        let (x0, y0, x1, y1) = self.pixel_span(visible);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let columns: Vec<(usize, f32)> = (x0..x1)
            .map(|x| {
                let center_x = self.css_x(x as f32 + 0.5);
                let sx = ((((center_x - rect.x) / rect.width * image.width as f32).ceil() - 1.0)
                    .max(0.0) as u32)
                    .min(image.width - 1);
                let left = if self.antialias {
                    self.axis_coverage(visible.x, visible.x + visible.width, x, 0)
                } else {
                    1.0
                };
                (sx as usize * 4, left)
            })
            .collect();
        for y in y0..y1 {
            let center_y = self.css_y(y as f32 + 0.5);
            let sy = ((((center_y - rect.y) / rect.height * image.height as f32).ceil() - 1.0)
                .max(0.0) as u32)
                .min(image.height - 1);
            let top = if self.antialias {
                self.axis_coverage(visible.y, visible.y + visible.height, y, 1)
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
        let parsed=if let Some(reserved)=self.source_reserved {
            let Some(remaining)=MAX_LAYER_BYTES.checked_sub(reserved) else{self.error=Some(ImageError::TooLarge);return;};
            match lumen_common::svg_path::parse_svg_path_bounded(data,remaining) {
                Ok(parsed)=>parsed,
                Err(lumen_common::svg_path::SvgPathError::Invalid(_))=>return,
                Err(lumen_common::svg_path::SvgPathError::BudgetExceeded)=>{self.error=Some(ImageError::TooLarge);return;},
            }
        }else{let Ok(parsed)=canvas::parse_svg_path(data)else{return;};parsed};
        let Some(path) = parsed.path else {
            return;
        };
        let path_bytes=path.allocated_bytes();
        let Some(path_bounds) = transformed_svg_path_bounds(
            &path,
            transform,
            lumen_html::paint::svg_paint_has_ink(stroke),
            stroke_width,
            self.scale,
        ) else {
            return;
        };
        let replay_clip=self.current_clip();
        let Some(visible) = self.visible_rect(bounds).and_then(|visible|visible.intersection(path_bounds))
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
        let source_peak=match self.source_reserved.map(|reserved|len.checked_mul(2)
            .and_then(|bytes|bytes.checked_add(path_bytes))
            .and_then(|bytes|bytes.checked_add(reserved)).filter(|bytes|*bytes<=MAX_LAYER_BYTES)
            .ok_or(ImageError::TooLarge)).transpose() {
            Ok(peak)=>peak,Err(error)=>{self.error=Some(error);return;}
        };
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
            self.device_x(transform.e) - x0 as f32,
            self.device_y(transform.f) - y0 as f32,
        );
        surface.state_mut().transform = path_transform;
        // SVG initial butt/miter geometry belongs to the canonical SVG
        // stroker; Canvas keeps its separate initial miter limit of ten.
        {
            let state=surface.state_mut();
            state.line=tiny_skia::Stroke{width:stroke_width,..tiny_skia::Stroke::default()};
            state.miter_limit=f64::from(state.line.miter_limit);
        }

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
            e: -((i64::from(x0)+self.device_origin[0]) as f32),
            f: -((i64::from(y0)+self.device_origin[1]) as f32),
        };
        if let Err(error)=apply_svg_clips(&mut surface,clips,transform,object_box,screen,source_peak) {
            self.error=Some(error);return;
        }

        let paint_reserved=match source_peak.map(|peak|peak.checked_add(surface.clip_alpha().map_or(0,|alpha|alpha.len()))
            .filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)).transpose() {
            Ok(peak)=>peak,Err(error)=>{self.error=Some(error);return;}
        };
        let mut gradient_bytes=0usize;
        let mut make_gradient = |paint: Option<&lumen_html::paint::SvgPaint>|->Result<Option<canvas::CanvasGradient>,ImageError> {
            let Some(lumen_html::paint::SvgPaint::Gradient(gradient)) = paint else {
                return Ok(None);
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
            let resolved=if let Some(occupied)=paint_reserved {
                let remaining=MAX_LAYER_BYTES.checked_sub(occupied).and_then(|bytes|bytes.checked_sub(gradient_bytes)).ok_or(ImageError::TooLarge)?;
                canvas::CanvasGradient::new_reserved(kind,gradient.stops.len(),remaining)?
            }else{canvas::CanvasGradient::new(kind)};
            for stop in gradient.stops.iter() {
                if resolved
                    .add_color_stop(
                        stop.offset,
                        [stop.color.r, stop.color.g, stop.color.b, stop.color.a],
                    )
                    .is_err()
                {
                    return Ok(None);
                }
            }
            if paint_reserved.is_some(){gradient_bytes=gradient_bytes.checked_add(resolved.allocated_bytes()).ok_or(ImageError::TooLarge)?;}
            Ok(Some(resolved))
        };
        if let Some(lumen_html::paint::SvgPaint::Color(color)) = fill {
            surface.state_mut().fill = [color.r, color.g, color.b, color.a];
        }
        if let Some(lumen_html::paint::SvgPaint::Color(color)) = stroke {
            surface.state_mut().stroke = [color.r, color.g, color.b, color.a];
        }
        let gradients=(||Ok::<_,ImageError>((make_gradient(fill)?,make_gradient(stroke)?)))();
        let (fill_gradient,stroke_gradient)=match gradients {Ok(value)=>value,Err(error)=>{self.error=Some(error);return;}};
        surface.state_mut().fill_gradient=fill_gradient;surface.state_mut().stroke_gradient=stroke_gradient;
        let paint_budget=match paint_reserved.map(|occupied|MAX_LAYER_BYTES.checked_sub(occupied).and_then(|bytes|bytes.checked_sub(gradient_bytes)).ok_or(ImageError::TooLarge)).transpose() {
            Ok(budget)=>budget,Err(error)=>{self.error=Some(error);return;}
        };
        if lumen_html::paint::svg_paint_has_ink(fill) {
            surface.state_mut().alpha = match fill {
                Some(lumen_html::paint::SvgPaint::Gradient(gradient)) => gradient.opacity,
                _ => 1.0,
            };
            let rule=match fill_rule {PaintSvgFillRule::NonZero=>tiny_skia::FillRule::Winding,PaintSvgFillRule::EvenOdd=>tiny_skia::FillRule::EvenOdd};
            let result=if let Some(budget)=paint_budget {surface.source_path_bounded(&path,rule,false,budget)}else{surface.fill_path(&path,rule)};
            if let Err(error)=result {
                self.error = Some(error);
                return;
            }
        }
        if lumen_html::paint::svg_paint_has_ink(stroke) && stroke_width > 0.0 {
            surface.state_mut().alpha = match stroke {
                Some(lumen_html::paint::SvgPaint::Gradient(gradient)) => gradient.opacity,
                _ => 1.0,
            };
            let result=if let Some(budget)=paint_budget {surface.source_path_bounded(&path,tiny_skia::FillRule::Winding,true,budget)}else{surface.stroke_path(&path)};
            if let Err(error)=result {
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
                let coverage = if self.antialias {
                    replay_clip.map_or(1.0,|clip|self.axis_coverage(clip.x,clip.x+clip.width,x0+column as u32,0)
                        * self.axis_coverage(clip.y,clip.y+clip.height,y0+row as u32,1))
                } else { 1.0 };
                composite(
                    &mut self.image.pixels[target..target + 4],
                    Rgba {
                        r: output.pixels[source],
                        g: output.pixels[source + 1],
                        b: output.pixels[source + 2],
                        a: output.pixels[source + 3],
                    },
                    coverage,
                );
            }
        }
    }
}

// A rectangular replay clip has the same device-pixel coverage for vector
// commands and the sprites produced by group compositing. SvgPath::bounds
// limits the crop and is not another antialiased mask.

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
    let source = [src.r, src.g, src.b].map(|channel| channel as f32 * sa * rgb_scale);
    composite_premultiplied(dst, source, sa, quantize_sources);
}

// Border samples partition one surface; adjoining colors do not overlap.
// Average their premultiplied samples before one source-over operation.
fn composite_disjoint_samples(dst: &mut [u8], colors: &[Rgba; 8], hits: &[u32; 8], total: u32, coverage: f32) {
    if total == 0 { return; }
    let mut weighted = [0u64; 4];
    for (color, count) in colors.iter().zip(hits) {
        let alpha = u64::from(color.a) * u64::from(*count);
        weighted[3] += alpha;
        for (channel, value) in [color.r, color.g, color.b].into_iter().enumerate() {
            weighted[channel] += u64::from(value) * alpha;
        }
    }
    let scale = coverage / (total as f32 * 255.0);
    composite_premultiplied(dst, core::array::from_fn(|channel| weighted[channel] as f32 * scale),
        weighted[3] as f32 * scale, true);
}

fn composite_premultiplied(dst: &mut [u8], source: [f32; 3], sa: f32, quantize_sources: bool) {
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
    for (channel, source) in source.into_iter().enumerate() {
        // RGBA8 surfaces quantize premultiplied color before source-over.
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
    rasterize_gpui_region(list,scale,font,cache,rasterize_text,None)
}

/// Capture source dependencies outside the visible viewport, while retaining
/// only bounded visible output tiles for the native quad consumer.
pub fn rasterize_for_gpui_in_region(list:&DisplayList,scale:f32,font:&dyn FontProvider,
    cache:&mut GlyphCache,rasterize_text:bool,region:Rect)->Result<DisplayList,ImageError> {
    if ![region.x,region.y,region.width,region.height].iter().all(|v|v.is_finite())
        || region.width<=0.0 || region.height<=0.0 {return Err(ImageError::InvalidViewport);}
    rasterize_gpui_region(list,scale,font,cache,rasterize_text,Some(region))
}

fn rasterize_gpui_region(list:&DisplayList,scale:f32,font:&dyn FontProvider,
    cache:&mut GlyphCache,rasterize_text:bool,region:Option<Rect>)->Result<DisplayList,ImageError> {
    let _html_allocations = lumen_common::memcat::enter(lumen_common::memcat::CategoryTag::HTML);
    list.validate().map_err(ImageError::DisplayList)?;
    if !scale.is_finite() || scale <= 0.0 {
        return Err(ImageError::InvalidViewport);
    }
    let mut bytes = 0;
    let mut resolved = resolve_layers_region(&list.0, scale, Some(font), cache, &mut bytes,false,region)?;
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
        let rect=if let Some(region)=region {
            let Some(rect)=rect.intersection(region)else {
                *command=Command::FillRect{rect:Rect{x:0.0,y:0.0,width:0.0,height:0.0},color:Rgba{r:0,g:0,b:0,a:0}};
                continue;
            };rect
        }else{rect};
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

/// Resolve a text/decorative ink mask using the shared glyph cache. Only the
/// bounded coverage surface, alpha plane and blur scratch coexist; no second
/// source/paint RGBA surface is allocated for a solid shadow color.
fn resolve_text_ink_shadow(mask:&lumen_html::paint::MaskedBackground,scale:f32,
    font:Option<&dyn FontProvider>,cache:&mut GlyphCache,bytes:&mut usize)
    ->Result<Option<Command>,ImageError> {
    let blur=mask.shadow_blur.ok_or(ImageError::InvalidViewport)?;
    let Command::FillRect{color,..}=&*mask.paint else{return Err(ImageError::InvalidViewport);};
    let color=*color;
    if color.a==0||mask.mask.0.is_empty(){return Ok(None);}
    let left=(mask.rect.x*scale).floor();let top=(mask.rect.y*scale).floor();
    let width=((mask.rect.x+mask.rect.width)*scale).ceil()-left;
    let height=((mask.rect.y+mask.rect.height)*scale).ceil()-top;
    if width<=0.0||height<=0.0{return Ok(None);}
    if !width.is_finite()||!height.is_finite()||width>u32::MAX as f32||height>u32::MAX as f32{return Err(ImageError::TooLarge);}
    let(width,height)=(width as u32,height as u32);
    let pixels=(width as usize).checked_mul(height as usize).ok_or(ImageError::TooLarge)?;
    let output=pixels.checked_mul(4).ok_or(ImageError::TooLarge)?;
    let output_bytes=bytes.checked_add(output).filter(|n|*n<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
    // Coverage RGBA and extracted alpha coexist briefly. Output is charged
    // conservatively before sampling, while all earlier resolved images live.
    pixels.checked_mul(5).and_then(|n|n.checked_add(output_bytes)).filter(|n|*n<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
    shadow::validate_blur_budget(width as usize,height as usize,blur,scale,output_bytes)?;
    let(x,y)=(left/scale,top/scale);
    let mut ink=mask.mask.0.clone();
    for command in &mut ink {translate_command(command,mask.offset[0]-x,mask.offset[1]-y);}
    let coverage=render_scaled_region(&DisplayList(ink),width,height,scale,font,cache)?;
    let mut alpha=Vec::new();alpha.try_reserve_exact(pixels).map_err(|_|ImageError::TooLarge)?;
    alpha.extend(coverage.pixels.chunks_exact(4).map(|pixel|pixel[3]));
    drop(coverage);
    shadow::blur_alpha_mask(&mut alpha,width as usize,height as usize,blur,scale,output_bytes)?;
    let mut output=Vec::new();output.try_reserve_exact(pixels.checked_mul(4).ok_or(ImageError::TooLarge)?).map_err(|_|ImageError::TooLarge)?;
    for alpha in alpha {output.extend_from_slice(&[color.r,color.g,color.b,((u16::from(color.a)*u16::from(alpha)+127)/255)as u8]);}
    *bytes=output_bytes;
    Ok(Some(Command::Image{rect:Rect{x,y,width:width as f32/scale,height:height as f32/scale},
        image:Arc::new(ImageData{width,height,pixels:output})}))
}

fn filter_blur_padding(sigma: f32, scale: f32) -> Result<usize, ImageError> {
    if !sigma.is_finite() || sigma < 0.0 || !scale.is_finite() || scale <= 0.0 {
        return Err(ImageError::InvalidViewport);
    }
    let operating=sigma*scale;
    if !operating.is_finite() { return Err(ImageError::TooLarge); }
    if operating==0.0 || !operating.is_normal() { return Ok(0); }
    if operating>=2.0 { return shadow::blur_padding(operating*2.0); }
    // The maintained Gaussian's small-sigma kernel is contained within this
    // conservative support. Transparent padding makes its clamp edge zero.
    Ok((operating*4.0).ceil() as usize)
}

fn filter_capture_bounds(mut bounds: Rect, filters: &[lumen_common::filter::FilterOperation], scale:f32)
    -> Result<Rect,ImageError> {
    use lumen_common::filter::FilterOperation as F;
    for filter in filters {
        let (padding,offset)=match filter {
            F::Color(_) => continue,
            F::Url(_) => return Err(ImageError::Svg("unbound URL filter reached paint")),
            F::Resource(resource) => {
                let Some(region)=resource.filter_region() else{return Err(ImageError::TooLarge);};
                let matrix=resource.owner_transform;
                bounds=Affine{a:matrix[0],b:matrix[1],c:matrix[2],d:matrix[3],e:matrix[4],f:matrix[5]}
                    .bounds(Rect{x:region[0],y:region[1],width:region[2],height:region[3]});
                if ![bounds.x,bounds.y,bounds.width,bounds.height].iter().all(|value|value.is_finite()){return Err(ImageError::TooLarge);}
                continue;
            },
            F::Blur(sigma) => (filter_blur_padding(*sigma,scale)? as f32/scale,[0.0;2]),
            F::DropShadow(shadow) if shadow.color.alpha==0.0 => continue,
            F::DropShadow(shadow) => (filter_blur_padding(shadow.sigma,scale)? as f32/scale,shadow.offset),
        };
        let left=(padding-offset[0]).max(0.0);let top=(padding-offset[1]).max(0.0);
        let right=(padding+offset[0]).max(0.0);let bottom=(padding+offset[1]).max(0.0);
        bounds=Rect{x:bounds.x-left,y:bounds.y-top,width:bounds.width+left+right,height:bounds.height+top+bottom};
        if ![bounds.x,bounds.y,bounds.width,bounds.height].iter().all(|v|v.is_finite()) {return Err(ImageError::TooLarge);}
    }
    Ok(bounds)
}

fn filter_output_bounds(mut bounds:Option<Rect>,filters:&[lumen_common::filter::FilterOperation],scale:f32)->Result<Option<Rect>,ImageError> {
    use lumen_common::filter::FilterOperation as F;
    for filter in filters{
        match filter{
            F::Resource(resource)=>{
                if resource.program.nodes().is_empty(){bounds=None;continue;}
                // A resource generator does not require source alpha. Its
                // declared filter region bounds the output independently of
                // the descendant ink envelope and primitive result sharing.
                let seed=bounds.unwrap_or(Rect{x:0.0,y:0.0,width:0.0,height:0.0});
                let region=filter_capture_bounds(seed,core::slice::from_ref(filter),scale)?;
                bounds=(region.width>0.0 && region.height>0.0).then_some(region);
            },
            F::Url(_)=>return Err(ImageError::Svg("unbound URL filter reached paint")),
            _=>if let Some(source)=bounds{bounds=Some(filter_capture_bounds(source,core::slice::from_ref(filter),scale)?);},
        }
    }Ok(bounds)
}

fn blur_filter_plane(mask:Vec<u8>,width:u32,height:u32,sigma:f32,scale:f32,reserved:usize)
    -> Result<Vec<u8>,ImageError> {
    filter_blur_padding(sigma,scale)?;
    let operating=sigma*scale;
    if operating==0.0 || !operating.is_normal() {return Ok(mask);}
    if operating>=2.0 {
        let mut mask=mask;shadow::blur_alpha_mask(&mut mask,width as usize,height as usize,sigma*2.0,scale,reserved)?;
        return Ok(mask);
    }
    let pixels=(width as usize).checked_mul(height as usize).ok_or(ImageError::TooLarge)?;
    if pixels!=mask.len() {return Err(ImageError::InvalidViewport);}
    // Gray Gaussian: input/output planes, two f32 planes, optional convolution
    // transient, bounded small-kernel ring/row buffers and kernel copies.
    pixels.checked_mul(14).and_then(|n|n.checked_add((width as usize).checked_mul(128)?))
        .and_then(|n|n.checked_add(4096)).and_then(|n|n.checked_add(reserved))
        .filter(|n|*n<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
    let source=image::GrayImage::from_raw(width,height,mask).ok_or(ImageError::InvalidViewport)?;
    Ok(image::imageops::blur(&source,operating).into_raw())
}

fn resource_blur_parameters(sigma:[f32;2],scale:f32)->Result<image::imageops::GaussianBlurParameters,ImageError> {
    let operating=sigma.map(|sigma|sigma*scale);
    if !scale.is_finite() || scale<=0.0 || operating.iter().any(|sigma|!sigma.is_finite() || *sigma<0.0){return Err(ImageError::TooLarge);}
    Ok(image::imageops::GaussianBlurParameters::new_anisotropic_sigma(operating[0],operating[1]))
}
fn resource_blur_padding(sigma:[f32;2],scale:f32)->Result<[usize;2],ImageError>{
    let sizes=resource_blur_parameters(sigma,scale)?.kernel_sizes();
    Ok(sizes.map(|size|(size/2) as usize))
}
fn empty_filter_image(region:FilterPixelRegion,reserved:usize)->Result<Rgba8Image,ImageError> {
    let len=filter_region_budget(region,reserved)?;let mut pixels=Vec::new();pixels.try_reserve_exact(len).map_err(|_|ImageError::TooLarge)?;
    reserved.checked_add(pixels.capacity()).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;pixels.resize(len,0);
    Ok(Rgba8Image{width:region.width,height:region.height,pixels})
}
fn clip_filter_image(image:&mut Rgba8Image,region:FilterPixelRegion,domain:Option<FilterPixelRegion>)->Result<(),ImageError> {
    if image.width!=region.width || image.height!=region.height{return Err(ImageError::InvalidViewport);}
    let Some(domain)=domain else{image.pixels.fill(0);return Ok(());};
    let right=domain.left.checked_add(i64::from(domain.width)).ok_or(ImageError::TooLarge)?;let bottom=domain.top.checked_add(i64::from(domain.height)).ok_or(ImageError::TooLarge)?;
    for y in 0..region.height{let py=region.top.checked_add(i64::from(y)).ok_or(ImageError::TooLarge)?;
        for x in 0..region.width{let px=region.left.checked_add(i64::from(x)).ok_or(ImageError::TooLarge)?;
            if px<domain.left || px>=right || py<domain.top || py>=bottom{let at=(y as usize*region.width as usize+x as usize)*4;image.pixels[at..at+4].fill(0);}
        }
    }Ok(())
}
/// SVG primitive contours use the same maintained path coverage as native
/// SVG geometry. The integral ROI remains storage, never a replacement contour.
fn apply_filter_domain_coverage(image:&mut Rgba8Image,region:FilterPixelRegion,domain:Rect,scale:f32,reserved:usize)->Result<(),ImageError> {
    if image.pixels.chunks_exact(4).all(|pixel|pixel[3]==0){return Ok(());}
    let occupied=reserved.checked_add(image.pixels.len()).ok_or(ImageError::TooLarge)?;
    let available=MAX_LAYER_BYTES.checked_sub(occupied).ok_or(ImageError::TooLarge)?;
    // Integral device windows preserve the global SVG scan phase. One reusable
    // alpha tile replaces two full RGBA images and never enters backend tiling.
    let mut width=region.width.min(256);let mut height=region.height.min(256);
    loop {
        let mask_bytes=(width as usize).checked_mul(height as usize).ok_or(ImageError::TooLarge)?;
        let scan=tiny_skia::Mask::rectangle_path_scan_bytes(width).ok_or(ImageError::TooLarge)?;
        if mask_bytes.checked_add(scan).is_some_and(|bytes|bytes<available/2){break;}
        if height>1{height=(height+1)/2;}else if width>1{width=(width+1)/2;}else{return Err(ImageError::TooLarge);}
    }
    let len=(width as usize).checked_mul(height as usize).ok_or(ImageError::TooLarge)?;
    let mut data=Vec::new();data.try_reserve_exact(len).map_err(|_|ImageError::TooLarge)?;
    let mask_capacity=data.capacity();
    let scratch=available.checked_sub(mask_capacity).ok_or(ImageError::TooLarge)?;
    data.resize(len,0);
    let size=tiny_skia::IntSize::from_wh(width,height).ok_or(ImageError::InvalidViewport)?;
    let mut mask=tiny_skia::Mask::from_vec(data,size).ok_or(ImageError::InvalidViewport)?;
    let mut top=0u32;
    while top<region.height {
        let mut left=0u32;
        while left<region.width {
            mask.clear();
            let x=region.left.checked_add(i64::from(left)).ok_or(ImageError::TooLarge)? as f32;
            let y=region.top.checked_add(i64::from(top)).ok_or(ImageError::TooLarge)? as f32;
            let l=(domain.x*scale-x).max(0.0);let t=(domain.y*scale-y).max(0.0);
            let r=((domain.x+domain.width)*scale-x).min(width as f32);
            let b=((domain.y+domain.height)*scale-y).min(height as f32);
            if let Some(rect)=tiny_skia::Rect::from_ltrb(l,t,r,b){
                mask.fill_rectangle_path_bounded(rect,scratch).map_err(|_|ImageError::TooLarge)?;
            }
            let rows=height.min(region.height-top);let columns=width.min(region.width-left);
            for row in 0..rows {for column in 0..columns {
                let at=(((top+row) as usize*region.width as usize)+(left+column) as usize)*4;
                let coverage=mask.data()[row as usize*width as usize+column as usize];
                let pixel=&mut image.pixels[at..at+4];
                pixel[3]=((u16::from(pixel[3])*u16::from(coverage)+127)/255) as u8;
                if pixel[3]==0{pixel[..3].fill(0);}
            }}
            left=left.checked_add(width).ok_or(ImageError::TooLarge)?;
        }
        top=top.checked_add(height).ok_or(ImageError::TooLarge)?;
    }Ok(())
}
fn sample_filter_image(image:&Rgba8Image,region:FilterPixelRegion,x:f64,y:f64)->lumen_common::color::Color {
    use lumen_common::color::{Color,ColorSpace};
    let mut channels=[0.0f64;4];
    transform::visit_bilinear_samples(image.width,image.height,x-region.left as f64,y-region.top as f64,|at,weight|{
        let pixel=&image.pixels[at*4..at*4+4];let alpha=f64::from(pixel[3])/255.0;
        for channel in 0..3{channels[channel]+=f64::from(pixel[channel])/255.0*alpha*weight;}channels[3]+=alpha*weight;
    });
    if channels[3]>0.0{for channel in 0..3{channels[channel]/=channels[3];}}
    Color::new(ColorSpace::Srgb,[channels[0] as f32,channels[1] as f32,channels[2] as f32],channels[3] as f32,0)
}
fn resource_edge_coordinate(value:i64,length:u32,edge:lumen_common::filter::resource::EdgeMode)->Option<usize> {
    use lumen_common::filter::resource::EdgeMode;
    let length=i64::from(length);if length==0{return None;}
    let mapped=match edge{
        EdgeMode::None=>if value<0 || value>=length{return None;}else{value},
        EdgeMode::Duplicate=>value.clamp(0,length-1),EdgeMode::Wrap=>value.rem_euclid(length),
        EdgeMode::Mirror=>{let period=length*2;let value=value.rem_euclid(period);if value>=length{period-1-value}else{value}},
    };usize::try_from(mapped).ok()
}
fn resource_premultiply(image:&mut Rgba8Image,space:lumen_common::color::ColorSpace,opaque:bool) {
    use lumen_common::color::Color;
    for pixel in image.pixels.chunks_exact_mut(4){
        let color=Color::rgba8([pixel[0],pixel[1],pixel[2],pixel[3]]).to(space);
        let alpha=if opaque{1.0}else{color.alpha};
        for channel in 0..3{pixel[channel]=(color.components[channel]*alpha*255.0).round().clamp(0.0,255.0) as u8;}
        if opaque{pixel[3]=255;}
    }
}
fn resource_demultiply(image:&mut Rgba8Image,space:lumen_common::color::ColorSpace) {
    use lumen_common::color::Color;
    for pixel in image.pixels.chunks_exact_mut(4){let alpha=f32::from(pixel[3])/255.0;
        let components=if alpha>0.0{core::array::from_fn(|channel|(f32::from(pixel[channel])/255.0/alpha).clamp(0.0,1.0))}else{[0.0;3]};
        pixel.copy_from_slice(&Color::new(space,components,alpha,0).to_rgba8());}
}
fn resource_blend_mode(mode:lumen_common::filter::resource::BlendMode)->tiny_skia::BlendMode {
    use lumen_common::filter::resource::BlendMode as M;
    use tiny_skia::BlendMode as B;
    match mode {M::Normal=>B::SourceOver,M::Multiply=>B::Multiply,M::Screen=>B::Screen,M::Overlay=>B::Overlay,
        M::Darken=>B::Darken,M::Lighten=>B::Lighten,M::ColorDodge=>B::ColorDodge,M::ColorBurn=>B::ColorBurn,
        M::HardLight=>B::HardLight,M::SoftLight=>B::SoftLight,M::Difference=>B::Difference,M::Exclusion=>B::Exclusion,
        M::Hue=>B::Hue,M::Saturation=>B::Saturation,M::Color=>B::Color,M::Luminosity=>B::Luminosity}
}
fn blend_resource_images(mut source:Rgba8Image,mut backdrop:Rgba8Image,mode:lumen_common::filter::resource::BlendMode,
    composite:bool,space:lumen_common::color::ColorSpace,reserved:usize)->Result<Rgba8Image,ImageError> {
    if source.width!=backdrop.width || source.height!=backdrop.height{return Err(ImageError::InvalidViewport);}
    let occupied=reserved.checked_add(source.pixels.len()).and_then(|bytes|bytes.checked_add(backdrop.pixels.len()))
        .filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
    let mut alpha=Vec::new();
    if !composite{
        let count=source.pixels.len()/4;
        occupied.checked_add(count).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
        alpha.try_reserve_exact(count).map_err(|_|ImageError::TooLarge)?;
        occupied.checked_add(alpha.capacity()).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
        alpha.extend(source.pixels.chunks_exact(4).map(|pixel|pixel[3]));
    }
    // With an opaque source the backend's source-over blend computes exactly
    // the blending-only mixing formula. Restore its original alpha afterwards;
    // this avoids a private implementation of separable/nonseparable modes.
    resource_premultiply(&mut source,space,!composite);
    resource_premultiply(&mut backdrop,space,false);
    let paint=tiny_skia::PixmapPaint{blend_mode:resource_blend_mode(mode),..tiny_skia::PixmapPaint::default()};
    // tiny-skia's identity rectangle path uses only its fixed pipeline stack
    // below the backend's 8192 coordinate tiler threshold. Very long narrow
    // graph windows use borrowed one-row slices, avoiding its allocated path
    // tiler without another image or a hidden unleased backend allocation.
    let draw=|source:&[u8],target:&mut [u8],width:u32,height:u32|->Result<(),ImageError>{
        let source_map=tiny_skia::PixmapRef::from_bytes(source,width,height).ok_or(ImageError::InvalidViewport)?;
        let mut target=tiny_skia::PixmapMut::from_bytes(target,width,height).ok_or(ImageError::InvalidViewport)?;
        target.draw_pixmap(0,0,source_map,&paint,tiny_skia::Transform::identity(),None);Ok(())
    };
    if source.width<8192 && source.height<8192{draw(&source.pixels,&mut backdrop.pixels,source.width,source.height)?;}
    else{
        let row_bytes=(source.width as usize).checked_mul(4).ok_or(ImageError::TooLarge)?;
        for (source,target) in source.pixels.chunks_exact(row_bytes).zip(backdrop.pixels.chunks_exact_mut(row_bytes)){
            for (source,target) in source.chunks(8191*4).zip(target.chunks_mut(8191*4)){draw(source,target,(source.len()/4) as u32,1)?;}
        }
    }
    resource_demultiply(&mut backdrop,space);
    if !composite{for (pixel,alpha) in backdrop.pixels.chunks_exact_mut(4).zip(alpha){pixel[3]=alpha;if alpha==0{pixel[..3].fill(0);}}}
    Ok(backdrop)
}
fn blur_resource_image(input:Rgba8Image,input_region:FilterPixelRegion,target:FilterPixelRegion,
    sigma:[f32;2],edge:lumen_common::filter::resource::EdgeMode,space:lumen_common::color::ColorSpace,scale:f32,reserved:usize)->Result<Rgba8Image,ImageError> {
    use lumen_common::filter::resource::EdgeMode;
    let parameters=resource_blur_parameters(sigma,scale)?;let padding=resource_blur_padding(sigma,scale)?;
    if padding==[0,0] && input_region==target{return Ok(input);}
    let expanded=target.padded_axes(padding)?;
    let mut working=if edge==EdgeMode::None && input_region==expanded{input}else{
        let mut working=empty_filter_image(expanded,reserved.checked_add(input.pixels.len()).ok_or(ImageError::TooLarge)?)?;
        for y in 0..expanded.height{for x in 0..expanded.width{
            let px=expanded.left.checked_add(i64::from(x)).and_then(|x|x.checked_sub(input_region.left)).ok_or(ImageError::TooLarge)?;
            let py=expanded.top.checked_add(i64::from(y)).and_then(|y|y.checked_sub(input_region.top)).ok_or(ImageError::TooLarge)?;
            if let (Some(xi),Some(yi))=(resource_edge_coordinate(px,input_region.width,edge),resource_edge_coordinate(py,input_region.height,edge)){
                let from=(yi*input_region.width as usize+xi)*4;let at=(y as usize*expanded.width as usize+x as usize)*4;
                working.pixels[at..at+4].copy_from_slice(&input.pixels[from..from+4]);
            }
        }}drop(input);working
    };
    resource_premultiply(&mut working,space,false);
    let bytes=reserved.checked_add(working.pixels.len()).ok_or(ImageError::TooLarge)?;
    let mut output=empty_filter_image(target,bytes)?;
    let occupied=bytes.checked_add(output.pixels.len()).ok_or(ImageError::TooLarge)?;
    let pixels=(expanded.width as usize).checked_mul(expanded.height as usize).ok_or(ImageError::TooLarge)?;
    let sizes=parameters.kernel_sizes();
    // Maintained gray backend: input/output u8 planes, two f32 planes,
    // convolution f32 transient plus both vertical padding arenas, row arena,
    // original/scanned/symmetric kernels and borrowed column-row references.
    let scratch=pixels.checked_mul(18).and_then(|bytes|bytes.checked_add((expanded.width as usize).checked_mul(4)?))
        .and_then(|bytes|bytes.checked_add((sizes[0] as usize).checked_add(sizes[1] as usize)?.checked_mul(64)?)).ok_or(ImageError::TooLarge)?;
    occupied.checked_add(scratch).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
    let left=usize::try_from(target.left.checked_sub(expanded.left).ok_or(ImageError::TooLarge)?).map_err(|_|ImageError::TooLarge)?;
    let top=usize::try_from(target.top.checked_sub(expanded.top).ok_or(ImageError::TooLarge)?).map_err(|_|ImageError::TooLarge)?;
    for channel in 0..4{
        let mut plane=Vec::new();plane.try_reserve_exact(pixels).map_err(|_|ImageError::TooLarge)?;
        occupied.checked_add(scratch).and_then(|bytes|bytes.checked_add(plane.capacity().saturating_sub(pixels))).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
        plane.extend(working.pixels.chunks_exact(4).map(|pixel|pixel[channel]));
        let source=image::GrayImage::from_raw(expanded.width,expanded.height,plane).ok_or(ImageError::InvalidViewport)?;
        let blurred=image::imageops::blur_advanced(&source,parameters);
        for y in 0..target.height as usize{for x in 0..target.width as usize{output.pixels[(y*target.width as usize+x)*4+channel]=blurred.as_raw()[(y+top)*expanded.width as usize+x+left];}}
    }
    resource_demultiply(&mut output,space);
    Ok(output)
}
fn apply_filter_operations(image:&mut Rgba8Image,filters:&[lumen_common::filter::FilterOperation],scale:f32,reserved:usize)
    -> Result<(),ImageError> {
    use lumen_common::{color::Color,filter::{FilterOperation as F,ColorMatrix}};
    if filters.is_empty() || filters.len()>lumen_common::filter::MAX_COLOR_FILTERS
        || filters.iter().any(|filter|!filter.is_valid()) {return Err(ImageError::InvalidViewport);}
    let mut index=0;
    while index<filters.len() {
        if matches!(filters[index],F::Color(_)) {
            let mut matrices=[ColorMatrix::IDENTITY;lumen_common::filter::MAX_COLOR_FILTERS];let mut count=0;
            while let Some(F::Color(filter))=filters.get(index) {
                matrices[count]=filter.matrix();count+=1;index+=1;
            }
            for pixel in image.pixels.chunks_exact_mut(4) {
                let mut color=Color::rgba8([pixel[0],pixel[1],pixel[2],pixel[3]]);
                for matrix in &matrices[..count] {color=matrix.apply(color);}
                let output=color.to_rgba8();pixel.copy_from_slice(&output);
            }
            continue;
        }
        if let F::Blur(sigma)=&filters[index] {
            if filter_blur_padding(*sigma,scale)?==0 {index+=1;continue;}
        }
        if matches!(&filters[index],F::DropShadow(shadow) if shadow.color.alpha==0.0){index+=1;continue;}
        // The captured canvas is a finite SourceGraphic. A positive displaced
        // shadow can leave its first nonzero alpha cell at the canvas edge;
        // convolution must see transparent source outside that edge, never
        // the Gaussian implementation's clamped boundary value.
        let padding=match &filters[index]{F::DropShadow(shadow)=>u32::try_from(filter_blur_padding(shadow.sigma,scale)?).map_err(|_|ImageError::TooLarge)?,_=>0};
        let extent=padding.checked_mul(2).ok_or(ImageError::TooLarge)?;
        let plane_width=image.width.checked_add(extent).ok_or(ImageError::TooLarge)?;
        let plane_height=image.height.checked_add(extent).ok_or(ImageError::TooLarge)?;
        let plane_pixels=(plane_width as usize).checked_mul(plane_height as usize).ok_or(ImageError::TooLarge)?;
        reserved.checked_add(plane_pixels).filter(|n|*n<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
        let mut plane=Vec::new();plane.try_reserve_exact(plane_pixels).map_err(|_|ImageError::TooLarge)?;
        reserved.checked_add(plane.capacity()).filter(|n|*n<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
        match &filters[index] {
            F::Blur(sigma) => {
                let sigma=*sigma;
                // Filter primitives convolve premultiplied sRGB components;
                // process one plane at a time instead of four f32 images.
                for pixel in image.pixels.chunks_exact_mut(4) {
                    for channel in 0..3 {pixel[channel]=((u16::from(pixel[channel])*u16::from(pixel[3])+127)/255) as u8;}
                }
                for channel in 0..4 {
                    plane.clear();plane.extend(image.pixels.chunks_exact(4).map(|pixel|pixel[channel]));
                    plane=blur_filter_plane(plane,image.width,image.height,sigma,scale,reserved)?;
                    for (pixel,value) in image.pixels.chunks_exact_mut(4).zip(plane.iter()) {pixel[channel]=*value;}
                }
                for pixel in image.pixels.chunks_exact_mut(4) {
                    if pixel[3]==0 {pixel.fill(0);continue;}
                    for channel in 0..3 {pixel[channel]=((u32::from(pixel[channel])*255+u32::from(pixel[3])/2)/u32::from(pixel[3])).min(255) as u8;}
                }
            }
            F::DropShadow(shadow) => {
                let offset=shadow.offset;let sigma=shadow.sigma;let color=shadow.color;
                plane.resize(plane_pixels,0);
                for y in 0..image.height {for x in 0..image.width {
                    plane[((y+padding) as usize*plane_width as usize)+(x+padding) as usize]=image.pixels[(y as usize*image.width as usize+x as usize)*4+3];
                }}
                plane=blur_filter_plane(plane,plane_width,plane_height,sigma,scale,reserved)?;
                let color=color.to_rgba8();let rgba=Rgba{r:color[0],g:color[1],b:color[2],a:color[3]};
                for y in 0..image.height {for x in 0..image.width {
                    let mut alpha=0.0;
                    transform::visit_bilinear_samples(plane_width,plane_height,f64::from(x)-f64::from(offset[0])*f64::from(scale)+f64::from(padding),
                        f64::from(y)-f64::from(offset[1])*f64::from(scale)+f64::from(padding),|at,weight|alpha+=f64::from(plane[at])*weight);
                    let at=(y as usize*image.width as usize+x as usize)*4;
                    let pixel=&mut image.pixels[at..at+4];let source=Rgba{r:pixel[0],g:pixel[1],b:pixel[2],a:pixel[3]};
                    pixel.fill(0);composite(pixel,rgba,(alpha/255.0) as f32);composite(pixel,source,1.0);
                }}
            }
            F::Color(_) => unreachable!(),
            F::Url(_)|F::Resource(_)=>return Err(ImageError::Svg("resource filter requires the shared operation graph")),
        }
        index+=1;
    }
    Ok(())
}

fn svg_source_object_box(list:&[Command],owner:Affine,font:Option<&dyn FontProvider>,reserved:usize)->Result<Affine,ImageError> {
    let rect=svg_source_geometry_bounds(list,owner,font,reserved,None)?.unwrap_or(Rect{x:0.0,y:0.0,width:0.0,height:0.0});
    Ok(Affine{a:rect.width,d:rect.height,e:rect.x,f:rect.y,..Affine::IDENTITY})
}

/// The object-box path remains paint-free. Ink support additionally includes
/// bounded maintained stroke outlines at the raster's device resolution.
/// None declines a certificate when a separate raster algorithm owns support.
fn svg_source_geometry_bounds(list:&[Command],owner:Affine,font:Option<&dyn FontProvider>,reserved:usize,ink_scale:Option<f32>)->Result<Option<Rect>,ImageError> {
    let Some(inverse)=owner.inverse() else{return Ok(Some(Rect{x:0.0,y:0.0,width:0.0,height:0.0}));};
    let mut bounds:Option<Rect>=None;
    let mut transforms=Vec::new();let mut current=Affine::IDENTITY;
    let include=|bounds:&mut Option<Rect>,rect:Rect| {
        *bounds=Some(bounds.map_or(rect,|old|Rect{x:old.x.min(rect.x),y:old.y.min(rect.y),width:(old.x+old.width).max(rect.x+rect.width)-old.x.min(rect.x),height:(old.y+old.height).max(rect.y+rect.height)-old.y.min(rect.y)}));
    };
    for command in list {match command {
        Command::PushTransform(matrix)=>{
            if transforms.len()==256{return Err(ImageError::DisplayList(ReplayError::ClipLimit));}
            let required=transforms.len().checked_add(1).ok_or(ImageError::TooLarge)?;
            if required>transforms.capacity(){
                let capacity=required.max(transforms.capacity().checked_mul(2).ok_or(ImageError::TooLarge)?).max(4);
                let old=transforms.capacity().checked_mul(core::mem::size_of::<Affine>()).ok_or(ImageError::TooLarge)?;
                let new=capacity.checked_mul(core::mem::size_of::<Affine>()).ok_or(ImageError::TooLarge)?;
                reserved.checked_add(old).and_then(|bytes|bytes.checked_add(new))
                    .filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
                transforms.try_reserve_exact(capacity-transforms.len()).map_err(|_|ImageError::TooLarge)?;
                reserved.checked_add(old).and_then(|bytes|transforms.capacity().checked_mul(core::mem::size_of::<Affine>()).and_then(|new|bytes.checked_add(new)))
                    .filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
            }
            transforms.push(current);current=current.then(*matrix);
        }
        Command::PopTransform=>current=transforms.pop().ok_or(ImageError::DisplayList(ReplayError::UnbalancedClip))?,
        Command::SvgPath{data,transform,stroke,stroke_width,..}=>{
            let stack=transforms.capacity().checked_mul(core::mem::size_of::<Affine>()).ok_or(ImageError::TooLarge)?;
            let occupied=reserved.checked_add(stack).ok_or(ImageError::TooLarge)?;
            let remaining=MAX_LAYER_BYTES.checked_sub(occupied).ok_or(ImageError::TooLarge)?;
            if let Some(scale)=ink_scale.filter(|_|*stroke_width>0.0 && lumen_html::paint::svg_paint_has_ink(stroke.as_ref())) {
                let matrix=inverse.then(current).then(*transform);
                let device=tiny_skia::Transform::from_row(matrix.a*scale,matrix.b*scale,matrix.c*scale,matrix.d*scale,0.0,0.0);
                // If neither transformed stroke axis is wider than a device
                // pixel, the maintained raster may choose its hairline path.
                // A geometric outline cannot certify that separate support.
                let wide=stroke_width*device.sx.hypot(device.ky).max(device.kx.hypot(device.sy));
                if !wide.is_finite() || wide<=1.0 {return Ok(None);}
                let resolution=tiny_skia::PathStroker::compute_resolution_scale(&device);
                let mut geometry=lumen_common::svg_path::SvgGeometryArena::new(remaining,1);
                let Some(id)=geometry.path_data_at_resolution(data,Some(*stroke_width),resolution).map_err(|_|ImageError::TooLarge)? else{continue;};
                if let Some((_,outline))=geometry.projected_bounds(id,[matrix.a,matrix.b,matrix.c,matrix.d,matrix.e,matrix.f]).map_err(|_|ImageError::TooLarge)? {
                    include(&mut bounds,Rect{x:outline[0],y:outline[1],width:outline[2],height:outline[3]});
                }
                continue;
            }
            match lumen_common::svg_path::parse_svg_path_bounded(data,remaining) {
                Ok(parsed)=>{if let Some(path)=parsed.path {
                    occupied.checked_add(path.allocated_bytes()).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
                    let matrix=inverse.then(current).then(*transform);
                    let matrix=tiny_skia::Transform::from_row(matrix.a,matrix.b,matrix.c,matrix.d,matrix.e,matrix.f);
                    // The maintained owned transform updates this same point
                    // buffer in place; tight-bound evaluation is stack-only.
                    if let Some(path)=path.transform(matrix) {
                        let rect=path.compute_tight_bounds().unwrap_or_else(||path.bounds());
                        include(&mut bounds,Rect{x:rect.x(),y:rect.y(),width:rect.width(),height:rect.height()});
                    }
                }},
                Err(lumen_common::svg_path::SvgPathError::BudgetExceeded)=>return Err(ImageError::TooLarge),
                Err(lumen_common::svg_path::SvgPathError::Invalid(_))=>{},
            }
        }
        Command::GlyphRun{origin_x,baseline_y,size,glyphs,..}=>{
            let font=font.ok_or(ImageError::Font("font required for SVG object bounds"))?;
            for glyph in glyphs.iter() {
                let cell=font.glyph_cell_bounds(glyph.face,glyph.id,*size*glyph.size_scale)
                    .ok_or(ImageError::Font("glyph cell metrics required for SVG object bounds"))?;
                include(&mut bounds,inverse.then(current).bounds(Rect{
                    x:origin_x+glyph.x+cell.x,y:baseline_y-glyph.y+cell.y,width:cell.width,height:cell.height}));
            }
        }
        Command::FillRect{rect,..}|Command::FillRoundedRect{rect,..}|Command::FillGradient{rect,..}
        |Command::StrokeBorder{rect,..}|Command::StrokePatternBorder{rect,..}|Command::Image{rect,..}
        |Command::ReservedImage{rect,..}=>include(&mut bounds,inverse.then(current).bounds(*rect)),
        Command::FillBackground(fill)=>include(&mut bounds,inverse.then(current).bounds(fill.rect)),
        Command::StrokeBoxBorder(border)=>include(&mut bounds,inverse.then(current).bounds(border.rect)),
        // Layer rectangles, clipping/masking and shadows describe effects;
        // they cannot replace or expand their children's object geometry.
        _=>{}
    }}
    if !transforms.is_empty(){return Err(ImageError::DisplayList(ReplayError::UnbalancedClip));}
    Ok(Some(bounds.unwrap_or(Rect{x:0.0,y:0.0,width:0.0,height:0.0})))
}

/// Device-grid source windows keep blur halos and displaced shadow input
/// separate from the visible output, without allocating transparent gaps.
#[derive(Clone,Copy,Eq,PartialEq,Ord,PartialOrd)]
struct FilterPixelRegion {left:i64,top:i64,width:u32,height:u32}
impl FilterPixelRegion {
    fn from_rect(rect:Rect,scale:f32)->Result<Self,ImageError> {
        let left=(f64::from(rect.x)*f64::from(scale)).floor();
        let top=(f64::from(rect.y)*f64::from(scale)).floor();
        let right=(f64::from(rect.x+rect.width)*f64::from(scale)).ceil();
        let bottom=(f64::from(rect.y+rect.height)*f64::from(scale)).ceil();
        if ![left,top,right,bottom].iter().all(|value|value.is_finite())
            || left<i64::MIN as f64 || top<i64::MIN as f64 || right>i64::MAX as f64 || bottom>i64::MAX as f64
            || right-left<1.0 || bottom-top<1.0 || right-left>f64::from(u32::MAX) || bottom-top>f64::from(u32::MAX) {
            return Err(ImageError::TooLarge);
        }
        let region=Self{left:left as i64,top:top as i64,width:(right-left)as u32,height:(bottom-top)as u32};
        region.left.checked_add(i64::from(region.width)).ok_or(ImageError::TooLarge)?;
        region.top.checked_add(i64::from(region.height)).ok_or(ImageError::TooLarge)?;
        Ok(region)
    }
    fn rect(self,scale:f32)->Rect {Rect{x:self.left as f32/scale,y:self.top as f32/scale,
        width:self.width as f32/scale,height:self.height as f32/scale}}
    fn bytes(self)->Result<usize,ImageError> {(self.width as usize).checked_mul(self.height as usize)
        .and_then(|pixels|pixels.checked_mul(4)).ok_or(ImageError::TooLarge)}
    fn padded(self,padding:usize)->Result<Self,ImageError>{self.padded_axes([padding;2])}
    fn padded_axes(self,padding:[usize;2])->Result<Self,ImageError> {
        let x=u32::try_from(padding[0]).map_err(|_|ImageError::TooLarge)?;let y=u32::try_from(padding[1]).map_err(|_|ImageError::TooLarge)?;
        self.left.checked_add(i64::from(self.width)).and_then(|right|right.checked_add(i64::from(x))).ok_or(ImageError::TooLarge)?;
        self.top.checked_add(i64::from(self.height)).and_then(|bottom|bottom.checked_add(i64::from(y))).ok_or(ImageError::TooLarge)?;
        Ok(Self{left:self.left.checked_sub(i64::from(x)).ok_or(ImageError::TooLarge)?,top:self.top.checked_sub(i64::from(y)).ok_or(ImageError::TooLarge)?,
            width:self.width.checked_add(x.checked_mul(2).ok_or(ImageError::TooLarge)?).ok_or(ImageError::TooLarge)?,height:self.height.checked_add(y.checked_mul(2).ok_or(ImageError::TooLarge)?).ok_or(ImageError::TooLarge)?})
    }
    fn shadow_input(self,offset:[f32;2],padding:usize,scale:f32)->Result<Self,ImageError> {
        let x=f64::from(offset[0])*f64::from(scale);let y=f64::from(offset[1])*f64::from(scale);
        let left=(self.left as f64-x).floor();let top=(self.top as f64-y).floor();
        let right=(self.left.checked_add(i64::from(self.width)).ok_or(ImageError::TooLarge)? as f64-x).ceil();
        let bottom=(self.top.checked_add(i64::from(self.height)).ok_or(ImageError::TooLarge)? as f64-y).ceil();
        if ![left,top,right,bottom].iter().all(|value|value.is_finite()) || left<i64::MIN as f64
            || top<i64::MIN as f64 || right>i64::MAX as f64 || bottom>i64::MAX as f64
            || right-left>f64::from(u32::MAX) || bottom-top>f64::from(u32::MAX) {return Err(ImageError::TooLarge);}
        Self{left:left as i64,top:top as i64,width:(right-left)as u32,height:(bottom-top)as u32}
            .padded(padding.checked_add(1).ok_or(ImageError::TooLarge)?)
    }
}

fn filter_region_budget(region:FilterPixelRegion,reserved:usize)->Result<usize,ImageError> {
    let bytes=region.bytes()?;
    reserved.checked_add(bytes).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
    Ok(bytes)
}

fn crop_filter_region(image:&Rgba8Image,source:FilterPixelRegion,target:FilterPixelRegion,reserved:usize)
    ->Result<Rgba8Image,ImageError> {
    let len=filter_region_budget(target,reserved.checked_add(image.pixels.len()).ok_or(ImageError::TooLarge)?)?;
    let x=usize::try_from(target.left.checked_sub(source.left).ok_or(ImageError::TooLarge)?).map_err(|_|ImageError::TooLarge)?;
    let y=usize::try_from(target.top.checked_sub(source.top).ok_or(ImageError::TooLarge)?).map_err(|_|ImageError::TooLarge)?;
    if x+target.width as usize>image.width as usize || y+target.height as usize>image.height as usize {return Err(ImageError::InvalidViewport);}
    let mut pixels=Vec::new();pixels.try_reserve_exact(len).map_err(|_|ImageError::TooLarge)?;
    let stride=image.width as usize*4;let row=target.width as usize*4;
    for at in y..y+target.height as usize {pixels.extend_from_slice(&image.pixels[at*stride+x*4..at*stride+x*4+row]);}
    Ok(Rgba8Image{width:target.width,height:target.height,pixels})
}

/// An operation-local dependency graph evaluates every distinct ordered
/// primitive input window once. Its storage and live image results share the
/// existing layer budget, including graph construction before source painting.
struct FilterSourceDependency {start:usize,end:usize,node:Option<usize>}
const RESOURCE_SOURCE_ALPHA:usize=usize::MAX-1;
const RESOURCE_FILL_PAINT:usize=usize::MAX-2;
const RESOURCE_STROKE_PAINT:usize=usize::MAX-3;
const RESOURCE_TRANSPARENT:usize=usize::MAX-4;
struct FilterResourceSource {
    parent:usize,parent_prefix:usize,
    used:Arc<lumen_common::filter::resource::Use>,
    regions:Arc<[Option<[f32;4]>]>,
}
struct FilterGraphSource {
    view:SourceScopeView,end:usize,layer:Option<usize>,capture:bool,
    geometric_support:Option<Option<Rect>>,
    filters:Arc<[lumen_common::filter::FilterOperation]>,
    resource:Option<Box<FilterResourceSource>>,
}
impl FilterGraphSource {
    fn resource_bytes(&self)->Result<usize,ImageError>{
        self.resource.as_ref().map_or(Ok(0),|resource|resource.regions.len().checked_mul(core::mem::size_of::<Option<[f32;4]>>())
            .and_then(|bytes|bytes.checked_add(core::mem::size_of::<FilterResourceSource>()+2*core::mem::size_of::<usize>())).ok_or(ImageError::TooLarge))
    }
}
struct FilterRegionNode {
    source:usize,prefix:usize,region:FilterPixelRegion,
    inputs:[Option<usize>;2],extra_inputs:Option<Box<[usize]>>,children:Vec<FilterSourceDependency>,uses:usize,image:Option<Rgba8Image>,
    /// Actual output contour restriction, established by a raster mask or
    /// certified composition of already restricted inputs. Declared subregions
    /// alone never establish this provenance.
    contour:Option<Rect>,
}
struct FilterRegionGraph {
    nodes:Vec<FilterRegionNode>,sources:Vec<FilterGraphSource>,image_bytes:usize,construction_bytes:usize,payload_bytes:usize,source_search_bytes:usize,
    source_index:Option<std::collections::BTreeMap<(usize,usize,Option<usize>,bool,[u32;2],Option<(usize,usize)>),usize>>,
}
impl FilterRegionGraph {
    fn metadata_bytes(&self)->Result<usize,ImageError> {
        self.nodes.capacity().checked_mul(core::mem::size_of::<FilterRegionNode>())
            .and_then(|bytes|bytes.checked_add(self.construction_bytes))
            .and_then(|bytes|self.sources.capacity().checked_mul(core::mem::size_of::<FilterGraphSource>()).and_then(|source|bytes.checked_add(source)))
            .and_then(|bytes|bytes.checked_add(self.payload_bytes))
            .and_then(|bytes|bytes.checked_add(self.source_search_bytes)).ok_or(ImageError::TooLarge)
    }
    fn finish_construction(&mut self) {
        self.source_index=None;self.source_search_bytes=0;self.construction_bytes=0;
    }
    #[cfg(test)]
    fn assert_metadata_accounting(&self) {
        let nodes=self.nodes.capacity()*core::mem::size_of::<FilterRegionNode>();
        let sources=self.sources.capacity()*core::mem::size_of::<FilterGraphSource>();
        let filters=self.sources.iter().map(|source|source.filters.len()*core::mem::size_of::<lumen_common::filter::FilterOperation>()+2*core::mem::size_of::<usize>()+source.resource_bytes().unwrap()).sum::<usize>();
        let children=self.nodes.iter().map(|node|node.children.capacity()*core::mem::size_of::<FilterSourceDependency>()+node.extra_inputs.as_ref().map_or(0,|inputs|inputs.len()*core::mem::size_of::<usize>())).sum::<usize>();
        let searches=self.source_index.as_ref().map_or(0,|index|index.len()*(core::mem::size_of::<((usize,usize,Option<usize>,bool,[u32;2],Option<(usize,usize)>),usize)>()*16+128));
        assert_eq!(self.payload_bytes,filters+children,"charged filter and actual dependency capacities");
        assert_eq!(self.source_search_bytes,searches,"construction lookup lifetime");
        assert_eq!(self.metadata_bytes().unwrap(),nodes+sources+filters+children+searches+self.construction_bytes);
    }
    fn occupied(&self,reserved:usize)->Result<usize,ImageError> {
        reserved.checked_add(self.metadata_bytes()?).and_then(|bytes|bytes.checked_add(self.image_bytes))
            .filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)
    }
    #[cfg(test)]
    fn build(filters:&[lumen_common::filter::FilterOperation],region:FilterPixelRegion,scale:f32,reserved:usize)
        ->Result<(Self,usize),ImageError> {
        let mut graph=Self{nodes:Vec::new(),sources:Vec::new(),image_bytes:0,construction_bytes:0,payload_bytes:0,source_search_bytes:0,source_index:Some(std::collections::BTreeMap::new())};
        graph.register_source(FilterGraphSource{view:SourceScopeView::default(),end:0,layer:None,capture:false,filters:Arc::from(filters),resource:None,geometric_support:None},reserved)?;
        let mut index=std::collections::BTreeMap::new();
        let root=graph.add(0,filters.len(),region,reserved,&mut index)?;
        // The search index is construction-only. Pixel evaluation retains
        // the compact graph and the actual live dependency results.
        graph.compile_pending(0,scale,reserved,&mut index,None)?;
        drop(index);graph.finish_construction();graph.nodes[root].uses=1;
        Ok((graph,root))
    }
    fn add(&mut self,source:usize,prefix:usize,region:FilterPixelRegion,
        reserved:usize,index:&mut std::collections::BTreeMap<(usize,usize,FilterPixelRegion),usize>)
        ->Result<usize,ImageError> {
        if let Some(node)=index.get(&(source,prefix,region)) {return Ok(*node);}
        let count=self.nodes.len().checked_add(1).ok_or(ImageError::TooLarge)?;
        let capacity=if count<=self.nodes.capacity(){self.nodes.capacity()}else{count.max(self.nodes.capacity().saturating_mul(2))};
        // Conservative BTree node envelope (including a mostly empty root,
        // key/value slots, links and allocator overhead). No bitmap painting
        // occurs while construction is still expanding its dependency graph.
        let search_bytes=index.len().checked_add(1).and_then(|count|count.checked_mul(core::mem::size_of::<((usize,usize,FilterPixelRegion),usize)>()*16+128))
            .ok_or(ImageError::TooLarge)?;
        let old_bytes=self.metadata_bytes()?;
        let old_nodes=self.nodes.capacity().checked_mul(core::mem::size_of::<FilterRegionNode>()).ok_or(ImageError::TooLarge)?;
        let new_bytes=old_bytes.checked_sub(old_nodes).and_then(|bytes|bytes.checked_sub(self.construction_bytes))
            .and_then(|bytes|capacity.checked_mul(core::mem::size_of::<FilterRegionNode>()).and_then(|nodes|bytes.checked_add(nodes)))
            .and_then(|bytes|bytes.checked_add(search_bytes)).ok_or(ImageError::TooLarge)?;
        reserved.checked_add(new_bytes).and_then(|bytes|bytes.checked_add(self.image_bytes)).and_then(|bytes|bytes.checked_add(if capacity>self.nodes.capacity(){old_nodes}else{0}))
            .filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
        if capacity>self.nodes.capacity(){self.nodes.try_reserve_exact(capacity-self.nodes.len()).map_err(|_|ImageError::TooLarge)?;}
        self.construction_bytes=search_bytes;self.occupied(reserved)?;
        let node=self.nodes.len();self.nodes.push(FilterRegionNode{source,prefix,region,inputs:[None;2],extra_inputs:None,children:Vec::new(),uses:0,image:None,contour:None});
        index.insert((source,prefix,region),node);
        Ok(node)
    }
    fn compile_pending(&mut self,start:usize,scale:f32,reserved:usize,index:&mut std::collections::BTreeMap<(usize,usize,FilterPixelRegion),usize>,
        mut operation:Option<(&mut SourceScopeAnalysis,Option<&dyn FontProvider>,&mut GlyphCache)>)->Result<(),ImageError>{
        use lumen_common::filter::FilterOperation as F;
        let mut node=start;
        while node<self.nodes.len(){
        let source=self.nodes[node].source;let prefix=self.nodes[node].prefix;let region=self.nodes[node].region;
        let filters=self.sources[source].filters.clone();
        if self.sources[source].resource.is_some(){self.compile_resource_pending(node,scale,reserved,index)?;node+=1;continue;}
        if prefix!=usize::MAX {
            if let Some(F::Resource(used))=filters.get(prefix.wrapping_sub(1)) {
                let count=used.program.nodes().len();
                let resource=self.register_resource_source(source,prefix-1,used.clone(),reserved)?;
                let child=self.add(resource,if count==0{RESOURCE_TRANSPARENT}else{count},region,reserved,index)?;
                self.nodes[child].uses=self.nodes[child].uses.checked_add(1).ok_or(ImageError::TooLarge)?;
                self.nodes[node].inputs[0]=Some(child);node+=1;continue;
            }
        }
        let requests=if prefix==usize::MAX {[Some((filters.len(),region)),None]}else{match filters.get(prefix.wrapping_sub(1)) {
            None=>[None,None],
            Some(F::Color(_))=>{
                let first=filters[..prefix].iter().rposition(|filter|!matches!(filter,F::Color(_))).map_or(0,|i|i+1);
                [Some((first,region)),None]
            }
            Some(F::Blur(sigma))=>[Some((prefix-1,region.padded(filter_blur_padding(*sigma,scale)?)?)),None],
            Some(F::Resource(_))=>return Err(ImageError::InvalidViewport),
            Some(F::Url(_))=>return Err(ImageError::Svg("unbound URL filter reached paint")),
            Some(F::DropShadow(shadow))=>[Some((prefix-1,region)),
                if shadow.color.alpha==0.0 {None}else{Some((prefix-1,region.shadow_input(shadow.offset,filter_blur_padding(shadow.sigma,scale)?,scale)?))}],
        }};
        for (slot,request) in requests.into_iter().enumerate() {
            if let Some((prefix,region))=request {
                let child=self.add(source,prefix,region,reserved,index)?;
                self.nodes[child].uses=self.nodes[child].uses.checked_add(1).ok_or(ImageError::TooLarge)?;
                self.nodes[node].inputs[slot]=Some(child);
            }
        }
        if prefix==0 {
            if let Some((scene,font,cache))=operation.as_mut() {
                self.add_source_dependencies(node,scale,reserved,index,&mut **scene,*font,&mut **cache)?;
            }
        }
        node+=1;
        }
        Ok(())
    }
    fn register_source(&mut self,source:FilterGraphSource,reserved:usize)->Result<usize,ImageError>{
        let key=(source.view.start,source.end,source.layer,source.capture,source.view.translation.map(f32::to_bits),source.resource.as_ref().map(|resource|(resource.parent,resource.parent_prefix)));
        if let Some(index)=self.source_index.as_ref().and_then(|index|index.get(&key)){return Ok(*index);}
        let count=self.sources.len().checked_add(1).ok_or(ImageError::TooLarge)?;
        let capacity=if count<=self.sources.capacity(){self.sources.capacity()}else{count.max(self.sources.capacity().saturating_mul(2))};
        let filter_bytes=source.filters.len().checked_mul(core::mem::size_of::<lumen_common::filter::FilterOperation>())
            .and_then(|bytes|bytes.checked_add(2*core::mem::size_of::<usize>())).and_then(|bytes|bytes.checked_add(source.resource_bytes().ok()?)).ok_or(ImageError::TooLarge)?;
        let search_bytes=core::mem::size_of::<((usize,usize,Option<usize>,bool,[u32;2],Option<(usize,usize)>),usize)>()*16+128;
        let additional=if capacity>self.sources.capacity(){capacity.checked_mul(core::mem::size_of::<FilterGraphSource>()).ok_or(ImageError::TooLarge)?}else{0};
        self.occupied(reserved)?.checked_add(additional).and_then(|bytes|bytes.checked_add(filter_bytes)).and_then(|bytes|bytes.checked_add(search_bytes))
            .filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
        if capacity>self.sources.capacity(){self.sources.try_reserve_exact(capacity-self.sources.len()).map_err(|_|ImageError::TooLarge)?;}
        self.occupied(reserved)?;
        self.payload_bytes=self.payload_bytes.checked_add(filter_bytes).ok_or(ImageError::TooLarge)?;
        self.source_search_bytes=self.source_search_bytes.checked_add(search_bytes).ok_or(ImageError::TooLarge)?;
        self.occupied(reserved)?;
        let index=self.sources.len();self.sources.push(source);
        self.source_index.as_mut().ok_or(ImageError::InvalidViewport)?.insert(key,index);self.occupied(reserved)?;Ok(index)
    }
    fn source_geometric_support(&mut self,source:usize,scene:&SourceScopeAnalysis,
        font:Option<&dyn FontProvider>,scale:f32,reserved:usize)->Result<Option<Rect>,ImageError>{
        if let Some(bounds)=self.sources[source].geometric_support{return Ok(bounds);}
        let owner=&self.sources[source];
        let commands=scene.commands.get(owner.view.start..owner.end).ok_or(ImageError::InvalidViewport)?;
        // Geometric support cannot stand in for effect-expanded support. Such
        // scopes still receive the real contour, and never a guessed bbox proof.
        let unknown=commands.iter().any(|command|match command {
            Command::PushLayer{filters:Some(filters),..}=>!filters.is_empty(),
            // SVG object cells describe advance/ascent/descent, not raster
            // ink overhang. Until actual glyph ink support is joined here,
            // they cannot certify a source contour.
            Command::GlyphRun{..}|Command::BoxShadow{..}|Command::MaskedBackground(_)=>true,_=>false,
        });
        let bounds=if unknown{None}else{
            svg_source_geometry_bounds(commands,Affine::IDENTITY,font,self.occupied(reserved)?,Some(scale))?
                .map(|rect|Rect{x:rect.x+owner.view.translation[0],y:rect.y+owner.view.translation[1],..rect})
        };
        self.sources[source].geometric_support=Some(bounds);Ok(bounds)
    }
    fn resource_rect(&self,source:usize,rect:[f32;4])->Result<Rect,ImageError> {
        let resource=self.sources[source].resource.as_ref().ok_or(ImageError::InvalidViewport)?;
        let matrix=resource.used.owner_transform;
        // The general affine capture is joined separately to the existing
        // isolated transformed-source route; never approximate its sigma here.
        if matrix[..4]!=[1.0,0.0,0.0,1.0]{return Err(ImageError::Svg("filter local affine source capture is not yet supported"));}
        let view=self.sources[source].view;
        let rect=Rect{x:rect[0]+matrix[4]+view.translation[0],y:rect[1]+matrix[5]+view.translation[1],width:rect[2],height:rect[3]};
        if ![rect.x,rect.y,rect.width,rect.height].iter().all(|v|v.is_finite()){return Err(ImageError::TooLarge);}Ok(rect)
    }
    fn resource_declared_rect(&self,source:usize,prefix:usize)->Result<Option<Rect>,ImageError> {
        let resource=self.sources[source].resource.as_ref().ok_or(ImageError::InvalidViewport)?;
        let rect=if prefix>0 && prefix<=resource.regions.len(){resource.regions[prefix-1]}else{resource.used.filter_region().filter(|rect|rect[2]>0.0 && rect[3]>0.0)};
        rect.map(|rect|self.resource_rect(source,rect)).transpose()
    }
    fn resource_declared_region(&self,source:usize,prefix:usize,scale:f32)->Result<Option<FilterPixelRegion>,ImageError> {
        self.resource_declared_rect(source,prefix)?.map(|rect|FilterPixelRegion::from_rect(rect,scale)).transpose()
    }
    fn compile_resource_pending(&mut self,node:usize,scale:f32,reserved:usize,index:&mut std::collections::BTreeMap<(usize,usize,FilterPixelRegion),usize>)->Result<(),ImageError> {
        use lumen_common::filter::resource::{Primitive,EdgeMode};
        let source=self.nodes[node].source;let prefix=self.nodes[node].prefix;let region=self.nodes[node].region;
        if self.resource_declared_region(source,prefix,scale)?.is_none(){return Ok(());}
        let resource=self.sources[source].resource.as_ref().ok_or(ImageError::InvalidViewport)?;
        if prefix==0 || prefix==RESOURCE_SOURCE_ALPHA {
            let (parent,parent_prefix)=if prefix==0{(resource.parent,resource.parent_prefix)}else{(source,0)};
            let child=self.add(parent,parent_prefix,region,reserved,index)?;
            self.nodes[child].uses=self.nodes[child].uses.checked_add(1).ok_or(ImageError::TooLarge)?;self.nodes[node].inputs[0]=Some(child);return Ok(());
        }
        if matches!(prefix,RESOURCE_TRANSPARENT|RESOURCE_FILL_PAINT|RESOURCE_STROKE_PAINT){return Ok(());}
        let used=resource.used.clone();let primitive=used.program.nodes().get(prefix-1).ok_or(ImageError::InvalidViewport)?;
        let count=primitive.inputs.len();let mut extras=Vec::new();let extra_count=count.saturating_sub(2);
        let extra_bytes=extra_count.checked_mul(core::mem::size_of::<usize>()).ok_or(ImageError::TooLarge)?;
        self.occupied(reserved)?.checked_add(extra_bytes).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
        extras.try_reserve_exact(extra_count).map_err(|_|ImageError::TooLarge)?;
        let scratch=extras.capacity().checked_mul(core::mem::size_of::<usize>()).ok_or(ImageError::TooLarge)?;
        self.occupied(reserved)?.checked_add(scratch).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
        let working=reserved.checked_add(scratch).ok_or(ImageError::TooLarge)?;
        for (slot,input) in primitive.inputs.iter().enumerate() {
            let child_prefix=self.resource_input(source,*input)?;
            let requested=match &primitive.operation {
                Primitive::GaussianBlur{sigma,edge}=>{
                    let sigma=used.primitive_lengths(*sigma).ok_or(ImageError::TooLarge)?;
                    if *edge==EdgeMode::None{region.padded_axes(resource_blur_padding(sigma,scale)?)?}
                    else{self.resource_declared_region(source,child_prefix,scale)?.unwrap_or(region)}
                }
                Primitive::Offset(offset)=>region.shadow_input(used.primitive_lengths(*offset).ok_or(ImageError::TooLarge)?,0,scale)?,
                _=>region,
            };
            let child=self.add(source,child_prefix,requested,working,index)?;
            self.nodes[child].uses=self.nodes[child].uses.checked_add(1).ok_or(ImageError::TooLarge)?;
            if slot<2{self.nodes[node].inputs[slot]=Some(child);}else{extras.push(child);}
        }
        if !extras.is_empty(){
            self.occupied(working)?.checked_add(extra_bytes).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
            let extras=extras.into_boxed_slice();self.payload_bytes=self.payload_bytes.checked_add(extras.len()*core::mem::size_of::<usize>()).ok_or(ImageError::TooLarge)?;
            self.nodes[node].extra_inputs=Some(extras);self.occupied(reserved)?;
        }
        Ok(())
    }
    fn register_resource_source(&mut self,parent:usize,parent_prefix:usize,used:Arc<lumen_common::filter::resource::Use>,reserved:usize)->Result<usize,ImageError> {
        let source=&self.sources[parent];
        let key=(source.view.start,source.end,None,source.capture,source.view.translation.map(f32::to_bits),Some((parent,parent_prefix)));
        if let Some(index)=self.source_index.as_ref().and_then(|index|index.get(&key)){return Ok(*index);}
        let view=source.view;let end=source.end;let capture=source.capture;
        // Region geometry is a single operation-owned publication, compiled
        // from declared subregions and shared by all requested primitive tiles.
        let wrapper=core::mem::size_of::<FilterResourceSource>();
        let remaining=MAX_LAYER_BYTES.checked_sub(self.occupied(reserved)?).and_then(|bytes|bytes.checked_sub(wrapper)).ok_or(ImageError::TooLarge)?;
        let regions=used.resolved_regions(remaining).map_err(|_|ImageError::TooLarge)?;
        let bytes=regions.len().checked_mul(core::mem::size_of::<Option<[f32;4]>>()).and_then(|bytes|bytes.checked_add(2*core::mem::size_of::<usize>()+wrapper)).ok_or(ImageError::TooLarge)?;
        self.occupied(reserved)?.checked_add(bytes).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
        self.register_source(FilterGraphSource{view,end,layer:None,capture,filters:Arc::from([]),resource:Some(Box::new(FilterResourceSource{parent,parent_prefix,used,regions})),geometric_support:None},reserved)
    }
    fn resource_input(&self,source:usize,input:lumen_common::filter::resource::Input)->Result<usize,ImageError> {
        use lumen_common::filter::resource::Input;
        let resource=self.sources[source].resource.as_ref().ok_or(ImageError::InvalidViewport)?;
        match input {
            Input::SourceGraphic=>Ok(0),Input::SourceAlpha=>Ok(RESOURCE_SOURCE_ALPHA),Input::Result(at)=>Ok(at as usize+1),
            Input::FillPaint=>match resource.used.fill{lumen_common::filter::resource::PaintInput::Unsupported=>Err(ImageError::Svg("unsupported filter FillPaint source")),_=>Ok(RESOURCE_FILL_PAINT)},
            Input::StrokePaint=>match resource.used.stroke{lumen_common::filter::resource::PaintInput::Unsupported=>Err(ImageError::Svg("unsupported filter StrokePaint source")),_=>Ok(RESOURCE_STROKE_PAINT)},
            Input::BackgroundImage|Input::BackgroundAlpha=>Err(ImageError::Svg("unsupported filter background source")),
        }
    }
    fn add_source_dependencies(&mut self,node:usize,scale:f32,reserved:usize,
        index:&mut std::collections::BTreeMap<(usize,usize,FilterPixelRegion),usize>,
        scene:&mut SourceScopeAnalysis,font:Option<&dyn FontProvider>,cache:&mut GlyphCache)->Result<(),ImageError>{
        let source=self.nodes[node].source;let view=self.sources[source].view;let end=self.sources[source].end;
        let region=self.nodes[node].region;let capture=self.sources[source].capture;
        let reserved=reserved.checked_add(core::mem::size_of::<[Affine;256]>()).ok_or(ImageError::TooLarge)?;
        self.occupied(reserved)?;
        let mut transforms=[Affine::IDENTITY;256];let mut depth=0usize;let mut matrix=Affine::IDENTITY;
        let mut at=view.start;
        while at<end {
            match scene.commands[at].clone() {
                Command::PushTransform(transform)=>{
                    if depth==transforms.len(){return Err(ImageError::DisplayList(ReplayError::ClipLimit));}
                    transforms[depth]=matrix;depth+=1;
                    matrix=matrix.then(transform.translated_space(view.translation[0],view.translation[1]));
                    at+=1;
                }
                Command::PopTransform=>{
                    depth=depth.checked_sub(1).ok_or(ImageError::DisplayList(ReplayError::UnbalancedClip))?;
                    matrix=transforms[depth];at+=1;
                }
                Command::PushLayer{filters,..}=>{
                    let last=scene.end(at)?;
                    let nested_capture=capture || filters.as_ref().is_some_and(|filters|filters.iter().any(lumen_common::filter::FilterOperation::is_spatial));
                    let output=scene.output_bounds(at,capture,scale,font,cache,self.occupied(reserved)?)?;
                    let requested=output.and_then(|mut output|{
                        output.x+=view.translation[0];output.y+=view.translation[1];
                        let integral=matrix.a==1.0 && matrix.b==0.0 && matrix.c==0.0 && matrix.d==1.0
                            && (matrix.e*scale).fract()==0.0 && (matrix.f*scale).fract()==0.0;
                        if integral {matrix.inverse().and_then(|inverse|inverse.bounds(region.rect(scale)).intersection(output))}
                        else if matrix.inverse().is_some(){Some(output)}else{None}
                    });
                    let child=if let Some(requested)=requested {
                        let child=self.register_source(FilterGraphSource{view:SourceScopeView{start:at+1,..view},end:last,
                            layer:Some(at),capture:nested_capture,filters:filters.unwrap_or_else(||Arc::from([])),resource:None,geometric_support:None},reserved)?;
                        let child=self.add(child,usize::MAX,FilterPixelRegion::from_rect(requested,scale)?,reserved,index)?;
                        self.nodes[child].uses=self.nodes[child].uses.checked_add(1).ok_or(ImageError::TooLarge)?;Some(child)
                    }else{None};
                    let required=self.nodes[node].children.len().checked_add(1).ok_or(ImageError::TooLarge)?;
                    let capacity=if required<=self.nodes[node].children.capacity(){self.nodes[node].children.capacity()}else{required.max(self.nodes[node].children.capacity().saturating_mul(2))};
                    if capacity>self.nodes[node].children.capacity(){
                        self.occupied(reserved)?.checked_add(capacity.checked_mul(core::mem::size_of::<FilterSourceDependency>()).ok_or(ImageError::TooLarge)?)
                            .filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
                        let old_capacity=self.nodes[node].children.capacity();
                        let amount=capacity-self.nodes[node].children.len();
                        self.nodes[node].children.try_reserve_exact(amount).map_err(|_|ImageError::TooLarge)?;
                        let additional=self.nodes[node].children.capacity().checked_sub(old_capacity)
                            .and_then(|count|count.checked_mul(core::mem::size_of::<FilterSourceDependency>())).ok_or(ImageError::TooLarge)?;
                        self.payload_bytes=self.payload_bytes.checked_add(additional).ok_or(ImageError::TooLarge)?;
                        self.occupied(reserved)?;
                    }
                    self.nodes[node].children.push(FilterSourceDependency{start:at,end:last,node:child});
                    self.occupied(reserved)?;at=last+1;
                }
                _=>at+=1,
            }
        }
        if depth!=0{return Err(ImageError::DisplayList(ReplayError::UnbalancedClip));}
        Ok(())
    }
    fn publish(&mut self,node:usize,image:Rgba8Image,reserved:usize)->Result<(),ImageError> {
        self.occupied(reserved)?.checked_add(image.pixels.len()).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
        self.image_bytes=self.image_bytes.checked_add(image.pixels.len()).ok_or(ImageError::TooLarge)?;
        self.nodes[node].image=Some(image);Ok(())
    }
    fn input(&mut self,node:usize,source:&[Command],filters:&[lumen_common::filter::FilterOperation],scale:f32,
        font:Option<&dyn FontProvider>,cache:&mut GlyphCache,reserved:usize,capture:bool,scene:&mut SourceScopeAnalysis,view:SourceScopeView)->Result<Rgba8Image,ImageError> {
        if self.nodes[node].image.is_none(){self.evaluate_pending(node,source,filters,scale,font,cache,reserved,capture,scene,view)?;}
        self.nodes[node].uses=self.nodes[node].uses.checked_sub(1).ok_or(ImageError::InvalidViewport)?;
        if self.nodes[node].uses==0 {
            let image=self.nodes[node].image.take().ok_or(ImageError::InvalidViewport)?;
            self.image_bytes=self.image_bytes.checked_sub(image.pixels.len()).ok_or(ImageError::InvalidViewport)?;
            Ok(image)
        }else{
            let image=self.nodes[node].image.as_ref().ok_or(ImageError::InvalidViewport)?;
            self.occupied(reserved)?.checked_add(image.pixels.len()).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
            let mut pixels=Vec::new();pixels.try_reserve_exact(image.pixels.len()).map_err(|_|ImageError::TooLarge)?;
            pixels.extend_from_slice(&image.pixels);Ok(Rgba8Image{width:image.width,height:image.height,pixels})
        }
    }
    fn evaluate_pending(&mut self,root:usize,source:&[Command],filters:&[lumen_common::filter::FilterOperation],scale:f32,
        font:Option<&dyn FontProvider>,cache:&mut GlyphCache,reserved:usize,capture:bool,scene:&mut SourceScopeAnalysis,view:SourceScopeView)->Result<(),ImageError>{
        let mut stack:Vec<(usize,bool)>=Vec::new();
        self.push_pending(&mut stack,(root,false),reserved)?;
        while let Some((node,ready))=stack.pop(){
            if self.nodes[node].image.is_some(){continue;}
            if ready {
                let scratch=stack.capacity().checked_mul(core::mem::size_of::<(usize,bool)>()).and_then(|bytes|bytes.checked_add(reserved)).ok_or(ImageError::TooLarge)?;
                self.evaluate_ready(node,source,filters,scale,font,cache,scratch,capture,scene,view)?;
            }else{
                self.push_pending(&mut stack,(node,true),reserved)?;
                // LIFO order evaluates source inputs in their original order.
                for at in (0..self.nodes[node].children.len()).rev(){if let Some(child)=self.nodes[node].children[at].node{self.push_pending(&mut stack,(child,false),reserved)?;}}
                for at in (0..self.nodes[node].extra_inputs.as_ref().map_or(0,|inputs|inputs.len())).rev(){let child=self.nodes[node].extra_inputs.as_ref().unwrap()[at];self.push_pending(&mut stack,(child,false),reserved)?;}
                for at in (0..2).rev(){if let Some(child)=self.nodes[node].inputs[at]{self.push_pending(&mut stack,(child,false),reserved)?;}}
            }
        }
        Ok(())
    }
    fn push_pending(&self,stack:&mut Vec<(usize,bool)>,entry:(usize,bool),reserved:usize)->Result<(),ImageError>{
        let required=stack.len().checked_add(1).ok_or(ImageError::TooLarge)?;
        if required>stack.capacity(){
            let capacity=required.max(stack.capacity().saturating_mul(2));
            let peak=capacity.checked_add(stack.capacity()).and_then(|slots|slots.checked_mul(core::mem::size_of::<(usize,bool)>())).ok_or(ImageError::TooLarge)?;
            self.occupied(reserved)?.checked_add(peak).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
            stack.try_reserve_exact(capacity-stack.len()).map_err(|_|ImageError::TooLarge)?;
        }
        self.occupied(reserved)?.checked_add(stack.capacity().checked_mul(core::mem::size_of::<(usize,bool)>()).ok_or(ImageError::TooLarge)?)
            .filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
        stack.push(entry);Ok(())
    }
    fn evaluate_resource_ready(&mut self,node:usize,source:&[Command],scale:f32,
        font:Option<&dyn FontProvider>,cache:&mut GlyphCache,reserved:usize,
        scene:&mut SourceScopeAnalysis)->Result<(),ImageError> {
        use lumen_common::{color::{Color,ColorSpace},filter::resource::{Primitive,PaintInput}};
        let source_id=self.nodes[node].source;let prefix=self.nodes[node].prefix;
        let region=self.nodes[node].region;let domain=self.resource_declared_region(source_id,prefix,scale)?;let exact_domain=self.resource_declared_rect(source_id,prefix)?;
        let mut contour=None;let mut fresh_contour=true;
        let resource=self.sources[source_id].resource.as_ref().ok_or(ImageError::InvalidViewport)?;
        let used=resource.used.clone();let resource_parent=resource.parent;let resource_parent_prefix=resource.parent_prefix;let view=self.sources[source_id].view;let capture=self.sources[source_id].capture;
        let filters=self.sources[source_id].filters.clone();let inputs=self.nodes[node].inputs;
        // A declared empty subregion is transparent. Compilation deliberately
        // creates no dependencies for this stage, including generators.
        if domain.is_none(){let image=empty_filter_image(region,self.occupied(reserved)?)?;return self.publish(node,image,reserved);}
        let mut image=if prefix==0 || prefix==RESOURCE_SOURCE_ALPHA {
            let input=inputs[0].ok_or(ImageError::InvalidViewport)?;
            let prior=self.nodes[input].contour;
            let contains=|support:Rect|exact_domain.is_some_and(|domain|support.x>=domain.x && support.y>=domain.y
                && support.x+support.width<=domain.x+domain.width && support.y+support.height<=domain.y+domain.height);
            if prior.is_some_and(contains){fresh_contour=false;contour=prior;}
            else if prefix==0 && self.sources[resource_parent].filters[..resource_parent_prefix].iter()
                .all(|filter|matches!(filter,lumen_common::filter::FilterOperation::Color(_))) {
                // Matching native geometric support already owns its AA; the
                // padded raster ROI is unsuitable evidence for that ownership.
                if self.source_geometric_support(resource_parent,scene,font,scale,reserved)?.is_some_and(contains){
                    fresh_contour=false;contour=exact_domain;
                }
            }
            let mut image=self.input(input,source,&filters,scale,font,cache,reserved,capture,scene,view)?;
            if prefix==RESOURCE_SOURCE_ALPHA{for pixel in image.pixels.chunks_exact_mut(4){pixel[..3].fill(0);}}
            image
        }else if matches!(prefix,RESOURCE_FILL_PAINT|RESOURCE_STROKE_PAINT|RESOURCE_TRANSPARENT){
            let paint=match prefix{RESOURCE_FILL_PAINT=>used.fill,RESOURCE_STROKE_PAINT=>used.stroke,_=>PaintInput::None};
            let mut image=empty_filter_image(region,self.occupied(reserved)?)?;
            match paint{PaintInput::Solid(color)=>{let color=color.to_rgba8();for pixel in image.pixels.chunks_exact_mut(4){pixel.copy_from_slice(&color);}},
                PaintInput::None=>{},PaintInput::Unsupported=>return Err(ImageError::Svg("unsupported filter paint source"))};image
        }else{
            let primitive=used.program.nodes().get(prefix-1).ok_or(ImageError::InvalidViewport)?;
            let contained=|rect:Rect|exact_domain.is_some_and(|domain|rect.x>=domain.x && rect.y>=domain.y
                && rect.x+rect.width<=domain.x+domain.width && rect.y+rect.height<=domain.y+domain.height);
            let restricted_inputs=||->bool{
                !primitive.inputs.is_empty() && (0..primitive.inputs.len()).all(|slot|{
                    let input=if slot<2{inputs[slot]}else{self.nodes[node].extra_inputs.as_ref().and_then(|inputs|inputs.get(slot-2)).copied()};
                    input.and_then(|input|self.nodes[input].contour).is_some_and(contained)
                })
            };
            // Source-over, merge and blends cannot introduce alpha outside
            // their actual restricted input support. This is proven output
            // contour provenance, not equality/containment of declared regions.
            let compositional=matches!(primitive.operation,Primitive::Merge|Primitive::DropShadowMerge{..}|Primitive::Blend{..}
                |Primitive::Composite(lumen_common::filter::resource::Composite::Over
                    |lumen_common::filter::resource::Composite::In|lumen_common::filter::resource::Composite::Out
                    |lumen_common::filter::resource::Composite::Atop|lumen_common::filter::resource::Composite::Xor
                    |lumen_common::filter::resource::Composite::Lighter))
                || matches!(primitive.operation,Primitive::Composite(lumen_common::filter::resource::Composite::Arithmetic(k)) if k[3]<=0.0);
            if compositional && restricted_inputs(){fresh_contour=false;contour=exact_domain;}
            match &primitive.operation{
                Primitive::Transparent=>empty_filter_image(region,self.occupied(reserved)?)?,
                Primitive::Flood(_)=>{let mut image=empty_filter_image(region,self.occupied(reserved)?)?;
                    let transparent=Color::new(ColorSpace::Srgb,[0.0;3],0.0,0);
                    let color=primitive.operation.sample(transparent,transparent,primitive.color_space).ok_or(ImageError::InvalidViewport)?.to_rgba8();
                    for pixel in image.pixels.chunks_exact_mut(4){pixel.copy_from_slice(&color);}image},
                Primitive::GaussianBlur{sigma,edge}=>{
                    let input=inputs[0].ok_or(ImageError::InvalidViewport)?;let input_region=self.nodes[input].region;
                    if *sigma==[0.0;2] && self.resource_declared_rect(source_id,self.resource_input(source_id,primitive.inputs[0])?)?==exact_domain{
                        fresh_contour=false;contour=self.nodes[input].contour;
                    }
                    let image=self.input(input,source,&filters,scale,font,cache,reserved,capture,scene,view)?;
                    blur_resource_image(image,input_region,region,used.primitive_lengths(*sigma).ok_or(ImageError::TooLarge)?,*edge,primitive.color_space,scale,self.occupied(reserved)?)?
                },
                Primitive::Offset(offset)=>{
                    let input=inputs[0].ok_or(ImageError::InvalidViewport)?;let input_region=self.nodes[input].region;
                    if *offset==[0.0;2] && self.resource_declared_rect(source_id,self.resource_input(source_id,primitive.inputs[0])?)?==exact_domain{
                        fresh_contour=false;contour=self.nodes[input].contour;
                    }
                    let original=self.input(input,source,&filters,scale,font,cache,reserved,capture,scene,view)?;
                    let mut output=empty_filter_image(region,self.occupied(reserved)?.checked_add(original.pixels.len()).ok_or(ImageError::TooLarge)?)?;
                    let offset=used.primitive_lengths(*offset).ok_or(ImageError::TooLarge)?;
                    for y in 0..region.height{for x in 0..region.width{
                        let px=region.left as f64+f64::from(x)-f64::from(offset[0])*f64::from(scale);
                        let py=region.top as f64+f64::from(y)-f64::from(offset[1])*f64::from(scale);
                        let at=(y as usize*region.width as usize+x as usize)*4;
                        output.pixels[at..at+4].copy_from_slice(&sample_filter_image(&original,input_region,px,py).to_rgba8());
                    }}output
                },
                Primitive::Merge|Primitive::DropShadowMerge{..}=>{
                    // Evaluate ordered merge inputs one at a time; the graph's
                    // shared-image uses retain only inputs still needed by a
                    // later primitive, rather than a second variadic raster set.
                    let mut output=empty_filter_image(region,self.occupied(reserved)?)?;
                    let count=primitive.inputs.len();
                    for slot in 0..count{
                        let input=if slot<2{inputs[slot].ok_or(ImageError::InvalidViewport)?}
                            else{*self.nodes[node].extra_inputs.as_ref().and_then(|inputs|inputs.get(slot-2)).ok_or(ImageError::InvalidViewport)?};
                        let working=reserved.checked_add(output.pixels.len()).ok_or(ImageError::TooLarge)?;
                        let image=self.input(input,source,&filters,scale,font,cache,working,capture,scene,view)?;
                        if image.width!=region.width || image.height!=region.height{return Err(ImageError::InvalidViewport);}
                        output=blend_resource_images(image,output,lumen_common::filter::resource::BlendMode::Normal,
                            true,primitive.color_space,self.occupied(reserved)?)?;
                    }output
                },
                Primitive::Blend{mode,composite}=>{
                    let first=self.input(inputs[0].ok_or(ImageError::InvalidViewport)?,source,&filters,scale,font,cache,reserved,capture,scene,view)?;
                    let second=self.input(inputs[1].ok_or(ImageError::InvalidViewport)?,source,&filters,scale,font,cache,
                        reserved.checked_add(first.pixels.len()).ok_or(ImageError::TooLarge)?,capture,scene,view)?;
                    blend_resource_images(first,second,*mode,*composite,primitive.color_space,self.occupied(reserved)?)?
                },
                Primitive::Matrix(_)|Primitive::ComponentTransfer(_)|Primitive::Composite(_)=>{
                    let input=inputs[0].ok_or(ImageError::InvalidViewport)?;
                    let prior_contour=self.nodes[input].contour;
                    let input_domain=self.resource_declared_rect(source_id,self.resource_input(source_id,primitive.inputs[0])?)?;
                    let mut alpha_unchanged=true;
                    let mut first=self.input(input,source,&filters,scale,font,cache,reserved,capture,scene,view)?;
                    let second=if let Some(input)=inputs[1]{Some(self.input(input,source,&filters,scale,font,cache,
                        reserved.checked_add(first.pixels.len()).ok_or(ImageError::TooLarge)?,capture,scene,view)?)}else{None};
                    if first.width!=region.width || first.height!=region.height || second.as_ref().is_some_and(|image|image.width!=region.width || image.height!=region.height){return Err(ImageError::InvalidViewport);}
                    for (at,pixel) in first.pixels.chunks_exact_mut(4).enumerate(){
                        let color=Color::rgba8([pixel[0],pixel[1],pixel[2],pixel[3]]);
                        let other=second.as_ref().map(|image|{let pixel=&image.pixels[at*4..at*4+4];Color::rgba8([pixel[0],pixel[1],pixel[2],pixel[3]])})
                            .unwrap_or_else(||Color::new(ColorSpace::Srgb,[0.0;3],0.0,0));
                        let output=primitive.operation.sample(color,other,primitive.color_space).ok_or(ImageError::InvalidViewport)?.to_rgba8();
                        alpha_unchanged &= output[3]==pixel[3];pixel.copy_from_slice(&output);
                    }
                    // Retain actual native/previous contour only when this
                    // complete operation preserves alpha and its exact domain.
                    let transparent=Color::new(ColorSpace::Srgb,[0.0;3],0.0,0);
                    let transparency_preserving=matches!(primitive.operation,Primitive::Matrix(_)|Primitive::ComponentTransfer(_))
                        && primitive.operation.sample(transparent,transparent,primitive.color_space)
                            .is_some_and(|color|color.alpha==0.0);
                    // A pointwise operation preserving transparent black cannot
                    // escape an already applied input contour, even when it
                    // changes alpha. The geometric restriction remains owned by
                    // that image; multiplying its coverage again would square AA.
                    if transparency_preserving && prior_contour.is_some_and(contained){fresh_contour=false;contour=prior_contour;}
                    else if alpha_unchanged && input_domain==exact_domain && (prior_contour.is_none() || prior_contour==exact_domain){fresh_contour=false;contour=prior_contour;}
                    first
                },
            }
        };
        // All primitive inputs denote premultiplied RGBA. Straight-alpha
        // buffering is an optimization, not hidden color state that a later
        // alpha-changing matrix/transfer may recover from transparent pixels.
        for pixel in image.pixels.chunks_exact_mut(4){if pixel[3]==0{pixel[..3].fill(0);}}
        clip_filter_image(&mut image,region,domain)?;
        if fresh_contour{if let Some(domain)=exact_domain{apply_filter_domain_coverage(&mut image,region,domain,scale,self.occupied(reserved)?)?;contour=Some(domain);}}
        self.nodes[node].contour=contour;self.publish(node,image,reserved)
    }
    fn evaluate_ready(&mut self,node:usize,source:&[Command],_filters:&[lumen_common::filter::FilterOperation],scale:f32,
        font:Option<&dyn FontProvider>,cache:&mut GlyphCache,reserved:usize,_capture:bool,scene:&mut SourceScopeAnalysis,_view:SourceScopeView)->Result<(),ImageError> {
    use lumen_common::filter::FilterOperation as F;
    let region=self.nodes[node].region;
    let prefix=self.nodes[node].prefix;
    let source_id=self.nodes[node].source;
    let source_view=self.sources[source_id].view;
    let capture=self.sources[source_id].capture;
    let filters=self.sources[source_id].filters.clone();
    let inputs=self.nodes[node].inputs;
    if self.nodes[node].inputs.iter().flatten().any(|input|self.nodes[*input].image.is_none())
        || self.nodes[node].extra_inputs.as_ref().is_some_and(|inputs|inputs.iter().any(|input|self.nodes[*input].image.is_none()))
        || self.nodes[node].children.iter().filter_map(|child|child.node).any(|input|self.nodes[input].image.is_none()) {return Err(ImageError::InvalidViewport);}
    let occupied=self.occupied(reserved)?;
    filter_region_budget(region,occupied)?;
    if self.sources[source_id].resource.is_some(){return self.evaluate_resource_ready(node,source,scale,font,cache,reserved,scene);}
    if prefix==usize::MAX {
        let mut image=self.input(inputs[0].unwrap(),source,&filters,scale,font,cache,reserved,capture,scene,source_view)?;
        let push=self.sources[source_id].layer.ok_or(ImageError::InvalidViewport)?;
        let mut command=scene.commands[push].clone();
        let original_owner=match &command{Command::PushLayer{svg_clip,..}=>svg_clip.as_ref().filter(|owner|owner.clip.units==lumen_html::paint::SvgGradientUnits::ObjectBoundingBox).map(|owner|owner.transform),_=>None};
        let working=self.occupied(reserved)?.checked_add(image.pixels.len()).ok_or(ImageError::TooLarge)?;
        let svg_object_box=original_owner.map(|owner|scene.svg_object_box(push,owner,font,working)).transpose()?;
        translate_command(&mut command,source_view.translation[0],source_view.translation[1]);
        let Command::PushLayer{rect,radius,corners,opacity,clip,svg_clip,..}=command else{return Err(ImageError::InvalidViewport);};
        apply_layer_result_coverage(&mut image,rect,radius,corners.as_deref().copied(),opacity,clip,svg_clip.as_deref(),svg_object_box,
            scale,region.left as f32,region.top as f32,working)?;
        return self.publish(node,image,reserved);
    }
    let all_filters=filters.as_ref();
    let filters=&all_filters[..prefix];
    let Some(last)=filters.last() else {
        // Source composition consumes already evaluated nested scope outputs.
        // Original primitive/clip/transform commands keep their source order;
        // replacing each child owner once prevents recursively rebuilding its
        // filter graph for every parent input branch.
        let count=self.sources[source_id].end.checked_sub(source_view.start).ok_or(ImageError::InvalidViewport)?;
        let mut owned_payload=0usize;let mut at=source_view.start;let mut dependency=0usize;
        while at<self.sources[source_id].end {
            if dependency<self.nodes[node].children.len() && self.nodes[node].children[dependency].start==at {
                at=self.nodes[node].children[dependency].end+1;dependency+=1;
            }else{
                owned_payload=owned_payload.checked_add(scene.commands[at].clone_owned_bytes().ok_or(ImageError::TooLarge)?).ok_or(ImageError::TooLarge)?;
                at+=1;
            }
        }
        let command_bytes=count.checked_mul(core::mem::size_of::<Command>()).and_then(|bytes|bytes.checked_add(owned_payload)).ok_or(ImageError::TooLarge)?;
        self.occupied(reserved)?.checked_add(command_bytes).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
        let mut composed=DisplayList::default();composed.0.try_reserve_exact(count).map_err(|_|ImageError::TooLarge)?;
        let command_bytes=composed.0.capacity().checked_mul(core::mem::size_of::<Command>()).and_then(|bytes|bytes.checked_add(owned_payload)).ok_or(ImageError::TooLarge)?;
        let mut retained=command_bytes;let mut at=source_view.start;let mut dependency=0usize;
        while at<self.sources[source_id].end {
            if dependency<self.nodes[node].children.len() && self.nodes[node].children[dependency].start==at {
                let end=self.nodes[node].children[dependency].end;
                if let Some(child)=self.nodes[node].children[dependency].node {
                    let child_region=self.nodes[child].region;
                    let working=reserved.checked_add(retained).ok_or(ImageError::TooLarge)?;
                    let image=self.input(child,source,all_filters,scale,font,cache,working,capture,scene,source_view)?;
                    let wrapper=core::mem::size_of::<ImageData>()+2*core::mem::size_of::<usize>();
                    self.occupied(reserved)?.checked_add(retained).and_then(|bytes|bytes.checked_add(image.pixels.len())).and_then(|bytes|bytes.checked_add(wrapper))
                        .filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
                    retained=retained.checked_add(image.pixels.len()).and_then(|bytes|bytes.checked_add(wrapper)).ok_or(ImageError::TooLarge)?;
                    composed.0.push(Command::Image{rect:child_region.rect(scale),image:Arc::new(ImageData{width:image.width,height:image.height,pixels:image.pixels})});
                }
                at=end+1;dependency+=1;
            }else{
                let mut command=scene.commands[at].clone();
                translate_command(&mut command,source_view.translation[0],source_view.translation[1]);
                composed.0.push(command);at+=1;
            }
        }
        let resolver_storage=composed.0.len().checked_mul(core::mem::size_of::<Command>()).and_then(|bytes|bytes.checked_add(owned_payload)).ok_or(ImageError::TooLarge)?;
        let mut bytes=self.occupied(reserved)?.checked_add(retained).and_then(|bytes|bytes.checked_add(resolver_storage)).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
        // Only structural transform/mask resolution remains. Every ordinary
        // nested layer has been replaced by its graph result above.
        let child=resolve_layers_region(&composed.0,scale,font,cache,&mut bytes,capture,Some(region.rect(scale)))?;
        filter_region_budget(region,bytes)?;
        let image=render_scaled_region_at(&child,region.width,region.height,scale,font,cache,[region.left,region.top],Some(bytes))?;
        return self.publish(node,image,reserved);
    };
    let image=match last {
        F::Color(_)=>{
            // Typed CSS color functions preserve transparent alpha and cannot
            // move ink outside an existing geometric contour.
            self.nodes[node].contour=self.nodes[inputs[0].ok_or(ImageError::InvalidViewport)?].contour;
            let first=filters.iter().rposition(|filter|!matches!(filter,F::Color(_))).map_or(0,|index|index+1);
            let mut image=self.input(inputs[0].unwrap(),source,all_filters,scale,font,cache,reserved,capture,scene,source_view)?;
            let working=self.occupied(reserved)?.checked_add(image.pixels.len()).ok_or(ImageError::TooLarge)?;
            apply_filter_operations(&mut image,&filters[first..],scale,working)?;
            Ok(image)
        }
        F::Blur(sigma)=>{
            let padding=filter_blur_padding(*sigma,scale)?;
            if padding==0 {let image=self.input(inputs[0].unwrap(),source,all_filters,scale,font,cache,reserved,capture,scene,source_view)?;return self.publish(node,image,reserved);}
            let expanded=region.padded(padding)?;
            let mut image=self.input(inputs[0].unwrap(),source,all_filters,scale,font,cache,reserved,capture,scene,source_view)?;
            let working=self.occupied(reserved)?.checked_add(image.pixels.len()).ok_or(ImageError::TooLarge)?;
            apply_filter_operations(&mut image,core::slice::from_ref(last),scale,working)?;
            crop_filter_region(&image,expanded,region,self.occupied(reserved)?)
        }
        F::Resource(_)=>{
            self.nodes[node].contour=self.nodes[inputs[0].ok_or(ImageError::InvalidViewport)?].contour;
            let image=self.input(inputs[0].ok_or(ImageError::InvalidViewport)?,source,all_filters,scale,font,cache,reserved,capture,scene,source_view)?;Ok(image)
        }
        F::Url(_)=>Err(ImageError::Svg("unbound URL filter reached paint")),
        F::DropShadow(shadow)=>{
            if shadow.color.alpha==0.0 {let image=self.input(inputs[0].unwrap(),source,all_filters,scale,font,cache,reserved,capture,scene,source_view)?;return self.publish(node,image,reserved);}
            let mut original=self.input(inputs[0].unwrap(),source,all_filters,scale,font,cache,reserved,capture,scene,source_view)?;
            let working=reserved.checked_add(original.pixels.len()).ok_or(ImageError::TooLarge)?;
            let input=region.shadow_input(shadow.offset,filter_blur_padding(shadow.sigma,scale)?,scale)?;
            let image=self.input(inputs[1].unwrap(),source,all_filters,scale,font,cache,working,capture,scene,source_view)?;
            let working=self.occupied(working)?.checked_add(image.pixels.len()).ok_or(ImageError::TooLarge)?;
            let pixels=image.pixels.len()/4;
            working.checked_add(pixels).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
            let mut plane=Vec::new();plane.try_reserve_exact(pixels).map_err(|_|ImageError::TooLarge)?;
            plane.extend(image.pixels.chunks_exact(4).map(|pixel|pixel[3]));
            let plane=blur_filter_plane(plane,image.width,image.height,shadow.sigma,scale,working)?;
            let color=shadow.color.to_rgba8();let color=Rgba{r:color[0],g:color[1],b:color[2],a:color[3]};
            let dx=(region.left-input.left)as f64-f64::from(shadow.offset[0])*f64::from(scale);
            let dy=(region.top-input.top)as f64-f64::from(shadow.offset[1])*f64::from(scale);
            for y in 0..region.height {for x in 0..region.width {
                let mut alpha=0.0;
                transform::visit_bilinear_samples(image.width,image.height,f64::from(x)+dx,f64::from(y)+dy,
                    |at,weight|alpha+=f64::from(plane[at])*weight);
                let at=(y as usize*region.width as usize+x as usize)*4;let pixel=&mut original.pixels[at..at+4];
                let source=Rgba{r:pixel[0],g:pixel[1],b:pixel[2],a:pixel[3]};
                pixel.fill(0);composite(pixel,color,(alpha/255.0)as f32);composite(pixel,source,1.0);
            }}
            Ok(original)
        }
    }?;
    self.publish(node,image,reserved)
}
}

#[cfg(test)]
fn render_filter_region(source:&[Command],filters:&[lumen_common::filter::FilterOperation],region:FilterPixelRegion,
    scale:f32,font:Option<&dyn FontProvider>,cache:&mut GlyphCache,reserved:usize,capture:bool)->Result<Rgba8Image,ImageError> {
    let mut scene=SourceScopeAnalysis::new(source,reserved)?;
    let reserved=reserved.checked_add(scene.bytes()?).ok_or(ImageError::TooLarge)?;
    render_filter_region_in_operation(source,filters,region,scale,font,cache,reserved,capture,&mut scene,SourceScopeView::default())
}

#[cfg(test)]
fn render_filter_region_in_operation(source:&[Command],filters:&[lumen_common::filter::FilterOperation],region:FilterPixelRegion,
    scale:f32,font:Option<&dyn FontProvider>,cache:&mut GlyphCache,reserved:usize,capture:bool,scene:&mut SourceScopeAnalysis,view:SourceScopeView)->Result<Rgba8Image,ImageError> {
    let mut graph=FilterRegionGraph{nodes:Vec::new(),sources:Vec::new(),image_bytes:0,construction_bytes:0,payload_bytes:0,source_search_bytes:0,source_index:Some(std::collections::BTreeMap::new())};
    let filter_bytes=filters.len().checked_mul(core::mem::size_of::<lumen_common::filter::FilterOperation>()).and_then(|bytes|bytes.checked_add(2*core::mem::size_of::<usize>())).ok_or(ImageError::TooLarge)?;
    graph.occupied(reserved)?.checked_add(filter_bytes).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
    let source_id=graph.register_source(FilterGraphSource{view,end:view.start+source.len(),layer:None,capture,filters:Arc::from(filters),resource:None,geometric_support:None},reserved)?;
    let mut index=std::collections::BTreeMap::new();
    let root=graph.add(source_id,filters.len(),region,reserved,&mut index)?;
    graph.compile_pending(0,scale,reserved,&mut index,Some((scene,font,cache)))?;
    drop(index);graph.finish_construction();graph.nodes[root].uses=1;
    graph.input(root,source,filters,scale,font,cache,reserved,capture,scene,view)
}

fn apply_layer_result_coverage(image:&mut Rgba8Image,rect:Rect,radius:f32,corners:Option<[[f32;2];4]>,
    opacity:f32,clip:bool,svg_clip:Option<&lumen_html::paint::SvgLayerClip>,svg_object_box:Option<Affine>,
    scale:f32,left:f32,top:f32,reserved:usize)->Result<(),ImageError> {
        let width=image.width as f32;let height=image.height as f32;
        if let Some(owner)=svg_clip {
            // Source image + mask Canvas + its snapshot remain under one shared
            // layer budget. Clip coverage is applied once, after all filters.
            let len=image.pixels.len();
            let pixels=len/4;
            let peak=len.checked_mul(2).and_then(|rgba|pixels.checked_mul(3).and_then(|alpha|rgba.checked_add(alpha)))
                .and_then(|scratch|reserved.checked_add(scratch)).filter(|bytes|*bytes<=MAX_LAYER_BYTES)
                .ok_or(ImageError::TooLarge)?;
            let path_bytes=owner.clip.shapes.iter().try_fold(0usize,|bytes,shape|shape.data.len().checked_mul(64)
                .and_then(|path|path.checked_add(256)).and_then(|path|bytes.checked_add(path))).ok_or(ImageError::TooLarge)?;
            peak.checked_add(path_bytes).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
            let mut mask=canvas::CanvasSurface::new(width as u32,height as u32)?;
            let screen=Affine{a:scale,d:scale,e:-left,f:-top,..Affine::IDENTITY};
            apply_svg_clips(&mut mask,core::slice::from_ref(owner.clip.as_ref()),owner.transform,
                svg_object_box.unwrap_or(Affine::IDENTITY),screen,Some(peak))?;
            mask.state_mut().fill=[255;4];mask.fill_rect(0.0,0.0,width,height)?;
            let coverage=mask.snapshot();
            for(pixel,mask)in image.pixels.chunks_exact_mut(4).zip(coverage.pixels.chunks_exact(4)) {
                pixel[3]=((u16::from(pixel[3])*u16::from(mask[3])+127)/255)as u8;
            }
        }
        let radius = radius.min(rect.width * 0.5).min(rect.height * 0.5) * scale;
        // Rounded contour quantization is defined in device coordinates. Rebasing
        // its origin into each ROI changes half-tie rounding at negative local
        // coordinates, so all tiles must sample the same absolute contour.
        let clip_left = rect.x * scale;
        let clip_top = rect.y * scale;
        let right = rect.x * scale + rect.width * scale;
        let bottom = rect.y * scale + rect.height * scale;
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
                let device_y = py as f32 + top;
                let row_inside = device_y >= clip_top && device_y + 1.0 <= bottom;
                for (px, pixel) in row.chunks_exact_mut(4).enumerate() {
                    let device_x = px as f32 + left;
                    if row_inside
                        && device_x >= interior_start
                        && device_x + 1.0 <= interior_end
                    {
                        if opacity != 1.0 {
                            pixel[3] = (pixel[3] as f32 * opacity).round() as u8;
                        }
                        continue;
                    }
                    let coverage = if let Some(corners) = corners {
                        coverage::rounded_corners(
                            device_x,
                            device_y,
                            clip_left,
                            clip_top,
                            right,
                            bottom,
                            corners.map(|r| [r[0] * scale, r[1] * scale]),
                        )
                    } else {
                        coverage::rounded(
                            device_x, device_y, clip_left, clip_top, right, bottom, radius,
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
    Ok(())
}

fn filter_visible_region(image:&Rgba8Image,region:FilterPixelRegion)->Option<FilterPixelRegion> {
    let mut left=image.width;let mut top=image.height;let mut right=0;let mut bottom=0;
    for (index,pixel) in image.pixels.chunks_exact(4).enumerate() {
        if pixel[3]==0 {continue;}
        let x=(index%image.width as usize)as u32;let y=(index/image.width as usize)as u32;
        left=left.min(x);top=top.min(y);right=right.max(x+1);bottom=bottom.max(y+1);
    }
    (right>left && bottom>top).then(||FilterPixelRegion{left:region.left+i64::from(left),
        top:region.top+i64::from(top),width:right-left,height:bottom-top})
}

fn resolve_layer_region_tiles(source:&[Command],filters:Option<&[lumen_common::filter::FilterOperation]>,
    region:Rect,rect:Rect,radius:f32,corners:Option<[[f32;2];4]>,opacity:f32,clip:bool,
    svg_clip:Option<&lumen_html::paint::SvgLayerClip>,svg_object_box:Option<Affine>,
    scale:f32,font:Option<&dyn FontProvider>,cache:&mut GlyphCache,bytes:&mut usize,capture:bool,scene:&mut SourceScopeAnalysis,view:SourceScopeView)
    ->Result<DisplayList,ImageError> {
    let filters=filters.unwrap_or(&[]);
    if filters.len()>lumen_common::filter::MAX_COLOR_FILTERS || filters.iter().any(|filter|!filter.is_valid()) {
        return Err(ImageError::InvalidViewport);
    }
    let region=if clip {let Some(region)=region.intersection(rect)else{return Ok(DisplayList::default());};region}else{region};
    let region=FilterPixelRegion::from_rect(region,scale)?;
    let mut graph=if let Some(graph)=scene.graph.take(){
        let metadata=graph.metadata_bytes()?.checked_add(core::mem::size_of::<FilterRegionGraph>()).ok_or(ImageError::TooLarge)?;
        *bytes=bytes.checked_sub(metadata).ok_or(ImageError::InvalidViewport)?;
        *graph
    }else{FilterRegionGraph{nodes:Vec::new(),sources:Vec::new(),image_bytes:0,construction_bytes:0,payload_bytes:0,source_search_bytes:0,source_index:None}};
    graph.source_index=Some(std::collections::BTreeMap::new());
    let first_pending=graph.nodes.len();
    let mut output=DisplayList::default();
    let count=(region.height as usize).checked_add(31).ok_or(ImageError::TooLarge)?/32;
    let root_bytes=count.checked_mul(core::mem::size_of::<(FilterPixelRegion,usize)>()).ok_or(ImageError::TooLarge)?;
    bytes.checked_add(root_bytes).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
    let mut roots=Vec::new();roots.try_reserve_exact(count).map_err(|_|ImageError::TooLarge)?;
    let root_bytes=roots.capacity().checked_mul(core::mem::size_of::<(FilterPixelRegion,usize)>()).ok_or(ImageError::TooLarge)?;
    let reserved=bytes.checked_add(root_bytes).ok_or(ImageError::TooLarge)?;
    let filter_bytes=filters.len().checked_mul(core::mem::size_of::<lumen_common::filter::FilterOperation>()).and_then(|bytes|bytes.checked_add(2*core::mem::size_of::<usize>())).ok_or(ImageError::TooLarge)?;
    graph.occupied(reserved)?.checked_add(filter_bytes).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
    let source_id=graph.register_source(FilterGraphSource{view,end:view.start+source.len(),layer:None,capture,filters:Arc::from(filters),resource:None,geometric_support:None},reserved)?;
    let mut index=std::collections::BTreeMap::new();let mut row=0u32;
    while row<region.height {
        let tile=FilterPixelRegion{height:(region.height-row).min(32),top:region.top+i64::from(row),..region};
        let root=graph.add(source_id,filters.len(),tile,reserved,&mut index)?;
        graph.nodes[root].uses=graph.nodes[root].uses.checked_add(1).ok_or(ImageError::TooLarge)?;
        roots.push((tile,root));row+=tile.height;
    }
    graph.compile_pending(first_pending,scale,reserved,&mut index,Some((scene,font,cache)))?;
    drop(index);graph.finish_construction();
    for (tile,root) in roots {
        let reserved=bytes.checked_add(root_bytes).ok_or(ImageError::TooLarge)?;
        let mut image=graph.input(root,source,filters,scale,font,cache,reserved,capture,scene,view)?;
        let coverage_reserved=graph.occupied(reserved)?.checked_add(image.pixels.len()).ok_or(ImageError::TooLarge)?;
        apply_layer_result_coverage(&mut image,rect,radius,corners,opacity,clip,svg_clip,svg_object_box,
            scale,tile.left as f32,tile.top as f32,coverage_reserved)?;
        if let Some(visible)=filter_visible_region(&image,tile) {
            if visible!=tile {image=crop_filter_region(&image,tile,visible,graph.occupied(reserved)?)?;}
            let wrapper=core::mem::size_of::<ImageData>()+2*core::mem::size_of::<usize>();
            graph.occupied(reserved)?.checked_add(image.pixels.len()).and_then(|bytes|bytes.checked_add(wrapper))
                .filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
            *bytes=bytes.checked_add(image.pixels.len()).and_then(|bytes|bytes.checked_add(wrapper)).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
            output.0.push(Command::Image{rect:visible.rect(scale),image:Arc::new(ImageData{
                width:image.width,height:image.height,pixels:image.pixels})});
        }
    }
    if graph.image_bytes!=0 || graph.nodes.iter().any(|node|node.uses!=0){return Err(ImageError::InvalidViewport);}
    let metadata=graph.metadata_bytes()?.checked_add(core::mem::size_of::<FilterRegionGraph>()).ok_or(ImageError::TooLarge)?;
    *bytes=bytes.checked_add(metadata).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
    scene.graph=Some(Box::new(graph));
    Ok(output)
}

fn capture_source_command(command:&Command,scale:f32,reserved:usize)->Result<Command,ImageError> {
    let mut command=command.clone();
    if let Command::SvgPath{bounds,data,transform,stroke,stroke_width,..}=&mut command {
        // This is the existing crop-only SVG projection. It is not an author
        // clip; explicit viewport/overflow owners remain in the scope tree.
        let remaining=MAX_LAYER_BYTES.checked_sub(reserved).ok_or(ImageError::TooLarge)?;
        match lumen_common::svg_path::parse_svg_path_bounded(data,remaining) {
            Ok(parsed)=>{
                if let Some(path)=parsed.path {
                    reserved.checked_add(path.allocated_bytes()).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
                    // This crop projection transforms the maintained path's
                    // bounds and inflates the existing conservative stroke
                    // envelope. It does not allocate or invoke a stroker.
                    if let Some(ink)=transformed_svg_path_bounds(&path,*transform,
                        lumen_html::paint::svg_paint_has_ink(stroke.as_ref()),*stroke_width,scale) {*bounds=ink;}
                }
            }
            Err(lumen_common::svg_path::SvgPathError::BudgetExceeded)=>return Err(ImageError::TooLarge),
            Err(lumen_common::svg_path::SvgPathError::Invalid(_))=>{},
        }
    }
    Ok(command)
}

/// Immutable source identity is a coordinate in the operation's original
/// command stream. Integral translation views preserve that identity; temporary
/// translated Vec addresses are never cache keys.
#[derive(Clone,Copy,Default)]
struct SourceScopeView { start:usize, translation:[f32;2] }
struct SourceScopeBounds {
    push:usize,end:usize,
    source:[Option<Option<Rect>>;2],output:[Option<Option<Rect>>;2],svg_object_box:Option<Affine>,
}
struct SourceScopeAnalysis<'a> {
    commands:&'a [Command],scopes:Vec<SourceScopeBounds>,graph:Option<Box<FilterRegionGraph>>,
}
impl<'a> SourceScopeAnalysis<'a> {
    fn new(commands:&'a [Command],reserved:usize)->Result<Self,ImageError> {
        let count=commands.iter().filter(|command|matches!(command,Command::PushLayer{..}|Command::PushTransform(_))).count();
        let metadata=count.checked_mul(core::mem::size_of::<SourceScopeBounds>()).ok_or(ImageError::TooLarge)?;
        let stack_bytes=256usize*core::mem::size_of::<usize>();
        reserved.checked_add(metadata).and_then(|bytes|bytes.checked_add(stack_bytes)).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
        let mut scopes=Vec::new();scopes.try_reserve_exact(count).map_err(|_|ImageError::TooLarge)?;
        let mut stack=[0usize;256];let mut depth=0usize;
        for (index,command) in commands.iter().enumerate() {
            match command {
                Command::PushLayer{..}|Command::PushTransform(_)=>{
                    if depth==stack.len(){return Err(ImageError::DisplayList(ReplayError::ClipLimit));}
                    stack[depth]=scopes.len();depth+=1;
                    scopes.push(SourceScopeBounds{push:index,end:0,source:[None;2],output:[None;2],svg_object_box:None});
                }
                Command::PopLayer|Command::PopTransform=>{
                    depth=depth.checked_sub(1).ok_or(ImageError::DisplayList(ReplayError::UnbalancedClip))?;
                    let scope=&mut scopes[stack[depth]];
                    if !matches!((&commands[scope.push],command),(Command::PushLayer{..},Command::PopLayer)|(Command::PushTransform(_),Command::PopTransform)) {
                        return Err(ImageError::DisplayList(ReplayError::UnbalancedClip));
                    }
                    scope.end=index;
                }
                _=>{},
            }
        }
        if depth!=0{return Err(ImageError::DisplayList(ReplayError::UnbalancedClip));}
        let result=Self{commands,scopes,graph:None};
        reserved.checked_add(result.bytes()?).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
        Ok(result)
    }
    fn bytes(&self)->Result<usize,ImageError>{
        let bytes=self.scopes.capacity().checked_mul(core::mem::size_of::<SourceScopeBounds>()).ok_or(ImageError::TooLarge)?;
        if let Some(graph)=&self.graph {bytes.checked_add(graph.metadata_bytes()?).and_then(|bytes|bytes.checked_add(core::mem::size_of::<FilterRegionGraph>())).ok_or(ImageError::TooLarge)}else{Ok(bytes)}
    }
    fn scope(&self,push:usize)->Result<usize,ImageError>{self.scopes.binary_search_by_key(&push,|scope|scope.push).map_err(|_|ImageError::DisplayList(ReplayError::UnbalancedClip))}
    fn end(&self,push:usize)->Result<usize,ImageError>{Ok(self.scopes[self.scope(push)?].end)}
    fn source_bounds(&mut self,push:usize,spatial:bool,scale:f32,font:Option<&dyn FontProvider>,cache:&mut GlyphCache,reserved:usize)->Result<Option<Rect>,ImageError>{
        let scope=self.scope(push)?;let slot=usize::from(spatial);
        if let Some(bounds)=self.scopes[scope].source[slot]{return Ok(bounds);}
        let end=self.scopes[scope].end;
        let bounds=self.range_bounds(push+1,end,spatial,scale,font,cache,reserved)?;
        self.scopes[scope].source[slot]=Some(bounds);Ok(bounds)
    }
    fn output_bounds(&mut self,push:usize,spatial:bool,scale:f32,font:Option<&dyn FontProvider>,cache:&mut GlyphCache,reserved:usize)->Result<Option<Rect>,ImageError>{
        let scope=self.scope(push)?;let slot=usize::from(spatial);
        if let Some(bounds)=self.scopes[scope].output[slot]{return Ok(bounds);}
        let command=self.commands[push].clone();
        let bounds=match command {
            Command::PushLayer{filters,rect,opacity,clip,..}=>{
                let capture=spatial || filters.as_ref().is_some_and(|filters|filters.iter().any(lumen_common::filter::FilterOperation::is_spatial));
                let mut bounds=if opacity==0.0 {None}else{self.source_bounds(push,capture,scale,font,cache,reserved)?};
                if opacity!=0.0 {if let Some(filters)=filters.as_deref(){bounds=filter_output_bounds(bounds,filters,scale)?;}}
                if clip{bounds=bounds.and_then(|bounds|bounds.intersection(rect));}
                bounds.map(|bounds|FilterPixelRegion::from_rect(bounds,scale).map(|region|region.rect(scale))).transpose()?
            }
            Command::PushTransform(matrix)=>{
                if matrix.inverse().is_none(){None}else{self.source_bounds(push,spatial,scale,font,cache,reserved)?
                    .map(|bounds|FilterPixelRegion::from_rect(bounds,scale)
                        .and_then(|region|FilterPixelRegion::from_rect(matrix.bounds(region.rect(scale)),scale))
                        .map(|region|region.rect(scale))).transpose()?}
            }
            _=>return Err(ImageError::InvalidViewport),
        };
        self.scopes[scope].output[slot]=Some(bounds);Ok(bounds)
    }
    fn svg_object_box(&mut self,push:usize,owner:Affine,font:Option<&dyn FontProvider>,reserved:usize)->Result<Affine,ImageError>{
        let scope=self.scope(push)?;
        if let Some(bounds)=self.scopes[scope].svg_object_box{return Ok(bounds);}
        let bounds=svg_source_object_box(&self.commands[push+1..self.scopes[scope].end],owner,font,reserved)?;
        self.scopes[scope].svg_object_box=Some(bounds);Ok(bounds)
    }
    fn range_bounds(&mut self,start:usize,end:usize,spatial:bool,scale:f32,font:Option<&dyn FontProvider>,cache:&mut GlyphCache,reserved:usize)->Result<Option<Rect>,ImageError>{
        let reserved=reserved.checked_add(core::mem::size_of::<[Option<Rect>;256]>()).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)?;
        let mut bounds:Option<Rect>=None;let mut clips=[None;256];let mut depth=0usize;
        let mut current_clip:Option<Rect>=None;let mut index=start;
        while index<end {
            let command=&self.commands[index];
            let ink=match command {
                Command::PushClip(rect)|Command::PushBoxClip(rect)=>{
                    let rect=*rect;
                    if depth==clips.len(){return Err(ImageError::DisplayList(ReplayError::ClipLimit));}
                    clips[depth]=current_clip;depth+=1;
                    current_clip=Some(current_clip.map_or(rect,|clip|clip.intersection(rect)
                        .unwrap_or(Rect{x:rect.x,y:rect.y,width:0.0,height:0.0})));index+=1;continue;
                }
                Command::PopClip=>{depth=depth.checked_sub(1).ok_or(ImageError::DisplayList(ReplayError::UnbalancedClip))?;current_clip=clips[depth];index+=1;continue;}
                Command::PushLayer{..}|Command::PushTransform(_)=>{
                    let ink=self.output_bounds(index,spatial,scale,font,cache,reserved)?;
                    index=self.end(index)?;ink
                }
                Command::SvgPath{..} if spatial=>{
                    let command=capture_source_command(command,scale,reserved)?;
                    paint_bounds(core::slice::from_ref(&command),scale,font,cache)?
                }
                _=>paint_bounds(core::slice::from_ref(command),scale,font,cache)?,
            };
            if let Some(rect)=ink.and_then(|rect|current_clip.map_or(Some(rect),|clip|rect.intersection(clip))) {
                if !rect.is_valid(){return Err(ImageError::TooLarge);}
                bounds=Some(bounds.map_or(rect,|old|{let x=old.x.min(rect.x);let y=old.y.min(rect.y);
                    Rect{x,y,width:(old.x+old.width).max(rect.x+rect.width)-x,height:(old.y+old.height).max(rect.y+rect.height)-y}}));
            }
            index+=1;
        }
        if depth!=0{return Err(ImageError::DisplayList(ReplayError::UnbalancedClip));}
        Ok(bounds)
    }
}

#[cfg(test)]
fn scoped_source_output_bounds(list:&[Command],scale:f32,font:Option<&dyn FontProvider>,cache:&mut GlyphCache,spatial:bool,reserved:usize)->Result<Option<Rect>,ImageError>{
    let mut scene=SourceScopeAnalysis::new(list,reserved)?;
    scene.range_bounds(0,list.len(),spatial,scale,font,cache,reserved+scene.bytes()?)
}

fn resolve_layers(list:&[Command],scale:f32,font:Option<&dyn FontProvider>,
    cache:&mut GlyphCache,bytes:&mut usize)->Result<DisplayList,ImageError> {
    resolve_layers_with_source(list,scale,font,cache,bytes,false)
}

fn resolve_layers_with_source(
    list: &[Command],
    scale: f32,
    font: Option<&dyn FontProvider>,
    cache: &mut GlyphCache,
    bytes: &mut usize,
    spatial_source: bool,
) -> Result<DisplayList, ImageError> {
    resolve_layers_region(list,scale,font,cache,bytes,spatial_source,None)
}

fn resolve_layers_region(
    list:&[Command],scale:f32,font:Option<&dyn FontProvider>,cache:&mut GlyphCache,
    bytes:&mut usize,spatial_source:bool,region:Option<Rect>,
)->Result<DisplayList,ImageError> {
    let mut scene=SourceScopeAnalysis::new(list,*bytes)?;
    let metadata=scene.bytes()?;
    *bytes=bytes.checked_add(metadata).ok_or(ImageError::TooLarge)?;
    let result=resolve_layers_region_in_operation(list,scale,font,cache,bytes,spatial_source,region,&mut scene,SourceScopeView::default());
    *bytes=bytes.checked_sub(scene.bytes()?).ok_or(ImageError::InvalidViewport)?;
    result
}

fn resolve_layers_region_in_operation(
    list:&[Command],scale:f32,font:Option<&dyn FontProvider>,cache:&mut GlyphCache,bytes:&mut usize,
    spatial_source:bool,region:Option<Rect>,scene:&mut SourceScopeAnalysis,view:SourceScopeView,
)->Result<DisplayList,ImageError>{
    let mut output = DisplayList(Vec::with_capacity(list.len()));
    let mut index = 0;
    while index < list.len() {
        if let Command::MaskedBackground(mask) = &list[index] {
            if mask.shadow_blur.is_some() {
                if let Some(command)=resolve_text_ink_shadow(mask,scale,font,cache,bytes)? {output.0.push(command);}
                index+=1;continue;
            }
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
            let end = scene.end(view.start+index)?.checked_sub(view.start).ok_or(ImageError::InvalidViewport)?;
            index = end + 1;
            if matrix.inverse().is_none() {
                continue;
            }
            // Place a translated glyph through the same global-baseline
            // authority as ordinary text. Its cached mask keeps its source AA;
            // a second bitmap interpolation would change the mask at fractional
            // layout origins. Other primitives retain the integral fast path.
            if matrix.is_finite()
                && matrix.a == 1.0
                && matrix.b == 0.0
                && matrix.c == 0.0
                && matrix.d == 1.0
                && ((matrix.e * scale).fract() == 0.0
                    && (matrix.f * scale).fract() == 0.0
                    || list[start..end].iter().all(|command| matches!(command, Command::GlyphRun { .. })))
            {
                let mut child = list[start..end].to_vec();
                for command in &mut child {
                    translate_command(command, matrix.e, matrix.f);
                }
                output
                    .0
                    .extend(resolve_layers_region_in_operation(&child, scale, font, cache, bytes,spatial_source,region,scene,SourceScopeView{start:view.start+start,translation:[view.translation[0]+matrix.e,view.translation[1]+matrix.f]})?.0);
                continue;
            }
            // Nonintegral transforms sample a complete isolated source.
            // Independently transforming local source tiles would lose
            // bilinear neighbors at their seams and alter vector coverage.
            let mut child = resolve_layers_region_in_operation(&list[start..end], scale, font, cache, bytes,spatial_source,None,scene,SourceScopeView{start:view.start+start,..view})?;
            if matrix.b == 0.0
                && matrix.c == 0.0
                && child.0.iter().all(|command| {
                    matches!(
                        command,
                        Command::FillRect { .. } | Command::PushClip(_) | Command::PushBoxClip(_) | Command::PopClip
                    ) || matrix.a > 0.0 && matrix.d > 0.0 && match command {
                        Command::Image { .. } => true,
                        Command::FillBackground(fill) => fill.radius == 0.0 && fill.corners.is_none()
                            && matches!(fill.image, lumen_html::paint::BackgroundPaint::Image(_)
                                | lumen_html::paint::BackgroundPaint::Solid(_)),
                        _ => false,
                    }
                })
            {
                for command in &mut child.0 {
                    match command {
                        Command::FillRect { rect, .. } | Command::Image { rect, .. } | Command::ReservedImage { rect, .. } | Command::PushClip(rect) | Command::PushBoxClip(rect) => {
                            *rect = matrix.bounds(*rect)
                        }
                        Command::FillBackground(fill) => {
                            let fill=&mut **fill;
                            for rect in [&mut fill.rect, &mut fill.positioning_rect, &mut fill.image_rect] {
                                *rect = matrix.bounds(*rect);
                            }
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
            ref svg_clip,
            ref filters,
            ref corners,
            rect,
            radius,
            opacity,
            clip,
        } = list[index]
        else {
            let command=if spatial_source {capture_source_command(&list[index],scale,*bytes)?}
                else{list[index].clone()};
            output.0.push(command);
            index += 1;
            continue;
        };
        let start = index + 1;
        let end = scene.end(view.start+index)?.checked_sub(view.start).ok_or(ImageError::InvalidViewport)?;
        index = end + 1;
        if (clip && (rect.width == 0.0 || rect.height == 0.0)) || opacity == 0.0 {
            continue;
        }
        let svg_object_box=svg_clip.as_ref().filter(|owner|owner.clip.units==lumen_html::paint::SvgGradientUnits::ObjectBoundingBox)
            .map(|owner|scene.svg_object_box(view.start+start-1,
                Affine{e:owner.transform.e-view.translation[0],f:owner.transform.f-view.translation[1],..owner.transform},font,*bytes)).transpose()?;
        let capture=spatial_source || filters.as_ref().is_some_and(|filters|filters.iter().any(lumen_common::filter::FilterOperation::is_spatial));
        // A transformed parent can request a complete isolated source rather
        // than a viewport ROI. Resource filters still require the same graph:
        // the pointwise legacy raster path cannot evaluate their input DAG.
        if region.is_some() || filters.as_ref().is_some_and(|filters|filters.iter().any(|filter|
            matches!(filter,lumen_common::filter::FilterOperation::Resource(_)))) {
            let source_bounds=scene.source_bounds(view.start+start-1,capture,scale,font,cache,*bytes)?.map(|mut bounds|{bounds.x+=view.translation[0];bounds.y+=view.translation[1];bounds});
            let output_bounds=if let Some(filters)=filters.as_deref(){filter_output_bounds(source_bounds,filters,scale)?}else{source_bounds};
            let Some(output_bounds)=output_bounds else{continue;};
            // Bounds select a virtual device window. They do not introduce an
            // author clip, and input halos are still expanded independently.
            let output_bounds=FilterPixelRegion::from_rect(output_bounds,scale)?.rect(scale);
            let region=if let Some(region)=region{let Some(region)=region.intersection(output_bounds)else{continue;};region}else{output_bounds};
            output.0.extend(resolve_layer_region_tiles(&list[start..end],filters.as_deref(),region,
                rect,radius,corners.as_deref().copied(),opacity,clip,svg_clip.as_deref(),svg_object_box,
                scale,font,cache,bytes,capture,scene,SourceScopeView{start:view.start+start,..view})?.0);
            continue;
        }
        let child = resolve_layers_region_in_operation(&list[start..end], scale, font, cache, bytes,capture,None,scene,SourceScopeView{start:view.start+start,..view})?;
        // An identity, square rectangle whose device edges are integral has
        // binary coverage. Only then may clipping distribute through source-
        // over without changing overlapping children's composited alpha.
        if clip && opacity == 1.0 && filters.is_none() && svg_clip.is_none() && radius == 0.0 && corners.is_none()
            && [rect.x, rect.y, rect.x + rect.width, rect.y + rect.height]
                .iter().all(|edge| (edge * scale).is_finite() && (edge * scale).fract() == 0.0)
        {
            output.0.push(Command::PushClip(rect));
            output.0.extend(child.0);
            output.0.push(Command::PopClip);
            continue;
        }
        let source_bounds=paint_bounds(&child.0,scale,font,cache)?;
        // Own layer clipping consumes the filtered result. Source ink outside
        // that clip can contribute to pixels inside it through spatial filters.
        let crop=if let Some(filters)=filters {filter_output_bounds(source_bounds,filters,scale)?}
            else if clip {source_bounds.and_then(|bounds|bounds.intersection(rect))}else{source_bounds};
        let Some(crop)=crop else{continue;};
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
        let mut image = render_scaled_region_at(&child,width as u32,height as u32,scale,font,cache,[left as i64,top as i64],Some(*bytes-len))?;
        if let Some(filters)=filters {apply_filter_operations(&mut image,filters,scale,*bytes)?;}
        apply_layer_result_coverage(&mut image,rect,radius,corners.as_deref().copied(),opacity,clip,
            svg_clip.as_deref(),svg_object_box,scale,left,top,*bytes)?;
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
    let mut clips: Vec<Option<Rect>>=Vec::new();
    let mut current_clip: Option<Rect>=None;
    for command in list {
        match command {
            Command::PushClip(rect)|Command::PushBoxClip(rect) => {
                if clips.len()==512 {return Err(ImageError::DisplayList(ReplayError::ClipLimit));}
                clips.try_reserve(1).map_err(|_|ImageError::TooLarge)?;clips.push(current_clip);
                current_clip=Some(current_clip.map_or(*rect,|clip|clip.intersection(*rect)
                    .unwrap_or(Rect{x:rect.x,y:rect.y,width:0.0,height:0.0})));
                continue;
            }
            Command::PopClip => {current_clip=clips.pop().ok_or(ImageError::DisplayList(ReplayError::UnbalancedClip))?;continue;}
            _=>{}
        }
        let mut include = |rect: Rect| {
        let rect=if let Some(clip)=current_clip {let Some(rect)=rect.intersection(clip) else{return;};rect}else{rect};
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
        match command {
            Command::FillRect { rect, color }
            | Command::FillRoundedRect { rect, color, .. }
            | Command::StrokeBorder { rect, color, .. }
            | Command::StrokePatternBorder { rect, color, .. }
                if color.a != 0 =>
            {
                include(*rect)
            }
            Command::Image { rect, .. } | Command::ReservedImage { rect, .. } | Command::FillGradient { rect, .. } => include(*rect),
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
    if !clips.is_empty() {return Err(ImageError::DisplayList(ReplayError::UnbalancedClip));}
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
        Command::PushLayer{rect,svg_clip,..}=>{
            rect.x+=x;rect.y+=y;
            if let Some(owner)=svg_clip {let owner=Arc::make_mut(owner);owner.transform.e+=x;owner.transform.f+=y;}
        }
        Command::PushClip(rect) | Command::PushBoxClip(rect)
        | Command::FillRect { rect, .. }
        | Command::FillRoundedRect { rect, .. }
        | Command::FillGradient { rect, .. }
        | Command::BoxShadow { rect, .. }
        | Command::StrokePatternBorder { rect, .. }
        | Command::StrokeBorder { rect, .. }
        | Command::Image { rect, .. } | Command::ReservedImage { rect, .. } => {
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
    render_scaled_region_at(list,width,height,scale,font,cache,[0,0],None)
}

fn render_scaled_region_at(list:&DisplayList,width:u32,height:u32,scale:f32,font:Option<&dyn FontProvider>,
    cache:&mut GlyphCache,device_origin:[i64;2],reserved:Option<usize>)->Result<Rgba8Image,ImageError> {
    let len=(width as usize).checked_mul(height as usize).and_then(|pixels|pixels.checked_mul(4)).ok_or(ImageError::TooLarge)?;
    let source_reserved=reserved.map(|reserved|reserved.checked_add(len).filter(|bytes|*bytes<=MAX_LAYER_BYTES).ok_or(ImageError::TooLarge)).transpose()?;
    let mut image=Rgba8Image{width,height,pixels:vec![0;len]};
    let mut sink = Raster {
        image: &mut image,
        scale,
        antialias: true,
        gpui_coverage: false,
        device_origin,
        source_window:true,
        source_reserved,
        clips: vec![Rect {
            x: device_origin[0] as f32/scale,
            y: device_origin[1] as f32/scale,
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
        resolved = resolve_layers_region(&list.0, scale, font, cache, &mut 0,false,Some(Rect{
            x:0.0,y:0.0,width:width_css as f32,height:height_css as f32}))?;
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
        device_origin:[0,0],
        source_window:false,
        source_reserved:None,
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

/// Decode a PNG with bounded pixel storage.
pub fn decode_png(bytes: &[u8]) -> Result<Rgba8Image, ImageError> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(ImageError::Png("invalid PNG signature"));
    }
    decode_raster_image_bounded(bytes, true, MAX_IMAGE_BYTES)
}

/// Decode one still raster image through the shared image crate. PNG, JPEG, WebP, GIF and BMP
/// are decoded by their upstream image codecs under size limits. Animated formats yield their
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
    decode_image_with_fragment_and_limit(bytes,None,max_bytes)
}

pub fn decode_image_with_fragment_and_limit(bytes:&[u8],fragment:Option<&str>,max_bytes:usize)->Result<Rgba8Image,ImageError> {
    decode_image_with_fragment_and_intrinsic_limit(bytes,fragment,max_bytes).map(|(image,_)|image)
}
/// Preserve actual SVG natural dimensions independently of the raster storage size.
pub fn decode_image_with_fragment_and_intrinsic_limit(bytes:&[u8],fragment:Option<&str>,max_bytes:usize)->Result<(Rgba8Image,Option<lumen_html::object::IntrinsicSize>),ImageError> {
    if SvgViewportImages::is_vector(bytes) {
        decode_svg_image_in_viewport_with_intrinsic(bytes,fragment,None,lumen_html::css::UsedColorScheme::Light,max_bytes).map(|(image,natural,_)|(image,Some(natural)))
    }else{
        decode_raster_image_with_limit(bytes,max_bytes).map(|image|(image,None))
    }
}

/// Rasterize an SVG image without an interactive document or external fetches.
/// Reuses the renderer's capability checks so unsupported drawing is never
/// silently accepted as a successfully decoded image.
pub fn decode_svg_image_with_limit(
    bytes: &[u8],
    max_bytes: usize,
) -> Result<Rgba8Image, ImageError> {
    decode_svg_image_with_fragment_and_limit(bytes,None,max_bytes)
}

pub fn decode_svg_image_with_fragment_and_limit(bytes:&[u8],fragment:Option<&str>,max_bytes:usize)->Result<Rgba8Image,ImageError> {
    decode_svg_image_in_viewport(bytes,fragment,None,max_bytes)
}

pub fn decode_svg_image_in_viewport(bytes:&[u8],fragment:Option<&str>,viewport:Option<(u32,u32)>,max_bytes:usize)->Result<Rgba8Image,ImageError> {
    decode_svg_image_in_viewport_with_intrinsic(bytes,fragment,viewport,lumen_html::css::UsedColorScheme::Light,max_bytes).map(|(image,_,_)|image)
}
fn prepare_svg_image(bytes:&[u8],fragment:Option<&str>,scheme:lumen_html::css::UsedColorScheme)->Result<(lumen_html::session::RenderSession,lumen_html::object::IntrinsicSize,(Option<lumen_html::svg::ViewBox>,lumen_html::svg::AspectRatio)),ImageError> {
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
    session.set_color_scheme_preference(match scheme {lumen_html::css::UsedColorScheme::Light=>lumen_html::css::ColorSchemePreference::Light,lumen_html::css::UsedColorScheme::Dark=>lumen_html::css::ColorSchemePreference::Dark}).map_err(ImageError::Layout)?;
    session.set_svg_fragment(fragment).map_err(ImageError::Layout)?;
    let view=session.svg_fragment().and_then(|fragment|fragment.resolve(session.document()));
    let coordinates=match session.document().kind(root) {
        Ok(lumen_html::NodeKind::Element{attributes,..})=>(
            view.and_then(|view|view.view_box).or_else(||lumen_html::svg::attribute(attributes,"viewBox").and_then(lumen_html::svg::parse_view_box)),
            view.and_then(|view|view.aspect).or_else(||lumen_html::svg::attribute(attributes,"preserveAspectRatio").and_then(lumen_html::svg::parse_aspect_ratio)).unwrap_or_default()),
        _=>return Err(ImageError::Svg("invalid SVG root")),
    };

    let natural=session.embedded_document_intrinsic_size(false,None).map_err(ImageError::Layout)?
        .ok_or(ImageError::Svg("missing SVG intrinsic metadata"))?;
    Ok((session,natural,coordinates))
}
fn decode_svg_image_in_viewport_with_intrinsic(bytes:&[u8],fragment:Option<&str>,viewport:Option<(u32,u32)>,scheme:lumen_html::css::UsedColorScheme,max_bytes:usize)->Result<(Rgba8Image,lumen_html::object::IntrinsicSize,(Option<lumen_html::svg::ViewBox>,lumen_html::svg::AspectRatio)),ImageError> {
    let (mut session,natural,coordinates)=prepare_svg_image(bytes,fragment,scheme)?;
    let (width,height)=viewport.map(|(width,height)|(width as f32,height as f32)).unwrap_or_else(||natural.default_dimensions());
    session.set_svg_image_viewport(viewport.map(|(width,height)|(width as f32,height as f32))).map_err(ImageError::Layout)?;
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
    render_with_font(list, width, height, 1.0, true, font).map(|image|(image,natural,coordinates))
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
    }
    if !lumen_common::mime::image_type_supported(format.to_mime_type()) {
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
    let font_generation=font.generation();
    match session.display_list_with_images(width_css, height_css, font, images) {
        Err(lumen_html::layout::LayoutError::ImagePending) => Ok(None),
        Err(error) => Err(ImageError::Layout(error)),
        // A shaping pass can record first-use font demand. Give the existing
        // owner task pump another turn before committing that stale snapshot.
        Ok(_) if font.generation()!=font_generation=>Ok(None),
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
    use lumen_html::paint::TextShaper;

    #[test]
    fn specification_svg_metadata_free_page_negotiates_default_but_root_uses_owner_preference() {
        use lumen_html::css::{UsedColorScheme,ColorSchemePreference};
        let source=b"<svg xmlns='http://www.w3.org/2000/svg' style='color-scheme:light dark'><style>svg{width:10px;height:5px}@media(prefers-color-scheme:dark){svg{width:20px}}</style></svg>";
        let (mut session,natural,_)=prepare_svg_image(source,None,UsedColorScheme::Dark).unwrap();
        let root=session.document().document_element_at(session.document().root()).unwrap().unwrap();
        let environment=session.media_environment();
        assert_eq!(environment.color_schemes.preference,ColorSchemePreference::Dark);
        assert_eq!(environment.color_schemes.page(),lumen_html::css::UsedScheme{scheme:UsedColorScheme::Light,defaulted:true});
        assert_eq!(session.computed_style(root).unwrap().used_color_scheme(environment).scheme,UsedColorScheme::Dark);
        assert_eq!(natural.width,Some(10.0),"MQ5 queries negotiated page scheme, not unconditionally the user/owner preference");
    }

    #[test]
    fn specification_svg_intrinsic_owner_scheme_metadata_reuses_source_without_rasterizing() {
        use lumen_html::css::UsedColorScheme;
        let source=b"<svg xmlns='http://www.w3.org/2000/svg'><meta xmlns='http://www.w3.org/1999/xhtml' name='color-scheme' content='light dark'/><style>svg{width:10px;height:5px}@media(prefers-color-scheme:dark){svg{width:20px;height:8px}}</style></svg>";
        let cache=SvgViewportImages::default();
        let natural=Arc::new(ImageData{width:10,height:5,pixels:vec![0;200]});
        cache.remember("metrics",lumen_common::bytes::Bytes::owned(Arc::from(&source[..])),None,4096).unwrap();
        cache.attach_natural("metrics",&natural);
        for _ in 0..3 {
            assert_eq!(cache.intrinsic_size(&natural,UsedColorScheme::Light).unwrap().width,Some(10.0));
            let dark=cache.intrinsic_size(&natural,UsedColorScheme::Dark).unwrap();
            assert_eq!((dark.width,dark.height),(Some(20.0),Some(8.0)));
        }
        assert!(cache.rasters.borrow().is_empty(),"intrinsic query uses shared SVG metric preparation, not pixel decode");
        assert_eq!(cache.sources.borrow().len(),1,"light/dark share source/fragment ownership");
        assert_eq!((natural.width,natural.height),(10,5),"owner sizing never mutates admitted natural pixels");
    }

    #[test]
    fn specification_svg_raster_cache_discriminates_owner_used_color_scheme() {
        use lumen_html::css::UsedColorScheme;
        let source=b"<svg xmlns='http://www.w3.org/2000/svg' width='2' height='2' style='color-scheme:light dark'><rect width='2' height='2' style='fill:light-dark(red,blue)'/></svg>";
        let cache=SvgViewportImages::default();
        let natural=Arc::new(ImageData{width:2,height:2,pixels:vec![0;16]});
        cache.remember("scheme",lumen_common::bytes::Bytes::owned(Arc::from(&source[..])),None,4096).unwrap();
        cache.attach_natural("scheme",&natural);
        let light=cache.render("scheme",2.0,2.0,UsedColorScheme::Light,4096).unwrap().unwrap();
        let dark=cache.render("scheme",2.0,2.0,UsedColorScheme::Dark,4096).unwrap().unwrap();
        assert_eq!(&light.pixels[..4],&[255,0,0,255]);
        assert_eq!(&dark.pixels[..4],&[0,0,255,255]);
        assert!(!Arc::ptr_eq(&light,&dark));
        assert!(Arc::ptr_eq(&dark,&cache.render("scheme",2.0,2.0,UsedColorScheme::Dark,4096).unwrap().unwrap()));
        assert!(Arc::ptr_eq(&light,&cache.render("scheme",2.0,2.0,UsedColorScheme::Light,4096).unwrap().unwrap()));
    }

    #[test]
    fn specification_text_shadow_color_extrapolation_reuses_premultiplied_interpolation() {
        let gray = |value| Rgba { r: value, g: value, b: value, a: 255 };
        assert_eq!(interpolate_color(gray(100),gray(200),-0.3),gray(70));
        assert_eq!(interpolate_color(gray(100),gray(200),1.5),gray(250));
        assert_eq!(interpolate_color(gray(100),gray(200),0.5),gray(150));
        let transparent=Rgba {r:0,g:0,b:0,a:0};
        let green=Rgba {r:0,g:128,b:0,a:255};
        assert_eq!(interpolate_color(transparent,green,0.5),Rgba {r:0,g:128,b:0,a:128});
        // Unpremultiply with the actual interpolated alpha; only conversion
        // into the existing RGBA8 carrier clips the final alpha/channel range.
        assert_eq!(interpolate_color(transparent,green,1.5),green);
    }

    #[test]
    fn specification_image_set_vector_density_preserves_real_owner_dimensions_cold_and_warm() {
        let svg=br#"<svg xmlns="http://www.w3.org/2000/svg" width="80" height="40"><rect width="80" height="40" fill="green"/></svg>"#;
        let url=format!("data:image/svg+xml,{}",lumen_common::codec::percent_encode(svg,|byte|!byte.is_ascii_alphanumeric()));
        let markup=format!("<!doctype html><style>body{{margin:0}}div{{width:100px;height:100px;background-repeat:no-repeat;background-image:image-set(url('{url}') 2x type('image/svg+xml'))}}</style><div></div>");
        let document=lumen_html::html::parse(&markup,64).unwrap();
        let mut session=lumen_html::session::RenderSession::new(document);
        let images=FileImages::new(".");
        for _ in 0..2 {
            let list=session.display_list_with_images(100,100,default_font().unwrap(),&images).unwrap();
            let fill=list.0.iter().find_map(|command|match command {lumen_html::paint::Command::FillBackground(fill)=>Some(fill),_=>None}).expect("typed vector background");
            assert_eq!((fill.image_rect.width,fill.image_rect.height),(80.0,40.0));
        }
    }

    #[test]
    fn specification_cross_fade_natural_size_negotiates_real_svg_dimensions_in_context() {
        let ratio_only=br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 50"><rect width="100" height="50" fill="green"/></svg>"#;
        let definite=br#"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="40"><rect width="40" height="40" fill="green"/></svg>"#;
        let url=|source:&[u8]|format!("data:image/svg+xml,{}",lumen_common::codec::percent_encode(source,|byte|!byte.is_ascii_alphanumeric()));
        let mut document=lumen_html::html::parse("<!doctype html><style>body{margin:0}</style><div id=background></div>",64).unwrap();
        let node=lumen_html::selector::get_element_by_id(&document,document.root(),"background").unwrap().unwrap();
        let style=format!("width:100px;height:80px;background-repeat:no-repeat;background-image:cross-fade(75% url('{}'),25% url('{}'))",url(ratio_only),url(definite));
        document.set_attribute(node,"style",&style).unwrap();
        let mut session=lumen_html::session::RenderSession::new(document);
        session.set_canvas_background(None);
        let images=FileImages::new(".");
        let list=session.display_list_with_images(100,80,default_font().unwrap(),&images).unwrap();
        let background=list.0.iter().find_map(|command|match command {lumen_html::paint::Command::FillBackground(background) if matches!(&background.image,lumen_html::paint::BackgroundPaint::CrossFade(_))=>Some(background),_=>None}).expect("actual typed cross-fade source");
        assert_eq!((background.image_rect.width,background.image_rect.height),(85.0,47.5),"ratio-only operand negotiates 100x50 in this positioning area before weighted natural sizing");
        let raster=render(list,100,80,1.0,true).unwrap();
        assert_eq!(&raster.pixels[(20*100+20)*4..(20*100+21)*4],&[0,128,0,255],"actual viewport image carriers feed the shared weighted painter");
    }

    #[test]
    fn specification_border_image_conic_zero_stops_fill_without_border_radius_clipping() {
        let document=lumen_html::html::parse("<!doctype html><style>body{margin:0}#back{width:100px;height:100px;background:red}#target{width:100px;height:100px;background:conic-gradient(rgba(255,0,0,.5) 0 0),conic-gradient(red 0 0);border-radius:40px;border-image:conic-gradient(green 0 0) 1 fill / 10px}</style><div id=back><div id=target></div></div>",64).unwrap();
        let mut session=lumen_html::session::RenderSession::new(document);
        let list=session.display_list(100,100,default_font().unwrap()).unwrap();
        assert!(list.0.iter().any(|command|matches!(command,lumen_html::paint::Command::FillBackground(fill) if matches!(&fill.image,lumen_html::paint::BackgroundPaint::Border(image) if image.fill))));
        let raster=render(list,100,100,1.0,true).unwrap();
        assert!(raster.pixels.chunks_exact(4).all(|pixel| pixel==[0,128,0,255]), "nine-slice fill covers the square, independently of the normal rounded background");
    }

    #[test]
    fn specification_border_image_svg_vector_slices_paint_nine_regions_and_keep_source_identity() {
        use lumen_html::paint::{BackgroundPaint,Command};
        let source=br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 3 3"><rect x="0" y="0" width="1" height="1" fill="green"/><rect x="1" y="0" width="1" height="1" fill="yellow"/><rect x="2" y="0" width="1" height="1" fill="blue"/><rect x="0" y="1" width="1" height="1" fill="cyan"/><rect x="1" y="1" width="1" height="1" fill="red"/><rect x="2" y="1" width="1" height="1" fill="magenta"/><rect x="0" y="2" width="1" height="1" fill="black"/><rect x="1" y="2" width="1" height="1" fill="white"/><rect x="2" y="2" width="1" height="1" fill="orange"/></svg>"#;
        let url=format!("data:image/svg+xml,{}",lumen_common::codec::percent_encode(source,|byte|!byte.is_ascii_alphanumeric()));
        let images=FileImages::new(".");let font=default_font().unwrap();
        for fill in [false,true] {
            let source=format!("<!doctype html><style>body{{margin:0}}div{{width:4px;height:4px;border:4px solid red;border-image:url('{}') 1 {} / 4px stretch}}</style><div></div>",url,if fill{"fill"}else{""});
            let document=lumen_html::html::parse(&source,64).unwrap();let mut session=lumen_html::session::RenderSession::new(document);
            session.set_canvas_background(None);
            let list=session.display_list_with_images(12,12,font,&images).unwrap().clone();
            let carriers=list.0.iter().filter_map(|command|match command {Command::FillBackground(background)=>match &background.image {BackgroundPaint::Border(image)=>Some(image),_=>None},_=>None}).collect::<Vec<_>>();
            assert_eq!(carriers.len(),1,"one retained carrier represents every border tile");
            assert_eq!(carriers[0].source_size,[12.0,12.0]);
            assert_eq!(carriers[0].slices,[4.0;4],"SVG vector units use the actual active viewport scale");
            let replay=session.display_list_with_images(12,12,font,&images).unwrap();
            assert!(replay.0.iter().any(|command|matches!(command,Command::FillBackground(background) if matches!(&background.image,BackgroundPaint::Border(image) if Arc::ptr_eq(image,carriers[0])))));
            let raster=render(&list,12,12,1.0,true).unwrap();
            let pixel=|x:usize,y:usize|&raster.pixels[(y*12+x)*4..(y*12+x+1)*4];
            assert_eq!(pixel(1,1),[0,128,0,255]);assert_eq!(pixel(6,1),[255,255,0,255]);assert_eq!(pixel(10,1),[0,0,255,255]);
            assert_eq!(pixel(1,6),[0,255,255,255]);assert_eq!(pixel(10,6),[255,0,255,255]);
            assert_eq!(pixel(6,6),if fill{[255,0,0,255]}else{[0,0,0,0]});
        }
    }

    #[test]
    fn specification_border_image_pending_procedural_source_uses_canonical_normal_border_fallback() {
        use lumen_html::paint::{BackgroundFill,BackgroundPaint,BackgroundRepeat,BorderImagePaint,BoxBorder,PaintWorkletImage,PaintWorkletRequest};
        let green=Rgba{r:0,g:128,b:0,a:255};
        let border=BorderImagePaint {image:BackgroundPaint::Worklet(Arc::new(PaintWorkletImage{
            request:PaintWorkletRequest{name:Arc::from("pending"),arguments:Arc::from([]),registration_revision:1,width:14.0,height:14.0,properties:Arc::from([])},input_properties:Arc::from([]),pixels:None})),
            fallback:BoxBorder{rect:Rect{x:2.0,y:2.0,width:10.0,height:10.0},radius:0.0,widths:[2.0;4],colors:[green;4],pattern:None,side_patterns:None,corners:None},
            source_size:[14.0;2],slices:[3.0;4],widths:[3.0;4],repeat:[lumen_html::css::BorderImageRepeat::Stretch;2],fill:true};
        let rect=Rect{x:0.0,y:0.0,width:14.0,height:14.0};
        let list=DisplayList(vec![Command::FillBackground(Box::new(BackgroundFill {rect,radius:0.0,corners:None,positioning_rect:rect,image_rect:rect,repeat:[BackgroundRepeat::NoRepeat;2],image:BackgroundPaint::Border(Arc::new(border))}))]);
        let raster=render(&list,14,14,1.0,true).unwrap();
        let pixel=|x:usize,y:usize|&raster.pixels[(y*14+x)*4..(y*14+x+1)*4];
        assert_eq!(pixel(0,0),[0,0,0,0],"pending image does not paint its outset area");
        assert_eq!(pixel(2,2),[0,128,0,255],"normal border is painted by the existing border rasterizer");
        assert_eq!(pixel(7,7),[0,0,0,0],"pending fill does not replace fallback with a transparent border image");
    }

    #[test]
    fn specification_svg_natural_background_size_keeps_dimensions_distinct_from_raster_storage() {
        use lumen_html::layout::ImageResolver;
        use lumen_html::object::IntrinsicSize;
        let source=br#"<svg xmlns="http://www.w3.org/2000/svg"><rect width="100" height="80" fill="green"/></svg>"#;
        let url=format!("data:image/svg+xml,{}#svgView(viewBox(0,0,100,80))",lumen_common::codec::percent_encode(source,|byte|!byte.is_ascii_alphanumeric()));
        let images=FileImages::new(".");
        let natural=match images.resolve(&url) {ImageState::Ready(image)=>image,_=>panic!("real SVG source")};
        assert_eq!((natural.width,natural.height),(188,150),"default raster storage rounds a ratio-only SVG independently of its natural dimensions");
        assert_eq!(images.image_intrinsic_size(&natural,lumen_html::css::UsedColorScheme::Light),Some(IntrinsicSize{width:None,height:None,ratio:Some(1.25)}));
        let same=match images.resolve(&url) {ImageState::Ready(image)=>image,_=>panic!("cached source")};
        assert!(Arc::ptr_eq(&natural,&same));
        let font=default_font().unwrap();
        for (size,expected) in [("auto auto",(100.0,80.0)),("contain",(100.0,80.0)),("cover",(100.0,80.0)),("50px auto",(50.0,40.0)),("auto 40px",(50.0,40.0))] {
            let mut document=lumen_html::html::parse("<!doctype html><style>body{margin:0}</style><div id=background></div>",64).unwrap();
            let node=lumen_html::selector::get_element_by_id(&document,document.root(),"background").unwrap().unwrap();
            document.set_attribute(node,"style",&format!("width:100px;height:80px;background-image:url('{}');background-size:{};background-repeat:no-repeat",url,size)).unwrap();
            let mut session=lumen_html::session::RenderSession::new(document);
            session.set_canvas_background(None);
            let list=session.display_list_with_images(100,80,font,&images).unwrap().clone();
            let background=list.0.iter().find_map(|command|match command {lumen_html::paint::Command::FillBackground(background) if matches!(&background.image,lumen_html::paint::BackgroundPaint::Image(_))=>Some(background),_=>None}).expect("actual background image command");
            assert_eq!((background.image_rect.width,background.image_rect.height),expected,"{size} resolves against natural metadata");
            let raster=render(&list,100,80,1.0,true).unwrap();
            for y in 0..expected.1 as usize {for x in 0..expected.0 as usize {
                assert_eq!(&raster.pixels[(y*100+x)*4..(y*100+x+1)*4],&[0,128,0,255],"{size}: no fractional left-edge alpha from rounded decode dimensions");
            }}
            let replay=session.display_list_with_images(100,80,font,&images).unwrap();
            let current=match &background.image {lumen_html::paint::BackgroundPaint::Image(image)=>image,_=>unreachable!()};
            assert!(replay.0.iter().any(|command|matches!(command,lumen_html::paint::Command::FillBackground(background) if matches!(&background.image,lumen_html::paint::BackgroundPaint::Image(image) if Arc::ptr_eq(image,current)))));
        }
        let one_axis=br#"<svg xmlns="http://www.w3.org/2000/svg" width="40" viewBox="0 0 100 80"/>"#;
        let (_,intrinsic)=decode_image_with_fragment_and_intrinsic_limit(one_axis,None,1024*1024).unwrap();
        assert_eq!(intrinsic,Some(IntrinsicSize{width:Some(40.0),height:None,ratio:Some(1.25)}));
        assert_eq!(intrinsic.unwrap().default_dimensions_in((100.0,80.0)),(40.0,32.0));
        let (_,intrinsic)=decode_image_with_fragment_and_intrinsic_limit(source,None,1024*1024).unwrap();
        assert_eq!(intrinsic,Some(IntrinsicSize::default()),"unselected source does not inherit another fragment's ratio");
    }

    #[test]
    fn specification_svg_viewport_concrete_image_box_preserves_meet_and_reuses_raster() {
        let source=br#"<svg xmlns="http://www.w3.org/2000/svg" width="4" viewBox="0 0 4 2"><rect width="2" height="2" fill="red"/><rect x="2" width="2" height="2" fill="blue"/></svg>"#;
        let cache=SvgViewportImages::default();
        let bytes=lumen_common::bytes::Bytes::owned(Arc::from(&source[..]));
        let cost=SvgViewportImages::source_cost("actual",source.len(),Some("svgView(viewBox(2,0,2,2))")).unwrap();
        assert!(cache.remember("actual",bytes.clone(),Some("svgView(viewBox(2,0,2,2))"),cost-1).is_err());
        cache.remember("actual",bytes,Some("svgView(viewBox(2,0,2,2))"),cost).unwrap();
        let first=cache.render("actual",8.0,4.0,lumen_html::css::UsedColorScheme::Light,4096).unwrap().unwrap();
        assert_eq!((first.width,first.height),(8,4));
        assert_eq!(&first.pixels[..4],&[255,0,0,255]);
        assert_eq!(&first.pixels[8..12],&[0,0,255,255]);
        assert_eq!(&first.pixels[28..32],&[0,0,0,0]);
        let same=cache.render("actual",8.0,4.0,lumen_html::css::UsedColorScheme::Light,4096).unwrap().unwrap();
        assert!(Arc::ptr_eq(&first,&same));
        let square=cache.render("actual",4.0,4.0,lumen_html::css::UsedColorScheme::Light,4096).unwrap().unwrap();
        assert!(square.pixels.chunks_exact(4).all(|pixel|pixel==[0,0,255,255]));
        assert!(cache.render("actual",100.0,100.0,lumen_html::css::UsedColorScheme::Light,4096).unwrap().is_err());
        let none=decode_svg_image_in_viewport(source,Some("svgView(viewBox(2,0,2,2);preserveAspectRatio(none))"),Some((8,4)),4096).unwrap();
        assert!(none.pixels.chunks_exact(4).all(|pixel|pixel==[0,0,255,255]));

        let images=FileImages::new(".");
        let url=format!("data:image/svg+xml,{}#svgView(viewBox(2,0,2,2))",lumen_common::codec::percent_encode(source,|byte|!byte.is_ascii_alphanumeric()));
        let mut document=lumen_html::html::parse("<!doctype html><style>body{margin:0}img{display:block;width:8px;height:4px}</style><img id=actual>",64).unwrap();
        let node=lumen_html::selector::get_element_by_id(&document,document.root(),"actual").unwrap().unwrap();
        document.set_attribute(node,"src",&url).unwrap();
        let mut session=lumen_html::session::RenderSession::new(document);
        session.set_canvas_background(None);
        let font=default_font().unwrap();
        let list=session.display_list_with_images(8,4,font,&images).unwrap();
        let painted=list.0.iter().find_map(|command|match command {lumen_html::paint::Command::Image{rect,image} if rect.width==8.0 && rect.height==4.0=>Some(image.clone()),_=>None}).expect("actual replaced image viewport");
        assert_eq!(painted.pixels,first.pixels);
        let natural=match lumen_html::layout::ImageResolver::resolve(&images,&url) {ImageState::Ready(image)=>image,_=>panic!("actual loaded source")};
        let stale_selected="data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='8'%20height='4'%3E%3Crect%20width='8'%20height='4'%20fill='green'/%3E%3C/svg%3E";
        assert!(matches!(lumen_html::layout::ImageResolver::resolve(&images,stale_selected),ImageState::Ready(_)));
        let still_current=lumen_html::layout::ImageResolver::resolve_viewport(&images,Some(node),"",stale_selected,&natural,8.0,4.0,lumen_html::css::UsedColorScheme::Light).unwrap();
        assert!(matches!(still_current,ImageState::Ready(image) if image.pixels==first.pixels));

        let replay=session.display_list_with_images(8,4,font,&images).unwrap();
        assert!(replay.0.iter().any(|command|matches!(command,lumen_html::paint::Command::Image{image,..} if Arc::ptr_eq(image,&painted))));
        session.document_mut().set_attribute(node,"style","display:block;width:4px;height:4px").unwrap();
        let resized=session.display_list_with_images(8,4,font,&images).unwrap();
        let square_paint=resized.0.iter().find_map(|command|match command {lumen_html::paint::Command::Image{rect,image} if rect.width==4.0 && rect.height==4.0=>Some(image),_=>None}).unwrap();
        assert!(square_paint.pixels.chunks_exact(4).all(|pixel|pixel==[0,0,255,255]));
        let mut document=lumen_html::html::parse("<!doctype html><style>body{margin:0}</style><div id=background></div>",64).unwrap();
        let node=lumen_html::selector::get_element_by_id(&document,document.root(),"background").unwrap().unwrap();
        document.set_attribute(node,"style",&format!("width:8px;height:4px;background-image:url('{}');background-size:8px 4px;background-repeat:no-repeat",url)).unwrap();
        let mut session=lumen_html::session::RenderSession::new(document);
        session.set_canvas_background(None);
        let list=session.display_list_with_images(8,4,font,&images).unwrap();
        assert!(list.0.iter().any(|command|match command {
            lumen_html::paint::Command::FillBackground(background)=>match &background.image {lumen_html::paint::BackgroundPaint::Image(image)=>image.width==8 && image.height==4 && image.pixels==first.pixels,_=>false},_=>false
        }));


    }

    #[test]
    fn specification_svg_view_raster_and_intrinsic_use_the_same_selected_view() {
        let source = br#"<svg xmlns="http://www.w3.org/2000/svg" width="4" viewBox="0 0 4 2"><view id="right" viewBox="2 0 2 2"/><rect width="2" height="2" fill="red"/><rect x="2" width="2" height="2" fill="blue"/></svg>"#;
        let whole = decode_svg_image_with_fragment_and_limit(source, None, 128).unwrap();
        assert_eq!((whole.width,whole.height),(4,2));
        assert_eq!(&whole.pixels[..4], &[255,0,0,255]);
        let named = decode_svg_image_with_fragment_and_limit(source, Some("right"), 128).unwrap();
        let specified = decode_svg_image_with_fragment_and_limit(source, Some("svgView(viewBox(2,0,2,2))"), 128).unwrap();
        assert_eq!((named.width,named.height),(4,4));
        assert_eq!(named.pixels,specified.pixels);
        assert!(named.pixels.chunks_exact(4).all(|pixel|pixel==[0,0,255,255]));
        let escaped = decode_svg_image_with_fragment_and_limit(source, Some("svgView%28viewBox%282%2C0%2C2%2C2%29%29"), 128).unwrap();
        assert_eq!(escaped.pixels,named.pixels);
        let invalid = decode_svg_image_with_fragment_and_limit(source, Some("svgView(viewBox(2,0,2,2);viewBox(0,0,1,1))"), 128).unwrap();
        assert_eq!(invalid.pixels,whole.pixels);
        let disabled = decode_svg_image_with_fragment_and_limit(source, Some("svgView(viewBox(0,0,0,2))"), 4096).unwrap();
        assert!(disabled.pixels.chunks_exact(4).all(|pixel|pixel==[0,0,0,0]));
        assert!(matches!(decode_svg_image_with_fragment_and_limit(source,Some("right"), 63),Err(ImageError::TooLarge)));
    }

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
            assert_eq!(&image.pixels[..4], &[127, 127, 255, 255], "display:{display}");
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
            Command::PushLayer { svg_clip: None,
                filters: None,
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
    fn specification_svg_source_draw_uses_complete_native_scratch_budget() {
        use lumen_html::paint::{SvgPaint,SvgGradient,SvgGradientKind,SvgGradientUnits,SvgGradientStop,SvgClip,SvgClipShape};
        let data:Arc<str>=Arc::from(format!("{}M2 14 Q2 -3 16 2 C40 2 20 24 8 19 Z"," ".repeat(4096)));
        let stops:Arc<[SvgGradientStop]>=Arc::from([
            SvgGradientStop{offset:0.0,color:Rgba{r:255,g:0,b:0,a:191}},SvgGradientStop{offset:0.3,color:Rgba{r:0,g:255,b:0,a:255}},
            SvgGradientStop{offset:0.8,color:Rgba{r:0,g:0,b:255,a:128}},SvgGradientStop{offset:1.0,color:Rgba{r:255,g:0,b:255,a:255}}]);
        for kind in [None,Some(SvgGradientKind::Linear{start:[0.0,0.0],end:[24.0,0.0]}),
            Some(SvgGradientKind::Radial{start:[4.0,4.0],end:[12.0,12.0],start_radius:0.0,end_radius:16.0}),
            Some(SvgGradientKind::Radial{start:[4.0,4.0],end:[12.0,12.0],start_radius:8.0,end_radius:8.0})] {
            let paint=kind.map_or(SvgPaint::Color(Rgba{r:255,g:0,b:0,a:191}),|kind|SvgPaint::Gradient(Arc::new(SvgGradient{
                opacity:0.75,units:SvgGradientUnits::UserSpaceOnUse,transform:Affine::IDENTITY,kind,stops:stops.clone()})));
            for scale in [1.0,1.25,2.0] {for width in [0.125,4.0] {
                let transform=Affine{a:1.0,b:0.125,c:0.25,d:1.0,e:0.625,f:0.125};
                let clips=Arc::from([SvgClip{units:SvgGradientUnits::UserSpaceOnUse,transform:Affine::IDENTITY,
                    shapes:Arc::from([SvgClipShape{data:Arc::from("M0 0H24V24H0Z"),transform:Affine::IDENTITY,fill_rule:PaintSvgFillRule::NonZero},
                        SvgClipShape{data:Arc::from("M24 0L32 12L24 24Z"),transform:Affine::IDENTITY,fill_rule:PaintSvgFillRule::NonZero}])}]);
                let list=DisplayList(vec![Command::SvgPath{bounds:Rect{x:0.0,y:0.0,width:40.0,height:32.0},data:data.clone(),transform,
                    fill:Some(paint.clone()),stroke:Some(paint.clone()),stroke_width:width,fill_rule:PaintSvgFillRule::NonZero,clips}]);
                let region=FilterPixelRegion::from_rect(Rect{x:0.0,y:0.0,width:40.0,height:32.0},scale).unwrap();
                let render=|reserved|render_scaled_region_at(&list,region.width,region.height,scale,None,&mut GlyphCache::default(),[0,0],reserved);
                let expected=render(None).unwrap();let actual=render(Some(MAX_LAYER_BYTES-128*1024)).unwrap();
                assert_exact_spatial_pixels(&actual,&expected,&format!("complete source scratch kind={kind:?} scale={scale} stroke={width}"));
                let pixels=region.width as usize*region.height as usize*4;
                assert!(matches!(render(Some(MAX_LAYER_BYTES-pixels-64)),Err(ImageError::TooLarge)),"rejection before source raster publication");
            }}
        }
    }

    #[test]
    fn specification_svg_source_clip_scan_charges_actual_geometry_and_union_scratch() {
        let first=canvas::parse_svg_path("M0 2Q16 -8 24 2L24 20H0Z").unwrap().path.unwrap();
        let second=canvas::parse_svg_path("M16 0L32 8L16 24Z").unwrap().path.unwrap();
        for scale in [1.0,1.25,2.0] {for width in [64,9001] {
            let transform=tiny_skia::Transform::from_row(scale,0.125*scale,0.25*scale,scale,if width==64{0.625}else{8184.625},0.125);
            let paths=[(&first,tiny_skia::FillRule::Winding,transform),(&second,tiny_skia::FillRule::EvenOdd,transform)];
            let mut expected=canvas::CanvasSurface::new(width,64).unwrap();let mut actual=canvas::CanvasSurface::new(width,64).unwrap();
            for _ in 0..2 {expected.clip_paths_union(&paths).unwrap();actual.clip_paths_union_bounded(&paths,MAX_LAYER_BYTES).unwrap();}
            assert_eq!(actual.clip_alpha().unwrap(),expected.clip_alpha().unwrap(),"native clip union/intersection width={width} scale={scale}");
        }}
        let encoded=format!("{}M0 0H4V4H0Z"," ".repeat(4096));
        let clip=lumen_html::paint::SvgClip{units:lumen_html::paint::SvgGradientUnits::UserSpaceOnUse,transform:Affine::IDENTITY,
            shapes:Arc::from([lumen_html::paint::SvgClipShape{data:Arc::from(encoded),transform:Affine::IDENTITY,fill_rule:PaintSvgFillRule::NonZero}])};
        let mut actual=canvas::CanvasSurface::new(8,8).unwrap();
        apply_svg_clips(&mut actual,&[clip],Affine::IDENTITY,Affine::IDENTITY,Affine::IDENTITY,Some(MAX_LAYER_BYTES-4096)).unwrap();
        assert!(actual.clip_alpha().unwrap().iter().any(|alpha|*alpha!=0),"borrowed whitespace does not allocate geometry");
        let mut rejected=canvas::CanvasSurface::new(64,64).unwrap();
        assert!(matches!(rejected.clip_paths_union_bounded(&[(&first,tiny_skia::FillRule::Winding,tiny_skia::Transform::identity())],16),Err(ImageError::TooLarge)));
        assert!(rejected.clip_alpha().is_none(),"failed clipping never publishes a partial contour");
    }

    #[test]
    fn specification_svg_bounded_scan_matches_native_fill_stroke_gradient_and_tiler() {
        let path=canvas::parse_svg_path("M2 14 Q2 -3 16 2 C40 2 20 24 8 19 Z").unwrap().path.unwrap();
        for width in [64,9001] {for aa in [false,true] {for scale in [1.0,1.25,2.0] {
            let transform=tiny_skia::Transform::from_row(scale,0.125*scale,0.25*scale,scale,if width==64{0.625}else{8184.625},0.125);
            for gradient in [false,true] {for stroke in [None,Some(0.125),Some(4.0)] {
                let mut paint=tiny_skia::Paint{anti_alias:aa,..tiny_skia::Paint::default()};
                if gradient {
                    paint.shader=tiny_skia::LinearGradient::new(tiny_skia::Point::from_xy(0.0,0.0),tiny_skia::Point::from_xy(24.0,0.0),
                        vec![tiny_skia::GradientStop::new(0.0,tiny_skia::Color::from_rgba8(255,0,0,191)),
                            tiny_skia::GradientStop::new(0.3,tiny_skia::Color::from_rgba8(0,255,0,255)),
                            tiny_skia::GradientStop::new(0.8,tiny_skia::Color::from_rgba8(0,0,255,128)),
                            tiny_skia::GradientStop::new(1.0,tiny_skia::Color::from_rgba8(255,0,255,255))],
                        tiny_skia::SpreadMode::Pad,tiny_skia::Transform::identity()).unwrap();
                }else{paint.set_color_rgba8(255,0,0,191);}
                let mut reference=tiny_skia::Pixmap::new(width,64).unwrap();
                let mut actual=tiny_skia::Pixmap::new(width,64).unwrap();
                if let Some(line_width)=stroke {
                    let line=tiny_skia::Stroke{width:line_width,..tiny_skia::Stroke::default()};
                    reference.stroke_path(&path,&paint,&line,transform,None);
                    actual.stroke_path_bounded(&path,&paint,&line,transform,None,256*1024).unwrap();
                }else {
                    reference.fill_path(&path,&paint,tiny_skia::FillRule::Winding,transform,None);
                    actual.fill_path_bounded(&path,&paint,tiny_skia::FillRule::Winding,transform,None,256*1024).unwrap();
                }
                if actual.data()!=reference.data() {
                    let first=actual.data().chunks_exact(4).zip(reference.data().chunks_exact(4)).enumerate().find(|(_, (a,b))|a!=b);
                    let differing=actual.data().chunks_exact(4).zip(reference.data().chunks_exact(4)).filter(|(a,b)|a!=b).count();
                    panic!("bounded native scan width={width} aa={aa} scale={scale} gradient={gradient} stroke={stroke:?} differing={differing} first={first:?}");
                }
            }}
        }}}
        let mut builder=tiny_skia::PathBuilder::new();
        for index in 0..1024 {let x=(index%32)as f32;let y=(index/32)as f32;builder.move_to(x,y);builder.line_to(x+0.625,y);builder.line_to(x+0.625,y+0.625);builder.close();}
        let path=builder.finish().unwrap();let paint=tiny_skia::Paint::default();
        let mut output=tiny_skia::Pixmap::new(64,64).unwrap();let prior=output.data().to_vec();
        assert!(output.fill_path_bounded(&path,&paint,tiny_skia::FillRule::Winding,tiny_skia::Transform::identity(),None,2048).is_err());
        assert_eq!(output.data(),prior,"edge budget rejects before any source publication");
        assert!(output.stroke_path_bounded(&path,&paint,&tiny_skia::Stroke{width:4.0,..tiny_skia::Stroke::default()},tiny_skia::Transform::identity(),None,64).is_err());
        assert_eq!(output.data(),prior,"outline budget rejects before any source publication");
    }

    #[test]
    fn specification_svg_stroke_uses_initial_miter_geometry_and_canvas_keeps_its_own_default() {
        let data="M20 60L30 30L32 60Z";
        let path=lumen_common::svg_path::parse_svg_path(data).unwrap().path.unwrap();
        for scale in [1.0,1.25,2.0] {for origin in [0.0,0.125,0.625] {
            let transform=Affine{e:origin,f:origin,..Affine::IDENTITY};
            let command=Command::SvgPath{bounds:Rect{x:0.0,y:0.0,width:80.0,height:80.0},data:Arc::from(data),
                transform,fill:None,stroke:Some(lumen_html::paint::SvgPaint::Color(Rgba{r:0,g:0,b:0,a:255})),
                stroke_width:8.0,fill_rule:PaintSvgFillRule::NonZero,clips:Arc::from([])};
            let actual=render(&DisplayList(vec![command]),80,80,scale,true).unwrap();
            let mut reference=canvas::CanvasSurface::new(actual.width,actual.height).unwrap();
            assert_eq!(reference.state().line.miter_limit,10.0,"Canvas initial value remains independent");
            reference.state_mut().transform=tiny_skia::Transform::from_row(scale,0.0,0.0,scale,origin*scale,origin*scale);
            {
                let state=reference.state_mut();state.line=tiny_skia::Stroke{width:8.0,..tiny_skia::Stroke::default()};
                state.miter_limit=f64::from(state.line.miter_limit);
            }
            reference.stroke_path(&path).unwrap();
            assert_exact_spatial_pixels(&actual,&reference.snapshot(),&format!("SVG canonical miter scale={scale} origin={origin}"));
        }}
        let svg=path.stroke_bounded(&tiny_skia::Stroke{width:8.0,..tiny_skia::Stroke::default()},1.0,MAX_LAYER_BYTES).unwrap().unwrap();
        let canvas=path.stroke_bounded(&tiny_skia::Stroke{width:8.0,miter_limit:10.0,..tiny_skia::Stroke::default()},1.0,MAX_LAYER_BYTES).unwrap().unwrap();
        assert_ne!(svg,canvas,"the acute join meaningfully distinguishes the two initial geometries");
    }

    #[test]
    fn specification_svg_unfiltered_object_bounds_include_native_glyph_cells_and_geometry() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let run=lumen_html::paint::TextShaper::shape(&font," ",20.0).unwrap();assert_eq!(run.glyphs.len(),1);
        let glyph=run.glyphs[0];let cell=font.glyph_cell_bounds(glyph.face,glyph.id,20.0*glyph.size_scale).unwrap();
        assert!(cell.width>0.0&&cell.height>0.0,"spaces have object geometry despite no visible ink");
        let owner=Affine{e:5.0,f:7.0,..Affine::IDENTITY};
        let commands=[Command::PushTransform(Affine{e:3.0,f:2.0,..Affine::IDENTITY}),
            Command::GlyphRun{origin_x:1.0,baseline_y:20.0,size:20.0,color:Rgba{r:0,g:0,b:0,a:0},glyphs:run.glyphs},Command::PopTransform];
        let bounds=svg_source_object_box(&commands,owner,Some(&font),0).unwrap();
        assert_eq!(bounds,Affine{a:cell.width,d:cell.height,e:-1.0+glyph.x+cell.x,f:15.0-glyph.y+cell.y,..Affine::IDENTITY},"native glyph cells remain in owner space through nested transforms");
        let path=Command::SvgPath{bounds:Rect{x:0.0,y:0.0,width:1.0,height:1.0},data:Arc::from("M0 0Q10 20 20 0"),
            transform:Affine::IDENTITY,fill:None,stroke:None,stroke_width:100.0,fill_rule:PaintSvgFillRule::NonZero,clips:Arc::from([])};
        assert_eq!(svg_source_object_box(&[path.clone()],Affine::IDENTITY,None,0).unwrap(),Affine{a:20.0,d:10.0,..Affine::IDENTITY},"tight geometry excludes control-box excess, crop, stroke and absent paint");
        assert_eq!(svg_source_object_box(&[path],Affine::IDENTITY,None,MAX_LAYER_BYTES),Err(ImageError::TooLarge),"source path workspace is admitted before parsing");
        assert_eq!(svg_source_object_box(&[Command::PushTransform(Affine::IDENTITY)],Affine::IDENTITY,None,MAX_LAYER_BYTES),Err(ImageError::TooLarge),"transform stack is admitted before growth");
    }

    #[test]
    fn specification_svg_object_bounds_charge_geometry_not_encoded_length() {
        let data=format!("M0 0{}H20V10H0Z"," ".repeat(4096));
        let command=Command::SvgPath{bounds:Rect{x:0.0,y:0.0,width:1.0,height:1.0},data:Arc::from(data),
            transform:Affine{a:2.0,d:3.0,e:5.0,f:7.0,..Affine::IDENTITY},fill:None,stroke:None,
            stroke_width:0.0,fill_rule:PaintSvgFillRule::NonZero,clips:Arc::from([])};
        assert_eq!(svg_source_object_box(&[command.clone()],Affine::IDENTITY,None,MAX_LAYER_BYTES-2048).unwrap(),
            Affine{a:40.0,d:30.0,e:5.0,f:7.0,..Affine::IDENTITY},"borrowed encoded whitespace needs no geometry allocation");
        assert_eq!(svg_source_object_box(&[command],Affine::IDENTITY,None,MAX_LAYER_BYTES-1),Err(ImageError::TooLarge));
    }

    #[test]
    fn specification_svg_css_owner_clip_is_a_single_composited_group() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let source="<style>body{margin:0}</style><svg width=30 height=20><defs><clipPath id=c clipPathUnits='objectBoundingBox'><rect width='.5' height='1'/></clipPath></defs><g clip-path='url(#c)'><rect x=4 y=4 width=2 height=2 fill='red'/><rect x=8 y=4 width=2 height=2 fill='none'/></g></svg>";
        let reference="<style>body{margin:0}</style><svg width=30 height=20><rect x=4 y=4 width=2 height=2 fill='red'/></svg>";
        assert_eq!(render_html_with_font(source,40,24,1.0,&font).unwrap(),render_html_with_font(reference,40,24,1.0,&font).unwrap(),"an unpainted child extends the container object box before clipping");
        for transform in ["","transform='translate(.5 .25)'","transform='scale(2)'"] {
            let source=format!("<style>body{{margin:0}}</style><svg width=20 height=12><defs><clipPath id=c><rect x=4.5 y=4 width=1.5 height=2/></clipPath></defs><g {transform} clip-path='url(#c)' filter='invert()'><rect x=4 y=4 width=2 height=2 fill='magenta'/><rect x=4 y=4 width=2 height=2 fill='cyan'/></g></svg>");
            let reference=source.replace("filter='invert()'","filter='none'").replace("fill='magenta'","fill='lime'").replace("fill='cyan'","fill='red'");
            for scale in [1.0,2.0] {
                assert_eq!(render_html_with_font(&source,40,24,scale,&font).unwrap(),render_html_with_font(&reference,40,24,scale,&font).unwrap(),"SVG group owns filter then clip: {transform},scale={scale}");
            }
        }
    }

    #[test]
    fn specification_svg_owner_clip_filters_order_overlap_and_unfiltered_object_box() {
        use lumen_common::filter::{FilterOperation as F,DropShadowFilter};
        use lumen_common::color::Color;
        use lumen_html::paint::{SvgClip,SvgClipShape,SvgLayerClip,SvgGradientUnits,SvgPaint};
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let viewport=Rect{x:0.0,y:0.0,width:10.0,height:10.0};
        let clip=|data:&str,units,transform|Arc::new(SvgLayerClip{transform,clip:Arc::new(SvgClip{
            units,transform:Affine::IDENTITY,shapes:Arc::from([SvgClipShape{data:Arc::from(data),transform:Affine::IDENTITY,fill_rule:PaintSvgFillRule::NonZero}])})});
        let path=|data:&str,color|Command::SvgPath{bounds:viewport,data:Arc::from(data),transform:Affine::IDENTITY,
            fill:Some(SvgPaint::Color(color)),stroke:None,stroke_width:0.0,fill_rule:PaintSvgFillRule::NonZero,clips:Arc::from([])};
        let layer=|filters:Option<Arc<[F]>>,svg_clip|Command::PushLayer{svg_clip,filters,corners:None,rect:viewport,radius:0.0,opacity:1.0,clip:false};
        let render=|commands| {
            let resolved=rasterize_layers(&DisplayList(commands),1.0,&font,&mut GlyphCache::default()).unwrap();
            render_scaled_region(&resolved,10,10,1.0,Some(&font),&mut GlyphCache::default()).unwrap()
        };
        let red=Rgba{r:255,g:0,b:0,a:255};let blue=Rgba{r:0,g:0,b:255,a:255};
        for owner_clip in [false,true] {
            let mask=Some(clip("M2 2H3V3H2Z",SvgGradientUnits::UserSpaceOnUse,Affine::IDENTITY));
            let filter=Some(Arc::from([F::Blur(0.5)]));
            let source=path("M3 2H4V3H3Z",red);
            let commands=if owner_clip {vec![layer(filter,mask),source,Command::PopLayer]}
                else {vec![layer(filter,None),layer(None,mask),source,Command::PopLayer,Command::PopLayer]};
            let image=render(commands);
            assert_eq!(&image.pixels[(2*10+2)*4..(2*10+2)*4+4],if owner_clip{&[255,0,0,21]}else{&[0,0,0,0]},
                "an owner clip follows blur; a descendant clip belongs to SourceGraphic");
        }
        let image=render(vec![layer(None,Some(clip("M4.5 4H5V5H4.5Z",SvgGradientUnits::UserSpaceOnUse,Affine::IDENTITY))),
            path("M4 4H5V5H4Z",red),path("M4 4H5V5H4Z",blue),Command::PopLayer]);
        assert_eq!(&image.pixels[(4*10+4)*4..(4*10+4)*4+4],&[0,0,255,128],"fractional owner coverage applies once to composited overlap");
        let shadow=F::DropShadow(Arc::new(DropShadowFilter{offset:[3.0,0.0],sigma:0.0,color:Color::rgba8([0,0,255,255])}));
        let image=render(vec![layer(Some(Arc::from([shadow])),Some(clip("M0 0H.5V1H0Z",SvgGradientUnits::ObjectBoundingBox,Affine::IDENTITY))),
            path("M4 4H6V6H4Z",red),Command::PopLayer]);
        assert_eq!(&image.pixels[(4*10+4)*4..(4*10+4)*4+4],&[255,0,0,255]);
        assert_eq!(&image.pixels[(4*10+5)*4..(4*10+5)*4+4],&[0,0,0,0],"object bounds exclude ordered filter outsets");
        assert_eq!(&image.pixels[(4*10+7)*4..(4*10+7)*4+4],&[0,0,0,0],"owner clips consume the shadow result too");
    }

    #[test]
    fn specification_spatial_filter_svg_viewport_crop_is_source_optimization_not_owner_clip() {
        use lumen_common::filter::FilterOperation as F;
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let viewport=Rect{x:0.0,y:0.0,width:10.0,height:10.0};
        let path=Command::SvgPath{bounds:viewport,data:Arc::from("M10 4H11V5H10Z"),
            transform:Affine::IDENTITY,fill:Some(lumen_html::paint::SvgPaint::Color(Rgba{r:255,g:0,b:0,a:255})),
            stroke:None,stroke_width:0.0,fill_rule:PaintSvgFillRule::NonZero,clips:Arc::from([])};
        let layer=|filters:Option<Arc<[F]>>,clip:bool|Command::PushLayer{ svg_clip: None,filters,corners:None,rect:viewport,radius:0.0,opacity:1.0,clip};
        let make=|root_filter:bool| {
            let filter=layer(Some(Arc::from([F::Blur(0.5)])),false);
            let clip=layer(None,true);
            DisplayList(if root_filter {vec![filter,clip,path.clone(),Command::PopLayer,Command::PopLayer]}
                else {vec![clip,filter,path.clone(),Command::PopLayer,Command::PopLayer]})
        };
        for root_filter in [false,true] {
            let resolved=rasterize_layers(&make(root_filter),1.0,&font,&mut GlyphCache::default()).unwrap();
            let image=render_scaled_region(&resolved,10,10,1.0,Some(&font),&mut GlyphCache::default()).unwrap();
            assert_eq!(&image.pixels[(4*10+9)*4..(4*10+9)*4+4],if root_filter {&[0,0,0,0]}else{&[255,0,0,21]},
                "descendant filter captures outside viewport ink; root filter captures viewport-clipped contents");
        }
        // Existing direct SVG paths continue to obey their crop optimization.
        let direct=render_scaled_region(&DisplayList(vec![path]),10,10,1.0,Some(&font),&mut GlyphCache::default()).unwrap();
        assert!(direct.pixels.iter().all(|byte|*byte==0));
    }

    #[test]
    fn specification_spatial_filter_html_source_ink_survives_viewport_culling_and_ancestor_clip() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let make=|ancestor_clip:bool,own_clip:bool|format!(
            "<style>html,body{{margin:0}}#ancestor{{position:relative;width:10px;height:10px;overflow:{}}}#host{{position:absolute;left:10px;top:4px;width:1px;height:1px;filter:blur(1px);overflow:{}}}#source{{width:2px;height:1px;background:red}}</style><div id=ancestor><div id=host><div id=source></div></div></div>",
            if ancestor_clip{"hidden"}else{"visible"},if own_clip{"hidden"}else{"visible"});
        for ancestor_clip in [false,true] {
        let mut unclipped_source=None;
        for own_clip in [false,true] {
            let source=make(ancestor_clip,own_clip);
            let document=lumen_html::html::parse(&source,64).unwrap();
            let list=lumen_html::layout::display_list(&document,10,10,&font).unwrap();
            assert!(list.0.iter().any(|command|matches!(command,Command::FillRect{color,..}if *color==Rgba{r:255,g:0,b:0,a:255})),"offscreen source participates in the filter: ancestor={ancestor_clip},own={own_clip}");
            let resolved=rasterize_layers(&list,1.0,&font,&mut GlyphCache::default()).unwrap();
            let image=render_scaled_region(&resolved,10,10,1.0,Some(&font),&mut GlyphCache::default()).unwrap();
            let viewport=Rect{x:0.0,y:0.0,width:10.0,height:10.0};
            let source_rect=Rect{x:10.0,y:4.0,width:1.0,height:1.0};
            let mut reference=vec![Command::FillRect{rect:viewport,color:Rgba{r:255,g:255,b:255,a:255}}];
            if ancestor_clip {reference.push(Command::PushClip(viewport));}
            reference.push(Command::PushLayer{svg_clip:None,filters:Some(Arc::from([
                lumen_common::filter::FilterOperation::Blur(1.0)])),corners:None,
                rect:source_rect,radius:0.0,opacity:1.0,clip:false});
            // Overflow clips descendant content inside SourceGraphic. Unlike
            // an element clip-path, it does not clip the final filter result.
            if own_clip {reference.push(Command::PushClip(source_rect));}
            reference.push(Command::FillRect{rect:Rect{width:2.0,..source_rect},color:Rgba{r:255,g:0,b:0,a:255}});
            if own_clip {reference.push(Command::PopClip);}
            reference.push(Command::PopLayer);
            if ancestor_clip {reference.push(Command::PopClip);}
            let expected=render_with_font(&DisplayList(reference),10,10,1.0,true,&font).unwrap();
            assert_exact_spatial_pixels(&image,&expected,&format!("offscreen source ancestor={ancestor_clip}/own={own_clip}"));
            let sample=&image.pixels[(4*10+9)*4..(4*10+9)*4+4];
            assert!(sample[0]==255 && sample[1]<255 && sample[2]<255 && sample[3]==255,
                "offscreen descendant ink produces a visible halo after its owner's content clip: {sample:?}");
            if own_clip {assert_ne!(Some(&image.pixels),unclipped_source.as_ref(),
                "overflow clips real source ink extending past the one-pixel host, rather than clipping the filter result");}
            else {unclipped_source=Some(image.pixels.clone());}
        }}
        let source="<style>html,body{margin:0}#host{position:absolute;left:4px;top:4px;width:1px;height:1px;filter:drop-shadow(3px 0 blue);background:red}</style><div id=host></div>";
        let actual=render_html_with_font(source,12,10,1.0,&font).unwrap();
        let reference="<style>html,body{margin:0}div{position:absolute;top:4px;width:1px;height:1px;background:red}i{position:absolute;left:7px;top:4px;width:1px;height:1px;background:blue}</style><i></i><div style='left:4px'></div>";
        assert_eq!(actual,render_html_with_font(reference,12,10,1.0,&font).unwrap(),"actual drop-shadow output is offset below its source");
    }

    #[test]
    fn specification_spatial_filter_source_capture_gaussian_and_ordered_shadow() {
        use lumen_common::{filter::{FilterOperation as F,ColorFilter as C,DropShadowFilter},color::Color};
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let resolve=|filters:Vec<F>,rect:Rect,scale:f32,color:Rgba| {
            let list=DisplayList(vec![Command::PushLayer{ svg_clip: None,filters:Some(Arc::from(filters)),corners:None,
                rect,radius:0.0,opacity:1.0,clip:false},Command::FillRect{rect,color},Command::PopLayer]);
            let resolved=rasterize_layers(&list,scale,&font,&mut GlyphCache::default()).unwrap();
            let Command::Image{rect,image}=&resolved.0[0] else {panic!("filtered sprite missing")};
            (*rect,image.clone())
        };
        let red=Rgba{r:255,g:0,b:0,a:255};
        let mut prior=None;
        for scale in [1.0,2.0] {
            let rect=Rect{x:4.0/scale,y:4.0/scale,width:1.0/scale,height:1.0/scale};
            let (crop,image)=resolve(vec![F::Blur(0.5/scale)],rect,scale,red);
            assert_eq!(crop,Rect{x:2.0/scale,y:2.0/scale,width:5.0/scale,height:5.0/scale});
            let pixel=|x:usize,y:usize|&image.pixels[(y*5+x)*4..(y*5+x)*4+4];
            assert_eq!(pixel(2,2),[255,0,0,158]);assert_eq!(pixel(1,2),[255,0,0,21]);
            assert_eq!(pixel(1,2),pixel(3,2));assert_eq!(pixel(2,1),pixel(2,3));
            if let Some(previous)=prior {assert_eq!(previous,image.pixels);}prior=Some(image.pixels.clone());
        }
        let rect=Rect{x:4.0,y:4.0,width:1.0,height:1.0};
        let (_,identity)=resolve(vec![F::Blur(0.0)],rect,1.0,Rgba{a:128,..red});
        assert_eq!(identity.pixels,[255,0,0,128],"zero sigma preserves bytes and alpha exactly");
        let shadow=F::DropShadow(Arc::new(DropShadowFilter{offset:[3.0,0.0],sigma:0.0,color:Color::rgba8([0,0,255,255])}));
        let (crop,plain)=resolve(vec![shadow.clone()],rect,1.0,red);
        assert_eq!(crop,Rect{x:4.0,y:4.0,width:4.0,height:1.0});
        assert_eq!(&plain.pixels[..4],[255,0,0,255]);assert_eq!(&plain.pixels[12..16],[0,0,255,255]);
        let (_,before)=resolve(vec![F::Color(C::Invert(1.0)),shadow.clone()],rect,1.0,red);
        let (_,after)=resolve(vec![shadow.clone(),F::Color(C::Invert(1.0))],rect,1.0,red);
        assert_eq!(&before.pixels[..4],[0,255,255,255]);assert_eq!(&before.pixels[12..16],[0,0,255,255]);
        assert_eq!(&after.pixels[..4],[0,255,255,255]);assert_eq!(&after.pixels[12..16],[255,255,0,255]);
        let (_,fractional)=resolve(vec![F::DropShadow(Arc::new(DropShadowFilter{offset:[0.5,0.0],sigma:0.0,color:Color::rgba8([0,0,255,255])}))],rect,1.0,red);
        assert_eq!(&fractional.pixels[..4],[255,0,0,255]);assert_eq!(&fractional.pixels[4..8],[0,0,255,128]);
    }

    #[test]
    fn specification_spatial_filter_source_and_output_clips_have_distinct_order() {
        use lumen_common::filter::FilterOperation as F;
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let clip=Rect{x:2.0,y:2.0,width:1.0,height:1.0};
        let source=Rect{x:3.0,y:2.0,width:1.0,height:1.0};
        let make=|source_clip:bool| {
            let mut commands=vec![Command::PushLayer{ svg_clip: None,filters:Some(Arc::from([F::Blur(0.5)])),corners:None,
                rect:clip,radius:0.0,opacity:1.0,clip:!source_clip}];
            if source_clip {commands.push(Command::PushClip(clip));}
            commands.push(Command::FillRect{rect:source,color:Rgba{r:255,g:0,b:0,a:255}});
            if source_clip {commands.push(Command::PopClip);}
            commands.push(Command::PopLayer);DisplayList(commands)
        };
        for source_clip in [false,true] {
            let resolved=rasterize_layers(&make(source_clip),1.0,&font,&mut GlyphCache::default()).unwrap();
            let output=render_scaled_region(&resolved,6,5,1.0,Some(&font),&mut GlyphCache::default()).unwrap();
            let at=(2*6+2)*4;
            assert_eq!(&output.pixels[at..at+4],if source_clip {&[0,0,0,0]}else{&[255,0,0,21]},
                "source clipping removes outside ink before blur; an output clip retains its blurred contribution");
        }
    }

    #[test]
    fn specification_spatial_filter_source_clip_bounds_avoid_invisible_surface_allocation() {
        use lumen_common::filter::FilterOperation as F;
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let clip=Rect{x:4.0,y:4.0,width:1.0,height:1.0};
        let list=DisplayList(vec![Command::PushLayer{ svg_clip: None,filters:Some(Arc::from([F::Blur(0.5)])),corners:None,
            rect:clip,radius:0.0,opacity:1.0,clip:false},Command::PushBoxClip(clip),
            Command::FillRect{rect:Rect{x:0.0,y:0.0,width:100000.0,height:100000.0},color:Rgba{r:255,g:0,b:0,a:255}},
            Command::PopClip,Command::PopLayer]);
        let resolved=rasterize_layers(&list,1.0,&font,&mut GlyphCache::default()).unwrap();
        let Command::Image{rect,image}=&resolved.0[0] else {panic!("filtered source missing")};
        assert_eq!(*rect,Rect{x:2.0,y:2.0,width:5.0,height:5.0});
        assert_eq!(image.pixels.len(),100,"only clipped SourceGraphic and Gaussian support allocate");
        assert_eq!(&image.pixels[(2*5+2)*4..(2*5+2)*4+4],[255,0,0,158]);
    }

    #[test]
    fn specification_spatial_filter_scratch_is_bounded_and_large_sigma_reuses_shadow_kernel() {
        let mut source=vec![0;31*31];source[15*31+15]=255;
        let actual=blur_filter_plane(source.clone(),31,31,2.5,1.0,0).unwrap();
        let mut expected=source.clone();shadow::blur_alpha_mask(&mut expected,31,31,5.0,1.0,0).unwrap();
        assert_eq!(actual,expected);assert_eq!(filter_blur_padding(2.5,1.0).unwrap(),shadow::blur_padding(5.0).unwrap());
        assert_eq!(blur_filter_plane(source,31,31,0.5,1.0,MAX_LAYER_BYTES),Err(ImageError::TooLarge));
        for sigma in [-1.0,f32::NAN,f32::INFINITY] {assert!(filter_blur_padding(sigma,1.0).is_err());}
    }

    #[test]
    fn specification_color_filters_group_overlap_and_apply_before_opacity() {
        use lumen_common::filter::ColorFilter as F;
        let rect = Rect { x: 4.0, y: 4.0, width: 4.0, height: 4.0 };
        let font = FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let list = DisplayList(vec![
            Command::PushLayer { svg_clip: None, filters: Some(Arc::from([F::Invert(1.0).into(), F::Opacity(0.5).into()])), corners: None,
                rect, radius: 0.0, opacity: 0.5, clip: false },
            Command::FillRect { rect, color: Rgba { r: 255, g: 0, b: 0, a: 255 } },
            Command::FillRect { rect, color: Rgba { r: 0, g: 0, b: 255, a: 255 } },
            Command::PopLayer,
        ]);
        for scale in [1.0, 1.25, 2.0] {
            let resolved = rasterize_layers(&list, scale, &font, &mut GlyphCache::default()).unwrap();
            let Command::Image { image, .. } = &resolved.0[0] else { panic!("shared filter layer sprite missing") };
            assert!(image.pixels.chunks_exact(4).all(|p| p == [255, 255, 0, 64]), "the top blue child is inverted once and the two alpha factors multiply");
        }
        let source = "<style>body{margin:0}#host{margin:10px;filter:invert();overflow:hidden;width:20px;height:20px}#fixed{position:fixed;left:0;top:0;width:40px;height:10px;background:linear-gradient(to right,magenta 50%,cyan 50%)}</style><div id=host><div id=fixed></div></div>";
        let reference = "<style>body{margin:0}div{margin:10px;width:20px;height:10px;background:lime}</style><div></div>";
        for scale in [1.0, 2.0] {
            assert_eq!(render_html_with_font(source, 60, 50, scale, &font).unwrap(), render_html_with_font(reference, 60, 50, scale, &font).unwrap(), "the shared filter establishes the fixed CB and clips the inverted magenta half");
        }
    }

    #[test]
    fn specification_svg_replay_clips_share_vector_and_group_sprite_coverage() {
        use lumen_html::paint::{SvgPaint, SvgFillRule};
        let bounds = Rect { x: 0.0, y: 0.0, width: 8.0, height: 8.0 };
        let lime = Rgba { r: 0, g: 255, b: 0, a: 255 };
        for scale in [1.0, 1.25, 2.0] {
            for offset in [0.25, 0.5, 0.75] {
                for translated in [false, true] {
                    let clip = Rect { x: offset, y: offset, width: 3.0, height: 3.0 };
                    let matrix = if translated { Affine { e: offset, f: -offset, ..Affine::IDENTITY } } else { Affine::IDENTITY };
                    let path = Command::SvgPath { bounds, data: Arc::from("M-4 -4H12V12H-4Z"), transform: matrix,
                        fill: Some(SvgPaint::Color(lime)), stroke: None, stroke_width: 0.0,
                        fill_rule: SvgFillRule::NonZero, clips: Arc::from([]) };
                    let direct = DisplayList(vec![Command::PushClip(clip), path.clone(), Command::PopClip]);
                    let grouped = DisplayList(vec![Command::PushClip(clip),
                        Command::PushLayer { svg_clip: None, filters: Some(Arc::from([lumen_common::filter::ColorFilter::Invert(0.0).into()])),
                            corners: None, rect: bounds, radius: 0.0, opacity: 1.0, clip: false },
                        path, Command::PopLayer, Command::PopClip]);
                    let reference = DisplayList(vec![Command::PushClip(clip), Command::FillRect { rect: bounds, color: lime }, Command::PopClip]);
                    let expected = render(&reference, 8, 8, scale, true).unwrap();
                    if (clip.x * scale).fract() != 0.0 || ((clip.x + clip.width) * scale).fract() != 0.0 {
                        assert!(expected.pixels.chunks_exact(4).any(|pixel| pixel[3] > 0 && pixel[3] < 255), "fixture has fractional clip coverage");
                    }
                    assert!(expected.pixels.chunks_exact(4).any(|pixel| pixel == [0, 255, 0, 255]), "fixture has opaque interior ink");
                    assert_eq!(render(&direct, 8, 8, scale, true).unwrap(), expected, "vector clip: scale={scale}, offset={offset}, translated={translated}");
                    assert_eq!(render(&grouped, 8, 8, scale, true).unwrap(), expected, "group sprite clip: scale={scale}, offset={offset}, translated={translated}");
                }
            }
        }
    }

    #[test]
    fn specification_integral_square_group_clip_preserves_bounded_direct_replay() {
        let clip = Rect { x: 2.0, y: 2.0, width: 4.0, height: 4.0 };
        let blue = Rgba { r: 0, g: 0, b: 255, a: 255 };
        let list = DisplayList(vec![Command::PushLayer { svg_clip: None, filters: None, corners: None,
            rect: clip, radius: 0.0, opacity: 1.0, clip: true },
            Command::FillRect { rect: Rect { x: -1000.0, y: -1000.0, width: 10000.0, height: 10000.0 }, color: blue },
            Command::PopLayer]);
        for scale in [1.0, 1.5, 2.0] {
            let mut bytes = 0;
            let resolved = resolve_layers(&list.0, scale, None, &mut GlyphCache::default(), &mut bytes).unwrap();
            assert_eq!(bytes, 0, "binary clip needs no offscreen payload for oversized ink");
            assert!(!resolved.0.iter().any(|command| matches!(command, Command::Image { .. })));
            let reference = DisplayList(vec![Command::FillRect { rect: clip, color: blue }]);
            assert_eq!(render(&list, 8, 8, scale, true).unwrap(), render(&reference, 8, 8, scale, true).unwrap());
        }
    }

    #[test]
    fn specification_wrapped_inline_filter_owns_all_fragments_once_and_keeps_sibling_ink() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for alignment in ["left","center","right"] {for nested in [false,true] {
            let inner=if nested {"<span id=inner style='filter:invert(0)'>AA</span>"}else{"AA"};
            let source=format!("<style>html,body{{margin:0}}main{{width:24px;font:12px/14px sans-serif;text-align:{alignment}}}#host{{background:magenta;color:transparent;filter:invert()}}#sibling{{background:blue;color:transparent}}</style><main><span id=host>AA AA {inner} AA</span><span id=sibling>BB BB</span></main>");
            let reference=source.replace("filter:invert()","filter:none").replace("background:magenta","background:lime");
            let document=lumen_html::html::parse(&source,128).unwrap();let mut session=lumen_html::session::RenderSession::new(document);
            let host=lumen_html::selector::query_selector(session.document(),session.document().root(),"#host").unwrap().unwrap();
            let list=session.display_list(64,100,&font).unwrap().clone();
            assert!(session.client_rects(host).len()>=2,"fixture really wraps: {alignment}/{nested}");
            assert_eq!(list.0.iter().filter(|command|matches!(command,Command::PushLayer{filters:Some(_),..})).count(),if nested{2}else{1},"each inline captures one complete source, never one layer per line");
            assert_eq!(list,session.display_list(64,100,&font).unwrap().clone(),"warm fragment owner replay");
            assert_eq!(list,lumen_html::layout::display_list(session.document(),64,100,&font).unwrap(),"fresh fragment owner replay");
            for scale in [1.0,1.25,2.0] {
                assert_eq!(render_html_with_font(&source,64,100,scale,&font).unwrap(),render_html_with_font(&reference,64,100,scale,&font).unwrap(),"wrapped owned source and unrelated following inline: {alignment}/{nested}/scale={scale}");
            }
        }}
    }

    fn assert_exact_spatial_pixels(actual:&Rgba8Image,expected:&Rgba8Image,label:&str) {
        assert_eq!((actual.width,actual.height),(expected.width,expected.height),"{label}: dimensions");
        assert_eq!(actual.pixels.len(),expected.pixels.len(),"{label}: pixel storage");
        let mut count=0;let mut first=None;let mut maximum=0;
        for (index,(a,b)) in actual.pixels.chunks_exact(4).zip(expected.pixels.chunks_exact(4)).enumerate() {
            if a!=b {
                count+=1;
                if first.is_none(){first=Some((index%actual.width as usize,index/actual.width as usize,a,b));}
                for (&a,&b) in a.iter().zip(b){maximum=maximum.max(a.abs_diff(b));}
            }
        }
        assert_eq!(count,0,"{label}: differing pixels={count}, max channel difference={maximum}, first={first:?}");
    }

    #[test]
    fn specification_filter_shadow_convolution_sees_transparent_outside_finite_source() {
        use lumen_common::filter::{FilterOperation as F,DropShadowFilter};
        for scale in [1.0,1.25,2.0] {for sigma in [0.0,0.5] {for offset in [[2.5,3.25],[-2.5,3.25],[2.5,-3.25],[-2.5,-3.25],[0.0,0.0]] {
            let mut source=Rgba8Image{width:7,height:7,pixels:vec![0;7*7*4]};
            source.pixels[..4].copy_from_slice(&[255,0,0,192]);
            source.pixels[48*4..49*4].copy_from_slice(&[0,0,255,128]);
            let padding=filter_blur_padding(sigma,scale).unwrap() as u32;
            let width=source.width+padding*2;let height=source.height+padding*2;
            let mut field=image::GrayImage::new(width,height);
            field.put_pixel(padding,padding,image::Luma([192]));
            field.put_pixel(6+padding,6+padding,image::Luma([128]));
            let field=if sigma==0.0{field}else{image::imageops::blur(&field,sigma*scale)};
            let mut expected=source.clone();
            let shadow=lumen_common::color::Color::rgba8([0,255,0,192]);
            for y in 0..source.height {for x in 0..source.width {
                let mut alpha=0.0;
                transform::visit_bilinear_samples(width,height,f64::from(x)-f64::from(offset[0])*f64::from(scale)+f64::from(padding),
                    f64::from(y)-f64::from(offset[1])*f64::from(scale)+f64::from(padding),|at,weight|alpha+=f64::from(field.as_raw()[at])*weight);
                let at=(y as usize*source.width as usize+x as usize)*4;
                let original=Rgba{r:source.pixels[at],g:source.pixels[at+1],b:source.pixels[at+2],a:source.pixels[at+3]};
                expected.pixels[at..at+4].fill(0);
                composite(&mut expected.pixels[at..at+4],Rgba{r:0,g:255,b:0,a:192},(alpha/255.0)as f32);
                composite(&mut expected.pixels[at..at+4],original,1.0);
            }}
            let mut actual=source.clone();
            apply_filter_operations(&mut actual,&[F::DropShadow(Arc::new(DropShadowFilter{offset,sigma,color:shadow}))],scale,source.pixels.len()).unwrap();
            assert_exact_spatial_pixels(&actual,&expected,&format!("finite alpha outside edgeMode:none source offset={offset:?} sigma={sigma} scale={scale}"));
        }}}
    }

    #[test]
    fn specification_url_filter_css_local_origin_and_percentages_use_the_transform_reference_box() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for (keyword,inset,width,height) in [("content-box",5.0,20.0,12.0),("fill-box",5.0,20.0,12.0),
            ("border-box",0.0,30.0,22.0),("stroke-box",0.0,30.0,22.0),("view-box",0.0,30.0,22.0)] {
            let source=format!("<style>html,body{{margin:0;background:white}}#host{{margin:8px;width:20px;height:12px;padding:2px;border:3px solid transparent;transform-box:{keyword};filter:url(#paint)}}</style><svg style='display:none'><filter id='paint' filterUnits='userSpaceOnUse' x='0' y='0' width='50%' height='50%'><feFlood flood-color='red'/></filter></svg><div id='host'></div>");
            let mut session=lumen_html::session::RenderSession::new(lumen_html::html::parse(&source,128).unwrap());
            let list=session.display_list(64,48,&font).unwrap().clone();
            let used=list.0.iter().find_map(|command|match command {
                Command::PushLayer{filters:Some(filters),..}=>filters.iter().find_map(|filter|match filter {
                    lumen_common::filter::FilterOperation::Resource(used)=>Some(used),_=>None}),_=>None,
            }).expect("real bound URL filter");
            assert_eq!(used.owner_transform,[1.0,0.0,0.0,1.0,8.0+inset,8.0+inset],"{keyword}: local origin");
            assert_eq!(used.reference_box,[-inset,-inset,30.0,22.0],"{keyword}: object bounds remain the border box");
            assert_eq!(used.viewport,[0.0,0.0,width,height],"{keyword}: percentage reference");
            assert_eq!(used.filter_region(),Some([0.0,0.0,width*0.5,height*0.5]));
            let reference=format!("<style>html,body{{margin:0;background:white}}div{{position:absolute;left:{}px;top:{}px;width:{}px;height:{}px;background:red}}</style><div></div>",8.0+inset,8.0+inset,width*0.5,height*0.5);
            assert_eq!(list,session.display_list(64,48,&font).unwrap().clone(),"{keyword}: warm source");
            for scale in [1.0,2.0] {
                let mut snapped=list.clone();snap::boxes(&mut snapped,scale);
                assert_exact_spatial_pixels(&render_with_font(&snapped,64,48,scale,true,&font).unwrap(),
                    &render_html_with_font(&reference,64,48,scale,&font).unwrap(),&format!("local CSS URL coordinate keyword={keyword} scale={scale}"));
            }
        }
    }

    #[test]
    fn specification_url_filter_dom_generators_keep_empty_sources_and_live_definition_cascade() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let source="<style>html,body{margin:0;background:white}#paint{display:none}#flood{flood-color:red}#host{margin:8px;width:12px;height:10px;filter:url(#paint)}</style><svg style='display:none'><defs><filter id=paint x='0' y='0' width='100%' height='100%'><feFlood id='flood'/></filter></defs></svg><div id=host></div>";
        let mut session=lumen_html::session::RenderSession::new(lumen_html::html::parse(source,128).unwrap());
        let flood=lumen_html::selector::query_selector(session.document(),session.document().root(),"#flood").unwrap().unwrap();
        for color in ["red","blue","red"] {
            session.document_mut().set_attribute(flood,"style",&format!("flood-color:{color}")).unwrap();
            let reference=format!("<style>html,body{{margin:0;background:white}}div{{margin:8px;width:12px;height:10px;background:{color}}}</style><div></div>");
            let list=session.display_list(40,32,&font).unwrap().clone();
            assert!(list.0.iter().any(|command|matches!(command,Command::PushLayer{filters:Some(filters),..} if filters.iter().any(|filter|matches!(filter,lumen_common::filter::FilterOperation::Resource(_))))),"empty source retains its real generator owner");
            assert_eq!(list,session.display_list(40,32,&font).unwrap().clone(),"warm definition replay");
            assert_eq!(list,lumen_html::layout::display_list(session.document(),40,32,&font).unwrap(),"fresh definition replay");
            for scale in [1.0,2.0] {
                let mut snapped=list.clone();snap::boxes(&mut snapped,scale);
                assert_exact_spatial_pixels(&render_with_font(&snapped,40,32,scale,true,&font).unwrap(),
                    &render_html_with_font(&reference,40,32,scale,&font).unwrap(),&format!("DOM flood color={color} scale={scale}"));
            }
        }
    }

    #[test]
    fn specification_url_filter_missing_resource_ignores_the_chain_but_empty_filter_is_transparent() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for (definition,filter,expected) in [
            ("","invert() url(#absent)","red"),
            ("<div id=wrong></div>","url(#wrong) invert()","red"),
            ("<svg style='display:none'><filter id=empty></filter></svg>","url(#empty)","white"),
        ] {
            let source=format!("<style>html,body{{margin:0;background:white}}#host{{width:12px;height:10px;background:red;filter:{filter}}}</style>{definition}<div id=host></div>");
            let reference=format!("<style>html,body{{margin:0;background:white}}div{{width:12px;height:10px;background:{expected}}}</style><div></div>");
            for scale in [1.0,1.25,2.0] {
                assert_exact_spatial_pixels(&render_html_with_font(&source,24,20,scale,&font).unwrap(),
                    &render_html_with_font(&reference,24,20,scale,&font).unwrap(),&format!("missing/empty definition={definition} scale={scale}"));
            }
        }
    }

    #[test]
    fn specification_url_filter_svg_affine_source_keeps_local_sigma_and_world_hits() {
        use lumen_common::{color::ColorSpace,filter::{FilterOperation as F,resource::{Builder,Coordinate,Units,Node,Primitive,Input,Use,PaintInput,EdgeMode}}};
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for matrix in [Affine{a:2.0,d:1.0,e:8.0,f:8.0,..Affine::IDENTITY},Affine{a:0.0,b:1.0,c:-2.0,d:0.0,e:40.0,f:8.0}] {
            let transform=format!("matrix({} {} {} {} {} {})",matrix.a,matrix.b,matrix.c,matrix.d,matrix.e,matrix.f);
            let source=format!("<style>html,body{{margin:0;background:white}}</style><svg width='64' height='48'><defs><filter id='paint' filterUnits='userSpaceOnUse' x='0' y='0' width='20' height='20' color-interpolation-filters='sRGB'><feGaussianBlur stdDeviation='0.5 1'/></filter></defs><g transform='{transform}' filter='url(#paint)'><rect id='shape' x='4' y='4' width='2' height='2' fill='red'/></g></svg>");
            let plain=source.replace("filter='url(#paint)'","");
            let document=lumen_html::html::parse(&source,128).unwrap();
            let mut session=lumen_html::session::RenderSession::new(document);
            let shape=lumen_html::selector::query_selector(session.document(),session.document().root(),"#shape").unwrap().unwrap();
            let actual=session.display_list(64,48,&font).unwrap().clone();
            let plain_document=lumen_html::html::parse(&plain,128).unwrap();let mut plain_session=lumen_html::session::RenderSession::new(plain_document);
            let plain_shape=lumen_html::selector::query_selector(plain_session.document(),plain_session.document().root(),"#shape").unwrap().unwrap();
            let mut expected=plain_session.display_list(64,48,&font).unwrap().clone();
            assert_eq!(session.client_rects(shape),plain_session.client_rects(plain_shape),"filter-local capture does not change actual world hit geometry");
            assert_eq!(actual,session.display_list(64,48,&font).unwrap().clone(),"warm affine source replay");
            assert_eq!(actual,lumen_html::layout::display_list(session.document(),64,48,&font).unwrap(),"fresh affine source replay");
            let coordinate=|value|Coordinate{value,percentage:false};let mut builder=Builder::new();
            builder.push(Node{operation:Primitive::GaussianBlur{sigma:[0.5,1.0],edge:EdgeMode::None},inputs:Arc::from([Input::SourceGraphic]),region:[None;4],color_space:ColorSpace::Srgb},None,MAX_LAYER_BYTES).unwrap();
            let program=builder.finish([coordinate(0.0),coordinate(0.0),coordinate(20.0),coordinate(20.0)],Units::UserSpaceOnUse,Units::UserSpaceOnUse,MAX_LAYER_BYTES).unwrap();
            let used=Arc::new(Use{url:Arc::from("#paint"),program,reference_box:[4.0,4.0,2.0,2.0],viewport:[0.0,0.0,64.0,48.0],owner_transform:[1.0,0.0,0.0,1.0,0.0,0.0],fill:PaintInput::None,stroke:PaintInput::None});
            let at=expected.0.iter().position(|command|matches!(command,Command::SvgPath{..})).unwrap();
            let mut path=expected.0.remove(at);
            let crop=if let Command::SvgPath{transform,bounds,..}=&mut path {assert_eq!(*transform,matrix);*transform=Affine::IDENTITY;*bounds=matrix.inverse().unwrap().bounds(*bounds);*bounds}else{unreachable!()};
            expected.0.splice(at..at,[Command::PushTransform(matrix),Command::PushLayer{svg_clip:None,filters:Some(Arc::from([F::Resource(used)])),corners:None,rect:crop,radius:0.0,opacity:1.0,clip:false},path,Command::PopLayer,Command::PopTransform]);
            for scale in [1.0,1.25,2.0] {
                let mut actual=actual.clone();let mut expected=expected.clone();snap::boxes(&mut actual,scale);snap::boxes(&mut expected,scale);
                assert_exact_spatial_pixels(&render_with_font(&actual,64,48,scale,true,&font).unwrap(),
                    &render_with_font(&expected,64,48,scale,true,&font).unwrap(),&format!("actual DOM local anisotropic source matrix={matrix:?} scale={scale}"));
            }
        }
    }

    #[test]
    fn specification_url_filter_wrapped_inline_uses_one_resolved_owner_for_all_fragments() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for alignment in ["left","center","right"] {for nested in [false,true] {
            let inner=if nested{"<span style='filter:invert(0)'>AA</span>"}else{"AA"};
            let source=format!("<style>html,body{{margin:0;background:white}}main{{width:24px;font:12px/14px sans-serif;text-align:{alignment}}}#host{{background:magenta;color:transparent;filter:url(#paint)}}#sibling{{background:blue;color:transparent}}</style><svg style='display:none'><filter id='paint' filterUnits='userSpaceOnUse' x='-10' y='-10' width='100' height='120' color-interpolation-filters='sRGB'><feComponentTransfer><feFuncR type='table' tableValues='1 0'/><feFuncG type='table' tableValues='1 0'/><feFuncB type='table' tableValues='1 0'/></feComponentTransfer></filter></svg><main><span id=host>AA AA {inner} AA</span><span id=sibling>BB BB</span></main>");
            let reference=source.replace("filter:url(#paint)","filter:invert()");
            let mut session=lumen_html::session::RenderSession::new(lumen_html::html::parse(&source,128).unwrap());
            let host=lumen_html::selector::query_selector(session.document(),session.document().root(),"#host").unwrap().unwrap();
            let list=session.display_list(64,100,&font).unwrap().clone();
            assert!(session.client_rects(host).len()>=2,"actual URL owner spans multiple physical lines");
            assert_eq!(list.0.iter().filter(|command|matches!(command,Command::PushLayer{filters:Some(filters),..} if filters.iter().any(|filter|matches!(filter,lumen_common::filter::FilterOperation::Resource(_))))).count(),1,"one typed resource per wrapped owner");
            assert_eq!(list,session.display_list(64,100,&font).unwrap().clone(),"warm wrapped URL replay");
            assert_eq!(list,lumen_html::layout::display_list(session.document(),64,100,&font).unwrap(),"fresh wrapped URL replay");
            for scale in [1.0,1.25,2.0] {
                assert_exact_spatial_pixels(&render_html_with_font(&source,64,100,scale,&font).unwrap(),
                    &render_html_with_font(&reference,64,100,scale,&font).unwrap(),&format!("canonical wrapped URL owner alignment={alignment} nested={nested} scale={scale}"));
            }
        }}
    }

    #[test]
    fn specification_url_filter_svg_hidden_sources_keep_generators_and_empty_shapes_stay_disabled() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let definition="<defs><filter id='paint' filterUnits='userSpaceOnUse' x='0' y='0' width='10' height='10'><feFlood flood-color='green'/></filter></defs>";
        let reference="<style>html,body{margin:0;background:white}</style><div style='width:10px;height:10px;background:green'></div>";
        for (root,content) in [
            ("","<rect width='4' height='4' fill='red' visibility='hidden' filter='url(#paint)'/>"),
            (" style='visibility:hidden'","<rect width='4' height='4' fill='red' filter='url(#paint)'/>"),
            ("","<text x='0' y='12' visibility='hidden' filter='url(#paint)'>AB</text>"),
            ("","<g filter='url(#paint)'></g>"),
            ("","<rect width='4' height='4' fill='none' filter='url(#paint)'/>"),
        ] {
            let source=format!("<style>html,body{{margin:0;background:white}}</style><svg width='40' height='32'{root}>{definition}{content}</svg>");
            for scale in [1.0,2.0] {
                assert_exact_spatial_pixels(&render_html_with_font(&source,40,32,scale,&font).unwrap(),
                    &render_html_with_font(reference,40,32,scale,&font).unwrap(),&format!("actual hidden/generator source={content} root={root} scale={scale}"));
            }
        }
        let source=format!("<style>html,body{{margin:0;background:white}}</style><svg width='40' height='32'>{definition}<rect width='0' height='0' filter='url(#paint)'/><circle r='0' filter='url(#paint)'/><path d='' filter='url(#paint)'/><polygon points='' filter='url(#paint)'/></svg>");
        assert_exact_spatial_pixels(&render_html_with_font(&source,40,32,1.0,&font).unwrap(),
            &render_html_with_font("<style>html,body{margin:0;background:white}</style>",40,32,1.0,&font).unwrap(),"SVG zero-sized shapes disable the complete effect");
    }

    #[test]
    fn specification_svg_canvas_source_excludes_root_transforms_even_when_singular() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for transform in ["translate(5px,3px)","scale(2)","scale(0)","rotate(90deg)"] {
            for (filter,color) in [("none",Rgba{r:255,g:0,b:0,a:255}),("invert()",Rgba{r:0,g:255,b:255,a:255}),("url(#invert)",Rgba{r:0,g:255,b:255,a:255})] {
                for view in ["", " viewBox='0 0 20 16'"] {
                    let svg=format!("<svg xmlns='http://www.w3.org/2000/svg' width='40' height='32'{view} style='background:red;filter:{filter};transform:{transform};transform-origin:0 0'><defs><filter id='invert' filterUnits='userSpaceOnUse' x='0' y='0' width='100%' height='100%' color-interpolation-filters='sRGB'><feComponentTransfer><feFuncR type='linear' slope='-1' intercept='1'/><feFuncG type='linear' slope='-1' intercept='1'/><feFuncB type='linear' slope='-1' intercept='1'/></feComponentTransfer></filter></defs></svg>");
                    let mut session=lumen_html::session::RenderSession::new(lumen_html::xml::parse(&svg,64).unwrap());
                    let list=session.display_list(40,32,&font).unwrap().clone();
                    assert_eq!(list,session.display_list(40,32,&font).unwrap().clone(),"warm transformed root canvas");
                    assert_eq!(list,lumen_html::layout::display_list(session.document(),40,32,&font).unwrap(),"fresh transformed root canvas");
                    let reference=DisplayList(vec![Command::FillRect{rect:Rect{x:0.0,y:0.0,width:40.0,height:32.0},color}]);
                    for scale in [1.0,2.0] {assert_exact_spatial_pixels(&render_with_font(&list,40,32,scale,true,&font).unwrap(),&render_with_font(&reference,40,32,scale,true,&font).unwrap(),&format!("canvas excludes root transform={transform} filter={filter} view={view:?} scale={scale}"));}
                }
            }
        }
        // CSS transforms still affect graphics and their viewport clip. An
        // untransformed sibling SVG is an independent geometric reference.
        for filter in ["none","invert()"] {
            let actual=format!("<svg xmlns='http://www.w3.org/2000/svg' width='40' height='32' style='background:red;filter:{filter};transform:translate(4px,3px);transform-origin:0 0'><rect x='2' y='2' width='4' height='3' fill='lime'/></svg>");
            let (background,fill)=if filter=="none"{("red","lime")}else{("cyan","magenta")};
            let expected=format!("<svg xmlns='http://www.w3.org/2000/svg' width='40' height='32' style='background:{background}'><rect x='6' y='5' width='4' height='3' fill='{fill}'/></svg>");
            let actual=lumen_html::layout::display_list(&lumen_html::xml::parse(&actual,32).unwrap(),40,32,&font).unwrap();
            let reference=lumen_html::layout::display_list(&lumen_html::xml::parse(&expected,32).unwrap(),40,32,&font).unwrap();
            for scale in [1.0,2.0] {assert_exact_spatial_pixels(&render_with_font(&actual,40,32,scale,true,&font).unwrap(),&render_with_font(&reference,40,32,scale,true,&font).unwrap(),&format!("root content retains direct transform filter={filter} scale={scale}"));}
        }
    }

    #[test]
    fn specification_svg_canvas_background_is_one_full_canvas_source_before_filter_and_opacity() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for (filter,opacity,color) in [("none",1.0,Rgba{r:255,g:0,b:0,a:255}),
            ("invert()",1.0,Rgba{r:0,g:255,b:255,a:255}),
            ("none",0.5,Rgba{r:255,g:127,b:127,a:255})] {
            let svg=format!("<svg xmlns='http://www.w3.org/2000/svg' width='8' height='6' style='background:red;filter:{filter};opacity:{opacity}'/>");
            let mut session=lumen_html::session::RenderSession::new(lumen_html::xml::parse(&svg,32).unwrap());
            let list=session.display_list(40,32,&font).unwrap().clone();
            assert_eq!(list,session.display_list(40,32,&font).unwrap().clone(),"warm root canvas source filter={filter} opacity={opacity}");
            assert_eq!(list,lumen_html::layout::display_list(session.document(),40,32,&font).unwrap(),"fresh root canvas source");
            for scale in [1.0,2.0] {
                let mut actual=list.clone();snap::boxes(&mut actual,scale);
                let reference=DisplayList(vec![Command::FillRect{rect:Rect{x:0.0,y:0.0,width:40.0,height:32.0},color}]);
                assert_exact_spatial_pixels(&render_with_font(&actual,40,32,scale,true,&font).unwrap(),
                    &render_with_font(&reference,40,32,scale,true,&font).unwrap(),&format!("standalone propagated canvas filter={filter} opacity={opacity} scale={scale}"));
            }
        }
    }

    #[test]
    fn specification_url_filter_svg_root_has_one_source_owner_and_one_opacity_application() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for standalone in [false,true] {
            let svg="<svg xmlns='http://www.w3.org/2000/svg' width='40' height='32' style='background:red;filter:url(#paint);opacity:0.5'><defs><filter id='paint' filterUnits='userSpaceOnUse' x='0' y='0' width='10' height='10'><feFlood flood-color='green'/></filter></defs><rect width='4' height='4' fill='blue'/></svg>";
            let document=if standalone{lumen_html::xml::parse(svg,128).unwrap()}else{lumen_html::html::parse(&format!("<style>html,body{{margin:0;background:white;font-size:0;line-height:0}}</style>{svg}"),128).unwrap()};
            let mut session=lumen_html::session::RenderSession::new(document);
            let list=session.display_list(40,32,&font).unwrap().clone();
            assert_eq!(list.0.iter().filter(|command|matches!(command,Command::PushLayer{filters:Some(filters),..} if filters.iter().any(|filter|matches!(filter,lumen_common::filter::FilterOperation::Resource(_))))).count(),1,"root source has one actual URL operation: standalone={standalone}");
            assert_eq!(list.0.iter().filter(|command|matches!(command,Command::PushLayer{opacity,..} if *opacity==0.5)).count(),1,"root opacity is applied exactly once");
            for scale in [1.0,2.0] {
                let mut snapped=list.clone();snap::boxes(&mut snapped,scale);
                let actual=render_with_font(&snapped,40,32,scale,true,&font).unwrap();
                let reference=DisplayList(vec![Command::FillRect{rect:Rect{x:0.0,y:0.0,width:40.0,height:32.0},color:Rgba{r:255,g:255,b:255,a:255}},
                    Command::PushLayer{svg_clip:None,filters:None,corners:None,rect:Rect{x:0.0,y:0.0,width:10.0,height:10.0},radius:0.0,opacity:0.5,clip:false},
                    Command::FillRect{rect:Rect{x:0.0,y:0.0,width:10.0,height:10.0},color:Rgba{r:0,g:128,b:0,a:255}},Command::PopLayer]);
                assert_exact_spatial_pixels(&actual,&render_with_font(&reference,40,32,scale,true,&font).unwrap(),&format!("actual root source standalone={standalone} scale={scale}"));
            }
        }
    }

    #[test]
    fn specification_url_filter_blend_uses_backend_modes_working_space_and_bounded_scratch() {
        use lumen_common::{color::{Color,ColorSpace},filter::FilterOperation as F};
        use tiny_skia::BlendMode as B;
        let modes=[("normal",B::SourceOver),("multiply",B::Multiply),("screen",B::Screen),("overlay",B::Overlay),
            ("darken",B::Darken),("lighten",B::Lighten),("color-dodge",B::ColorDodge),("color-burn",B::ColorBurn),
            ("hard-light",B::HardLight),("soft-light",B::SoftLight),("difference",B::Difference),("exclusion",B::Exclusion),
            ("hue",B::Hue),("saturation",B::Saturation),("color",B::Color),("luminosity",B::Luminosity)];
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        // Independent one-pixel maintained backend oracle, not the production
        // operation adapter or shared mode mapping. Every graph pixel is equal.
        let oracle=|mode:B,space:ColorSpace,composite:bool,opacity:f32| {
            let mut source=Color::rgba8([170,85,204,(opacity*255.0).round() as u8]).to(space);
            if source.alpha==0.0{source.components=[0.0;3];}
            let backdrop=Color::rgba8([68,136,221,64]).to(space);
            let encode=|color:Color,alpha:f32|{let p:[u8;4]=[ (color.components[0]*alpha*255.0).round() as u8,
                (color.components[1]*alpha*255.0).round() as u8,(color.components[2]*alpha*255.0).round() as u8,(alpha*255.0).round() as u8];p};
            let source_bytes=encode(source,if composite{source.alpha}else{1.0});
            let mut target_bytes=encode(backdrop,backdrop.alpha);
            let source_map=tiny_skia::PixmapRef::from_bytes(&source_bytes,1,1).unwrap();
            let mut target=tiny_skia::PixmapMut::from_bytes(&mut target_bytes,1,1).unwrap();
            target.draw_pixmap(0,0,source_map,&tiny_skia::PixmapPaint{blend_mode:mode,..Default::default()},tiny_skia::Transform::identity(),None);
            let alpha=f32::from(target_bytes[3])/255.0;
            let components=if alpha==0.0{[0.0;3]}else{core::array::from_fn(|channel|(f32::from(target_bytes[channel])/255.0/alpha).clamp(0.0,1.0))};
            let mut pixel=Color::new(space,components,alpha,0).to_rgba8();
            if !composite{pixel[3]=(source.alpha*255.0).round() as u8;if pixel[3]==0{pixel[..3].fill(0);}}
            pixel
        };
        for (keyword,mode) in modes {for (space,space_name) in [(ColorSpace::Srgb,"sRGB"),(ColorSpace::SrgbLinear,"linearRGB")] {
            for composite in [false,true] {for opacity in [0.0_f32,0.5,1.0] {
                let flag=if composite{""}else{" no-composite='no-composite'"};
                let xml=format!("<svg xmlns='http://www.w3.org/2000/svg' width='8' height='8' style='filter:url(#f)'><filter id='f' filterUnits='userSpaceOnUse' x='0' y='0' width='8' height='8' color-interpolation-filters='{space_name}'><feFlood flood-color='#4488dd' flood-opacity='.25' result='backdrop'/><feFlood flood-color='#aa55cc' flood-opacity='{opacity}' result='source'/><feBlend in='source' in2='backdrop' mode='{keyword}'{flag}/></filter></svg>");
                let mut session=lumen_html::session::RenderSession::new(lumen_html::xml::parse(&xml,64).unwrap());
                let list=session.display_list(8,8,&font).unwrap().clone();
                assert_eq!(list,session.display_list(8,8,&font).unwrap().clone(),"warm blend {keyword}");
                assert_eq!(list,lumen_html::layout::display_list(session.document(),8,8,&font).unwrap(),"fresh blend {keyword}");
                let used=list.0.iter().find_map(|command|if let Command::PushLayer{filters:Some(filters),..}=command{filters.iter().find_map(|filter|if let F::Resource(used)=filter{Some(used.clone())}else{None})}else{None}).unwrap();
                let expected=oracle(mode,space,composite,opacity);
                for scale in [1.0,1.25,2.0] {
                    let region=FilterPixelRegion::from_rect(Rect{x:0.0,y:0.0,width:8.0,height:8.0},scale).unwrap();
                    let actual=render_filter_region(&[],&[F::Resource(used.clone())],region,scale,None,&mut GlyphCache::default(),0,true).unwrap();
                    assert!(actual.pixels.chunks_exact(4).all(|pixel|pixel==expected),"blend={keyword} space={space_name} composite={composite} opacity={opacity} scale={scale} first={:?} expected={expected:?}",&actual.pixels[..4]);
                }
            }}
        }}
        // Independently known opaque primary colors also detect input reversal.
        for (mode,expected) in [(lumen_common::filter::resource::BlendMode::Normal,[255,0,0,255]),
            (lumen_common::filter::resource::BlendMode::Multiply,[0,0,0,255]),
            (lumen_common::filter::resource::BlendMode::Screen,[255,0,255,255])] {
            let one=|pixel:[u8;4]|Rgba8Image{width:1,height:1,pixels:Vec::from(pixel)};
            assert_eq!(blend_resource_images(one([255,0,0,255]),one([0,0,255,255]),mode,true,ColorSpace::Srgb,0).unwrap().pixels,expected);
        }
        let wide=|pixel:[u8;4]|Rgba8Image{width:10240,height:1,pixels:pixel.repeat(10240)};
        let wide=blend_resource_images(wide([255,0,0,255]),wide([0,0,255,255]),lumen_common::filter::resource::BlendMode::Screen,true,ColorSpace::Srgb,0).unwrap();
        assert!(wide.pixels.chunks_exact(4).all(|pixel|pixel==[255,0,255,255]),"long borrowed row backend path preserves every source pixel");
        let one=||Rgba8Image{width:1,height:1,pixels:vec![255,0,0,128]};
        assert!(matches!(blend_resource_images(one(),one(),lumen_common::filter::resource::BlendMode::Normal,false,ColorSpace::Srgb,MAX_LAYER_BYTES-8),Err(ImageError::TooLarge)),"no-composite alpha scratch is preflighted");
    }

    #[test]
    fn specification_url_filter_generators_share_the_global_graph_and_declared_subregions() {
        use lumen_common::{color::{Color,ColorSpace},filter::{FilterOperation as F,resource::{Builder,Coordinate,Units,Node,Primitive,Input,Use,PaintInput}}};
        let coordinate=|value|Coordinate{value,percentage:false};
        let mut builder=Builder::new();
        builder.push(Node{operation:Primitive::Flood(Color::rgba8([255,0,0,255])),inputs:Arc::from([]),
            region:[Some(coordinate(2.0)),Some(coordinate(1.0)),Some(coordinate(3.0)),Some(coordinate(2.0))],color_space:ColorSpace::Srgb},None,MAX_LAYER_BYTES).unwrap();
        builder.push(Node{operation:Primitive::Flood(Color::rgba8([0,0,255,255])),inputs:Arc::from([]),
            region:[Some(coordinate(4.0)),Some(coordinate(2.0)),Some(coordinate(2.0)),Some(coordinate(2.0))],color_space:ColorSpace::Srgb},None,MAX_LAYER_BYTES).unwrap();
        builder.push(Node{operation:Primitive::Merge,inputs:Arc::from([Input::Result(0),Input::Result(1),Input::Result(0)]),
            region:[None;4],color_space:ColorSpace::Srgb},None,MAX_LAYER_BYTES).unwrap();
        let program=builder.finish([coordinate(0.0),coordinate(0.0),coordinate(8.0),coordinate(6.0)],Units::UserSpaceOnUse,Units::UserSpaceOnUse,MAX_LAYER_BYTES).unwrap();
        let used=Arc::new(Use{url:Arc::from("#f"),program,reference_box:[0.0,0.0,8.0,6.0],viewport:[0.0,0.0,8.0,6.0],owner_transform:[1.0,0.0,0.0,1.0,0.0,0.0],fill:PaintInput::None,stroke:PaintInput::None});
        for scale in [1.0,1.25,2.0]{
            let region=FilterPixelRegion::from_rect(Rect{x:0.0,y:0.0,width:8.0,height:6.0},scale).unwrap();
            let actual=render_filter_region(&[],&[F::Resource(used.clone())],region,scale,None,&mut GlyphCache::default(),0,true).unwrap();
            // feMerge composes completed input rasters, rather than applying
            // vector coverage to the evolving backdrop. Independently rasterize
            // each native shape, then compose the three ImageCommands in order.
            let mut reference=DisplayList::default();
            for (rect,color) in [([2.0,1.0,3.0,2.0],[255,0,0,255]),([4.0,2.0,2.0,2.0],[0,0,255,255]),([2.0,1.0,3.0,2.0],[255,0,0,255])] {
                let mut surface=canvas::CanvasSurface::new(region.width,region.height).unwrap();
                surface.state_mut().transform=tiny_skia::Transform::from_scale(scale,scale);
                let path=tiny_skia::PathBuilder::from_rect(tiny_skia::Rect::from_xywh(rect[0],rect[1],rect[2],rect[3]).unwrap());
                surface.state_mut().fill=color;surface.fill_path(&path,tiny_skia::FillRule::Winding).unwrap();
                let raster=surface.snapshot();
                reference.0.push(Command::Image{rect:region.rect(scale),image:Arc::new(ImageData{width:raster.width,height:raster.height,pixels:raster.pixels})});
            }
            let expected=render_scaled_region_at(&reference,region.width,region.height,scale,None,&mut GlyphCache::default(),[0,0],None).unwrap();
            assert_exact_spatial_pixels(&actual,&expected,&format!("empty SourceGraphic flood/ordered shared result merge scale={scale}"));
            assert!(filter_output_bounds(None,&[F::Resource(used.clone())],scale).unwrap().is_some(),"generator output is not culled with its empty SourceGraphic");
        }
    }

    fn assert_fractional_url_domains(case:&str) {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let render=|xml:&str,scale:f32|{
            let mut session=lumen_html::session::RenderSession::new(lumen_html::xml::parse(xml,128).unwrap());
            let mut list=session.display_list(40,32,&font).unwrap().clone();
            assert_eq!(list,session.display_list(40,32,&font).unwrap().clone(),"warm fractional domain");
            assert_eq!(list,lumen_html::layout::display_list(session.document(),40,32,&font).unwrap(),"fresh fractional domain");
            snap::boxes(&mut list,scale);render_with_font(&list,40,32,scale,true,&font).unwrap()
        };
        for origin in [".125",".625",".875"] {for scale in [1.0,1.25,2.0] {
            let svg=|body:&str|format!("<svg xmlns='http://www.w3.org/2000/svg' width='40' height='32'><g transform='translate({origin} {origin})'>{body}</g></svg>");
            let rectangle="<rect x='2' y='2' width='16' height='12' fill='black'/>";
            let region="filterUnits='userSpaceOnUse' x='2' y='2' width='16' height='12' color-interpolation-filters='sRGB'";
            let identity="<feColorMatrix/>";
            if case=="native" {
            let native=svg(&format!("<filter id='f' {region}>{identity}{identity}</filter><rect x='2' y='2' width='16' height='12' fill='black' filter='url(#f)'/>"));
            assert_exact_spatial_pixels(&render(&native,scale),&render(&svg(rectangle),scale),&format!("native source repeated identity origin={origin} scale={scale}"));
            }
            if case=="flood" {
            let flood=svg(&format!("<filter id='f' {region}><feFlood flood-color='black'/>{identity}{identity}</filter><rect x='2' y='2' width='16' height='12' fill='red' filter='url(#f)'/>"));
            assert_exact_spatial_pixels(&render(&flood,scale),&render(&svg(rectangle),scale),&format!("generated source repeated identity origin={origin} scale={scale}"));
            }
            let clip="<clipPath id='c'><rect x='2' y='2' width='16' height='12'/></clipPath>";
            if case=="offset" {
            for offset in [-0.5_f32,0.5] {
                let actual=svg(&format!("<filter id='f' {region}><feFlood flood-color='black'/><feOffset dx='{offset}' dy='{offset}'/></filter><rect x='2' y='2' width='16' height='12' fill='red' filter='url(#f)'/>"));
                // feOffset translates an image, not vector geometry. Render
                // the native SVG source independently, then exercise the shared
                // ImageCommand sampling consumer beneath the actual SVG clip.
                let source_document=lumen_html::xml::parse(&svg(rectangle),64).unwrap();
                let source=lumen_html::layout::display_list(&source_document,40,32,&font).unwrap();
                let path=source.0.iter().find(|command|matches!(command,Command::SvgPath{..})).unwrap().clone();
                let viewport=FilterPixelRegion::from_rect(Rect{x:0.0,y:0.0,width:40.0,height:32.0},scale).unwrap();
                let source=render_scaled_region_at(&DisplayList(vec![path]),viewport.width,viewport.height,scale,Some(&font),&mut GlyphCache::default(),[0,0],None).unwrap();
                let (source_rect,source)=transform::rasterize(source,Rect{x:0.0,y:0.0,width:40.0,height:32.0},
                    Affine{e:offset,f:offset,..Affine::IDENTITY},scale,MAX_LAYER_BYTES).unwrap().unwrap();
                let clip_document=lumen_html::xml::parse(&svg(&format!("{clip}<g clip-path='url(#c)'>{rectangle}</g>")),64).unwrap();
                let clipping=lumen_html::layout::display_list(&clip_document,40,32,&font).unwrap();
                let clipping=clipping.0.iter().find(|command|matches!(command,Command::PushLayer{svg_clip:Some(_),..})).unwrap().clone();
                // The actual document includes its opaque white canvas; the
                // independent image/clip reference keeps that same outer canvas.
                let expected=DisplayList(vec![Command::FillRect{rect:Rect{x:0.0,y:0.0,width:40.0,height:32.0},color:Rgba{r:255,g:255,b:255,a:255}},clipping,Command::Image{rect:source_rect,
                    image:Arc::new(ImageData{width:source.width,height:source.height,pixels:source.pixels})},Command::PopLayer]);
                assert_exact_spatial_pixels(&render(&actual,scale),&render_with_font(&expected,40,32,scale,true,&font).unwrap(),&format!("generated raster offset resets domain origin={origin} offset={offset} scale={scale}"));
            }
            }
            if case=="blur" {
            let actual=svg(&format!("<filter id='f' {region}><feFlood flood-color='black'/><feGaussianBlur stdDeviation='.5'/></filter><rect x='2' y='2' width='16' height='12' fill='red' filter='url(#f)'/>"));
            let expected=svg(&format!("{clip}<g clip-path='url(#c)'><g style='filter:blur(.5px)'>{rectangle}</g></g>"));
            assert_exact_spatial_pixels(&render(&actual,scale),&render(&expected,scale),&format!("generated blur resets domain origin={origin} scale={scale}"));
            }
        }}
    }

    #[test]
    fn specification_url_filter_contour_tiles_match_native_svg_scan_and_budget() {
        for width in [513u32,10241] {for scale in [1.0f32,1.25,2.0] {
            let region=FilterPixelRegion{left:-7,top:13,width,height:9};
            for edge in [256.125f32,512.625,10000.875] {
                let domain=Rect{x:-6.875,y:13.125/scale,width:edge,height:5.625/scale};
                let mut actual=Rgba8Image{width,height:9,pixels:vec![0;width as usize*9*4]};
                for pixel in actual.pixels.chunks_exact_mut(4){pixel[3]=255;}
                apply_filter_domain_coverage(&mut actual,region,domain,scale,0).unwrap();
                // Independent native path consumer, including its >8192 tiler.
                let rect=tiny_skia::Rect::from_xywh(domain.x,domain.y,domain.width,domain.height).unwrap();
                let path=tiny_skia::PathBuilder::from_rect(rect);
                let mut reference=canvas::CanvasSurface::new(width,9).unwrap();
                reference.state_mut().transform=tiny_skia::Transform::from_row(scale,0.0,0.0,scale,7.0,-13.0);
                reference.state_mut().fill=[0,0,0,255];
                reference.fill_path(&path,tiny_skia::FillRule::Winding).unwrap();
                assert_exact_spatial_pixels(&actual,&reference.snapshot(),&format!("contour tile width={width} edge={edge} scale={scale}"));
            }
        }}
        let region=FilterPixelRegion{left:0,top:0,width:8,height:8};
        let mut image=Rgba8Image{width:8,height:8,pixels:vec![255;256]};
        let prior=image.clone();
        assert!(matches!(apply_filter_domain_coverage(&mut image,region,Rect{x:0.125,y:0.125,width:4.0,height:4.0},1.0,MAX_LAYER_BYTES-256-8),Err(ImageError::TooLarge)));
        assert_eq!(image.pixels,prior.pixels,"scratch rejection precedes publication");
        let mut mask=tiny_skia::Mask::new(8,8).unwrap();
        assert!(mask.fill_rectangle_path_bounded(tiny_skia::Rect::from_xywh(0.125,0.125,4.0,4.0).unwrap(),0).is_err());
        assert!(mask.data().iter().all(|alpha|*alpha==0));
    }

    #[test]
    fn specification_url_filter_pointwise_alpha_changes_retain_applied_contour() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let render=|xml:&str,scale:f32|{
            let document=lumen_html::xml::parse(xml,128).unwrap();
            render_document(&document,40,32,scale,&font).unwrap()
        };
        for origin in [0.125,0.625,0.875] {for scale in [1.0,1.25,2.0] {
            let svg=|body:&str|format!("<svg xmlns='http://www.w3.org/2000/svg' width='40' height='32'><g transform='translate({origin} {origin})'>{body}</g></svg>");
            let domain="filterUnits='userSpaceOnUse' x='2' y='2' width='16' height='12' color-interpolation-filters='sRGB'";
            let owner="<rect x='2' y='2' width='16' height='12' fill='red' filter='url(#f)'/>";
            let reference=svg("<rect x='2' y='2' width='16' height='12' fill='black' fill-opacity='.5'/>");
            for operation in ["<feColorMatrix values='1 0 0 0 0 0 1 0 0 0 0 0 1 0 0 0 0 0 .5 0'/>",
                "<feComponentTransfer><feFuncA type='linear' slope='.5'/></feComponentTransfer>"] {
                let actual=svg(&format!("<filter id='f' {domain}><feFlood flood-color='black'/>{operation}</filter>{owner}"));
                assert_exact_spatial_pixels(&render(&actual,scale),&render(&reference,scale),&format!("pointwise retained contour origin={origin} scale={scale} operation={operation}"));
            }
            // An alpha-generating primitive must still receive a new contour.
            let generated=svg(&format!("<filter id='f' {domain}><feFlood flood-opacity='0'/><feColorMatrix values='0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 .5'/></filter>{owner}"));
            assert_exact_spatial_pixels(&render(&generated,scale),&render(&reference,scale),&format!("new pointwise alpha requires contour origin={origin} scale={scale}"));
        }}
    }

    #[test]
    fn specification_url_filter_source_stroke_uses_actual_bounded_outline_support() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for origin in [0.125,0.625,0.875] {for scale in [1.0,1.25,2.0] {
            let svg=|body:&str|format!("<svg xmlns='http://www.w3.org/2000/svg' width='40' height='32'><g transform='translate({origin} {origin})'>{body}</g></svg>");
            let stroke="<rect x='3' y='3' width='14' height='10' fill='none' stroke='black' stroke-width='2'/>";
            let actual=svg(&format!("<filter id='f' filterUnits='userSpaceOnUse' x='2' y='2' width='16' height='12' color-interpolation-filters='sRGB'><feColorMatrix/><feColorMatrix/></filter><g filter='url(#f)'>{stroke}</g>"));
            let expected=svg(stroke);
            let render=|xml:&str|render_document(&lumen_html::xml::parse(xml,128).unwrap(),40,32,scale,&font).unwrap();
            assert_exact_spatial_pixels(&render(&actual),&render(&expected),&format!("stroke outline source contour origin={origin} scale={scale}"));
            let document=lumen_html::xml::parse(&expected,64).unwrap();
            let list=lumen_html::layout::display_list(&document,40,32,&font).unwrap();
            let paths:Vec<_>=list.0.iter().filter(|command|matches!(command,Command::SvgPath{..})).cloned().collect();
            let object=svg_source_object_box(&paths,Affine::IDENTITY,Some(&font),0).unwrap();
            let ink=svg_source_geometry_bounds(&paths,Affine::IDENTITY,Some(&font),0,Some(scale)).unwrap().unwrap();
            assert!(ink.x<object.e && ink.y<object.f && ink.width>object.a && ink.height>object.d,"stroke support is distinct from paint-free object geometry");
            let mut hairline=paths.clone();
            for command in &mut hairline {if let Command::SvgPath{stroke_width,..}=command {*stroke_width=0.125;}}
            assert!(svg_source_geometry_bounds(&hairline,Affine::IDENTITY,Some(&font),0,Some(scale)).unwrap().is_none(),"hairline support is owned by a separate raster algorithm");
            assert!(matches!(svg_source_geometry_bounds(&paths,Affine::IDENTITY,Some(&font),MAX_LAYER_BYTES-64,Some(scale)),Err(ImageError::TooLarge)));
        }}
    }

    #[test]
    fn specification_url_filter_source_glyph_overhang_does_not_use_object_cell_proof() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let run=lumen_html::paint::TextShaper::shape(&font,"j",20.0).unwrap();
        let glyph=run.glyphs[0];let cell=font.glyph_cell_bounds(glyph.face,glyph.id,20.0*glyph.size_scale).unwrap();
        let ink=font.glyph_ink_bounds(&glyph,20.0).unwrap();
        assert!(ink.x<glyph.x+cell.x,"actual glyph overhang lies outside the SVG object cell");
        for origin in [0.125,0.625,0.875] {for scale in [1.0,1.25,2.0] {
            let svg=|body:&str|format!("<svg xmlns='http://www.w3.org/2000/svg' width='40' height='32'><g transform='translate({} {origin})'>{body}</g></svg>",8.0+origin);
            let text="<text x='0' y='20' font-size='20' fill='black'>j</text>";
            let actual=svg(&format!("<filter id='f' filterUnits='userSpaceOnUse' x='0' y='0' width='20' height='24' color-interpolation-filters='sRGB'><feColorMatrix/></filter><g filter='url(#f)'>{text}</g>"));
            let expected=svg(&format!("<clipPath id='c'><rect width='20' height='24'/></clipPath><g clip-path='url(#c)'>{text}</g>"));
            let render=|xml:&str|render_document(&lumen_html::xml::parse(xml,128).unwrap(),40,32,scale,&font).unwrap();
            assert_exact_spatial_pixels(&render(&actual),&render(&expected),&format!("glyph overhang source clip origin={origin} scale={scale}"));
        }}
    }

    #[test]
    fn specification_url_filter_fractional_source_input_clips_oversized_native_ink() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for origin in [0.125,0.625,0.875] {for scale in [1.0,1.25,2.0] {for input in ["SourceGraphic","SourceAlpha"] {
            let svg=|body:&str|format!("<svg xmlns='http://www.w3.org/2000/svg' width='40' height='32'><g transform='translate({origin} {origin})'>{body}</g></svg>");
            let shape="<rect width='32' height='24' fill='black'/>";
            let actual=svg(&format!("<filter id='f' filterUnits='userSpaceOnUse' x='2' y='2' width='16' height='12' color-interpolation-filters='sRGB'><feColorMatrix in='{input}'/></filter><g filter='url(#f)'>{shape}</g>"));
            let expected=svg(&format!("<clipPath id='c'><rect x='2' y='2' width='16' height='12'/></clipPath><g clip-path='url(#c)'>{shape}</g>"));
            let render=|xml:&str|render_document(&lumen_html::xml::parse(xml,128).unwrap(),40,32,scale,&font).unwrap();
            assert_exact_spatial_pixels(&render(&actual),&render(&expected),&format!("hard filter input contour origin={origin} scale={scale} input={input}"));
            if input=="SourceGraphic" {
                let reference=svg("<rect x='2' y='2' width='16' height='12' fill='lime'/>");
                for chain in ["invert() url(#f)","url(#f) invert()","invert() url(#f) opacity(1)"] {
                    let actual=svg(&format!("<filter id='f' filterUnits='userSpaceOnUse' x='2' y='2' width='16' height='12' color-interpolation-filters='sRGB'><feColorMatrix/></filter><rect x='2' y='2' width='16' height='12' fill='magenta' filter='{chain}'/>"));
                    assert_exact_spatial_pixels(&render(&actual),&render(&reference),&format!("CSS/URL source contour origin={origin} scale={scale} chain={chain}"));
                }
            }
        }}}
    }

    #[test]
    fn specification_url_filter_fractional_native_source_repeated_identity(){assert_fractional_url_domains("native");}
    #[test]
    fn specification_url_filter_fractional_generated_source_repeated_identity(){assert_fractional_url_domains("flood");}
    #[test]
    fn specification_url_filter_fractional_offset_resets_contour(){assert_fractional_url_domains("offset");}
    #[test]
    fn specification_url_filter_fractional_blur_resets_contour(){assert_fractional_url_domains("blur");}

    #[test]
    fn specification_url_filter_flood_retains_fractional_svg_rectangle_coverage() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for offset in ["0px",".125px",".625px",".875px"] {
            let shared=format!("html,body{{margin:0}}svg{{position:absolute;left:8px;top:{offset}}}");
            let actual=format!("<!doctype html><style>{shared}</style><svg width='40' height='32'><filter id='f' x='0' y='0' width='1' height='1'><feFlood flood-color='black'/></filter><rect x='2' y='2' width='16' height='12' fill='red' filter='url(#f)'/></svg>");
            let reference=format!("<!doctype html><style>{shared}</style><svg width='40' height='32'><rect x='2' y='2' width='16' height='12' fill='black'/></svg>");
            for scale in [1.0,1.25,2.0] {
                assert_exact_spatial_pixels(&render_html_with_font(&actual,64,48,scale,&font).unwrap(),&render_html_with_font(&reference,64,48,scale,&font).unwrap(),&format!("flood fractional owner offset={offset} scale={scale}"));
            }
        }
    }

    #[test]
    fn specification_url_filter_transformed_complete_source_uses_the_shared_graph() {
        use lumen_common::filter::FilterOperation as F;
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for sigma in ["0",".5 1.25"] {
            let html=format!("<style>html,body{{margin:0}}#outer{{transform:scale(3);transform-origin:0 0;width:8px;height:6px}}#host{{width:8px;height:6px;background:green;filter:url(#f)}}</style><div id='outer'><div id='host'></div></div><svg style='display:none'><filter id='f' color-interpolation-filters='sRGB'><feGaussianBlur stdDeviation='{sigma}'/></filter></svg>");
            let mut session=lumen_html::session::RenderSession::new(lumen_html::html::parse(&html,128).unwrap());
            let list=session.display_list(40,32,&font).unwrap().clone();
            assert_eq!(list,session.display_list(40,32,&font).unwrap().clone(),"warm transformed graph");
            assert_eq!(list,lumen_html::layout::display_list(session.document(),40,32,&font).unwrap(),"fresh transformed graph");
            let used=list.0.iter().find_map(|command|match command{Command::PushLayer{filters:Some(filters),..}=>filters.iter().find_map(|filter|match filter{F::Resource(used)=>Some(used.clone()),_=>None}),_=>None}).unwrap();
            assert_eq!(used.owner_transform,[1.0,0.0,0.0,1.0,0.0,0.0]);
            let source=[Command::FillRect{rect:Rect{x:0.0,y:0.0,width:8.0,height:6.0},color:Rgba{r:0,g:128,b:0,a:255}}];
            for scale in [1.0,1.25,2.0] {
                let region=used.filter_region().unwrap();let crop=FilterPixelRegion::from_rect(Rect{x:region[0],y:region[1],width:region[2],height:region[3]},scale).unwrap();
                let image=render_filter_region(&source,&[F::Resource(used.clone())],crop,scale,Some(&font),&mut GlyphCache::default(),0,true).unwrap();
                let reference=DisplayList(vec![Command::FillRect{rect:Rect{x:0.0,y:0.0,width:40.0,height:32.0},color:Rgba{r:255,g:255,b:255,a:255}},
                    Command::Image{rect:Affine{a:3.0,d:3.0,..Affine::IDENTITY}.bounds(crop.rect(scale)),image:Arc::new(ImageData{width:image.width,height:image.height,pixels:image.pixels})}]);
                assert_exact_spatial_pixels(&render_with_font(&list,40,32,scale,true,&font).unwrap(),&render_with_font(&reference,40,32,scale,true,&font).unwrap(),&format!("complete transformed resource source sigma={sigma} scale={scale}"));
            }
        }
    }

    #[test]
    fn specification_url_drop_shadow_matches_its_private_primitive_tree_and_tiles() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for (sigma,offset) in [("0",[0,0]),("0 .5",[-2,3]),(".75 0",[3,-2]),(".5 1.25",[-2,-3])] {
            for space in ["sRGB","linearRGB"] {
                let input="<rect x='5' y='4' width='4' height='3' fill='red' fill-opacity='.5'/><rect x='7' y='5' width='3' height='3' fill='blue'/>";
                let actual=format!("<feDropShadow stdDeviation='{sigma}' dx='{}' dy='{}' flood-color='lime' flood-opacity='.5'/>",offset[0],offset[1]);
                let expected=format!("<feGaussianBlur in='SourceAlpha' stdDeviation='{sigma}'/><feOffset dx='{}' dy='{}' result='offsetblur'/><feFlood flood-color='lime' flood-opacity='.5'/><feComposite in2='offsetblur' operator='in'/><feMerge><feMergeNode/><feMergeNode in='SourceGraphic'/></feMerge>",offset[0],offset[1]);
                let markup=|primitives:&str|format!("<!doctype html><style>body{{margin:0}}</style><svg width='24' height='20'><defs><filter id='f' filterUnits='userSpaceOnUse' x='0' y='0' width='24' height='20' color-interpolation-filters='{space}'>{primitives}</filter></defs><g filter='url(#f)'>{input}</g></svg>");
                let mut session=lumen_html::session::RenderSession::new(lumen_html::html::parse(&markup(&actual),128).unwrap());
                let actual=session.display_list(24,20,&font).unwrap().clone();
                assert_eq!(actual,session.display_list(24,20,&font).unwrap().clone(),"warm shorthand graph");
                assert_eq!(actual,lumen_html::layout::display_list(session.document(),24,20,&font).unwrap(),"fresh shorthand graph");
                let reference=lumen_html::layout::display_list(&lumen_html::html::parse(&markup(&expected),128).unwrap(),24,20,&font).unwrap();
                for scale in [1.0,1.25,2.0] {
                    let mut actual=actual.clone();let mut reference=reference.clone();snap::boxes(&mut actual,scale);snap::boxes(&mut reference,scale);
                    assert_exact_spatial_pixels(&render_with_font(&actual,24,20,scale,true,&font).unwrap(),&render_with_font(&reference,24,20,scale,true,&font).unwrap(),&format!("drop shadow shorthand sigma={sigma} offset={offset:?} space={space} scale={scale}"));
                }
            }
        }
    }

    #[test]
    fn specification_url_filter_gaussian_identity_axes_keep_edge_and_singleton_cells() {
        let parameters=image::imageops::GaussianBlurParameters::new_anisotropic_sigma;
        for (width,height) in [(1,1),(1,7),(7,1),(7,7)] {
            let mut source=image::GrayImage::new(width,height);source.put_pixel(0,0,image::Luma([255]));
            assert_eq!(image::imageops::blur_advanced(&source,parameters(0.0,0.0)),source,"two identity axes {width}x{height}");
            let vertical=image::imageops::blur_advanced(&source,parameters(0.0,0.5));
            let horizontal=image::imageops::blur_advanced(&source,parameters(0.5,0.0));
            assert!(vertical.get_pixel(0,0)[0]>0 && horizontal.get_pixel(0,0)[0]>0,"singleton kernels retain the first row and column {width}x{height}");
            if width==1 {assert_eq!(vertical,image::imageops::blur_advanced(&source,parameters(0.5,0.5)),"one-column horizontal convolution is identity");}
            if height==1 {assert_eq!(horizontal,image::imageops::blur_advanced(&source,parameters(0.5,0.5)),"one-row vertical convolution is identity");}
            for y in 0..height {for x in 0..width {
                if x!=0{assert_eq!(vertical.get_pixel(x,y)[0],0,"zero horizontal sigma cannot move source columns");}
                if y!=0{assert_eq!(horizontal.get_pixel(x,y)[0],0,"zero vertical sigma cannot move source rows");}
            }}
        }
    }

    #[test]
    fn specification_url_filter_anisotropic_none_edges_preserve_complete_and_tiled_device_phase() {
        use lumen_common::{color::ColorSpace,filter::{FilterOperation as F,resource::{Builder,Coordinate,Units,Node,Primitive,Input,Use,PaintInput,EdgeMode}}};
        let coordinate=|value|Coordinate{value,percentage:false};
        let source=[Command::FillRect{rect:Rect{x:4.0,y:3.0,width:1.0,height:1.0},color:Rgba{r:255,g:0,b:0,a:255}}];
        for sigma in [[0.0,0.5],[0.5,0.0],[0.125,0.75],[1.25,0.5]]{
            let mut builder=Builder::new();builder.push(Node{operation:Primitive::GaussianBlur{sigma,edge:EdgeMode::None},inputs:Arc::from([Input::SourceGraphic]),region:[None;4],color_space:ColorSpace::Srgb},None,MAX_LAYER_BYTES).unwrap();
            let program=builder.finish([coordinate(0.0),coordinate(0.0),coordinate(10.0),coordinate(8.0)],Units::UserSpaceOnUse,Units::UserSpaceOnUse,MAX_LAYER_BYTES).unwrap();
            let used=Arc::new(Use{url:Arc::from("#f"),program,reference_box:[0.0,0.0,10.0,8.0],viewport:[0.0,0.0,10.0,8.0],owner_transform:[1.0,0.0,0.0,1.0,0.0,0.0],fill:PaintInput::None,stroke:PaintInput::None});
            for scale in [1.0,1.25,2.0]{
                let full=FilterPixelRegion::from_rect(Rect{x:0.0,y:0.0,width:10.0,height:8.0},scale).unwrap();
                let actual=render_filter_region(&source,&[F::Resource(used.clone())],full,scale,None,&mut GlyphCache::default(),0,true).unwrap();
                for left in 0..full.width{let region=FilterPixelRegion{left:i64::from(left),top:0,width:1,height:full.height};
                    let tile=render_filter_region(&source,&[F::Resource(used.clone())],region,scale,None,&mut GlyphCache::default(),0,true).unwrap();
                    let expected=crop_filter_region(&actual,full,region,0).unwrap();
                    assert_exact_spatial_pixels(&tile,&expected,&format!("anisotropic sigma={sigma:?} scale={scale} column={left}"));
                }
            }
        }
    }

    #[test]
    fn specification_filter_graph_metadata_tracks_live_construction_and_released_lookups() {
        use lumen_common::filter::FilterOperation as F;
        let mut graph=FilterRegionGraph{nodes:Vec::new(),sources:Vec::new(),image_bytes:0,construction_bytes:0,payload_bytes:0,source_search_bytes:0,source_index:Some(std::collections::BTreeMap::new())};
        let mut index=std::collections::BTreeMap::new();
        for at in 0..64 {
            let source=graph.register_source(FilterGraphSource{view:SourceScopeView{start:at,translation:[0.0,0.0]},end:at+1,layer:None,capture:false,filters:Arc::from([F::Blur(0.5)]),resource:None,geometric_support:None},0).unwrap();
            let root=graph.add(source,1,FilterPixelRegion{left:0,top:0,width:2,height:2},0,&mut index).unwrap();
            graph.compile_pending(root,1.25,0,&mut index,None).unwrap();
            graph.assert_metadata_accounting();
        }
        let retained=graph.metadata_bytes().unwrap();
        let released=graph.construction_bytes+graph.source_search_bytes;
        drop(index);graph.finish_construction();graph.assert_metadata_accounting();
        assert_eq!(graph.metadata_bytes().unwrap(),retained-released);
        assert!(graph.register_source(FilterGraphSource{view:SourceScopeView::default(),end:0,layer:None,capture:false,filters:Arc::from([]),resource:None,geometric_support:None},MAX_LAYER_BYTES).is_err(),"source growth still respects the existing operation envelope");
    }

    #[test]
    fn specification_filter_source_path_capture_uses_actual_bounded_geometry_storage() {
        let data=format!("M0 0{}", "h1v1h-1v-1".repeat(512));
        let parsed=lumen_common::svg_path::parse_svg_path_bounded(&data,MAX_LAYER_BYTES).unwrap();
        let allocated=parsed.path.as_ref().unwrap().allocated_bytes();
        let remaining=allocated.checked_mul(3).unwrap()+1024;
        assert!(remaining<MAX_LAYER_BYTES && data.len()*64>remaining,"compact path proves actual geometry storage differs from the old source-text estimate");
        let path=Command::SvgPath{bounds:Rect{x:0.0,y:0.0,width:1.0,height:1.0},data:Arc::from(data),transform:Affine::IDENTITY,
            fill:Some(lumen_html::paint::SvgPaint::Color(Rgba{r:255,g:0,b:0,a:255})),stroke:None,stroke_width:0.0,
            fill_rule:lumen_html::paint::SvgFillRule::NonZero,clips:Arc::from([])};
        assert!(capture_source_command(&path,1.0,MAX_LAYER_BYTES-remaining).is_ok(),"maintained bounded parser admits actual remaining geometry budget");
        assert_eq!(capture_source_command(&path,1.0,MAX_LAYER_BYTES),Err(ImageError::TooLarge),"geometry allocation is rejected before exceeding the live lease");
    }

    #[test]
    fn specification_filter_source_capture_borrows_large_input_images_without_cloning_pixels() {
        use lumen_common::filter::FilterOperation as F;
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let pixels=vec![255u8;1024*1025*4];
        assert!(pixels.len()>MAX_LAYER_BYTES,"immutable admitted asset exceeds transient source envelope");
        let input=Command::Image{rect:Rect{x:3.25,y:4.5,width:3.5,height:4.25},image:Arc::new(ImageData{width:1024,height:1025,pixels})};
        assert_eq!(input.clone_owned_bytes(),Some(0),"cloning a command keeps its immutable pixels shared");
        let viewport=Rect{x:0.0,y:0.0,width:20.0,height:20.0};
        let source=DisplayList(vec![Command::PushLayer{rect:viewport,radius:0.0,corners:None,opacity:0.75,clip:false,svg_clip:None,
            filters:Some(Arc::from([F::Blur(0.5)]))},input,Command::PopLayer]);
        for scale in [1.0,1.25,2.0] {
            let complete=rasterize_layers(&source,scale,&font,&mut GlyphCache::default()).unwrap();
            assert_exact_spatial_pixels(&render_with_font(&source,20,20,scale,true,&font).unwrap(),
                &render_with_font(&complete,20,20,scale,true,&font).unwrap(),&format!("large shared input / small real blur source scale={scale}"));
        }
    }

    #[test]
    fn specification_filter_operation_graph_shares_nested_scope_windows_and_releases_images() {
        use lumen_common::filter::{FilterOperation as F,DropShadowFilter};
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let viewport=Rect{x:0.0,y:0.0,width:40.0,height:48.0};
        let mut source=DisplayList(vec![Command::FillRect{rect:viewport,color:Rgba{r:255,g:255,b:255,a:255}}]);
        for _ in 0..32 {source.0.push(Command::PushLayer{rect:viewport,radius:0.0,corners:None,opacity:1.0,clip:false,svg_clip:None,
            filters:Some(Arc::from([F::DropShadow(Arc::new(DropShadowFilter{offset:[0.5,0.5],sigma:0.0,
                color:lumen_common::color::Color::rgba8([0,255,0,32])}))]))});}
        source.0.push(Command::FillRect{rect:Rect{x:1.25,y:2.5,width:1.5,height:1.25},color:Rgba{r:255,g:0,b:0,a:192}});
        source.0.extend((0..32).map(|_|Command::PopLayer));
        // A later independent owner shares the rendering-operation registry;
        // it cannot reset either metadata accounting or live output charges.
        source.0.extend([Command::PushLayer{rect:viewport,radius:0.0,corners:None,opacity:0.75,clip:false,svg_clip:None,
            filters:Some(Arc::from([F::Blur(0.5)]))},
            Command::FillRect{rect:Rect{x:26.25,y:35.5,width:1.5,height:1.25},color:Rgba{r:0,g:0,b:255,a:192}},Command::PopLayer]);
        for scale in [1.0,1.25,2.0] {
            let complete=rasterize_layers(&source,scale,&font,&mut GlyphCache::default()).unwrap();
            let mut scene=SourceScopeAnalysis::new(&source.0,0).unwrap();let mut bytes=scene.bytes().unwrap();
            let output=resolve_layers_region_in_operation(&source.0,scale,Some(&font),&mut GlyphCache::default(),&mut bytes,false,Some(viewport),
                &mut scene,SourceScopeView::default()).unwrap();
            let graph=scene.graph.as_ref().expect("one operation registry holds both completed owners");
            graph.assert_metadata_accounting();
            assert!(graph.sources.len()>=33,"all real nested sources are represented");
            assert!(graph.nodes.len()<8192,"shared distinct source windows avoid binary repeated-shadow expansion: {}",graph.nodes.len());
            assert_eq!(graph.image_bytes,0,"completed dependencies release RGBA at final actual use");
            assert!(graph.nodes.iter().all(|node|node.uses==0 && node.image.is_none()),"no stale intermediate result remains after independent owner completion");
            assert!(bytes<=MAX_LAYER_BYTES,"source metadata and retained result sprites share the same4MiB envelope");
            assert_exact_spatial_pixels(&render_with_font(&output,40,48,scale,true,&font).unwrap(),
                &render_with_font(&complete,40,48,scale,true,&font).unwrap(),&format!("32 nested positive shadows plus later owner scale={scale}"));
        }
    }

    #[test]
    fn specification_filter_source_scope_identity_survives_integral_translation_views() {
        use lumen_common::filter::{FilterOperation as F,DropShadowFilter};
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let viewport=Rect{x:0.0,y:0.0,width:80.0,height:96.0};
        for scale in [1.0,1.25,2.0] {
            let shift=4.0/scale;
            let mut source=DisplayList(vec![Command::FillRect{rect:viewport,color:Rgba{r:255,g:255,b:255,a:255}},
                Command::PushLayer{rect:viewport,radius:0.0,corners:None,opacity:1.0,clip:false,svg_clip:None,filters:Some(Arc::from([F::Blur(0.5)]))}]);
            for at in 0..2 {
                source.0.extend([Command::PushTransform(Affine{e:shift+at as f32*28.0,f:shift,..Affine::IDENTITY}),
                    Command::PushLayer{rect:viewport,radius:0.0,corners:None,opacity:0.75,clip:false,svg_clip:None,
                        filters:Some(Arc::from([F::DropShadow(Arc::new(DropShadowFilter{offset:[2.5,3.25],sigma:0.5,
                            color:lumen_common::color::Color::rgba8([0,255,0,192])}))]))},
                    Command::FillRect{rect:Rect{x:1.25,y:30.25,width:7.5,height:40.5},color:Rgba{r:255,g:0,b:255,a:192}},
                    Command::PopLayer,Command::PopTransform]);
            }
            source.0.push(Command::PopLayer);
            let complete=rasterize_layers(&source,scale,&font,&mut GlyphCache::default()).unwrap();
            assert_exact_spatial_pixels(&render_with_font(&source,80,96,scale,true,&font).unwrap(),
                &render_with_font(&complete,80,96,scale,true,&font).unwrap(),&format!("stable source coordinates through sibling translated views scale={scale}"));
        }
        let mismatched=[Command::PushTransform(Affine::IDENTITY),Command::PopLayer];
        assert!(matches!(SourceScopeAnalysis::new(&mismatched,0),Err(ImageError::DisplayList(ReplayError::UnbalancedClip))),
            "boundary analysis uses actual typed scope matches");
    }

    #[test]
    fn specification_filter_scope_bounds_preserve_nested_outsets_and_device_cells() {
        use lumen_common::filter::{FilterOperation as F,ColorFilter as C,DropShadowFilter};
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let viewport=Rect{x:0.0,y:0.0,width:20.0,height:20.0};
        let layer=|filters:Vec<F>,clip|Command::PushLayer{rect:viewport,radius:0.0,corners:None,
            opacity:1.0,clip,svg_clip:None,filters:Some(Arc::from(filters))};
        let displaced=vec![Command::PushClip(viewport),layer(vec![F::DropShadow(Arc::new(DropShadowFilter{
            offset:[1000.0,1000.0],sigma:0.0,color:lumen_common::color::Color::rgba8([0,255,0,255])}))],false),
            Command::FillRect{rect:Rect{x:-1000.0,y:-1000.0,width:10.0,height:10.0},color:Rgba{r:255,g:0,b:0,a:255}},
            Command::PopLayer,Command::PopClip];
        assert_eq!(scoped_source_output_bounds(&displaced,1.0,Some(&font),&mut GlyphCache::default(),true,0).unwrap(),Some(Rect{x:0.0,y:0.0,width:10.0,height:10.0}),
            "ancestor clip intersects completed descendant shadow, never original offscreen SourceGraphic");
        for scale in [1.0,1.25,2.0] {
            let source=DisplayList(vec![Command::PushTransform(Affine{a:4.0,d:4.0,..Affine::IDENTITY}),
                layer(vec![F::Color(C::Invert(0.0))],false),
                Command::FillRect{rect:Rect{x:0.125,y:0.125,width:0.25,height:0.25},color:Rgba{r:255,g:0,b:0,a:255}},
                Command::PopLayer,Command::PopTransform]);
            let projected=scoped_source_output_bounds(&source.0,scale,Some(&font),&mut GlyphCache::default(),true,0).unwrap().unwrap();
            let complete=rasterize_layers(&source,scale,&font,&mut GlyphCache::default()).unwrap();
            let actual=paint_bounds(&complete.0,scale,Some(&font),&mut GlyphCache::default()).unwrap().unwrap();
            assert!(projected.x<=actual.x && projected.y<=actual.y && projected.x+projected.width>=actual.x+actual.width
                && projected.y+projected.height>=actual.y+actual.height,"layer edge cells survive parent scale: scale={scale} projected={projected:?} actual={actual:?}");
            assert_exact_spatial_pixels(&render_with_font(&source,20,20,scale,true,&font).unwrap(),
                &render_with_font(&complete,20,20,scale,true,&font).unwrap(),&format!("fractional nested cell scale={scale}"));
        }
    }

    #[test]
    fn specification_filter_tiny_unclipped_groups_request_actual_output_windows() {
        use lumen_common::filter::FilterOperation as F;
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let viewport=Rect{x:0.0,y:0.0,width:4096.0,height:4096.0};
        let mut source=DisplayList::default();
        for at in 0..40 {
            source.0.extend([Command::PushLayer{rect:viewport,radius:0.0,corners:None,opacity:1.0,clip:false,
                svg_clip:None,filters:Some(Arc::from([F::Blur(0.5)]))},
                Command::FillRect{rect:Rect{x:3.25+(at%8)as f32*10.0,y:2.75+(at/8)as f32*10.0,width:1.5,height:1.25},color:Rgba{r:255,g:0,b:0,a:192}},Command::PopLayer]);
        }
        for scale in [1.0,1.25,2.0] {
            let mut bytes=0;
            let output=resolve_layers_region(&source.0,scale,Some(&font),&mut GlyphCache::default(),&mut bytes,false,Some(viewport)).unwrap();
            assert!(bytes<64*1024,"actual tiny outputs, not viewport buffers: scale={scale} bytes={bytes}");
            assert!(output.0.iter().all(|command|matches!(command,Command::Image{image,..}if image.width<=16 && image.height<=16)),"small SourceGraphic determines each output tile");
            let complete=rasterize_layers(&source,scale,&font,&mut GlyphCache::default()).unwrap();
            assert_exact_spatial_pixels(&render_with_font(&output,90,60,scale,true,&font).unwrap(),
                &render_with_font(&complete,90,60,scale,true,&font).unwrap(),&format!("unclipped tiny groups scale={scale}"));
        }
    }

    #[test]
    fn specification_filter_repeated_shadow_dependencies_are_shared_and_bounded() {
        use lumen_common::filter::{FilterOperation as F,DropShadowFilter};
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let filters:Vec<_>=(0..32).map(|_|F::DropShadow(Arc::new(DropShadowFilter{
            offset:[0.5,0.5],sigma:0.0,color:lumen_common::color::Color::rgba8([0,255,0,32])}))).collect();
        let region=FilterPixelRegion{left:0,top:0,width:32,height:32};
        let (graph,_)=FilterRegionGraph::build(&filters,region,1.0,0).unwrap();
        assert!(graph.nodes.len()<=(33*34)/2,"shared repeated-offset dependency graph: {}",graph.nodes.len());
        let viewport=Rect{x:0.0,y:0.0,width:20.0,height:20.0};
        let source=DisplayList(vec![Command::FillRect{rect:viewport,color:Rgba{r:255,g:255,b:255,a:255}},
            Command::PushLayer{rect:viewport,radius:0.0,corners:None,opacity:1.0,clip:false,svg_clip:None,
                filters:Some(Arc::from(filters))},
            Command::FillRect{rect:Rect{x:1.25,y:2.5,width:1.5,height:1.25},color:Rgba{r:255,g:0,b:0,a:192}},
            Command::PopLayer]);
        for scale in [1.0,1.25] {
            let complete=rasterize_layers(&source,scale,&font,&mut GlyphCache::default()).unwrap();
            assert_exact_spatial_pixels(&render_with_font(&source,20,20,scale,true,&font).unwrap(),
                &render_with_font(&complete,20,20,scale,true,&font).unwrap(),
                &format!("32 positive fractional shadows preserve ordered complete-source output scale={scale}"));
        }
        let divergent:Vec<_>=(0..32).map(|at|F::DropShadow(Arc::new(DropShadowFilter{
            offset:[2.0f32.powi(at),0.0],sigma:0.0,color:lumen_common::color::Color::rgba8([0,255,0,32])}))).collect();
        assert!(matches!(FilterRegionGraph::build(&divergent,FilterPixelRegion{width:1,height:1,..region},1.0,0),
            Err(ImageError::TooLarge)),"divergent windows exhaust the existing metadata budget before pixel evaluation");
    }

    #[test]
    fn specification_filter_nested_source_scratch_shares_live_dependency_budget() {
        use lumen_html::paint::{BoxShadow,SvgPaint,SvgFillRule};
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let region=FilterPixelRegion{left:0,top:0,width:400,height:400};
        let rect=region.rect(1.0);let color=Rgba{r:255,g:0,b:0,a:255};
        let sources=[Command::BoxShadow{rect:Rect{x:20.0,y:20.0,width:360.0,height:360.0},radius:0.0,corners:None,
            shadow:BoxShadow{offset_x:0.0,offset_y:0.0,blur:8.0,spread:0.0,color,inset:false}},
            Command::SvgPath{bounds:rect,data:Arc::from("M0 0H400V400H0Z"),transform:Affine::IDENTITY,
                fill:Some(SvgPaint::Color(color)),stroke:None,stroke_width:0.0,fill_rule:SvgFillRule::NonZero,clips:Arc::from([])}];
        for source in sources {
            let source=core::slice::from_ref(&source);
            let image=render_filter_region(source,&[],region,1.0,Some(&font),&mut GlyphCache::default(),0,false).unwrap();
            assert!(image.pixels.chunks_exact(4).any(|pixel|pixel[3]!=0));
            let reserved=MAX_LAYER_BYTES-region.bytes().unwrap()-64*1024;
            assert!(matches!(render_filter_region(source,&[],region,1.0,Some(&font),&mut GlyphCache::default(),reserved,false),
                Err(ImageError::TooLarge)),"source image, nested shadow/SVG scratch and retained dependencies share the existing budget");
        }
    }

    #[test]
    fn specification_filter_device_origin_culls_extreme_glyphs_without_integer_wrap() {
        let coverage=GlyphCoverage{x_min:0,y_min:0,width:1,height:1,alpha:vec![255]};
        let clip=Rect{x:-f32::MAX/4.0,y:-f32::MAX/4.0,width:f32::MAX/2.0,height:f32::MAX/2.0};
        for origin in [[0,0],[i64::MAX,i64::MIN]] {
            for position in [(f32::MAX,f32::MAX),(-f32::MAX,-f32::MAX)] {
                let mut image=Rgba8Image{width:1,height:1,pixels:vec![0;4]};
                blit_glyph(&mut image,2.0,origin,Some(clip),position,Rgba{r:255,g:0,b:0,a:255},&coverage);
                assert_eq!(image.pixels,vec![0;4]);
            }
        }
        assert!(FilterPixelRegion{left:i64::MAX,top:0,width:1,height:1}.padded(0).is_err());
        assert!(FilterPixelRegion{left:i64::MAX-1,top:0,width:1,height:1}.padded(1).is_err());
    }

    #[test]
    fn specification_filter_source_windows_preserve_device_phase() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let source=vec![
            Command::FillRect{rect:Rect{x:2.25,y:-3.5,width:24.5,height:78.5},color:Rgba{r:255,g:0,b:255,a:192}},
            Command::FillRect{rect:Rect{x:12.75,y:17.25,width:31.5,height:60.5},color:Rgba{r:0,g:255,b:255,a:128}},
        ];
        let full=FilterPixelRegion{left:-3,top:-8,width:86,height:131};
        let window=FilterPixelRegion{left:-3,top:93,width:86,height:30};
        let complete=render_filter_region(&source,&[],full,1.25,Some(&font),&mut GlyphCache::default(),0,false).unwrap();
        let expected=crop_filter_region(&complete,full,window,0).unwrap();
        let actual=render_filter_region(&source,&[],window,1.25,Some(&font),&mut GlyphCache::default(),0,false).unwrap();
        assert_exact_spatial_pixels(&actual,&expected,"unfiltered source device phase before convolution");
    }

    #[test]
    fn specification_filter_result_clip_preserves_device_phase_across_windows() {
        use lumen_common::filter::FilterOperation as F;
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let viewport=Rect{x:0.0,y:0.0,width:64.0,height:96.0};
        let clip=Rect{x:4.15,y:18.35,width:40.33,height:59.27};
        for (radius,corners) in [(0.0,None),(2.7,None),(0.0,Some(Arc::new([[1.3,2.7],[4.15,1.8],[2.2,3.7],[1.8,2.3]])))] {
            let source=DisplayList(vec![Command::FillRect{rect:viewport,color:Rgba{r:255,g:255,b:255,a:255}},
                Command::PushLayer{rect:clip,radius,corners,opacity:0.75,clip:true,svg_clip:None,filters:Some(Arc::from([F::Blur(0.5)]))},
                Command::FillRect{rect:Rect{x:2.25,y:-3.5,width:48.5,height:87.5},color:Rgba{r:255,g:0,b:255,a:192}},
                Command::FillRect{rect:Rect{x:12.75,y:17.25,width:31.5,height:60.5},color:Rgba{r:0,g:255,b:255,a:128}},Command::PopLayer]);
            for scale in [1.0,1.25,2.0] {
                let complete=rasterize_layers(&source,scale,&font,&mut GlyphCache::default()).unwrap();
                assert_exact_spatial_pixels(&render_with_font(&source,64,96,scale,true,&font).unwrap(),
                    &render_with_font(&complete,64,96,scale,true,&font).unwrap(),
                    &format!("fractional square/rounded result clip radius={radius} scale={scale}"));
            }
        }
    }

    #[test]
    fn specification_filter_visible_tiles_match_complete_source_capture_without_seams() {
        use lumen_common::filter::{FilterOperation as F,ColorFilter as C,DropShadowFilter};
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let viewport=Rect{x:0.0,y:0.0,width:64.0,height:96.0};
        for (filter_case,filters) in [vec![F::Blur(0.5)],vec![F::Blur(3.0)],
            vec![F::Blur(0.5),F::DropShadow(Arc::new(DropShadowFilter{offset:[38.5,17.25],sigma:0.5,
                color:lumen_common::color::Color::rgba8([0,0,255,192])})),F::Color(C::Invert(0.75)),F::Color(C::Opacity(0.8))]].into_iter().enumerate() {
            let source=DisplayList(vec![
                Command::FillRect{rect:viewport,color:Rgba{r:255,g:255,b:255,a:255}},
                Command::PushClip(Rect{x:0.25,y:0.75,width:60.5,height:90.5}),
                Command::PushLayer{svg_clip:None,filters:Some(Arc::from(filters)),corners:None,
                    rect:viewport,radius:0.0,opacity:0.75,clip:false},
                Command::FillRect{rect:Rect{x:2.25,y:-3.5,width:24.5,height:78.5},color:Rgba{r:255,g:0,b:255,a:192}},
                Command::FillRect{rect:Rect{x:12.75,y:17.25,width:31.5,height:60.5},color:Rgba{r:0,g:255,b:255,a:128}},
                Command::PopLayer,Command::PopClip,
            ]);
            for scale in [1.0,1.25,2.0] {
                let complete=rasterize_layers(&source,scale,&font,&mut GlyphCache::default()).unwrap();
                let expected=render_with_font(&complete,64,96,scale,true,&font).unwrap();
                let actual=render_with_font(&source,64,96,scale,true,&font).unwrap();
                assert_exact_spatial_pixels(&actual,&expected,&format!("complete capture / visible device rows case={filter_case} scale={scale}"));
                let quads=rasterize_for_gpui_in_region(&source,scale,&font,&mut GlyphCache::default(),false,viewport).unwrap();
                assert_exact_spatial_pixels(&render_with_font(&quads,64,96,scale,true,&font).unwrap(),&expected,
                    &format!("shared quad output windows scale={scale}"));
            }
        }
    }

    #[test]
    fn specification_filter_output_windows_bound_large_root_and_fixed_source() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let root="<style>html,body{margin:0}html{filter:blur(0px)}div{width:2000px;height:2000px;background:lightgreen}i{position:fixed;left:150px;top:100px;width:100px;height:100px;background:green}</style><div></div><i></i>";
        let reference=root.replace("filter:blur(0px)","filter:none");
        assert_exact_spatial_pixels(&render_html_with_font(root,800,600,1.0,&font).unwrap(),
            &render_html_with_font(&reference,800,600,1.0,&font).unwrap(),"root SourceGraphic2000 square visible output800x600");
        let fixed="<style>html,body{margin:0}html{overflow:hidden}div{position:fixed;left:-50px;top:-50px;width:900px;height:700px;background:green;filter:blur(10px)}</style><div></div>";
        let reference="<style>html,body{margin:0;background:green}</style>";
        let actual=render_html_with_font(fixed,800,600,1.0,&font).unwrap();
        assert_exact_spatial_pixels(&actual,&render_html_with_font(reference,800,600,1.0,&font).unwrap(),
            "uniform SourceGraphic extends50px outside viewport, farther than blur support");
        let document=lumen_html::html::parse(fixed,64).unwrap();
        let list=lumen_html::layout::display_list(&document,800,600,&font).unwrap();
        let viewport=Rect{x:0.0,y:0.0,width:800.0,height:600.0};
        let mut bytes=0;
        let resolved=resolve_layers_region(&list.0,1.0,Some(&font),&mut GlyphCache::default(),&mut bytes,false,Some(viewport)).unwrap();
        assert!(bytes<=MAX_LAYER_BYTES,"retained output uses the existing total layer budget");
        assert!(resolved.0.iter().any(|command|matches!(command,Command::Image{image,..}if image.height<=32)),
            "large source resolves through bounded device rows");
        assert_exact_spatial_pixels(&render_with_font(&resolved,800,600,1.0,true,&font).unwrap(),&actual,"resolved row replay");
    }

    #[test]
    fn specification_displaced_filter_shadow_has_independent_source_window_without_gap_allocation() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let source="<style>html,body{margin:0}div{position:relative;left:-1000px;top:-1000px;width:300px;height:300px;background:red;filter:drop-shadow(1000px 1000px 0 green)}</style><div></div>";
        let reference="<style>html,body{margin:0}div{width:300px;height:300px;background:green}</style><div></div>";
        for scale in [1.0,1.25,2.0] {
            assert_exact_spatial_pixels(&render_html_with_font(source,800,600,scale,&font).unwrap(),
                &render_html_with_font(reference,800,600,scale,&font).unwrap(),&format!("offscreen300px source / displaced shadow scale={scale}"));
        }
        let document=lumen_html::html::parse(source,64).unwrap();
        let list=lumen_html::layout::display_list(&document,800,600,&font).unwrap();
        let mut bytes=0;
        resolve_layers_region(&list.0,1.0,Some(&font),&mut GlyphCache::default(),&mut bytes,false,
            Some(Rect{x:0.0,y:0.0,width:800.0,height:600.0})).unwrap();
        assert!(bytes<=300*300*4,"no retained transparent gap between -1000px source and visible shadow: {bytes}");
    }

    #[test]
    fn specification_wrapped_inline_spatial_source_matches_one_complete_group() {
        use lumen_common::filter::FilterOperation as F;
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let plain="<style>html,body{margin:0}main{width:24px;font:12px/8px sans-serif}#host{background:magenta;color:transparent}</style><main><span id=host>AA AA AA AA</span></main>";
        let document=lumen_html::html::parse(plain,128).unwrap();
        let unfiltered=lumen_html::layout::display_list(&document,64,100,&font).unwrap();
        let magenta=Rgba{r:255,g:0,b:255,a:255};
        let mut ink=Vec::new();let mut background=Vec::new();
        for command in unfiltered.0 {match &command {
            Command::FillRect{color,..} if *color==magenta=>ink.push(command),
            _=>background.push(command)
        }}
        assert!(ink.len()>=2,"independent source contains multiple actual wrapped fragments");
        for filters in [vec![F::Blur(0.5)],vec![F::Blur(0.5),F::DropShadow(Arc::new(lumen_common::filter::DropShadowFilter{
            offset:[2.0,1.0],sigma:0.5,color:lumen_common::color::Color::rgba8([0,0,255,255])}))]] {
            let declaration=if filters.len()==1 {"filter:blur(.5px)"}else{"filter:blur(.5px) drop-shadow(2px 1px .5px blue)"};
            let source=plain.replace("#host{",&format!("#host{{{declaration};"));
            let mut commands=background.clone();
            commands.push(Command::PushLayer{svg_clip:None,filters:Some(Arc::from(filters)),corners:None,
                rect:Rect{x:0.0,y:0.0,width:64.0,height:100.0},radius:0.0,opacity:1.0,clip:false});
            commands.extend(ink.iter().cloned());commands.push(Command::PopLayer);
            let expected=DisplayList(commands);
            for scale in [1.0,1.25,2.0] {
                let mut snapped=expected.clone();
                snap::boxes(&mut snapped,scale);
                let expected=render_with_font(&snapped,64,100,scale,true,&font).unwrap();
                let actual=render_html_with_font(&source,64,100,scale,&font).unwrap();
                assert_exact_spatial_pixels(&actual,&expected,&format!("{declaration}/scale={scale}: complete overlapping fragment source"));
            }
        }
    }

    #[test]
    fn specification_collapsed_table_tracks_preserve_the_complete_winning_border_surface() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for direction in ["ltr", "rtl"] {
            let source=format!("<style>html,body{{margin:0}}td{{border:50px solid green;padding:0}}</style><table style='border-collapse:collapse;background:red;direction:{direction}'><col style='visibility:collapse'><tr style='visibility:collapse'><td></td><td></td></tr><tr><td></td><td></td></tr></table>");
            let document=lumen_html::html::parse(&source,64).unwrap();
            let mut session=lumen_html::session::RenderSession::new(document);
            let list=session.display_list(120,120,&font).unwrap().clone();
            let image=render_with_font(&list,120,120,1.0,true,&font).unwrap();
            for y in 0..120usize {for x in 0..120usize {
                let expected=if x<100 && y<100 {[0,128,0,255]}else{[255,255,255,255]};
                assert_eq!(&image.pixels[(y*120+x)*4..(y*120+x)*4+4],&expected,
                    "{direction}: collapsed tracks preserve the winning outer border at ({x},{y})");
            }}
            assert_eq!(list,session.display_list(120,120,&font).unwrap().clone());
            assert_eq!(list,lumen_html::layout::display_list(session.document(),120,120,&font).unwrap());
        }
    }

    #[test]
    fn specification_collapsed_table_hidden_winner_keeps_only_the_visible_neighbor_half() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for direction in ["ltr", "rtl"] {
          for (left,top,offset) in [(0.0,0.0,0usize),(0.25,0.875,0),(0.875,0.25,1)] {
            let source=format!("<style>html,body{{margin:0}}table{{border-collapse:collapse;direction:{direction};position:relative;left:{left}px;top:{top}px}}td{{width:20px;height:20px;padding:0;border:10px solid green;background:green}}#hidden{{visibility:hidden;border-color:red}}</style><table><tr><td id=hidden></td><td></td></tr></table>");
            let document=lumen_html::html::parse(&source,64).unwrap();
            let hidden=lumen_html::selector::get_element_by_id(&document,document.root(),"hidden").unwrap().unwrap();
            let mut session=lumen_html::session::RenderSession::new(document);
            let list=session.display_list(80,50,&font).unwrap().clone();
            let image=render_with_mode_cached(&list,80,50,1.0,RasterizationMode::CssPixelSnapped,&font,&mut GlyphCache::default()).unwrap();
            for x in (30+offset)..(40+offset) {
                let visible=if direction=="ltr" {x>=35+offset}else{x<35+offset};
                let expected=if visible {[255,0,0,255]}else{[255,255,255,255]};
                assert_eq!(&image.pixels[(15*80+x)*4..(15*80+x)*4+4],&expected,
                    "{direction}/{left},{top}: a hidden style winner still harmonizes the visible cell border at x={x}");
            }
            session.document_mut().set_attribute(hidden,"style","visibility:visible").unwrap();
            let changed=session.display_list(80,50,&font).unwrap().clone();
            assert_eq!(changed,lumen_html::layout::display_list(session.document(),80,50,&font).unwrap());
            let image=render_with_mode_cached(&changed,80,50,1.0,RasterizationMode::CssPixelSnapped,&font,&mut GlyphCache::default()).unwrap();
            for x in (30+offset)..(40+offset) {
                assert_eq!(&image.pixels[(15*80+x)*4..(15*80+x)*4+4],&[255,0,0,255],
                    "{direction}: both visible cells own the complete harmonized border");
            }
          }
        }
    }

    #[test]
    fn specification_color_filter_inline_and_svg_groups_share_host_compositor() {
        let font = FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let inline = "<style>body{margin:0;font-size:0;line-height:0}#host{filter:invert()}i{display:inline-block;width:20px;height:10px;background:magenta}i+i{margin-left:-10px;background:cyan}</style><span id=host><i></i><i></i></span>";
        let reference = inline.replace("filter:invert()", "filter:none").replace("background:magenta", "background:lime").replace("background:cyan", "background:red");
        for scale in [1.0, 2.0] {
            for nested in [false, true] {
                let source = if nested { inline.replace("<span id=host>", "<span><span id=host>").replace("</span>", "</span></span>") } else { inline.into() };
                let expected = if nested { reference.replace("<span id=host>", "<span><span id=host>").replace("</span>", "</span></span>") } else { reference.clone() };
                assert_eq!(render_html_with_font(&source, 40, 20, scale, &font).unwrap(), render_html_with_font(&expected, 40, 20, scale, &font).unwrap(), "ordinary inline filter owns its overlapping atomic children: nested={nested}, scale={scale}");
            }
        }
        for declaration in ["filter='invert()'", "style='filter:invert()'"] {
            let source = format!("<style>body{{margin:0}}</style><svg width=20 height=10><g {declaration}><rect width=20 height=10 fill='magenta'/><rect x=10 width=10 height=10 fill='cyan'/></g></svg>");
            let reference = "<style>body{margin:0}</style><svg width=20 height=10><rect width=20 height=10 fill='lime'/><rect x=10 width=10 height=10 fill='red'/></svg>";
            let document = lumen_html::html::parse(&source, 64).unwrap();
            let mut session = lumen_html::session::RenderSession::new(document);
            assert!(session.unsupported_svg_features().unwrap().is_empty(), "{declaration}: SVG capability");
            let svg = lumen_html::selector::query_selector(session.document(), session.document().root(), "svg").unwrap().unwrap();
            let group = lumen_html::selector::query_selector(session.document(), session.document().root(), "g").unwrap().unwrap();
            let first_rect = lumen_html::selector::query_selector(session.document(), session.document().root(), "rect").unwrap().unwrap();
            let group_style = session.computed_style(group).unwrap();
            assert_eq!(group_style.filters.as_deref(), Some(&[lumen_common::filter::FilterOperation::Color(lumen_common::filter::ColorFilter::Invert(1.0))][..]), "{declaration}: computed group filter");
            assert_eq!(session.computed_style(first_rect).unwrap().svg_fill, lumen_html::css::SvgPaint::Color(Rgba { r: 255, g: 0, b: 255, a: 255 }), "{declaration}: first rectangle fill");
            let svg_style = session.computed_style(svg).unwrap();
            assert_eq!((svg_style.width, svg_style.height), (Some(20.0), Some(10.0)), "{declaration}: SVG viewport");
            let list = lumen_html::layout::display_list(session.document(), 40, 20, &font).unwrap();
            assert!(list.0.iter().any(|command| matches!(command, Command::PushLayer { filters: Some(filters), .. } if filters.as_ref() == [lumen_common::filter::FilterOperation::Color(lumen_common::filter::ColorFilter::Invert(1.0))])), "{declaration}: filter group command: {list:?}");
            assert!(list.0.iter().any(|command| matches!(command, Command::SvgPath { .. })), "{declaration}: SVG shape commands: {list:?}");
            assert_eq!(render_html_with_font(&source, 40, 20, 1.0, &font).unwrap(), render_html_with_font(reference, 40, 20, 1.0, &font).unwrap(), "{declaration}: presentation and CSS filters use the same group/cascade");
        }
    }

    #[test]
    fn specification_root_color_filter_owns_canvas_background_and_content_once() {
        let font = FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for source in ["html", "body"] {
            let filtered = format!("<style>html{{filter:invert()}}{source}{{background:magenta}}body{{margin:0}}#content{{width:20px;height:10px;background:cyan}}</style><div id=content></div>");
            let reference = format!("<style>{source}{{background:lime}}body{{margin:0}}#content{{width:20px;height:10px;background:red}}</style><div id=content></div>");
            for scale in [1.0, 1.25, 2.0] {
                assert_eq!(render_html_with_font(&filtered, 40, 20, scale, &font).unwrap(), render_html_with_font(&reference, 40, 20, scale, &font).unwrap(), "{source}: root filter includes viewport canvas and root content once");
            }
        }
        let filtered = "<style>html{filter:invert();transform:translateX(10px)}body{margin:0;background:magenta}div{width:10px;height:10px;background:cyan}</style><div></div>";
        let reference = "<style>html{transform:translateX(10px)}body{margin:0;background:lime}div{width:10px;height:10px;background:red}</style><div></div>";
        assert_eq!(render_html_with_font(filtered, 40, 20, 1.0, &font).unwrap(), render_html_with_font(reference, 40, 20, 1.0, &font).unwrap(), "canvas remains untransformed while content is translated before filtering");
    }

    #[test]
    fn specification_generated_flex_full_document_sources_preserve_head_and_body_context() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for (name,source,reference) in [
            ("001",include_str!("../tests/fixtures/flexbox-with-pseudo-elements-001.html"),include_str!("../tests/fixtures/flexbox-with-pseudo-elements-001-ref.html")),
            ("003",include_str!("../tests/fixtures/flexbox-with-pseudo-elements-003.html"),include_str!("../tests/fixtures/flexbox-with-pseudo-elements-003-ref.html")),
        ] {
            let actual=render_html_with_font(source,800,600,1.0,&font).unwrap();
            let expected=render_html_with_font(reference,800,600,1.0,&font).unwrap();
            let different=actual.pixels.chunks_exact(4).zip(expected.pixels.chunks_exact(4)).filter(|(a,b)|a!=b).count();
            assert_eq!(different,0,"full upstream source {name} retains generated items through document and font layout");
        }
    }

    #[test]
    fn specification_generated_flex_default_font_sources_retain_three_container_paint_order() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for (before_display,after_display) in [("inline","inline"),("table-row","table-cell")] {
            let shared=format!(".flexContainer{{display:flex;align-items:flex-end;justify-content:space-between;height:50px;width:300px;margin-bottom:2px;background:lightgray}}div.withBefore::before,.fakeBefore{{display:{before_display};align-self:center;background:yellow;order:1}}div.withAfter::after,.fakeAfter{{display:{after_display};align-self:center;background:lightblue;order:-1}}");
            let generated=format!("<!doctype html><style>{shared}div.withBefore::before{{content:'b'}}div.withAfter::after{{content:'a'}}</style><div class='flexContainer withBefore'>\n x\n <div>y</div>\n z\n</div><div class='flexContainer withAfter'>\n x\n <div>y</div>\n z\n</div><div class='flexContainer withBefore withAfter'>\n x\n <div>y</div>\n z\n</div>");
            let reference=format!("<!doctype html><style>{shared}</style><div class=flexContainer><div class=fakeBefore>b</div>\n x\n <div>y</div>\n z\n</div><div class=flexContainer>\n x\n <div>y</div>\n z\n<div class=fakeAfter>a</div></div><div class=flexContainer><div class=fakeBefore>b</div>\n x\n <div>y</div>\n z\n<div class=fakeAfter>a</div></div>");
            for scale in [1.0,1.25,2.0] {
                assert_eq!(render_html_with_font(&generated,400,200,scale,&font).unwrap(),render_html_with_font(&reference,400,200,scale,&font).unwrap(),"default-font principal source retains physical paint order: {before_display}/{after_display}/{scale}");
            }
        }
    }

    #[test]
    fn specification_svg_paint_opacity_shares_solid_gradient_text_and_separate_stroke_operations() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let definitions="<defs><linearGradient id=red><stop stop-color='red'/><stop offset=1 stop-color='red'/></linearGradient><linearGradient id=blue><stop stop-color='blue'/><stop offset=1 stop-color='blue'/></linearGradient></defs>";
        for declaration in ["fill-opacity:.5;stroke-opacity:.25","fill-opacity:50%;stroke-opacity:calc(50% - 25%)"] {
            for paint in ["red","url(#red)"] {
                let source=format!("<style>body{{margin:0}}</style><svg width=100 height=80>{definitions}<g style='{declaration}'><rect x=10 y=10 width=40 height=30 fill='{paint}' stroke='url(#blue)' stroke-width='8'/><text x=5 y=65 fill=red font-size=20>XO</text></g></svg>");
                let reference="<style>body{margin:0}</style><svg width=100 height=80><rect x=10 y=10 width=40 height=30 fill='rgba(255,0,0,.5)' stroke='rgba(0,0,255,.25)' stroke-width='8'/><text x=5 y=65 fill='rgba(255,0,0,.5)' font-size=20>XO</text></svg>";
                for scale in [1.0,1.25,2.0] {
                    let actual=render_html_with_font(&source,100,80,scale,&font).unwrap();
                    let expected=render_html_with_font(reference,100,80,scale,&font).unwrap();
                    let differences=actual.pixels.chunks_exact(4).zip(expected.pixels.chunks_exact(4)).enumerate().filter(|(_, (a,b))|a!=b).map(|(index,(a,b))|(index,a,b)).collect::<Vec<_>>();
                    assert!(actual==expected,"separate paint opacity preserves inherited glyph color, gradient alpha and overlapping fill/stroke: {declaration}; {paint}; {scale}; different pixels {}; first {:?}",differences.len(),&differences[..differences.len().min(8)]);
                }
            }
        }
    }

    #[test]
    fn specification_grid_item_surfaces_follow_inline_block_paint_phases() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let base="html,body{margin:0}.grid{display:grid;width:40px;height:20px;grid-template-columns:20px 20px;grid-template-rows:20px;background:gray}.first{background:yellow;order:1}.last{background:blue;order:-1}";
        let reference="<!doctype html><style>html,body{margin:0}div{position:absolute;top:0;width:20px;height:20px}.left{left:0;background:blue}.right{left:20px;background:yellow}</style><div class=left></div><div class=right></div>";
        for generated in [false,true] {
            let source=if generated {
                format!("<!doctype html><style>{base}.grid::before{{content:'';background:yellow;order:1}}.grid::after{{content:'';background:blue;order:-1}}</style><div class=grid></div>")
            } else {
                format!("<!doctype html><style>{base}</style><div class=grid><div class=first></div><div class=last></div></div>")
            };
            let mut session=lumen_html::session::RenderSession::new(lumen_html::html::parse(&source,64).unwrap());
            let actual=session.display_list(40,20,&font).unwrap().clone();
            assert_eq!(actual,session.display_list(40,20,&font).unwrap().clone());
            assert_eq!(actual,lumen_html::layout::display_list(session.document(),40,20,&font).unwrap());
            for scale in [1.0,1.25,2.0] {
                let mut painted=actual.clone();snap::boxes(&mut painted,scale);
                assert_exact_spatial_pixels(&render_with_font(&painted,40,20,scale,true,&font).unwrap(),&render_html_with_font(reference,40,20,scale,&font).unwrap(),&format!("grid item phase generated={generated} scale={scale}"));
            }
        }
        // A formatting owner groups block surfaces, while positioned children
        // escape to the ancestor unless the item creates a real context.
        for item_context in [false,true] {
            let context=if item_context {"z-index:0"} else {""};
            let source=format!("<!doctype html><style>html,body{{margin:0}}.grid{{position:relative;display:grid;width:40px;height:20px;background:gray;grid-template-columns:40px;grid-template-rows:20px}}.item{{background:yellow;{context}}}.positive,.negative{{position:absolute;inset:0;width:40px;height:20px}}.positive{{background:lime;z-index:1}}.negative{{background:red;z-index:-1}}</style><div class=grid><div class=item><div class=negative></div><div class=positive></div></div></div>");
            let reference="<!doctype html><style>html,body{margin:0}div{width:40px;height:20px;background:lime}</style><div></div>";
            for scale in [1.0,1.25,2.0] {
                assert_exact_spatial_pixels(&render_html_with_font(&source,40,20,scale,&font).unwrap(),&render_html_with_font(reference,40,20,scale,&font).unwrap(),&format!("positioned grid descendants item context={item_context} scale={scale}"));
            }
            let overlap=format!("<!doctype html><style>html,body{{margin:0}}.grid{{position:relative;display:grid;width:40px;height:20px;background:gray;grid-template-columns:40px;grid-template-rows:20px}}.item,.sibling{{grid-area:1/1}}.item{{background:yellow;{context}}}.positive{{position:absolute;inset:0;width:40px;height:20px;background:lime;z-index:2}}.sibling{{background:blue;z-index:1}}</style><div class=grid><div class=item><div class=positive></div></div><div class=sibling></div></div>");
            let top=if item_context {"blue"} else {"lime"};
            let reference=format!("<!doctype html><style>html,body{{margin:0}}div{{width:40px;height:20px;background:{top}}}</style><div></div>");
            for scale in [1.0,1.25,2.0] {
                assert_exact_spatial_pixels(&render_html_with_font(&overlap,40,20,scale,&font).unwrap(),&render_html_with_font(&reference,40,20,scale,&font).unwrap(),&format!("grid descendant escapes formatting owner but respects real context={item_context} scale={scale}"));
            }
        }
    }

    #[test]
    fn specification_generated_flex_grid_source_paint_matches_principal_boxes() {
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        for pseudo in ["before","after"] {
            for display in ["flex","inline-flex","grid","inline-grid"] {
                for sizing in ["width:80px;height:60px","width:min-content","width:80px;aspect-ratio:2"] {
                    let shared=format!("html,body{{margin:0}}#host{{font:10px/16px serif;width:240px}}#container,#host::{pseudo}{{display:{display};{sizing};background:green;padding:3px;border:2px solid blue;justify-content:center;align-items:center;grid-template-columns:70px;gap:5px;opacity:.75;transform:translate(.125px,.875px);filter:blur(.5px)}}#next{{font:10px/16px serif}}");
                    let generated=format!("<!doctype html><style>{shared}#host::{pseudo}{{content:'A B C'}}</style><div id='host'></div><div id='next'>END</div>");
                    let reference=format!("<!doctype html><style>{shared}</style><div id='host'><div id='container'>A B C</div></div><div id='next'>END</div>");
                    for scale in [1.0,1.25,2.0] {
                        assert_eq!(render_html_with_font(&generated,280,160,scale,&font).unwrap(),
                            render_html_with_font(&reference,280,160,scale,&font).unwrap(),
                            "generated principal and canonical anonymous item retain paint/effect coordinates: {pseudo}; {display}; {sizing}; {scale}");
                    }
                }
            }
        }
    }

    #[test]
    fn specification_fractional_glyph_translation_shares_global_baseline_and_cached_mask() {
        use lumen_html::paint::Affine;
        let font=FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let run=font.shape("XO",40.0).unwrap();
        for scale in [1.0,1.25,2.0] {
            for shift in [-0.625,-0.125,0.125,0.875] {
                for alpha in [128,255] {
                    let matrix=Affine{e:8.125+shift,f:9.875+shift,..Affine::IDENTITY};
                    let command=Command::GlyphRun{origin_x:8.25,baseline_y:48.875,size:40.0,
                        color:Rgba{r:0,g:128,b:0,a:alpha},glyphs:run.glyphs.clone()};
                    let translated=DisplayList(vec![Command::PushTransform(matrix),command.clone(),Command::PopTransform]);
                    let mut direct=command;translate_command(&mut direct,matrix.e,matrix.f);
                    let direct=DisplayList(vec![direct]);
                    let mut cache=GlyphCache::default();let mut bytes=0;
                    assert_eq!(resolve_layers(&translated.0,scale,Some(&font),&mut cache,&mut bytes).unwrap(),direct);
                    assert_eq!(bytes,0,"translation retains the canonical glyph mask without a sprite");
                    for mode in [RasterizationMode::Antialiased,RasterizationMode::CssPixelSnapped,RasterizationMode::Gpui] {
                        let expected=render_with_mode_cached(&direct,128,96,scale,mode,&font,&mut GlyphCache::default()).unwrap();
                        let cold=render_with_mode_cached(&translated,128,96,scale,mode,&font,&mut cache).unwrap();
                        let warm=render_with_mode_cached(&translated,128,96,scale,mode,&font,&mut cache).unwrap();
                        assert_eq!(cold,expected,"phase={shift}; scale={scale}; alpha={alpha}; mode={mode:?}");
                        assert_eq!(warm,cold,"retained glyph cache preserves placement and source coverage");
                    }
                    let full=render_scaled_region(&direct,128,96,scale,Some(&font),&mut cache).unwrap();
                    let mut joined=Rgba8Image{width:128,height:96,pixels:vec![0;128*96*4]};
                    for top in (0..96).step_by(16) {
                        let region=Rect{x:0.0,y:top as f32/scale,width:128.0/scale,height:16.0/scale};
                        let mut scene=SourceScopeAnalysis::new(&translated.0,0).unwrap();let mut bytes=scene.bytes().unwrap();
                        let tile_source=resolve_layers_region_in_operation(&translated.0,scale,Some(&font),&mut cache,&mut bytes,false,
                            Some(region),&mut scene,SourceScopeView::default()).unwrap();
                        let tile=render_scaled_region_at(&tile_source,128,16,scale,Some(&font),&mut cache,[0,top as i64],None).unwrap();
                        joined.pixels[top*128*4..(top+16)*128*4].copy_from_slice(&tile.pixels);
                    }
                    assert_eq!(joined,full,"ROI device origins preserve the same global glyph baseline: scale={scale}; shift={shift}; alpha={alpha}");
                }
            }
        }
    }

    #[test]
    fn specification_positive_axis_image_transform_paints_viewport_without_oversized_surface() {
        let image=Arc::new(ImageData {width:2,height:2,pixels:vec![
            255,0,0,255, 0,255,0,255, 0,0,255,255, 255,255,255,255]});
        let source=Rect {x:-300.0,y:0.0,width:1600.0,height:1200.0};
        let transformed=DisplayList(vec![
            Command::PushTransform(lumen_html::paint::Affine {a:0.5,d:0.5,e:150.0,..Default::default()}),
            Command::PushClip(source),Command::Image {rect:source,image:image.clone()},
            Command::PopClip,Command::PopTransform]);
        let expected=DisplayList(vec![Command::Image {
            rect:Rect{x:0.0,y:0.0,width:800.0,height:600.0},image}]);
        for scale in [1.0,1.25] {
            let actual=render(&transformed,800,600,scale,true).expect("positive image scaling paints directly within viewport budget");
            assert_eq!(actual,render(&expected,800,600,scale,true).unwrap(),"image sampling and alpha stay equivalent at each device scale");
        }
    }

    #[test]
    fn specification_positive_axis_background_transform_preserves_tiles_without_oversized_surface() {
        use lumen_html::paint::{Affine,BackgroundFill,BackgroundPaint,BackgroundRepeat};
        let image=Arc::new(ImageData {width:2,height:2,pixels:vec![
            255,0,0,255, 0,255,0,128, 0,0,255,255, 255,255,255,128]});
        for repeat in [BackgroundRepeat::Repeat,BackgroundRepeat::NoRepeat,BackgroundRepeat::Space,BackgroundRepeat::Round] {
            for vertical_scale in [0.5,0.75] {
                let matrix=Affine {a:0.5,d:vertical_scale,e:150.0,..Default::default()};
                let source=BackgroundFill {
                    corners:None,radius:0.0,
                    rect:Rect{x:-300.0,y:0.0,width:1600.0,height:1200.0},
                    positioning_rect:Rect{x:50.0,y:40.0,width:400.0,height:300.0},
                    image_rect:Rect{x:-300.0,y:0.0,width:200.0,height:200.0},
                    repeat:[repeat;2],image:BackgroundPaint::Image(image.clone()),
                };
                let expected=BackgroundFill {
                    rect:Rect{x:0.0,y:0.0,width:800.0,height:1200.0*vertical_scale},
                    positioning_rect:Rect{x:175.0,y:40.0*vertical_scale,width:200.0,height:300.0*vertical_scale},
                    image_rect:Rect{x:0.0,y:0.0,width:100.0,height:200.0*vertical_scale},
                    ..source.clone()
                };
                let transformed=DisplayList(vec![Command::PushTransform(matrix),
                    Command::FillBackground(Box::new(source)),Command::PopTransform]);
                let expected=DisplayList(vec![Command::FillBackground(Box::new(expected))]);
                let mut bytes=0;
                assert_eq!(resolve_layers(&transformed.0,1.0,None,&mut GlyphCache::default(),&mut bytes).unwrap(),expected);
                assert_eq!(bytes,0,"tile coordinates transform without a raster plane or image copy");
                for scale in [1.0,1.25] {
                    assert_eq!(render(&transformed,800,600,scale,true).unwrap(),
                        render(&expected,800,600,scale,true).unwrap(),"repeat, clipping and alpha at device scale {scale}");
                }
            }
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
            Command::PushLayer { svg_clip: None,
                filters: None,
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
            Command::PushLayer { svg_clip: None,
                filters: None,
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
            Command::PushLayer { svg_clip: None,
                filters: None,
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
            Command::PushLayer { svg_clip: None,
                filters: None,
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
        assert!(decode_png(&corrupt).is_err());
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
    fn specification_background_text_masks_ignore_descendant_color_filter_compositing() {
        let font = FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let source = "<style>body{margin:0;background:white}div{font-size:24px;width:60px;height:30px;background:lime;background-clip:text;color:transparent}</style><div><span style='filter:opacity(.5)'>A</span></div>";
        let reference = source.replace("filter:opacity(.5)", "opacity:.5");
        for scale in [1.0, 2.0] {
            let expected = render_html_with_font(&reference, 60, 30, scale, &font).unwrap();
            assert!(expected.pixels.chunks_exact(4).any(|p| p[1] > p[0]), "the actual glyph mask paints green ink");
            assert_eq!(render_html_with_font(source, 60, 30, scale, &font).unwrap(), expected,
                "child filter-opacity and child opacity affect foreground groups, not the parent's glyph-coverage mask");
        }
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
    fn specification_background_shorthand_actual_color_invalidation_and_multilayer_clip_raster() {
        let font=FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap();let mut cache=GlyphCache::default();
        for shorthand in ["linear-gradient(transparent,transparent) 0/10px 20px no-repeat, none currentcolor","none content-box, none border-box currentcolor","rgb(from currentcolor r g b)"] {
            let markup=format!("<!doctype html><style>html,body{{margin:0}}#target{{width:30px;height:20px;padding:5px;border:3px solid transparent;background:{shorthand}}}</style><div id=target style='color:red'></div>");
            let document=lumen_html::html::parse(&markup,64).unwrap();let node=lumen_html::selector::query_selector(&document,document.root(),"#target").unwrap().unwrap();
            let mut session=lumen_html::session::RenderSession::new(document);
            for color in ["red","blue"] {
                session.document_mut().set_attribute(node,"style",&format!("color:{color}")).unwrap();
                let actual=session.display_list(100,80,&font).unwrap().clone();
                let reference=markup.replace("style='color:red'",&format!("style='color:{color}'")).replace("currentcolor",color);
                let expected=lumen_html::layout::display_list(&lumen_html::html::parse(&reference,64).unwrap(),100,80,&font).unwrap();
                assert_eq!(actual,expected,"actual currentColor source follows own color mutation: {shorthand} {color}");
                assert_eq!(actual,*session.display_list(100,80,&font).unwrap(),"stable frame replay");
                for scale in [1.0,1.25] {assert_eq!(render_with_font_cached(&actual,100,80,scale,true,&font,&mut cache).unwrap(),render_with_font_cached(&expected,100,80,scale,true,&font,&mut cache).unwrap());}
            }
        }
        let source="<!doctype html><style>html,body{margin:0}div{width:30px;height:20px;padding:5px;border:3px solid transparent;background:none content-box, none border-box green}</style><div></div>";
        let reference=source.replace("none content-box, none border-box green","green");
        let actual=lumen_html::layout::display_list(&lumen_html::html::parse(source,64).unwrap(),100,80,&font).unwrap();
        let expected=lumen_html::layout::display_list(&lumen_html::html::parse(&reference,64).unwrap(),100,80,&font).unwrap();
        assert_eq!(render_with_font_cached(&actual,100,80,1.25,true,&font,&mut cache).unwrap(),render_with_font_cached(&expected,100,80,1.25,true,&font,&mut cache).unwrap(),"last none layer's border-box clip paints beneath the transparent border");
    }

    #[test]
    fn specification_computed_inheritance_query_percentage_matches_real_declaring_container_raster() {
        let font=FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap();let mut cache=GlyphCache::default();
        for (expression,computed) in [
            ("calc(10cqw + 10%)","calc(30px + 10%)"),
            ("min(calc(10cqw + 10%), 70px)","min(calc(30px + 10%), 70px)"),
            ("clamp(20px, calc(10cqw + 10%), 60px)","clamp(20px, calc(30px + 10%), 60px)")] {
            let markup=format!("<!doctype html><style>html,body{{margin:0}}#outer{{width:300px;container-type:inline-size}}#parent{{width:200px;container-type:inline-size;margin:{expression};padding:{expression};background:blue}}#child{{width:50px;height:30px;margin:inherit;padding:inherit;background:green}}</style><div id=outer><div id=parent><div id=child></div></div></div>");
            let reference=markup.replace("margin:inherit;padding:inherit",&format!("margin:{computed};padding:{computed}"));
            let actual=lumen_html::layout::display_list(&lumen_html::html::parse(&markup,64).unwrap(),600,400,&font).unwrap();
            let expected=lumen_html::layout::display_list(&lumen_html::html::parse(&reference,64).unwrap(),600,400,&font).unwrap();
            assert_eq!(actual,expected,"parent query terms must not use the child query container: {expression}");
            for scale in [1.0,1.25] {assert_eq!(render_with_font_cached(&actual,600,400,scale,true,&font,&mut cache).unwrap(),render_with_font_cached(&expected,600,400,scale,true,&font,&mut cache).unwrap());}
        }
    }

    #[test]
    fn specification_border_logical_pairs_match_actual_physical_edges_and_raster() {
        let font=FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap();let mut cache=GlyphCache::default();
        for (mode,direction,physical) in [
            ("horizontal-tb","ltr","border-top:3px double red;border-right:2px dashed green;border-bottom:4px dotted transparent;border-left:1px solid blue"),
            ("vertical-rl","rtl","border-top:2px dashed green;border-right:3px double red;border-bottom:1px solid blue;border-left:4px dotted transparent"),
            ("vertical-lr","ltr","border-top:1px solid blue;border-right:4px dotted transparent;border-bottom:2px dashed green;border-left:3px double red")] {
            let pairs="border-inline-width:1px 2px;border-block-width:3px 4px;border-inline-style:solid dashed;border-block-style:double dotted;border-inline-color:blue green;border-block-color:red transparent";
            let markup=format!("<!doctype html><style>html,body{{margin:0}}div{{writing-mode:{mode};direction:{direction};width:30px;height:20px;{pairs}}}</style><div></div>");
            let reference=markup.replace(pairs,physical);
            let actual=lumen_html::layout::display_list(&lumen_html::html::parse(&markup,64).unwrap(),100,80,&font).unwrap();
            let expected=lumen_html::layout::display_list(&lumen_html::html::parse(&reference,64).unwrap(),100,80,&font).unwrap();
            assert_eq!(actual,expected,"{mode} {direction}");
            for scale in [1.0,1.25] {assert_eq!(render_with_font_cached(&actual,100,80,scale,true,&font,&mut cache).unwrap(),render_with_font_cached(&expected,100,80,scale,true,&font,&mut cache).unwrap());}
        }
    }

    #[test]
    fn specification_border_inherited_computed_widths_match_actual_raster_and_layout() {
        let font=FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap();
        let mut cache=GlyphCache::default();
        for style in ["none","hidden","solid"] {
            for inherited in ["border-width:inherit","border-top-width:inherit;border-right-width:inherit;border-bottom-width:inherit;border-left-width:inherit",
                "border-block-start-width:inherit;border-block-end-width:inherit;border-inline-start-width:inherit;border-inline-end-width:inherit"] {
                let markup=format!("<!doctype html><style>html,body{{margin:0}}#parent{{font-size:10px;border:2em {style} transparent}}#child{{font-size:100px;width:30px;height:20px;border-style:solid;border-color:green;{inherited}}}</style><div id=parent><div id=child></div></div>");
                let reference=markup.replace(inherited,"border-width:20px");
                let document=lumen_html::html::parse(&markup,64).unwrap();
                let reference=lumen_html::html::parse(&reference,64).unwrap();
                let actual=lumen_html::layout::display_list(&document,160,120,&font).unwrap();
                let expected=lumen_html::layout::display_list(&reference,160,120,&font).unwrap();
                assert_eq!(actual,expected,"computed width controls actual box edges, not merely CSSOM: {style} {inherited}");
                for scale in [1.0,1.25] {
                    assert_eq!(render_with_font_cached(&actual,160,120,scale,true,&font,&mut cache).unwrap(),
                        render_with_font_cached(&expected,160,120,scale,true,&font,&mut cache).unwrap(),"{style} {inherited} scale {scale}");
                }
            }
        }
        let source="<!doctype html><style>html,body{margin:0}div{width:20px;height:20px;font-size:10px;color:green;border-top:calc(1em + 5px) solid;border-right:THIN solid;border-bottom:2em double;border-left:3em dashed}</style><div></div>";
        let reference=source.replace("calc(1em + 5px)","15px").replace("THIN","1px").replace("2em","20px").replace("3em","30px");
        let document=lumen_html::html::parse(source,64).unwrap();let reference=lumen_html::html::parse(&reference,64).unwrap();
        let actual=lumen_html::layout::display_list(&document,160,120,&font).unwrap();let expected=lumen_html::layout::display_list(&reference,160,120,&font).unwrap();
        assert_eq!(actual,expected,"all side styles share actual font-relative computed lengths and border commands");
        assert_eq!(render_with_font_cached(&actual,160,120,1.25,true,&font,&mut cache).unwrap(),render_with_font_cached(&expected,160,120,1.25,true,&font,&mut cache).unwrap());
    }

    #[test]
    fn specification_text_shadow_raster_uses_real_cached_glyphs_and_decoration_masks() {
        let font=FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap();let mut cache=GlyphCache::default();
        for decor in ["","text-decoration:underline line-through;text-decoration-color:transparent;"] {
            let markup=format!("<!doctype html><style>html,body{{margin:0}}div{{font-size:16px;color:transparent;{decor}text-shadow:3px 0 red}}</style><div>ABC</div>");
            let source=lumen_html::html::parse(&markup,64).unwrap();let list=lumen_html::layout::display_list(&source,120,60,&font).unwrap();
            let image=render_with_font_cached(&list,120,60,1.0,true,&font,&mut cache).unwrap();
            let reference=markup.replace("text-shadow:3px 0 red","text-shadow:none;position:relative;left:3px").replace("color:transparent","color:red");
            let reference=lumen_html::html::parse(&reference,64).unwrap();let reference=lumen_html::layout::display_list(&reference,120,60,&font).unwrap();
            assert_eq!(image,render_with_font_cached(&reference,120,60,1.0,true,&font,&mut cache).unwrap(),"glyphs and decorations shadow their real coverage");
        }
        for fragment in ["", "div::first-line{color:lime}"] {
            let markup=format!("<!doctype html><style>html,body{{margin:0}}body{{color:blue;text-shadow:3px 0 currentcolor}}div{{color:red;font-size:16px}}{fragment}</style><div>ABC</div>");
            let source=lumen_html::html::parse(&markup,64).unwrap();let list=lumen_html::layout::display_list(&source,120,60,&font).unwrap();
            let reference=markup.replace("currentcolor",if fragment.is_empty(){"red"}else{"lime"});
            let reference=lumen_html::html::parse(&reference,64).unwrap();let reference=lumen_html::layout::display_list(&reference,120,60,&font).unwrap();
            assert_eq!(render_with_font_cached(&list,120,60,1.0,true,&font,&mut cache).unwrap(),render_with_font_cached(&reference,120,60,1.0,true,&font,&mut cache).unwrap(),"inherited currentcolor follows actual descendant and first-line ink");
        }
        let entries=cache.glyphs.len();assert!(entries>0&&cache.bytes()<=GLYPH_CACHE_BYTES);
        let source=lumen_html::html::parse("<!doctype html><style>html,body{margin:0}div{font-size:16px;color:transparent;text-shadow:0 0 2px red,0 0 blue}</style><div>ABC</div>",64).unwrap();
        let list=lumen_html::layout::display_list(&source,120,60,&font).unwrap();let first=render_with_font_cached(&list,120,60,1.0,true,&font,&mut cache).unwrap();
        assert!(first.pixels.chunks_exact(4).any(|pixel|pixel[0]>pixel[1]&&pixel[1]>0&&pixel[1]<255),"Gaussian alpha blur actually paints surrounding ink");
        assert_eq!(first,render_with_font_cached(&list,120,60,1.0,true,&font,&mut cache).unwrap());assert_eq!(entries,cache.glyphs.len());
        let huge=Command::MaskedBackground(Box::new(lumen_html::paint::MaskedBackground{rect:Rect{x:0.0,y:0.0,width:4000.0,height:4000.0},text_clipped:false,shadow_blur:Some(20.0),
            paint:Box::new(Command::FillRect{rect:Rect{x:0.0,y:0.0,width:4000.0,height:4000.0},color:Rgba{r:255,g:0,b:0,a:255}}),
            mask:Arc::new(DisplayList(vec![Command::FillRect{rect:Rect{x:0.0,y:0.0,width:1.0,height:1.0},color:Rgba{r:255,g:255,b:255,a:255}}])),offset:[0.0,0.0]}));
        assert_eq!(resolve_layers(&[huge],1.0,Some(&font),&mut cache,&mut 0),Err(ImageError::TooLarge),"peak shadow workspace is rejected before allocation");
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
    }    #[test]
    fn specification_background_size_math_actual_query_font_and_nonlinear_raster() {
        let font=FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap();let mut cache=GlyphCache::default();
        for (area, width) in [(40,20),(100,50),(200,80)] {
            let markup=format!("<!doctype html><style>html,body{{margin:0}}div{{width:{area}px;height:100px;background:linear-gradient(green,green) 0/min(50%, 80px) clamp(10px,25%,50px) no-repeat}}</style><div></div>");
            let reference=markup.replace("min(50%, 80px)",&format!("{width}px")).replace("clamp(10px,25%,50px)","25px");
            let actual=lumen_html::html::parse(&markup,64).unwrap();let expected=lumen_html::html::parse(&reference,64).unwrap();
            let actual=lumen_html::layout::display_list(&actual,300,150,&font).unwrap();let expected=lumen_html::layout::display_list(&expected,300,150,&font).unwrap();
            assert_eq!(actual,expected);
            for scale in [1.0,1.25] {assert_eq!(render_with_font_cached(&actual,300,150,scale,true,&font,&mut cache).unwrap(),
                render_with_font_cached(&expected,300,150,scale,true,&font,&mut cache).unwrap());}
        }
        let source="<!doctype html><style>html,body{margin:0}#container{width:300px;container-type:inline-size}#target{width:200px;height:100px;font-size:10cqw;background:linear-gradient(green,green) 0/min(50%, 2em) calc(25% + 10cqw) no-repeat}</style><div id=container><div id=target></div></div>";
        let expected=source.replace("min(50%, 2em)","60px").replace("calc(25% + 10cqw)","55px");
        let source=lumen_html::html::parse(source,64).unwrap();let expected=lumen_html::html::parse(&expected,64).unwrap();
        let actual=lumen_html::layout::display_list(&source,400,150,&font).unwrap();let expected=lumen_html::layout::display_list(&expected,400,150,&font).unwrap();
        assert_eq!(actual,expected,"original declaring container and final query font resolve before actual area math");
        assert_eq!(render_with_font_cached(&actual,400,150,1.25,true,&font,&mut cache).unwrap(),
            render_with_font_cached(&expected,400,150,1.25,true,&font,&mut cache).unwrap());
    }

    #[test]
    fn specification_appearance_paints_native_state_and_primitive_css_pixels() {
        let font=FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap();
        let mut cache=GlyphCache::default();
        let source="<!doctype html><style>html,body{margin:0}input{all:unset;display:inline-block;width:20px;height:20px;vertical-align:top;color:black;background:white}</style><input type=checkbox checked style='appearance:auto'>";
        let native=lumen_html::html::parse(source,64).unwrap();
        let native=lumen_html::layout::display_list(&native,40,30,&font).unwrap();
        let native=render_with_font_cached(&native,40,30,1.0,true,&font,&mut cache).unwrap();
        let center=(10*40+10)*4;
        assert_eq!(&native.pixels[center..center+4],&[0,0,0,255],"real native checked mark");
        let primitive=lumen_html::html::parse(&source.replace("appearance:auto","appearance:none"),64).unwrap();
        let primitive=lumen_html::layout::display_list(&primitive,40,30,&font).unwrap();
        let primitive=render_with_font_cached(&primitive,40,30,1.0,true,&font,&mut cache).unwrap();
        assert_eq!(&primitive.pixels[center..center+4],&[255,255,255,255],"decorative checked mark suppressed");
        let reference=source.replace("input{","div{").replace("<input type=checkbox checked style='appearance:auto'>","<div></div>");
        let reference=lumen_html::html::parse(&reference,64).unwrap();
        let reference=lumen_html::layout::display_list(&reference,40,30,&font).unwrap();
        assert_eq!(primitive,render_with_font_cached(&reference,40,30,1.0,true,&font,&mut cache).unwrap(),"primitive control uses genuine ordinary CSS paint");
        let range=source.replace("type=checkbox checked","type=range min=0 max=100 value=50").replace("appearance:auto","appearance:none");
        let range=lumen_html::html::parse(&range,64).unwrap();
        let range=lumen_html::layout::display_list(&range,40,30,&font).unwrap();
        let range=render_with_font_cached(&range,40,30,1.0,true,&font,&mut cache).unwrap();
        assert!(range.pixels.chunks_exact(4).any(|pixel|pixel==[0,0,0,255]),"primitive range retains actual operating thumb");
    }

}
