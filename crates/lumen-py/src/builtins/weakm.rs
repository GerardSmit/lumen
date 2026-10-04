//! `_weakref`: `ref`, `proxy` and friends, backed by `Rc::downgrade`.

use super::native::*;
use crate::ast::{BinOp, CmpOp};
use crate::object::*;
use crate::vm::*;
use crate::weak::{self, ProxyData, WeakRefData};
use std::rc::Rc;

fn weak_target(it: &mut Interp, v: &Value, what: &str) -> R<Obj> {
    let ok = match v {
        Value::Obj(o) => match &o.kind {
            Kind::Str(_)
            | Kind::Int(_)
            | Kind::Float(_)
            | Kind::Complex(..)
            | Kind::Tuple(_)
            | Kind::List(_)
            | Kind::Dict(_)
            | Kind::Bytes(_)
            | Kind::ByteArray(_) => o.cls.is_some(),
            Kind::Slice(..)
            | Kind::Range(_)
            | Kind::BigRange(_)
            | Kind::Iter(_)
            | Kind::Cell(_)
            | Kind::Code(_) => o.cls.is_some(),
            _ => true,
        },
        _ => false,
    };
    match v {
        Value::Obj(o) if ok => Ok(o.clone()),
        _ => {
            let t = it.type_name_of(v);
            Err(it.type_error(&format!(
                "cannot create weak reference to '{}' object{}",
                t, what
            )))
        }
    }
}

fn ref_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args(
        "__new__",
        &a[1.min(a.len())..],
        kw,
        &["object", "callback"],
        1,
    )?;
    let cls = match &a[0] {
        Value::Obj(c) => c.clone(),
        _ => return Err(it.type_error("ref.__new__(X): X is not a type object")),
    };
    let target = weak_target(it, b[0].as_ref().unwrap(), "")?;
    let callback = b[1].clone().unwrap_or(Value::None);
    let exact = Rc::ptr_eq(&cls, &it.weak_types()[0]);
    if exact && callback.is_none() {
        for r in weak::live_refs(&target) {
            let reusable = matches!(&r.cls, Some(c) if Rc::ptr_eq(c, &cls))
                && with_opaque::<WeakRefData, bool>(&Value::Obj(r.clone()), |d| {
                    d.callback.is_none()
                })
                .unwrap_or(false);
            if reusable {
                return Ok(Value::Obj(r));
            }
        }
    }
    let data = WeakRefData {
        target: Rc::downgrade(&target),
        callback,
        hash: None,
    };
    let v = new_opaque(&cls, data);
    if let Value::Obj(o) = &v {
        weak::register(&target, o);
    }
    Ok(v)
}

fn ref_init(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::None)
}

fn referent(v: &Value) -> Option<Option<Obj>> {
    with_opaque::<WeakRefData, _>(v, |d| d.target.upgrade())
}

fn ref_call(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("weakref", kw)?;
    it.check_args("weakref", &a[1.min(a.len())..], 0, 0)?;
    match referent(&a[0]) {
        Some(Some(o)) => Ok(Value::Obj(o)),
        Some(None) => Ok(Value::None),
        None => Err(it.self_state_err("weakref")),
    }
}

fn ref_hash(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    if let Some(Some(h)) = with_opaque::<WeakRefData, _>(&a[0], |d| d.hash) {
        return Ok(Value::Int(h));
    }
    match referent(&a[0]) {
        Some(Some(o)) => {
            let h = it.hash_value(&Value::Obj(o))?;
            with_opaque::<WeakRefData, _>(&a[0], |d| d.hash = Some(h));
            Ok(Value::Int(h))
        }
        _ => Err(it.type_error("weak object has gone away")),
    }
}

fn ref_eq_impl(it: &mut Interp, a: &[Value], ne: bool) -> R<Value> {
    let other_is_ref = with_opaque::<WeakRefData, _>(&a[1], |_| ()).is_some();
    if !other_is_ref {
        return Ok(Value::NotImplemented);
    }
    let (x, y) = (referent(&a[0]).flatten(), referent(&a[1]).flatten());
    let eq = match (x, y) {
        (Some(x), Some(y)) => {
            let r = it.compare_op(CmpOp::Eq, &Value::Obj(x), &Value::Obj(y))?;
            it.truthy(&r)?
        }
        _ => a[0].is(&a[1]),
    };
    Ok(Value::Bool(eq != ne))
}

fn ref_eq(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__eq__", a, 2, 2)?;
    ref_eq_impl(it, a, false)
}

fn ref_ne(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__ne__", a, 2, 2)?;
    ref_eq_impl(it, a, true)
}

fn ref_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let id = it.id_of(&a[0]);
    Ok(Value::string(match referent(&a[0]) {
        Some(Some(o)) => {
            let t = it.type_name_of(&Value::Obj(o.clone()));
            format!(
                "<weakref at {:#x}; to '{}' at {:#x}>",
                id,
                t,
                it.id_of(&Value::Obj(o))
            )
        }
        _ => format!("<weakref at {:#x}; dead>", id),
    }))
}

fn ref_callback(_it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(with_opaque::<WeakRefData, _>(&a[0], |d| d.callback.clone()).unwrap_or(Value::None))
}

fn class_getitem(_it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(a.first().cloned().unwrap_or(Value::None))
}

fn proxy_target(it: &mut Interp, v: &Value) -> R<Value> {
    match with_opaque::<ProxyData, _>(v, |d| d.target.upgrade()) {
        Some(Some(o)) => Ok(Value::Obj(o)),
        Some(None) => Err(it.new_exc_str(
            "ReferenceError",
            "weakly-referenced object no longer exists",
        )),
        None => Err(it.self_state_err("weakproxy")),
    }
}

fn proxy_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let _ = kw;
    it.check_args("proxy", a, 1, 2)?;
    let target = weak_target(it, &a[0], "")?;
    let callback = a.get(1).cloned().unwrap_or(Value::None);
    let callable = it.is_callable(&a[0]);
    let cls = if callable {
        it.weak_types()[2].clone()
    } else {
        it.weak_types()[1].clone()
    };
    let v = new_opaque(
        &cls,
        ProxyData {
            target: Rc::downgrade(&target),
            callback,
        },
    );
    if let Value::Obj(o) = &v {
        weak::register(&target, o);
    }
    Ok(v)
}

fn proxy_getattribute(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__getattribute__", a, 2, 2)?;
    let t = proxy_target(it, &a[0])?;
    match &a[1] {
        Value::Obj(n) if matches!(n.kind, Kind::Str(_)) => it.get_attr(&t, n),
        _ => Err(it.type_error("attribute name must be string")),
    }
}

fn proxy_setattr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__setattr__", a, 3, 3)?;
    let t = proxy_target(it, &a[0])?;
    match &a[1] {
        Value::Obj(n) if matches!(n.kind, Kind::Str(_)) => it.set_attr(&t, n, a[2].clone())?,
        _ => return Err(it.type_error("attribute name must be string")),
    }
    Ok(Value::None)
}

fn proxy_delattr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__delattr__", a, 2, 2)?;
    let t = proxy_target(it, &a[0])?;
    match &a[1] {
        Value::Obj(n) if matches!(n.kind, Kind::Str(_)) => it.del_attr(&t, n)?,
        _ => return Err(it.type_error("attribute name must be string")),
    }
    Ok(Value::None)
}

fn proxy_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let id = it.id_of(&a[0]);
    Ok(Value::string(match proxy_target(it, &a[0]) {
        Ok(t) => format!(
            "<weakproxy at {:#x}; to '{}' at {:#x}>",
            id,
            it.type_name_of(&t),
            it.id_of(&t)
        ),
        Err(_) => format!("<weakproxy at {:#x}; dead>", id),
    }))
}

fn proxy_str(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let t = proxy_target(it, &a[0])?;
    it.str_value(&t)
}

fn proxy_bool(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let t = proxy_target(it, &a[0])?;
    Ok(Value::Bool(it.truthy(&t)?))
}

fn proxy_len(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let t = proxy_target(it, &a[0])?;
    Ok(Value::Int(it.len_of(&t)? as i64))
}

fn proxy_iter(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let t = proxy_target(it, &a[0])?;
    it.get_iter(&t)
}

fn proxy_next(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let t = proxy_target(it, &a[0])?;
    match it.iter_next(&t)? {
        Some(v) => Ok(v),
        None => Err(it.new_exc_str("StopIteration", "")),
    }
}

fn proxy_getitem(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__getitem__", a, 2, 2)?;
    let t = proxy_target(it, &a[0])?;
    it.getitem(&t, &a[1])
}

fn proxy_setitem(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__setitem__", a, 3, 3)?;
    let t = proxy_target(it, &a[0])?;
    it.setitem(&t, a[1].clone(), a[2].clone())?;
    Ok(Value::None)
}

fn proxy_delitem(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__delitem__", a, 2, 2)?;
    let t = proxy_target(it, &a[0])?;
    it.delitem(&t, &a[1])?;
    Ok(Value::None)
}

fn proxy_contains(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__contains__", a, 2, 2)?;
    let t = proxy_target(it, &a[0])?;
    Ok(Value::Bool(it.contains(&t, &a[1])?))
}

fn proxy_call(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let t = proxy_target(it, &a[0])?;
    it.call(&t, a[1..].to_vec(), kw.to_vec())
}

fn unproxy(it: &mut Interp, v: &Value) -> R<Value> {
    if with_opaque::<ProxyData, _>(v, |_| ()).is_some() {
        proxy_target(it, v)
    } else {
        Ok(v.clone())
    }
}

fn proxy_hash(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Err(it.type_error("unhashable type: 'weakref.ProxyType'"))
}

macro_rules! proxy_cmp {
    ($name:ident, $op:expr) => {
        fn $name(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
            it.check_args("comparison", a, 2, 2)?;
            let (x, y) = (unproxy(it, &a[0])?, unproxy(it, &a[1])?);
            it.compare_op($op, &x, &y)
        }
    };
}
proxy_cmp!(proxy_eq, CmpOp::Eq);
proxy_cmp!(proxy_ne, CmpOp::NotEq);
proxy_cmp!(proxy_lt, CmpOp::Lt);
proxy_cmp!(proxy_le, CmpOp::LtE);
proxy_cmp!(proxy_gt, CmpOp::Gt);
proxy_cmp!(proxy_ge, CmpOp::GtE);

macro_rules! proxy_bin {
    ($name:ident, $rname:ident, $op:expr) => {
        fn $name(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
            it.check_args("binary op", a, 2, 2)?;
            let (x, y) = (unproxy(it, &a[0])?, unproxy(it, &a[1])?);
            it.binary_op($op, &x, &y)
        }
        fn $rname(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
            it.check_args("binary op", a, 2, 2)?;
            let (x, y) = (unproxy(it, &a[0])?, unproxy(it, &a[1])?);
            it.binary_op($op, &y, &x)
        }
    };
}
proxy_bin!(proxy_add, proxy_radd, BinOp::Add);
proxy_bin!(proxy_sub, proxy_rsub, BinOp::Sub);
proxy_bin!(proxy_mul, proxy_rmul, BinOp::Mult);
proxy_bin!(proxy_truediv, proxy_rtruediv, BinOp::Div);
proxy_bin!(proxy_floordiv, proxy_rfloordiv, BinOp::FloorDiv);
proxy_bin!(proxy_mod, proxy_rmod, BinOp::Mod);

fn proxy_index(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let t = proxy_target(it, &a[0])?;
    Ok(Value::Int(it.index_of(&t)?))
}

fn getweakrefcount(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("getweakrefcount", a, 1, 1)?;
    Ok(Value::Int(match &a[0] {
        Value::Obj(o) => weak::live_refs(o).len() as i64,
        _ => 0,
    }))
}

fn getweakrefs(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("getweakrefs", a, 1, 1)?;
    Ok(Value::list(match &a[0] {
        Value::Obj(o) => weak::live_refs(o).into_iter().map(Value::Obj).collect(),
        _ => Vec::new(),
    }))
}

fn remove_dead_weakref(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("_remove_dead_weakref", a, 2, 2)?;
    let Value::Obj(d) = &a[0] else {
        return Err(it.type_error("_remove_dead_weakref() argument 1 must be dict"));
    };
    if !matches!(d.kind, Kind::Dict(_)) {
        return Err(it.type_error("_remove_dead_weakref() argument 1 must be dict"));
    }
    if let Some(Value::Obj(r)) = it.dict_get(d, &a[1])? {
        if matches!(referent(&Value::Obj(r)), Some(None)) {
            it.dict_remove(d, &a[1])?;
        }
    }
    Ok(Value::None)
}

impl Interp {
    /// `[ReferenceType, ProxyType, CallableProxyType]` of the loaded `_weakref` module.
    pub fn weak_types(&mut self) -> Vec<Obj> {
        let m = match dict_get_str(&self.modules, "_weakref") {
            Some(Value::Obj(m)) => m,
            _ => match crate::builtins::modules::builtin_module(self, "_weakref") {
                Some(m) => {
                    self.register_module("_weakref", &m);
                    m
                }
                None => unreachable!(),
            },
        };
        let d = self.module_dict(&m);
        ["ReferenceType", "ProxyType", "CallableProxyType"]
            .iter()
            .filter_map(|n| dict_get_str(&d, n))
            .filter_map(|v| v.as_obj().cloned())
            .collect()
    }

    /// Runs the callbacks of weak references whose referent has died since the last check.
    pub fn run_weak_callbacks(&mut self) {
        while weak::has_pending() {
            for r in weak::take_pending() {
                if let Some(cb) = weak::take_callback(&r) {
                    if let Err(e) = self.call(&cb, vec![Value::Obj(r.clone())], Vec::new()) {
                        let repr = self.repr_of(&cb).unwrap_or_default();
                        self.write_stderr(&format!("Exception ignored in: {}\n", repr));
                        let text = self.format_exception(&e);
                        self.write_stderr(&text);
                    }
                }
            }
        }
    }
}

pub fn make(it: &mut Interp) -> Obj {
    let m = it.new_module("_weakref");
    let d = it.module_dict(&m);
    let refty = new_type(it, "weakref", "ReferenceType", None, Layout::Other);
    it.reg_new(&refty, ref_new);
    it.reg(&refty, "__init__", ref_init);
    it.reg(&refty, "__call__", ref_call);
    it.reg(&refty, "__hash__", ref_hash);
    it.reg(&refty, "__eq__", ref_eq);
    it.reg(&refty, "__ne__", ref_ne);
    it.reg(&refty, "__repr__", ref_repr);
    it.reg_prop(&refty, "__callback__", ref_callback);
    it.reg_class(&refty, "__class_getitem__", class_getitem);
    set_type(&d, "ReferenceType", &refty);
    set_type(&d, "ref", &refty);

    let protos: Vec<(&'static str, NativeFn)> = vec![
        ("__getattribute__", proxy_getattribute),
        ("__setattr__", proxy_setattr),
        ("__delattr__", proxy_delattr),
        ("__repr__", proxy_repr),
        ("__str__", proxy_str),
        ("__bool__", proxy_bool),
        ("__len__", proxy_len),
        ("__iter__", proxy_iter),
        ("__next__", proxy_next),
        ("__getitem__", proxy_getitem),
        ("__setitem__", proxy_setitem),
        ("__delitem__", proxy_delitem),
        ("__contains__", proxy_contains),
        ("__hash__", proxy_hash),
        ("__eq__", proxy_eq),
        ("__ne__", proxy_ne),
        ("__lt__", proxy_lt),
        ("__le__", proxy_le),
        ("__gt__", proxy_gt),
        ("__ge__", proxy_ge),
        ("__add__", proxy_add),
        ("__radd__", proxy_radd),
        ("__sub__", proxy_sub),
        ("__rsub__", proxy_rsub),
        ("__mul__", proxy_mul),
        ("__rmul__", proxy_rmul),
        ("__truediv__", proxy_truediv),
        ("__rtruediv__", proxy_rtruediv),
        ("__floordiv__", proxy_floordiv),
        ("__rfloordiv__", proxy_rfloordiv),
        ("__mod__", proxy_mod),
        ("__rmod__", proxy_rmod),
        ("__index__", proxy_index),
    ];
    let proxy_ty = new_type(it, "weakref", "ProxyType", None, Layout::Other);
    let callable_proxy_ty = new_type(it, "weakref", "CallableProxyType", None, Layout::Other);
    for ty in [&proxy_ty, &callable_proxy_ty] {
        for (n, f) in &protos {
            it.reg(ty, n, *f);
        }
    }
    it.reg(&callable_proxy_ty, "__call__", proxy_call);
    set_type(&d, "ProxyType", &proxy_ty);
    set_type(&d, "CallableProxyType", &callable_proxy_ty);

    set_fn(it, &d, "proxy", proxy_new);
    set_fn(it, &d, "getweakrefcount", getweakrefcount);
    set_fn(it, &d, "getweakrefs", getweakrefs);
    set_fn(it, &d, "_remove_dead_weakref", remove_dead_weakref);
    m
}
