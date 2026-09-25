//! Prepared calls: a builtin that invokes one JS callback many times (`Array.prototype.map`,
//! a `sort` comparator, `String.prototype.replace` with a function, reaction jobs, …) resolves
//! the callee once and reuses the resolved state for every invocation.
//!
//! [`Interp::call`] re-derives everything per call: the dispatch chain (proxy / realm / class
//! checks and hash lookups), a frame record from the pool, an `Rc` clone of the chunk and of the
//! closure environment, argument clones. A [`PreparedCall`] does that work in [`PreparedCall::new`]
//! and keeps its own slot / operand / handler buffers, so a call of a plain compiled function is
//! just the per-call engine bookkeeping (recursion depth, GC poll, the reflection frame, the
//! per-body flags), the arguments moved into the slots, and the body run on [`drive_vm`] — whose
//! whole-function JIT entry then runs the callee's native code when it has some.
//!
//! Only what can't change between calls is hoisted: the callee's identity and kind, its chunk,
//! environment and flags. Everything the ordinary path checks that CAN change mid-iteration (the
//! recursion limit, a second realm appearing, `f.arguments` reflection being switched on) is
//! re-checked per call, and anything unusual takes [`Interp::call`] with identical semantics.
use super::*;
use crate::interpreter::{FnFrame, MAX_EVAL_DEPTH};
use crate::value::{Callable, NativeFn};

/// A callee resolved for repeated calls with a fixed `this` (see the module docs).
pub(crate) struct PreparedCall {
    callee: Value,
    this: Value,
    kind: Kind,
}

enum Kind {
    /// Anything else: every call takes [`Interp::call`].
    Generic,
    /// A plain native function (`Boolean`, `Number`, `Math.abs`, …).
    Native(NativeFn),
    /// A synchronous compiled user function without an activation environment, `arguments`
    /// object or rest parameter.
    Compiled(Box<Compiled>),
}

struct Compiled {
    chunk: Rc<Chunk>,
    env: Env,
    fn_ptr: usize,
    strict: bool,
    arrow: bool,
    slots: Vec<Value>,
    stack: Vec<Value>,
    handlers: Vec<Handler>,
}

impl PreparedCall {
    /// Resolve `callee` (any value: a non-callable one simply takes the generic path, which
    /// throws the ordinary TypeError at the first call).
    pub(crate) fn new(i: &mut Interp, callee: Value, this: Value) -> PreparedCall {
        let kind = resolve(i, &callee);
        PreparedCall { callee, this, kind }
    }

    /// Call the callee with the prepared `this`. The arguments are moved out of `args` (each is
    /// left `undefined`, or cloned when the call takes the generic path).
    #[inline]
    pub(crate) fn call(&mut self, i: &mut Interp, args: &mut [Value]) -> Result<Value, Abrupt> {
        match &mut self.kind {
            Kind::Compiled(c) if compiled_ok(i, c) => call_compiled_prepared(i, c, &self.this, |s| {
                for v in args.iter_mut() {
                    if !s.wants() {
                        break;
                    }
                    s.push(std::mem::take(v));
                }
            }),
            Kind::Native(f) if i.depth < MAX_EVAL_DEPTH && !i.multi_realm() => {
                let f = *f;
                call_native_prepared(i, f, &self.this, args)
            }
            _ => i.call(self.callee.clone(), self.this.clone(), args),
        }
    }

    /// The `(value, index, object)` call of the Array iteration methods, with a thrown value as
    /// the error: `obj` is only cloned when the callee declares a third parameter.
    #[inline]
    pub(crate) fn call3(
        &mut self,
        i: &mut Interp,
        a: Value,
        b: Value,
        obj: &Value,
    ) -> Result<Value, Value> {
        let r = match &mut self.kind {
            Kind::Compiled(c) if compiled_ok(i, c) => {
                call_compiled_prepared(i, c, &self.this, |s| {
                    s.push(a);
                    s.push(b);
                    if s.wants() {
                        s.push(obj.clone());
                    }
                })
            }
            _ => self.call(i, &mut [a, b, obj.clone()]),
        };
        match r {
            Ok(v) => Ok(v),
            Err(Abrupt::Throw(e)) => Err(e),
            Err(_) => Err(Value::Undefined),
        }
    }

    /// [`PreparedCall::call`] with an explicit `this` (the callee stays prepared).
    #[inline]
    pub(crate) fn call_with_this(
        &mut self,
        i: &mut Interp,
        this: Value,
        args: &mut [Value],
    ) -> Result<Value, Abrupt> {
        let saved = std::mem::replace(&mut self.this, this);
        let r = self.call(i, args);
        self.this = saved;
        r
    }

    /// Hand the buffers back to the engine's pool (optional: dropping is also fine).
    pub(crate) fn finish(self, i: &mut Interp) {
        if let Kind::Compiled(c) = self.kind {
            let Compiled { slots, stack, .. } = *c;
            if slots.capacity() != 0 && i.vm_pool.len() < 64 {
                i.vm_pool.push((slots, stack));
            }
        }
    }
}

/// A single call `callee(arg)` with `this` undefined (a reaction job's handler) as a direct
/// call of the callee's function code, when [`resolve`] would prepare it and it has some; `None`
/// (nothing done, `arg` handed back) otherwise, and the caller takes [`Interp::call`].
pub(crate) fn call_once_direct(
    i: &mut Interp,
    callee: &Value,
    arg: Value,
) -> Result<Result<Value, Abrupt>, Value> {
    let Value::Obj(o) = callee else { return Err(arg) };
    if matches!(i.tier, Tier::Interp) || i.depth >= MAX_EVAL_DEPTH || i.multi_realm() {
        return Err(arg);
    }
    let key = Gc::as_ptr(o) as usize;
    if !i.proxies.is_empty() && i.proxies.contains_key(&key) {
        return Err(arg);
    }
    let (chunk, env, strict, arrow) = {
        let b = o.borrow();
        let Callable::User(u) = &b.call else { return Err(arg) };
        let f = &u.func;
        let Some(Some(chunk)) = f.code.get() else { return Err(arg) };
        if f.is_generator
            || f.is_async
            || (!i.class_info.is_empty() && i.class_info.contains_key(&key))
            || u.env.borrow().under_with
            || chunk.activation_layout.is_some()
            || chunk.arguments_slot.is_some()
            || chunk.rest_slot.is_some()
            || (chunk.reflect_args && reflect::enabled())
            || !crate::bytecode::jit::direct_ready(chunk)
        {
            return Err(arg);
        }
        (chunk.clone(), u.env.clone(), f.is_strict, f.is_arrow)
    };
    i.depth += 1;
    if let Err(e) = i.gc_check_amortized() {
        i.depth -= 1;
        return Ok(Err(e));
    }
    // SAFETY: a live compiled function (the checks above are `resolve`'s).
    let r = unsafe {
        crate::bytecode::jit::call_direct(i, &chunk, &env, key, strict, arrow, &Value::Undefined, |s| {
            if s.wants() {
                s.push(arg);
            }
        })
    };
    Ok(finish_call(i, r))
}

fn resolve(i: &mut Interp, callee: &Value) -> Kind {
    let Value::Obj(o) = callee else {
        return Kind::Generic;
    };
    if matches!(i.tier, Tier::Interp) {
        return Kind::Generic;
    }
    let key = Gc::as_ptr(o) as usize;
    if !i.proxies.is_empty() && i.proxies.contains_key(&key) {
        return Kind::Generic;
    }
    let b = o.borrow();
    match &b.call {
        Callable::Native(f) => {
            // A realm's `eval` has its own dispatch (see `call_dispatch`).
            if i.eval_realm_fns.contains(&key) {
                return Kind::Generic;
            }
            Kind::Native(*f)
        }
        Callable::User(u) => {
            let f = &u.func;
            let Some(Some(chunk)) = f.code.get() else {
                return Kind::Generic;
            };
            if f.is_generator
                || f.is_async
                || (!i.class_info.is_empty() && i.class_info.contains_key(&key))
                || u.env.borrow().under_with
                || chunk.activation_layout.is_some()
                || chunk.arguments_slot.is_some()
                || chunk.rest_slot.is_some()
            {
                return Kind::Generic;
            }
            let (slots, stack) = i.vm_pool.pop().unwrap_or_default();
            Kind::Compiled(Box::new(Compiled {
                chunk: chunk.clone(),
                env: u.env.clone(),
                fn_ptr: key,
                strict: f.is_strict,
                arrow: f.is_arrow,
                slots,
                stack,
                handlers: Vec::new(),
            }))
        }
        _ => Kind::Generic,
    }
}

/// `Interp::call` → `call_compiled` → `enter_frame` / [`drive_vm`] / `leave_frame` for a
/// [`Kind::Compiled`] callee, in the same order, on the prepared buffers — or, when the callee
/// has function code, as a direct call of it (`jit::call_direct`). `seed` writes the parameter
/// values (surplus ones are dropped).
#[inline]
fn call_compiled_prepared(
    i: &mut Interp,
    c: &mut Compiled,
    this: &Value,
    seed: impl FnOnce(&mut crate::bytecode::jit::Seed),
) -> Result<Value, Abrupt> {
    i.depth += 1;
    if let Err(e) = i.gc_check_amortized() {
        i.depth -= 1;
        return Err(e);
    }
    let chunk: &Chunk = &c.chunk;
    if crate::bytecode::jit::direct_ready(chunk) {
        // SAFETY: `c` is a live compiled function (see `resolve`).
        let r = unsafe {
            crate::bytecode::jit::call_direct(i, chunk, &c.env, c.fn_ptr, c.strict, c.arrow, this, seed)
        };
        return finish_call(i, r);
    }
    // call_dispatch: a plain call is never constructing, and clears new.target (an arrow
    // inherits it).
    let saved_ctor = std::mem::replace(&mut i.constructing, false);
    let saved_nt = if !c.arrow && !matches!(i.new_target, Value::Undefined) {
        Some(std::mem::replace(&mut i.new_target, Value::Undefined))
    } else {
        None
    };
    crate::bytecode::jit::sync_frames(i);
    i.fn_frames.push(FnFrame {
        fn_ptr: c.fn_ptr,
        coro: i.cur_coro,
        caller_site: std::mem::replace(&mut i.cur_site, crate::interpreter::frames::NO_SITE),
        strict: c.strict,
        construct: false,
        extra: None,
    });
    let this_val = if chunk.uses_this() {
        i.bind_compiled_this_flags(c.strict, chunk, this.clone(), false)
    } else {
        Value::Undefined
    };
    let saved_strict = std::mem::replace(&mut i.strict, c.strict);
    let saved_tco = std::mem::replace(&mut i.tco_ok, c.strict);
    let saved_field_init = i.in_field_init_code;
    let saved_agb = i.in_async_gen_body;
    if !c.arrow {
        i.in_field_init_code = false;
        i.in_async_gen_body = false;
    }
    let slots = &mut c.slots;
    slots.clear();
    slots.reserve(chunk.n_slots);
    // SAFETY: the seed writes at most `min(n_params, n_slots)` values into the reserved
    // capacity, which then become the vector's first elements.
    unsafe {
        let n = crate::bytecode::jit::seed_raw(slots.as_mut_ptr(), chunk, seed);
        slots.set_len(n);
    }
    while slots.len() < chunk.n_slots {
        slots.push(Value::Undefined);
    }
    for &s in &chunk.var_force_resets {
        slots[s as usize] = Value::Undefined;
    }
    let mut pc = 0usize;
    // Function code runs without the driver; the driver takes over when there is none or it
    // exited part-way (at `pc`, which is then past 0: the entry is not counted twice).
    let r = match super::jit::run_entered(i, chunk, &c.env, slots, &mut c.stack, &mut pc, &this_val)
    {
        Some(r) => r,
        None => drive_vm(
            i,
            chunk,
            &c.env,
            slots,
            &mut c.stack,
            &mut pc,
            &this_val,
            &mut c.handlers,
            None,
        ),
    };
    // leave_frame
    clear_values_fast(&mut c.slots);
    clear_values_fast(&mut c.stack);
    c.handlers.clear();
    drop_value_fast(this_val);
    i.strict = saved_strict;
    i.tco_ok = saved_tco;
    i.in_field_init_code = saved_field_init;
    i.in_async_gen_body = saved_agb;
    if let Some(f) = i.fn_frames.pop() {
        i.cur_site = f.caller_site;
    }
    i.constructing = saved_ctor;
    if let Some(nt) = saved_nt {
        drop_value_fast(std::mem::replace(&mut i.new_target, nt));
    }
    let r = match r {
        Ok(VmStep::Done(v)) => Ok(v),
        Ok(VmStep::Await(_)) => unreachable!("a synchronous bytecode function cannot await"),
        Err(e) => Err(e),
    };
    finish_call(i, r)
}

/// What can change between calls and send a [`Kind::Compiled`] call down the ordinary path:
/// the recursion limit, a second realm, `f.arguments` reflection being switched on (the
/// ordinary path records the arguments for it).
#[inline(always)]
fn compiled_ok(i: &Interp, c: &Compiled) -> bool {
    i.depth < MAX_EVAL_DEPTH && !i.multi_realm() && !(c.chunk.reflect_args && reflect::enabled())
}

/// `Interp::call` → `call_dispatch` → `dispatch_native` for a [`Kind::Native`] callee.
#[inline(never)]
fn call_native_prepared(
    i: &mut Interp,
    f: NativeFn,
    this: &Value,
    args: &[Value],
) -> Result<Value, Abrupt> {
    i.depth += 1;
    if let Err(e) = i.gc_check_amortized() {
        i.depth -= 1;
        return Err(e);
    }
    let saved_ctor = std::mem::replace(&mut i.constructing, false);
    let saved_nt = std::mem::replace(&mut i.new_target, Value::Undefined);
    // The builtin-caller tag and frame record (see `Interp::dispatch_native`); the prepared
    // receiver outlives the call.
    let site = i.cur_site;
    let ctx = crate::interpreter::frames::NativeCtx {
        prev: i.native_top,
        site,
        construct: false,
        hidden: std::cell::Cell::new(false),
        id: f as usize,
        recv_kind: crate::interpreter::frames::recv_kind(this),
        this,
    };
    i.native_top = &ctx as *const crate::interpreter::frames::NativeCtx as usize;
    i.cur_site = site | crate::interpreter::frames::SITE_NATIVE;
    let r = f(i, this.clone(), args).map_err(Abrupt::Throw);
    i.cur_site = site;
    i.native_top = ctx.prev;
    i.constructing = saved_ctor;
    drop_value_fast(std::mem::replace(&mut i.new_target, saved_nt));
    finish_call(i, r)
}

/// `Interp::call`'s tail: run a pending proper tail call out of the callee, then give back the
/// recursion depth.
#[inline(always)]
fn finish_call(i: &mut Interp, mut r: Result<Value, Abrupt>) -> Result<Value, Abrupt> {
    while r.is_ok() {
        match i.pending_tail.take() {
            Some(bx) => {
                let (f, t, a) = *bx;
                if let Err(e) = i.gc_check_amortized() {
                    r = Err(e);
                    break;
                }
                // A strict tail caller: hidden from `fn.caller` (see `bytecode::tail_leave`).
                let site = i.cur_site;
                i.cur_site = site | crate::interpreter::frames::SITE_NATIVE;
                r = i.call_inner(f, t, &a);
                i.cur_site = site;
            }
            None => break,
        }
    }
    i.depth -= 1;
    r
}
