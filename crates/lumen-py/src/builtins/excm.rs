//! Exception classes: construction, `args`, notes and the attributes of the specialised types.

use crate::object::*;
use crate::vm::*;

type Kw<'a> = &'a [(Obj, Value)];

fn exc_args_cell<'a>(it: &mut Interp, a: &'a [Value]) -> R<&'a Obj> {
    match a.first() {
        Some(Value::Obj(o)) if matches!(o.kind, Kind::Exception(_)) => Ok(o),
        _ => Err(it.type_error("descriptor requires a 'BaseException' object")),
    }
}

fn exc_new(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let cls = match a.first() {
        Some(Value::Obj(c)) if matches!(c.kind, Kind::Type(_)) => c.clone(),
        _ => return Err(it.type_error("BaseException.__new__(X): X is not a type object")),
    };
    let o = it.alloc_instance(&cls)?;
    if let Value::Obj(e) = &o {
        if let Kind::Exception(d) = &e.kind {
            d.borrow_mut().args = Value::tuple(a[1..].to_vec());
        }
    }
    Ok(o)
}

fn exc_init(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let e = exc_args_cell(it, a)?;
    if !kw.is_empty() {
        let n = it.type_name_of(&a[0]);
        return Err(it.type_error(&format!("{}() takes no keyword arguments", n)));
    }
    if let Kind::Exception(d) = &e.kind {
        d.borrow_mut().args = Value::tuple(a[1..].to_vec());
    }
    Ok(Value::None)
}

fn exc_str(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__str__", a, 1, 1)?;
    Ok(Value::string(it.native_str(&a[0])?))
}

fn unicode_field(e: &Obj, name: &str) -> Option<Value> {
    e.dict.borrow().as_ref().and_then(|d| dict_get_str(d, name))
}

/// `UnicodeEncodeError.__init__` and friends: `(encoding, object, start, end, reason)`, without
/// `encoding` for `UnicodeTranslateError`.
fn unicode_exc_init(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let e = exc_args_cell(it, a)?.clone();
    let args = &a[1..];
    if let Kind::Exception(d) = &e.kind {
        d.borrow_mut().args = Value::tuple(args.to_vec());
    }
    let translate = it.exc_is(&e, "UnicodeTranslateError");
    let decode = it.exc_is(&e, "UnicodeDecodeError");
    let want = if translate { 4 } else { 5 };
    if args.len() != want {
        return Err(it.type_error(&format!("function takes exactly {} arguments ({} given)", want, args.len())));
    }
    let (encoding, rest) = if translate { (Value::None, args) } else { (args[0].clone(), &args[1..]) };
    let check_str = |it: &mut Interp, v: &Value, n: usize| -> R<()> {
        if v.as_str().is_some() {
            return Ok(());
        }
        let t = it.type_name_of(v);
        Err(it.type_error(&format!("argument {} must be str, not {}", n, t)))
    };
    let off = if translate { 0 } else { 1 };
    if !translate {
        check_str(it, &encoding, 1)?;
    }
    let object = if decode {
        Value::bytes(it.bytes_of(&rest[0])?)
    } else {
        check_str(it, &rest[0], off + 1)?;
        rest[0].clone()
    };
    let start = it.index_of(&rest[1])?;
    let end = it.index_of(&rest[2])?;
    check_str(it, &rest[3], off + 4)?;
    let d = it.instance_dict(&e);
    dict_set_str(&d, "encoding", encoding);
    dict_set_str(&d, "object", object);
    dict_set_str(&d, "start", Value::Int(start));
    dict_set_str(&d, "end", Value::Int(end));
    dict_set_str(&d, "reason", rest[3].clone());
    Ok(Value::None)
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

fn exc_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__repr__", a, 1, 1)?;
    Ok(Value::string(it.native_repr(&a[0])?))
}

fn with_traceback(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("with_traceback", a, 2, 2)?;
    Ok(a[0].clone())
}

fn add_note(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("add_note", a, 2, 2)?;
    if a[1].as_str().is_none() {
        return Err(it.type_error("note must be a str"));
    }
    let e = exc_args_cell(it, a)?.clone();
    let d = it.instance_dict(&e);
    match dict_get_str(&d, "__notes__") {
        Some(Value::Obj(l)) => {
            if let Kind::List(l) = &l.kind {
                l.borrow_mut().push(a[1].clone());
            }
        }
        _ => dict_set_str(&d, "__notes__", Value::list(vec![a[1].clone()])),
    }
    Ok(Value::None)
}

fn exc_reduce(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__reduce__", a, 1, 1)?;
    let e = exc_args_cell(it, a)?.clone();
    let args = match &e.kind {
        Kind::Exception(d) => d.borrow().args.clone(),
        _ => Value::tuple(Vec::new()),
    };
    let mut out = vec![Value::Obj(it.type_of_obj(&e)), args];
    if let Some(d) = e.dict.borrow().as_ref() {
        if matches!(&d.kind, Kind::Dict(m) if !m.borrow().is_empty()) {
            out.push(Value::Obj(d.clone()));
        }
    }
    Ok(Value::tuple(out))
}

fn exc_setstate(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__setstate__", a, 2, 2)?;
    if a[1].is_none() {
        return Ok(Value::None);
    }
    let Some(state) = dict_of(&a[1]) else {
        return Err(it.type_error("state is not a dictionary"));
    };
    let entries: Vec<(Value, Value)> = state.borrow().iter().map(|en| (en.key.clone(), en.val.clone())).collect();
    for (k, v) in entries {
        let Value::Obj(name) = &k else { return Err(it.type_error("attribute name must be string")) };
        it.set_attr(&a[0], name, v)?;
    }
    Ok(Value::None)
}

fn stop_value_prop(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let e = exc_args_cell(it, a)?.clone();
    if let Some(d) = e.dict.borrow().as_ref() {
        if let Some(v) = dict_get_str(d, "value") {
            return Ok(v);
        }
    }
    Ok(it.stop_value(&e))
}

fn exit_code_prop(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let e = exc_args_cell(it, a)?.clone();
    if let Some(d) = e.dict.borrow().as_ref() {
        if let Some(v) = dict_get_str(d, "code") {
            return Ok(v);
        }
    }
    Ok(match &e.kind {
        Kind::Exception(d) => match d.borrow().args.tuple_items() {
            Some([]) | None => Value::None,
            Some([x]) => x.clone(),
            Some(_) => d.borrow().args.clone(),
        },
        _ => Value::None,
    })
}

fn import_error_init(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let e = exc_args_cell(it, a)?.clone();
    if let Kind::Exception(d) = &e.kind {
        d.borrow_mut().args = Value::tuple(a[1..].to_vec());
    }
    let d = it.instance_dict(&e);
    dict_set_str(&d, "name", Value::None);
    dict_set_str(&d, "path", Value::None);
    dict_set_str(&d, "msg", a.get(1).cloned().unwrap_or(Value::None));
    for (k, v) in kw {
        match k.as_str_kind().unwrap_or("") {
            "name" | "path" => dict_set_name(&d, k, v.clone()),
            other => return Err(it.type_error(&format!("'{}' is an invalid keyword argument for ImportError()", other))),
        }
    }
    Ok(Value::None)
}

fn attr_error_init(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let e = exc_args_cell(it, a)?.clone();
    if let Kind::Exception(d) = &e.kind {
        d.borrow_mut().args = Value::tuple(a[1..].to_vec());
    }
    let d = it.instance_dict(&e);
    if dict_get_str(&d, "name").is_none() {
        dict_set_str(&d, "name", Value::None);
    }
    if dict_get_str(&d, "obj").is_none() {
        dict_set_str(&d, "obj", Value::None);
    }
    for (k, v) in kw {
        match k.as_str_kind().unwrap_or("") {
            "name" | "obj" => dict_set_name(&d, k, v.clone()),
            other => return Err(it.type_error(&format!("'{}' is an invalid keyword argument for AttributeError()", other))),
        }
    }
    Ok(Value::None)
}

fn name_error_init(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let e = exc_args_cell(it, a)?.clone();
    if let Kind::Exception(d) = &e.kind {
        d.borrow_mut().args = Value::tuple(a[1..].to_vec());
    }
    let d = it.instance_dict(&e);
    if dict_get_str(&d, "name").is_none() {
        dict_set_str(&d, "name", Value::None);
    }
    for (k, v) in kw {
        match k.as_str_kind().unwrap_or("") {
            "name" => dict_set_name(&d, k, v.clone()),
            other => return Err(it.type_error(&format!("'{}' is an invalid keyword argument for NameError()", other))),
        }
    }
    Ok(Value::None)
}

fn syntax_error_init(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let e = exc_args_cell(it, a)?.clone();
    if let Kind::Exception(d) = &e.kind {
        d.borrow_mut().args = Value::tuple(a[1..].to_vec());
    }
    let d = it.instance_dict(&e);
    dict_set_str(&d, "msg", a.get(1).cloned().unwrap_or(Value::None));
    if let Some(Value::Obj(det)) = a.get(2) {
        if let Kind::Tuple(t) = &det.kind {
            for (n, v) in ["filename", "lineno", "offset", "text"].iter().zip(t.iter()) {
                dict_set_str(&d, n, v.clone());
            }
        }
    }
    for n in ["filename", "lineno", "offset", "text"] {
        if dict_get_str(&d, n).is_none() {
            dict_set_str(&d, n, Value::None);
        }
    }
    Ok(Value::None)
}

pub fn init(it: &mut Interp) {
    let base = it.exc_type("BaseException");
    it.reg_new(&base, exc_new);
    it.reg(&base, "__init__", exc_init);
    it.reg(&base, "__str__", exc_str);
    it.reg(&base, "__repr__", exc_repr);
    it.reg(&base, "with_traceback", with_traceback);
    it.reg(&base, "add_note", add_note);
    it.reg(&base, "__reduce__", exc_reduce);
    it.reg(&base, "__setstate__", exc_setstate);
    let si = it.exc_type("StopIteration");
    it.reg_prop(&si, "value", stop_value_prop);
    let se = it.exc_type("SystemExit");
    it.reg_prop(&se, "code", exit_code_prop);
    crate::builtins::oserror::init(it);
    let ie = it.exc_type("ImportError");
    it.reg(&ie, "__init__", import_error_init);
    let ae = it.exc_type("AttributeError");
    it.reg(&ae, "__init__", attr_error_init);
    let ne = it.exc_type("NameError");
    it.reg(&ne, "__init__", name_error_init);
    for name in ["UnicodeEncodeError", "UnicodeDecodeError", "UnicodeTranslateError"] {
        let t = it.exc_type(name);
        it.reg(&t, "__init__", unicode_exc_init);
    }
    let sy = it.exc_type("SyntaxError");
    it.reg(&sy, "__init__", syntax_error_init);
}
