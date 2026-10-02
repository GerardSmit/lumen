//! `_typing`: the native core of `typing` (CPython's `Objects/typevarobject.c` and
//! `Modules/_typingmodule.c`): `TypeVar`, `ParamSpec`, `ParamSpecArgs`, `ParamSpecKwargs`,
//! `TypeVarTuple`, `TypeAliasType`, `Generic` and `_idfunc`, plus the intrinsics the compiler
//! emits for PEP 695 syntax. Like CPython, the heavier logic (type checks, substitution,
//! `Generic[...]`) is delegated to private helpers in `typing.py`.

use super::native::*;
use crate::bytecode::*;
use crate::object::*;
use crate::vm::*;

pub struct TypeVarData {
    name: Value,
    bound: Option<Value>,
    evaluate_bound: Option<Value>,
    constraints: Option<Value>,
    evaluate_constraints: Option<Value>,
    covariant: bool,
    contravariant: bool,
    infer_variance: bool,
}

pub struct ParamSpecData {
    name: Value,
    bound: Option<Value>,
    covariant: bool,
    contravariant: bool,
    infer_variance: bool,
}

pub struct TypeVarTupleData {
    name: Value,
}

struct ParamSpecAttr {
    origin: Value,
}

pub struct TypeAliasData {
    name: Value,
    type_params: Option<Value>,
    compute_value: Option<Value>,
    value: Option<Value>,
    module: Option<Value>,
}

pub fn is_type_alias(v: &Value) -> bool {
    with_opaque::<TypeAliasData, _>(v, |_| ()).is_some()
}

fn typing_dict(it: &mut Interp) -> R<Obj> {
    let m = it.import_module("_typing")?;
    Ok(it.module_dict(&m))
}

fn typing_type(it: &mut Interp, name: &str) -> R<Obj> {
    let d = typing_dict(it)?;
    match dict_get_str(&d, name) {
        Some(Value::Obj(t)) => Ok(t),
        _ => Err(it.new_exc_str("SystemError", &format!("Cannot find {name} type"))),
    }
}

/// Calls `typing.<name>(*args, **kw)`.
fn call_typing(it: &mut Interp, name: &str, args: Vec<Value>, kw: Vec<(Obj, Value)>) -> R<Value> {
    let m = it.import_module("typing")?;
    let f = it.get_attr_str(&Value::Obj(m), name)?;
    it.call(&f, args, kw)
}

/// `typing._type_check(arg, msg)`, with `None` meaning `type(None)` as in CPython's C helper.
fn type_check(it: &mut Interp, arg: &Value, msg: &str) -> R<Value> {
    if arg.is_none() {
        return Ok(Value::Obj(it.types.none_type.clone()));
    }
    call_typing(it, "_type_check", vec![arg.clone(), Value::str(msg)], Vec::new())
}

/// The `__name__` of the module whose code is running (CPython's `caller()`).
fn caller_module(it: &mut Interp) -> Value {
    match it.frames.last() {
        Some(f) => dict_get_str(&f.globals, "__name__").unwrap_or(Value::None),
        None => Value::None,
    }
}

fn set_module(it: &mut Interp, v: &Value, module: Value) {
    if let Value::Obj(o) = v {
        let d = it.instance_dict(o);
        dict_set_str(&d, "__module__", module);
    }
}

fn name_arg(it: &mut Interp, fname: &str, v: &Value) -> R<Value> {
    if v.as_str().is_none() {
        let t = it.type_name_of(v);
        return Err(it.type_error(&format!("{fname}() argument 'name' must be str, not {t}")));
    }
    Ok(v.clone())
}

fn variance_checks(it: &mut Interp, covariant: bool, contravariant: bool, infer_variance: bool) -> R<()> {
    if covariant && contravariant {
        return Err(it.value_error("Bivariant types are not supported."));
    }
    if infer_variance && (covariant || contravariant) {
        return Err(it.value_error("Variance cannot be specified with infer_variance."));
    }
    Ok(())
}

fn variance_repr(name: &Value, covariant: bool, contravariant: bool, infer_variance: bool) -> Value {
    let n = name.as_str().unwrap_or("");
    if infer_variance {
        return Value::str(n);
    }
    let prefix = if covariant {
        '+'
    } else if contravariant {
        '-'
    } else {
        '~'
    };
    Value::string(format!("{prefix}{n}"))
}

/// Replaces each `TypeVarTuple` in `params` with `Unpack[tvt]`.
fn unpack_typevartuples(it: &mut Interp, params: &Value) -> R<Value> {
    let Some(items) = params.tuple_items() else { return Ok(params.clone()) };
    if !items.iter().any(is_typevartuple) {
        return Ok(params.clone());
    }
    let items = items.to_vec();
    let mut out = Vec::with_capacity(items.len());
    for p in items {
        out.push(if is_typevartuple(&p) { unpack(it, &p)? } else { p });
    }
    Ok(Value::tuple(out))
}

fn is_typevartuple(v: &Value) -> bool {
    with_opaque::<TypeVarTupleData, _>(v, |_| ()).is_some()
}

fn unpack(it: &mut Interp, tvt: &Value) -> R<Value> {
    let m = it.import_module("typing")?;
    let u = it.get_attr_str(&Value::Obj(m), "Unpack")?;
    it.getitem(&u, tvt)
}

fn bool_kw(it: &mut Interp, v: &Option<Value>) -> R<bool> {
    match v {
        Some(v) => it.truthy(v),
        None => Ok(false),
    }
}

fn noop_init(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::None)
}

fn idfunc(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("_idfunc", kw)?;
    it.check_args("_idfunc", a, 1, 1)?;
    Ok(a[0].clone())
}

fn set_final(t: &Obj) {
    if let Kind::Type(td) = &t.kind {
        td.flags.set(td.flags.get() | TF_FINAL);
    }
}

// ---- TypeVar ----

fn new_typevar(it: &mut Interp, data: TypeVarData) -> R<Value> {
    let ty = typing_type(it, "TypeVar")?;
    Ok(new_opaque(&ty, data))
}

fn tv<X>(it: &mut Interp, v: &Value, f: impl FnOnce(&mut TypeVarData) -> X) -> R<X> {
    with_opaque::<TypeVarData, _>(v, f).ok_or_else(|| it.self_state_err("typing.TypeVar"))
}

fn typevar_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let rest = a.get(1..).unwrap_or(&[]);
    let mut name = rest.first().cloned();
    let constraints: Vec<Value> = rest.iter().skip(1).cloned().collect();
    let mut opts: [Option<Value>; 4] = Default::default();
    const NAMES: [&str; 4] = ["bound", "covariant", "contravariant", "infer_variance"];
    for (k, v) in kw {
        let kn = k.as_str_kind().unwrap_or("");
        if kn == "name" && name.is_none() {
            name = Some(v.clone());
            continue;
        }
        match NAMES.iter().position(|n| *n == kn) {
            Some(i) => opts[i] = Some(v.clone()),
            None => return Err(it.type_error(&format!("typevar() got an unexpected keyword argument '{kn}'"))),
        }
    }
    let Some(name) = name else {
        return Err(it.type_error("typevar() missing required argument 'name' (pos 1)"));
    };
    let name = name_arg(it, "typevar", &name)?;
    let covariant = bool_kw(it, &opts[1])?;
    let contravariant = bool_kw(it, &opts[2])?;
    let infer_variance = bool_kw(it, &opts[3])?;
    variance_checks(it, covariant, contravariant, infer_variance)?;
    let bound = match &opts[0] {
        Some(b) if !b.is_none() => Some(type_check(it, b, "Bound must be a type.")?),
        _ => None,
    };
    let constraints = match constraints.len() {
        0 => None,
        1 => return Err(it.type_error("A single constraint is not allowed")),
        _ if bound.is_some() => return Err(it.type_error("Constraints cannot be combined with bound=...")),
        _ => Some(Value::tuple(constraints)),
    };
    let module = caller_module(it);
    let v = new_typevar(
        it,
        TypeVarData {
            name,
            bound,
            evaluate_bound: None,
            constraints,
            evaluate_constraints: None,
            covariant,
            contravariant,
            infer_variance,
        },
    )?;
    set_module(it, &v, module);
    Ok(v)
}

fn typevar_name(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    tv(it, &a[0], |d| d.name.clone())
}

fn typevar_bound(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let (bound, eval) = tv(it, &a[0], |d| (d.bound.clone(), d.evaluate_bound.clone()))?;
    if let Some(b) = bound {
        return Ok(b);
    }
    let Some(f) = eval else { return Ok(Value::None) };
    let b = it.call(&f, Vec::new(), Vec::new())?;
    tv(it, &a[0], |d| d.bound = Some(b.clone()))?;
    Ok(b)
}

fn typevar_constraints(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let (cons, eval) = tv(it, &a[0], |d| (d.constraints.clone(), d.evaluate_constraints.clone()))?;
    if let Some(c) = cons {
        return Ok(c);
    }
    let Some(f) = eval else { return Ok(Value::tuple(Vec::new())) };
    let c = it.call(&f, Vec::new(), Vec::new())?;
    tv(it, &a[0], |d| d.constraints = Some(c.clone()))?;
    Ok(c)
}

fn typevar_covariant(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    tv(it, &a[0], |d| Value::Bool(d.covariant))
}

fn typevar_contravariant(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    tv(it, &a[0], |d| Value::Bool(d.contravariant))
}

fn typevar_infer_variance(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    tv(it, &a[0], |d| Value::Bool(d.infer_variance))
}

fn typevar_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    tv(it, &a[0], |d| variance_repr(&d.name, d.covariant, d.contravariant, d.infer_variance))
}

fn typevar_subst(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__typing_subst__", a, 2, 2)?;
    tv(it, &a[0], |_| ())?;
    call_typing(it, "_typevar_subst", a.to_vec(), Vec::new())
}

fn typevar_reduce(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    tv(it, &a[0], |d| d.name.clone())
}

fn typevar_mro_entries(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Err(it.type_error("Cannot subclass an instance of TypeVar"))
}

fn make_union_or(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__or__", a, 2, 2)?;
    call_typing(it, "_make_union", vec![a[0].clone(), a[1].clone()], Vec::new())
}

fn make_union_ror(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__ror__", a, 2, 2)?;
    call_typing(it, "_make_union", vec![a[1].clone(), a[0].clone()], Vec::new())
}

// ---- ParamSpec ----

fn ps<X>(it: &mut Interp, v: &Value, f: impl FnOnce(&mut ParamSpecData) -> X) -> R<X> {
    with_opaque::<ParamSpecData, _>(v, f).ok_or_else(|| it.self_state_err("typing.ParamSpec"))
}

fn new_paramspec(it: &mut Interp, data: ParamSpecData) -> R<Value> {
    let ty = typing_type(it, "ParamSpec")?;
    Ok(new_opaque(&ty, data))
}

fn paramspec_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("paramspec", a.get(1..).unwrap_or(&[]), kw, &["name", "bound", "covariant", "contravariant", "infer_variance"], 1)?;
    if a.len() > 2 {
        return Err(it.type_error(&format!("paramspec() takes exactly 1 positional argument ({} given)", a.len() - 1)));
    }
    let name = name_arg(it, "paramspec", b[0].as_ref().unwrap_or(&Value::None))?;
    let covariant = bool_kw(it, &b[2])?;
    let contravariant = bool_kw(it, &b[3])?;
    let infer_variance = bool_kw(it, &b[4])?;
    variance_checks(it, covariant, contravariant, infer_variance)?;
    let bound = type_check(it, b[1].as_ref().unwrap_or(&Value::None), "Bound must be a type.")?;
    let module = caller_module(it);
    let v = new_paramspec(it, ParamSpecData { name, bound: Some(bound), covariant, contravariant, infer_variance })?;
    set_module(it, &v, module);
    Ok(v)
}

fn paramspec_name(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    ps(it, &a[0], |d| d.name.clone())
}

fn paramspec_bound(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    ps(it, &a[0], |d| d.bound.clone().unwrap_or(Value::None))
}

fn paramspec_covariant(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    ps(it, &a[0], |d| Value::Bool(d.covariant))
}

fn paramspec_contravariant(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    ps(it, &a[0], |d| Value::Bool(d.contravariant))
}

fn paramspec_infer_variance(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    ps(it, &a[0], |d| Value::Bool(d.infer_variance))
}

fn paramspec_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    ps(it, &a[0], |d| variance_repr(&d.name, d.covariant, d.contravariant, d.infer_variance))
}

fn paramspec_args(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    ps(it, &a[0], |_| ())?;
    let ty = typing_type(it, "ParamSpecArgs")?;
    Ok(new_opaque(&ty, ParamSpecAttr { origin: a[0].clone() }))
}

fn paramspec_kwargs(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    ps(it, &a[0], |_| ())?;
    let ty = typing_type(it, "ParamSpecKwargs")?;
    Ok(new_opaque(&ty, ParamSpecAttr { origin: a[0].clone() }))
}

fn paramspec_subst(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__typing_subst__", a, 2, 2)?;
    ps(it, &a[0], |_| ())?;
    call_typing(it, "_paramspec_subst", a.to_vec(), Vec::new())
}

fn paramspec_prepare_subst(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__typing_prepare_subst__", a, 3, 3)?;
    ps(it, &a[0], |_| ())?;
    call_typing(it, "_paramspec_prepare_subst", a.to_vec(), Vec::new())
}

fn paramspec_reduce(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    ps(it, &a[0], |d| d.name.clone())
}

fn paramspec_mro_entries(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Err(it.type_error("Cannot subclass an instance of ParamSpec"))
}

// ---- ParamSpecArgs / ParamSpecKwargs ----

fn psattr_origin(it: &mut Interp, v: &Value) -> R<Value> {
    with_opaque::<ParamSpecAttr, _>(v, |d| d.origin.clone()).ok_or_else(|| it.self_state_err("typing.ParamSpecArgs"))
}

fn psattr_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("paramspecargs", a.get(1..).unwrap_or(&[]), kw, &["origin"], 1)?;
    let Some(Value::Obj(cls)) = a.first() else { return Err(it.type_error("__new__ needs a type")) };
    Ok(new_opaque(cls, ParamSpecAttr { origin: b[0].clone().unwrap_or(Value::None) }))
}

fn psattr_origin_get(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    psattr_origin(it, &a[0])
}

fn psattr_repr(it: &mut Interp, a: &[Value], suffix: &str) -> R<Value> {
    let origin = psattr_origin(it, &a[0])?;
    let base = match with_opaque::<ParamSpecData, _>(&origin, |d| d.name.clone()) {
        Some(n) => n.as_str().unwrap_or("").to_string(),
        None => it.repr_of(&origin)?,
    };
    Ok(Value::string(format!("{base}.{suffix}")))
}

fn psargs_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    psattr_repr(it, a, "args")
}

fn pskwargs_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    psattr_repr(it, a, "kwargs")
}

fn psattr_cmp(it: &mut Interp, a: &[Value], negate: bool) -> R<Value> {
    it.check_args("__eq__", a, 2, 2)?;
    let (t1, t2) = (it.type_of(&a[0]), it.type_of(&a[1]));
    if !std::rc::Rc::ptr_eq(&t1, &t2) {
        return Ok(Value::NotImplemented);
    }
    let o1 = psattr_origin(it, &a[0])?;
    let o2 = psattr_origin(it, &a[1])?;
    let eq = it.values_eq(&o1, &o2)?;
    Ok(Value::Bool(eq != negate))
}

fn psattr_eq(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    psattr_cmp(it, a, false)
}

fn psattr_ne(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    psattr_cmp(it, a, true)
}

fn psargs_mro_entries(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Err(it.type_error("Cannot subclass an instance of ParamSpecArgs"))
}

fn pskwargs_mro_entries(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Err(it.type_error("Cannot subclass an instance of ParamSpecKwargs"))
}

// ---- TypeVarTuple ----

fn tvt_name_of(it: &mut Interp, v: &Value) -> R<Value> {
    with_opaque::<TypeVarTupleData, _>(v, |d| d.name.clone()).ok_or_else(|| it.self_state_err("typing.TypeVarTuple"))
}

fn new_typevartuple(it: &mut Interp, name: Value) -> R<Value> {
    let ty = typing_type(it, "TypeVarTuple")?;
    Ok(new_opaque(&ty, TypeVarTupleData { name }))
}

fn tvt_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("typevartuple", a.get(1..).unwrap_or(&[]), kw, &["name"], 1)?;
    let name = name_arg(it, "typevartuple", b[0].as_ref().unwrap_or(&Value::None))?;
    let module = caller_module(it);
    let v = new_typevartuple(it, name)?;
    set_module(it, &v, module);
    Ok(v)
}

fn tvt_name(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    tvt_name_of(it, &a[0])
}

fn tvt_iter(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    tvt_name_of(it, &a[0])?;
    let u = unpack(it, &a[0])?;
    it.native_get_iter(&Value::tuple(vec![u]))
}

fn tvt_subst(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Err(it.type_error("Substitution of bare TypeVarTuple is not supported"))
}

fn tvt_prepare_subst(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__typing_prepare_subst__", a, 3, 3)?;
    tvt_name_of(it, &a[0])?;
    call_typing(it, "_typevartuple_prepare_subst", a.to_vec(), Vec::new())
}

fn tvt_mro_entries(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Err(it.type_error("Cannot subclass an instance of TypeVarTuple"))
}

// ---- TypeAliasType ----

fn ta<X>(it: &mut Interp, v: &Value, f: impl FnOnce(&mut TypeAliasData) -> X) -> R<X> {
    with_opaque::<TypeAliasData, _>(v, f).ok_or_else(|| it.self_state_err("typing.TypeAliasType"))
}

fn new_typealias(it: &mut Interp, data: TypeAliasData) -> R<Value> {
    let ty = typing_type(it, "TypeAliasType")?;
    let module = match (&data.module, &data.compute_value) {
        (Some(m), _) => Some(m.clone()),
        (None, Some(f)) => Some(it.get_attr_str(f, "__module__")?),
        (None, None) => None,
    };
    let v = new_opaque(&ty, data);
    if let Some(m) = module {
        set_module(it, &v, m);
    }
    Ok(v)
}

fn typealias_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let pos = a.get(1..).unwrap_or(&[]);
    if pos.len() > 2 {
        return Err(it.type_error(&format!("typealias() takes exactly 2 positional arguments ({} given)", pos.len())));
    }
    let b = it.bind_args("typealias", pos, kw, &["name", "value", "type_params"], 2)?;
    let name = name_arg(it, "typealias", b[0].as_ref().unwrap_or(&Value::None))?;
    let type_params = match &b[2] {
        None | Some(Value::None) => None,
        Some(tp) if tp.tuple_items().is_some() => Some(tp.clone()),
        Some(_) => return Err(it.type_error("type_params must be a tuple")),
    };
    let module = caller_module(it);
    new_typealias(
        it,
        TypeAliasData {
            name,
            type_params,
            compute_value: None,
            value: b[1].clone(),
            module: Some(module),
        },
    )
}

fn typealias_name(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    ta(it, &a[0], |d| d.name.clone())
}

fn typealias_value(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let (value, compute) = ta(it, &a[0], |d| (d.value.clone(), d.compute_value.clone()))?;
    if let Some(v) = value {
        return Ok(v);
    }
    let Some(f) = compute else { return Ok(Value::None) };
    let v = it.call(&f, Vec::new(), Vec::new())?;
    ta(it, &a[0], |d| d.value = Some(v.clone()))?;
    Ok(v)
}

fn typealias_type_params(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    ta(it, &a[0], |d| d.type_params.clone().unwrap_or_else(|| Value::tuple(Vec::new())))
}

fn typealias_parameters(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    match ta(it, &a[0], |d| d.type_params.clone())? {
        Some(tp) => unpack_typevartuples(it, &tp),
        None => Ok(Value::tuple(Vec::new())),
    }
}

fn typealias_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    ta(it, &a[0], |d| d.name.clone())
}

fn typealias_getitem(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__getitem__", a, 2, 2)?;
    if ta(it, &a[0], |d| d.type_params.is_none())? {
        return Err(it.type_error("Only generic type aliases are subscriptable"));
    }
    Ok(it.make_alias(a[0].clone(), &a[1]))
}

fn typealias_or(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__or__", a, 2, 2)?;
    Ok(it.union_binop(&a[0], &a[1])?.unwrap_or(Value::NotImplemented))
}

fn typealias_ror(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__ror__", a, 2, 2)?;
    Ok(it.union_binop(&a[1], &a[0])?.unwrap_or(Value::NotImplemented))
}

fn typealias_reduce(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    ta(it, &a[0], |d| d.name.clone())
}

fn typealias_setattr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__setattr__", a, 3, 3)?;
    let name = a[1].as_str().unwrap_or("").to_string();
    let msg = match name.as_str() {
        "__name__" | "__type_params__" => "readonly attribute".to_string(),
        "__value__" | "__parameters__" | "__module__" => {
            format!("attribute '{name}' of 'typing.TypeAliasType' objects is not writable")
        }
        _ => format!("'typing.TypeAliasType' object has no attribute '{name}'"),
    };
    Err(it.new_exc_str("AttributeError", &msg))
}

// ---- Generic ----

fn generic_class_getitem(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__class_getitem__", a, 2, 2)?;
    call_typing(it, "_generic_class_getitem", a.to_vec(), Vec::new())
}

fn generic_init_subclass(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    call_typing(it, "_generic_init_subclass", a.to_vec(), kw.to_vec())
}

// ---- intrinsics ----

pub fn intrinsic1(it: &mut Interp, k: u32, v: Value) -> R<Value> {
    match k {
        INTRINSIC1_TYPEVAR => new_typevar(
            it,
            TypeVarData {
                name: v,
                bound: None,
                evaluate_bound: None,
                constraints: None,
                evaluate_constraints: None,
                covariant: false,
                contravariant: false,
                infer_variance: true,
            },
        ),
        INTRINSIC1_PARAMSPEC => new_paramspec(
            it,
            ParamSpecData { name: v, bound: None, covariant: false, contravariant: false, infer_variance: true },
        ),
        INTRINSIC1_TYPEVARTUPLE => new_typevartuple(it, v),
        INTRINSIC1_SUBSCRIPT_GENERIC => {
            let params = unpack_typevartuples(it, &v)?;
            let generic = typing_type(it, "Generic")?;
            call_typing(it, "_GenericAlias", vec![Value::Obj(generic), params], Vec::new())
        }
        INTRINSIC1_TYPEALIAS => {
            let items = v.tuple_items().map(|t| t.to_vec()).unwrap_or_default();
            let [name, type_params, compute] = <[Value; 3]>::try_from(items)
                .map_err(|_| it.new_exc_str("SystemError", "bad type alias intrinsic argument"))?;
            new_typealias(
                it,
                TypeAliasData {
                    name,
                    type_params: (!type_params.is_none()).then_some(type_params),
                    compute_value: Some(compute),
                    value: None,
                    module: None,
                },
            )
        }
        _ => Err(it.new_exc_str("SystemError", "unknown intrinsic")),
    }
}

pub fn intrinsic2(it: &mut Interp, k: u32, a: Value, b: Value) -> R<Value> {
    match k {
        INTRINSIC2_TYPEVAR_WITH_BOUND | INTRINSIC2_TYPEVAR_WITH_CONSTRAINTS => {
            let constraints = k == INTRINSIC2_TYPEVAR_WITH_CONSTRAINTS;
            new_typevar(
                it,
                TypeVarData {
                    name: a,
                    bound: None,
                    evaluate_bound: (!constraints).then(|| b.clone()),
                    constraints: None,
                    evaluate_constraints: constraints.then_some(b),
                    covariant: false,
                    contravariant: false,
                    infer_variance: true,
                },
            )
        }
        INTRINSIC2_SET_FUNCTION_TYPE_PARAMS => {
            if let Value::Obj(f) = &a {
                if let Kind::Function(func) = &f.kind {
                    *func.type_params.borrow_mut() = Some(b);
                }
            }
            Ok(a)
        }
        _ => Err(it.new_exc_str("SystemError", "unknown intrinsic")),
    }
}

impl Interp {
    /// `mapping[key]`, or `None` when the key is missing (`KeyError`).
    pub fn mapping_lookup(&mut self, mapping: &Value, key: &Obj) -> R<Option<Value>> {
        if let Value::Obj(o) = mapping {
            if matches!(o.kind, Kind::Dict(_)) && o.cls.is_none() {
                return Ok(dict_get_name(o, key));
            }
        }
        match self.getitem(mapping, &Value::Obj(key.clone())) {
            Ok(v) => Ok(Some(v)),
            Err(e) if self.exc_is(&e, "KeyError") => Ok(None),
            Err(e) => Err(e),
        }
    }
}

pub fn make(it: &mut Interp) -> Obj {
    let m = it.new_module("_typing");
    let d = it.module_dict(&m);
    it.register_module("_typing", &m);
    set_fn(it, &d, "_idfunc", idfunc);

    let typevar = new_type(it, "typing", "TypeVar", None, Layout::Other);
    it.reg_new(&typevar, typevar_new);
    let methods: &[(&'static str, NativeFn)] = &[
        ("__init__", noop_init),
        ("__repr__", typevar_repr),
        ("__typing_subst__", typevar_subst),
        ("__reduce__", typevar_reduce),
        ("__mro_entries__", typevar_mro_entries),
        ("__or__", make_union_or),
        ("__ror__", make_union_ror),
    ];
    for (n, f) in methods {
        it.reg(&typevar, n, *f);
    }
    for (n, f) in [
        ("__name__", typevar_name as NativeFn),
        ("__bound__", typevar_bound),
        ("__constraints__", typevar_constraints),
        ("__covariant__", typevar_covariant),
        ("__contravariant__", typevar_contravariant),
        ("__infer_variance__", typevar_infer_variance),
    ] {
        it.reg_prop(&typevar, n, f);
    }
    set_final(&typevar);
    set_type(&d, "TypeVar", &typevar);

    let paramspec = new_type(it, "typing", "ParamSpec", None, Layout::Other);
    it.reg_new(&paramspec, paramspec_new);
    let methods: &[(&'static str, NativeFn)] = &[
        ("__init__", noop_init),
        ("__repr__", paramspec_repr),
        ("__typing_subst__", paramspec_subst),
        ("__typing_prepare_subst__", paramspec_prepare_subst),
        ("__reduce__", paramspec_reduce),
        ("__mro_entries__", paramspec_mro_entries),
        ("__or__", make_union_or),
        ("__ror__", make_union_ror),
    ];
    for (n, f) in methods {
        it.reg(&paramspec, n, *f);
    }
    for (n, f) in [
        ("__name__", paramspec_name as NativeFn),
        ("__bound__", paramspec_bound),
        ("__covariant__", paramspec_covariant),
        ("__contravariant__", paramspec_contravariant),
        ("__infer_variance__", paramspec_infer_variance),
        ("args", paramspec_args),
        ("kwargs", paramspec_kwargs),
    ] {
        it.reg_prop(&paramspec, n, f);
    }
    set_final(&paramspec);
    set_type(&d, "ParamSpec", &paramspec);

    for (name, repr, mro) in [
        ("ParamSpecArgs", psargs_repr as NativeFn, psargs_mro_entries as NativeFn),
        ("ParamSpecKwargs", pskwargs_repr, pskwargs_mro_entries),
    ] {
        let ty = new_type(it, "typing", name, None, Layout::Other);
        it.reg_new(&ty, psattr_new);
        it.reg(&ty, "__init__", noop_init);
        it.reg(&ty, "__repr__", repr);
        it.reg(&ty, "__eq__", psattr_eq);
        it.reg(&ty, "__ne__", psattr_ne);
        it.reg(&ty, "__mro_entries__", mro);
        it.reg_prop(&ty, "__origin__", psattr_origin_get);
        if let Some(td) = ty.dict.borrow().as_ref() {
            dict_set_str(td, "__hash__", Value::None);
        }
        set_final(&ty);
        set_type(&d, name, &ty);
    }

    let tvt = new_type(it, "typing", "TypeVarTuple", None, Layout::Other);
    it.reg_new(&tvt, tvt_new);
    let methods: &[(&'static str, NativeFn)] = &[
        ("__init__", noop_init),
        ("__repr__", tvt_name),
        ("__iter__", tvt_iter),
        ("__typing_subst__", tvt_subst),
        ("__typing_prepare_subst__", tvt_prepare_subst),
        ("__reduce__", tvt_name),
        ("__mro_entries__", tvt_mro_entries),
    ];
    for (n, f) in methods {
        it.reg(&tvt, n, *f);
    }
    it.reg_prop(&tvt, "__name__", tvt_name);
    set_final(&tvt);
    set_type(&d, "TypeVarTuple", &tvt);

    let alias = new_type(it, "typing", "TypeAliasType", None, Layout::Other);
    it.reg_new(&alias, typealias_new);
    let methods: &[(&'static str, NativeFn)] = &[
        ("__init__", noop_init),
        ("__repr__", typealias_repr),
        ("__getitem__", typealias_getitem),
        ("__or__", typealias_or),
        ("__ror__", typealias_ror),
        ("__reduce__", typealias_reduce),
        ("__setattr__", typealias_setattr),
        ("__delattr__", typealias_setattr),
    ];
    for (n, f) in methods {
        it.reg(&alias, n, *f);
    }
    for (n, f) in [
        ("__name__", typealias_name as NativeFn),
        ("__value__", typealias_value),
        ("__type_params__", typealias_type_params),
        ("__parameters__", typealias_parameters),
    ] {
        it.reg_prop(&alias, n, f);
    }
    set_final(&alias);
    set_type(&d, "TypeAliasType", &alias);

    let generic = new_type(it, "typing", "Generic", None, Layout::Object);
    it.reg_class(&generic, "__class_getitem__", generic_class_getitem);
    it.reg_class(&generic, "__init_subclass__", generic_init_subclass);
    set_type(&d, "Generic", &generic);
    m
}
