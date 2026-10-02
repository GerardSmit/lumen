//! `sys`.

/// This module provides access to some objects used or maintained by the
/// interpreter and to functions that interact strongly with the interpreter.
#[lumen_bind::module(name = "sys")]
pub mod sys {
    use crate::builtins::genm::AsyncGenHooks;
    use crate::builtins::sysextra::{new_structseq_type, structseq};
    use crate::object::*;
    use crate::vm::*;
    use std::rc::Rc;

    /// Exit the interpreter by raising SystemExit(status).
    ///
    /// If the status is omitted or None, it defaults to zero (i.e., success).
    /// If the status is an integer, it will be used as the system exit status.
    /// If it is another kind of object, it will be printed and the system
    /// exit status will be one (i.e., failure).
    #[op]
    fn exit(it: &mut Interp, status: Option<&Value>) -> R<()> {
        let cls = it.exc_type("SystemExit");
        Err(it.new_exc(&cls, status.into_iter().cloned().collect()))
    }

    /// Return the current value of the recursion limit.
    ///
    /// The recursion limit is the maximum depth of the Python interpreter
    /// stack.  This limit prevents infinite recursion from causing an overflow
    /// of the C stack and crashing Python.
    #[op]
    fn getrecursionlimit(it: &mut Interp) -> usize {
        it.recursion_limit
    }

    /// Set the maximum depth of the Python interpreter stack to n.
    ///
    /// This limit prevents infinite recursion from causing an overflow of the C
    /// stack and crashing Python.  The highest possible limit is platform-
    /// dependent.
    #[op]
    fn setrecursionlimit(it: &mut Interp, limit: &Value) -> R<()> {
        let n = it.index_of(limit)?;
        if n < 1 {
            return Err(it.value_error("recursion limit must be greater or equal than 1"));
        }
        it.recursion_limit = (n as usize).min(200_000);
        Ok(())
    }

    /// Return current exception information: (type, value, traceback).
    ///
    /// Return information about the most recent exception caught by an except
    /// clause in the current stack frame or in an older stack frame.
    #[op]
    fn exc_info(it: &mut Interp) -> Value {
        match it.handled.clone() {
            Some(e) => {
                let t = Value::Obj(it.type_of_obj(&e));
                let tb = match &e.kind {
                    Kind::Exception(d) => it.make_tb(&d.borrow().tb),
                    _ => Value::None,
                };
                Value::tuple(vec![t, Value::Obj(e), tb])
            }
            None => Value::tuple(vec![Value::None, Value::None, Value::None]),
        }
    }

    /// Return the current exception.
    ///
    /// Return the most recent exception caught by an except clause
    /// in the current stack frame or in an older stack frame, or None
    /// if no such exception exists.
    #[op]
    fn exception(it: &mut Interp) -> Value {
        it.handled.clone().map(Value::Obj).unwrap_or(Value::None)
    }

    /// ``Intern'' the given string.
    ///
    /// This enters the string in the (global) table of interned strings whose
    /// purpose is to speed up dictionary lookups. Return the string itself or
    /// the previously interned string object with the same value.
    #[op]
    fn intern(it: &mut Interp, string: &Value) -> R<Value> {
        if string.as_str().is_none() {
            let t = it.type_name_of(string);
            return Err(it.type_error(&format!("intern() argument must be str, not {t}")));
        }
        Ok(string.clone())
    }

    /// Return the maximum string digits limit for non-binary int<->str conversions.
    #[op]
    fn get_int_max_str_digits(it: &mut Interp) -> usize {
        it.int_max_str_digits()
    }

    /// Set the maximum string digits limit for non-binary int<->str conversions.
    #[op]
    fn set_int_max_str_digits(it: &mut Interp, #[kw] maxdigits: &Value) -> R<()> {
        let n = it.index_of(maxdigits)?;
        if n < 0 || !it.set_int_max_str_digits(n as usize) {
            return Err(it.value_error(&format!("maxdigits must be >= {} or 0 for unlimited", crate::limits::INT_MAX_STR_DIGITS_THRESHOLD)));
        }
        // sys.flags is built lazily and cached; drop it so it reflects the new limit, as CPython does.
        if let Some(m) = it.sys_module.clone() {
            let d = it.module_dict(&m);
            dict_del_str(&d, "flags");
        }
        Ok(())
    }

    /// Return a frame object from the call stack.
    ///
    /// If optional integer depth is given, return the frame object that many
    /// calls below the top of the stack.  If that is deeper than the call
    /// stack, ValueError is raised.  The default for depth is zero, returning
    /// the frame at the top of the call stack.
    ///
    /// This function should be used for internal and specialized purposes
    /// only.
    #[op]
    fn _getframe(it: &mut Interp, #[default(0)] depth: i64) -> R<Value> {
        let n = it.frames.len() as i64;
        if depth < 0 || depth >= n {
            return Err(it.value_error("call stack is not deep enough"));
        }
        Ok(it.frame_object((n - 1 - depth) as usize))
    }

    /// Return the reference count of object.
    ///
    /// The count returned is generally one higher than you might expect,
    /// because it includes the (temporary) reference as an argument to
    /// getrefcount().
    #[op]
    fn getrefcount(object: &Value) -> i64 {
        match object {
            Value::Obj(o) => Rc::strong_count(o) as i64,
            _ => 1 << 30,
        }
    }

    /// getsizeof(object [, default]) -> int
    ///
    /// Return the size of object in bytes.
    #[op(hint(py(text_signature = "")))]
    fn getsizeof(#[kw] object: &Value, #[kw] default: Option<&Value>) -> i64 {
        let _ = default;
        size_of_value(object) as i64
    }

    /// Return the current default encoding used by the Unicode implementation.
    #[op]
    fn getdefaultencoding() -> &'static str {
        "utf-8"
    }

    /// Return the encoding used to convert Unicode filenames to OS filenames.
    #[op]
    fn getfilesystemencoding() -> &'static str {
        "utf-8"
    }

    /// Return the error mode used Unicode to OS filename conversion.
    #[op]
    fn getfilesystemencodeerrors() -> &'static str {
        "surrogateescape"
    }

    /// Return True if Python is exiting.
    #[op]
    fn is_finalizing() -> bool {
        false
    }

    /// Return the global debug tracing function set with sys.settrace.
    ///
    /// See the debugger chapter in the library manual.
    #[op]
    fn gettrace() -> Value {
        Value::None
    }

    /// settrace(function)
    ///
    /// Set the global debug tracing function.  It will be called on each
    /// function call.  See the debugger chapter in the library manual.
    #[op(hint(py(text_signature = "")))]
    fn settrace(function: &Value) {
        let _ = function;
    }

    /// Return the profiling function set with sys.setprofile.
    ///
    /// See the profiler chapter in the library manual.
    #[op]
    fn getprofile() -> Value {
        Value::None
    }

    /// setprofile(function)
    ///
    /// Set the profiling function.  It will be called on each function call
    /// and return.  See the profiler chapter in the library manual.
    #[op(hint(py(text_signature = "")))]
    fn setprofile(function: &Value) {
        let _ = function;
    }

    /// audit(event, *args)
    ///
    /// Passes the event to any audit hooks that are attached.
    #[op(hint(py(text_signature = "")))]
    fn audit(event: &str, #[varargs] args: &[Value]) {
        let _ = (event, args);
    }

    /// Adds a new audit hook callback.
    #[op]
    fn addaudithook(#[kw] hook: &Value) {
        let _ = hook;
    }

    /// Return the installed asynchronous generators hooks.
    ///
    /// This returns a namedtuple of the form (firstiter, finalizer).
    #[op]
    fn get_asyncgen_hooks(it: &mut Interp) -> Value {
        let h = it.native_state::<AsyncGenHooks>();
        let vals = vec![h.firstiter.clone().unwrap_or(Value::None), h.finalizer.clone().unwrap_or(Value::None)];
        let ty = crate::builtins::sysextra::structseq_type::<AsyncGenHooks>(it, "builtins", "asyncgen_hooks", &["firstiter", "finalizer"], 2);
        structseq(&ty, vals)
    }

    /// set_asyncgen_hooks([firstiter] [, finalizer])
    ///
    /// Set a finalizer for async generators objects.
    #[op]
    fn set_asyncgen_hooks(it: &mut Interp, #[kw] firstiter: lumen_bind::Passed<&Value>, #[kw] finalizer: lumen_bind::Passed<&Value>) -> R<()> {
        let check = |it: &mut Interp, name: &str, v: Option<&Value>| -> R<Option<Option<Value>>> {
            match v {
                None => Ok(None),
                Some(Value::None) => Ok(Some(None)),
                Some(v) if it.is_callable(v) => Ok(Some(Some(v.clone()))),
                Some(v) => {
                    let t = it.tp_name_of(v);
                    Err(it.type_error(&format!("callable {name} expected, got {t}")))
                }
            }
        };
        let fin = check(it, "finalizer", finalizer.0)?;
        let first = check(it, "firstiter", firstiter.0)?;
        let h = it.native_state::<AsyncGenHooks>();
        if let Some(f) = fin {
            h.finalizer = f;
        }
        if let Some(f) = first {
            h.firstiter = f;
        }
        Ok(())
    }

    /// Return the current thread switch interval; see sys.setswitchinterval().
    #[op]
    fn getswitchinterval() -> f64 {
        0.005
    }

    /// Set the ideal thread switching delay inside the Python interpreter.
    ///
    /// The actual frequency of switching threads can be lower if the
    /// interpreter executes long sequences of uninterruptible code
    /// (this is implementation-specific and workload-dependent).
    ///
    /// The parameter must represent the desired switching delay in seconds
    /// A typical value is 0.005 (5 milliseconds).
    #[op]
    fn setswitchinterval(it: &mut Interp, interval: f64) -> R<()> {
        if interval <= 0.0 {
            return Err(it.value_error("switch interval must be strictly positive"));
        }
        Ok(())
    }

    /// Handle an exception by displaying it with a traceback on sys.stderr.
    #[op]
    fn excepthook(it: &mut Interp, exctype: &Value, value: &Value, traceback: &Value) -> R<()> {
        let _ = (exctype, traceback);
        let text = match value {
            Value::Obj(e) if matches!(e.kind, Kind::Exception(_)) => it.format_exception(e),
            _ => {
                let t = it.type_name_of(value);
                format!("TypeError: print_exception(): Exception expected for value, {t} found\n")
            }
        };
        it.write_stderr(&text);
        Ok(())
    }

    /// Handle an unraisable exception.
    ///
    /// The argument is an object with the attributes exc_type, exc_value, exc_traceback,
    /// err_msg and object.
    #[op]
    fn unraisablehook(it: &mut Interp, unraisable: &Value) -> R<()> {
        let exc = it.getitem(unraisable, &Value::Int(1))?;
        let err_msg = it.getitem(unraisable, &Value::Int(3))?;
        let object = it.getitem(unraisable, &Value::Int(4))?;
        let Value::Obj(e) = &exc else { return Ok(()) };
        it.default_unraisable(e, Some(&err_msg), Some(&object));
        Ok(())
    }

    /// Clear the internal type lookup cache.
    #[op]
    fn _clear_type_cache() {
        crate::watch::clear_type_cache();
    }

    /// Print an object to sys.stdout and also save it in builtins._
    #[op]
    fn displayhook(it: &mut Interp, object: &Value) -> R<()> {
        if object.is_none() {
            return Ok(());
        }
        let builtins = it.builtins.clone();
        dict_set_str(&builtins, "_", Value::None);
        let text = it.repr_of(object)?;
        let out = it.sys_module.clone().and_then(|m| dict_get_str(&it.module_dict(&m), "stdout")).unwrap_or(Value::None);
        if out.is_none() {
            return Err(it.runtime_error("lost sys.stdout"));
        }
        it.write_to(&out, &format!("{text}\n"))?;
        dict_set_str(&builtins, "_", object.clone());
        Ok(())
    }

    /// The attributes computed on first use (`flags`, `version_info`, ...).
    #[op]
    fn __getattr__(it: &mut Interp, name: &str) -> R<Value> {
        lazy_attr(it, name)
    }

    fn size_of_value(v: &Value) -> usize {
        match v {
            Value::Obj(o) => match &o.kind {
                Kind::Str(s) => 49 + s.s.len(),
                Kind::Int(b) => 24 + 4 * b.to_string_radix(16).len() / 2,
                Kind::Tuple(t) => 40 + 8 * t.len(),
                Kind::List(l) => 56 + 8 * l.borrow().capacity(),
                Kind::Dict(d) | Kind::Set(d) | Kind::FrozenSet(d) => 64 + 24 * d.borrow().slots(),
                Kind::Bytes(b) => 33 + b.len(),
                Kind::ByteArray(b) => 56 + b.len(),
                Kind::Instance => 48,
                _ => 64,
            },
            Value::Int(i) => {
                if *i == 0 {
                    24
                } else {
                    24 + 4 * (((64 - i.unsigned_abs().leading_zeros()) as usize).div_ceil(30))
                }
            }
            Value::Float(_) => 24,
            Value::Bool(_) => 28,
            _ => 16,
        }
    }

    fn lazy_attr(it: &mut Interp, name: &str) -> R<Value> {
        let name = name.to_string();
        let v = match name.as_str() {
            "version_info" => {
                let ty = new_structseq_type(it, "sys", "version_info", &["major", "minor", "micro", "releaselevel", "serial"]);
                structseq(&ty, vec![Value::Int(3), Value::Int(12), Value::Int(15), Value::str("final"), Value::Int(0)])
            }
            "flags" => {
                let names = [
                    "debug",
                    "inspect",
                    "interactive",
                    "optimize",
                    "dont_write_bytecode",
                    "no_user_site",
                    "no_site",
                    "ignore_environment",
                    "verbose",
                    "bytes_warning",
                    "quiet",
                    "hash_randomization",
                    "isolated",
                    "dev_mode",
                    "utf8_mode",
                    "warn_default_encoding",
                    "safe_path",
                    "int_max_str_digits",
                ];
                let ty = new_structseq_type(it, "sys", "flags", &names);
                let digits = it.int_max_str_digits() as i64;
                let utf8 = crate::builtins::iom::utf8_mode(it) as i64;
                let vals = names
                    .iter()
                    .map(|n| match *n {
                        "hash_randomization" | "dont_write_bytecode" => Value::Int(1),
                        "utf8_mode" => Value::Int(utf8),
                        "int_max_str_digits" => Value::Int(digits),
                        "dev_mode" => Value::Bool(false),
                        "safe_path" => Value::Bool(false),
                        _ => Value::Int(0),
                    })
                    .collect();
                structseq(&ty, vals)
            }
            "float_info" => {
                let names = ["max", "max_exp", "max_10_exp", "min", "min_exp", "min_10_exp", "dig", "mant_dig", "epsilon", "radix", "rounds"];
                let ty = new_structseq_type(it, "sys", "float_info", &names);
                structseq(
                    &ty,
                    vec![
                        Value::Float(f64::MAX),
                        Value::Int(1024),
                        Value::Int(308),
                        Value::Float(f64::MIN_POSITIVE),
                        Value::Int(-1021),
                        Value::Int(-307),
                        Value::Int(15),
                        Value::Int(53),
                        Value::Float(f64::EPSILON),
                        Value::Int(2),
                        Value::Int(1),
                    ],
                )
            }
            "int_info" => {
                let ty = new_structseq_type(it, "sys", "int_info", &["bits_per_digit", "sizeof_digit", "default_max_str_digits", "str_digits_check_threshold"]);
                structseq(
                    &ty,
                    vec![
                        Value::Int(30),
                        Value::Int(4),
                        Value::Int(crate::limits::DEFAULT_INT_MAX_STR_DIGITS as i64),
                        Value::Int(crate::limits::INT_MAX_STR_DIGITS_THRESHOLD as i64),
                    ],
                )
            }
            "hash_info" => {
                let names = ["width", "modulus", "inf", "nan", "imag", "algorithm", "hash_bits", "seed_bits", "cutoff"];
                let ty = new_structseq_type(it, "sys", "hash_info", &names);
                structseq(
                    &ty,
                    vec![Value::Int(64), Value::Int((1 << 61) - 1), Value::Int(314159), Value::Int(0), Value::Int(1000003), Value::str("siphash13"), Value::Int(64), Value::Int(128), Value::Int(0)],
                )
            }
            "implementation" => {
                let sys = it.sys_module.clone();
                let vi = match &sys {
                    Some(m) => {
                        let d = it.module_dict(m);
                        match dict_get_str(&d, "version_info") {
                            Some(v) => v,
                            None => it.get_attr_str(&Value::Obj(m.clone()), "version_info")?,
                        }
                    }
                    None => Value::None,
                };
                it.new_namespace(vec![
                    ("name", Value::str("lumen-py")),
                    ("cache_tag", Value::str("lumen-312")),
                    ("version", vi),
                    ("hexversion", Value::Int(0x030c0ff0)),
                    ("_multiarch", Value::str("")),
                ])
            }
            _ => {
                let m = it.sys_module.clone().map(Value::Obj).unwrap_or(Value::None);
                return Err(it.attr_error(&m, &name));
            }
        };
        if let Some(m) = it.sys_module.clone() {
            let d = it.module_dict(&m);
            dict_set_str(&d, &name, v.clone());
        }
        Ok(v)
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        it.sys_module = Some(m.clone());
        dict_set_str(&d, "modules", Value::Obj(it.modules.clone()));
        dict_set_str(&d, "argv", Value::list(vec![Value::str("")]));
        dict_set_str(&d, "path", Value::list(vec![Value::str(crate::frozen::FROZEN_DIR)]));
        dict_set_str(&d, "maxsize", Value::Int(i64::MAX));
        dict_set_str(&d, "maxunicode", Value::Int(0x10ffff));
        dict_set_str(&d, "byteorder", Value::str("little"));
        dict_set_str(&d, "version", Value::str("3.12.15 (main, Jan  1 2026, 00:00:00) [lumen-py]"));
        dict_set_str(&d, "hexversion", Value::Int(0x030c0ff0));
        let (platform, executable, argv) = {
            let p = it.platform.borrow();
            (p.platform_name(), p.executable(), p.argv())
        };
        dict_set_str(&d, "platform", Value::str(&platform));
        if platform == "darwin" {
            dict_set_str(&d, "_framework", Value::str(""));
        }
        dict_set_str(&d, "executable", Value::str(&executable));
        dict_set_str(&d, "_base_executable", Value::str(&executable));
        dict_set_str(&d, "_git", Value::tuple(vec![Value::str("lumen-py"), Value::str(""), Value::str("")]));
        if !argv.is_empty() {
            dict_set_str(&d, "argv", Value::list(argv.iter().map(|a| Value::str(a)).collect()));
            it.argv = argv;
        }
        let names = crate::builtins::modules::builtin_module_names();
        dict_set_str(&d, "builtin_module_names", Value::tuple(names.iter().map(|n| Value::str(n)).collect()));
        let std_names = super::STDLIB_MODULE_NAMES.iter().map(|n| Value::str(n)).collect();
        if let Ok(std_names) = it.new_frozenset_from(std_names) {
            dict_set_str(&d, "stdlib_module_names", std_names);
        }
        for (alias, name) in [("__excepthook__", "excepthook"), ("__displayhook__", "displayhook"), ("__unraisablehook__", "unraisablehook")] {
            if let Some(f) = dict_get_str(&d, name) {
                dict_set_str(&d, alias, f);
            }
        }
        dict_set_str(&d, "warnoptions", Value::list(Vec::new()));
        dict_set_str(&d, "meta_path", Value::list(Vec::new()));
        dict_set_str(&d, "path_hooks", Value::list(Vec::new()));
        dict_set_str(&d, "path_importer_cache", Value::Obj(it.new_dict()));
        dict_set_str(&d, "_xoptions", Value::Obj(it.new_dict()));
        dict_set_str(&d, "dont_write_bytecode", Value::Bool(true));
        dict_set_str(&d, "pycache_prefix", Value::None);
        dict_set_str(&d, "abiflags", Value::str(""));
        dict_set_str(&d, "api_version", Value::Int(1013));
        dict_set_str(&d, "float_repr_style", Value::str("short"));
        dict_set_str(&d, "copyright", Value::str("Python lumen-py"));
        for k in ["prefix", "exec_prefix", "base_prefix", "base_exec_prefix"] {
            dict_set_str(&d, k, Value::str("/usr/local"));
        }
        dict_set_str(&d, "platlibdir", Value::str("lib"));
        dict_set_str(&d, "stdlib_dir", Value::str(crate::frozen::FROZEN_DIR));
        dict_set_str(&d, "orig_argv", Value::list(Vec::new()));
    }
}

/// CPython 3.12's `sys.stdlib_module_names`.
#[rustfmt::skip]
const STDLIB_MODULE_NAMES: &[&str] = &[
    "__future__", "_abc", "_aix_support", "_ast", "_asyncio", "_bisect", "_blake2", "_bz2", "_codecs",
    "_codecs_cn", "_codecs_hk", "_codecs_iso2022", "_codecs_jp", "_codecs_kr", "_codecs_tw", "_collections",
    "_collections_abc", "_compat_pickle", "_compression", "_contextvars", "_crypt", "_csv", "_ctypes",
    "_curses", "_curses_panel", "_datetime", "_dbm", "_decimal", "_elementtree", "_frozen_importlib",
    "_frozen_importlib_external", "_functools", "_gdbm", "_hashlib", "_heapq", "_imp", "_io", "_json",
    "_locale", "_lsprof", "_lzma", "_markupbase", "_md5", "_msi", "_multibytecodec", "_multiprocessing",
    "_opcode", "_operator", "_osx_support", "_overlapped", "_pickle", "_posixshmem", "_posixsubprocess",
    "_py_abc", "_pydatetime", "_pydecimal", "_pyio", "_pylong", "_queue", "_random", "_scproxy", "_sha1",
    "_sha2", "_sha3", "_signal", "_sitebuiltins", "_socket", "_sqlite3", "_sre", "_ssl", "_stat",
    "_statistics", "_string", "_strptime", "_struct", "_symtable", "_thread", "_threading_local", "_tkinter",
    "_tokenize", "_tracemalloc", "_typing", "_uuid", "_warnings", "_weakref", "_weakrefset", "_winapi",
    "_wmi", "_zoneinfo", "abc", "aifc", "antigravity", "argparse", "array", "ast", "asyncio", "atexit",
    "audioop", "base64", "bdb", "binascii", "bisect", "builtins", "bz2", "cProfile", "calendar", "cgi",
    "cgitb", "chunk", "cmath", "cmd", "code", "codecs", "codeop", "collections", "colorsys", "compileall",
    "concurrent", "configparser", "contextlib", "contextvars", "copy", "copyreg", "crypt", "csv", "ctypes",
    "curses", "dataclasses", "datetime", "dbm", "decimal", "difflib", "dis", "doctest", "email", "encodings",
    "ensurepip", "enum", "errno", "faulthandler", "fcntl", "filecmp", "fileinput", "fnmatch", "fractions",
    "ftplib", "functools", "gc", "genericpath", "getopt", "getpass", "gettext", "glob", "graphlib", "grp",
    "gzip", "hashlib", "heapq", "hmac", "html", "http", "idlelib", "imaplib", "imghdr", "importlib",
    "inspect", "io", "ipaddress", "itertools", "json", "keyword", "lib2to3", "linecache", "locale", "logging",
    "lzma", "mailbox", "mailcap", "marshal", "math", "mimetypes", "mmap", "modulefinder", "msilib", "msvcrt",
    "multiprocessing", "netrc", "nis", "nntplib", "nt", "ntpath", "nturl2path", "numbers", "opcode",
    "operator", "optparse", "os", "ossaudiodev", "pathlib", "pdb", "pickle", "pickletools", "pipes",
    "pkgutil", "platform", "plistlib", "poplib", "posix", "posixpath", "pprint", "profile", "pstats", "pty",
    "pwd", "py_compile", "pyclbr", "pydoc", "pydoc_data", "pyexpat", "queue", "quopri", "random", "re",
    "readline", "reprlib", "resource", "rlcompleter", "runpy", "sched", "secrets", "select", "selectors",
    "shelve", "shlex", "shutil", "signal", "site", "smtplib", "sndhdr", "socket", "socketserver", "spwd",
    "sqlite3", "sre_compile", "sre_constants", "sre_parse", "ssl", "stat", "statistics", "string",
    "stringprep", "struct", "subprocess", "sunau", "symtable", "sys", "sysconfig", "syslog", "tabnanny",
    "tarfile", "telnetlib", "tempfile", "termios", "textwrap", "this", "threading", "time", "timeit",
    "tkinter", "token", "tokenize", "tomllib", "trace", "traceback", "tracemalloc", "tty", "turtle",
    "turtledemo", "types", "typing", "unicodedata", "unittest", "urllib", "uu", "uuid", "venv", "warnings",
    "wave", "weakref", "webbrowser", "winreg", "winsound", "wsgiref", "xdrlib", "xml", "xmlrpc", "zipapp",
    "zipfile", "zipimport", "zlib", "zoneinfo",
];
