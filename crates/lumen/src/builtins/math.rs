//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

pub(crate) fn nf_math_sqrt(i: &mut Interp, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let x = ab(i.to_number(&arg(args, 0)))?;
    Ok(Value::Num(x.sqrt()))
}

// The Math functions the loop JIT inlines are named, so it can recognize the unmodified
// intrinsics by function identity (see `jit_math_fns`).
macro_rules! unary_fn {
    ($name:ident, $f:expr) => {
        pub(crate) fn $name(i: &mut Interp, _this: Value, a: &[Value]) -> Result<Value, Value> {
            let x = ab(i.to_number(&arg(a, 0)))?;
            Ok(Value::Num($f(x)))
        }
    };
}
unary_fn!(nf_math_abs, f64::abs);
unary_fn!(nf_math_floor, f64::floor);
unary_fn!(nf_math_ceil, f64::ceil);
unary_fn!(nf_math_trunc, f64::trunc);
// Math.round ties toward +Inf and keeps a negative sign for [-0.5, 0). Computed from floor(x)
// (not floor(x + 0.5), which wrongly rounds up e.g. 0.5 - ε/4 and some large odd integers).
unary_fn!(nf_math_round, |x: f64| {
    if x.is_nan() || x.is_infinite() || x == 0.0 {
        x
    } else {
        let f = x.floor();
        let r = if x - f >= 0.5 { f + 1.0 } else { f };
        if r == 0.0 && x < 0.0 { -0.0 } else { r }
    }
});

pub(crate) fn nf_math_imul(i: &mut Interp, _this: Value, a: &[Value]) -> Result<Value, Value> {
    let x = to_uint32(ab(i.to_number(&arg(a, 0)))?) as i32;
    let y = to_uint32(ab(i.to_number(&arg(a, 1)))?) as i32;
    Ok(Value::Num(x.wrapping_mul(y) as f64))
}

pub(crate) fn nf_math_max(i: &mut Interp, _this: Value, a: &[Value]) -> Result<Value, Value> {
    // ToNumber every argument (side effects in order, even after a NaN), reducing as we go —
    // no per-call buffer. +0 is larger than -0.
    let mut m = f64::NEG_INFINITY;
    let mut nan = false;
    for v in a {
        let n = match v {
            Value::Num(n) => *n,
            _ => ab(i.to_number(v))?,
        };
        if n.is_nan() {
            nan = true;
        } else if n > m || (n == 0.0 && m == 0.0 && n.is_sign_positive() && m.is_sign_negative()) {
            m = n;
        }
    }
    Ok(Value::Num(if nan { f64::NAN } else { m }))
}

pub(crate) fn nf_math_min(i: &mut Interp, _this: Value, a: &[Value]) -> Result<Value, Value> {
    let mut m = f64::INFINITY;
    let mut nan = false;
    for v in a {
        let n = match v {
            Value::Num(n) => *n,
            _ => ab(i.to_number(v))?,
        };
        if n.is_nan() {
            nan = true;
        } else if n < m || (n == 0.0 && m == 0.0 && n.is_sign_negative() && m.is_sign_positive()) {
            m = n;
        }
    }
    Ok(Value::Num(if nan { f64::NAN } else { m }))
}

/// The Math functions the loop JIT inlines, by property name, as the native fn pointers the
/// intrinsics are installed with.
pub(crate) fn jit_math_fns() -> [(&'static str, crate::value::NativeFn); 9] {
    [
        ("sqrt", nf_math_sqrt),
        ("abs", nf_math_abs),
        ("floor", nf_math_floor),
        ("ceil", nf_math_ceil),
        ("round", nf_math_round),
        ("trunc", nf_math_trunc),
        ("max", nf_math_max),
        ("min", nf_math_min),
        ("imul", nf_math_imul),
    ]
}

pub(super) fn install_math(it: &mut Interp) {
    let math = it.new_object();
    // The Math constants are { writable:false, enumerable:false, configurable:false }.
    for (name, val) in [
        ("E", std::f64::consts::E),
        ("LN10", std::f64::consts::LN_10),
        ("LN2", std::f64::consts::LN_2),
        ("LOG10E", std::f64::consts::LOG10_E),
        ("LOG2E", std::f64::consts::LOG2_E),
        ("PI", std::f64::consts::PI),
        ("SQRT1_2", std::f64::consts::FRAC_1_SQRT_2),
        ("SQRT2", std::f64::consts::SQRT_2),
    ] {
        math.borrow_mut()
            .props
            .insert(name, Property::data(Value::Num(val), false, false, false));
    }
    // Math[@@toStringTag] = "Math" (non-writable, non-enumerable, configurable).
    if let Some(key) = well_known_key(it, "toStringTag") {
        math.borrow_mut()
            .props
            .insert(key, Property::data(Value::str("Math"), false, false, true));
    }
    macro_rules! unary {
        ($name:expr, $f:expr) => {
            it.def_method(&math, $name, 1, |i, _t, a| {
                let x = ab(i.to_number(&arg(a, 0)))?;
                Ok(Value::Num($f(x)))
            });
        };
    }
    it.def_method(&math, "abs", 1, nf_math_abs);
    it.def_method(&math, "floor", 1, nf_math_floor);
    it.def_method(&math, "ceil", 1, nf_math_ceil);
    it.def_method(&math, "round", 1, nf_math_round);
    it.def_method(&math, "trunc", 1, nf_math_trunc);
    it.def_method(&math, "sqrt", 1, nf_math_sqrt);
    unary!("cbrt", f64::cbrt);
    unary!("sign", |x: f64| if x.is_nan() || x == 0.0 {
        x
    } else {
        x.signum()
    });
    unary!("expm1", f64::exp_m1);
    unary!("log1p", f64::ln_1p);
    unary!("sinh", f64::sinh);
    unary!("cosh", f64::cosh);
    unary!("tanh", f64::tanh);
    unary!("asinh", lumen_common::float::asinh);
    unary!("acosh", lumen_common::float::acosh);
    unary!("atanh", lumen_common::float::atanh);
    unary!("fround", |x: f64| x as f32 as f64);
    unary!("f16round", lumen_common::buffer::format::f16_round);
    unary!("clz32", |x: f64| (to_uint32(x)).leading_zeros() as f64);
    it.def_method(&math, "sumPrecise", 1, |i, _t, a| {
        // Iterate the argument, requiring every element to be a Number; compute the correctly
        // rounded sum. Infinities dominate (mixed signs → NaN), any NaN → NaN, empty → -0.
        let (iter, next) = ab(i.get_iterator(&arg(a, 0)))?;
        let mut finite: Vec<f64> = Vec::new();
        let (mut pos_inf, mut neg_inf, mut nan) = (false, false, false);
        loop {
            let item = match ab(i.iterator_step(&iter, &next))? {
                Some(v) => v,
                None => break,
            };
            match item {
                Value::Num(n) => {
                    if n.is_nan() {
                        nan = true;
                    } else if n.is_infinite() {
                        if n > 0.0 {
                            pos_inf = true;
                        } else {
                            neg_inf = true;
                        }
                    } else {
                        finite.push(n);
                    }
                }
                _ => {
                    i.iterator_close(&iter);
                    return Err(i.make_error("TypeError", "Math.sumPrecise: not a number"));
                }
            }
        }
        let result = if pos_inf && neg_inf || nan {
            f64::NAN
        } else if pos_inf {
            f64::INFINITY
        } else if neg_inf {
            f64::NEG_INFINITY
        } else if finite.iter().all(|x| *x == 0.0 && x.is_sign_negative()) {
            -0.0
        } else {
            match lumen_common::float::fsum(&finite) {
                Ok(s) => s,
                Err(_) => {
                    // The exact-summation partials transiently overflowed. Retry on values scaled
                    // by a power of two (exact, so the correctly rounded result is unchanged)
                    // centred near 2^500, then scale back: a genuine overflow becomes ±Infinity.
                    let max_abs = finite.iter().map(|x| x.abs()).fold(0.0_f64, f64::max);
                    let scale_exp = max_abs.log2().floor() as i32 - 500;
                    let down = 2f64.powi(-scale_exp);
                    let up = 2f64.powi(scale_exp);
                    let scaled: Vec<f64> = finite.iter().map(|&x| x * down).collect();
                    lumen_common::float::fsum(&scaled).unwrap_or(f64::NAN) * up
                }
            }
        };
        Ok(Value::Num(result))
    });
    it.def_method(&math, "hypot", 2, |i, _t, a| {
        // Every argument is coerced (in order) before any is inspected.
        let mut coords = Vec::with_capacity(a.len());
        for v in a {
            coords.push(ab(i.to_number(v))?);
        }
        Ok(Value::Num(lumen_common::float::hypot(&mut coords)))
    });
    it.def_method(&math, "imul", 2, nf_math_imul);
    it.def_method(&math, "random", 0, |_i, _t, _a| {
        Ok(Value::Num(next_random()))
    });
    unary!("log", f64::ln);
    unary!("log2", f64::log2);
    unary!("log10", f64::log10);
    unary!("exp", f64::exp);
    unary!("sin", f64::sin);
    unary!("cos", f64::cos);
    unary!("tan", f64::tan);
    unary!("atan", f64::atan);
    unary!("asin", f64::asin);
    unary!("acos", f64::acos);
    it.def_method(&math, "pow", 2, |i, _t, a| {
        let base = ab(i.to_number(&arg(a, 0)))?;
        let exp = ab(i.to_number(&arg(a, 1)))?;
        // Number::exponentiate special cases Rust's powf doesn't share: a NaN exponent is NaN even
        // for base 1, and a base of ±1 with an infinite exponent is NaN.
        let r = if exp.is_nan() || (base.abs() == 1.0 && exp.is_infinite()) {
            f64::NAN
        } else {
            base.powf(exp)
        };
        Ok(Value::Num(r))
    });
    it.def_method(&math, "atan2", 2, |i, _t, a| {
        Ok(Value::Num(
            ab(i.to_number(&arg(a, 0)))?.atan2(ab(i.to_number(&arg(a, 1)))?),
        ))
    });
    it.def_method(&math, "max", 2, nf_math_max);
    it.def_method(&math, "min", 2, nf_math_min);
    set_to_string_tag(it, &math, "Math");
    set_builtin(&it.global, "Math", Value::Obj(math));
}
