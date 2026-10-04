//! Euclidean norm with CPython's `vector_norm` algorithm (`math.hypot`, `math.dist`): the
//! coordinates are scaled by a power of two, squared and summed in double-length arithmetic, and
//! the root gets one differential correction, so the result is almost always correctly rounded.

/// `sqrt(sum(x*x))` of `|coordinates|`; infinity wins over NaN, as `hypot` requires.
pub fn hypot(coordinates: &mut [f64]) -> f64 {
    let mut max = 0.0f64;
    let mut nan = false;
    for x in coordinates.iter_mut() {
        *x = x.abs();
        nan |= x.is_nan();
        if *x > max {
            max = *x;
        }
    }
    vector_norm(coordinates, max, nan)
}

/// The norm of `vec` (non-negative values, `max` their maximum, `nan` whether any is NaN).
pub fn vector_norm(vec: &mut [f64], max: f64, nan: bool) -> f64 {
    if max.is_infinite() {
        return max;
    }
    if nan {
        return f64::NAN;
    }
    if max == 0.0 || vec.len() <= 1 {
        return max;
    }
    let max_e = frexp_exp(max);
    if max_e < -1023 {
        // The scale 2^-max_e would overflow: lift subnormals into the normal range first.
        for x in vec.iter_mut() {
            *x /= f64::MIN_POSITIVE;
        }
        return f64::MIN_POSITIVE * vector_norm(vec, max / f64::MIN_POSITIVE, nan);
    }
    let scale = pow2(-max_e);
    let (mut csum, mut frac1, mut frac2) = (1.0f64, 0.0f64, 0.0f64);
    for &x in vec.iter() {
        let x = x * scale;
        let (hi, lo) = dl_mul(x, x);
        let (shi, slo) = dl_fast_sum(csum, hi);
        csum = shi;
        frac1 += lo;
        frac2 += slo;
    }
    let mut h = (csum - 1.0 + (frac1 + frac2)).sqrt();
    let (hi, lo) = dl_mul(-h, h);
    let (shi, slo) = dl_fast_sum(csum, hi);
    csum = shi;
    frac1 += lo;
    frac2 += slo;
    let x = csum - 1.0 + (frac1 + frac2);
    h += x / (2.0 * h);
    h / scale
}

fn dl_mul(x: f64, y: f64) -> (f64, f64) {
    let hi = x * y;
    (hi, x.mul_add(y, -hi))
}

fn dl_fast_sum(a: f64, b: f64) -> (f64, f64) {
    let hi = a + b;
    (hi, (a - hi) + b)
}

/// `frexp`'s exponent: `x = m * 2^e` with `0.5 <= m < 1` (subnormals report their true exponent).
pub fn frexp_exp(x: f64) -> i32 {
    frexp(x).1
}

/// C's `frexp`: `(m, e)` with `x = m * 2^e` and `0.5 <= |m| < 1`; zero, infinities and NaN come
/// back unchanged with `e == 0`.
pub fn frexp(x: f64) -> (f64, i32) {
    if x == 0.0 || !x.is_finite() {
        return (x, 0);
    }
    let bits = x.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    if exp == 0 {
        let (m, e) = frexp(x * pow2(64));
        return (m, e - 64);
    }
    (
        f64::from_bits((bits & !(0x7ffu64 << 52)) | (1022u64 << 52)),
        exp - 1022,
    )
}

/// C's `ldexp`: `x * 2^e` rounded once, for any exponent.
pub fn ldexp(x: f64, e: i64) -> f64 {
    if x == 0.0 || !x.is_finite() {
        return x;
    }
    let (m, ex) = frexp(x);
    let total = (ex as i64).saturating_add(e);
    if total > 1024 {
        return f64::INFINITY.copysign(x);
    }
    if total < -1080 {
        return 0f64.copysign(x);
    }
    let total = total as i32;
    if total > -1022 {
        (m * 2.0) * pow2(total - 1)
    } else {
        // Scale exactly into the normal range, then round once into the subnormals.
        (m * pow2(total + 1074)) * pow2(-1074)
    }
}

/// 2^e for `e` in -1074..=1023.
fn pow2(e: i32) -> f64 {
    if e < -1022 {
        f64::from_bits(1u64 << (e + 1074))
    } else {
        f64::from_bits(((e + 1023) as u64) << 52)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norms() {
        assert_eq!(hypot(&mut [3.0, 4.0]), 5.0);
        assert_eq!(hypot(&mut [-2e200]), 2e200);
        assert_eq!(hypot(&mut [1e200, 1e200]), 1.414213562373095e200);
        assert_eq!(hypot(&mut [f64::NAN, f64::NEG_INFINITY]), f64::INFINITY);
        assert!(hypot(&mut [f64::NAN, 1.0]).is_nan());
        assert_eq!(hypot(&mut []), 0.0);
        assert_eq!(hypot(&mut [f64::MAX, 1.0]), f64::MAX);
        assert_eq!(hypot(&mut [5e-324, 5e-324]), 5e-324);
        assert_eq!(frexp_exp(1.0), 1);
        assert_eq!(frexp_exp(0.75), 0);
        assert_eq!(frexp_exp(5e-324), -1073);
        assert_eq!(ldexp(1e-300, 1 << 40), f64::INFINITY);
        assert_eq!(ldexp(-5e-324, 1074), -1.0);
        assert_eq!(ldexp(1.0, -1074), 5e-324);
        assert_eq!(ldexp(1.0, -1075), 0.0);
        assert_eq!(ldexp(1.5, -1075), 5e-324);
        assert_eq!(ldexp(0.75, 1024), 1.348269851146737e308);
        assert_eq!(ldexp(3.0, i64::MIN), 0.0);
    }
}
