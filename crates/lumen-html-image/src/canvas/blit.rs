//! Drawing a premultiplied pixmap at a whole-pixel offset without tiny-skia's
//! pattern pipeline.
//!
//! `drawImage` at natural size under a translate, and compositing a canvas layer
//! back onto the canvas, are pixmap draws whose texels land exactly on pixels.
//! tiny-skia still runs its high-precision pattern pipeline for them (a
//! per-pixel gather, then f32 blending). This computes the same result
//! directly: the same texel per pixel, the same f32 load, opacity, blend and
//! rounding, so the pixels are identical.

use tiny_skia::{BlendMode, Pixmap, PixmapRef};

const FACTOR: f32 = 1.0 / 255.0;

/// Draws `source` with its top-left corner at (`dx`, `dy`), as
/// `target.draw_pixmap` would with a translate-only transform, no mask and
/// this opacity and blend mode. Returns `false`, drawing nothing, for a blend
/// mode or opacity it does not handle, so the caller draws through tiny-skia.
pub(super) fn draw_at_offset(target: &mut Pixmap, source: PixmapRef<'_>, dx: i64, dy: i64, opacity: f32, blend: BlendMode) -> bool {
    let Some(blend) = Blend::from_mode(blend) else { return false };
    if !(0.0..=1.0).contains(&opacity) {
        return false;
    }
    let (target_width, target_height) = (i64::from(target.width()), i64::from(target.height()));
    let left = dx.max(0);
    let top = dy.max(0);
    let right = (dx + i64::from(source.width())).min(target_width);
    let bottom = (dy + i64::from(source.height())).min(target_height);
    if left >= right || top >= bottom || matches!(blend, Blend::Destination) {
        return true;
    }
    let (source_width, row_width) = (source.width() as usize, target.width() as usize);
    let (span, first_column) = ((right - left) as usize, (left - dx) as usize);
    let source_pixels = source.data();
    let target_pixels = target.data_mut();
    for y in top..bottom {
        let source_row = ((y - dy) as usize * source_width + first_column) * 4;
        let target_row = (y as usize * row_width + left as usize) * 4;
        let source_row = &source_pixels[source_row..source_row + span * 4];
        let target_row = &mut target_pixels[target_row..target_row + span * 4];
        blend_row(blend, opacity, source_row, target_row);
    }
    true
}

#[derive(Clone, Copy)]
enum Blend {
    Clear,
    Source,
    Destination,
    SourceOver,
    DestinationOver,
    SourceIn,
    DestinationIn,
    SourceOut,
    DestinationOut,
    SourceAtop,
    DestinationAtop,
    Xor,
    Plus,
}

impl Blend {
    fn from_mode(mode: BlendMode) -> Option<Self> {
        Some(match mode {
            BlendMode::Clear => Self::Clear,
            BlendMode::Source => Self::Source,
            BlendMode::Destination => Self::Destination,
            BlendMode::SourceOver => Self::SourceOver,
            BlendMode::DestinationOver => Self::DestinationOver,
            BlendMode::SourceIn => Self::SourceIn,
            BlendMode::DestinationIn => Self::DestinationIn,
            BlendMode::SourceOut => Self::SourceOut,
            BlendMode::DestinationOut => Self::DestinationOut,
            BlendMode::SourceAtop => Self::SourceAtop,
            BlendMode::DestinationAtop => Self::DestinationAtop,
            BlendMode::Xor => Self::Xor,
            BlendMode::Plus => Self::Plus,
            _ => return None,
        })
    }

    /// One channel, as tiny-skia's high-precision blend stages compute it.
    #[inline(always)]
    fn apply(self, s: f32, d: f32, sa: f32, da: f32) -> f32 {
        let inv = |v: f32| 1.0 - v;
        match self {
            Self::Clear => 0.0,
            Self::Source => s,
            Self::Destination => d,
            Self::SourceOver => d * inv(sa) + s,
            Self::DestinationOver => s * inv(da) + d,
            Self::SourceIn => s * da,
            Self::DestinationIn => d * sa,
            Self::SourceOut => s * inv(da),
            Self::DestinationOut => d * inv(sa),
            Self::SourceAtop => s * da + d * inv(sa),
            Self::DestinationAtop => d * sa + s * inv(da),
            Self::Xor => s * inv(da) + d * inv(sa),
            Self::Plus => (s + d).min(1.0),
        }
    }
}

#[inline(always)]
fn store(value: f32) -> u8 {
    // tiny-skia's `unnorm`: clamp, scale and round half to even, as the SSE2 and
    // NEON conversions do. Adding and removing 2^23 rounds a value in 0..=255
    // half to even without a libm call.
    const ROUND: f32 = 8_388_608.0;
    ((value.max(0.0).min(1.0) * 255.0 + ROUND) - ROUND) as u8
}

/// The result for pixels whose answer follows from a transparent or opaque
/// alpha without arithmetic; each matches the f32 blend exactly (255 loads as
/// exactly 1.0). `None` computes the blend.
#[inline(always)]
fn shortcut(blend: Blend, whole_opacity: bool, s: [u8; 4], d: [u8; 4]) -> Option<[u8; 4]> {
    const ZERO: [u8; 4] = [0; 4];
    let (sa, da) = (s[3], d[3]);
    match blend {
        Blend::SourceOver if sa == 0 => Some(d),
        Blend::SourceOver if sa == 255 && whole_opacity => Some(s),
        Blend::SourceIn | Blend::SourceOut if s == ZERO => Some(ZERO),
        Blend::SourceIn if da == 0 => Some(ZERO),
        Blend::SourceIn if da == 255 && whole_opacity => Some(s),
        Blend::SourceOut if da == 255 => Some(ZERO),
        Blend::SourceOut if da == 0 && whole_opacity => Some(s),
        Blend::DestinationIn if sa == 0 => Some(ZERO),
        Blend::DestinationIn if sa == 255 && whole_opacity => Some(d),
        Blend::DestinationOut if sa == 0 => Some(d),
        Blend::DestinationOut if sa == 255 && whole_opacity => Some(ZERO),
        Blend::DestinationOver if da == 255 => Some(d),
        Blend::DestinationOver if da == 0 && whole_opacity => Some(s),
        _ => None,
    }
}

fn blend_row(blend: Blend, opacity: f32, source: &[u8], target: &mut [u8]) {
    match blend {
        // `Clear` without coverage is a transparent fill.
        Blend::Clear => target.fill(0),
        // Converting to f32 and back keeps every byte.
        Blend::Source if opacity == 1.0 => target.copy_from_slice(source),
        _ => {
            let whole_opacity = opacity == 1.0;
            for (s, d) in source.chunks_exact(4).zip(target.chunks_exact_mut(4)) {
                let (source_pixel, target_pixel) = ([s[0], s[1], s[2], s[3]], [d[0], d[1], d[2], d[3]]);
                if let Some(result) = shortcut(blend, whole_opacity, source_pixel, target_pixel) {
                    d.copy_from_slice(&result);
                    continue;
                }
                let mut sc = [s[0] as f32 * FACTOR, s[1] as f32 * FACTOR, s[2] as f32 * FACTOR, s[3] as f32 * FACTOR];
                if opacity != 1.0 {
                    for channel in &mut sc {
                        *channel *= opacity;
                    }
                }
                let dc = [d[0] as f32 * FACTOR, d[1] as f32 * FACTOR, d[2] as f32 * FACTOR, d[3] as f32 * FACTOR];
                let (sa, da) = (sc[3], dc[3]);
                for channel in 0..4 {
                    d[channel] = store(blend.apply(sc[channel], dc[channel], sa, da));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_skia::{FilterQuality, PixmapPaint, Transform};

    fn premultiplied(width: u32, height: u32, seed: u32) -> Pixmap {
        let mut pixmap = Pixmap::new(width, height).unwrap();
        let mut state = seed;
        for pixel in pixmap.data_mut().chunks_exact_mut(4) {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let alpha = match state >> 28 { 0 => 0, 1 => 255, _ => (state >> 16) as u8 };
            for channel in &mut pixel[..3] {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                *channel = ((state >> 16) as u8).min(alpha);
            }
            pixel[3] = alpha;
        }
        pixmap
    }

    #[test]
    fn whole_pixel_draws_match_the_pattern_pipeline() {
        let source = premultiplied(37, 23, 7);
        let modes = [
            BlendMode::Clear, BlendMode::Source, BlendMode::Destination, BlendMode::SourceOver,
            BlendMode::DestinationOver, BlendMode::SourceIn, BlendMode::DestinationIn, BlendMode::SourceOut,
            BlendMode::DestinationOut, BlendMode::SourceAtop, BlendMode::DestinationAtop, BlendMode::Xor, BlendMode::Plus,
        ];
        for mode in modes {
            for opacity in [1.0, 0.5, 0.123, 0.0] {
                for (dx, dy) in [(0, 0), (5, 3), (-4, -6), (40, 2), (-37, 0), (20, 15)] {
                    let mut expected = premultiplied(48, 32, 11);
                    let mut actual = expected.clone();
                    let paint = PixmapPaint { opacity, blend_mode: mode, quality: FilterQuality::Bilinear };
                    expected.draw_pixmap(0, 0, source.as_ref(), &paint, Transform::from_translate(dx as f32, dy as f32), None);
                    assert!(draw_at_offset(&mut actual, source.as_ref(), dx, dy, opacity, mode));
                    assert!(expected.data() == actual.data(), "{mode:?} at {opacity} offset ({dx}, {dy})");
                }
            }
        }
        let mut target = premultiplied(4, 4, 3);
        assert!(!draw_at_offset(&mut target, source.as_ref(), 0, 0, 1.0, BlendMode::Multiply));
        assert!(!draw_at_offset(&mut target, source.as_ref(), 0, 0, 1.5, BlendMode::SourceOver));
    }
}
