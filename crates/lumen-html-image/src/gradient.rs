use super::{Raster, composite};
use lumen_html::paint::{
    Gradient, GradientKind, GradientPosition, RadialShape, RadialSize, Rect, Rgba,
};

enum Geometry {
    Linear {
        x: f32,
        y: f32,
        dx: f32,
        dy: f32,
        length: f32,
    },
    Radial {
        x: f32,
        y: f32,
        rx: f32,
        ry: f32,
    },
}
pub(super) struct Prepared<'a> {
    gradient: &'a Gradient,
    offsets: [f32; 32],
    geometry: Geometry,
    solid: Option<Rgba>,
}
impl<'a> Prepared<'a> {
    pub(super) fn new(gradient: &'a Gradient, rect: Rect) -> Option<Self> {
        if !gradient.is_valid() || rect.width <= 0.0 || rect.height <= 0.0 {
            return None;
        }
        let geometry = match gradient.kind {
            GradientKind::Linear { angle, corner } => {
                let (dx, dy) = if let Some((x, y)) = corner {
                    (rect.height * x as f32, -rect.width * y as f32)
                } else {
                    let radians = angle.rem_euclid(360.0).to_radians();
                    (radians.sin(), -radians.cos())
                };
                let norm = dx.hypot(dy);
                if norm == 0.0 {
                    return None;
                }
                let (dx, dy) = (dx / norm, dy / norm);
                Geometry::Linear {
                    x: rect.x + rect.width * 0.5,
                    y: rect.y + rect.height * 0.5,
                    dx,
                    dy,
                    length: rect.width * dx.abs() + rect.height * dy.abs(),
                }
            }
            GradientKind::Radial {
                shape,
                size,
                center,
            } => {
                let (x, y) = (
                    center[0].resolve(rect.width),
                    center[1].resolve(rect.height),
                );
                let (near_x, far_x) = (
                    x.abs().min((rect.width - x).abs()),
                    x.abs().max((rect.width - x).abs()),
                );
                let (near_y, far_y) = (
                    y.abs().min((rect.height - y).abs()),
                    y.abs().max((rect.height - y).abs()),
                );
                let near = matches!(size, RadialSize::ClosestSide | RadialSize::ClosestCorner);
                let corners =
                    matches!(size, RadialSize::ClosestCorner | RadialSize::FarthestCorner);
                let (mut rx, mut ry) = match size {
                    RadialSize::Radii(radii) => (
                        radii[0].resolve(rect.width).max(0.0),
                        radii[1].resolve(rect.height).max(0.0),
                    ),
                    _ => {
                        if near {
                            (near_x, near_y)
                        } else {
                            (far_x, far_y)
                        }
                    }
                };
                if !matches!(size, RadialSize::Radii(_)) {
                    if shape == RadialShape::Circle {
                        let r = if corners {
                            let values = [
                                x.hypot(y),
                                (rect.width - x).hypot(y),
                                x.hypot(rect.height - y),
                                (rect.width - x).hypot(rect.height - y),
                            ];
                            values
                                .into_iter()
                                .fold(if near { f32::INFINITY } else { 0.0 }, |a, b| {
                                    if near { a.min(b) } else { a.max(b) }
                                })
                        } else if near {
                            rx.min(ry)
                        } else {
                            rx.max(ry)
                        };
                        (rx, ry) = (r, r);
                    } else if corners && rx > 0.0 && ry > 0.0 {
                        let (cx, cy) = if near {
                            (near_x, near_y)
                        } else {
                            (far_x, far_y)
                        };
                        let multiplier = (cx / rx).hypot(cy / ry);
                        rx *= multiplier;
                        ry *= multiplier;
                    }
                }
                Geometry::Radial {
                    x: rect.x + x,
                    y: rect.y + y,
                    rx,
                    ry,
                }
            }
        };
        let length = match geometry {
            Geometry::Linear { length, .. } => length,
            Geometry::Radial { rx, .. } => rx,
        };
        let count = gradient.stops.len();
        let mut offsets = [f32::NAN; 32];
        for (index, stop) in gradient.stops.iter().enumerate() {
            offsets[index] = match stop.position {
                Some(GradientPosition::Fraction(v)) => v,
                Some(GradientPosition::Pixels(v)) => v / length.max(f32::MIN_POSITIVE),
                Some(GradientPosition::Mixed(v)) => {
                    v.resolve(length) / length.max(f32::MIN_POSITIVE)
                }
                None => f32::NAN,
            };
        }
        if offsets[0].is_nan() {
            offsets[0] = 0.0;
        }
        if offsets[count - 1].is_nan() {
            offsets[count - 1] = 1.0;
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
        let degenerate = match geometry {
            Geometry::Radial { rx, ry, .. } => rx == 0.0 || ry == 0.0,
            _ => length == 0.0,
        };
        let mut result = Self {
            gradient,
            offsets,
            geometry,
            solid: None,
        };
        if gradient.repeating && (offsets[count - 1] <= offsets[0] || degenerate) {
            result.solid = Some(result.average());
        } else if degenerate {
            result.solid = Some(gradient.stops[count - 1].color);
        }
        Some(result)
    }
    fn average(&self) -> Rgba {
        let count = self.gradient.stops.len();
        let total = self.offsets[count - 1] - self.offsets[0];
        let mut sum = [0.0; 4];
        for index in 1..count {
            let weight = if total > 0.0 {
                (self.offsets[index] - self.offsets[index - 1]) / total * 0.5
            } else {
                0.5 / (count - 1) as f32
            };
            for color in [
                self.gradient.stops[index - 1].color,
                self.gradient.stops[index].color,
            ] {
                let alpha = color.a as f32;
                sum[3] += alpha * weight;
                for (channel, value) in [color.r, color.g, color.b].into_iter().enumerate() {
                    sum[channel] += value as f32 * alpha * weight;
                }
            }
        }
        if sum[3] == 0.0 {
            return Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 0,
            };
        }
        Rgba {
            r: (sum[0] / sum[3]).round() as u8,
            g: (sum[1] / sum[3]).round() as u8,
            b: (sum[2] / sum[3]).round() as u8,
            a: sum[3].round() as u8,
        }
    }
    pub(super) fn color(&self, px: f32, py: f32) -> Rgba {
        if let Some(color) = self.solid {
            return color;
        }
        let mut position = match self.geometry {
            Geometry::Linear {
                x,
                y,
                dx,
                dy,
                length,
            } => 0.5 + ((px - x) * dx + (py - y) * dy) / length,
            Geometry::Radial { x, y, rx, ry } => ((px - x) / rx).hypot((py - y) / ry),
        };
        let count = self.gradient.stops.len();
        if self.gradient.repeating {
            position = (position - self.offsets[0])
                .rem_euclid(self.offsets[count - 1] - self.offsets[0])
                + self.offsets[0];
        }
        let end = self.offsets[..count].partition_point(|offset| *offset <= position);
        if end == 0 {
            self.gradient.stops[0].color
        } else if end == count {
            self.gradient.stops[count - 1].color
        } else {
            interpolate(
                self.gradient.stops[end - 1].color,
                self.gradient.stops[end].color,
                ((position - self.offsets[end - 1]) / (self.offsets[end] - self.offsets[end - 1]))
                    .clamp(0.0, 1.0),
            )
        }
    }
}

pub(super) fn fill(raster: &mut Raster<'_>, rect: Rect, radius: f32, gradient: &Gradient) {
    let Some(prepared) = Prepared::new(gradient, rect) else {
        return;
    };
    let Some(visible) = raster.clips.last().and_then(|clip| clip.intersection(rect)) else {
        return;
    };
    let scale = raster.scale;
    let (x0, y0) = (
        (visible.x * scale).floor().max(0.0) as u32,
        (visible.y * scale).floor().max(0.0) as u32,
    );
    let (x1, y1) = (
        ((visible.x + visible.width) * scale)
            .ceil()
            .min(raster.image.width as f32) as u32,
        ((visible.y + visible.height) * scale)
            .ceil()
            .min(raster.image.height as f32) as u32,
    );
    for y in y0..y1 {
        for x in x0..x1 {
            let color = prepared.color((x as f32 + 0.5) / scale, (y as f32 + 0.5) / scale);
            let coverage = if raster.antialias {
                super::coverage::rounded(
                    x as f32,
                    y as f32,
                    rect.x * scale,
                    rect.y * scale,
                    (rect.x + rect.width) * scale,
                    (rect.y + rect.height) * scale,
                    radius * scale,
                )
            } else {
                rect.contains_rounded((x as f32 + 0.5) / scale, (y as f32 + 0.5) / scale, radius)
                    as u8 as f32
            };
            let offset = (y as usize * raster.image.width as usize + x as usize) * 4;
            let pixel = &mut raster.image.pixels[offset..offset + 4];
            if coverage == 1.0 && color.a == 255 {
                pixel.copy_from_slice(&[color.r, color.g, color.b, 255]);
            } else if coverage > 0.0 {
                composite(pixel, color, coverage);
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
        let gradient = Arc::new(Gradient {
            kind: GradientKind::Linear {
                angle: 90.0,
                corner: None,
            },
            repeating: false,
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
        let list = DisplayList(vec![Command::FillGradient {
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
        let gradient = Arc::new(Gradient {
            kind: GradientKind::Linear {
                angle: 135.0,
                corner: None,
            },
            repeating: false,
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
        let list = DisplayList(vec![Command::FillGradient {
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
