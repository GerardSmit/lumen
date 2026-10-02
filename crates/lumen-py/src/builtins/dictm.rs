//! `dict`, dict views, `set` and `frozenset`.

use super::numeric::{reg_binops, reg_compare};
use super::slots::{reg_method_forms, reg_slots};
use crate::containers::pydict_of;
use crate::dict::PyDict;
use crate::object::*;
use crate::vm::*;
use std::cell::RefCell;
use std::rc::Rc;

type Kw<'a> = &'a [(Obj, Value)];

fn dict_this<'a>(it: &mut Interp, a: &'a [Value], name: &str) -> R<&'a Obj> {
    match a.first() {
        Some(Value::Obj(o)) if matches!(o.kind, Kind::Dict(_)) => Ok(o),
        _ => {
            let t = a.first().map(|v| it.type_name_of(v)).unwrap_or_default();
            Err(it.type_error(&format!("descriptor '{}' for 'dict' objects doesn't apply to a '{}' object", name, t)))
        }
    }
}

fn dict_new(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    match a.first() {
        Some(Value::Obj(c)) if matches!(c.kind, Kind::Type(_)) => it.alloc_instance(c),
        _ => Err(it.type_error("dict.__new__(X): X is not a type object")),
    }
}

fn dict_init(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let d = dict_this(it, a, "__init__")?.clone();
    if a.len() > 2 {
        return Err(it.type_error(&format!("dict expected at most 1 argument, got {}", a.len() - 1)));
    }
    if let Some(src) = a.get(1) {
        it.dict_update_from(&d, src)?;
    }
    for (k, v) in kw {
        it.dict_set(&d, Value::Obj(k.clone()), v.clone())?;
    }
    Ok(Value::None)
}

fn dict_get(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("dict.get", a, 2, 3)?;
    let d = dict_this(it, a, "get")?;
    Ok(it.dict_get(d, &a[1])?.unwrap_or_else(|| a.get(2).cloned().unwrap_or(Value::None)))
}

fn setdefault(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("dict.setdefault", a, 2, 3)?;
    let d = dict_this(it, a, "setdefault")?.clone();
    if let Some(v) = it.dict_get(&d, &a[1])? {
        return Ok(v);
    }
    let v = a.get(2).cloned().unwrap_or(Value::None);
    it.dict_set(&d, a[1].clone(), v.clone())?;
    Ok(v)
}

fn dict_pop(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("dict.pop", a, 2, 3)?;
    let d = dict_this(it, a, "pop")?.clone();
    match it.dict_remove(&d, &a[1])? {
        Some(v) => Ok(v),
        None => match a.get(2) {
            Some(dflt) => Ok(dflt.clone()),
            None => Err(it.new_exc_val("KeyError", a[1].clone())),
        },
    }
}

fn popitem(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("dict.popitem", a, 1, 1)?;
    let d = dict_this(it, a, "popitem")?;
    let pd = pydict_of(d).unwrap();
    let last = pd.borrow().last_live();
    match last {
        Some(i) => {
            let e = pd.borrow_mut().remove(i);
            match e {
                Some(e) => Ok(Value::tuple(vec![e.key, e.val])),
                None => Err(it.new_exc_str("KeyError", "popitem(): dictionary is empty")),
            }
        }
        None => Err(it.new_exc_str("KeyError", "popitem(): dictionary is empty")),
    }
}

fn view(it: &mut Interp, a: &[Value], vk: ViewKind) -> R<Value> {
    it.check_args("dict view", a, 1, 1)?;
    let d = dict_this(it, a, "keys")?;
    Ok(Value::Obj(Object::new(Kind::DictView(d.clone(), vk))))
}

fn keys(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    view(it, a, ViewKind::Keys)
}
fn values(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    view(it, a, ViewKind::Values)
}
fn items(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    view(it, a, ViewKind::Items)
}

fn update(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let d = dict_this(it, a, "update")?.clone();
    if a.len() > 2 {
        return Err(it.type_error(&format!("update expected at most 1 argument, got {}", a.len() - 1)));
    }
    if let Some(src) = a.get(1) {
        it.dict_update_from(&d, src)?;
    }
    for (k, v) in kw {
        it.dict_set(&d, Value::Obj(k.clone()), v.clone())?;
    }
    Ok(Value::None)
}

fn dict_clear(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let d = dict_this(it, a, "clear")?;
    if let Some(p) = pydict_of(d) {
        p.borrow_mut().clear();
    }
    Ok(Value::None)
}

fn dict_copy(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let d = dict_this(it, a, "copy")?;
    let p = pydict_of(d).map(|p| p.borrow().clone()).unwrap_or_default();
    Ok(Value::dict(p))
}

fn fromkeys(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("dict.fromkeys", a, 2, 3)?;
    let cls = match &a[0] {
        Value::Obj(c) if matches!(c.kind, Kind::Type(_)) => c.clone(),
        _ => it.types.dict.clone(),
    };
    let d = it.call(&Value::Obj(cls), Vec::new(), Vec::new())?;
    let dflt = a.get(2).cloned().unwrap_or(Value::None);
    for k in it.iterate_to_vec(&a[1])? {
        it.setitem(&d, k, dflt.clone())?;
    }
    Ok(d)
}

fn dict_reversed(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let d = dict_this(it, a, "__reversed__")?;
    let ks = pydict_of(d).map(|p| p.borrow().keys()).unwrap_or_default();
    let rev: Vec<Value> = ks.into_iter().rev().collect();
    let l = Value::list(rev);
    it.get_iter(&l)
}

fn dict_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::string(it.native_repr(&a[0])?))
}

fn dict_ior(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__ior__", a, 2, 2)?;
    let d = dict_this(it, a, "__ior__")?.clone();
    it.dict_update_from(&d, &a[1])?;
    Ok(a[0].clone())
}

// ---- views -------------------------------------------------------------------------------------

fn view_isdisjoint(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("isdisjoint", a, 2, 2)?;
    for x in it.iterate_to_vec(&a[1])? {
        if it.native_contains(&a[0], &x)? {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(true))
}

fn view_reversed(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let mut items = it.iterate_to_vec(&a[0])?;
    items.reverse();
    let l = Value::list(items);
    it.get_iter(&l)
}

fn view_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::string(it.native_repr(&a[0])?))
}

// ---- set / frozenset ------------------------------------------------------------------------------

fn set_this<'a>(it: &mut Interp, a: &'a [Value], name: &str) -> R<&'a Obj> {
    match a.first() {
        Some(Value::Obj(o)) if matches!(o.kind, Kind::Set(_) | Kind::FrozenSet(_)) => Ok(o),
        _ => {
            let t = a.first().map(|v| it.type_name_of(v)).unwrap_or_default();
            Err(it.type_error(&format!("descriptor '{}' for 'set' objects doesn't apply to a '{}' object", name, t)))
        }
    }
}

fn set_new(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    match a.first() {
        Some(Value::Obj(c)) if matches!(c.kind, Kind::Type(_)) => it.alloc_instance(c),
        _ => Err(it.type_error("set.__new__(X): X is not a type object")),
    }
}

fn set_init(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("set", kw)?;
    it.check_args("set", &a[1.min(a.len())..], 0, 1)?;
    let s = set_this(it, a, "__init__")?.clone();
    if let Some(p) = pydict_of(&s) {
        p.borrow_mut().clear();
    }
    if let Some(src) = a.get(1) {
        for x in it.iterate_to_vec(src)? {
            it.set_add_obj(&s, x)?;
        }
    }
    Ok(Value::None)
}

fn frozenset_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let cls = match a.first() {
        Some(Value::Obj(c)) if matches!(c.kind, Kind::Type(_)) => c.clone(),
        _ => return Err(it.type_error("frozenset.__new__(X): X is not a type object")),
    };
    it.no_kwargs("frozenset", kw)?;
    it.check_args("frozenset", &a[1..], 0, 1)?;
    let items = match a.get(1) {
        Some(src) => it.iterate_to_vec(src)?,
        None => Vec::new(),
    };
    let fs = Object::new(Kind::FrozenSet(RefCell::new(PyDict::new_set())));
    for x in items {
        it.set_add_obj(&fs, x)?;
    }
    if Rc::ptr_eq(&cls, &it.types.frozenset) {
        return Ok(Value::Obj(fs));
    }
    let d = match &fs.kind {
        Kind::FrozenSet(d) => d.borrow().clone(),
        _ => PyDict::new(),
    };
    Ok(Value::Obj(Object::with_cls(cls, Kind::FrozenSet(RefCell::new(d)))))
}

fn set_add(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("set.add", a, 2, 2)?;
    let s = set_this(it, a, "add")?.clone();
    it.set_add_obj(&s, a[1].clone())?;
    Ok(Value::None)
}

fn set_hashable_key(it: &mut Interp, k: &Value) -> Value {
    if let Value::Obj(o) = k {
        if let Kind::Set(d) = &o.kind {
            let keys = d.borrow().keys();
            if let Ok(fs) = it.new_frozenset_from(keys) {
                return fs;
            }
        }
    }
    k.clone()
}

fn set_remove(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("set.remove", a, 2, 2)?;
    let s = set_this(it, a, "remove")?.clone();
    let key = set_hashable_key(it, &a[1]);
    match it.dict_remove(&s, &key)? {
        Some(_) => Ok(Value::None),
        None => Err(it.new_exc_val("KeyError", a[1].clone())),
    }
}

fn set_discard(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("set.discard", a, 2, 2)?;
    let s = set_this(it, a, "discard")?.clone();
    let key = set_hashable_key(it, &a[1]);
    it.dict_remove(&s, &key)?;
    Ok(Value::None)
}

fn set_pop(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("set.pop", a, 1, 1)?;
    let s = set_this(it, a, "pop")?;
    let pd = pydict_of(s).unwrap();
    let first = pd.borrow().next_live(0);
    match first {
        Some(i) => match pd.borrow_mut().remove(i) {
            Some(e) => Ok(e.key),
            None => Err(it.new_exc_str("KeyError", "pop from an empty set")),
        },
        None => Err(it.new_exc_str("KeyError", "pop from an empty set")),
    }
}

fn set_clear(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let s = set_this(it, a, "clear")?;
    if let Some(p) = pydict_of(s) {
        p.borrow_mut().clear();
    }
    Ok(Value::None)
}

fn set_copy(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let s = set_this(it, a, "copy")?;
    let d = pydict_of(s).map(|p| p.borrow().clone()).unwrap_or_default();
    Ok(Value::Obj(Object::new(match &s.kind {
        Kind::FrozenSet(_) => Kind::FrozenSet(RefCell::new(d)),
        _ => Kind::Set(RefCell::new(d)),
    })))
}

fn as_set_value(it: &mut Interp, v: &Value) -> R<Value> {
    if let Value::Obj(o) = v {
        if matches!(o.kind, Kind::Set(_) | Kind::FrozenSet(_)) {
            return Ok(v.clone());
        }
    }
    let items = it.iterate_to_vec(v)?;
    it.new_set(items)
}

fn set_algebra(it: &mut Interp, a: &[Value], op: crate::ast::BinOp, name: &str) -> R<Value> {
    let s = set_this(it, a, name)?.clone();
    let mut cur = Value::Obj(s.clone());
    if a.len() == 1 {
        return set_copy(it, a, &[]);
    }
    for other in &a[1..] {
        let o = as_set_value(it, other)?;
        cur = it.set_binop(op, &cur, &o)?.unwrap_or(Value::None);
    }
    if let Value::Obj(c) = &cur {
        if Rc::ptr_eq(c, &s) {
            return set_copy(it, a, &[]);
        }
    }
    Ok(cur)
}

fn union(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    set_algebra(it, a, crate::ast::BinOp::BitOr, "union")
}
fn intersection(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    set_algebra(it, a, crate::ast::BinOp::BitAnd, "intersection")
}
fn difference(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    set_algebra(it, a, crate::ast::BinOp::Sub, "difference")
}
fn symmetric_difference(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("symmetric_difference", a, 2, 2)?;
    set_algebra(it, a, crate::ast::BinOp::BitXor, "symmetric_difference")
}

fn set_update_with(it: &mut Interp, a: &[Value], op: crate::ast::BinOp, name: &str) -> R<Value> {
    let s = set_this(it, a, name)?.clone();
    let res = set_algebra(it, a, op, name)?;
    if let (Value::Obj(r), Some(dst)) = (&res, pydict_of(&s)) {
        if let Some(src) = pydict_of(r) {
            *dst.borrow_mut() = src.borrow().clone();
        }
    }
    Ok(Value::None)
}

fn set_update(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    set_update_with(it, a, crate::ast::BinOp::BitOr, "update")
}
fn intersection_update(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    set_update_with(it, a, crate::ast::BinOp::BitAnd, "intersection_update")
}
fn difference_update(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    set_update_with(it, a, crate::ast::BinOp::Sub, "difference_update")
}
fn symmetric_difference_update(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("symmetric_difference_update", a, 2, 2)?;
    set_update_with(it, a, crate::ast::BinOp::BitXor, "symmetric_difference_update")
}

fn issubset(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("issubset", a, 2, 2)?;
    set_this(it, a, "issubset")?;
    let o = as_set_value(it, &a[1])?;
    Ok(Value::Bool(it.native_compare(crate::ast::CmpOp::LtE, &a[0], &o)?.unwrap_or(false)))
}

fn issuperset(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("issuperset", a, 2, 2)?;
    set_this(it, a, "issuperset")?;
    let o = as_set_value(it, &a[1])?;
    Ok(Value::Bool(it.native_compare(crate::ast::CmpOp::GtE, &a[0], &o)?.unwrap_or(false)))
}

fn set_isdisjoint(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("isdisjoint", a, 2, 2)?;
    let s = set_this(it, a, "isdisjoint")?.clone();
    for x in it.iterate_to_vec(&a[1])? {
        if it.set_contains(&s, &x)? {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(true))
}

fn set_ior(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__ior__", a, 2, 2)?;
    set_update_with(it, a, crate::ast::BinOp::BitOr, "__ior__")?;
    Ok(a[0].clone())
}

fn set_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::string(it.native_repr(&a[0])?))
}

fn set_hash(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(it.native_hash(&a[0])?))
}

pub fn init(it: &mut Interp) {
    let (dict, set, frozenset) = (it.types.dict.clone(), it.types.set.clone(), it.types.frozenset.clone());
    it.reg_new(&dict, dict_new);
    it.reg(&dict, "__init__", dict_init);
    let dm: &[(&'static str, NativeFn)] = &[
        ("get", dict_get),
        ("setdefault", setdefault),
        ("pop", dict_pop),
        ("popitem", popitem),
        ("keys", keys),
        ("values", values),
        ("items", items),
        ("update", update),
        ("clear", dict_clear),
        ("copy", dict_copy),
        ("__reversed__", dict_reversed),
        ("__repr__", dict_repr),
        ("__ior__", dict_ior),
    ];
    for (n, f) in dm {
        it.reg(&dict, n, *f);
    }
    it.reg_class(&dict, "fromkeys", fromkeys);
    if let Some(d) = dict.dict.borrow().as_ref() {
        dict_set_str(d, "__hash__", Value::None);
    }
    reg_slots(it, &dict, &["__setitem__", "__delitem__", "__len__", "__iter__"]);
    reg_method_forms(&dict, &["__getitem__", "__contains__"]);
    reg_binops(it, &dict, &["__or__", "__ror__"]);
    reg_compare(it, &dict, false);

    for vt in [it.types.dict_keys.clone(), it.types.dict_values.clone(), it.types.dict_items.clone()] {
        it.reg(&vt, "isdisjoint", view_isdisjoint);
        it.reg(&vt, "__reversed__", view_reversed);
        it.reg(&vt, "__repr__", view_repr);
        reg_slots(it, &vt, &["__len__", "__contains__", "__iter__"]);
        reg_binops(it, &vt, &["__and__", "__rand__", "__or__", "__ror__", "__sub__", "__rsub__", "__xor__", "__rxor__"]);
        reg_compare(it, &vt, true);
    }

    it.reg_new(&set, set_new);
    it.reg(&set, "__init__", set_init);
    it.reg_new(&frozenset, frozenset_new);
    let sm: &[(&'static str, NativeFn)] = &[
        ("copy", set_copy),
        ("union", union),
        ("intersection", intersection),
        ("difference", difference),
        ("symmetric_difference", symmetric_difference),
        ("issubset", issubset),
        ("issuperset", issuperset),
        ("isdisjoint", set_isdisjoint),
        ("__repr__", set_repr),
    ];
    for (n, f) in sm {
        it.reg(&set, n, *f);
        it.reg(&frozenset, n, *f);
    }
    it.reg(&frozenset, "__hash__", set_hash);
    let mutating: &[(&'static str, NativeFn)] = &[
        ("add", set_add),
        ("remove", set_remove),
        ("discard", set_discard),
        ("pop", set_pop),
        ("clear", set_clear),
        ("update", set_update),
        ("intersection_update", intersection_update),
        ("difference_update", difference_update),
        ("symmetric_difference_update", symmetric_difference_update),
        ("__ior__", set_ior),
    ];
    for (n, f) in mutating {
        it.reg(&set, n, *f);
    }
    if let Some(d) = set.dict.borrow().as_ref() {
        dict_set_str(d, "__hash__", Value::None);
    }
    for t in [&set, &frozenset] {
        reg_slots(it, t, &["__len__", "__iter__"]);
        reg_method_forms(t, &["__contains__"]);
        reg_binops(it, t, &["__and__", "__rand__", "__or__", "__ror__", "__sub__", "__rsub__", "__xor__", "__rxor__"]);
        reg_compare(it, t, true);
    }
}
