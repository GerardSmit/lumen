//! `_testcapi` wrappers of the abstract object API (`PyObject_*`, `PyNumber_*`, `PySequence_*`,
//! `PyMapping_*`). A `None` argument stands for a `NULL` pointer, as in CPython's `NULLABLE`.

use super::{builtin, name_obj, nonnull, system_error};
use crate::ast::{BinOp, CmpOp};
use crate::bytecode::UnOp;
use crate::object::*;
use crate::vm::Interp;

fn has_getitem(it: &Interp, v: &Value) -> bool {
    match v {
        Value::Obj(o) => match &o.kind {
            Kind::List(_) | Kind::Tuple(_) | Kind::Dict(_) | Kind::Str(_) | Kind::Bytes(_) | Kind::ByteArray(_) | Kind::Range(_) => true,
            _ => it.lookup_mro(&it.type_of(v), "__getitem__").is_some(),
        },
        _ => false,
    }
}

fn is_sequence_like(it: &Interp, v: &Value) -> bool {
    if let Value::Obj(o) = v {
        if matches!(o.kind, Kind::Dict(_)) {
            return false;
        }
    }
    has_getitem(it, v)
}

fn slice_value(i1: i64, i2: i64) -> Value {
    Value::Obj(Object::new(Kind::Slice(Value::Int(i1), Value::Int(i2), Value::None)))
}

fn call_builtin(it: &mut Interp, name: &str, args: Vec<Value>) -> R<Value> {
    let f = builtin(it, name);
    it.call(&f, args, Vec::new())
}

fn require_sequence(it: &mut Interp, v: &Value, what: &str) -> R<()> {
    if is_sequence_like(it, v) {
        return Ok(());
    }
    let t = it.tp_name_of(v);
    Err(it.type_error(&format!("'{t}' object does not {what}")))
}

fn binary(it: &mut Interp, op: BinOp, a: &Value, b: &Value) -> R<Value> {
    let a = nonnull(it, a)?;
    let b = nonnull(it, b)?;
    it.binary_op(op, a, b)
}

fn inplace(it: &mut Interp, op: BinOp, a: &Value, b: &Value) -> R<Value> {
    let a = nonnull(it, a)?;
    let b = nonnull(it, b)?;
    it.inplace_op(op, a.clone(), b)
}

fn unary(it: &mut Interp, op: UnOp, a: &Value) -> R<Value> {
    let a = nonnull(it, a)?;
    it.unary_op(op, a)
}

fn method_list(it: &mut Interp, obj: &Value, name: &str) -> R<Value> {
    let obj = nonnull(it, obj)?;
    let r = it.call_method(obj, name, Vec::new())?;
    match it.iterate_to_vec(&r) {
        Ok(items) => Ok(Value::list(items)),
        Err(e) if it.exc_is(&e, "TypeError") => Err(it.type_error(&format!("o.{name}() are not iterable"))),
        Err(e) => Err(e),
    }
}

fn index_error_text(it: &mut Interp, v: &Value) -> Obj {
    let t = it.tp_name_of(v);
    it.type_error(&format!("'{t}' object cannot be interpreted as an integer"))
}

#[lumen_bind::module(name = "_testcapi")]
pub mod abstractm {
    use super::*;

    #[op]
    fn object_repr(it: &mut Interp, obj: &Value) -> R<Value> {
        if obj.is_none() {
            return Ok(Value::str("<NULL>"));
        }
        Ok(Value::string(it.repr_of(obj)?))
    }

    #[op]
    fn object_ascii(it: &mut Interp, obj: &Value) -> R<Value> {
        if obj.is_none() {
            return Ok(Value::str("<NULL>"));
        }
        call_builtin(it, "ascii", vec![obj.clone()])
    }

    #[op]
    fn object_str(it: &mut Interp, obj: &Value) -> R<Value> {
        if obj.is_none() {
            return Ok(Value::str("<NULL>"));
        }
        it.str_value(obj)
    }

    #[op]
    fn object_bytes(it: &mut Interp, obj: &Value) -> R<Value> {
        let obj = nonnull(it, obj)?;
        if let Value::Obj(o) = obj {
            if o.cls.is_none() && matches!(o.kind, Kind::Bytes(_)) {
                return Ok(obj.clone());
            }
        }
        if it.user_special(obj, "__bytes__").is_some() {
            let r = it.call_method(obj, "__bytes__", Vec::new())?;
            if !matches!(&r, Value::Obj(o) if matches!(o.kind, Kind::Bytes(_))) {
                let t = it.tp_name_of(&r);
                return Err(it.type_error(&format!("__bytes__ returned non-bytes (type {t})")));
            }
            return Ok(r);
        }
        Ok(Value::bytes(it.bytes_from_object(obj)?))
    }

    #[op]
    fn object_getattr(it: &mut Interp, obj: &Value, name: &Value) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let name = nonnull(it, name)?;
        let name = name_obj(it, name)?;
        it.get_attr(obj, &name)
    }

    #[op]
    fn object_getattrstring(it: &mut Interp, obj: &Value, name: &str) -> R<Value> {
        let obj = nonnull(it, obj)?;
        it.get_attr_str(obj, name)
    }

    #[op]
    fn object_hasattr(it: &mut Interp, obj: &Value, name: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        let name = nonnull(it, name)?;
        if name.as_str().is_none() {
            return Ok(0);
        }
        let name = name_obj(it, name)?;
        Ok(i64::from(it.get_attr(obj, &name).is_ok()))
    }

    #[op]
    fn object_hasattrstring(it: &mut Interp, obj: &Value, name: &str) -> R<i64> {
        let obj = nonnull(it, obj)?;
        Ok(i64::from(it.get_attr_str(obj, name).is_ok()))
    }

    #[op]
    fn object_setattr(it: &mut Interp, obj: &Value, name: &Value, value: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        let name = nonnull(it, name)?;
        let value = nonnull(it, value)?;
        let name = name_obj(it, name)?;
        it.set_attr(obj, &name, value.clone())?;
        Ok(0)
    }

    #[op]
    fn object_setattrstring(it: &mut Interp, obj: &Value, name: &str, value: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        let value = nonnull(it, value)?;
        it.set_attr_str(obj, name, value.clone())?;
        Ok(0)
    }

    #[op]
    fn object_delattr(it: &mut Interp, obj: &Value, name: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        let name = nonnull(it, name)?;
        let name = name_obj(it, name)?;
        it.del_attr(obj, &name)?;
        Ok(0)
    }

    #[op]
    fn object_delattrstring(it: &mut Interp, obj: &Value, name: &str) -> R<i64> {
        let obj = nonnull(it, obj)?;
        let name = name_obj(it, &Value::str(name))?;
        it.del_attr(obj, &name)?;
        Ok(0)
    }

    #[op]
    fn mapping_check(it: &mut Interp, obj: &Value) -> i64 {
        i64::from(!obj.is_none() && has_getitem(it, obj))
    }

    #[op]
    fn mapping_size(it: &mut Interp, obj: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        Ok(it.len_of(obj)? as i64)
    }

    #[op]
    fn mapping_length(it: &mut Interp, obj: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        Ok(it.len_of(obj)? as i64)
    }

    #[op]
    fn object_getitem(it: &mut Interp, mapping: &Value, key: &Value) -> R<Value> {
        let mapping = nonnull(it, mapping)?;
        let key = nonnull(it, key)?;
        it.getitem(mapping, key)
    }

    #[op]
    fn mapping_getitemstring(it: &mut Interp, mapping: &Value, key: &str) -> R<Value> {
        let mapping = nonnull(it, mapping)?;
        it.getitem(mapping, &Value::str(key))
    }

    #[op]
    fn mapping_haskey(it: &mut Interp, mapping: &Value, key: &Value) -> R<i64> {
        let mapping = nonnull(it, mapping)?;
        let key = nonnull(it, key)?;
        Ok(i64::from(it.getitem(mapping, key).is_ok()))
    }

    #[op]
    fn mapping_haskeystring(it: &mut Interp, mapping: &Value, key: &str) -> R<i64> {
        let mapping = nonnull(it, mapping)?;
        Ok(i64::from(it.getitem(mapping, &Value::str(key)).is_ok()))
    }

    #[op]
    fn object_setitem(it: &mut Interp, mapping: &Value, key: &Value, value: &Value) -> R<i64> {
        let mapping = nonnull(it, mapping)?;
        let key = nonnull(it, key)?;
        let value = nonnull(it, value)?;
        it.setitem(mapping, key.clone(), value.clone())?;
        Ok(0)
    }

    #[op]
    fn mapping_setitemstring(it: &mut Interp, mapping: &Value, key: &str, value: &Value) -> R<i64> {
        let mapping = nonnull(it, mapping)?;
        let value = nonnull(it, value)?;
        it.setitem(mapping, Value::str(key), value.clone())?;
        Ok(0)
    }

    #[op]
    fn object_delitem(it: &mut Interp, mapping: &Value, key: &Value) -> R<i64> {
        let mapping = nonnull(it, mapping)?;
        let key = nonnull(it, key)?;
        it.delitem(mapping, key)?;
        Ok(0)
    }

    #[op]
    fn mapping_delitem(it: &mut Interp, mapping: &Value, key: &Value) -> R<i64> {
        let mapping = nonnull(it, mapping)?;
        let key = nonnull(it, key)?;
        it.delitem(mapping, key)?;
        Ok(0)
    }

    #[op]
    fn mapping_delitemstring(it: &mut Interp, mapping: &Value, key: &str) -> R<i64> {
        let mapping = nonnull(it, mapping)?;
        it.delitem(mapping, &Value::str(key))?;
        Ok(0)
    }

    #[op]
    fn mapping_keys(it: &mut Interp, obj: &Value) -> R<Value> {
        method_list(it, obj, "keys")
    }

    #[op]
    fn mapping_values(it: &mut Interp, obj: &Value) -> R<Value> {
        method_list(it, obj, "values")
    }

    #[op]
    fn mapping_items(it: &mut Interp, obj: &Value) -> R<Value> {
        method_list(it, obj, "items")
    }

    #[op]
    fn sequence_check(it: &mut Interp, obj: &Value) -> i64 {
        i64::from(!obj.is_none() && is_sequence_like(it, obj))
    }

    #[op]
    fn sequence_size(it: &mut Interp, obj: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        Ok(it.len_of(obj)? as i64)
    }

    #[op]
    fn sequence_length(it: &mut Interp, obj: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        Ok(it.len_of(obj)? as i64)
    }

    #[op]
    fn sequence_concat(it: &mut Interp, seq1: &Value, seq2: &Value) -> R<Value> {
        let s1 = nonnull(it, seq1)?;
        require_sequence(it, s1, "support concatenation")?;
        binary(it, BinOp::Add, seq1, seq2)
    }

    #[op]
    fn sequence_repeat(it: &mut Interp, seq: &Value, count: i64) -> R<Value> {
        let s = nonnull(it, seq)?;
        require_sequence(it, s, "support repeat")?;
        it.binary_op(BinOp::Mult, s, &Value::Int(count))
    }

    #[op]
    fn sequence_inplaceconcat(it: &mut Interp, seq1: &Value, seq2: &Value) -> R<Value> {
        let s1 = nonnull(it, seq1)?;
        require_sequence(it, s1, "support concatenation")?;
        inplace(it, BinOp::Add, seq1, seq2)
    }

    #[op]
    fn sequence_inplacerepeat(it: &mut Interp, seq: &Value, count: i64) -> R<Value> {
        let s = nonnull(it, seq)?;
        require_sequence(it, s, "support repeat")?;
        it.inplace_op(BinOp::Mult, s.clone(), &Value::Int(count))
    }

    #[op]
    fn sequence_getitem(it: &mut Interp, seq: &Value, i: i64) -> R<Value> {
        let seq = nonnull(it, seq)?;
        require_sequence(it, seq, "support indexing")?;
        let i = if i < 0 { i + it.len_of(seq)? as i64 } else { i };
        it.getitem(seq, &Value::Int(i))
    }

    #[op]
    fn sequence_setitem(it: &mut Interp, seq: &Value, i: i64, value: &Value) -> R<i64> {
        let seq = nonnull(it, seq)?;
        let value = nonnull(it, value)?;
        require_sequence(it, seq, "support item assignment")?;
        let i = if i < 0 { i + it.len_of(seq)? as i64 } else { i };
        it.setitem(seq, Value::Int(i), value.clone())?;
        Ok(0)
    }

    #[op]
    fn sequence_delitem(it: &mut Interp, seq: &Value, i: i64) -> R<i64> {
        let seq = nonnull(it, seq)?;
        require_sequence(it, seq, "support item deletion")?;
        let i = if i < 0 { i + it.len_of(seq)? as i64 } else { i };
        it.delitem(seq, &Value::Int(i))?;
        Ok(0)
    }

    #[op]
    fn sequence_setslice(it: &mut Interp, seq: &Value, i1: i64, i2: i64, obj: &Value) -> R<i64> {
        let seq = nonnull(it, seq)?;
        let obj = nonnull(it, obj)?;
        require_sequence(it, seq, "support slice assignment")?;
        it.setitem(seq, slice_value(i1, i2), obj.clone())?;
        Ok(0)
    }

    #[op]
    fn sequence_delslice(it: &mut Interp, seq: &Value, i1: i64, i2: i64) -> R<i64> {
        let seq = nonnull(it, seq)?;
        require_sequence(it, seq, "support slice deletion")?;
        it.delitem(seq, &slice_value(i1, i2))?;
        Ok(0)
    }

    #[op]
    fn sequence_count(it: &mut Interp, seq: &Value, value: &Value) -> R<i64> {
        let seq = nonnull(it, seq)?;
        let value = nonnull(it, value)?;
        let mut n = 0;
        for item in it.iterate_to_vec(seq)? {
            if item.is(value) || it.values_eq(&item, value)? {
                n += 1;
            }
        }
        Ok(n)
    }

    #[op]
    fn sequence_contains(it: &mut Interp, seq: &Value, value: &Value) -> R<i64> {
        let seq = nonnull(it, seq)?;
        let value = nonnull(it, value)?;
        Ok(i64::from(it.contains(seq, value)?))
    }

    #[op]
    fn sequence_index(it: &mut Interp, seq: &Value, value: &Value) -> R<i64> {
        let seq = nonnull(it, seq)?;
        let value = nonnull(it, value)?;
        for (i, item) in it.iterate_to_vec(seq)?.into_iter().enumerate() {
            if item.is(value) || it.values_eq(&item, value)? {
                return Ok(i as i64);
            }
        }
        Err(it.value_error("sequence.index(x): x not in sequence"))
    }

    #[op]
    fn sequence_list(it: &mut Interp, obj: &Value) -> R<Value> {
        let obj = nonnull(it, obj)?;
        Ok(Value::list(it.iterate_to_vec(obj)?))
    }

    #[op]
    fn sequence_tuple(it: &mut Interp, obj: &Value) -> R<Value> {
        let obj = nonnull(it, obj)?;
        Ok(Value::tuple(it.iterate_to_vec(obj)?))
    }

    #[op]
    fn number_check(it: &mut Interp, obj: &Value) -> i64 {
        i64::from(!obj.is_none() && it.number_check(obj))
    }

    #[op] fn number_add(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { binary(it, BinOp::Add, a, b) }
    #[op] fn number_subtract(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { binary(it, BinOp::Sub, a, b) }
    #[op] fn number_multiply(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { binary(it, BinOp::Mult, a, b) }
    #[op] fn number_matrixmultiply(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { binary(it, BinOp::MatMult, a, b) }
    #[op] fn number_floordivide(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { binary(it, BinOp::FloorDiv, a, b) }
    #[op] fn number_truedivide(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { binary(it, BinOp::Div, a, b) }
    #[op] fn number_remainder(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { binary(it, BinOp::Mod, a, b) }
    #[op] fn number_lshift(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { binary(it, BinOp::LShift, a, b) }
    #[op] fn number_rshift(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { binary(it, BinOp::RShift, a, b) }
    #[op] fn number_and(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { binary(it, BinOp::BitAnd, a, b) }
    #[op] fn number_xor(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { binary(it, BinOp::BitXor, a, b) }
    #[op] fn number_or(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { binary(it, BinOp::BitOr, a, b) }
    #[op] fn number_inplaceadd(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { inplace(it, BinOp::Add, a, b) }
    #[op] fn number_inplacesubtract(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { inplace(it, BinOp::Sub, a, b) }
    #[op] fn number_inplacemultiply(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { inplace(it, BinOp::Mult, a, b) }
    #[op] fn number_inplacematrixmultiply(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { inplace(it, BinOp::MatMult, a, b) }
    #[op] fn number_inplacefloordivide(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { inplace(it, BinOp::FloorDiv, a, b) }
    #[op] fn number_inplacetruedivide(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { inplace(it, BinOp::Div, a, b) }
    #[op] fn number_inplaceremainder(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { inplace(it, BinOp::Mod, a, b) }
    #[op] fn number_inplacelshift(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { inplace(it, BinOp::LShift, a, b) }
    #[op] fn number_inplacershift(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { inplace(it, BinOp::RShift, a, b) }
    #[op] fn number_inplaceand(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { inplace(it, BinOp::BitAnd, a, b) }
    #[op] fn number_inplacexor(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { inplace(it, BinOp::BitXor, a, b) }
    #[op] fn number_inplaceor(it: &mut Interp, a: &Value, b: &Value) -> R<Value> { inplace(it, BinOp::BitOr, a, b) }
    #[op] fn number_negative(it: &mut Interp, a: &Value) -> R<Value> { unary(it, UnOp::Neg, a) }
    #[op] fn number_positive(it: &mut Interp, a: &Value) -> R<Value> { unary(it, UnOp::Pos, a) }
    #[op] fn number_invert(it: &mut Interp, a: &Value) -> R<Value> { unary(it, UnOp::Invert, a) }

    #[op]
    fn number_absolute(it: &mut Interp, a: &Value) -> R<Value> {
        let a = nonnull(it, a)?;
        call_builtin(it, "abs", vec![a.clone()])
    }

    #[op]
    fn number_divmod(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        let a = nonnull(it, a)?;
        let b = nonnull(it, b)?;
        call_builtin(it, "divmod", vec![a.clone(), b.clone()])
    }

    #[op]
    fn number_power(it: &mut Interp, a: &Value, b: &Value, c: Option<&Value>) -> R<Value> {
        let a = nonnull(it, a)?;
        let b = nonnull(it, b)?;
        let mut args = vec![a.clone(), b.clone()];
        if let Some(c) = c.filter(|c| !c.is_none()) {
            args.push(c.clone());
        }
        call_builtin(it, "pow", args)
    }

    #[op]
    fn number_inplacepower(it: &mut Interp, a: &Value, b: &Value, c: Option<&Value>) -> R<Value> {
        let a = nonnull(it, a)?;
        let b = nonnull(it, b)?;
        match c.filter(|c| !c.is_none()) {
            None => it.inplace_op(BinOp::Pow, a.clone(), b),
            Some(c) => call_builtin(it, "pow", vec![a.clone(), b.clone(), c.clone()]),
        }
    }

    #[op]
    fn number_long(it: &mut Interp, a: &Value) -> R<Value> {
        let a = nonnull(it, a)?;
        call_builtin(it, "int", vec![a.clone()])
    }

    #[op]
    fn number_float(it: &mut Interp, a: &Value) -> R<Value> {
        let a = nonnull(it, a)?;
        call_builtin(it, "float", vec![a.clone()])
    }

    #[op]
    fn number_index(it: &mut Interp, a: &Value) -> R<Value> {
        let a = nonnull(it, a)?;
        if !it.has_index(a) {
            return Err(index_error_text(it, a));
        }
        let m = it.import_module("operator")?;
        let f = it.get_attr_str(&Value::Obj(m), "index")?;
        it.call(&f, vec![a.clone()], Vec::new())
    }

    #[op]
    fn number_tobase(it: &mut Interp, n: &Value, base: i64) -> R<Value> {
        let n = nonnull(it, n)?;
        if !matches!(base, 2 | 8 | 10 | 16) {
            return Err(system_error(it, "PyNumber_ToBase: base must be 2, 8, 10 or 16"));
        }
        if !it.has_index(n) {
            return Err(index_error_text(it, n));
        }
        match base {
            2 => call_builtin(it, "bin", vec![n.clone()]),
            8 => call_builtin(it, "oct", vec![n.clone()]),
            16 => call_builtin(it, "hex", vec![n.clone()]),
            _ => {
                let i = call_builtin(it, "int", vec![n.clone()])?;
                it.str_value(&i)
            }
        }
    }

    #[op]
    fn number_asssizet(it: &mut Interp, o: &Value, exc: &Value) -> R<i64> {
        let o = nonnull(it, o)?;
        if !it.has_index(o) {
            return Err(index_error_text(it, o));
        }
        let big = call_builtin(it, "int", vec![o.clone()])?;
        match it.index_of(&big) {
            Ok(n) => Ok(n),
            Err(e) if it.exc_is(&e, "OverflowError") => {
                if exc.is_none() {
                    let negative = it.rich_compare(CmpOp::Lt, &big, &Value::Int(0))?;
                    return Ok(if it.truthy(&negative)? { i64::MIN } else { i64::MAX });
                }
                let msg = format!("cannot fit '{}' into an index-sized integer", it.tp_name_of(o));
                match it.call(exc, vec![Value::string(msg)], Vec::new())? {
                    Value::Obj(e) => Err(e),
                    _ => Err(system_error(it, "exception class expected")),
                }
            }
            Err(e) => Err(e),
        }
    }
}
