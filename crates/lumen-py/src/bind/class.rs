//! Native classes and modules in Python: the per-interpreter type registry, the members a
//! `#[methods]` block installs, typed instance handles ([`Py`]) and native iterators.
//!
//! Python hints on `#[class]`: `hint(py(unhashable))` (`__hash__ = None`), `hint(py(native_iter))`
//! (the constructor returns a [`NativeIter`] the VM steps directly), `hint(py(final))` (cannot be
//! subclassed), `hint(py(base = "module.Class"))` (a Python base class, imported when the type is
//! first created).

use super::args::{self, py_name, HOST};
use super::PyHost;
use crate::builtins::native::new_type;
use crate::object::*;
use crate::vm::*;
use lumen_bind::{flags, Class, FnDesc, FnItem, Methods, Module, ModuleItems, Role, CLASS_GENERIC};
use std::any::{Any, TypeId};
use std::cell::{Ref, RefCell, RefMut};
use std::marker::PhantomData;

/// A `builtin_function_or_method` object for a bound fn.
pub fn native_value(item: &FnItem<PyHost>) -> Value {
    let d = item.desc;
    Value::Obj(Object::new(Kind::Native(NativeData {
        name: py_name(d),
        f: item.entry,
        method: matches!(d.role, Role::Method | Role::Proto(_)),
        desc: Some(d),
        owner: None,
    })))
}

/// The type object of `T` in this interpreter, created (and its members installed) on first use.
pub fn type_object<T: Methods<PyHost>>(it: &mut Interp) -> Obj {
    if let Some(t) = it.native_types.get(&TypeId::of::<T>()) {
        return t.clone();
    }
    let c = T::DESC;
    let module = c.module.unwrap_or("builtins");
    let base = c.hint(HOST, "base").and_then(|path| python_base(it, path));
    let ty = new_type(it, module, c.name_for(HOST), base.as_ref(), Layout::Other);
    it.native_types.insert(TypeId::of::<T>(), ty.clone());
    // Like CPython's extension types, native classes reject attribute assignment unless they are
    // declared `mutable` (`ast.AST`, whose node classes are mutable heap types there too).
    if c.hint(HOST, "mutable").is_none() {
        if let Kind::Type(td) = &ty.kind {
            td.flags.set(td.flags.get() | TF_IMMUTABLE);
        }
    }
    if c.hint(HOST, "final").is_some() {
        if let Kind::Type(td) = &ty.kind {
            td.flags.set(td.flags.get() | TF_FINAL);
        }
    }
    if c.hint(HOST, "native_iter").is_some() {
        if let Kind::Type(td) = &ty.kind {
            td.flags.set(td.flags.get() & !TF_DISPATCH);
        }
        crate::builtins::slots::reg_iterator(it, &ty);
    }
    let mut members = Vec::new();
    T::members(&mut members);
    install_members(&ty, &members);
    if let Some(d) = ty.dict.borrow().as_ref() {
        if c.hint(HOST, "unhashable").is_some() {
            dict_set_str(d, "__hash__", Value::None);
        }
        if dict_get_str(d, "__doc__").is_none() {
            dict_set_str(d, "__doc__", c.doc.map_or(Value::None, Value::str));
        }
        let sig = members.iter().find(|m| m.desc.role == Role::Constructor).and_then(|m| args::text_signature(m.desc));
        dict_set_str(d, "__text_signature__", sig.map_or(Value::None, Value::string));
        if !members.iter().any(|m| m.desc.role == Role::Constructor) {
            let f = Value::Obj(Object::new(Kind::Native(NativeData { name: "__new__", f: no_new, method: false, desc: None, owner: None })));
            dict_set_str(d, "__new__", f);
        }
        if c.flags & CLASS_GENERIC != 0 {
            let f = Value::Obj(Object::new(Kind::Native(NativeData {
                name: "__class_getitem__",
                f: class_getitem,
                method: false,
                desc: None,
                owner: None,
            })));
            dict_set_str(d, "__class_getitem__", Value::Obj(Object::new(Kind::ClassMethod(f))));
        }
    }
    ty
}

/// Install `T`'s members into the existing type `ty` and make `ty` `T`'s type object: the
/// methods of a core type (`str`, `OSError`, ...) whose instances are the interpreter's own
/// kinds rather than opaque native state. `T` is a marker struct named after the type.
pub fn extend_type<T: Methods<PyHost>>(it: &mut Interp, ty: &Obj) {
    it.native_types.insert(TypeId::of::<T>(), ty.clone());
    let mut members = Vec::new();
    T::members(&mut members);
    install_members(ty, &members);
}

/// The class `module.name` of a `base` hint, imported on first use of the native class.
fn python_base(it: &mut Interp, path: &str) -> Option<Obj> {
    let (module, name) = path.rsplit_once('.')?;
    let m = it.import_module(module).ok()?;
    match it.get_attr_str(&Value::Obj(m), name) {
        Ok(Value::Obj(t)) if matches!(t.kind, Kind::Type(_)) => Some(t),
        _ => None,
    }
}

fn install_members(ty: &Obj, members: &[FnItem<PyHost>]) {
    let Some(d) = ty.dict.borrow().clone() else { return };
    for m in members {
        let desc = m.desc;
        if !desc.exposed_to(HOST) {
            continue;
        }
        let names = std::iter::once(py_name(desc)).chain(args::aliases(desc));
        for name in names {
            let f = native_value(m);
            let v = match desc.role {
                Role::Static if desc.has(flags::CLASS_RECV) => Value::Obj(Object::new(Kind::ClassMethod(f))),
                Role::Static => Value::Obj(Object::new(Kind::StaticMethod(f))),
                Role::Getter | Role::Setter => {
                    let (mut fget, mut fset) = match dict_get_str(&d, name) {
                        Some(Value::Obj(p)) => match &p.kind {
                            Kind::Property(pd) => (pd.fget.clone(), pd.fset.clone()),
                            _ => (Value::None, Value::None),
                        },
                        _ => (Value::None, Value::None),
                    };
                    if desc.role == Role::Getter {
                        fget = f;
                    } else {
                        fset = f;
                    }
                    let doc = desc.doc.map(Value::str).unwrap_or(Value::None);
                    Value::Obj(Object::new(Kind::Property(PropData { fget, fset, fdel: Value::None, doc })))
                }
                _ => f,
            };
            dict_set_str(&d, name, v);
        }
    }
}

/// `__new__` of a native class without a constructor.
fn no_new(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    let name = match a.first() {
        Some(Value::Obj(t)) => it.type_display(t),
        _ => "object".to_string(),
    };
    Err(it.type_error(&format!("cannot create '{}' instances", name)))
}

/// `cls[item]` for `generic` classes (`deque[int]`).
fn class_getitem(it: &mut Interp, a: &[Value], kw: &[(Obj, Value)]) -> R<Value> {
    if !kw.is_empty() {
        return Err(it.type_error("__class_getitem__() takes no keyword arguments"));
    }
    if a.len() != 2 {
        let msg = format!("__class_getitem__() takes exactly one argument ({} given)", a.len().saturating_sub(1));
        return Err(it.type_error(&msg));
    }
    Ok(it.make_alias(a[0].clone(), &a[1]))
}

/// A fresh module object holding everything `M` declares (registered in `sys.modules` first, so
/// its init hook may import it back).
pub fn module_object<M: Module<PyHost>>(it: &mut Interp) -> R<Obj> {
    let desc = M::DESC;
    let name = desc.name_for(HOST);
    let m = it.new_module(name);
    it.register_module(name, &m);
    let d = it.module_dict(&m);
    if let Some(doc) = desc.doc {
        dict_set_str(&d, "__doc__", Value::str(doc));
    }
    let items = ModuleItems::<PyHost>::of::<M>();
    for f in &items.functions {
        if !f.desc.exposed_to(HOST) {
            continue;
        }
        for n in std::iter::once(py_name(f.desc)).chain(args::aliases(f.desc)) {
            dict_set_str(&d, n, native_value(f));
        }
    }
    for c in &items.classes {
        if !c.desc.exposed_to(HOST) {
            continue;
        }
        let t = (c.object)(it)?;
        dict_set_str(&d, c.desc.name_for(HOST), t);
    }
    for k in &items.constants {
        let v = (k.value)(it)?;
        dict_set_str(&d, k.name, v);
    }
    if let Some(init) = items.init {
        init(it, &Value::Obj(m.clone()))?;
    }
    Ok(m)
}

/// `__module__`/`__qualname__` owner of a bound native: the module of a function, the qualified
/// class of a member.
pub fn owner_of(d: &FnDesc) -> String {
    match d.class() {
        Some(c) => args::class_qualname(c),
        None => d.module().unwrap_or("").to_string(),
    }
}

// ---- instances ------------------------------------------------------------------------------

/// The opaque cell of an instance of a native class.
#[inline]
pub(super) fn opaque_cell(v: &Value) -> Option<&RefCell<Box<dyn Any>>> {
    match v {
        Value::Obj(o) => match &o.kind {
            Kind::Opaque(c) => Some(c),
            _ => None,
        },
        _ => None,
    }
}

/// A new instance of class `cls` holding `value`.
pub fn opaque_instance<T: Any>(cls: &Obj, value: T) -> Value {
    Value::Obj(Object::with_cls(cls.clone(), Kind::Opaque(RefCell::new(Box::new(value)))))
}

/// Whether `v` is an instance of `T` (exact type or a Python subclass).
pub fn is_instance<T: Class>(it: &Interp, v: &Value) -> bool {
    let Some(cell) = opaque_cell(v) else { return false };
    match cell.try_borrow() {
        Ok(b) => b.is::<T>(),
        Err(_) => match (it.native_types.get(&TypeId::of::<T>()), v) {
            (Some(t), Value::Obj(o)) => it.is_subtype(&it.type_of_obj(o), t),
            _ => false,
        },
    }
}

#[cold]
#[inline(never)]
pub(super) fn reentrant<T: Class>(it: &mut Interp) -> Obj {
    let q = args::class_qualname(T::DESC);
    it.new_exc_str("RuntimeError", &format!("reentrant access to a '{}' object", q))
}

/// A typed handle to an instance of a native class: the Python object, not borrowed. Take
/// `slf: This<Py<Self>>` instead of `&mut self` in methods that call back into Python
/// (comparisons, iteration, user callbacks), and borrow only around the Rust-side state access.
pub struct Py<T> {
    v: Value,
    _t: PhantomData<fn() -> T>,
}

impl<T> Clone for Py<T> {
    fn clone(&self) -> Self {
        Py { v: self.v.clone(), _t: PhantomData }
    }
}

impl<T: Class> Py<T> {
    pub(super) fn from_value_unchecked(v: Value) -> Py<T> {
        Py { v, _t: PhantomData }
    }

    /// A new instance of `T`'s own class.
    pub fn new(it: &mut Interp, value: T) -> Py<T>
    where
        T: Methods<PyHost>,
    {
        let cls = type_object::<T>(it);
        Py::from_value_unchecked(opaque_instance(&cls, value))
    }

    /// `v` as a `Py<T>`, if it is an instance of `T`.
    pub fn from_value(it: &Interp, v: &Value) -> Option<Py<T>> {
        is_instance::<T>(it, v).then(|| Py::from_value_unchecked(v.clone()))
    }

    pub fn value(&self) -> &Value {
        &self.v
    }

    pub fn into_value(self) -> Value {
        self.v
    }

    fn cell(&self) -> &RefCell<Box<dyn Any>> {
        opaque_cell(&self.v).expect("Py<T> holds a native instance")
    }

    /// Shared borrow; `RuntimeError` when the object is borrowed mutably (re-entrancy).
    pub fn borrow(&self, it: &mut Interp) -> R<Ref<'_, T>> {
        match self.cell().try_borrow() {
            Ok(b) => Ok(Ref::map(b, |b| b.downcast_ref::<T>().expect("Py<T> type"))),
            Err(_) => Err(reentrant::<T>(it)),
        }
    }

    /// Exclusive borrow; `RuntimeError` when the object is already borrowed (re-entrancy).
    pub fn borrow_mut(&self, it: &mut Interp) -> R<RefMut<'_, T>> {
        match self.cell().try_borrow_mut() {
            Ok(b) => Ok(RefMut::map(b, |b| b.downcast_mut::<T>().expect("Py<T> type"))),
            Err(_) => Err(reentrant::<T>(it)),
        }
    }

    /// Runs `f` on the state, borrowed only for `f`.
    pub fn with<X>(&self, it: &mut Interp, f: impl FnOnce(&mut T) -> X) -> R<X> {
        let mut b = self.borrow_mut(it)?;
        Ok(f(&mut b))
    }
}

/// A native iterator: a step closure returning the next item or `None` when exhausted. As the
/// result of the constructor of a `native_iter` class it becomes an instance of that class that
/// the VM steps directly (no `__next__` lookup per item).
pub struct NativeIter(pub Box<dyn FnMut(&mut Interp) -> R<Option<Value>>>);

impl NativeIter {
    pub fn new(f: impl FnMut(&mut Interp) -> R<Option<Value>> + 'static) -> NativeIter {
        NativeIter(Box::new(f))
    }

    /// An instance of class `cls` stepping this closure.
    pub fn into_object(self, cls: &Obj) -> Value {
        Value::Obj(Object::with_cls(cls.clone(), Kind::Iter(RefCell::new(IterState::Native(self.0)))))
    }

    /// An instance of the native class `T` (a `native_iter` class) stepping this closure.
    pub fn instance_of<T: Methods<PyHost>>(self, it: &mut Interp) -> Value {
        let cls = type_object::<T>(it);
        self.into_object(&cls)
    }
}
