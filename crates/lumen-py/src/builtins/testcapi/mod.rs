//! `_testcapi`: the part of CPython's C API test module that is observable without a C API.
//! Wrappers stand in for the C functions (`None` is a `NULL` pointer); the module is assembled
//! from several declarations, see [`crate::bind::install_module`].

mod abstractm;
mod containers;
mod getargsm;
mod numbersm;
mod unicodem;
mod watchersm;
mod evalm;
mod typesm;
mod miscm;
mod timecapi;
mod filem;
mod memm;
mod threadsm;
mod structmembersm;
mod hamtm;
mod excm;
mod extram;
pub(super) mod internal;

use crate::object::*;
use crate::pyint::BigInt;
use crate::vm::{dict_get_str, dict_set_str, Interp};

fn system_error(it: &mut Interp, msg: &str) -> Obj {
    it.new_exc_str("SystemError", msg)
}

fn bad_internal_call(it: &mut Interp) -> Obj {
    system_error(it, "bad argument to internal function")
}

/// `NULLABLE` arguments: a `None` is a NULL pointer, which the C API rejects.
fn nonnull<'a>(it: &mut Interp, v: &'a Value) -> R<&'a Value> {
    if v.is_none() {
        return Err(system_error(it, "null argument to internal routine"));
    }
    Ok(v)
}

fn builtin(it: &Interp, name: &str) -> Value {
    dict_get_str(&it.builtins, name).unwrap_or(Value::None)
}

fn call_builtin(it: &mut Interp, name: &str, args: Vec<Value>) -> R<Value> {
    let f = builtin(it, name);
    it.call(&f, args, Vec::new())
}

fn int_value(n: i128) -> Value {
    Value::big(BigInt::from_i128(n))
}

fn complex_value(re: f64, im: f64) -> Value {
    Value::Obj(Object::new(Kind::Complex(re, im)))
}

fn pointer_of(it: &Interp, v: &Value) -> usize {
    it.id_of(v)
}

fn object_at(_it: &Interp, addr: usize) -> Value {
    int_value(addr as i128)
}

fn name_obj(it: &mut Interp, v: &Value) -> R<Obj> {
    match v {
        Value::Obj(o) if matches!(o.kind, Kind::Str(_)) => Ok(o.clone()),
        _ => {
            let t = it.type_name_of(v);
            Err(it.type_error(&format!("attribute name must be string, not '{t}'")))
        }
    }
}

fn int_const(d: &Obj, name: &str, v: i128) {
    dict_set_str(d, name, Value::big(BigInt::from_i128(v)));
}

#[lumen_bind::module(name = "_testcapi")]
pub mod _testcapi {
    use super::*;

    #[constant(name = "Py_single_input")]
    const PY_SINGLE_INPUT: i64 = 256;
    #[constant(name = "Py_file_input")]
    const PY_FILE_INPUT: i64 = 257;
    #[constant(name = "Py_eval_input")]
    const PY_EVAL_INPUT: i64 = 258;
    #[constant(name = "the_number_three")]
    const THE_NUMBER_THREE: i64 = 3;
    #[constant(name = "Py_Version")]
    const PY_VERSION: i64 = 0x030E08F0;
    #[constant(name = "WITH_PYMALLOC")]
    const WITH_PYMALLOC: bool = false;
    #[constant(name = "LIMITED_API_AVAILABLE")]
    const LIMITED_API_AVAILABLE: bool = false;
    #[constant(name = "ALIGNOF_MAX_ALIGN_T")]
    const ALIGNOF_MAX_ALIGN_T: i64 = 16;
    #[constant(name = "INT_MAX")]
    const INT_MAX: i64 = i32::MAX as i64;
    #[constant(name = "INT_MIN")]
    const INT_MIN: i64 = i32::MIN as i64;
    #[constant(name = "UINT_MAX")]
    const UINT_MAX: i64 = u32::MAX as i64;
    #[constant(name = "SHRT_MAX")]
    const SHRT_MAX: i64 = i16::MAX as i64;
    #[constant(name = "SHRT_MIN")]
    const SHRT_MIN: i64 = i16::MIN as i64;
    #[constant(name = "USHRT_MAX")]
    const USHRT_MAX: i64 = u16::MAX as i64;
    #[constant(name = "CHAR_MAX")]
    const CHAR_MAX: i64 = i8::MAX as i64;
    #[constant(name = "CHAR_MIN")]
    const CHAR_MIN: i64 = i8::MIN as i64;
    #[constant(name = "UCHAR_MAX")]
    const UCHAR_MAX: i64 = u8::MAX as i64;
    #[constant(name = "LONG_MAX")]
    const LONG_MAX: i64 = i64::MAX;
    #[constant(name = "LONG_MIN")]
    const LONG_MIN: i64 = i64::MIN;
    #[constant(name = "LLONG_MAX")]
    const LLONG_MAX: i64 = i64::MAX;
    #[constant(name = "LLONG_MIN")]
    const LLONG_MIN: i64 = i64::MIN;
    #[constant(name = "PY_SSIZE_T_MAX")]
    const PY_SSIZE_T_MAX: i64 = i64::MAX;
    #[constant(name = "PY_SSIZE_T_MIN")]
    const PY_SSIZE_T_MIN: i64 = i64::MIN;
    #[constant(name = "SIZEOF_TIME_T")]
    const SIZEOF_TIME_T: i64 = 8;
    #[constant(name = "SIZEOF_WCHAR_T")]
    const SIZEOF_WCHAR_T: i64 = 4;
    #[constant(name = "SIZEOF_VOID_P")]
    const SIZEOF_VOID_P: i64 = 8;
    #[constant(name = "SIZEOF_LONG")]
    const SIZEOF_LONG: i64 = 8;
    #[constant(name = "SIZEOF_LONG_LONG")]
    const SIZEOF_LONG_LONG: i64 = 8;
    #[constant(name = "FLT_MAX")]
    const FLT_MAX: f64 = f32::MAX as f64;
    #[constant(name = "FLT_MIN")]
    const FLT_MIN: f64 = f32::MIN_POSITIVE as f64;
    #[constant(name = "DBL_MAX")]
    const DBL_MAX: f64 = f64::MAX;
    #[constant(name = "DBL_MIN")]
    const DBL_MIN: f64 = f64::MIN_POSITIVE;

    // raise_exception(exc, args): instantiate `exc(*args)` and raise it.
    #[op]
    fn raise_exception(it: &mut Interp, exc: &Value, args: &Value) -> R<Value> {
        let Some(items) = args.tuple_items() else {
            return Err(it.type_error("raise_exception() argument 2 must be tuple"));
        };
        let inst = it.call(exc, items.to_vec(), Vec::new())?;
        match inst {
            Value::Obj(o) => Err(o),
            _ => Err(it.type_error("exceptions must derive from BaseException")),
        }
    }

    // exception_print(exc, legacy=0): print the exception and its traceback to `sys.stderr`.
    #[op]
    fn exception_print(it: &mut Interp, exc: &Value, legacy: Option<i64>) -> R<()> {
        let _ = legacy;
        let tb = it.import_module("traceback")?;
        let f = it.get_attr_str(&Value::Obj(tb), "print_exception")?;
        it.call(&f, vec![exc.clone()], Vec::new())?;
        Ok(())
    }

    // traceback_print(tb, file): print a traceback object to `file`.
    #[op]
    fn traceback_print(it: &mut Interp, tb: &Value, file: &Value) -> R<()> {
        let m = it.import_module("traceback")?;
        let f = it.get_attr_str(&Value::Obj(m), "print_tb")?;
        it.call(&f, vec![tb.clone(), Value::None, file.clone()], Vec::new())?;
        Ok(())
    }

    /// get_recursion_depth() -> int
    #[op]
    fn get_recursion_depth(it: &mut Interp) -> i64 {
        it.frames.len() as i64
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        int_const(&d, "ULONG_MAX", i128::from(u64::MAX));
        int_const(&d, "ULLONG_MAX", i128::from(u64::MAX));
        int_const(&d, "SIZE_MAX", i128::from(u64::MAX));
        let exc = it.exc_type("Exception");
        let error = crate::builtins::native::new_type(it, "_testcapi", "error", Some(&exc), Layout::Exception);
        dict_set_str(&d, "error", Value::Obj(error));
        let _ = crate::bind::install_module::<abstractm::abstractm::Module>(it, m);
        let _ = crate::bind::install_module::<containers::containers::Module>(it, m);
        let _ = crate::bind::install_module::<numbersm::numbersm::Module>(it, m);
        let _ = crate::bind::install_module::<unicodem::unicodem::Module>(it, m);
        let _ = crate::bind::install_module::<getargsm::getargsm::Module>(it, m);
        let _ = crate::bind::install_module::<watchersm::watchersm::Module>(it, m);
        let _ = crate::bind::install_module::<evalm::evalm::Module>(it, m);
        let _ = crate::bind::install_module::<typesm::typesm::Module>(it, m);
        let _ = crate::bind::install_module::<miscm::miscm::Module>(it, m);
        let _ = crate::bind::install_module::<timecapi::timecapi::Module>(it, m);
        let _ = crate::bind::install_module::<filem::filem::Module>(it, m);
        let _ = crate::bind::install_module::<memm::memm::Module>(it, m);
        let _ = crate::bind::install_module::<threadsm::threadsm::Module>(it, m);
        let _ = crate::bind::install_module::<structmembersm::structmembersm::Module>(it, m);
        let _ = crate::bind::install_module::<hamtm::hamtm::Module>(it, m);
        let _ = crate::bind::install_module::<excm::excm::Module>(it, m);
        let _ = crate::bind::install_module::<extram::extram::Module>(it, m);
    }
}
