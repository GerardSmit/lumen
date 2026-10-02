use super::{ImageError, MAX_LAYER_BYTES, Rgba8Image};
use lumen_html::paint::{Affine, Rect};

pub(super) fn rasterize(
    source: Rgba8Image,
    source_rect: Rect,
    affine: Affine,
    scale: f32,
    budget: usize,
) -> Result<Option<(Rect, Rgba8Image)>, ImageError> {
    let source_bytes = (source.width as usize)
        .checked_mul(source.height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or(ImageError::TooLarge)?;
    if source_bytes != source.pixels.len()
        || !affine.is_finite()
        || ![
            source_rect.x,
            source_rect.y,
            source_rect.width,
            source_rect.height,
            scale,
        ]
        .iter()
        .all(|v| v.is_finite())
        || scale <= 0.0
        || source_rect.width < 0.0
        || source_rect.height < 0.0
    {
        return Err(ImageError::InvalidViewport);
    }
    if source_bytes > budget.min(MAX_LAYER_BYTES) {
        return Err(ImageError::TooLarge);
    }
    if source_bytes == 0 || source_rect.width == 0.0 || source_rect.height == 0.0 {
        return Ok(None);
    }
    let [a, b, c, d, e, f] =
        [affine.a, affine.b, affine.c, affine.d, affine.e, affine.f].map(f64::from);
    let det = a * d - b * c;
    if det == 0.0 {
        return Ok(None);
    }
    let (sx, sy, sw, sh, scale) = (
        f64::from(source_rect.x),
        f64::from(source_rect.y),
        f64::from(source_rect.width),
        f64::from(source_rect.height),
        f64::from(scale),
    );
    let mut bounds = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for (x, y) in [(sx, sy), (sx + sw, sy), (sx, sy + sh), (sx + sw, sy + sh)] {
        let (x, y) = ((a * x + c * y + e) * scale, (b * x + d * y + f) * scale);
        bounds[0] = bounds[0].min(x.floor());
        bounds[1] = bounds[1].min(y.floor());
        bounds[2] = bounds[2].max(x.ceil());
        bounds[3] = bounds[3].max(y.ceil());
    }
    if !bounds.iter().all(|v| v.is_finite()) {
        return Err(ImageError::InvalidViewport);
    }
    let (width, height) = (bounds[2] - bounds[0], bounds[3] - bounds[1]);
    if width > u32::MAX as f64 || height > u32::MAX as f64 {
        return Err(ImageError::TooLarge);
    }
    let (width, height) = (width as u32, height as u32);
    let rect = Rect {
        x: (bounds[0] / scale) as f32,
        y: (bounds[1] / scale) as f32,
        width: (width as f64 / scale) as f32,
        height: (height as f64 / scale) as f32,
    };
    if ![rect.x, rect.y, rect.width, rect.height]
        .iter()
        .all(|v| v.is_finite())
    {
        return Err(ImageError::InvalidViewport);
    }
    if a == 1.0
        && d == 1.0
        && b == 0.0
        && c == 0.0
        && [sx * scale, sy * scale, e * scale, f * scale]
            .iter()
            .all(|v| v.fract() == 0.0)
        && sw * scale == source.width as f64
        && sh * scale == source.height as f64
    {
        return Ok(Some((rect, source)));
    }
    let bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .filter(|n| {
            source_bytes
                .checked_add(*n)
                .is_some_and(|total| total <= budget.min(MAX_LAYER_BYTES))
        })
        .ok_or(ImageError::TooLarge)?;
    let mut output = Rgba8Image {
        width,
        height,
        pixels: vec![0; bytes],
    };
    for y in 0..height {
        for x in 0..width {
            let (wx, wy) = (
                (bounds[0] + x as f64 + 0.5) / scale - e,
                (bounds[1] + y as f64 + 0.5) / scale - f,
            );
            let px = ((d * wx - c * wy) / det - sx) / sw * source.width as f64 - 0.5;
            let py = ((-b * wx + a * wy) / det - sy) / sh * source.height as f64 - 0.5;
            if !px.is_finite()
                || !py.is_finite()
                || px < -1.0
                || py < -1.0
                || px >= source.width as f64
                || py >= source.height as f64
            {
                continue;
            }
            let (ix, iy) = (px.floor() as i64, py.floor() as i64);
            let (tx, ty) = (px - px.floor(), py - py.floor());
            let mut sum = [0.0; 4];
            for (dx, dy, weight) in [
                (0, 0, (1.0 - tx) * (1.0 - ty)),
                (1, 0, tx * (1.0 - ty)),
                (0, 1, (1.0 - tx) * ty),
                (1, 1, tx * ty),
            ] {
                let (xx, yy) = (ix + dx, iy + dy);
                if xx < 0 || yy < 0 || xx >= source.width as i64 || yy >= source.height as i64 {
                    continue;
                }
                let offset = (yy as usize * source.width as usize + xx as usize) * 4;
                let pixel = &source.pixels[offset..offset + 4];
                let alpha = pixel[3] as f64;
                sum[3] += alpha * weight;
                for channel in 0..3 {
                    sum[channel] += pixel[channel] as f64 * alpha * weight;
                }
            }
            let offset = (y as usize * width as usize + x as usize) * 4;
            let alpha = sum[3].round() as u8;
            if alpha != 0 {
                for channel in 0..3 {
                    output.pixels[offset + channel] = (sum[channel] / sum[3]).round() as u8;
                }
                output.pixels[offset + 3] = alpha;
            }
        }
    }
    Ok(Some((rect, output)))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(
        source: &Rgba8Image,
        rect: Rect,
        scale: f32,
        affine: Affine,
    ) -> Result<Option<(Rect, Rgba8Image)>, ImageError> {
        rasterize(source.clone(), rect, affine, scale, MAX_LAYER_BYTES)
    }
    fn source() -> Rgba8Image {
        Rgba8Image {
            width: 2,
            height: 1,
            pixels: vec![255, 0, 0, 255, 0, 255, 0, 255],
        }
    }
    fn rect() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            width: 2.0,
            height: 1.0,
        }
    }
    #[test]
    fn translation_preserves_exact_pixels() {
        let (bounds, image) = sample(
            &source(),
            rect(),
            1.0,
            Affine {
                e: 3.0,
                f: -2.0,
                ..Affine::IDENTITY
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            bounds,
            Rect {
                x: 3.0,
                y: -2.0,
                ..rect()
            }
        );
        assert_eq!(image, source());
    }
    #[test]
    fn quarter_turn_rotates_pixels() {
        let (bounds, image) = sample(
            &source(),
            rect(),
            1.0,
            Affine {
                a: 0.0,
                b: 1.0,
                c: -1.0,
                d: 0.0,
                e: 0.0,
                f: 0.0,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            bounds,
            Rect {
                x: -1.0,
                y: 0.0,
                width: 1.0,
                height: 2.0
            }
        );
        assert_eq!(image.pixels, source().pixels);
    }
    #[test]
    fn fractional_translation_interpolates_premultiplied_alpha() {
        let source = Rgba8Image {
            width: 2,
            height: 1,
            pixels: vec![255, 0, 0, 255, 0, 0, 255, 0],
        };
        let (_, image) = sample(
            &source,
            rect(),
            1.0,
            Affine {
                e: 0.5,
                ..Affine::IDENTITY
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            image.pixels,
            vec![255, 0, 0, 128, 255, 0, 0, 128, 0, 0, 0, 0]
        );
    }
    #[test]
    fn singular_and_invalid_geometry_are_checked() {
        assert!(
            sample(
                &source(),
                rect(),
                1.0,
                Affine {
                    a: 0.0,
                    ..Affine::IDENTITY
                }
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(
            sample(&source(), rect(), f32::NAN, Affine::IDENTITY),
            Err(ImageError::InvalidViewport)
        );
        let invalid = Rgba8Image {
            pixels: vec![],
            ..source()
        };
        assert_eq!(
            sample(&invalid, rect(), 1.0, Affine::IDENTITY),
            Err(ImageError::InvalidViewport)
        );
    }
    #[test]
    fn combined_source_and_output_budget_is_checked() {
        assert_eq!(
            sample(
                &source(),
                rect(),
                1.0,
                Affine {
                    a: 1_000_000.0,
                    ..Affine::IDENTITY
                }
            ),
            Err(ImageError::TooLarge)
        );
        let source = Rgba8Image {
            width: 1024,
            height: 1024,
            pixels: vec![0; MAX_LAYER_BYTES],
        };
        assert_eq!(
            sample(
                &source,
                Rect {
                    width: 1024.0,
                    height: 1024.0,
                    ..rect()
                },
                1.0,
                Affine {
                    e: 0.5,
                    ..Affine::IDENTITY
                }
            ),
            Err(ImageError::TooLarge)
        );
        let (_, moved) = rasterize(
            source,
            Rect {
                width: 1024.0,
                height: 1024.0,
                ..rect()
            },
            Affine::IDENTITY,
            1.0,
            MAX_LAYER_BYTES,
        )
        .unwrap()
        .unwrap();
        assert_eq!(moved.pixels.len(), MAX_LAYER_BYTES);
        assert_eq!(
            rasterize(
                self::source(),
                rect(),
                Affine {
                    e: 0.5,
                    ..Affine::IDENTITY
                },
                1.0,
                19
            ),
            Err(ImageError::TooLarge)
        );
    }
}
