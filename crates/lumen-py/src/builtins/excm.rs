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

fn oserror_init(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let e = exc_args_cell(it, a)?.clone();
    let _ = kw;
    let args = a[1..].to_vec();
    if let Kind::Exception(d) = &e.kind {
        let shown = if args.len() > 2 { args[..2].to_vec() } else { args.clone() };
        d.borrow_mut().args = Value::tuple(shown);
    }
    let d = it.instance_dict(&e);
    if args.len() >= 2 {
        dict_set_str(&d, "errno", args[0].clone());
        dict_set_str(&d, "strerror", args[1].clone());
        if let Some(f) = args.get(2) {
            dict_set_str(&d, "filename", f.clone());
        }
    }
    for n in ["errno", "strerror", "filename"] {
        if dict_get_str(&d, n).is_none() {
            dict_set_str(&d, n, Value::None);
        }
    }
    Ok(Value::None)
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
    let si = it.exc_type("StopIteration");
    it.reg_prop(&si, "value", stop_value_prop);
    let se = it.exc_type("SystemExit");
    it.reg_prop(&se, "code", exit_code_prop);
    let oe = it.exc_type("OSError");
    it.reg(&oe, "__init__", oserror_init);
    let ie = it.exc_type("ImportError");
    it.reg(&ie, "__init__", import_error_init);
    let ae = it.exc_type("AttributeError");
    it.reg(&ae, "__init__", attr_error_init);
    let ne = it.exc_type("NameError");
    it.reg(&ne, "__init__", name_error_init);
    let sy = it.exc_type("SyntaxError");
    it.reg(&sy, "__init__", syntax_error_init);
}
