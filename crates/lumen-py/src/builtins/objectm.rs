//! `object`, `type`, `property`, `staticmethod`, `classmethod`, `super` and friends.

use crate::ast::CmpOp;
use crate::object::*;
use crate::vm::*;
use std::rc::Rc;

fn name_obj(it: &mut Interp, v: &Value, what: &str) -> R<Obj> {
    match v {
        Value::Obj(o) if matches!(o.kind, Kind::Str(_)) => Ok(o.clone()),
        _ => {
            let t = it.type_name_of(v);
            Err(it.type_error(&format!("{}(): attribute name must be string, not '{}'", what, t)))
        }
    }
}

fn obj_new(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    let cls = match a.first() {
        Some(Value::Obj(c)) if matches!(c.kind, Kind::Type(_)) => c.clone(),
        Some(v) => {
            let t = it.type_name_of(v);
            return Err(it.type_error(&format!("object.__new__(X): X is not a type object ({})", t)));
        }
        None => return Err(it.type_error("object.__new__(): not enough arguments")),
    };
    it.alloc_instance(&cls)
}

fn obj_init(_it: &mut Interp, _a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    Ok(Value::None)
}

fn obj_setattr(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    it.check_args("object.__setattr__", a, 3, 3)?;
    let n = name_obj(it, &a[1], "setattr")?;
    let cls = it.type_of(&a[0]);
    it.generic_setattr(&a[0], &cls, &n, a[2].clone())?;
    Ok(Value::None)
}

fn obj_delattr(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    it.check_args("object.__delattr__", a, 2, 2)?;
    let n = name_obj(it, &a[1], "delattr")?;
    let cls = it.type_of(&a[0]);
    it.generic_delattr(&a[0], &cls, &n)?;
    Ok(Value::None)
}

fn obj_getattribute(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    it.check_args("object.__getattribute__", a, 2, 2)?;
    let n = name_obj(it, &a[1], "getattr")?;
    let cls = it.type_of(&a[0]);
    it.generic_getattr(&a[0], &cls, &n)
}

fn obj_eq(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    it.check_args("__eq__", a, 2, 2)?;
    if a[0].is(&a[1]) {
        return Ok(Value::Bool(true));
    }
    match it.native_compare(CmpOp::Eq, &a[0], &a[1])? {
        Some(b) => Ok(Value::Bool(b)),
        None => Ok(Value::NotImplemented),
    }
}

fn obj_ne(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    it.check_args("__ne__", a, 2, 2)?;
    let r = if it.user_special(&a[0], "__eq__").is_some() {
        let m = it.get_attr_str(&a[0], "__eq__")?;
        it.call(&m, vec![a[1].clone()], Vec::new())?
    } else {
        obj_eq(it, a, &[])?
    };
    if matches!(r, Value::NotImplemented) {
        return Ok(r);
    }
    Ok(Value::Bool(!it.truthy(&r)?))
}

macro_rules! cmp_fn {
    ($name:ident, $op:expr) => {
        fn $name(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
            it.check_args("comparison", a, 2, 2)?;
            match it.native_compare($op, &a[0], &a[1])? {
                Some(b) => Ok(Value::Bool(b)),
                None => Ok(Value::NotImplemented),
            }
        }
    };
}
cmp_fn!(obj_lt, CmpOp::Lt);
cmp_fn!(obj_le, CmpOp::LtE);
cmp_fn!(obj_gt, CmpOp::Gt);
cmp_fn!(obj_ge, CmpOp::GtE);

fn obj_hash(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    it.check_args("__hash__", a, 1, 1)?;
    Ok(Value::Int(it.native_hash(&a[0])?))
}

fn obj_repr(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    it.check_args("__repr__", a, 1, 1)?;
    Ok(Value::string(it.native_repr(&a[0])?))
}

fn obj_str(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    it.check_args("__str__", a, 1, 1)?;
    Ok(Value::string(it.native_str(&a[0])?))
}

fn obj_format(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    it.check_args("__format__", a, 2, 2)?;
    let spec = it.str_arg(&a[1], "format_spec")?;
    if !spec.is_empty() {
        let t = it.type_name_of(&a[0]);
        return Err(it.type_error(&format!("unsupported format string passed to {}.__format__", t)));
    }
    Ok(Value::string(it.str_of(&a[0])?))
}

fn obj_init_subclass(it: &mut Interp, a: &[Value], kw: &[(Obj, Value)]) -> R<Value> {
    if !kw.is_empty() {
        let n = match a.first() {
            Some(Value::Obj(c)) => it.type_display(c),
            _ => "type".into(),
        };
        return Err(it.type_error(&format!("{}.__init_subclass__() takes no keyword arguments", n)));
    }
    Ok(Value::None)
}

fn obj_dir(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    it.check_args("__dir__", a, 1, 1)?;
    let names = it.dir_names(&a[0])?;
    Ok(Value::list(names.into_iter().map(Value::string).collect()))
}

fn obj_sizeof(_it: &mut Interp, _a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    Ok(Value::Int(64))
}

fn obj_subclasshook(_it: &mut Interp, _a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    Ok(Value::NotImplemented)
}

fn obj_reduce_ex(it: &mut Interp, _a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    Err(it.type_error("cannot pickle object"))
}

impl Interp {
    pub fn dir_names(&mut self, v: &Value) -> R<Vec<String>> {
        let mut names: Vec<String> = Vec::new();
        let push = |names: &mut Vec<String>, d: &Obj| {
            if let Kind::Dict(p) = &d.kind {
                for k in p.borrow().keys() {
                    if let Some(s) = k.as_str() {
                        names.push(s.to_string());
                    }
                }
            }
        };
        if let Value::Obj(o) = v {
            if let Kind::Type(td) = &o.kind {
                for c in td.mro.borrow().iter() {
                    if let Some(d) = c.dict.borrow().as_ref() {
                        push(&mut names, d);
                    }
                }
            } else {
                if let Some(d) = o.dict.borrow().as_ref() {
                    push(&mut names, d);
                }
                let cls = self.type_of_obj(o);
                if let Kind::Type(td) = &cls.kind {
                    for c in td.mro.borrow().iter() {
                        if let Some(d) = c.dict.borrow().as_ref() {
                            push(&mut names, d);
                        }
                    }
                }
            }
        } else {
            let cls = self.type_of(v);
            if let Kind::Type(td) = &cls.kind {
                for c in td.mro.borrow().iter() {
                    if let Some(d) = c.dict.borrow().as_ref() {
                        push(&mut names, d);
                    }
                }
            }
        }
        names.sort();
        names.dedup();
        Ok(names)
    }
}

// ---- type ------------------------------------------------------------------------------------

fn type_new(it: &mut Interp, a: &[Value], kw: &[(Obj, Value)]) -> R<Value> {
    let meta = match a.first() {
        Some(Value::Obj(m)) if matches!(m.kind, Kind::Type(_)) => m.clone(),
        _ => return Err(it.type_error("type.__new__(X): X is not a type object")),
    };
    if a.len() == 2 && Rc::ptr_eq(&meta, &it.types.type_) {
        return Ok(Value::Obj(it.type_of(&a[1])));
    }
    if a.len() != 4 {
        return Err(it.type_error("type.__new__() takes exactly 3 arguments"));
    }
    it.type_new_from_args(meta, &a[1..], kw.to_vec())
}

fn type_init(_it: &mut Interp, _a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    Ok(Value::None)
}

fn type_call(it: &mut Interp, a: &[Value], kw: &[(Obj, Value)]) -> R<Value> {
    match a.first() {
        Some(Value::Obj(c)) if matches!(c.kind, Kind::Type(_)) => it.type_call_default(c, a[1..].to_vec(), kw.to_vec()),
        _ => Err(it.type_error("descriptor '__call__' requires a 'type' object")),
    }
}

fn type_prepare(it: &mut Interp, _a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    Ok(Value::Obj(it.new_dict()))
}

fn type_instancecheck(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    it.check_args("__instancecheck__", a, 2, 2)?;
    let t = it.type_of(&a[1]);
    match &a[0] {
        Value::Obj(c) => Ok(Value::Bool(it.is_subtype(&t, c))),
        _ => Ok(Value::Bool(false)),
    }
}

fn type_subclasscheck(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    it.check_args("__subclasscheck__", a, 2, 2)?;
    match (&a[0], &a[1]) {
        (Value::Obj(c), Value::Obj(s)) if matches!(s.kind, Kind::Type(_)) => Ok(Value::Bool(it.is_subtype(s, c))),
        _ => Err(it.type_error("issubclass() arg 1 must be a class")),
    }
}

fn type_mro(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    match a.first() {
        Some(Value::Obj(c)) => match &c.kind {
            Kind::Type(td) => Ok(Value::list(td.mro.borrow().iter().map(|m| Value::Obj(m.clone())).collect())),
            _ => Err(it.type_error("descriptor 'mro' requires a 'type' object")),
        },
        _ => Err(it.type_error("descriptor 'mro' requires a 'type' object")),
    }
}

fn type_subclasses(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    let c = match a.first() {
        Some(Value::Obj(c)) => c.clone(),
        _ => return Err(it.type_error("descriptor '__subclasses__' requires a 'type' object")),
    };
    let mut out = Vec::new();
    let reg = it.subclass_registry.clone();
    for w in reg.iter().filter_map(|w| w.upgrade()) {
        if let Kind::Type(td) = &w.kind {
            if td.bases.borrow().iter().any(|b| Rc::ptr_eq(b, &c)) {
                out.push(Value::Obj(w.clone()));
            }
        }
    }
    Ok(Value::list(out))
}

fn type_or(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    it.check_args("__or__", a, 2, 2)?;
    Ok(Value::tuple(vec![a[0].clone(), a[1].clone()]))
}

// ---- property / staticmethod / classmethod ------------------------------------------------------

fn property_new(it: &mut Interp, a: &[Value], kw: &[(Obj, Value)]) -> R<Value> {
    let cls = match a.first() {
        Some(Value::Obj(c)) => c.clone(),
        _ => return Err(it.type_error("property.__new__(X): X is not a type object")),
    };
    let b = it.bind_args("property", &a[1..], kw, &["fget", "fset", "fdel", "doc"], 0)?;
    let get = |i: usize| b[i].clone().unwrap_or(Value::None);
    let mut doc = get(3);
    if doc.is_none() {
        if let Value::Obj(f) = &get(0) {
            if let Kind::Function(func) = &f.kind {
                doc = func.code.doc.clone().unwrap_or(Value::None);
            }
        }
    }
    let kind = Kind::Property(PropData { fget: get(0), fset: get(1), fdel: get(2), doc });
    Ok(Value::Obj(if Rc::ptr_eq(&cls, &it.types.property) { Object::new(kind) } else { Object::with_cls(cls, kind) }))
}

fn property_init(_it: &mut Interp, _a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    Ok(Value::None)
}

fn prop_with(it: &mut Interp, a: &[Value], slot: usize) -> R<Value> {
    it.check_args("property", a, 2, 2)?;
    let o = match &a[0] {
        Value::Obj(o) => o,
        _ => return Err(it.type_error("not a property")),
    };
    let p = match &o.kind {
        Kind::Property(p) => p,
        _ => return Err(it.type_error("not a property")),
    };
    let mut np = PropData { fget: p.fget.clone(), fset: p.fset.clone(), fdel: p.fdel.clone(), doc: p.doc.clone() };
    match slot {
        0 => np.fget = a[1].clone(),
        1 => np.fset = a[1].clone(),
        _ => np.fdel = a[1].clone(),
    }
    let kind = Kind::Property(np);
    Ok(Value::Obj(match &o.cls {
        Some(c) => Object::with_cls(c.clone(), kind),
        None => Object::new(kind),
    }))
}

fn prop_getter(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    prop_with(it, a, 0)
}
fn prop_setter(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    prop_with(it, a, 1)
}
fn prop_deleter(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    prop_with(it, a, 2)
}

fn none_bool(_it: &mut Interp, _a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    Ok(Value::Bool(false))
}

fn prop_get(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    it.check_args("__get__", a, 2, 3)?;
    if a[1].is_none() {
        return Ok(a[0].clone());
    }
    let cls = it.type_of(&a[1]);
    it.bind_descr(&a[0], &a[1], &cls)
}

fn prop_set(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    it.check_args("__set__", a, 3, 3)?;
    if let Value::Obj(o) = &a[0] {
        if let Kind::Property(p) = &o.kind {
            if p.fset.is_none() {
                return Err(it.new_exc_str("AttributeError", "property has no setter"));
            }
            let f = p.fset.clone();
            it.call(&f, vec![a[1].clone(), a[2].clone()], Vec::new())?;
            return Ok(Value::None);
        }
    }
    Err(it.type_error("not a property"))
}

fn static_new(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    it.check_args("staticmethod", a, 2, 2)?;
    Ok(Value::Obj(Object::new(Kind::StaticMethod(a[1].clone()))))
}

fn class_new(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    it.check_args("classmethod", a, 2, 2)?;
    Ok(Value::Obj(Object::new(Kind::ClassMethod(a[1].clone()))))
}

fn wrapper_get(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    it.check_args("__get__", a, 2, 3)?;
    let cls = match a.get(2) {
        Some(Value::Obj(c)) if matches!(c.kind, Kind::Type(_)) => c.clone(),
        _ => it.type_of(&a[1]),
    };
    if a[1].is_none() {
        return it.bind_descr_cls(&a[0], &cls);
    }
    it.bind_descr(&a[0], &a[1], &cls)
}

// ---- super -------------------------------------------------------------------------------------

fn super_new(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    let args = &a[1..];
    let (typ, inst) = if args.is_empty() {
        let fr = match it.frames.last() {
            Some(f) => f,
            None => return Err(it.new_exc_str("RuntimeError", "super(): no current frame")),
        };
        let code = fr.code.clone();
        let ci = code.freevars.iter().position(|n| &**n == "__class__");
        let class_val = ci.and_then(|i| fr.cells.get(code.cellvars.len() + i)).and_then(|c| match &c.kind {
            Kind::Cell(v) => v.borrow().clone(),
            _ => None,
        });
        let first = if code.argcount > 0 {
            let local = fr.locals.first().cloned().flatten();
            match local {
                Some(v) => Some(v),
                None => {
                    let name = &code.varnames[0];
                    code.cellvars.iter().position(|n| n == name).and_then(|i| fr.cells.get(i)).and_then(|c| match &c.kind {
                        Kind::Cell(v) => v.borrow().clone(),
                        _ => None,
                    })
                }
            }
        } else {
            None
        };
        match (class_val, first) {
            (Some(c), Some(f)) => (c, f),
            (None, _) => return Err(it.new_exc_str("RuntimeError", "super(): __class__ cell not found")),
            (_, None) => return Err(it.new_exc_str("RuntimeError", "super(): no arguments")),
        }
    } else if args.len() == 1 {
        (args[0].clone(), Value::None)
    } else if args.len() == 2 {
        (args[0].clone(), args[1].clone())
    } else {
        return Err(it.type_error(&format!("super() takes at most 2 arguments ({} given)", args.len())));
    };
    let typ_obj = match &typ {
        Value::Obj(t) if matches!(t.kind, Kind::Type(_)) => t.clone(),
        _ => return Err(it.type_error("super() argument 1 must be a type")),
    };
    let objtype = if inst.is_none() {
        Value::None
    } else {
        match &inst {
            Value::Obj(io) if matches!(io.kind, Kind::Type(_)) && it.is_subtype(io, &typ_obj) => inst.clone(),
            _ => {
                let t = it.type_of(&inst);
                if !it.is_subtype(&t, &typ_obj) {
                    return Err(it.type_error("super(type, obj): obj must be an instance or subtype of type"));
                }
                Value::Obj(t)
            }
        }
    };
    Ok(Value::Obj(Object::new(Kind::Super(typ, inst, objtype))))
}

fn super_init(_it: &mut Interp, _a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    Ok(Value::None)
}

pub fn init(it: &mut Interp) {
    let t = &it.types;
    let (object, type_, property, staticmethod, classmethod, super_) =
        (t.object.clone(), t.type_.clone(), t.property.clone(), t.staticmethod.clone(), t.classmethod.clone(), t.super_.clone());
    it.reg_new(&object, obj_new);
    it.reg(&object, "__init__", obj_init);
    it.reg(&object, "__setattr__", obj_setattr);
    it.reg(&object, "__delattr__", obj_delattr);
    it.reg(&object, "__getattribute__", obj_getattribute);
    it.reg(&object, "__eq__", obj_eq);
    it.reg(&object, "__ne__", obj_ne);
    it.reg(&object, "__lt__", obj_lt);
    it.reg(&object, "__le__", obj_le);
    it.reg(&object, "__gt__", obj_gt);
    it.reg(&object, "__ge__", obj_ge);
    it.reg(&object, "__hash__", obj_hash);
    it.reg(&object, "__repr__", obj_repr);
    it.reg(&object, "__str__", obj_str);
    it.reg(&object, "__format__", obj_format);
    it.reg(&object, "__dir__", obj_dir);
    it.reg(&object, "__sizeof__", obj_sizeof);
    it.reg(&object, "__reduce_ex__", obj_reduce_ex);
    it.reg_class(&object, "__init_subclass__", obj_init_subclass);
    it.reg_class(&object, "__subclasshook__", obj_subclasshook);
    if let Some(d) = object.dict.borrow().as_ref() {
        it.obj_new = dict_get_str(d, "__new__").and_then(|v| v.as_obj().cloned());
        it.obj_init = dict_get_str(d, "__init__").and_then(|v| v.as_obj().cloned());
    }

    it.reg_new(&type_, type_new);
    it.reg(&type_, "__init__", type_init);
    it.reg(&type_, "__call__", type_call);
    it.reg_class(&type_, "__prepare__", type_prepare);
    it.reg(&type_, "__instancecheck__", type_instancecheck);
    it.reg(&type_, "__subclasscheck__", type_subclasscheck);
    it.reg(&type_, "mro", type_mro);
    it.reg(&type_, "__subclasses__", type_subclasses);
    it.reg(&type_, "__or__", type_or);

    it.reg_new(&property, property_new);
    it.reg(&property, "__init__", property_init);
    it.reg(&property, "getter", prop_getter);
    it.reg(&property, "setter", prop_setter);
    it.reg(&property, "deleter", prop_deleter);
    it.reg(&property, "__get__", prop_get);
    it.reg(&property, "__set__", prop_set);
    let none_type = it.types.none_type.clone();
    it.reg(&none_type, "__bool__", none_bool);
    let function = it.types.function.clone();
    it.reg(&function, "__get__", prop_get);

    it.reg_new(&staticmethod, static_new);
    it.reg(&staticmethod, "__get__", wrapper_get);
    it.reg_new(&classmethod, class_new);
    it.reg(&classmethod, "__get__", wrapper_get);

    it.reg_new(&super_, super_new);
    it.reg(&super_, "__init__", super_init);
}
