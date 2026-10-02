//! CPython's `path_t` argument converter: a file-system path given as `str`, `bytes` or an
//! `os.PathLike` (and, where the function allows it, an open file descriptor).

use super::*;
use lumen_bind::FromArg;

/// A converted path argument. `FD`: an `int` is accepted as a file descriptor.
pub struct FsPath<const FD: bool> {
    /// The path as text (empty when `fd` is set).
    pub path: String,
    pub fd: Option<i32>,
    /// Given as `bytes` (directly or through `__fspath__`): path results come back as bytes.
    pub bytes: bool,
    /// The argument as given, for `OSError.filename`.
    pub obj: Value,
}

/// A path (`str`, `bytes` or `os.PathLike`).
pub type PathArg = FsPath<false>;
/// A path or an open file descriptor.
pub type PathOrFd = FsPath<true>;

impl<const FD: bool> FsPath<FD> {
    /// A result path in the argument's flavour (`bytes` in, `bytes` out).
    pub fn wrap(&self, s: String) -> Value {
        wrap_path(self.bytes, s)
    }

    /// The `filename` an `OSError` about this path carries.
    pub fn filename(&self) -> Option<&Value> {
        Some(&self.obj)
    }
}

/// `s` as `str`, or as `bytes` when `bytes`.
pub fn wrap_path(bytes: bool, s: String) -> Value {
    if bytes {
        Value::bytes(lumen_common::smuggle::unescape_text(&s).into_owned().into_bytes())
    } else {
        Value::string(s)
    }
}

/// Text of a `bytes` path (undecodable bytes become smuggled surrogates, as `surrogateescape`).
pub fn bytes_path(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => lumen_common::smuggle::escape_text_owned(String::from_utf8_lossy(b).into_owned()),
    }
}

/// `os.fspath(v)`: `str` or `bytes` unchanged, else `type(v).__fspath__(v)`.
pub fn fspath(it: &mut Interp, v: &Value) -> R<Value> {
    if v.as_str().is_some() || matches!(v, Value::Obj(o) if matches!(o.kind, Kind::Bytes(_))) {
        return Ok(v.clone());
    }
    let t = it.type_of(v);
    let Some(m) = it.lookup_mro(&t, "__fspath__") else {
        let n = it.type_name(&t);
        return Err(it.type_error(&format!("expected str, bytes or os.PathLike object, not {}", n)));
    };
    let f = it.bind_descr(&m, v, &t)?;
    let r = it.call(&f, Vec::new(), Vec::new())?;
    if r.as_str().is_some() || matches!(&r, Value::Obj(o) if matches!(o.kind, Kind::Bytes(_))) {
        return Ok(r);
    }
    let (n, rn) = (it.type_name(&t), it.type_name_of(&r));
    Err(it.type_error(&format!("expected {}.__fspath__() to return str or bytes, not {}", n, rn)))
}

fn convert<const FD: bool>(it: &mut Interp, d: &'static FnDesc, at: Slot, v: &Value) -> R<FsPath<FD>> {
    let fname = args::py_name(d);
    let argname = at.index().and_then(|i| d.named().nth(i as usize)).map(|p| p.name).unwrap_or("path");
    convert_path(it, fname, argname, v, FD, false)
}

/// CPython's `path_converter` for `fname`'s argument `argname`: `allow_fd` accepts an `int`,
/// `nullable` accepts `None` (as `"."`).
pub fn convert_path<const FD: bool>(it: &mut Interp, fname: &str, argname: &str, v: &Value, allow_fd: bool, nullable: bool) -> R<FsPath<FD>> {
    if nullable && matches!(v, Value::None) {
        return Ok(FsPath { path: ".".to_string(), fd: None, bytes: false, obj: Value::None });
    }
    if allow_fd && v.is_int_like() && !matches!(v, Value::Bool(_)) {
        let fd = convert::to_int(it, v, IntKind::new(32, true, false))? as i32;
        return Ok(FsPath { path: String::new(), fd: Some(fd), bytes: false, obj: v.clone() });
    }
    let resolved = if v.as_str().is_some() || matches!(v, Value::Obj(o) if matches!(o.kind, Kind::Bytes(_))) {
        v.clone()
    } else {
        let t = it.type_of(v);
        if it.lookup_mro(&t, "__fspath__").is_none() {
            let what = match (allow_fd, nullable) {
                (true, true) => "string, bytes, os.PathLike, integer or None",
                (true, false) => "string, bytes, os.PathLike or integer",
                (false, true) => "string, bytes, os.PathLike or None",
                (false, false) => "string, bytes or os.PathLike",
            };
            let tn = it.type_name(&t);
            return Err(it.type_error(&format!("{}: {} should be {}, not {}", fname, argname, what, tn)));
        }
        fspath(it, v)?
    };
    let (path, bytes) = match &resolved {
        Value::Obj(o) => match &o.kind {
            Kind::Str(s) => (s.s.to_string(), false),
            Kind::Bytes(b) => (bytes_path(b), true),
            _ => unreachable!("fspath returns str or bytes"),
        },
        _ => unreachable!("fspath returns str or bytes"),
    };
    if path.contains('\0') {
        return Err(it.value_error(&format!("{}: embedded null character in {}", fname, argname)));
    }
    Ok(FsPath { path, fd: None, bytes, obj: v.clone() })
}

impl<'a, const FD: bool> FromArg<'a, PyHost> for FsPath<FD> {
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        convert::<FD>(cx.it(), cx.desc, at, v)
    }
}
