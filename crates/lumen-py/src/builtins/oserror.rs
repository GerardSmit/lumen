//! The `OSError` family: errno-driven subclass selection, the `errno`/`strerror`/`filename`/
//! `filename2`/`characters_written` attributes, `__str__`/`__reduce__`, and the constructors the
//! native modules raise with.

use crate::object::*;
use crate::platform::IoError;
use crate::vm::*;
use std::rc::Rc;

use crate::bind::{Exc, KwArgs, This};

const FIELDS: [&str; 5] = ["errno", "strerror", "filename", "filename2", "characters_written"];

/// The builtin `OSError` subclass that `OSError(errno, ...)` produces for an errno of this host.
pub fn errno_exception(errno: i32) -> Option<&'static str> {
    Some(match lumen_os::errno::code_of_errno(errno)? {
        "EAGAIN" | "EWOULDBLOCK" | "EALREADY" | "EINPROGRESS" => "BlockingIOError",
        "ECHILD" => "ChildProcessError",
        "EPIPE" | "ESHUTDOWN" => "BrokenPipeError",
        "ECONNABORTED" => "ConnectionAbortedError",
        "ECONNREFUSED" => "ConnectionRefusedError",
        "ECONNRESET" => "ConnectionResetError",
        "EEXIST" => "FileExistsError",
        "ENOENT" => "FileNotFoundError",
        "EINTR" => "InterruptedError",
        "EISDIR" => "IsADirectoryError",
        "ENOTDIR" => "NotADirectoryError",
        "EACCES" | "EPERM" => "PermissionError",
        "ESRCH" => "ProcessLookupError",
        "ETIMEDOUT" => "TimeoutError",
        _ => return None,
    })
}

/// Whether `cls` inherits `OSError`'s own `name` member.
fn inherits(it: &Interp, cls: &Obj, name: &str) -> bool {
    use lumen_bind::Class;
    matches!(it.lookup_mro(cls, name), Some(Value::Obj(o))
        if matches!(&o.kind, Kind::Native(n) if n.desc.and_then(|d| d.class()).is_some_and(|c| std::ptr::eq(c, OSErrorType::DESC))))
}

/// CPython's `oserror_use_init`: a subclass that overrides `__init__` but not `__new__` parses its
/// arguments in `__init__`; everything else parses them in `__new__`.
fn uses_init(it: &Interp, cls: &Obj) -> bool {
    !inherits(it, cls, "__init__") && inherits(it, cls, "__new__")
}

fn set_args(e: &Obj, args: Vec<Value>) {
    if let Kind::Exception(d) = &e.kind {
        d.borrow_mut().args = Value::tuple(args);
    }
}

fn is_number(v: &Value) -> bool {
    match v {
        Value::Int(_) | Value::Bool(_) | Value::Float(_) => true,
        Value::Obj(o) => matches!(o.kind, Kind::Int(_) | Kind::Float(_)),
        _ => false,
    }
}

fn init_fields(it: &mut Interp, e: &Obj, args: &[Value]) -> R<()> {
    let d = it.instance_dict(e);
    for n in FIELDS {
        dict_del_str(&d, n);
    }
    let mut shown = args.to_vec();
    if (2..=5).contains(&args.len()) {
        dict_set_str(&d, "errno", args[0].clone());
        dict_set_str(&d, "strerror", args[1].clone());
        if let Some(f) = args.get(2).filter(|f| !f.is_none()) {
            let exact_blocking = Rc::ptr_eq(&it.type_of_obj(e), &it.exc_type("BlockingIOError"));
            if exact_blocking && is_number(f) {
                let n = match f {
                    Value::Float(_) => return Err(it.type_error("'float' object cannot be interpreted as an integer")),
                    _ => it.index_of(f)?,
                };
                dict_set_str(&d, "characters_written", Value::Int(n));
            } else {
                dict_set_str(&d, "filename", f.clone());
                if let Some(f2) = args.get(4).filter(|f| !f.is_none()) {
                    dict_set_str(&d, "filename2", f2.clone());
                }
                shown.truncate(2);
            }
        }
    }
    set_args(e, shown);
    Ok(())
}

// `OSError`'s constructor, initializer and pickling (installed into the core type).
#[lumen_bind::class(name = "OSError")]
pub struct OSErrorType;

#[lumen_bind::methods]
impl OSErrorType {
    #[constructor(hint(py(text_signature = "")))]
    fn new(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        let Value::Obj(cls) = &*cls else { unreachable!("checked by the entry") };
        let use_init = uses_init(it, cls);
        let mut ty = cls.clone();
        if !use_init {
            if !kw.is_empty() {
                let n = it.type_name(cls);
                return Err(it.type_error(&format!("{}() takes no keyword arguments", n)));
            }
            if Rc::ptr_eq(cls, &it.exc_type("OSError")) && (2..=5).contains(&args.len()) {
                let mapped = args[0].as_bigint().and_then(|b| b.to_i64()).and_then(|n| i32::try_from(n).ok()).and_then(errno_exception);
                if let Some(name) = mapped {
                    ty = it.exc_type(name);
                }
            }
        }
        let o = it.alloc_instance(&ty)?;
        if let Value::Obj(e) = &o {
            if use_init {
                set_args(e, Vec::new());
            } else {
                init_fields(it, e, args)?;
            }
        }
        Ok(o)
    }

    #[proto(init)]
    fn init(slf: This<Exc<'_>>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<()> {
        let e = slf.0 .0;
        let ty = it.type_of_obj(e);
        if !uses_init(it, &ty) {
            return Ok(());
        }
        if !kw.is_empty() {
            let n = it.type_name(&ty);
            return Err(it.type_error(&format!("{}() takes no keyword arguments", n)));
        }
        init_fields(it, e, args)
    }

    #[proto(reduce)]
    fn reduce(slf: This<Exc<'_>>, it: &mut Interp) -> R<Value> {
        let e = slf.0 .0;
        let mut args = match &e.kind {
            Kind::Exception(d) => d.borrow().args.tuple_items().map(|t| t.to_vec()).unwrap_or_default(),
            _ => Vec::new(),
        };
        if let (2, Some(f)) = (args.len(), field(e, "filename")) {
            args.push(f);
            if let Some(f2) = field(e, "filename2") {
                args.push(Value::None);
                args.push(f2);
            }
        }
        let cls = Value::Obj(it.type_of_obj(e));
        let state = it.new_dict();
        let entries: Vec<(Value, Value)> = match e.dict.borrow().as_ref().map(|d| &d.kind) {
            Some(Kind::Dict(src)) => src.borrow().iter().map(|en| (en.key.clone(), en.val.clone())).collect(),
            _ => Vec::new(),
        };
        for (k, v) in entries {
            if !k.as_str().is_some_and(|s| FIELDS.contains(&s)) {
                it.dict_set(&state, k, v)?;
            }
        }
        let empty = matches!(&state.kind, Kind::Dict(d) if d.borrow().is_empty());
        let mut out = vec![cls, Value::tuple(args)];
        if !empty {
            out.push(Value::Obj(state));
        }
        Ok(Value::tuple(out))
    }
}

fn field(e: &Obj, name: &str) -> Option<Value> {
    e.dict.borrow().as_ref().and_then(|d| dict_get_str(d, name))
}

fn or_none(v: Option<Value>) -> Value {
    v.unwrap_or(Value::None)
}

impl Interp {
    /// `str()` of an `OSError`, or `None` when it falls back to `BaseException.__str__`.
    pub fn oserror_str(&mut self, e: &Obj) -> R<Option<String>> {
        let errno = field(e, "errno");
        let strerror = field(e, "strerror");
        if let Some(f) = field(e, "filename") {
            let head = format!("[Errno {}] {}: {}", self.str_of(&or_none(errno))?, self.str_of(&or_none(strerror))?, self.repr_of(&f)?);
            return Ok(Some(match field(e, "filename2") {
                Some(f2) => format!("{} -> {}", head, self.repr_of(&f2)?),
                None => head,
            }));
        }
        match (errno, strerror) {
            (Some(n), Some(s)) => Ok(Some(format!("[Errno {}] {}", self.str_of(&n)?, self.str_of(&s)?))),
            _ => Ok(None),
        }
    }

    fn os_error_args(&mut self, args: Vec<Value>) -> Obj {
        let cls = self.exc_type("OSError");
        self.os_error_of(&cls, args)
    }

    /// An instance of the `OSError` subclass `cls` built by calling it, so `errno`, `strerror`
    /// and the filenames are set from `args`.
    pub fn os_error_of(&mut self, cls: &Obj, args: Vec<Value>) -> Obj {
        match self.call(&Value::Obj(cls.clone()), args, Vec::new()) {
            Ok(Value::Obj(o)) => o,
            Ok(_) => self.new_exc_str("OSError", ""),
            Err(e) => e,
        }
    }

    /// `OSError` (or the subclass its errno selects) for a platform error; an empty `filename`
    /// means none.
    pub fn os_error(&mut self, e: &IoError, filename: &str) -> Obj {
        let name = (!filename.is_empty()).then(|| Value::str(filename));
        self.os_error_io(e, name.as_ref())
    }

    pub fn os_error_io(&mut self, e: &IoError, filename: Option<&Value>) -> Obj {
        self.os_error_errno(e.errno, filename, None)
    }

    /// `OSError(errno, strerror(errno), filename, None, filename2)`, as CPython's
    /// `PyErr_SetFromErrnoWithFilenameObjects`.
    pub fn os_error_errno(&mut self, errno: i32, filename: Option<&Value>, filename2: Option<&Value>) -> Obj {
        let mut args = vec![Value::Int(errno as i64), Value::string(lumen_os::errno::strerror(errno))];
        match (filename, filename2) {
            (Some(f), Some(f2)) => args.extend([f.clone(), Value::None, f2.clone()]),
            (Some(f), None) => args.push(f.clone()),
            (None, Some(f2)) => args.extend([Value::None, Value::None, f2.clone()]),
            (None, None) => {}
        }
        self.os_error_args(args)
    }
}

pub fn init(it: &mut Interp) {
    let oe = it.exc_type("OSError");
    crate::bind::extend_type::<OSErrorType>(it, &oe);
    let d = oe.dict.borrow().clone();
    if let Some(d) = d {
        for n in ["errno", "strerror", "filename", "filename2"] {
            dict_set_str(&d, n, Value::None);
        }
    }
}
