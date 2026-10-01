//! `int`, `bool`, `float`, `complex`: constructors, methods and operator slot wrappers.

use super::funcs::{float_to_int, round_half_even};
use crate::fmath;
use crate::ast::{BinOp, CmpOp};
use crate::pyint::{BigInt, PyInt};
use crate::bytecode::UnOp;
use crate::num::{float_repr, to_num, Num};
use crate::object::*;
use crate::vm::*;
use std::rc::Rc;

type Kw<'a> = &'a [(Obj, Value)];

fn cls_of(it: &mut Interp, a: &[Value], what: &str) -> R<Obj> {
    match a.first() {
        Some(Value::Obj(c)) if matches!(c.kind, Kind::Type(_)) => Ok(c.clone()),
        _ => Err(it.type_error(&format!("{}.__new__(X): X is not a type object", what))),
    }
}

pub fn parse_int_str(s: &str, base: u32) -> Option<BigInt> {
    let t = s.trim();
    let (neg, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let lower = rest.to_ascii_lowercase();
    let mut base = base;
    let mut digits: &str = &lower;
    let prefix = |p: &str| digits.starts_with(p);
    if base == 0 {
        if prefix("0x") {
            base = 16;
            digits = &digits[2..];
        } else if prefix("0o") {
            base = 8;
            digits = &digits[2..];
        } else if prefix("0b") {
            base = 2;
            digits = &digits[2..];
        } else {
            base = 10;
            if digits.len() > 1 && digits.starts_with('0') && digits.chars().any(|c| c != '0' && c != '_') {
                return None;
            }
        }
        if digits.starts_with('_') {
            digits = &digits[1..];
        }
    } else if (base == 16 && prefix("0x")) || (base == 8 && prefix("0o")) || (base == 2 && prefix("0b")) {
        digits = &digits[2..];
        if digits.starts_with('_') {
            digits = &digits[1..];
        }
    }
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") {
        return None;
    }
    let clean: String = digits.chars().filter(|c| *c != '_').collect();
    if !clean.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let v = BigInt::parse_signed(&clean, base)?;
    Some(if neg { v.neg() } else { v })
}

pub fn parse_float_str(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    let (neg, body) = match lower.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, lower.strip_prefix('+').unwrap_or(&lower)),
    };
    let sgn = |f: f64| if neg { -f } else { f };
    match body {
        "inf" | "infinity" => return Some(sgn(f64::INFINITY)),
        "nan" => {
            thread_local!(static NAN_SEQ: std::cell::Cell<u64> = const { std::cell::Cell::new(1) });
            let n = NAN_SEQ.with(|c| {
                let v = c.get();
                c.set(v + 1);
                v
            });
            return Some(f64::from_bits(0x7ff8_0000_0000_0000 | (n << 3)));
        }
        _ => {}
    }
    let b = body.as_bytes();
    if b.is_empty() || !(b[0].is_ascii_digit() || b[0] == b'.') {
        return None;
    }
    let mut clean = String::with_capacity(body.len());
    let chars: Vec<char> = body.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' {
            let prev = i.checked_sub(1).map(|j| chars[j]);
            let next = chars.get(i + 1).copied();
            if !(prev.is_some_and(|p| p.is_ascii_digit()) && next.is_some_and(|n| n.is_ascii_digit())) {
                return None;
            }
        } else if c.is_ascii_digit() || matches!(c, '.' | 'e' | '+' | '-') {
            clean.push(c);
        } else {
            return None;
        }
    }
    clean.parse::<f64>().ok().map(sgn)
}

fn rewrap_int(it: &mut Interp, cls: &Obj, v: Value) -> Value {
    if Rc::ptr_eq(cls, &it.types.int) {
        return v;
    }
    let b = v.as_bigint().unwrap_or_else(BigInt::zero);
    Value::Obj(Object::with_cls(cls.clone(), Kind::Int(b)))
}

fn int_from_value(it: &mut Interp, x: &Value, base: Option<i64>) -> R<Value> {
    if let Some(b) = base {
        if !(b == 0 || (2..=36).contains(&b)) {
            return Err(it.value_error("int() base must be >= 2 and <= 36, or 0"));
        }
        let text = match x {
            Value::Obj(o) => match &o.kind {
                Kind::Str(s) => s.s.to_string(),
                Kind::Bytes(bs) => String::from_utf8_lossy(bs).into_owned(),
                Kind::ByteArray(bs) => String::from_utf8_lossy(&bs.borrow()).into_owned(),
                _ => return Err(it.type_error("int() can't convert non-string with explicit base")),
            },
            _ => return Err(it.type_error("int() can't convert non-string with explicit base")),
        };
        return match parse_int_str(&text, b as u32) {
            Some(v) => Ok(Value::big(v)),
            None => {
                let r = it.repr_of(x)?;
                Err(it.value_error(&format!("invalid literal for int() with base {}: {}", b, r)))
            }
        };
    }
    match x {
        Value::Int(_) => return Ok(x.clone()),
        Value::Bool(b) => return Ok(Value::Int(*b as i64)),
        Value::Float(f) => return float_to_int_checked(it, *f),
        _ => {}
    }
    if let Value::Obj(o) = x {
        match &o.kind {
            Kind::Int(b) => return Ok(Value::big(b.clone())),
            Kind::Float(f) => return float_to_int_checked(it, *f),
            Kind::Str(s) => {
                return match parse_int_str(&s.s, 10) {
                    Some(v) => Ok(Value::big(v)),
                    None => {
                        let r = it.repr_of(x)?;
                        Err(it.value_error(&format!("invalid literal for int() with base 10: {}", r)))
                    }
                }
            }
            Kind::Bytes(_) | Kind::ByteArray(_) => {
                let raw = match &o.kind {
                    Kind::Bytes(bs) => bs.clone(),
                    Kind::ByteArray(bs) => bs.borrow().clone(),
                    _ => Vec::new(),
                };
                let text = String::from_utf8_lossy(&raw).into_owned();
                return match parse_int_str(&text, 10) {
                    Some(v) => Ok(Value::big(v)),
                    None => {
                        let r = it.repr_of(x)?;
                        Err(it.value_error(&format!("invalid literal for int() with base 10: {}", r)))
                    }
                };
            }
            _ => {}
        }
    }
    let cls = it.type_of(x);
    for name in ["__int__", "__index__", "__trunc__"] {
        if let Some(m) = it.lookup_mro(&cls, name) {
            let b = it.bind_descr(&m, x, &cls)?;
            let r = it.call(&b, Vec::new(), Vec::new())?;
            if r.is_int_like() {
                return Ok(match r {
                    Value::Bool(b) => Value::Int(b as i64),
                    other => match other.as_bigint() {
                        Some(b) => Value::big(b),
                        None => other,
                    },
                });
            }
            let t = it.type_name_of(&r);
            let tn = it.type_name_of(x);
            return Err(it.type_error(&format!("{}.{}() returned non-int (type {})", tn, name, t)));
        }
    }
    let t = it.type_name_of(x);
    Err(it.type_error(&format!("int() argument must be a string, a bytes-like object or a real number, not '{}'", t)))
}

fn float_to_int_checked(it: &mut Interp, f: f64) -> R<Value> {
    if f.is_nan() {
        return Err(it.value_error("cannot convert float NaN to integer"));
    }
    if f.is_infinite() {
        return Err(it.overflow_err("cannot convert float infinity to integer"));
    }
    Ok(float_to_int(fmath::trunc(f)))
}

fn int_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let cls = cls_of(it, a, "int")?;
    let b = it.bind_args("int", &a[1..], kw, &["x", "base"], 0)?;
    let v = match &b[0] {
        None => {
            if b[1].is_some() {
                return Err(it.type_error("int() missing string argument"));
            }
            Value::Int(0)
        }
        Some(x) => {
            let base = match &b[1] {
                Some(bs) => Some(it.index_of(bs)?),
                None => None,
            };
            int_from_value(it, x, base)?
        }
    };
    Ok(rewrap_int(it, &cls, v))
}

fn bool_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let _ = cls_of(it, a, "bool")?;
    it.no_kwargs("bool", kw)?;
    it.check_args("bool", &a[1..], 0, 1)?;
    match a.get(1) {
        Some(v) => Ok(Value::Bool(it.truthy(v)?)),
        None => Ok(Value::Bool(false)),
    }
}

fn float_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let cls = cls_of(it, a, "float")?;
    let b = it.bind_args("float", &a[1..], kw, &["x"], 0)?;
    let f = match &b[0] {
        None => 0.0,
        Some(x) => match x {
            Value::Obj(o) if matches!(o.kind, Kind::Str(_)) => {
                let s = x.as_str().unwrap_or("");
                match parse_float_str(s) {
                    Some(f) => f,
                    None => {
                        let r = it.repr_of(x)?;
                        return Err(it.value_error(&format!("could not convert string to float: {}", r)));
                    }
                }
            }
            Value::Obj(o) if matches!(o.kind, Kind::Bytes(_)) => {
                let s = match &o.kind {
                    Kind::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
                    _ => String::new(),
                };
                match parse_float_str(&s) {
                    Some(f) => f,
                    None => {
                        let r = it.repr_of(x)?;
                        return Err(it.value_error(&format!("could not convert string to float: {}", r)));
                    }
                }
            }
            _ => match it.float_arg(x) {
                Ok(f) => f,
                Err(e) => {
                    if it.exc_is(&e, "TypeError") {
                        let t = it.type_name_of(x);
                        return Err(it.type_error(&format!("float() argument must be a string or a real number, not '{}'", t)));
                    }
                    return Err(e);
                }
            },
        },
    };
    if Rc::ptr_eq(&cls, &it.types.float) {
        Ok(Value::Float(f))
    } else {
        Ok(Value::Obj(Object::with_cls(cls, Kind::Float(f))))
    }
}

fn parse_complex(s: &str) -> Option<(f64, f64)> {
    let t = s.trim();
    let t = t.strip_prefix('(').and_then(|x| x.strip_suffix(')')).unwrap_or(t).trim();
    if t.is_empty() {
        return None;
    }
    if let Some(body) = t.strip_suffix(['j', 'J']) {
        let bytes: Vec<char> = body.chars().collect();
        let mut split = None;
        for i in (1..bytes.len()).rev() {
            if (bytes[i] == '+' || bytes[i] == '-') && !matches!(bytes[i - 1], 'e' | 'E') {
                split = Some(i);
                break;
            }
        }
        match split {
            Some(i) => {
                let re: String = bytes[..i].iter().collect();
                let im: String = bytes[i..].iter().collect();
                let imv = if im == "+" || im == "-" { if im == "-" { -1.0 } else { 1.0 } } else { parse_float_str(&im)? };
                Some((parse_float_str(&re)?, imv))
            }
            None => {
                let imv = if body.is_empty() || body == "+" { 1.0 } else if body == "-" { -1.0 } else { parse_float_str(body)? };
                Some((0.0, imv))
            }
        }
    } else {
        parse_float_str(t).map(|f| (f, 0.0))
    }
}

fn complex_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let cls = cls_of(it, a, "complex")?;
    let b = it.bind_args("complex", &a[1..], kw, &["real", "imag"], 0)?;
    let (mut re, mut im) = (0.0, 0.0);
    let parts = |it: &mut Interp, v: &Value| -> R<(f64, f64)> {
        if let Value::Obj(o) = v {
            if let Kind::Complex(r, i) = &o.kind {
                return Ok((*r, *i));
            }
            if let Kind::Str(s) = &o.kind {
                return match parse_complex(&s.s) {
                    Some(p) => Ok(p),
                    None => Err(it.value_error("complex() arg is a malformed string")),
                };
            }
        }
        match it.float_arg(v) {
            Ok(f) => Ok((f, 0.0)),
            Err(_) => {
                let t = it.type_name_of(v);
                Err(it.type_error(&format!("complex() first argument must be a string or a number, not '{}'", t)))
            }
        }
    };
    let mut first_complex = false;
    if let Some(r) = &b[0] {
        let (x, y) = parts(it, r)?;
        first_complex = matches!(r, Value::Obj(o) if matches!(o.kind, Kind::Complex(..) | Kind::Str(_)));
        re = x;
        im = y;
    }
    if let Some(i) = &b[1] {
        let (x, y) = parts(it, i)?;
        if matches!(i, Value::Obj(o) if matches!(o.kind, Kind::Complex(..))) {
            re -= y;
            im += x;
        } else if first_complex {
            im += x;
        } else {
            im = x;
        }
    }
    let kind = Kind::Complex(re, im);
    Ok(Value::Obj(if Rc::ptr_eq(&cls, &it.types.complex) { Object::new(kind) } else { Object::with_cls(cls, kind) }))
}

// ---- int methods -----------------------------------------------------------------------------

fn big_of(it: &mut Interp, v: &Value) -> R<BigInt> {
    match v.as_bigint() {
        Some(b) => Ok(b),
        None => {
            let t = it.type_name_of(v);
            Err(it.type_error(&format!("descriptor requires an 'int' object but received a '{}'", t)))
        }
    }
}

fn int_bit_length(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("int.bit_length", a, 1, 1)?;
    let b = big_of(it, &a[0])?;
    Ok(Value::Int(b.abs().bit_len() as i64))
}

fn int_bit_count(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("int.bit_count", a, 1, 1)?;
    let b = big_of(it, &a[0])?;
    Ok(Value::Int(b.abs().count_ones() as i64))
}

fn int_to_bytes(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("to_bytes", &a[1..], kw, &["length", "byteorder", "signed"], 0)?;
    let n = big_of(it, &a[0])?;
    let len = match &b[0] {
        Some(l) => it.index_of(l)?.max(0) as usize,
        None => 1,
    };
    let big_endian = match &b[1] {
        Some(o) => match o.as_str() {
            Some("big") => true,
            Some("little") => false,
            _ => return Err(it.value_error("byteorder must be either 'little' or 'big'")),
        },
        None => true,
    };
    let signed = match &b[2] {
        Some(s) => it.truthy(s)?,
        None => false,
    };
    if n.is_negative() && !signed {
        return Err(it.overflow_err("can't convert negative int to unsigned"));
    }
    match n.to_py_bytes(len, big_endian, signed) {
        Some(v) => Ok(Value::bytes(v)),
        None => Err(it.overflow_err("int too big to convert")),
    }
}

fn int_from_bytes(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let cls = cls_of(it, a, "int")?;
    let b = it.bind_args("from_bytes", &a[1..], kw, &["bytes", "byteorder", "signed"], 1)?;
    let data = it.bytes_of(&b[0].clone().unwrap_or(Value::None))?;
    let big_endian = match &b[1] {
        Some(o) => match o.as_str() {
            Some("big") => true,
            Some("little") => false,
            _ => return Err(it.value_error("byteorder must be either 'little' or 'big'")),
        },
        None => true,
    };
    let signed = match &b[2] {
        Some(s) => it.truthy(s)?,
        None => false,
    };
    let v = Value::big(BigInt::from_py_bytes(&data, big_endian, signed));
    Ok(rewrap_int(it, &cls, v))
}

fn int_conjugate(_it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(match &a[0] {
        Value::Bool(b) => Value::Int(*b as i64),
        v => v.clone(),
    })
}

fn int_ratio(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let n = int_conjugate(it, a, &[])?;
    Ok(Value::tuple(vec![n, Value::Int(1)]))
}

fn int_is_integer(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Bool(true))
}

fn int_index(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    int_conjugate(it, a, &[])
}

fn int_float(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Float(it.float_arg(&a[0])?))
}

fn num_bool(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Bool(it.truthy(&a[0])?))
}

fn int_round(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.call(&it.builtins_fn("round"), a.to_vec(), Vec::new())
}

impl Interp {
    pub fn builtins_fn(&self, name: &str) -> Value {
        dict_get_str(&self.builtins, name).unwrap_or(Value::None)
    }
}

// ---- float methods -----------------------------------------------------------------------------

fn float_is_integer(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let f = it.float_arg(&a[0])?;
    Ok(Value::Bool(f.is_finite() && f == fmath::trunc(f)))
}

fn float_trunc(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let f = it.float_arg(&a[0])?;
    float_to_int_checked(it, f)
}

fn float_floor(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let f = it.float_arg(&a[0])?;
    float_to_int_checked(it, fmath::floor(f))
}

fn float_ceil(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let f = it.float_arg(&a[0])?;
    float_to_int_checked(it, fmath::ceil(f))
}

fn float_as_ratio(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let f = it.float_arg(&a[0])?;
    if f.is_nan() {
        return Err(it.value_error("cannot convert NaN to integer ratio"));
    }
    if f.is_infinite() {
        return Err(it.overflow_err("cannot convert Infinity to integer ratio"));
    }
    if f == 0.0 {
        return Ok(Value::tuple(vec![Value::Int(0), Value::Int(1)]));
    }
    let bits = f.to_bits();
    let neg = (bits >> 63) != 0;
    let exp = ((bits >> 52) & 0x7ff) as i64;
    let frac = bits & ((1u64 << 52) - 1);
    let (mant, e) = if exp == 0 { (frac, -1074) } else { (frac | (1u64 << 52), exp - 1075) };
    let mut num = BigInt::from_u64(mant);
    let mut den = BigInt::from_i64(1);
    if e >= 0 {
        num = num.shl(e as u64);
    } else {
        den = den.shl((-e) as u64);
    }
    let g = num.gcd(&den);
    num = num.floor_div(&g);
    den = den.floor_div(&g);
    if neg {
        num = num.neg();
    }
    Ok(Value::tuple(vec![Value::big(num), Value::big(den)]))
}

fn float_hex(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let f = it.float_arg(&a[0])?;
    if f.is_nan() || f.is_infinite() {
        return Ok(Value::string(float_repr(f)));
    }
    if f == 0.0 {
        return Ok(Value::str(if f.is_sign_negative() { "-0x0.0p+0" } else { "0x0.0p+0" }));
    }
    let bits = f.to_bits();
    let neg = (bits >> 63) != 0;
    let exp = ((bits >> 52) & 0x7ff) as i64;
    let frac = bits & ((1u64 << 52) - 1);
    let (lead, e) = if exp == 0 { (0, -1022) } else { (1, exp - 1023) };
    let hex = format!("{:013x}", frac);
    Ok(Value::string(format!("{}0x{}.{}p{}{}", if neg { "-" } else { "" }, lead, hex, if e < 0 { '-' } else { '+' }, e.abs())))
}

pub fn parse_hex_float(s: &str) -> Option<f64> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let low = s.to_ascii_lowercase();
    match low.as_str() {
        "inf" | "infinity" => return Some(if neg { f64::NEG_INFINITY } else { f64::INFINITY }),
        "nan" => return Some(f64::NAN),
        _ => {}
    }
    let body = low.strip_prefix("0x").unwrap_or(&low);
    let (mant, exp) = match body.split_once('p') {
        Some((m, e)) => (m, e.parse::<i64>().ok()?),
        None => (body, 0),
    };
    let (ip, fp) = mant.split_once('.').unwrap_or((mant, ""));
    if ip.is_empty() && fp.is_empty() {
        return None;
    }
    let mut v: f64 = 0.0;
    for c in ip.chars() {
        v = v * 16.0 + c.to_digit(16)? as f64;
    }
    let mut scale = 1.0 / 16.0;
    for c in fp.chars() {
        v += c.to_digit(16)? as f64 * scale;
        scale /= 16.0;
    }
    let e = exp.clamp(-3000, 3000) as i32;
    let r = v * fmath::powi(2.0, e.clamp(-1000, 1000)) * fmath::powi(2.0, (e - e.clamp(-1000, 1000)).clamp(-1000, 1000));
    Some(if neg { -r } else { r })
}

fn float_fromhex(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("fromhex", a, 2, 2)?;
    let s = it.str_arg(&a[1], "fromhex() argument")?;
    match parse_hex_float(&s) {
        Some(f) => Ok(Value::Float(f)),
        None => Err(it.value_error("invalid hexadecimal floating-point string")),
    }
}

fn complex_conjugate(_it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    match &a[0] {
        Value::Obj(o) => match &o.kind {
            Kind::Complex(r, i) => Ok(Value::Obj(Object::new(Kind::Complex(*r, -*i)))),
            _ => Ok(a[0].clone()),
        },
        v => Ok(v.clone()),
    }
}

// ---- slot wrappers -------------------------------------------------------------------------------

macro_rules! binop_fns {
    ($($fwd:ident, $rev:ident, $op:expr;)*) => {
        $(
            fn $fwd(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
                it.check_args("binary operator", a, 2, 2)?;
                Ok(it.native_binop($op, &a[0], &a[1])?.unwrap_or(Value::NotImplemented))
            }
            fn $rev(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
                it.check_args("binary operator", a, 2, 2)?;
                Ok(it.native_binop($op, &a[1], &a[0])?.unwrap_or(Value::NotImplemented))
            }
        )*
    };
}

binop_fns! {
    w_add, w_radd, BinOp::Add;
    w_sub, w_rsub, BinOp::Sub;
    w_mul, w_rmul, BinOp::Mult;
    w_matmul, w_rmatmul, BinOp::MatMult;
    w_truediv, w_rtruediv, BinOp::Div;
    w_mod, w_rmod, BinOp::Mod;
    w_pow, w_rpow, BinOp::Pow;
    w_lshift, w_rlshift, BinOp::LShift;
    w_rshift, w_rrshift, BinOp::RShift;
    w_or, w_ror, BinOp::BitOr;
    w_xor, w_rxor, BinOp::BitXor;
    w_and, w_rand, BinOp::BitAnd;
    w_floordiv, w_rfloordiv, BinOp::FloorDiv;
}

macro_rules! cmp_wrappers {
    ($($name:ident, $op:expr;)*) => {
        $(
            fn $name(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
                it.check_args("comparison", a, 2, 2)?;
                Ok(match it.native_compare($op, &a[0], &a[1])? {
                    Some(b) => Value::Bool(b),
                    None => Value::NotImplemented,
                })
            }
        )*
    };
}
cmp_wrappers! {
    c_eq, CmpOp::Eq;
    c_ne, CmpOp::NotEq;
    c_lt, CmpOp::Lt;
    c_le, CmpOp::LtE;
    c_gt, CmpOp::Gt;
    c_ge, CmpOp::GtE;
}

fn u_neg(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.unary_op(UnOp::Neg, &a[0])
}
fn u_pos(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.unary_op(UnOp::Pos, &a[0])
}
fn u_invert(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.unary_op(UnOp::Invert, &a[0])
}
fn u_abs(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let f = it.builtins_fn("abs");
    it.call(&f, vec![a[0].clone()], Vec::new())
}

fn w_hash(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(it.native_hash(&a[0])?))
}

fn w_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::string(it.native_repr(&a[0])?))
}

fn w_format(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__format__", a, 2, 2)?;
    let spec = it.str_arg(&a[1], "format_spec")?;
    Ok(Value::string(it.native_format(&a[0], &spec)?))
}

fn w_divmod(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let f = it.builtins_fn("divmod");
    it.call(&f, vec![a[0].clone(), a[1].clone()], Vec::new())
}

pub fn reg_binops(it: &mut Interp, ty: &Obj, which: &[&str]) {
    let table: &[(&'static str, NativeFn, &'static str, NativeFn)] = &[
        ("__add__", w_add, "__radd__", w_radd),
        ("__sub__", w_sub, "__rsub__", w_rsub),
        ("__mul__", w_mul, "__rmul__", w_rmul),
        ("__matmul__", w_matmul, "__rmatmul__", w_rmatmul),
        ("__truediv__", w_truediv, "__rtruediv__", w_rtruediv),
        ("__mod__", w_mod, "__rmod__", w_rmod),
        ("__pow__", w_pow, "__rpow__", w_rpow),
        ("__lshift__", w_lshift, "__rlshift__", w_rlshift),
        ("__rshift__", w_rshift, "__rrshift__", w_rrshift),
        ("__or__", w_or, "__ror__", w_ror),
        ("__xor__", w_xor, "__rxor__", w_rxor),
        ("__and__", w_and, "__rand__", w_rand),
        ("__floordiv__", w_floordiv, "__rfloordiv__", w_rfloordiv),
    ];
    for (f, ff, r, rf) in table {
        if which.contains(f) {
            it.reg(ty, f, *ff);
        }
        if which.contains(r) {
            it.reg(ty, r, *rf);
        }
    }
}

pub fn reg_compare(it: &mut Interp, ty: &Obj, ordering: bool) {
    it.reg(ty, "__eq__", c_eq);
    it.reg(ty, "__ne__", c_ne);
    if ordering {
        it.reg(ty, "__lt__", c_lt);
        it.reg(ty, "__le__", c_le);
        it.reg(ty, "__gt__", c_gt);
        it.reg(ty, "__ge__", c_ge);
    }
}

fn pow3(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__pow__", a, 2, 3)?;
    if a.len() == 3 && !a[2].is_none() {
        let f = it.builtins_fn("pow");
        return it.call(&f, a.to_vec(), Vec::new());
    }
    Ok(it.native_binop(BinOp::Pow, &a[0], &a[1])?.unwrap_or(Value::NotImplemented))
}

pub fn init(it: &mut Interp) {
    let (int, bool_, float, complex) = (it.types.int.clone(), it.types.bool_.clone(), it.types.float.clone(), it.types.complex.clone());
    it.reg_new(&int, int_new);
    it.reg(&int, "bit_length", int_bit_length);
    it.reg(&int, "bit_count", int_bit_count);
    it.reg(&int, "to_bytes", int_to_bytes);
    it.reg_class(&int, "from_bytes", int_from_bytes);
    it.reg(&int, "conjugate", int_conjugate);
    it.reg(&int, "as_integer_ratio", int_ratio);
    it.reg(&int, "is_integer", int_is_integer);
    it.reg(&int, "__index__", int_index);
    it.reg(&int, "__int__", int_index);
    it.reg(&int, "__trunc__", int_index);
    it.reg(&int, "__floor__", int_index);
    it.reg(&int, "__ceil__", int_index);
    it.reg(&int, "__float__", int_float);
    it.reg(&int, "__bool__", num_bool);
    it.reg(&int, "__round__", int_round);
    it.reg(&int, "__neg__", u_neg);
    it.reg(&int, "__pos__", u_pos);
    it.reg(&int, "__invert__", u_invert);
    it.reg(&int, "__abs__", u_abs);
    it.reg(&int, "__hash__", w_hash);
    it.reg(&int, "__repr__", w_repr);
    it.reg(&int, "__format__", w_format);
    it.reg(&int, "__divmod__", w_divmod);
    it.reg(&int, "__pow__", pow3);
    let all_ops = [
        "__add__", "__radd__", "__sub__", "__rsub__", "__mul__", "__rmul__", "__truediv__", "__rtruediv__", "__mod__", "__rmod__", "__lshift__",
        "__rlshift__", "__rshift__", "__rrshift__", "__or__", "__ror__", "__xor__", "__rxor__", "__and__", "__rand__", "__floordiv__", "__rfloordiv__",
        "__rpow__",
    ];
    reg_binops(it, &int, &all_ops);
    reg_compare(it, &int, true);

    it.reg_new(&bool_, bool_new);
    it.reg(&bool_, "__repr__", w_repr);

    it.reg_new(&float, float_new);
    it.reg(&float, "is_integer", float_is_integer);
    it.reg(&float, "as_integer_ratio", float_as_ratio);
    it.reg(&float, "hex", float_hex);
    it.reg_class(&float, "fromhex", float_fromhex);
    it.reg(&float, "conjugate", int_conjugate);
    it.reg(&float, "__trunc__", float_trunc);
    it.reg(&float, "__int__", float_trunc);
    it.reg(&float, "__floor__", float_floor);
    it.reg(&float, "__ceil__", float_ceil);
    it.reg(&float, "__float__", int_conjugate);
    it.reg(&float, "__bool__", num_bool);
    it.reg(&float, "__round__", int_round);
    it.reg(&float, "__neg__", u_neg);
    it.reg(&float, "__pos__", u_pos);
    it.reg(&float, "__abs__", u_abs);
    it.reg(&float, "__hash__", w_hash);
    it.reg(&float, "__repr__", w_repr);
    it.reg(&float, "__format__", w_format);
    it.reg(&float, "__divmod__", w_divmod);
    it.reg(&float, "__pow__", pow3);
    let float_ops = [
        "__add__", "__radd__", "__sub__", "__rsub__", "__mul__", "__rmul__", "__truediv__", "__rtruediv__", "__mod__", "__rmod__", "__floordiv__",
        "__rfloordiv__", "__rpow__",
    ];
    reg_binops(it, &float, &float_ops);
    reg_compare(it, &float, true);

    it.reg_new(&complex, complex_new);
    it.reg(&complex, "conjugate", complex_conjugate);
    it.reg(&complex, "__neg__", u_neg);
    it.reg(&complex, "__pos__", u_pos);
    it.reg(&complex, "__abs__", u_abs);
    it.reg(&complex, "__hash__", w_hash);
    it.reg(&complex, "__repr__", w_repr);
    it.reg(&complex, "__format__", w_format);
    it.reg(&complex, "__pow__", pow3);
    let complex_ops = ["__add__", "__radd__", "__sub__", "__rsub__", "__mul__", "__rmul__", "__truediv__", "__rtruediv__", "__rpow__"];
    reg_binops(it, &complex, &complex_ops);
    reg_compare(it, &complex, false);
    let _ = (to_num as fn(&Value) -> Option<Num>, round_half_even as fn(f64) -> f64);
}
