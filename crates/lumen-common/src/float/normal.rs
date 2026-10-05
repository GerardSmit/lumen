//! The inverse of the normal distribution's CDF.

use super::MathError;

/// Horner evaluation with fused multiply-adds, matching the contraction a C compiler applies to
/// CPython's `_statistics` so results agree to the last bit.
fn horner(r: f64, coeffs: &[f64]) -> f64 {
    coeffs[1..]
        .iter()
        .fold(coeffs[0], |acc, &c| acc.mul_add(r, c))
}

/// The `p`-quantile of the normal distribution with mean `mu` and standard deviation `sigma`:
/// Wichura's algorithm AS241 (PPND16), as CPython's `statistics` computes it. A `p` of 0 or 1 is
/// a domain error.
#[allow(clippy::excessive_precision)]
pub fn normal_inv_cdf(p: f64, mu: f64, sigma: f64) -> Result<f64, MathError> {
    let q = p - 0.5;
    if q.abs() <= 0.425 {
        let r = (-q).mul_add(q, 0.180625);
        let num = horner(
            r,
            &[
                2.5090809287301226727e+3,
                3.3430575583588128105e+4,
                6.7265770927008700853e+4,
                4.5921953931549871457e+4,
                1.3731693765509461125e+4,
                1.9715909503065514427e+3,
                1.3314166789178437745e+2,
                3.3871328727963666080e+0,
            ],
        ) * q;
        let den = horner(
            r,
            &[
                5.2264952788528545610e+3,
                2.8729085735721942674e+4,
                3.9307895800092710610e+4,
                2.1213794301586595867e+4,
                5.3941960214247511077e+3,
                6.8718700749205790830e+2,
                4.2313330701600911252e+1,
                1.0,
            ],
        );
        return Ok((num / den).mul_add(sigma, mu));
    }
    let r = if q <= 0.0 { p } else { 1.0 - p };
    if r.is_nan() {
        return Ok(f64::NAN);
    }
    if r <= 0.0 {
        return Err(MathError::Domain);
    }
    let r = (-r.ln()).sqrt();
    let (num, den) = if r <= 5.0 {
        let r = r - 1.6;
        (
            horner(
                r,
                &[
                    7.74545014278341407640e-4,
                    2.27238449892691845833e-2,
                    2.41780725177450611770e-1,
                    1.27045825245236838258e+0,
                    3.64784832476320460504e+0,
                    5.76949722146069140550e+0,
                    4.63033784615654529590e+0,
                    1.42343711074968357734e+0,
                ],
            ),
            horner(
                r,
                &[
                    1.05075007164441684324e-9,
                    5.47593808499534494600e-4,
                    1.51986665636164571966e-2,
                    1.48103976427480074590e-1,
                    6.89767334985100004550e-1,
                    1.67638483018380384940e+0,
                    2.05319162663775882187e+0,
                    1.0,
                ],
            ),
        )
    } else {
        let r = r - 5.0;
        (
            horner(
                r,
                &[
                    2.01033439929228813265e-7,
                    2.71155556874348757815e-5,
                    1.24266094738807843860e-3,
                    2.65321895265761230930e-2,
                    2.96560571828504891230e-1,
                    1.78482653991729133580e+0,
                    5.46378491116411436990e+0,
                    6.65790464350110377720e+0,
                ],
            ),
            horner(
                r,
                &[
                    2.04426310338993978564e-15,
                    1.42151175831644588870e-7,
                    1.84631831751005468180e-5,
                    7.86869131145613259100e-4,
                    1.48753612908506148525e-2,
                    1.36929880922735805310e-1,
                    5.99832206555887937690e-1,
                    1.0,
                ],
            ),
        )
    };
    let x = if q < 0.0 { -num / den } else { num / den };
    Ok(x.mul_add(sigma, mu))
}
