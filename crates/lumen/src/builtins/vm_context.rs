//! `vm` contexts: a realm of its own (fresh intrinsics) whose global object is fronted by a
//! *global proxy* that intercepts onto a host-supplied sandbox object, like V8's named and
//! indexed interceptors in Node's `node_contextify`. Reads try the sandbox (and its prototype
//! chain) first and fall back to the realm's global; writes and declarations land on the
//! sandbox. The proxy is an engine Proxy whose handler holds native traps; the trap results are
//! not held to the Proxy invariants (an interceptor reports the sandbox's own descriptors).

use super::reflect::{
    reflect_define, reflect_delete, reflect_get, reflect_gopd, reflect_has, reflect_own_keys,
    reflect_set,
};
use super::*;

const SANDBOX_KEY: &str = "#\u{0}vm.sandbox";
const PROXY_KEY: &str = "#\u{0}vm.proxy";
const DECORATED_KEY: &str = "#\u{0}vm.decorated";

fn internal(h: &Value, key: &str) -> Value {
    match h {
        Value::Obj(o) => o
            .borrow()
            .props
            .get(key)
            .map(|p| p.value())
            .unwrap_or(Value::Undefined),
        _ => Value::Undefined,
    }
}

/// Whether `handler` is a `vm` context's interceptor handler.
pub(crate) fn is_vm_handler(handler: &Value) -> bool {
    matches!(handler, Value::Obj(o) if o.borrow().props.contains(SANDBOX_KEY))
}

/// How V8 shows a property key in a message: `Symbol(desc)` for a symbol.
pub(crate) fn key_display(i: &Interp, key: &str) -> String {
    match i.sym_from_key(key) {
        Some(Value::Sym(s)) => format!("Symbol({})", s.description.as_deref().unwrap_or("")),
        _ => key.to_string(),
    }
}

fn same_obj(a: &Value, b: &Value) -> bool {
    matches!((a, b), (Value::Obj(x), Value::Obj(y)) if Gc::ptr_eq(x, y))
}

fn get(i: &mut Interp, o: &Value, key: &Value, recv: &Value) -> Result<Value, Value> {
    reflect_get(i, Value::Undefined, &[o.clone(), key.clone(), recv.clone()])
}

fn has(i: &mut Interp, o: &Value, key: &Value) -> Result<bool, Value> {
    Ok(matches!(
        reflect_has(i, Value::Undefined, &[o.clone(), key.clone()])?,
        Value::Bool(true)
    ))
}

fn set(i: &mut Interp, o: &Value, key: &Value, v: &Value, recv: &Value) -> Result<bool, Value> {
    Ok(matches!(
        reflect_set(i, Value::Undefined, &[o.clone(), key.clone(), v.clone(), recv.clone()])?,
        Value::Bool(true)
    ))
}

fn gopd(i: &mut Interp, o: &Value, key: &Value) -> Result<Value, Value> {
    reflect_gopd(i, Value::Undefined, &[o.clone(), key.clone()])
}

fn define(i: &mut Interp, o: &Value, key: &Value, desc: &Value) -> Result<bool, Value> {
    Ok(matches!(
        reflect_define(i, Value::Undefined, &[o.clone(), key.clone(), desc.clone()])?,
        Value::Bool(true)
    ))
}

fn delete(i: &mut Interp, o: &Value, key: &Value) -> Result<bool, Value> {
    Ok(matches!(
        reflect_delete(i, Value::Undefined, &[o.clone(), key.clone()])?,
        Value::Bool(true)
    ))
}

/// The descriptor of `key` on `o` or the nearest prototype that has it (V8's
/// `GetRealNamedPropertyAttributes`).
fn find_desc(i: &mut Interp, o: &Value, key: &Value) -> Result<Option<PartialDesc>, Value> {
    let mut cur = o.clone();
    for _ in 0..10_000 {
        if !matches!(cur, Value::Obj(_)) {
            return Ok(None);
        }
        let d = gopd(i, &cur, key)?;
        if !matches!(d, Value::Undefined) {
            return Ok(Some(ab(build_partial(i, &d))?));
        }
        cur = js_get_prototype_of(i, &cur)?;
    }
    Ok(None)
}

fn read_only(d: &PartialDesc) -> bool {
    !d.is_accessor() && d.writable == Some(false)
}

/// IsCompatiblePropertyDescriptor(extensible = true, desc, current).
fn compatible(desc: &PartialDesc, current: &PartialDesc) -> bool {
    if current.configurable != Some(false) {
        return true;
    }
    if desc.configurable == Some(true) {
        return false;
    }
    if desc.enumerable.is_some() && desc.enumerable != current.enumerable {
        return false;
    }
    if current.is_accessor() {
        if desc.is_data() {
            return false;
        }
        let same = |a: &Option<Value>, b: &Option<Value>| match (a, b) {
            (Some(x), Some(y)) => same_value(x, y),
            (Some(x), None) | (None, Some(x)) => matches!(x, Value::Undefined),
            (None, None) => true,
        };
        return (desc.get.is_none() || same(&desc.get, &current.get))
            && (desc.set.is_none() || same(&desc.set, &current.set));
    }
    if desc.is_accessor() {
        return false;
    }
    if current.writable == Some(false) {
        if desc.writable == Some(true) {
            return false;
        }
        if let (Some(v), Some(c)) = (&desc.value, &current.value) {
            return same_value(v, c);
        }
    }
    true
}

fn desc_object(i: &mut Interp, d: &PartialDesc) -> Value {
    let o = i.new_object();
    if let Some(v) = &d.value {
        set_data(&o, "value", v.clone());
    }
    if let Some(w) = d.writable {
        set_data(&o, "writable", Value::Bool(w));
    }
    if let Some(g) = &d.get {
        set_data(&o, "get", g.clone());
    }
    if let Some(s) = &d.set {
        set_data(&o, "set", s.clone());
    }
    if let Some(e) = d.enumerable {
        set_data(&o, "enumerable", Value::Bool(e));
    }
    if let Some(c) = d.configurable {
        set_data(&o, "configurable", Value::Bool(c));
    }
    Value::Obj(o)
}

fn trap_get(i: &mut Interp, h: Value, a: &[Value]) -> Result<Value, Value> {
    let (target, key, recv) = (arg(a, 0), arg(a, 1), arg(a, 2));
    let sandbox = internal(&h, SANDBOX_KEY);
    if has(i, &sandbox, &key)? {
        let v = get(i, &sandbox, &key, &sandbox)?;
        if same_obj(&v, &sandbox) {
            return Ok(internal(&h, PROXY_KEY));
        }
        return Ok(v);
    }
    get(i, &target, &key, &recv)
}

fn trap_has(i: &mut Interp, h: Value, a: &[Value]) -> Result<Value, Value> {
    let (target, key) = (arg(a, 0), arg(a, 1));
    let sandbox = internal(&h, SANDBOX_KEY);
    Ok(Value::Bool(has(i, &sandbox, &key)? || has(i, &target, &key)?))
}

fn trap_set(i: &mut Interp, h: Value, a: &[Value]) -> Result<Value, Value> {
    let (target, key, value) = (arg(a, 0), arg(a, 1), arg(a, 2));
    let sandbox = internal(&h, SANDBOX_KEY);
    let on_global = find_desc(i, &target, &key)?;
    let on_sandbox = find_desc(i, &sandbox, &key)?;
    if on_global.as_ref().is_some_and(read_only) || on_sandbox.as_ref().is_some_and(read_only) {
        return Ok(Value::Bool(false));
    }
    if on_global.is_none() && on_sandbox.is_none() && matches!(key, Value::Sym(_)) {
        return Ok(Value::Bool(set(i, &target, &key, &value, &target)?));
    }
    if set(i, &sandbox, &key, &value, &sandbox)? {
        return Ok(Value::Bool(true));
    }
    Ok(Value::Bool(set(i, &target, &key, &value, &target)?))
}

fn trap_gopd(i: &mut Interp, h: Value, a: &[Value]) -> Result<Value, Value> {
    let (target, key) = (arg(a, 0), arg(a, 1));
    let sandbox = internal(&h, SANDBOX_KEY);
    let d = gopd(i, &sandbox, &key)?;
    if !matches!(d, Value::Undefined) {
        return Ok(d);
    }
    gopd(i, &target, &key)
}

fn trap_define(i: &mut Interp, h: Value, a: &[Value]) -> Result<Value, Value> {
    let (target, key, desc) = (arg(a, 0), arg(a, 1), arg(a, 2));
    let sandbox = internal(&h, SANDBOX_KEY);
    let d = ab(build_partial(i, &desc))?;
    // A read-only, non-deletable global property is left to the global's own validation.
    if let Some(g) = find_desc(i, &target, &key)? {
        if read_only(&g) && g.configurable == Some(false) {
            return Ok(Value::Bool(define(i, &target, &key, &desc)?));
        }
    }
    let mut for_sandbox = PartialDesc {
        enumerable: d.enumerable,
        configurable: d.configurable,
        ..Default::default()
    };
    if d.is_accessor() {
        for_sandbox.get = d.get.clone();
        for_sandbox.set = d.set.clone();
    } else {
        for_sandbox.value = Some(d.value.clone().unwrap_or(Value::Undefined));
        for_sandbox.writable = d.writable;
    }
    let sd = desc_object(i, &for_sandbox);
    define(i, &sandbox, &key, &sd)?;
    // The definition then applies to the global, validated against the property the context
    // reports (the sandbox's own first).
    let mut current = gopd(i, &sandbox, &key)?;
    if matches!(current, Value::Undefined) {
        current = gopd(i, &target, &key)?;
    }
    if !matches!(current, Value::Undefined) {
        let cur = ab(build_partial(i, &current))?;
        if !compatible(&d, &cur) {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(define(i, &target, &key, &desc)?))
}

fn trap_delete(i: &mut Interp, h: Value, a: &[Value]) -> Result<Value, Value> {
    let (target, key) = (arg(a, 0), arg(a, 1));
    let sandbox = internal(&h, SANDBOX_KEY);
    if !delete(i, &sandbox, &key)? {
        return Ok(Value::Bool(false));
    }
    Ok(Value::Bool(delete(i, &target, &key)?))
}

fn trap_own_keys(i: &mut Interp, h: Value, a: &[Value]) -> Result<Value, Value> {
    let target = arg(a, 0);
    let sandbox = internal(&h, SANDBOX_KEY);
    let list = reflect_own_keys(i, Value::Undefined, &[sandbox])?;
    let mut keys = create_list_from_array_like(i, &list)?;
    let list = reflect_own_keys(i, Value::Undefined, &[target])?;
    let own = create_list_from_array_like(i, &list)?;
    for k in own {
        if !keys.iter().any(|x| same_value(x, &k)) {
            keys.push(k);
        }
    }
    Ok(i.make_array(keys))
}

impl Interp {
    /// Create a `vm` context over `sandbox`: a new realm whose global proxy (returned) intercepts
    /// onto it. The realm is dropped by the collector once the proxy and everything of the realm
    /// are unreachable.
    pub fn vm_create_context(&mut self, sandbox: &Value) -> Result<Value, Value> {
        if !matches!(sandbox, Value::Obj(_)) {
            return Err(self.make_error("TypeError", "vm context sandbox must be an object"));
        }
        let global = self.create_realm();
        let Value::Obj(g) = &global else {
            unreachable!()
        };
        let gptr = Gc::as_ptr(g) as usize;
        let saved = self.snapshot_realm();
        let realm = self.realms[&gptr].snapshot_clone();
        self.restore_realm(&realm);
        // The test262 host hook is not part of a context's global.
        if g.borrow_mut().props.remove("$262") {
            self.htmldda.pop();
        }
        let handler = Object::new(None);
        self.def_method(&handler, "get", 3, trap_get);
        self.def_method(&handler, "set", 4, trap_set);
        self.def_method(&handler, "has", 2, trap_has);
        self.def_method(&handler, "getOwnPropertyDescriptor", 2, trap_gopd);
        self.def_method(&handler, "defineProperty", 3, trap_define);
        self.def_method(&handler, "deleteProperty", 2, trap_delete);
        self.def_method(&handler, "ownKeys", 1, trap_own_keys);
        set_internal(&handler, SANDBOX_KEY, sandbox.clone());
        let proxy = match super::proxy::make_proxy(self, global.clone(), Value::Obj(handler.clone())) {
            Ok(p) => p,
            Err(e) => {
                self.restore_realm(&saved);
                return Err(e);
            }
        };
        set_internal(&handler, PROXY_KEY, proxy.clone());
        set_builtin(g, "globalThis", proxy.clone());
        if let Some(b) = self.global_env.borrow_mut().vars.get_mut("this") {
            b.value = proxy.clone();
        }
        let Value::Obj(p) = &proxy else { unreachable!() };
        self.global_proxy = Some(p.clone());
        let mut state = self.snapshot_realm();
        state.collectable = true;
        self.realms.insert(gptr, state);
        self.restore_realm(&saved);
        Ok(proxy)
    }

    /// The realm global behind a `vm` context's global proxy.
    pub(crate) fn vm_context_realm(&self, proxy: &Value) -> Option<usize> {
        let Value::Obj(p) = proxy else { return None };
        let (target, handler) = self.proxies.get(&(Gc::as_ptr(p) as usize))?;
        if !is_vm_handler(handler) {
            return None;
        }
        let Value::Obj(g) = target else { return None };
        let gptr = Gc::as_ptr(g) as usize;
        self.realms.contains_key(&gptr).then_some(gptr)
    }

    /// Whether `v` is a `vm` context's global proxy.
    pub fn vm_is_context(&self, v: &Value) -> bool {
        self.vm_context_realm(v).is_some()
    }

    /// The sandbox a `vm` context's global proxy intercepts onto.
    pub fn vm_context_sandbox(&self, proxy: &Value) -> Option<Value> {
        let Value::Obj(p) = proxy else { return None };
        let (_, handler) = self.proxies.get(&(Gc::as_ptr(p) as usize))?;
        is_vm_handler(handler).then(|| internal(handler, SANDBOX_KEY))
    }

    /// Run `f` with the realm of `context` (a global proxy; `None`: the current realm) active.
    pub(crate) fn in_vm_realm<T>(
        &mut self,
        context: Option<&Value>,
        f: impl FnOnce(&mut Interp) -> Result<T, Value>,
    ) -> Result<T, Value> {
        let Some(ctx) = context else { return f(self) };
        let Some(gptr) = self.vm_context_realm(ctx) else {
            return Err(self.make_error("TypeError", "not a vm context"));
        };
        let saved = self.snapshot_realm();
        let realm = self.realms[&gptr].snapshot_clone();
        self.restore_realm(&realm);
        let r = f(self);
        self.restore_realm(&saved);
        r
    }

    /// The active `vm` context's global proxy, as a value (with its key) for the generic MOP.
    fn vm_global_value(&self) -> Value {
        Value::Obj(self.name_global().clone())
    }

    fn vm_key(&self, name: &str) -> Value {
        self.sym_from_key(name)
            .unwrap_or_else(|| Value::from_string(name.to_string()))
    }

    /// [[GetOwnProperty]] of a global name through the active context's global proxy.
    pub(crate) fn vm_global_own(&mut self, name: &str) -> Result<Option<Property>, Abrupt> {
        let (g, k) = (self.vm_global_value(), self.vm_key(name));
        let d = gopd(self, &g, &k).map_err(Abrupt::Throw)?;
        if matches!(d, Value::Undefined) {
            return Ok(None);
        }
        Ok(Some(complete_descriptor(build_partial(self, &d)?)))
    }

    /// DefinePropertyOrThrow of a global name through the active context's global proxy.
    pub(crate) fn vm_global_define(
        &mut self,
        name: &str,
        value: Option<Value>,
        attrs: Option<(bool, bool, bool)>,
    ) -> Result<(), Abrupt> {
        let (g, k) = (self.vm_global_value(), self.vm_key(name));
        let mut d = PartialDesc {
            value,
            ..Default::default()
        };
        if let Some((w, e, c)) = attrs {
            d.writable = Some(w);
            d.enumerable = Some(e);
            d.configurable = Some(c);
        }
        let desc = desc_object(self, &d);
        if !define(self, &g, &k, &desc).map_err(Abrupt::Throw)? {
            return Err(self.throw("TypeError", format!("Cannot redefine property: {name}")));
        }
        Ok(())
    }

    /// Whether the active context's global object is extensible.
    pub(crate) fn vm_global_extensible(&mut self) -> Result<bool, Abrupt> {
        let g = self.vm_global_value();
        js_is_extensible(self, &g).map_err(Abrupt::Throw)
    }

    /// [[Delete]] of a global name through the active context's global proxy.
    pub(crate) fn vm_global_delete(&mut self, name: &str) -> Result<bool, Abrupt> {
        let (g, k) = (self.vm_global_value(), self.vm_key(name));
        delete(self, &g, &k).map_err(Abrupt::Throw)
    }

    /// The V8 message for a failed strict-mode store to `key` through a context's global proxy
    /// (`None` when `base` is not one).
    pub(crate) fn vm_store_error(&self, base: &Value, key: &str) -> Option<String> {
        let gptr = self.vm_context_realm(base)?;
        let g = &self.realms[&gptr].global;
        let read_only = g
            .borrow()
            .props
            .get(key)
            .is_some_and(|p| !p.accessor() && !p.writable());
        let shown = key_display(self, key);
        Some(if read_only {
            format!("Cannot assign to read only property '{shown}' of object '#<Object>'")
        } else {
            format!("Cannot redefine property: {shown}")
        })
    }
}

fn find_fn_expr(e: &crate::ast::Expr) -> Option<Rc<crate::ast::Function>> {
    match e {
        crate::ast::Expr::Func(f) => Some(f.clone()),
        crate::ast::Expr::Paren(inner) => find_fn_expr(inner),
        _ => None,
    }
}

impl Interp {
    /// Run `src` as a script — GlobalDeclarationInstantiation and all — in the realm of
    /// `context` (a `vm` global proxy; `None`: the current realm). Its frames print as
    /// `filename`, lines and first-line columns shifted by the offsets.
    pub fn vm_run_script(
        &mut self,
        context: Option<&Value>,
        src: &str,
        filename: &str,
        line_offset: i32,
        column_offset: i32,
        display_errors: bool,
    ) -> Result<Value, Value> {
        let body = crate::parser::parse_script(src, false)
            .map_err(|e| self.vm_syntax_error(e.message, src, filename, line_offset))?;
        let source = self.adopt_parsed_source(Some(filename), 0);
        if let Some(s) = &source {
            self.set_source_offsets(s, line_offset, column_offset);
        }
        let directive_strict = matches!(
            body.first(),
            Some(crate::ast::Stmt::Expr(crate::ast::Expr::Str(s))) if &**s == "use strict"
        );
        self.in_vm_realm(context, |i| {
            let saved = std::mem::replace(&mut i.strict, directive_strict);
            let r = i.with_script_frame(source, false, |i| i.run_program(&body));
            i.strict = saved;
            match r {
                Ok(Value::Empty) => Ok(Value::Undefined),
                other => other,
            }
        })
        .map_err(|e| {
            if display_errors {
                self.vm_decorate_runtime_error(&e, src, filename, line_offset, column_offset);
            }
            e
        })
    }

    /// Parse `src` as a script, throwing the decorated SyntaxError `new vm.Script` throws.
    pub fn vm_compile_script(
        &mut self,
        src: &str,
        filename: &str,
        line_offset: i32,
    ) -> Result<(), Value> {
        crate::parser::parse_script(src, false)
            .map(drop)
            .map_err(|e| self.vm_syntax_error(e.message, src, filename, line_offset))
    }

    /// A SyntaxError whose stack is prefixed by V8's source arrow for the last parse error.
    fn vm_syntax_error(&mut self, message: String, src: &str, filename: &str, line_offset: i32) -> Value {
        let err = self.make_error("SyntaxError", message);
        let (start, end) = crate::parser::last_error_span();
        let arrow = error_arrow(src, filename, line_offset, start as usize, end as usize);
        self.vm_decorate(&err, &arrow);
        err
    }

    /// Prefix a native error's stack with `arrow` once (Node's DecorateErrorStack).
    fn vm_decorate(&mut self, err: &Value, arrow: &str) {
        let Value::Obj(o) = err else { return };
        if !matches!(o.borrow().exotic, Exotic::Error) || o.borrow().props.contains(DECORATED_KEY) {
            return;
        }
        let Ok(Value::Str(stack)) = self.get_member(err, "stack") else {
            return;
        };
        let decorated = format!("{arrow}\n{stack}");
        if self.set_member(err, "stack", Value::from_string(decorated)).is_ok() {
            set_internal(o, DECORATED_KEY, Value::Bool(true));
        }
    }

    /// Decorate an error thrown while running a script with the source line of its innermost
    /// frame, when that frame is in the script.
    fn vm_decorate_runtime_error(
        &mut self,
        err: &Value,
        src: &str,
        filename: &str,
        line_offset: i32,
        column_offset: i32,
    ) {
        let Value::Obj(o) = err else { return };
        if !matches!(o.borrow().exotic, Exotic::Error) || o.borrow().props.contains(DECORATED_KEY) {
            return;
        }
        let Ok(Value::Str(stack)) = self.get_member(err, "stack") else {
            return;
        };
        let stack = stack.to_string();
        let Some((file, line, col)) = stack.lines().find_map(frame_location) else {
            return;
        };
        if file != filename {
            return;
        }
        let raw_line = line - line_offset;
        if raw_line < 1 {
            return;
        }
        let raw_col = if raw_line == 1 { col - column_offset } else { col };
        let Some(line_start) = line_start_offset(src, raw_line as usize) else {
            return;
        };
        let line_text = &src[line_start..];
        let line_len = line_text.find('\n').unwrap_or(line_text.len());
        let col_bytes = line_text[..line_len]
            .char_indices()
            .nth((raw_col.max(1) - 1) as usize)
            .map_or(line_len, |(b, _)| b);
        let start = line_start + col_bytes;
        let arrow = error_arrow(src, filename, line_offset, start, start + 1);
        self.vm_decorate(err, &arrow);
    }

    /// `vm.compileFunction`: a function with `params` whose body is `code`, closing over the
    /// global scope of `context`'s realm (or the current one) under `with`-like scopes for each
    /// of `extensions` (the last innermost).
    #[allow(clippy::too_many_arguments)]
    pub fn vm_compile_function(
        &mut self,
        context: Option<&Value>,
        code: &str,
        params: &[String],
        extensions: &[Value],
        filename: &str,
        line_offset: i32,
        column_offset: i32,
    ) -> Result<Value, Value> {
        let names: Vec<&str> = params.iter().map(String::as_str).collect();
        crate::parser::parse_cjs_function(code, &names, false)
            .map_err(|e| self.make_error("SyntaxError", e.message))?;
        let header = format!("(function ({}) {{\n", params.join(", "));
        let src = format!("{header}{code}\n}})");
        let program = crate::parser::parse_script(&src, false)
            .map_err(|e| self.make_error("SyntaxError", e.message))?;
        let source = crate::interpreter::stack_trace::take_parsed_source();
        let func = match program.first() {
            Some(crate::ast::Stmt::Expr(e)) => find_fn_expr(e),
            _ => None,
        }
        .ok_or_else(|| self.make_error("SyntaxError", "Unexpected token"))?;
        func.ensure_body()
            .map_err(|e| self.make_error("SyntaxError", e.message))?;
        if let Some((s, _)) = &source {
            let name = (!filename.is_empty()).then_some(filename);
            self.register_source(s, name, Some(header.len() as u32), 0, None);
            self.set_source_offsets(s, line_offset, column_offset);
        }
        let extensions = extensions.to_vec();
        self.in_vm_realm(context, move |i| {
            let mut env = i.global_env.clone();
            for ext in extensions {
                env = crate::interpreter::new_with_scope(env, ext);
            }
            Ok(i.make_function(func, env))
        })
    }
}

/// The byte offset where 1-based line `line` of `src` starts.
fn line_start_offset(src: &str, line: usize) -> Option<usize> {
    if line == 1 {
        return Some(0);
    }
    src.match_indices('\n').nth(line - 2).map(|(i, _)| i + 1)
}

/// `file:line:col` of a stack line (`    at f (file:1:2)` or `    at file:1:2`).
fn frame_location(l: &str) -> Option<(String, i32, i32)> {
    let rest = l.trim_start().strip_prefix("at ")?;
    let loc = match rest.strip_suffix(')') {
        Some(r) => &r[r.rfind('(')? + 1..],
        None => rest,
    };
    let (head, col) = loc.rsplit_once(':')?;
    let (file, line) = head.rsplit_once(':')?;
    Some((file.to_string(), line.parse().ok()?, col.parse().ok()?))
}

/// V8's source arrow for an error at bytes `start..end` of `src`: `file:line`, the source line
/// and a `^` underline (tabs kept so it lines up).
fn error_arrow(src: &str, filename: &str, line_offset: i32, start: usize, end: usize) -> String {
    let mut start = start.min(src.len());
    while !src.is_char_boundary(start) {
        start -= 1;
    }
    let line_start = src[..start].rfind('\n').map_or(0, |p| p + 1);
    let line_end = src[start..].find('\n').map_or(src.len(), |p| start + p);
    let line_no = src[..start].matches('\n').count() as i32 + 1 + line_offset;
    let line = src[line_start..line_end].trim_end_matches('\r');
    let mut under: String = src[line_start..start]
        .chars()
        .map(|c| if c == '\t' { '\t' } else { ' ' })
        .collect();
    let mut end = end.clamp(start, line_end);
    while !src.is_char_boundary(end) {
        end -= 1;
    }
    let n = src[start..end].chars().count().max(1);
    under.extend(std::iter::repeat_n('^', n));
    format!("{filename}:{line_no}\n{line}\n{under}\n")
}
