use super::{Raster, composite};
use lumen_html::paint::{GradientPosition, LinearGradient, Rect, Rgba};

pub(super) fn fill(raster: &mut Raster<'_>, rect: Rect, radius: f32, gradient: &LinearGradient) {
    let Some(visible) = raster.clips.last().and_then(|clip| clip.intersection(rect)) else {
        return;
    };
    let (dx, dy) = if let Some((x, y)) = gradient.corner {
        (rect.height * x as f32, -rect.width * y as f32)
    } else {
        let radians = gradient.angle.rem_euclid(360.0).to_radians();
        (radians.sin(), -radians.cos())
    };
    let norm = dx.hypot(dy);
    if norm == 0.0 {
        return;
    }
    let (dx, dy) = (dx / norm, dy / norm);
    let length = rect.width * dx.abs() + rect.height * dy.abs();
    if length <= 0.0 {
        return;
    }
    let count = gradient.stops.len();
    let mut offsets = [f32::NAN; 32];
    for (index, stop) in gradient.stops.iter().enumerate() {
        offsets[index] = match stop.position {
            Some(GradientPosition::Fraction(value)) => value,
            Some(GradientPosition::Pixels(value)) => value / length,
            None => f32::NAN,
        };
    }
    if offsets[0].is_nan() {
        offsets[0] = 0.0
    }
    if offsets[count - 1].is_nan() {
        offsets[count - 1] = 1.0
    }
    let mut anchor = 0;
    for end in 1..count {
        if offsets[end].is_nan() {
            continue;
        }
        offsets[end] = offsets[end].max(offsets[anchor]);
        for index in anchor + 1..end {
            offsets[index] = offsets[anchor]
                + (offsets[end] - offsets[anchor]) * (index - anchor) as f32
                    / (end - anchor) as f32;
        }
        anchor = end;
    }
    let x0 = (visible.x * raster.scale).floor().max(0.0) as u32;
    let y0 = (visible.y * raster.scale).floor().max(0.0) as u32;
    let x1 = ((visible.x + visible.width) * raster.scale)
        .ceil()
        .min(raster.image.width as f32) as u32;
    let y1 = ((visible.y + visible.height) * raster.scale)
        .ceil()
        .min(raster.image.height as f32) as u32;
    let center_x = rect.x + rect.width * 0.5;
    let center_y = rect.y + rect.height * 0.5;
    let radius = radius.min(rect.width * 0.5).min(rect.height * 0.5);
    for y in y0..y1 {
        for x in x0..x1 {
            let px = (x as f32 + 0.5) / raster.scale;
            let py = (y as f32 + 0.5) / raster.scale;
            let position = 0.5 + ((px - center_x) * dx + (py - center_y) * dy) / length;
            let end = offsets[..count].partition_point(|offset| *offset <= position);
            let color = if end == 0 {
                gradient.stops[0].color
            } else if end == count {
                gradient.stops[count - 1].color
            } else {
                let mix = ((position - offsets[end - 1]) / (offsets[end] - offsets[end - 1]))
                    .clamp(0.0, 1.0);
                interpolate(
                    gradient.stops[end - 1].color,
                    gradient.stops[end].color,
                    mix,
                )
            };
            let samples = if raster.antialias { 4 } else { 1 };
            let mut hits = 0;
            let left = x as f32 / raster.scale;
            let top = y as f32 / raster.scale;
            let right = (x as f32 + 1.0) / raster.scale;
            let bottom = (y as f32 + 1.0) / raster.scale;
            if left >= visible.x
                && top >= visible.y
                && right <= visible.x + visible.width
                && bottom <= visible.y + visible.height
                && (left >= rect.x + radius && right <= rect.x + rect.width - radius
                    || top >= rect.y + radius && bottom <= rect.y + rect.height - radius)
            {
                hits = samples * samples;
            } else {
                for sy in 0..samples {
                    for sx in 0..samples {
                        let px = (x as f32 + (sx as f32 + 0.5) / samples as f32) / raster.scale;
                        let py = (y as f32 + (sy as f32 + 0.5) / samples as f32) / raster.scale;
                        hits += (rect.contains_rounded(px, py, radius)
                            && visible.contains_rounded(px, py, 0.0))
                            as u32;
                    }
                }
            }
            if hits == 0 {
                continue;
            }
            let offset = (y as usize * raster.image.width as usize + x as usize) * 4;
            let pixel = &mut raster.image.pixels[offset..offset + 4];
            if hits == samples * samples && color.a == 255 {
                pixel.copy_from_slice(&[color.r, color.g, color.b, 255]);
            } else {
                composite(pixel, color, hits as f32 / (samples * samples) as f32);
            }
        }
    }
}

fn interpolate(from: Rgba, to: Rgba, mix: f32) -> Rgba {
    let alpha = from.a as f32 * (1.0 - mix) + to.a as f32 * mix;
    if alpha == 0.0 {
        return Rgba {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        };
    }
    let channel = |a: u8, b: u8| {
        ((a as f32 * from.a as f32 * (1.0 - mix) + b as f32 * to.a as f32 * mix) / alpha).round()
            as u8
    };
    Rgba {
        r: channel(from.r, to.r),
        g: channel(from.g, to.g),
        b: channel(from.b, to.b),
        a: alpha.round() as u8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_html::paint::{Command, DisplayList, GradientStop};
    use std::sync::Arc;

    #[test]
    fn hard_stops_preserve_pixel_boundary_and_premultiplied_colors() {
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
        let gradient = Arc::new(LinearGradient {
            angle: 90.0,
            corner: None,
            stops: Arc::from([
                GradientStop {
                    color: red,
                    position: None,
                },
                GradientStop {
                    color: red,
                    position: Some(GradientPosition::Fraction(0.5)),
                },
                GradientStop {
                    color: blue,
                    position: Some(GradientPosition::Fraction(0.5)),
                },
                GradientStop {
                    color: blue,
                    position: None,
                },
            ]),
        });
        let list = DisplayList(vec![Command::FillLinearGradient {
            rect: Rect {
                x: 0.0,
                y: 0.0,
                width: 4.0,
                height: 1.0,
            },
            radius: 0.0,
            gradient,
        }]);
        let pixels = crate::render(&list, 4, 1, 1.0, true).unwrap().pixels;
        assert_eq!(
            pixels,
            [
                255, 0, 0, 255, 255, 0, 0, 255, 0, 0, 255, 255, 0, 0, 255, 255
            ]
        );
        let font =
            lumen_html_text::FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let html = "<style>html,body{margin:0}div{width:4px;height:1px;background:linear-gradient(to right,red 50%,blue 50%)}</style><div></div>";
        assert_eq!(
            crate::render_html_with_font(html, 4, 1, 1.0, &font)
                .unwrap()
                .pixels,
            pixels
        );
        assert_eq!(
            interpolate(red, Rgba { a: 0, ..blue }, 0.5),
            Rgba { a: 128, ..red }
        );
    }

    #[test]
    fn gradient_sprite_fallback_matches_fractional_clipped_raster() {
        let gradient = Arc::new(LinearGradient {
            angle: 135.0,
            corner: None,
            stops: Arc::from([
                GradientStop {
                    color: Rgba {
                        r: 255,
                        g: 0,
                        b: 0,
                        a: 255,
                    },
                    position: None,
                },
                GradientStop {
                    color: Rgba {
                        r: 0,
                        g: 0,
                        b: 255,
                        a: 255,
                    },
                    position: Some(GradientPosition::Pixels(5.0)),
                },
            ]),
        });
        let list = DisplayList(vec![Command::FillLinearGradient {
            rect: Rect {
                x: 0.5,
                y: 1.25,
                width: 6.0,
                height: 5.0,
            },
            radius: 1.5,
            gradient,
        }]);
        let font =
            lumen_html_text::FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let sprites =
            crate::rasterize_layers(&list, 2.0, &font, &mut crate::GlyphCache::default()).unwrap();
        assert!(matches!(sprites.0[0], Command::Image { .. }));
        assert_eq!(
            crate::render(&list, 8, 8, 2.0, true).unwrap(),
            crate::render(&sprites, 8, 8, 2.0, true).unwrap()
        );
    }
}
