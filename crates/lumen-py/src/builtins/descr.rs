//! `mappingproxy` and the descriptor types (`getset_descriptor`, `member_descriptor`) that the
//! standard library probes through `types.py`.

use super::native::*;
use crate::ast::CmpOp;
use crate::object::*;
use crate::vm::*;
use std::rc::Rc;

pub struct ProxyOf {
    mapping: Value,
}

impl Interp {
    pub fn mappingproxy_type(&mut self) -> Obj {
        if let Some(t) = &self.mappingproxy_type {
            return t.clone();
        }
        let ty = new_type(self, "builtins", "mappingproxy", None, Layout::Other);
        self.reg_new(&ty, mp_new);
        self.reg(&ty, "__getitem__", mp_getitem);
        self.reg(&ty, "__iter__", mp_iter);
        self.reg(&ty, "__len__", mp_len);
        self.reg(&ty, "__contains__", mp_contains);
        self.reg(&ty, "__reversed__", mp_reversed);
        self.reg(&ty, "__repr__", mp_repr);
        self.reg(&ty, "__str__", mp_str);
        self.reg(&ty, "__eq__", mp_eq);
        self.reg(&ty, "__ne__", mp_ne);
        self.reg(&ty, "__or__", mp_or);
        self.reg(&ty, "__ror__", mp_ror);
        self.reg(&ty, "get", mp_get);
        self.reg(&ty, "keys", mp_keys);
        self.reg(&ty, "values", mp_values);
        self.reg(&ty, "items", mp_items);
        self.reg(&ty, "copy", mp_copy);
        self.reg_class(&ty, "__class_getitem__", mp_class_getitem);
        if let Some(d) = ty.dict.borrow().as_ref() {
            dict_set_str(d, "__hash__", Value::None);
        }
        self.mappingproxy_type = Some(ty.clone());
        ty
    }

    pub fn new_mappingproxy(&mut self, mapping: Value) -> Value {
        let ty = self.mappingproxy_type();
        new_opaque(&ty, ProxyOf { mapping })
    }

    fn is_mapping_value(&mut self, v: &Value) -> bool {
        match v {
            Value::Obj(o) => {
                if matches!(o.kind, Kind::List(_) | Kind::Tuple(_) | Kind::Str(_) | Kind::Bytes(_) | Kind::ByteArray(_)) && o.cls.is_none() {
                    return false;
                }
                let cls = self.type_of_obj(o);
                self.lookup_mro(&cls, "__getitem__").is_some() && self.lookup_mro(&cls, "keys").is_some()
            }
            _ => false,
        }
    }
}

fn target(it: &mut Interp, v: &Value) -> R<Value> {
    match with_opaque::<ProxyOf, _>(v, |p| p.mapping.clone()) {
        Some(m) => Ok(m),
        None => Err(it.self_state_err("mappingproxy")),
    }
}

fn mp_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("mappingproxy", &a[1.min(a.len())..], kw, &["mapping"], 1)?;
    let m = b[0].clone().unwrap();
    if !it.is_mapping_value(&m) {
        let t = it.type_name_of(&m);
        return Err(it.type_error(&format!("mappingproxy() argument must be a mapping, not {}", t)));
    }
    Ok(it.new_mappingproxy(m))
}

fn mp_getitem(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__getitem__", a, 2, 2)?;
    let t = target(it, &a[0])?;
    it.getitem(&t, &a[1])
}

fn mp_iter(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let t = target(it, &a[0])?;
    it.get_iter(&t)
}

fn mp_len(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let t = target(it, &a[0])?;
    Ok(Value::Int(it.len_of(&t)? as i64))
}

fn mp_contains(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__contains__", a, 2, 2)?;
    let t = target(it, &a[0])?;
    Ok(Value::Bool(it.contains(&t, &a[1])?))
}

fn mp_reversed(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let t = target(it, &a[0])?;
    let items = it.iterate_to_vec(&t)?;
    let rev: Vec<Value> = items.into_iter().rev().collect();
    it.native_get_iter(&Value::list(rev))
}

fn mp_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let t = target(it, &a[0])?;
    Ok(Value::string(format!("mappingproxy({})", it.repr_of(&t)?)))
}

fn mp_str(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let t = target(it, &a[0])?;
    it.str_value(&t)
}

fn mp_eq(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__eq__", a, 2, 2)?;
    let t = target(it, &a[0])?;
    let other = match with_opaque::<ProxyOf, _>(&a[1], |p| p.mapping.clone()) {
        Some(m) => m,
        None => a[1].clone(),
    };
    it.compare_op(CmpOp::Eq, &t, &other)
}

fn mp_ne(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let r = mp_eq(it, a, kw)?;
    Ok(Value::Bool(!it.truthy(&r)?))
}

fn mp_or(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__or__", a, 2, 2)?;
    let t = target(it, &a[0])?;
    let other = with_opaque::<ProxyOf, _>(&a[1], |p| p.mapping.clone()).unwrap_or_else(|| a[1].clone());
    it.binary_op(crate::ast::BinOp::BitOr, &t, &other)
}

fn mp_ror(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__ror__", a, 2, 2)?;
    let t = target(it, &a[0])?;
    it.binary_op(crate::ast::BinOp::BitOr, &a[1], &t)
}

fn mp_get(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("get", a, 2, 3)?;
    let t = target(it, &a[0])?;
    let m = it.get_attr_str(&t, "get")?;
    it.call(&m, a[1..].to_vec(), Vec::new())
}

fn forward(it: &mut Interp, a: &[Value], name: &str) -> R<Value> {
    let t = target(it, &a[0])?;
    let m = it.get_attr_str(&t, name)?;
    it.call(&m, Vec::new(), Vec::new())
}

fn mp_keys(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    forward(it, a, "keys")
}

fn mp_values(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    forward(it, a, "values")
}

fn mp_items(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    forward(it, a, "items")
}

fn mp_copy(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    forward(it, a, "copy")
}

fn mp_class_getitem(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__class_getitem__", a, 2, 2)?;
    Ok(it.make_alias(a[0].clone(), &a[1]))
}

// ---- getset / member descriptors ----------------------------------------------------------------

fn function_code(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(it.special_attr(&a[0], "__code__")?.unwrap_or(Value::None))
}

fn function_globals(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(it.special_attr(&a[0], "__globals__")?.unwrap_or(Value::None))
}

fn function_closure(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(it.special_attr(&a[0], "__closure__")?.unwrap_or(Value::None))
}

fn memoryview_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    super::bytesm::memoryview_fn()(it, &a[1.min(a.len())..], kw)
}

fn call_forward(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    if a.is_empty() {
        return Err(it.type_error("descriptor '__call__' needs an argument"));
    }
    it.call(&a[0], a[1..].to_vec(), kw.to_vec())
}

fn module_new(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let Some(Value::Obj(cls)) = a.first() else { return Err(it.type_error("module.__new__(X): X is not a type object")) };
    let m = it.new_module("");
    if Rc::ptr_eq(cls, &it.types.module) {
        return Ok(Value::Obj(m));
    }
    let d = it.module_dict(&m);
    let o = Object::with_cls(cls.clone(), Kind::Module);
    *o.dict.borrow_mut() = Some(d);
    Ok(Value::Obj(o))
}

fn module_init(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("module", &a[1.min(a.len())..], kw, &["name", "doc"], 1)?;
    if let Value::Obj(m) = &a[0] {
        let d = it.module_dict(m);
        dict_set_str(&d, "__name__", b[0].clone().unwrap_or(Value::None));
        dict_set_str(&d, "__doc__", b[1].clone().unwrap_or(Value::None));
    }
    Ok(Value::None)
}

fn method_new(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("method", &a[1.min(a.len())..], 2, 2)?;
    if !it.is_callable(&a[1]) {
        return Err(it.type_error("first argument must be callable"));
    }
    if a[2].is_none() {
        return Err(it.type_error("instance must not be None"));
    }
    Ok(Value::Obj(Object::new(Kind::Method(a[1].clone(), a[2].clone()))))
}

pub fn init(it: &mut Interp) {
    let module_ty = it.types.module.clone();
    it.reg_new(&module_ty, module_new);
    it.reg(&module_ty, "__init__", module_init);
    let method_ty = it.types.method.clone();
    it.reg_new(&method_ty, method_new);
    for ty in [it.types.function.clone(), it.types.method.clone(), it.types.builtin_function.clone()] {
        it.reg(&ty, "__call__", call_forward);
    }
    let mv = new_type(it, "builtins", "memoryview", None, Layout::Other);
    it.reg_new(&mv, memoryview_new);
    let b = it.builtins.clone();
    set_type(&b, "memoryview", &mv);
    let getset = new_type(it, "builtins", "getset_descriptor", None, Layout::Other);
    let member = new_type(it, "builtins", "member_descriptor", None, Layout::Other);
    let func = it.types.function.clone();
    let entries: [(&'static str, NativeFn, &Obj); 3] = [("__code__", function_code, &getset), ("__globals__", function_globals, &member), ("__closure__", function_closure, &member)];
    for (name, f, ty) in entries {
        let g = it.new_native(name, f, false);
        let p = Object::with_cls(ty.clone(), Kind::Property(PropData { fget: g, fset: Value::None, fdel: Value::None, doc: Value::None }));
        if let Some(d) = func.dict.borrow().as_ref() {
            dict_set_str(d, name, Value::Obj(p));
        }
    }
}
