use super::{composite, ImageError, Raster, MAX_LAYER_BYTES};
use lumen_html::paint::{BoxShadow, Rect};

pub(super) fn draw(
    raster: &mut Raster<'_>,
    rect: Rect,
    radius: f32,
    corners: Option<&[[f32; 2]; 4]>,
    shadow: BoxShadow,
) -> Result<(), ImageError> {
    if shadow.color.a == 0 {
        return Ok(());
    }
    let Some(visible) = raster.visible_rect(shadow.bounds(rect))
    else {
        return Ok(());
    };
    let (_, padding) = blur_sizes(shadow.blur, raster.scale)?;
    let (x0,y0,x1,y1)=raster.pixel_span(visible);
    if x1<=x0 || y1<=y0 {return Ok(());}
    let left=(i64::from(x0)+raster.device_origin[0]) as f32-padding as f32;
    let top=(i64::from(y0)+raster.device_origin[1]) as f32-padding as f32;
    let width=(x1-x0) as f32+padding as f32*2.0;
    let height=(y1-y0) as f32+padding as f32*2.0;
    if width <= 0.0 || height <= 0.0 {
        return Ok(());
    }
    if width > usize::MAX as f32 || height > usize::MAX as f32 {
        return Err(ImageError::TooLarge);
    }
    let (width, height) = (width as usize, height as usize);
    let pixels = lumen_common::limits::size::repeat(width, height, MAX_LAYER_BYTES / 2)
        .map_err(|_| ImageError::TooLarge)?;
    validate_blur_budget(width,height,shadow.blur,raster.scale,raster.source_reserved.unwrap_or(0))?;
    let mut mask = vec![0u8; pixels];
    let spread = if shadow.inset {
        -shadow.spread
    } else {
        shadow.spread
    };
    let shape = Rect {
        x: rect.x + shadow.offset_x - spread,
        y: rect.y + shadow.offset_y - spread,
        width: (rect.width + 2.0 * spread).max(0.0),
        height: (rect.height + 2.0 * spread).max(0.0),
    };
    let shape_radius = if spread > 0.0 && radius < spread {
        radius + spread * (1.0 - (1.0 - radius / spread).powi(3))
    } else {
        (radius + spread).max(0.0)
    };
    let shape_corners = corners.map(|corners| {
        corners.map(|r| {
            r.map(|radius| {
                if spread > 0.0 && radius < spread {
                    radius + spread * (1.0 - (1.0 - radius / spread).powi(3))
                } else {
                    (radius + spread).max(0.0)
                }
            })
        })
    });
    for (y, row) in mask.chunks_exact_mut(width).enumerate() {
        for (x, value) in row.iter_mut().enumerate() {
            *value = (coverage(
                shape,
                shape_radius,
                shape_corners.as_ref(),
                left + x as f32,
                top + y as f32,
                raster.scale,
                raster.antialias,
            ) * 255.0)
                .round() as u8;
        }
    }
    blur_alpha_mask(&mut mask, width, height, shadow.blur, raster.scale, raster.source_reserved.unwrap_or(0))?;
    let (x0, y0, x1, y1) = raster.pixel_span(visible);
    let transparent = if shadow.inset { 255 } else { 0 };
    for y in y0..y1 {
        for x in x0..x1 {
            let mask_offset = ((i64::from(y)+raster.device_origin[1]) as f32 - top) as usize * width + ((i64::from(x)+raster.device_origin[0]) as f32 - left) as usize;
            let level = mask[mask_offset];
            if level == transparent {
                continue;
            }
            let original = coverage(
                rect,
                radius,
                corners,
                (i64::from(x)+raster.device_origin[0]) as f32,
                (i64::from(y)+raster.device_origin[1]) as f32,
                raster.scale,
                raster.antialias,
            );
            let alpha = level as f32 / 255.0;
            let alpha = if shadow.inset {
                (1.0 - alpha) * original
            } else {
                alpha * (1.0 - original)
            };
            if alpha == 0.0 {
                continue;
            }
            let offset = (y as usize * raster.image.width as usize + x as usize) * 4;
            composite(
                &mut raster.image.pixels[offset..offset + 4],
                shadow.color,
                alpha,
            );
        }
    }
    Ok(())
}

/// Return the three box widths and padding used to approximate a Gaussian blur.
/// The standard deviation for canvas shadows is half the blur value.
pub(super) fn blur_padding(blur: f32) -> Result<usize, ImageError> {
    Ok(blur_sizes(blur, 1.0)?.1)
}

fn blur_sizes(blur: f32, scale: f32) -> Result<([usize; 3], usize), ImageError> {
    if !blur.is_finite() || blur < 0.0 || !scale.is_finite() || scale <= 0.0 {
        return Err(ImageError::TooLarge);
    }
    let sigma = f64::from(blur) * f64::from(scale) * 0.5;
    let window = (sigma * 3.0 * (2.0 * std::f64::consts::PI).sqrt() / 4.0 + 0.5)
        .floor()
        .max(1.0);
    if !window.is_finite() || window > MAX_LAYER_BYTES as f64 / 24.0 {
        return Err(ImageError::TooLarge);
    }
    let window = window as usize;
    let sizes = [window, window, window + usize::from(window % 2 == 0)];
    let padding = (sizes.iter().sum::<usize>() - 3) / 2;
    Ok((sizes, padding))
}

/// Blur an alpha plane in place using the same bounded three-box Gaussian
/// approximation as CSS box shadows. `reserved_bytes` accounts for storage
/// owned by the caller while this helper is active (for example the temporary
/// pixmap from which a canvas alpha plane was extracted).
pub(super) fn blur_alpha_mask(
    mask: &mut [u8],
    width: usize,
    height: usize,
    blur: f32,
    scale: f32,
    reserved_bytes: usize,
) -> Result<(), ImageError> {
    let Some(pixels) = width.checked_mul(height) else {
        return Err(ImageError::TooLarge);
    };
    if pixels != mask.len() {
        return Err(ImageError::InvalidViewport);
    }
    validate_blur_budget(width, height, blur, scale, reserved_bytes)?;
    let (sizes, _) = blur_sizes(blur, scale)?;
    if sizes[0] == 1 {
        return Ok(());
    }
    let ring_len = sizes
        .iter()
        .try_fold(0usize, |sum, size| sum.checked_add(*size))
        .and_then(|sum| sum.checked_mul(LANES))
        .ok_or(ImageError::TooLarge)?;
    let mut scratch = Vec::new();
    scratch
        .try_reserve_exact(pixels)
        .map_err(|_| ImageError::TooLarge)?;
    scratch.resize(pixels, 0);
    let mut ring = Vec::new();
    ring.try_reserve_exact(ring_len)
        .map_err(|_| ImageError::TooLarge)?;
    ring.resize(ring_len, 0);
    for y in 0..height {
        let start = y * width;
        scan(
            &mask[start..],
            &mut scratch[start..],
            width,
            1,
            sizes,
            &mut ring,
        );
    }
    for x0 in (0..width).step_by(LANES) {
        let lanes = LANES.min(width - x0);
        scan_columns(&scratch, mask, width, height, x0, lanes, sizes, &mut ring);
    }
    Ok(())
}

pub(super) fn validate_blur_budget(
    width: usize,
    height: usize,
    blur: f32,
    scale: f32,
    reserved_bytes: usize,
) -> Result<(), ImageError> {
    let pixels = width.checked_mul(height).ok_or(ImageError::TooLarge)?;
    let (sizes, _) = blur_sizes(blur, scale)?;
    if sizes[0] == 1 {
        let total_bytes = pixels
            .checked_add(reserved_bytes)
            .ok_or(ImageError::TooLarge)?;
        return if total_bytes <= MAX_LAYER_BYTES {
            Ok(())
        } else {
            Err(ImageError::TooLarge)
        };
    }
    let ring_len = sizes
        .iter()
        .try_fold(0usize, |sum, size| sum.checked_add(*size))
        .and_then(|sum| sum.checked_mul(LANES))
        .ok_or(ImageError::TooLarge)?;
    let ring_bytes = ring_len
        .checked_mul(core::mem::size_of::<u64>())
        .ok_or(ImageError::TooLarge)?;
    let total_bytes = pixels
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(ring_bytes))
        .and_then(|bytes| bytes.checked_add(reserved_bytes))
        .ok_or(ImageError::TooLarge)?;
    if total_bytes > MAX_LAYER_BYTES {
        return Err(ImageError::TooLarge);
    }
    Ok(())
}

fn coverage(
    rect: Rect,
    radius: f32,
    corners: Option<&[[f32; 2]; 4]>,
    x: f32,
    y: f32,
    scale: f32,
    antialias: bool,
) -> f32 {
    if let Some(corners) = corners {
        return if antialias {
            super::coverage::rounded_corners(
                x,
                y,
                rect.x * scale,
                rect.y * scale,
                (rect.x + rect.width) * scale,
                (rect.y + rect.height) * scale,
                corners.map(|r| [r[0] * scale, r[1] * scale]),
            )
        } else {
            rect.contains_corners((x + 0.5) / scale, (y + 0.5) / scale, corners) as u8 as f32
        };
    }
    let radius = radius.min(rect.width * 0.5).min(rect.height * 0.5);
    let (left, top, right, bottom) = (x / scale, y / scale, (x + 1.0) / scale, (y + 1.0) / scale);
    // Every sample point lies inside the pixel, so none can hit a disjoint rect.
    if right < rect.x
        || bottom < rect.y
        || left >= rect.x + rect.width
        || top >= rect.y + rect.height
    {
        return 0.0;
    }
    if left >= rect.x
        && top >= rect.y
        && right <= rect.x + rect.width
        && bottom <= rect.y + rect.height
        && (left >= rect.x + radius && right <= rect.x + rect.width - radius
            || top >= rect.y + radius && bottom <= rect.y + rect.height - radius)
    {
        return 1.0;
    }
    let samples = if antialias { 4 } else { 1 };
    let mut hits = 0;
    for sy in 0..samples {
        for sx in 0..samples {
            hits += rect.contains_rounded(
                (x + (sx as f32 + 0.5) / samples as f32) / scale,
                (y + (sy as f32 + 0.5) / samples as f32) / scale,
                radius,
            ) as u32;
        }
    }
    hits as f32 / (samples * samples) as f32
}

// Three centered box convolutions approximate a Gaussian in linear time. Keep
// unnormalized sums until the final stage to avoid rounding each intermediate pass.
fn scan(
    source: &[u8],
    output: &mut [u8],
    length: usize,
    stride: usize,
    sizes: [usize; 3],
    ring: &mut [u64],
) {
    ring.fill(0);
    let starts = [0, sizes[0], sizes[0] + sizes[1]];
    let mut cursors = [0; 3];
    let mut sums = [0u64; 3];
    let divisor = sizes.iter().map(|n| *n as u64).product::<u64>();
    let padding = (sizes.iter().sum::<usize>() - 3) / 2;
    for index in 0..length + padding {
        let mut value = if index < length {
            source[index * stride] as u64
        } else {
            0
        };
        for stage in 0..3 {
            let slot = starts[stage] + cursors[stage];
            sums[stage] += value;
            sums[stage] -= ring[slot];
            ring[slot] = value;
            value = sums[stage];
            cursors[stage] += 1;
            if cursors[stage] == sizes[stage] {
                cursors[stage] = 0;
            }
        }
        if index >= padding {
            output[(index - padding) * stride] = ((value + divisor / 2) / divisor) as u8;
        }
    }
}

const LANES: usize = 16;

// Column-wise `scan` over `lanes` adjacent columns at once, so every row read touches
// contiguous bytes. ring holds `LANES` entries per slot.
#[allow(clippy::too_many_arguments)]
fn scan_columns(
    source: &[u8],
    output: &mut [u8],
    width: usize,
    height: usize,
    x0: usize,
    lanes: usize,
    sizes: [usize; 3],
    ring: &mut [u64],
) {
    ring.fill(0);
    let starts = [0, sizes[0], sizes[0] + sizes[1]];
    let mut cursors = [0; 3];
    let mut sums = [[0u64; LANES]; 3];
    let divisor = sizes.iter().map(|n| *n as u64).product::<u64>();
    let padding = (sizes.iter().sum::<usize>() - 3) / 2;
    for index in 0..height + padding {
        let mut value = [0u64; LANES];
        if index < height {
            let row = &source[index * width + x0..][..lanes];
            for (slot, byte) in value.iter_mut().zip(row) {
                *slot = *byte as u64;
            }
        }
        for stage in 0..3 {
            let base = (starts[stage] + cursors[stage]) * LANES;
            let slots = &mut ring[base..base + lanes];
            let sum = &mut sums[stage];
            for lane in 0..lanes {
                sum[lane] += value[lane];
                sum[lane] -= slots[lane];
                slots[lane] = value[lane];
                value[lane] = sum[lane];
            }
            cursors[stage] += 1;
            if cursors[stage] == sizes[stage] {
                cursors[stage] = 0;
            }
        }
        if index >= padding {
            let out = &mut output[(index - padding) * width + x0..][..lanes];
            for (byte, total) in out.iter_mut().zip(&value) {
                *byte = ((total + divisor / 2) / divisor) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_html::paint::{Command, DisplayList, Rgba};

    #[test]
    fn rolling_three_box_filter_is_centered_and_normalized() {
        let source = [0, 0, 0, 0, 255, 0, 0, 0, 0];
        let mut output = [0; 9];
        scan(&source, &mut output, 9, 1, [3; 3], &mut [0; 9]);
        assert_eq!(output, [0, 9, 28, 57, 66, 57, 28, 9, 0]);
    }

    #[test]
    fn sharp_shadow_excludes_the_box_and_inset_stays_inside() {
        let rect = Rect {
            x: 2.0,
            y: 2.0,
            width: 4.0,
            height: 4.0,
        };
        for inset in [false, true] {
            let shadow = BoxShadow {
                offset_x: 2.0,
                offset_y: 0.0,
                blur: 0.0,
                spread: 0.0,
                color: Rgba {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 255,
                },
                inset,
            };
            let list = DisplayList(vec![Command::BoxShadow {
                corners: None,
                rect,
                radius: 0.0,
                shadow,
            }]);
            let image = crate::render(&list, 10, 10, 1.0, true).unwrap();
            for y in 0..10 {
                for x in 0..10 {
                    let expected = if inset {
                        (2..4).contains(&x)
                    } else {
                        (6..8).contains(&x)
                    } && (2..6).contains(&y);
                    assert_eq!(image.pixels[(y * 10 + x) * 4 + 3] != 0, expected);
                }
            }
        }
    }

    #[test]
    fn spread_keeps_square_shadow_corners_square() {
        let shadow = BoxShadow {
            offset_x: 8.0,
            offset_y: 0.0,
            blur: 0.0,
            spread: 2.0,
            color: lumen_html::paint::Rgba {
                r: 255,
                g: 0,
                b: 0,
                a: 255,
            },
            inset: false,
        };
        let list = DisplayList(vec![Command::BoxShadow {
            corners: None,
            rect: Rect {
                x: 2.0,
                y: 2.0,
                width: 4.0,
                height: 4.0,
            },
            radius: 0.0,
            shadow,
        }]);
        let image = crate::render(&list, 16, 10, 1.0, true).unwrap();
        assert_eq!(&image.pixels[8 * 4..][..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn excessive_shadow_workspace_returns_resource_error() {
        let shadow = BoxShadow {
            offset_x: 0.0,
            offset_y: 0.0,
            blur: 1_000_000.0,
            spread: 0.0,
            color: Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            },
            inset: false,
        };
        let list = DisplayList(vec![Command::BoxShadow {
            corners: None,
            rect: Rect {
                x: 2.0,
                y: 2.0,
                width: 4.0,
                height: 4.0,
            },
            radius: 0.0,
            shadow,
        }]);
        assert_eq!(
            crate::render(&list, 10, 10, 1.0, true),
            Err(ImageError::TooLarge)
        );
    }
}
