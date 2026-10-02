//! Argument parsing and conversion helpers shared by the `_decimal` classes.

use crate::bind::KwArgs;
use crate::object::*;
use crate::vm::Interp;

/// The value of a `float` (or a subclass instance).
pub fn float_of(v: &Value) -> Option<f64> {
    match v {
        Value::Float(f) => Some(*f),
        Value::Obj(o) => match &o.kind {
            Kind::Float(f) => Some(*f),
            _ => None,
        },
        _ => None,
    }
}

/// The parts of a `complex` (or a subclass instance).
pub fn complex_of(v: &Value) -> Option<(f64, f64)> {
    match v {
        Value::Obj(o) => match &o.kind {
            Kind::Complex(re, im) => Some((*re, *im)),
            _ => None,
        },
        _ => None,
    }
}

/// `PyArg_ParseTupleAndKeywords` with a `"O..|O.."` format and no function name: positional and
/// keyword arguments into one slot per name, `min` of them required.
pub fn parse_args(it: &mut Interp, args: &[Value], kw: KwArgs<'_>, names: &[&str], min: usize) -> R<Vec<Option<Value>>> {
    let total = args.len() + kw.len();
    let n = names.len();
    if total > n {
        let msg = format!(
            "function takes at most {} {}argument{} ({} given)",
            n,
            if args.is_empty() { "keyword " } else { "" },
            if n == 1 { "" } else { "s" },
            total
        );
        return Err(it.type_error(&msg));
    }
    let mut out: Vec<Option<Value>> = vec![None; n];
    for (slot, a) in out.iter_mut().zip(args) {
        *slot = Some(a.clone());
    }
    for (k, v) in kw.iter() {
        match names.iter().position(|name| *name == k) {
            Some(i) => {
                if out[i].is_some() {
                    let msg = format!("argument for function given by name ('{}') and position ({})", k, i + 1);
                    return Err(it.type_error(&msg));
                }
                out[i] = Some(v.clone());
            }
            None => {
                let msg = format!("this function got an unexpected keyword argument '{}'", k);
                return Err(it.type_error(&msg));
            }
        }
    }
    for (i, slot) in out.iter().enumerate().take(min) {
        if slot.is_none() {
            let msg = format!("function missing required argument '{}' (pos {})", names[i], i + 1);
            return Err(it.type_error(&msg));
        }
    }
    Ok(out)
}

/// `PyLong_AsSsize_t` on an exact or subclassed `int`.
pub fn ssize(it: &mut Interp, v: &Value) -> R<i64> {
    match v.as_bigint() {
        Some(b) => match b.to_i64() {
            Some(i) => Ok(i),
            None => Err(it.overflow_err("Python int too large to convert to C ssize_t")),
        },
        None => Err(it.type_error("an integer is required")),
    }
}

pub fn key_error(it: &mut Interp, msg: &str) -> Obj {
    it.new_exc_str("KeyError", msg)
}

pub fn memory_error(it: &mut Interp) -> Obj {
    it.new_exc_str("MemoryError", "")
}
