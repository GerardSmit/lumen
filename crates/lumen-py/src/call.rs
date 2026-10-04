//! Calls: argument binding, native/Python dispatch, generators and coroutines.

use crate::bytecode::*;
use crate::dict::PyDict;
use crate::object::*;
use crate::vm::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn join_names(names: &[String]) -> String {
    let q: Vec<String> = names.iter().map(|n| format!("'{}'", n)).collect();
    match q.len() {
        0 => String::new(),
        1 => q[0].clone(),
        2 => format!("{} and {}", q[0], q[1]),
        n => format!("{}, and {}", q[..n - 1].join(", "), q[n - 1]),
    }
}

impl Interp {
    pub fn call(&mut self, f: &Value, args: Vec<Value>, kw: Vec<(Obj, Value)>) -> R<Value> {
        let o = match f {
            Value::Obj(o) => o,
            _ => {
                let t = self.type_name_of(f);
                return Err(
                    self.new_exc_str("TypeError", &format!("'{}' object is not callable", t))
                );
            }
        };
        if lumen_common::stack::exhausted() {
            return Err(self.new_exc_str("RecursionError", "maximum recursion depth exceeded"));
        }
        match &o.kind {
            Kind::Function(_) => {
                let frame = self.bind_frame(o, args, kw)?;
                self.run_frame(frame)
            }
            Kind::Native(nd) => (nd.f)(self, &args, &kw),
            Kind::Method(func, this) => {
                let mut a = Vec::with_capacity(args.len() + 1);
                a.push(this.clone());
                a.extend(args);
                if let Value::Obj(fo) = func {
                    match &fo.kind {
                        Kind::Function(_) => {
                            let frame = self.bind_frame(fo, a, kw)?;
                            return self.run_frame(frame);
                        }
                        Kind::Native(nd) => return (nd.f)(self, &a, &kw),
                        _ => {}
                    }
                }
                self.call(&func.clone(), a, kw)
            }
            Kind::Type(_) => self.call_type(o, args, kw),
            Kind::StaticMethod(inner) => self.call(&inner.clone(), args, kw),
            _ => {
                let cls = self.type_of_obj(o);
                match self.lookup_mro(&cls, "__call__") {
                    Some(m) => {
                        let bound = self.bind_descr(&m, f, &cls)?;
                        self.call(&bound, args, kw)
                    }
                    None => {
                        let t = self.type_name_of(f);
                        Err(self
                            .new_exc_str("TypeError", &format!("'{}' object is not callable", t)))
                    }
                }
            }
        }
    }

    pub fn call_args(&mut self, f: &Value, args: &[Value]) -> R<Value> {
        self.call(f, args.to_vec(), Vec::new())
    }

    pub fn run_frame(&mut self, frame: Frame) -> R<Value> {
        if frame.code.flags & (CO_GENERATOR | CO_COROUTINE | CO_ASYNC_GENERATOR) != 0 {
            return Ok(self.make_generator(frame));
        }
        self.push_frame(frame)?;
        let entry = self.frames.len() - 1;
        self.run(entry, None)
    }

    pub fn call_inline(
        &mut self,
        f: &Value,
        mut args: Vec<Value>,
        kw: Vec<(Obj, Value)>,
    ) -> R<bool> {
        if let Value::Obj(o) = f {
            match &o.kind {
                Kind::Function(_) => {
                    let frame = self.bind_frame(o, args, kw)?;
                    return self.start_frame(frame);
                }
                Kind::Method(Value::Obj(fo), this) if matches!(fo.kind, Kind::Function(_)) => {
                    args.insert(0, this.clone());
                    let frame = self.bind_frame(fo, args, kw)?;
                    return self.start_frame(frame);
                }
                _ => {}
            }
        }
        let r = self.call(f, args, kw)?;
        self.frames.last_mut().unwrap().stack.push(r);
        Ok(false)
    }

    fn start_frame(&mut self, frame: Frame) -> R<bool> {
        if frame.code.flags & (CO_GENERATOR | CO_COROUTINE | CO_ASYNC_GENERATOR) != 0 {
            let g = self.make_generator(frame);
            self.frames.last_mut().unwrap().stack.push(g);
            return Ok(false);
        }
        self.push_frame(frame)?;
        Ok(true)
    }

    pub fn call_special(&mut self, obj: &Value, name: &str, args: Vec<Value>) -> R<Value> {
        let cls = self.type_of(obj);
        match self.lookup_mro(&cls, name) {
            Some(m) => {
                let bound = self.bind_descr(&m, obj, &cls)?;
                self.call(&bound, args, Vec::new())
            }
            None => {
                let t = self.type_name_of(obj);
                Err(self.new_exc_str(
                    "AttributeError",
                    &format!("'{}' object has no attribute '{}'", t, name),
                ))
            }
        }
    }

    pub fn call_method(&mut self, obj: &Value, name: &str, args: Vec<Value>) -> R<Value> {
        let f = self.get_attr_str(obj, name)?;
        self.call(&f, args, Vec::new())
    }

    pub fn bind_frame(
        &mut self,
        func_obj: &Obj,
        args: Vec<Value>,
        kw: Vec<(Obj, Value)>,
    ) -> R<Frame> {
        let func = match &func_obj.kind {
            Kind::Function(f) => f,
            _ => unreachable!(),
        };
        let code = func.code.clone();
        let mut frame = self.new_frame(
            code.clone(),
            func.globals.clone(),
            None,
            Some(func_obj.clone()),
            &func.closure,
        );
        let argcount = code.argcount as usize;
        let kwonly = code.kwonly as usize;
        let has_varargs = code.has(CO_VARARGS);
        let has_varkw = code.has(CO_VARKEYWORDS);
        let nargs = args.len();
        let mut extra: Vec<Value> = Vec::new();
        for (i, a) in args.into_iter().enumerate() {
            if i < argcount {
                frame.locals[i] = Some(a);
            } else {
                extra.push(a);
            }
        }
        if !extra.is_empty() && !has_varargs {
            let defaults_n = func.defaults.borrow().len();
            let name = func.qualname.borrow().to_string();
            let msg = if defaults_n > 0 {
                format!(
                    "{}() takes from {} to {} positional arguments but {} were given",
                    name,
                    argcount - defaults_n,
                    argcount,
                    nargs
                )
            } else {
                format!(
                    "{}() takes {} positional argument{} but {} {} given",
                    name,
                    argcount,
                    plural(argcount),
                    nargs,
                    if nargs == 1 { "was" } else { "were" }
                )
            };
            return Err(self.new_exc_str("TypeError", &msg));
        }
        let mut kwdict: Option<Obj> = None;
        if has_varargs {
            frame.locals[argcount + kwonly] = Some(Value::tuple(extra));
        }
        if has_varkw {
            let idx = argcount + kwonly + has_varargs as usize;
            let d = Object::new(Kind::Dict(RefCell::new(PyDict::new())));
            frame.locals[idx] = Some(Value::Obj(d.clone()));
            kwdict = Some(d);
        }
        let posonly = code.posonly as usize;
        for (k, v) in kw {
            let kname = k.as_str_kind().unwrap_or("");
            let mut slot = None;
            for i in posonly..(argcount + kwonly) {
                if &*code.varnames[i] == kname {
                    slot = Some(i);
                    break;
                }
            }
            match slot {
                Some(i) => {
                    if frame.locals[i].is_some() {
                        let name = func.qualname.borrow().to_string();
                        return Err(self.new_exc_str(
                            "TypeError",
                            &format!("{}() got multiple values for argument '{}'", name, kname),
                        ));
                    }
                    frame.locals[i] = Some(v);
                }
                None => match &kwdict {
                    Some(d) => {
                        self.dict_set(d, Value::Obj(k.clone()), v)?;
                    }
                    None => {
                        let name = func.qualname.borrow().to_string();
                        let in_posonly = code.varnames[..posonly].iter().any(|n| &**n == kname);
                        let msg = if in_posonly {
                            format!("{}() got some positional-only arguments passed as keyword arguments: '{}'", name, kname)
                        } else {
                            format!("{}() got an unexpected keyword argument '{}'", name, kname)
                        };
                        return Err(self.new_exc_str("TypeError", &msg));
                    }
                },
            }
        }
        let defaults = func.defaults.borrow();
        let first_default = argcount - defaults.len().min(argcount);
        let mut missing: Vec<String> = Vec::new();
        for i in 0..argcount {
            if frame.locals[i].is_none() {
                if i >= first_default {
                    frame.locals[i] = Some(defaults[i - first_default].clone());
                } else {
                    missing.push(code.varnames[i].to_string());
                }
            }
        }
        if !missing.is_empty() {
            let name = func.qualname.borrow().to_string();
            return Err(self.new_exc_str(
                "TypeError",
                &format!(
                    "{}() missing {} required positional argument{}: {}",
                    name,
                    missing.len(),
                    plural(missing.len()),
                    join_names(&missing)
                ),
            ));
        }
        if kwonly > 0 {
            let kwd = func.kwdefaults.borrow();
            let mut missing: Vec<String> = Vec::new();
            for i in argcount..argcount + kwonly {
                if frame.locals[i].is_none() {
                    let n = &code.varnames[i];
                    match kwd.iter().find(|(k, _)| k.as_str_kind() == Some(&**n)) {
                        Some((_, v)) => frame.locals[i] = Some(v.clone()),
                        None => missing.push(n.to_string()),
                    }
                }
            }
            if !missing.is_empty() {
                let name = func.qualname.borrow().to_string();
                return Err(self.new_exc_str(
                    "TypeError",
                    &format!(
                        "{}() missing {} required keyword-only argument{}: {}",
                        name,
                        missing.len(),
                        plural(missing.len()),
                        join_names(&missing)
                    ),
                ));
            }
        }
        Ok(frame)
    }

    pub fn make_function(
        &mut self,
        code: Value,
        closure: Option<Value>,
        ann: Option<Value>,
        kwd: Option<Value>,
        defaults: Option<Value>,
    ) -> R<Value> {
        let code = match &code {
            Value::Obj(o) => match &o.kind {
                Kind::Code(c) => c.clone(),
                _ => {
                    return Err(
                        self.new_exc_str("SystemError", "MakeFunction expects a code object")
                    )
                }
            },
            _ => return Err(self.new_exc_str("SystemError", "MakeFunction expects a code object")),
        };
        let globals = self.frames.last().unwrap().globals.clone();
        let closure: Vec<Obj> = match closure {
            Some(c) => c
                .tuple_items()
                .unwrap_or(&[])
                .iter()
                .filter_map(|v| v.as_obj().cloned())
                .collect(),
            None => Vec::new(),
        };
        let defaults: Vec<Value> = match defaults {
            Some(d) => d.tuple_items().unwrap_or(&[]).to_vec(),
            None => Vec::new(),
        };
        let mut kwdefaults = Vec::new();
        if let Some(Value::Obj(d)) = kwd {
            if let Kind::Dict(dd) = &d.kind {
                for e in dd.borrow().iter() {
                    if let Value::Obj(k) = &e.key {
                        kwdefaults.push((k.clone(), e.val.clone()));
                    }
                }
            }
        }
        let annotations = match ann {
            Some(Value::Obj(d)) => Some(d),
            _ => None,
        };
        let f = Function {
            name: RefCell::new(code.name.clone()),
            qualname: RefCell::new(code.qualname.clone()),
            code,
            globals,
            defaults: RefCell::new(defaults),
            kwdefaults: RefCell::new(kwdefaults),
            closure,
            annotations: RefCell::new(annotations),
            type_params: RefCell::new(None),
        };
        Ok(Value::Obj(Object::new(Kind::Function(Box::new(f)))))
    }

    pub fn kw_merge(&mut self, d: &Obj, src: &Value, f: &Value) -> R<()> {
        let fname = match self.get_attr_str(f, "__qualname__") {
            Ok(v) => v.as_str().unwrap_or("function").to_string(),
            Err(_) => "function".to_string(),
        };
        let is_dict = dict_of(src).is_some();
        if !is_dict && self.get_attr_str(src, "keys").is_err() {
            let t = self.type_name_of(src);
            return Err(self.type_error(&format!(
                "{}() argument after ** must be a mapping, not {}",
                fname, t
            )));
        }
        let pairs = self.dict_to_kwargs(src)?;
        for (k, v) in pairs {
            let kv = Value::Obj(k.clone());
            if self.dict_get(d, &kv)?.is_some() {
                let name = kv.as_str().unwrap_or("").to_string();
                return Err(self.type_error(&format!(
                    "{}() got multiple values for keyword argument '{}'",
                    fname, name
                )));
            }
            self.dict_set(d, kv, v)?;
        }
        Ok(())
    }

    pub fn dict_to_kwargs(&mut self, d: &Value) -> R<Vec<(Obj, Value)>> {
        let mut out = Vec::new();
        if let Some(dd) = dict_of(d) {
            for e in dd.borrow().iter() {
                match &e.key {
                    Value::Obj(k) if matches!(k.kind, Kind::Str(_)) => {
                        out.push((k.clone(), e.val.clone()))
                    }
                    _ => return Err(self.new_exc_str("TypeError", "keywords must be strings")),
                }
            }
            return Ok(out);
        }
        let kv = self.call_method(d, "keys", Vec::new())?;
        let keys = self.iterate_to_vec(&kv)?;
        for k in keys {
            let v = self.getitem(d, &k)?;
            match &k {
                Value::Obj(ko) if matches!(ko.kind, Kind::Str(_)) => out.push((ko.clone(), v)),
                _ => return Err(self.new_exc_str("TypeError", "keywords must be strings")),
            }
        }
        Ok(out)
    }

    /// Calls the class-body function with `ns` as its local namespace.
    pub fn call_body(&mut self, func: &Obj, ns: &Obj) -> R<Value> {
        let mut frame = self.bind_frame(func, Vec::new(), Vec::new())?;
        frame.names = Some(ns.clone());
        self.push_frame(frame)?;
        let entry = self.frames.len() - 1;
        self.run(entry, None)
    }

    // ---- generators ------------------------------------------------------------------------

    pub fn make_generator(&mut self, frame: Frame) -> Value {
        let kind = if frame.code.has(CO_ASYNC_GENERATOR) {
            GenKind::AsyncGen
        } else if frame.code.has(CO_COROUTINE) {
            GenKind::Coroutine
        } else {
            GenKind::Generator
        };
        let name = frame.code.name.clone();
        let qualname = frame.code.qualname.clone();
        Value::Obj(Object::new(Kind::Generator(Box::new(GenData {
            state: RefCell::new(GenState::Created(Box::new(frame))),
            kind,
            name: RefCell::new(name),
            qualname: RefCell::new(qualname),
            running_async: Cell::new(false),
            hooks_inited: Cell::new(false),
        }))))
    }

    pub fn gen_send(&mut self, g: &Obj, value: Value) -> R<GenResult> {
        let gd = match &g.kind {
            Kind::Generator(gd) => gd,
            _ => return Err(self.new_exc_str("TypeError", "not a generator")),
        };
        let state = std::mem::replace(&mut *gd.state.borrow_mut(), GenState::Running);
        let (frame, first) = match state {
            GenState::Created(f) => {
                if !value.is_none() {
                    *gd.state.borrow_mut() = GenState::Created(f);
                    return Err(self.new_exc_str(
                        "TypeError",
                        "can't send non-None value to a just-started generator",
                    ));
                }
                (*f, true)
            }
            GenState::Suspended(f) => (*f, false),
            GenState::Running => {
                let what = if gd.kind == GenKind::Coroutine {
                    "coroutine"
                } else {
                    "generator"
                };
                return Err(self.new_exc_str("ValueError", &format!("{} already executing", what)));
            }
            GenState::Done => {
                *gd.state.borrow_mut() = GenState::Done;
                if gd.kind == GenKind::Coroutine {
                    return Err(
                        self.new_exc_str("RuntimeError", "cannot reuse already awaited coroutine")
                    );
                }
                return Ok(GenResult::Return(Value::None));
            }
        };
        self.resume_frame(g, frame, if first { None } else { Some(value) }, None)
    }

    fn resume_frame(
        &mut self,
        g: &Obj,
        mut frame: Frame,
        send: Option<Value>,
        throw: Option<Obj>,
    ) -> R<GenResult> {
        let gd = match &g.kind {
            Kind::Generator(gd) => gd,
            _ => unreachable!(),
        };
        if let Some(v) = send {
            frame.stack.push(v);
        }
        if let Err(e) = self.push_frame(frame) {
            *gd.state.borrow_mut() = GenState::Done;
            return Err(e);
        }
        let entry = self.frames.len() - 1;
        self.yielded = None;
        let r = self.run(entry, throw);
        match r {
            Ok(v) => match self.yielded.take() {
                Some(f) => {
                    *gd.state.borrow_mut() = GenState::Suspended(Box::new(f));
                    Ok(GenResult::Yield(v))
                }
                None => {
                    *gd.state.borrow_mut() = GenState::Done;
                    Ok(GenResult::Return(v))
                }
            },
            Err(e) => {
                *gd.state.borrow_mut() = GenState::Done;
                let (stop, what) = match gd.kind {
                    GenKind::Generator => ("StopIteration", "generator"),
                    GenKind::Coroutine => ("StopIteration", "coroutine"),
                    _ => ("StopAsyncIteration", "async generator"),
                };
                if self.exc_is(&e, stop) {
                    let re = self.new_exc_str("RuntimeError", &format!("{} raised {}", what, stop));
                    if let Kind::Exception(d) = &re.kind {
                        let mut d = d.borrow_mut();
                        d.cause = Some(e.clone());
                        d.context = Some(e);
                        d.suppress_context = true;
                        d.ctx_set = true;
                    }
                    return Err(re);
                }
                Err(e)
            }
        }
    }

    pub fn gen_throw(&mut self, g: &Obj, exc: Obj) -> R<GenResult> {
        let gd = match &g.kind {
            Kind::Generator(gd) => gd,
            _ => return Err(self.new_exc_str("TypeError", "not a generator")),
        };
        let state = std::mem::replace(&mut *gd.state.borrow_mut(), GenState::Running);
        match state {
            GenState::Created(_) | GenState::Done => {
                *gd.state.borrow_mut() = GenState::Done;
                Err(exc)
            }
            GenState::Running => Err(self.new_exc_str("ValueError", "generator already executing")),
            GenState::Suspended(mut frame) => {
                let at_yield_from = matches!(frame.code.ops.get(frame.pc), Some(Op::YieldFrom));
                if at_yield_from {
                    let sub = frame.stack.last().cloned().unwrap_or(Value::None);
                    let is_close = self.exc_is(&exc, "GeneratorExit");
                    let r = self.throw_into_iter(&sub, exc.clone(), is_close);
                    match r {
                        Ok(GenResult::Yield(y)) => {
                            *gd.state.borrow_mut() = GenState::Suspended(frame);
                            return Ok(GenResult::Yield(y));
                        }
                        Ok(GenResult::Return(v)) => {
                            frame.stack.pop();
                            frame.stack.push(v);
                            frame.pc += 1;
                            return self.resume_frame(g, *frame, None, None);
                        }
                        Err(e) => return self.resume_frame(g, *frame, None, Some(e)),
                    }
                }
                self.resume_frame(g, *frame, None, Some(exc))
            }
        }
    }

    fn throw_into_iter(&mut self, it: &Value, exc: Obj, is_close: bool) -> R<GenResult> {
        if let Value::Obj(o) = it {
            if matches!(o.kind, Kind::Generator(_)) {
                return self.gen_throw(o, exc);
            }
        }
        let cls = self.type_of(it);
        if is_close {
            if let Some(m) = self.lookup_mro(&cls, "close") {
                let b = self.bind_descr(&m, it, &cls)?;
                self.call(&b, Vec::new(), Vec::new())?;
            }
            return Err(exc);
        }
        match self.lookup_mro(&cls, "throw") {
            Some(m) => {
                let b = self.bind_descr(&m, it, &cls)?;
                let ev = Value::Obj(exc);
                match self.call(&b, vec![ev], Vec::new()) {
                    Ok(v) => Ok(GenResult::Yield(v)),
                    Err(e) => {
                        if self.exc_is(&e, "StopIteration") {
                            Ok(GenResult::Return(self.stop_value(&e)))
                        } else {
                            Err(e)
                        }
                    }
                }
            }
            None => Err(exc),
        }
    }

    pub fn gen_close(&mut self, g: &Obj) -> R<()> {
        let gd = match &g.kind {
            Kind::Generator(gd) => gd,
            _ => return Ok(()),
        };
        let is_suspended = matches!(&*gd.state.borrow(), GenState::Suspended(_));
        if !is_suspended {
            let created = matches!(&*gd.state.borrow(), GenState::Created(_));
            if created {
                *gd.state.borrow_mut() = GenState::Done;
            }
            return Ok(());
        }
        let exc = self.new_exc_str("GeneratorExit", "");
        let exc = {
            if let Kind::Exception(d) = &exc.kind {
                d.borrow_mut().args = Value::tuple(Vec::new());
            }
            exc
        };
        match self.gen_throw(g, exc) {
            Ok(GenResult::Yield(_)) => Err(self.new_exc_str(
                "RuntimeError",
                if gd.kind == GenKind::Coroutine {
                    "coroutine ignored GeneratorExit"
                } else {
                    "generator ignored GeneratorExit"
                },
            )),
            Ok(GenResult::Return(_)) => Ok(()),
            Err(e) => {
                if self.exc_is(&e, "GeneratorExit") || self.exc_is(&e, "StopIteration") {
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }

    pub fn stop_value(&mut self, e: &Obj) -> Value {
        match &e.kind {
            Kind::Exception(d) => match d.borrow().args.tuple_items() {
                Some(t) if !t.is_empty() => t[0].clone(),
                _ => Value::None,
            },
            _ => Value::None,
        }
    }

    pub fn send_to_iter(&mut self, it: &Value, v: Value) -> R<GenResult> {
        if let Value::Obj(o) = it {
            if matches!(o.kind, Kind::Generator(_)) {
                return self.gen_send(o, v);
            }
        }
        let r = if v.is_none() {
            self.call_special(it, "__next__", Vec::new())
        } else {
            self.call_method(it, "send", vec![v])
        };
        match r {
            Ok(v) => Ok(GenResult::Yield(v)),
            Err(e) => {
                if self.exc_is(&e, "StopIteration") {
                    Ok(GenResult::Return(self.stop_value(&e)))
                } else {
                    Err(e)
                }
            }
        }
    }

    pub fn get_awaitable(&mut self, v: &Value) -> R<Value> {
        if let Value::Obj(o) = v {
            if let Kind::Generator(gd) = &o.kind {
                if gd.kind == GenKind::Coroutine {
                    return Ok(v.clone());
                }
            }
        }
        let cls = self.type_of(v);
        match self.lookup_mro(&cls, "__await__") {
            Some(m) => {
                let b = self.bind_descr(&m, v, &cls)?;
                let it = self.call(&b, Vec::new(), Vec::new())?;
                if let Value::Obj(o) = &it {
                    if let Kind::Generator(gd) = &o.kind {
                        if gd.kind == GenKind::Coroutine {
                            return Err(
                                self.new_exc_str("TypeError", "__await__() returned a coroutine")
                            );
                        }
                    }
                }
                Ok(it)
            }
            None => {
                let t = self.type_name_of(v);
                Err(self.new_exc_str("TypeError", &format!("'{}' object can't be awaited", t)))
            }
        }
    }

    pub fn lookup_context_methods(&mut self, mgr: &Value, is_async: bool) -> R<(Value, Value)> {
        let cls = self.type_of(mgr);
        let (en, ex) = if is_async {
            ("__aenter__", "__aexit__")
        } else {
            ("__enter__", "__exit__")
        };
        let enter = self.lookup_mro(&cls, en);
        let exit = self.lookup_mro(&cls, ex);
        match (enter, exit) {
            (Some(en), Some(ex)) => {
                let e = self.bind_descr(&en, mgr, &cls)?;
                let x = self.bind_descr(&ex, mgr, &cls)?;
                Ok((e, x))
            }
            _ => {
                let t = self.type_name_of(mgr);
                let proto = if is_async {
                    "asynchronous context manager protocol"
                } else {
                    "context manager protocol"
                };
                Err(self.new_exc_str(
                    "TypeError",
                    &format!("'{}' object does not support the {}", t, proto),
                ))
            }
        }
    }
}

pub fn rc_str(s: &str) -> Rc<str> {
    s.into()
}
