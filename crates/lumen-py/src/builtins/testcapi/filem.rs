//! `_testcapi` wrappers of `PyFile_*` (`file.c`) and the `PyMarshal_*` file functions of
//! `_testcapimodule.c`, written over the `_io`, `io` and `marshal` modules.

#![allow(non_snake_case)]

use super::evalm::{c_string, required_c_string};
use super::{call_builtin, int_value, system_error};
use crate::object::*;
use crate::vm::Interp;

fn open_binary(it: &mut Interp, filename: &Value, mode: &str) -> R<Value> {
    call_builtin(it, "open", vec![filename.clone(), Value::str(mode)])
}

fn optional_text(it: &mut Interp, v: &Value) -> R<Value> {
    Ok(match c_string(it, v)? {
        Some(s) => Value::string(s),
        None => Value::None,
    })
}

fn read_exact(it: &mut Interp, file: &Value, n: usize, what: &str) -> R<Vec<u8>> {
    let data = it.call_method(file, "read", vec![Value::Int(n as i64)])?;
    let bytes = it.bytes_from_object(&data)?;
    if bytes.len() < n {
        return Err(it.new_exc_str("EOFError", what));
    }
    Ok(bytes)
}

fn position(it: &mut Interp, file: &Value) -> R<Value> {
    it.call_method(file, "tell", Vec::new())
}

fn close(it: &mut Interp, file: &Value) -> R<()> {
    it.call_method(file, "close", Vec::new())?;
    Ok(())
}

/// `PyMarshal_ReadObjectFromFile` stops after the object; the `Last` variant reads to the end of
/// the file.
fn read_object(it: &mut Interp, filename: &Value, to_end: bool) -> R<Value> {
    let file = open_binary(it, filename, "rb")?;
    let marshal = it.import_module("marshal")?;
    let load = it.get_attr_str(&Value::Obj(marshal), "load")?;
    let obj = it.call(&load, vec![file.clone()], Vec::new());
    if to_end && obj.is_ok() {
        let _ = it.call_method(&file, "read", Vec::new());
    }
    let pos = position(it, &file);
    close(it, &file)?;
    Ok(Value::tuple(vec![obj?, pos?]))
}

/// The unbuffered writer of `PyFile_NewStdPrinter` that backs `sys.stdout`/`sys.stderr` before
/// the `io` module is available: it only writes to its descriptor.
#[lumen_bind::class(module = "builtins", name = "stderrprinter", hint(py(final)))]
pub struct StdPrinter {
    fd: i64,
}

#[lumen_bind::methods]
impl StdPrinter {
    #[getter]
    fn closed(&self) -> bool {
        false
    }

    #[getter]
    fn encoding(&self) -> Value {
        Value::None
    }

    #[getter]
    fn mode(&self) -> &'static str {
        "w"
    }

    fn fileno(&self) -> i64 {
        self.fd
    }

    fn isatty(&self, it: &mut Interp) -> R<Value> {
        let os = Value::Obj(it.import_module("os")?);
        it.call_method(&os, "isatty", vec![Value::Int(self.fd)])
    }

    fn flush(&self) {}

    fn close(&self) {}

    fn write(&self, it: &mut Interp, text: &Value) -> R<Value> {
        if text.as_str().is_none() {
            let t = it.type_name_of(text);
            return Err(it.type_error(&format!("write() argument must be str, not {t}")));
        }
        let data = it.call_method(text, "encode", vec![Value::str("utf-8"), Value::str("backslashreplace")])?;
        let os = Value::Obj(it.import_module("os")?);
        it.call_method(&os, "write", vec![Value::Int(self.fd), data])
    }
}

#[lumen_bind::module(name = "_testcapi")]
pub mod filem {
    use super::*;

    /// pyfile_fromfd(fd, name, mode, buffering, encoding, errors, newline, closefd): `PyFile_FromFd`.
    #[op(hint(py(arg_style = "parse", arg_name = "pyfile_fromfd")))]
    #[allow(clippy::too_many_arguments)]
    fn pyfile_fromfd(it: &mut Interp, fd: i64, name: &Value, mode: &Value, buffering: i64, encoding: &Value, errors: &Value, newline: &Value, closefd: i64) -> R<Value> {
        let _ = name;
        let io = it.import_module("_io")?;
        let open = it.get_attr_str(&Value::Obj(io), "open")?;
        let mode = optional_text(it, mode)?;
        let encoding = optional_text(it, encoding)?;
        let errors = optional_text(it, errors)?;
        let newline = optional_text(it, newline)?;
        let args = vec![Value::Int(fd), mode, Value::Int(buffering), encoding, errors, newline, Value::Bool(closefd != 0)];
        it.call(&open, args, Vec::new())
    }

    /// pyfile_writestring(str, file): `PyFile_WriteString`.
    #[op(hint(py(arg_style = "parse", arg_name = "pyfile_writestring")))]
    fn pyfile_writestring(it: &mut Interp, text: &Value, file: &Value) -> R<i64> {
        let s = required_c_string(it, text)?;
        if file.is_none() {
            return Err(system_error(it, "null file for PyFile_WriteString"));
        }
        it.call_method(file, "write", vec![Value::string(s)])?;
        Ok(0)
    }

    /// pyfile_getline(file, n, /): `PyFile_GetLine`.
    #[op]
    fn pyfile_getline(it: &mut Interp, file: &Value, n: i64) -> R<Value> {
        let args = if n > 0 { vec![Value::Int(n)] } else { Vec::new() };
        let line = it.call_method(file, "readline", args)?;
        let is_bytes = matches!(&line, Value::Obj(o) if matches!(o.kind, Kind::Bytes(_)));
        if !is_bytes && line.as_str().is_none() {
            return Err(it.type_error("object.readline() returned non-string"));
        }
        if n >= 0 {
            return Ok(line);
        }
        if is_bytes {
            let mut b = it.bytes_from_object(&line)?;
            match b.last() {
                None => return Err(it.new_exc_str("EOFError", "EOF when reading a line")),
                Some(b'\n') => {
                    b.pop();
                    return Ok(Value::bytes(b));
                }
                Some(_) => return Ok(line),
            }
        }
        let s = line.as_str().unwrap_or_default();
        if s.is_empty() {
            return Err(it.new_exc_str("EOFError", "EOF when reading a line"));
        }
        match s.strip_suffix('\n') {
            Some(stripped) => Ok(Value::str(stripped)),
            None => Ok(line),
        }
    }

    /// pyfile_writeobject(obj, file, flags, /): `PyFile_WriteObject`.
    #[op]
    fn pyfile_writeobject(it: &mut Interp, obj: &Value, file: &Value, flags: i64) -> R<i64> {
        if file.is_none() {
            return Err(it.type_error("writeobject with NULL file"));
        }
        let writer = it.get_attr_str(file, "write")?;
        let text = if obj.is_none() {
            "<NULL>".to_string()
        } else if flags & 1 != 0 {
            it.str_of(obj)?
        } else {
            it.repr_of(obj)?
        };
        it.call(&writer, vec![Value::string(text)], Vec::new())?;
        Ok(0)
    }

    /// pyobject_asfiledescriptor(obj, /): `PyObject_AsFileDescriptor`.
    #[op]
    fn pyobject_asfiledescriptor(it: &mut Interp, obj: &Value) -> R<i64> {
        let fd = if obj.is_int_like() {
            obj.clone()
        } else {
            let fileno = match it.get_attr_str(obj, "fileno") {
                Ok(f) => f,
                Err(e) if it.exc_is(&e, "AttributeError") => {
                    return Err(it.type_error("argument must be an int, or have a fileno() method."));
                }
                Err(e) => return Err(e),
            };
            let v = it.call(&fileno, Vec::new(), Vec::new())?;
            if !v.is_int_like() {
                return Err(it.type_error("fileno() returned a non-integer"));
            }
            v
        };
        let Some(n) = fd.as_i64().and_then(|n| i32::try_from(n).ok()) else {
            return Err(it.overflow_err("Python int too large to convert to C int"));
        };
        if n < 0 {
            return Err(it.value_error(&format!("file descriptor cannot be a negative integer ({n})")));
        }
        Ok(i64::from(n))
    }

    /// pyfile_newstdprinter(fd, /): `PyFile_NewStdPrinter`.
    #[op]
    fn pyfile_newstdprinter(it: &mut Interp, fd: i64) -> R<Value> {
        if fd != 1 && fd != 2 {
            return Err(system_error(it, "bad argument to internal function"));
        }
        Ok(crate::bind::Py::new(it, StdPrinter { fd }).into_value())
    }

    /// pymarshal_write_long_to_file(value, filename, version)
    #[op(hint(py(arg_style = "parse", arg_name = "pymarshal_write_long_to_file")))]
    fn pymarshal_write_long_to_file(it: &mut Interp, value: i64, filename: &Value, version: i64) -> R<()> {
        let _ = version;
        let file = open_binary(it, filename, "wb")?;
        let written = it.call_method(&file, "write", vec![Value::bytes((value as u32).to_le_bytes().to_vec())]);
        close(it, &file)?;
        written.map(|_| ())
    }

    /// pymarshal_write_object_to_file(obj, filename, version)
    #[op(hint(py(arg_style = "parse", arg_name = "pymarshal_write_object_to_file")))]
    fn pymarshal_write_object_to_file(it: &mut Interp, obj: &Value, filename: &Value, version: i64) -> R<()> {
        let marshal = it.import_module("marshal")?;
        let dumps = it.get_attr_str(&Value::Obj(marshal), "dumps")?;
        let data = it.call(&dumps, vec![obj.clone(), Value::Int(version)], Vec::new())?;
        let file = open_binary(it, filename, "wb")?;
        let written = it.call_method(&file, "write", vec![data]);
        close(it, &file)?;
        written.map(|_| ())
    }

    /// pymarshal_read_short_from_file(filename) -> (value, position)
    #[op(hint(py(arg_style = "parse", arg_name = "pymarshal_read_short_from_file")))]
    fn pymarshal_read_short_from_file(it: &mut Interp, filename: &Value) -> R<Value> {
        let file = open_binary(it, filename, "rb")?;
        let read = read_exact(it, &file, 2, "EOF read where not expected");
        let pos = position(it, &file);
        close(it, &file)?;
        let b = read?;
        let value = i16::from_le_bytes([b[0], b[1]]);
        Ok(Value::tuple(vec![Value::Int(i64::from(value)), pos?]))
    }

    /// pymarshal_read_long_from_file(filename) -> (value, position)
    #[op(hint(py(arg_style = "parse", arg_name = "pymarshal_read_long_from_file")))]
    fn pymarshal_read_long_from_file(it: &mut Interp, filename: &Value) -> R<Value> {
        let file = open_binary(it, filename, "rb")?;
        let read = read_exact(it, &file, 4, "EOF read where not expected");
        let pos = position(it, &file);
        close(it, &file)?;
        let b = read?;
        let value = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        Ok(Value::tuple(vec![int_value(i128::from(value)), pos?]))
    }

    /// pymarshal_read_object_from_file(filename) -> (object, position)
    #[op(hint(py(arg_style = "parse", arg_name = "pymarshal_read_object_from_file")))]
    fn pymarshal_read_object_from_file(it: &mut Interp, filename: &Value) -> R<Value> {
        read_object(it, filename, false)
    }

    /// pymarshal_read_last_object_from_file(filename) -> (object, position)
    #[op(hint(py(arg_style = "parse", arg_name = "pymarshal_read_last_object_from_file")))]
    fn pymarshal_read_last_object_from_file(it: &mut Interp, filename: &Value) -> R<Value> {
        read_object(it, filename, true)
    }
}
