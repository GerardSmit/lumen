//! Class construction on the bytecode tier.
//!
//! * Instance field initializers (see `interpreter::class_fields`, which synthesizes the
//!   initializer method): the `%definefield%(key, init, named)` intrinsic statement compiles to
//!   `LoadThis; <init>; DefineField` — never reachable from source (`%` is not an identifier
//!   character), and the synthetic method only ever runs compiled.
//! * Class constructor bodies run as VM frames ([`run_class_body`]) instead of through
//!   `run_constructor_on` → `call_user` → `call_user_inner` → `run`: `new C()` of a base class
//!   from bytecode enters an inline frame ([`inline_class_ctor`]); other constructs
//!   ([`construct_class`]) and `super(…)` reaching a class parent ([`run_class_ctor_on`]) run
//!   the body on a fresh driver.
use super::{ArrayElem, Bail, CResult, Compiler, Op};
use crate::ast::Expr;
use crate::interpreter::{Abrupt, Interp};
use crate::value::{Callable, Gc, Value};
use std::rc::Rc;

/// The intrinsic's callee name.
pub(crate) const DEFINE_FIELD: &str = "%definefield%";

impl Compiler {
    /// `%definefield%("key", init, named)`: evaluates to undefined.
    pub(super) fn define_field_intrinsic(&mut self, args: &[ArrayElem]) -> CResult {
        let [ArrayElem::Item(Expr::Str(key)), ArrayElem::Item(init), ArrayElem::Item(Expr::Bool(named))] =
            args
        else {
            return Err(Bail);
        };
        self.expr(&Expr::This)?;
        self.expr(init)?;
        let n = self.name_idx(key);
        self.emit(Op::DefineField(n, *named));
        self.emit(Op::Undef);
        Ok(())
    }
}

/// `Op::DefineField`: name an anonymous function value after the field, then DefineField.
#[inline]
pub(super) fn define_field(
    i: &mut Interp,
    this: Value,
    key: &Rc<str>,
    named: bool,
    v: Value,
) -> Result<(), Abrupt> {
    if named {
        i.set_fn_name(&v, crate::eval::private_display(key));
    }
    i.define_instance_field(&this, key, v)
}

/// A compiled class constructor that may run as a VM frame, and whether its class is derived
/// (a derived construct runs only a derived-mode chunk and a base one only a plain chunk, see
/// `Interp::call_user_inner`); `allow_derived` admits derived classes at all. The instance
/// fields are the caller's job.
#[inline]
fn class_ctor(i: &Interp, callee: &Value, allow_derived: bool) -> Option<(super::InlineCallee, bool)> {
    let Value::Obj(o) = callee else {
        return None;
    };
    if i.class_info.is_empty() {
        return None;
    }
    let key = Gc::as_ptr(o) as usize;
    let derived = i.class_info.get(&key)?.derived;
    if derived && !allow_derived {
        return None;
    }
    let b = o.borrow();
    let Callable::User(u) = &b.call else {
        return None;
    };
    let f = &u.func;
    let Some(Some(chunk)) = f.code.get() else {
        return None;
    };
    if chunk.is_derived() != derived
        || f.is_arrow
        || f.is_method
        || f.is_generator
        || f.is_async
        || i.depth >= crate::interpreter::MAX_EVAL_DEPTH
        || matches!(i.tier, super::Tier::Interp)
        || i.multi_realm()
        || (!i.proxies.is_empty() && i.proxies.contains_key(&key))
        || u.env.borrow().under_with
    {
        return None;
    }
    let ic = super::InlineCallee {
        chunk: chunk.clone(),
        env: u.env.clone(),
        strict: f.is_strict,
        arrow: false,
        class_fields: !derived,
    };
    Some((ic, derived))
}

/// [`super::inline_ctor`] for a *base* class constructor (`new C()` from the bytecode VM): its
/// compiled body runs as an inline frame after `enter_inline` has initialized the instance's
/// fields (the ordinary path's `run_constructor_on` order). Derived classes (their `this` is
/// created by `super()`) run through [`construct_class`] instead.
#[inline]
pub(crate) fn inline_class_ctor(i: &Interp, callee: &Value) -> Option<super::InlineCallee> {
    class_ctor(i, callee, false).map(|(ic, _)| ic)
}

/// A user function's own `prototype` value. Every user function's `prototype` is a
/// non-configurable own data property (never an accessor), so for a non-proxy object this is
/// exactly `Get(F, "prototype")` — without the generic lookup. `None`: no such own property.
#[inline]
pub(super) fn own_prototype(o: &Gc) -> Option<Value> {
    let b = o.borrow();
    let p = b.props.entry_at(b.props.prototype_slot()? as usize)?;
    if p.accessor() {
        return None;
    }
    Some(p.value())
}

/// Run class constructor `ctor`'s compiled body on `inst` as a VM frame, with the active
/// new.target `new_target` (`run_constructor_on`'s user arm → `call_user` → `call_user_inner`,
/// net): `super(…)` is legal exactly in a derived body. Returns the body's completion value (a
/// derived body's is already the [[Construct]] result, see `Op::DerivedReturn`).
fn run_class_body(
    i: &mut Interp,
    ctor: &Value,
    inst: Gc,
    args: &[Value],
    ic: super::InlineCallee,
    new_target: Value,
    derived: bool,
) -> Result<Value, Abrupt> {
    let saved_super = std::mem::replace(&mut i.super_call_ok, derived);
    let mut rec = i.vm_frame_one.pop().unwrap_or_default();
    // SAFETY: `args` is only read (not moved from) with `move_args == false`.
    unsafe {
        super::enter_frame(
            i,
            &mut rec,
            ctor.clone(),
            Value::Undefined,
            Some(inst),
            ic,
            args.as_ptr() as *mut Value,
            args.len(),
            false,
        )
    };
    // `enter_frame` installed new.target = the constructor itself (its entry value is saved in
    // the record and restored by `leave_frame`); a `super(…)` / `Reflect.construct` target
    // differs.
    i.new_target = new_target;
    let r = {
        let super::InlineFrame {
            chunk,
            env,
            this_val,
            pc,
            slots,
            stack,
            handlers,
            ..
        } = &mut rec;
        // SAFETY: `enter_frame` filled both.
        let (chunk, env) = unsafe {
            (
                chunk.as_deref().unwrap_unchecked(),
                env.as_ref().unwrap_unchecked(),
            )
        };
        super::drive_vm(i, chunk, env, slots, stack, pc, this_val, handlers, None)
    };
    super::leave_frame(i, &mut rec);
    if i.vm_frame_one.len() < 64 {
        i.vm_frame_one.push(rec);
    }
    i.super_call_ok = saved_super;
    match r? {
        super::VmStep::Done(v) => Ok(v),
        super::VmStep::Await(_) => unreachable!("a synchronous bytecode function cannot await"),
    }
}

/// `construct_dispatch`'s shortcut for `new C(…)` / `Reflect.construct(C, …, nt)` of a class
/// `C` with a compiled constructor and a non-proxy user-function new.target:
/// OrdinaryCreateFromConstructor, a base class's instance fields, then the body as a VM frame
/// with the [[Construct]] return rule. `None`: not eligible (nothing was done).
pub(crate) fn construct_class(
    i: &mut Interp,
    callee: &Value,
    new_target: &Value,
    args: &[Value],
) -> Option<Result<Value, Abrupt>> {
    let (Value::Obj(c), Value::Obj(nt)) = (callee, new_target) else {
        return None;
    };
    let (ic, derived) = class_ctor(i, callee, true)?;
    let proto = if Gc::ptr_eq(c, nt) {
        own_prototype(c)?
    } else {
        if (!i.proxies.is_empty() && i.proxies.contains_key(&(Gc::as_ptr(nt) as usize)))
            || !matches!(nt.borrow().call, Callable::User(_))
        {
            return None;
        }
        own_prototype(nt)?
    };
    let proto = match proto {
        Value::Obj(p) => p,
        // (Single realm: GetFunctionRealm is the current one.)
        _ => i.object_proto.clone(),
    };
    let capacity = i.learned_construct_capacity(c);
    let inst = crate::value::Object::new_with_capacity(Some(proto), capacity);
    let this = Value::Obj(inst.clone());
    if !derived {
        if let Err(e) = i.init_instance_fields(callee, &this) {
            return Some(Err(e));
        }
    }
    let r = run_class_body(i, callee, inst.clone(), args, ic, new_target.clone(), derived);
    Some(r.map(|v| {
        i.observe_construct_capacity(c, &inst);
        match v {
            Value::Obj(_) => v,
            _ => this,
        }
    }))
}

/// `run_constructor_on`'s shortcut (a `super(…)` reaching a class parent) for a class with a
/// compiled constructor running on the existing object `this`, with the pending new.target:
/// a base class's fields, then the body as a VM frame. Returns the body's completion value.
/// `None`: not eligible (nothing was done).
pub(crate) fn run_class_ctor_on(
    i: &mut Interp,
    ctor: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, Abrupt>> {
    let Value::Obj(inst) = this else {
        return None;
    };
    let (ic, derived) = class_ctor(i, ctor, true)?;
    // Taken before the fields run: a construct inside an initializer must not consume it.
    let nt = std::mem::take(&mut i.pending_new_target);
    if !derived {
        if let Err(e) = i.init_instance_fields(ctor, this) {
            return Some(Err(e));
        }
    }
    Some(run_class_body(i, ctor, inst.clone(), args, ic, nt, derived))
}
