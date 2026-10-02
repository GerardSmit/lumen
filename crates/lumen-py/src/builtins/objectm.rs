//! `object`, `type`, `property`, `staticmethod`, `classmethod`, `super` and friends.

use crate::ast::CmpOp;
use crate::bind::{extend_type, extend_type_documented, install_into, KwArgs, PyCx, PyHost, This};
use crate::object::*;
use crate::vm::*;
use lumen_bind::{FromArg, Slot};
use std::rc::Rc;

/// A type object: the receiver of `type`'s methods.
#[derive(Clone, Copy)]
pub struct TypeRef<'a>(pub &'a Obj);

impl<'a> FromArg<'a, PyHost> for TypeRef<'a> {
    #[inline(always)]
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        match v {
            Value::Obj(o) if matches!(o.kind, Kind::Type(_)) => Ok(TypeRef(o)),
            _ => Err(cx.arg_error(at, "type", v)),
        }
    }
}

fn name_obj(it: &mut Interp, v: &Value) -> R<Obj> {
    match v {
        Value::Obj(o) if matches!(o.kind, Kind::Str(_)) => Ok(o.clone()),
        _ => {
            let t = it.type_name_of(v);
            Err(it.type_error(&format!("attribute name must be string, not '{}'", t)))
        }
    }
}

fn is_native(v: Option<Value>, f: &Option<Obj>) -> bool {
    matches!((v, f), (Some(Value::Obj(a)), Some(b)) if Rc::ptr_eq(&a, b))
}

fn compare(it: &mut Interp, op: CmpOp, a: &Value, b: &Value) -> R<Value> {
    Ok(match it.native_compare(op, a, b)? {
        Some(b) => Value::Bool(b),
        None => Value::NotImplemented,
    })
}

#[lumen_bind::class(name = "object")]
/// The base class of the class hierarchy.
///
/// When called, it accepts no arguments and returns a new featureless
/// instance that has no instance attributes and cannot be given any.
///
pub struct ObjectType;

#[lumen_bind::methods]
impl ObjectType {
    #[constructor]
    fn new(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
        let Value::Obj(cls) = &*cls else { unreachable!("checked by the entry") };
        if !args.is_empty() || !kwargs.is_empty() {
            if !is_native(it.lookup_mro(cls, "__new__"), &it.obj_new) {
                return Err(it.type_error("object.__new__() takes exactly one argument (the type to instantiate)"));
            }
            if is_native(it.lookup_mro(cls, "__init__"), &it.obj_init) {
                let msg = format!("{}() takes no arguments", it.type_name(cls));
                return Err(it.type_error(&msg));
            }
        }
        it.alloc_instance(cls)
    }

    #[proto(init)]
    fn init(slf: This<&Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<()> {
        if args.is_empty() && kwargs.is_empty() {
            return Ok(());
        }
        let cls = it.type_of(&slf);
        if !is_native(it.lookup_mro(&cls, "__init__"), &it.obj_init) {
            return Err(it.type_error("object.__init__() takes exactly one argument (the instance to initialize)"));
        }
        if is_native(it.lookup_mro(&cls, "__new__"), &it.obj_new) {
            let msg = format!("{}.__init__() takes exactly one argument (the instance to initialize)", it.type_name(&cls));
            return Err(it.type_error(&msg));
        }
        Ok(())
    }

    #[proto(setattr)]
    fn setattr(slf: This<&Value>, it: &mut Interp, name: &Value, value: &Value) -> R<()> {
        let n = name_obj(it, name)?;
        let cls = it.type_of(&slf);
        it.generic_setattr(&slf, &cls, &n, value.clone())
    }

    #[proto(delattr)]
    fn delattr(slf: This<&Value>, it: &mut Interp, name: &Value) -> R<()> {
        let n = name_obj(it, name)?;
        let cls = it.type_of(&slf);
        it.generic_delattr(&slf, &cls, &n)
    }

    #[proto(getattribute)]
    fn getattribute(slf: This<&Value>, it: &mut Interp, name: &Value) -> R<Value> {
        let n = name_obj(it, name)?;
        let cls = it.type_of(&slf);
        it.generic_getattr(&slf, &cls, &n)
    }

    #[proto(eq)]
    fn eq(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        if slf.is(value) {
            return Ok(Value::Bool(true));
        }
        compare(it, CmpOp::Eq, &slf, value)
    }

    #[proto(ne)]
    fn ne(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        let r = if it.user_special(&slf, "__eq__").is_some() {
            let m = it.get_attr_str(&slf, "__eq__")?;
            it.call(&m, vec![value.clone()], Vec::new())?
        } else if slf.is(value) {
            Value::Bool(true)
        } else {
            compare(it, CmpOp::Eq, &slf, value)?
        };
        if matches!(r, Value::NotImplemented) {
            return Ok(r);
        }
        Ok(Value::Bool(!it.truthy(&r)?))
    }

    #[proto(lt)]
    fn lt(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        compare(it, CmpOp::Lt, &slf, value)
    }

    #[proto(le)]
    fn le(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        compare(it, CmpOp::LtE, &slf, value)
    }

    #[proto(gt)]
    fn gt(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        compare(it, CmpOp::Gt, &slf, value)
    }

    #[proto(ge)]
    fn ge(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        compare(it, CmpOp::GtE, &slf, value)
    }

    #[proto(hash)]
    fn hash(slf: This<&Value>, it: &mut Interp) -> R<i64> {
        it.native_hash(&slf)
    }

    #[proto(repr)]
    fn repr(slf: This<&Value>, it: &mut Interp) -> R<String> {
        it.native_repr(&slf)
    }

    #[proto(str)]
    fn str(slf: This<&Value>, it: &mut Interp) -> R<String> {
        it.native_str(&slf)
    }

    /// Default object formatter.
    ///
    /// Return str(self) if format_spec is empty. Raise TypeError otherwise.
    #[method(name = "__format__")]
    fn format(slf: This<&Value>, it: &mut Interp, format_spec: &str) -> R<String> {
        if !format_spec.is_empty() {
            let t = it.type_name_of(&slf);
            return Err(it.type_error(&format!("unsupported format string passed to {}.__format__", t)));
        }
        it.str_of(&slf)
    }

    /// Default dir() implementation.
    #[method(name = "__dir__")]
    fn dir(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        let names = it.dir_names(&slf)?;
        Ok(Value::list(names.into_iter().map(Value::string).collect()))
    }

    /// Size of object in memory, in bytes.
    #[method(name = "__sizeof__")]
    fn sizeof(slf: This<&Value>) -> i64 {
        let _ = slf;
        64
    }

    /// Helper for pickle.
    #[method(name = "__reduce_ex__")]
    fn reduce_ex(slf: This<&Value>, it: &mut Interp, protocol: &Value) -> R<Value> {
        let obj: &Value = &slf;
        let cls = it.type_of(obj);
        if let Some((owner, m)) = it.lookup_mro_with_owner(&cls, "__reduce__") {
            if !Rc::ptr_eq(&owner, &it.types.object) {
                let b = it.bind_descr(&m, obj, &cls)?;
                return it.call(&b, Vec::new(), Vec::new());
            }
        }
        let proto = it.index_of(protocol)?;
        common_reduce(it, obj, &cls, proto)
    }

    /// Helper for pickle.
    #[method(name = "__reduce__")]
    fn reduce(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        let cls = it.type_of(&slf);
        common_reduce(it, &slf, &cls, 0)
    }

    /// Helper for pickle.
    #[method(name = "__getstate__")]
    fn getstate(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        default_getstate(it, &slf, false)
    }

    /// This method is called when a class is subclassed.
    ///
    /// The default implementation does nothing. It may be
    /// overridden to extend subclasses.
    ///
    #[classmethod(name = "__init_subclass__", hint(py(text_signature = "")))]
    fn init_subclass(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<()> {
        if args.is_empty() && kwargs.is_empty() {
            return Ok(());
        }
        let n = match &*cls {
            Value::Obj(c) => it.type_display(c),
            _ => "type".into(),
        };
        if !kwargs.is_empty() {
            return Err(it.type_error(&format!("{}.__init_subclass__() takes no keyword arguments", n)));
        }
        Err(it.type_error(&format!("{}.__init_subclass__() takes no arguments ({} given)", n, args.len())))
    }

    /// Abstract classes can override this to customize issubclass().
    ///
    /// This is invoked early on by abc.ABCMeta.__subclasscheck__().
    /// It should return True, False or NotImplemented.  If it returns
    /// NotImplemented, the normal algorithm is used.  Otherwise, it
    /// overrides the normal algorithm (and the outcome is cached).
    ///
    #[classmethod(name = "__subclasshook__", hint(py(text_signature = "")))]
    fn subclasshook(cls: This<Value>, #[varargs] args: &[Value]) -> Value {
        let _ = (cls, args);
        Value::NotImplemented
    }
}

/// `object.__getstate__`: the instance dict, plus a dict of the slot values when the class has slots.
fn default_getstate(it: &mut Interp, obj: &Value, required: bool) -> R<Value> {
    let cls = it.type_of(obj);
    if required && matches!(it.type_layout(&cls), Layout::Other) {
        let t = it.tp_name(&cls);
        return Err(it.type_error(&format!("cannot pickle '{t}' object")));
    }
    let mut state = match obj {
        Value::Obj(o) => match o.dict.borrow().as_ref() {
            Some(d) if matches!(&d.kind, Kind::Dict(p) if !p.borrow().is_empty()) => Value::Obj(d.clone()),
            _ => Value::None,
        },
        _ => Value::None,
    };
    let copyreg = it.import_module("copyreg")?;
    let cached = cls.dict.borrow().as_ref().and_then(|d| dict_get_str(d, "__slotnames__"));
    let slotnames = match cached {
        Some(v) => v,
        None => {
            let f = it.get_attr_str(&Value::Obj(copyreg), "_slotnames")?;
            it.call(&f, vec![Value::Obj(cls.clone())], Vec::new())?
        }
    };
    let names = match &slotnames {
        Value::None => Vec::new(),
        Value::Obj(o) if matches!(o.kind, Kind::List(_)) => it.iterate_to_vec(&slotnames)?,
        _ => {
            let t = it.tp_name_of(&slotnames);
            return Err(it.type_error(&format!("copyreg._slotnames didn't return a list or None but {t}")));
        }
    };
    if !names.is_empty() {
        // Slot values live in the instance dict here; the state's dict part holds only the rest.
        if let Value::Obj(d) = &state {
            let slot_names: Vec<String> = names.iter().filter_map(|n| n.as_str().map(str::to_string)).collect();
            let rest: Vec<(Value, Value)> = match &d.kind {
                Kind::Dict(p) => p.borrow().iter().filter(|e| !e.key.as_str().is_some_and(|k| slot_names.iter().any(|s| s == k))).map(|e| (e.key.clone(), e.val.clone())).collect(),
                _ => Vec::new(),
            };
            state = if rest.is_empty() {
                Value::None
            } else {
                let kept = Object::new(Kind::Dict(std::cell::RefCell::new(crate::dict::PyDict::new())));
                for (k, v) in rest {
                    it.dict_set(&kept, k, v)?;
                }
                Value::Obj(kept)
            };
        }
        let slots = Object::new(Kind::Dict(std::cell::RefCell::new(crate::dict::PyDict::new())));
        let mut any = false;
        for n in names {
            let Some(name) = n.as_str().map(str::to_string) else { continue };
            match it.get_attr_str(obj, &name) {
                Ok(v) => {
                    it.dict_set(&slots, n, v)?;
                    any = true;
                }
                Err(e) if it.exc_is(&e, "AttributeError") => {}
                Err(e) => return Err(e),
            }
        }
        if any {
            state = Value::tuple(vec![state, Value::Obj(slots)]);
        }
    }
    Ok(state)
}

/// `object_getstate`: a user `__getstate__` is called, `object.__getstate__` runs the default.
fn object_getstate(it: &mut Interp, obj: &Value, cls: &Obj, required: bool) -> R<Value> {
    match it.lookup_mro_with_owner(cls, "__getstate__") {
        Some((owner, _)) if Rc::ptr_eq(&owner, &it.types.object) => default_getstate(it, obj, required),
        _ => it.call_method(obj, "__getstate__", Vec::new()),
    }
}

/// `_PyObject_GetNewArguments`: `(args, kwargs)` from `__getnewargs_ex__`, else `__getnewargs__`.
fn new_arguments(it: &mut Interp, obj: &Value, cls: &Obj) -> R<(Option<Vec<Value>>, Option<Value>)> {
    if let Some(m) = it.lookup_mro(cls, "__getnewargs_ex__") {
        let b = it.bind_descr(&m, obj, cls)?;
        let r = it.call(&b, Vec::new(), Vec::new())?;
        let Some(items) = r.tuple_items() else {
            let t = it.tp_name_of(&r);
            return Err(it.type_error(&format!("__getnewargs_ex__ should return a tuple, not '{t}'")));
        };
        if items.len() != 2 {
            let n = items.len();
            return Err(it.type_error(&format!("__getnewargs_ex__ should return a tuple of length 2, not {n}")));
        }
        let (args, kwargs) = (items[0].clone(), items[1].clone());
        let Some(a) = args.tuple_items() else {
            let t = it.tp_name_of(&args);
            return Err(it.type_error(&format!("first item of the tuple returned by __getnewargs_ex__ must be a tuple, not '{t}'")));
        };
        if !matches!(&kwargs, Value::Obj(o) if matches!(o.kind, Kind::Dict(_))) {
            let t = it.tp_name_of(&kwargs);
            return Err(it.type_error(&format!("second item of the tuple returned by __getnewargs_ex__ must be a dict, not '{t}'")));
        }
        return Ok((Some(a.to_vec()), Some(kwargs)));
    }
    if let Some(m) = it.lookup_mro(cls, "__getnewargs__") {
        let b = it.bind_descr(&m, obj, cls)?;
        let r = it.call(&b, Vec::new(), Vec::new())?;
        let Some(items) = r.tuple_items() else {
            let t = it.tp_name_of(&r);
            return Err(it.type_error(&format!("__getnewargs__ should return a tuple, not '{t}'")));
        };
        return Ok((Some(items.to_vec()), None));
    }
    Ok((None, None))
}

/// `_common_reduce`: `copyreg._reduce_ex` below protocol 2, `reduce_newobj` from it on.
fn common_reduce(it: &mut Interp, obj: &Value, cls: &Obj, proto: i64) -> R<Value> {
    if matches!(obj, Value::Obj(o) if matches!(o.kind, Kind::Type(_) | Kind::Function(_) | Kind::Native(_) | Kind::Method(..) | Kind::Module)) || !matches!(obj, Value::Obj(_)) {
        let t = it.tp_name(cls);
        return Err(it.type_error(&format!("cannot pickle '{t}' object")));
    }
    if proto < 2 {
        let copyreg = it.import_module("copyreg")?;
        let f = it.get_attr_str(&Value::Obj(copyreg), "_reduce_ex")?;
        return it.call(&f, vec![obj.clone(), Value::Int(proto)], Vec::new());
    }
    let cls = cls.clone();
    let (args, kwargs) = new_arguments(it, obj, &cls)?;
    let mut hasargs = args.is_some();
    let mut args = args.unwrap_or_default();
    let layout = it.type_layout(&cls);
    if matches!(layout, Layout::Set | Layout::FrozenSet) {
        let state = object_getstate(it, obj, &cls, false)?;
        let items = it.iterate_to_vec(obj)?;
        return Ok(Value::tuple(vec![Value::Obj(cls), Value::tuple(vec![Value::list(items)]), state]));
    }
    let base = match layout {
        Layout::Tuple => Some(it.types.tuple.clone()),
        Layout::Str => Some(it.types.str_.clone()),
        Layout::Int => Some(it.types.int.clone()),
        Layout::Float => Some(it.types.float.clone()),
        Layout::Bytes => Some(it.types.bytes.clone()),
        _ => None,
    };
    if let (Some(base), false) = (base, hasargs) {
        args.push(it.call(&Value::Obj(base), vec![obj.clone()], Vec::new())?);
        hasargs = true;
    }
    let copyreg = Value::Obj(it.import_module("copyreg")?);
    let kwargs = kwargs.filter(|k| matches!(k, Value::Obj(o) if matches!(&o.kind, Kind::Dict(p) if !p.borrow().is_empty())));
    let (newobj, newargs) = match kwargs {
        None => {
            let mut v = vec![Value::Obj(cls.clone())];
            v.extend(args);
            (it.get_attr_str(&copyreg, "__newobj__")?, v)
        }
        Some(k) => (
            it.get_attr_str(&copyreg, "__newobj_ex__")?,
            vec![Value::Obj(cls.clone()), Value::tuple(args), k],
        ),
    };
    let is_list = it.isinstance_value(obj, &Value::Obj(it.types.list.clone()))?;
    let is_dict = it.isinstance_value(obj, &Value::Obj(it.types.dict.clone()))?;
    let state = object_getstate(it, obj, &cls, !(hasargs || is_list || is_dict))?;
    let listitems = if is_list { it.get_iter(obj)? } else { Value::None };
    let dictitems = if is_dict {
        let items = it.call_method(obj, "items", Vec::new())?;
        it.get_iter(&items)?
    } else {
        Value::None
    };
    Ok(Value::tuple(vec![newobj, Value::tuple(newargs), state, listitems, dictitems]))
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

/// A getter of `type`'s own getset descriptors: the class's slot, else its own dict entry.
fn type_getset(it: &mut Interp, cls: &Obj, name: &str) -> R<Value> {
    let is_type = Rc::ptr_eq(cls, &it.types.type_);
    let own = match is_type {
        true => None,
        false => cls.dict.borrow().as_ref().and_then(|d| dict_get_str(d, name)),
    };
    match (name, own) {
        ("__module__", Some(v)) => Ok(v),
        ("__doc__", Some(v)) => it.bind_descr_cls(&v, cls),
        ("__module__", None) if it.is_heap(cls) => Err(it.new_exc_str("AttributeError", "__module__")),
        ("__module__", None) => Ok(Value::str("builtins")),
        ("__doc__", None) if is_type => Ok(<Type as lumen_bind::Class>::DESC.doc.map_or(Value::None, Value::str)),
        ("__doc__", None) => Ok(Value::None),
        ("__abstractmethods__", Some(v)) => Ok(v),
        ("__abstractmethods__", None) => Err(it.new_exc_str("AttributeError", "__abstractmethods__")),
        ("__type_params__", Some(v)) => Ok(v),
        ("__type_params__", None) => Ok(Value::tuple(Vec::new())),
        ("__annotations__", _) if !it.is_heap(cls) => {
            let msg = format!("type object '{}' has no attribute '__annotations__'", it.tp_name(cls));
            Err(it.new_exc_str("AttributeError", &msg))
        }
        ("__annotations__", Some(v)) => it.bind_descr_cls(&v, cls),
        ("__annotations__", None) => {
            let v = Value::Obj(it.new_dict());
            type_getset_store(it, cls, "__annotations__", &v)?;
            Ok(v)
        }
        _ => match it.type_special_attr(cls, name) {
            Some(v) => Ok(v),
            None => Err(it.new_exc_str("AttributeError", &format!("type object '{}' has no attribute '{name}'", it.type_name(cls)))),
        },
    }
}

fn type_getset_store(it: &mut Interp, cls: &Obj, name: &str, v: &Value) -> R<()> {
    let Value::Obj(n) = Value::str(name) else { unreachable!("a str") };
    it.type_store_attr(cls, &n, v.clone())
}

/// `check_set_special_type_attr`: the slots of `type` cannot be written on an immutable type.
fn check_special_set(it: &mut Interp, cls: &Obj, name: &str) -> R<()> {
    let Kind::Type(td) = &cls.kind else { return Ok(()) };
    if td.flags.get() & TF_IMMUTABLE != 0 {
        let msg = format!("cannot set '{}' attribute of immutable type '{}'", name, it.tp_name(cls));
        return Err(it.type_error(&msg));
    }
    Ok(())
}

/// The deleter of the `type` slots that cannot be deleted: always a `TypeError` (CPython's
/// setters receive a NULL value).
fn del_special(it: &mut Interp, a: &[Value], name: &str) -> R<Value> {
    let tp = match a.first() {
        Some(Value::Obj(c)) if matches!(c.kind, Kind::Type(_)) => it.tp_name(c),
        _ => String::new(),
    };
    Err(it.type_error(&format!("cannot delete '{}' attribute of immutable type '{}'", name, tp)))
}

macro_rules! type_deleter {
    ($($f:ident => $name:literal),* $(,)?) => {
        $(fn $f(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
            del_special(it, a, $name)
        })*
    };
}

type_deleter! {
    del_name => "__name__",
    del_qualname => "__qualname__",
    del_bases => "__bases__",
    del_module => "__module__",
    del_doc => "__doc__",
    del_type_params => "__type_params__",
}

/// Deleting a dict-backed slot of `type` (`__annotations__`, `__abstractmethods__`): the entry
/// goes, an absent one is an `AttributeError`.
fn del_dict_slot(it: &mut Interp, a: &[Value], name: &'static str) -> R<Value> {
    let Some(Value::Obj(c)) = a.first() else { return Err(it.type_error("descriptor requires a 'type' object")) };
    let Kind::Type(td) = &c.kind else { return Err(it.type_error("descriptor requires a 'type' object")) };
    if name == "__annotations__" {
        check_special_set(it, c, name)?;
    }
    let removed = match c.dict.borrow().as_ref() {
        Some(d) => dict_del_str(d, name),
        None => None,
    };
    if removed.is_none() {
        return Err(it.new_exc_str("AttributeError", name));
    }
    if name == "__abstractmethods__" {
        td.flags.set(td.flags.get() & !TF_ABSTRACT);
    }
    it.type_epoch += 1;
    Ok(Value::None)
}

fn del_annotations(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    del_dict_slot(it, a, "__annotations__")
}

fn del_abstractmethods(it: &mut Interp, a: &[Value], _kw: &[(Obj, Value)]) -> R<Value> {
    del_dict_slot(it, a, "__abstractmethods__")
}

/// CPython's `tp_flags` bits that Python code tests (`copyreg`, `inspect`, `abc`).
fn type_flags(it: &mut Interp, cls: &Obj) -> i64 {
    const HEAPTYPE: i64 = 1 << 9;
    const BASETYPE: i64 = 1 << 10;
    const READY: i64 = 1 << 12;
    const IS_ABSTRACT: i64 = 1 << 20;
    const METHOD_DESCRIPTOR: i64 = 1 << 17;
    const HAVE_VECTORCALL: i64 = 1 << 11;
    let Kind::Type(td) = &cls.kind else { return 0 };
    let mut f = BASETYPE | READY;
    let own = td.flags.get();
    if own & TF_METHOD_DESCRIPTOR != 0 {
        f |= METHOD_DESCRIPTOR;
    }
    if own & TF_VECTORCALL != 0 {
        f |= HAVE_VECTORCALL;
    }
    if !it.is_heap(cls) {
        match &**td.name.borrow() {
            "function" | "method_descriptor" | "wrapper_descriptor" | "_lru_cache_wrapper" => f |= METHOD_DESCRIPTOR | HAVE_VECTORCALL,
            "builtin_function_or_method" | "method" | "classmethod_descriptor" | "partial" => f |= HAVE_VECTORCALL,
            _ => {}
        }
    }
    if it.is_heap(cls) {
        f |= HEAPTYPE;
    }
    if td.flags.get() & TF_ABSTRACT != 0 {
        f |= IS_ABSTRACT;
    }
    let t = &it.types;
    let subclass_bits = [
        (t.int.clone(), 24),
        (t.list.clone(), 25),
        (t.tuple.clone(), 26),
        (t.bytes.clone(), 27),
        (t.str_.clone(), 28),
        (t.dict.clone(), 29),
        (it.exc_type("BaseException"), 30),
        (t.type_.clone(), 31),
    ];
    for (base, bit) in subclass_bits {
        if it.is_subtype(cls, &base) {
            f |= 1 << bit;
        }
    }
    f
}

#[lumen_bind::class(name = "type")]
/// type(object) -> the object's type
/// type(name, bases, dict, **kwds) -> a new type
pub struct Type;

#[lumen_bind::methods]
impl Type {
    #[constructor(hint(py(text_signature = "")))]
    fn new(meta: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
        let Value::Obj(meta) = &*meta else { unreachable!("checked by the entry") };
        if args.len() == 1 && kwargs.is_empty() && Rc::ptr_eq(meta, &it.types.type_) {
            return Ok(Value::Obj(it.type_of(&args[0])));
        }
        if args.len() != 3 {
            return Err(it.type_error("type.__new__() takes exactly 3 arguments"));
        }
        it.type_new_from_args(meta.clone(), args, kwargs.to_vec())
    }

    #[proto(init)]
    fn init(slf: This<TypeRef<'_>>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<()> {
        let _ = slf;
        if args.len() == 1 && !kwargs.is_empty() {
            return Err(it.type_error("type.__init__() takes no keyword arguments"));
        }
        if args.len() != 1 && args.len() != 3 {
            return Err(it.type_error("type.__init__() takes 1 or 3 arguments"));
        }
        Ok(())
    }

    #[proto(call)]
    fn call(slf: This<TypeRef<'_>>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
        it.type_call_default(slf.0 .0, args.to_vec(), kwargs.to_vec())
    }

    /// __prepare__() -> dict
    /// used to create the namespace for the class statement
    #[classmethod(name = "__prepare__", hint(py(text_signature = "")))]
    fn prepare(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> Value {
        let _ = (cls, args, kwargs);
        Value::Obj(it.new_dict())
    }

    /// Check if an object is an instance.
    #[method(name = "__instancecheck__")]
    fn instancecheck(slf: This<TypeRef<'_>>, it: &mut Interp, instance: &Value) -> bool {
        let t = it.type_of(instance);
        it.is_subtype(&t, slf.0 .0)
    }

    /// Check if a class is a subclass.
    #[method(name = "__subclasscheck__")]
    fn subclasscheck(slf: This<TypeRef<'_>>, it: &mut Interp, subclass: &Value) -> R<bool> {
        match subclass {
            Value::Obj(s) if matches!(s.kind, Kind::Type(_)) => Ok(it.is_subtype(s, slf.0 .0)),
            _ => Err(it.type_error("issubclass() arg 1 must be a class")),
        }
    }

    /// Return a type's method resolution order.
    #[method]
    fn mro(slf: This<TypeRef<'_>>) -> Value {
        let Kind::Type(td) = &slf.0 .0.kind else { unreachable!("a type") };
        Value::list(td.mro.borrow().iter().map(|m| Value::Obj(m.clone())).collect())
    }

    /// Return a list of immediate subclasses.
    #[method(name = "__subclasses__")]
    fn subclasses(slf: This<TypeRef<'_>>, it: &mut Interp) -> Value {
        let c = slf.0 .0;
        let mut out = Vec::new();
        let reg = it.subclass_registry.clone();
        for w in reg.iter().filter_map(|w| w.upgrade()) {
            if let Kind::Type(td) = &w.kind {
                if td.bases.borrow().iter().any(|b| Rc::ptr_eq(b, c)) {
                    out.push(Value::Obj(w.clone()));
                }
            }
        }
        Value::list(out)
    }

    #[proto(or)]
    fn or(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        Ok(it.union_binop(&slf, value)?.unwrap_or(Value::NotImplemented))
    }

    #[proto(ror)]
    fn ror(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        Ok(it.union_binop(value, &slf)?.unwrap_or(Value::NotImplemented))
    }

    #[getter(name = "__name__")]
    fn get_name(slf: This<TypeRef<'_>>, it: &mut Interp) -> R<Value> {
        type_getset(it, slf.0 .0, "__name__")
    }

    #[setter(name = "__name__")]
    fn set_name(slf: This<TypeRef<'_>>, it: &mut Interp, value: &Value) -> R<()> {
        let cls = slf.0 .0;
        check_special_set(it, cls, "__name__")?;
        let Some(s) = value.as_str() else {
            let msg = format!("can only assign string to {}.__name__, not '{}'", it.tp_name(cls), it.tp_name_of(value));
            return Err(it.type_error(&msg));
        };
        if s.contains('\0') {
            return Err(it.value_error("type name must not contain null characters"));
        }
        type_getset_store(it, cls, "__name__", value)
    }

    #[getter(name = "__qualname__")]
    fn get_qualname(slf: This<TypeRef<'_>>, it: &mut Interp) -> R<Value> {
        type_getset(it, slf.0 .0, "__qualname__")
    }

    #[setter(name = "__qualname__")]
    fn set_qualname(slf: This<TypeRef<'_>>, it: &mut Interp, value: &Value) -> R<()> {
        let cls = slf.0 .0;
        check_special_set(it, cls, "__qualname__")?;
        type_getset_store(it, cls, "__qualname__", value)
    }

    #[getter(name = "__bases__")]
    fn get_bases(slf: This<TypeRef<'_>>, it: &mut Interp) -> R<Value> {
        type_getset(it, slf.0 .0, "__bases__")
    }

    #[setter(name = "__bases__")]
    fn set_bases(slf: This<TypeRef<'_>>, it: &mut Interp, value: &Value) -> R<()> {
        let cls = slf.0 .0;
        check_special_set(it, cls, "__bases__")?;
        let name = it.tp_name(cls);
        let Some(items) = value.tuple_items() else {
            let msg = format!("can only assign tuple to {}.__bases__, not {}", name, it.tp_name_of(value));
            return Err(it.type_error(&msg));
        };
        if items.is_empty() {
            let msg = format!("can only assign non-empty tuple to {}.__bases__, not ()", name);
            return Err(it.type_error(&msg));
        }
        let mut bases = Vec::with_capacity(items.len());
        for b in items {
            match b {
                Value::Obj(o) if matches!(o.kind, Kind::Type(_)) => {
                    if it.is_subtype(o, cls) {
                        return Err(it.type_error("a __bases__ item causes an inheritance cycle"));
                    }
                    bases.push(o.clone());
                }
                _ => {
                    let msg = format!("{}.__bases__ must be tuple of classes, not '{}'", name, it.tp_name_of(b));
                    return Err(it.type_error(&msg));
                }
            }
        }
        if it.compute_mro(cls, &bases).is_none() {
            return Err(it.type_error("Cannot create a consistent method resolution order (MRO) for bases"));
        }
        it.reassign_bases(cls, bases);
        Ok(())
    }

    #[getter(name = "__base__")]
    fn get_base(slf: This<TypeRef<'_>>, it: &mut Interp) -> R<Value> {
        type_getset(it, slf.0 .0, "__base__")
    }

    #[getter(name = "__mro__")]
    fn get_mro(slf: This<TypeRef<'_>>, it: &mut Interp) -> R<Value> {
        type_getset(it, slf.0 .0, "__mro__")
    }

    #[getter(name = "__module__")]
    fn get_module(slf: This<TypeRef<'_>>, it: &mut Interp) -> R<Value> {
        type_getset(it, slf.0 .0, "__module__")
    }

    #[setter(name = "__module__")]
    fn set_module(slf: This<TypeRef<'_>>, it: &mut Interp, value: &Value) -> R<()> {
        let cls = slf.0 .0;
        check_special_set(it, cls, "__module__")?;
        type_getset_store(it, cls, "__module__", value)
    }

    #[getter(name = "__doc__")]
    fn get_doc(slf: This<TypeRef<'_>>, it: &mut Interp) -> R<Value> {
        type_getset(it, slf.0 .0, "__doc__")
    }

    #[setter(name = "__doc__")]
    fn set_doc(slf: This<TypeRef<'_>>, it: &mut Interp, value: &Value) -> R<()> {
        let cls = slf.0 .0;
        check_special_set(it, cls, "__doc__")?;
        type_getset_store(it, cls, "__doc__", value)
    }

    #[getter(name = "__abstractmethods__")]
    fn get_abstractmethods(slf: This<TypeRef<'_>>, it: &mut Interp) -> R<Value> {
        type_getset(it, slf.0 .0, "__abstractmethods__")
    }

    #[setter(name = "__abstractmethods__")]
    fn set_abstractmethods(slf: This<TypeRef<'_>>, it: &mut Interp, value: &Value) -> R<()> {
        type_getset_store(it, slf.0 .0, "__abstractmethods__", value)
    }

    #[getter(name = "__annotations__")]
    fn get_annotations(slf: This<TypeRef<'_>>, it: &mut Interp) -> R<Value> {
        type_getset(it, slf.0 .0, "__annotations__")
    }

    #[setter(name = "__annotations__")]
    fn set_annotations(slf: This<TypeRef<'_>>, it: &mut Interp, value: &Value) -> R<()> {
        let cls = slf.0 .0;
        check_special_set(it, cls, "__annotations__")?;
        type_getset_store(it, cls, "__annotations__", value)
    }

    #[getter(name = "__type_params__")]
    fn get_type_params(slf: This<TypeRef<'_>>, it: &mut Interp) -> R<Value> {
        type_getset(it, slf.0 .0, "__type_params__")
    }

    #[setter(name = "__type_params__")]
    fn set_type_params(slf: This<TypeRef<'_>>, it: &mut Interp, value: &Value) -> R<()> {
        let cls = slf.0 .0;
        check_special_set(it, cls, "__type_params__")?;
        if value.tuple_items().is_none() {
            return Err(it.type_error("__type_params__ must be set to a tuple"));
        }
        type_getset_store(it, cls, "__type_params__", value)
    }

    #[getter(name = "__dict__")]
    fn get_dict(slf: This<TypeRef<'_>>, it: &mut Interp) -> R<Value> {
        type_getset(it, slf.0 .0, "__dict__")
    }

    #[getter(name = "__text_signature__")]
    fn get_text_signature(slf: This<TypeRef<'_>>, it: &mut Interp) -> Value {
        crate::bind::type_text_signature(it, slf.0 .0)
    }

    #[getter(name = "__flags__")]
    fn get_flags(slf: This<TypeRef<'_>>, it: &mut Interp) -> i64 {
        type_flags(it, slf.0 .0)
    }
}

// ---- descriptors -------------------------------------------------------------------------------

// `__get__` / `__set__` / `__delete__` of `property`, `function` and the getset and member
// descriptors.
#[lumen_bind::class(name = "descriptor", hint(py(shared)))]
pub struct DescrMethods;

#[lumen_bind::methods]
impl DescrMethods {
    /// Return an attribute of instance, which is of type owner.
    #[method(name = "__get__", hint(py(text_signature = "($self, instance, owner=None, /)")))]
    fn get(slf: This<&Value>, it: &mut Interp, instance: &Value, owner: Option<&Value>) -> R<Value> {
        if instance.is_none() {
            if let (Value::Obj(o), Some(Value::Obj(owner))) = (&*slf, owner) {
                if matches!(o.kind, Kind::ClassMethod(_)) && matches!(owner.kind, Kind::Type(_)) {
                    return it.bind_descr_cls(&slf, owner);
                }
            }
            return Ok(slf.clone());
        }
        if let Value::Obj(o) = &*slf {
            if matches!(&o.kind, Kind::Property(p) if p.fget.is_none()) {
                let t = it.type_name_of(instance);
                return Err(it.new_exc_str("AttributeError", &format!("property of '{t}' object has no getter")));
            }
        }
        let cls = it.type_of(instance);
        it.bind_descr(&slf, instance, &cls)
    }

    /// Set an attribute of instance to value.
    #[method(name = "__set__", hint(py(text_signature = "($self, instance, value, /)")))]
    fn set(slf: This<&Value>, it: &mut Interp, instance: &Value, value: &Value) -> R<()> {
        let f = accessor(it, &slf, instance, true)?;
        it.call(&f, vec![instance.clone(), value.clone()], Vec::new())?;
        Ok(())
    }

    /// Delete an attribute of instance.
    #[method(name = "__delete__", hint(py(text_signature = "($self, instance, /)")))]
    fn delete(slf: This<&Value>, it: &mut Interp, instance: &Value) -> R<()> {
        let f = accessor(it, &slf, instance, false)?;
        it.call(&f, vec![instance.clone()], Vec::new())?;
        Ok(())
    }
}

/// The setter (or deleter) of the property-like `descr`, or the error for its absence.
fn accessor(it: &mut Interp, descr: &Value, instance: &Value, set: bool) -> R<Value> {
    let Value::Obj(o) = descr else { return Err(it.type_error("not a property")) };
    let Kind::Property(p) = &o.kind else { return Err(it.type_error("not a property")) };
    let f = if set { &p.fset } else { &p.fdel };
    if !f.is_none() {
        return Ok(f.clone());
    }
    let t = it.type_name_of(instance);
    if o.cls.is_some() {
        let name = o.dict.borrow().as_ref().and_then(|d| dict_get_str(d, "__name__")).and_then(|n| n.as_str().map(str::to_string)).unwrap_or_default();
        return Err(it.new_exc_str("AttributeError", &format!("attribute '{name}' of '{t}' objects is not writable")));
    }
    let what = if set { "setter" } else { "deleter" };
    Err(it.new_exc_str("AttributeError", &format!("property of '{t}' object has no {what}")))
}

/// A `property` (or subclass instance).
pub struct PropRef<'a>(&'a Obj, &'a PropData);

impl<'a> FromArg<'a, PyHost> for PropRef<'a> {
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        if let Value::Obj(o) = v {
            if let Kind::Property(p) = &o.kind {
                return Ok(PropRef(o, p));
            }
        }
        Err(cx.arg_error(at, "property", v))
    }
}

impl PropRef<'_> {
    /// A copy of the property with accessor `slot` (0 get, 1 set, 2 delete) replaced by `f`.
    fn with(&self, slot: usize, f: &Value) -> Value {
        let p = self.1;
        let mut np = PropData { fget: p.fget.clone(), fset: p.fset.clone(), fdel: p.fdel.clone(), doc: p.doc.clone() };
        match slot {
            0 => np.fget = f.clone(),
            1 => np.fset = f.clone(),
            _ => np.fdel = f.clone(),
        }
        let kind = Kind::Property(np);
        Value::Obj(match &self.0.cls {
            Some(c) => Object::with_cls(c.clone(), kind),
            None => Object::new(kind),
        })
    }
}

#[lumen_bind::class(name = "property")]
/// Property attribute.
///
///   fget
///     function to be used for getting an attribute value
///   fset
///     function to be used for setting an attribute value
///   fdel
///     function to be used for del'ing an attribute
///   doc
///     docstring
///
/// Typical use is to define a managed attribute x:
///
/// class C(object):
///     def getx(self): return self._x
///     def setx(self, value): self._x = value
///     def delx(self): del self._x
///     x = property(getx, setx, delx, "I'm the 'x' property.")
///
/// Decorators make defining new properties or modifying existing ones easy:
///
/// class C(object):
///     @property
///     def x(self):
///         "I am the 'x' property."
///         return self._x
///     @x.setter
///     def x(self, value):
///         self._x = value
///     @x.deleter
///     def x(self):
///         del self._x
pub struct Property;

#[lumen_bind::methods]
impl Property {
    #[constructor(hint(py(text_signature = "(fget=None, fset=None, fdel=None, doc=None)")))]
    fn new(cls: This<Value>, it: &mut Interp, #[kw] fget: Option<&Value>, #[kw] fset: Option<&Value>, #[kw] fdel: Option<&Value>, #[kw] doc: Option<&Value>) -> Value {
        let Value::Obj(cls) = &*cls else { unreachable!("checked by the entry") };
        let get = |v: Option<&Value>| v.cloned().unwrap_or(Value::None);
        let mut doc = get(doc);
        if doc.is_none() {
            if let Some(Value::Obj(f)) = fget {
                if let Kind::Function(func) = &f.kind {
                    doc = func.code.borrow().doc.clone().unwrap_or(Value::None);
                }
            }
        }
        let kind = Kind::Property(PropData { fget: get(fget), fset: get(fset), fdel: get(fdel), doc });
        Value::Obj(if Rc::ptr_eq(cls, &it.types.property) { Object::new(kind) } else { Object::with_cls(cls.clone(), kind) })
    }

    #[proto(init)]
    fn init(slf: This<&Value>, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) {
        let _ = (slf, args, kwargs);
    }

    /// Descriptor to obtain a copy of the property with a different getter.
    #[method(hint(py(text_signature = "")))]
    fn getter(slf: This<PropRef<'_>>, func: &Value) -> Value {
        slf.0.with(0, func)
    }

    /// Descriptor to obtain a copy of the property with a different setter.
    #[method(hint(py(text_signature = "")))]
    fn setter(slf: This<PropRef<'_>>, func: &Value) -> Value {
        slf.0.with(1, func)
    }

    /// Descriptor to obtain a copy of the property with a different deleter.
    #[method(hint(py(text_signature = "")))]
    fn deleter(slf: This<PropRef<'_>>, func: &Value) -> Value {
        slf.0.with(2, func)
    }

    /// Method to set name of a property.
    #[method(name = "__set_name__", hint(py(text_signature = "")))]
    fn set_name(slf: This<PropRef<'_>>, owner: &Value, name: &Value) {
        let _ = (slf, owner, name);
    }
}

// ---- staticmethod / classmethod ------------------------------------------------------------------

fn wrap_callable(cls: &Value, base: &Obj, kind: Kind) -> Value {
    match cls {
        Value::Obj(c) if !Rc::ptr_eq(c, base) => Value::Obj(Object::with_cls(c.clone(), kind)),
        _ => Value::Obj(Object::new(kind)),
    }
}

fn wrapper_get(it: &mut Interp, slf: &Value, instance: &Value, owner: Option<&Value>) -> R<Value> {
    let cls = match owner {
        Some(Value::Obj(c)) if matches!(c.kind, Kind::Type(_)) => c.clone(),
        _ => it.type_of(instance),
    };
    if instance.is_none() {
        return it.bind_descr_cls(slf, &cls);
    }
    it.bind_descr(slf, instance, &cls)
}

#[lumen_bind::class(name = "staticmethod")]
/// staticmethod(function) -> method
///
/// Convert a function to be a static method.
///
/// A static method does not receive an implicit first argument.
/// To declare a static method, use this idiom:
///
///      class C:
///          @staticmethod
///          def f(arg1, arg2, argN):
///              ...
///
/// It can be called either on the class (e.g. C.f()) or on an instance
/// (e.g. C().f()). Both the class and the instance are ignored, and
/// neither is passed implicitly as the first argument to the method.
///
/// Static methods in Python are similar to those found in Java or C++.
/// For a more advanced concept, see the classmethod builtin.
pub struct StaticMethod;

#[lumen_bind::methods]
impl StaticMethod {
    #[constructor(hint(py(text_signature = "")))]
    fn new(cls: This<Value>, it: &mut Interp, function: &Value) -> Value {
        let base = it.types.staticmethod.clone();
        wrap_callable(&cls, &base, Kind::StaticMethod(function.clone()))
    }

    #[proto(init)]
    fn init(slf: This<&Value>, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) {
        let _ = (slf, args, kwargs);
    }

    /// Return an attribute of instance, which is of type owner.
    #[method(name = "__get__", hint(py(text_signature = "($self, instance, owner=None, /)")))]
    fn get(slf: This<&Value>, it: &mut Interp, instance: &Value, owner: Option<&Value>) -> R<Value> {
        wrapper_get(it, &slf, instance, owner)
    }

    #[proto(call)]
    fn call(slf: This<&Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
        let f = match &*slf {
            Value::Obj(o) => match &o.kind {
                Kind::StaticMethod(f) => f.clone(),
                _ => return Err(it.type_error("not a staticmethod")),
            },
            _ => return Err(it.type_error("not a staticmethod")),
        };
        it.call(&f, args.to_vec(), kwargs.to_vec())
    }
}

#[lumen_bind::class(name = "classmethod")]
/// classmethod(function) -> method
///
/// Convert a function to be a class method.
///
/// A class method receives the class as implicit first argument,
/// just like an instance method receives the instance.
/// To declare a class method, use this idiom:
///
///   class C:
///       @classmethod
///       def f(cls, arg1, arg2, argN):
///           ...
///
/// It can be called either on the class (e.g. C.f()) or on an instance
/// (e.g. C().f()).  The instance is ignored except for its class.
/// If a class method is called for a derived class, the derived class
/// object is passed as the implied first argument.
///
/// Class methods are different than C++ or Java static methods.
/// If you want those, see the staticmethod builtin.
pub struct ClassMethod;

#[lumen_bind::methods]
impl ClassMethod {
    #[constructor(hint(py(text_signature = "")))]
    fn new(cls: This<Value>, it: &mut Interp, function: &Value) -> Value {
        let base = it.types.classmethod.clone();
        wrap_callable(&cls, &base, Kind::ClassMethod(function.clone()))
    }

    #[proto(init)]
    fn init(slf: This<&Value>, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) {
        let _ = (slf, args, kwargs);
    }

    /// Return an attribute of instance, which is of type owner.
    #[method(name = "__get__", hint(py(text_signature = "($self, instance, owner=None, /)")))]
    fn get(slf: This<&Value>, it: &mut Interp, instance: &Value, owner: Option<&Value>) -> R<Value> {
        wrapper_get(it, &slf, instance, owner)
    }
}

// ---- super -------------------------------------------------------------------------------------

/// `super()` without arguments: `__class__` and the first argument of the calling frame.
fn implicit_super(it: &mut Interp) -> R<(Value, Value)> {
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
        match fr.locals.first().cloned().flatten() {
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
        (Some(c), Some(f)) => Ok((c, f)),
        (None, _) => Err(it.new_exc_str("RuntimeError", "super(): __class__ cell not found")),
        (_, None) => Err(it.new_exc_str("RuntimeError", "super(): no arguments")),
    }
}

fn make_super(it: &mut Interp, typ: Value, inst: Value) -> R<Value> {
    let typ_obj = match &typ {
        Value::Obj(t) if matches!(t.kind, Kind::Type(_)) => t.clone(),
        _ => {
            let t = it.type_name_of(&typ);
            return Err(it.type_error(&format!("super() argument 1 must be a type, not {t}")));
        }
    };
    let objtype = match &inst {
        Value::None => Value::None,
        Value::Obj(io) if matches!(io.kind, Kind::Type(_)) && it.is_subtype(io, &typ_obj) => inst.clone(),
        _ => {
            let t = it.type_of(&inst);
            if !it.is_subtype(&t, &typ_obj) {
                return Err(it.type_error("super(type, obj): obj must be an instance or subtype of type"));
            }
            Value::Obj(t)
        }
    };
    Ok(Value::Obj(Object::new(Kind::Super(typ, inst, objtype))))
}

fn super_parts(v: &Value) -> (Value, Value, Value) {
    match v {
        Value::Obj(o) => match &o.kind {
            Kind::Super(t, i, ot) => (t.clone(), i.clone(), ot.clone()),
            _ => (Value::None, Value::None, Value::None),
        },
        _ => (Value::None, Value::None, Value::None),
    }
}

#[lumen_bind::class(name = "super")]
/// super() -> same as super(__class__, <first argument>)
/// super(type) -> unbound super object
/// super(type, obj) -> bound super object; requires isinstance(obj, type)
/// super(type, type2) -> bound super object; requires issubclass(type2, type)
/// Typical use to call a cooperative superclass method:
/// class C(B):
///     def meth(self, arg):
///         super().meth(arg)
/// This works for class methods too:
/// class C(B):
///     @classmethod
///     def cmeth(cls, arg):
///         super().cmeth(arg)
///
pub struct Super;

#[lumen_bind::methods]
impl Super {
    #[constructor(hint(py(text_signature = "")))]
    fn new(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
        let _ = cls;
        if !kwargs.is_empty() {
            return Err(it.type_error("super() takes no keyword arguments"));
        }
        let (typ, inst) = match args {
            [] => implicit_super(it)?,
            [t] => (t.clone(), Value::None),
            [t, o] => (t.clone(), o.clone()),
            _ => return Err(it.type_error(&format!("super() expected at most 2 arguments, got {}", args.len()))),
        };
        make_super(it, typ, inst)
    }

    #[proto(init)]
    fn init(slf: This<&Value>, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) {
        let _ = (slf, args, kwargs);
    }

    /// the class invoking super()
    #[getter(name = "__thisclass__")]
    fn thisclass(slf: This<&Value>) -> Value {
        super_parts(&slf).0
    }

    /// the instance invoking super(); may be None
    #[getter(name = "__self__")]
    fn self_(slf: This<&Value>) -> Value {
        super_parts(&slf).1
    }

    /// the type of the instance invoking super(); may be None
    #[getter(name = "__self_class__")]
    fn self_class(slf: This<&Value>) -> Value {
        super_parts(&slf).2
    }

    /// Return an attribute of instance, which is of type owner.
    #[method(name = "__get__", hint(py(text_signature = "($self, instance, owner=None, /)")))]
    fn get(slf: This<&Value>, it: &mut Interp, instance: &Value, owner: Option<&Value>) -> R<Value> {
        let _ = owner;
        let Value::Obj(o) = &*slf else { unreachable!("a super") };
        let Kind::Super(typ, inst, _) = &o.kind else { return Err(it.type_error("descriptor '__get__' requires a 'super' object")) };
        if instance.is_none() || !inst.is_none() {
            return Ok(slf.clone());
        }
        make_super(it, typ.clone(), instance.clone())
    }
}

#[lumen_bind::class(name = "NoneType")]
pub struct NoneType;

#[lumen_bind::methods]
impl NoneType {
    #[constructor]
    fn new(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
        let _ = cls;
        if !args.is_empty() || !kwargs.is_empty() {
            return Err(it.type_error("NoneType takes no arguments"));
        }
        Ok(Value::None)
    }

    #[proto(bool)]
    fn bool(slf: This<&Value>, it: &mut Interp) -> R<bool> {
        if !slf.is_none() {
            let t = it.type_name_of(&slf);
            return Err(it.type_error(&format!("descriptor '__bool__' requires a 'NoneType' object but received a '{t}'")));
        }
        Ok(false)
    }
}

#[lumen_bind::class(name = "ellipsis")]
pub struct EllipsisType;

#[lumen_bind::methods]
impl EllipsisType {
    #[constructor]
    fn new(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
        if !kwargs.is_empty() {
            return Err(it.type_error("ellipsis() takes no keyword arguments"));
        }
        if !args.is_empty() {
            return Err(it.type_error(&format!("ellipsis expected 0 arguments, got {}", args.len())));
        }
        Ok(Value::Ellipsis)
    }

    #[method(name = "__reduce__", hint(py(text_signature = "")))]
    fn reduce(slf: This<&Value>) -> String {
        let _ = slf;
        "Ellipsis".to_string()
    }
}

#[lumen_bind::class(name = "NotImplementedType")]
pub struct NotImplementedType;

#[lumen_bind::methods]
impl NotImplementedType {
    #[constructor]
    fn new(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
        if !args.is_empty() || !kwargs.is_empty() {
            return Err(it.type_error("NotImplementedType takes no arguments"));
        }
        Ok(Value::NotImplemented)
    }

    #[method(name = "__reduce__", hint(py(text_signature = "")))]
    fn reduce(slf: This<&Value>) -> String {
        let _ = slf;
        "NotImplemented".to_string()
    }
}

pub fn init(it: &mut Interp) {
    let t = &it.types;
    let (object, type_, property, staticmethod, classmethod, super_, none_type, function) = (
        t.object.clone(),
        t.type_.clone(),
        t.property.clone(),
        t.staticmethod.clone(),
        t.classmethod.clone(),
        t.super_.clone(),
        t.none_type.clone(),
        t.function.clone(),
    );
    extend_type_documented::<ObjectType>(it, &object);
    crate::bind::set_type_text_signature(it, &object, Value::str("()"));
    if let Some(d) = object.dict.borrow().as_ref() {
        it.obj_new = dict_get_str(d, "__new__").and_then(|v| v.as_obj().cloned());
        it.obj_init = dict_get_str(d, "__init__").and_then(|v| v.as_obj().cloned());
    }
    extend_type_documented::<Type>(it, &type_);
    extend_type_documented::<Property>(it, &property);
    install_into::<DescrMethods>(&property, &["__get__", "__set__", "__delete__"]);
    install_into::<DescrMethods>(&function, &["__get__"]);
    extend_type_documented::<StaticMethod>(it, &staticmethod);
    extend_type_documented::<ClassMethod>(it, &classmethod);
    extend_type_documented::<Super>(it, &super_);
    extend_type::<NoneType>(it, &none_type);
    let (ellipsis, not_implemented) = (it.types.ellipsis_type.clone(), it.types.notimpl_type.clone());
    extend_type::<EllipsisType>(it, &ellipsis);
    extend_type::<NotImplementedType>(it, &not_implemented);
}

/// Installs the getset and member descriptors of `type` and `super` (once the descriptor types
/// exist).
pub fn install_getset_descriptors(it: &mut Interp) {
    let (type_, super_) = (it.types.type_.clone(), it.types.super_.clone());
    super::descr::install_getsets::<Type>(it, &type_, &["__base__", "__mro__", "__flags__"]);
    super::descr::install_deleters(
        it,
        &type_,
        &[
            ("__name__", del_name),
            ("__qualname__", del_qualname),
            ("__bases__", del_bases),
            ("__module__", del_module),
            ("__doc__", del_doc),
            ("__type_params__", del_type_params),
            ("__annotations__", del_annotations),
            ("__abstractmethods__", del_abstractmethods),
        ],
    );
    super::descr::install_getsets::<Super>(it, &super_, &["__thisclass__", "__self__", "__self_class__"]);
}

/// Installs `__get__` into a native method descriptor type.
pub fn install_descr_methods_get(ty: &Obj) {
    install_into::<DescrMethods>(ty, &["__get__"]);
}

/// Installs the descriptor methods into the getset / member descriptor type `ty`.
pub fn install_descr_methods(ty: &Obj) {
    install_into::<DescrMethods>(ty, &["__get__", "__set__", "__delete__"]);
}
