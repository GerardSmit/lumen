//! `_typing`: the native core of `typing` (CPython's `Objects/typevarobject.c` and
//! `Modules/_typingmodule.c`): `TypeVar`, `ParamSpec`, `ParamSpecArgs`, `ParamSpecKwargs`,
//! `TypeVarTuple`, `TypeAliasType`, `Generic` and `_idfunc`, plus the intrinsics the compiler
//! emits for PEP 695 syntax. Like CPython, the heavier logic (type checks, substitution,
//! `Generic[...]`) is delegated to private helpers in `typing.py`.

use super::native::{new_type, with_opaque};
use super::strm::StrRef;
use crate::bind::{type_object, KwArgs, Py, This};
use crate::bytecode::*;
use crate::object::*;
use crate::vm::*;

#[lumen_bind::class(name = "TypeVar", module = "typing", hint(py(final)))]
/// Type variable.
///
/// The preferred way to construct a type variable is via the dedicated
/// syntax for generic functions, classes, and type aliases::
///
///     class Sequence[T]:  # T is a TypeVar
///         ...
///
/// This syntax can also be used to create bound and constrained type
/// variables::
///
///     # S is a TypeVar bound to str
///     class StrSequence[S: str]:
///         ...
///
///     # A is a TypeVar constrained to str or bytes
///     class StrOrBytesSequence[A: (str, bytes)]:
///         ...
///
/// However, if desired, reusable type variables can also be constructed
/// manually, like so::
///
///    T = TypeVar('T')  # Can be anything
///    S = TypeVar('S', bound=str)  # Can be any subtype of str
///    A = TypeVar('A', str, bytes)  # Must be exactly str or bytes
///
/// Type variables exist primarily for the benefit of static type
/// checkers.  They serve as the parameters for generic types as well
/// as for generic function and type alias definitions.
///
/// The variance of type variables is inferred by type checkers when they
/// are created through the type parameter syntax and when
/// ``infer_variance=True`` is passed. Manually created type variables may
/// be explicitly marked covariant or contravariant by passing
/// ``covariant=True`` or ``contravariant=True``. By default, manually
/// created type variables are invariant. See PEP 484 and PEP 695 for more
/// details.
///
pub struct TypeVar {
    name: Value,
    bound: Option<Value>,
    evaluate_bound: Option<Value>,
    constraints: Option<Value>,
    evaluate_constraints: Option<Value>,
    covariant: bool,
    contravariant: bool,
    infer_variance: bool,
    default: Option<Value>,
    evaluate_default: Option<Value>,
}

#[lumen_bind::class(name = "ParamSpec", module = "typing", hint(py(final)))]
/// Parameter specification variable.
///
/// The preferred way to construct a parameter specification is via the
/// dedicated syntax for generic functions, classes, and type aliases,
/// where the use of '**' creates a parameter specification::
///
///     type IntFunc[**P] = Callable[P, int]
///
/// For compatibility with Python 3.11 and earlier, ParamSpec objects
/// can also be created as follows::
///
///     P = ParamSpec('P')
///
/// Parameter specification variables exist primarily for the benefit of
/// static type checkers.  They are used to forward the parameter types of
/// one callable to another callable, a pattern commonly found in
/// higher-order functions and decorators.  They are only valid when used
/// in ``Concatenate``, or as the first argument to ``Callable``, or as
/// parameters for user-defined Generics. See class Generic for more
/// information on generic types.
///
/// An example for annotating a decorator::
///
///     def add_logging[**P, T](f: Callable[P, T]) -> Callable[P, T]:
///         '''A type-safe decorator to add logging to a function.'''
///         def inner(*args: P.args, **kwargs: P.kwargs) -> T:
///             logging.info(f'{f.__name__} was called')
///             return f(*args, **kwargs)
///         return inner
///
///     @add_logging
///     def add_two(x: float, y: float) -> float:
///         '''Add two numbers together.'''
///         return x + y
///
/// Parameter specification variables can be introspected. e.g.::
///
///     >>> P = ParamSpec("P")
///     >>> P.__name__
///     'P'
///
/// Note that only parameter specification variables defined in the global
/// scope can be pickled.
///
pub struct ParamSpec {
    name: Value,
    bound: Option<Value>,
    covariant: bool,
    contravariant: bool,
    infer_variance: bool,
    default: Option<Value>,
    evaluate_default: Option<Value>,
}

#[lumen_bind::class(name = "ParamSpecArgs", module = "typing", hint(py(final, unhashable)))]
/// The args for a ParamSpec object.
///
/// Given a ParamSpec object P, P.args is an instance of ParamSpecArgs.
///
/// ParamSpecArgs objects have a reference back to their ParamSpec::
///
///     >>> P = ParamSpec("P")
///     >>> P.args.__origin__ is P
///     True
///
/// This type is meant for runtime introspection and has no special meaning
/// to static type checkers.
///
pub struct ParamSpecArgs {
    origin: Value,
}

#[lumen_bind::class(
    name = "ParamSpecKwargs",
    module = "typing",
    hint(py(final, unhashable))
)]
/// The kwargs for a ParamSpec object.
///
/// Given a ParamSpec object P, P.kwargs is an instance of ParamSpecKwargs.
///
/// ParamSpecKwargs objects have a reference back to their ParamSpec::
///
///     >>> P = ParamSpec("P")
///     >>> P.kwargs.__origin__ is P
///     True
///
/// This type is meant for runtime introspection and has no special meaning
/// to static type checkers.
///
pub struct ParamSpecKwargs {
    origin: Value,
}

#[lumen_bind::class(name = "TypeVarTuple", module = "typing", hint(py(final)))]
/// Type variable tuple. A specialized form of type variable that enables
/// variadic generics.
///
/// The preferred way to construct a type variable tuple is via the
/// dedicated syntax for generic functions, classes, and type aliases,
/// where a single '*' indicates a type variable tuple::
///
///     def move_first_element_to_last[T, *Ts](tup: tuple[T, *Ts]) -> tuple[*Ts, T]:
///         return (*tup[1:], tup[0])
///
/// For compatibility with Python 3.11 and earlier, TypeVarTuple objects
/// can also be created as follows::
///
///     Ts = TypeVarTuple('Ts')  # Can be given any name
///
/// Just as a TypeVar (type variable) is a placeholder for a single type,
/// a TypeVarTuple is a placeholder for an *arbitrary* number of types. For
/// example, if we define a generic class using a TypeVarTuple::
///
///     class C[*Ts]: ...
///
/// Then we can parameterize that class with an arbitrary number of type
/// arguments::
///
///     C[int]       # Fine
///     C[int, str]  # Also fine
///     C[()]        # Even this is fine
///
/// For more details, see PEP 646.
///
/// Note that only TypeVarTuples defined in the global scope can be
/// pickled.
///
pub struct TypeVarTuple {
    name: Value,
    default: Option<Value>,
    evaluate_default: Option<Value>,
}

#[lumen_bind::class(name = "NoDefaultType", module = "typing", hint(py(final)))]
/// The type of the NoDefault singleton.
pub struct NoDefaultType;

/// `typing.NoDefault`, the value of `__default__` of a type parameter without a default.
pub fn no_default(it: &mut Interp) -> Value {
    if let Some(v) = it.native_state::<NoDefaultSingleton>().0.clone() {
        return v;
    }
    let v = Py::new(it, NoDefaultType).into_value();
    it.native_state::<NoDefaultSingleton>().0 = Some(v.clone());
    v
}

#[derive(Default)]
struct NoDefaultSingleton(Option<Value>);

fn is_no_default(it: &mut Interp, v: &Value) -> bool {
    v.is(&no_default(it))
}

/// The `default` argument of a type parameter constructor: `None` for `NoDefault`.
fn default_arg(it: &mut Interp, default: Option<&Value>) -> Option<Value> {
    default.filter(|d| !is_no_default(it, d)).cloned()
}

/// A type parameter's `__default__`: evaluated on first use when it came from the type
/// parameter syntax, `NoDefault` when there is none.
fn default_of(
    it: &mut Interp,
    default: Option<Value>,
    evaluate: Option<Value>,
) -> R<(Value, bool)> {
    if let Some(d) = default {
        return Ok((d, false));
    }
    match evaluate {
        Some(f) => Ok((it.call(&f, Vec::new(), Vec::new())?, true)),
        None => Ok((no_default(it), false)),
    }
}

#[lumen_bind::class(name = "TypeAliasType", module = "typing", hint(py(final)))]
/// Type alias.
///
/// Type aliases are created through the type statement::
///
///     type Alias = int
///
/// In this example, Alias and int will be treated equivalently by static
/// type checkers.
///
/// At runtime, Alias is an instance of TypeAliasType. The __name__
/// attribute holds the name of the type alias. The value of the type alias
/// is stored in the __value__ attribute. It is evaluated lazily, so the
/// value is computed only if the attribute is accessed.
///
/// Type aliases can also be generic::
///
///     type ListOrSet[T] = list[T] | set[T]
///
/// In this case, the type parameters of the alias are stored in the
/// __type_params__ attribute.
///
/// See PEP 695 for more information.
///
pub struct TypeAliasType {
    name: Value,
    type_params: Option<Value>,
    compute_value: Option<Value>,
    value: Option<Value>,
}

#[lumen_bind::class(name = "Generic", module = "typing")]
/// Abstract base class for generic types.
///
/// On Python 3.12 and newer, generic classes implicitly inherit from
/// Generic when they declare a parameter list after the class's name::
///
///     class Mapping[KT, VT]:
///         def __getitem__(self, key: KT) -> VT:
///             ...
///         # Etc.
///
/// On older versions of Python, however, generic classes have to
/// explicitly inherit from Generic.
///
/// After a class has been declared to be generic, it can then be used as
/// follows::
///
///     def lookup_name[KT, VT](mapping: Mapping[KT, VT], key: KT, default: VT) -> VT:
///         try:
///             return mapping[key]
///         except KeyError:
///             return default
///
pub struct Generic;

pub fn is_type_alias(v: &Value) -> bool {
    with_opaque::<TypeAliasType, _>(v, |_| ()).is_some()
}

/// Calls `typing.<name>(*args, **kw)`.
fn call_typing(it: &mut Interp, name: &str, args: Vec<Value>, kw: Vec<(Obj, Value)>) -> R<Value> {
    let m = it.import_module("typing")?;
    let f = it.get_attr_str(&Value::Obj(m), name)?;
    it.call(&f, args, kw)
}

/// `typing._type_check(arg, msg)`, with `None` meaning `type(None)` as in CPython's C helper.
pub(crate) fn type_check(it: &mut Interp, arg: &Value, msg: &str) -> R<Value> {
    if arg.is_none() {
        return Ok(Value::Obj(it.types.none_type.clone()));
    }
    call_typing(
        it,
        "_type_check",
        vec![arg.clone(), Value::str(msg)],
        Vec::new(),
    )
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

fn variance_checks(
    it: &mut Interp,
    covariant: bool,
    contravariant: bool,
    infer_variance: bool,
) -> R<()> {
    if covariant && contravariant {
        return Err(it.value_error("Bivariant types are not supported."));
    }
    if infer_variance && (covariant || contravariant) {
        return Err(it.value_error("Variance cannot be specified with infer_variance."));
    }
    Ok(())
}

fn variance_repr(
    name: &Value,
    covariant: bool,
    contravariant: bool,
    infer_variance: bool,
) -> String {
    let n = name.as_str().unwrap_or("");
    if infer_variance {
        return n.to_string();
    }
    let prefix = if covariant {
        '+'
    } else if contravariant {
        '-'
    } else {
        '~'
    };
    format!("{prefix}{n}")
}

/// Replaces each `TypeVarTuple` in `params` with `Unpack[tvt]`.
fn unpack_typevartuples(it: &mut Interp, params: &Value) -> R<Value> {
    let Some(items) = params.tuple_items() else {
        return Ok(params.clone());
    };
    if !items.iter().any(is_typevartuple) {
        return Ok(params.clone());
    }
    let items = items.to_vec();
    let mut out = Vec::with_capacity(items.len());
    for p in items {
        out.push(if is_typevartuple(&p) {
            unpack(it, &p)?
        } else {
            p
        });
    }
    Ok(Value::tuple(out))
}

fn is_typevartuple(v: &Value) -> bool {
    with_opaque::<TypeVarTuple, _>(v, |_| ()).is_some()
}

fn unpack(it: &mut Interp, tvt: &Value) -> R<Value> {
    let m = it.import_module("typing")?;
    let u = it.get_attr_str(&Value::Obj(m), "Unpack")?;
    it.getitem(&u, tvt)
}

fn no_subclass(it: &mut Interp, what: &str) -> Obj {
    it.type_error(&format!("Cannot subclass an instance of {what}"))
}

/// `T | other` for a type variable: `typing.Union[...]`.
fn make_union(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
    it.make_union(vec![a.clone(), b.clone()])
}

// ---- TypeVar ----

#[lumen_bind::methods]
impl TypeVar {
    #[constructor(hint(py(text_signature = "", arg_name = "typevar")))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        _cls: This<Value>,
        it: &mut Interp,
        #[kw] name: StrRef<'_>,
        #[varargs] constraints: &[Value],
        #[kwonly] bound: Option<&Value>,
        #[kwonly]
        #[default(false)]
        covariant: bool,
        #[kwonly]
        #[default(false)]
        contravariant: bool,
        #[kwonly]
        #[default(false)]
        infer_variance: bool,
        #[kwonly] default: Option<&Value>,
    ) -> R<Value> {
        variance_checks(it, covariant, contravariant, infer_variance)?;
        let default = default_arg(it, default);
        let bound = match bound {
            Some(b) => Some(type_check(it, b, "Bound must be a type.")?),
            None => None,
        };
        let constraints = match constraints.len() {
            0 => None,
            1 => return Err(it.type_error("A single constraint is not allowed")),
            _ if bound.is_some() => {
                return Err(it.type_error("Constraints cannot be combined with bound=..."));
            }
            _ => Some(Value::tuple(constraints.to_vec())),
        };
        let module = caller_module(it);
        let data = TypeVar {
            name: name.0.clone(),
            bound,
            evaluate_bound: None,
            constraints,
            evaluate_constraints: None,
            covariant,
            contravariant,
            infer_variance,
            default,
            evaluate_default: None,
        };
        let v = Py::new(it, data).into_value();
        set_module(it, &v, module);
        Ok(v)
    }

    #[getter(name = "__name__")]
    fn name(&self) -> Value {
        self.name.clone()
    }

    #[getter(name = "__default__")]
    fn default(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let (d, e) = slf
            .0
            .with(it, |d| (d.default.clone(), d.evaluate_default.clone()))?;
        let (v, evaluated) = default_of(it, d, e)?;
        if evaluated {
            slf.0.with(it, |d| d.default = Some(v.clone()))?;
        }
        Ok(v)
    }

    fn has_default(&self) -> bool {
        self.default.is_some() || self.evaluate_default.is_some()
    }

    #[getter(name = "__bound__")]
    fn bound(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let (bound, eval) = slf
            .0
            .with(it, |d| (d.bound.clone(), d.evaluate_bound.clone()))?;
        if let Some(b) = bound {
            return Ok(b);
        }
        let Some(f) = eval else {
            return Ok(Value::None);
        };
        let b = it.call(&f, Vec::new(), Vec::new())?;
        slf.0.with(it, |d| d.bound = Some(b.clone()))?;
        Ok(b)
    }

    #[getter(name = "__constraints__")]
    fn constraints(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let (cons, eval) = slf.0.with(it, |d| {
            (d.constraints.clone(), d.evaluate_constraints.clone())
        })?;
        if let Some(c) = cons {
            return Ok(c);
        }
        let Some(f) = eval else {
            return Ok(Value::tuple(Vec::new()));
        };
        let c = it.call(&f, Vec::new(), Vec::new())?;
        slf.0.with(it, |d| d.constraints = Some(c.clone()))?;
        Ok(c)
    }

    #[getter(name = "__covariant__")]
    fn covariant(&self) -> bool {
        self.covariant
    }

    #[getter(name = "__contravariant__")]
    fn contravariant(&self) -> bool {
        self.contravariant
    }

    #[getter(name = "__infer_variance__")]
    fn infer_variance(&self) -> bool {
        self.infer_variance
    }

    #[proto(repr)]
    fn repr(&self) -> String {
        variance_repr(
            &self.name,
            self.covariant,
            self.contravariant,
            self.infer_variance,
        )
    }

    #[method(name = "__typing_subst__")]
    fn typing_subst(slf: This<Py<Self>>, it: &mut Interp, #[kw] arg: &Value) -> R<Value> {
        call_typing(
            it,
            "_typevar_subst",
            vec![slf.0.value().clone(), arg.clone()],
            Vec::new(),
        )
    }

    #[method(name = "__reduce__")]
    fn reduce(&self) -> Value {
        self.name.clone()
    }

    #[method(name = "__mro_entries__", hint(py(text_signature = "")))]
    fn mro_entries(slf: This<Py<Self>>, it: &mut Interp, bases: &Value) -> R<Value> {
        let _ = (slf, bases);
        Err(no_subclass(it, "TypeVar"))
    }

    #[proto(or)]
    fn or(slf: This<&Value>, it: &mut Interp, right: &Value) -> R<Value> {
        make_union(it, &slf, right)
    }

    #[proto(ror)]
    fn ror(slf: This<&Value>, it: &mut Interp, left: &Value) -> R<Value> {
        make_union(it, left, &slf)
    }
}

// ---- ParamSpec ----

#[lumen_bind::methods]
impl ParamSpec {
    #[constructor(hint(py(text_signature = "", arg_name = "paramspec")))]
    fn new(
        _cls: This<Value>,
        it: &mut Interp,
        #[kw] name: StrRef<'_>,
        #[kwonly] bound: Option<&Value>,
        #[kwonly]
        #[default(false)]
        covariant: bool,
        #[kwonly]
        #[default(false)]
        contravariant: bool,
        #[kwonly]
        #[default(false)]
        infer_variance: bool,
        #[kwonly] default: Option<&Value>,
    ) -> R<Value> {
        variance_checks(it, covariant, contravariant, infer_variance)?;
        let default = default_arg(it, default);
        let bound = type_check(it, bound.unwrap_or(&Value::None), "Bound must be a type.")?;
        let module = caller_module(it);
        let data = ParamSpec {
            name: name.0.clone(),
            bound: Some(bound),
            covariant,
            contravariant,
            infer_variance,
            default,
            evaluate_default: None,
        };
        let v = Py::new(it, data).into_value();
        set_module(it, &v, module);
        Ok(v)
    }

    #[getter(name = "__name__")]
    fn name(&self) -> Value {
        self.name.clone()
    }

    #[getter(name = "__bound__")]
    fn bound(&self) -> Value {
        self.bound.clone().unwrap_or(Value::None)
    }

    #[getter(name = "__default__")]
    fn default(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let (d, e) = slf
            .0
            .with(it, |d| (d.default.clone(), d.evaluate_default.clone()))?;
        let (v, evaluated) = default_of(it, d, e)?;
        if evaluated {
            slf.0.with(it, |d| d.default = Some(v.clone()))?;
        }
        Ok(v)
    }

    fn has_default(&self) -> bool {
        self.default.is_some() || self.evaluate_default.is_some()
    }

    #[getter(name = "__covariant__")]
    fn covariant(&self) -> bool {
        self.covariant
    }

    #[getter(name = "__contravariant__")]
    fn contravariant(&self) -> bool {
        self.contravariant
    }

    #[getter(name = "__infer_variance__")]
    fn infer_variance(&self) -> bool {
        self.infer_variance
    }

    /// Represents positional arguments.
    #[getter]
    fn args(slf: This<Py<Self>>, it: &mut Interp) -> Value {
        Py::new(
            it,
            ParamSpecArgs {
                origin: slf.0.value().clone(),
            },
        )
        .into_value()
    }

    /// Represents keyword arguments.
    #[getter]
    fn kwargs(slf: This<Py<Self>>, it: &mut Interp) -> Value {
        Py::new(
            it,
            ParamSpecKwargs {
                origin: slf.0.value().clone(),
            },
        )
        .into_value()
    }

    #[proto(repr)]
    fn repr(&self) -> String {
        variance_repr(
            &self.name,
            self.covariant,
            self.contravariant,
            self.infer_variance,
        )
    }

    #[method(name = "__typing_subst__")]
    fn typing_subst(slf: This<Py<Self>>, it: &mut Interp, #[kw] arg: &Value) -> R<Value> {
        call_typing(
            it,
            "_paramspec_subst",
            vec![slf.0.value().clone(), arg.clone()],
            Vec::new(),
        )
    }

    #[method(name = "__typing_prepare_subst__")]
    fn typing_prepare_subst(
        slf: This<Py<Self>>,
        it: &mut Interp,
        #[kw] alias: &Value,
        #[kw] args: &Value,
    ) -> R<Value> {
        call_typing(
            it,
            "_paramspec_prepare_subst",
            vec![slf.0.value().clone(), alias.clone(), args.clone()],
            Vec::new(),
        )
    }

    #[method(name = "__reduce__")]
    fn reduce(&self) -> Value {
        self.name.clone()
    }

    #[method(name = "__mro_entries__", hint(py(text_signature = "")))]
    fn mro_entries(slf: This<Py<Self>>, it: &mut Interp, bases: &Value) -> R<Value> {
        let _ = (slf, bases);
        Err(no_subclass(it, "ParamSpec"))
    }

    #[proto(or)]
    fn or(slf: This<&Value>, it: &mut Interp, right: &Value) -> R<Value> {
        make_union(it, &slf, right)
    }

    #[proto(ror)]
    fn ror(slf: This<&Value>, it: &mut Interp, left: &Value) -> R<Value> {
        make_union(it, left, &slf)
    }
}

// ---- ParamSpecArgs / ParamSpecKwargs ----

/// `P.args` / `P.kwargs` (the name of a `ParamSpec` origin, else its repr).
fn psattr_repr(it: &mut Interp, origin: &Value, suffix: &str) -> R<String> {
    let base = match with_opaque::<ParamSpec, _>(origin, |d| d.name.clone()) {
        Some(n) => n.as_str().unwrap_or("").to_string(),
        None => it.repr_of(origin)?,
    };
    Ok(format!("{base}.{suffix}"))
}

/// `==` / `!=` of two `ParamSpecArgs` (or two `ParamSpecKwargs`): their origins compared.
fn psattr_cmp(
    it: &mut Interp,
    a: &Value,
    b: &Value,
    origin: fn(&Value) -> Option<Value>,
    negate: bool,
) -> R<Value> {
    let (t1, t2) = (it.type_of(a), it.type_of(b));
    if !std::rc::Rc::ptr_eq(&t1, &t2) {
        return Ok(Value::NotImplemented);
    }
    let (Some(o1), Some(o2)) = (origin(a), origin(b)) else {
        return Ok(Value::NotImplemented);
    };
    let eq = it.values_eq(&o1, &o2)?;
    Ok(Value::Bool(eq != negate))
}

fn args_origin(v: &Value) -> Option<Value> {
    with_opaque::<ParamSpecArgs, _>(v, |d| d.origin.clone())
}

fn kwargs_origin(v: &Value) -> Option<Value> {
    with_opaque::<ParamSpecKwargs, _>(v, |d| d.origin.clone())
}

#[lumen_bind::methods]
impl ParamSpecArgs {
    #[constructor(hint(py(text_signature = "", arg_name = "paramspecargs")))]
    fn new(#[kw] origin: &Value) -> ParamSpecArgs {
        ParamSpecArgs {
            origin: origin.clone(),
        }
    }

    #[getter(name = "__origin__")]
    fn origin(&self) -> Value {
        self.origin.clone()
    }

    #[proto(repr)]
    fn repr(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
        let origin = slf.0.borrow(it)?.origin.clone();
        psattr_repr(it, &origin, "args")
    }

    #[proto(eq)]
    fn eq(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        psattr_cmp(it, &slf, value, args_origin, false)
    }

    #[proto(ne)]
    fn ne(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        psattr_cmp(it, &slf, value, args_origin, true)
    }

    #[method(name = "__mro_entries__", hint(py(text_signature = "")))]
    fn mro_entries(slf: This<Py<Self>>, it: &mut Interp, bases: &Value) -> R<Value> {
        let _ = (slf, bases);
        Err(no_subclass(it, "ParamSpecArgs"))
    }
}

#[lumen_bind::methods]
impl ParamSpecKwargs {
    #[constructor(hint(py(text_signature = "", arg_name = "paramspeckwargs")))]
    fn new(#[kw] origin: &Value) -> ParamSpecKwargs {
        ParamSpecKwargs {
            origin: origin.clone(),
        }
    }

    #[getter(name = "__origin__")]
    fn origin(&self) -> Value {
        self.origin.clone()
    }

    #[proto(repr)]
    fn repr(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
        let origin = slf.0.borrow(it)?.origin.clone();
        psattr_repr(it, &origin, "kwargs")
    }

    #[proto(eq)]
    fn eq(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        psattr_cmp(it, &slf, value, kwargs_origin, false)
    }

    #[proto(ne)]
    fn ne(slf: This<&Value>, it: &mut Interp, value: &Value) -> R<Value> {
        psattr_cmp(it, &slf, value, kwargs_origin, true)
    }

    #[method(name = "__mro_entries__", hint(py(text_signature = "")))]
    fn mro_entries(slf: This<Py<Self>>, it: &mut Interp, bases: &Value) -> R<Value> {
        let _ = (slf, bases);
        Err(no_subclass(it, "ParamSpecKwargs"))
    }
}

// ---- TypeVarTuple ----

#[lumen_bind::methods]
impl TypeVarTuple {
    #[constructor(hint(py(text_signature = "", arg_name = "typevartuple")))]
    fn new(
        _cls: This<Value>,
        it: &mut Interp,
        #[kw] name: StrRef<'_>,
        #[kwonly] default: Option<&Value>,
    ) -> R<Value> {
        let module = caller_module(it);
        let default = default_arg(it, default);
        let v = Py::new(
            it,
            TypeVarTuple {
                name: name.0.clone(),
                default,
                evaluate_default: None,
            },
        )
        .into_value();
        set_module(it, &v, module);
        Ok(v)
    }

    #[getter(name = "__name__")]
    fn name(&self) -> Value {
        self.name.clone()
    }

    #[getter(name = "__default__")]
    fn default(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let (d, e) = slf
            .0
            .with(it, |d| (d.default.clone(), d.evaluate_default.clone()))?;
        let (v, evaluated) = default_of(it, d, e)?;
        if evaluated {
            slf.0.with(it, |d| d.default = Some(v.clone()))?;
        }
        Ok(v)
    }

    fn has_default(&self) -> bool {
        self.default.is_some() || self.evaluate_default.is_some()
    }

    #[proto(repr)]
    fn repr(&self) -> Value {
        self.name.clone()
    }

    #[proto(iter)]
    fn iter(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        let u = unpack(it, &slf)?;
        it.native_get_iter(&Value::tuple(vec![u]))
    }

    #[method(name = "__typing_subst__")]
    fn typing_subst(slf: This<Py<Self>>, it: &mut Interp, #[kw] arg: &Value) -> R<Value> {
        let _ = (slf, arg);
        Err(it.type_error("Substitution of bare TypeVarTuple is not supported"))
    }

    #[method(name = "__typing_prepare_subst__")]
    fn typing_prepare_subst(
        slf: This<Py<Self>>,
        it: &mut Interp,
        #[kw] alias: &Value,
        #[kw] args: &Value,
    ) -> R<Value> {
        call_typing(
            it,
            "_typevartuple_prepare_subst",
            vec![slf.0.value().clone(), alias.clone(), args.clone()],
            Vec::new(),
        )
    }

    #[method(name = "__reduce__")]
    fn reduce(&self) -> Value {
        self.name.clone()
    }

    #[method(name = "__mro_entries__", hint(py(text_signature = "")))]
    fn mro_entries(slf: This<Py<Self>>, it: &mut Interp, bases: &Value) -> R<Value> {
        let _ = (slf, bases);
        Err(no_subclass(it, "TypeVarTuple"))
    }
}

// ---- TypeAliasType ----

fn new_typealias(it: &mut Interp, data: TypeAliasType, module: Option<Value>) -> R<Value> {
    let module = match (module, &data.compute_value) {
        (Some(m), _) => Some(m),
        (None, Some(f)) => Some(it.get_attr_str(f, "__module__")?),
        (None, None) => None,
    };
    let v = Py::new(it, data).into_value();
    if let Some(m) = module {
        set_module(it, &v, m);
    }
    Ok(v)
}

#[lumen_bind::methods]
impl TypeAliasType {
    #[constructor(hint(py(text_signature = "", arg_name = "typealias")))]
    fn new(
        _cls: This<Value>,
        it: &mut Interp,
        #[kw] name: StrRef<'_>,
        #[kw] value: &Value,
        #[kwonly] type_params: Option<&Value>,
    ) -> R<Value> {
        let type_params = match type_params {
            None => None,
            Some(tp) if tp.tuple_items().is_some() => Some(tp.clone()),
            Some(_) => return Err(it.type_error("type_params must be a tuple")),
        };
        let module = caller_module(it);
        let data = TypeAliasType {
            name: name.0.clone(),
            type_params,
            compute_value: None,
            value: Some(value.clone()),
        };
        new_typealias(it, data, Some(module))
    }

    #[getter(name = "__name__")]
    fn name(&self) -> Value {
        self.name.clone()
    }

    #[getter(name = "__value__")]
    fn value(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let (value, compute) = slf
            .0
            .with(it, |d| (d.value.clone(), d.compute_value.clone()))?;
        if let Some(v) = value {
            return Ok(v);
        }
        let Some(f) = compute else {
            return Ok(Value::None);
        };
        let v = it.call(&f, Vec::new(), Vec::new())?;
        slf.0.with(it, |d| d.value = Some(v.clone()))?;
        Ok(v)
    }

    #[getter(name = "__type_params__")]
    fn type_params(&self) -> Value {
        self.type_params
            .clone()
            .unwrap_or_else(|| Value::tuple(Vec::new()))
    }

    #[getter(name = "__parameters__")]
    fn parameters(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        match slf.0.borrow(it)?.type_params.clone() {
            Some(tp) => unpack_typevartuples(it, &tp),
            None => Ok(Value::tuple(Vec::new())),
        }
    }

    #[proto(repr)]
    fn repr(&self) -> Value {
        self.name.clone()
    }

    #[proto(getitem)]
    fn getitem(slf: This<Py<Self>>, it: &mut Interp, key: &Value) -> R<Value> {
        if slf.0.borrow(it)?.type_params.is_none() {
            return Err(it.type_error("Only generic type aliases are subscriptable"));
        }
        Ok(it.make_alias(slf.0.value().clone(), key))
    }

    #[proto(or)]
    fn or(slf: This<&Value>, it: &mut Interp, right: &Value) -> R<Value> {
        Ok(it
            .union_binop(&slf, right)?
            .unwrap_or(Value::NotImplemented))
    }

    #[proto(ror)]
    fn ror(slf: This<&Value>, it: &mut Interp, left: &Value) -> R<Value> {
        Ok(it.union_binop(left, &slf)?.unwrap_or(Value::NotImplemented))
    }

    #[method(name = "__reduce__")]
    fn reduce(&self) -> Value {
        self.name.clone()
    }

    // CPython's alias objects have no `__dict__` and read-only members; `__module__` lives in
    // the instance dict here, so every assignment is refused explicitly.
    #[proto(setattr)]
    fn setattr(slf: This<Py<Self>>, it: &mut Interp, name: &Value, value: &Value) -> R<()> {
        let _ = (slf, value);
        Err(alias_attr_error(it, name))
    }

    #[proto(delattr)]
    fn delattr(slf: This<Py<Self>>, it: &mut Interp, name: &Value) -> R<()> {
        let _ = slf;
        Err(alias_attr_error(it, name))
    }
}

fn alias_attr_error(it: &mut Interp, name: &Value) -> Obj {
    let name = name.as_str().unwrap_or("");
    let msg = match name {
        "__name__" | "__type_params__" => "readonly attribute".to_string(),
        "__value__" | "__parameters__" | "__module__" => {
            format!("attribute '{name}' of 'typing.TypeAliasType' objects is not writable")
        }
        _ => format!("'typing.TypeAliasType' object has no attribute '{name}'"),
    };
    it.new_exc_str("AttributeError", &msg)
}

#[lumen_bind::methods]
impl NoDefaultType {
    #[constructor(hint(py(text_signature = "")))]
    fn new(_cls: This<Value>, it: &mut Interp) -> Value {
        no_default(it)
    }

    #[proto(repr)]
    fn repr(&self) -> &'static str {
        "typing.NoDefault"
    }

    #[method(name = "__reduce__")]
    fn reduce(&self) -> &'static str {
        "NoDefault"
    }
}

// ---- Generic ----

#[lumen_bind::methods]
impl Generic {
    /// Parameterizes a generic class.
    ///
    /// At least, parameterizing a generic class is the *main* thing this
    /// method does. For example, for some generic class `Foo`, this is called
    /// when we do `Foo[int]` - there, with `cls=Foo` and `params=int`.
    ///
    /// However, note that this method is also called when defining generic
    /// classes in the first place with `class Foo[T]: ...`.
    ///
    #[classmethod(name = "__class_getitem__", hint(py(text_signature = "")))]
    fn class_getitem(cls: This<Value>, it: &mut Interp, params: &Value) -> R<Value> {
        call_typing(
            it,
            "_generic_class_getitem",
            vec![cls.0, params.clone()],
            Vec::new(),
        )
    }

    /// Function to initialize subclasses.
    #[classmethod(name = "__init_subclass__", hint(py(text_signature = "")))]
    fn init_subclass(
        cls: This<Value>,
        it: &mut Interp,
        #[varargs] args: &[Value],
        #[varkw] kwargs: KwArgs,
    ) -> R<Value> {
        let mut all = vec![cls.0];
        all.extend_from_slice(args);
        call_typing(it, "_generic_init_subclass", all, kwargs.to_vec())
    }
}

// ---- intrinsics ----

pub fn intrinsic1(it: &mut Interp, k: u32, v: Value) -> R<Value> {
    match k {
        INTRINSIC1_TYPEVAR => Ok(Py::new(
            it,
            TypeVar {
                name: v,
                bound: None,
                evaluate_bound: None,
                constraints: None,
                evaluate_constraints: None,
                covariant: false,
                contravariant: false,
                infer_variance: true,
                default: None,
                evaluate_default: None,
            },
        )
        .into_value()),
        INTRINSIC1_PARAMSPEC => {
            let data = ParamSpec {
                name: v,
                bound: None,
                covariant: false,
                contravariant: false,
                infer_variance: true,
                default: None,
                evaluate_default: None,
            };
            Ok(Py::new(it, data).into_value())
        }
        INTRINSIC1_TYPEVARTUPLE => Ok(Py::new(
            it,
            TypeVarTuple {
                name: v,
                default: None,
                evaluate_default: None,
            },
        )
        .into_value()),
        INTRINSIC1_SUBSCRIPT_GENERIC => {
            let params = unpack_typevartuples(it, &v)?;
            let generic = generic_type(it);
            call_typing(
                it,
                "_GenericAlias",
                vec![Value::Obj(generic), params],
                Vec::new(),
            )
        }
        INTRINSIC1_TYPEALIAS => {
            let items = v.tuple_items().map(|t| t.to_vec()).unwrap_or_default();
            let [name, type_params, compute] = <[Value; 3]>::try_from(items)
                .map_err(|_| it.new_exc_str("SystemError", "bad type alias intrinsic argument"))?;
            let data = TypeAliasType {
                name,
                type_params: (!type_params.is_none()).then_some(type_params),
                compute_value: Some(compute),
                value: None,
            };
            new_typealias(it, data, None)
        }
        _ => Err(it.new_exc_str("SystemError", "unknown intrinsic")),
    }
}

pub fn intrinsic2(it: &mut Interp, k: u32, a: Value, b: Value) -> R<Value> {
    match k {
        INTRINSIC2_TYPEVAR_WITH_BOUND | INTRINSIC2_TYPEVAR_WITH_CONSTRAINTS => {
            let constraints = k == INTRINSIC2_TYPEVAR_WITH_CONSTRAINTS;
            let data = TypeVar {
                name: a,
                bound: None,
                evaluate_bound: (!constraints).then(|| b.clone()),
                constraints: None,
                evaluate_constraints: constraints.then_some(b),
                covariant: false,
                contravariant: false,
                infer_variance: true,
                default: None,
                evaluate_default: None,
            };
            Ok(Py::new(it, data).into_value())
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

/// `typing.Generic`: an ordinary class (its subclasses are plain Python classes), created once
/// per interpreter with the native members installed.
fn generic_type(it: &mut Interp) -> Obj {
    if let Some(t) = it.native_types.get(&std::any::TypeId::of::<Generic>()) {
        return t.clone();
    }
    let t = new_type(it, "typing", "Generic", None, Layout::Object);
    crate::bind::extend_type_documented::<Generic>(it, &t);
    t
}

/// Accelerators for the typing module.
///
#[lumen_bind::module(name = "_typing")]
pub mod _typing {
    use super::*;

    #[op(name = "_idfunc")]
    fn idfunc(x: &Value) -> Value {
        x.clone()
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        use crate::builtins::descr::install_getsets;
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let tv = type_object::<TypeVar>(it);
        install_getsets::<TypeVar>(
            it,
            &tv,
            &[
                "__name__",
                "__covariant__",
                "__contravariant__",
                "__infer_variance__",
            ],
        );
        let ps = type_object::<ParamSpec>(it);
        install_getsets::<ParamSpec>(
            it,
            &ps,
            &[
                "__name__",
                "__bound__",
                "__covariant__",
                "__contravariant__",
                "__infer_variance__",
            ],
        );
        let psa = type_object::<ParamSpecArgs>(it);
        install_getsets::<ParamSpecArgs>(it, &psa, &["__origin__"]);
        let psk = type_object::<ParamSpecKwargs>(it);
        install_getsets::<ParamSpecKwargs>(it, &psk, &["__origin__"]);
        let tvt = type_object::<TypeVarTuple>(it);
        install_getsets::<TypeVarTuple>(it, &tvt, &["__name__"]);
        let ta = type_object::<TypeAliasType>(it);
        install_getsets::<TypeAliasType>(it, &ta, &["__name__"]);
        let generic = generic_type(it);
        let union = type_object::<crate::builtins::alias::UnionData>(it);
        let no_default_type = type_object::<NoDefaultType>(it);
        let _ = no_default_type;
        dict_set_str(&d, "NoDefault", no_default(it));
        for (n, t) in [
            ("Union", union),
            ("TypeVar", tv),
            ("ParamSpec", ps),
            ("ParamSpecArgs", psa),
            ("ParamSpecKwargs", psk),
            ("TypeVarTuple", tvt),
            ("TypeAliasType", ta),
            ("Generic", generic),
        ] {
            dict_set_str(&d, n, Value::Obj(t));
        }
    }
}
