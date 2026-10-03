// Dash spacing and thin-dot endpoint adjustment adapted from Chromium's
// styled_stroke_data.cc and box_border_painter.cc.
// Copyright (C) 2013 Google Inc. All rights reserved.
// Copyright 2015 The Chromium Authors.
// Copyright 2018 Google LLC (SkContourMeasure.cpp conic measurement).
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
// * Redistributions of source code must retain the above copyright notice,
//   this list of conditions and the following disclaimer.
// * Redistributions in binary form must reproduce the above copyright notice,
//   this list of conditions and the following disclaimer in the documentation
//   and/or other materials provided with the distribution.
// * Neither the name of Google Inc. nor the names of its contributors may be
//   used to endorse or promote products derived from this software without
//   specific prior written permission.
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
// ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR CONTRIBUTORS BE
// LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
// CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
// SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
// INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
// CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
// ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
// POSSIBILITY OF SUCH DAMAGE.
use super::{Raster, composite, rounded_contains};
use lumen_html::paint::{BorderPattern, Rect, Rgba};
pub(super) fn draw(
    raster: &mut Raster<'_>,
    rect: Rect,
    radius: f32,
    width: f32,
    color: Rgba,
    pattern: Option<BorderPattern>,
) {
    let Some(visible) = raster
        .clips
        .last()
        .copied()
        .and_then(|clip| clip.intersection(rect))
    else {
        return;
    };
    let x0 = (visible.x * raster.scale).floor().max(0.0) as u32;
    let y0 = (visible.y * raster.scale).floor().max(0.0) as u32;
    let x1 = ((visible.x + visible.width) * raster.scale)
        .ceil()
        .min(raster.image.width as f32) as u32;
    let y1 = ((visible.y + visible.height) * raster.scale)
        .ceil()
        .min(raster.image.height as f32) as u32;
    let left = rect.x * raster.scale;
    let top = rect.y * raster.scale;
    let right = (rect.x + rect.width) * raster.scale;
    let bottom = (rect.y + rect.height) * raster.scale;
    let border = (width * raster.scale)
        .min((right - left) * 0.5)
        .min((bottom - top) * 0.5);
    let outer_radius = (radius * raster.scale)
        .min((right - left) * 0.5)
        .min((bottom - top) * 0.5);
    let inner_radius = (outer_radius - border).max(0.0);
    let inner_left = left + border;
    let inner_top = top + border;
    let inner_right = right - border;
    let inner_bottom = bottom - border;
    let arc_measure = pattern
        .filter(|_| outer_radius > border * 0.5)
        .map(|_| ArcMeasure::new(outer_radius - border * 0.5));
    for y in y0..y1 {
        for x in x0..x1 {
            let fx = x as f32;
            let fy = y as f32;
            if fx >= inner_left + inner_radius
                && fx + 1.0 <= inner_right - inner_radius
                && fy >= inner_top
                && fy + 1.0 <= inner_bottom
                || fy >= inner_top + inner_radius
                    && fy + 1.0 <= inner_bottom - inner_radius
                    && fx >= inner_left
                    && fx + 1.0 <= inner_right
            {
                continue;
            }
            let mut coverage = if raster.antialias {
                (super::coverage::rounded(fx, fy, left, top, right, bottom, outer_radius)
                    - super::coverage::rounded(
                        fx,
                        fy,
                        inner_left,
                        inner_top,
                        inner_right,
                        inner_bottom,
                        inner_radius,
                    ))
                .clamp(0.0, 1.0)
            } else {
                (rounded_contains(fx + 0.5, fy + 0.5, left, top, right, bottom, outer_radius)
                    && !rounded_contains(
                        fx + 0.5,
                        fy + 0.5,
                        inner_left,
                        inner_top,
                        inner_right,
                        inner_bottom,
                        inner_radius,
                    )) as u8 as f32
            };
            if coverage == 0.0 {
                continue;
            }
            if let Some(pattern) = pattern {
                let samples = if raster.antialias { 4 } else { 1 };
                let mut hits = 0;
                for sy in 0..samples {
                    for sx in 0..samples {
                        let px = fx + (sx as f32 + 0.5) / samples as f32;
                        let py = fy + (sy as f32 + 0.5) / samples as f32;
                        hits += patterned(
                            px,
                            py,
                            left + border * 0.5,
                            top + border * 0.5,
                            right - border * 0.5,
                            bottom - border * 0.5,
                            (outer_radius - border * 0.5).max(0.0),
                            border,
                            pattern,
                            arc_measure.as_ref(),
                        ) as u32;
                    }
                }
                coverage *= hits as f32 / (samples * samples) as f32;
            }
            if coverage == 0.0 {
                continue;
            }
            let offset = ((y as usize * raster.image.width as usize) + x as usize) * 4;
            if coverage == 1.0 && color.a == 255 {
                raster.image.pixels[offset..offset + 4]
                    .copy_from_slice(&[color.r, color.g, color.b, 255]);
            } else {
                composite(
                    &mut raster.image.pixels[offset..offset + 4],
                    color,
                    coverage,
                );
            }
        }
    }
}

fn patterned(
    x: f32,
    y: f32,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    radius: f32,
    width: f32,
    pattern: BorderPattern,
    arc_measure: Option<&ArcMeasure>,
) -> bool {
    if width <= 0.0 {
        return false;
    }
    if radius <= 0.0 {
        let horizontal =
            (y - top).abs().min((y - bottom).abs()) <= (x - left).abs().min((x - right).abs());
        let (distance, length, radial) = if horizontal {
            (
                x - left + width * 0.5,
                right - left + width,
                (y - top).abs().min((y - bottom).abs()),
            )
        } else {
            (
                y - top + width * 0.5,
                bottom - top + width,
                (x - left).abs().min((x - right).abs()),
            )
        };
        if pattern == BorderPattern::Dotted && width <= 3.0 {
            return thin_dots(distance, length.round() as i32, width.round() as i32);
        }
        let (dash, gap) = dash_intervals(length, width, pattern, false);
        if gap <= 0.0 {
            return true;
        }
        return if dash == 0.0 {
            let along = (distance - width * 0.5 + gap * 0.5).rem_euclid(gap) - gap * 0.5;
            along * along + radial * radial <= width * width * 0.25
        } else {
            distance.rem_euclid(dash + gap) < dash
        };
    }
    let horizontal = (right - left - radius * 2.0).max(0.0);
    let vertical = (bottom - top - radius * 2.0).max(0.0);
    let arc_measure = arc_measure.expect("rounded pattern has a contour measure");
    let arc = arc_measure.length();
    let perimeter = 2.0 * (horizontal + vertical) + 4.0 * arc;
    if perimeter <= 0.0 {
        return false;
    }
    let (distance, radial) = if x < left + radius && y < top + radius {
        corner(
            x,
            y,
            left + radius,
            top + radius,
            radius,
            2.0 * horizontal + 2.0 * vertical + 3.0 * arc,
            std::f32::consts::PI,
            arc_measure,
        )
    } else if x > right - radius && y < top + radius {
        corner(
            x,
            y,
            right - radius,
            top + radius,
            radius,
            horizontal,
            std::f32::consts::FRAC_PI_2,
            arc_measure,
        )
    } else if x > right - radius && y > bottom - radius {
        corner(
            x,
            y,
            right - radius,
            bottom - radius,
            radius,
            horizontal + vertical + arc,
            0.0,
            arc_measure,
        )
    } else if x < left + radius && y > bottom - radius {
        corner(
            x,
            y,
            left + radius,
            bottom - radius,
            radius,
            2.0 * horizontal + vertical + 2.0 * arc,
            -std::f32::consts::FRAC_PI_2,
            arc_measure,
        )
    } else if (y - top).abs().min((y - bottom).abs()) <= (x - left).abs().min((x - right).abs()) {
        if (y - top).abs() <= (y - bottom).abs() {
            (x - left - radius, y - top)
        } else {
            (
                2.0 * horizontal + vertical + 2.0 * arc - (x - left - radius),
                y - bottom,
            )
        }
    } else if (x - right).abs() <= (x - left).abs() {
        (horizontal + arc + y - top - radius, x - right)
    } else {
        (
            2.0 * horizontal + vertical + 3.0 * arc + bottom - radius - y,
            x - left,
        )
    };
    // Blink explicitly starts its clockwise contour after the upper-left corner.
    let distance = distance.rem_euclid(perimeter);
    let (dash, gap) = dash_intervals(perimeter.floor(), width, pattern, true);
    if gap <= 0.0 {
        return true;
    }
    if dash != 0.0 {
        return distance.rem_euclid(dash + gap) < dash;
    }
    let tangent = (distance + gap * 0.5).rem_euclid(gap) - gap * 0.5;
    tangent * tangent + radial * radial <= width * width * 0.25
}

fn best_gap(length: f32, dash: f32, gap: f32, closed: bool) -> f32 {
    let count = ((length + if closed { 0.0 } else { gap }) / (dash + gap))
        .floor()
        .max(1.0);
    let gaps = if closed { count } else { count - 1.0 };
    let low = if gaps > 0.0 {
        (length - count * dash) / gaps
    } else {
        f32::INFINITY
    };
    let high = (length - (count + 1.0) * dash) / (gaps + 1.0);
    if high <= 0.0 || (low - gap).abs() < (high - gap).abs() {
        low
    } else {
        high
    }
}

fn dash_intervals(length: f32, width: f32, pattern: BorderPattern, closed: bool) -> (f32, f32) {
    if pattern == BorderPattern::Dashed || width <= 3.0 {
        let (dash, gap) = if pattern == BorderPattern::Dashed {
            if width >= 3.0 {
                (2.0 * width, width)
            } else {
                (3.0 * width, 2.0 * width)
            }
        } else {
            (width, width)
        };
        if length <= 2.0 * dash {
            return (length, 0.0);
        }
        let minimum = 2.0 * dash + gap * if closed { 2.0 } else { 1.0 };
        if length <= minimum {
            let scale = length / minimum;
            return (dash * scale, gap * scale);
        }
        (
            dash,
            if pattern == BorderPattern::Dashed {
                best_gap(length, dash, gap, closed)
            } else {
                gap
            },
        )
    } else {
        (
            0.0,
            if length < 2.0 * width {
                2.0 * width
            } else {
                best_gap(length, width, width, closed) + width - 0.01
            },
        )
    }
}

fn thin_dots(distance: f32, length: i32, width: i32) -> bool {
    if width <= 0 {
        return false;
    }
    let (mod4, mod6) = (length % 4, length % 6);
    let grow_start = (width == 1 && length % 2 == 0) || (width == 3 && mod6 == 0);
    let shorten_start =
        (width == 2 && (mod4 == 0 || mod4 == 1)) || (width == 3 && (mod6 == 1 || mod6 == 2));
    let widen_start = (width == 2 && mod4 == 3) || (width == 3 && (mod6 == 4 || mod6 == 5));
    let end = (width == 2 && mod4 == 0) || (width == 3 && (mod6 == 0 || mod6 == 1 || mod6 == 5));
    let start = grow_start || shorten_start || widen_start;
    let start_growth = i32::from(grow_start);
    let end_growth = i32::from(width == 3 && mod6 == 0);
    if start && distance < (width + start_growth) as f32
        || end && distance >= (length - width - end_growth) as f32
    {
        return true;
    }
    let origin = if start {
        2 * width + if shorten_start { -1 } else { 1 }
    } else {
        0
    };
    let limit = if end {
        length - width - end_growth - 1
    } else {
        length
    };
    distance >= origin as f32
        && distance < (limit as f32)
        && (distance - origin as f32).rem_euclid((width * 2) as f32) < width as f32
}

fn corner(
    x: f32,
    y: f32,
    cx: f32,
    cy: f32,
    radius: f32,
    start: f32,
    angle_offset: f32,
    measure: &ArcMeasure,
) -> (f32, f32) {
    let (dx, dy) = (x - cx, y - cy);
    let angle = (dy.atan2(dx) + angle_offset).clamp(0.0, std::f32::consts::FRAC_PI_2);
    (start + measure.at_angle(angle), dx.hypot(dy) - radius)
}

// SkContourMeasure.cpp measures conics by recursively bisecting parameter space
// until midpoint deviation is at most half a pixel (maximum recursion depth 8).
// Retain only this one quarter's bounded table for the duration of the draw.
struct ArcMeasure(Vec<(f32, f32)>);
impl ArcMeasure {
    fn new(radius: f32) -> Self {
        fn point(t: f32, r: f32) -> (f32, f32) {
            let s = 1.0 - t;
            let cross = std::f32::consts::SQRT_2 * s * t;
            let denominator = s * s + cross + t * t;
            (
                r * (s * s + cross) / denominator,
                r * (cross + t * t) / denominator,
            )
        }
        fn append(
            out: &mut Vec<(f32, f32)>,
            r: f32,
            t0: f32,
            p0: (f32, f32),
            t1: f32,
            p1: (f32, f32),
            depth: u8,
        ) {
            let tm = (t0 + t1) * 0.5;
            let mid = point(tm, r);
            if depth < 8
                && (mid.0 - (p0.0 + p1.0) * 0.5)
                    .abs()
                    .max((mid.1 - (p0.1 + p1.1) * 0.5).abs())
                    > 0.5
            {
                append(out, r, t0, p0, tm, mid, depth + 1);
                append(out, r, tm, mid, t1, p1, depth + 1);
            } else {
                let distance = out.last().map_or(0.0, |p| p.1) + (p1.0 - p0.0).hypot(p1.1 - p0.1);
                out.push((t1, distance));
            }
        }
        let mut points = Vec::with_capacity(4);
        append(
            &mut points,
            radius,
            0.0,
            (radius, 0.0),
            1.0,
            (0.0, radius),
            0,
        );
        Self(points)
    }
    fn length(&self) -> f32 {
        self.0.last().map_or(0.0, |p| p.1)
    }
    fn at_angle(&self, angle: f32) -> f32 {
        let u = (angle * 0.5).tan();
        let t = u / (std::f32::consts::FRAC_1_SQRT_2 + u * (1.0 - std::f32::consts::FRAC_1_SQRT_2));
        let index = self.0.partition_point(|p| p.0 < t).min(self.0.len() - 1);
        let (t0, d0) = if index == 0 {
            (0.0, 0.0)
        } else {
            self.0[index - 1]
        };
        let (t1, d1) = self.0[index];
        d0 + (d1 - d0) * (t - t0) / (t1 - t0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_html::paint::{Command, DisplayList};

    #[test]
    fn conic_measure_is_bounded_and_interpolates_tangents() {
        let measure = ArcMeasure::new(7.0);
        assert_eq!(measure.0.len(), 4);
        assert!(measure.length() < 7.0 * std::f32::consts::FRAC_PI_2);
        assert_eq!(measure.at_angle(0.0), 0.0);
        assert!((measure.at_angle(std::f32::consts::FRAC_PI_2) - measure.length()).abs() < 1e-5);
        assert!(
            (measure.at_angle(std::f32::consts::FRAC_PI_4) - measure.length() * 0.5).abs() < 1e-5
        );
        assert!(ArcMeasure::new(100_000.0).0.len() <= 256);
    }

    #[test]
    fn square_patterns_match_chrome_endpoint_spacing() {
        let dotted: Vec<_> = (0..24)
            .filter(|x| thin_dots(*x as f32 + 0.5, 24, 2))
            .collect();
        assert_eq!(dotted, [0, 1, 3, 4, 7, 8, 11, 12, 15, 16, 19, 20, 22, 23]);
        let (dash, gap) = dash_intervals(24.0, 2.0, BorderPattern::Dashed, false);
        assert_eq!((dash, gap), (6.0, 3.0));
        let dashed: Vec<_> = (0..24)
            .filter(|x| (*x as f32 + 0.5).rem_euclid(dash + gap) < dash)
            .collect();
        assert_eq!(
            dashed,
            [
                0, 1, 2, 3, 4, 5, 9, 10, 11, 12, 13, 14, 18, 19, 20, 21, 22, 23
            ]
        );
    }

    #[test]
    fn rounded_patterns_have_gaps_and_remain_within_the_border_mask() {
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 32.0,
            height: 24.0,
        };
        let color = Rgba {
            r: 0,
            g: 0,
            b: 0,
            a: 255,
        };
        let solid = crate::render(
            &DisplayList(vec![Command::StrokeBorder {
                rect,
                radius: 8.0,
                width: 4.0,
                color,
            }]),
            32,
            24,
            1.0,
            true,
        )
        .unwrap();
        for pattern in [BorderPattern::Dashed, BorderPattern::Dotted] {
            let image = crate::render(
                &DisplayList(vec![Command::StrokePatternBorder {
                    rect,
                    radius: 8.0,
                    width: 4.0,
                    color,
                    pattern,
                }]),
                32,
                24,
                1.0,
                true,
            )
            .unwrap();
            let mut painted = false;
            let mut gap = false;
            for (pixel, base) in image
                .pixels
                .chunks_exact(4)
                .zip(solid.pixels.chunks_exact(4))
            {
                assert!(pixel[3] <= base[3]);
                painted |= pixel[3] > 0;
                gap |= base[3] > 0 && pixel[3] == 0;
            }
            assert!(painted && gap);
            assert_eq!(image.pixels[(12 * 32 + 16) * 4 + 3], 0);
        }
    }
}
