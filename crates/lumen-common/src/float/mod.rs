//! Floating-point algorithms whose results the languages specify more tightly than the platform
//! libm does: shortest round-trip and correctly rounded fixed-precision formatting, exact summation, Euclidean norms, gamma and the error
//! functions.

pub mod complex;
mod erf;
pub mod format;
mod fsum;
mod gamma;
mod norm;
mod shortest;

pub use erf::{erf, erfc};
pub use fsum::{fsum, Fsum, FsumError};
pub use gamma::{lgamma, sinpi, tgamma};
pub use norm::{frexp, frexp_exp, hypot, ldexp, vector_norm};
pub use shortest::{shortest, Digits};

/// Why a math function has no finite result (C's `EDOM` / `ERANGE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MathError {
    Domain,
    Range,
}

/// IEEE 754 remainder `x - n*y` with `n` the integer nearest `x/y` (ties to even), computed
/// exactly (CPython's `m_remainder`); NaN for an infinite `x` or a zero `y`.
pub fn remainder(x: f64, y: f64) -> f64 {
    if x.is_finite() && y.is_finite() {
        if y == 0.0 {
            return f64::NAN;
        }
        let (absx, absy) = (x.abs(), y.abs());
        let m = absx % absy;
        let c = absy - m;
        let r = if m < c {
            m
        } else if m > c {
            -c
        } else {
            // Halfway: choose the even multiple. `0.5 * (absx - m)` is exact here.
            m - 2.0 * ((0.5 * (absx - m)) % absy)
        };
        return 1f64.copysign(x) * r;
    }
    if x.is_nan() {
        return x;
    }
    if y.is_nan() {
        return y;
    }
    if x.is_infinite() {
        return f64::NAN;
    }
    x
}

#[cfg(unix)]
mod sys {
    extern "C" {
        pub fn asinh(x: f64) -> f64;
        pub fn acosh(x: f64) -> f64;
        pub fn atanh(x: f64) -> f64;
    }
}

// Rust's std computes the inverse hyperbolics with its own short formulas (hundreds of ulps off
// near |x| = 1 for atanh); C and CPython use the platform libm, which std links on Unix.

pub fn asinh(x: f64) -> f64 {
    #[cfg(unix)]
    // SAFETY: a pure C math function on a plain double.
    return unsafe { sys::asinh(x) };
    #[cfg(not(unix))]
    return x.asinh();
}

pub fn acosh(x: f64) -> f64 {
    #[cfg(unix)]
    // SAFETY: a pure C math function on a plain double.
    return unsafe { sys::acosh(x) };
    #[cfg(not(unix))]
    return x.acosh();
}

pub fn atanh(x: f64) -> f64 {
    #[cfg(unix)]
    // SAFETY: a pure C math function on a plain double.
    return unsafe { sys::atanh(x) };
    #[cfg(not(unix))]
    return fdlibm_atanh(x);
}

/// fdlibm's `atanh`, accurate where `log1p` is.
#[cfg_attr(unix, allow(dead_code))]
fn fdlibm_atanh(x: f64) -> f64 {
    let ax = x.abs();
    if ax > 1.0 {
        return f64::NAN;
    }
    let t = if ax < 0.5 {
        let t = ax + ax;
        0.5 * (t + t * ax / (1.0 - ax)).ln_1p()
    } else {
        0.5 * ((ax + ax) / (1.0 - ax)).ln_1p()
    };
    if x < 0.0 {
        -t
    } else if x == 0.0 {
        x
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_hyperbolics() {
        assert_eq!(atanh(-0.9999999999999999), -18.714973875118524);
        // Both go through the platform libm (`log1p` for fdlibm's), and glibc and Apple's differ
        // by an ulp here.
        assert!((atanh(-0.5) - -0.5493061443340549).abs() <= f64::EPSILON);
        assert!((fdlibm_atanh(-0.5) - -0.5493061443340549).abs() <= f64::EPSILON);
        assert_eq!(atanh(1.0), f64::INFINITY);
        assert!(atanh(1.5).is_nan());
        assert_eq!(asinh(-0.0).to_bits(), (-0.0f64).to_bits());
        assert_eq!(acosh(1.0), 0.0);
    }

    #[test]
    fn remainders() {
        assert_eq!(remainder(5.0, 2.0), 1.0);
        assert_eq!(remainder(7.0, 2.0), -1.0);
        assert_eq!(remainder(-7.0, 2.0), 1.0);
        assert_eq!(remainder(1e308, 3.0), 1e308 % 3.0 - 3.0);
        assert!(remainder(f64::INFINITY, 1.0).is_nan());
        assert_eq!(remainder(1.0, f64::INFINITY), 1.0);
    }
}
