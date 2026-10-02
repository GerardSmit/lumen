//! Explicit-font shaping and glyph coverage shared by render backends.
#![no_std]

extern crate alloc;

pub use lumen_common::ucd::{
    BreakOpportunity, graphemes, line_breaks, next_grapheme_boundary, previous_grapheme_boundary,
};

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};
use lumen_html::paint::{Glyph, ShapedRun, TextShaper};

static NEXT_FACE_ID: AtomicU64 = AtomicU64::new(1);
const MAX_FONT_BYTES: usize = 2 * 1024 * 1024;
const MAX_SHAPE_TEXT_BYTES: usize = 64 * 1024;

pub struct FontFace {
    id: u64,
    bytes: Arc<[u8]>,
    rasterizer: fontdue::Font,
    units_per_em: f32,
    ascent: f32,
    descent: f32,
    underline: (f32,f32),
    strike: (f32,f32),
}

#[derive(Clone, Debug, PartialEq)]
pub struct GlyphCoverage {
    pub x_min: i32,
    pub y_min: i32,
    pub width: usize,
    pub height: usize,
    pub alpha: Vec<u8>,
}

impl FontFace {
    pub fn new(bytes: Arc<[u8]>) -> Result<Self, &'static str> {
        if bytes.len() > MAX_FONT_BYTES {
            return Err("font too large");
        }
        let face = ttf_parser::Face::parse(&bytes, 0).map_err(|_| "invalid font")?;
        let units_per_em = face.units_per_em() as f32;
        let ascent = face.ascender() as f32 / units_per_em;
        let descent = face.descender() as f32 / units_per_em;
        let underline = face.underline_metrics().map_or((0.1,1.0/16.0),|m|(-m.position as f32 / units_per_em,m.thickness as f32 / units_per_em));
        let strike = face.strikeout_metrics().map_or((-0.3,1.0/16.0),|m|(-m.position as f32 / units_per_em,m.thickness as f32 / units_per_em));
        let rasterizer = fontdue::Font::from_bytes(bytes.clone(), fontdue::FontSettings::default())
            .map_err(|_| "unsupported font")?;
        if rustybuzz::Face::from_slice(&bytes, 0).is_none() {
            return Err("font cannot be shaped");
        }
        Ok(Self {
            id: NEXT_FACE_ID.fetch_add(1, Ordering::Relaxed),
            bytes,
            rasterizer,
            units_per_em,
            ascent,
            descent,
            underline,
            strike,
        })
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn shape(&self, text: &str, size: f32) -> Result<ShapedRun, &'static str> {
        let shaped = self.shape_buffer(text, size)?;
        let scale = size / self.units_per_em;
        let mut glyphs = Vec::new();
        glyphs
            .try_reserve_exact(shaped.len())
            .map_err(|_| "glyph allocation failed")?;
        let mut x = 0.0;
        for (info, pos) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
            glyphs.push(Glyph {
                id: u16::try_from(info.glyph_id).map_err(|_| "glyph id out of range")?,
                x: x + pos.x_offset as f32 * scale,
                y: pos.y_offset as f32 * scale,
            });
            x += pos.x_advance as f32 * scale;
        }
        Ok(ShapedRun { glyphs, width: x })
    }

    pub fn measure(&self, text: &str, size: f32) -> Result<f32, &'static str> {
        let shaped = self.shape_buffer(text, size)?;
        let scale = size / self.units_per_em;
        Ok(shaped
            .glyph_positions()
            .iter()
            .map(|position| position.x_advance as f32 * scale)
            .sum())
    }

    fn shape_buffer(&self, text: &str, size: f32) -> Result<rustybuzz::GlyphBuffer, &'static str> {
        if !size.is_finite() || size <= 0.0 || size > 512.0 {
            return Err("invalid font size");
        }
        if text.len() > MAX_SHAPE_TEXT_BYTES {
            return Err("text run too large");
        }
        let face = rustybuzz::Face::from_slice(&self.bytes, 0).ok_or("font cannot be shaped")?;
        let mut buffer = rustybuzz::UnicodeBuffer::new();
        buffer.push_str(text);
        buffer.guess_segment_properties();
        Ok(rustybuzz::shape(&face, &[], buffer))
    }

    pub fn line_height(&self, size: f32) -> f32 {
        (self.ascent - self.descent) * size
    }
    pub fn ascent(&self, size: f32) -> f32 {
        self.ascent * size
    }

    pub fn rasterize(&self, id: u16, size: f32) -> Result<GlyphCoverage, &'static str> {
        if !size.is_finite() || size <= 0.0 || size > 512.0 {
            return Err("invalid font size");
        }
        if id >= self.rasterizer.glyph_count() {
            return Err("glyph id out of range");
        }
        let (metrics, alpha) = self.rasterizer.rasterize_indexed(id, size);
        Ok(GlyphCoverage {
            x_min: metrics.xmin,
            y_min: metrics.ymin,
            width: metrics.width,
            height: metrics.height,
            alpha,
        })
    }
}

impl TextShaper for FontFace {
    fn underline_metrics(&self,size:f32)->(f32,f32) {
        (self.underline.0*size,(self.underline.1*size).max(1.0))
    }
    fn strike_metrics(&self,size:f32)->(f32,f32) {
        (self.strike.0*size,(self.strike.1*size).max(1.0))
    }
    fn shape(&self, text: &str, size: f32) -> Result<ShapedRun, ()> {
        FontFace::shape(self, text, size).map_err(|_| ())
    }
    fn measure(&self, text: &str, size: f32) -> Result<f32, ()> {
        FontFace::measure(self, text, size).map_err(|_| ())
    }
    fn ascent(&self, size: f32) -> f32 {
        FontFace::ascent(self, size)
    }
    fn line_height(&self, size: f32) -> f32 {
        FontFace::line_height(self, size)
    }
}

pub const DEFAULT_FONT_BYTES: &[u8] = include_bytes!("../fonts/Inconsolata-Regular.ttf");
/// Proportional conformance face, redistributed unchanged under SIL OFL 1.1.
pub const TEST_FONT_BYTES: &[u8] = include_bytes!("../fonts/LiberationSans-Regular.ttf");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_font_and_shape_input() {
        let oversized: Arc<[u8]> = alloc::vec![0; MAX_FONT_BYTES + 1].into();
        assert!(matches!(FontFace::new(oversized), Err("font too large")));
        let font = FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap();
        let oversized_text = "a".repeat(MAX_SHAPE_TEXT_BYTES + 1);
        assert!(matches!(
            font.shape(&oversized_text, 16.0),
            Err("text run too large")
        ));
        let proportional = FontFace::new(Arc::from(TEST_FONT_BYTES)).unwrap();
        for text in ["Hello world", "office", "\u{05e9}\u{05dc}\u{05d5}\u{05dd}"] {
            assert_eq!(
                proportional.measure(text, 17.5).unwrap(),
                proportional.shape(text, 17.5).unwrap().width
            );
        }
    }
}
