//! `mappingproxy` and the descriptor types (`getset_descriptor`, `member_descriptor`) that the
//! standard library probes through `types.py`.

use super::native::*;
use crate::ast::{BinOp, CmpOp};
use crate::bind::{type_object, KwArgs, Py, This};
use crate::object::*;
use crate::vm::*;
use std::rc::Rc;

#[lumen_bind::class(name = "mappingproxy")]
pub struct ProxyOf {
    mapping: Value,
}

impl Interp {
    pub fn mappingproxy_type(&mut self) -> Obj {
        type_object::<ProxyOf>(self)
    }

    pub fn new_mappingproxy(&mut self, mapping: Value) -> Value {
        Py::new(self, ProxyOf { mapping }).into_value()
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

/// The mapping a proxy wraps, or `v` itself.
fn unwrap_proxy(v: &Value) -> Value {
    with_opaque::<ProxyOf, _>(v, |p| p.mapping.clone()).unwrap_or_else(|| v.clone())
}

#[lumen_bind::methods]
impl ProxyOf {
    #[constructor]
    fn new(it: &mut Interp, #[kw] mapping: &Value) -> R<Value> {
        if !it.is_mapping_value(mapping) {
            let t = it.type_name_of(mapping);
            return Err(it.type_error(&format!("mappingproxy() argument must be a mapping, not {}", t)));
        }
        Ok(it.new_mappingproxy(mapping.clone()))
    }

    #[proto(getitem)]
    fn getitem(&self, it: &mut Interp, key: &Value) -> R<Value> {
        it.getitem(&self.mapping, key)
    }

    #[proto(iter)]
    fn iter(&self, it: &mut Interp) -> R<Value> {
        it.get_iter(&self.mapping)
    }

    #[proto(len)]
    fn len(&self, it: &mut Interp) -> R<usize> {
        it.len_of(&self.mapping)
    }

    #[proto(contains)]
    fn contains(&self, it: &mut Interp, key: &Value) -> R<bool> {
        it.contains(&self.mapping, key)
    }

    #[proto(hash)]
    fn hash(&self, it: &mut Interp) -> R<i64> {
        it.hash_value(&self.mapping)
    }

    /// D.__reversed__() -> reverse iterator
    #[method(name = "__reversed__", hint(py(text_signature = "")))]
    fn reversed(&self, it: &mut Interp) -> R<Value> {
        let f = it.builtins_fn("reversed");
        it.call(&f, vec![self.mapping.clone()], Vec::new())
    }

    #[proto(repr)]
    fn repr(&self, it: &mut Interp) -> R<String> {
        Ok(format!("mappingproxy({})", it.repr_of(&self.mapping)?))
    }

    #[proto(str)]
    fn str(&self, it: &mut Interp) -> R<Value> {
        it.str_value(&self.mapping)
    }

    #[proto(eq)]
    fn eq(&self, it: &mut Interp, value: &Value) -> R<Value> {
        it.compare_op(CmpOp::Eq, &self.mapping, value)
    }

    #[proto(ne)]
    fn ne(&self, it: &mut Interp, value: &Value) -> R<Value> {
        it.compare_op(CmpOp::NotEq, &self.mapping, value)
    }

    #[proto(lt)]
    fn lt(&self, it: &mut Interp, value: &Value) -> R<Value> {
        it.compare_op(CmpOp::Lt, &self.mapping, value)
    }

    #[proto(le)]
    fn le(&self, it: &mut Interp, value: &Value) -> R<Value> {
        it.compare_op(CmpOp::LtE, &self.mapping, value)
    }

    #[proto(gt)]
    fn gt(&self, it: &mut Interp, value: &Value) -> R<Value> {
        it.compare_op(CmpOp::Gt, &self.mapping, value)
    }

    #[proto(ge)]
    fn ge(&self, it: &mut Interp, value: &Value) -> R<Value> {
        it.compare_op(CmpOp::GtE, &self.mapping, value)
    }

    #[proto(or)]
    fn or(&self, it: &mut Interp, value: &Value) -> R<Value> {
        it.binary_op(BinOp::BitOr, &self.mapping, &unwrap_proxy(value))
    }

    #[proto(ror)]
    fn ror(&self, it: &mut Interp, value: &Value) -> R<Value> {
        it.binary_op(BinOp::BitOr, &unwrap_proxy(value), &self.mapping)
    }

    #[proto(ior)]
    fn ior(&self, it: &mut Interp, value: &Value) -> R<Value> {
        let _ = value;
        Err(it.type_error("'|=' is not supported by mappingproxy; use '|' instead"))
    }

    /// D.get(k[,d]) -> D[k] if k in D, else d.  d defaults to None.
    #[method(hint(py(text_signature = "")))]
    fn get(&self, it: &mut Interp, key: &Value, default: lumen_bind::Passed<&Value>) -> R<Value> {
        let mut args = vec![key.clone()];
        args.extend(default.0.cloned());
        let m = it.get_attr_str(&self.mapping, "get")?;
        it.call(&m, args, Vec::new())
    }

    /// D.keys() -> a set-like object providing a view on D's keys
    #[method(hint(py(text_signature = "")))]
    fn keys(&self, it: &mut Interp) -> R<Value> {
        it.call_method(&self.mapping, "keys", Vec::new())
    }

    /// D.values() -> an object providing a view on D's values
    #[method(hint(py(text_signature = "")))]
    fn values(&self, it: &mut Interp) -> R<Value> {
        it.call_method(&self.mapping, "values", Vec::new())
    }

    /// D.items() -> a set-like object providing a view on D's items
    #[method(hint(py(text_signature = "")))]
    fn items(&self, it: &mut Interp) -> R<Value> {
        it.call_method(&self.mapping, "items", Vec::new())
    }

    /// D.copy() -> a shallow copy of D
    #[method(hint(py(text_signature = "")))]
    fn copy(&self, it: &mut Interp) -> R<Value> {
        it.call_method(&self.mapping, "copy", Vec::new())
    }

    /// See PEP 585
    #[classmethod(name = "__class_getitem__", hint(py(text_signature = "")))]
    fn class_getitem(cls: This<Value>, it: &mut Interp, item: &Value) -> Value {
        it.make_alias(cls.0, item)
    }
}

// `__class_getitem__` of the core types PEP 585 makes subscriptable.
#[lumen_bind::class(name = "generic", hint(py(shared)))]
pub struct ClassGetitem;

#[lumen_bind::methods]
impl ClassGetitem {
    /// See PEP 585
    #[classmethod(name = "__class_getitem__", hint(py(text_signature = "")))]
    fn class_getitem(cls: This<Value>, it: &mut Interp, item: &Value) -> Value {
        it.make_alias(cls.0, item)
    }
}

// ---- function, method, builtin_function_or_method and module ------------------------------------

// `__call__` of the callable core types.
#[lumen_bind::class(name = "callable", hint(py(shared)))]
pub struct CallSlot;

#[lumen_bind::methods]
impl CallSlot {
    #[proto(call)]
    fn call(slf: This<&Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
        it.call(&slf, args.to_vec(), kwargs.to_vec())
    }

    // `meth_reduce`, `method_reduce` and `descr_reduce`: a bound method pickles as
    // `getattr(self, name)`, a method descriptor as `getattr(type, name)`, a module function as
    // its name.
    #[method(name = "__reduce__", hint(py(text_signature = "")))]
    fn reduce(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        let bound_to = match &*slf {
            Value::Obj(o) => match &o.kind {
                Kind::Method(_, this) => Some(this.clone()),
                Kind::Native(NativeData { owner: Some(NativeOwner::Class(c)), .. }) => Some(Value::Obj(c.clone())),
                _ => None,
            },
            _ => None,
        };
        let name = it.get_attr_str(&slf, "__name__")?;
        match bound_to {
            Some(this) if !matches!(&this, Value::Obj(o) if matches!(o.kind, Kind::Module)) => {
                let getattr = dict_get_str(&it.builtins, "getattr").unwrap_or(Value::None);
                Ok(Value::tuple(vec![getattr, Value::tuple(vec![this, name])]))
            }
            _ => Ok(name),
        }
    }
}

/// Create a bound instance method object.
#[lumen_bind::class(name = "method")]
pub struct MethodType;

#[lumen_bind::methods]
impl MethodType {
    #[constructor(hint(py(text_signature = "(function, instance, /)")))]
    fn new(it: &mut Interp, function: &Value, instance: &Value) -> R<Value> {
        if !it.is_callable(function) {
            return Err(it.type_error("first argument must be callable"));
        }
        if instance.is_none() {
            return Err(it.type_error("instance must not be None"));
        }
        Ok(Value::Obj(Object::new(Kind::Method(function.clone(), instance.clone()))))
    }
}

/// A module object (or subclass instance).
pub struct ModuleRef<'a>(pub &'a Obj);

impl<'a> lumen_bind::FromArg<'a, crate::bind::PyHost> for ModuleRef<'a> {
    #[inline]
    fn from_arg(cx: &'a crate::bind::PyCx<'_>, v: &'a Value, at: lumen_bind::Slot) -> Result<Self, Obj> {
        match v {
            Value::Obj(o) if matches!(o.kind, Kind::Module) => Ok(ModuleRef(o)),
            _ => Err(cx.arg_error(at, "module", v)),
        }
    }
}

/// Create a module object.
///
/// The name must be a string; the optional doc argument can have any type.
#[lumen_bind::class(name = "module")]
pub struct ModuleType;

#[lumen_bind::methods]
impl ModuleType {
    #[constructor(hint(py(text_signature = "(name, doc=None)")))]
    fn new(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> Value {
        let _ = (args, kwargs);
        let Value::Obj(cls) = &*cls else { unreachable!("checked by the entry") };
        let m = it.new_module("");
        if Rc::ptr_eq(cls, &it.types.module) {
            return Value::Obj(m);
        }
        let d = it.module_dict(&m);
        let o = Object::with_cls(cls.clone(), Kind::Module);
        *o.dict.borrow_mut() = Some(d);
        Value::Obj(o)
    }

    #[proto(init)]
    fn init(slf: This<ModuleRef<'_>>, it: &mut Interp, #[kw] name: &Value, #[kw] doc: Option<&Value>) {
        let d = it.module_dict(slf.0 .0);
        dict_set_str(&d, "__name__", name.clone());
        dict_set_str(&d, "__doc__", doc.cloned().unwrap_or(Value::None));
    }

    /// __dir__() -> list
    /// specialized dir() implementation
    #[method(name = "__dir__", hint(py(text_signature = "")))]
    fn dir(slf: This<ModuleRef<'_>>, it: &mut Interp) -> R<Value> {
        let d = it.get_attr_str(&Value::Obj(slf.0 .0.clone()), "__dict__")?;
        let Value::Obj(dobj) = &d else { return Err(it.type_error("<module>.__dict__ is not a dictionary")) };
        if !matches!(dobj.kind, Kind::Dict(_)) {
            return Err(it.type_error("<module>.__dict__ is not a dictionary"));
        }
        if let Some(f) = dict_get_str(dobj, "__dir__") {
            return it.call(&f, Vec::new(), Vec::new());
        }
        let keys = it.call_method(&d, "keys", Vec::new())?;
        Ok(Value::list(it.iterate_to_vec(&keys)?))
    }
}

// ---- getset / member descriptors ----------------------------------------------------------------

// The data descriptors of `function` that CPython exposes as getset/member descriptors.
#[lumen_bind::class(name = "function")]
pub struct FunctionType;

#[lumen_bind::methods]
impl FunctionType {
    #[getter(name = "__code__")]
    fn code(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        Ok(it.special_attr(&slf, "__code__")?.unwrap_or(Value::None))
    }

    #[getter(name = "__globals__")]
    fn globals(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        Ok(it.special_attr(&slf, "__globals__")?.unwrap_or(Value::None))
    }

    #[getter(name = "__closure__")]
    fn closure(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        Ok(it.special_attr(&slf, "__closure__")?.unwrap_or(Value::None))
    }
}

// `__repr__` of `getset_descriptor` and `member_descriptor`.
#[lumen_bind::class(name = "descriptor", hint(py(shared)))]
pub struct DescriptorRepr;

#[lumen_bind::methods]
impl DescriptorRepr {
    #[proto(repr)]
    fn repr(slf: This<&Value>, it: &mut Interp) -> R<String> {
        let name = it.get_attr_str(&slf, "__name__")?;
        let name = it.str_of(&name)?;
        let owner = it.get_attr_str(&slf, "__objclass__")?;
        let owner = match &owner {
            Value::Obj(o) => it.type_display(o),
            _ => String::new(),
        };
        let is_member = matches!(&*slf, Value::Obj(o) if o.cls.as_ref().is_some_and(|c| it.type_name(c) == "member_descriptor"));
        let kind = if is_member { "member" } else { "attribute" };
        Ok(format!("<{kind} '{name}' of '{owner}' objects>"))
    }
}

pub fn init(it: &mut Interp) {
    use crate::bind::{extend_type, install_into};
    let module_ty = it.types.module.clone();
    extend_type::<ModuleType>(it, &module_ty);
    let method_ty = it.types.method.clone();
    extend_type::<MethodType>(it, &method_ty);
    for ty in [
        it.types.list.clone(),
        it.types.tuple.clone(),
        it.types.dict.clone(),
        it.types.set.clone(),
        it.types.frozenset.clone(),
        it.types.generator.clone(),
        it.types.coroutine.clone(),
        it.types.async_generator.clone(),
    ] {
        install_into::<ClassGetitem>(&ty, &["__class_getitem__"]);
    }
    let native_descr = [
        it.types.method_descriptor.clone(),
        it.types.wrapper_descriptor.clone(),
        it.types.classmethod_descriptor.clone(),
        it.types.method_wrapper.clone(),
    ];
    for ty in [it.types.function.clone(), it.types.method.clone(), it.types.builtin_function.clone()].iter().chain(&native_descr) {
        install_into::<CallSlot>(ty, &["__call__"]);
    }
    for ty in [it.types.method.clone(), it.types.builtin_function.clone()].iter().chain(&native_descr) {
        install_into::<CallSlot>(ty, &["__reduce__"]);
    }
    for ty in &native_descr[..3] {
        super::objectm::install_descr_methods_get(ty);
    }
    super::memview::init(it);
    let getset = new_type(it, "builtins", "getset_descriptor", None, Layout::Other);
    let member = new_type(it, "builtins", "member_descriptor", None, Layout::Other);
    for ty in [&getset, &member] {
        super::objectm::install_descr_methods(ty);
        install_into::<DescriptorRepr>(ty, &["__repr__"]);
    }
    *it.native_state::<DescrTypes>() = DescrTypes { getset: Some(getset), member: Some(member) };
    let func = it.types.function.clone();
    install_getsets::<FunctionType>(it, &func, &["__globals__", "__closure__"]);
    super::objectm::install_getset_descriptors(it);
}

#[derive(Default)]
struct DescrTypes {
    getset: Option<Obj>,
    member: Option<Obj>,
}

/// Installs the getters (with their setters) `T` declares into the builtin class `owner` as
/// CPython's `getset_descriptor`s (`member_descriptor`s for the names in `members`).
pub fn install_getsets<T: lumen_bind::Methods<crate::bind::PyHost>>(it: &mut Interp, owner: &Obj, members: &[&str]) {
    use crate::bind::args::py_name;
    use lumen_bind::Role;
    let mut items = Vec::new();
    T::members(&mut items);
    for item in &items {
        if item.desc.role != Role::Getter {
            continue;
        }
        let name = py_name(item.desc);
        let fget = crate::bind::native_value(item);
        let setter = items.iter().find(|s| s.desc.role == Role::Setter && py_name(s.desc) == name);
        let fset = setter.map_or(Value::None, crate::bind::native_value);
        let doc = item.desc.doc.map_or(Value::None, Value::str);
        put_descriptor(it, owner, name, fget, fset, doc, members.contains(&name));
    }
}

/// Installs `fget` into the builtin class `owner` as the read-only `member_descriptor` `name` (a
/// plain property while the descriptor types do not exist yet).
pub fn install_member(it: &mut Interp, owner: &Obj, name: &'static str, fget: Value) {
    if it.native_state::<DescrTypes>().member.is_none() {
        let p = Value::Obj(Object::new(Kind::Property(PropData { fget, fset: Value::None, fdel: Value::None, doc: Value::None, name: Default::default() })));
        if let Some(d) = owner.dict.borrow().as_ref() {
            dict_set_str(d, name, p);
        }
        return;
    }
    put_descriptor(it, owner, name, fget, Value::None, Value::None, true);
}

/// Gives the getset descriptors `name` of the builtin class `owner` the deleter `f` (CPython's
/// setters double as deleters; ours are separate natives).
pub fn install_deleters(it: &mut Interp, owner: &Obj, deleters: &[(&'static str, NativeFn)]) {
    let Some(od) = owner.dict.borrow().clone() else { return };
    for &(name, f) in deleters {
        let Some(Value::Obj(old)) = dict_get_str(&od, name) else { continue };
        let Kind::Property(p) = &old.kind else { continue };
        let fdel = it.new_native(name, f, false);
        let cls = old.cls.clone().unwrap_or_else(|| it.types.property.clone());
        let new = Object::with_cls(cls, Kind::Property(PropData { fget: p.fget.clone(), fset: p.fset.clone(), fdel, doc: p.doc.clone(), name: p.name.clone() }));
        let dict = old.dict.borrow().clone();
        *new.dict.borrow_mut() = dict;
        dict_set_str(&od, name, Value::Obj(new));
    }
}

fn put_descriptor(it: &mut Interp, owner: &Obj, name: &'static str, fget: Value, fset: Value, doc: Value, member: bool) {
    let types = it.native_state::<DescrTypes>();
    let Some(ty) = (if member { types.member.clone() } else { types.getset.clone() }) else { return };
    let p = Object::with_cls(ty, Kind::Property(PropData { fget, fset, fdel: Value::None, doc, name: Default::default() }));
    let d = it.instance_dict(&p);
    let owner_name = it.type_name(owner);
    dict_set_str(&d, "__name__", Value::str(name));
    dict_set_str(&d, "__qualname__", Value::string(format!("{owner_name}.{name}")));
    dict_set_str(&d, "__objclass__", Value::Obj(owner.clone()));
    if let Some(od) = owner.dict.borrow().as_ref() {
        dict_set_str(od, name, Value::Obj(p));
    }
}
