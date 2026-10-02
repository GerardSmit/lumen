//! `BaseExceptionGroup` / `ExceptionGroup`: construction, `split`, `subgroup`, `derive` and the
//! helpers behind `except*`.

use crate::bind::{Inst, KwArgs, This};
use crate::object::*;
use crate::vm::*;
use std::rc::Rc;

type Group<'a> = Inst<'a, BaseExceptionGroup>;

pub fn is_group(it: &Interp, v: &Value) -> bool {
    matches!(v, Value::Obj(o) if matches!(o.kind, Kind::Exception(_)) && it.exc_is(o, "BaseExceptionGroup"))
}

pub fn group_items(e: &Obj) -> Vec<Value> {
    e.dict
        .borrow()
        .as_ref()
        .and_then(|d| dict_get_str(d, "exceptions"))
        .and_then(|t| t.tuple_items().map(|x| x.to_vec()))
        .unwrap_or_default()
}

fn group_message(e: &Obj) -> Value {
    e.dict.borrow().as_ref().and_then(|d| dict_get_str(d, "message")).unwrap_or_else(|| Value::str(""))
}

pub fn group_text(e: &Obj) -> String {
    let n = group_items(e).len();
    let msg = group_message(e);
    format!("{} ({} sub-exception{})", msg.as_str().unwrap_or(""), n, if n == 1 { "" } else { "s" })
}

// `BaseExceptionGroup`'s members (installed into the core type).
#[lumen_bind::class(name = "BaseExceptionGroup")]
pub struct BaseExceptionGroup;

#[lumen_bind::methods]
impl BaseExceptionGroup {
    #[constructor(hint(py(arg_style = "parse", arg_name = "BaseExceptionGroup.__new__", text_signature = "")))]
    fn new(cls: This<Value>, it: &mut Interp, message: &Value, exceptions: &Value, #[varkw] kw: KwArgs) -> R<Value> {
        let Value::Obj(cls) = &*cls else { unreachable!("checked by the entry") };
        let cname = it.type_name(cls);
        if !kw.is_empty() {
            return Err(it.type_error(&format!("{}() takes no keyword arguments", cname)));
        }
        let Some(msg) = message.as_str().map(str::to_string) else {
            let t = it.type_name_of(message);
            return Err(it.type_error(&format!("BaseExceptionGroup.__new__() argument 1 must be str, not {}", t)));
        };
        let seq_ok = matches!(exceptions, Value::Obj(o) if matches!(o.kind, Kind::List(_) | Kind::Tuple(_)));
        if !seq_ok {
            return Err(it.type_error("second argument (exceptions) must be a sequence"));
        }
        let items = it.iterate_to_vec(exceptions)?;
        if items.is_empty() {
            return Err(it.value_error("second argument (exceptions) must be a non-empty sequence"));
        }
        let base_exc = Value::Obj(it.exc_type("BaseException"));
        let exc_cls = it.exc_type("Exception");
        let exc_val = Value::Obj(exc_cls.clone());
        let mut all_exc = true;
        for (i, x) in items.iter().enumerate() {
            if !it.isinstance_value(x, &base_exc)? {
                return Err(it.value_error(&format!("Item {} of second argument (exceptions) is not an exception", i)));
            }
            if !it.isinstance_value(x, &exc_val)? {
                all_exc = false;
            }
        }
        let beg = it.exc_type("BaseExceptionGroup");
        let eg = it.exc_type("ExceptionGroup");
        let target = if Rc::ptr_eq(cls, &beg) {
            if all_exc {
                eg
            } else {
                beg
            }
        } else {
            if !all_exc && it.is_subtype(cls, &exc_cls) {
                let msg = if Rc::ptr_eq(cls, &eg) { "Cannot nest BaseExceptions in an ExceptionGroup".to_string() } else { format!("Cannot nest BaseExceptions in '{}'", cname) };
                return Err(it.type_error(&msg));
            }
            cls.clone()
        };
        let o = it.alloc_instance(&target)?;
        if let Value::Obj(e) = &o {
            if let Kind::Exception(d) = &e.kind {
                d.borrow_mut().args = Value::tuple(vec![message.clone(), exceptions.clone()]);
            }
            let d = it.instance_dict(e);
            dict_set_str(&d, "message", Value::string(msg));
            dict_set_str(&d, "exceptions", Value::tuple(items));
        }
        Ok(o)
    }

    #[proto(init)]
    fn init(_slf: This<Group<'_>>, #[varargs] _args: &[Value], #[varkw] _kw: KwArgs) {}

    #[proto(str)]
    fn str(slf: This<Group<'_>>) -> String {
        group_text(slf.0 .0)
    }

    #[method(hint(py(arg_style = "unpack", text_signature = "")))]
    fn split(slf: This<Group<'_>>, it: &mut Interp, matcher_value: &Value) -> R<Value> {
        let (m, r) = split_value(it, &Value::Obj(slf.0 .0.clone()), matcher_value)?;
        Ok(Value::tuple(vec![m.unwrap_or(Value::None), r.unwrap_or(Value::None)]))
    }

    #[method(hint(py(arg_style = "unpack", text_signature = "")))]
    fn subgroup(slf: This<Group<'_>>, it: &mut Interp, matcher_value: &Value) -> R<Value> {
        let (m, _) = split_value(it, &Value::Obj(slf.0 .0.clone()), matcher_value)?;
        Ok(m.unwrap_or(Value::None))
    }

    #[method(hint(py(arg_style = "unpack", text_signature = "")))]
    fn derive(slf: This<Group<'_>>, it: &mut Interp, excs: &Value) -> R<Value> {
        let beg = Value::Obj(it.exc_type("BaseExceptionGroup"));
        it.call(&beg, vec![group_message(slf.0 .0), excs.clone()], Vec::new())
    }
}

fn matches_cond(it: &mut Interp, e: &Value, cond: &Value) -> R<bool> {
    let is_type = matches!(cond, Value::Obj(c) if matches!(c.kind, Kind::Type(_)));
    let is_tuple_of_types = cond.tuple_items().is_some_and(|t| t.iter().all(|x| matches!(x, Value::Obj(c) if matches!(c.kind, Kind::Type(_)))));
    if is_type || is_tuple_of_types {
        return it.isinstance_value(e, cond);
    }
    let r = it.call(cond, vec![e.clone()], Vec::new())?;
    it.truthy(&r)
}

fn copy_meta(it: &mut Interp, from: &Obj, to: &Value) {
    let Value::Obj(t) = to else { return };
    if let (Kind::Exception(f), Kind::Exception(d)) = (&from.kind, &t.kind) {
        let f = f.borrow();
        let mut d = d.borrow_mut();
        d.tb = f.tb.clone();
        d.cause = f.cause.clone();
        d.context = f.context.clone();
        d.suppress_context = f.suppress_context;
        d.ctx_set = f.ctx_set;
    }
    let notes = from.dict.borrow().as_ref().and_then(|d| dict_get_str(d, "__notes__"));
    if let Some(Value::Obj(l)) = &notes {
        if let Kind::List(l) = &l.kind {
            let copy = Value::list(l.borrow().clone());
            let d = it.instance_dict(t);
            dict_set_str(&d, "__notes__", copy);
        }
    }
}

fn split_value(it: &mut Interp, exc: &Value, cond: &Value) -> R<(Option<Value>, Option<Value>)> {
    if matches_cond(it, exc, cond)? {
        return Ok((Some(exc.clone()), None));
    }
    let Value::Obj(e) = exc else { return Ok((None, Some(exc.clone()))) };
    if !is_group(it, exc) {
        return Ok((None, Some(exc.clone())));
    }
    let mut matched = Vec::new();
    let mut rest = Vec::new();
    for sub in group_items(e) {
        let (m, r) = split_value(it, &sub, cond)?;
        if let Some(m) = m {
            matched.push(m);
        }
        if let Some(r) = r {
            rest.push(r);
        }
    }
    if matched.is_empty() {
        return Ok((None, Some(exc.clone())));
    }
    let build = |it: &mut Interp, items: Vec<Value>| -> R<Option<Value>> {
        if items.is_empty() {
            return Ok(None);
        }
        let g = it.call_method(exc, "derive", vec![Value::list(items)])?;
        copy_meta(it, e, &g);
        Ok(Some(g))
    };
    let m = build(it, matched)?;
    let r = build(it, rest)?;
    Ok((m, r))
}

/// Wraps a plain exception so `except*` can treat it as a group; remembers the original.
pub fn star_wrap(it: &mut Interp, exc: &Value) -> R<Value> {
    if is_group(it, exc) {
        return Ok(exc.clone());
    }
    let beg = Value::Obj(it.exc_type("BaseExceptionGroup"));
    let g = it.call(&beg, vec![Value::str(""), Value::list(vec![exc.clone()])], Vec::new())?;
    if let Value::Obj(go) = &g {
        let d = it.instance_dict(go);
        dict_set_str(&d, "__star_orig__", exc.clone());
    }
    Ok(g)
}

/// Undoes `star_wrap` when nothing of the wrapped exception was consumed.
pub fn star_unwrap(rest: Value) -> Value {
    if let Value::Obj(o) = &rest {
        let orig = o.dict.borrow().as_ref().and_then(|d| dict_get_str(d, "__star_orig__"));
        if let Some(orig) = orig {
            return orig;
        }
    }
    rest
}

pub fn star_split(it: &mut Interp, rem: &Value, cond: &Value) -> R<(Value, Value)> {
    let is_ty = |v: &Value| matches!(v, Value::Obj(c) if matches!(c.kind, Kind::Type(_)));
    let ok = is_ty(cond) || cond.tuple_items().is_some_and(|t| t.iter().all(is_ty));
    if !ok {
        return Err(it.type_error("catching classes that do not inherit from BaseException is not allowed"));
    }
    let (m, r) = split_value(it, rem, cond)?;
    let m = m.unwrap_or(Value::None);
    if let (Value::Obj(mo), Value::Obj(ro)) = (&m, rem) {
        if Rc::ptr_eq(mo, ro) {
            return Ok((Value::None, m));
        }
    }
    Ok((r.unwrap_or(Value::None), m))
}

pub fn init(it: &mut Interp) {
    let beg = it.exc_type("BaseExceptionGroup");
    crate::bind::extend_type::<BaseExceptionGroup>(it, &beg);
}
