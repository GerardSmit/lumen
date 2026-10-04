//! The `math` module.

#[lumen_bind::module(name = "math")]
pub mod math {
    use crate::ast::BinOp;
    use crate::bind::{NativeError, NativeResult};
    use crate::fmath;
    use crate::object::*;
    use crate::pyint::{BigInt, PyInt};
    use crate::vm::Interp;
    use lumen_common::float::{self as lf, MathError};

    fn domain() -> NativeError {
        NativeError::value_error("math domain error")
    }

    fn range_err() -> NativeError {
        NativeError::overflow("math range error")
    }

    /// CPython's `math_1`: a NaN from a non-NaN argument is a domain error; an infinity from a
    /// finite one is an overflow when `can_overflow`, else a singularity (a domain error).
    fn math_1(x: f64, r: f64, can_overflow: bool) -> NativeResult<f64> {
        if r.is_nan() && !x.is_nan() {
            return Err(domain());
        }
        if r.is_infinite() && x.is_finite() {
            return Err(if can_overflow { range_err() } else { domain() });
        }
        Ok(r)
    }

    fn math_err(e: MathError) -> NativeError {
        match e {
            MathError::Domain => domain(),
            MathError::Range => range_err(),
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

    #[op]
    fn sin(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::sin(x), false)
    }

    #[op]
    fn cos(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::cos(x), false)
    }

    #[op]
    fn tan(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::tan(x), false)
    }

    #[op]
    fn asin(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::asin(x), false)
    }

    #[op]
    fn acos(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::acos(x), false)
    }

    #[op]
    fn atan(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::atan(x), false)
    }

    #[op]
    fn sinh(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::sinh(x), true)
    }

    #[op]
    fn cosh(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::cosh(x), true)
    }

    #[op]
    fn tanh(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::tanh(x), false)
    }

    #[op]
    fn asinh(x: f64) -> NativeResult<f64> {
        math_1(x, lf::asinh(x), false)
    }

    #[op]
    fn acosh(x: f64) -> NativeResult<f64> {
        math_1(x, lf::acosh(x), false)
    }

    #[op]
    fn atanh(x: f64) -> NativeResult<f64> {
        math_1(x, lf::atanh(x), false)
    }

    #[op]
    fn exp(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::exp(x), true)
    }

    #[op]
    fn expm1(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::exp_m1(x), true)
    }

    #[op]
    fn exp2(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::exp2(x), true)
    }

    #[op]
    fn fabs(x: f64) -> f64 {
        f64::abs(x)
    }

    #[op]
    fn degrees(x: f64) -> f64 {
        f64::to_degrees(x)
    }

    #[op]
    fn radians(x: f64) -> f64 {
        f64::to_radians(x)
    }

    #[op]
    fn erf(x: f64) -> NativeResult<f64> {
        math_1(x, lf::erf(x), false)
    }

    #[op]
    fn erfc(x: f64) -> NativeResult<f64> {
        math_1(x, lf::erfc(x), false)
    }

    #[op]
    fn sqrt(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::sqrt(x), false)
    }

    #[op]
    fn cbrt(x: f64) -> f64 {
        fmath::cbrt(x)
    }

    /// CPython's `loghelper`: an int too large for a double is taken as `x * 2**e` (`x` from
    /// `frexp`), anything else goes through `math_1`.
    fn loghelper(it: &mut Interp, v: &Value, func: fn(f64) -> f64) -> R<f64> {
        let big = match v {
            Value::Int(i) if *i <= 0 => return Err(it.value_error("math domain error")),
            Value::Bool(false) => return Err(it.value_error("math domain error")),
            Value::Obj(o) => match &o.kind {
                Kind::Int(b) => Some(b),
                _ => None,
            },
            _ => None,
        };
        if let Some(b) = big {
            if b.is_negative() || b.is_zero() {
                return Err(it.value_error("math domain error"));
            }
            if b.to_float().is_none() {
                let (x, e) = b.frexp();
                return Ok(func(2.0).mul_add(e as f64, func(x)));
            }
        }
        let x = it.float_arg(v)?;
        let r = func(x);
        if (r.is_nan() && !x.is_nan()) || (r.is_infinite() && x.is_finite()) {
            return Err(it.value_error("math domain error"));
        }
        Ok(r)
    }

    #[op(hint(py(text_signature = "")))]
    fn log(it: &mut Interp, x: &Value, base: Option<&Value>) -> R<f64> {
        let num = loghelper(it, x, fmath::ln)?;
        match base {
            Some(b) => {
                let den = loghelper(it, b, fmath::ln)?;
                if den == 0.0 {
                    return Err(it.new_exc_str("ZeroDivisionError", "float division by zero"));
                }
                Ok(num / den)
            }
            None => Ok(num),
        }
    }

    #[op]
    fn log2(it: &mut Interp, x: &Value) -> R<f64> {
        loghelper(it, x, fmath::log2)
    }

    #[op]
    fn log10(it: &mut Interp, x: &Value) -> R<f64> {
        loghelper(it, x, fmath::log10)
    }

    #[op]
    fn log1p(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::ln_1p(x), false)
    }

    #[op]
    fn pow(x: f64, y: f64) -> NativeResult<f64> {
        let r = fmath::powf(x, y);
        if r.is_nan() && !x.is_nan() && !y.is_nan() {
            return Err(domain());
        }
        if r.is_infinite() && x.is_finite() && y.is_finite() {
            return Err(if x == 0.0 { domain() } else { range_err() });
        }
        Ok(r)
    }

    #[op]
    fn atan2(y: f64, x: f64) -> f64 {
        fmath::atan2(y, x)
    }

    #[op(hint(py(text_signature = "")))]
    fn hypot(it: &mut Interp, #[varargs] coordinates: &[Value]) -> R<f64> {
        let mut vals = Vec::with_capacity(coordinates.len());
        for v in coordinates {
            vals.push(it.float_arg(v)?);
        }
        Ok(lf::hypot(&mut vals))
    }

    #[op]
    fn copysign(x: f64, y: f64) -> f64 {
        x.copysign(y)
    }

    #[op]
    fn nextafter(x: f64, y: f64, #[kwonly] steps: Option<i64>) -> NativeResult<f64> {
        let Some(steps) = steps else {
            return Ok(next_toward(x, y));
        };
        if steps < 0 {
            return Err(NativeError::value_error(
                "steps must be a non-negative integer",
            ));
        }
        let mut x = x;
        for _ in 0..steps {
            let n = next_toward(x, y);
            if n == x || n.is_nan() {
                return Ok(n);
            }
            x = n;
        }
        Ok(x)
    }

    #[op]
    fn ulp(x: f64) -> f64 {
        if x.is_nan() {
            return x;
        }
        let x = x.abs();
        if x.is_infinite() {
            return x;
        }
        let next = next_toward(x, f64::INFINITY);
        if next.is_infinite() {
            return x - next_toward(x, 0.0);
        }
        next - x
    }

    #[op]
    fn sumprod(it: &mut Interp, p: &Value, q: &Value) -> R<Value> {
        let p = it.iterate_to_vec(p)?;
        let q = it.iterate_to_vec(q)?;
        if p.len() != q.len() {
            return Err(it.value_error("Inputs are not the same length"));
        }
        let mut acc = Value::Int(0);
        for (x, y) in p.iter().zip(q.iter()) {
            let m = it.binary_op(BinOp::Mult, x, y)?;
            acc = it.binary_op(BinOp::Add, &acc, &m)?;
        }
        Ok(acc)
    }

    fn next_toward(x: f64, y: f64) -> f64 {
        if x.is_nan() || y.is_nan() {
            return f64::NAN;
        }
        if x == y {
            return y;
        }
        if x == 0.0 {
            let tiny = f64::from_bits(1);
            return if y > 0.0 { tiny } else { -tiny };
        }
        let bits = x.to_bits();
        let up = (y > x) == (x > 0.0);
        f64::from_bits(if up { bits + 1 } else { bits - 1 })
    }

    /// CPython's `math_2` check: a NaN result needs a NaN argument.
    fn math_2(x: f64, y: f64, r: f64) -> NativeResult<f64> {
        if r.is_nan() && !x.is_nan() && !y.is_nan() {
            return Err(domain());
        }
        Ok(r)
    }

    #[op]
    fn fmod(x: f64, y: f64) -> NativeResult<f64> {
        if y.is_infinite() && x.is_finite() {
            return Ok(x);
        }
        math_2(x, y, x % y)
    }

    #[op]
    fn remainder(x: f64, y: f64) -> NativeResult<f64> {
        math_2(x, y, lf::remainder(x, y))
    }

    #[op]
    fn modf(x: f64) -> (f64, f64) {
        if x.is_infinite() {
            return (0.0f64.copysign(x), x);
        }
        let i = fmath::trunc(x);
        ((x - i).copysign(x), i)
    }

    #[op]
    fn frexp(x: f64) -> (f64, i64) {
        let (m, e) = lf::frexp(x);
        (m, e as i64)
    }

    #[op]
    fn ldexp(it: &mut Interp, x: f64, i: &Value) -> R<f64> {
        if !i.is_int_like() {
            return Err(it.type_error("Expected an int as second argument to ldexp."));
        }
        let e = match crate::bind::index(it, i)? {
            Value::Int(e) => e,
            big if int_value(&big).is_negative() => i64::MIN,
            _ => i64::MAX,
        };
        let r = lf::ldexp(x, e);
        if r.is_infinite() && x.is_finite() {
            return Err(it.new_exc_str("OverflowError", "math range error"));
        }
        Ok(r)
    }

    fn f_to_int(it: &mut Interp, f: f64) -> R<Value> {
        if f.is_nan() {
            return Err(it.value_error("cannot convert float NaN to integer"));
        }
        if f.is_infinite() {
            return Err(it.new_exc_str("OverflowError", "cannot convert float infinity to integer"));
        }
        if f.abs() < 9.0e18 {
            Ok(Value::Int(f as i64))
        } else {
            Ok(Value::big(BigInt::from_f64_trunc(f)))
        }
    }

    fn round_op(it: &mut Interp, x: &Value, dunder: &str, f: fn(f64) -> f64) -> R<Value> {
        match x {
            Value::Int(_) | Value::Bool(_) => {
                return it
                    .call_method(x, "__index__", Vec::new())
                    .or_else(|_| Ok(x.clone()));
            }
            Value::Obj(o) if matches!(o.kind, Kind::Int(_)) => return Ok(x.clone()),
            Value::Float(v) => return f_to_int(it, f(*v)),
            v => {
                let cls = it.type_of(v);
                if it.lookup_mro(&cls, dunder).is_some() {
                    return it.call_method(v, dunder, Vec::new());
                }
                if dunder == "__trunc__" {
                    let t = it.type_name_of(v);
                    return Err(
                        it.type_error(&format!("type {} doesn't define __trunc__ method", t))
                    );
                }
            }
        }
        let v = it.float_arg(x)?;
        f_to_int(it, f(v))
    }

    #[op]
    fn floor(it: &mut Interp, x: &Value) -> R<Value> {
        round_op(it, x, "__floor__", fmath::floor)
    }

    #[op]
    fn ceil(it: &mut Interp, x: &Value) -> R<Value> {
        round_op(it, x, "__ceil__", fmath::ceil)
    }

    #[op]
    fn trunc(it: &mut Interp, x: &Value) -> R<Value> {
        round_op(it, x, "__trunc__", fmath::trunc)
    }

    #[op]
    fn isnan(x: f64) -> bool {
        x.is_nan()
    }

    #[op]
    fn isinf(x: f64) -> bool {
        x.is_infinite()
    }

    #[op]
    fn isfinite(x: f64) -> bool {
        x.is_finite()
    }

    #[op]
    fn isclose(
        #[kw] a: f64,
        #[kw] b: f64,
        #[kwonly]
        #[default(1e-09)]
        rel_tol: f64,
        #[kwonly]
        #[default(0.0)]
        abs_tol: f64,
    ) -> NativeResult<bool> {
        if rel_tol < 0.0 || abs_tol < 0.0 {
            return Err(NativeError::value_error("tolerances must be non-negative"));
        }
        if a == b {
            return Ok(true);
        }
        if a.is_infinite() || b.is_infinite() {
            return Ok(false);
        }
        let diff = (a - b).abs();
        Ok(diff <= (rel_tol * b).abs() || diff <= (rel_tol * a).abs() || diff <= abs_tol)
    }

    #[op]
    fn gcd(it: &mut Interp, #[varargs] integers: &[Value]) -> R<BigInt> {
        let mut acc = BigInt::from_i64(0);
        for v in integers {
            let b = crate::bind::index(it, v)?;
            acc = acc.gcd(&int_value(&b).abs());
        }
        Ok(acc)
    }

    #[op]
    fn lcm(it: &mut Interp, #[varargs] integers: &[Value]) -> R<BigInt> {
        let mut acc = BigInt::from_i64(1);
        for v in integers {
            let b = crate::bind::index(it, v)?;
            let b = int_value(&b).abs();
            if b.is_zero() || acc.is_zero() {
                acc = BigInt::from_i64(0);
                continue;
            }
            let g = acc.gcd(&b);
            acc = acc.mul(&b).floor_div(&g);
        }
        Ok(acc)
    }

    fn int_value(v: &Value) -> BigInt {
        match v {
            Value::Int(i) => BigInt::from_i64(*i),
            Value::Obj(o) => match &o.kind {
                Kind::Int(b) => b.clone(),
                _ => BigInt::from_i64(0),
            },
            _ => BigInt::from_i64(0),
        }
    }

    #[op]
    fn factorial(it: &mut Interp, n: &Value) -> R<BigInt> {
        if matches!(n, Value::Float(_)) {
            return Err(it.type_error("'float' object cannot be interpreted as an integer"));
        }
        let n = match crate::bind::index(it, n)? {
            Value::Int(n) => n,
            big if int_value(&big).is_negative() => -1,
            _ => {
                return Err(it.overflow_err(&format!(
                    "factorial() argument should not exceed {}",
                    i64::MAX
                )))
            }
        };
        if n < 0 {
            return Err(it.value_error("factorial() not defined for negative values"));
        }
        let nf = n as f64;
        it.check_int_bits((nf * (nf / std::f64::consts::E).log2().max(0.0)) as u128)?;
        let mut acc = BigInt::from_i64(1);
        let mut small: i64 = 1;
        for i in 2..=n {
            if i & 0xfff == 0 {
                it.poll()?;
            }
            match small.checked_mul(i) {
                Some(v) => small = v,
                None => {
                    acc = acc.mul(&BigInt::from_i64(small));
                    small = i;
                }
            }
        }
        Ok(acc.mul(&BigInt::from_i64(small)))
    }

    #[op]
    fn isqrt(n: BigInt) -> NativeResult<BigInt> {
        if n.is_negative() {
            return Err(NativeError::value_error(
                "isqrt() argument must be nonnegative",
            ));
        }
        if n.is_zero() {
            return Ok(n);
        }
        let mut x = BigInt::from_i64(1).shl(n.bit_len().div_ceil(2) as u64);
        loop {
            let y = x.add(&n.floor_div(&x)).shr(1);
            if y.cmp(&x) != std::cmp::Ordering::Less {
                return Ok(x);
            }
            x = y;
        }
    }

    fn comb_perm(it: &mut Interp, n: &Value, k: Option<&Value>, perm: bool) -> R<BigInt> {
        let n = int_value(&crate::bind::index(it, n)?);
        let k = match k {
            Some(Value::None) | None => n.clone(),
            Some(v) => int_value(&crate::bind::index(it, v)?),
        };
        if n.is_negative() {
            return Err(it.value_error("n must be a non-negative integer"));
        }
        if k.is_negative() {
            return Err(it.value_error("k must be a non-negative integer"));
        }
        if k.cmp(&n) == std::cmp::Ordering::Greater {
            return Ok(BigInt::from_i64(0));
        }
        let k = if perm {
            k
        } else {
            let rest = n.sub(&k);
            if rest.cmp(&k) == std::cmp::Ordering::Less {
                rest
            } else {
                k
            }
        };
        let Some(k) = k.to_i64() else {
            let msg = if perm {
                "k must not exceed 9223372036854775807"
            } else {
                "min(n - k, k) must not exceed 9223372036854775807"
            };
            return Err(it.overflow_err(msg));
        };
        let (nbits, kf) = (n.bit_len() as f64, k as f64);
        let bits = if perm {
            kf * nbits
        } else {
            kf * (nbits - kf.max(1.0).log2() + std::f64::consts::LOG2_E)
        };
        it.check_int_bits(bits.max(0.0) as u128)?;
        let mut acc = BigInt::from_i64(1);
        for i in 0..k {
            if i & 0xff == 0 {
                it.poll()?;
            }
            acc = acc.mul(&n.sub(&BigInt::from_i64(i)));
            if !perm {
                acc = acc.floor_div(&BigInt::from_i64(i + 1));
            }
        }
        Ok(acc)
    }

    #[op]
    fn comb(it: &mut Interp, n: &Value, k: &Value) -> R<BigInt> {
        comb_perm(it, n, Some(k), false)
    }

    #[op]
    fn perm(it: &mut Interp, n: &Value, k: Option<&Value>) -> R<BigInt> {
        comb_perm(it, n, k, true)
    }

    #[op]
    fn fsum(it: &mut Interp, seq: &Value) -> R<f64> {
        let iter = it.get_iter(seq)?;
        let mut sum = lf::Fsum::new();
        while let Some(v) = it.iter_next(&iter)? {
            sum.add(it.float_arg(&v)?);
        }
        sum.result().map_err(|e| match e {
            lf::FsumError::Overflow => it.overflow_err("intermediate overflow in fsum"),
            lf::FsumError::InfMinusInf => it.value_error("-inf + inf in fsum"),
        })
    }

    #[op]
    fn prod(
        it: &mut Interp,
        iterable: &Value,
        #[kwonly]
        #[default(1)]
        start: Value,
    ) -> R<Value> {
        let items = it.iterate_to_vec(iterable)?;
        let mut acc = start;
        for v in items {
            acc = it.binary_op(BinOp::Mult, &acc, &v)?;
        }
        Ok(acc)
    }

    #[op]
    fn dist(it: &mut Interp, p: &Value, q: &Value) -> R<f64> {
        let p = it.iterate_to_vec(p)?;
        let q = it.iterate_to_vec(q)?;
        if p.len() != q.len() {
            return Err(it.value_error("both points must have the same number of dimensions"));
        }
        let mut diffs = Vec::with_capacity(p.len());
        let (mut max, mut nan) = (0.0f64, false);
        for (x, y) in p.iter().zip(q.iter()) {
            let d = (it.float_arg(x)? - it.float_arg(y)?).abs();
            nan |= d.is_nan();
            if d > max {
                max = d;
            }
            diffs.push(d);
        }
        Ok(lf::vector_norm(&mut diffs, max, nan))
    }

    #[op]
    fn gamma(x: f64) -> NativeResult<f64> {
        lf::tgamma(x).map_err(math_err)
    }

    #[op]
    fn lgamma(x: f64) -> NativeResult<f64> {
        lf::lgamma(x).map_err(math_err)
    }
}
