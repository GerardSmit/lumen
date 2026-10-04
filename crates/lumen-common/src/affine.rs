//! Finite two-dimensional affine interpolation, independent of CSS and hosts.
//! Coefficients use column-vector order [a, b, c, d, e, f].

#[derive(Clone, Copy)]
struct Parts {
    translation: [f64; 2],
    scale: [f64; 2],
    angle: f64,
    remainder: [f64; 4],
}

fn decompose(m: [f64; 6]) -> Option<Parts> {
    if !m.iter().all(|v| v.is_finite()) {
        return None;
    }
    let [a, b, c, d, e, f] = m;
    let det = a * d - b * c;
    if det == 0.0 || !det.is_finite() {
        return None;
    }
    let mut sx = a.hypot(b);
    let mut sy = c.hypot(d);
    if det < 0.0 {
        if a < d {
            sx = -sx;
        } else {
            sy = -sy;
        }
    }
    let (a, b, c, d) = (a / sx, b / sx, c / sy, d / sy);
    let angle = b.atan2(a);
    let (sin, cos) = angle.sin_cos();
    Some(Parts {
        translation: [e, f],
        scale: [sx, sy],
        angle,
        // Remove rotation before retaining the normalized shear remainder.
        remainder: [
            cos * a + sin * b,
            -sin * a + cos * b,
            cos * c + sin * d,
            -sin * c + cos * d,
        ],
    })
}

fn recompose(parts: Parts) -> Option<[f64; 6]> {
    let [sx, sy] = parts.scale;
    let r = parts.remainder;
    let (sin, cos) = parts.angle.sin_cos();
    let result = [
        (cos * r[0] - sin * r[1]) * sx,
        (sin * r[0] + cos * r[1]) * sx,
        (cos * r[2] - sin * r[3]) * sy,
        (sin * r[2] + cos * r[3]) * sy,
        parts.translation[0],
        parts.translation[1],
    ];
    result.iter().all(|v| v.is_finite()).then_some(result)
}

/// Accumulate decomposed parameters, using a delta from one for scale and
/// diagonal remainder entries, and a delta from zero for other parameters.
pub fn accumulate(from: [f64; 6], to: [f64; 6]) -> Option<[f64; 6]> {
    let a = decompose(from)?;
    let b = decompose(to)?;
    recompose(Parts {
        translation: core::array::from_fn(|i| a.translation[i] + b.translation[i]),
        scale: core::array::from_fn(|i| a.scale[i] + b.scale[i] - 1.0),
        angle: a.angle + b.angle,
        remainder: core::array::from_fn(|i| {
            a.remainder[i] + b.remainder[i] - if i == 0 || i == 3 { 1.0 } else { 0.0 }
        }),
    })
}

/// Interpolate decomposed affine matrices using the shorter rotation path.
/// Singular endpoints have no decomposition and require discrete sampling.
pub fn interpolate(from: [f64; 6], to: [f64; 6], progress: f64) -> Option<[f64; 6]> {
    if !progress.is_finite() {
        return None;
    }
    let mut a = decompose(from)?;
    let mut b = decompose(to)?;
    let pi = core::f64::consts::PI;
    if (a.scale[0] < 0.0 && b.scale[1] < 0.0) || (a.scale[1] < 0.0 && b.scale[0] < 0.0) {
        a.scale = [-a.scale[0], -a.scale[1]];
        a.angle += if a.angle < 0.0 { pi } else { -pi };
    }
    if a.angle == 0.0 {
        a.angle = 2.0 * pi;
    }
    if b.angle == 0.0 {
        b.angle = 2.0 * pi;
    }
    if (a.angle - b.angle).abs() > pi {
        if a.angle > b.angle {
            a.angle -= 2.0 * pi;
        } else {
            b.angle -= 2.0 * pi;
        }
    }
    let lerp = |x: f64, y: f64| x + (y - x) * progress;
    recompose(Parts {
        translation: core::array::from_fn(|i| lerp(a.translation[i], b.translation[i])),
        scale: core::array::from_fn(|i| lerp(a.scale[i], b.scale[i])),
        angle: lerp(a.angle, b.angle),
        remainder: core::array::from_fn(|i| lerp(a.remainder[i], b.remainder[i])),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn close(a: [f64; 6], b: [f64; 6]) {
        for (a, b) in a.into_iter().zip(b) {
            assert!((a - b).abs() < 1e-9, "{a} != {b}");
        }
    }
    #[test]
    fn affine_interpolation_preserves_shear_translation_and_reflection_endpoints() {
        let a = [1.2, 0.8, -0.3, 2.0, 17.0, -9.0];
        let b = [-2.0, 0.3, 1.1, 0.8, -7.0, 21.0];
        close(interpolate(a, b, 0.0).unwrap(), a);
        close(interpolate(a, b, 1.0).unwrap(), b);
        let mid = interpolate(a, b, 0.5).unwrap();
        assert!((mid[4] - 5.0).abs() < 1e-9 && (mid[5] - 6.0).abs() < 1e-9);
        close(interpolate(a, a, 0.37).unwrap(), a);
    }
    #[test]
    fn affine_interpolation_short_rotation_and_singular_fallback() {
        let rotation = |deg: f64| {
            let (sin, cos) = deg.to_radians().sin_cos();
            [cos, sin, -sin, cos, 0.0, 0.0]
        };
        close(
            interpolate(rotation(170.0), rotation(-170.0), 0.5).unwrap(),
            rotation(180.0),
        );
        assert!(interpolate([0.0; 6], rotation(0.0), 0.5).is_none());
        assert!(interpolate(rotation(0.0), rotation(90.0), f64::NAN).is_none());
    }
}
