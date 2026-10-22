//! Encoded still images whose pixels are decoded on demand into a bounded process-wide cache.
//!
//! An embedder keeps only the encoded bytes and the header size of each image. Pixels exist while
//! the cache holds them or a consumer holds the returned `Arc`, and are decoded again after eviction.
use crate::{ImageError, Rgba8Image, SvgViewportImages};
use lumen_common::byte_lru::ByteLru;
use lumen_html::layout::{ImageState, LazyImage, LazyImageHandle};
use lumen_html::paint::ImageData;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

pub const DEFAULT_DECODED_IMAGE_BUDGET: usize = 256 * 1024 * 1024;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static HITS: AtomicU64 = AtomicU64::new(0);
static DECODES: AtomicU64 = AtomicU64::new(0);
static DECODED_BYTES: AtomicU64 = AtomicU64::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);
static CACHE: OnceLock<Mutex<ByteLru<u64, Arc<ImageData>>>> = OnceLock::new();

fn cache() -> MutexGuard<'static, ByteLru<u64, Arc<ImageData>>> {
    CACHE
        .get_or_init(|| Mutex::new(ByteLru::new(DEFAULT_DECODED_IMAGE_BUDGET)))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Bounds the decoded pixels the process keeps for lazily decoded images; the most recently
/// decoded image always stays, so one image larger than the budget still decodes once per use.
pub fn set_decoded_image_budget(bytes: usize) {
    cache().set_budget(bytes);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodedImageCacheStats {
    pub entries: usize,
    pub bytes: usize,
    pub budget: usize,
    pub hits: u64,
    pub decodes: u64,
    pub decoded_bytes: u64,
    pub peak_bytes: usize,
}

pub fn decoded_image_cache_stats() -> DecodedImageCacheStats {
    let cache = cache();
    DecodedImageCacheStats {
        entries: cache.len(),
        bytes: cache.bytes(),
        budget: cache.budget(),
        hits: HITS.load(Ordering::Relaxed),
        decodes: DECODES.load(Ordering::Relaxed),
        decoded_bytes: DECODED_BYTES.load(Ordering::Relaxed),
        peak_bytes: PEAK_BYTES.load(Ordering::Relaxed),
    }
}

/// A still raster image: encoded bytes, header size, and on-demand decoded pixels.
/// A body that fails to decode makes `pixels` return `None` from then on; the size read
/// from the header stays, as in browsers that fire `load` from header metadata.
pub struct EncodedImage {
    id: u64,
    bytes: Box<dyn AsRef<[u8]>>,
    width: u32,
    height: u32,
    failed: Cell<bool>,
    opaque: Cell<Option<bool>>,
}

impl EncodedImage {
    /// Reads the oriented size from the header without decoding pixels.
    pub fn probe(bytes: impl AsRef<[u8]> + 'static) -> Result<Self, ImageError> {
        let (width, height) = crate::probe_raster_dimensions(bytes.as_ref())?;
        if width == 0 || height == 0 {
            return Err(ImageError::InvalidViewport);
        }
        let opaque = header_is_opaque(bytes.as_ref());
        Ok(Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            bytes: Box::new(bytes),
            width,
            height,
            failed: Cell::new(false),
            opaque: Cell::new(opaque),
        })
    }

    pub fn into_handle(self) -> LazyImageHandle {
        LazyImageHandle::new(Rc::new(self))
    }
}

impl LazyImage for EncodedImage {
    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn is_opaque(&self) -> Option<bool> {
        self.opaque.get()
    }

    fn is_known_corrupt(&self) -> bool {
        self.failed.get()
    }

    fn pixels(&self) -> Option<Arc<ImageData>> {
        if let Some(hit) = cache().get(&self.id) {
            HITS.fetch_add(1, Ordering::Relaxed);
            return Some(hit);
        }
        if self.failed.get() {
            return None;
        }
        let decoded = crate::decode_raster_image_with_limit(self.bytes.as_ref().as_ref(), crate::MAX_IMAGE_BYTES)
            .ok()
            .filter(Rgba8Image::is_valid);
        let Some(image) = decoded else {
            self.failed.set(true);
            return None;
        };
        let bytes = image.pixels.len();
        if self.opaque.get().is_none() {
            self.opaque.set(Some(image.pixels.chunks_exact(4).all(|pixel| pixel[3] == 255)));
        }
        let image = Arc::new(image);
        DECODES.fetch_add(1, Ordering::Relaxed);
        DECODED_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
        let mut cache = cache();
        cache.insert(self.id, image.clone(), bytes);
        PEAK_BYTES.fetch_max(cache.bytes(), Ordering::Relaxed);
        drop(cache);
        Some(image)
    }
}

/// Opacity that the container format guarantees: JPEG, and PNG colour types without alpha and
/// without a transparency chunk. `None` when only the pixels can tell.
fn header_is_opaque(bytes: &[u8]) -> Option<bool> {
    if bytes.starts_with(&[0xFF, 0xD8]) {
        return Some(true);
    }
    const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if !bytes.starts_with(&PNG_SIGNATURE) {
        return None;
    }
    let mut position = PNG_SIGNATURE.len();
    let mut has_alpha_channel = None;
    while let Some(header) = bytes.get(position..position + 8) {
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let kind = &header[4..8];
        let body = position + 8;
        match kind {
            b"IHDR" => has_alpha_channel = Some(matches!(bytes.get(body + 9)?, 4 | 6)),
            b"tRNS" => return None,
            b"IDAT" | b"IEND" => break,
            _ => {}
        }
        position = body.checked_add(length)?.checked_add(4)?;
    }
    (has_alpha_channel? == false).then_some(true)
}

impl Drop for EncodedImage {
    fn drop(&mut self) {
        cache().remove(&self.id);
    }
}

/// Resource state for encoded image bytes: lazy for still rasters, decoded at once for SVG
/// (whose size comes from rendering), `Failed` for bytes that are not a supported image.
pub fn lazy_image_state(bytes: impl AsRef<[u8]> + 'static) -> ImageState {
    if SvgViewportImages::is_vector(bytes.as_ref()) {
        return crate::decode_image_with_limit(bytes.as_ref(), crate::MAX_IMAGE_BYTES)
            .ok()
            .filter(Rgba8Image::is_valid)
            .map_or(ImageState::Failed, |image| ImageState::Ready(Arc::new(image)));
    }
    EncodedImage::probe(bytes).map_or(ImageState::Failed, |image| ImageState::Lazy(image.into_handle()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut out = Vec::new();
        let pixels = rgba.repeat((width * height) as usize);
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(&pixels, width, height, image::ExtendedColorType::Rgba8)
            .unwrap();
        out
    }

    use image::ImageEncoder as _;

    static SERIAL: Mutex<()> = Mutex::new(());
    fn serial() -> MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn header_probe_reports_size_without_decoding_and_pixels_match_eager_decode() {
        let _serial = serial();
        let bytes = png(5, 3, [10, 20, 30, 255]);
        let lazy = EncodedImage::probe(bytes.clone()).unwrap();
        assert_eq!((lazy.width(), lazy.height()), (5, 3));
        assert_eq!(cache().get(&lazy.id), None);
        let eager = crate::decode_raster_image(&bytes).unwrap();
        assert_eq!(*lazy.pixels().unwrap(), eager);
    }

    #[test]
    fn all_supported_still_formats_probe_and_decode_lazily() {
        let _serial = serial();
        let source = image::RgbaImage::from_pixel(4, 2, image::Rgba([200, 100, 50, 255]));
        let mut encodings = Vec::new();
        for format in [image::ImageFormat::Png, image::ImageFormat::Gif, image::ImageFormat::Bmp, image::ImageFormat::Jpeg] {
            let mut out = Cursor::new(Vec::new());
            let encoded = if format == image::ImageFormat::Jpeg {
                image::DynamicImage::ImageRgba8(source.clone()).into_rgb8().write_to(&mut out, format)
            } else {
                source.write_to(&mut out, format)
            };
            encoded.unwrap();
            encodings.push((format, out.into_inner()));
        }
        for (format, bytes) in encodings {
            let lazy = EncodedImage::probe(bytes.clone()).unwrap_or_else(|error| panic!("{format:?}: {error:?}"));
            assert_eq!((lazy.width(), lazy.height()), (4, 2), "{format:?}");
            assert_eq!(*lazy.pixels().unwrap(), crate::decode_raster_image(&bytes).unwrap(), "{format:?}");
        }
    }

    use std::io::Cursor;

    #[test]
    fn evicted_pixels_decode_again_identically() {
        let _serial = serial();
        let first = EncodedImage::probe(png(8, 8, [1, 2, 3, 255])).unwrap();
        let second = EncodedImage::probe(png(8, 8, [9, 8, 7, 255])).unwrap();
        let before = first.pixels().unwrap();
        let budget = decoded_image_cache_stats().budget;
        set_decoded_image_budget(0);
        second.pixels().unwrap();
        assert!(cache().get(&first.id).is_none(), "the older entry was evicted");
        let again = first.pixels().unwrap();
        assert!(!Arc::ptr_eq(&before, &again));
        assert_eq!(*before, *again);
        set_decoded_image_budget(budget);
    }

    #[test]
    fn corrupt_body_keeps_header_size_and_fails_on_decode() {
        let _serial = serial();
        let mut bytes = png(16, 16, [5, 5, 5, 255]);
        bytes.truncate(48);
        let lazy = EncodedImage::probe(bytes).unwrap();
        assert_eq!((lazy.width(), lazy.height()), (16, 16));
        assert!(lazy.pixels().is_none());
        assert!(lazy.pixels().is_none());
    }

    #[test]
    fn dropping_an_image_releases_its_cached_pixels() {
        let _serial = serial();
        let lazy = EncodedImage::probe(png(4, 4, [0, 0, 0, 255])).unwrap();
        let id = lazy.id;
        lazy.pixels().unwrap();
        assert!(cache().get(&id).is_some());
        drop(lazy);
        assert!(cache().get(&id).is_none());
    }

    #[test]
    fn encoded_rgba_roundtrip_and_probe_agree_with_the_eager_decoder() {
        let _serial = serial();
        let source = Rgba8Image { width: 3, height: 2, pixels: (0u8..24).map(|byte| byte.wrapping_mul(11)).collect() };
        let bytes = crate::encode_png(&source);
        assert_eq!(crate::probe_raster_dimensions(&bytes).unwrap(), (3, 2));
        assert_eq!(crate::decode_raster_image(&bytes).unwrap(), source);
        assert!(matches!(lazy_image_state(bytes), ImageState::Lazy(lazy) if lazy.width() == 3 && lazy.height() == 2));
    }

    #[test]
    fn non_image_bytes_and_svg_state() {
        assert_eq!(lazy_image_state(b"not an image".to_vec()), ImageState::Failed);
        assert!(matches!(lazy_image_state(png(2, 2, [1, 1, 1, 255])), ImageState::Lazy(_)));
    }
    #[test]
    fn stats_count_decodes_hits_and_the_cache_high_water_mark() {
        let _serial = serial();
        let before = decoded_image_cache_stats();
        let lazy = EncodedImage::probe(png(8, 8, [1, 2, 3, 255])).unwrap();
        lazy.pixels().unwrap();
        lazy.pixels().unwrap();
        lazy.pixels().unwrap();
        let after = decoded_image_cache_stats();
        assert_eq!(after.decodes - before.decodes, 1);
        assert_eq!(after.hits - before.hits, 2);
        assert_eq!(after.decoded_bytes - before.decoded_bytes, 8 * 8 * 4);
        assert!(after.peak_bytes >= after.bytes && after.bytes >= 8 * 8 * 4);
    }

}
