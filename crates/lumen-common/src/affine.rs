//! Finite two-dimensional affine interpolation, independent of CSS and hosts.
//! Coefficients use column-vector order [a, b, c, d, e, f].

#[derive(Clone, Copy)]
struct Parts {
    translation: [f64; 2],
    scale: [f64; 2],
    angle: f64,
    skew: f64,
}

/// CSS Transforms 2D matrix decomposition: the second column is split into
/// a component along the rotated x axis (the skew) and one orthogonal to it.
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
    if det < 0.0 && a < d {
        sx = -sx;
    }
    let (ux, uy) = (a / sx, b / sx);
    let sy = det / sx;
    let along = ux * c + uy * d;
    Some(Parts {
        translation: [e, f],
        scale: [sx, sy],
        angle: uy.atan2(ux),
        skew: along / sy,
    })
}

fn recompose(parts: Parts) -> Option<[f64; 6]> {
    let [sx, sy] = parts.scale;
    let (sin, cos) = parts.angle.sin_cos();
    let result = [
        cos * sx,
        sin * sx,
        sy * (parts.skew * cos - sin),
        sy * (parts.skew * sin + cos),
        parts.translation[0],
        parts.translation[1],
    ];
    result.iter().all(|v| v.is_finite()).then_some(result)
}

/// Accumulate decomposed parameters, using a delta from one for scale
/// and a delta from zero for the other parameters.
pub fn accumulate(from: [f64; 6], to: [f64; 6]) -> Option<[f64; 6]> {
    accumulate_weighted(from,to,1.0)
}

/// Weighted accumulation used by an animation's completed iterations.
pub fn accumulate_weighted(from:[f64;6],to:[f64;6],count:f64)->Option<[f64;6]> {
    if !count.is_finite(){return None;}
    let a = decompose(from)?;
    let b = decompose(to)?;
    recompose(Parts {
        translation: core::array::from_fn(|i| count*a.translation[i] + b.translation[i]),
        scale: core::array::from_fn(|i| count*(a.scale[i]-1.0)+b.scale[i]),
        angle: count*a.angle + b.angle,
        skew: count*a.skew+b.skew,
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
        skew: lerp(a.skew, b.skew),
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
