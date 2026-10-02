//! `_weakref`: `ref`, `proxy` and friends, backed by `Rc::downgrade`.

use super::native::with_opaque;
use crate::ast::{BinOp, CmpOp};
use crate::bind::{opaque_instance, type_object, KwArgs, Py, This};
use crate::bytecode::UnOp;
use crate::object::*;
use crate::vm::*;
use crate::weak::{self, ProxyData, WeakRefData};
use std::rc::Rc;

fn weak_target(it: &mut Interp, v: &Value) -> R<Obj> {
    let ok = match v {
        Value::Obj(o) => match &o.kind {
            Kind::Str(_) | Kind::Int(_) | Kind::Float(_) | Kind::Complex(..) | Kind::Tuple(_) | Kind::List(_) | Kind::Dict(_) | Kind::Bytes(_) | Kind::ByteArray(_) => o.cls.is_some(),
            Kind::Slice(..) | Kind::Range(_) | Kind::BigRange(_) | Kind::Iter(_) | Kind::Cell(_) | Kind::Code(_) => o.cls.is_some(),
            _ => true,
        },
        _ => false,
    };
    match v {
        Value::Obj(o) if ok => Ok(o.clone()),
        _ => {
            let t = it.type_name_of(v);
            Err(it.type_error(&format!("cannot create weak reference to '{}' object", t)))
        }
    }
}

fn referent(v: &Value) -> Option<Option<Obj>> {
    with_opaque::<WeakRefData, _>(v, |d| d.target.upgrade())
}

fn arity(it: &mut Interp, name: &str, given: usize, min: usize, max: usize) -> R<()> {
    if given < min {
        let s = if min == 1 { "" } else { "s" };
        return Err(it.type_error(&format!("{} expected at least {} argument{}, got {}", name, min, s, given)));
    }
    if given > max {
        let s = if max == 1 { "" } else { "s" };
        return Err(it.type_error(&format!("{} expected at most {} argument{}, got {}", name, max, s, given)));
    }
    Ok(())
}

#[lumen_bind::methods]
impl WeakRefData {
    #[constructor(hint(py(text_signature = "")))]
    fn new(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
        let _ = kwargs;
        arity(it, "__new__", args.len(), 1, 2)?;
        let Value::Obj(cls) = &*cls else { return Err(it.type_error("ref.__new__(X): X is not a type object")) };
        let target = weak_target(it, &args[0])?;
        let callback = args.get(1).cloned().unwrap_or(Value::None);
        if callback.is_none() && Rc::ptr_eq(cls, &type_object::<WeakRefData>(it)) {
            for r in weak::live_refs(&target) {
                let reusable = matches!(&r.cls, Some(c) if Rc::ptr_eq(c, cls)) && with_opaque::<WeakRefData, bool>(&Value::Obj(r.clone()), |d| d.callback.is_none()).unwrap_or(false);
                if reusable {
                    return Ok(Value::Obj(r));
                }
            }
        }
        let v = opaque_instance(cls, WeakRefData { target: Rc::downgrade(&target), callback, hash: None });
        if let Value::Obj(o) = &v {
            weak::register(&target, o);
        }
        Ok(v)
    }

    #[proto(init)]
    fn init(slf: This<&Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<()> {
        let _ = (slf, kwargs);
        arity(it, "__init__", args.len(), 1, 2)
    }

    #[proto(call)]
    fn call(&self, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
        if !kwargs.is_empty() {
            return Err(it.type_error("weakref() takes no keyword arguments"));
        }
        if !args.is_empty() {
            return Err(it.type_error(&format!("weakref expected 0 arguments, got {}", args.len())));
        }
        Ok(self.target.upgrade().map_or(Value::None, Value::Obj))
    }

    #[proto(hash)]
    fn hash(slf: This<Py<Self>>, it: &mut Interp) -> R<i64> {
        let (cached, target) = {
            let d = slf.0.borrow(it)?;
            (d.hash, d.target.upgrade())
        };
        if let Some(h) = cached {
            return Ok(h);
        }
        let Some(o) = target else { return Err(it.type_error("weak object has gone away")) };
        let h = it.hash_value(&Value::Obj(o))?;
        slf.0.borrow_mut(it)?.hash = Some(h);
        Ok(h)
    }

    #[proto(eq)]
    fn eq(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        ref_eq(it, &slf, value, false)
    }

    #[proto(ne)]
    fn ne(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        ref_eq(it, &slf, value, true)
    }

    #[proto(lt)]
    fn lt(&self, value: &Value) -> Value {
        let _ = value;
        Value::NotImplemented
    }

    #[proto(le)]
    fn le(&self, value: &Value) -> Value {
        let _ = value;
        Value::NotImplemented
    }

    #[proto(gt)]
    fn gt(&self, value: &Value) -> Value {
        let _ = value;
        Value::NotImplemented
    }

    #[proto(ge)]
    fn ge(&self, value: &Value) -> Value {
        let _ = value;
        Value::NotImplemented
    }

    #[proto(repr)]
    fn repr(slf: This<Py<Self>>, it: &mut Interp) -> String {
        let id = it.id_of(slf.0.value());
        match referent(slf.0.value()).flatten() {
            Some(o) => {
                let t = it.type_name_of(&Value::Obj(o.clone()));
                format!("<weakref at {:#x}; to '{}' at {:#x}>", id, t, it.id_of(&Value::Obj(o)))
            }
            None => format!("<weakref at {:#x}; dead>", id),
        }
    }

    #[getter(name = "__callback__")]
    fn callback(&self) -> Value {
        self.callback.clone()
    }

    /// See PEP 585
    #[classmethod(name = "__class_getitem__", hint(py(text_signature = "")))]
    fn class_getitem(cls: This<Value>, it: &mut Interp, item: &Value) -> Value {
        it.make_alias(cls.0, item)
    }
}

fn ref_eq(it: &mut Interp, a: &Value, b: &Value, ne: bool) -> R<Value> {
    if with_opaque::<WeakRefData, _>(b, |_| ()).is_none() {
        return Ok(Value::NotImplemented);
    }
    let eq = match (referent(a).flatten(), referent(b).flatten()) {
        (Some(x), Some(y)) => {
            let r = it.compare_op(CmpOp::Eq, &Value::Obj(x), &Value::Obj(y))?;
            it.truthy(&r)?
        }
        _ => a.is(b),
    };
    Ok(Value::Bool(eq != ne))
}

fn proxy_target(it: &mut Interp, p: &Py<ProxyData>) -> R<Value> {
    match p.borrow(it)?.target.upgrade() {
        Some(o) => Ok(Value::Obj(o)),
        None => Err(it.new_exc_str("ReferenceError", "weakly-referenced object no longer exists")),
    }
}

fn unproxy(it: &mut Interp, v: &Value) -> R<Value> {
    match with_opaque::<ProxyData, _>(v, |d| d.target.upgrade()) {
        Some(Some(o)) => Ok(Value::Obj(o)),
        Some(None) => Err(it.new_exc_str("ReferenceError", "weakly-referenced object no longer exists")),
        None => Ok(v.clone()),
    }
}

fn attr_name<'a>(it: &mut Interp, name: &'a Value) -> R<&'a Obj> {
    match name {
        Value::Obj(n) if matches!(n.kind, Kind::Str(_)) => Ok(n),
        _ => Err(it.type_error("attribute name must be string")),
    }
}

fn binop(it: &mut Interp, op: BinOp, a: &Value, b: &Value) -> R<Value> {
    let (x, y) = (unproxy(it, a)?, unproxy(it, b)?);
    it.binary_op(op, &x, &y)
}

fn binop_cmp(it: &mut Interp, op: CmpOp, a: &Value, b: &Value) -> R<Value> {
    let (x, y) = (unproxy(it, a)?, unproxy(it, b)?);
    it.compare_op(op, &x, &y)
}

fn inplace(it: &mut Interp, op: BinOp, slf: &Py<ProxyData>, value: &Value) -> R<Value> {
    let (x, y) = (proxy_target(it, slf)?, unproxy(it, value)?);
    it.inplace_op(op, x, &y)
}

fn call_builtin(it: &mut Interp, name: &str, arg: Value) -> R<Value> {
    let f = dict_get_str(&it.builtins, name).unwrap_or(Value::None);
    it.call(&f, vec![arg], Vec::new())
}

#[lumen_bind::methods]
impl ProxyData {
    #[method(name = "__getattribute__", hint(py(text_signature = "")))]
    fn getattribute(slf: This<Py<Self>>, it: &mut Interp, name: &Value) -> R<Value> {
        let t = proxy_target(it, &slf.0)?;
        let n = attr_name(it, name)?;
        it.get_attr(&t, n)
    }

    #[method(name = "__setattr__", hint(py(text_signature = "")))]
    fn setattr(slf: This<Py<Self>>, it: &mut Interp, name: &Value, value: &Value) -> R<()> {
        let t = proxy_target(it, &slf.0)?;
        let n = attr_name(it, name)?;
        it.set_attr(&t, n, value.clone())
    }

    #[method(name = "__delattr__", hint(py(text_signature = "")))]
    fn delattr(slf: This<Py<Self>>, it: &mut Interp, name: &Value) -> R<()> {
        let t = proxy_target(it, &slf.0)?;
        let n = attr_name(it, name)?;
        it.del_attr(&t, n)
    }

    #[proto(repr)]
    fn repr(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
        let id = it.id_of(slf.0.value());
        let t = slf.0.borrow(it)?.target.upgrade().map_or(Value::None, Value::Obj);
        Ok(format!("<weakproxy at {:#x} to {} at {:#x}>", id, it.type_name_of(&t), it.id_of(&t)))
    }

    #[proto(str)]
    fn str(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let t = proxy_target(it, &slf.0)?;
        it.str_value(&t)
    }

    #[proto(bool)]
    fn bool(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
        let t = proxy_target(it, &slf.0)?;
        it.truthy(&t)
    }

    #[proto(len)]
    fn len(slf: This<Py<Self>>, it: &mut Interp) -> R<usize> {
        let t = proxy_target(it, &slf.0)?;
        it.len_of(&t)
    }

    #[proto(iter)]
    fn iter(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let t = proxy_target(it, &slf.0)?;
        it.get_iter(&t)
    }

    #[proto(next)]
    fn next(slf: This<Py<Self>>, it: &mut Interp) -> R<Option<Value>> {
        let t = proxy_target(it, &slf.0)?;
        it.iter_next(&t)
    }

    #[proto(getitem)]
    fn getitem(slf: This<Py<Self>>, it: &mut Interp, key: &Value) -> R<Value> {
        let t = proxy_target(it, &slf.0)?;
        it.getitem(&t, key)
    }

    #[proto(setitem)]
    fn setitem(slf: This<Py<Self>>, it: &mut Interp, key: &Value, value: &Value) -> R<()> {
        let t = proxy_target(it, &slf.0)?;
        it.setitem(&t, key.clone(), value.clone())
    }

    #[proto(delitem)]
    fn delitem(slf: This<Py<Self>>, it: &mut Interp, key: &Value) -> R<()> {
        let t = proxy_target(it, &slf.0)?;
        it.delitem(&t, key)
    }

    #[proto(contains)]
    fn contains(slf: This<Py<Self>>, it: &mut Interp, key: &Value) -> R<bool> {
        let t = proxy_target(it, &slf.0)?;
        it.contains(&t, key)
    }

    #[proto(index)]
    fn index(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let t = proxy_target(it, &slf.0)?;
        crate::bind::index(it, &t)
    }

    #[proto(int)]
    fn int(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let t = proxy_target(it, &slf.0)?;
        let f = Value::Obj(it.types.int.clone());
        it.call(&f, vec![t], Vec::new())
    }

    #[proto(float)]
    fn float(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let t = proxy_target(it, &slf.0)?;
        let f = Value::Obj(it.types.float.clone());
        it.call(&f, vec![t], Vec::new())
    }

    #[proto(neg)]
    fn neg(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let t = proxy_target(it, &slf.0)?;
        it.unary_op(UnOp::Neg, &t)
    }

    #[proto(pos)]
    fn pos(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let t = proxy_target(it, &slf.0)?;
        it.unary_op(UnOp::Pos, &t)
    }

    #[proto(invert)]
    fn invert(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let t = proxy_target(it, &slf.0)?;
        it.unary_op(UnOp::Invert, &t)
    }

    #[proto(abs)]
    fn abs(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let t = proxy_target(it, &slf.0)?;
        call_builtin(it, "abs", t)
    }

    #[proto(add)]
    fn add(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::Add, &slf, value)
    }

    #[proto(radd)]
    fn radd(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::Add, value, &slf)
    }

    #[proto(sub)]
    fn sub(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::Sub, &slf, value)
    }

    #[proto(rsub)]
    fn rsub(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::Sub, value, &slf)
    }

    #[proto(mul)]
    fn mul(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::Mult, &slf, value)
    }

    #[proto(rmul)]
    fn rmul(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::Mult, value, &slf)
    }

    #[proto(and)]
    fn and(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::BitAnd, &slf, value)
    }

    #[proto(rand)]
    fn rand(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::BitAnd, value, &slf)
    }

    #[proto(or)]
    fn or(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::BitOr, &slf, value)
    }

    #[proto(ror)]
    fn ror(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::BitOr, value, &slf)
    }

    #[proto(xor)]
    fn xor(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::BitXor, &slf, value)
    }

    #[proto(rxor)]
    fn rxor(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::BitXor, value, &slf)
    }

    #[method(name = "__truediv__", hint(py(text_signature = "")))]
    fn truediv(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::Div, &slf, value)
    }

    #[method(name = "__rtruediv__", hint(py(text_signature = "")))]
    fn rtruediv(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::Div, value, &slf)
    }

    #[method(name = "__floordiv__", hint(py(text_signature = "")))]
    fn floordiv(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::FloorDiv, &slf, value)
    }

    #[method(name = "__rfloordiv__", hint(py(text_signature = "")))]
    fn rfloordiv(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::FloorDiv, value, &slf)
    }

    #[method(name = "__mod__", hint(py(text_signature = "")))]
    fn r#mod(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::Mod, &slf, value)
    }

    #[method(name = "__rmod__", hint(py(text_signature = "")))]
    fn rmod(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::Mod, value, &slf)
    }

    #[method(name = "__matmul__", hint(py(text_signature = "")))]
    fn matmul(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::MatMult, &slf, value)
    }

    #[method(name = "__rmatmul__", hint(py(text_signature = "")))]
    fn rmatmul(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::MatMult, value, &slf)
    }

    #[method(name = "__lshift__", hint(py(text_signature = "")))]
    fn lshift(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::LShift, &slf, value)
    }

    #[method(name = "__rlshift__", hint(py(text_signature = "")))]
    fn rlshift(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::LShift, value, &slf)
    }

    #[method(name = "__rshift__", hint(py(text_signature = "")))]
    fn rshift(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::RShift, &slf, value)
    }

    #[method(name = "__rrshift__", hint(py(text_signature = "")))]
    fn rrshift(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::RShift, value, &slf)
    }

    #[method(name = "__pow__", hint(py(text_signature = "")))]
    fn pow(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::Pow, &slf, value)
    }

    #[method(name = "__rpow__", hint(py(text_signature = "")))]
    fn rpow(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop(it, BinOp::Pow, value, &slf)
    }

    #[proto(iadd)]
    fn iadd(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<Value> {
        inplace(it, BinOp::Add, &slf.0, value)
    }

    #[proto(isub)]
    fn isub(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<Value> {
        inplace(it, BinOp::Sub, &slf.0, value)
    }

    #[proto(imul)]
    fn imul(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<Value> {
        inplace(it, BinOp::Mult, &slf.0, value)
    }

    #[proto(iand)]
    fn iand(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<Value> {
        inplace(it, BinOp::BitAnd, &slf.0, value)
    }

    #[proto(ior)]
    fn ior(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<Value> {
        inplace(it, BinOp::BitOr, &slf.0, value)
    }

    #[proto(ixor)]
    fn ixor(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<Value> {
        inplace(it, BinOp::BitXor, &slf.0, value)
    }

    #[method(name = "__itruediv__", hint(py(text_signature = "")))]
    fn itruediv(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<Value> {
        inplace(it, BinOp::Div, &slf.0, value)
    }

    #[method(name = "__ifloordiv__", hint(py(text_signature = "")))]
    fn ifloordiv(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<Value> {
        inplace(it, BinOp::FloorDiv, &slf.0, value)
    }

    #[method(name = "__imod__", hint(py(text_signature = "")))]
    fn imod(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<Value> {
        inplace(it, BinOp::Mod, &slf.0, value)
    }

    #[method(name = "__imatmul__", hint(py(text_signature = "")))]
    fn imatmul(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<Value> {
        inplace(it, BinOp::MatMult, &slf.0, value)
    }

    #[method(name = "__ilshift__", hint(py(text_signature = "")))]
    fn ilshift(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<Value> {
        inplace(it, BinOp::LShift, &slf.0, value)
    }

    #[method(name = "__irshift__", hint(py(text_signature = "")))]
    fn irshift(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<Value> {
        inplace(it, BinOp::RShift, &slf.0, value)
    }

    #[method(name = "__ipow__", hint(py(text_signature = "")))]
    fn ipow(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<Value> {
        inplace(it, BinOp::Pow, &slf.0, value)
    }

    #[proto(eq)]
    fn eq(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop_cmp(it, CmpOp::Eq, &slf, value)
    }

    #[proto(ne)]
    fn ne(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop_cmp(it, CmpOp::NotEq, &slf, value)
    }

    #[proto(lt)]
    fn lt(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop_cmp(it, CmpOp::Lt, &slf, value)
    }

    #[proto(le)]
    fn le(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop_cmp(it, CmpOp::LtE, &slf, value)
    }

    #[proto(gt)]
    fn gt(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop_cmp(it, CmpOp::Gt, &slf, value)
    }

    #[proto(ge)]
    fn ge(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        binop_cmp(it, CmpOp::GtE, &slf, value)
    }

    #[method(name = "__bytes__", hint(py(text_signature = "")))]
    fn bytes(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let t = proxy_target(it, &slf.0)?;
        call_builtin(it, "bytes", t)
    }

    #[method(name = "__reversed__", hint(py(text_signature = "")))]
    fn reversed(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let t = proxy_target(it, &slf.0)?;
        call_builtin(it, "reversed", t)
    }
}

/// The type of proxies to callable objects: the proxy members plus `__call__`.
#[lumen_bind::class(name = "CallableProxyType", module = "weakref", hint(py(unhashable)))]
pub struct CallableProxy;

#[lumen_bind::methods]
impl CallableProxy {
    #[proto(call)]
    fn call(slf: This<Py<ProxyData>>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
        let t = proxy_target(it, &slf.0)?;
        it.call(&t, args.to_vec(), kwargs.to_vec())
    }
}

/// Weak-reference support module.
#[lumen_bind::module(name = "_weakref")]
pub mod _weakref {
    use super::*;

    /// Create a proxy object that weakly references 'object'.
    ///
    /// 'callback', if given, is called with a reference to the
    /// proxy when 'object' is about to be finalized.
    #[op(hint(py(arg_style = "unpack")))]
    fn proxy(it: &mut Interp, object: &Value, callback: Option<&Value>) -> R<Value> {
        let target = weak_target(it, object)?;
        let callback = callback.cloned().unwrap_or(Value::None);
        let cls = if it.is_callable(object) { type_object::<CallableProxy>(it) } else { type_object::<ProxyData>(it) };
        let v = opaque_instance(&cls, ProxyData { target: Rc::downgrade(&target), callback });
        if let Value::Obj(o) = &v {
            weak::register(&target, o);
        }
        Ok(v)
    }

    /// Return the number of weak references to 'object'.
    #[op]
    fn getweakrefcount(object: &Value) -> usize {
        match object {
            Value::Obj(o) => weak::live_refs(o).len(),
            _ => 0,
        }
    }

    /// Return a list of all weak reference objects pointing to 'object'.
    #[op]
    fn getweakrefs(object: &Value) -> Value {
        Value::list(match object {
            Value::Obj(o) => weak::live_refs(o).into_iter().map(Value::Obj).collect(),
            _ => Vec::new(),
        })
    }

    /// Atomically remove key from dict if it points to a dead weakref.
    #[op(hint(py(arg_style = "unpack")))]
    fn _remove_dead_weakref(it: &mut Interp, dct: &Value, key: &Value) -> R<()> {
        let d = match dct {
            Value::Obj(d) if matches!(d.kind, Kind::Dict(_)) => d,
            _ => {
                let t = it.type_name_of(dct);
                return Err(it.type_error(&format!("_remove_dead_weakref() argument 1 must be dict, not {}", t)));
            }
        };
        if let Some(r) = it.dict_get(d, key)? {
            if matches!(referent(&r), Some(None)) {
                it.dict_remove(d, key)?;
            }
        }
        Ok(())
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let refty = type_object::<WeakRefData>(it);
        super::super::descr::install_getsets::<WeakRefData>(it, &refty, &["__callback__"]);
        let proxy = type_object::<ProxyData>(it);
        let callable = type_object::<CallableProxy>(it);
        crate::bind::install_all::<ProxyData>(&callable);
        let d = it.module_dict(m);
        for (name, ty) in [("ReferenceType", &refty), ("ref", &refty), ("ProxyType", &proxy), ("CallableProxyType", &callable)] {
            dict_set_str(&d, name, Value::Obj(ty.clone()));
        }
    }
}

impl Interp {
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
