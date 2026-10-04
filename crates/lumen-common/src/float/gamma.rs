//! Gamma and log-gamma with CPython's Lanczos approximation (`m_tgamma` / `m_lgamma`), which is
//! accurate to a few ulps everywhere and exact on small integers.
//!
//! The multiply-adds are fused, as the C compilers that build CPython contract them on targets
//! with FMA (AArch64 always), so the results agree bit for bit with those builds.

use super::MathError;

const LANCZOS_N: usize = 13;
// Exactly 6.024680040776729583740234375 and 5.524680040776729583740234375, as CPython writes them.
const LANCZOS_G: f64 = 6.02468004077673;
const LANCZOS_G_MINUS_HALF: f64 = 5.52468004077673;
#[allow(clippy::excessive_precision)]
const LANCZOS_NUM: [f64; LANCZOS_N] = [
    23531376880.410759688572007674451636754734846804940,
    42919803642.649098768957899047001988850926355848959,
    35711959237.355668049440185451547166705960488635843,
    17921034426.037209699919755754458931112671403265390,
    6039542586.3520280050642916443072979210699388420708,
    1439720407.3117216736632230727949123939715485786772,
    248874557.86205415651146038641322942321632125127801,
    31426415.585400194380614231628318205362874684987640,
    2876370.6289353724412254090516208496135991145378768,
    186056.26539522349504029498971604569928220784236328,
    8071.6720023658162106380029022722506138218516325024,
    210.82427775157934587250973392071336271166969580291,
    2.5066282746310002701649081771338373386264310793408,
];
const LANCZOS_DEN: [f64; LANCZOS_N] = [
    0.0,
    39916800.0,
    120543840.0,
    150917976.0,
    105258076.0,
    45995730.0,
    13339535.0,
    2637558.0,
    357423.0,
    32670.0,
    1925.0,
    66.0,
    1.0,
];
const GAMMA_INTEGRAL: [f64; 23] = [
    1.0,
    1.0,
    2.0,
    6.0,
    24.0,
    120.0,
    720.0,
    5040.0,
    40320.0,
    362880.0,
    3628800.0,
    39916800.0,
    479001600.0,
    6227020800.0,
    87178291200.0,
    1307674368000.0,
    20922789888000.0,
    355687428096000.0,
    6402373705728000.0,
    121645100408832000.0,
    2432902008176640000.0,
    51090942171709440000.0,
    1124000727777607680000.0,
];
#[allow(clippy::excessive_precision)]
const LOG_PI: f64 = 1.144729885849400174143427351353058711647;
const PI: f64 = core::f64::consts::PI;

fn lanczos_sum(x: f64) -> f64 {
    let (mut num, mut den) = (0.0f64, 0.0f64);
    // Evaluate the rational function in whichever direction keeps the terms from overflowing.
    if x < 5.0 {
        for i in (0..LANCZOS_N).rev() {
            num = num.mul_add(x, LANCZOS_NUM[i]);
            den = den.mul_add(x, LANCZOS_DEN[i]);
        }
    } else {
        for i in 0..LANCZOS_N {
            num = num / x + LANCZOS_NUM[i];
            den = den / x + LANCZOS_DEN[i];
        }
    }
    num / den
}

/// sin(pi * x), exact at the multiples of 1/2.
pub fn sinpi(x: f64) -> f64 {
    let y = x.abs() % 2.0;
    let n = (2.0 * y).round() as i32;
    let r = match n {
        0 => (PI * y).sin(),
        1 => (PI * (y - 0.5)).cos(),
        2 => (PI * (1.0 - y)).sin(),
        3 => -(PI * (y - 1.5)).cos(),
        _ => (PI * (y - 2.0)).sin(),
    };
    1f64.copysign(x) * r
}

/// The gamma function; `Domain` at the poles and `-inf`, `Range` on overflow.
pub fn tgamma(x: f64) -> Result<f64, MathError> {
    if !x.is_finite() {
        return if x.is_nan() || x > 0.0 {
            Ok(x)
        } else {
            Err(MathError::Domain)
        };
    }
    if x == 0.0 {
        return Err(MathError::Domain);
    }
    if x == x.floor() {
        if x < 0.0 {
            return Err(MathError::Domain);
        }
        if x <= GAMMA_INTEGRAL.len() as f64 {
            return Ok(GAMMA_INTEGRAL[x as usize - 1]);
        }
    }
    let absx = x.abs();
    if absx < 1e-20 {
        let r = 1.0 / x;
        return if r.is_infinite() {
            Err(MathError::Range)
        } else {
            Ok(r)
        };
    }
    if absx > 200.0 {
        return if x < 0.0 {
            Ok(0.0 / sinpi(x))
        } else {
            Err(MathError::Range)
        };
    }
    let y = absx + LANCZOS_G_MINUS_HALF;
    // The rounding error of `y`, recovered exactly.
    let z = if absx > LANCZOS_G_MINUS_HALF {
        (y - absx) - LANCZOS_G_MINUS_HALF
    } else {
        (y - LANCZOS_G_MINUS_HALF) - absx
    };
    let z = z * LANCZOS_G / y;
    let mut r;
    if x < 0.0 {
        r = -PI / sinpi(absx) / absx * y.exp() / lanczos_sum(absx);
        r = (-z).mul_add(r, r);
        if absx < 140.0 {
            r /= y.powf(absx - 0.5);
        } else {
            let sqrtpow = y.powf(absx / 2.0 - 0.25);
            r /= sqrtpow;
            r /= sqrtpow;
        }
    } else {
        r = lanczos_sum(absx) / y.exp();
        r = z.mul_add(r, r);
        if absx < 140.0 {
            r *= y.powf(absx - 0.5);
        } else {
            let sqrtpow = y.powf(absx / 2.0 - 0.25);
            r *= sqrtpow;
            r *= sqrtpow;
        }
    }
    if r.is_infinite() {
        return Err(MathError::Range);
    }
    Ok(r)
}

/// The natural log of |gamma(x)|; `Domain` at the poles, `Range` on overflow.
pub fn lgamma(x: f64) -> Result<f64, MathError> {
    if !x.is_finite() {
        return Ok(if x.is_nan() { x } else { f64::INFINITY });
    }
    if x == x.floor() && x <= 2.0 {
        return if x <= 0.0 {
            Err(MathError::Domain)
        } else {
            Ok(0.0)
        };
    }
    let absx = x.abs();
    if absx < 1e-20 {
        return Ok(-absx.ln());
    }
    let mut r = lanczos_sum(absx).ln() - LANCZOS_G;
    r = (absx - 0.5).mul_add((absx + LANCZOS_G - 0.5).ln() - 1.0, r);
    if x < 0.0 {
        r = LOG_PI - sinpi(absx).abs().ln() - absx.ln() - r;
    }
    if r.is_infinite() {
        return Err(MathError::Range);
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamma_values() {
        assert_eq!(tgamma(5.0), Ok(24.0));
        assert_eq!(tgamma(0.5), Ok(1.7724538509055159));
        assert_eq!(tgamma(0.0), Err(MathError::Domain));
        assert_eq!(tgamma(-3.0), Err(MathError::Domain));
        assert_eq!(tgamma(171.7), Err(MathError::Range));
        assert_eq!(lgamma(1.0), Ok(0.0));
        assert_eq!(lgamma(0.0), Err(MathError::Domain));
        assert_eq!(lgamma(f64::NEG_INFINITY), Ok(f64::INFINITY));
    }
}
