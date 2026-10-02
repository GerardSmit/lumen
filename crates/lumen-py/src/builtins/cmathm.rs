//! The `cmath` module, on the shared complex functions of `lumen_common::float::complex`.

/// This module provides access to mathematical functions for complex
/// numbers.
#[lumen_bind::module(name = "cmath")]
pub mod cmath {
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};
    use lumen_common::float::complex::{self as cx, CResult, Complex};
    use lumen_common::float::MathError;

    fn arg(it: &mut Interp, v: &Value) -> R<Complex> {
        let (re, im) = it.complex_arg(v)?;
        Ok(Complex::new(re, im))
    }

    fn value(z: Complex) -> Value {
        Value::Obj(Object::new(Kind::Complex(z.re, z.im)))
    }

    fn error(it: &mut Interp, e: MathError) -> Obj {
        match e {
            MathError::Domain => it.value_error("math domain error"),
            MathError::Range => it.overflow_err("math range error"),
        }
    }

    fn apply(it: &mut Interp, z: &Value, f: fn(Complex) -> CResult) -> R<Value> {
        let z = arg(it, z)?;
        match f(z) {
            Ok(r) => Ok(value(r)),
            Err((e, _)) => Err(error(it, e)),
        }
    }

    #[constant(name = "pi")]
    const PI: f64 = std::f64::consts::PI;
    #[constant(name = "e")]
    const E: f64 = std::f64::consts::E;
    #[constant(name = "tau")]
    const TAU: f64 = std::f64::consts::TAU;
    #[constant(name = "inf")]
    const INF: f64 = f64::INFINITY;
    #[constant(name = "nan")]
    const NAN: f64 = f64::NAN;

    #[init]
    fn init(it: &mut Interp, m: &Value) -> R<()> {
        let Value::Obj(m) = m else { return Ok(()) };
        let d = it.module_dict(m);
        dict_set_str(&d, "infj", value(Complex::new(0.0, f64::INFINITY)));
        dict_set_str(&d, "nanj", value(Complex::new(0.0, f64::NAN)));
        Ok(())
    }

    /// Return the square root of z.
    #[op]
    fn sqrt(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::sqrt)
    }

    /// Return the exponential value e**z.
    #[op]
    fn exp(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::exp)
    }

    /// Return the base-10 logarithm of z.
    #[op]
    fn log10(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::log10)
    }

    /// Return the arc cosine of z.
    #[op]
    fn acos(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::acos)
    }

    /// Return the arc sine of z.
    #[op]
    fn asin(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::asin)
    }

    /// Return the arc tangent of z.
    #[op]
    fn atan(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::atan)
    }

    /// Return the cosine of z.
    #[op]
    fn cos(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::cos)
    }

    /// Return the sine of z.
    #[op]
    fn sin(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::sin)
    }

    /// Return the tangent of z.
    #[op]
    fn tan(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::tan)
    }

    /// Return the inverse hyperbolic cosine of z.
    #[op]
    fn acosh(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::acosh)
    }

    /// Return the inverse hyperbolic sine of z.
    #[op]
    fn asinh(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::asinh)
    }

    /// Return the inverse hyperbolic tangent of z.
    #[op]
    fn atanh(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::atanh)
    }

    /// Return the hyperbolic cosine of z.
    #[op]
    fn cosh(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::cosh)
    }

    /// Return the hyperbolic sine of z.
    #[op]
    fn sinh(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::sinh)
    }

    /// Return the hyperbolic tangent of z.
    #[op]
    fn tanh(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::tanh)
    }

    /// log(z[, base]) -> the logarithm of z to the given base.
    ///
    /// If the base is not specified, returns the natural logarithm (base e) of z.
    #[op]
    fn log(it: &mut Interp, x: &Value, base: Option<&Value>) -> R<Value> {
        let x = arg(it, x)?;
        let base = base.map(|b| arg(it, b)).transpose()?;
        let mut err = None;
        let mut l = |z| {
            cx::log(z).unwrap_or_else(|(e, r)| {
                err = Some(e);
                r
            })
        };
        let mut r = l(x);
        if let Some(b) = base {
            r = cx::quot(r, l(b)).unwrap_or_else(|| {
                err = Some(MathError::Domain);
                Complex::new(f64::NAN, f64::NAN)
            });
        }
        match err {
            Some(e) => Err(error(it, e)),
            None => Ok(value(r)),
        }
    }

    /// Return argument, also known as the phase angle, of a complex.
    #[op]
    fn phase(it: &mut Interp, z: &Value) -> R<f64> {
        Ok(cx::phase(arg(it, z)?))
    }

    /// Convert a complex from rectangular coordinates to polar coordinates.
    ///
    /// r is the distance from 0 and phi the phase angle.
    #[op]
    fn polar(it: &mut Interp, z: &Value) -> R<Value> {
        let z = arg(it, z)?;
        let phi = cx::phase(z);
        match cx::abs(z) {
            Ok(r) => Ok(Value::tuple(vec![Value::Float(r), Value::Float(phi)])),
            Err(e) => Err(error(it, e)),
        }
    }

    /// Convert from polar coordinates to rectangular coordinates.
    #[op]
    fn rect(it: &mut Interp, r: f64, phi: f64) -> R<Value> {
        match cx::rect(r, phi) {
            Ok(z) => Ok(value(z)),
            Err((e, _)) => Err(error(it, e)),
        }
    }

    /// Return True if both the real and imaginary parts of z are finite, else False.
    #[op]
    fn isfinite(it: &mut Interp, z: &Value) -> R<bool> {
        let z = arg(it, z)?;
        Ok(z.re.is_finite() && z.im.is_finite())
    }

    /// Checks if the real or imaginary part of z not a number (NaN).
    #[op]
    fn isnan(it: &mut Interp, z: &Value) -> R<bool> {
        let z = arg(it, z)?;
        Ok(z.re.is_nan() || z.im.is_nan())
    }

    /// Checks if the real or imaginary part of z is infinite.
    #[op]
    fn isinf(it: &mut Interp, z: &Value) -> R<bool> {
        let z = arg(it, z)?;
        Ok(z.re.is_infinite() || z.im.is_infinite())
    }

    /// Determine whether two complex numbers are close in value.
    ///
    ///   rel_tol
    ///     maximum difference for being considered "close", relative to the
    ///     magnitude of the input values
    ///   abs_tol
    ///     maximum difference for being considered "close", regardless of the
    ///     magnitude of the input values
    ///
    /// Return True if a is close in value to b, and False otherwise.
    ///
    /// For the values to be considered close, the difference between them must be
    /// smaller than at least one of the tolerances.
    ///
    /// -inf, inf and NaN behave similarly to the IEEE 754 Standard. That is, NaN is
    /// not close to anything, even itself. inf and -inf are only close to themselves.
    #[op]
    fn isclose(
        it: &mut Interp,
        #[kw] a: &Value,
        #[kw] b: &Value,
        #[kwonly]
        #[default(1e-09)]
        rel_tol: f64,
        #[kwonly]
        #[default(0.0)]
        abs_tol: f64,
    ) -> R<bool> {
        let (a, b) = (arg(it, a)?, arg(it, b)?);
        if rel_tol < 0.0 || abs_tol < 0.0 {
            return Err(it.value_error("tolerances must be non-negative"));
        }
        Ok(cx::isclose(a, b, rel_tol, abs_tol))
    }
}
