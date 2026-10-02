use super::{ImageError, MAX_LAYER_BYTES, Raster, composite};
use lumen_html::paint::{BoxShadow, Rect};

pub(super) fn draw(
    raster: &mut Raster<'_>,
    rect: Rect,
    radius: f32,
    shadow: BoxShadow,
) -> Result<(), ImageError> {
    if shadow.color.a == 0 {
        return Ok(());
    }
    let Some(visible) = raster
        .clips
        .last()
        .and_then(|clip| clip.intersection(shadow.bounds(rect)))
    else {
        return Ok(());
    };
    let sigma = shadow.blur as f64 * raster.scale as f64 * 0.5;
    let window = (sigma * 3.0 * (2.0 * std::f64::consts::PI).sqrt() / 4.0 + 0.5)
        .floor()
        .max(1.0);
    if !window.is_finite() || window > MAX_LAYER_BYTES as f64 / 24.0 {
        return Err(ImageError::TooLarge);
    }
    let window = window as usize;
    let sizes = [window, window, window + usize::from(window % 2 == 0)];
    let padding = (sizes.iter().sum::<usize>() - 3) / 2;
    let left = (visible.x * raster.scale).floor() - padding as f32;
    let top = (visible.y * raster.scale).floor() - padding as f32;
    let width = ((visible.x + visible.width) * raster.scale).ceil() - left + padding as f32;
    let height = ((visible.y + visible.height) * raster.scale).ceil() - top + padding as f32;
    if width <= 0.0 || height <= 0.0 {
        return Ok(());
    }
    if width > usize::MAX as f32 || height > usize::MAX as f32 {
        return Err(ImageError::TooLarge);
    }
    let (width, height) = (width as usize, height as usize);
    let ring_bytes = sizes.iter().sum::<usize>() * 8;
    let pixels =
        lumen_common::limits::size::repeat(width, height, (MAX_LAYER_BYTES - ring_bytes) / 2)
            .map_err(|_| ImageError::TooLarge)?;
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
    for y in 0..height {
        for x in 0..width {
            mask[y * width + x] = (coverage(
                shape,
                shape_radius,
                left + x as f32,
                top + y as f32,
                raster.scale,
                raster.antialias,
            ) * 255.0)
                .round() as u8;
        }
    }
    if window > 1 {
        let mut scratch = vec![0u8; pixels];
        let mut ring = vec![0u64; sizes.iter().sum()];
        for y in 0..height {
            scan(
                &mask[y * width..],
                &mut scratch[y * width..],
                width,
                1,
                sizes,
                &mut ring,
            );
        }
        for x in 0..width {
            scan(
                &scratch[x..],
                &mut mask[x..],
                height,
                width,
                sizes,
                &mut ring,
            );
        }
    }
    let x0 = (visible.x * raster.scale).floor().max(0.0) as u32;
    let y0 = (visible.y * raster.scale).floor().max(0.0) as u32;
    let x1 = ((visible.x + visible.width) * raster.scale)
        .ceil()
        .min(raster.image.width as f32) as u32;
    let y1 = ((visible.y + visible.height) * raster.scale)
        .ceil()
        .min(raster.image.height as f32) as u32;
    for y in y0..y1 {
        for x in x0..x1 {
            let original = coverage(
                rect,
                radius,
                x as f32,
                y as f32,
                raster.scale,
                raster.antialias,
            );
            let mask_offset = (y as f32 - top) as usize * width + (x as f32 - left) as usize;
            let alpha = mask[mask_offset] as f32 / 255.0;
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

fn coverage(rect: Rect, radius: f32, x: f32, y: f32, scale: f32, antialias: bool) -> f32 {
    let radius = radius.min(rect.width * 0.5).min(rect.height * 0.5);
    let (left, top, right, bottom) = (x / scale, y / scale, (x + 1.0) / scale, (y + 1.0) / scale);
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
        assert_eq!(&image.pixels[(0 * 16 + 8) * 4..][..4], &[255, 0, 0, 255]);
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
