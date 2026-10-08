use super::{composite, Raster};
use lumen_common::color::{Color, ColorSpace, HueInterpolation, InterpolationMethod, PreparedPair};
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
    Conic {
        x: f32,
        y: f32,
        from: f32,
    },
}
fn prepare_conic_hints(offsets: &mut [f32; 32], count: usize, metadata: impl Iterator<Item=(usize,f32)>) -> [f32; 32] {
    // Stop and hint order is one component sequence. An intervening hint
    // terminates a run of adjacent unpositioned color stops.
    let mut hints = [f32::NAN; 32];
    for (after, position) in metadata { hints[after + 1] = position; }
    let mut sequence = [f32::NAN; 63];
    let mut color_indices = [0usize; 32];
    let mut hint_indices = [usize::MAX; 32];
    let mut sequence_length = 0;
    for index in 0..count {
        color_indices[index] = sequence_length;
        sequence[sequence_length] = offsets[index];
        sequence_length += 1;
        if index + 1 < count && hints[index + 1].is_finite() {
            hint_indices[index + 1] = sequence_length;
            sequence[sequence_length] = hints[index + 1];
            sequence_length += 1;
        }
    }
    let mut anchor = 0;
    for end in 1..sequence_length {
        if sequence[end].is_nan() { continue; }
        sequence[end] = sequence[end].max(sequence[anchor]);
        for index in anchor + 1..end {
            sequence[index] = sequence[anchor]
                + (sequence[end] - sequence[anchor]) * (index - anchor) as f32
                    / (end - anchor) as f32;
        }
        anchor = end;
    }
    for index in 0..count {
        offsets[index] = sequence[color_indices[index]];
        if hint_indices[index] != usize::MAX { hints[index] = sequence[hint_indices[index]]; }
    }
    for end in 1..count {
        if hints[end].is_finite() {
            let span = offsets[end] - offsets[end - 1];
            let midpoint = if span > 0.0 {
                ((hints[end] - offsets[end - 1]) / span).clamp(0.0, 1.0)
            } else { 0.5 };
            // Store the exponent once, not a logarithm per painted pixel.
            hints[end] = if midpoint == 0.0 { 0.0 }
                else if midpoint == 1.0 { f32::INFINITY }
                else { 0.5f32.ln() / midpoint.ln() };
        }
    }
    hints
}

pub(super) struct Prepared<'a> {
    gradient: &'a Gradient,
    offsets: [f32; 32],
    hints: [f32; 32],
    pairs: [Option<PreparedPair>; 32],
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
                            values.into_iter().fold(
                                if near { f32::INFINITY } else { 0.0 },
                                |a, b| {
                                    if near {
                                        a.min(b)
                                    } else {
                                        a.max(b)
                                    }
                                },
                            )
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
            GradientKind::Conic { from, center } => Geometry::Conic {
                x: rect.x + center[0].resolve(rect.width),
                y: rect.y + center[1].resolve(rect.height),
                from,
            },
        };
        // A conic turn is 360 virtual units so angular stop positions
        // normalize through the shared solver.
        let length = match geometry {
            Geometry::Linear { length, .. } => length,
            Geometry::Radial { rx, .. } => rx,
            Geometry::Conic { .. } => 360.0,
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
        let hints = if let Some(metadata) = gradient.angular.as_ref().filter(|metadata| metadata.hints().next().is_some()) {
            prepare_conic_hints(&mut offsets, count, metadata.hints())
        } else if let Some(metadata)=gradient.color.as_ref().filter(|metadata|!metadata.hints.is_empty()) {
            prepare_conic_hints(&mut offsets,count,metadata.hints.iter().map(|(after,position)| {
                let position=match position {
                    GradientPosition::Fraction(value)=>*value,
                    GradientPosition::Pixels(value)=>*value/length.max(f32::MIN_POSITIVE),
                    GradientPosition::Mixed(value)=>value.resolve(length)/length.max(f32::MIN_POSITIVE),
                };(*after,position)
            }))
        } else {
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
            [f32::NAN; 32]
        };
        let degenerate = match geometry {
            Geometry::Radial { rx, ry, .. } => rx == 0.0 || ry == 0.0,
            _ => length == 0.0,
        };
        let method=gradient.color.as_ref().map_or(InterpolationMethod{space:ColorSpace::Srgb,hue:HueInterpolation::Shorter},|metadata|metadata.method);
        let endpoint=|index:usize|gradient.color.as_ref().and_then(|metadata|metadata.colors.get(index)).copied()
            .unwrap_or_else(|| {let color=gradient.stops[index].color;Color::rgba8([color.r,color.g,color.b,color.a])});
        let mut pairs=[None;32];
        let exact_srgb=method.space==ColorSpace::Srgb && gradient.color.as_ref().is_none_or(|metadata|metadata.colors.is_empty());
        if !exact_srgb { for end in 1..count {
            let left=endpoint(end-1);let right=endpoint(end);
            let same=left==right && !(method.space.hue().is_some() && method.hue==HueInterpolation::Longer);
            if !exact_srgb && !same {pairs[end]=Some(PreparedPair::new(left,right,method));}
        }
        }
        let mut result = Self {
            gradient,
            offsets,
            hints,
            pairs,
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
        if let Some(colors)=self.gradient.color.as_ref().map(|metadata|&metadata.colors).filter(|colors|!colors.is_empty()) {
            // CSS Images' degenerate-repeat average is premultiplied sRGBA,
            // independently of the interpolation method of a visible segment.
            let total=self.offsets[colors.len()-1]-self.offsets[0];let mut sum=[0.0;4];
            for end in 1..colors.len() {
                let weight=if total>0.0 {(self.offsets[end]-self.offsets[end-1])/total*0.5}else {0.5/(colors.len()-1) as f32};
                for color in [colors[end-1],colors[end]] {
                    let color=color.to(ColorSpace::Srgb);let alpha=if color.missing&8==0 {color.alpha}else {0.0};
                    sum[3]+=alpha*weight;
                    for i in 0..3 {sum[i]+=color.components[i]*alpha*weight;}
                }
            }
            if sum[3]>0.0 {for i in 0..3 {sum[i]/=sum[3];}}
            let [r,g,b,a]=Color::new(ColorSpace::Srgb,[sum[0],sum[1],sum[2]],sum[3],0).to_rgba8();return Rgba{r,g,b,a};
        }
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
    /// The part of the gradient parameter that depends only on the row.
    pub(super) fn row_term(&self, py: f32) -> f32 {
        match self.geometry {
            Geometry::Linear { y, dy, .. } => (py - y) * dy,
            Geometry::Radial { y, ry, .. } => (py - y) / ry,
            Geometry::Conic { y, .. } => py - y,
        }
    }
    /// `hint` caches the last stop segment; it only skips the search, never changes the result.
    pub(super) fn color_at(&self, px: f32, row: f32, hint: &mut usize) -> Rgba {
        if let Some(color) = self.solid {
            return color;
        }
        let mut position = match self.geometry {
            Geometry::Linear { x, dx, length, .. } => 0.5 + ((px - x) * dx + row) / length,
            Geometry::Radial { x, rx, .. } => ((px - x) / rx).hypot(row),
            // CSS degrees: zero points up, angles sweep clockwise.
            Geometry::Conic { x, from, .. } => {
                let dx = px - x;
                let angle = dx.atan2(-row).to_degrees().rem_euclid(360.0);
                ((angle - from).rem_euclid(360.0)) / 360.0
            }
        };
        let count = self.gradient.stops.len();
        if self.gradient.repeating {
            position = (position - self.offsets[0])
                .rem_euclid(self.offsets[count - 1] - self.offsets[0])
                + self.offsets[0];
        }
        let mut end = *hint;
        if !((end == 0 || self.offsets[end - 1] <= position)
            && (end == count || self.offsets[end] > position))
        {
            end = self.offsets[..count].partition_point(|offset| *offset <= position);
            *hint = end;
        }
        if end == 0 {
            self.gradient.stops[0].color
        } else if end == count {
            self.gradient.stops[count - 1].color
        } else {
            let mut mix = ((position - self.offsets[end - 1])
                / (self.offsets[end] - self.offsets[end - 1])).clamp(0.0, 1.0);
            let exponent = self.hints[end];
            if !exponent.is_nan() && exponent != 1.0 {
                mix = if exponent == 0.0 { 1.0 }
                    else if exponent.is_infinite() { 0.0 }
                    else { mix.powf(exponent) };
            }
            if let Some(pair)=self.pairs[end] {
                let [r,g,b,a]=pair.sample(mix).to_rgba8();Rgba{r,g,b,a}
            } else {interpolate(self.gradient.stops[end-1].color,self.gradient.stops[end].color,mix)}
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
    let (x0, y0, x1, y1) = raster.pixel_span(visible);
    let mut hint = 0;
    for y in y0..y1 {
        let row = prepared.row_term((y as f32 + 0.5) / scale);
        for x in x0..x1 {
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
            if !(coverage > 0.0) {
                continue;
            }
            let color = prepared.color_at((x as f32 + 0.5) / scale, row, &mut hint);
            let offset = (y as usize * raster.image.width as usize + x as usize) * 4;
            let pixel = &mut raster.image.pixels[offset..offset + 4];
            if coverage == 1.0 && color.a == 255 {
                pixel.copy_from_slice(&[color.r, color.g, color.b, 255]);
            } else {
                composite(pixel, color, coverage);
            }
        }
    }
}

pub(crate) fn interpolate(from: Rgba, to: Rgba, mix: f32) -> Rgba {
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
    fn inline_blocks_flow_side_by_side() {
        let font =
            lumen_html_text::FontFace::new(std::sync::Arc::from(lumen_html_text::TEST_FONT_BYTES))
                .unwrap();
        let html = "<style>html,body{margin:0;background:#fff}div{width:100px;height:145px;margin:0;display:inline-block;vertical-align:top}div:first-child{background:#f00}div:last-child{background:#0f0}</style><div></div><div></div>";
        let image = crate::render_html_with_font(html, 200, 200, 1.0, &font).unwrap();
        let at = |x: usize, y: usize| {
            let o = (y * 200 + x) * 4;
            &image.pixels[o..o + 3]
        };
        assert_eq!(at(5, 10), [255, 0, 0]);
        assert_eq!(at(105, 10), [0, 255, 0]);
    }

    #[test]
    fn inline_blocks_wrap_when_the_line_cannot_fit() {
        let font =
            lumen_html_text::FontFace::new(std::sync::Arc::from(lumen_html_text::TEST_FONT_BYTES))
                .unwrap();
        let html = "<style>html,body{margin:0;background:#fff}div{width:145px;height:145px;margin:5px;display:inline-block;vertical-align:top}div:first-child{background:#f00}div:last-child{background:#0f0}</style><div></div><div></div>";
        let image = crate::render_html_with_font(html, 300, 320, 1.0, &font).unwrap();
        let at = |x: usize, y: usize| {
            let o = (y * 300 + x) * 4;
            &image.pixels[o..o + 3]
        };
        assert_eq!(at(10, 10), [255, 0, 0]);
        assert_eq!(at(160, 10), [255, 255, 255]);
        assert_eq!(at(10, 180), [0, 255, 0]);
    }

    #[test]
    fn specification_gradient_color_spaces_paint_real_midpoints_alpha_missing_and_longer_hue() {
        let font=lumen_html_text::FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let render=|source:&str| {
            let html=format!("<style>html,body{{margin:0;background:white}}div{{width:1px;height:1px;background:{source}}}</style><div></div>");
            crate::render_html_with_font(&html,1,1,1.0,&font).unwrap().pixels
        };
        // Independent CSS Color 4 sample conversions: linear-light half gray,
        // and Oklab red/blue midpoint (not an expected value from our converter).
        for (source,expected) in [
            ("linear-gradient(to right in srgb-linear,black,white)",[188u8,188,188,255]),
            ("linear-gradient(to right in oklab,red,blue)",[140,83,162,255]),
            ("linear-gradient(to right,color(srgb 1 0 0),color(srgb 0 0 1))",[140,83,162,255]),
            ("linear-gradient(to right,red,blue)",[128,0,128,255]),
            ("conic-gradient(from 270deg at left center in hsl longer hue,red)",[0,255,255,255]),
            ("linear-gradient(to right in srgb,rgb(none 0 0),blue)",[0,0,128,255]),
        ] {
            let actual=render(source);
            for (a,b) in actual.iter().copied().zip(expected) {assert!((i16::from(a)-i16::from(b)).abs()<=1,"{source}: {actual:?}");}
        }
        // A transparent endpoint cannot bleed its red into the blue endpoint.
        let image=render("linear-gradient(to right in srgb,rgb(255 0 0 / 0),blue)");
        // The normal HTML canvas is white: half-transparent blue composites to
        // (127,127,255), preserving the real shared raster/compositor path.
        assert!(image[0].abs_diff(127)<=1 && image[1].abs_diff(127)<=1 && image[2]==255 && image[3]==255,"{image:?}");
    }

    #[test]
    fn specification_conic_hints_use_fixed_capacity_stop_fixup_and_exponential_weighting() {
        let font = lumen_html_text::FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        let html = "<style>html,body{margin:0}div{width:100px;height:100px;background:conic-gradient(in srgb,red 0deg,90deg,blue 360deg)}</style><div></div>";
        let image = crate::render_html_with_font(html, 100, 100, 1.0, &font).unwrap();
        let offset = (49 * 100 + 99) * 4;
        let pixel = &image.pixels[offset..offset+4];
        assert!((i16::from(pixel[0])-128).abs() <= 2 && (i16::from(pixel[2])-128).abs() <= 2, "hint midpoint: {pixel:?}");
        assert_eq!(pixel[1], 0);
        assert_eq!(pixel[3], 255);
        // A later hint raises a subsequent explicit stop during ordered fixup.
        let html = "<style>html,body{margin:0}div{width:100px;height:100px;background:conic-gradient(red 0deg,270deg,blue 90deg)}</style><div></div>";
        let image = crate::render_html_with_font(html, 100, 100, 1.0, &font).unwrap();
        assert_eq!(&image.pixels[offset..offset+4], &[255,0,0,255]);
    }

    #[test]
    fn conic_gradient_sweeps_clockwise_from_the_top() {
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
            angular: None,
            color: None,
            kind: GradientKind::Conic {
                from: 0.0,
                center: [
                    lumen_html::paint::LengthPercentage {
                        pixels: 1.0,
                        fraction: 0.0,
                    },
                    lumen_html::paint::LengthPercentage {
                        pixels: 2.0,
                        fraction: 0.0,
                    },
                ],
            },
            repeating: false,
            stops: Arc::from([
                GradientStop {
                    color: red,
                    position: None,
                },
                GradientStop {
                    color: blue,
                    position: Some(GradientPosition::Fraction(0.5)),
                },
                GradientStop {
                    color: red,
                    position: Some(GradientPosition::Fraction(1.0)),
                },
            ]),
        });
        let list = DisplayList(vec![Command::FillGradient {
            rect: Rect {
                x: 0.0,
                y: 0.0,
                width: 2.0,
                height: 4.0,
            },
            radius: 0.0,
            gradient,
        }]);
        let pixels = crate::render(&list, 2, 4, 1.0, true).unwrap().pixels;
        let at = |x: usize, y: usize| &pixels[(y * 2 + x) * 4..(y * 2 + x) * 4 + 3];
        // Center (1,2): 18 degrees is near the red start, 162 degrees near the
        // blue middle, and 315 degrees wraps back to red.
        assert!(at(1, 0)[0] > 200, "top is near red {:?}", at(1, 0));
        assert!(at(1, 3)[2] > 200, "bottom is near blue {:?}", at(1, 3));
        assert!(at(0, 1)[0] > 150, "left wraps back to red {:?}", at(0, 1));
    }

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
            angular: None,
            color: None,
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
            [255, 0, 0, 255, 255, 0, 0, 255, 0, 0, 255, 255, 0, 0, 255, 255]
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
            angular: None,
            color: None,
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
