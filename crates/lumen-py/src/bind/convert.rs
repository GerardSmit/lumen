//! Python's conversion rules for the neutral types: `PyNumber_Index` integers with CPython's
//! overflow messages, and the mapping of neutral errors to exceptions.

use crate::object::*;
use crate::vm::Interp;
use lumen_bind::IntKind;
use lumen_common::buffer::BufferError;
use lumen_common::native::{ErrorKind, NativeError};

/// `operator.index(v)` (`PyNumber_Index`): an `int` value, through `__index__` when needed.
pub fn index(it: &mut Interp, v: &Value) -> R<Value> {
    match v {
        Value::Int(_) => Ok(v.clone()),
        Value::Bool(b) => Ok(Value::Int(*b as i64)),
        Value::Obj(o) if matches!(o.kind, Kind::Int(_)) => {
            if o.cls.is_none() {
                return Ok(v.clone());
            }
            match &o.kind {
                Kind::Int(b) => Ok(Value::big(b.clone())),
                _ => unreachable!(),
            }
        }
        Value::Obj(_) => match it.user_special(v, "__index__") {
            Some(m) => {
                let r = it.call_user_special(v, &m, Vec::new())?;
                if r.is_int_like() {
                    return Ok(match r {
                        Value::Bool(b) => Value::Int(b as i64),
                        r => r,
                    });
                }
                let t = it.type_name_of(&r);
                Err(it.type_error(&format!("__index__ returned non-int (type {})", t)))
            }
            None => Err(not_integer(it, v)),
        },
        _ => Err(not_integer(it, v)),
    }
}

#[cold]
fn not_integer(it: &mut Interp, v: &Value) -> Obj {
    let t = it.type_name_of(v);
    it.type_error(&format!("'{}' object cannot be interpreted as an integer", t))
}

/// An index as i128 (big values saturate: callers only compare against narrower ranges).
fn index_i128(it: &mut Interp, v: &Value) -> R<i128> {
    match index(it, v)? {
        Value::Int(i) => Ok(i as i128),
        Value::Obj(o) => match &o.kind {
            Kind::Int(b) => Ok(b.to_i128().unwrap_or(if b.is_negative() { i128::MIN } else { i128::MAX })),
            _ => Ok(0),
        },
        _ => Ok(0),
    }
}

/// The C type CPython names in its overflow messages.
fn c_name(k: IntKind) -> &'static str {
    match (k.bits, k.signed, k.size) {
        (_, true, true) => "ssize_t",
        (_, false, true) => "size_t",
        (8, true, _) => "char",
        (16, true, _) => "short",
        (32, true, _) => "int",
        (_, true, _) => "long",
        (8, false, _) => "unsigned char",
        (16, false, _) => "unsigned short",
        (32, false, _) => "unsigned int",
        _ => "unsigned long",
    }
}

/// An integer argument of `kind` (`PyLong_AsLong` and friends): `__index__`, then CPython's
/// `OverflowError` wording when out of range.
#[inline]
pub fn to_int(it: &mut Interp, v: &Value, k: IntKind) -> R<i128> {
    let n = match v {
        Value::Int(i) => *i as i128,
        _ => index_i128(it, v)?,
    };
    if n >= k.min() && n <= k.max() {
        return Ok(n);
    }
    Err(int_overflow(it, n, k))
}

#[cold]
#[inline(never)]
fn int_overflow(it: &mut Interp, n: i128, k: IntKind) -> Obj {
    let msg = if n < 0 && !k.signed {
        if k.size { "can't convert negative value to size_t".to_string() } else { "can't convert negative int to unsigned".to_string() }
    } else {
        format!("Python int too large to convert to C {}", c_name(k))
    };
    it.overflow_err(&msg)
}

/// A neutral error as a Python exception.
pub fn native_error(it: &mut Interp, e: NativeError) -> Obj {
    let name = match e.kind {
        ErrorKind::Type => "TypeError",
        ErrorKind::Value => "ValueError",
        ErrorKind::Overflow => "OverflowError",
        ErrorKind::Index => "IndexError",
        ErrorKind::Key => "KeyError",
        ErrorKind::ZeroDivision => "ZeroDivisionError",
        ErrorKind::Runtime => "RuntimeError",
        ErrorKind::Buffer => "BufferError",
        ErrorKind::Memory => "MemoryError",
        ErrorKind::NotImplemented => "NotImplementedError",
        ErrorKind::Os(errno) => {
            let exc = it.os_error_errno(errno, None, None);
            if !e.message.is_empty() {
                it.set_exc_attr(&exc, "strerror", Value::str(&e.message));
            }
            return exc;
        }
    };
    it.new_exc_str(name, &e.message)
}

/// A shared-buffer error as CPython words it for `bytearray` / `memoryview`.
pub fn buffer_error(it: &mut Interp, e: BufferError) -> Obj {
    match e {
        BufferError::Pinned => it.new_exc_str("BufferError", "Existing exports of data: object cannot be re-sized"),
        BufferError::Detached => it.value_error("operation forbidden on released memoryview object"),
        BufferError::ReadOnly => it.type_error("cannot modify read-only memory"),
        BufferError::TooLarge => it.new_exc_str("MemoryError", ""),
        BufferError::NotResizable => it.new_exc_str("BufferError", "buffer is not resizable"),
        BufferError::OutOfBounds => it.new_exc_str("IndexError", "index out of range"),
        BufferError::Borrowed => it.new_exc_str("BufferError", "Existing exports of data: object cannot be re-sized"),
    }
}
