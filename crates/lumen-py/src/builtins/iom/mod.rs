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

pub const DEFAULT_BUFFER_SIZE: usize = 128 * 1024;

/// The largest `st_blksize` that sizes the buffer of `open()`.
const MAX_BLKSIZE: i64 = 8 * 1024 * 1024;

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

/// `warnings.warn(msg, ResourceWarning, stacklevel=1, source=source)`.
pub fn resource_warning(it: &mut Interp, source: &Value, msg: &str) -> R<()> {
    let warnings = it.import_module("_warnings")?;
    let warn = it.get_attr_str(&Value::Obj(warnings), "warn")?;
    let category = Value::Obj(it.exc_type("ResourceWarning"));
    let kw = vec![(it.str_obj("source"), source.clone())];
    it.call(&warn, vec![Value::str(msg), category, Value::Int(1)], kw)?;
    Ok(())
}

/// Calls `target._dealloc_warn(source)` of a stream being finalized, ignoring failures.
pub fn dealloc_warn(it: &mut Interp, target: &Value, source: &Value) {
    let _ = it.call_method(target, "_dealloc_warn", vec![source.clone()]);
}

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
/// The io module provides the Python interfaces to stream handling. The
/// builtin open function is defined in this module.
///
/// At the top of the I/O hierarchy is the abstract base class IOBase. It
/// defines the basic interface to a stream. Note, however, that there is no
/// separation between reading and writing to streams; implementations are
/// allowed to raise an OSError if they do not support a given operation.
///
/// Extending IOBase is RawIOBase which deals simply with the reading and
/// writing of raw bytes to a stream. FileIO subclasses RawIOBase to provide
/// an interface to OS files.
///
/// BufferedIOBase deals with buffering on a raw byte stream (RawIOBase). Its
/// subclasses, BufferedWriter, BufferedReader, and BufferedRWPair buffer
/// streams that are readable, writable, and both respectively.
/// BufferedRandom provides a buffered interface to random access
/// streams. BytesIO is a simple stream of in-memory bytes.
///
/// Another IOBase subclass, TextIOBase, deals with the encoding and decoding
/// of streams into text. TextIOWrapper, which extends it, is a buffered text
/// interface to a buffered raw stream (`BufferedIOBase`). Finally, StringIO
/// is an in-memory stream for text.
///
/// Argument names are not part of the specification, and only the arguments
/// of open() are intended to be used as keyword arguments.
///
/// data:
///
/// DEFAULT_BUFFER_SIZE
///
///    An int containing the default buffer size used by the module's buffered
///    I/O classes.
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
    ///
    /// file is either a text or byte string giving the name (and the path
    /// if the file isn't in the current working directory) of the file to
    /// be opened or an integer file descriptor of the file to be
    /// wrapped. (If a file descriptor is given, it is closed when the
    /// returned I/O object is closed, unless closefd is set to False.)
    ///
    /// mode is an optional string that specifies the mode in which the file
    /// is opened. It defaults to 'r' which means open for reading in text
    /// mode.  Other common values are 'w' for writing (truncating the file if
    /// it already exists), 'x' for creating and writing to a new file, and
    /// 'a' for appending (which on some Unix systems, means that all writes
    /// append to the end of the file regardless of the current seek position).
    /// In text mode, if encoding is not specified the encoding used is platform
    /// dependent: locale.getencoding() is called to get the current locale encoding.
    /// (For reading and writing raw bytes use binary mode and leave encoding
    /// unspecified.) The available modes are:
    ///
    /// ========= ===============================================================
    /// Character Meaning
    /// --------- ---------------------------------------------------------------
    /// 'r'       open for reading (default)
    /// 'w'       open for writing, truncating the file first
    /// 'x'       create a new file and open it for writing
    /// 'a'       open for writing, appending to the end of the file if it exists
    /// 'b'       binary mode
    /// 't'       text mode (default)
    /// '+'       open a disk file for updating (reading and writing)
    /// ========= ===============================================================
    ///
    /// The default mode is 'rt' (open for reading text). For binary random
    /// access, the mode 'w+b' opens and truncates the file to 0 bytes, while
    /// 'r+b' opens the file without truncation. The 'x' mode implies 'w' and
    /// raises an `FileExistsError` if the file already exists.
    ///
    /// Python distinguishes between files opened in binary and text modes,
    /// even when the underlying operating system doesn't. Files opened in
    /// binary mode (appending 'b' to the mode argument) return contents as
    /// bytes objects without any decoding. In text mode (the default, or when
    /// 't' is appended to the mode argument), the contents of the file are
    /// returned as strings, the bytes having been first decoded using a
    /// platform-dependent encoding or using the specified encoding if given.
    ///
    /// buffering is an optional integer used to set the buffering policy.
    /// Pass 0 to switch buffering off (only allowed in binary mode), 1 to select
    /// line buffering (only usable in text mode), and an integer > 1 to indicate
    /// the size of a fixed-size chunk buffer.  When no buffering argument is
    /// given, the default buffering policy works as follows:
    ///
    /// * Binary files are buffered in fixed-size chunks; the size of the buffer
    ///  is max(min(blocksize, 8 MiB), DEFAULT_BUFFER_SIZE)
    ///  when the device block size is available.
    ///  On most systems, the buffer will typically be 128 kilobytes long.
    ///
    /// * "Interactive" text files (files for which isatty() returns True)
    ///   use line buffering.  Other text files use the policy described above
    ///   for binary files.
    ///
    /// encoding is the name of the encoding used to decode or encode the
    /// file. This should only be used in text mode. The default encoding is
    /// platform dependent, but any encoding supported by Python can be
    /// passed.  See the codecs module for the list of supported encodings.
    ///
    /// errors is an optional string that specifies how encoding errors are to
    /// be handled---this argument should not be used in binary mode. Pass
    /// 'strict' to raise a ValueError exception if there is an encoding error
    /// (the default of None has the same effect), or pass 'ignore' to ignore
    /// errors. (Note that ignoring encoding errors can lead to data loss.)
    /// See the documentation for codecs.register or run 'help(codecs.Codec)'
    /// for a list of the permitted encoding error strings.
    ///
    /// newline controls how universal newlines works (it only applies to text
    /// mode). It can be None, '', '\n', '\r', and '\r\n'.  It works as
    /// follows:
    ///
    /// * On input, if newline is None, universal newlines mode is
    ///   enabled. Lines in the input can end in '\n', '\r', or '\r\n', and
    ///   these are translated into '\n' before being returned to the
    ///   caller. If it is '', universal newline mode is enabled, but line
    ///   endings are returned to the caller untranslated. If it has any of
    ///   the other legal values, input lines are only terminated by the given
    ///   string, and the line ending is returned to the caller untranslated.
    ///
    /// * On output, if newline is None, any '\n' characters written are
    ///   translated to the system default line separator, os.linesep. If
    ///   newline is '' or '\n', no translation takes place. If newline is any
    ///   of the other legal values, any '\n' characters written are translated
    ///   to the given string.
    ///
    /// If closefd is False, the underlying file descriptor will be kept open
    /// when the file is closed. This does not work when a file name is given
    /// and must be True in that case.
    ///
    /// A custom opener can be used by passing a callable as *opener*. The
    /// underlying file descriptor for the file object is then obtained by
    /// calling *opener* with (*file*, *flags*). *opener* must return an open
    /// file descriptor (passing os.open as *opener* results in functionality
    /// similar to passing None).
    ///
    /// open() returns a file object whose type depends on the mode, and
    /// through which the standard file operations such as reading and writing
    /// are performed. When open() is used to open a file in a text mode ('w',
    /// 'r', 'wt', 'rt', etc.), it returns a TextIOWrapper. When used to open
    /// a file in a binary mode, the returned class varies: in read binary
    /// mode, it returns a BufferedReader; in write binary and append binary
    /// modes, it returns a BufferedWriter, and in read/write mode, it returns
    /// a BufferedRandom.
    ///
    /// It is also possible to use a string or bytearray as a file for both
    /// reading and writing. For strings StringIO can be used like a file
    /// opened in a text mode, and for bytes a BytesIO can be used like a file
    /// opened in a binary mode.
    #[op(hint(py(text_signature = "($module, /, file, mode='r', buffering=-1, encoding=None,\n     errors=None, newline=None, closefd=True, opener=None)")))]
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
    ///
    /// This may perform extra validation beyond open(), but is otherwise interchangeable
    /// with calling open(path, 'rb').
    #[op(hint(py(text_signature = "($module, /, path)")))]
    fn open_code(it: &mut Interp, #[kw] path: &str) -> R<Value> {
        super::open_impl(it, &Value::str(path), "rb", -1, None, None, None, true, None)
    }

    /// A helper function to choose the text encoding.
    ///
    /// When encoding is not None, this function returns it.
    /// Otherwise, this function returns the default text encoding
    /// (i.e. "locale" or "utf-8" depends on UTF-8 mode).
    ///
    /// This function emits an EncodingWarning if encoding is None and
    /// sys.flags.warn_default_encoding is true.
    ///
    /// This can be used in APIs with an encoding=None parameter.
    /// However, please consider using encoding="utf-8" for new APIs.
    #[op(hint(py(text_signature = "($module, encoding, stacklevel=2, /)")))]
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
        let exporter = type_object::<super::bytesio::BytesIOBuffer>(it);
        dict_set_str(&d, "_BytesIOBuffer", Value::Obj(exporter));
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
        if buffering == 1 || (buffering < 0 && call_bool(it, &raw, "_isatty_open_only")?) {
            buffering = -1;
            line_buffering = true;
        }
        if buffering < 0 {
            let bs = fileio::blksize(it, &raw);
            buffering = bs.clamp(0, MAX_BLKSIZE).max(DEFAULT_BUFFER_SIZE as i64);
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
