//! `_testcapi` wrappers of the exception state API (`exceptions.c`, `_testcapimodule.c`):
//! `PyErr_SetObject`, `PyErr_SetString`, `PyErr_SetFromErrnoWithFilename`,
//! `PyErr_NewExceptionWithDoc`, the handled exception and `Py_FatalError`.

use super::{builtin, call_builtin, system_error};
use crate::object::*;
use crate::vm::{dict_set_str, Interp};

fn is_exception_class(it: &mut Interp, v: &Value) -> bool {
    if !v.is_type() {
        return false;
    }
    let base = Value::Obj(it.exc_type("BaseException"));
    it.issubclass_value(v, &base).unwrap_or(false)
}

fn require_exception_class(it: &mut Interp, exc: &Value) -> R<()> {
    if is_exception_class(it, exc) {
        return Ok(());
    }
    let r = it.repr_of(exc)?;
    Err(system_error(it, &format!("_PyErr_SetObject: exception {r} is not a BaseException subclass")))
}

fn is_exception_instance(v: &Value) -> bool {
    matches!(v, Value::Obj(o) if matches!(o.kind, Kind::Exception(_)))
}

/// The exception `PyErr_SetObject(exc, value)` raises: `value` itself when it is an instance of
/// `exc`, else `exc(value)` (`exc(*value)` for a tuple, `exc()` for None).
fn exception_for(it: &mut Interp, exc: &Value, value: &Value) -> R<Obj> {
    require_exception_class(it, exc)?;
    if is_exception_instance(value) {
        let t = Value::Obj(it.type_of(value));
        let subclass = call_builtin(it, "issubclass", vec![t, exc.clone()])?;
        if let (true, Value::Obj(o)) = (it.truthy(&subclass)?, value) {
            return Ok(o.clone());
        }
    }
    let args = match value {
        Value::None => Vec::new(),
        v => match v.tuple_items() {
            Some(items) => items.to_vec(),
            None => vec![v.clone()],
        },
    };
    match it.call(exc, args, Vec::new()) {
        Ok(Value::Obj(o)) if matches!(o.kind, Kind::Exception(_)) => Ok(o),
        Ok(other) => {
            let t = it.type_name_of(&other);
            let r = it.repr_of(exc)?;
            Err(it.type_error(&format!("calling {r} should have returned an instance of BaseException, not {t}")))
        }
        Err(e) => {
            let name = it.get_attr_str(exc, "__name__").ok().and_then(|n| n.as_str().map(str::to_string)).unwrap_or_default();
            let shown = match it.repr_of(value) {
                Ok(r) => r,
                Err(_) => "<unknown>".to_string(),
            };
            let note = format!("Normalization failed: type={name} args={shown}");
            it.call_method(&Value::Obj(e.clone()), "add_note", vec![Value::string(note)])?;
            Ok(e)
        }
    }
}

fn sys_function(it: &mut Interp, name: &str) -> R<Value> {
    let sys = it.import_module("sys")?;
    it.get_attr_str(&Value::Obj(sys), name)
}

fn bytes_or_str(it: &mut Interp, v: &Value) -> R<String> {
    if let Some(s) = v.as_str() {
        return Ok(s.to_string());
    }
    let b = Value::bytes(it.bytes_from_object(v)?);
    let s = it.call_method(&b, "decode", vec![Value::str("utf-8")])?;
    Ok(s.as_str().unwrap_or_default().to_string())
}

fn extension_modules(it: &mut Interp) -> R<Vec<String>> {
    let sys = Value::Obj(it.import_module("sys")?);
    let loaded = it.get_attr_str(&sys, "modules")?;
    let builtin_names = it.get_attr_str(&sys, "builtin_module_names")?;
    let stdlib = it.get_attr_str(&sys, "stdlib_module_names")?;
    let builtin_names = it.iterate_to_vec(&builtin_names)?;
    let mut out = Vec::new();
    for name in builtin_names {
        let Some(n) = name.as_str() else { continue };
        let is_loaded = it.contains(&loaded, &name)?;
        let is_stdlib = it.contains(&stdlib, &name)?;
        if is_loaded && !is_stdlib {
            out.push(n.to_string());
        }
    }
    out.sort();
    Ok(out)
}

#[lumen_bind::module(name = "_testcapi")]
pub mod excm {
    use super::*;

    /// set_exception(exc): `PyErr_SetHandledException`; returns the previous handled exception.
    #[op]
    fn set_exception(it: &mut Interp, new_exc: &Value) -> Value {
        let old = it.handled.take();
        it.handled = new_exc.as_obj().cloned();
        old.map(Value::Obj).unwrap_or(Value::None)
    }

    /// set_exc_info(type, value, traceback): `PyErr_SetExcInfo`; returns the previous
    /// `(type, value, traceback)`.
    #[op]
    fn set_exc_info(it: &mut Interp, new_type: &Value, new_value: &Value, new_tb: &Value) -> R<Value> {
        let _ = new_type;
        let exc_info = sys_function(it, "exc_info")?;
        let old = it.call(&exc_info, Vec::new(), Vec::new())?;
        if is_exception_instance(new_value) {
            if !new_tb.is_none() {
                it.set_attr_str(new_value, "__traceback__", new_tb.clone())?;
            }
            it.handled = new_value.as_obj().cloned();
        } else {
            it.handled = None;
        }
        Ok(old)
    }

    /// err_set_raised(exc): `PyErr_SetRaisedException`, then return NULL.
    #[op]
    fn err_set_raised(it: &mut Interp, exc: &Value) -> R<Value> {
        match exc {
            Value::Obj(o) if matches!(o.kind, Kind::Exception(_)) => Err(o.clone()),
            _ => Err(system_error(it, "bad argument to internal function")),
        }
    }

    /// exc_set_object(exc, obj): `PyErr_SetObject`, then return NULL.
    #[op]
    fn exc_set_object(it: &mut Interp, exc: &Value, obj: &Value) -> R<Value> {
        Err(exception_for(it, exc, obj)?)
    }

    /// exc_set_object_fetch(exc, obj): `PyErr_SetObject` followed by `PyErr_Fetch`.
    #[op]
    fn exc_set_object_fetch(it: &mut Interp, exc: &Value, obj: &Value) -> R<Value> {
        Ok(Value::Obj(exception_for(it, exc, obj)?))
    }

    /// err_setstring(exc, message): `PyErr_SetString`, then return NULL.
    #[op]
    fn err_setstring(it: &mut Interp, exc: &Value, message: &Value) -> R<Value> {
        let text = bytes_or_str(it, message)?;
        Err(exception_for(it, exc, &Value::string(text))?)
    }

    /// err_setfromerrnowithfilename(errno, exc, filename): `PyErr_SetFromErrnoWithFilename`.
    #[op]
    fn err_setfromerrnowithfilename(it: &mut Interp, i: i64, exc: &Value, filename: &Value) -> R<Value> {
        let os = Value::Obj(it.import_module("os")?);
        let message = if i == 0 {
            Value::str("Error")
        } else {
            let strerror = it.get_attr_str(&os, "strerror")?;
            it.call(&strerror, vec![Value::Int(i)], Vec::new())?
        };
        let mut args = vec![Value::Int(i), message];
        if !filename.is_none() {
            let name = if filename.as_str().is_some() {
                filename.clone()
            } else {
                let fsdecode = it.get_attr_str(&os, "fsdecode")?;
                let raw = Value::bytes(it.bytes_from_object(filename)?);
                it.call(&fsdecode, vec![raw], Vec::new())?
            };
            args.push(name);
        }
        match it.call(exc, args, Vec::new())? {
            Value::Obj(o) if matches!(o.kind, Kind::Exception(_)) => Err(o),
            _ => Err(system_error(it, "bad argument to internal function")),
        }
    }

    /// make_exception_with_doc(name, doc=None, base=None, dict=None): `PyErr_NewExceptionWithDoc`.
    #[op]
    fn make_exception_with_doc(it: &mut Interp, name: &str, doc: Option<&Value>, base: Option<&Value>, dict: Option<&Value>) -> R<Value> {
        let Some((module, class)) = name.rsplit_once('.') else {
            return Err(system_error(it, "PyErr_NewException: name must be module.class"));
        };
        let attrs = match dict.filter(|d| !d.is_none()) {
            Some(d) => call_builtin(it, "dict", vec![d.clone()])?,
            None => Value::Obj(it.new_dict()),
        };
        let Value::Obj(attrs) = attrs else { return Err(system_error(it, "bad argument to internal function")) };
        if crate::vm::dict_get_str(&attrs, "__module__").is_none() {
            dict_set_str(&attrs, "__module__", Value::str(module));
        }
        if let Some(doc) = doc.filter(|d| !d.is_none()) {
            let text = bytes_or_str(it, doc)?;
            dict_set_str(&attrs, "__doc__", Value::string(text));
        }
        let bases = match base.filter(|b| !b.is_none()) {
            Some(b) if b.tuple_items().is_some() => b.clone(),
            Some(b) => Value::tuple(vec![b.clone()]),
            None => Value::tuple(vec![Value::Obj(it.exc_type("Exception"))]),
        };
        let type_fn = builtin(it, "type");
        it.call(&type_fn, vec![Value::str(class), bases, Value::Obj(attrs)], Vec::new())
    }

    /// raise_memoryerror(): `PyErr_NoMemory`.
    #[op]
    fn raise_memoryerror(it: &mut Interp) -> R<Value> {
        Err(it.new_exc_str("MemoryError", ""))
    }

    /// fatal_error(message, release_gil=0): `Py_FatalError` reports the message and the loaded
    /// extension modules on stderr, then aborts the process.
    #[op]
    fn fatal_error(it: &mut Interp, message: &Value, release_gil: Option<i64>) -> R<Value> {
        let _ = release_gil;
        let text = bytes_or_str(it, message)?;
        let modules = extension_modules(it)?;
        let report = format!(
            "Fatal Python error: _testcapi_fatal_error_impl: {text}\nPython runtime state: initialized\n\nExtension modules: {} (total: {})\n",
            modules.join(", "),
            modules.len()
        );
        it.write_stderr(&report);
        std::process::abort()
    }
}
