//! Container slot wrappers (`__getitem__`, `__len__`, ...) shared by the builtin collection types.

use crate::bind::This;
use crate::object::*;
use crate::vm::*;

/// The slot wrappers, declared once and installed into each container type that has the slot.
#[lumen_bind::class(name = "slots", hint(py(shared)))]
pub struct Slots;

#[lumen_bind::methods]
impl Slots {
    #[proto(getitem)]
    fn getitem(slf: This<&Value>, it: &mut Interp, key: &Value) -> R<Value> {
        it.native_getitem(&slf, key)
    }

    #[proto(setitem)]
    fn setitem(slf: This<&Value>, it: &mut Interp, key: &Value, value: &Value) -> R<()> {
        it.native_setitem(&slf, key.clone(), value.clone())
    }

    #[proto(delitem)]
    fn delitem(slf: This<&Value>, it: &mut Interp, key: &Value) -> R<()> {
        it.native_delitem(&slf, key)
    }

    #[proto(len)]
    fn len(slf: This<&Value>, it: &mut Interp) -> R<usize> {
        it.native_len(&slf)
    }

    #[proto(contains)]
    fn contains(slf: This<&Value>, it: &mut Interp, key: &Value) -> R<bool> {
        it.native_contains(&slf, key)
    }

    #[proto(iter)]
    fn iter(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        it.native_get_iter(&slf)
    }

    #[proto(reversed)]
    fn reversed(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        let seq: &Value = &slf;
        if let Value::Obj(o) = seq {
            if let Kind::Range(r) = &o.kind {
                // A range reverses to a range iterator, as in CPython (`__setstate__` counts items).
                let n = crate::ops::slice_len(r.start, r.stop, r.step) as i64;
                let last = (n - 1)
                    .checked_mul(r.step)
                    .and_then(|d| r.start.checked_add(d));
                let stop = r.start.checked_sub(r.step);
                if let (Some(cur), Some(stop), Some(step)) = (last, stop, r.step.checked_neg()) {
                    let cur = if n == 0 { stop } else { cur };
                    return Ok(it.mk_iter(IterState::Range { cur, stop, step }));
                }
            }
        }
        let n = it.native_len(seq)? as i64;
        Ok(it.mk_iter(IterState::Reversed {
            seq: seq.clone(),
            idx: n - 1,
        }))
    }
}

/// `__getitem__` / `__contains__` as the methods (not slot wrappers) some types define over the
/// slot: `list` and `dict` (`__getitem__`), `dict`, `set` and `frozenset` (`__contains__`).
#[lumen_bind::class(name = "methods", hint(py(shared)))]
pub struct MethodForms;

#[lumen_bind::methods]
impl MethodForms {
    #[method(name = "__getitem__")]
    fn getitem(slf: This<&Value>, it: &mut Interp, key: &Value) -> R<Value> {
        it.native_getitem(&slf, key)
    }

    #[method(name = "__contains__")]
    fn contains(slf: This<&Value>, it: &mut Interp, key: &Value) -> R<bool> {
        it.native_contains(&slf, key)
    }
}

/// The iterator protocol of a builtin iterator type.
#[lumen_bind::class(name = "iterator", hint(py(shared)))]
pub struct Iterator;

#[lumen_bind::methods]
impl Iterator {
    #[proto(iter)]
    fn iter(slf: This<Value>) -> Value {
        slf.0
    }

    #[proto(next)]
    fn next(slf: This<&Value>, it: &mut Interp) -> R<Option<Value>> {
        it.native_iter_next(&slf)
    }
}

pub fn reg_slots(_it: &mut Interp, ty: &Obj, which: &[&str]) {
    crate::bind::install_into::<Slots>(ty, which);
}

/// The method forms of `__getitem__` / `__contains__` named in `which`.
pub fn reg_method_forms(ty: &Obj, which: &[&str]) {
    crate::bind::install_into::<MethodForms>(ty, which);
}

pub fn reg_iterator(_it: &mut Interp, ty: &Obj) {
    crate::bind::install_into::<Iterator>(ty, &["__iter__", "__next__"]);
}
