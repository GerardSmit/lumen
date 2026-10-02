//! `_testcapi` types and type-level helpers: the `meth_*` calling-convention probes, the
//! vectorcall and method-descriptor classes (`vectorcall.c`), the heap types
//! (`heaptype.c`), type version tags and the operator/awaitable/generic probe types of
//! `_testcapimodule.c`. The probes of CPython's C slots map onto the engine's equivalent
//! protocol hooks; `Py_TPFLAGS_*` bits are the engine's `TF_*` flags.

use super::{call_builtin, name_obj};
use crate::bind::{install_all, is_instance, opaque_instance, type_object, KwArgs, This};
use crate::builtins::native::{new_type, with_opaque};
use crate::object::*;
use crate::vm::{dict_get_str, dict_set_str, Interp};
use crate::watch;

fn testcapi(it: &mut Interp) -> Value {
    it.import_module("_testcapi").map(Value::Obj).unwrap_or(Value::None)
}

fn type_arg(it: &mut Interp, v: &Value, func: &str) -> R<Obj> {
    match v {
        Value::Obj(o) if matches!(o.kind, Kind::Type(_)) => Ok(o.clone()),
        _ => {
            let t = it.type_name_of(v);
            Err(it.type_error(&format!("{func}() argument 1 must be type, not {t}")))
        }
    }
}

fn must_be_type(it: &mut Interp, v: &Value) -> R<Obj> {
    match v {
        Value::Obj(o) if matches!(o.kind, Kind::Type(_)) => Ok(o.clone()),
        _ => Err(it.type_error("argument must be a type")),
    }
}

fn heap_type_arg(it: &mut Interp, v: &Value) -> R<Obj> {
    match v {
        Value::Obj(o) if matches!(&o.kind, Kind::Type(_)) && it.is_heap(o) => Ok(o.clone()),
        _ => {
            let r = it.repr_of(v)?;
            Err(it.type_error(&format!("heap type expected, got {r}")))
        }
    }
}

fn type_flag_bits(t: &Obj) -> u32 {
    match &t.kind {
        Kind::Type(td) => td.flags.get(),
        _ => 0,
    }
}

fn set_type_flags(t: &Obj, set: u32, clear: u32) {
    if let Kind::Type(td) = &t.kind {
        td.flags.set((td.flags.get() | set) & !clear);
    }
}

fn instance_of(it: &mut Interp, cls: &Value, state: impl std::any::Any) -> R<Value> {
    match cls {
        Value::Obj(c) if matches!(c.kind, Kind::Type(_)) => Ok(opaque_instance(c, state)),
        _ => Err(it.type_error("__new__(X): X is not a type object")),
    }
}

/// A heap type made like `type(name, bases, {})`, with `__module__` set.
fn make_class(it: &mut Interp, meta: &Obj, name: &str, module: &str, bases: &[Obj], doc: Option<&str>) -> R<Obj> {
    let ns = it.new_dict();
    dict_set_str(&ns, "__module__", Value::str(module));
    if let Some(d) = doc {
        dict_set_str(&ns, "__doc__", Value::str(d));
    }
    let bases = Value::tuple(bases.iter().cloned().map(Value::Obj).collect());
    let args = vec![Value::str(name), bases, Value::Obj(ns)];
    match it.type_new_from_args(meta.clone(), &args, Vec::new())? {
        Value::Obj(t) => Ok(t),
        _ => unreachable!("type.__new__ returns a type"),
    }
}

fn has_custom_new(it: &mut Interp, meta: &Obj) -> bool {
    let type_new = it.lookup_mro(&it.types.type_.clone(), "__new__");
    let own = it.lookup_mro(meta, "__new__");
    match (own, type_new) {
        (Some(Value::Obj(a)), Some(Value::Obj(b))) => !std::rc::Rc::ptr_eq(&a, &b),
        (Some(_), None) => true,
        _ => false,
    }
}

#[derive(Default, Clone, Copy)]
struct Ints {
    value: i64,
    value2: i64,
    pvalue: i64,
}

fn ints<X>(v: &Value, f: impl FnOnce(&mut Ints) -> X) -> Option<X> {
    let mut f = Some(f);
    macro_rules! each {
        ($($t:ty),*) => {$(
            if let Some(r) = with_opaque::<$t, _>(v, |s| (f.take().expect("called once"))(&mut s.0)) {
                return Some(r);
            }
        )*};
    }
    each!(typesm::HeapGcCType, typesm::HeapCType, typesm::HeapCTypeSubclass, typesm::HeapCTypeSubclassWithFinalizer, typesm::HeapCTypeSetattr);
    None
}

fn ints_missing(it: &mut Interp, v: &Value) -> Obj {
    let t = it.type_name_of(v);
    it.type_error(&format!("descriptor requires a heap C type object but received '{t}'"))
}

fn weakref_list_head(it: &mut Interp, v: &Value) -> R<Value> {
    let m = it.import_module("_weakref")?;
    let f = it.get_attr_str(&Value::Obj(m), "getweakrefs")?;
    let refs = it.call(&f, vec![v.clone()], Vec::new())?;
    Ok(it.iterate_to_vec(&refs)?.into_iter().next().unwrap_or(Value::None))
}

fn call_args_tuple(it: &mut Interp, args: &Value, what: &str) -> R<Vec<Value>> {
    if args.is_none() {
        return Ok(Vec::new());
    }
    match args.tuple_items() {
        Some(t) => Ok(t.to_vec()),
        None => Err(it.type_error(&format!("{what} must be None or a tuple"))),
    }
}

#[lumen_bind::module(name = "_testcapi")]
pub mod typesm {
    #![allow(clippy::new_ret_no_self, non_snake_case)]
    use super::*;

    // ---- METH_* calling conventions -----------------------------------------------------------

    /// meth_varargs(*args) -> (module, args)
    #[op]
    fn meth_varargs(it: &mut Interp, #[varargs] args: &[Value]) -> Value {
        Value::tuple(vec![testcapi(it), Value::tuple(args.to_vec())])
    }

    /// meth_varargs_keywords(*args, **kwargs) -> (module, args, kwargs or None)
    #[op]
    fn meth_varargs_keywords(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
        let kw = if kwargs.is_empty() { Value::None } else { Value::Obj(it.kwargs_to_dict(&kwargs.to_vec())?) };
        Ok(Value::tuple(vec![testcapi(it), Value::tuple(args.to_vec()), kw]))
    }

    /// meth_o(obj) -> (module, obj)
    #[op]
    fn meth_o(it: &mut Interp, obj: &Value) -> Value {
        Value::tuple(vec![testcapi(it), obj.clone()])
    }

    /// meth_noargs() -> module
    #[op]
    fn meth_noargs(it: &mut Interp) -> Value {
        testcapi(it)
    }

    /// meth_fastcall(*args) -> (module, args)
    #[op]
    fn meth_fastcall(it: &mut Interp, #[varargs] args: &[Value]) -> Value {
        Value::tuple(vec![testcapi(it), Value::tuple(args.to_vec())])
    }

    /// meth_fastcall_keywords(*args, **kwargs) -> (module, args, kwargs)
    #[op]
    fn meth_fastcall_keywords(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
        let kw = Value::Obj(it.kwargs_to_dict(&kwargs.to_vec())?);
        Ok(Value::tuple(vec![testcapi(it), Value::tuple(args.to_vec()), kw]))
    }

    #[class(module = "_testcapi", name = "MethInstance")]
    /// Class with instance methods to test calling conventions
    pub struct MethInstance;

    #[methods]
    impl MethInstance {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp) -> R<Value> {
            instance_of(it, &cls.0, MethInstance)
        }

        fn meth_varargs(slf: This<Value>, #[varargs] args: &[Value]) -> Value {
            Value::tuple(vec![slf.0, Value::tuple(args.to_vec())])
        }

        fn meth_varargs_keywords(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
            let kw = if kwargs.is_empty() { Value::None } else { Value::Obj(it.kwargs_to_dict(&kwargs.to_vec())?) };
            Ok(Value::tuple(vec![slf.0, Value::tuple(args.to_vec()), kw]))
        }

        fn meth_o(slf: This<Value>, obj: &Value) -> Value {
            Value::tuple(vec![slf.0, obj.clone()])
        }

        fn meth_noargs(slf: This<Value>) -> Value {
            slf.0
        }

        fn meth_fastcall(slf: This<Value>, #[varargs] args: &[Value]) -> Value {
            Value::tuple(vec![slf.0, Value::tuple(args.to_vec())])
        }

        fn meth_fastcall_keywords(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
            let kw = Value::Obj(it.kwargs_to_dict(&kwargs.to_vec())?);
            Ok(Value::tuple(vec![slf.0, Value::tuple(args.to_vec()), kw]))
        }
    }

    #[class(module = "_testcapi", name = "MethClass")]
    /// Class with class methods to test calling conventions
    pub struct MethClass;

    #[methods]
    impl MethClass {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp) -> R<Value> {
            instance_of(it, &cls.0, MethClass)
        }

        #[classmethod]
        fn meth_varargs(cls: This<Value>, #[varargs] args: &[Value]) -> Value {
            Value::tuple(vec![cls.0, Value::tuple(args.to_vec())])
        }

        #[classmethod]
        fn meth_varargs_keywords(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
            let kw = if kwargs.is_empty() { Value::None } else { Value::Obj(it.kwargs_to_dict(&kwargs.to_vec())?) };
            Ok(Value::tuple(vec![cls.0, Value::tuple(args.to_vec()), kw]))
        }

        #[classmethod]
        fn meth_o(cls: This<Value>, obj: &Value) -> Value {
            Value::tuple(vec![cls.0, obj.clone()])
        }

        #[classmethod]
        fn meth_noargs(cls: This<Value>) -> Value {
            cls.0
        }

        #[classmethod]
        fn meth_fastcall(cls: This<Value>, #[varargs] args: &[Value]) -> Value {
            Value::tuple(vec![cls.0, Value::tuple(args.to_vec())])
        }

        #[classmethod]
        fn meth_fastcall_keywords(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
            let kw = Value::Obj(it.kwargs_to_dict(&kwargs.to_vec())?);
            Ok(Value::tuple(vec![cls.0, Value::tuple(args.to_vec()), kw]))
        }
    }

    #[class(module = "_testcapi", name = "MethStatic")]
    /// Class with static methods to test calling conventions
    pub struct MethStatic;

    #[methods]
    impl MethStatic {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp) -> R<Value> {
            instance_of(it, &cls.0, MethStatic)
        }

        fn meth_varargs(#[varargs] args: &[Value]) -> Value {
            Value::tuple(vec![Value::None, Value::tuple(args.to_vec())])
        }

        fn meth_varargs_keywords(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
            let kw = if kwargs.is_empty() { Value::None } else { Value::Obj(it.kwargs_to_dict(&kwargs.to_vec())?) };
            Ok(Value::tuple(vec![Value::None, Value::tuple(args.to_vec()), kw]))
        }

        fn meth_o(obj: &Value) -> Value {
            Value::tuple(vec![Value::None, obj.clone()])
        }

        fn meth_noargs() -> Value {
            Value::None
        }

        fn meth_fastcall(#[varargs] args: &[Value]) -> Value {
            Value::tuple(vec![Value::None, Value::tuple(args.to_vec())])
        }

        fn meth_fastcall_keywords(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
            let kw = Value::Obj(it.kwargs_to_dict(&kwargs.to_vec())?);
            Ok(Value::tuple(vec![Value::None, Value::tuple(args.to_vec()), kw]))
        }
    }

    // ---- vectorcall ---------------------------------------------------------------------------

    /// pyobject_fastcall(func, args)
    #[op]
    fn pyobject_fastcall(it: &mut Interp, func: &Value, args: &Value) -> R<Value> {
        let a = call_args_tuple(it, args, "args")?;
        it.call(func, a, Vec::new())
    }

    /// pyobject_fastcalldict(func, args, kwargs)
    #[op]
    fn pyobject_fastcalldict(it: &mut Interp, func: &Value, args: &Value, kwargs: &Value) -> R<Value> {
        let a = call_args_tuple(it, args, "args")?;
        let kw = if kwargs.is_none() {
            Vec::new()
        } else if matches!(kwargs, Value::Obj(o) if matches!(o.kind, Kind::Dict(_))) {
            it.dict_to_kwargs(kwargs)?
        } else {
            return Err(it.type_error("kwnames must be None or a dict"));
        };
        it.call(func, a, kw)
    }

    /// pyobject_vectorcall(func, args, kwnames)
    #[op]
    fn pyobject_vectorcall(it: &mut Interp, func: &Value, args: &Value, kwnames: &Value) -> R<Value> {
        let mut a = call_args_tuple(it, args, "args")?;
        let mut kw = Vec::new();
        if !kwnames.is_none() {
            let Some(names) = kwnames.tuple_items() else {
                return Err(it.type_error("kwnames must be None or a tuple"));
            };
            if a.len() < names.len() {
                return Err(it.value_error("kwnames longer than args"));
            }
            let split = a.len() - names.len();
            let values = a.split_off(split);
            for (n, v) in names.iter().zip(values) {
                kw.push((name_obj(it, n)?, v));
            }
        }
        it.call(func, a, kw)
    }

    /// pyvectorcall_call(func, args, kwargs=None)
    #[op]
    fn pyvectorcall_call(it: &mut Interp, func: &Value, args: &Value, kwargs: Option<&Value>) -> R<Value> {
        let Some(a) = args.tuple_items() else {
            return Err(it.type_error("args must be a tuple"));
        };
        let a = a.to_vec();
        let kw = match kwargs {
            Some(k) if matches!(k, Value::Obj(o) if matches!(o.kind, Kind::Dict(_))) => it.dict_to_kwargs(k)?,
            Some(_) => return Err(it.type_error("kwargs must be a dict")),
            None => Vec::new(),
        };
        it.call(func, a, kw)
    }

    /// function_setvectorcall(func): later calls of `func` return "overridden".
    #[op]
    fn function_setvectorcall(it: &mut Interp, func: &Value) -> R<()> {
        let f = match func {
            Value::Obj(o) if matches!(o.kind, Kind::Function(_)) => o.clone(),
            _ => return Err(it.type_error("'func' must be a function")),
        };
        let src = Value::str("lambda *args, **kwargs: 'overridden'");
        let code = call_builtin(it, "compile", vec![src, Value::str("<_testcapi>"), Value::str("eval")])?;
        let lam = call_builtin(it, "eval", vec![code])?;
        let replacement = match &lam {
            Value::Obj(o) => match &o.kind {
                Kind::Function(l) => l.code.borrow().clone(),
                _ => return Err(it.type_error("'func' must be a function")),
            },
            _ => return Err(it.type_error("'func' must be a function")),
        };
        if let Kind::Function(target) = &f.kind {
            *target.code.borrow_mut() = replacement;
        }
        Ok(())
    }

    #[class(module = "_testcapi", name = "VectorcallClass", hint(py(mutable)))]
    /// Instances return "tp_call" when called; set_vectorcall installs a vectorcall function.
    pub struct VectorCallClass;

    #[methods]
    impl VectorCallClass {
        /// Set self's vectorcall function for `type` to one that returns "vectorcall"
        #[method(name = "set_vectorcall", hint(py(text_signature = "($self, type, /)")))]
        fn set_vectorcall(slf: This<Value>, it: &mut Interp, ty: &Value) -> R<()> {
            let t = type_arg(it, ty, "set_vectorcall")?;
            let cls = match &slf.0 {
                Value::Obj(o) => it.type_of_obj(o),
                _ => it.type_of(&slf.0),
            };
            if !it.is_subtype(&cls, &t) {
                let n = it.type_name(&t);
                return Err(it.type_error(&format!("expected {n} instance")));
            }
            if it.lookup_mro(&t, "__vectorcalloffset__").is_none() {
                let n = it.type_name(&t);
                return Err(it.type_error(&format!("type {n} has no vectorcall offset")));
            }
            let Value::Obj(o) = &slf.0 else { return Ok(()) };
            let d = it.instance_dict(o);
            dict_set_str(&d, "__vectorcall__", Value::Bool(true));
            Ok(())
        }

        #[proto(call)]
        fn call(slf: This<Value>, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> Value {
            let overridden = match &slf.0 {
                Value::Obj(o) => o.dict.borrow().as_ref().is_some_and(|d| dict_get_str(d, "__vectorcall__").is_some()),
                _ => false,
            };
            Value::str(if overridden { "vectorcall" } else { "tp_call" })
        }
    }

    /// make_vectorcall_class(base=object, /): a class whose instances return "tp_call" when called.
    #[op]
    fn make_vectorcall_class(it: &mut Interp, base: Option<&Value>) -> R<Value> {
        let base = match base {
            Some(b) => type_arg(it, b, "make_vectorcall_class")?,
            None => it.types.object.clone(),
        };
        let meta = it.types.type_.clone();
        let ty = make_class(it, &meta, "VectorcallClass", "_testcapi", &[base], None)?;
        install_all::<VectorCallClass>(&ty);
        if let Some(d) = ty.dict.borrow().as_ref() {
            dict_set_str(d, "__vectorcalloffset__", Value::Int(16));
        }
        set_type_flags(&ty, TF_VECTORCALL, 0);
        Ok(Value::Obj(ty))
    }

    /// has_vectorcall_flag(type, /): whether Py_TPFLAGS_HAVE_VECTORCALL is set on the class.
    #[op]
    fn has_vectorcall_flag(it: &mut Interp, ty: &Value) -> R<bool> {
        let t = type_arg(it, ty, "has_vectorcall_flag")?;
        Ok(type_flag_bits(&t) & TF_VECTORCALL != 0)
    }

    #[class(module = "builtins", name = "MethodDescriptorBase")]
    pub struct MethodDescriptorBase;

    #[methods]
    impl MethodDescriptorBase {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<Value> {
            instance_of(it, &cls.0, MethodDescriptorBase)
        }

        #[proto(call)]
        fn call(slf: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> bool {
            !is_instance::<MethodDescriptor2>(it, &slf.0)
        }

        #[method(name = "__get__")]
        fn get(slf: This<Value>, instance: Option<&Value>, owner: Option<&Value>) -> Value {
            let _ = owner;
            match instance {
                Some(i) if !i.is_none() => Value::Obj(Object::new(Kind::Method(slf.0, i.clone()))),
                _ => slf.0,
            }
        }
    }

    #[class(module = "builtins", name = "MethodDescriptorDerived", hint(py(base = "_testcapi.MethodDescriptorBase")))]
    pub struct MethodDescriptorDerived;

    #[methods]
    impl MethodDescriptorDerived {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<Value> {
            instance_of(it, &cls.0, MethodDescriptorDerived)
        }
    }

    #[class(module = "builtins", name = "MethodDescriptorNopGet", hint(py(base = "_testcapi.MethodDescriptorBase")))]
    pub struct MethodDescriptorNopGet;

    #[methods]
    impl MethodDescriptorNopGet {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<Value> {
            instance_of(it, &cls.0, MethodDescriptorNopGet)
        }

        #[proto(call)]
        fn call(_slf: This<Value>, #[varargs] args: &[Value], #[varkw] _kwargs: KwArgs) -> Value {
            Value::tuple(args.to_vec())
        }

        #[method(name = "__get__")]
        fn get(slf: This<Value>, instance: Option<&Value>, owner: Option<&Value>) -> Value {
            let _ = (instance, owner);
            slf.0
        }
    }

    #[class(module = "builtins", name = "MethodDescriptor2", hint(py(base = "_testcapi.MethodDescriptorBase")))]
    pub struct MethodDescriptor2;

    #[methods]
    impl MethodDescriptor2 {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<Value> {
            instance_of(it, &cls.0, MethodDescriptor2)
        }
    }

    // ---- operator, awaitable and generic probe types --------------------------------------------

    #[class(module = "builtins", name = "matmulType")]
    /// C level type with matrix operations defined
    pub struct MatmulType;

    #[methods]
    impl MatmulType {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp) -> R<Value> {
            instance_of(it, &cls.0, MatmulType)
        }

        #[proto(matmul)]
        fn matmul(slf: This<Value>, other: &Value) -> Value {
            Value::tuple(vec![Value::str("matmul"), slf.0, other.clone()])
        }

        #[proto(rmatmul)]
        fn rmatmul(slf: This<Value>, other: &Value) -> Value {
            Value::tuple(vec![Value::str("matmul"), other.clone(), slf.0])
        }

        #[proto(imatmul)]
        fn imatmul(slf: This<Value>, other: &Value) -> Value {
            Value::tuple(vec![Value::str("imatmul"), slf.0, other.clone()])
        }
    }

    #[class(module = "builtins", name = "ipowType")]
    pub struct IpowType;

    #[methods]
    impl IpowType {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp) -> R<Value> {
            instance_of(it, &cls.0, IpowType)
        }

        #[proto(ipow)]
        fn ipow(_slf: This<Value>, other: &Value, modulo: Option<&Value>) -> Value {
            Value::tuple(vec![other.clone(), modulo.cloned().unwrap_or(Value::None)])
        }
    }

    #[class(module = "builtins", name = "awaitType")]
    /// C level type with tp_as_async
    pub struct AwaitType {
        iterator: Value,
    }

    #[methods]
    impl AwaitType {
        #[constructor(hint(py(arg_style = "unpack", arg_name = "awaitObject")))]
        fn new(cls: This<Value>, it: &mut Interp, iterator: &Value) -> R<Value> {
            instance_of(it, &cls.0, AwaitType { iterator: iterator.clone() })
        }

        #[proto(await)]
        fn await_(&self) -> Value {
            self.iterator.clone()
        }
    }

    #[class(module = "builtins", name = "GenericAlias")]
    pub struct GenericAlias {
        item: Value,
    }

    #[methods]
    impl GenericAlias {
        #[method(name = "__mro_entries__")]
        fn mro_entries(&self, bases: &Value) -> Value {
            let _ = bases;
            Value::tuple(vec![self.item.clone()])
        }
    }

    #[class(module = "builtins", name = "Generic")]
    pub struct Generic;

    #[methods]
    impl Generic {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp) -> R<Value> {
            instance_of(it, &cls.0, Generic)
        }

        #[classmethod]
        fn __class_getitem__(cls: This<Value>, it: &mut Interp, item: &Value) -> Value {
            let _ = cls;
            let t = type_object::<GenericAlias>(it);
            opaque_instance(&t, GenericAlias { item: item.clone() })
        }
    }

    #[class(module = "_testcapi", name = "ContainerNoGC")]
    pub struct ContainerNoGC {
        value: Value,
    }

    #[methods]
    impl ContainerNoGC {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[kw] value: &Value) -> R<Value> {
            instance_of(it, &cls.0, ContainerNoGC { value: value.clone() })
        }

        /// a container value for test purposes
        #[getter]
        fn value(&self) -> Value {
            self.value.clone()
        }
    }

    #[class(module = "_testcapi", name = "instancemethod")]
    /// instancemethod(function)
    ///
    /// Bind a function to a class.
    pub struct InstanceMethod {
        func: Value,
    }

    #[methods]
    impl InstanceMethod {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, function: &Value) -> R<Value> {
            if !it.is_callable(function) {
                return Err(it.type_error("first argument must be callable"));
            }
            instance_of(it, &cls.0, InstanceMethod { func: function.clone() })
        }

        #[proto(call)]
        fn call(&self, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
            it.call(&self.func, args.to_vec(), kwargs.to_vec())
        }

        #[method(name = "__get__")]
        fn get(&self, instance: Option<&Value>, owner: Option<&Value>) -> Value {
            let _ = owner;
            match instance {
                Some(i) if !i.is_none() => Value::Obj(Object::new(Kind::Method(self.func.clone(), i.clone()))),
                _ => self.func.clone(),
            }
        }

        #[getter(name = "__func__")]
        fn func(&self) -> Value {
            self.func.clone()
        }
    }

    // ---- heap types ---------------------------------------------------------------------------

    #[class(module = "_testcapi", name = "HeapDocCType")]
    /// somedoc
    pub struct HeapDocCType;

    #[methods]
    impl HeapDocCType {
        #[constructor(hint(py(text_signature = "(arg1, arg2)")))]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<Value> {
            instance_of(it, &cls.0, HeapDocCType)
        }
    }

    #[class(module = "_testcapi", name = "NullTpDocType")]
    pub struct NullTpDocType;

    #[methods]
    impl NullTpDocType {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<Value> {
            instance_of(it, &cls.0, NullTpDocType)
        }
    }

    #[class(module = "_testcapi", name = "HeapGcCType")]
    /// A heap type with GC, and with overridden dealloc.
    ///
    /// The 'value' attribute is set to 10 in __init__.
    pub struct HeapGcCType(pub(super) Ints);

    #[methods]
    impl HeapGcCType {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<Value> {
            instance_of(it, &cls.0, HeapGcCType(Ints::default()))
        }

        #[proto(init)]
        fn init(slf: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<()> {
            ints(&slf.0, |i| i.value = 10).ok_or_else(|| ints_missing(it, &slf.0))
        }

        #[getter]
        fn value(slf: This<Value>, it: &mut Interp) -> R<i64> {
            ints(&slf.0, |i| i.value).ok_or_else(|| ints_missing(it, &slf.0))
        }

        #[setter]
        fn set_value(slf: This<Value>, it: &mut Interp, v: i64) -> R<()> {
            ints(&slf.0, |i| i.value = v).ok_or_else(|| ints_missing(it, &slf.0))
        }
    }

    #[class(module = "_testcapi", name = "HeapCType")]
    /// A heap type without GC, but with overridden dealloc.
    ///
    /// The 'value' attribute is set to 10 in __init__.
    pub struct HeapCType(pub(super) Ints);

    #[methods]
    impl HeapCType {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<Value> {
            instance_of(it, &cls.0, HeapCType(Ints::default()))
        }

        #[proto(init)]
        fn init(slf: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<()> {
            ints(&slf.0, |i| i.value = 10).ok_or_else(|| ints_missing(it, &slf.0))
        }

        #[getter]
        fn value(slf: This<Value>, it: &mut Interp) -> R<i64> {
            ints(&slf.0, |i| i.value).ok_or_else(|| ints_missing(it, &slf.0))
        }

        #[setter]
        fn set_value(slf: This<Value>, it: &mut Interp, v: i64) -> R<()> {
            ints(&slf.0, |i| i.value = v).ok_or_else(|| ints_missing(it, &slf.0))
        }
    }

    #[class(module = "_testcapi", name = "HeapCTypeSubclass", hint(py(base = "_testcapi.HeapCType")))]
    /// Subclass of HeapCType, without GC.
    ///
    /// __init__ sets the 'value' attribute to 10 and 'value2' to 20.
    pub struct HeapCTypeSubclass(pub(super) Ints);

    #[methods]
    impl HeapCTypeSubclass {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<Value> {
            instance_of(it, &cls.0, HeapCTypeSubclass(Ints::default()))
        }

        #[proto(init)]
        fn init(slf: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<()> {
            ints(&slf.0, |i| {
                i.value = 10;
                i.value2 = 20;
            })
            .ok_or_else(|| ints_missing(it, &slf.0))
        }

        #[getter]
        fn value2(slf: This<Value>, it: &mut Interp) -> R<i64> {
            ints(&slf.0, |i| i.value2).ok_or_else(|| ints_missing(it, &slf.0))
        }

        #[setter]
        fn set_value2(slf: This<Value>, it: &mut Interp, v: i64) -> R<()> {
            ints(&slf.0, |i| i.value2 = v).ok_or_else(|| ints_missing(it, &slf.0))
        }
    }

    #[class(module = "_testcapi", name = "HeapCTypeSubclassWithFinalizer", hint(py(base = "_testcapi.HeapCTypeSubclass")))]
    /// Subclass of HeapCType with a finalizer that reassigns __class__.
    ///
    /// __class__ is set to plain HeapCTypeSubclass during finalization.
    /// __init__ sets the 'value' attribute to 10 and 'value2' to 20.
    pub struct HeapCTypeSubclassWithFinalizer(pub(super) Ints);

    #[methods]
    impl HeapCTypeSubclassWithFinalizer {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<Value> {
            instance_of(it, &cls.0, HeapCTypeSubclassWithFinalizer(Ints::default()))
        }

        #[method(name = "__del__")]
        fn finalize(slf: This<Value>, it: &mut Interp) {
            let m = testcapi(it);
            let sys = it.import_module("sys").map(Value::Obj).unwrap_or(Value::None);
            let refcount = |it: &mut Interp, t: &Value| -> Option<Value> {
                let f = it.get_attr_str(&sys, "getrefcount").ok()?;
                it.call(&f, vec![t.clone()], Vec::new()).ok()
            };
            let (Ok(oldtype), Ok(newtype)) =
                (it.get_attr_str(&m, "HeapCTypeSubclassWithFinalizer"), it.get_attr_str(&m, "HeapCTypeSubclass"))
            else {
                return;
            };
            if it.set_attr_str(&slf.0, "__class__", newtype.clone()).is_err() {
                return;
            }
            for t in [&oldtype, &newtype] {
                let Some(n) = refcount(it, t) else { return };
                if it.set_attr_str(t, "refcnt_in_del", n).is_err() {
                    return;
                }
            }
        }
    }

    #[class(module = "_testcapi", name = "HeapCTypeWithBuffer")]
    /// Heap type with buffer support.
    ///
    /// The buffer is set to [b'1', b'2', b'3', b'4']
    pub struct HeapCTypeWithBuffer;

    #[methods]
    impl HeapCTypeWithBuffer {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<Value> {
            instance_of(it, &cls.0, HeapCTypeWithBuffer)
        }

        #[method(name = "__buffer__")]
        fn buffer(slf: This<Value>, it: &mut Interp, flags: i64) -> R<Value> {
            let _ = (slf, flags);
            call_builtin(it, "memoryview", vec![Value::bytes(b"1234".to_vec())])
        }

        #[method(name = "__release_buffer__")]
        fn release_buffer(slf: This<Value>, view: &Value) {
            let _ = (slf, view);
        }
    }

    #[class(module = "_testcapi", name = "HeapCTypeWithDict")]
    pub struct HeapCTypeWithDict;

    #[methods]
    impl HeapCTypeWithDict {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<Value> {
            instance_of(it, &cls.0, HeapCTypeWithDict)
        }

        #[getter]
        fn dictobj(slf: This<Value>, it: &mut Interp) -> Value {
            match &slf.0 {
                Value::Obj(o) => Value::Obj(it.instance_dict(o)),
                _ => Value::None,
            }
        }
    }

    #[class(module = "_testcapi", name = "HeapCTypeWithDict2")]
    pub struct HeapCTypeWithDict2;

    #[methods]
    impl HeapCTypeWithDict2 {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<Value> {
            instance_of(it, &cls.0, HeapCTypeWithDict2)
        }

        #[getter]
        fn dictobj(slf: This<Value>, it: &mut Interp) -> Value {
            match &slf.0 {
                Value::Obj(o) => Value::Obj(it.instance_dict(o)),
                _ => Value::None,
            }
        }
    }

    #[class(module = "_testcapi", name = "HeapCTypeWithNegativeDict")]
    pub struct HeapCTypeWithNegativeDict;

    #[methods]
    impl HeapCTypeWithNegativeDict {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<Value> {
            instance_of(it, &cls.0, HeapCTypeWithNegativeDict)
        }

        #[getter]
        fn dictobj(slf: This<Value>, it: &mut Interp) -> Value {
            match &slf.0 {
                Value::Obj(o) => Value::Obj(it.instance_dict(o)),
                _ => Value::None,
            }
        }
    }

    #[class(module = "_testcapi", name = "HeapCTypeWithManagedDict")]
    pub struct HeapCTypeWithManagedDict;

    #[methods]
    impl HeapCTypeWithManagedDict {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<Value> {
            instance_of(it, &cls.0, HeapCTypeWithManagedDict)
        }
    }

    #[class(module = "_testcapi", name = "HeapCTypeWithManagedWeakref")]
    pub struct HeapCTypeWithManagedWeakref;

    #[methods]
    impl HeapCTypeWithManagedWeakref {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<Value> {
            instance_of(it, &cls.0, HeapCTypeWithManagedWeakref)
        }
    }

    #[class(module = "_testcapi", name = "HeapCTypeWithWeakref")]
    pub struct HeapCTypeWithWeakref;

    #[methods]
    impl HeapCTypeWithWeakref {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<Value> {
            instance_of(it, &cls.0, HeapCTypeWithWeakref)
        }

        #[getter]
        fn weakreflist(slf: This<Value>, it: &mut Interp) -> R<Value> {
            weakref_list_head(it, &slf.0)
        }
    }

    #[class(module = "_testcapi", name = "HeapCTypeWithWeakref2")]
    pub struct HeapCTypeWithWeakref2;

    #[methods]
    impl HeapCTypeWithWeakref2 {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<Value> {
            instance_of(it, &cls.0, HeapCTypeWithWeakref2)
        }

        #[getter]
        fn weakreflist(slf: This<Value>, it: &mut Interp) -> R<Value> {
            weakref_list_head(it, &slf.0)
        }
    }

    #[class(module = "_testcapi", name = "HeapCTypeSetattr")]
    /// A heap type without GC, but with overridden __setattr__.
    ///
    /// The 'value' attribute is set to 10 in __init__ and updated via attribute setting.
    pub struct HeapCTypeSetattr(pub(super) Ints);

    #[methods]
    impl HeapCTypeSetattr {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<Value> {
            instance_of(it, &cls.0, HeapCTypeSetattr(Ints::default()))
        }

        #[proto(init)]
        fn init(slf: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<()> {
            ints(&slf.0, |i| i.pvalue = 10).ok_or_else(|| ints_missing(it, &slf.0))
        }

        #[getter]
        fn pvalue(slf: This<Value>, it: &mut Interp) -> R<i64> {
            ints(&slf.0, |i| i.pvalue).ok_or_else(|| ints_missing(it, &slf.0))
        }

        #[setter]
        fn set_pvalue(slf: This<Value>, it: &mut Interp, v: i64) -> R<()> {
            ints(&slf.0, |i| i.pvalue = v).ok_or_else(|| ints_missing(it, &slf.0))
        }

        #[proto(setattr)]
        fn setattr(slf: This<Value>, it: &mut Interp, name: &Value, value: &Value) -> R<()> {
            if name.as_str() != Some("value") {
                let object = Value::Obj(it.types.object.clone());
                let f = it.get_attr_str(&object, "__setattr__")?;
                it.call(&f, vec![slf.0, name.clone(), value.clone()], Vec::new())?;
                return Ok(());
            }
            let n = call_builtin(it, "int", vec![value.clone()])?;
            let Value::Int(n) = n else {
                return Err(it.new_exc_str("OverflowError", "Python int too large to convert to C long"));
            };
            ints(&slf.0, |i| i.pvalue = n).ok_or_else(|| ints_missing(it, &slf.0))
        }

        #[proto(delattr)]
        fn delattr(slf: This<Value>, it: &mut Interp, name: &Value) -> R<()> {
            if name.as_str() != Some("value") {
                let object = Value::Obj(it.types.object.clone());
                let f = it.get_attr_str(&object, "__delattr__")?;
                it.call(&f, vec![slf.0, name.clone()], Vec::new())?;
                return Ok(());
            }
            ints(&slf.0, |i| i.pvalue = 0).ok_or_else(|| ints_missing(it, &slf.0))
        }
    }

    #[class(module = "_testcapi", name = "HeapCCollection")]
    /// Tuple-like heap type that uses PyObject_GetItemData for items.
    pub struct HeapCCollection {
        items: Vec<Value>,
    }

    #[methods]
    impl HeapCCollection {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
            instance_of(it, &cls.0, HeapCCollection { items: args.to_vec() })
        }

        #[proto(len)]
        fn len(&self) -> i64 {
            self.items.len() as i64
        }

        #[proto(getitem)]
        fn getitem(&self, it: &mut Interp, index: i64) -> R<Value> {
            let n = self.items.len() as i64;
            let i = if index < 0 { index + n } else { index };
            if i < 0 || i >= n {
                return Err(it.new_exc_str("IndexError", &format!("index {index} out of range")));
            }
            Ok(self.items[i as usize].clone())
        }
    }

    /// pyobject_getitemdata(obj): the address of the items of a variable-size heap object.
    #[op]
    fn pyobject_getitemdata(it: &mut Interp, obj: &Value) -> R<i64> {
        if !is_instance::<HeapCCollection>(it, obj) {
            let t = it.tp_name_of(obj);
            return Err(it.type_error(&format!("type {t} does not have Py_TPFLAGS_ITEMS_AT_END")));
        }
        Ok(it.id_of(obj) as i64 + 32)
    }

    // ---- metaclasses and types from specs -------------------------------------------------------

    #[class(module = "_testcapi", name = "HeapCTypeMetaclassNullNewMarker")]
    pub struct NullNew;

    #[methods]
    impl NullNew {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] _args: &[Value], #[varkw] _kwargs: KwArgs) -> R<Value> {
            let n = match &cls.0 {
                Value::Obj(t) => it.type_display(t),
                _ => "type".to_string(),
            };
            Err(it.type_error(&format!("cannot create '{n}' instances")))
        }
    }

    #[class(module = "_testcapi", name = "HeapCTypeMetaclassCustomNewMarker")]
    pub struct CustomNew;

    #[methods]
    impl CustomNew {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
            let Value::Obj(meta) = cls.0 else { return Err(it.type_error("type.__new__(X): X is not a type object")) };
            if args.len() != 3 {
                return Err(it.type_error("type() takes 1 or 3 arguments"));
            }
            it.type_new_from_args(meta, args, kwargs.to_vec())
        }
    }

    /// pytype_fromspec_meta(meta): create a class from a spec with `meta` as its metaclass.
    #[op]
    fn pytype_fromspec_meta(it: &mut Interp, meta: &Value) -> R<Value> {
        let Value::Obj(m) = meta else {
            return Err(it.type_error("pytype_fromspec_meta: must be invoked with a type argument!"));
        };
        if !matches!(m.kind, Kind::Type(_)) {
            return Err(it.type_error("pytype_fromspec_meta: must be invoked with a type argument!"));
        }
        if has_custom_new(it, m) {
            return Err(it.type_error("Metaclasses with custom tp_new are not supported."));
        }
        let ns = it.new_dict();
        dict_set_str(&ns, "__module__", Value::str("_testcapi"));
        let args = vec![Value::str("HeapCTypeViaMetaclass"), Value::tuple(Vec::new()), Value::Obj(ns)];
        it.type_new_from_args(m.clone(), &args, Vec::new())
    }

    /// test_from_spec_metatype_inheritance(): a type made from a class of a metaclass inherits the metaclass.
    #[op]
    fn test_from_spec_metatype_inheritance(it: &mut Interp) -> R<()> {
        let type_ = it.types.type_.clone();
        let meta = make_class(it, &type_, "MinimalMetaclass", "_testcapi", &[type_.clone()], None)?;
        let class = make_class(it, &meta, "TestClass", "_testcapi", &[], None)?;
        let new = make_class(it, &meta, "MinimalSpecType", "_testcapi", &[class.clone()], None)?;
        let new_meta = it.type_of_obj(&new);
        if !std::rc::Rc::ptr_eq(&new_meta, &meta) {
            return Err(it.new_exc_str("AssertionError", "Metaclass not set properly!"));
        }
        let subs = it.call_method(&Value::Obj(class), "__subclasses__", Vec::new())?;
        let found = it.iterate_to_vec(&subs)?.iter().any(|s| matches!(s, Value::Obj(o) if std::rc::Rc::ptr_eq(o, &new)));
        if !found {
            return Err(it.new_exc_str("AssertionError", "subclasses not set properly!"));
        }
        Ok(())
    }

    /// test_from_spec_invalid_metatype_inheritance(): bases with unrelated metaclasses conflict.
    #[op]
    fn test_from_spec_invalid_metatype_inheritance(it: &mut Interp) -> R<()> {
        let type_ = it.types.type_.clone();
        let meta_a = make_class(it, &type_, "MinimalMetaclass", "_testcapi", &[type_.clone()], None)?;
        let meta_b = make_class(it, &type_, "MinimalMetaclass", "_testcapi", &[type_.clone()], None)?;
        let class_a = make_class(it, &meta_a, "TestClassA", "_testcapi", &[], None)?;
        let class_b = make_class(it, &meta_b, "TestClassB", "_testcapi", &[], None)?;
        match make_class(it, &type_, "MinimalSpecType", "_testcapi", &[class_a, class_b], None) {
            Ok(_) => Err(it.new_exc_str("AssertionError", "MetaType conflict not recognized by PyType_FromSpecWithBases")),
            Err(e) => {
                let te = it.exc_type("TypeError");
                if !it.is_subtype(&it.type_of_obj(&e), &te) {
                    return Err(e);
                }
                let msg = it.str_of(&Value::Obj(e))?;
                if msg.contains("metaclass conflict:") {
                    Ok(())
                } else {
                    Err(it.new_exc_str("AssertionError", "TypeError did not include expected message."))
                }
            }
        }
    }

    /// test_type_from_ephemeral_spec(): a type keeps its name, doc and slots without its spec.
    #[op]
    fn test_type_from_ephemeral_spec(it: &mut Interp) -> R<()> {
        let object = it.types.object.clone();
        let type_ = it.types.type_.clone();
        let ns = it.new_dict();
        dict_set_str(&ns, "__module__", Value::str("testcapi"));
        dict_set_str(&ns, "__doc__", Value::str("a test class"));
        let str_fn = call_builtin(it, "eval", vec![Value::str("lambda self: '<test>'")])?;
        dict_set_str(&ns, "__str__", str_fn);
        let args = vec![Value::str("_Test"), Value::tuple(vec![Value::Obj(object)]), Value::Obj(ns)];
        let Value::Obj(class) = it.type_new_from_args(type_, &args, Vec::new())? else { return Ok(()) };
        let instance = it.call(&Value::Obj(class), Vec::new(), Vec::new())?;
        let s = it.str_of(&instance)?;
        if s != "<test>" {
            return Err(it.new_exc_str("AssertionError", "unexpected __str__"));
        }
        Ok(())
    }

    /// create_type_from_repeated_slots(variant): a spec with a repeated slot is rejected.
    #[op]
    fn create_type_from_repeated_slots(it: &mut Interp, variant: i64) -> R<Value> {
        match variant {
            0 => Err(it.new_exc_str("SystemError", "Py_tp_doc slot was specified multiple times")),
            1 => Err(it.new_exc_str("SystemError", "Py_tp_members slot was specified multiple times")),
            _ => Err(it.value_error("bad test variant")),
        }
    }

    /// make_immutable_type_with_base(base): an immutable subclass of `base`.
    #[op]
    fn make_immutable_type_with_base(it: &mut Interp, base: &Value) -> R<Value> {
        let b = must_be_type(it, base)?;
        if type_flag_bits(&b) & TF_IMMUTABLE == 0 {
            let n = it.tp_name(&b);
            let msg = format!("Creating immutable type ImmutableSubclass from mutable base {n} is deprecated and will be disabled in Python 3.14.");
            crate::builtins::warningsm::warn_category(it, "DeprecationWarning", &msg, 1)?;
        }
        let meta = it.type_of_obj(&b);
        let t = make_class(it, &meta, "ImmutableSubclass", "builtins", &[b], None)?;
        set_type_flags(&t, TF_IMMUTABLE, 0);
        Ok(Value::Obj(t))
    }

    /// make_type_with_base(base): a subclass of `base` named `_testcapi.Subclass`.
    #[op]
    fn make_type_with_base(it: &mut Interp, base: &Value) -> R<Value> {
        let b = must_be_type(it, base)?;
        let meta = it.type_of_obj(&b);
        if has_custom_new(it, &meta) {
            let msg = "Type _testcapi.Subclass uses PyType_Spec with a metaclass that has custom tp_new. This is deprecated and will no longer be allowed in Python 3.14.";
            crate::builtins::warningsm::warn_category(it, "DeprecationWarning", msg, 1)?;
        }
        let t = make_class(it, &meta, "Subclass", "_testcapi", &[b], None)?;
        Ok(Value::Obj(t))
    }

    // ---- type version tags, bases and classes ---------------------------------------------------

    /// type_get_version(type): the type's version tag, 0 when none is assigned.
    #[op]
    fn type_get_version(it: &mut Interp, ty: &Value) -> R<i64> {
        let t = must_be_type(it, ty)?;
        match &t.kind {
            Kind::Type(td) => Ok(i64::from(watch::version_of(td))),
            _ => Ok(0),
        }
    }

    /// type_modified(type): invalidate the type and its subclasses.
    #[op]
    fn type_modified(it: &mut Interp, ty: &Value) -> R<()> {
        let t = must_be_type(it, ty)?;
        it.type_modified(&t);
        Ok(())
    }

    /// type_assign_version(type): give the type a version tag; 1 on success.
    #[op]
    fn type_assign_version(it: &mut Interp, ty: &Value) -> R<i64> {
        let t = must_be_type(it, ty)?;
        match &t.kind {
            Kind::Type(td) => Ok(i64::from(watch::assign_version(td) != 0)),
            _ => Ok(0),
        }
    }

    /// type_assign_specific_version_unsafe(type, version)
    #[op(hint(py(arg_style = "parse", arg_name = "type_assign_specific_version_unsafe")))]
    fn type_assign_specific_version_unsafe(it: &mut Interp, ty: &Value, version: i64) -> R<()> {
        let t = must_be_type(it, ty)?;
        if let Kind::Type(td) = &t.kind {
            watch::set_version(td, version as u32);
        }
        Ok(())
    }

    /// type_get_tp_bases(type): the `tp_bases` tuple, or None.
    #[op]
    fn type_get_tp_bases(it: &mut Interp, ty: &Value) -> R<Value> {
        let t = must_be_type(it, ty)?;
        match &t.kind {
            Kind::Type(td) => Ok(Value::tuple(td.bases.borrow().iter().cloned().map(Value::Obj).collect())),
            _ => Ok(Value::None),
        }
    }

    /// type_get_tp_mro(type): the `tp_mro` tuple, or None.
    #[op]
    fn type_get_tp_mro(it: &mut Interp, ty: &Value) -> R<Value> {
        let t = must_be_type(it, ty)?;
        match &t.kind {
            Kind::Type(td) => Ok(Value::tuple(td.mro.borrow().iter().cloned().map(Value::Obj).collect())),
            _ => Ok(Value::None),
        }
    }

    /// get_basic_static_type(base=object): one of two preallocated static types.
    #[op]
    fn get_basic_static_type(it: &mut Interp, base: Option<&Value>) -> R<Value> {
        let used = it.native_state::<StaticTypes>();
        if used.0 >= 2 {
            return Err(it.new_exc_str("RuntimeError", "no more available basic static types"));
        }
        used.0 += 1;
        let base = match base {
            Some(b) => Some(must_be_type(it, b)?),
            None => None,
        };
        Ok(Value::Obj(new_type(it, "builtins", "BasicStaticType", base.as_ref(), Layout::Other)))
    }

    /// bad_get(self, obj, cls, /): call `cls()` then return `repr(self)`.
    #[op]
    fn bad_get(it: &mut Interp, this: &Value, obj: &Value, cls: &Value) -> R<String> {
        let _ = obj;
        it.call(cls, Vec::new(), Vec::new())?;
        it.repr_of(this)
    }

    /// create_cfunction(): a builtin function bound to the module.
    #[op]
    fn create_cfunction(it: &mut Interp) -> R<Value> {
        let m = testcapi(it);
        it.get_attr_str(&m, "create_cfunction")
    }

    /// without_gc(type): the heap type, without the garbage-collection flag.
    #[op]
    fn without_gc(it: &mut Interp, ty: &Value) -> R<Value> {
        heap_type_arg(it, ty)?;
        Ok(ty.clone())
    }

    /// with_tp_del(type): the heap type, with `__tp_del__` called as its finalizer.
    #[op(hint(py(arg_style = "parse", arg_name = "with_tp_del")))]
    fn with_tp_del(it: &mut Interp, ty: &Value) -> R<Value> {
        let t = heap_type_arg(it, ty)?;
        crate::bind::install_into::<TpDel>(&t, &["__del__"]);
        Ok(ty.clone())
    }

    /// clear_managed_dict(obj): drop the instance dictionary.
    #[op]
    fn clear_managed_dict(obj: &Value) {
        if let Value::Obj(o) = obj {
            *o.dict.borrow_mut() = None;
        }
    }

    #[class(module = "_testcapi", name = "TpDel", hint(py(final)))]
    pub struct TpDel;

    #[methods]
    impl TpDel {
        #[method(name = "__del__")]
        fn del(slf: This<Value>, it: &mut Interp) -> R<()> {
            let cls = it.type_of(&slf.0);
            let Some(f) = it.lookup_mro(&cls, "__tp_del__") else { return Ok(()) };
            it.call(&f, vec![slf.0], Vec::new())?;
            Ok(())
        }
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let both = TF_METHOD_DESCRIPTOR | TF_VECTORCALL;
        for (t, flags) in [
            (type_object::<MethodDescriptorBase>(it), both),
            (type_object::<MethodDescriptorDerived>(it), both),
            (type_object::<MethodDescriptorNopGet>(it), 0),
            (type_object::<MethodDescriptor2>(it), both),
        ] {
            set_type_flags(&t, flags, 0);
        }
        let weak = type_object::<HeapCTypeWithWeakref>(it);
        let weak2 = type_object::<HeapCTypeWithWeakref2>(it);
        for t in [&weak, &weak2] {
            if let Some(td) = t.dict.borrow().as_ref() {
                dict_set_str(td, "__weaklistoffset__", Value::Int(16));
                dict_set_str(td, "__dictoffset__", Value::Int(0));
            }
        }
        for (name, offset) in [("HeapCTypeWithDict", 16), ("HeapCTypeWithDict2", 16), ("HeapCTypeWithNegativeDict", -8)] {
            if let Some(Value::Obj(t)) = dict_get_str(&d, name) {
                if let Some(td) = t.dict.borrow().as_ref() {
                    dict_set_str(td, "__dictoffset__", Value::Int(offset));
                }
            }
        }
        let type_ = it.types.type_.clone();
        let list = it.types.list.clone();
        for (name, base, flags) in [
            ("HeapCTypeMetaclass", &type_, 0),
            ("HeapCTypeMetaclassCustomNew", &type_, 0),
            ("HeapCTypeMetaclassNullNew", &type_, 0),
            ("MyList", &list, 0),
        ] {
            let module = if name == "MyList" { "builtins" } else { "_testcapi" };
            let Ok(t) = make_class(it, &type_, name, module, &[base.clone()], None) else { continue };
            set_type_flags(&t, flags, 0);
            dict_set_str(&d, name, Value::Obj(t));
        }
        if let Some(Value::Obj(t)) = dict_get_str(&d, "HeapCTypeMetaclassNullNew") {
            install_all::<NullNew>(&t);
        }
        if let Some(Value::Obj(t)) = dict_get_str(&d, "HeapCTypeMetaclassCustomNew") {
            install_all::<CustomNew>(&t);
        }
        for hidden in ["HeapCTypeMetaclassNullNewMarker", "HeapCTypeMetaclassCustomNewMarker", "TpDel", "VectorcallClass"] {
            crate::vm::dict_del_str(&d, hidden);
        }
    }
}

#[derive(Default)]
struct StaticTypes(usize);
