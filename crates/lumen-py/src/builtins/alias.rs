//! `types.GenericAlias` (`list[int]`) and `types.UnionType` (`int | str`).

use super::native::*;
use crate::ast::CmpOp;
use crate::object::*;
use crate::vm::*;
use std::rc::Rc;

pub struct AliasData {
    origin: Value,
    args: Vec<Value>,
}

pub struct UnionData {
    args: Vec<Value>,
}

pub fn union_members(v: &Value) -> Option<Vec<Value>> {
    with_opaque::<UnionData, _>(v, |u| u.args.clone())
}

pub struct AliasTypes {
    pub generic: Obj,
    pub union: Obj,
}

const ALIAS_OWN: &[&str] = &[
    "__origin__",
    "__args__",
    "__parameters__",
    "__mro_entries__",
    "__reduce_ex__",
    "__reduce__",
    "__copy__",
    "__deepcopy__",
    "__unpacked__",
    "__typing_unpacked_tuple_args__",
    "__class__",
    "__getitem__",
    "__call__",
    "__repr__",
    "__eq__",
    "__hash__",
    "__or__",
    "__ror__",
    "__iter__",
    "__instancecheck__",
    "__subclasscheck__",
    "__setattr__",
    "__dict__",
];

impl Interp {
    pub fn alias_types(&mut self) -> Rc<AliasTypes> {
        if let Some(t) = &self.alias_types {
            return t.clone();
        }
        let generic = new_type(self, "types", "GenericAlias", None, Layout::Other);
        self.reg_new(&generic, generic_new);
        self.reg(&generic, "__init__", noop_init);
        self.reg(&generic, "__getattribute__", generic_getattribute);
        self.reg(&generic, "__setattr__", generic_setattr);
        self.reg(&generic, "__repr__", generic_repr);
        self.reg(&generic, "__call__", generic_call);
        self.reg(&generic, "__getitem__", generic_getitem);
        self.reg(&generic, "__eq__", generic_eq);
        self.reg(&generic, "__hash__", generic_hash);
        self.reg(&generic, "__mro_entries__", generic_mro_entries);
        self.reg(&generic, "__instancecheck__", generic_instancecheck);
        self.reg(&generic, "__subclasscheck__", generic_instancecheck);
        self.reg(&generic, "__or__", generic_or);
        self.reg(&generic, "__ror__", generic_ror);
        self.reg(&generic, "__iter__", generic_iter);
        self.reg(&generic, "__reduce__", generic_reduce);
        self.reg_prop(&generic, "__origin__", generic_origin);
        self.reg_prop(&generic, "__args__", generic_args);
        self.reg_prop(&generic, "__parameters__", generic_parameters);
        self.reg_prop(&generic, "__unpacked__", generic_unpacked);
        self.reg_prop(&generic, "__typing_unpacked_tuple_args__", generic_unpacked_args);

        let union = new_type(self, "types", "UnionType", None, Layout::Other);
        self.reg(&union, "__repr__", union_repr);
        self.reg(&union, "__eq__", union_eq);
        self.reg(&union, "__hash__", union_hash);
        self.reg(&union, "__or__", union_or);
        self.reg(&union, "__ror__", union_ror);
        self.reg(&union, "__getitem__", union_getitem);
        self.reg(&union, "__instancecheck__", union_instancecheck);
        self.reg(&union, "__subclasscheck__", union_subclasscheck);
        self.reg_prop(&union, "__args__", union_args);
        self.reg_prop(&union, "__parameters__", union_parameters);
        let t = Rc::new(AliasTypes { generic, union });
        self.alias_types = Some(t.clone());
        t
    }

    pub fn make_alias(&mut self, origin: Value, key: &Value) -> Value {
        let args = match key.tuple_items() {
            Some(t) => t.to_vec(),
            None => vec![key.clone()],
        };
        let ty = self.alias_types().generic.clone();
        new_opaque(&ty, AliasData { origin, args })
    }

    fn type_arg_repr(&mut self, v: &Value) -> R<String> {
        if matches!(v, Value::Ellipsis) {
            return Ok("...".into());
        }
        if matches!(v, Value::None) {
            return Ok("None".into());
        }
        if let Value::Obj(o) = v {
            if matches!(o.kind, Kind::Type(_)) && Rc::ptr_eq(o, &self.types.none_type) {
                return Ok("None".into());
            }
            let has_origin = self.get_attr_str(v, "__origin__").is_ok() && self.get_attr_str(v, "__args__").is_ok();
            if !has_origin {
                if matches!(o.kind, Kind::Type(_)) {
                    return Ok(self.type_display(o));
                }
                let q = self.get_attr_str(v, "__qualname__");
                let m = self.get_attr_str(v, "__module__");
                if let (Ok(q), Ok(m)) = (q, m) {
                    if let (Some(q), Some(m)) = (q.as_str(), m.as_str()) {
                        return Ok(if m == "builtins" { q.to_string() } else { format!("{}.{}", m, q) });
                    }
                }
            }
        }
        self.repr_of(v)
    }

    fn alias_args_repr(&mut self, args: &[Value]) -> R<String> {
        let mut parts = Vec::new();
        if args.is_empty() {
            return Ok("()".into());
        }
        for a in args {
            if let Some(l) = list_of(a) {
                let inner = l.borrow().clone();
                let mut ps = Vec::new();
                for x in &inner {
                    ps.push(self.type_arg_repr(x)?);
                }
                parts.push(format!("[{}]", ps.join(", ")));
            } else {
                parts.push(self.type_arg_repr(a)?);
            }
        }
        Ok(parts.join(", "))
    }

    fn collect_parameters(&mut self, args: &[Value]) -> R<Vec<Value>> {
        let mut out: Vec<Value> = Vec::new();
        for a in args {
            if let Value::Obj(o) = a {
                if matches!(o.kind, Kind::Type(_)) {
                    continue;
                }
                if self.get_attr_str(a, "__typing_subst__").is_ok() {
                    if !self.contains_value(&out, a)? {
                        out.push(a.clone());
                    }
                    continue;
                }
                if let Ok(p) = self.get_attr_str(a, "__parameters__") {
                    if let Some(items) = p.tuple_items() {
                        for x in items.iter().cloned() {
                            if !self.contains_value(&out, &x)? {
                                out.push(x);
                            }
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    fn contains_value(&mut self, items: &[Value], v: &Value) -> R<bool> {
        for x in items {
            if x.is(v) || self.values_eq(x, v)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `a | b` for the operand kinds that make a union; `None` when they do not.
    pub fn union_binop(&mut self, a: &Value, b: &Value) -> R<Option<Value>> {
        let ok = |it: &mut Interp, v: &Value| -> bool {
            match v {
                Value::None => true,
                Value::Obj(o) => {
                    matches!(o.kind, Kind::Type(_))
                        || with_opaque::<AliasData, _>(v, |_| ()).is_some()
                        || with_opaque::<UnionData, _>(v, |_| ()).is_some()
                        || super::typingm::is_type_alias(v)
                        || {
                            let _ = it;
                            false
                        }
                }
                _ => false,
            }
        };
        if !(ok(self, a) && ok(self, b)) {
            return Ok(None);
        }
        let is_typeish = |v: &Value| !v.is_none();
        if !is_typeish(a) && !is_typeish(b) {
            return Ok(None);
        }
        self.make_union(vec![a.clone(), b.clone()]).map(Some)
    }

    pub fn make_union(&mut self, members: Vec<Value>) -> R<Value> {
        let mut flat: Vec<Value> = Vec::new();
        for m in members {
            let inner = with_opaque::<UnionData, _>(&m, |u| u.args.clone());
            let items = match inner {
                Some(args) => args,
                None => vec![if m.is_none() { Value::Obj(self.types.none_type.clone()) } else { m }],
            };
            for x in items {
                if !self.contains_value(&flat, &x)? {
                    flat.push(x);
                }
            }
        }
        if flat.len() == 1 {
            return Ok(flat.pop().unwrap());
        }
        let ty = self.alias_types().union.clone();
        Ok(new_opaque(&ty, UnionData { args: flat }))
    }
}

fn noop_init(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::None)
}

fn generic_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("GenericAlias", kw)?;
    it.check_args("GenericAlias", &a[1.min(a.len())..], 2, 2)?;
    Ok(it.make_alias(a[1].clone(), &a[2]))
}

fn alias_of<X>(it: &mut Interp, v: &Value, f: impl FnOnce(&AliasData) -> X) -> R<X> {
    match with_opaque::<AliasData, _>(v, |d| f(d)) {
        Some(x) => Ok(x),
        None => Err(it.self_state_err("GenericAlias")),
    }
}

fn generic_getattribute(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__getattribute__", a, 2, 2)?;
    let name = a[1].as_str().unwrap_or("").to_string();
    let origin = alias_of(it, &a[0], |d| d.origin.clone())?;
    let Value::Obj(n) = &a[1] else { return Err(it.type_error("attribute name must be string")) };
    if ALIAS_OWN.contains(&name.as_str()) {
        let cls = it.type_of(&a[0]);
        return it.generic_getattr(&a[0], &cls, n);
    }
    it.get_attr(&origin, n)
}

fn generic_setattr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__setattr__", a, 3, 3)?;
    let name = a[1].as_str().unwrap_or("").to_string();
    if ALIAS_OWN.contains(&name.as_str()) {
        return Err(it.new_exc_str("AttributeError", &format!("readonly attribute '{}'", name)));
    }
    let origin = alias_of(it, &a[0], |d| d.origin.clone())?;
    let Value::Obj(n) = &a[1] else { return Err(it.type_error("attribute name must be string")) };
    it.set_attr(&origin, n, a[2].clone())?;
    Ok(Value::None)
}

fn generic_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let (origin, args) = alias_of(it, &a[0], |d| (d.origin.clone(), d.args.clone()))?;
    let o = it.type_arg_repr(&origin)?;
    let inner = it.alias_args_repr(&args)?;
    Ok(Value::string(format!("{}[{}]", o, inner)))
}

fn generic_call(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let origin = alias_of(it, &a[0], |d| d.origin.clone())?;
    let r = it.call(&origin, a[1..].to_vec(), kw.to_vec())?;
    if let Value::Obj(ro) = &r {
        let name = it.str_obj("__orig_class__");
        let cls = it.type_of(&r);
        let _ = ro;
        if let Err(e) = it.generic_setattr(&r, &cls, &name, a[0].clone()) {
            if !(it.exc_is(&e, "AttributeError") || it.exc_is(&e, "TypeError")) {
                return Err(e);
            }
        }
    }
    Ok(r)
}

fn generic_getitem(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__getitem__", a, 2, 2)?;
    let (origin, args) = alias_of(it, &a[0], |d| (d.origin.clone(), d.args.clone()))?;
    let params = it.collect_parameters(&args)?;
    if params.is_empty() {
        let r = it.repr_of(&a[0])?;
        return Err(it.type_error(&format!("{} is not a generic class", r)));
    }
    let given = match a[1].tuple_items() {
        Some(t) => t.to_vec(),
        None => vec![a[1].clone()],
    };
    if given.len() != params.len() {
        let r = it.repr_of(&a[0])?;
        let word = if given.len() > params.len() { "many" } else { "few" };
        return Err(it.type_error(&format!("Too {} arguments for {}; actual {}, expected {}", word, r, given.len(), params.len())));
    }
    let mut new_args = Vec::new();
    for arg in &args {
        let mut replaced = None;
        for (p, g) in params.iter().zip(given.iter()) {
            if arg.is(p) || it.values_eq(arg, p)? {
                replaced = Some(g.clone());
                break;
            }
        }
        match replaced {
            Some(v) => new_args.push(v),
            None => {
                if let Value::Obj(o) = arg {
                    if !matches!(o.kind, Kind::Type(_)) && it.get_attr_str(arg, "__parameters__").is_ok() {
                        let sub = it.get_attr_str(arg, "__parameters__")?;
                        let sub_params = sub.tuple_items().map(|t| t.to_vec()).unwrap_or_default();
                        if !sub_params.is_empty() {
                            let mut pick = Vec::new();
                            for sp in &sub_params {
                                for (p, g) in params.iter().zip(given.iter()) {
                                    if sp.is(p) || it.values_eq(sp, p)? {
                                        pick.push(g.clone());
                                    }
                                }
                            }
                            let r = it.getitem(arg, &Value::tuple(pick))?;
                            new_args.push(r);
                            continue;
                        }
                    }
                }
                new_args.push(arg.clone());
            }
        }
    }
    let ty = it.alias_types().generic.clone();
    Ok(new_opaque(&ty, AliasData { origin, args: new_args }))
}

fn generic_eq(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__eq__", a, 2, 2)?;
    let Some((o2, a2)) = with_opaque::<AliasData, _>(&a[1], |d| (d.origin.clone(), d.args.clone())) else { return Ok(Value::NotImplemented) };
    let (o1, a1) = alias_of(it, &a[0], |d| (d.origin.clone(), d.args.clone()))?;
    if !it.values_eq(&o1, &o2)? {
        return Ok(Value::Bool(false));
    }
    let r = it.compare_op(CmpOp::Eq, &Value::tuple(a1), &Value::tuple(a2))?;
    Ok(r)
}

fn generic_hash(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let (o, args) = alias_of(it, &a[0], |d| (d.origin.clone(), d.args.clone()))?;
    let h1 = it.hash_value(&o)?;
    let h2 = it.hash_value(&Value::tuple(args))?;
    Ok(Value::Int(h1 ^ h2))
}

fn generic_mro_entries(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let o = alias_of(it, &a[0], |d| d.origin.clone())?;
    Ok(Value::tuple(vec![o]))
}

fn generic_instancecheck(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Err(it.type_error("isinstance() argument 2 cannot be a parameterized generic"))
}

fn generic_or(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__or__", a, 2, 2)?;
    Ok(it.union_binop(&a[0], &a[1])?.unwrap_or(Value::NotImplemented))
}

fn generic_ror(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__ror__", a, 2, 2)?;
    Ok(it.union_binop(&a[1], &a[0])?.unwrap_or(Value::NotImplemented))
}

fn generic_iter(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let ty = it.alias_types().generic.clone();
    let copy = alias_of(it, &a[0], |d| (d.origin.clone(), d.args.clone()))?;
    let v = new_opaque(&ty, AliasData { origin: copy.0, args: copy.1 });
    let items = vec![v];
    it.native_get_iter(&Value::tuple(items))
}

fn generic_reduce(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let (o, args) = alias_of(it, &a[0], |d| (d.origin.clone(), d.args.clone()))?;
    let ty = Value::Obj(it.alias_types().generic.clone());
    Ok(Value::tuple(vec![ty, Value::tuple(vec![o, Value::tuple(args)])]))
}

fn generic_origin(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    alias_of(it, &a[0], |d| d.origin.clone())
}

fn generic_args(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::tuple(alias_of(it, &a[0], |d| d.args.clone())?))
}

fn generic_parameters(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let args = alias_of(it, &a[0], |d| d.args.clone())?;
    Ok(Value::tuple(it.collect_parameters(&args)?))
}

fn generic_unpacked(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Bool(false))
}

fn generic_unpacked_args(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::None)
}

fn union_of<X>(it: &mut Interp, v: &Value, f: impl FnOnce(&UnionData) -> X) -> R<X> {
    match with_opaque::<UnionData, _>(v, |d| f(d)) {
        Some(x) => Ok(x),
        None => Err(it.self_state_err("UnionType")),
    }
}

fn union_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let args = union_of(it, &a[0], |d| d.args.clone())?;
    let mut parts = Vec::new();
    for x in &args {
        parts.push(it.type_arg_repr(x)?);
    }
    Ok(Value::string(parts.join(" | ")))
}

fn union_eq(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__eq__", a, 2, 2)?;
    let Some(b) = with_opaque::<UnionData, _>(&a[1], |d| d.args.clone()) else { return Ok(Value::NotImplemented) };
    let x = union_of(it, &a[0], |d| d.args.clone())?;
    if x.len() != b.len() {
        return Ok(Value::Bool(false));
    }
    for v in &x {
        if !it.contains_value(&b, v)? {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(true))
}

fn union_hash(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let args = union_of(it, &a[0], |d| d.args.clone())?;
    let fs = it.new_frozenset_from(args)?;
    Ok(Value::Int(it.hash_value(&fs)?))
}

fn union_or(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__or__", a, 2, 2)?;
    Ok(it.union_binop(&a[0], &a[1])?.unwrap_or(Value::NotImplemented))
}

fn union_ror(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__ror__", a, 2, 2)?;
    Ok(it.union_binop(&a[1], &a[0])?.unwrap_or(Value::NotImplemented))
}

fn union_args(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::tuple(union_of(it, &a[0], |d| d.args.clone())?))
}

fn union_parameters(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let args = union_of(it, &a[0], |d| d.args.clone())?;
    Ok(Value::tuple(it.collect_parameters(&args)?))
}

fn union_getitem(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__getitem__", a, 2, 2)?;
    let args = union_of(it, &a[0], |d| d.args.clone())?;
    let params = it.collect_parameters(&args)?;
    if params.is_empty() {
        let r = it.repr_of(&a[0])?;
        return Err(it.type_error(&format!("{} is not a generic class", r)));
    }
    let given = match a[1].tuple_items() {
        Some(t) => t.to_vec(),
        None => vec![a[1].clone()],
    };
    let mut out = Vec::new();
    for x in args {
        let mut v = x.clone();
        for (p, g) in params.iter().zip(given.iter()) {
            if x.is(p) {
                v = g.clone();
            }
        }
        out.push(v);
    }
    it.make_union(out)
}

fn union_members_check(it: &mut Interp, a: &[Value], subclass: bool) -> R<Value> {
    it.check_args("__instancecheck__", a, 2, 2)?;
    let args = union_of(it, &a[0], |d| d.args.clone())?;
    for x in &args {
        if with_opaque::<AliasData, _>(x, |_| ()).is_some() {
            return Err(it.type_error(if subclass { "issubclass() argument 2 cannot be a parameterized generic" } else { "isinstance() argument 2 cannot be a parameterized generic" }));
        }
    }
    for x in &args {
        let hit = if subclass { it.issubclass_value(&a[1], x)? } else { it.isinstance_value(&a[1], x)? };
        if hit {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

fn union_instancecheck(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    union_members_check(it, a, false)
}

fn union_subclasscheck(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    union_members_check(it, a, true)
}
