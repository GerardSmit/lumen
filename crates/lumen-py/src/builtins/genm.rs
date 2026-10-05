//! Generator, coroutine and async-generator methods.

use crate::bind::{PyCx, PyHost, This};
use crate::object::*;
use crate::vm::*;
use lumen_bind::{FromArg, Slot};

/// A generator, coroutine or async generator.
#[derive(Clone, Copy)]
pub struct Gen<'a>(pub &'a Obj);

impl<'a> FromArg<'a, PyHost> for Gen<'a> {
    #[inline]
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        match v {
            Value::Obj(o) if matches!(o.kind, Kind::Generator(_)) => Ok(Gen(o)),
            _ => Err(cx.arg_error(at, "generator", v)),
        }
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

    /// The exception `throw(typ, val, tb)` raises into the generator; the three-argument form
    /// warns as deprecated, as in CPython 3.12.
    fn throw_exc(
        &mut self,
        name: &str,
        typ: &Value,
        val: Option<&Value>,
        tb: Option<&Value>,
    ) -> R<Obj> {
        if val.is_some() || tb.is_some() {
            let msg = format!("the (type, exc, tb) signature of {name}() is deprecated, use the single-arg signature instead.");
            crate::builtins::warningsm::warn_category(self, "DeprecationWarning", &msg, 1)?;
        }
        let typ = typ.clone();
        let val = val.cloned().filter(|v| !v.is_none());
        let exc = match (&typ, val) {
            (Value::Obj(o), Some(v)) if matches!(o.kind, Kind::Type(_)) => {
                let inst = if matches!(&v, Value::Obj(vo) if matches!(vo.kind, Kind::Exception(_)))
                {
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

// `send`, `throw` and `close`, shared by generators, coroutines and async generators.
#[lumen_bind::class(name = "generator", hint(py(shared)))]
pub struct GenMethods;

#[lumen_bind::methods]
impl GenMethods {
    /// send(arg) -> send 'arg' into generator,
    /// return next yielded value or raise StopIteration.
    #[method(hint(py(text_signature = "")))]
    fn send(slf: This<Gen<'_>>, it: &mut Interp, arg: &Value) -> R<Value> {
        let r = it.gen_send(slf.0 .0, arg.clone())?;
        it.gen_result(r)
    }

    /// throw(value)
    /// throw(type[,value[,tb]])
    ///
    /// Raise exception in generator, return next yielded value or raise
    /// StopIteration.
    /// the (type, val, tb) signature is deprecated,
    /// and may be removed in a future version of Python.
    #[method(hint(py(text_signature = "")))]
    fn throw(
        slf: This<Gen<'_>>,
        it: &mut Interp,
        typ: &Value,
        val: Option<&Value>,
        tb: Option<&Value>,
    ) -> R<Value> {
        let exc = it.throw_exc("throw", typ, val, tb)?;
        let r = it.gen_throw(slf.0 .0, exc)?;
        it.gen_result(r)
    }

    /// close() -> raise GeneratorExit inside generator.
    #[method(hint(py(text_signature = "")))]
    fn close(slf: This<Gen<'_>>, it: &mut Interp) -> R<()> {
        it.gen_close(slf.0 .0)
    }
}

/// The iterator protocol of generators.
#[lumen_bind::class(name = "generator")]
pub struct Generator;

#[lumen_bind::methods]
impl Generator {
    /// Implement iter(self).
    #[proto(iter)]
    fn iter(slf: This<Value>) -> Value {
        slf.0
    }

    /// Implement next(self).
    #[proto(next)]
    fn next(slf: This<Gen<'_>>, it: &mut Interp) -> R<Value> {
        let r = it.gen_send(slf.0 .0, Value::None)?;
        it.gen_result(r)
    }
}

// `coroutine.__await__`.
#[lumen_bind::class(name = "coroutine")]
pub struct Coroutine;

#[lumen_bind::methods]
impl Coroutine {
    /// Return an iterator to be used in await expression.
    #[proto(await)]
    fn await_(slf: This<Value>) -> Value {
        slf.0
    }
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

/// The hooks set by `sys.set_asyncgen_hooks`.
#[derive(Default)]
pub struct AsyncGenHooks {
    pub firstiter: Option<Value>,
    pub finalizer: Option<Value>,
}

/// `async_gen_init_hooks`: on the first `__anext__`/`asend`/`athrow`/`aclose` of `g`, calls
/// the `firstiter` hook with it.
fn agen_init_hooks(it: &mut Interp, g: &Obj) -> R<()> {
    let Kind::Generator(gd) = &g.kind else {
        return Ok(());
    };
    if gd.hooks_inited.replace(true) {
        return Ok(());
    }
    if let Some(f) = it.native_state::<AsyncGenHooks>().firstiter.clone() {
        it.call(&f, vec![Value::Obj(g.clone())], Vec::new())?;
    }
    Ok(())
}

// `async_generator`'s own members.
#[lumen_bind::class(name = "async_generator")]
pub struct AsyncGenerator;

#[lumen_bind::methods]
impl AsyncGenerator {
    /// Return an awaitable, that resolves in asynchronous iterator.
    #[proto(aiter)]
    fn aiter(slf: This<Value>) -> Value {
        slf.0
    }

    /// Return a value or raise StopAsyncIteration.
    #[proto(anext)]
    fn anext(slf: This<Gen<'_>>, it: &mut Interp) -> R<Value> {
        let g = slf.0 .0;
        agen_init_hooks(it, g)?;
        Ok(new_asend(it, &Value::Obj(g.clone()), "anext", Value::None))
    }

    /// asend(v) -> send 'v' in generator.
    #[method(hint(py(text_signature = "")))]
    fn asend(slf: This<Gen<'_>>, it: &mut Interp, arg: &Value) -> R<Value> {
        let g = slf.0 .0;
        agen_init_hooks(it, g)?;
        Ok(new_asend(it, &Value::Obj(g.clone()), "asend", arg.clone()))
    }

    /// athrow(value)
    /// athrow(type[,value[,tb]])
    ///
    /// raise exception in generator.
    /// the (type, val, tb) signature is deprecated,
    /// and may be removed in a future version of Python.
    #[method(hint(py(text_signature = "")))]
    fn athrow(
        slf: This<Gen<'_>>,
        it: &mut Interp,
        typ: &Value,
        val: Option<&Value>,
        tb: Option<&Value>,
    ) -> R<Value> {
        let g = slf.0 .0;
        agen_init_hooks(it, g)?;
        let exc = it.throw_exc("athrow", typ, val, tb)?;
        Ok(new_asend(
            it,
            &Value::Obj(g.clone()),
            "athrow",
            Value::Obj(exc),
        ))
    }

    /// aclose() -> raise GeneratorExit inside generator.
    #[method(hint(py(text_signature = "")))]
    fn aclose(slf: This<Gen<'_>>, it: &mut Interp) -> R<Value> {
        let g = slf.0 .0;
        agen_init_hooks(it, g)?;
        Ok(new_asend(it, &Value::Obj(g.clone()), "aclose", Value::None))
    }
}

fn asend_step(it: &mut Interp, this: &Value, send: Value, throw: Option<Obj>) -> R<Value> {
    let o = match this {
        Value::Obj(o) => o.clone(),
        _ => return Err(it.type_error("bad awaitable")),
    };
    let d = it.instance_dict(&o);
    let get = |n: &str| dict_get_str(&d, n).unwrap_or(Value::None);
    if matches!(get("done"), Value::Bool(true)) {
        return Err(it.new_exc_str(
            "RuntimeError",
            "cannot reuse already awaited __anext__()/asend()",
        ));
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
                let v = if mode == "asend" {
                    get("arg")
                } else {
                    Value::None
                };
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
                        return Err(
                            it.new_exc_str("RuntimeError", "async generator ignored GeneratorExit")
                        );
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
            if mode == "aclose"
                && (it.exc_is(&e, "GeneratorExit") || it.exc_is(&e, "StopAsyncIteration"))
            {
                return Err(it.stop_iteration(Value::None));
            }
            Err(e)
        }
    }
}

// The awaitable `__anext__`, `asend`, `athrow` and `aclose` return.
#[lumen_bind::class(name = "async_generator_asend")]
pub struct AsyncGenAsend;

#[lumen_bind::methods]
impl AsyncGenAsend {
    /// Return an iterator to be used in await expression.
    #[proto(await)]
    fn await_(slf: This<Value>) -> Value {
        slf.0
    }

    /// Implement iter(self).
    #[proto(iter)]
    fn iter(slf: This<Value>) -> Value {
        slf.0
    }

    /// Implement next(self).
    #[proto(next)]
    fn next(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        asend_step(it, &slf, Value::None, None)
    }

    /// send(arg) -> send 'arg' into generator,
    /// return next yielded value or raise StopIteration.
    #[method(hint(py(text_signature = "")))]
    fn send(slf: This<&Value>, it: &mut Interp, arg: &Value) -> R<Value> {
        asend_step(it, &slf, arg.clone(), None)
    }

    /// throw(value)
    /// throw(type[,value[,tb]])
    ///
    /// Raise exception in generator, return next yielded value or raise
    /// StopIteration.
    /// the (type, val, tb) signature is deprecated,
    /// and may be removed in a future version of Python.
    #[method(hint(py(text_signature = "")))]
    fn throw(
        slf: This<&Value>,
        it: &mut Interp,
        typ: &Value,
        val: Option<&Value>,
        tb: Option<&Value>,
    ) -> R<Value> {
        let exc = it.throw_exc("throw", typ, val, tb)?;
        asend_step(it, &slf, Value::None, Some(exc))
    }

    /// close() -> raise GeneratorExit inside generator.
    #[method(hint(py(text_signature = "")))]
    fn close(slf: This<&Value>, it: &mut Interp) {
        if let Value::Obj(o) = *slf {
            let d = it.instance_dict(o);
            dict_set_str(&d, "done", Value::Bool(true));
        }
    }
}

pub fn init(it: &mut Interp) {
    for t in [
        it.types.generator.clone(),
        it.types.coroutine.clone(),
        it.types.async_generator.clone(),
    ] {
        crate::bind::install_into::<GenMethods>(&t, &["send", "throw", "close"]);
    }
    let g = it.types.generator.clone();
    crate::bind::extend_type::<Generator>(it, &g);
    let c = it.types.coroutine.clone();
    crate::bind::extend_type::<Coroutine>(it, &c);
    let ag = it.types.async_generator.clone();
    crate::bind::extend_type::<AsyncGenerator>(it, &ag);
    let w = it.types.asend.clone();
    crate::bind::extend_type::<AsyncGenAsend>(it, &w);
}
