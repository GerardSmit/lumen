//! Generator, coroutine and async-generator methods.

use crate::object::*;
use crate::vm::*;

type Kw<'a> = &'a [(Obj, Value)];

fn gen_of<'a>(it: &mut Interp, a: &'a [Value]) -> R<&'a Obj> {
    match a.first() {
        Some(Value::Obj(o)) if matches!(o.kind, Kind::Generator(_)) => Ok(o),
        _ => Err(it.type_error("descriptor requires a generator object")),
    }
}

impl Interp {
    pub fn stop_iteration(&mut self, v: Value) -> Obj {
        let cls = self.exc_type("StopIteration");
        let args = if v.is_none() { Vec::new() } else { vec![v] };
        self.new_exc(&cls, args)
    }

    fn gen_result(&mut self, r: GenResult) -> R<Value> {
        match r {
            GenResult::Yield(v) => Ok(v),
            GenResult::Return(v) => Err(self.stop_iteration(v)),
        }
    }

    fn throw_exc(&mut self, a: &[Value]) -> R<Obj> {
        let typ = a.first().cloned().unwrap_or(Value::None);
        let val = a.get(1).cloned().filter(|v| !v.is_none());
        let exc = match (&typ, val) {
            (Value::Obj(o), Some(v)) if matches!(o.kind, Kind::Type(_)) => {
                let inst = if matches!(&v, Value::Obj(vo) if matches!(vo.kind, Kind::Exception(_))) {
                    v
                } else {
                    let args = match v.tuple_items() {
                        Some(t) => t.to_vec(),
                        None => vec![v],
                    };
                    self.call(&typ, args, Vec::new())?
                };
                Some(inst)
            }
            _ => Some(typ),
        };
        self.do_raise(exc, None)
    }
}

fn gen_next(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let g = gen_of(it, a)?.clone();
    let r = it.gen_send(&g, Value::None)?;
    it.gen_result(r)
}

fn gen_send(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("send", a, 2, 2)?;
    let g = gen_of(it, a)?.clone();
    let r = it.gen_send(&g, a[1].clone())?;
    it.gen_result(r)
}

fn gen_throw(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("throw", a, 2, 4)?;
    let g = gen_of(it, a)?.clone();
    let exc = it.throw_exc(&a[1..])?;
    let r = it.gen_throw(&g, exc)?;
    it.gen_result(r)
}

fn gen_close(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("close", a, 1, 1)?;
    let g = gen_of(it, a)?.clone();
    it.gen_close(&g)?;
    Ok(Value::None)
}

fn self_ret(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__iter__", a, 1, 1)?;
    Ok(a[0].clone())
}

// ---- async generators --------------------------------------------------------------------------

fn new_asend(it: &mut Interp, g: &Value, mode: &str, arg: Value) -> Value {
    let o = Object::with_cls(it.types.asend.clone(), Kind::Instance);
    let d = it.instance_dict(&o);
    dict_set_str(&d, "agen", g.clone());
    dict_set_str(&d, "mode", Value::str(mode));
    dict_set_str(&d, "arg", arg);
    dict_set_str(&d, "started", Value::Bool(false));
    dict_set_str(&d, "done", Value::Bool(false));
    Value::Obj(o)
}

fn agen_anext(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__anext__", a, 1, 1)?;
    gen_of(it, a)?;
    Ok(new_asend(it, &a[0], "anext", Value::None))
}

fn agen_asend(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("asend", a, 2, 2)?;
    gen_of(it, a)?;
    Ok(new_asend(it, &a[0], "asend", a[1].clone()))
}

fn agen_athrow(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("athrow", a, 2, 4)?;
    gen_of(it, a)?;
    let exc = it.throw_exc(&a[1..])?;
    Ok(new_asend(it, &a[0], "athrow", Value::Obj(exc)))
}

fn agen_aclose(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("aclose", a, 1, 1)?;
    gen_of(it, a)?;
    Ok(new_asend(it, &a[0], "aclose", Value::None))
}

fn asend_step(it: &mut Interp, this: &Value, send: Value, throw: Option<Obj>) -> R<Value> {
    let o = match this {
        Value::Obj(o) => o.clone(),
        _ => return Err(it.type_error("bad awaitable")),
    };
    let d = it.instance_dict(&o);
    let get = |n: &str| dict_get_str(&d, n).unwrap_or(Value::None);
    if matches!(get("done"), Value::Bool(true)) {
        return Err(it.new_exc_str("RuntimeError", "cannot reuse already awaited __anext__()/asend()"));
    }
    let g = match get("agen") {
        Value::Obj(g) => g,
        _ => return Err(it.type_error("bad async generator")),
    };
    let mode = get("mode").as_str().unwrap_or("").to_string();
    let started = matches!(get("started"), Value::Bool(true));
    dict_set_str(&d, "started", Value::Bool(true));
    let r = if !started {
        match mode.as_str() {
            "anext" | "asend" => {
                let v = if mode == "asend" { get("arg") } else { Value::None };
                match throw {
                    Some(e) => it.gen_throw(&g, e),
                    None => it.gen_send(&g, v),
                }
            }
            "athrow" => match get("arg") {
                Value::Obj(e) => it.gen_throw(&g, e),
                _ => Err(it.type_error("bad exception")),
            },
            _ => {
                let e = it.new_exc_str("GeneratorExit", "");
                it.gen_throw(&g, e)
            }
        }
    } else {
        match throw {
            Some(e) => it.gen_throw(&g, e),
            None => it.gen_send(&g, send),
        }
    };
    let finish = |d: &Obj| dict_set_str(d, "done", Value::Bool(true));
    match r {
        Ok(GenResult::Yield(v)) => {
            if let Value::Obj(vo) = &v {
                if let Kind::AsyncGenValue(x) = &vo.kind {
                    finish(&d);
                    if mode == "aclose" {
                        return Err(it.new_exc_str("RuntimeError", "async generator ignored GeneratorExit"));
                    }
                    return Err(it.stop_iteration(x.clone()));
                }
            }
            Ok(v)
        }
        Ok(GenResult::Return(_)) => {
            finish(&d);
            if mode == "aclose" {
                Err(it.stop_iteration(Value::None))
            } else {
                Err(it.new_exc_str("StopAsyncIteration", ""))
            }
        }
        Err(e) => {
            finish(&d);
            if mode == "aclose" && (it.exc_is(&e, "GeneratorExit") || it.exc_is(&e, "StopAsyncIteration")) {
                return Err(it.stop_iteration(Value::None));
            }
            Err(e)
        }
    }
}

fn asend_next(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    asend_step(it, &a[0], Value::None, None)
}

fn asend_send(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("send", a, 2, 2)?;
    asend_step(it, &a[0], a[1].clone(), None)
}

fn asend_throw(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("throw", a, 2, 4)?;
    let exc = it.throw_exc(&a[1..])?;
    asend_step(it, &a[0], Value::None, Some(exc))
}

fn asend_close(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let _ = it;
    if let Value::Obj(o) = &a[0] {
        let d = it.instance_dict(o);
        dict_set_str(&d, "done", Value::Bool(true));
    }
    Ok(Value::None)
}

pub fn init(it: &mut Interp) {
    for t in [it.types.generator.clone(), it.types.coroutine.clone(), it.types.async_generator.clone()] {
        it.reg(&t, "send", gen_send);
        it.reg(&t, "throw", gen_throw);
        it.reg(&t, "close", gen_close);
    }
    let g = it.types.generator.clone();
    it.reg(&g, "__iter__", self_ret);
    it.reg(&g, "__next__", gen_next);
    let c = it.types.coroutine.clone();
    it.reg(&c, "__await__", self_ret);
    let ag = it.types.async_generator.clone();
    it.reg(&ag, "__aiter__", self_ret);
    it.reg(&ag, "__anext__", agen_anext);
    it.reg(&ag, "asend", agen_asend);
    it.reg(&ag, "athrow", agen_athrow);
    it.reg(&ag, "aclose", agen_aclose);
    let w = it.types.asend.clone();
    it.reg(&w, "__await__", self_ret);
    it.reg(&w, "__iter__", self_ret);
    it.reg(&w, "__next__", asend_next);
    it.reg(&w, "send", asend_send);
    it.reg(&w, "throw", asend_throw);
    it.reg(&w, "close", asend_close);
}
