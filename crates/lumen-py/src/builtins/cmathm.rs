//! The `cmath` module, on the shared complex functions of `lumen_common::float::complex`.

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

    #[op]
    fn sqrt(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::sqrt)
    }

    #[op]
    fn exp(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::exp)
    }

    #[op]
    fn log10(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::log10)
    }

    #[op]
    fn acos(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::acos)
    }

    #[op]
    fn asin(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::asin)
    }

    #[op]
    fn atan(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::atan)
    }

    #[op]
    fn cos(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::cos)
    }

    #[op]
    fn sin(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::sin)
    }

    #[op]
    fn tan(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::tan)
    }

    #[op]
    fn acosh(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::acosh)
    }

    #[op]
    fn asinh(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::asinh)
    }

    #[op]
    fn atanh(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::atanh)
    }

    #[op]
    fn cosh(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::cosh)
    }

    #[op]
    fn sinh(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::sinh)
    }

    #[op]
    fn tanh(it: &mut Interp, z: &Value) -> R<Value> {
        apply(it, z, cx::tanh)
    }

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

    #[op]
    fn phase(it: &mut Interp, z: &Value) -> R<f64> {
        Ok(cx::phase(arg(it, z)?))
    }

    #[op]
    fn polar(it: &mut Interp, z: &Value) -> R<Value> {
        let z = arg(it, z)?;
        let phi = cx::phase(z);
        match cx::abs(z) {
            Ok(r) => Ok(Value::tuple(vec![Value::Float(r), Value::Float(phi)])),
            Err(e) => Err(error(it, e)),
        }
    }

    #[op]
    fn rect(it: &mut Interp, r: f64, phi: f64) -> R<Value> {
        match cx::rect(r, phi) {
            Ok(z) => Ok(value(z)),
            Err((e, _)) => Err(error(it, e)),
        }
    }

    #[op]
    fn isfinite(it: &mut Interp, z: &Value) -> R<bool> {
        let z = arg(it, z)?;
        Ok(z.re.is_finite() && z.im.is_finite())
    }

    #[op]
    fn isnan(it: &mut Interp, z: &Value) -> R<bool> {
        let z = arg(it, z)?;
        Ok(z.re.is_nan() || z.im.is_nan())
    }

    #[op]
    fn isinf(it: &mut Interp, z: &Value) -> R<bool> {
        let z = arg(it, z)?;
        Ok(z.re.is_infinite() || z.im.is_infinite())
    }

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
