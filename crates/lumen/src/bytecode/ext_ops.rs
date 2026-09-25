//! The widened compiler subset: private names, general object literals (methods, accessors,
//! computed keys, spread, `__proto__`), class definitions, and `super` property reads/calls.
//! Every run-time helper here delegates to the tree-walker's own routine for the operation, in
//! the tree-walker's evaluation order — the compiler only reorders nothing and interleaves the
//! compiled sub-expressions exactly where the oracle evaluates them.
use super::{step_and_store, Bail, CResult, Compiler, Home, Op, UpdKind};
use crate::ast::{Class, Expr, PropDef, PropKey};
use crate::interpreter::{Abrupt, Env, Interp};
use crate::value::Value;
use std::rc::Rc;

// ---------------------------------------------------------------------------------------------
// Run time
// ---------------------------------------------------------------------------------------------

#[inline]
fn pop(stack: &mut Vec<Value>) -> Value {
    stack.pop().expect("vm stack underflow")
}

pub(super) fn get_private(
    i: &mut Interp,
    env: &Env,
    name: &str,
    stack: &mut Vec<Value>,
) -> Result<(), Abrupt> {
    let base = pop(stack);
    let k = i.resolve_private(name, env);
    let v = i.get_private_member(&base, &k)?;
    stack.push(v);
    Ok(())
}

/// PutValue on a private Reference (the oracle's `put_reference` for a resolved private key).
fn put_private(i: &mut Interp, base: &Value, k: &str, v: Value) -> Result<(), Abrupt> {
    if Interp::is_private_key(k) {
        i.set_private_member(base, k, v)
    } else {
        i.set_member(base, k, v)
    }
}

pub(super) fn set_private(
    i: &mut Interp,
    env: &Env,
    name: &str,
    stack: &mut Vec<Value>,
) -> Result<(), Abrupt> {
    let v = pop(stack);
    let base = pop(stack);
    let k = i.resolve_private(name, env);
    put_private(i, &base, &k, v.clone())?;
    stack.push(v);
    Ok(())
}

pub(super) fn get_private_method(
    i: &mut Interp,
    env: &Env,
    name: &str,
    stack: &mut Vec<Value>,
) -> Result<(), Abrupt> {
    let base = pop(stack);
    let k = i.resolve_private(name, env);
    let f = i.get_private_member(&base, &k)?;
    stack.push(base);
    stack.push(f);
    Ok(())
}

pub(super) fn private_in(
    i: &mut Interp,
    env: &Env,
    name: &str,
    stack: &mut Vec<Value>,
) -> Result<(), Abrupt> {
    let o = pop(stack);
    let k = i.resolve_private(name, env);
    match o {
        Value::Obj(obj) => {
            let has = obj.borrow().props.contains(k.as_str());
            stack.push(Value::Bool(has));
            Ok(())
        }
        _ => Err(i.throw("TypeError", "the right-hand side of 'in' must be an object")),
    }
}

pub(super) fn update_private(
    i: &mut Interp,
    env: &Env,
    name: &str,
    kind: UpdKind,
    stack: &mut Vec<Value>,
) -> Result<(), Abrupt> {
    let base = pop(stack);
    let k = i.resolve_private(name, env);
    let old = if Interp::is_private_key(&k) {
        i.get_private_member(&base, &k)?
    } else {
        i.get_member(&base, &k)?
    };
    step_and_store(i, stack, kind, old, |i, v| put_private(i, &base, &k, v))
}

fn literal_obj(stack: &[Value]) -> crate::value::Gc {
    match stack.last() {
        Some(Value::Obj(o)) => o.clone(),
        _ => unreachable!("object literal under construction"),
    }
}

pub(super) fn init_prop(
    i: &mut Interp,
    key: &str,
    named: bool,
    stack: &mut Vec<Value>,
) -> Result<(), Abrupt> {
    let v = pop(stack);
    if named {
        let name = i.fn_name_for_key(key);
        i.set_fn_name(&v, &name);
    }
    literal_obj(stack)
        .borrow_mut()
        .props
        .insert(key, crate::value::Property::plain(v));
    Ok(())
}

pub(super) fn init_prop_computed(
    i: &mut Interp,
    named: bool,
    stack: &mut Vec<Value>,
) -> Result<(), Abrupt> {
    let v = pop(stack);
    let key = pop(stack);
    // ToPropKey already ran any observable coercion; this is the side-effect-free remainder.
    let k = i.to_property_key(&key)?;
    stack.push(v);
    init_prop(i, &k, named, stack)
}

pub(super) fn init_method(
    i: &mut Interp,
    env: &Env,
    func: &Rc<crate::ast::Function>,
    key: Option<&str>,
    kind: u16,
    stack: &mut Vec<Value>,
) -> Result<(), Abrupt> {
    let k = match key {
        Some(k) => k.to_string(),
        None => {
            let key = pop(stack);
            i.to_property_key(&key)?
        }
    };
    let obj = literal_obj(stack);
    // The oracle's per-literal home scope; one per method is indistinguishable (it binds only
    // the literal as `%homeobject%`). A method that can't reach its home object (no `super`,
    // no direct `eval`) closes over `env` directly: no scope and no literal <-> method cycle.
    let fenv = if func.may_use_home() {
        let home_env = crate::interpreter::new_scope(Some(env.clone()));
        crate::eval::bind(&home_env, "%homeobject%", Value::Obj(obj.clone()));
        home_env
    } else {
        env.clone()
    };
    let name = i.fn_name_for_key(&k);
    let (f, named) = i.make_function_named(func.clone(), fenv, (kind == 0).then_some(&*name));
    match kind {
        0 => {
            if !named {
                i.set_fn_name(&f, &name);
            }
            obj.borrow_mut()
                .props
                .insert(k, crate::value::Property::plain(f));
        }
        1 => {
            i.set_fn_name(&f, &format!("get {name}"));
            i.define_accessor(&obj, &k, Some(f), None);
        }
        _ => {
            i.set_fn_name(&f, &format!("set {name}"));
            i.define_accessor(&obj, &k, None, Some(f));
        }
    }
    Ok(())
}

pub(super) fn copy_data_props(i: &mut Interp, stack: &mut Vec<Value>) -> Result<(), Abrupt> {
    let v = pop(stack);
    let obj = literal_obj(stack);
    i.copy_data_properties_into(&obj, &v, &[])
}

pub(super) fn set_proto_lit(stack: &mut Vec<Value>) {
    let v = pop(stack);
    let obj = literal_obj(stack);
    match v {
        Value::Obj(p) => obj.borrow_mut().proto = Some(p),
        Value::Null => obj.borrow_mut().proto = None,
        _ => {}
    }
}

pub(super) fn make_class(
    i: &mut Interp,
    env: &Env,
    class: &Rc<Class>,
    name: Option<&str>,
    stack: &mut Vec<Value>,
) -> Result<(), Abrupt> {
    // The oracle's NamedEvaluation for `x = class {}`: the pending name is consumed inside the
    // class evaluation (and, exactly like the oracle, left set when the evaluation throws).
    if let Some(n) = name {
        i.pending_fn_name = Some(n.to_string());
    }
    let v = i.eval_class(class, env)?;
    if let Some(n) = name {
        i.pending_fn_name = None;
        i.set_fn_name(&v, n);
    }
    stack.push(v);
    Ok(())
}

pub(super) fn super_get(
    i: &mut Interp,
    env: &Env,
    key: Option<&str>,
    stack: &mut Vec<Value>,
) -> Result<(), Abrupt> {
    // Oracle order: GetThisBinding (already pushed), the key expression (already pushed),
    // GetSuperBase, ToPropertyKey, the null-base check, then the receiver-aware get.
    let idx = match key {
        Some(_) => None,
        None => Some(pop(stack)),
    };
    let this = pop(stack);
    let home = i.super_base(env)?;
    let key = match (key, idx) {
        (Some(k), _) => k.to_string(),
        (None, Some(idx)) => i.to_property_key(&idx)?,
        _ => unreachable!(),
    };
    if matches!(home, Value::Undefined | Value::Null) {
        return Err(i.throw(
            "TypeError",
            format!("cannot read property '{key}' of a null super base"),
        ));
    }
    let v = crate::builtins::reflect_ordinary_get(i, &home, &key, &this).map_err(Abrupt::Throw)?;
    stack.push(v);
    Ok(())
}

pub(super) fn super_base(i: &mut Interp, env: &Env, stack: &mut Vec<Value>) -> Result<(), Abrupt> {
    let home = i.super_base(env)?;
    stack.push(home);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn super_method(
    i: &mut Interp,
    env: &Env,
    this_val: &Value,
    key: Option<&str>,
    lexical: bool,
    stack: &mut Vec<Value>,
) -> Result<(), Abrupt> {
    let idx = match key {
        Some(_) => None,
        None => Some(pop(stack)),
    };
    let home = pop(stack);
    let key = match (key, idx) {
        (Some(k), _) => k.to_string(),
        (None, Some(idx)) => i.to_property_key(&idx)?,
        _ => unreachable!(),
    };
    let f = i.get_member(&home, &key)?;
    let this = if lexical {
        i.lexical_this(env)?
    } else {
        this_val.clone()
    };
    stack.push(this);
    stack.push(f);
    Ok(())
}

/// Append `items` to the array literal on top of the stack (an elision: `hole`, no items),
/// keeping its `length` as the next index — exactly the oracle's `eval_array` stores.
pub(super) fn array_append(
    stack: &mut [Value],
    items: impl IntoIterator<Item = Value>,
    hole: bool,
) {
    let Some(Value::Obj(ao)) = stack.last() else {
        unreachable!("array literal under construction")
    };
    append_to(ao, items, hole);
}

/// Append one value to the array literal `ao` (see [`append_to`]): the common single-element
/// case appends at the packed frontier with one `length` lookup.
#[inline]
pub(super) fn append_one(ao: &crate::value::Gc, v: Value) {
    let r = ao.borrow_mut().props.push_array_element(v);
    if let Err(v) = r {
        append_to(ao, std::iter::once(v), false);
    }
}

/// [`array_append`] on the array literal `ao` itself (the compiled code's `ArrayAppend`
/// helper appends through here without an operand stack).
pub(super) fn append_to(ao: &crate::value::Gc, items: impl IntoIterator<Item = Value>, hole: bool) {
    let mut o = ao.borrow_mut();
    let mut idx = match o.props.get("length").map(|p| p.value()) {
        Some(Value::Num(n)) => n as usize,
        _ => 0,
    };
    // A run appended at the packed frontier lands in one reservation.
    let items = items.into_iter();
    let items = match u32::try_from(idx) {
        Ok(n) => match o.props.try_extend_elements(n, items) {
            Ok(added) => {
                idx += added;
                None
            }
            Err(items) => Some(items),
        },
        Err(_) => Some(items),
    };
    for v in items.into_iter().flatten() {
        // The literal under construction is an ordinary Array no code has seen: its elements
        // are exactly the dense run appended so far (plus holes), so the next index is the
        // dense frontier (or a bounded hole run past it) — no decimal key, no hashing.
        let prop = crate::value::Property::plain(v);
        let prop = match u32::try_from(idx) {
            Ok(n) => match o.props.try_append_element(n, prop) {
                Ok(()) => {
                    idx += 1;
                    continue;
                }
                Err(p) => match o.props.try_define_dense_element(n, p) {
                    Ok(()) => {
                        idx += 1;
                        continue;
                    }
                    Err(p) => p,
                },
            },
            Err(_) => prop,
        };
        o.props.insert(idx.to_string(), prop);
        idx += 1;
    }
    if hole {
        idx += 1;
    }
    if let Some(pr) = o.props.get_mut("length") {
        pr.set_value(Value::Num(idx as f64));
    }
}

pub(super) fn obj_rest(
    i: &mut Interp,
    keys: &[Rc<str>],
    stack: &mut Vec<Value>,
) -> Result<(), Abrupt> {
    let value = stack.last().expect("vm stack underflow").clone();
    let used: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
    let obj = i.copy_data_properties(&value, &used)?;
    stack.push(Value::Obj(obj));
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Compiler
// ---------------------------------------------------------------------------------------------

/// Whether a class-definition-time expression (heritage, computed key, decorator) can be
/// evaluated by the oracle over the compiled body's env: capture analysis already homes every
/// name it references in the activation (see `CaptureScan::class`); what remains is refusing the
/// constructs whose meaning depends on the compiled frame itself.
fn class_time_expr_ok(e: &Expr) -> bool {
    match e {
        Expr::Num(_)
        | Expr::BigInt(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Undefined
        | Expr::This
        | Expr::Regex { .. } => true,
        Expr::Ident(n) => n != "arguments",
        Expr::Paren(x) | Expr::ToStr(x) => class_time_expr_ok(x),
        Expr::Unary { op, arg } => *op != "delete" && class_time_expr_ok(arg),
        Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
            class_time_expr_ok(left) && class_time_expr_ok(right)
        }
        Expr::Cond { test, cons, alt } => {
            class_time_expr_ok(test) && class_time_expr_ok(cons) && class_time_expr_ok(alt)
        }
        Expr::Member { obj, prop, .. } => {
            !matches!(**obj, Expr::Super) && !prop.starts_with('#') && class_time_expr_ok(obj)
        }
        Expr::Index { obj, index, .. } => {
            !matches!(**obj, Expr::Super) && class_time_expr_ok(obj) && class_time_expr_ok(index)
        }
        // (TypeScript's down-levelled decorators: `[(_m_decorators = [dec(function () {})],
        // key)]`.) A non-arrow function owns its `this`/`arguments`/`new.target`; everything
        // else it names is captured (the class is scanned one function level down).
        Expr::Seq(items) => items.iter().all(class_time_expr_ok),
        Expr::Assign { target, value, .. } => {
            class_time_expr_ok(target) && class_time_expr_ok(value)
        }
        Expr::Array(elems) => elems.iter().all(|a| match a {
            crate::ast::ArrayElem::Item(x) | crate::ast::ArrayElem::Spread(x) => {
                class_time_expr_ok(x)
            }
            crate::ast::ArrayElem::Hole => true,
        }),
        // An arrow: its `this` flows through the env like the class's own (CaptureScan walks
        // the class as an arrow chain); `arguments`, `super` and `new.target` would resolve
        // against the compiled frame — refused by a conservative source-text check.
        Expr::Func(f) => {
            !f.is_arrow
                || f.source().is_some_and(|src| {
                    !src.contains("arguments") && !src.contains("super") && !src.contains("new.target")
                })
        }
        Expr::Call { callee, args, .. } | Expr::New { callee, args, .. } => {
            !matches!(**callee, Expr::Super)
                && !matches!(&**callee, Expr::Ident(n) if n == "eval")
                && class_time_expr_ok(callee)
                && args.iter().all(|a| match a {
                    crate::ast::ArrayElem::Item(x) | crate::ast::ArrayElem::Spread(x) => {
                        class_time_expr_ok(x)
                    }
                    crate::ast::ArrayElem::Hole => true,
                })
        }
        _ => false,
    }
}

fn class_ok(c: &Class) -> bool {
    c.decorators.is_empty()
        && c.superclass.as_deref().is_none_or(class_time_expr_ok)
        && c.members.iter().all(|m| {
            m.decorators.is_empty()
                && match &m.key {
                    PropKey::Computed(k) => class_time_expr_ok(k),
                    _ => true,
                }
        })
}

impl Compiler {
    /// `target &&= / ||= / ??= value`: the Reference is evaluated once, GetValue, then either
    /// the current value (short-circuit, no PutValue) or the RHS stored and produced. An
    /// anonymous function RHS is named after an identifier target (the oracle names it after
    /// evaluation; an anonymous class, whose name is observable during its definition, bails).
    pub(super) fn logical_assign(&mut self, op: &str, target: &Expr, value: &Expr) -> CResult {
        let jump = |c: &mut Compiler| match op {
            "&&=" => c.emit(Op::JumpIfFalsePeek(0)),
            "||=" => c.emit(Op::JumpIfTruePeek(0)),
            _ => c.emit(Op::JumpIfNotNullishPeek(0)),
        };
        match target {
            Expr::Ident(name) => {
                if matches!(value, Expr::Class(c) if c.name.is_none()) {
                    return Err(Bail);
                }
                let (load, store) = match self.home(name) {
                    Some(Home::Slot(_, true)) | Some(Home::Env(true)) | Some(Home::Blk(_, true)) => {
                        return Err(Bail)
                    }
                    Some(Home::Slot(slot, false)) => (Op::LoadLocal(slot), Op::StoreLocal(slot)),
                    Some(Home::Blk(c, false)) => {
                        let n = self.name_idx(name);
                        (Op::BlkLoad(c, n), Op::BlkStore(c, n))
                    }
                    Some(Home::Env(false)) => {
                        let n = self.name_idx(name);
                        (Op::LoadCap(n), Op::StoreCap(n))
                    }
                    None => {
                        let n = self.name_idx(name);
                        let c = self.new_name_cache();
                        let cs = self.new_name_cache();
                        (Op::LoadName(n, c), Op::StoreNameCached(n, cs))
                    }
                };
                self.emit(load);
                let j = jump(self);
                self.emit(Op::Pop);
                self.named_expr(value, name)?;
                self.emit(Op::Dup);
                self.emit(store);
                self.patch(j);
                Ok(())
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                self.expr(obj)?;
                self.emit(Op::Dup);
                let n = self.name_idx(prop);
                let private = prop.starts_with('#');
                if private {
                    self.emit(Op::GetPrivate(n));
                } else {
                    let c = self.new_cache();
                    self.emit(Op::GetProp(n, c));
                }
                let j = jump(self);
                self.emit(Op::Pop);
                self.expr(value)?;
                if private {
                    self.emit(Op::SetPrivate(n));
                } else {
                    let c = self.new_cache();
                    self.emit(Op::SetProp(n, c));
                }
                let end = self.emit(Op::Jump(0));
                self.patch(j);
                self.emit(Op::Nip);
                self.patch(end);
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                // As the compound form: the key is coerced once, before the read.
                self.expr(obj)?;
                self.expr(index)?;
                self.emit(Op::ToPropKey);
                self.emit(Op::Dup2);
                self.emit(Op::GetElem);
                let j = jump(self);
                self.emit(Op::Pop);
                self.expr(value)?;
                self.emit(Op::SetElem);
                let end = self.emit(Op::Jump(0));
                self.patch(j);
                self.emit(Op::Nip);
                self.emit(Op::Nip);
                self.patch(end);
                Ok(())
            }
            _ => Err(Bail),
        }
    }

    /// A class expression/declaration value: ClassDefinitionEvaluation by the oracle over the
    /// current env. `name` is the NamedEvaluation target for an anonymous class.
    pub(super) fn class_value(&mut self, c: &Rc<Class>, name: Option<&str>) -> CResult {
        if !class_ok(c) {
            if super::bail_log_enabled() {
                let bad = c
                    .superclass
                    .iter()
                    .map(|e| &**e)
                    .chain(c.members.iter().filter_map(|m| match &m.key {
                        PropKey::Computed(k) => Some(k),
                        _ => None,
                    }))
                    .find(|e| !class_time_expr_ok(e));
                match bad {
                    Some(e) if c.decorators.is_empty() => super::log_bail(
                        "class",
                        &format!("definition-time expression {}", super::node_kind(e)),
                    ),
                    _ => super::log_bail("class", "decorators"),
                }
            }
            return Err(Bail);
        }
        // A computed key / heritage reading `this` resolves it through the env chain: the
        // activation must carry it (CaptureScan set env_this for exactly this case).
        let cidx = self.classes.len() as u32;
        self.classes.push(c.clone());
        let n = match name {
            Some(n) if c.name.is_none() => self.name_idx(n),
            _ => u32::MAX,
        };
        self.env_prefix();
        self.emit(Op::MakeClass(cidx, n));
        Ok(())
    }

    /// General object literal (anything the template path `object_literal` refuses).
    pub(super) fn object_literal_general(&mut self, props: &[PropDef]) -> CResult {
        self.emit(Op::NewObject);
        for prop in props {
            match prop {
                PropDef::KeyValue { key, value } | PropDef::Cover { key, value } => {
                    let named = crate::eval::is_anonymous_fn(value);
                    match key {
                        PropKey::Ident(k) => {
                            let n = self.name_idx(k);
                            self.expr(value)?;
                            self.emit(Op::InitProp(n, named));
                        }
                        PropKey::Str(k) => {
                            let n = self.name_idx(k);
                            self.expr(value)?;
                            self.emit(Op::InitProp(n, named));
                        }
                        PropKey::Num(x) => {
                            let ci = self.const_idx(Value::Num(*x));
                            self.emit(Op::Const(ci));
                            self.expr(value)?;
                            self.emit(Op::InitPropComputed(named));
                        }
                        PropKey::Computed(e) => {
                            self.expr(e)?;
                            self.emit(Op::ToPropKey);
                            self.expr(value)?;
                            self.emit(Op::InitPropComputed(named));
                        }
                    }
                }
                PropDef::Method { key, func }
                | PropDef::Getter { key, func }
                | PropDef::Setter { key, func } => {
                    let kind = match prop {
                        PropDef::Method { .. } => 0,
                        PropDef::Getter { .. } => 1,
                        _ => 2,
                    };
                    let n = match key {
                        PropKey::Ident(k) => self.name_idx(k),
                        PropKey::Str(k) => self.name_idx(k),
                        PropKey::Num(x) => {
                            let ci = self.const_idx(Value::Num(*x));
                            self.emit(Op::Const(ci));
                            u32::MAX
                        }
                        PropKey::Computed(e) => {
                            self.expr(e)?;
                            self.emit(Op::ToPropKey);
                            u32::MAX
                        }
                    };
                    let fidx = self.funcs.len() as u32;
                    self.funcs.push(func.clone());
                    self.env_prefix();
                    self.emit(Op::InitMethod(fidx, n, kind));
                }
                PropDef::Spread(e) => {
                    self.expr(e)?;
                    self.emit(Op::CopyDataProps);
                }
                PropDef::Proto(e) => {
                    self.expr(e)?;
                    self.emit(Op::SetProtoLit);
                }
            }
        }
        Ok(())
    }
}

