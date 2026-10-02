//! `int`, `bool`, `float`, `complex`: constructors, methods and operator slot wrappers.

use super::funcs::{float_to_int, round_half_even};
use crate::fmath;
use crate::ast::BinOp;
use crate::pyint::{BigInt, PyInt};
use crate::bytecode::UnOp;
use crate::num::{float_repr, to_num, Num};
use super::slots::numeric_wider;
use crate::bind::{PyCx, PyHost, This};
use crate::object::*;
use crate::vm::*;
use lumen_bind::{FromArg, Passed, Slot};
use std::rc::Rc;

pub enum IntParseError {
    Invalid,
    /// More digits than `sys.get_int_max_str_digits()` allows; carries the digit count.
    TooManyDigits(usize),
}

/// Parses `s` as `int(s, base)` would; `max_digits` of 0 means no limit.
pub fn parse_int_str(s: &str, base: u32, max_digits: usize) -> Result<BigInt, IntParseError> {
    parse_int_inner(s, base, max_digits).ok_or(IntParseError::Invalid).and_then(|r| r)
}

fn parse_int_inner(s: &str, base: u32, max_digits: usize) -> Option<Result<BigInt, IntParseError>> {
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
    if !base.is_power_of_two() && lumen_common::bigint::digits_exceed(max_digits, clean.len()) {
        return Some(Err(IntParseError::TooManyDigits(clean.len())));
    }
    let v = BigInt::parse_signed(&clean, base)?;
    Some(Ok(if neg { v.neg() } else { v }))
}

fn parse_int_checked(it: &mut Interp, text: &str, base: u32) -> R<Option<BigInt>> {
    match parse_int_str(text, base, it.int_max_str_digits()) {
        Ok(v) => Ok(Some(v)),
        Err(IntParseError::Invalid) => Ok(None),
        Err(IntParseError::TooManyDigits(n)) => {
            it.check_parse_digits(n)?;
            Ok(None)
        }
    }
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
            return Some(sgn(f64::from_bits(0x7ff8_0000_0000_0000 | (n << 3))));
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
                Kind::ByteArray(bs) => String::from_utf8_lossy(&bs.bytes()).into_owned(),
                _ => return Err(it.type_error("int() can't convert non-string with explicit base")),
            },
            _ => return Err(it.type_error("int() can't convert non-string with explicit base")),
        };
        return match parse_int_checked(it, &text, b as u32)? {
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
                return match parse_int_checked(it, &s.s, 10)? {
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
                    Kind::ByteArray(bs) => bs.to_vec(),
                    _ => Vec::new(),
                };
                let text = String::from_utf8_lossy(&raw).into_owned();
                return match parse_int_checked(it, &text, 10)? {
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

fn byteorder_big(it: &mut Interp, byteorder: &str) -> R<bool> {
    match byteorder {
        "big" => Ok(true),
        "little" => Ok(false),
        _ => Err(it.value_error("byteorder must be either 'little' or 'big'")),
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

/// An `int` (or `bool`, or a subclass instance): the receiver of the int methods.
#[derive(Clone, Copy)]
pub struct IntArg<'a>(pub &'a Value);

impl IntArg<'_> {
    /// The value as an exact `int` (`True` is `1`).
    fn exact(self) -> Value {
        match self.0 {
            Value::Bool(b) => Value::Int(*b as i64),
            Value::Int(_) => self.0.clone(),
            v => match v.as_bigint() {
                Some(b) => Value::big(b),
                None => v.clone(),
            },
        }
    }

    fn big(self) -> BigInt {
        self.0.as_bigint().unwrap_or_else(BigInt::zero)
    }
}

impl<'a> FromArg<'a, PyHost> for IntArg<'a> {
    #[inline(always)]
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        if v.is_int_like() {
            Ok(IntArg(v))
        } else {
            Err(cx.arg_error(at, "int", v))
        }
    }
}

/// A `float` (or subclass instance): the receiver of the float methods.
#[derive(Clone, Copy)]
pub struct FloatArg(pub f64);

impl<'a> FromArg<'a, PyHost> for FloatArg {
    #[inline(always)]
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        match v {
            Value::Float(f) => Ok(FloatArg(*f)),
            Value::Obj(o) if matches!(o.kind, Kind::Float(_)) => match o.kind {
                Kind::Float(f) => Ok(FloatArg(f)),
                _ => unreachable!(),
            },
            _ => Err(cx.arg_error(at, "float", v)),
        }
    }
}

/// A `complex` (or subclass instance): the receiver of the complex methods.
#[derive(Clone, Copy)]
pub struct ComplexArg(pub f64, pub f64);

impl<'a> FromArg<'a, PyHost> for ComplexArg {
    #[inline(always)]
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        match v {
            Value::Obj(o) => match o.kind {
                Kind::Complex(re, im) => Ok(ComplexArg(re, im)),
                _ => Err(cx.arg_error(at, "complex", v)),
            },
            _ => Err(cx.arg_error(at, "complex", v)),
        }
    }
}

/// int([x]) -> integer
/// int(x, base=10) -> integer
///
/// Convert a number or string to an integer, or return 0 if no arguments
/// are given.  If x is a number, return x.__int__().  For floating-point
/// numbers, this truncates towards zero.
///
/// If x is not a number or if base is given, then x must be a string,
/// bytes, or bytearray instance representing an integer literal in the
/// given base.  The literal can be preceded by '+' or '-' and be surrounded
/// by whitespace.  The base defaults to 10.  Valid bases are 0 and 2-36.
/// Base 0 means to interpret the base from the string as an integer literal.
/// >>> int('0b100', base=0)
/// 4
#[lumen_bind::class(name = "int")]
pub struct Int;

#[lumen_bind::methods]
impl Int {
    #[constructor(hint(py(text_signature = "")))]
    fn new(cls: This<Value>, it: &mut Interp, x: Passed<&Value>, #[kw] base: Passed<&Value>) -> R<Value> {
        let Value::Obj(cls) = &*cls else { unreachable!("checked by the entry") };
        let v = match x.0 {
            None if base.0.is_some() => return Err(it.type_error("int() missing string argument")),
            None => Value::Int(0),
            Some(x) => {
                let base = match base.0 {
                    Some(b) => Some(it.index_of(b)?),
                    None => None,
                };
                int_from_value(it, x, base)?
            }
        };
        Ok(rewrap_int(it, cls, v))
    }

    /// Number of bits necessary to represent self in binary.
    ///
    /// >>> bin(37)
    /// '0b100101'
    /// >>> (37).bit_length()
    /// 6
    #[method]
    fn bit_length(slf: This<IntArg<'_>>) -> Value {
        match slf.0 .0 {
            Value::Int(i) => Value::Int(64 - i.unsigned_abs().leading_zeros() as i64),
            Value::Bool(b) => Value::Int(*b as i64),
            _ => Value::Int(slf.0.big().abs().bit_len() as i64),
        }
    }

    /// Number of ones in the binary representation of the absolute value of self.
    ///
    /// Also known as the population count.
    ///
    /// >>> bin(13)
    /// '0b1101'
    /// >>> (13).bit_count()
    /// 3
    #[method]
    fn bit_count(slf: This<IntArg<'_>>) -> Value {
        match slf.0 .0 {
            Value::Int(i) => Value::Int(i.unsigned_abs().count_ones() as i64),
            Value::Bool(b) => Value::Int(*b as i64),
            _ => Value::Int(slf.0.big().abs().count_ones() as i64),
        }
    }

    /// Return an array of bytes representing an integer.
    ///
    ///   length
    ///     Length of bytes object to use.  An OverflowError is raised if the
    ///     integer is not representable with the given number of bytes.  Default
    ///     is length 1.
    ///   byteorder
    ///     The byte order used to represent the integer.  If byteorder is 'big',
    ///     the most significant byte is at the beginning of the byte array.  If
    ///     byteorder is 'little', the most significant byte is at the end of the
    ///     byte array.  To request the native byte order of the host system, use
    ///     `sys.byteorder' as the byte order value.  Default is to use 'big'.
    ///   signed
    ///     Determines whether two's complement is used to represent the integer.
    ///     If signed is False and a negative integer is given, an OverflowError
    ///     is raised.
    #[method]
    fn to_bytes(
        slf: This<IntArg<'_>>,
        it: &mut Interp,
        #[kw] #[default(1)] length: isize,
        #[kw] #[default("big")] byteorder: &str,
        #[kwonly] #[default(false)] signed: bool,
    ) -> R<Value> {
        if length < 0 {
            return Err(it.value_error("length argument must be non-negative"));
        }
        let len = length as usize;
        it.check_bytes_len(len)?;
        let big_endian = byteorder_big(it, byteorder)?;
        let n = slf.0.big();
        if n.is_negative() && !signed {
            return Err(it.overflow_err("can't convert negative int to unsigned"));
        }
        match n.to_py_bytes(len, big_endian, signed) {
            Some(v) => Ok(Value::bytes(v)),
            None => Err(it.overflow_err("int too big to convert")),
        }
    }

    /// Return the integer represented by the given array of bytes.
    ///
    ///   bytes
    ///     Holds the array of bytes to convert.  The argument must either
    ///     support the buffer protocol or be an iterable object producing bytes.
    ///     Bytes and bytearray are examples of built-in objects that support the
    ///     buffer protocol.
    ///   byteorder
    ///     The byte order used to represent the integer.  If byteorder is 'big',
    ///     the most significant byte is at the beginning of the byte array.  If
    ///     byteorder is 'little', the most significant byte is at the end of the
    ///     byte array.  To request the native byte order of the host system, use
    ///     `sys.byteorder' as the byte order value.  Default is to use 'big'.
    ///   signed
    ///     Indicates whether two's complement is used to represent the integer.
    #[classmethod]
    fn from_bytes(
        cls: This<Value>,
        it: &mut Interp,
        #[kw] bytes: &Value,
        #[kw] #[default("big")] byteorder: &str,
        #[kwonly] #[default(false)] signed: bool,
    ) -> R<Value> {
        let big_endian = byteorder_big(it, byteorder)?;
        let data = it.bytes_from_object(bytes)?;
        let v = Value::big(BigInt::from_py_bytes(&data, big_endian, signed));
        let Value::Obj(c) = &*cls else { return Ok(v) };
        if Rc::ptr_eq(c, &it.types.int) {
            return Ok(v);
        }
        it.call(&cls, vec![v], Vec::new())
    }

    /// Returns self, the complex conjugate of any int.
    #[method(hint(py(text_signature = "")))]
    fn conjugate(slf: This<IntArg<'_>>) -> Value {
        slf.0.exact()
    }

    /// Return a pair of integers, whose ratio is equal to the original int.
    ///
    /// The ratio is in lowest terms and has a positive denominator.
    ///
    /// >>> (10).as_integer_ratio()
    /// (10, 1)
    /// >>> (-10).as_integer_ratio()
    /// (-10, 1)
    /// >>> (0).as_integer_ratio()
    /// (0, 1)
    #[method]
    fn as_integer_ratio(slf: This<IntArg<'_>>) -> Value {
        Value::tuple(vec![slf.0.exact(), Value::Int(1)])
    }

    /// Returns True. Exists for duck type compatibility with float.is_integer.
    #[method]
    fn is_integer(slf: This<IntArg<'_>>) -> bool {
        let _ = slf;
        true
    }

    #[proto(index)]
    fn index(slf: This<IntArg<'_>>) -> Value {
        slf.0.exact()
    }

    #[proto(int)]
    fn int(slf: This<IntArg<'_>>) -> Value {
        slf.0.exact()
    }

    #[proto(float)]
    fn float(slf: This<IntArg<'_>>, it: &mut Interp) -> R<f64> {
        it.float_arg(slf.0 .0)
    }

    /// Truncating an Integral returns itself.
    #[method(name = "__trunc__", hint(py(text_signature = "")))]
    fn trunc(slf: This<IntArg<'_>>) -> Value {
        slf.0.exact()
    }

    /// Flooring an Integral returns itself.
    #[method(name = "__floor__", hint(py(text_signature = "")))]
    fn floor(slf: This<IntArg<'_>>) -> Value {
        slf.0.exact()
    }

    /// Ceiling of an Integral returns itself.
    #[method(name = "__ceil__", hint(py(text_signature = "")))]
    fn ceil(slf: This<IntArg<'_>>) -> Value {
        slf.0.exact()
    }

    /// Rounding an Integral returns itself.
    ///
    /// Rounding with an ndigits argument also returns an integer.
    #[method(name = "__round__", hint(py(text_signature = "($self, ndigits=<unrepresentable>, /)")))]
    fn round(slf: This<IntArg<'_>>, it: &mut Interp, ndigits: Option<&Value>) -> R<Value> {
        round_number(it, slf.0.exact(), ndigits)
    }

    #[method(name = "__getnewargs__")]
    fn getnewargs(slf: This<IntArg<'_>>) -> Value {
        Value::tuple(vec![slf.0.exact()])
    }

    #[proto(repr)]
    fn repr(slf: This<IntArg<'_>>, it: &mut Interp) -> R<String> {
        it.native_repr(&slf.0.exact())
    }

    /// the real part of a complex number
    #[getter]
    fn real(slf: This<IntArg<'_>>) -> Value {
        slf.0.exact()
    }

    /// the imaginary part of a complex number
    #[getter]
    fn imag(slf: This<IntArg<'_>>) -> Value {
        let _ = slf;
        Value::Int(0)
    }

    /// the numerator of a rational number in lowest terms
    #[getter]
    fn numerator(slf: This<IntArg<'_>>) -> Value {
        slf.0.exact()
    }

    /// the denominator of a rational number in lowest terms
    #[getter]
    fn denominator(slf: This<IntArg<'_>>) -> Value {
        let _ = slf;
        Value::Int(1)
    }
}

/// bool(x) -> bool
///
/// Returns True when the argument x is true, False otherwise.
/// The builtins True and False are the only two instances of the class bool.
/// The class bool is a subclass of the class int, and cannot be subclassed.
#[lumen_bind::class(name = "bool")]
pub struct Bool;

#[lumen_bind::methods]
impl Bool {
    #[constructor(hint(py(text_signature = "")))]
    fn new(it: &mut Interp, x: Passed<&Value>) -> R<Value> {
        Ok(Value::Bool(match x.0 {
            Some(v) => it.truthy(v)?,
            None => false,
        }))
    }

    #[proto(repr)]
    fn repr(slf: This<IntArg<'_>>, it: &mut Interp) -> R<String> {
        it.native_repr(slf.0 .0)
    }
}

/// Convert a string or number to a floating-point number, if possible.
#[lumen_bind::class(name = "float")]
pub struct Float;

#[lumen_bind::methods]
impl Float {
    #[constructor(hint(py(text_signature = "(x=0, /)")))]
    fn new(cls: This<Value>, it: &mut Interp, x: Passed<&Value>) -> R<Value> {
        let Value::Obj(cls) = &*cls else { unreachable!("checked by the entry") };
        let f = match x.0 {
            None => 0.0,
            Some(x) => float_from_value(it, x)?,
        };
        Ok(if Rc::ptr_eq(cls, &it.types.float) { Value::Float(f) } else { Value::Obj(Object::with_cls(cls.clone(), Kind::Float(f))) })
    }

    /// Return True if the float is an integer.
    #[method]
    fn is_integer(slf: This<FloatArg>) -> bool {
        let f = slf.0 .0;
        f.is_finite() && f == fmath::trunc(f)
    }

    /// Return a pair of integers, whose ratio is exactly equal to the original float.
    ///
    /// The ratio is in lowest terms and has a positive denominator.  Raise
    /// OverflowError on infinities and a ValueError on NaNs.
    ///
    /// >>> (10.0).as_integer_ratio()
    /// (10, 1)
    /// >>> (0.0).as_integer_ratio()
    /// (0, 1)
    /// >>> (-.25).as_integer_ratio()
    /// (-1, 4)
    #[method]
    fn as_integer_ratio(slf: This<FloatArg>, it: &mut Interp) -> R<Value> {
        float_as_ratio(it, slf.0 .0)
    }

    /// Return a hexadecimal representation of a floating-point number.
    ///
    /// >>> (-0.1).hex()
    /// '-0x1.999999999999ap-4'
    /// >>> 3.14159.hex()
    /// '0x1.921f9f01b866ep+1'
    #[method]
    fn hex(slf: This<FloatArg>) -> String {
        float_hex(slf.0 .0)
    }

    /// Create a floating-point number from a hexadecimal string.
    ///
    /// >>> float.fromhex('0x1.ffffp10')
    /// 2047.984375
    /// >>> float.fromhex('-0x1p-1074')
    /// -5e-324
    #[classmethod]
    fn fromhex(cls: This<Value>, it: &mut Interp, string: &Value) -> R<Value> {
        let Some(s) = string.as_str() else { return Err(it.type_error("bad argument type for built-in operation")) };
        let Some(f) = parse_hex_float(s) else { return Err(it.value_error("invalid hexadecimal floating-point string")) };
        let Value::Obj(c) = &*cls else { return Ok(Value::Float(f)) };
        if Rc::ptr_eq(c, &it.types.float) {
            return Ok(Value::Float(f));
        }
        it.call(&cls, vec![Value::Float(f)], Vec::new())
    }

    /// You probably don't want to use this function.
    ///
    ///   typestr
    ///     Must be 'double' or 'float'.
    ///
    /// It exists mainly to be used in Python's test suite.
    ///
    /// This function returns whichever of 'unknown', 'IEEE, big-endian' or 'IEEE,
    /// little-endian' best describes the format of floating-point numbers used by the
    /// C type named by typestr.
    #[classmethod(name = "__getformat__")]
    fn getformat(cls: This<Value>, it: &mut Interp, typestr: &Value) -> R<String> {
        let _ = cls;
        let Some(t) = typestr.as_str() else {
            let n = it.type_name_of(typestr);
            return Err(it.type_error(&format!("__getformat__() argument must be str, not {n}")));
        };
        if t != "double" && t != "float" {
            return Err(it.value_error("__getformat__() argument 1 must be 'double' or 'float'"));
        }
        let order = if cfg!(target_endian = "little") { "little" } else { "big" };
        Ok(format!("IEEE, {order}-endian"))
    }

    /// Return self, the complex conjugate of any float.
    #[method]
    fn conjugate(slf: This<FloatArg>) -> f64 {
        slf.0 .0
    }

    /// Return the Integral closest to x between 0 and x.
    #[method(name = "__trunc__")]
    fn trunc(slf: This<FloatArg>, it: &mut Interp) -> R<Value> {
        float_to_int_checked(it, slf.0 .0)
    }

    #[proto(int)]
    fn int(slf: This<FloatArg>, it: &mut Interp) -> R<Value> {
        float_to_int_checked(it, slf.0 .0)
    }

    /// Return the floor as an Integral.
    #[method(name = "__floor__")]
    fn floor(slf: This<FloatArg>, it: &mut Interp) -> R<Value> {
        float_to_int_checked(it, fmath::floor(slf.0 .0))
    }

    /// Return the ceiling as an Integral.
    #[method(name = "__ceil__")]
    fn ceil(slf: This<FloatArg>, it: &mut Interp) -> R<Value> {
        float_to_int_checked(it, fmath::ceil(slf.0 .0))
    }

    #[proto(float)]
    fn float(slf: This<FloatArg>) -> f64 {
        slf.0 .0
    }

    /// Return the Integral closest to x, rounding half toward even.
    ///
    /// When an argument is passed, work like built-in round(x, ndigits).
    #[method(name = "__round__", hint(py(text_signature = "($self, ndigits=None, /)")))]
    fn round(slf: This<FloatArg>, it: &mut Interp, ndigits: Option<&Value>) -> R<Value> {
        round_number(it, Value::Float(slf.0 .0), ndigits)
    }

    #[method(name = "__getnewargs__")]
    fn getnewargs(slf: This<FloatArg>) -> Value {
        Value::tuple(vec![Value::Float(slf.0 .0)])
    }

    #[proto(repr)]
    fn repr(slf: This<FloatArg>) -> String {
        float_repr(slf.0 .0)
    }

    /// Formats the float according to format_spec.
    #[method(name = "__format__")]
    fn format(slf: This<&Value>, it: &mut Interp, format_spec: &Value) -> R<String> {
        format_number(it, &slf, format_spec)
    }

    /// the real part of a complex number
    #[getter]
    fn real(slf: This<FloatArg>) -> f64 {
        slf.0 .0
    }

    /// the imaginary part of a complex number
    #[getter]
    fn imag(slf: This<FloatArg>) -> f64 {
        let _ = slf;
        0.0
    }
}

/// Create a complex number from a string or numbers.
///
/// If a string is given, parse it as a complex number.
/// If a single number is given, convert it to a complex number.
/// If the 'real' or 'imag' arguments are given, create a complex number
/// with the specified real and imaginary components.
#[lumen_bind::class(name = "complex")]
pub struct Complex;

#[lumen_bind::methods]
impl Complex {
    #[constructor(hint(py(text_signature = "(real=0, imag=0)")))]
    fn new(cls: This<Value>, it: &mut Interp, #[kw] real: Passed<&Value>, #[kw] imag: Passed<&Value>) -> R<Value> {
        let Value::Obj(cls) = &*cls else { unreachable!("checked by the entry") };
        complex_new(it, cls, real.0, imag.0)
    }

    /// Return the complex conjugate of its argument. (3-4j).conjugate() == 3+4j.
    #[method]
    fn conjugate(slf: This<ComplexArg>) -> Value {
        Value::Obj(Object::new(Kind::Complex(slf.0 .0, -slf.0 .1)))
    }

    /// Convert this value to exact type complex.
    #[method(name = "__complex__")]
    fn complex(slf: This<ComplexArg>) -> Value {
        Value::Obj(Object::new(Kind::Complex(slf.0 .0, slf.0 .1)))
    }

    #[method(name = "__getnewargs__")]
    fn getnewargs(slf: This<ComplexArg>) -> Value {
        Value::tuple(vec![Value::Float(slf.0 .0), Value::Float(slf.0 .1)])
    }

    #[proto(repr)]
    fn repr(slf: This<&Value>, it: &mut Interp) -> R<String> {
        it.native_repr(&slf)
    }

    /// the real part of a complex number
    #[getter]
    fn real(slf: This<ComplexArg>) -> f64 {
        slf.0 .0
    }

    /// the imaginary part of a complex number
    #[getter]
    fn imag(slf: This<ComplexArg>) -> f64 {
        slf.0 .1
    }
}

/// The number slots `int`, `float` and `complex` share.
#[lumen_bind::class(name = "number", hint(py(shared)))]
pub struct NumberSlots;

#[lumen_bind::methods]
impl NumberSlots {
    #[proto(neg)]
    fn neg(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        it.unary_op(UnOp::Neg, &slf)
    }

    #[proto(pos)]
    fn pos(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        it.unary_op(UnOp::Pos, &slf)
    }

    #[proto(invert)]
    fn invert(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        it.unary_op(UnOp::Invert, &slf)
    }

    #[proto(abs)]
    fn abs(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        let f = it.builtins_fn("abs");
        it.call(&f, vec![slf.0.clone()], Vec::new())
    }

    #[proto(bool)]
    fn bool(slf: This<&Value>, it: &mut Interp) -> R<bool> {
        it.truthy(&slf)
    }

    #[proto(hash)]
    fn hash(slf: This<&Value>, it: &mut Interp) -> R<i64> {
        it.native_hash(&slf)
    }

    #[proto(divmod)]
    fn divmod(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        num_divmod(it, &slf, value, false)
    }

    #[proto(rdivmod)]
    fn rdivmod(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        num_divmod(it, &slf, value, true)
    }

    #[proto(pow)]
    fn pow(slf: This<&Value>, it: &mut Interp, value: &Value, r#mod: Option<&Value>) -> R<Value> {
        num_pow(it, &slf, value, r#mod, false)
    }

    #[proto(rpow)]
    fn rpow(slf: This<&Value>, it: &mut Interp, value: &Value, r#mod: Option<&Value>) -> R<Value> {
        num_pow(it, &slf, value, r#mod, true)
    }

    /// Convert to a string according to format_spec.
    #[method(name = "__format__")]
    fn format(slf: This<&Value>, it: &mut Interp, format_spec: &Value) -> R<String> {
        format_number(it, &slf, format_spec)
    }
}

fn format_number(it: &mut Interp, v: &Value, spec: &Value) -> R<String> {
    let Some(spec) = spec.as_str() else {
        let t = it.type_name_of(spec);
        return Err(it.type_error(&format!("__format__() argument must be str, not {t}")));
    };
    let spec = spec.to_string();
    it.native_format(v, &spec)
}

fn round_number(it: &mut Interp, v: Value, ndigits: Option<&Value>) -> R<Value> {
    let f = it.builtins_fn("round");
    let mut args = vec![v];
    args.extend(ndigits.cloned());
    it.call(&f, args, Vec::new())
}

fn num_divmod(it: &mut Interp, slf: &Value, other: &Value, reflected: bool) -> R<Value> {
    if numeric_wider(slf, other) {
        return Ok(Value::NotImplemented);
    }
    let (a, b) = if reflected { (other, slf) } else { (slf, other) };
    let Some(q) = it.native_binop(BinOp::FloorDiv, a, b)? else { return Ok(Value::NotImplemented) };
    let Some(r) = it.native_binop(BinOp::Mod, a, b)? else { return Ok(Value::NotImplemented) };
    Ok(Value::tuple(vec![q, r]))
}

fn num_pow(it: &mut Interp, slf: &Value, other: &Value, m: Option<&Value>, reflected: bool) -> R<Value> {
    let (a, b) = if reflected { (other, slf) } else { (slf, other) };
    if let Some(m) = m.filter(|m| !m.is_none()) {
        let f = it.builtins_fn("pow");
        return it.call(&f, vec![a.clone(), b.clone(), m.clone()], Vec::new());
    }
    if numeric_wider(slf, other) {
        return Ok(Value::NotImplemented);
    }
    Ok(it.native_binop(BinOp::Pow, a, b)?.unwrap_or(Value::NotImplemented))
}

fn float_from_value(it: &mut Interp, x: &Value) -> R<f64> {
    let text = match x {
        Value::Obj(o) => match &o.kind {
            Kind::Str(s) => Some(s.s.to_string()),
            Kind::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        },
        _ => None,
    };
    if let Some(s) = text {
        return match parse_float_str(&s) {
            Some(f) => Ok(f),
            None => {
                let r = it.repr_of(x)?;
                Err(it.value_error(&format!("could not convert string to float: {}", r)))
            }
        };
    }
    match it.float_arg(x) {
        Ok(f) => Ok(f),
        Err(e) if it.exc_is(&e, "TypeError") => {
            let t = it.type_name_of(x);
            Err(it.type_error(&format!("float() argument must be a string or a real number, not '{}'", t)))
        }
        Err(e) => Err(e),
    }
}

fn complex_new(it: &mut Interp, cls: &Obj, real: Option<&Value>, imag: Option<&Value>) -> R<Value> {
    if let (Some(v @ Value::Obj(o)), None) = (real, imag) {
        if o.cls.is_none() && matches!(o.kind, Kind::Complex(..)) && Rc::ptr_eq(cls, &it.types.complex) {
            return Ok(v.clone());
        }
    }
    let (mut re, mut im) = (0.0, 0.0);
    let parts = |it: &mut Interp, v: &Value, first: bool| -> R<(f64, f64)> {
        if let Value::Obj(o) = v {
            if let Kind::Complex(..) = &o.kind {
                return it.complex_arg(v);
            }
            if first && o.cls.is_some() && it.user_special(v, "__complex__").is_some() {
                return it.complex_arg(v);
            }
            if let Kind::Str(s) = &o.kind {
                if !first {
                    return Err(it.type_error("complex() second arg can't be a string"));
                }
                return match parse_complex(&s.s) {
                    Some(p) => Ok(p),
                    None => Err(it.value_error("complex() arg is a malformed string")),
                };
            }
        }
        match it.float_arg(v) {
            Ok(f) => Ok((f, 0.0)),
            Err(e) if !it.exc_is(&e, "TypeError") => Err(e),
            Err(_) => {
                let t = it.type_name_of(v);
                Err(it.type_error(&if first {
                    format!("complex() first argument must be a string or a number, not '{t}'")
                } else {
                    format!("complex() second argument must be a number, not '{t}'")
                }))
            }
        }
    };
    if let (Some(Value::Obj(o)), Some(_)) = (real, imag) {
        if matches!(o.kind, Kind::Str(_)) {
            return Err(it.type_error("complex() can't take second arg if first is a string"));
        }
    }
    let mut first_complex = false;
    if let Some(r) = real {
        let (x, y) = parts(it, r, true)?;
        first_complex = matches!(r, Value::Obj(o) if matches!(o.kind, Kind::Complex(..) | Kind::Str(_)) || (o.cls.is_some() && it.user_special(r, "__complex__").is_some()));
        re = x;
        im = y;
    }
    if let Some(i) = imag {
        let (x, y) = parts(it, i, false)?;
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
    Ok(Value::Obj(if Rc::ptr_eq(cls, &it.types.complex) { Object::new(kind) } else { Object::with_cls(cls.clone(), kind) }))
}

impl Interp {
    pub fn builtins_fn(&self, name: &str) -> Value {
        dict_get_str(&self.builtins, name).unwrap_or(Value::None)
    }
}

fn float_as_ratio(it: &mut Interp, f: f64) -> R<Value> {
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

fn float_hex(f: f64) -> String {
    if f.is_nan() || f.is_infinite() {
        return float_repr(f);
    }
    if f == 0.0 {
        return (if f.is_sign_negative() { "-0x0.0p+0" } else { "0x0.0p+0" }).to_string();
    }
    let bits = f.to_bits();
    let neg = (bits >> 63) != 0;
    let exp = ((bits >> 52) & 0x7ff) as i64;
    let frac = bits & ((1u64 << 52) - 1);
    let (lead, e) = if exp == 0 { (0, -1022) } else { (1, exp - 1023) };
    let hex = format!("{:013x}", frac);
    format!("{}0x{}.{}p{}{}", if neg { "-" } else { "" }, lead, hex, if e < 0 { '-' } else { '+' }, e.abs())
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

pub fn init(it: &mut Interp) {
    use crate::bind::{extend_type_documented as extend_type, install_into};
    use super::slots::{reg_binops, reg_compare};
    let (int, bool_, float, complex) = (it.types.int.clone(), it.types.bool_.clone(), it.types.float.clone(), it.types.complex.clone());
    let shared = ["__neg__", "__pos__", "__abs__", "__bool__", "__hash__", "__pow__", "__rpow__", "__format__"];
    let all_ops = [
        "__add__", "__radd__", "__sub__", "__rsub__", "__mul__", "__rmul__", "__truediv__", "__rtruediv__", "__mod__", "__rmod__",
        "__floordiv__", "__rfloordiv__", "__lshift__", "__rlshift__", "__rshift__", "__rrshift__", "__and__", "__rand__", "__or__",
        "__ror__", "__xor__", "__rxor__",
    ];

    install_into::<NumberSlots>(&int, &shared);
    install_into::<NumberSlots>(&int, &["__invert__", "__divmod__", "__rdivmod__"]);
    extend_type::<Int>(it, &int);
    reg_binops(it, &int, &all_ops);
    reg_compare(it, &int, true);

    extend_type::<Bool>(it, &bool_);
    install_into::<NumberSlots>(&bool_, &["__invert__"]);
    reg_binops(it, &bool_, &["__and__", "__rand__", "__or__", "__ror__", "__xor__", "__rxor__"]);

    install_into::<NumberSlots>(&float, &shared);
    install_into::<NumberSlots>(&float, &["__divmod__", "__rdivmod__"]);
    extend_type::<Float>(it, &float);
    let float_ops = [
        "__add__", "__radd__", "__sub__", "__rsub__", "__mul__", "__rmul__", "__truediv__", "__rtruediv__", "__mod__", "__rmod__",
        "__floordiv__", "__rfloordiv__",
    ];
    reg_binops(it, &float, &float_ops);
    reg_compare(it, &float, true);

    install_into::<NumberSlots>(&complex, &shared);
    extend_type::<Complex>(it, &complex);
    let complex_ops = ["__add__", "__radd__", "__sub__", "__rsub__", "__mul__", "__rmul__", "__truediv__", "__rtruediv__"];
    reg_binops(it, &complex, &complex_ops);
    reg_compare(it, &complex, true);
    let _ = (to_num as fn(&Value) -> Option<Num>, round_half_even as fn(f64) -> f64);
}

/// The `real`/`imag`/... attributes as CPython's getset and member descriptors (their types exist
/// once `descr` is initialised).
pub fn init_descriptors(it: &mut Interp) {
    let (int, float, complex) = (it.types.int.clone(), it.types.float.clone(), it.types.complex.clone());
    super::descr::install_getsets::<Int>(it, &int, &[]);
    super::descr::install_getsets::<Float>(it, &float, &[]);
    super::descr::install_getsets::<Complex>(it, &complex, &["real", "imag"]);
}
