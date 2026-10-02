//! `types.GenericAlias` (`list[int]`) and `types.UnionType` (`int | str`).

use super::native::*;
use crate::ast::CmpOp;
use crate::bind::{type_object, KwArgs, Py, This};
use crate::object::*;
use crate::vm::*;
use std::rc::Rc;

/// Represent a PEP 585 generic type
///
/// E.g. for t = list[int], t.__origin__ is list and t.__args__ is (int,).
#[lumen_bind::class(name = "GenericAlias", module = "types")]
pub struct AliasData {
    origin: Value,
    args: Vec<Value>,
    /// `*list[int]` (an alias unpacked by iteration).
    starred: bool,
}

/// Represent a PEP 604 union type
///
/// E.g. for int | str
#[lumen_bind::class(name = "UnionType", module = "types")]
pub struct UnionData {
    args: Vec<Value>,
}

pub fn union_members(v: &Value) -> Option<Vec<Value>> {
    with_opaque::<UnionData, _>(v, |u| u.args.clone())
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
    pub fn make_alias(&mut self, origin: Value, key: &Value) -> Value {
        let args = match key.tuple_items() {
            Some(t) => t.to_vec(),
            None => vec![key.clone()],
        };
        self.new_alias(origin, args, false)
    }

    fn new_alias(&mut self, origin: Value, args: Vec<Value>, starred: bool) -> Value {
        Py::new(self, AliasData { origin, args, starred }).into_value()
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

    /// `alias[key]` for a generic alias or union with type parameters: `args` with each
    /// parameter replaced by its argument (nested generics substituted in turn).
    fn subst_args(&mut self, this: &Value, args: &[Value], key: &Value) -> R<Vec<Value>> {
        let params = self.collect_parameters(args)?;
        if params.is_empty() {
            let r = self.repr_of(this)?;
            return Err(self.type_error(&format!("{} is not a generic class", r)));
        }
        let given = match key.tuple_items() {
            Some(t) => t.to_vec(),
            None => vec![key.clone()],
        };
        if given.len() != params.len() {
            let r = self.repr_of(this)?;
            let word = if given.len() > params.len() { "many" } else { "few" };
            return Err(self.type_error(&format!("Too {} arguments for {}; actual {}, expected {}", word, r, given.len(), params.len())));
        }
        let mut new_args = Vec::new();
        for arg in args {
            let mut replaced = None;
            for (p, g) in params.iter().zip(given.iter()) {
                if arg.is(p) || self.values_eq(arg, p)? {
                    replaced = Some(g.clone());
                    break;
                }
            }
            if let Some(v) = replaced {
                new_args.push(v);
                continue;
            }
            if let Value::Obj(o) = arg {
                if !matches!(o.kind, Kind::Type(_)) && self.get_attr_str(arg, "__parameters__").is_ok() {
                    let sub = self.get_attr_str(arg, "__parameters__")?;
                    let sub_params = sub.tuple_items().map(|t| t.to_vec()).unwrap_or_default();
                    if !sub_params.is_empty() {
                        let mut pick = Vec::new();
                        for sp in &sub_params {
                            for (p, g) in params.iter().zip(given.iter()) {
                                if sp.is(p) || self.values_eq(sp, p)? {
                                    pick.push(g.clone());
                                }
                            }
                        }
                        let r = self.getitem(arg, &Value::tuple(pick))?;
                        new_args.push(r);
                        continue;
                    }
                }
            }
            new_args.push(arg.clone());
        }
        Ok(new_args)
    }

    /// `a | b` for the operand kinds that make a union; `None` when they do not.
    pub fn union_binop(&mut self, a: &Value, b: &Value) -> R<Option<Value>> {
        let ok = |v: &Value| -> bool {
            match v {
                Value::None => true,
                Value::Obj(o) => {
                    matches!(o.kind, Kind::Type(_))
                        || with_opaque::<AliasData, _>(v, |_| ()).is_some()
                        || with_opaque::<UnionData, _>(v, |_| ()).is_some()
                        || super::typingm::is_type_alias(v)
                }
                _ => false,
            }
        };
        if !(ok(a) && ok(b)) || (a.is_none() && b.is_none()) {
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
        Ok(Py::new(self, UnionData { args: flat }).into_value())
    }
}

#[lumen_bind::methods]
impl AliasData {
    #[constructor(hint(py(text_signature = "")))]
    fn new(cls: This<Value>, it: &mut Interp, origin: &Value, args: &Value) -> Value {
        let Value::Obj(cls) = &*cls else { unreachable!("checked by the entry") };
        let alias = it.make_alias(origin.clone(), args);
        let base = type_object::<AliasData>(it);
        if Rc::ptr_eq(cls, &base) {
            return alias;
        }
        let data = with_opaque::<AliasData, _>(&alias, |d| AliasData { origin: d.origin.clone(), args: d.args.clone(), starred: false });
        new_opaque(cls, data.expect("a new alias"))
    }

    #[method(name = "__getattribute__", hint(py(text_signature = "")))]
    fn getattribute(slf: This<Py<Self>>, it: &mut Interp, name: &Value) -> R<Value> {
        let this = slf.0.value().clone();
        let Value::Obj(n) = name else { return Err(it.type_error("attribute name must be string")) };
        if ALIAS_OWN.contains(&name.as_str().unwrap_or("")) {
            let cls = it.type_of(&this);
            return it.generic_getattr(&this, &cls, n);
        }
        let origin = slf.0.borrow(it)?.origin.clone();
        it.get_attr(&origin, n)
    }

    #[method(name = "__setattr__", hint(py(text_signature = "")))]
    fn setattr(&self, it: &mut Interp, name: &Value, value: &Value) -> R<()> {
        if ALIAS_OWN.contains(&name.as_str().unwrap_or("")) {
            return Err(it.new_exc_str("AttributeError", "readonly attribute"));
        }
        let Value::Obj(n) = name else { return Err(it.type_error("attribute name must be string")) };
        it.set_attr(&self.origin, n, value.clone())
    }

    #[proto(repr)]
    fn repr(&self, it: &mut Interp) -> R<String> {
        let o = it.type_arg_repr(&self.origin)?;
        let inner = it.alias_args_repr(&self.args)?;
        Ok(format!("{}{}[{}]", if self.starred { "*" } else { "" }, o, inner))
    }

    #[proto(call)]
    fn call(slf: This<Py<Self>>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
        let this = slf.0.value().clone();
        let origin = slf.0.borrow(it)?.origin.clone();
        let r = it.call(&origin, args.to_vec(), kwargs.to_vec())?;
        if let Value::Obj(_) = &r {
            let name = it.str_obj("__orig_class__");
            let cls = it.type_of(&r);
            if let Err(e) = it.generic_setattr(&r, &cls, &name, this) {
                if !(it.exc_is(&e, "AttributeError") || it.exc_is(&e, "TypeError")) {
                    return Err(e);
                }
            }
        }
        Ok(r)
    }

    #[proto(getitem)]
    fn getitem(slf: This<Py<Self>>, it: &mut Interp, key: &Value) -> R<Value> {
        let this = slf.0.value().clone();
        let (origin, args) = {
            let d = slf.0.borrow(it)?;
            (d.origin.clone(), d.args.clone())
        };
        let new_args = it.subst_args(&this, &args, key)?;
        Ok(it.new_alias(origin, new_args, false))
    }

    #[proto(eq)]
    fn eq(&self, it: &mut Interp, value: &Value) -> R<Value> {
        let Some((o2, a2, s2)) = with_opaque::<AliasData, _>(value, |d| (d.origin.clone(), d.args.clone(), d.starred)) else {
            return Ok(Value::NotImplemented);
        };
        if self.starred != s2 || !it.values_eq(&self.origin, &o2)? {
            return Ok(Value::Bool(false));
        }
        it.compare_op(CmpOp::Eq, &Value::tuple(self.args.clone()), &Value::tuple(a2))
    }

    #[proto(hash)]
    fn hash(&self, it: &mut Interp) -> R<i64> {
        let h1 = it.hash_value(&self.origin)?;
        let h2 = it.hash_value(&Value::tuple(self.args.clone()))?;
        Ok(h1 ^ h2)
    }

    #[method(name = "__mro_entries__", hint(py(text_signature = "")))]
    fn mro_entries(&self, bases: &Value) -> (Value,) {
        let _ = bases;
        (self.origin.clone(),)
    }

    #[method(name = "__instancecheck__", hint(py(text_signature = "")))]
    fn instancecheck(&self, it: &mut Interp, obj: &Value) -> R<bool> {
        let _ = obj;
        Err(it.type_error("isinstance() argument 2 cannot be a parameterized generic"))
    }

    #[method(name = "__subclasscheck__", hint(py(text_signature = "")))]
    fn subclasscheck(&self, it: &mut Interp, cls: &Value) -> R<bool> {
        let _ = cls;
        Err(it.type_error("issubclass() argument 2 cannot be a parameterized generic"))
    }

    #[proto(or)]
    fn or(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        Ok(it.union_binop(&slf, value)?.unwrap_or(Value::NotImplemented))
    }

    #[proto(ror)]
    fn ror(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        Ok(it.union_binop(value, &slf)?.unwrap_or(Value::NotImplemented))
    }

    #[proto(iter)]
    fn iter(&self, it: &mut Interp) -> R<Value> {
        let v = it.new_alias(self.origin.clone(), self.args.clone(), true);
        it.native_get_iter(&Value::tuple(vec![v]))
    }

    #[method(name = "__reduce__", hint(py(text_signature = "")))]
    fn reduce(&self, it: &mut Interp) -> R<Value> {
        let ty = Value::Obj(type_object::<AliasData>(it));
        let plain = Value::tuple(vec![ty, Value::tuple(vec![self.origin.clone(), Value::tuple(self.args.clone())])]);
        if !self.starred {
            return Ok(plain);
        }
        // `*alias` pickles as `next(iter(alias))`.
        let alias = it.new_alias(self.origin.clone(), self.args.clone(), false);
        let iter = it.builtins_fn("iter");
        let next = it.builtins_fn("next");
        let _ = plain;
        let inner = it.call(&iter, vec![alias], Vec::new())?;
        Ok(Value::tuple(vec![next, Value::tuple(vec![inner])]))
    }

    #[getter(name = "__origin__")]
    fn origin(&self) -> Value {
        self.origin.clone()
    }

    #[getter(name = "__args__")]
    fn args(&self) -> Value {
        Value::tuple(self.args.clone())
    }

    /// Type variables in the GenericAlias.
    #[getter(name = "__parameters__")]
    fn parameters(&self, it: &mut Interp) -> R<Value> {
        Ok(Value::tuple(it.collect_parameters(&self.args)?))
    }

    #[getter(name = "__unpacked__")]
    fn unpacked(&self) -> bool {
        self.starred
    }

    #[getter(name = "__typing_unpacked_tuple_args__")]
    fn typing_unpacked_tuple_args(&self, it: &mut Interp) -> Value {
        let is_tuple = matches!(&self.origin, Value::Obj(o) if Rc::ptr_eq(o, &it.types.tuple));
        if self.starred && is_tuple {
            Value::tuple(self.args.clone())
        } else {
            Value::None
        }
    }
}

#[lumen_bind::methods]
impl UnionData {
    #[proto(repr)]
    fn repr(&self, it: &mut Interp) -> R<String> {
        let mut parts = Vec::new();
        for x in &self.args {
            parts.push(it.type_arg_repr(x)?);
        }
        Ok(parts.join(" | "))
    }

    #[proto(eq)]
    fn eq(&self, it: &mut Interp, value: &Value) -> R<Value> {
        let Some(b) = with_opaque::<UnionData, _>(value, |d| d.args.clone()) else { return Ok(Value::NotImplemented) };
        if self.args.len() != b.len() {
            return Ok(Value::Bool(false));
        }
        for v in &self.args {
            if !it.contains_value(&b, v)? {
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    }

    #[proto(hash)]
    fn hash(&self, it: &mut Interp) -> R<i64> {
        let fs = it.new_frozenset_from(self.args.clone())?;
        it.hash_value(&fs)
    }

    #[proto(or)]
    fn or(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        Ok(it.union_binop(&slf, value)?.unwrap_or(Value::NotImplemented))
    }

    #[proto(ror)]
    fn ror(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        Ok(it.union_binop(value, &slf)?.unwrap_or(Value::NotImplemented))
    }

    #[proto(getitem)]
    fn getitem(slf: This<Py<Self>>, it: &mut Interp, key: &Value) -> R<Value> {
        let this = slf.0.value().clone();
        let args = slf.0.borrow(it)?.args.clone();
        let new_args = it.subst_args(&this, &args, key)?;
        it.make_union(new_args)
    }

    #[getter(name = "__args__")]
    fn args(&self) -> Value {
        Value::tuple(self.args.clone())
    }

    /// Type variables in the types.UnionType.
    #[getter(name = "__parameters__")]
    fn parameters(&self, it: &mut Interp) -> R<Value> {
        Ok(Value::tuple(it.collect_parameters(&self.args)?))
    }
}

/// Creates both types, their attributes being CPython's member and getset descriptors.
pub fn init(it: &mut Interp) {
    let generic = type_object::<AliasData>(it);
    super::descr::install_getsets::<AliasData>(it, &generic, &["__origin__", "__args__", "__unpacked__"]);
    let union = type_object::<UnionData>(it);
    super::descr::install_getsets::<UnionData>(it, &union, &["__args__"]);
}
