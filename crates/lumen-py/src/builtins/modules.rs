//! Native modules: `sys`, `math` and `time`.

use super::file::new_file;
use crate::ast::BinOp;
use crate::pyint::{BigInt, PyInt};
use crate::object::*;
use crate::vm::*;

type Kw<'a> = &'a [(Obj, Value)];

pub fn builtin_module(it: &mut Interp, name: &str) -> Option<Obj> {
    match name {
        "sys" => Some(make_sys(it)),
        "math" => Some(make_math(it)),
        "time" => Some(make_time(it)),
        _ => None,
    }
}

pub fn init(it: &mut Interp) {
    let sys = make_sys(it);
    it.register_module("sys", &sys);
}

fn set_fn(it: &mut Interp, d: &Obj, name: &'static str, f: NativeFn) {
    let v = it.new_native(name, f, false);
    dict_set_str(d, name, v);
}

fn make_sys(it: &mut Interp) -> Obj {
    if let Some(Value::Obj(m)) = dict_get_str(&it.modules, "sys") {
        return m;
    }
    let m = it.new_module("sys");
    let d = it.module_dict(&m);
    it.sys_module = Some(m.clone());
    dict_set_str(&d, "modules", Value::Obj(it.modules.clone()));
    dict_set_str(&d, "argv", Value::list(vec![Value::str("")]));
    dict_set_str(&d, "path", Value::list(Vec::new()));
    dict_set_str(&d, "maxsize", Value::Int(i64::MAX));
    dict_set_str(&d, "maxunicode", Value::Int(0x10ffff));
    dict_set_str(&d, "byteorder", Value::str("little"));
    dict_set_str(&d, "version", Value::str("3.14.0 (lumen-py)"));
    dict_set_str(&d, "version_info", Value::tuple(vec![Value::Int(3), Value::Int(14), Value::Int(0), Value::str("final"), Value::Int(0)]));
    dict_set_str(&d, "hexversion", Value::Int(0x030e00f0));
    let platform = if cfg!(target_os = "macos") { "darwin" } else { std::env::consts::OS };
    dict_set_str(&d, "platform", Value::str(platform));
    dict_set_str(&d, "executable", Value::str("lumen-py"));
    dict_set_str(&d, "builtin_module_names", Value::tuple(vec![Value::str("sys"), Value::str("math"), Value::str("time")]));
    dict_set_str(&d, "stdout", new_file(it, FileMode::Stdout, true, "<stdout>"));
    dict_set_str(&d, "stderr", new_file(it, FileMode::Stderr, true, "<stderr>"));
    dict_set_str(&d, "stdin", new_file(it, FileMode::Stdin, true, "<stdin>"));
    set_fn(it, &d, "exit", sys_exit);
    set_fn(it, &d, "getrecursionlimit", sys_getrecursionlimit);
    set_fn(it, &d, "setrecursionlimit", sys_setrecursionlimit);
    set_fn(it, &d, "exc_info", sys_exc_info);
    set_fn(it, &d, "getsizeof", sys_getsizeof);
    set_fn(it, &d, "intern", sys_intern);
    set_fn(it, &d, "get_int_max_str_digits", sys_get_int_max_str_digits);
    set_fn(it, &d, "set_int_max_str_digits", sys_set_int_max_str_digits);
    let flags = make_record(
        it,
        "flags",
        &[
            ("debug", Value::Int(0)),
            ("inspect", Value::Int(0)),
            ("interactive", Value::Int(0)),
            ("optimize", Value::Int(0)),
            ("dont_write_bytecode", Value::Int(1)),
            ("no_user_site", Value::Int(0)),
            ("no_site", Value::Int(0)),
            ("ignore_environment", Value::Int(0)),
            ("verbose", Value::Int(0)),
            ("bytes_warning", Value::Int(0)),
            ("quiet", Value::Int(0)),
            ("hash_randomization", Value::Int(1)),
            ("isolated", Value::Int(0)),
            ("dev_mode", Value::Bool(false)),
            ("utf8_mode", Value::Int(1)),
            ("int_max_str_digits", Value::Int(it.int_max_str_digits() as i64)),
        ],
    );
    let int_info = make_record(
        it,
        "int_info",
        &[
            ("bits_per_digit", Value::Int(30)),
            ("sizeof_digit", Value::Int(4)),
            ("default_max_str_digits", Value::Int(crate::limits::DEFAULT_INT_MAX_STR_DIGITS as i64)),
            ("str_digits_check_threshold", Value::Int(crate::limits::INT_MAX_STR_DIGITS_THRESHOLD as i64)),
        ],
    );
    dict_set_str(&d, "flags", flags);
    dict_set_str(&d, "int_info", int_info);
    m
}

/// An instance of a fresh class `name` in module `sys` carrying `fields` as attributes.
fn make_record(it: &mut Interp, name: &str, fields: &[(&str, Value)]) -> Value {
    let ns = Value::dict(crate::dict::PyDict::new());
    if let Value::Obj(o) = &ns {
        dict_set_str(o, "__module__", Value::str("sys"));
    }
    let ty = Value::Obj(it.types.type_.clone());
    let Ok(cls) = it.call(&ty, vec![Value::str(name), Value::tuple(Vec::new()), ns], Vec::new()) else { return Value::None };
    let Ok(obj) = it.call(&cls, Vec::new(), Vec::new()) else { return Value::None };
    for (k, v) in fields {
        if let Value::Obj(n) = Value::str(k) {
            let _ = it.set_attr(&obj, &n, v.clone());
        }
    }
    obj
}

fn sys_get_int_max_str_digits(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("get_int_max_str_digits", a, 0, 0)?;
    Ok(Value::Int(it.int_max_str_digits() as i64))
}

fn sys_set_int_max_str_digits(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("set_int_max_str_digits", a, kw, &["maxdigits"], 1)?;
    let n = it.index_of(b[0].as_ref().unwrap_or(&Value::None))?;
    if n < 0 || !it.set_int_max_str_digits(n as usize) {
        return Err(it.value_error(&format!("maxdigits must be >= {} or 0 for unlimited", crate::limits::INT_MAX_STR_DIGITS_THRESHOLD)));
    }
    Ok(Value::None)
}

fn sys_exit(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("exit", a, 0, 1)?;
    let cls = it.exc_type("SystemExit");
    Err(it.new_exc(&cls, a.to_vec()))
}

fn sys_getrecursionlimit(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(it.recursion_limit as i64))
}

fn sys_setrecursionlimit(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("setrecursionlimit", a, 1, 1)?;
    let n = it.index_of(&a[0])?;
    if n < 1 {
        return Err(it.value_error("recursion limit must be greater or equal than 1"));
    }
    it.recursion_limit = (n as usize).min(200_000);
    Ok(Value::None)
}

fn sys_exc_info(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(match it.handled.clone() {
        Some(e) => {
            let t = Value::Obj(it.type_of_obj(&e));
            let tb = match &e.kind {
                Kind::Exception(d) => it.make_tb(&d.borrow().tb),
                _ => Value::None,
            };
            Value::tuple(vec![t, Value::Obj(e), tb])
        }
        None => Value::tuple(vec![Value::None, Value::None, Value::None]),
    })
}

fn sys_getsizeof(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(64))
}

fn sys_intern(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("intern", a, 1, 1)?;
    Ok(a[0].clone())
}

// ---- math ---------------------------------------------------------------------------------------

fn domain(it: &mut Interp) -> Obj {
    it.value_error("math domain error")
}

fn range_err(it: &mut Interp) -> Obj {
    it.new_exc_str("OverflowError", "math range error")
}

fn num(it: &mut Interp, v: &Value) -> R<f64> {
    it.float_arg(v)
}

fn float_checked(it: &mut Interp, x: f64, input_finite: bool) -> R<Value> {
    if x.is_nan() && input_finite {
        return Err(domain(it));
    }
    if x.is_infinite() && input_finite {
        return Err(range_err(it));
    }
    Ok(Value::Float(x))
}

macro_rules! unary {
    ($name:ident, $pyname:expr, $f:expr) => {
        fn $name(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
            it.check_args($pyname, a, 1, 1)?;
            let x = num(it, &a[0])?;
            let f: fn(f64) -> f64 = $f;
            float_checked(it, f(x), x.is_finite())
        }
    };
}

unary!(m_sin, "sin", f64::sin);
unary!(m_cos, "cos", f64::cos);
unary!(m_tan, "tan", f64::tan);
unary!(m_asin, "asin", f64::asin);
unary!(m_acos, "acos", f64::acos);
unary!(m_atan, "atan", f64::atan);
unary!(m_sinh, "sinh", f64::sinh);
unary!(m_cosh, "cosh", f64::cosh);
unary!(m_tanh, "tanh", f64::tanh);
unary!(m_asinh, "asinh", f64::asinh);
unary!(m_acosh, "acosh", f64::acosh);
unary!(m_atanh, "atanh", f64::atanh);
unary!(m_exp, "exp", f64::exp);
unary!(m_expm1, "expm1", f64::exp_m1);
unary!(m_fabs, "fabs", f64::abs);
unary!(m_degrees, "degrees", f64::to_degrees);
unary!(m_radians, "radians", f64::to_radians);
unary!(m_erf, "erf", erf);
unary!(m_erfc, "erfc", |x| 1.0 - erf(x));

fn erf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.3275911 * x.abs());
    let y = 1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * (-x * x).exp();
    if x >= 0.0 {
        y
    } else {
        -y
    }
}

fn m_sqrt(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("sqrt", a, 1, 1)?;
    let x = num(it, &a[0])?;
    if x < 0.0 {
        return Err(domain(it));
    }
    Ok(Value::Float(x.sqrt()))
}

fn ln_of(it: &mut Interp, v: &Value) -> R<f64> {
    if let Value::Obj(o) = v {
        if let Kind::Int(b) = &o.kind {
            if b.is_negative() {
                return Err(domain(it));
            }
            let bits = b.bit_len() as i64;
            if bits > 1000 {
                let shift = (bits - 1000) as usize;
                let top = b.shr(shift as u64).to_float().unwrap_or(f64::INFINITY);
                return Ok(top.ln() + shift as f64 * std::f64::consts::LN_2);
            }
        }
    }
    let x = num(it, v)?;
    if x <= 0.0 {
        return Err(domain(it));
    }
    Ok(x.ln())
}

fn m_log(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("log", a, 1, 2)?;
    let x = ln_of(it, &a[0])?;
    match a.get(1) {
        Some(b) => {
            let base = ln_of(it, b)?;
            if base == 0.0 {
                return Err(it.new_exc_str("ZeroDivisionError", "division by zero"));
            }
            Ok(Value::Float(x / base))
        }
        None => Ok(Value::Float(x)),
    }
}

fn m_log2(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("log2", a, 1, 1)?;
    if let Value::Int(i) = &a[0] {
        if *i > 0 && (*i & (*i - 1)) == 0 {
            return Ok(Value::Float(i.trailing_zeros() as f64));
        }
    }
    let x = ln_of(it, &a[0])?;
    Ok(Value::Float(x / std::f64::consts::LN_2))
}

fn m_log10(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("log10", a, 1, 1)?;
    if let Value::Int(i) = &a[0] {
        if *i > 0 {
            return Ok(Value::Float((*i as f64).log10()));
        }
    }
    let x = ln_of(it, &a[0])?;
    Ok(Value::Float(x / std::f64::consts::LN_10))
}

fn m_log1p(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("log1p", a, 1, 1)?;
    let x = num(it, &a[0])?;
    if x <= -1.0 {
        return Err(domain(it));
    }
    Ok(Value::Float(x.ln_1p()))
}

fn m_pow(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("pow", a, 2, 2)?;
    let x = num(it, &a[0])?;
    let y = num(it, &a[1])?;
    let r = x.powf(y);
    if r.is_nan() && !x.is_nan() && !y.is_nan() {
        return Err(domain(it));
    }
    if r.is_infinite() && x.is_finite() && y.is_finite() {
        if x == 0.0 {
            return Err(domain(it));
        }
        return Err(range_err(it));
    }
    Ok(Value::Float(r))
}

fn m_atan2(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("atan2", a, 2, 2)?;
    let y = num(it, &a[0])?;
    let x = num(it, &a[1])?;
    Ok(Value::Float(y.atan2(x)))
}

fn m_hypot(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let mut vals = Vec::new();
    for v in a {
        vals.push(num(it, v)?.abs());
    }
    if vals.iter().any(|v| v.is_infinite()) {
        return Ok(Value::Float(f64::INFINITY));
    }
    let max = vals.iter().cloned().fold(0.0, f64::max);
    if max == 0.0 || max.is_nan() {
        return Ok(Value::Float(max));
    }
    let sum: f64 = vals.iter().map(|v| (v / max) * (v / max)).sum();
    Ok(Value::Float(max * sum.sqrt()))
}

fn m_copysign(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("copysign", a, 2, 2)?;
    let x = num(it, &a[0])?;
    let y = num(it, &a[1])?;
    Ok(Value::Float(x.copysign(y)))
}

fn m_nextafter(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("nextafter", a, 2, 2)?;
    let x = num(it, &a[0])?;
    let y = num(it, &a[1])?;
    if x.is_nan() || y.is_nan() {
        return Ok(Value::Float(f64::NAN));
    }
    if x == y {
        return Ok(Value::Float(y));
    }
    if x == 0.0 {
        let tiny = f64::from_bits(1);
        return Ok(Value::Float(if y > 0.0 { tiny } else { -tiny }));
    }
    let bits = x.to_bits();
    let up = (y > x) == (x > 0.0);
    Ok(Value::Float(f64::from_bits(if up { bits + 1 } else { bits - 1 })))
}

fn m_fmod(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("fmod", a, 2, 2)?;
    let x = num(it, &a[0])?;
    let y = num(it, &a[1])?;
    if y == 0.0 || x.is_infinite() {
        return Err(domain(it));
    }
    Ok(Value::Float(x % y))
}

fn m_remainder(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("remainder", a, 2, 2)?;
    let x = num(it, &a[0])?;
    let y = num(it, &a[1])?;
    if y == 0.0 || x.is_infinite() {
        return Err(domain(it));
    }
    let n = (x / y).round_ties_even();
    Ok(Value::Float(x - n * y))
}

fn m_modf(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("modf", a, 1, 1)?;
    let x = num(it, &a[0])?;
    if x.is_infinite() {
        return Ok(Value::tuple(vec![Value::Float(0.0f64.copysign(x)), Value::Float(x)]));
    }
    let i = x.trunc();
    Ok(Value::tuple(vec![Value::Float((x - i).copysign(x)), Value::Float(i)]))
}

fn m_frexp(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("frexp", a, 1, 1)?;
    let x = num(it, &a[0])?;
    if x == 0.0 || !x.is_finite() {
        return Ok(Value::tuple(vec![Value::Float(x), Value::Int(0)]));
    }
    let mut e = x.abs().log2().floor() as i32 + 1;
    let mut m = x / 2f64.powi(e);
    if m.abs() >= 1.0 {
        m /= 2.0;
        e += 1;
    } else if m.abs() < 0.5 {
        m *= 2.0;
        e -= 1;
    }
    Ok(Value::tuple(vec![Value::Float(m), Value::Int(e as i64)]))
}

fn m_ldexp(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("ldexp", a, 2, 2)?;
    let x = num(it, &a[0])?;
    let e = it.index_of(&a[1])?.clamp(-5000, 5000) as i32;
    let r = x * 2f64.powi(e.clamp(-1000, 1000)) * 2f64.powi((e - e.clamp(-1000, 1000)).clamp(-1000, 1000));
    if r.is_infinite() && x.is_finite() {
        return Err(range_err(it));
    }
    Ok(Value::Float(r))
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

fn round_op(it: &mut Interp, a: &[Value], name: &'static str, f: fn(f64) -> f64) -> R<Value> {
    it.check_args(name, a, 1, 1)?;
    match &a[0] {
        Value::Int(_) | Value::Bool(_) => return it.call_method(&a[0], "__index__", Vec::new()).or_else(|_| Ok(a[0].clone())),
        Value::Obj(o) if matches!(o.kind, Kind::Int(_)) => return Ok(a[0].clone()),
        Value::Float(_) => {}
        Value::Obj(o) if matches!(o.kind, Kind::Float(_)) => {}
        v => {
            let dunder = if name == "floor" { "__floor__" } else if name == "ceil" { "__ceil__" } else { "__trunc__" };
            let cls = it.type_of(v);
            if it.lookup_mro(&cls, dunder).is_some() {
                return it.call_method(v, dunder, Vec::new());
            }
        }
    }
    let x = num(it, &a[0])?;
    f_to_int(it, f(x))
}

fn m_floor(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    round_op(it, a, "floor", f64::floor)
}

fn m_ceil(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    round_op(it, a, "ceil", f64::ceil)
}

fn m_trunc(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    round_op(it, a, "trunc", f64::trunc)
}

fn m_isnan(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("isnan", a, 1, 1)?;
    Ok(Value::Bool(num(it, &a[0])?.is_nan()))
}

fn m_isinf(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("isinf", a, 1, 1)?;
    Ok(Value::Bool(num(it, &a[0])?.is_infinite()))
}

fn m_isfinite(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("isfinite", a, 1, 1)?;
    Ok(Value::Bool(num(it, &a[0])?.is_finite()))
}

fn m_isclose(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("isclose", a, kw, &["a", "b", "rel_tol", "abs_tol"], 2)?;
    let x = num(it, b[0].as_ref().unwrap_or(&Value::None))?;
    let y = num(it, b[1].as_ref().unwrap_or(&Value::None))?;
    let rel = match &b[2] {
        Some(v) => num(it, v)?,
        None => 1e-9,
    };
    let abs = match &b[3] {
        Some(v) => num(it, v)?,
        None => 0.0,
    };
    if rel < 0.0 || abs < 0.0 {
        return Err(it.value_error("tolerances must be non-negative"));
    }
    if x == y {
        return Ok(Value::Bool(true));
    }
    if x.is_infinite() || y.is_infinite() {
        return Ok(Value::Bool(false));
    }
    let diff = (x - y).abs();
    Ok(Value::Bool(diff <= (rel * y).abs() || diff <= (rel * x).abs() || diff <= abs))
}

fn int_of_big(it: &mut Interp, v: &Value) -> R<BigInt> {
    match v {
        Value::Int(i) => Ok(BigInt::from_i64(*i)),
        Value::Bool(b) => Ok(BigInt::from_i64(*b as i64)),
        Value::Obj(o) => match &o.kind {
            Kind::Int(b) => Ok(b.clone()),
            _ => Ok(BigInt::from_i64(it.index_of(v)?)),
        },
        _ => {
            let t = it.type_name_of(v);
            Err(it.type_error(&format!("'{}' object cannot be interpreted as an integer", t)))
        }
    }
}

fn m_gcd(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let mut acc = BigInt::from_i64(0);
    for v in a {
        let b = int_of_big(it, v)?.abs();
        acc = acc.gcd(&b);
    }
    Ok(Value::big(acc))
}

fn m_lcm(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let mut acc = BigInt::from_i64(1);
    for v in a {
        let b = int_of_big(it, v)?.abs();
        if b.is_zero() {
            return Ok(Value::Int(0));
        }
        let g = acc.gcd(&b);
        acc = acc.mul(&b).floor_div(&g);
    }
    Ok(Value::big(acc))
}

fn m_factorial(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("factorial", a, 1, 1)?;
    if matches!(a[0], Value::Float(_)) {
        return Err(it.type_error("'float' object cannot be interpreted as an integer"));
    }
    let n = it.index_of(&a[0])?;
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
    Ok(Value::big(acc.mul(&BigInt::from_i64(small))))
}

fn m_isqrt(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("isqrt", a, 1, 1)?;
    let n = int_of_big(it, &a[0])?;
    if n.is_negative() {
        return Err(it.value_error("isqrt() argument must be nonnegative"));
    }
    if n.is_zero() {
        return Ok(Value::Int(0));
    }
    let mut x = BigInt::from_i64(1).shl(n.bit_len().div_ceil(2) as u64);
    loop {
        let y = x.add(&n.floor_div(&x)).shr(1);
        if y.cmp(&x) != std::cmp::Ordering::Less {
            return Ok(Value::big(x));
        }
        x = y;
    }
}

fn comb_perm(it: &mut Interp, a: &[Value], perm: bool) -> R<Value> {
    it.check_args(if perm { "perm" } else { "comb" }, a, if perm { 1 } else { 2 }, 2)?;
    let n = it.index_of(&a[0])?;
    let k = match a.get(1) {
        Some(Value::None) | None => n,
        Some(v) => it.index_of(v)?,
    };
    if n < 0 {
        return Err(it.value_error("n must be a non-negative integer"));
    }
    if k < 0 {
        return Err(it.value_error("k must be a non-negative integer"));
    }
    if k > n {
        return Ok(Value::Int(0));
    }
    let k = if perm { k } else { k.min(n - k) };
    let (nf, kf) = (n as f64, k as f64);
    let bits = if perm { kf * nf.log2() } else { kf * ((nf / kf.max(1.0)).log2() + std::f64::consts::LOG2_E) };
    it.check_int_bits(bits as u128)?;
    let mut acc = BigInt::from_i64(1);
    for i in 0..k {
        if i & 0xff == 0 {
            it.poll()?;
        }
        acc = acc.mul(&BigInt::from_i64(n - i));
        if !perm {
            acc = acc.floor_div(&BigInt::from_i64(i + 1));
        }
    }
    Ok(Value::big(acc))
}

fn m_comb(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    comb_perm(it, a, false)
}

fn m_perm(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    comb_perm(it, a, true)
}

fn m_fsum(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("fsum", a, 1, 1)?;
    let items = it.iterate_to_vec(&a[0])?;
    let mut partials: Vec<f64> = Vec::new();
    for v in &items {
        let mut x = num(it, v)?;
        let mut i = 0;
        for j in 0..partials.len() {
            let mut y = partials[j];
            if x.abs() < y.abs() {
                std::mem::swap(&mut x, &mut y);
            }
            let hi = x + y;
            let lo = y - (hi - x);
            if lo != 0.0 {
                partials[i] = lo;
                i += 1;
            }
            x = hi;
        }
        partials.truncate(i);
        partials.push(x);
    }
    Ok(Value::Float(partials.iter().sum()))
}

fn m_prod(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("prod", a, kw, &["iterable", "start"], 1)?;
    let items = it.iterate_to_vec(b[0].as_ref().unwrap_or(&Value::None))?;
    let mut acc = b[1].clone().unwrap_or(Value::Int(1));
    for v in items {
        acc = it.binary_op(BinOp::Mult, &acc, &v)?;
    }
    Ok(acc)
}

fn m_dist(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("dist", a, 2, 2)?;
    let p = it.iterate_to_vec(&a[0])?;
    let q = it.iterate_to_vec(&a[1])?;
    if p.len() != q.len() {
        return Err(it.value_error("both points must have the same number of dimensions"));
    }
    let mut s = 0.0;
    for (x, y) in p.iter().zip(q.iter()) {
        let d = num(it, x)? - num(it, y)?;
        s += d * d;
    }
    Ok(Value::Float(s.sqrt()))
}

fn m_cbrt(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("cbrt", a, 1, 1)?;
    Ok(Value::Float(num(it, &a[0])?.cbrt()))
}

fn m_exp2(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("exp2", a, 1, 1)?;
    let x = num(it, &a[0])?;
    float_checked(it, x.exp2(), x.is_finite())
}

fn m_gamma(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("gamma", a, 1, 1)?;
    let x = num(it, &a[0])?;
    if x == x.floor() && x <= 0.0 {
        return Err(domain(it));
    }
    if x == x.floor() && x < 171.0 {
        let mut r = 1.0;
        for i in 2..(x as i64) {
            r *= i as f64;
        }
        return Ok(Value::Float(r));
    }
    Ok(Value::Float(gamma(x)))
}

fn gamma(x: f64) -> f64 {
    if x < 0.5 {
        return std::f64::consts::PI / ((std::f64::consts::PI * x).sin() * gamma(1.0 - x));
    }
    let g = 7.0;
    let c = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    let x = x - 1.0;
    let mut a = c[0];
    let t = x + g + 0.5;
    for (i, ci) in c.iter().enumerate().skip(1) {
        a += ci / (x + i as f64);
    }
    (2.0 * std::f64::consts::PI).sqrt() * t.powf(x + 0.5) * (-t).exp() * a
}

fn m_lgamma(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("lgamma", a, 1, 1)?;
    let x = num(it, &a[0])?;
    Ok(Value::Float(gamma(x).abs().ln()))
}

fn make_math(it: &mut Interp) -> Obj {
    let m = it.new_module("math");
    let d = it.module_dict(&m);
    dict_set_str(&d, "pi", Value::Float(std::f64::consts::PI));
    dict_set_str(&d, "e", Value::Float(std::f64::consts::E));
    dict_set_str(&d, "tau", Value::Float(std::f64::consts::TAU));
    dict_set_str(&d, "inf", Value::Float(f64::INFINITY));
    dict_set_str(&d, "nan", Value::Float(f64::NAN));
    let fns: &[(&'static str, NativeFn)] = &[
        ("sin", m_sin),
        ("cos", m_cos),
        ("tan", m_tan),
        ("asin", m_asin),
        ("acos", m_acos),
        ("atan", m_atan),
        ("atan2", m_atan2),
        ("sinh", m_sinh),
        ("cosh", m_cosh),
        ("tanh", m_tanh),
        ("asinh", m_asinh),
        ("acosh", m_acosh),
        ("atanh", m_atanh),
        ("exp", m_exp),
        ("expm1", m_expm1),
        ("exp2", m_exp2),
        ("fabs", m_fabs),
        ("degrees", m_degrees),
        ("radians", m_radians),
        ("erf", m_erf),
        ("erfc", m_erfc),
        ("sqrt", m_sqrt),
        ("cbrt", m_cbrt),
        ("log", m_log),
        ("log2", m_log2),
        ("log10", m_log10),
        ("log1p", m_log1p),
        ("pow", m_pow),
        ("hypot", m_hypot),
        ("copysign", m_copysign),
        ("nextafter", m_nextafter),
        ("fmod", m_fmod),
        ("remainder", m_remainder),
        ("modf", m_modf),
        ("frexp", m_frexp),
        ("ldexp", m_ldexp),
        ("floor", m_floor),
        ("ceil", m_ceil),
        ("trunc", m_trunc),
        ("isnan", m_isnan),
        ("isinf", m_isinf),
        ("isfinite", m_isfinite),
        ("isclose", m_isclose),
        ("gcd", m_gcd),
        ("lcm", m_lcm),
        ("factorial", m_factorial),
        ("isqrt", m_isqrt),
        ("comb", m_comb),
        ("perm", m_perm),
        ("fsum", m_fsum),
        ("prod", m_prod),
        ("dist", m_dist),
        ("gamma", m_gamma),
        ("lgamma", m_lgamma),
    ];
    for (n, f) in fns {
        set_fn(it, &d, n, *f);
    }
    m
}

// ---- time ---------------------------------------------------------------------------------------

fn t_time(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    Ok(Value::Float(d.as_secs_f64()))
}

fn t_time_ns(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    Ok(Value::Int(d.as_nanos() as i64))
}

fn t_perf(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Float(it.start.elapsed().as_secs_f64() + 1000.0))
}

fn t_perf_ns(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(it.start.elapsed().as_nanos() as i64 + 1_000_000_000_000))
}

fn t_sleep(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("sleep", a, 1, 1)?;
    let s = num(it, &a[0])?;
    if s < 0.0 {
        return Err(it.value_error("sleep length must be non-negative"));
    }
    it.flush_out();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(s.min(1e9));
    loop {
        it.poll()?;
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Ok(Value::None);
        }
        std::thread::sleep(left.min(std::time::Duration::from_millis(20)));
    }
}

fn make_time(it: &mut Interp) -> Obj {
    let m = it.new_module("time");
    let d = it.module_dict(&m);
    let fns: &[(&'static str, NativeFn)] = &[
        ("time", t_time),
        ("time_ns", t_time_ns),
        ("perf_counter", t_perf),
        ("monotonic", t_perf),
        ("process_time", t_perf),
        ("perf_counter_ns", t_perf_ns),
        ("monotonic_ns", t_perf_ns),
        ("sleep", t_sleep),
    ];
    for (n, f) in fns {
        set_fn(it, &d, n, *f);
    }
    m
}
