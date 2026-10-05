//! The `math` module.

/// This module provides access to the mathematical functions
/// defined by the C standard.
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

    /// Return the sine of x (measured in radians).
    #[op]
    fn sin(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::sin(x), false)
    }

    /// Return the cosine of x (measured in radians).
    #[op]
    fn cos(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::cos(x), false)
    }

    /// Return the tangent of x (measured in radians).
    #[op]
    fn tan(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::tan(x), false)
    }

    /// Return the arc sine (measured in radians) of x.
    ///
    /// The result is between -pi/2 and pi/2.
    #[op]
    fn asin(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::asin(x), false)
    }

    /// Return the arc cosine (measured in radians) of x.
    ///
    /// The result is between 0 and pi.
    #[op]
    fn acos(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::acos(x), false)
    }

    /// Return the arc tangent (measured in radians) of x.
    ///
    /// The result is between -pi/2 and pi/2.
    #[op]
    fn atan(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::atan(x), false)
    }

    /// Return the hyperbolic sine of x.
    #[op]
    fn sinh(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::sinh(x), true)
    }

    /// Return the hyperbolic cosine of x.
    #[op]
    fn cosh(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::cosh(x), true)
    }

    /// Return the hyperbolic tangent of x.
    #[op]
    fn tanh(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::tanh(x), false)
    }

    /// Return the inverse hyperbolic sine of x.
    #[op]
    fn asinh(x: f64) -> NativeResult<f64> {
        math_1(x, lf::asinh(x), false)
    }

    /// Return the inverse hyperbolic cosine of x.
    #[op]
    fn acosh(x: f64) -> NativeResult<f64> {
        math_1(x, lf::acosh(x), false)
    }

    /// Return the inverse hyperbolic tangent of x.
    #[op]
    fn atanh(x: f64) -> NativeResult<f64> {
        math_1(x, lf::atanh(x), false)
    }

    /// Return e raised to the power of x.
    #[op]
    fn exp(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::exp(x), true)
    }

    /// Return exp(x)-1.
    ///
    /// This function avoids the loss of precision involved in the direct evaluation of exp(x)-1 for small x.
    #[op]
    fn expm1(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::exp_m1(x), true)
    }

    /// Return 2 raised to the power of x.
    #[op]
    fn exp2(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::exp2(x), true)
    }

    /// Return the absolute value of the float x.
    #[op]
    fn fabs(x: f64) -> f64 {
        f64::abs(x)
    }

    /// Convert angle x from radians to degrees.
    #[op]
    fn degrees(x: f64) -> f64 {
        f64::to_degrees(x)
    }

    /// Convert angle x from degrees to radians.
    #[op]
    fn radians(x: f64) -> f64 {
        f64::to_radians(x)
    }

    /// Error function at x.
    #[op]
    fn erf(x: f64) -> NativeResult<f64> {
        math_1(x, lf::erf(x), false)
    }

    /// Complementary error function at x.
    #[op]
    fn erfc(x: f64) -> NativeResult<f64> {
        math_1(x, lf::erfc(x), false)
    }

    /// Return the square root of x.
    #[op]
    fn sqrt(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::sqrt(x), false)
    }

    /// Return the cube root of x.
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

    /// log(x, [base=math.e])
    /// Return the logarithm of x to the given base.
    ///
    /// If the base is not specified, returns the natural logarithm (base e) of x.
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

    /// Return the base 2 logarithm of x.
    #[op]
    fn log2(it: &mut Interp, x: &Value) -> R<f64> {
        loghelper(it, x, fmath::log2)
    }

    /// Return the base 10 logarithm of x.
    #[op]
    fn log10(it: &mut Interp, x: &Value) -> R<f64> {
        loghelper(it, x, fmath::log10)
    }

    /// Return the natural logarithm of 1+x (base e).
    ///
    /// The result is computed in a way which is accurate for x near zero.
    #[op]
    fn log1p(x: f64) -> NativeResult<f64> {
        math_1(x, fmath::ln_1p(x), false)
    }

    /// Return x**y (x to the power of y).
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

    /// Return the arc tangent (measured in radians) of y/x.
    ///
    /// Unlike atan(y/x), the signs of both x and y are considered.
    #[op]
    fn atan2(y: f64, x: f64) -> f64 {
        fmath::atan2(y, x)
    }

    /// Multidimensional Euclidean distance from the origin to a point.
    ///
    /// Roughly equivalent to:
    ///     sqrt(sum(x**2 for x in coordinates))
    ///
    /// For a two dimensional point (x, y), gives the hypotenuse
    /// using the Pythagorean theorem:  sqrt(x*x + y*y).
    ///
    /// For example, the hypotenuse of a 3/4/5 right triangle is:
    ///
    ///     >>> hypot(3.0, 4.0)
    ///     5.0
    #[op(hint(py(text_signature = "")))]
    fn hypot(it: &mut Interp, #[varargs] coordinates: &[Value]) -> R<f64> {
        let mut vals = Vec::with_capacity(coordinates.len());
        for v in coordinates {
            vals.push(it.float_arg(v)?);
        }
        Ok(lf::hypot(&mut vals))
    }

    /// Return a float with the magnitude (absolute value) of x but the sign of y.
    ///
    /// On platforms that support signed zeros, copysign(1.0, -0.0)
    /// returns -1.0.
    #[op]
    fn copysign(x: f64, y: f64) -> f64 {
        x.copysign(y)
    }

    /// Fused multiply-add operation.
    ///
    /// Compute (x * y) + z with a single round.
    #[op]
    fn fma(x: f64, y: f64, z: f64) -> NativeResult<f64> {
        let r = x.mul_add(y, z);
        if r.is_nan() && !x.is_nan() && !y.is_nan() && !z.is_nan() {
            return Err(NativeError::value_error("invalid operation in fma"));
        }
        if r.is_infinite() && x.is_finite() && y.is_finite() && z.is_finite() {
            return Err(NativeError::overflow("overflow in fma"));
        }
        Ok(r)
    }

    /// Return the floating-point value the given number of steps after x towards y.
    ///
    /// If steps is not specified or is None, it defaults to 1.
    ///
    /// Raises a TypeError, if x or y is not a double, or if steps is not an integer.
    /// Raises ValueError if steps is negative.
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

    /// Return the value of the least significant bit of the float x.
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

    /// Return the sum of products of values from two iterables p and q.
    ///
    /// Roughly equivalent to:
    ///
    ///     sum(map(operator.mul, p, q, strict=True))
    ///
    /// For float and mixed int/float inputs, the intermediate products
    /// and sums are computed with extended precision.
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

    /// Return fmod(x, y), according to platform C.
    ///
    /// x % y may differ.
    #[op]
    fn fmod(x: f64, y: f64) -> NativeResult<f64> {
        if y.is_infinite() && x.is_finite() {
            return Ok(x);
        }
        math_2(x, y, x % y)
    }

    /// Difference between x and the closest integer multiple of y.
    ///
    /// Return x - n*y where n*y is the closest integer multiple of y.
    /// In the case where x is exactly halfway between two multiples of
    /// y, the nearest even value of n is used. The result is always exact.
    #[op]
    fn remainder(x: f64, y: f64) -> NativeResult<f64> {
        math_2(x, y, lf::remainder(x, y))
    }

    /// Return the fractional and integer parts of x.
    ///
    /// Both results carry the sign of x and are floats.
    #[op]
    fn modf(x: f64) -> (f64, f64) {
        if x.is_infinite() {
            return (0.0f64.copysign(x), x);
        }
        let i = fmath::trunc(x);
        ((x - i).copysign(x), i)
    }

    /// Return the mantissa and exponent of x, as pair (m, e).
    ///
    /// m is a float and e is an int, such that x = m * 2.**e.
    /// If x is 0, m and e are both 0.  Else 0.5 <= abs(m) < 1.0.
    #[op]
    fn frexp(x: f64) -> (f64, i64) {
        let (m, e) = lf::frexp(x);
        (m, e as i64)
    }

    /// Return x * (2**i).
    ///
    /// This is essentially the inverse of frexp().
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

    /// Return the floor of x as an Integral.
    ///
    /// This is the largest integer <= x.
    #[op]
    fn floor(it: &mut Interp, x: &Value) -> R<Value> {
        round_op(it, x, "__floor__", fmath::floor)
    }

    /// Return the ceiling of x as an Integral.
    ///
    /// This is the smallest integer >= x.
    #[op]
    fn ceil(it: &mut Interp, x: &Value) -> R<Value> {
        round_op(it, x, "__ceil__", fmath::ceil)
    }

    /// Truncates the Real x to the nearest Integral toward 0.
    ///
    /// Uses the __trunc__ magic method.
    #[op]
    fn trunc(it: &mut Interp, x: &Value) -> R<Value> {
        round_op(it, x, "__trunc__", fmath::trunc)
    }

    /// Return True if x is a NaN (not a number), and False otherwise.
    #[op]
    fn isnan(x: f64) -> bool {
        x.is_nan()
    }

    /// Return True if x is a positive or negative infinity, and False otherwise.
    #[op]
    fn isinf(x: f64) -> bool {
        x.is_infinite()
    }

    /// Return True if x is neither an infinity nor a NaN, and False otherwise.
    #[op]
    fn isfinite(x: f64) -> bool {
        x.is_finite()
    }

    /// Determine whether two floating-point numbers are close in value.
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
    /// For the values to be considered close, the difference between them
    /// must be smaller than at least one of the tolerances.
    ///
    /// -inf, inf and NaN behave similarly to the IEEE 754 Standard.  That
    /// is, NaN is not close to anything, even itself.  inf and -inf are
    /// only close to themselves.
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

    /// Greatest Common Divisor.
    #[op]
    fn gcd(it: &mut Interp, #[varargs] integers: &[Value]) -> R<BigInt> {
        let mut acc = BigInt::from_i64(0);
        for v in integers {
            let b = crate::bind::index(it, v)?;
            acc = acc.gcd(&int_value(&b).abs());
        }
        Ok(acc)
    }

    /// Least Common Multiple.
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

    /// Product of `lo..=hi`, split in halves so the big multiplications stay balanced.
    fn range_product(it: &mut Interp, lo: i64, hi: i64) -> R<BigInt> {
        if lo > hi {
            return Ok(BigInt::from_i64(1));
        }
        if hi - lo < 24 {
            let mut acc = BigInt::from_i64(1);
            let mut small: i64 = 1;
            for i in lo..=hi {
                match small.checked_mul(i) {
                    Some(v) => small = v,
                    None => {
                        acc = acc.mul(&BigInt::from_i64(small));
                        small = i;
                    }
                }
            }
            return Ok(acc.mul(&BigInt::from_i64(small)));
        }
        it.poll()?;
        let mid = lo + (hi - lo) / 2;
        let a = range_product(it, lo, mid)?;
        let b = range_product(it, mid + 1, hi)?;
        Ok(a.mul(&b))
    }

    /// `n * (n-1) * ... * (n-k+1)`, divided by `k!` for combinations; divide and conquer as
    /// `P(n, k) = P(n, j) P(n-j, k-j)` and `C(n, k) = C(n, j) C(n-j, k-j) / C(k, j)`.
    fn perm_comb(it: &mut Interp, n: &BigInt, k: i64, comb: bool) -> R<BigInt> {
        if k == 0 {
            return Ok(BigInt::from_i64(1));
        }
        if k == 1 {
            return Ok(n.clone());
        }
        if k > 64 {
            it.poll()?;
        }
        let j = k / 2;
        let a = perm_comb(it, n, j, comb)?;
        let nj = n.sub(&BigInt::from_i64(j));
        let b = perm_comb(it, &nj, k - j, comb)?;
        let r = a.mul(&b);
        if comb {
            let c = perm_comb(it, &BigInt::from_i64(k), j, true)?;
            return Ok(r.floor_div(&c));
        }
        Ok(r)
    }

    /// Find n!.
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
        range_product(it, 2, n)
    }

    /// Return the integer part of the square root of the input.
    #[op]
    fn isqrt(n: BigInt) -> NativeResult<BigInt> {
        if n.is_negative() {
            return Err(NativeError::value_error(
                "isqrt() argument must be nonnegative",
            ));
        }
        Ok(n.isqrt().expect("checked non-negative"))
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
        perm_comb(it, &n, k, !perm)
    }

    /// Number of ways to choose k items from n items without repetition and without order.
    ///
    /// Evaluates to n! / (k! * (n - k)!) when k <= n and evaluates
    /// to zero when k > n.
    ///
    /// Also called the binomial coefficient because it is equivalent
    /// to the coefficient of k-th term in polynomial expansion of the
    /// expression (1 + x)**n.
    ///
    /// Raises TypeError if either of the arguments are not integers.
    /// Raises ValueError if either of the arguments are negative.
    #[op]
    fn comb(it: &mut Interp, n: &Value, k: &Value) -> R<BigInt> {
        comb_perm(it, n, Some(k), false)
    }

    /// Number of ways to choose k items from n items without repetition and with order.
    ///
    /// Evaluates to n! / (n - k)! when k <= n and evaluates
    /// to zero when k > n.
    ///
    /// If k is not specified or is None, then k defaults to n
    /// and the function returns n!.
    ///
    /// Raises TypeError if either of the arguments are not integers.
    /// Raises ValueError if either of the arguments are negative.
    #[op]
    fn perm(it: &mut Interp, n: &Value, k: Option<&Value>) -> R<BigInt> {
        comb_perm(it, n, k, true)
    }

    /// Return an accurate floating-point sum of values in the iterable seq.
    ///
    /// Assumes IEEE-754 floating-point arithmetic.
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

    /// Calculate the product of all the elements in the input iterable.
    ///
    /// The default start value for the product is 1.
    ///
    /// When the iterable is empty, return the start value.  This function is
    /// intended specifically for use with numeric values and may reject
    /// non-numeric types.
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

    /// Return the Euclidean distance between two points p and q.
    ///
    /// The points should be specified as sequences (or iterables) of
    /// coordinates.  Both inputs must have the same dimension.
    ///
    /// Roughly equivalent to:
    ///     sqrt(sum((px - qx) ** 2.0 for px, qx in zip(p, q)))
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

    /// Gamma function at x.
    #[op]
    fn gamma(x: f64) -> NativeResult<f64> {
        lf::tgamma(x).map_err(math_err)
    }

    /// Natural logarithm of absolute value of Gamma function at x.
    #[op]
    fn lgamma(x: f64) -> NativeResult<f64> {
        lf::lgamma(x).map_err(math_err)
    }
}
