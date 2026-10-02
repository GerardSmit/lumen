//! `_testcapi` wrappers of the numeric APIs (`PyLong_*`, `PyFloat_*`, `PyComplex_*`, `_Py_c_*`).
//! `None` stands for a `NULL` pointer, which the C API rejects with `SystemError`.

use super::{call_builtin, complex_value, int_value, nonnull, object_at, pointer_of};
use crate::bind::index;
use crate::object::*;
use crate::pyint::BigInt;
use crate::vm::Interp;
use lumen_common::float::complex::{self, Complex};
use lumen_common::float::MathError;

const EDOM: i64 = 33;
const ERANGE: i64 = 34;

fn exact_int(v: &Value) -> bool {
    matches!(v, Value::Int(_)) || matches!(v, Value::Obj(o) if o.cls.is_none() && matches!(o.kind, Kind::Int(_)))
}

/// The integer of an argument `PyLong_Check` accepts, else `TypeError: an integer is required`.
fn strict_int(it: &mut Interp, v: &Value) -> R<BigInt> {
    let v = nonnull(it, v)?;
    match v.as_bigint() {
        Some(b) => Ok(b),
        None => Err(it.type_error("an integer is required")),
    }
}

/// The integer of an argument through `__index__` (`_PyNumber_Index`).
fn indexed_int(it: &mut Interp, v: &Value) -> R<BigInt> {
    let v = nonnull(it, v)?;
    let i = index(it, v)?;
    Ok(i.as_bigint().unwrap_or_else(BigInt::zero))
}

fn signed_in(it: &mut Interp, b: &BigInt, min: i128, max: i128, c_type: &str) -> R<i128> {
    match b.to_i128() {
        Some(n) if n >= min && n <= max => Ok(n),
        _ => Err(it.overflow_err(&format!("Python int too large to convert to C {c_type}"))),
    }
}

fn unsigned_in(it: &mut Interp, b: &BigInt, max: u128, c_type: &str, negative: &str) -> R<i128> {
    if b.is_negative() {
        return Err(it.overflow_err(negative));
    }
    match b.to_i128() {
        Some(n) if (n as u128) <= max => Ok(n),
        _ => Err(it.overflow_err(&format!("Python int too large to convert to C {c_type}"))),
    }
}

/// `(value, overflow)` of `PyLong_AsLongAndOverflow`: a value outside `min..=max` is `-1` with
/// `overflow` set to its sign.
fn with_overflow(b: &BigInt, min: i128, max: i128) -> Value {
    let (value, overflow) = match b.to_i128() {
        Some(n) if n >= min && n <= max => (n, 0),
        _ => (-1, if b.is_negative() { -1 } else { 1 }),
    };
    Value::tuple(vec![int_value(value), Value::Int(overflow)])
}

fn mask(b: &BigInt, bits: u32) -> i128 {
    let n = b.to_i128_wrapping();
    if bits >= 128 {
        n
    } else {
        n & ((1i128 << bits) - 1)
    }
}

fn text_of_bytes(data: &[u8]) -> String {
    data.iter().map(|&b| b as char).collect()
}

fn float_of(it: &mut Interp, v: &Value) -> R<f64> {
    it.float_arg(v)
}

fn status(e: Option<MathError>) -> i64 {
    match e {
        None => 0,
        Some(MathError::Domain) => EDOM,
        Some(MathError::Range) => ERANGE,
    }
}

fn pair(it: &mut Interp, a: &Value, b: &Value) -> R<(Complex, Complex)> {
    let (ar, ai) = it.complex_arg(a)?;
    let (br, bi) = it.complex_arg(b)?;
    Ok((Complex::new(ar, ai), Complex::new(br, bi)))
}

fn with_errno(z: Complex, errno: i64) -> Value {
    Value::tuple(vec![complex_value(z.re, z.im), Value::Int(errno)])
}

#[lumen_bind::module(name = "_testcapi")]
pub mod numbersm {
    use super::*;

    #[op]
    fn call_long_compact_api(it: &mut Interp, arg: &Value) -> R<Value> {
        let b = strict_int(it, arg)?;
        let compact = b.bit_len() <= 30;
        let value = if compact { b.to_i64().unwrap_or(-1) } else { -1 };
        Ok(Value::tuple(vec![Value::Bool(compact), Value::Int(value)]))
    }

    #[op]
    fn pylong_check(it: &mut Interp, obj: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        Ok(i64::from(obj.is_int_like()))
    }

    #[op]
    fn pylong_checkexact(it: &mut Interp, obj: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        Ok(i64::from(exact_int(obj)))
    }

    #[op]
    fn pylong_fromdouble(it: &mut Interp, arg: &Value) -> R<Value> {
        let d = float_of(it, arg)?;
        call_builtin(it, "int", vec![Value::Float(d)])
    }

    #[op]
    fn pylong_fromstring(it: &mut Interp, data: &Value, base: i64) -> R<Value> {
        let nonnull_data = nonnull(it, data)?;
        let raw = it.bytes_from_object(nonnull_data)?;
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        let raw = &raw[..end];
        if base != 0 && !(2..=36).contains(&base) {
            return Err(it.value_error("int() arg 2 must be >= 2 and <= 36"));
        }
        let text = text_of_bytes(raw);
        if !raw.is_ascii() {
            let shown = it.repr_of(&Value::string(text))?;
            return Err(it.value_error(&format!("invalid literal for int() with base {}: {}", if base == 0 { 10 } else { base }, shown)));
        }
        let result = call_builtin(it, "int", vec![Value::string(text), Value::Int(base)])?;
        Ok(Value::tuple(vec![result, Value::Int(end as i64)]))
    }

    #[op]
    fn pylong_fromunicodeobject(it: &mut Interp, unicode: &Value, base: i64) -> R<Value> {
        let unicode = nonnull(it, unicode)?;
        if base != 0 && !(2..=36).contains(&base) {
            return Err(it.value_error("int() arg 2 must be >= 2 and <= 36"));
        }
        call_builtin(it, "int", vec![unicode.clone(), Value::Int(base)])
    }

    #[op]
    fn pylong_fromvoidptr(it: &mut Interp, arg: &Value) -> Value {
        let addr = pointer_of(it, arg);
        int_value(addr as i128)
    }

    #[op]
    fn pylong_aslong(it: &mut Interp, arg: &Value) -> R<Value> {
        let b = indexed_int(it, arg)?;
        Ok(int_value(signed_in(it, &b, i64::MIN as i128, i64::MAX as i128, "long")?))
    }

    #[op]
    fn pylong_aslongandoverflow(it: &mut Interp, arg: &Value) -> R<Value> {
        let b = indexed_int(it, arg)?;
        Ok(with_overflow(&b, i64::MIN as i128, i64::MAX as i128))
    }

    #[op]
    fn pylong_asunsignedlong(it: &mut Interp, arg: &Value) -> R<Value> {
        let b = strict_int(it, arg)?;
        Ok(int_value(unsigned_in(it, &b, u64::MAX as u128, "unsigned long", "can't convert negative value to unsigned int")?))
    }

    #[op]
    fn pylong_asunsignedlongmask(it: &mut Interp, arg: &Value) -> R<Value> {
        let b = indexed_int(it, arg)?;
        Ok(int_value(mask(&b, 64)))
    }

    #[op]
    fn pylong_aslonglong(it: &mut Interp, arg: &Value) -> R<Value> {
        let b = indexed_int(it, arg)?;
        Ok(int_value(signed_in(it, &b, i64::MIN as i128, i64::MAX as i128, "long long")?))
    }

    #[op]
    fn pylong_aslonglongandoverflow(it: &mut Interp, arg: &Value) -> R<Value> {
        let b = indexed_int(it, arg)?;
        Ok(with_overflow(&b, i64::MIN as i128, i64::MAX as i128))
    }

    #[op]
    fn pylong_asunsignedlonglong(it: &mut Interp, arg: &Value) -> R<Value> {
        let b = strict_int(it, arg)?;
        if b.is_negative() {
            return Err(it.overflow_err("can't convert negative int to unsigned"));
        }
        match b.to_i128() {
            Some(n) if (n as u128) <= u64::MAX as u128 => Ok(int_value(n)),
            _ => Err(it.overflow_err("int too big to convert")),
        }
    }

    #[op]
    fn pylong_asunsignedlonglongmask(it: &mut Interp, arg: &Value) -> R<Value> {
        let b = indexed_int(it, arg)?;
        Ok(int_value(mask(&b, 64)))
    }

    #[op]
    fn pylong_as_ssize_t(it: &mut Interp, arg: &Value) -> R<Value> {
        let b = strict_int(it, arg)?;
        Ok(int_value(signed_in(it, &b, i64::MIN as i128, i64::MAX as i128, "ssize_t")?))
    }

    #[op]
    fn pylong_as_size_t(it: &mut Interp, arg: &Value) -> R<Value> {
        let b = strict_int(it, arg)?;
        Ok(int_value(unsigned_in(it, &b, u64::MAX as u128, "size_t", "can't convert negative value to size_t")?))
    }

    #[op]
    fn pylong_asdouble(it: &mut Interp, arg: &Value) -> R<Value> {
        let b = strict_int(it, arg)?;
        match crate::pyint::PyInt::to_float(&b) {
            Some(f) => Ok(Value::Float(f)),
            None => Err(it.overflow_err("int too large to convert to float")),
        }
    }

    #[op]
    fn pylong_asvoidptr(it: &mut Interp, arg: &Value) -> R<Value> {
        let b = strict_int(it, arg)?;
        let addr = if b.is_negative() {
            signed_in(it, &b, i64::MIN as i128, i64::MAX as i128, "long")? as i64 as u64
        } else {
            unsigned_in(it, &b, u64::MAX as u128, "unsigned long", "can't convert negative value to unsigned int")? as u64
        };
        Ok(object_at(it, addr as usize))
    }

    #[op]
    fn pylong_aspid(it: &mut Interp, arg: &Value) -> R<Value> {
        let b = indexed_int(it, arg)?;
        Ok(int_value(signed_in(it, &b, i32::MIN as i128, i32::MAX as i128, "int")?))
    }

    #[op]
    fn float_check(it: &mut Interp, obj: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        Ok(i64::from(matches!(obj, Value::Float(_)) || matches!(obj, Value::Obj(o) if matches!(o.kind, Kind::Float(_)))))
    }

    #[op]
    fn float_checkexact(it: &mut Interp, obj: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        Ok(i64::from(matches!(obj, Value::Float(_)) || matches!(obj, Value::Obj(o) if o.cls.is_none() && matches!(o.kind, Kind::Float(_)))))
    }

    #[op]
    fn float_fromstring(it: &mut Interp, obj: &Value) -> R<Value> {
        let obj = nonnull(it, obj)?;
        if obj.as_str().is_some() {
            return call_builtin(it, "float", vec![obj.clone()]);
        }
        if it.is_buffer(obj) {
            let raw = it.buffer_bytes(obj)?;
            let text = text_of_bytes(&raw);
            return call_builtin(it, "float", vec![Value::string(text)]);
        }
        Err(it.type_error("must be str, not float"))
    }

    #[op]
    fn float_fromdouble(it: &mut Interp, obj: &Value) -> R<Value> {
        Ok(Value::Float(float_of(it, obj)?))
    }

    #[op]
    fn float_asdouble(it: &mut Interp, obj: &Value) -> R<Value> {
        let obj = nonnull(it, obj)?;
        Ok(Value::Float(float_of(it, obj)?))
    }

    #[op]
    fn float_getinfo(it: &mut Interp) -> R<Value> {
        let sys = it.import_module("sys")?;
        it.get_attr_str(&Value::Obj(sys), "float_info")
    }

    #[op]
    fn float_getmax() -> f64 {
        f64::MAX
    }

    #[op]
    fn float_getmin() -> f64 {
        f64::MIN_POSITIVE
    }

    /// Test PyFloat_Pack2(), PyFloat_Pack4() and PyFloat_Pack8()
    #[op]
    fn float_pack(it: &mut Interp, size: i64, d: f64, le: i64) -> R<Value> {
        let little = le != 0;
        let bytes: Vec<u8> = match size {
            2 => match lumen_common::float16::f64_to_f16_bits(d) {
                Some(bits) => if little { bits.to_le_bytes().to_vec() } else { bits.to_be_bytes().to_vec() },
                None => return Err(it.overflow_err("float too large to pack with e format")),
            },
            4 => {
                let f = d as f32;
                if f.is_infinite() && d.is_finite() {
                    return Err(it.overflow_err("float too large to pack with f format"));
                }
                if little { f.to_le_bytes().to_vec() } else { f.to_be_bytes().to_vec() }
            }
            8 => if little { d.to_le_bytes().to_vec() } else { d.to_be_bytes().to_vec() },
            _ => return Err(it.value_error("size must 2, 4 or 8")),
        };
        Ok(Value::bytes(bytes))
    }

    /// Test PyFloat_Unpack2(), PyFloat_Unpack4() and PyFloat_Unpack8()
    #[op]
    fn float_unpack(it: &mut Interp, data: &[u8], le: i64) -> R<f64> {
        let little = le != 0;
        match data.len() {
            2 => {
                let raw = [data[0], data[1]];
                let bits = if little { u16::from_le_bytes(raw) } else { u16::from_be_bytes(raw) };
                Ok(lumen_common::float16::f16_bits_to_f64(bits))
            }
            4 => {
                let raw = [data[0], data[1], data[2], data[3]];
                let bits = if little { u32::from_le_bytes(raw) } else { u32::from_be_bytes(raw) };
                Ok(f64::from(f32::from_bits(bits)))
            }
            8 => {
                let mut raw = [0u8; 8];
                raw.copy_from_slice(data);
                let bits = if little { u64::from_le_bytes(raw) } else { u64::from_be_bytes(raw) };
                Ok(f64::from_bits(bits))
            }
            _ => Err(it.value_error("data length must 2, 4 or 8 bytes")),
        }
    }

    #[op]
    fn complex_check(it: &mut Interp, obj: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        Ok(i64::from(matches!(obj, Value::Obj(o) if matches!(o.kind, Kind::Complex(..)))))
    }

    #[op]
    fn complex_checkexact(it: &mut Interp, obj: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        Ok(i64::from(matches!(obj, Value::Obj(o) if o.cls.is_none() && matches!(o.kind, Kind::Complex(..)))))
    }

    #[op]
    fn complex_fromccomplex(it: &mut Interp, obj: &Value) -> R<Value> {
        let (re, im) = it.complex_arg(obj)?;
        Ok(complex_value(re, im))
    }

    #[op]
    fn complex_fromdoubles(real: f64, imag: f64) -> Value {
        complex_value(real, imag)
    }

    #[op]
    fn complex_realasdouble(it: &mut Interp, obj: &Value) -> R<Value> {
        let obj = nonnull(it, obj)?;
        if let Value::Obj(o) = obj {
            if let Kind::Complex(re, _) = &o.kind {
                return Ok(Value::Float(*re));
            }
        }
        Ok(Value::Float(float_of(it, obj)?))
    }

    #[op]
    fn complex_imagasdouble(it: &mut Interp, obj: &Value) -> R<Value> {
        let obj = nonnull(it, obj)?;
        if let Value::Obj(o) = obj {
            if let Kind::Complex(_, im) = &o.kind {
                return Ok(Value::Float(*im));
            }
        }
        Ok(Value::Float(0.0))
    }

    #[op]
    fn complex_asccomplex(it: &mut Interp, obj: &Value) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let (re, im) = it.complex_arg(obj)?;
        Ok(complex_value(re, im))
    }

    #[op]
    fn _py_c_neg(it: &mut Interp, num: &Value) -> R<Value> {
        let (re, im) = it.complex_arg(num)?;
        Ok(complex_value(-re, -im))
    }

    #[op]
    fn _py_c_sum(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        let (x, y) = pair(it, a, b)?;
        Ok(with_errno(Complex::new(x.re + y.re, x.im + y.im), 0))
    }

    #[op]
    fn _py_c_diff(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        let (x, y) = pair(it, a, b)?;
        Ok(with_errno(Complex::new(x.re - y.re, x.im - y.im), 0))
    }

    #[op]
    fn _py_c_prod(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        let (x, y) = pair(it, a, b)?;
        Ok(with_errno(complex::prod(x, y), 0))
    }

    #[op]
    fn _py_c_quot(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        let (x, y) = pair(it, a, b)?;
        Ok(match complex::quot(x, y) {
            Some(z) => with_errno(z, 0),
            None => with_errno(Complex::new(0.0, 0.0), EDOM),
        })
    }

    #[op]
    fn _py_c_pow(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        let (x, y) = pair(it, a, b)?;
        let (z, e) = complex::pow_with_status(x, y);
        Ok(with_errno(z, status(e)))
    }

    #[op]
    fn _py_c_abs(it: &mut Interp, obj: &Value) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let (re, im) = it.complex_arg(obj)?;
        let (value, errno) = match complex::abs(Complex::new(re, im)) {
            Ok(r) => (r, 0),
            Err(_) => (f64::INFINITY, ERANGE),
        };
        Ok(Value::tuple(vec![Value::Float(value), Value::Int(errno)]))
    }
}
