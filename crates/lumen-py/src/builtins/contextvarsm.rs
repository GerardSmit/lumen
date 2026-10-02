//! `_contextvars`: `Context`, `ContextVar` and `Token`. A context is a copy-on-write map from
//! `ContextVar` objects (by identity) to values; `Interp::context` holds the current one.

use crate::bind::This;
use crate::object::*;
use std::rc::Rc;

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Obj(a), Value::Obj(b)) => Rc::ptr_eq(a, b),
        _ => false,
    }
}

/// Context Variables
#[lumen_bind::module(name = "_contextvars")]
pub mod _contextvars {
    use super::same;
    use crate::bind::{type_object, KwArgs, Py, This};
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};
    use lumen_bind::Passed;
    use std::rc::Rc;

    type Vars = Rc<Vec<(Value, Value)>>;

    fn lookup(vars: &Vars, var: &Value) -> Option<Value> {
        vars.iter().find(|(k, _)| same(k, var)).map(|(_, v)| v.clone())
    }

    fn with_set(vars: &Vars, var: &Value, val: Option<Value>) -> Vars {
        let mut out: Vec<(Value, Value)> = vars.iter().filter(|(k, _)| !same(k, var)).cloned().collect();
        if let Some(v) = val {
            match vars.iter().position(|(k, _)| same(k, var)) {
                Some(i) => out.insert(i, (var.clone(), v)),
                None => out.push((var.clone(), v)),
            }
        }
        Rc::new(out)
    }

    #[class(name = "Context", hint(py(final, unhashable)))]
    pub struct Context {
        vars: Vars,
        entered: bool,
        prev: Option<Value>,
    }

    #[class(name = "ContextVar", generic, hint(py(final)))]
    pub struct ContextVar {
        name: Value,
        default: Option<Value>,
    }

    #[class(name = "Token", generic, hint(py(final)))]
    pub struct Token {
        ctx: Value,
        var: Value,
        old: Option<Value>,
        used: bool,
    }

    fn new_context(it: &mut Interp, vars: Vars) -> Py<Context> {
        Py::new(it, Context { vars, entered: false, prev: None })
    }

    /// The current context, created empty on first use.
    fn current(it: &mut Interp) -> Py<Context> {
        if let Some(c) = it.context.as_ref().and_then(|c| Py::from_value(it, c)) {
            return c;
        }
        let c = new_context(it, Rc::new(Vec::new()));
        it.context = Some(c.value().clone());
        c
    }

    fn key<'a>(it: &mut Interp, v: &'a Value) -> R<&'a Value> {
        if Py::<ContextVar>::from_value(it, v).is_some() {
            return Ok(v);
        }
        let r = it.repr_of(v)?;
        Err(it.type_error(&format!("a ContextVar key was expected, got {r}")))
    }

    fn vars_of(it: &mut Interp, c: &Py<Context>) -> R<Vars> {
        Ok(c.borrow(it)?.vars.clone())
    }

    /// Return a copy of the current context.
    #[op]
    fn copy_context(it: &mut Interp) -> R<Value> {
        let cur = current(it);
        let vars = vars_of(it, &cur)?;
        Ok(new_context(it, vars).into_value())
    }

    #[methods]
    impl Context {
        #[constructor]
        fn new(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Context> {
            if !args.is_empty() || !kwargs.is_empty() {
                return Err(it.type_error("Context() does not accept any arguments"));
            }
            Ok(Context { vars: Rc::new(Vec::new()), entered: false, prev: None })
        }

        /// Call callable in the context, passing the remaining arguments.
        #[method(hint(py(text_signature = "")))]
        fn run(slf: This<Py<Self>>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
            // CPython checks this by hand, with its own wording.
            let Some((callable, args)) = args.split_first() else {
                return Err(it.type_error("run() missing 1 required positional argument"));
            };
            let ctx = slf.0;
            if ctx.borrow(it)?.entered {
                let r = it.repr_of(ctx.value())?;
                return Err(it.runtime_error(&format!("cannot enter context: {r} is already entered")));
            }
            let prev = it.context.replace(ctx.value().clone());
            ctx.with(it, |c| {
                c.entered = true;
                c.prev = prev;
            })?;
            let r = it.call(callable, args.to_vec(), kwargs.to_vec());
            let prev = ctx.with(it, |c| {
                c.entered = false;
                c.prev.take()
            })?;
            it.context = prev;
            r
        }

        /// Return a shallow copy of the context object.
        fn copy(&self, it: &mut Interp) -> Value {
            new_context(it, self.vars.clone()).into_value()
        }

        /// Return the value for `key` if `key` has the value in the context object.
        ///
        /// If `key` does not exist, return `default`. If `default` is not given,
        /// return None.
        fn get(slf: This<Py<Self>>, it: &mut Interp, key: &Value, default: Option<&Value>) -> R<Value> {
            let k = super::_contextvars::key(it, key)?;
            let vars = vars_of(it, &slf.0)?;
            Ok(lookup(&vars, k).unwrap_or_else(|| default.cloned().unwrap_or(Value::None)))
        }

        /// Return a list of all variables in the context object.
        fn keys(&self, it: &mut Interp) -> Value {
            let items = self.vars.iter().map(|(k, _)| k.clone()).collect();
            Py::new(it, super::Keys { items, pos: 0 }).into_value()
        }

        /// Return a list of all variables' values in the context object.
        fn values(&self, it: &mut Interp) -> Value {
            let items = self.vars.iter().map(|(_, v)| v.clone()).collect();
            Py::new(it, super::Values { items, pos: 0 }).into_value()
        }

        /// Return all variables and their values in the context object.
        ///
        /// The result is returned as a list of 2-tuples (variable, value).
        fn items(&self, it: &mut Interp) -> Value {
            let items = self.vars.iter().map(|(k, v)| Value::tuple(vec![k.clone(), v.clone()])).collect();
            Py::new(it, super::Items { items, pos: 0 }).into_value()
        }

        #[proto(len)]
        fn len(&self) -> usize {
            self.vars.len()
        }

        #[proto(getitem)]
        fn getitem(slf: This<Py<Self>>, it: &mut Interp, key: &Value) -> R<Value> {
            let k = super::_contextvars::key(it, key)?;
            let vars = vars_of(it, &slf.0)?;
            match lookup(&vars, k) {
                Some(v) => Ok(v),
                None => Err(it.new_exc_val("KeyError", key.clone())),
            }
        }

        #[proto(contains)]
        fn contains(slf: This<Py<Self>>, it: &mut Interp, key: &Value) -> R<bool> {
            let k = super::_contextvars::key(it, key)?;
            let vars = vars_of(it, &slf.0)?;
            Ok(lookup(&vars, k).is_some())
        }

        #[proto(iter)]
        fn iter(&self, it: &mut Interp) -> Value {
            self.keys(it)
        }

        #[proto(eq)]
        fn eq(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
            let Some(other) = Py::<Context>::from_value(it, other) else {
                return Ok(Value::NotImplemented);
            };
            let a = vars_of(it, &slf.0)?;
            let b = vars_of(it, &other)?;
            if a.len() != b.len() {
                return Ok(Value::Bool(false));
            }
            for (k, v) in a.iter() {
                let Some(w) = lookup(&b, k) else { return Ok(Value::Bool(false)) };
                if !it.values_eq(v, &w)? {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        }
    }

    #[methods]
    impl ContextVar {
        #[constructor]
        fn new(it: &mut Interp, #[kw] name: &Value, #[kwonly] default: Passed<&Value>) -> R<ContextVar> {
            if name.as_str().is_none() {
                return Err(it.type_error("context variable name must be a str"));
            }
            Ok(ContextVar { name: name.clone(), default: default.0.cloned() })
        }

        #[getter]
        fn name(&self) -> Value {
            self.name.clone()
        }

        /// Return a value for the context variable for the current context.
        ///
        /// If there is no value for the variable in the current context, the method will:
        ///  * return the value of the default argument of the method, if provided; or
        ///  * return the default value for the context variable, if it was created
        ///    with one; or
        ///  * raise a LookupError.
        fn get(slf: This<Py<Self>>, it: &mut Interp, default: Passed<&Value>) -> R<Value> {
            let cur = current(it);
            let vars = vars_of(it, &cur)?;
            if let Some(v) = lookup(&vars, slf.0.value()) {
                return Ok(v);
            }
            if let Some(d) = default.0 {
                return Ok(d.clone());
            }
            if let Some(d) = slf.0.borrow(it)?.default.clone() {
                return Ok(d);
            }
            Err(it.new_exc_val("LookupError", slf.0.into_value()))
        }

        /// Call to set a new value for the context variable in the current context.
        ///
        /// The required value argument is the new value for the context variable.
        ///
        /// Returns a Token object that can be used to restore the variable to its previous
        /// value via the `ContextVar.reset()` method.
        fn set(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<Value> {
            let cur = current(it);
            let var = slf.0.value();
            let old = cur.with(it, |c| {
                let old = lookup(&c.vars, var);
                c.vars = with_set(&c.vars, var, Some(value.clone()));
                old
            })?;
            let tok = Token { ctx: cur.into_value(), var: var.clone(), old, used: false };
            Ok(Py::new(it, tok).into_value())
        }

        /// Reset the context variable.
        ///
        /// The variable is reset to the value it had before the `ContextVar.set()` that
        /// created the token was used.
        fn reset(slf: This<Py<Self>>, it: &mut Interp, token: &Value) -> R<()> {
            let Some(tok) = Py::<Token>::from_value(it, token) else {
                let r = it.repr_of(token)?;
                return Err(it.type_error(&format!("expected an instance of Token, got {r}")));
            };
            let (used, var, ctx, old) = {
                let t = tok.borrow(it)?;
                (t.used, t.var.clone(), t.ctx.clone(), t.old.clone())
            };
            if used {
                let r = it.repr_of(token)?;
                return Err(it.runtime_error(&format!("{r} has already been used once")));
            }
            if !same(&var, slf.0.value()) {
                let r = it.repr_of(token)?;
                return Err(it.value_error(&format!("{r} was created by a different ContextVar")));
            }
            let cur = current(it);
            if !same(&ctx, cur.value()) {
                let r = it.repr_of(token)?;
                return Err(it.value_error(&format!("{r} was created in a different Context")));
            }
            tok.with(it, |t| t.used = true)?;
            cur.with(it, |c| c.vars = with_set(&c.vars, &var, old))?;
            Ok(())
        }

        #[proto(repr)]
        fn repr(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
            let (name, default) = {
                let v = slf.0.borrow(it)?;
                (v.name.clone(), v.default.clone())
            };
            let mut s = format!("<ContextVar name={}", it.repr_of(&name)?);
            if let Some(d) = default {
                s.push_str(&format!(" default={}", it.repr_of(&d)?));
            }
            s.push_str(&format!(" at {:#x}>", it.id_of(slf.0.value())));
            Ok(s)
        }
    }

    #[methods]
    impl Token {
        #[constructor]
        fn new(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Token> {
            let _ = (args, kwargs);
            Err(it.runtime_error("Tokens can only be created by ContextVars"))
        }

        #[getter]
        fn var(&self) -> Value {
            self.var.clone()
        }

        #[getter]
        fn old_value(&self, it: &mut Interp) -> R<Value> {
            match &self.old {
                Some(v) => Ok(v.clone()),
                None => token_missing(it),
            }
        }

        #[proto(repr)]
        fn repr(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
            let (used, var) = {
                let t = slf.0.borrow(it)?;
                (t.used, t.var.clone())
            };
            let var = it.repr_of(&var)?;
            let used = if used { " used" } else { "" };
            Ok(format!("<Token{used} var={var} at {:#x}>", it.id_of(slf.0.value())))
        }
    }

    fn token_missing(it: &mut Interp) -> R<Value> {
        let t = type_object::<Token>(it);
        let d = t.dict.borrow().clone();
        Ok(d.and_then(|d| crate::vm::dict_get_str(&d, "MISSING")).unwrap_or(Value::None))
    }

    #[init]
    fn init(it: &mut Interp, _m: &Value) {
        let missing = Py::new(it, super::Missing).into_value();
        let t = type_object::<Token>(it);
        let d = t.dict.borrow().clone();
        if let Some(d) = d {
            dict_set_str(&d, "MISSING", missing);
        }
    }
}

/// The variables of a context; its own iterator.
#[lumen_bind::class(name = "keys", hint(py(final)))]
pub struct Keys {
    items: Vec<Value>,
    pos: usize,
}

#[lumen_bind::methods]
impl Keys {
    #[proto(len)]
    fn len(&self) -> usize {
        self.items.len()
    }

    #[proto(iter)]
    fn iter(slf: This<Value>) -> Value {
        slf.0
    }

    #[proto(next)]
    fn next(&mut self) -> Option<Value> {
        let v = self.items.get(self.pos).cloned();
        self.pos += 1;
        v
    }
}

/// The values of a context; its own iterator.
#[lumen_bind::class(name = "values", hint(py(final)))]
pub struct Values {
    items: Vec<Value>,
    pos: usize,
}

#[lumen_bind::methods]
impl Values {
    #[proto(len)]
    fn len(&self) -> usize {
        self.items.len()
    }

    #[proto(iter)]
    fn iter(slf: This<Value>) -> Value {
        slf.0
    }

    #[proto(next)]
    fn next(&mut self) -> Option<Value> {
        let v = self.items.get(self.pos).cloned();
        self.pos += 1;
        v
    }
}

// The `(variable, value)` pairs of a context; its own iterator.
#[lumen_bind::class(name = "items", hint(py(final)))]
pub struct Items {
    items: Vec<Value>,
    pos: usize,
}

#[lumen_bind::methods]
impl Items {
    #[proto(len)]
    fn len(&self) -> usize {
        self.items.len()
    }

    #[proto(iter)]
    fn iter(slf: This<Value>) -> Value {
        slf.0
    }

    #[proto(next)]
    fn next(&mut self) -> Option<Value> {
        let v = self.items.get(self.pos).cloned();
        self.pos += 1;
        v
    }
}

// The `Token.MISSING` marker.
#[lumen_bind::class(name = "Token.MISSING", hint(py(final)))]
pub struct Missing;

#[lumen_bind::methods]
impl Missing {
    #[proto(repr)]
    fn repr(&self) -> &'static str {
        "<Token.MISSING>"
    }
}
