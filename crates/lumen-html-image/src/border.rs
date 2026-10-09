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
use super::{composite, composite_disjoint_samples, rounded_contains, Raster};
use lumen_html::paint::{BorderPattern, Rect, Rgba};

fn inset_radii(radii: [[f32; 2]; 4], widths: [f32; 4]) -> [[f32; 2]; 4] {
    core::array::from_fn(|corner| {
        let horizontal = if corner == 0 || corner == 3 { 3 } else { 1 };
        let vertical = if corner < 2 { 0 } else { 2 };
        [
            (radii[corner][0] - widths[horizontal]).max(0.0),
            (radii[corner][1] - widths[vertical]).max(0.0),
        ]
    })
}

fn corners_contains(x: f32, y: f32, bounds: [f32; 4], radii: [[f32; 2]; 4]) -> bool {
    let [left, top, right, bottom] = bounds;
    if left >= right || top >= bottom || x < left || x >= right || y < top || y >= bottom {
        return false;
    }
    for (corner, [rx, ry]) in radii.into_iter().enumerate() {
        let far_x = corner == 1 || corner == 2;
        let far_y = corner >= 2;
        let cx = if far_x { right - rx } else { left + rx };
        let cy = if far_y { bottom - ry } else { top + ry };
        if (if far_x { x > cx } else { x < cx })
            && (if far_y { y > cy } else { y < cy })
            && rx > 0.0
            && ry > 0.0
        {
            return ((x - cx) / rx).powi(2) + ((y - cy) / ry).powi(2) < 1.0;
        }
    }
    true
}

const UNIFORM_MARGIN: f32 = 1e-3;

// Range of `min(|v - a|, |v - b|)` over the unit interval starting at `lo`.
fn axis_range(lo: f32, a: f32, b: f32) -> (f32, f32) {
    let hi = lo + 1.0;
    let at = |v: f32| (v - a).abs().min((v - b).abs());
    let (d0, d1) = (at(lo), at(hi));
    let mut max = d0.max(d1);
    let mid = (a + b) * 0.5;
    if mid > lo && mid < hi {
        max = max.max(at(mid));
    }
    let min = if (a >= lo && a <= hi) || (b >= lo && b <= hi) {
        0.0
    } else {
        d0.min(d1)
    };
    (min, max)
}

// Square-cornered borders: when every sample of a pixel resolves to the same
// side, band and dash state, return that outcome without sampling.
// `Some(Some(slot))` is a uniformly painted pixel, `Some(None)` a uniformly
// unpainted one and `None` means the pixel needs per-sample evaluation.
fn uniform_border_pixel(
    fx: f32,
    fy: f32,
    outer: [f32; 4],
    inner: [f32; 4],
    middle: [f32; 4],
    widths: [f32; 4],
    patterns: [Option<BorderPattern>; 4],
) -> Option<Option<usize>> {
    let [left, top, right, bottom] = outer;
    if fx < left || fy < top || fx + 1.0 > right || fy + 1.0 > bottom {
        return None;
    }
    if !(fy + 1.0 <= inner[1] || fx + 1.0 <= inner[0] || fy >= inner[3] || fx >= inner[2]) {
        return None;
    }
    let mut side = None;
    for (cx, cy) in [
        (fx, fy),
        (fx + 1.0, fy),
        (fx, fy + 1.0),
        (fx + 1.0, fy + 1.0),
    ] {
        let distances = [cy - top, right - cx, bottom - cy, cx - left];
        let mut best: Option<(usize, f32)> = None;
        let mut second = f32::INFINITY;
        for candidate in 0..4 {
            if widths[candidate] <= 0.0 {
                continue;
            }
            let ratio = distances[candidate] / widths[candidate];
            match best {
                Some((_, current)) if ratio >= current => second = second.min(ratio),
                _ => {
                    if let Some((_, current)) = best {
                        second = second.min(current);
                    }
                    best = Some((candidate, ratio));
                }
            }
        }
        let (winner, ratio) = best?;
        if !(second - ratio > UNIFORM_MARGIN) || *side.get_or_insert(winner) != winner {
            return None;
        }
    }
    let side = side?;
    let width = widths[side];
    match patterns[side] {
        Some(BorderPattern::Double | BorderPattern::Dotted) => return None,
        Some(BorderPattern::Dashed) => {
            let half = width * 0.5;
            let (l, t, r, b) = (left + half, top + half, right - half, bottom - half);
            let (y_min, y_max) = axis_range(fy, t, b);
            let (x_min, x_max) = axis_range(fx, l, r);
            let horizontal = if y_max + UNIFORM_MARGIN < x_min {
                true
            } else if x_max + UNIFORM_MARGIN < y_min {
                false
            } else {
                return None;
            };
            let (length, start, coordinate) = if horizontal {
                (r - l + width, l, fx)
            } else {
                (b - t + width, t, fy)
            };
            let (dash, gap) = dash_intervals(length, width, BorderPattern::Dashed, false);
            if gap > 0.0 {
                let period = dash + gap;
                if !period.is_finite() || period <= 0.0 {
                    return None;
                }
                let phase = (coordinate - start + half).rem_euclid(period);
                if phase + 1.0 + UNIFORM_MARGIN <= dash {
                } else if phase >= dash + UNIFORM_MARGIN
                    && phase + 1.0 + UNIFORM_MARGIN <= period
                {
                    return Some(None);
                } else {
                    return None;
                }
            }
        }
        _ => {}
    }
    let inner_band = if matches!(
        patterns[side],
        Some(BorderPattern::Groove | BorderPattern::Ridge)
    ) {
        if fx >= middle[0] && fx + 1.0 <= middle[2] && fy >= middle[1] && fy + 1.0 <= middle[3] {
            true
        } else if fx + 1.0 <= middle[0]
            || fx >= middle[2]
            || fy + 1.0 <= middle[1]
            || fy >= middle[3]
        {
            false
        } else {
            return None;
        }
    } else {
        false
    };
    Some(Some(side + if inner_band { 4 } else { 0 }))
}

fn border_tone(color: Rgba, pattern: Option<BorderPattern>, side: usize, outer: bool) -> Rgba {
    let top_left = side == 0 || side == 3;
    let dark = match pattern {
        Some(BorderPattern::Inset) => top_left,
        Some(BorderPattern::Outset) => !top_left,
        Some(BorderPattern::Groove) => outer == top_left,
        Some(BorderPattern::Ridge) => outer != top_left,
        _ => return color,
    };
    // Jixr DisplayList's deterministic CSS 2.1 UA tones: 45% toward black/white.
    let tone = |channel: u8| {
        (if dark {
            channel as f32 * 0.55
        } else {
            channel as f32 + (255.0 - channel as f32) * 0.45
        })
        .round() as u8
    };
    Rgba {
        r: tone(color.r),
        g: tone(color.g),
        b: tone(color.b),
        a: color.a,
    }
}

// CSS Backgrounds and Borders 3 section 4.3: inner curves subtract the
// adjacent border widths independently; different edge colors meet along
// the line joining each outer corner to the corresponding inner corner.
pub(super) fn draw_box(raster: &mut Raster<'_>, border: &lumen_html::paint::BoxBorder) {
    let patterns = border.side_patterns.unwrap_or([border.pattern; 4]);
    if patterns == [Some(BorderPattern::Double); 4] {
        let widths = border.widths;
        let bands = widths.map(|width| {
            if width < 3.0 {
                width
            } else {
                (width / 3.0).round()
            }
        });
        let mut outer = border.clone();
        outer.pattern = None;
        outer.side_patterns = None;
        outer.widths = bands;
        draw_box(raster, &outer);
        let insets = core::array::from_fn(|side| widths[side] - bands[side]);
        let rect = Rect {
            x: border.rect.x + insets[3],
            y: border.rect.y + insets[0],
            width: (border.rect.width - insets[3] - insets[1]).max(0.0),
            height: (border.rect.height - insets[0] - insets[2]).max(0.0),
        };
        let radius = border
            .radius
            .min(border.rect.width * 0.5)
            .min(border.rect.height * 0.5);
        let corners = inset_radii(border.corners.unwrap_or([[radius; 2]; 4]), insets);
        let inner = lumen_html::paint::BoxBorder {
            rect,
            radius: 0.0,
            widths: core::array::from_fn(|side| if widths[side] < 3.0 { 0.0 } else { bands[side] }),
            colors: border.colors,
            pattern: None,
            side_patterns: None,
            corners: Some(corners),
        };
        draw_box(raster, &inner);
        return;
    }
    let Some(visible) = raster.visible_rect(border.rect)
    else {
        return;
    };
    let scale = raster.scale;
    let rect = border.rect;
    let (left, top, right, bottom) = (
        raster.device_x(rect.x),
        raster.device_y(rect.y),
        raster.device_x(rect.x + rect.width),
        raster.device_y(rect.y + rect.height),
    );
    let radius = (border.radius * scale)
        .min((right - left) * 0.5)
        .min((bottom - top) * 0.5);
    let widths = border.widths.map(|width| width * scale);
    let inner = [
        left + widths[3],
        top + widths[0],
        right - widths[1],
        bottom - widths[2],
    ];
    let outer_radii = border
        .corners
        .map(|corners| corners.map(|r| r.map(|v| v * scale)))
        .unwrap_or([[radius; 2]; 4]);
    let inner_radii = inset_radii(outer_radii, widths);
    let inner_contains = |px, py| corners_contains(px, py, inner, inner_radii);
    let has_double = patterns.contains(&Some(BorderPattern::Double));
    let bands = widths.map(|width| {
        if width < 3.0 {
            width
        } else {
            (width / 3.0).round()
        }
    });
    let band_widths = core::array::from_fn(|side| {
        if patterns[side] == Some(BorderPattern::Double) {
            bands[side]
        } else {
            widths[side]
        }
    });
    let gap_inner = [
        left + band_widths[3],
        top + band_widths[0],
        right - band_widths[1],
        bottom - band_widths[2],
    ];
    let gap_inner_radii = inset_radii(outer_radii, band_widths);
    let insets = core::array::from_fn(|side| {
        if patterns[side] == Some(BorderPattern::Double) {
            widths[side] - bands[side]
        } else {
            0.0
        }
    });
    let gap_outer = [
        left + insets[3],
        top + insets[0],
        right - insets[1],
        bottom - insets[2],
    ];
    let gap_outer_radii = inset_radii(outer_radii, insets);
    let half_widths = widths.map(|width| width * 0.5);
    let middle = [
        left + half_widths[3],
        top + half_widths[0],
        right - half_widths[1],
        bottom - half_widths[2],
    ];
    let middle_radii = inset_radii(outer_radii, half_widths);
    let colors: [Rgba; 8] = core::array::from_fn(|band| {
        border_tone(
            border.colors[band % 4],
            patterns[band % 4],
            band % 4,
            band < 4,
        )
    });
    let has_stroke = patterns
        .iter()
        .any(|pattern| matches!(pattern, Some(BorderPattern::Dashed | BorderPattern::Dotted)));
    // Each side can have a different stroke width and therefore a different
    // center-line radius. Measure once per side, outside the pixel loop.
    let arc_measures: [Option<ArcMeasure>; 4] = core::array::from_fn(|side| {
        (matches!(patterns[side], Some(BorderPattern::Dashed | BorderPattern::Dotted))
            && radius > widths[side] * 0.5)
            .then(|| ArcMeasure::new(radius - widths[side] * 0.5))
    });

    let square_corners = outer_radii
        .iter()
        .all(|corner| corner[0] == 0.0 && corner[1] == 0.0);
    let (x0, y0, x1, y1) = raster.pixel_span(visible);
    let samples = if raster.antialias { 8 } else { 1 };
    for y in y0..y1 {
        for x in x0..x1 {
            if inner_contains(x as f32, y as f32)
                && inner_contains(x as f32 + 1.0, y as f32)
                && inner_contains(x as f32, y as f32 + 1.0)
                && inner_contains(x as f32 + 1.0, y as f32 + 1.0)
            {
                continue;
            }
            let coverage = if raster.antialias {
                (super::coverage::rounded_corners(
                    x as f32,
                    y as f32,
                    left,
                    top,
                    right,
                    bottom,
                    outer_radii,
                ) - super::coverage::rounded_corners(
                    x as f32,
                    y as f32,
                    inner[0],
                    inner[1],
                    inner[2],
                    inner[3],
                    inner_radii,
                ))
                .clamp(0.0, 1.0)
            } else {
                1.0
            };
            if coverage == 0.0 {
                continue;
            }
            let mut hits = [0u32; 8];
            let uniform = if square_corners {
                uniform_border_pixel(
                    x as f32,
                    y as f32,
                    [left, top, right, bottom],
                    inner,
                    middle,
                    widths,
                    patterns,
                )
            } else {
                None
            };
            if let Some(slot) = uniform {
                if let Some(slot) = slot {
                    hits[slot] = samples * samples;
                }
            } else {
            for sy in 0..samples {
                for sx in 0..samples {
                    let px = x as f32 + (sx as f32 + 0.5) / samples as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / samples as f32;
                    if !corners_contains(px, py, [left, top, right, bottom], outer_radii)
                        || inner_contains(px, py)
                    {
                        continue;
                    }
                    let distances = [py - top, right - px, bottom - py, px - left];
                    let side = (0..4).filter(|&side| widths[side] > 0.0).min_by(|&a, &b| {
                        (distances[a] / widths[a]).total_cmp(&(distances[b] / widths[b]))
                    });
                    let Some(side) = side else { continue };
                    if patterns[side] == Some(BorderPattern::Double)
                        && widths[side] >= 3.0
                        && corners_contains(px, py, gap_inner, gap_inner_radii)
                        && !corners_contains(px, py, gap_outer, gap_outer_radii)
                    {
                        continue;
                    }
                    if let Some(pattern @ (BorderPattern::Dashed | BorderPattern::Dotted)) =
                        patterns[side]
                    {
                        let width = widths[side];
                        if !patterned(
                            px,
                            py,
                            left + width * 0.5,
                            top + width * 0.5,
                            right - width * 0.5,
                            bottom - width * 0.5,
                            (radius - width * 0.5).max(0.0),
                            width,
                            pattern,
                            arc_measures[side].as_ref(),
                        ) {
                            continue;
                        }
                    }
                    let inner_band = matches!(
                        patterns[side],
                        Some(BorderPattern::Groove | BorderPattern::Ridge)
                    ) && corners_contains(px, py, middle, middle_radii);
                    hits[side + if inner_band { 4 } else { 0 }] += 1;
                }
            }
            }
            let total = hits.iter().sum::<u32>();
            let coverage = if has_double || has_stroke {
                total as f32 / (samples * samples) as f32
            } else {
                coverage
            };
            if total == 0 {
                continue;
            }
            let offset = (y as usize * raster.image.width as usize + x as usize) * 4;
            if colors.iter().all(|color| *color == colors[0]) {
                composite(
                    &mut raster.image.pixels[offset..offset + 4],
                    colors[0],
                    coverage,
                );
            } else {
                composite_disjoint_samples(&mut raster.image.pixels[offset..offset + 4],
                    &colors, &hits, total, coverage);
            }
        }
    }
}
#[cfg(test)]
mod box_tests {
    use lumen_html::paint::{BoxBorder, Command, DisplayList, Rect, Rgba};
    use std::boxed::Box;

    #[test]
    fn specification_disjoint_border_samples_mix_premultiplied_color_before_source_over() {
        let mut colors = [Rgba { r: 0, g: 0, b: 0, a: 0 }; 8];
        colors[0] = Rgba { r: 255, g: 0, b: 0, a: 255 };
        colors[1] = Rgba { r: 0, g: 0, b: 255, a: 128 };
        let hits = [32, 32, 0, 0, 0, 0, 0, 0];
        let mut pixel = [0; 4];
        super::composite_disjoint_samples(&mut pixel, &colors, &hits, 64, 1.0);
        assert_eq!(pixel, [170, 0, 85, 192], "two adjacent halves combine their authored opacity");
        colors[1].a = 0;
        pixel = [0; 4];
        super::composite_disjoint_samples(&mut pixel, &colors, &hits, 64, 1.0);
        assert_eq!(pixel, [255, 0, 0, 128], "transparent samples contribute no color");
    }

    #[test]
    fn specification_adjoining_border_colors_keep_complete_coverage_and_authored_alpha() {
        use lumen_html::paint::BorderPattern;
        let rect = Rect { x: 0.0, y: 0.0, width: 8.0, height: 8.0 };
        for pattern in [None, Some(BorderPattern::Inset), Some(BorderPattern::Outset),
            Some(BorderPattern::Groove), Some(BorderPattern::Ridge)] {
            for alpha in [128, 255] {
                let colors = [Rgba { r: 255, g: 0, b: 0, a: alpha },
                    Rgba { r: 0, g: 0, b: 255, a: alpha },
                    Rgba { r: 0, g: 128, b: 0, a: alpha },
                    Rgba { r: 128, g: 0, b: 128, a: alpha }];
                let border = Command::StrokeBoxBorder(Box::new(BoxBorder { rect, radius: 0.0,
                    widths: [2.0; 4], colors, pattern, side_patterns: None, corners: None }));
                let image = crate::render(&DisplayList(vec![border.clone()]), 8, 8, 1.0, true).unwrap();
                for (x, y) in [(0, 0), (7, 0), (0, 7), (7, 7), (1, 1), (6, 6)] {
                    assert_eq!(image.pixels[(y * 8 + x) * 4 + 3], alpha,
                        "disjoint corner samples cover one border surface: {pattern:?}");
                }
                if alpha == 255 {
                    let mut painted = Vec::new();
                    for background in [Rgba { r: 255, g: 0, b: 0, a: 255 },
                        Rgba { r: 255, g: 255, b: 255, a: 255 }] {
                        painted.push(crate::render(&DisplayList(vec![Command::FillRect { rect, color: background },
                            border.clone()]), 8, 8, 1.0, true).unwrap());
                    }
                    for (x, y) in [(0, 0), (7, 0), (0, 7), (7, 7), (1, 1), (6, 6)] {
                        let offset = (y * 8 + x) * 4;
                        assert_eq!(&painted[0].pixels[offset..offset + 4], &painted[1].pixels[offset..offset + 4],
                            "opaque corner colors cannot reveal their different backgrounds: {pattern:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn double_borders_leave_the_background_visible_between_bands() {
        let red = Rgba {
            r: 255,
            g: 0,
            b: 0,
            a: 128,
        };
        let list = DisplayList(vec![Command::StrokeBoxBorder(Box::new(BoxBorder {
            rect: Rect {
                x: 0.0,
                y: 0.0,
                width: 40.0,
                height: 30.0,
            },
            radius: 0.0,
            widths: [6.0, 9.0, 6.0, 3.0],
            colors: [red; 4],
            pattern: Some(lumen_html::paint::BorderPattern::Double),
            side_patterns: None,
            corners: None,
        }))]);
        let image = crate::render(&list, 40, 30, 1.0, true).unwrap();
        let alpha = |x: usize, y: usize| image.pixels[(y * 40 + x) * 4 + 3];
        assert_eq!(alpha(20, 0), 128);
        assert_eq!(alpha(20, 2), 0);
        assert_eq!(alpha(20, 4), 128);
        assert_eq!(alpha(35, 15), 0);
        assert_eq!(alpha(32, 15), 128);
        assert_eq!(alpha(20, 15), 0);
    }

    #[test]
    fn mixed_solid_double_translucent_edges_composite_once() {
        let red = Rgba {
            r: 255,
            g: 0,
            b: 0,
            a: 128,
        };
        let double = Some(lumen_html::paint::BorderPattern::Double);
        let list = DisplayList(vec![Command::StrokeBoxBorder(Box::new(BoxBorder {
            rect: Rect {
                x: 0.0,
                y: 0.0,
                width: 40.0,
                height: 30.0,
            },
            radius: 0.0,
            widths: [6.0; 4],
            colors: [red; 4],
            pattern: double,
            side_patterns: Some([None, double, double, double]),
            corners: None,
        }))]);
        let image = crate::render(&list, 40, 30, 1.0, true).unwrap();
        let alpha = |x: usize, y: usize| image.pixels[(y * 40 + x) * 4 + 3];
        assert_eq!(alpha(20, 2), 128);
        assert_eq!(alpha(2, 2), 128);
        assert_eq!(alpha(2, 15), 0);
        assert_eq!(alpha(4, 15), 128);
    }

    #[test]
    fn three_dimensional_border_tones_preserve_alpha_and_reverse_faces() {
        use lumen_html::paint::BorderPattern;
        let color = Rgba {
            r: 200,
            g: 100,
            b: 50,
            a: 128,
        };
        let dark = super::border_tone(color, Some(BorderPattern::Inset), 0, true);
        assert_eq!(
            dark,
            Rgba {
                r: 110,
                g: 55,
                b: 28,
                a: 128
            }
        );
        assert_eq!(
            dark,
            super::border_tone(color, Some(BorderPattern::Outset), 2, true)
        );
        assert_eq!(
            dark,
            super::border_tone(color, Some(BorderPattern::Groove), 0, true)
        );
        assert_eq!(
            dark,
            super::border_tone(color, Some(BorderPattern::Ridge), 0, false)
        );
        assert_ne!(
            dark,
            super::border_tone(color, Some(BorderPattern::Groove), 0, false)
        );
    }

    #[test]
    fn asymmetric_round_border_preserves_inner_hole_and_edge_colors() {
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
        let list = DisplayList(vec![Command::StrokeBoxBorder(Box::new(BoxBorder {
            rect: Rect {
                x: 0.0,
                y: 0.0,
                width: 40.0,
                height: 30.0,
            },
            radius: 12.0,
            widths: [4.0, 8.0, 6.0, 2.0],
            colors: [red, blue, red, blue],
            pattern: None,
            side_patterns: None,
            corners: None,
        }))]);
        let image = crate::render(&list, 40, 30, 1.0, true).unwrap();
        let pixel = |x: usize, y: usize| &image.pixels[(y * 40 + x) * 4..(y * 40 + x) * 4 + 4];
        assert_eq!(pixel(20, 1), &[255, 0, 0, 255]);
        assert_eq!(pixel(37, 15), &[0, 0, 255, 255]);
        assert_eq!(pixel(20, 15), &[0, 0, 0, 0]);
        assert_eq!(pixel(0, 0), &[0, 0, 0, 0]);
        // The top-right inner curve is elliptical (rx 4, ry 8), not a
        // circular curve obtained by subtracting a single uniform width.
        assert_eq!(pixel(31, 11)[3], 0);
        assert_eq!(pixel(31, 5)[3], 255);
    }
}

pub(super) fn draw(
    raster: &mut Raster<'_>,
    rect: Rect,
    radius: f32,
    width: f32,
    color: Rgba,
    pattern: Option<BorderPattern>,
) {
    if matches!(
        pattern,
        Some(
            BorderPattern::Double
                | BorderPattern::Groove
                | BorderPattern::Ridge
                | BorderPattern::Inset
                | BorderPattern::Outset
        )
    ) {
        draw_box(
            raster,
            &lumen_html::paint::BoxBorder {
                rect,
                radius,
                widths: [width; 4],
                colors: [color; 4],
                pattern,
                side_patterns: None,
                corners: None,
            },
        );
        return;
    }
    let Some(visible) = raster.visible_rect(rect)
    else {
        return;
    };
    let (x0, y0, x1, y1) = raster.pixel_span(visible);
    let left = raster.device_x(rect.x);
    let top = raster.device_y(rect.y);
    let right = raster.device_x(rect.x + rect.width);
    let bottom = raster.device_y(rect.y + rect.height);
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
        let fy = y as f32;
        let inside = if fy >= inner_top && fy + 1.0 <= inner_bottom {
            if fy >= inner_top + inner_radius && fy + 1.0 <= inner_bottom - inner_radius {
                Some((inner_left, inner_right))
            } else {
                Some((inner_left + inner_radius, inner_right - inner_radius))
            }
        } else {
            None
        };
        let mut spans = (x0..x1, 0..0);
        if let Some((from, to)) = inside {
            let start = (from.ceil().max(x0 as f32) as u32).min(x1);
            let end = (to.floor().min(x1 as f32) as u32).max(start);
            if start < end {
                spans = (x0..start, end..x1);
            }
        }
        for x in spans.0.chain(spans.1) {
            let fx = x as f32;
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
    fn rounded_box_patterns_measure_each_side_contour() {
        let rect = Rect { x: 0.0, y: 0.0, width: 32.0, height: 24.0 };
        let color = Rgba { r: 0, g: 0, b: 0, a: 255 };
        for pattern in [BorderPattern::Dashed, BorderPattern::Dotted] {
            let image = crate::render(
                &DisplayList(vec![Command::StrokeBoxBorder(Box::new(lumen_html::paint::BoxBorder {
                    rect,
                    radius: 8.0,
                    widths: [2.0, 4.0, 6.0, 3.0],
                    colors: [color; 4],
                    pattern: Some(pattern),
                    side_patterns: None,
                    corners: None,
                }))]),
                32, 24, 1.0, true,
            ).unwrap();
            assert!(image.pixels.chunks_exact(4).any(|pixel| pixel[3] > 0));
            assert_eq!(image.pixels[(12 * 32 + 16) * 4 + 3], 0);
            assert_eq!(image.pixels[3], 0);
        }
    }

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
            [0, 1, 2, 3, 4, 5, 9, 10, 11, 12, 13, 14, 18, 19, 20, 21, 22, 23]
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
