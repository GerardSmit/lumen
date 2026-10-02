//! Exception classes: construction, `args`, notes and the attributes of the specialised types.

use crate::bind::{Exc, KwArgs, This};
use crate::object::*;
use crate::vm::*;

fn set_args(e: &Obj, args: &[Value]) {
    if let Kind::Exception(d) = &e.kind {
        d.borrow_mut().args = Value::tuple(args.to_vec());
    }
}

/// Keyword arguments `allowed` become attributes of `e` (`ImportError(name=..)`); any other is
/// an error naming `cls`.
fn keyword_fields(it: &mut Interp, d: &Obj, kw: KwArgs, allowed: &[&str], cls: &str) -> R<()> {
    for (k, v) in kw.iter() {
        if !allowed.contains(&k) {
            return Err(it.type_error(&format!("'{}' is an invalid keyword argument for {}()", k, cls)));
        }
        dict_set_str(d, k, v.clone());
    }
    Ok(())
}

/// `BaseException`'s members.
#[lumen_bind::class(name = "BaseException")]
pub struct BaseException;

#[lumen_bind::methods]
impl BaseException {
    #[constructor(hint(py(text_signature = "")))]
    fn new(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
        let _ = kwargs;
        let Value::Obj(cls) = &*cls else { unreachable!("checked by the entry") };
        let o = it.alloc_instance(cls)?;
        if let Value::Obj(e) = &o {
            set_args(e, args);
        }
        Ok(o)
    }

    /// Initialize self.  See help(type(self)) for accurate signature.
    #[proto(init)]
    fn init(slf: This<Exc<'_>>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<()> {
        let e = slf.0 .0;
        if !kwargs.is_empty() {
            let n = it.type_name(&it.type_of_obj(e));
            return Err(it.type_error(&format!("{}() takes no keyword arguments", n)));
        }
        set_args(e, args);
        Ok(())
    }

    /// Return str(self).
    #[proto(str)]
    fn str(slf: This<&Value>, it: &mut Interp) -> R<String> {
        it.native_str(&slf)
    }

    /// Return repr(self).
    #[proto(repr)]
    fn repr(slf: This<&Value>, it: &mut Interp) -> R<String> {
        it.native_repr(&slf)
    }

    /// Exception.with_traceback(tb) --
    ///     set self.__traceback__ to tb and return self.
    #[method(hint(py(text_signature = "")))]
    fn with_traceback(slf: This<Exc<'_>>, tb: &Value) -> Value {
        let _ = tb;
        Value::Obj(slf.0 .0.clone())
    }

    /// Exception.add_note(note) --
    ///     add a note to the exception
    #[method(hint(py(text_signature = "")))]
    fn add_note(slf: This<Exc<'_>>, it: &mut Interp, note: &Value) -> R<()> {
        if note.as_str().is_none() {
            let t = it.type_name_of(note);
            return Err(it.type_error(&format!("note must be a str, not '{t}'")));
        }
        let d = it.instance_dict(slf.0 .0);
        match dict_get_str(&d, "__notes__") {
            Some(Value::Obj(l)) => {
                if let Kind::List(l) = &l.kind {
                    l.borrow_mut().push(note.clone());
                }
            }
            _ => dict_set_str(&d, "__notes__", Value::list(vec![note.clone()])),
        }
        Ok(())
    }

    #[method(name = "__reduce__", hint(py(text_signature = "")))]
    fn reduce(slf: This<Exc<'_>>, it: &mut Interp) -> Value {
        let e = slf.0 .0;
        let args = match &e.kind {
            Kind::Exception(d) => d.borrow().args.clone(),
            _ => Value::tuple(Vec::new()),
        };
        let mut out = vec![Value::Obj(it.type_of_obj(e)), args];
        if let Some(d) = e.dict.borrow().as_ref() {
            if matches!(&d.kind, Kind::Dict(m) if !m.borrow().is_empty()) {
                out.push(Value::Obj(d.clone()));
            }
        }
        Value::tuple(out)
    }

    #[method(name = "__setstate__", hint(py(text_signature = "")))]
    fn setstate(slf: This<&Value>, it: &mut Interp, state: &Value) -> R<()> {
        if state.is_none() {
            return Ok(());
        }
        let Some(state) = dict_of(state) else {
            return Err(it.type_error("state is not a dictionary"));
        };
        let entries: Vec<(Value, Value)> = state.borrow().iter().map(|en| (en.key.clone(), en.val.clone())).collect();
        for (k, v) in entries {
            let Value::Obj(name) = &k else { return Err(it.type_error("attribute name must be string")) };
            it.set_attr(&slf, name, v)?;
        }
        Ok(())
    }
}

/// `StopIteration.value`.
#[lumen_bind::class(name = "StopIteration")]
pub struct StopIteration;

#[lumen_bind::methods]
impl StopIteration {
    /// generator return value
    #[getter]
    fn value(slf: This<Exc<'_>>, it: &mut Interp) -> Value {
        let e = slf.0 .0;
        if let Some(v) = e.dict.borrow().as_ref().and_then(|d| dict_get_str(d, "value")) {
            return v;
        }
        it.stop_value(e)
    }
}

/// `SystemExit.code`.
#[lumen_bind::class(name = "SystemExit")]
pub struct SystemExit;

#[lumen_bind::methods]
impl SystemExit {
    /// exception code
    #[getter]
    fn code(slf: This<Exc<'_>>) -> Value {
        let e = slf.0 .0;
        if let Some(v) = e.dict.borrow().as_ref().and_then(|d| dict_get_str(d, "code")) {
            return v;
        }
        match &e.kind {
            Kind::Exception(d) => match d.borrow().args.tuple_items() {
                Some([]) | None => Value::None,
                Some([x]) => x.clone(),
                Some(_) => d.borrow().args.clone(),
            },
            _ => Value::None,
        }
    }
}

/// `ImportError(*args, name=None, path=None)`.
#[lumen_bind::class(name = "ImportError")]
pub struct ImportError;

#[lumen_bind::methods]
impl ImportError {
    /// Initialize self.  See help(type(self)) for accurate signature.
    #[proto(init)]
    fn init(slf: This<Exc<'_>>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<()> {
        let e = slf.0 .0;
        set_args(e, args);
        let d = it.instance_dict(e);
        dict_set_str(&d, "name", Value::None);
        dict_set_str(&d, "path", Value::None);
        dict_set_str(&d, "msg", args.first().cloned().unwrap_or(Value::None));
        keyword_fields(it, &d, kwargs, &["name", "path"], "ImportError")
    }
}

/// `AttributeError(*args, name=None, obj=None)`.
#[lumen_bind::class(name = "AttributeError")]
pub struct AttributeError;

#[lumen_bind::methods]
impl AttributeError {
    /// Initialize self.  See help(type(self)) for accurate signature.
    #[proto(init)]
    fn init(slf: This<Exc<'_>>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<()> {
        let e = slf.0 .0;
        set_args(e, args);
        let d = it.instance_dict(e);
        for n in ["name", "obj"] {
            if dict_get_str(&d, n).is_none() {
                dict_set_str(&d, n, Value::None);
            }
        }
        keyword_fields(it, &d, kwargs, &["name", "obj"], "AttributeError")
    }
}

/// `NameError(*args, name=None)`.
#[lumen_bind::class(name = "NameError")]
pub struct NameError;

#[lumen_bind::methods]
impl NameError {
    /// Initialize self.  See help(type(self)) for accurate signature.
    #[proto(init)]
    fn init(slf: This<Exc<'_>>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<()> {
        let e = slf.0 .0;
        set_args(e, args);
        let d = it.instance_dict(e);
        if dict_get_str(&d, "name").is_none() {
            dict_set_str(&d, "name", Value::None);
        }
        keyword_fields(it, &d, kwargs, &["name"], "NameError")
    }
}

fn unicode_field(e: &Obj, name: &str) -> Option<Value> {
    e.dict.borrow().as_ref().and_then(|d| dict_get_str(d, name))
}

impl Interp {
    /// `str()` of a `UnicodeEncodeError` / `UnicodeDecodeError` / `UnicodeTranslateError`, from its
    /// `encoding`, `object`, `start`, `end` and `reason` attributes.
    pub fn unicode_exc_str(&mut self, e: &Obj) -> R<Option<String>> {
        let (Some(obj), Some(Value::Int(start)), Some(Value::Int(end)), Some(reason)) =
            (unicode_field(e, "object"), unicode_field(e, "start"), unicode_field(e, "end"), unicode_field(e, "reason"))
        else {
            return Ok(None);
        };
        let reason = self.str_of(&reason)?;
        let encoding = match unicode_field(e, "encoding") {
            Some(v) if !v.is_none() => self.str_of(&v)?,
            _ => String::new(),
        };
        let (start, end) = (start.max(0) as usize, end.max(0) as usize);
        let single = end == start + 1;
        if let Value::Obj(o) = &obj {
            if let Kind::Bytes(b) = &o.kind {
                return Ok(Some(match b.get(start) {
                    Some(byte) if single => format!("'{}' codec can't decode byte 0x{:02x} in position {}: {}", encoding, byte, start, reason),
                    _ => format!("'{}' codec can't decode bytes in position {}-{}: {}", encoding, start, end.saturating_sub(1), reason),
                }));
            }
        }
        let what = if encoding.is_empty() { "can't translate".to_string() } else { format!("'{}' codec can't encode", encoding) };
        Ok(Some(match obj.as_str().and_then(|s| lumen_common::smuggle::code_points(s).nth(start)) {
            Some(c) if single => {
                let shown = crate::builtins::codecsm::char_escape(c);
                format!("{} character '{}' in position {}: {}", what, shown, start, reason)
            }
            _ => format!("{} characters in position {}-{}: {}", what, start, end.saturating_sub(1), reason),
        }))
    }
}

const SYNTAX_FIELDS: [&str; 7] = ["filename", "lineno", "offset", "text", "end_lineno", "end_offset", "print_file_and_line"];

/// `SyntaxError(msg, (filename, lineno, offset, text[, end_lineno[, end_offset]]))`.
#[lumen_bind::class(name = "SyntaxError")]
pub struct SyntaxError;

#[lumen_bind::methods]
impl SyntaxError {
    /// Initialize self.  See help(type(self)) for accurate signature.
    #[proto(init)]
    fn init(slf: This<Exc<'_>>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<()> {
        let _ = kwargs;
        let e = slf.0 .0;
        set_args(e, args);
        let d = it.instance_dict(e);
        for n in ["msg"].iter().chain(SYNTAX_FIELDS.iter()) {
            if dict_get_str(&d, n).is_none() {
                dict_set_str(&d, n, Value::None);
            }
        }
        if let Some(m) = args.first() {
            dict_set_str(&d, "msg", m.clone());
        }
        if args.len() == 2 {
            let info = it.iterate_to_vec(&args[1])?;
            if !(4..=6).contains(&info.len()) {
                let msg = if info.len() < 4 {
                    format!("function takes at least 4 arguments ({} given)", info.len())
                } else {
                    format!("function takes at most 6 arguments ({} given)", info.len())
                };
                return Err(it.type_error(&msg));
            }
            for (n, v) in SYNTAX_FIELDS.iter().zip(info.iter()) {
                dict_set_str(&d, n, v.clone());
            }
        }
        Ok(())
    }
}

/// `msg (file, line N)` with the file's base name, as CPython's `SyntaxError_str`.
pub fn syntax_error_str(it: &mut Interp, e: &Obj) -> R<String> {
    let field = |n: &str| e.dict.borrow().as_ref().and_then(|d| dict_get_str(d, n)).unwrap_or(Value::None);
    let msg = field("msg");
    let msg = it.str_of(&msg)?;
    let filename = field("filename");
    let file = filename.as_str().map(|f| f.rsplit('/').next().unwrap_or(f).to_string());
    let line = match field("lineno") {
        Value::Int(n) => Some(n),
        _ => None,
    };
    Ok(match (file, line) {
        (Some(f), Some(l)) => format!("{msg} ({f}, line {l})"),
        (Some(f), None) => format!("{msg} ({f})"),
        (None, Some(l)) => format!("{msg} (line {l})"),
        (None, None) => msg,
    })
}

pub fn init(it: &mut Interp) {
    use crate::bind::extend_type;
    let base = it.exc_type("BaseException");
    extend_type::<BaseException>(it, &base);
    let si = it.exc_type("StopIteration");
    extend_type::<StopIteration>(it, &si);
    let se = it.exc_type("SystemExit");
    extend_type::<SystemExit>(it, &se);
    crate::builtins::oserror::init(it);
    let ie = it.exc_type("ImportError");
    extend_type::<ImportError>(it, &ie);
    let ae = it.exc_type("AttributeError");
    extend_type::<AttributeError>(it, &ae);
    let ne = it.exc_type("NameError");
    extend_type::<NameError>(it, &ne);
    let sy = it.exc_type("SyntaxError");
    extend_type::<SyntaxError>(it, &sy);
}
