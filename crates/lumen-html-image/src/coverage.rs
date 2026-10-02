// Conic subdivision and quadratic accuracy adapted from Skia's SkGeometry.cpp
// and SkAnalyticEdge.cpp. Copyright 2006 The Android Open Source Project.
// BSD redistribution conditions and disclaimer are reproduced in border.rs.
/// Intersection area of a unit pixel and the rounded rectangle's raster contour.
/// Straight edges and interior pixels need no integration or temporary storage.
pub(super) fn rounded(
    x: f32,
    y: f32,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    radius: f32,
) -> f32 {
    let (x0, y0, x1, y1) = (
        x.max(left),
        y.max(top),
        (x + 1.0).min(right),
        (y + 1.0).min(bottom),
    );
    if x1 <= x0 || y1 <= y0 {
        return 0.0;
    }
    let mut area = f64::from(x1 - x0) * f64::from(y1 - y0);
    let radius = radius
        .max(0.0)
        .min((right - left) * 0.5)
        .min((bottom - top) * 0.5);
    if radius == 0.0
        || x0 >= left + radius && x1 <= right - radius
        || y0 >= top + radius && y1 <= bottom - radius
    {
        return area as f32;
    }
    for (cx, cy, flip_x, flip_y) in [
        (left + radius, top + radius, true, true),
        (right - radius, top + radius, false, true),
        (left + radius, bottom - radius, true, false),
        (right - radius, bottom - radius, false, false),
    ] {
        if (if flip_x { x1 <= cx } else { x0 >= cx }) && (if flip_y { y1 <= cy } else { y0 >= cy })
        {
            let dx = f64::from(cx) - f64::from(cx.clamp(x0, x1));
            let dy = f64::from(cy) - f64::from(cy.clamp(y0, y1));
            if dx * dx + dy * dy >= f64::from(radius).powi(2) {
                return 0.0;
            }
        }
        let (lo_x, hi_x) = if flip_x {
            ((cx - x1).max(0.0), (cx - x0).min(radius))
        } else {
            ((x0 - cx).max(0.0), (x1 - cx).min(radius))
        };
        let (lo_y, hi_y) = if flip_y {
            ((cy - y1).max(0.0), (cy - y0).min(radius))
        } else {
            ((y0 - cy).max(0.0), (y1 - cy).min(radius))
        };
        if hi_x <= lo_x || hi_y <= lo_y {
            continue;
        }
        let (lo_x, hi_x, lo_y, hi_y, r) = (
            f64::from(lo_x),
            f64::from(hi_x),
            f64::from(lo_y),
            f64::from(hi_y),
            f64::from(radius),
        );
        let square = (hi_x - lo_x) * (hi_y - lo_y);
        let inside = if lo_x * lo_x + lo_y * lo_y >= r * r {
            0.0
        } else {
            contour_area(
                [[r, 0.0], [r, r], [0.0, r]],
                std::f64::consts::FRAC_1_SQRT_2,
                [lo_x, lo_y, hi_x, hi_y],
                0,
                f64::from(cy),
                flip_y,
            )
        };
        area -= square - inside;
    }
    area.clamp(0.0, 1.0) as f32
}

fn contour_area(
    points: [[f64; 2]; 3],
    w: f64,
    clip: [f64; 4],
    depth: u8,
    origin_y: f64,
    flip_y: bool,
) -> f64 {
    let [p0, p1, p2] = points;
    if p2[1] <= clip[1] || p0[1] >= clip[3] {
        return 0.0;
    }
    let k = (w - 1.0) / (4.0 * (w + 1.0));
    let error = (k * (p0[0] - 2.0 * p1[0] + p2[0])).hypot(k * (p0[1] - 2.0 * p1[1] + p2[1]));
    if error > 0.25 && depth < 5 {
        let mut a = [0.0; 2];
        let mut b = [0.0; 2];
        let mut mid = [0.0; 2];
        for i in 0..2 {
            a[i] = (p0[i] + w * p1[i]) / (1.0 + w);
            b[i] = (p2[i] + w * p1[i]) / (1.0 + w);
            mid[i] = (a[i] + b[i]) * 0.5;
        }
        let weight = ((1.0 + w) * 0.5).sqrt();
        return contour_area([p0, a, mid], weight, clip, depth + 1, origin_y, flip_y)
            + contour_area([mid, b, p2], weight, clip, depth + 1, origin_y, flip_y);
    }
    let quantized = points.map(|p| p.map(|v| (v * 256.0).trunc()));
    let delta = std::array::from_fn::<_, 2, _>(|i| {
        ((2.0 * quantized[1][i] - quantized[0][i] - quantized[2][i]) / 4.0)
            .floor()
            .abs()
    });
    let distance = delta[0].max(delta[1]) + (delta[0].min(delta[1]) / 2.0).floor();
    let distance = ((distance + 16.0) / 32.0).floor() as u64;
    let shift = ((64 - distance.leading_zeros()) >> 1).clamp(1, 6);
    let steps = 1u32 << shift;
    let mut area = 0.0;
    let [p0, p1, p2] = if flip_y { [p2, p1, p0] } else { [p0, p1, p2] };
    let world_y = |y: f64| origin_y + if flip_y { -y } else { y };
    let local_y = |y: f64| if flip_y { origin_y - y } else { y - origin_y };
    let mut prior = [p0[0], local_y((world_y(p0[1]) * 4.0).round() * 0.25)];
    for step in 1..=steps {
        let t = step as f64 / steps as f64;
        let s = 1.0 - t;
        let mut point: [f64; 2] =
            std::array::from_fn(|i| s * s * p0[i] + 2.0 * s * t * p1[i] + t * t * p2[i]);
        let absolute_y = world_y(point[1]);
        let prior_y = world_y(prior[1]);
        let next_t = (step - 1) as f64 / steps as f64;
        let raw_prior_y = world_y(
            (1.0 - next_t).powi(2) * p0[1]
                + 2.0 * (1.0 - next_t) * next_t * p1[1]
                + next_t * next_t * p2[1],
        );
        let snap_integer = step < steps
            && absolute_y - raw_prior_y >= 2.0
            && (absolute_y - raw_prior_y) * 64.0 > (point[0] - prior[0]).abs();
        let final_y = (world_y(p2[1]) * 4.0).round() * 0.25;
        let snapped_y = if snap_integer {
            absolute_y.round()
        } else {
            (absolute_y * 4.0).round() * 0.25
        }
        .min(final_y);
        if snap_integer && absolute_y != prior_y {
            point[0] -= (point[0] - prior[0]) / (absolute_y - prior_y) * (absolute_y - snapped_y);
        }
        point[1] = local_y(snapped_y);
        let (lower, upper) = if prior[1] <= point[1] {
            (prior, point)
        } else {
            (point, prior)
        };
        let low = lower[1].max(clip[1]);
        let high = upper[1].min(clip[3]);
        if low < high {
            let slope = (lower[0] - upper[0]) / (upper[1] - lower[1]);
            let width = clip[2] - clip[0];
            let z0 = lower[0] - slope * (low - lower[1]) - clip[0];
            let z1 = lower[0] - slope * (high - lower[1]) - clip[0];
            let integral = |z: f64| 0.5 * (z.max(0.0).powi(2) - (z - width).max(0.0).powi(2));
            area += if slope == 0.0 {
                z0.clamp(0.0, width) * (high - low)
            } else {
                (integral(z0) - integral(z1)) / slope
            };
        }
        prior = point;
    }
    area
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raster_contour_and_fractional_edges_have_expected_area() {
        assert!((rounded(0.0, 0.0, 0.0, 0.0, 2.0, 2.0, 1.0) - 0.75).abs() < 1e-6);
        assert_eq!(rounded(1.0, 1.0, 1.25, 1.5, 5.0, 5.0, 0.0), 0.375);
        assert_eq!(rounded(10.0, 10.0, 0.0, 0.0, 2.0, 2.0, 1.0), 0.0);
        assert_eq!(rounded(2.0, 2.0, 0.0, 0.0, 8.0, 8.0, 3.0), 1.0);
        assert_eq!(
            rounded(0.0, 0.0, 0.0, 0.0, f32::MAX, f32::MAX, f32::MAX),
            0.0
        );
    }
    #[test]
    fn rounded_area_is_symmetric_and_conserves_shape_area() {
        let mut area = 0.0;
        for y in 0..12 {
            for x in 0..16 {
                let value = rounded(x as f32, y as f32, 0.0, 0.0, 16.0, 12.0, 4.0);
                assert!(
                    (value - rounded((15 - x) as f32, (11 - y) as f32, 0.0, 0.0, 16.0, 12.0, 4.0))
                        .abs()
                        < 1e-6
                );
                area += value;
            }
        }
        let corner = contour_area(
            [[4.0, 0.0], [4.0, 4.0], [0.0, 4.0]],
            std::f64::consts::FRAC_1_SQRT_2,
            [0.0, 0.0, 4.0, 4.0],
            0,
            0.0,
            false,
        ) as f32;
        let expected = 16.0 * 12.0 - (16.0 - corner) * 4.0;
        assert!((area - expected).abs() < 1e-4);
    }
}
