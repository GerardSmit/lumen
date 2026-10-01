//! Container slot wrappers (`__getitem__`, `__len__`, ...) shared by the builtin collection types.

use crate::object::*;
use crate::vm::*;

type Kw<'a> = &'a [(Obj, Value)];

fn w_getitem(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__getitem__", a, 2, 2)?;
    it.native_getitem(&a[0], &a[1])
}

fn w_setitem(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__setitem__", a, 3, 3)?;
    it.native_setitem(&a[0], a[1].clone(), a[2].clone())?;
    Ok(Value::None)
}

fn w_delitem(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__delitem__", a, 2, 2)?;
    it.native_delitem(&a[0], &a[1])?;
    Ok(Value::None)
}

fn w_len(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__len__", a, 1, 1)?;
    Ok(Value::Int(it.native_len(&a[0])? as i64))
}

fn w_contains(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__contains__", a, 2, 2)?;
    Ok(Value::Bool(it.native_contains(&a[0], &a[1])?))
}

fn w_iter(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__iter__", a, 1, 1)?;
    it.native_get_iter(&a[0])
}

fn w_next(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__next__", a, 1, 1)?;
    match it.native_iter_next(&a[0])? {
        Some(v) => Ok(v),
        None => Err(it.new_exc_str("StopIteration", "")),
    }
}

fn w_iter_self(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__iter__", a, 1, 1)?;
    Ok(a[0].clone())
}

fn w_reversed(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__reversed__", a, 1, 1)?;
    let n = it.native_len(&a[0])? as i64;
    Ok(it.mk_iter(IterState::Reversed { seq: a[0].clone(), idx: n - 1 }))
}

pub fn reg_slots(it: &mut Interp, ty: &Obj, which: &[&str]) {
    let table: &[(&'static str, NativeFn)] = &[
        ("__getitem__", w_getitem),
        ("__setitem__", w_setitem),
        ("__delitem__", w_delitem),
        ("__len__", w_len),
        ("__contains__", w_contains),
        ("__iter__", w_iter),
        ("__reversed__", w_reversed),
    ];
    for (n, f) in table {
        if which.contains(n) {
            it.reg(ty, n, *f);
        }
    }
}

pub fn reg_iterator(it: &mut Interp, ty: &Obj) {
    it.reg(ty, "__iter__", w_iter_self);
    it.reg(ty, "__next__", w_next);
}
