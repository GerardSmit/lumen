//! `_io`: the layered stream stack behind `io` and `open()`. Raw file descriptors go through the
//! [`Platform`](crate::platform::Platform) layer; buffering, text decoding and newline
//! translation follow CPython's `Modules/_io` (and `Lib/_pyio.py`, its reference).

pub mod base;
pub mod buffered;
pub mod bytesio;
pub mod fileio;
pub mod stringio;
pub mod textio;

use crate::bind::Py;
use crate::object::*;
use crate::vm::Interp;
use lumen_bind::{Class, Methods};

pub use crate::bind::PyHost;

pub const DEFAULT_BUFFER_SIZE: usize = 8192;

struct UnsupportedOperationTy;

/// `io.UnsupportedOperation` (a subclass of `OSError` and `ValueError`).
pub fn unsupported_type(it: &mut Interp) -> Obj {
    let key = std::any::TypeId::of::<UnsupportedOperationTy>();
    if let Some(t) = it.native_types.get(&key) {
        return t.clone();
    }
    let bases = Value::tuple(vec![Value::Obj(it.exc_type("OSError")), Value::Obj(it.exc_type("ValueError"))]);
    let ns = it.new_dict();
    crate::vm::dict_set_str(&ns, "__module__", Value::str("io"));
    let meta = it.types.type_.clone();
    let t = match it.type_new_from_args(meta, &[Value::str("UnsupportedOperation"), bases, Value::Obj(ns)], Vec::new()) {
        Ok(Value::Obj(t)) => t,
        _ => it.exc_type("OSError"),
    };
    it.native_types.insert(key, t.clone());
    t
}

pub fn unsupported(it: &mut Interp, msg: &str) -> Obj {
    let t = unsupported_type(it);
    it.new_exc(&t, vec![Value::str(msg)])
}

pub fn closed_error(it: &mut Interp) -> Obj {
    it.value_error("I/O operation on closed file.")
}

/// `getattr(v, name)`, or `None` when it has no such attribute.
pub fn getattr_opt(it: &mut Interp, v: &Value, name: &str) -> R<Option<Value>> {
    match it.get_attr_str(v, name) {
        Ok(x) => Ok(Some(x)),
        Err(e) if it.exc_is(&e, "AttributeError") => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn call(it: &mut Interp, v: &Value, name: &str, args: Vec<Value>) -> R<Value> {
    it.call_method(v, name, args)
}

pub fn call_bool(it: &mut Interp, v: &Value, name: &str) -> R<bool> {
    let r = it.call_method(v, name, Vec::new())?;
    it.truthy(&r)
}

pub fn attr_bool(it: &mut Interp, v: &Value, name: &str) -> R<bool> {
    let r = it.get_attr_str(v, name)?;
    it.truthy(&r)
}

/// Raises `ValueError` when `v.closed` is true.
pub fn check_closed(it: &mut Interp, v: &Value) -> R<()> {
    if attr_bool(it, v, "closed")? {
        return Err(closed_error(it));
    }
    Ok(())
}

/// The bytes of a `bytes`/`bytearray`/`memoryview` result; `None` for `None`.
pub fn bytes_result(it: &mut Interp, v: &Value, what: &str) -> R<Option<Vec<u8>>> {
    match v {
        Value::None => Ok(None),
        Value::Obj(o) if matches!(o.kind, Kind::Bytes(_) | Kind::ByteArray(_)) => it.bytes_of(v).map(Some),
        Value::Obj(o) if matches!(o.kind, Kind::Opaque(_)) && crate::builtins::memview::is_buffer_object(it, v) => it.bytes_of(v).map(Some),
        _ => {
            let t = it.type_name_of(v);
            Err(it.type_error(&format!("{}() should have returned a bytes-like object, not '{}'", what, t)))
        }
    }
}

/// A `size` argument: `None` is -1, anything else goes through `__index__`.
pub fn size_arg(it: &mut Interp, v: Option<&Value>) -> R<i64> {
    match v {
        None | Some(Value::None) => Ok(-1),
        Some(v) if it.has_index(v) => it.index_of(v),
        Some(v) => {
            let t = it.type_name_of(v);
            Err(it.type_error(&format!("argument should be integer or None, not '{}'", t)))
        }
    }
}

/// `v` as an instance of exactly the native class `T` (not a Python subclass, which may
/// override methods), for the direct Rust paths between layers.
pub fn exact<T: Class + Methods<PyHost>>(it: &mut Interp, v: &Value) -> Option<Py<T>> {
    // The only class that is not a Python-defined (heap) class and holds a `T` is `T`'s own.
    let Value::Obj(o) = v else { return None };
    if it.is_heap(o.cls.as_ref()?) {
        return None;
    }
    Py::from_value(it, v)
}

/// `warnings.warn(msg, category, stacklevel)`.
/// CPython's UTF-8 mode: `PYTHONUTF8`, else on under the C/POSIX locale (PEP 538/540).
pub fn utf8_mode(it: &mut Interp) -> bool {
    let p = it.platform.borrow();
    match p.env_var("PYTHONUTF8").as_deref() {
        Some("1") => return true,
        Some("0") => return false,
        _ => {}
    }
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"].iter().find_map(|k| p.env_var(k).filter(|v| !v.is_empty()));
    matches!(locale.as_deref(), None | Some("C" | "POSIX"))
}

/// `locale.getencoding()`, or `utf-8` in UTF-8 mode (the encoding of `encoding="locale"`).
pub fn locale_encoding(it: &mut Interp) -> &'static str {
    if utf8_mode(it) {
        "utf-8"
    } else {
        "UTF-8"
    }
}

/// `_PyErr_ChainExceptions1`: `later.__context__ = earlier`.
pub fn chain(later: &Obj, earlier: &Obj) {
    if let Kind::Exception(d) = &later.kind {
        let mut d = d.borrow_mut();
        if d.context.is_none() {
            d.context = Some(earlier.clone());
        }
    }
}

fn eagain() -> i32 {
    lumen_os::errno::errno_of_code("EAGAIN").unwrap_or(35)
}

pub fn is_eagain(e: &crate::platform::IoError) -> bool {
    e.errno == eagain()
}

// The `_io` module: classes live in the sibling files and are registered by the init hook.
#[lumen_bind::module(name = "_io")]
pub mod _io {
    use super::base::{BufferedIOBase, IOBase, RawIOBase, TextIOBase};
    use super::buffered::{BufferedRWPair, BufferedRandom, BufferedReader, BufferedWriter};
    use super::bytesio::BytesIO;
    use super::fileio::FileIO;
    use super::stringio::StringIO;
    use super::textio::{IncrementalNewlineDecoder, TextIOWrapper};
    use crate::bind::type_object;
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};

    #[constant(name = "DEFAULT_BUFFER_SIZE")]
    const DEFAULT_BUFFER_SIZE: i64 = super::DEFAULT_BUFFER_SIZE as i64;

    /// Open file and return a stream.  Raise OSError upon failure.
    #[op]
    #[allow(clippy::too_many_arguments)]
    fn open(
        it: &mut Interp,
        #[kw] file: &Value,
        #[kw]
        #[default("r")]
        mode: &str,
        #[kw]
        #[default(-1)]
        buffering: i64,
        #[kw] encoding: Option<&str>,
        #[kw] errors: Option<&str>,
        #[kw] newline: Option<&str>,
        #[kw]
        #[default(true)]
        closefd: bool,
        #[kw] opener: Option<&Value>,
    ) -> R<Value> {
        super::open_impl(it, file, mode, buffering, encoding, errors, newline, closefd, opener)
    }

    /// Opens the provided file with the intent to import the contents.
    #[op]
    fn open_code(it: &mut Interp, #[kw] path: &str) -> R<Value> {
        super::open_impl(it, &Value::str(path), "rb", -1, None, None, None, true, None)
    }

    /// A helper function to choose the text encoding.
    #[op]
    fn text_encoding(it: &mut Interp, encoding: &Value, #[default(2)] stacklevel: i64) -> Value {
        let _ = stacklevel;
        match encoding {
            Value::None if super::utf8_mode(it) => Value::str("utf-8"),
            Value::None => Value::str("locale"),
            v => v.clone(),
        }
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let iobase = type_object::<IOBase>(it);
        let raw = type_object::<RawIOBase>(it);
        let buffered = type_object::<BufferedIOBase>(it);
        let text = type_object::<TextIOBase>(it);
        for t in [&raw, &buffered, &text] {
            it.set_bases(t, vec![iobase.clone()]);
        }
        let classes = [
            ("FileIO", type_object::<FileIO>(it), &raw),
            ("BytesIO", type_object::<BytesIO>(it), &buffered),
            ("BufferedReader", type_object::<BufferedReader>(it), &buffered),
            ("BufferedWriter", type_object::<BufferedWriter>(it), &buffered),
            ("BufferedRandom", type_object::<BufferedRandom>(it), &buffered),
            ("BufferedRWPair", type_object::<BufferedRWPair>(it), &buffered),
            ("TextIOWrapper", type_object::<TextIOWrapper>(it), &text),
            ("StringIO", type_object::<StringIO>(it), &text),
        ];
        let d = it.module_dict(m);
        for (name, t, base) in classes {
            it.set_bases(&t, vec![(*base).clone()]);
            dict_set_str(&d, name, Value::Obj(t));
        }
        for (name, t) in [("_IOBase", iobase), ("_RawIOBase", raw), ("_BufferedIOBase", buffered), ("_TextIOBase", text)] {
            dict_set_str(&d, name, Value::Obj(t));
        }
        let nl = type_object::<IncrementalNewlineDecoder>(it);
        dict_set_str(&d, "IncrementalNewlineDecoder", Value::Obj(nl));
        let unsupported = super::unsupported_type(it);
        dict_set_str(&d, "UnsupportedOperation", Value::Obj(unsupported));
        dict_set_str(&d, "BlockingIOError", Value::Obj(it.exc_type("BlockingIOError")));
        if let Some(open) = crate::vm::dict_get_str(&d, "open") {
            dict_set_str(&it.builtins.clone(), "open", open);
        }
    }
}

/// `sys.stdin`, `sys.stdout` and `sys.stderr` (and their `__x__` originals), as CPython's
/// `create_stdio`: text over buffered `FileIO`s on descriptors 0, 1 and 2.
pub fn init_std_streams(it: &mut Interp, sys: &Obj) {
    if crate::bind::module_object::<_io::Module>(it).is_err() {
        return;
    }
    let d = it.module_dict(sys);
    for (fd, attr) in [(0, "stdin"), (1, "stdout"), (2, "stderr")] {
        let v = std_stream(it, fd, attr).unwrap_or(Value::None);
        crate::vm::dict_set_str(&d, attr, v.clone());
        crate::vm::dict_set_str(&d, &format!("__{}__", attr), v);
    }
}

fn std_stream(it: &mut Interp, fd: i32, attr: &str) -> R<Value> {
    let write = fd != 0;
    let raw = fileio::new_fileio(it, &Value::Int(fd as i64), if write { "wb" } else { "rb" }, false, None)?;
    it.set_attr_str(&raw, "name", Value::string(format!("<{}>", attr)))?;
    let tty = it.platform.borrow_mut().fd_isatty(fd);
    let mode = if write { buffered::Mode::Writer } else { buffered::Mode::Reader };
    let buffer = buffered::new_buffered(it, mode, raw, DEFAULT_BUFFER_SIZE)?;
    let errors = if fd == 2 {
        "backslashreplace"
    } else if utf8_mode(it) {
        "surrogateescape"
    } else {
        "strict"
    };
    let text = textio::new_textio(it, buffer, Some("utf-8"), Some(errors), Some("\n"), tty || fd == 2, false)?;
    it.set_attr_str(&text, "mode", Value::str(if write { "w" } else { "r" }))?;
    Ok(text)
}

/// CPython's `flush_std_files` at exit: errors are ignored.
pub fn flush_std_streams(it: &mut Interp) {
    for name in ["stdout", "stderr"] {
        if let Some(f) = it.sys_attr(name) {
            if !f.is_none() && !attr_bool(it, &f, "closed").unwrap_or(true) {
                let _ = it.call_method(&f, "flush", Vec::new());
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn open_impl(
    it: &mut Interp,
    file: &Value,
    mode: &str,
    buffering: i64,
    encoding: Option<&str>,
    errors: Option<&str>,
    newline: Option<&str>,
    closefd: bool,
    opener: Option<&Value>,
) -> R<Value> {
    let file = if file.is_int_like() && !matches!(file, Value::Bool(_)) { file.clone() } else { crate::bind::fspath(it, file)? };
    let mut seen = std::collections::BTreeSet::new();
    let ok = mode.chars().all(|c| "axrwb+t".contains(c) && seen.insert(c));
    if !ok {
        let r = it.repr_of(&Value::str(mode))?;
        return Err(it.value_error(&format!("invalid mode: {}", r)));
    }
    let has = |c: char| seen.contains(&c);
    let (creating, reading, writing, appending, updating, text, binary) =
        (has('x'), has('r'), has('w'), has('a'), has('+'), has('t'), has('b'));
    if text && binary {
        return Err(it.value_error("can't have text and binary mode at once"));
    }
    if creating as u8 + reading as u8 + writing as u8 + appending as u8 > 1 {
        return Err(it.value_error("must have exactly one of create/read/write/append mode"));
    }
    if !(creating || reading || writing || appending) {
        return Err(it.value_error("Must have exactly one of create/read/write/append mode and at most one plus"));
    }
    if binary && encoding.is_some() {
        return Err(it.value_error("binary mode doesn't take an encoding argument"));
    }
    if binary && errors.is_some() {
        return Err(it.value_error("binary mode doesn't take an errors argument"));
    }
    if binary && newline.is_some() {
        return Err(it.value_error("binary mode doesn't take a newline argument"));
    }
    if binary && buffering == 1 {
        let msg = "line buffering (buffering=1) isn't supported in binary mode, the default buffer size will be used";
        crate::builtins::warningsm::warn_category(it, "RuntimeWarning", msg, 1)?;
    }
    let mut rawmode = String::new();
    for (on, c) in [(creating, 'x'), (reading, 'r'), (writing, 'w'), (appending, 'a'), (updating, '+')] {
        if on {
            rawmode.push(c);
        }
    }
    let raw = fileio::new_fileio(it, &file, &rawmode, closefd, opener)?;
    let result = (|| -> R<Value> {
        let mut buffering = buffering;
        let mut line_buffering = false;
        if buffering == 1 || (buffering < 0 && call_bool(it, &raw, "isatty")?) {
            buffering = -1;
            line_buffering = true;
        }
        if buffering < 0 {
            let bs = fileio::blksize(it, &raw);
            buffering = if bs > 1 { bs } else { DEFAULT_BUFFER_SIZE as i64 };
        }
        if buffering == 0 {
            if binary {
                return Ok(raw.clone());
            }
            return Err(it.value_error("can't have unbuffered text I/O"));
        }
        let kind = if updating {
            buffered::Mode::Random
        } else if creating || writing || appending {
            buffered::Mode::Writer
        } else {
            buffered::Mode::Reader
        };
        let buffer = buffered::new_buffered(it, kind, raw.clone(), buffering as usize)?;
        if binary {
            return Ok(buffer);
        }
        let enc = encoding.unwrap_or("locale");
        let text = textio::new_textio(it, buffer.clone(), Some(enc), errors, newline, line_buffering, false);
        let text = match text {
            Ok(t) => t,
            Err(e) => {
                let _ = it.call_method(&buffer, "close", Vec::new());
                return Err(e);
            }
        };
        it.set_attr_str(&text, "mode", Value::str(mode))?;
        Ok(text)
    })();
    match result {
        Ok(v) => Ok(v),
        Err(e) => {
            let _ = it.call_method(&raw, "close", Vec::new());
            Err(e)
        }
    }
}
