//! Constructor templates: `new C(a, b)` without running any code.
//!
//! A constructor whose whole effect on the instance is a fixed list of own data properties —
//! class fields with constant (or no) initializers, then a body of straight-line
//! `this.<name> = <parameter | constant>` statements — gets a [`CtorPlan`]: the instance's final
//! keys in definition order and, per slot, where its value comes from. A construct with a plan
//! allocates the instance once, on a precomputed template map (`Object::new_from_template`:
//! inline slots, final shape), and writes the slot values directly. No frame, no bytecode.
//!
//! Semantics the plan must reproduce:
//! * Class fields (public and private) are DefineField: never observable through the prototype
//!   chain. Private fields are non-enumerable own entries under their resolved private name.
//! * A body assignment to a key the fields already defined overwrites that own writable data
//!   property in place (same slot). An assignment creating a new key is OrdinarySet on a fresh
//!   object: it creates an own data property exactly when no setter / non-writable data
//!   property / exotic object sits on the prototype chain. That is proven per prototype object
//!   by the same walk the property-creation inline cache uses (every hop marked as a live
//!   prototype, so any later change bumps the global proto epoch) and cached per
//!   (prototype, epoch).
//! * A private assignment `this.#x = v` requires `#x` to be a declared private *field* (so it is
//!   a plain overwrite of the field's slot).
//! * Missing arguments read `undefined`; extra ones are ignored.
//! * A derived class's constructor must start with `super(<parameters | constants>)` (or be the
//!   default `constructor(...args) { super(...args) }`) into a parent with a plan: the parent's
//!   slots come first (its argument sources mapped through the super arguments), then the
//!   class's own fields (a public field the parent created is redefined in place), then its
//!   assignments. GetSuperConstructor is live, so every (class, parent) link the plan spans is
//!   re-checked per construct ([`CtorPlan::guards_ok`]). A `super(…)` from a derived
//!   constructor without a plan into a parent with one stamps the parent's slots onto the
//!   still-empty `this` ([`run_plan_on`]).
//!
//! Anything else — calls, other uses of `this`, `arguments`, returns, parameter defaults /
//! patterns, decorators, private methods, computed field values that are not constants — has no
//! plan and constructs through the ordinary paths.
//!
//! JIT use: [`plan_for`] gives the plan of a constructor object; [`CtorPlan::template`],
//! [`CtorPlan::slots`] describe the allocation; [`CtorPlan::proto_ok`] is the per-construct
//! prototype-chain guard, and [`construct_with_plan`] is the whole operation.
use crate::ast::{ArrayElem, Expr, Function, Pattern, Stmt};
use crate::interpreter::{Abrupt, Interp};
use crate::value::{Callable, Exotic, Gc, Object, Props, Property, Value, WeakGc};
use std::cell::RefCell;
use std::rc::Rc;

/// Where one instance slot's value comes from.
#[derive(Clone)]
pub(crate) enum PlanSrc {
    /// The constructor's argument `k` (`undefined` when absent).
    Arg(u16),
    /// A primitive constant (a literal field initializer or assigned literal).
    Const(Value),
}

/// One own property of a planned instance, in shape order.
#[derive(Clone)]
pub(crate) struct PlanSlot {
    pub(crate) key: Rc<str>,
    pub(crate) src: PlanSrc,
    /// A private field: a non-enumerable entry (`Property::data(v, true, false, false)`), as
    /// PrivateFieldAdd stamps it. Public slots are plain (writable/enumerable/configurable).
    pub(crate) private: bool,
}

/// A constructor template (see the module docs).
pub(crate) struct CtorPlan {
    pub(crate) slots: Vec<PlanSlot>,
    /// The final map: every slot's key in order (placeholder values). Instances are
    /// instantiated from it.
    pub(crate) template: Props,
    /// Whether any slot is private (then the instance's private entries are re-stamped with
    /// their attributes after the plain fill).
    has_private: bool,
    /// Keys first created by a body assignment (OrdinarySet): the prototype chain must prove
    /// them creatable.
    set_keys: Vec<Rc<str>>,
    /// Prototype objects already proven for `set_keys`, with the proto epoch of the proof
    /// (weak: a side table must not keep a prototype — and through `constructor` its class —
    /// alive under the refcount cycle collector).
    proven: RefCell<Vec<(WeakGc, u32)>>,
    /// `(class, parent)` pairs a derived plan was built on (weak, as `proven`): each class's
    /// live [[GetPrototypeOf]] must still be that parent (GetSuperConstructor).
    pub(crate) guards: Vec<(WeakGc, WeakGc)>,
}

/// The per-constructor plan state (on a class's `ClassInfo`; plain functions keep theirs in
/// `Interp::ctor_plans`).
#[derive(Default)]
pub(crate) enum PlanState {
    #[default]
    Cold,
    Ready(Rc<CtorPlan>),
    Off,
}

impl CtorPlan {
    fn new(parts: Parts) -> CtorPlan {
        let Parts {
            slots,
            set_keys,
            guards,
        } = parts;
        let mut template = Props::new();
        for s in &slots {
            template.insert(s.key.clone(), Property::plain(Value::Undefined));
        }
        let has_private = slots.iter().any(|s| s.private);
        CtorPlan {
            slots,
            template,
            has_private,
            set_keys,
            proven: RefCell::new(Vec::new()),
            guards,
        }
    }

    /// Every derived class the plan spans still has the parent it was built on.
    #[inline]
    pub(crate) fn guards_ok(&self) -> bool {
        self.guards.iter().all(|(c, p)| {
            c.upgrade().is_some_and(|c| {
                c.borrow()
                    .proto
                    .as_ref()
                    .is_some_and(|q| Gc::as_ptr(q) == p.as_ptr())
            })
        })
    }

    /// The template map instances are built from.
    #[allow(dead_code)]
    pub(crate) fn template(&self) -> &Props {
        &self.template
    }

    /// Whether an instance with prototype `proto` may be built from the plan: every key a body
    /// assignment creates is creatable through `proto`'s chain (no setter, no non-writable data
    /// property, only ordinary objects). Cached per (prototype, proto epoch).
    pub(crate) fn proto_ok(&self, proto: &Gc) -> bool {
        if self.set_keys.is_empty() {
            return true;
        }
        let epoch = crate::value::proto_epoch();
        if epoch != u32::MAX
            && self
                .proven
                .borrow()
                .iter()
                .any(|(p, e)| *e == epoch && p.as_ptr() == Gc::as_ptr(proto))
        {
            return true;
        }
        if !chain_allows_create(proto, &self.set_keys) {
            return false;
        }
        if epoch != u32::MAX {
            let mut proven = self.proven.borrow_mut();
            proven.retain(|(p, e)| *e == epoch && p.as_ptr() != Gc::as_ptr(proto));
            if proven.len() >= 4 {
                proven.remove(0);
            }
            proven.push((Gc::downgrade(proto), epoch));
        }
        true
    }

    #[inline]
    fn value_of(src: &PlanSrc, args: &[Value]) -> Value {
        match src {
            PlanSrc::Arg(k) => args.get(*k as usize).cloned().unwrap_or(Value::Undefined),
            PlanSrc::Const(v) => v.clone(),
        }
    }

    /// Allocate the instance on `proto` (the caller proved [`CtorPlan::proto_ok`]).
    #[inline]
    pub(crate) fn instantiate(&self, proto: Gc, args: &[Value]) -> Gc {
        let obj = Object::new_from_template(
            Some(proto),
            &self.template,
            self.slots.iter().map(|s| Self::value_of(&s.src, args)),
        );
        if self.has_private {
            self.stamp_private(&obj);
        }
        obj
    }

    #[inline(never)]
    fn stamp_private(&self, obj: &Gc) {
        let mut b = obj.borrow_mut();
        for (k, s) in self.slots.iter().enumerate() {
            if s.private {
                if let Some(p) = b.props.entry_at_mut(k) {
                    let v = p.value();
                    *p = Property::data(v, true, false, false);
                }
            }
        }
    }
}

/// OrdinarySet would create each of `keys` as an own data property on a fresh extensible
/// ordinary object whose prototype is `proto` (see `Interp::try_ic_create`, whose walk this is).
fn chain_allows_create(proto: &Gc, keys: &[Rc<str>]) -> bool {
    let mut pending: Vec<&Rc<str>> = keys.iter().collect();
    let mut walk = Some(proto.clone());
    let mut hops = 0;
    while let Some(h) = walk {
        hops += 1;
        if hops > 8 {
            return false;
        }
        let hb = h.borrow();
        if !(matches!(hb.exotic, Exotic::None) && hb.ic_plain.get()) {
            return false;
        }
        let mut bad = false;
        pending.retain(|k| match hb.props.slot_of(k) {
            Some(slot) => {
                let p = hb.props.entry_at(slot).unwrap();
                if p.accessor() || !p.writable() {
                    bad = true;
                }
                false
            }
            None => true,
        });
        hb.props.mark_proto();
        if bad {
            return false;
        }
        if pending.is_empty() {
            return true;
        }
        let next = hb.proto.clone();
        drop(hb);
        walk = next;
    }
    true
}

/// A primitive constant expression's value.
fn const_value(e: &Expr) -> Option<Value> {
    Some(match e {
        Expr::Num(n) => Value::Num(*n),
        Expr::Str(s) => Value::Str(s.clone().into()),
        Expr::Bool(b) => Value::Bool(*b),
        Expr::Null => Value::Null,
        Expr::Undefined => Value::Undefined,
        Expr::Paren(e) => return const_value(e),
        Expr::Unary { op: "-", arg } => match &**arg {
            Expr::Num(n) => Value::Num(-*n),
            _ => return None,
        },
        _ => return None,
    })
}

/// A key the template can hold as a plain named entry (no array-index keys).
fn plain_key(k: &str) -> bool {
    crate::value::canonical_index(k).is_none()
}

/// The instance-shaping parts of a plan under construction.
struct Parts {
    slots: Vec<PlanSlot>,
    set_keys: Vec<Rc<str>>,
    guards: Vec<(WeakGc, WeakGc)>,
}

/// A `super(…)` argument / assigned value: a parameter or a primitive constant.
fn value_src(e: &Expr, params: &[&str]) -> Option<PlanSrc> {
    match e {
        Expr::Ident(n) => params
            .iter()
            .position(|p| p == n)
            .map(|k| PlanSrc::Arg(k as u16)),
        e => Some(PlanSrc::Const(const_value(e)?)),
    }
}

/// Build the slot list: for a derived class, the parent plan's slots (argument sources mapped
/// through the leading `super(…)` call; `parent` is (parent plan, class, parent constructor));
/// then `fields` (key, constant-or-absent initializer); then the constructor body `func`'s
/// assignments. `private_key` resolves a `#name` in the body.
fn build(
    parent: Option<(&CtorPlan, &Gc, &Gc)>,
    fields: &[(Rc<str>, Option<&Expr>)],
    func: &Function,
    private_key: &dyn Fn(&str) -> Option<Rc<str>>,
) -> Option<Parts> {
    if func.is_arrow || func.is_method || func.is_generator || func.is_async {
        return None;
    }
    func.ensure_body().ok()?;
    let body = func.body();
    // (A directive prologue — or any constant expression statement — does nothing; nor does a
    // call-guard `if (!new.target) …`: under a construct `new.target` is an object.)
    let mut stmts = body.iter().filter(|s| {
        !matches!(s, Stmt::Expr(e) if const_value(e).is_some()) && !new_target_guard(s)
    });
    let mut parts = Parts {
        slots: Vec::new(),
        set_keys: Vec::new(),
        guards: Vec::new(),
    };
    // Parameters: simple, distinct identifiers — or the synthesized default derived
    // constructor's `...%args`, forwarded whole (no iterator protocol, see `DEFAULT_CTOR_ARGS`).
    let forward = matches!(
        func.params.as_slice(),
        [crate::ast::Param { pattern: Pattern::Ident(n), rest: true, .. }]
            if n == crate::eval::DEFAULT_CTOR_ARGS
    );
    let mut params: Vec<&str> = Vec::new();
    if !forward {
        for p in &func.params {
            let Pattern::Ident(n) = &p.pattern else {
                return None;
            };
            if p.rest || p.default.is_some() || params.contains(&n.as_str()) || params.len() >= 64
            {
                return None;
            }
            params.push(n);
        }
    }
    if let Some((pp, class, parent_ctor)) = parent {
        // The leading `super(…)`: the parent's slots with its arguments mapped.
        let Some(Stmt::Expr(Expr::Call {
            callee,
            args,
            optional: false,
            ..
        })) = stmts.next()
        else {
            return None;
        };
        if !matches!(**callee, Expr::Super) {
            return None;
        }
        let mapped: Option<Vec<PlanSrc>> = if forward {
            match args.as_slice() {
                [ArrayElem::Spread(Expr::Ident(n))] if n == crate::eval::DEFAULT_CTOR_ARGS => None,
                _ => return None,
            }
        } else {
            let mut m = Vec::new();
            for a in args {
                let ArrayElem::Item(e) = a else {
                    return None;
                };
                m.push(value_src(e, &params)?);
            }
            Some(m)
        };
        for s in &pp.slots {
            let src = match (&s.src, &mapped) {
                (PlanSrc::Arg(k), Some(m)) => m
                    .get(*k as usize)
                    .cloned()
                    .unwrap_or(PlanSrc::Const(Value::Undefined)),
                (src, _) => src.clone(),
            };
            parts.slots.push(PlanSlot {
                key: s.key.clone(),
                src,
                private: s.private,
            });
        }
        parts.set_keys.extend(pp.set_keys.iter().cloned());
        parts.guards.extend(
            pp.guards
                .iter()
                .map(|(c, p)| (c.clone(), p.clone())),
        );
        parts
            .guards
            .push((Gc::downgrade(class), Gc::downgrade(parent_ctor)));
    } else if forward {
        return None;
    }
    // Fields: DefineField. A public key the parent created is redefined in place (its entry is
    // a plain writable/enumerable/configurable data property); a private name is always new.
    let n_inherited = parts.slots.len();
    for (key, init) in fields {
        if !plain_key(key) || parts.slots[n_inherited..].iter().any(|s| s.key == *key) {
            return None;
        }
        let src = PlanSrc::Const(match init {
            Some(e) => const_value(e)?,
            None => Value::Undefined,
        });
        let private = Interp::is_private_key(key);
        match parts
            .slots
            .iter_mut()
            .find(|s| !private && !s.private && s.key == *key)
        {
            Some(slot) => slot.src = src,
            None => parts.slots.push(PlanSlot {
                key: key.clone(),
                src,
                private,
            }),
        }
    }
    for s in stmts {
        let Stmt::Expr(Expr::Assign { op: "=", target, value }) = s else {
            return None;
        };
        let Expr::Member {
            obj,
            prop,
            optional: false,
        } = &**target
        else {
            return None;
        };
        if !matches!(**obj, Expr::This) || forward {
            return None;
        }
        let src = value_src(value, &params)?;
        if prop.starts_with('#') {
            // PrivateSet on a declared private field of this class: an overwrite of its slot.
            let key = private_key(prop)?;
            let slot = parts.slots[n_inherited..]
                .iter_mut()
                .find(|s| s.private && s.key == key)?;
            slot.src = src;
            continue;
        }
        let key: Rc<str> = Rc::from(prop.as_str());
        if !plain_key(&key) {
            return None;
        }
        match parts.slots.iter_mut().find(|s| !s.private && s.key == key) {
            // An own writable data property already: overwrite in place.
            Some(slot) => slot.src = src,
            None => {
                parts.set_keys.push(key.clone());
                parts.slots.push(PlanSlot {
                    key,
                    src,
                    private: false,
                });
            }
        }
    }
    // The inline-slot template covers small instances; bigger ones gain little here.
    if parts.slots.len() > 16 {
        return None;
    }
    Some(parts)
}

/// The plan of class constructor object `c` (its `ClassInfo` key `ptr`).
fn class_plan(i: &mut Interp, c: &Gc, ptr: usize, func: &Function) -> Option<CtorPlan> {
    let derived = {
        let ci = i.class_info.get(&ptr)?;
        if !ci.private_members.is_empty()
            || !ci.instance_initializers.is_empty()
            || ci.fields.iter().any(|f| !f.transforms.is_empty())
        {
            return None;
        }
        ci.derived
    };
    // A derived class builds on its parent's plan: GetSuperConstructor is the constructor's
    // live [[GetPrototypeOf]], re-checked on every use (`CtorPlan::guards_ok`). (A parent with a
    // plan is a user function that is a constructor.)
    let parent = if derived {
        let pc = c.borrow().proto.clone()?;
        let pp = plan_for(i, &pc)?;
        Some((pp, pc))
    } else {
        None
    };
    let ci = i.class_info.get(&ptr)?;
    let fields: Vec<(Rc<str>, Option<&Expr>)> = ci
        .fields
        .iter()
        .map(|f| (Rc::from(f.key.as_str()), f.init.as_ref()))
        .collect();
    let env = ci.field_env.clone();
    let parts = build(
        parent.as_ref().map(|(pp, pc)| (&**pp, c, pc)),
        &fields,
        func,
        &|name| {
            let k = i.resolve_private(name, &env);
            (k != name).then(|| Rc::from(k.as_str()))
        },
    )?;
    Some(CtorPlan::new(parts))
}

/// The constructor plan of function object `c`, computed on first use. `None`: `c` has none
/// (not a user constructor, a body outside the planned subset, a proxy, or the tree-walking
/// tier).
pub(crate) fn plan_for(i: &mut Interp, c: &Gc) -> Option<Rc<CtorPlan>> {
    if matches!(i.tier, super::Tier::Interp) {
        return None;
    }
    let ptr = Gc::as_ptr(c) as usize;
    // (A plan, once made, never changes; the weak handle keeps the address from being reused.)
    if let Some((w, p)) = &i.plan_cache {
        if w.as_ptr() as usize == ptr {
            return Some(p.clone());
        }
    }
    let plan = plan_lookup(i, c, ptr)?;
    i.plan_cache = Some((Gc::downgrade(c), plan.clone()));
    Some(plan)
}

/// The plan [`plan_for`] would return when it is already made (no side effects: for the JIT,
/// which compiles `new` sites the interpreter has run).
pub(crate) fn plan_ready(i: &Interp, c: &Gc) -> Option<Rc<CtorPlan>> {
    if matches!(i.tier, super::Tier::Interp) {
        return None;
    }
    let ptr = Gc::as_ptr(c) as usize;
    if !i.proxies.is_empty() && i.proxies.contains_key(&ptr) {
        return None;
    }
    if let Some((w, p)) = &i.plan_cache {
        if w.as_ptr() as usize == ptr {
            return Some(p.clone());
        }
    }
    if let Some(ci) = i.class_info.get(&ptr) {
        return match &ci.plan {
            PlanState::Ready(p) => Some(p.clone()),
            _ => None,
        };
    }
    let func = match &c.borrow().call {
        Callable::User(u) => u.func.clone(),
        _ => return None,
    };
    i.ctor_plans
        .get(&(Rc::as_ptr(&func) as usize))
        .and_then(|(_, st)| st.clone())
}

#[inline(never)]
fn plan_lookup(i: &mut Interp, c: &Gc, ptr: usize) -> Option<Rc<CtorPlan>> {
    if !i.proxies.is_empty() && i.proxies.contains_key(&ptr) {
        return None;
    }
    if !i.class_info.is_empty() {
        if let Some(ci) = i.class_info.get_mut(&ptr) {
            match &ci.plan {
                PlanState::Ready(p) => return Some(p.clone()),
                PlanState::Off => return None,
                PlanState::Cold => {}
            }
            // (Off while it is computed: a parent chain leading back here finds no plan.)
            ci.plan = PlanState::Off;
            let func = match &c.borrow().call {
                Callable::User(u) => u.func.clone(),
                _ => return None,
            };
            let plan = class_plan(i, c, ptr, &func).map(Rc::new);
            if let (Some(p), Some(ci)) = (&plan, i.class_info.get_mut(&ptr)) {
                ci.plan = PlanState::Ready(p.clone());
            }
            return plan;
        }
    }
    let func = match &c.borrow().call {
        Callable::User(u) => u.func.clone(),
        _ => return None,
    };
    let fkey = Rc::as_ptr(&func) as usize;
    if let Some((_, st)) = i.ctor_plans.get(&fkey) {
        return st.clone();
    }
    let plan = build(None, &[], &func, &|_| None).map(|p| Rc::new(CtorPlan::new(p)));
    i.ctor_plans.insert(fkey, (func, plan.clone()));
    plan
}

/// `run_constructor_on`'s shortcut: a `super(…)` (from a derived constructor without a plan of
/// its own) reaching a parent `ctor` with a plan stamps the plan's properties onto the
/// still-empty `this` and returns the parent body's completion (undefined). `None`: not
/// applicable (nothing was done).
pub(crate) fn run_plan_on(
    i: &mut Interp,
    ctor: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, Abrupt>> {
    let (Value::Obj(c), Value::Obj(o)) = (ctor, this) else {
        return None;
    };
    if i.multi_realm() || !i.ordinary_get_ptr(Gc::as_ptr(o) as usize) {
        return None;
    }
    let plan = plan_for(i, c)?;
    if !plan.guards_ok() {
        return None;
    }
    let proto = {
        let b = o.borrow();
        if !matches!(b.exotic, Exotic::None)
            || !b.extensible
            || !matches!(b.call, Callable::None)
            || b.props.iter().next().is_some()
        {
            return None;
        }
        b.proto.clone()?
    };
    if !plan.proto_ok(&proto) {
        return None;
    }
    // The parent body would consume the pending new.target; nothing here reads it.
    i.pending_new_target = Value::Undefined;
    let mut b = o.borrow_mut();
    for s in &plan.slots {
        let v = CtorPlan::value_of(&s.src, args);
        let p = if s.private {
            Property::data(v, true, false, false)
        } else {
            Property::plain(v)
        };
        b.props.insert(s.key.clone(), p);
    }
    Some(Ok(Value::Undefined))
}

/// `new C(…args)` / `Reflect.construct(C, args, newTarget)` through `C`'s plan: the
/// prototype from new.target (a non-proxy user function), the chain proof, one allocation.
/// `None`: not applicable (nothing observable was done).
#[inline]
pub(crate) fn construct_with_plan(
    i: &mut Interp,
    callee: &Value,
    new_target: &Value,
    args: &[Value],
) -> Option<Result<Value, Abrupt>> {
    let (Value::Obj(c), Value::Obj(nt)) = (callee, new_target) else {
        return None;
    };
    if i.multi_realm() {
        return None;
    }
    if !i.proxies.is_empty()
        && (i.proxies.contains_key(&(Gc::as_ptr(c) as usize))
            || i.proxies.contains_key(&(Gc::as_ptr(nt) as usize)))
    {
        return None;
    }
    let plan = plan_for(i, c)?;
    if !plan.guards_ok() {
        return None;
    }
    if !Gc::ptr_eq(c, nt) && !matches!(nt.borrow().call, Callable::User(_)) {
        return None;
    }
    let proto = match super::class_fields::own_prototype(nt)? {
        Value::Obj(p) => p,
        // (Single realm: GetFunctionRealm is the current one.)
        _ => i.object_proto.clone(),
    };
    if !plan.proto_ok(&proto) {
        return None;
    }
    if let Err(e) = i.gc_check_amortized() {
        return Some(Err(e));
    }
    Some(Ok(Value::Obj(plan.instantiate(proto, args))))
}

#[cfg(test)]
mod tests {
    use crate::bytecode::Tier;
    use crate::{Completion, Engine};

    /// Evaluate on both tiers (the tree-walker never plans) and require agreement.
    fn run(src: &str) -> String {
        let mut out = Vec::new();
        for tier in [Tier::Interp, Tier::Bytecode] {
            let mut e = Engine::new();
            e.set_tier(tier);
            e.set_tier_threshold(0);
            out.push(match e.eval(src, false).unwrap() {
                Completion::Value(v) => v.to_string(),
                Completion::Throw { name, message } => format!("throw {name}: {message}"),
            });
        }
        assert_eq!(out[0], out[1], "tiers disagree on {src}");
        out.pop().unwrap()
    }

    fn planned(src: &str, name: &str) -> bool {
        let mut e = Engine::new();
        e.set_tier_threshold(0);
        let _ = e.eval(src, false).unwrap();
        let v = e.interp.global.borrow().props.get(name).unwrap().value();
        let c = v.as_obj().unwrap().clone();
        super::plan_for(&mut e.interp, &c).is_some()
    }

    #[test]
    fn target_shapes_get_plans() {
        let base = "class R { height; width; constructor(h, w) { this.height = h; this.width = w; } } \
                    class R2 { constructor(h, w) { this.height = h; this.width = w; } } \
                    class P { #h; #w; constructor(h, w) { this.#h = h; this.#w = w; } \
                    get area() { return this.#h * this.#w; } } \
                    class D extends R2 { d = 1; constructor(a) { super(a, 2); this.e = a; } } \
                    class DD extends P {} \
                    function F(a) { 'use strict'; this.a = a; this.k = -1; } \
                    function G(a) { this.a = a + 1; } \
                    class H { constructor(a) { this.a = a; foo(); } } \
                    Object.assign(globalThis, {R, R2, P, D, DD, F, G, H});";
        for name in ["R", "R2", "P", "D", "DD", "F"] {
            assert!(planned(base, name), "{name} should have a plan");
        }
        for name in ["G", "H"] {
            assert!(!planned(base, name), "{name} should have no plan");
        }
    }

    #[test]
    fn planned_constructs_match_the_oracle() {
        let cases = [
            "class R { height; width = 3; constructor(h) { this.height = h; this.x = 'x'; } } \
             const r = new R(4); JSON.stringify(r) + Object.keys(r)",
            "class P { #h; #w = 2; constructor(h) { this.#h = h; } get a() { return this.#h * this.#w; } \
             static has(o) { return #h in o; } } const p = new P(5); \
             [p.a, P.has(p), P.has({}), Object.getOwnPropertyNames(p).length].join()",
            "function F(a, b) { this.a = a; this.b = b; } const f = new F(1); \
             [f.a, f.b, 'b' in f, f instanceof F].join()",
            // a setter / read-only property on the chain, installed after the plan
            "class R { constructor(v) { this.v = v; } } new R(1); let log = []; \
             Object.defineProperty(R.prototype, 'v', { set(x) { log.push(x); } }); \
             const r = new R(2); [log.join(), Object.keys(r).length].join()",
            "function G(v) { this.w = v; } new G(1); \
             Object.defineProperty(Object.prototype, 'w', { value: 0, writable: false }); \
             const g = new G(2); [g.w, Object.keys(g).length].join()",
            // derived: argument remap, parent swap, new.target prototype
            "class A { constructor(x, y) { this.x = x; this.y = y; } } \
             class B extends A { z = 0; constructor(p) { super(9, p); this.x = p; } } \
             const b = new B(4); JSON.stringify(b)",
            "class A { constructor(x) { this.x = x; } } class B extends A {} new B(1); \
             Object.setPrototypeOf(B, function C(v) { this.c = v; }); JSON.stringify(new B(2))",
            "function F(a) { this.a = a; } class X {} const o = Reflect.construct(F, [1], X); \
             [o instanceof X, o.a].join()",
            "function F(a) { this.a = a; } F.prototype = 5; \
             Object.getPrototypeOf(new F(1)) === Object.prototype",
            // a derived constructor without a plan calling into a planned parent
            "class A { #p = 1; constructor(x) { this.x = x; } static p(o) { return o.#p; } } \
             class B extends A { constructor() { super(Math.max(1, 2)); this.y = this.x + 1; } } \
             const b = new B(); [b.x, b.y, A.p(b)].join()",
        ];
        for src in cases {
            run(src);
        }
    }
}

/// `if (<test>) …` (no `else`) whose side-effect-free test is false whenever `new.target` is an
/// object: `!new.target`, `new.target == null`, `new.target === undefined` (either operand order).
fn new_target_guard(s: &Stmt) -> bool {
    let Stmt::If {
        test,
        alt: None,
        ..
    } = s
    else {
        return false;
    };
    match test {
        Expr::Unary { op: "!", arg } => matches!(**arg, Expr::NewTarget),
        Expr::Binary { op, left, right } if matches!(*op, "==" | "===") => {
            let nullish = |e: &Expr| {
                matches!(e, Expr::Undefined) || (*op == "==" && matches!(e, Expr::Null))
            };
            (matches!(**left, Expr::NewTarget) && nullish(right))
                || (matches!(**right, Expr::NewTarget) && nullish(left))
        }
        _ => false,
    }
}
