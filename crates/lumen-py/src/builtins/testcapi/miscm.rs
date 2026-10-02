//! `_testcapi` odds and ends of `_testcapimodule.c`, `docstring.c`, `exceptions.c`, `immortal.c` and
//! `vectorcall_limited.c`: docstring probes, `structmember.h` type codes, feature macros, error
//! state helpers, trace-function installers and the pure C self-tests. `None` stands for a `NULL`
//! pointer.

#![allow(non_snake_case)]

use super::{call_builtin, system_error};
use crate::bind::{index, This};
use crate::object::*;
use crate::vm::{dict_set_str, Interp};

const RECORD_TRACE: &str = "def make(events):
    names = {'call': 0, 'exception': 1, 'line': 2, 'return': 3, 'opcode': 7}
    def record(frame, what, arg):
        events.append((names.get(what, -1), frame.f_lineno, arg))
        return record
    return record
";

const PREP_RERAISE_STAR: &str = "def prep(orig, excs):
    if not isinstance(orig, BaseException):
        raise TypeError('orig must be an exception instance')
    if not isinstance(excs, list):
        raise TypeError('excs must be a list')
    for e in excs:
        if e is not None and not isinstance(e, BaseException):
            raise TypeError(f'item in excs list is not an exception: {e!r}')
    if orig.__traceback__ is None:
        raise ValueError('orig must be a raised exception')
    if not excs:
        return None
    if not isinstance(orig, BaseExceptionGroup):
        return excs[0]
    raised = []
    reraised = []
    for e in excs:
        if e is None:
            continue
        if (e.__traceback__ is orig.__traceback__ and e.__cause__ is orig.__cause__
                and e.__context__ is orig.__context__):
            reraised.append(e)
        else:
            raised.append(e)
    if reraised:
        leaves = set()
        def collect(e):
            if isinstance(e, BaseExceptionGroup):
                for sub in e.exceptions:
                    collect(sub)
            else:
                leaves.add(id(e))
        for e in reraised:
            collect(e)
        match, _ = orig.split(lambda e: id(e) in leaves)
        if match is not None:
            raised.append(match)
    if len(raised) > 1:
        return BaseExceptionGroup('', raised)
    if raised:
        return raised[0]
    return None
";

const ERROR_TRACE: &str = "def make(events):
    def raiser(frame, what, arg):
        if len(events):
            return raiser
        events.append(None)
        raise Exception('an exception')
    return raiser
";

fn list_arg(it: &mut Interp, v: &Value) -> R<()> {
    if matches!(v, Value::Obj(o) if matches!(o.kind, Kind::List(_))) {
        Ok(())
    } else {
        Err(it.type_error("argument must be a list"))
    }
}

fn install_trace(it: &mut Interp, source: &str, events: &Value) -> R<()> {
    let globals = Value::Obj(it.new_dict());
    call_builtin(it, "exec", vec![Value::str(source), globals.clone()])?;
    let Value::Obj(g) = &globals else { unreachable!("a dict was just built") };
    let make = crate::vm::dict_get_str(g, "make").unwrap_or(Value::None);
    let trace = it.call(&make, vec![events.clone()], Vec::new())?;
    let sys = it.import_module("sys")?;
    let settrace = it.get_attr_str(&Value::Obj(sys), "settrace")?;
    it.call(&settrace, vec![trace], Vec::new())?;
    Ok(())
}

/// `PyErr_Restore`: the exception `type(value)` (or `value` itself) with `tb` attached.
fn restore(it: &mut Interp, ty: &Value, value: Option<&Value>, tb: Option<&Value>) -> Obj {
    let built = match value.filter(|v| !v.is_none()) {
        Some(v) if it.isinstance_value(v, ty).unwrap_or(false) => Ok(v.clone()),
        Some(v) => {
            let args = match v.tuple_items() {
                Some(items) => items.to_vec(),
                None => vec![v.clone()],
            };
            it.call(ty, args, Vec::new())
        }
        None => it.call(ty, Vec::new(), Vec::new()),
    };
    let exc = match built {
        Ok(Value::Obj(o)) if matches!(o.kind, Kind::Exception(_)) => o,
        Ok(_) => return it.type_error("exceptions must derive from BaseException"),
        Err(e) => return e,
    };
    if let Some(tb) = tb.filter(|t| !t.is_none()) {
        if let Err(e) = it.set_attr_str(&Value::Obj(exc.clone()), "__traceback__", tb.clone()) {
            return e;
        }
    }
    exc
}

#[lumen_bind::class(module = "_testcapi", name = "RecursingInit")]
pub struct RecursingInit;

#[lumen_bind::methods]
impl RecursingInit {
    #[proto(init)]
    fn init(slf: This<&Value>, it: &mut Interp) -> R<()> {
        let _ = slf;
        let ty = crate::bind::type_object::<RecursingInit>(it);
        it.call(&Value::Obj(ty), Vec::new(), Vec::new())?;
        Ok(())
    }
}

#[lumen_bind::module(name = "_testcapi")]
pub mod miscm {
    use super::*;

    #[constant(name = "T_SHORT")]
    const T_SHORT: i64 = 0;
    #[constant(name = "T_INT")]
    const T_INT: i64 = 1;
    #[constant(name = "T_LONG")]
    const T_LONG: i64 = 2;
    #[constant(name = "T_FLOAT")]
    const T_FLOAT: i64 = 3;
    #[constant(name = "T_DOUBLE")]
    const T_DOUBLE: i64 = 4;
    #[constant(name = "T_STRING")]
    const T_STRING: i64 = 5;
    #[constant(name = "T_OBJECT")]
    const T_OBJECT: i64 = 6;
    #[constant(name = "T_CHAR")]
    const T_CHAR: i64 = 7;
    #[constant(name = "T_BYTE")]
    const T_BYTE: i64 = 8;
    #[constant(name = "T_UBYTE")]
    const T_UBYTE: i64 = 9;
    #[constant(name = "T_USHORT")]
    const T_USHORT: i64 = 10;
    #[constant(name = "T_UINT")]
    const T_UINT: i64 = 11;
    #[constant(name = "T_ULONG")]
    const T_ULONG: i64 = 12;
    #[constant(name = "T_STRING_INPLACE")]
    const T_STRING_INPLACE: i64 = 13;
    #[constant(name = "T_BOOL")]
    const T_BOOL: i64 = 14;
    #[constant(name = "T_OBJECT_EX")]
    const T_OBJECT_EX: i64 = 16;
    #[constant(name = "T_LONGLONG")]
    const T_LONGLONG: i64 = 17;
    #[constant(name = "T_ULONGLONG")]
    const T_ULONGLONG: i64 = 18;
    #[constant(name = "T_PYSSIZET")]
    const T_PYSSIZET: i64 = 19;
    #[constant(name = "T_NONE")]
    const T_NONE: i64 = 20;
    #[constant(name = "SIZEOF_PID_T")]
    const SIZEOF_PID_T: i64 = 4;

    // ---- docstring probes (docstring.c) -----------------------------------------------------

    #[op(hint(py(text_signature = "")))]
    fn no_docstring() {}

    #[op(hint(py(text_signature = "")))]
    fn docstring_empty() {}

    #[op(hint(py(text_signature = "", doc = "This docstring has no signature.")))]
    fn docstring_no_signature() {}

    #[op(hint(py(text_signature = "", doc = "docstring_with_invalid_signature($module, /, boo)\n\nThis docstring has an invalid signature.")))]
    fn docstring_with_invalid_signature() {}

    #[op(hint(py(
        text_signature = "",
        doc = "docstring_with_invalid_signature2($module, /, boo)\n\n--\n\nThis docstring also has an invalid signature."
    )))]
    fn docstring_with_invalid_signature2() {}

    #[op(hint(py(text_signature = "($module, /, sig)", doc = "This docstring has a valid signature.")))]
    fn docstring_with_signature() {}

    #[op(hint(py(text_signature = "($module, /, sig)")))]
    fn docstring_with_signature_but_no_doc() {}

    #[op(hint(py(
        text_signature = "($module, /, parameter)",
        doc = "\nThis docstring has a valid signature and some extra newlines."
    )))]
    fn docstring_with_signature_and_extra_newlines() {}

    #[op(hint(py(
        text_signature = "(module, s='avocado',\n        b=b'bytes', d=3.14, i=35, n=None, t=True, f=False,\n        local=the_number_three, sys=sys.maxsize,\n        exp=sys.maxsize - 1)",
        doc = "\n\nThis docstring has a valid signature with parameters,\nand the parameters take defaults of varying types."
    )))]
    fn docstring_with_signature_with_defaults() {}

    #[op(hint(py(text_signature = "", doc = "This is a pretty normal docstring.")))]
    fn test_with_docstring() {}

    // ---- feature macros and C self-tests ------------------------------------------------------

    #[op]
    fn get_feature_macros(it: &mut Interp) -> Value {
        let d = it.new_dict();
        for (k, v) in [("HAVE_FORK", true), ("MS_WINDOWS", false), ("PY_HAVE_THREAD_NATIVE_ID", true), ("Py_REF_DEBUG", false), ("USE_STACKCHECK", false)] {
            dict_set_str(&d, k, Value::Bool(v));
        }
        Value::Obj(d)
    }

    #[op]
    fn test_config() {}

    #[op]
    fn test_macros() {}

    #[op]
    fn test_atexit() {}

    #[op]
    fn test_current_tstate_matches() {}

    #[op]
    fn test_immortal_builtins() {}

    #[op]
    fn test_immortal_small_ints() {}

    #[op]
    fn test_long_as_size_t() {}

    #[op]
    fn W_STOPCODE(sig: i64) -> i64 {
        (sig << 8) | 0x7f
    }

    #[op]
    fn set_errno(it: &mut Interp, new_errno: i64) -> R<()> {
        let n = i32::try_from(new_errno).map_err(|_| it.overflow_err("signed integer is greater than maximum"))?;
        lumen_os::errno::set_errno(n);
        Ok(())
    }

    // ---- error state --------------------------------------------------------------------------

    // err_restore(type[, value[, traceback]]): `PyErr_Restore`, then return NULL.
    #[op]
    fn err_restore(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        match args {
            [t] => Err(restore(it, t, None, None)),
            [t, v] => Err(restore(it, t, Some(v), None)),
            [t, v, tb] => Err(restore(it, t, Some(v), Some(tb))),
            _ => Err(it.type_error("wrong number of arguments")),
        }
    }

    // err_writeunraisable(exc, obj): `PyErr_WriteUnraisable` with `exc` set as the current error.
    #[op]
    fn err_writeunraisable(it: &mut Interp, exc: &Value, obj: &Value) -> R<()> {
        match exc {
            Value::Obj(e) if matches!(e.kind, Kind::Exception(_)) => {
                let object = (!obj.is_none()).then_some(obj);
                it.write_unraisable(e, None, object);
            }
            _ => {}
        }
        Ok(())
    }

    // write_unraisable_exc(exception, err_msg, obj): `_PyErr_WriteUnraisableMsg`.
    #[op]
    fn write_unraisable_exc(it: &mut Interp, exception: &Value, err_msg: &Value, obj: &Value) -> R<()> {
        let msg = match err_msg {
            Value::None => None,
            m => match m.as_str() {
                Some(s) => Some(s.to_string()),
                None => {
                    let t = it.type_name_of(m);
                    return Err(it.type_error(&format!("bad argument type for built-in operation: {t}")));
                }
            },
        };
        let Value::Obj(e) = exception else {
            return Err(it.type_error("exceptions must derive from BaseException"));
        };
        let object = (!obj.is_none()).then_some(obj);
        it.write_unraisable(e, msg.as_deref(), object);
        Ok(())
    }

    /// return_null_without_error(): a C function returning NULL with no error set.
    #[op]
    fn return_null_without_error(it: &mut Interp) -> R<Value> {
        Err(system_error(it, "<built-in function return_null_without_error> returned NULL without setting an exception"))
    }

    /// return_result_with_error(): a C function returning a result with an error set.
    #[op]
    fn return_result_with_error(it: &mut Interp) -> R<Value> {
        let e = it.new_exc_str("ValueError", "");
        let cause = system_error(it, "<built-in function return_result_with_error> returned a result with an exception set");
        let _ = it.set_attr_str(&Value::Obj(cause.clone()), "__cause__", Value::Obj(e));
        Err(cause)
    }

    // getitem_with_error(map, key): `PyObject_GetItem` with an error already set.
    #[op]
    fn getitem_with_error(it: &mut Interp, map: &Value, key: &Value) -> R<Value> {
        let first = it.new_exc_str("ValueError", "bug");
        match it.getitem(map, key) {
            Ok(_) => Err(system_error(it, "<built-in function getitem_with_error> returned a result with an exception set")),
            Err(e) => {
                let _ = it.set_attr_str(&Value::Obj(e.clone()), "__context__", Value::Obj(first));
                Err(e)
            }
        }
    }

    // unstable_exc_prep_reraise_star(orig, excs): `PyUnstable_Exc_PrepReraiseStar`.
    #[op]
    fn unstable_exc_prep_reraise_star(it: &mut Interp, orig: &Value, excs: &Value) -> R<Value> {
        let globals = Value::Obj(it.new_dict());
        call_builtin(it, "exec", vec![Value::str(PREP_RERAISE_STAR), globals.clone()])?;
        let Value::Obj(g) = &globals else { unreachable!("a dict was just built") };
        let prep = crate::vm::dict_get_str(g, "prep").unwrap_or(Value::None);
        it.call(&prep, vec![orig.clone(), excs.clone()], Vec::new())
    }

    // raise_SIGINT_then_send_None(gen): raise SIGINT, then `gen.send(None)`.
    #[op]
    fn raise_SIGINT_then_send_None(it: &mut Interp, gen: &Value) -> R<Value> {
        let is_gen = matches!(gen, Value::Obj(o) if matches!(o.kind, Kind::Generator(_)));
        if !is_gen {
            let t = it.tp_name_of(gen);
            return Err(it.type_error(&format!("raise_SIGINT_then_send_None() argument must be generator, not {t}")));
        }
        lumen_os::signal::raise(2).map_err(|_| system_error(it, "raise(SIGINT) failed"))?;
        it.call_method(gen, "send", vec![Value::None])
    }

    // ---- NULL pointer arguments ---------------------------------------------------------------

    #[op]
    fn pyobject_repr_from_null() -> &'static str {
        "<NULL>"
    }

    #[op]
    fn pyobject_str_from_null() -> &'static str {
        "<NULL>"
    }

    #[op]
    fn pyobject_bytes_from_null() -> Value {
        Value::bytes(b"<NULL>".to_vec())
    }

    // ---- numbers, buffers ---------------------------------------------------------------------

    // pynumber_tobase(n, base): `PyNumber_ToBase`.
    #[op]
    fn pynumber_tobase(it: &mut Interp, n: &Value, base: i64) -> R<Value> {
        let name = match base {
            2 => "bin",
            8 => "oct",
            10 => "str",
            16 => "hex",
            _ => return Err(system_error(it, "PyNumber_ToBase: base must be 2, 8, 10 or 16")),
        };
        let i = index(it, n)?;
        call_builtin(it, name, vec![i])
    }

    // PyBuffer_SizeFromFormat(format): the size `struct.calcsize` computes.
    #[op]
    fn PyBuffer_SizeFromFormat(it: &mut Interp, format: &str) -> R<Value> {
        let m = it.import_module("struct")?;
        let f = it.get_attr_str(&Value::Obj(m), "calcsize")?;
        it.call(&f, vec![Value::str(format)], Vec::new())
    }

    // getbuffer_with_null_view(obj): `PyObject_GetBuffer(obj, NULL, PyBUF_SIMPLE)`.
    #[op]
    fn getbuffer_with_null_view(it: &mut Interp, obj: &Value) -> R<()> {
        call_builtin(it, "memoryview", vec![obj.clone()])?;
        Err(it.new_exc_str("BufferError", "PyObject_GetBuffer: view==NULL argument is obsolete"))
    }

    // make_memoryview_from_NULL_pointer(): `PyMemoryView_FromBuffer` of a NULL buffer.
    #[op]
    fn make_memoryview_from_NULL_pointer(it: &mut Interp) -> R<()> {
        Err(it.value_error("PyMemoryView_FromBuffer(): info->buf must not be NULL"))
    }

    // ---- calling ------------------------------------------------------------------------------

    // call_vectorcall(callable): `PyObject_Vectorcall(callable, ["foo"], 1, kwnames=("baz",))`.
    #[op]
    fn call_vectorcall(it: &mut Interp, callable: &Value) -> R<Value> {
        let key = it.str_obj("baz");
        it.call(callable, vec![Value::str("foo")], vec![(key, Value::str("bar"))])
    }

    // call_vectorcall_method(obj): `PyObject_VectorcallMethod("f", ...)`.
    #[op]
    fn call_vectorcall_method(it: &mut Interp, obj: &Value) -> R<Value> {
        let f = it.get_attr_str(obj, "f")?;
        let key = it.str_obj("baz");
        it.call(&f, vec![Value::str("foo")], vec![(key, Value::str("bar"))])
    }

    // ---- tracing ------------------------------------------------------------------------------

    // settrace_to_record(events): trace function appending `(what, line, arg)` to `events`.
    #[op]
    fn settrace_to_record(it: &mut Interp, events: &Value) -> R<()> {
        list_arg(it, events)?;
        install_trace(it, RECORD_TRACE, events)
    }

    // settrace_to_error(events): trace function raising `Exception("an exception")` once.
    #[op]
    fn settrace_to_error(it: &mut Interp, events: &Value) -> R<()> {
        list_arg(it, events)?;
        install_trace(it, ERROR_TRACE, events)
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let exc = it.exc_type("Exception");
        let recursing = crate::builtins::native::new_type(it, "_testcapi", "RecursingInfinitelyError", Some(&exc), Layout::Exception);
        crate::bind::install_all::<RecursingInit>(&recursing);
        dict_set_str(&d, "RecursingInfinitelyError", Value::Obj(recursing));
        crate::vm::dict_del_str(&d, "RecursingInit");
    }
}
