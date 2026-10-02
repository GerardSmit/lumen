//! `_decimal`: CPython's libmpdec-backed decimal module. The arithmetic is
//! [`lumen_common::decimal`] (language-neutral); this module is the Python surface: `Decimal`,
//! `Context`, the signal exceptions, per-thread contexts (a context variable, as in CPython 3.12),
//! `localcontext`, and the exact error messages of the C module.

mod context;
mod decimal;
mod support;

use crate::bind::{type_object, KwArgs, Py};
use crate::builtins::native::new_type;
use crate::object::*;
use crate::vm::{dict_set_str, Interp};
use context::{apply_settings, copy_context, ctx_arg, current, make_templates, set_current, Excs, PyContext, SignalDict, State};
use decimal::PyDecimal;
use lumen_common::decimal as dec;
use std::rc::Rc;
use support::parse_args;

const ROUNDINGS: [&str; 8] = [
    "ROUND_UP",
    "ROUND_DOWN",
    "ROUND_CEILING",
    "ROUND_FLOOR",
    "ROUND_HALF_UP",
    "ROUND_HALF_DOWN",
    "ROUND_HALF_EVEN",
    "ROUND_05UP",
];

/// Module copies made by `import_fresh_module` share one set of exception classes: the state is
/// per interpreter, so exceptions raised by the arithmetic must match every copy's names.
fn signal_exceptions(it: &mut Interp, d: &Obj) {
    if let Some(excs) = it.native_state::<State>().excs.clone() {
        dict_set_str(d, "DecimalException", Value::Obj(excs.decimal_exception.clone()));
        for (name, cls) in context::SIGNAL_NAMES.iter().zip(&excs.signals) {
            dict_set_str(d, name, Value::Obj(cls.clone()));
        }
        return;
    }
    let arith = it.exc_type("ArithmeticError");
    let zero_div = it.exc_type("ZeroDivisionError");
    let type_err = it.exc_type("TypeError");
    let mk = |it: &mut Interp, name: &str, bases: Vec<Obj>| -> Obj {
        let ty = new_type(it, "decimal", name, Some(&bases[0]), Layout::Exception);
        if bases.len() > 1 {
            it.set_bases(&ty, bases);
        }
        ty
    };
    let base = mk(it, "DecimalException", vec![arith]);
    let clamped = mk(it, "Clamped", vec![base.clone()]);
    let invalid = mk(it, "InvalidOperation", vec![base.clone()]);
    let conversion = mk(it, "ConversionSyntax", vec![invalid.clone()]);
    let impossible = mk(it, "DivisionImpossible", vec![invalid.clone()]);
    let undefined = mk(it, "DivisionUndefined", vec![invalid.clone(), zero_div.clone()]);
    let invalid_context = mk(it, "InvalidContext", vec![invalid.clone()]);
    let by_zero = mk(it, "DivisionByZero", vec![base.clone(), zero_div]);
    let inexact = mk(it, "Inexact", vec![base.clone()]);
    let rounded = mk(it, "Rounded", vec![base.clone()]);
    let subnormal = mk(it, "Subnormal", vec![base.clone()]);
    let overflow = mk(it, "Overflow", vec![inexact.clone(), rounded.clone()]);
    let underflow = mk(it, "Underflow", vec![inexact.clone(), rounded.clone(), subnormal.clone()]);
    let float_operation = mk(it, "FloatOperation", vec![base.clone(), type_err]);
    let signals = vec![
        invalid.clone(),
        float_operation,
        by_zero,
        overflow,
        underflow,
        subnormal,
        inexact,
        rounded,
        clamped,
    ];
    let conds = vec![
        (dec::flag::INVALID_OPERATION, invalid),
        (dec::flag::CONVERSION_SYNTAX, conversion),
        (dec::flag::DIVISION_IMPOSSIBLE, impossible),
        (dec::flag::DIVISION_UNDEFINED, undefined),
        (dec::flag::INVALID_CONTEXT, invalid_context),
    ];
    dict_set_str(d, "DecimalException", Value::Obj(base.clone()));
    for (name, cls) in context::SIGNAL_NAMES.iter().zip(&signals) {
        dict_set_str(d, name, Value::Obj(cls.clone()));
    }
    it.native_state::<State>().excs = Some(Rc::new(Excs { decimal_exception: base, signals, conds }));
}

fn build(it: &mut Interp, m: &Value) -> R<()> {
    let Value::Obj(m) = m else { return Ok(()) };
    let d = it.module_dict(m);
    signal_exceptions(it, &d);

    let contextvars = it.import_module("_contextvars")?;
    let var_type = it.get_attr_str(&Value::Obj(contextvars), "ContextVar")?;
    let var = it.call(&var_type, vec![Value::str("decimal_context")], Vec::new())?;
    let get = it.get_attr_str(&var, "get")?;
    let set = it.get_attr_str(&var, "set")?;

    let templates = make_templates(it);
    dict_set_str(&d, "DefaultContext", templates[0].value().clone());
    dict_set_str(&d, "BasicContext", templates[1].value().clone());
    dict_set_str(&d, "ExtendedContext", templates[2].value().clone());

    let decimal_type = type_object::<PyDecimal>(it);
    dict_set_str(&d, "Decimal", Value::Obj(decimal_type.clone()));
    dict_set_str(&d, "Context", Value::Obj(type_object::<PyContext>(it)));

    let mixin = type_object::<SignalDict>(it);
    let abcs = it.import_module("_collections_abc")?;
    let mutable_mapping = it.get_attr_str(&Value::Obj(abcs), "MutableMapping")?;
    let type_value = Value::Obj(it.types.type_.clone());
    let bases = Value::tuple(vec![Value::Obj(mixin), mutable_mapping]);
    let namespace = Value::Obj(it.new_dict());
    let signal_dict = it.call(&type_value, vec![Value::str("SignalDict"), bases, namespace], Vec::new())?;

    let collections = it.import_module("collections")?;
    let namedtuple = it.get_attr_str(&Value::Obj(collections), "namedtuple")?;
    let module_kw = vec![(it.str_obj("module"), Value::str("decimal"))];
    let tuple = it.call(&namedtuple, vec![Value::str("DecimalTuple"), Value::str("sign digits exponent")], module_kw)?;
    dict_set_str(&d, "DecimalTuple", tuple.clone());

    let numbers = Value::Obj(it.import_module("numbers")?);
    let number = it.get_attr_str(&numbers, "Number")?;
    let rational = it.get_attr_str(&numbers, "Rational")?;
    let register = it.get_attr_str(&number, "register")?;
    it.call(&register, vec![Value::Obj(decimal_type)], Vec::new())?;

    {
        let st = it.native_state::<State>();
        st.get = Some(get);
        st.set = Some(set);
        st.templates = Some(templates);
        st.signal_dict = match signal_dict {
            Value::Obj(o) => Some(o),
            _ => None,
        };
        st.tuple = Some(tuple);
        st.rational = Some(rational);
    }

    for name in ROUNDINGS {
        dict_set_str(&d, name, Value::str(name));
    }
    dict_set_str(&d, "MAX_PREC", Value::Int(dec::MAX_PREC));
    dict_set_str(&d, "MAX_EMAX", Value::Int(dec::MAX_EMAX));
    dict_set_str(&d, "MIN_EMIN", Value::Int(dec::MIN_EMIN));
    dict_set_str(&d, "MIN_ETINY", Value::Int(dec::MIN_ETINY));
    dict_set_str(&d, "HAVE_THREADS", Value::Bool(true));
    dict_set_str(&d, "HAVE_CONTEXTVAR", Value::Bool(true));
    dict_set_str(&d, "__version__", Value::str("1.70"));
    dict_set_str(&d, "__libmpdec_version__", Value::str("2.5.1"));
    Ok(())
}

/// C implementation of the decimal module.
#[lumen_bind::module(name = "_decimal")]
pub mod _decimal {
    use super::{apply_settings, copy_context, ctx_arg, current, parse_args, set_current, PyContext, State};
    use crate::bind::{KwArgs, Py};
    use crate::object::*;
    use crate::vm::Interp;

    /// Get the current default context.
    #[op]
    fn getcontext(it: &mut Interp) -> R<Value> {
        Ok(current(it)?.into_value())
    }

    /// Set a new default context.
    #[op]
    fn setcontext(it: &mut Interp, context: &Value) -> R<()> {
        let Some(c) = Py::<PyContext>::from_value(it, context) else {
            return Err(it.type_error("argument must be a context"));
        };
        let is_template = it.native_state::<State>().templates.as_ref().is_some_and(|t| t.iter().any(|x| x.value().is(c.value())));
        let target = if is_template { copy_context(it, &c, true)? } else { c };
        set_current(it, &target)
    }

    /// Return a context manager that will set the default context to a copy of ctx on entry to the
    /// with-statement and restore the previous default context when exiting the with-statement. If
    /// no context is specified, a copy of the current default context is used.
    #[op]
    fn localcontext(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        let v = parse_args(it, args, kw, &["ctx", "prec", "rounding", "Emin", "Emax", "capitals", "clamp", "flags", "traps"], 0)?;
        let base = ctx_arg(it, v[0].as_ref())?;
        let local = copy_context(it, &base, false)?;
        apply_settings(it, &local, &v[1..])?;
        Ok(Py::new(it, super::context::ContextManager { local, global: None }).into_value())
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let _ = super::build(it, m);
    }
}
