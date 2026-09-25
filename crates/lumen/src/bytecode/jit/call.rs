//! Direct calls: a call site whose callee the planner resolved to one compiled JS function
//! calls that function's native code straight from native code.
//!
//! # The site
//!
//! The callee part is checked by identity against the resolved function — at the name load
//! that produces it (`LoadName` / `LoadNameForCall`, through [`Helper::NameWord`]; a mismatch
//! exits before the load and the site is left out of the next compile), or at the call itself
//! for any other producer (a local, a `GetMethod`). The call then runs the bookkeeping of an
//! ordinary call inline — the recursion depth, the amortized GC poll, the `fn_frames` record,
//! the engine flags (`strict`, `tco_ok`, `constructing`) — and anything unusual (a different
//! callee, no function code yet, `new.target` set, the depth limit, a GC poll due, a full
//! `fn_frames` buffer, reflection enabled for a sloppy callee, a primitive receiver for a sloppy
//! method, a full shadow stack) takes the ordinary [`Helper::Call`] path instead, which does
//! exactly what the interpreter does (including the `RangeError` at the depth limit).
//!
//! # The callee frame
//!
//! The callee's [`JitFrame`], slots and operand-stack area live on the per-thread shadow stack
//! ([`super::shadow`]): `[call record | header | slots | stack area]`, [`frame_bytes`] long. The arguments
//! are moved into the slots (unboxed ones stored, Boxed ones moved with the caller's copy left
//! trivially droppable, borrowed ones cloned), missing ones and the other slots are
//! `undefined`. The code is entered through the callee chunk's direct view
//! (`ChunkJit::dentry`, 0 while it has no function code) or, for recursion, by a direct call of
//! the code being compiled ([`SELF_ID`]). A return exit is completed inline (the result moved to
//! the caller's stack, the callee's slots dropped); any other exit — an entry-guard failure, a
//! deopt, a throw, a pending proper tail call — goes to [`Helper::CallFinish`], where the
//! interpreter finishes the call on the same slots. The frame never needs materializing for
//! anything else: the GC sees the values it holds as external references like any Rust-side
//! `Value`, and error stacks and `f.caller` read the `fn_frames` record.

use super::*;
use crate::bytecode::Op;
use lumen_codegen::{BinaryOp, IntCC, MemKind, Signature, Type, Value as V};
use std::sync::OnceLock;

/// Slots a directly called function may have (the site stores each one).
const MAX_DIRECT_SLOTS: usize = 64;

/// Callee frames up to this many slots are released inline after a direct call's return;
/// larger ones through [`Helper::DropN`].
const DROP_INLINE_SLOTS: usize = 6;

/// A direct call site (see the module docs).
#[derive(Clone, Debug)]
pub(super) struct DSite {
    /// The callee's payload word, as a `Value` stores it (0: bound by the first call).
    pub word: usize,
    /// The callee's `Gc::as_ptr` identity (with `word`; 0: bound by the first call).
    pub fn_ptr: usize,
    pub chunk: usize,
    pub consts: usize,
    /// The site's [`SiteCell`] (the callee's identity, `fn_frames` record and environment).
    pub cell: usize,
    /// The address of the pinned callee `Value` (an [`Src::Pin`] entry).
    pub pin: usize,
    pub strict: bool,
    pub arrow: bool,
    pub uses_this: bool,
    /// The callee records its arguments when reflection is enabled (see `bytecode::reflect`).
    pub reflect: bool,
    /// The callee's rest parameter slot (seeded with the surplus arguments).
    pub rest: Option<u16>,
    /// The callee's virtual `arguments` / rest object: its slot, seeded with the element
    /// count past the base (see `Chunk::virt_base`; the site passes at most `n_params`).
    pub virt: Option<(u16, u16)>,
    pub n_slots: usize,
    pub n_params: usize,
    /// A recursive call of the code being compiled.
    pub self_call: bool,
    /// The callee has proper tail calls (`Op::TailCall*`): only then can it return with
    /// `Interp::pending_tail` set.
    pub tails: bool,
    /// A proper tail call (`Op::TailCall`): taken directly within `TAIL_NEST` frames only,
    /// else by [`Helper::Call`] with `CALL_TAIL` (which parks it past `TAIL_NEST`).
    pub tail: bool,
    /// A `new` site: the callee is a plain (non-class) function constructor, identified exactly.
    pub construct: bool,
    /// A `GetMethod` producer validated inline: the receiver's and prototypes' shape ids down
    /// to the holder, and the holder's entry slot (see [`layout::method_probe`]).
    pub method: Option<(Vec<u32>, u32)>,
    /// A call adaptor the site sees through (see [`Adaptor`]).
    pub adaptor: Adaptor,
}

/// A call adaptor a direct site calls through to its target: the adaptor itself is checked by
/// identity at the call (a mismatch takes the ordinary path, which runs it). Neither has a
/// stack-trace frame of its own (`reflect::native_transparent`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Adaptor {
    None,
    /// `F.call(t, ...args)` (`<F> GetMethod(call) <t> <args> CallWithThis(1 + n)`): the
    /// payload word of `Function.prototype.call` and the address of its pinned `Value`. The
    /// site's callee is `F`, the receiver's entry; `t` is its `this` and the arguments follow
    /// it. With a method chain the `GetMethod` validates the adaptor inline (see
    /// [`Tr::direct_method`]).
    Call(usize, usize),
    /// `F.apply(t)` / `F.apply(t, list)`, as `Call` (with `Function.prototype.apply`): the
    /// list, `undefined`, `null` or a plain Array of data elements ([`Helper::ApplyOk`]), is
    /// copied into the parameter slots ([`Helper::ApplySeed`]).
    Apply(usize, usize),
    /// A bound function without bound arguments whose target is the site's callee (or another
    /// closure of its code), checked by [`Helper::BoundThis`], which also finds its `this`.
    Bound,
}

/// `f` is the native function `nf`.
fn is_native(f: &Value, nf: crate::value::NativeFn) -> bool {
    let Value::Obj(o) = f else { return false };
    let Ok(b) = o.try_borrow() else { return false };
    matches!(&b.call, crate::value::Callable::Native(fp) if *fp as usize == nf as usize)
}

/// A bound function without bound arguments: its target and bound `this`.
fn bound_parts(f: &Value) -> Option<(Value, Value)> {
    let Value::Obj(o) = f else { return None };
    let b = o.try_borrow().ok()?;
    match &b.call {
        crate::value::Callable::Bound(bc) if bc.args.is_empty() => {
            Some((Value::Obj(bc.target.clone()), bc.this.clone()))
        }
        _ => None,
    }
}

/// Byte offsets of the engine state a direct call site touches.
struct Offs {
    depth: i32,
    strict: i32,
    tco: i32,
    ctor: i32,
    new_target: i32,
    field_init: i32,
    agb: i32,
    gc_tick: i32,
    terminating: i32,
    cur_coro: i32,
    pending_tail: i32,
    /// `Interp::jit_frames` (the pending call records, see `super::sync_frames`).
    jit_frames: i32,
    /// `Interp::cur_site` (stack-trace positions).
    cur_site: i32,
}

fn offs() -> Option<&'static Offs> {
    use crate::interpreter::Interp as I;
    use std::mem::offset_of;
    static OFFS: OnceLock<Option<Offs>> = OnceLock::new();
    OFFS.get_or_init(|| {
        let i = |x: usize| i32::try_from(x).ok();
        Some(Offs {
            depth: i(offset_of!(I, depth))?,
            strict: i(offset_of!(I, strict))?,
            tco: i(offset_of!(I, tco_ok))?,
            ctor: i(offset_of!(I, constructing))?,
            new_target: i(offset_of!(I, new_target))?,
            field_init: i(offset_of!(I, in_field_init_code))?,
            agb: i(offset_of!(I, in_async_gen_body))?,
            gc_tick: i(offset_of!(I, gc_tick))?,
            terminating: i(offset_of!(I, terminating))?,
            cur_coro: i(offset_of!(I, cur_coro))?,
            pending_tail: i(offset_of!(I, pending_tail))?,
            jit_frames: i(offset_of!(I, jit_frames))?,
            cur_site: i(offset_of!(I, cur_site))?,
        })
    })
    .as_ref()
}

/// Where a name cache's scope-mode resolution lives (see `Chunk::name_ic_hit`), for the
/// inline check at direct sites' name loads. Offsets relative to the `RefCell<Scope>` unless
/// noted; measured on this target.
struct ScopeOffs {
    /// `Rc::as_ptr(env)` minus the `Env`'s stored pointer word.
    rc_value: i64,
    /// The cell's borrow counter (an `isize`).
    borrow: i32,
    /// `scope.vars`' generation (a `u32`).
    gen: i32,
    /// `NameIc` fields, relative to the cache cell.
    ic_env: i32,
    ic_binding: i32,
    ic_gen: i32,
    /// `Binding` fields.
    b_value: i32,
    b_init: i32,
}

fn scope_offs() -> Option<&'static ScopeOffs> {
    use crate::interpreter::{Binding, Scope, VarMap};
    use std::mem::{offset_of, size_of};
    static OFFS: OnceLock<Option<ScopeOffs>> = OnceLock::new();
    OFFS.get_or_init(|| {
        let i = |x: usize| i32::try_from(x).ok();
        // The generation's offset inside a `VarMap`: the one u32 that tracks it across
        // structural changes.
        let gen_off = {
            let mut m = VarMap::default();
            let read = |m: &VarMap, off: usize| unsafe {
                (m as *const VarMap as *const u8).add(off).cast::<u32>().read_unaligned()
            };
            let n = size_of::<VarMap>();
            let mut cands: Vec<usize> = (0..=n.checked_sub(4)?)
                .step_by(4)
                .filter(|&o| read(&m, o) == m.generation())
                .collect();
            // Re-inserting one name bumps the generation but no length.
            for _ in 0..3 {
                m.insert("x", Binding::data(Value::Undefined, true, true));
                let g = m.generation();
                cands.retain(|&o| read(&m, o) == g);
            }
            match cands[..] {
                [o] => o,
                _ => return None,
            }
        };
        let env = crate::interpreter::new_scope(None);
        let word = unsafe { *(&env as *const crate::interpreter::Env as *const usize) };
        let cell = std::rc::Rc::as_ptr(&env) as usize;
        let value = env.as_ptr() as usize - cell;
        let size = size_of::<std::cell::RefCell<Scope>>();
        let w = size_of::<isize>();
        let read = |o: usize| unsafe { ((cell + o) as *const isize).read_unaligned() };
        let outside = |o: usize| o + w <= value || o >= value + size_of::<Scope>();
        let borrow = {
            let g = env.borrow_mut();
            let c: Vec<usize> = (0..=size - w)
                .step_by(w)
                .filter(|&o| outside(o) && read(o) == -1)
                .collect();
            drop(g);
            match c[..] {
                [o] if read(o) == 0 => o,
                _ => return None,
            }
        };
        let ic = offset_of!(crate::bytecode::NameIc, env);
        Some(ScopeOffs {
            rc_value: (cell as i64) - (word as i64),
            borrow: i(borrow)?,
            gen: i(value + offset_of!(Scope, vars) + gen_off)?,
            ic_env: i(ic)?,
            ic_binding: i(offset_of!(crate::bytecode::NameIc, binding))?,
            ic_gen: i(offset_of!(crate::bytecode::NameIc, gen))?,
            b_value: i(offset_of!(Binding, value))?,
            b_init: i(offset_of!(Binding, initialized))?,
        })
    })
    .as_ref()
}

/// A `Value::Undefined` at a fixed address (the `this` of calls without a receiver).
static UNDEF: [u64; 2] = [0, 0];

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LUMEN_NO_DIRECT").is_none())
}

fn inline_this_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LUMEN_NO_INLINE_THIS").is_none())
}

/// The value of property `name` on `recv` when finding it runs no JS: an own or inherited data
/// property along a chain of ordinary objects.
fn method_value(i: &Interp, recv: &Value, name: &str) -> Option<Value> {
    let Value::Obj(o) = recv else { return None };
    let mut cur = o.clone();
    for _ in 0..8 {
        if !i.ordinary_get_ptr(crate::value::Gc::as_ptr(&cur) as usize) {
            return None;
        }
        let next = {
            let b = cur.try_borrow().ok()?;
            if !matches!(b.exotic, crate::value::Exotic::None) {
                return None;
            }
            if let Some(slot) = b.props.slot_of(name) {
                let p = b.props.entry_at(slot)?;
                return (!p.accessor()).then(|| p.value());
            }
            b.proto.clone()?
        };
        cur = next;
    }
    None
}

/// The shapes of `recv`'s lookup chain down to the holder of `name` and the holder's slot, when
/// every level is an ordinary plain object (at most 4 levels, like the interpreter's IC).
fn method_chain(i: &Interp, recv: &Value, name: &str) -> Option<(Vec<u32>, u32)> {
    let Value::Obj(o) = recv else { return None };
    let mut cur = o.clone();
    let mut shapes = Vec::new();
    for _ in 0..4 {
        if !i.ordinary_get_ptr(crate::value::Gc::as_ptr(&cur) as usize) {
            return None;
        }
        let next = {
            let b = cur.try_borrow().ok()?;
            if !matches!(b.exotic, crate::value::Exotic::None) || !b.ic_plain.get() {
                return None;
            }
            // A receiver with a shape of its own (a dictionary-mode object) fails the guard
            // for every other object: not a method site (prototypes are fine, being shared).
            if shapes.is_empty() && !b.props.shape_is_shared() {
                return None;
            }
            shapes.push(b.props.shape());
            if let Some(slot) = b.props.slot_of(name) {
                let p = b.props.entry_at(slot)?;
                return (!p.accessor()).then_some((shapes, u32::try_from(slot).ok()?));
            }
            b.proto.clone()?
        };
        cur = next;
    }
    None
}

/// Feedback of a method site whose callee and lookup chain a compile resolved (see
/// `ChunkJit::site_fb`).
#[derive(Clone)]
struct MethodFb {
    /// Weak (as are the bodies' chunks): feedback must not keep functions alive.
    callee: crate::value::WeakGc,
    chain: (Vec<u32>, u32),
    body: Option<super::inline_this::ThisBody>,
}

/// A property read or store resolved to an accessor function called directly (see
/// [`Tr::accessor_call`]): the accessor is found along the receiver's lookup chain by shape
/// ids (as [`layout::accessor_probe`] checks), the call is a direct call site's.
#[derive(Clone)]
pub(super) struct AccSite {
    /// The receiver's and prototypes' shape ids down to the holder.
    pub shapes: Vec<u32>,
    /// The holder's entry slot of the accessor.
    pub slot: u32,
    /// The accessor function's payload word.
    pub word: usize,
    pub site: DSite,
}

/// Feedback of a property access resolved to an accessor call.
#[derive(Clone)]
struct AccFb {
    f: crate::value::WeakGc,
    shapes: Vec<u32>,
    slot: u32,
}

/// The getter (`set`: setter) `recv.<name>` resolves to along a chain of ordinary plain
/// objects of shared shapes (at most 4 levels): the shapes, the holder's slot and the function.
fn accessor_chain(i: &Interp, recv: &Value, name: &str, set: bool) -> Option<(Vec<u32>, u32, Value)> {
    let Value::Obj(o) = recv else { return None };
    let mut cur = o.clone();
    let mut shapes = Vec::new();
    for _ in 0..4 {
        if !i.ordinary_get_ptr(crate::value::Gc::as_ptr(&cur) as usize) {
            return None;
        }
        let next = {
            let b = cur.try_borrow().ok()?;
            if !matches!(b.exotic, crate::value::Exotic::None)
                || !b.ic_plain.get()
                || !b.props.shape_is_shared()
            {
                return None;
            }
            shapes.push(b.props.shape());
            if let Some(slot) = b.props.slot_of(name) {
                let p = b.props.entry_at(slot)?;
                if !p.accessor() {
                    return None;
                }
                let f = if set { p.setter() } else { p.getter() }?.clone();
                return matches!(f, Value::Obj(_))
                    .then(|| Some((shapes, u32::try_from(slot).ok()?, f)))
                    .flatten();
            }
            b.proto.clone()?
        };
        cur = next;
    }
    None
}

/// A direct call site for accessor `f` called with `nargs` arguments and a receiver.
fn accessor_dsite(p: &mut Plan, interp: &Interp, chunk: &Chunk, f: &Value, nargs: usize) -> Option<DSite> {
    let ic = crate::bytecode::inline_callee(interp, f)?;
    let (env_addr, fn_ptr) = super::callee_env_addr(f)?;
    let word = super::value_word(f);
    let cc_rc = ic.chunk.clone();
    let cc: &Chunk = &cc_rc;
    if cc.activation_layout.is_some()
        || (cc.arguments_slot.is_some() && cc.virt_base.is_none())
        || (cc.virt_base.is_some() && nargs > cc.n_params)
        || cc.derived
        || cc.n_slots > MAX_DIRECT_SLOTS
        || cc.var_force_resets.iter().any(|&s| (s as usize) < cc.n_params.min(nargs))
        || std::ptr::eq(cc, chunk)
    {
        return None;
    }
    let pin: Box<Value> = Box::new(f.clone());
    let cell: Box<SiteCell> = Box::new(SiteCell {
        word,
        fn_ptr,
        env: env_addr,
        chunk: cc as *const Chunk as usize,
        pin: f.clone(),
    });
    let site = DSite {
        word,
        fn_ptr,
        chunk: cc as *const Chunk as usize,
        consts: cc.consts.as_ptr() as usize,
        cell: &*cell as *const SiteCell as usize,
        pin: &*pin as *const Value as usize,
        strict: ic.strict,
        arrow: ic.arrow,
        uses_this: cc.uses_this(),
        reflect: cc.reflect_args,
        rest: cc.rest_slot.filter(|_| cc.virt_base.is_none()),
        virt: virt_site(cc),
        n_slots: cc.n_slots,
        n_params: cc.n_params,
        self_call: false,
        tails: cc
            .ops
            .iter()
            .any(|op| matches!(op, Op::TailCall(..) | Op::TailCallSpread(..))),
        tail: false,
        construct: false,
        method: None,
        adaptor: Adaptor::None,
    };
    p.boxes.push(pin);
    p.boxes.push(cell);
    p.boxes.push(Box::new(cc_rc.clone()));
    Some(site)
}

/// Plan an accessor call at `q` (a read with `set == false`, else a store) on `recv`.
#[allow(clippy::too_many_arguments)]
fn plan_accessor(
    p: &mut Plan,
    interp: &Interp,
    chunk: &Chunk,
    q: usize,
    recv: Option<&Value>,
    name: Option<&str>,
    set: bool,
) {
    let found = match (recv, name) {
        (Some(r), Some(nm)) => accessor_chain(interp, r, nm, set).inspect(|(shapes, slot, f)| {
            if let Value::Obj(o) = f {
                chunk.jit.fb_put(
                    q,
                    AccFb {
                        f: crate::value::Gc::downgrade(o),
                        shapes: shapes.clone(),
                        slot: *slot,
                    },
                );
            }
        }),
        _ => chunk
            .jit
            .fb_get::<AccFb>(q)
            .and_then(|fb| Some((fb.shapes, fb.slot, Value::Obj(fb.f.upgrade()?)))),
    };
    let Some((shapes, slot, f)) = found else { return };
    if let Some(site) = accessor_dsite(p, interp, chunk, &f, set as usize) {
        let word = super::value_word(&f);
        p.dacc.insert(q, AccSite { shapes, slot, word, site });
    }
}

/// Feedback of a property read resolved to an inlinable getter.
#[derive(Clone)]
struct GetterFb {
    getter: crate::value::WeakGc,
    g: super::inline_this::Getter,
}

/// Find the direct call sites of the region (see the module docs).
#[allow(clippy::too_many_arguments)]
pub(super) fn plan_direct(
    p: &mut Plan,
    interp: &Interp,
    env: &Env,
    chunk: &Chunk,
    header: usize,
    backedge: usize,
    slots: &[Value],
    this_val: &Value,
    an: &Analysis,
    kinds: &[Kind],
    func_mode: bool,
) {
    if !enabled() || offs().is_none() || interp.multi_realm() {
        return;
    }
    let ops = &chunk.ops;
    let lead = |pc: usize| an.leader[pc - header];
    // The op in the basic block of `q` that pushed what is stack index `t` just before `q`,
    // and which of its pushes it is.
    let producer = |q: usize, t: usize| -> Option<(usize, usize)> {
        let mut pc = q;
        while pc > header {
            if lead(pc) {
                return None;
            }
            pc -= 1;
            let d = an.depth[pc - header]?;
            let (pops, pushes) = chunk.jit_stack_effect(pc)?;
            let lo = d.checked_sub(pops)?;
            if t >= lo {
                return (t < lo + pushes).then_some((pc, t - lo));
            }
        }
        None
    };
    let name = |n: u32| chunk.names.get(n as usize).map(|x| &**x);
    for q in header..=backedge {
        let Some(d) = an.depth[q - header] else { continue };
        let (argc, wt, construct) = match ops[q] {
            Op::Call(a) => (a as usize, 0, false),
            Op::CallWithThis(a) => (a as usize, 1, false),
            Op::TailCall(a, t) => (a as usize, t as usize, false),
            Op::New(a) => (a as usize, 0, true),
            _ => continue,
        };
        if p.math.values().any(|s| s.call == q)
            || p.strm.values().any(|&c| c == q)
            || p.strn.values().any(|&(c, _)| c == q)
        {
            continue;
        }
        let Some(base) = d.checked_sub(argc + 1 + wt) else { continue };
        let ci = base + wt;
        let Some((pp, pos)) = producer(q, ci) else { continue };
        if p.math.contains_key(&pp) || (pp > 0 && p.math.contains_key(&(pp - 1))) {
            continue;
        }
        if p.dname.contains_key(&pp) || p.dmethod.contains_key(&pp) {
            continue;
        }
        // A producer whose inline validation failed is translated as usual (its callee is
        // then checked at the call).
        let failed = helpers::math_site_failed(chunk, pp);
        let mut adaptor = Adaptor::None;
        // The adaptor function (pinned: its word is compared).
        let mut adaptor_pin: Option<Value> = None;
        let mut method = None;
        let mut recv_now = None;
        let mut method_fb: Option<MethodFb> = None;
        let (callee, by_name) = match ops[pp] {
            Op::LoadNameForCall(n, c) if wt == 1 && pos == 1 => {
                (helpers::name_value(interp, chunk, env, n, c), !failed)
            }
            Op::LoadName(n, c) if wt == 0 && pos == 0 => {
                (helpers::name_value(interp, chunk, env, n, c), !failed)
            }
            Op::LoadLocal(s) if pos == 0 && kinds.get(s as usize) == Some(&Kind::Boxed) => {
                (slots.get(s as usize).cloned(), false)
            }
            Op::GetMethod(m, _) if wt == 1 && pos == 1 => {
                let recv = match producer(pp, base) {
                    Some((rp, 0)) => match ops[rp] {
                        Op::LoadLocal(s) if kinds.get(s as usize) == Some(&Kind::Boxed) => {
                            slots.get(s as usize).cloned()
                        }
                        Op::LoadThis => Some(this_val.clone()),
                        Op::LoadName(n, c) => helpers::name_value(interp, chunk, env, n, c),
                        _ => None,
                    },
                    _ => None,
                };
                let (f, chain) = match (&recv, name(m)) {
                    (Some(r), Some(nm)) => {
                        (method_value(interp, r, nm), method_chain(interp, r, nm))
                    }
                    _ => (None, None),
                };
                let fcall = f.as_ref().and_then(|f| {
                    if argc >= 1 && is_native(f, crate::builtins::nf_function_call) {
                        Some((f, Adaptor::Call(super::value_word(f), 0)))
                    } else if (1..=2).contains(&argc)
                        && is_native(f, crate::builtins::nf_function_apply)
                    {
                        Some((f, Adaptor::Apply(super::value_word(f), 0)))
                    } else {
                        None
                    }
                });
                if let (Some((fc, a)), Some(r @ Value::Obj(_))) = (fcall, &recv) {
                    adaptor = a;
                    adaptor_pin = Some(fc.clone());
                    method = chain.filter(|_| !failed);
                    (Some(r.clone()), false)
                } else {
                let fb = match recv {
                    Some(Value::Obj(_)) => None,
                    _ => chunk.jit.fb_get::<MethodFb>(pp),
                };
                if let Some((fb, f)) = fb.and_then(|fb| {
                    let f = fb.callee.upgrade()?;
                    Some((fb, Value::Obj(f)))
                }) {
                    // A receiver seen by an earlier compile: its callee, chain and body (the
                    // call guards the chain and the callee at run time).
                    method = (!failed).then(|| fb.chain.clone());
                    method_fb = Some(fb);
                    (Some(f), false)
                } else {
                    method = chain.filter(|_| !failed);
                    recv_now = recv;
                    (f, false)
                }
                }
            }
            _ => continue,
        };
        // A bound function calls its target.
        let (callee, by_name) = match callee.as_ref().and_then(bound_parts) {
            Some((target, _)) if !construct => {
                adaptor = Adaptor::Bound;
                (Some(target), false)
            }
            _ => (callee, by_name),
        };
        let nargs = match adaptor {
            Adaptor::Call(..) => argc - 1,
            // (Seeded at run time.)
            Adaptor::Apply(..) => 0,
            _ => argc,
        };
        // A constructor with a template: the instance is built inline (see `new_plan`).
        if construct && !failed && matches!(ops[pp], Op::LoadName(..) | Op::LoadLocal(_)) {
            if let Some(c) = callee.as_ref().filter(|c| matches!(c, Value::Obj(_))) {
                if let Some(site) = super::new_plan::plan_site(interp, c, argc, &mut p.boxes) {
                    if by_name {
                        p.dname.insert(pp, q);
                        p.name_ref.remove(&pp);
                        global_name(interp, chunk, pp, ops[pp], p);
                    }
                    p.dnew.insert(q, site);
                    continue;
                }
            }
        }
        // The callee now, or — for a local only ever assigned closures of one nested function
        // (whole-function code, compiled before the local is) — that function's code, bound
        // by the first call.
        let resolved = match callee {
            Some(callee @ Value::Obj(_)) => {
                let ic = if construct {
                    crate::bytecode::inline_ctor(interp, &callee)
                } else {
                    crate::bytecode::inline_callee(interp, &callee)
                };
                let Some(ic) = ic else {
                    continue;
                };
                let Some((env_addr, fn_ptr)) = super::callee_env_addr(&callee) else {
                    continue;
                };
                let word = super::value_word(&callee);
                (ic.chunk.clone(), ic.strict, ic.arrow, word, fn_ptr, env_addr, callee)
            }
            _ => {
                let t = match ops[pp] {
                    Op::LoadLocal(s) => closure_template(chunk, s, construct),
                    _ => None,
                };
                // (Feedback names only the code: a constructor must be known to be one.)
                let t = match t {
                    Some(t) => Some(t),
                    None if !construct => feedback(chunk, q).map(|(t, bound)| {
                        if bound {
                            adaptor = Adaptor::Bound;
                        }
                        t
                    }),
                    None => None,
                };
                let Some((cc, strict, arrow)) = t else {
                    continue;
                };
                (cc, strict, arrow, 0, 0, 0, Value::Undefined)
            }
        };
        let (cc_rc, strict, arrow, word, fn_ptr, env_addr, callee) = resolved;
        let callee_weak = callee.as_obj().map(crate::value::Gc::downgrade);
        let cc: &Chunk = &cc_rc;
        if cc.activation_layout.is_some()
            || (cc.arguments_slot.is_some() && cc.virt_base.is_none())
            || (cc.virt_base.is_some()
                && (nargs > cc.n_params || matches!(adaptor, Adaptor::Apply(..))))
            || cc.derived
            || cc.n_slots > MAX_DIRECT_SLOTS
            || (!strict && cc.uses_this() && wt == 0 && !construct && adaptor != Adaptor::Bound)
            || cc
                .var_force_resets
                .iter()
                .any(|&s| (s as usize) < cc.n_params.min(nargs))
            || (word == 0 && (by_name || method.is_some()))
            || (matches!(adaptor, Adaptor::Apply(..))
                && (cc.rest_slot.is_some() || !cc.var_force_resets.is_empty()))
        {
            continue;
        }
        let self_call =
            func_mode && std::ptr::eq(cc, chunk) && !cfg!(target_arch = "wasm32");
        if let Some(ap) = adaptor_pin {
            let ap = Box::new(ap);
            let at = &*ap as *const Value as usize;
            match adaptor {
                Adaptor::Call(w, _) => adaptor = Adaptor::Call(w, at),
                Adaptor::Apply(w, _) => adaptor = Adaptor::Apply(w, at),
                _ => {}
            }
            p.boxes.push(ap);
        }
        let pin: Box<Value> = Box::new(callee.clone());
        let cell: Box<SiteCell> = Box::new(SiteCell {
            word,
            fn_ptr,
            env: env_addr,
            chunk: cc as *const Chunk as usize,
            pin: callee.clone(),
        });
        let site = DSite {
            word,
            fn_ptr,
            chunk: cc as *const Chunk as usize,
            consts: cc.consts.as_ptr() as usize,
            cell: &*cell as *const SiteCell as usize,
            pin: &*pin as *const Value as usize,
            strict,
            arrow,
            uses_this: cc.uses_this(),
            reflect: cc.reflect_args,
            rest: cc.rest_slot.filter(|_| cc.virt_base.is_none()),
            virt: virt_site(cc),
            n_slots: cc.n_slots,
            n_params: cc.n_params,
            self_call,
            tails: cc
                .ops
                .iter()
                .any(|op| matches!(op, Op::TailCall(..) | Op::TailCallSpread(..))),
            tail: matches!(ops[q], Op::TailCall(..)),
            construct,
            method: method.clone(),
            adaptor,
        };
        // A self tail call of a function whose frame is its slots alone becomes a jump back
        // to pc 0 (see `Tr::self_tail`).
        if site.tail
            && self_call
            && site.rest.is_none()
            && site.virt.is_none()
            && !site.reflect
            && !site.uses_this
            && adaptor == Adaptor::None
            && an.hs[q - header].is_empty()
        {
            p.tail_loops.insert(q);
        }
        p.boxes.push(pin);
        p.boxes.push(cell);
        p.boxes.push(Box::new(cc_rc.clone()));
        if by_name {
            p.dname.insert(pp, q);
            p.name_ref.remove(&pp);
            global_name(interp, chunk, pp, ops[pp], p);
        }
        if method.is_some() {
            p.dmethod.insert(pp, q);
        }
        if let Some(chain) = &method {
            let body = match (&recv_now, &method_fb) {
                (Some(r), _) => super::inline_this::this_body(
                    &cc_rc,
                    r,
                    chain.0[0],
                    &super::inline_this::private_keys(interp, &callee),
                ),
                (None, Some(fb)) => fb
                    .body
                    .clone()
                    .filter(|b| std::ptr::eq(b.chunk.as_ptr(), std::rc::Rc::as_ptr(&cc_rc))),
                _ => None,
            };
            if let (Some(_), Some(f)) = (&recv_now, callee_weak.clone()) {
                chunk.jit.fb_put(
                    pp,
                    MethodFb {
                        callee: f,
                        chain: chain.clone(),
                        body: body.clone(),
                    },
                );
            }
            if !construct && !self_call && inline_this_on() {
                if let Some(mut tb) = body {
                    // Only loads between the `GetMethod` and the call: nothing ran.
                    tb.fresh = (pp + 1..q).all(|k| {
                        matches!(
                            ops[k],
                            Op::LoadLocal(_) | Op::Const(_) | Op::Undef | Op::LoadThis | Op::Dup
                        )
                    });
                    p.dthis.insert(q, tb);
                }
            }
        }
        if !construct && !matches!(adaptor, Adaptor::Call(..) | Adaptor::Apply(..)) {
            note_feedback(chunk, q, &cc_rc, strict, arrow, adaptor == Adaptor::Bound);
        }
        p.direct.insert(q, site);
    }
    // Property reads of a getter with an inlinable body.
    if !inline_this_on() {
        return;
    }
    for q in header..=backedge {
        if an.depth[q - header].is_none() {
            continue;
        }
        let (recv, n) = match ops[q] {
            Op::GetPropThis(n, _) => (Some(this_val.clone()), n),
            Op::GetPropLocal(s, n, _) if kinds.get(s as usize) == Some(&Kind::Boxed) => {
                (slots.get(s as usize).cloned(), n)
            }
            Op::GetProp(n, _) => {
                let Some(d) = an.depth[q - header] else { continue };
                let r = match d.checked_sub(1).and_then(|t| producer(q, t)) {
                    Some((rp, 0)) => match ops[rp] {
                        Op::LoadLocal(s) if kinds.get(s as usize) == Some(&Kind::Boxed) => {
                            slots.get(s as usize).cloned()
                        }
                        Op::LoadThis => Some(this_val.clone()),
                        Op::LoadName(n, c) => helpers::name_value(interp, chunk, env, n, c),
                        _ => None,
                    },
                    _ => None,
                };
                (r, n)
            }
            _ => continue,
        };
        // Not an object yet (a compile at function entry sees unset locals): use feedback.
        let recv = recv.filter(|r| matches!(r, Value::Obj(_)));
        let recv2 = recv.clone();
        let found = match (recv, name(n)) {
            (Some(recv), Some(nm)) => {
                let r = super::inline_this::getter_site(interp, &recv, nm);
                if let Some((g, Value::Obj(f))) = &r {
                    chunk.jit.fb_put(
                        q,
                        GetterFb {
                            getter: crate::value::Gc::downgrade(f),
                            g: g.clone(),
                        },
                    );
                }
                r
            }
            // A receiver seen by an earlier compile (the read guards the chain and the getter).
            _ => chunk.jit.fb_get::<GetterFb>(q).and_then(|fb| {
                let f = Value::Obj(fb.getter.upgrade()?);
                (super::value_word(&f) == fb.g.getter && fb.g.body.chunk.strong_count() > 0)
                    .then_some((fb.g, f))
            }),
        };
        if let Some((g, f)) = found {
            p.pins.push(f);
            p.dget.insert(q, g);
        } else {
            plan_accessor(p, interp, chunk, q, recv2.as_ref(), name(n), false);
        }
    }
    // Property stores through a setter with an inlinable body.
    for q in header..=backedge {
        let Some(d) = an.depth[q - header] else { continue };
        let (recv, n) = match ops[q] {
            Op::SetPropThisDrop(n, _) => (Some(this_val.clone()), n),
            Op::SetPropLocalDrop(s, n, _) if kinds.get(s as usize) == Some(&Kind::Boxed) => {
                (slots.get(s as usize).cloned(), n)
            }
            Op::SetPropDrop(n, _) => {
                let r = match d.checked_sub(2).and_then(|t| producer(q, t)) {
                    Some((rp, 0)) => match ops[rp] {
                        Op::LoadLocal(s) if kinds.get(s as usize) == Some(&Kind::Boxed) => {
                            slots.get(s as usize).cloned()
                        }
                        Op::LoadThis => Some(this_val.clone()),
                        Op::LoadName(n, c) => helpers::name_value(interp, chunk, env, n, c),
                        _ => None,
                    },
                    _ => None,
                };
                (r, n)
            }
            _ => continue,
        };
        let recv = recv.filter(|r| matches!(r, Value::Obj(_)));
        let recv2 = recv.clone();
        let found = match (recv, name(n)) {
            (Some(recv), Some(nm)) => {
                let r = super::inline_this::setter_site(interp, &recv, nm);
                if let Some((g, Value::Obj(f))) = &r {
                    chunk.jit.fb_put(
                        q,
                        GetterFb {
                            getter: crate::value::Gc::downgrade(f),
                            g: g.clone(),
                        },
                    );
                }
                r
            }
            _ => chunk.jit.fb_get::<GetterFb>(q).and_then(|fb| {
                let f = Value::Obj(fb.getter.upgrade()?);
                (super::value_word(&f) == fb.g.getter && fb.g.body.chunk.strong_count() > 0)
                    .then_some((fb.g, f))
            }),
        };
        if let Some((g, f)) = found {
            p.pins.push(f);
            p.dset.insert(q, g);
        } else {
            plan_accessor(p, interp, chunk, q, recv2.as_ref(), name(n), true);
        }
    }
}

/// `(code, strict, arrow, through a bound function)` by `(chunk, call pc)`.
type Feedback =
    std::collections::HashMap<(usize, usize), (std::rc::Weak<Chunk>, bool, bool, bool)>;

thread_local! {
    /// The code each direct call site was resolved to, by `(chunk, call pc)`: a later compile
    /// of the chunk that cannot resolve the callee (whole-function code entered before the
    /// locals hold it) keeps the site direct, bound by its first call.
    static FEEDBACK: std::cell::RefCell<Feedback> = Default::default();
}

fn note_feedback(
    chunk: &Chunk,
    q: usize,
    cc: &std::rc::Rc<Chunk>,
    strict: bool,
    arrow: bool,
    bound: bool,
) {
    FEEDBACK.with(|f| {
        let mut f = f.borrow_mut();
        if f.len() < 1 << 16 {
            f.insert(
                (chunk as *const Chunk as usize, q),
                (std::rc::Rc::downgrade(cc), strict, arrow, bound),
            );
        }
    });
}

fn feedback(chunk: &Chunk, q: usize) -> Option<((std::rc::Rc<Chunk>, bool, bool), bool)> {
    FEEDBACK.with(|f| {
        let f = f.borrow();
        let (w, strict, arrow, bound) = f.get(&(chunk as *const Chunk as usize, q))?;
        Some(((w.upgrade()?, *strict, *arrow), *bound))
    })
}

/// The compiled code and `(strict, arrow)` of the one nested function whose closures are the
/// only values local `s` is ever assigned (`MakeClosure(k) StoreLocal(s)` in one basic block;
/// the local is otherwise only read, or put in its TDZ).
/// For a constructor (`ctor`) the function must be one: not an arrow or a method.
fn closure_template(
    chunk: &Chunk,
    s: u16,
    ctor: bool,
) -> Option<(std::rc::Rc<Chunk>, bool, bool)> {
    if (s as usize) < chunk.n_params
        || chunk.arguments_slot == Some(s)
        || chunk.rest_slot == Some(s)
    {
        return None;
    }
    let ops = &chunk.ops;
    let mut k = None;
    for (pc, op) in ops.iter().enumerate() {
        if !build::op_slots(op).contains(&s) {
            continue;
        }
        match *op {
            Op::LoadLocal(_) | Op::Tdz(_) => {}
            Op::StoreLocal(_) => {
                let Some(&Op::MakeClosure(f, _)) = pc.checked_sub(1).and_then(|q| ops.get(q))
                else {
                    return None;
                };
                // Nothing may jump between the two.
                if ops.iter().any(|o| crate::jit_ir::jump_target(o) == Some(pc)) {
                    return None;
                }
                match k {
                    None => k = Some(f),
                    Some(g) if g == f => {}
                    Some(_) => return None,
                }
            }
            _ => return None,
        }
    }
    let func = chunk.funcs.get(k? as usize)?;
    if func.is_generator || func.is_async || (ctor && (func.is_arrow || func.is_method)) {
        return None;
    }
    let Some(Some(cc)) = func.code.get() else {
        return None;
    };
    Some((cc.clone(), func.is_strict, func.is_arrow))
}

impl Tr<'_, '_> {
    /// Continue in a fresh block when `ok` (I32) is nonzero, else branch to `miss`.
    pub(super) fn guard_to(&mut self, ok: V, miss: Block) {
        let next = self.fb.create_block();
        self.fb.brif(ok, next, &[], miss, &[]);
        self.fb.seal_block(next);
        self.fb.switch_to_block(next);
    }

    /// A bitwise move of the `Value` at `src + so` to `dst + off`: its tag, Boolean and payload
    /// fields — each loaded with the width it was most likely stored with (the tag and a
    /// Boolean are byte stores in native code), so the loads forward from in-flight stores.
    pub(super) fn copy_value(&mut self, src: V, so: i32, dst: V, off: i32) {
        let t = self.fb.load(MemKind::I32U8, src, so);
        let b = self.fb.load(MemKind::I32U8, src, so + VALUE_BOOL);
        let w = self.fb.load(MemKind::I64, src, so + VALUE_PAYLOAD);
        self.fb.store(MemKind::I32U8, dst, t, off);
        self.fb.store(MemKind::I32U8, dst, b, off + VALUE_BOOL);
        self.fb.store(MemKind::I64, dst, w, off + VALUE_PAYLOAD);
    }

    pub(super) fn i64c(&mut self, v: i64) -> V {
        self.fb.iconst(Type::I64, v)
    }

    /// The name load producing a direct site's callee: check it still resolves to the
    /// function (exit before the load otherwise, and leave the site out of the next compile),
    /// then push the pinned callee (and `LoadNameForCall`'s undefined receiver).
    pub(super) fn direct_name(&mut self, pc: usize, n: u32, c: u32, lfc: bool) {
        let q = self.plan.dname[&pc];
        let (word, pin) = match self.plan.direct.get(&q) {
            Some(s) => (s.word, s.pin),
            None => {
                let s = &self.plan.dnew[&q];
                (s.word, s.pin)
            }
        };
        let d = self.stack.len();
        let cont = self.fb.create_block();
        let full = self.fb.create_block();
        let glob = self.plan.dglobal.get(&pc).copied();
        let miss1 = if glob.is_some() { self.fb.create_block() } else { full };
        // The name cache's scope mode, inline: the cache names this frame's scope, the scope's
        // map is structurally unchanged since the fill (so the binding pointer is live), and
        // the binding holds the function.
        match (scope_offs(), self.chunk.name_caches.get(c as usize)) {
            (Some(so), Some(cell)) if !cfg!(target_arch = "wasm32") => {
                let icp = self.ptrc(cell as *const _ as usize as i64);
                let ic_env = self.fb.load(PTR_MEM, icp, so.ic_env);
                let envp = self.fb.load(PTR_MEM, self.frame, FRAME_ENV);
                let rc = self.fb.load(PTR_MEM, envp, 0);
                let off = self.ptrc(so.rc_value);
                let scope = self.fb.binary(BinaryOp::Iadd, rc, off);
                let ok = self.fb.icmp(IntCC::Eq, ic_env, scope);
                self.guard_to(ok, miss1);
                let flag = self.fb.load(PTR_MEM, scope, so.borrow);
                let z = self.ptrc(0);
                let ok = self.fb.icmp(IntCC::Sge, flag, z);
                self.guard_to(ok, miss1);
                let g = self.fb.load(MemKind::I32, scope, so.gen);
                let icg = self.fb.load(MemKind::I32, icp, so.ic_gen);
                let ok = self.fb.icmp(IntCC::Eq, g, icg);
                self.guard_to(ok, miss1);
                let bd = self.fb.load(PTR_MEM, icp, so.ic_binding);
                let init = self.fb.load(MemKind::I32U8, bd, so.b_init);
                let z32 = self.i32c(0);
                let ok = self.fb.icmp(IntCC::Ne, init, z32);
                self.guard_to(ok, miss1);
                self.check_callee(bd, so.b_value, word, miss1);
                self.fb.jump(cont, &[]);
            }
            _ => self.fb.jump(miss1, &[]),
        }
        if let (Some(so), Some(cell), Some((gp, shape, slot))) =
            (scope_offs(), self.chunk.name_caches.get(c as usize), glob)
        {
            // The name cache's global-object mode: the frame's scope is the (global) scope the
            // cache names, still without the name (its generation), and the global object's
            // entry holds the function.
            self.fb.seal_block(miss1);
            self.fb.switch_to_block(miss1);
            let icp = self.ptrc(cell as *const _ as usize as i64);
            let ic_env = self.fb.load(PTR_MEM, icp, so.ic_env);
            let envp = self.fb.load(PTR_MEM, self.frame, FRAME_ENV);
            let rc = self.fb.load(PTR_MEM, envp, 0);
            let off = self.ptrc(so.rc_value + 1);
            let tagged = self.fb.binary(BinaryOp::Iadd, rc, off);
            let ok = self.fb.icmp(IntCC::Eq, ic_env, tagged);
            self.guard_to(ok, full);
            let off = self.ptrc(so.rc_value);
            let scope = self.fb.binary(BinaryOp::Iadd, rc, off);
            let flag = self.fb.load(PTR_MEM, scope, so.borrow);
            let z = self.ptrc(0);
            let ok = self.fb.icmp(IntCC::Sge, flag, z);
            self.guard_to(ok, full);
            let g = self.fb.load(MemKind::I32, scope, so.gen);
            let icg = self.fb.load(MemKind::I32, icp, so.ic_gen);
            let ok = self.fb.icmp(IntCC::Eq, g, icg);
            self.guard_to(ok, full);
            let gv = self.ptrc(gp as i64);
            let want = crate::value::PACK_OBJ | word as u64;
            layout::gc_probe(&mut self.fb, gv, &[shape], slot, want, full);
            self.fb.jump(cont, &[]);
        }
        self.fb.seal_block(full);
        self.fb.switch_to_block(full);
        let (nv, cv) = (self.i32c(n as i64), self.i32c(c as i64));
        let w = self
            .call(Helper::NameWord, &[self.frame, nv, cv])
            .expect("NameWord returns a word");
        let want = self.ptrc(word as i64);
        let ok = self.fb.icmp(IntCC::Eq, w, want);
        let bad = self.fb.create_block();
        self.fb.brif(ok, cont, &[], bad, &[]);
        self.fb.seal_block(bad);
        self.fb.switch_to_block(bad);
        let pcv = self.i32c(pc as i64);
        self.call(Helper::SiteFailed, &[self.frame, pcv]);
        let st = self.stack.clone();
        self.emit_exit(pc, EXIT_RESUME, &st, d, None);
        self.fb.seal_block(cont);
        self.fb.switch_to_block(cont);
        if lfc {
            self.set_stack_tag(d, TAG_UNDEFINED);
            self.stack.push(Entry::Boxed);
        }
        let p = self.ptrc(pin as i64);
        self.stack.push(Entry::Ref(p, Src::Pin));
    }

    /// The `GetMethod` producing a direct site's callee: validate the lookup inline (exit
    /// before the op otherwise, and leave the site out of the next compile) and push the pinned
    /// method, leaving the receiver entry as it is. `false` when the receiver entry is unboxed
    /// (the op is translated as usual).
    pub(super) fn direct_method(&mut self, pc: usize) -> bool {
        let q = self.plan.dmethod[&pc];
        let (word, pin, method) = {
            let s = &self.plan.direct[&q];
            match s.adaptor {
                // The method is the adaptor.
                Adaptor::Call(w, pin) | Adaptor::Apply(w, pin) => (w, pin, s.method.clone()),
                _ => (s.word, s.pin, s.method.clone()),
            }
        };
        let Some((shapes, slot)) = method else { return false };
        let d = self.stack.len();
        let recv = match self.stack[d - 1] {
            Entry::Ref(p, _) => p,
            Entry::Boxed => self.sptr(d - 1),
            Entry::Num(_) | Entry::Bool(_) => return false,
        };
        let miss = self.fb.create_block();
        let want = crate::value::PACK_OBJ | word as u64;
        layout::method_probe(&mut self.fb, recv, &shapes, slot, want, miss);
        let cont = self.fb.create_block();
        self.fb.jump(cont, &[]);
        self.fb.seal_block(miss);
        self.fb.switch_to_block(miss);
        let pcv = self.i32c(pc as i64);
        self.call(Helper::SiteFailed, &[self.frame, pcv]);
        let st = self.stack.clone();
        self.emit_exit(pc, EXIT_RESUME, &st, d, None);
        self.fb.seal_block(cont);
        self.fb.switch_to_block(cont);
        let p = self.ptrc(pin as i64);
        self.stack.push(Entry::Ref(p, Src::Pin));
        true
    }

    /// Branch to `miss` unless the `Value` at `addr + off` is the callee cached in the site's
    /// cell — or another closure of the same function, which [`Helper::SiteRebind`] then caches.
    fn check_cell(&mut self, addr: V, off: i32, cell: usize, miss: Block) {
        let tag = self.fb.load(MemKind::I32U8, addr, off);
        let obj = self.i32c(TAG_OBJ as i64);
        let is_obj = self.fb.icmp(IntCC::Eq, tag, obj);
        self.guard_to(is_obj, miss);
        let pl = self.fb.load(PTR_MEM, addr, off + VALUE_PAYLOAD);
        let cp = self.ptrc(cell as i64);
        let want = self.fb.load(PTR_MEM, cp, CELL_WORD);
        let same = self.fb.icmp(IntCC::Eq, pl, want);
        let ok = self.fb.create_block();
        let rebind = self.fb.create_block();
        self.fb.brif(same, ok, &[], rebind, &[]);
        self.fb.seal_block(rebind);
        self.fb.switch_to_block(rebind);
        let offc = self.ptrc(off as i64);
        let vp = self.fb.binary(BinaryOp::Iadd, addr, offc);
        let r = self
            .call(Helper::SiteRebind, &[self.frame, cp, vp])
            .expect("SiteRebind returns a flag");
        let z = self.i32c(0);
        let bound = self.fb.icmp(IntCC::Ne, r, z);
        self.fb.brif(bound, ok, &[], miss, &[]);
        self.fb.seal_block(ok);
        self.fb.switch_to_block(ok);
    }

    /// The engine flags a call sets up, read once at entry (`frame.canon`, see
    /// [`JitFrame::canon`]): they belong to the running activation, which every callee restores.
    /// When they are canonical for strictness `s` (the value is `1 + s`), a direct call of a
    /// callee of strictness `s` changes none of them (and has nothing to restore).
    pub(super) fn entry_flags(&mut self) -> Option<V> {
        offs()?;
        Some(self.fb.load(MemKind::I32U8, self.frame, FRAME_CANON))
    }

    /// Branch to `miss` unless the `Value` at `addr + off` is the callee `word`.
    pub(super) fn check_callee(&mut self, addr: V, off: i32, word: usize, miss: Block) {
        let tag = self.fb.load(MemKind::I32U8, addr, off);
        let obj = self.i32c(TAG_OBJ as i64);
        let is_obj = self.fb.icmp(IntCC::Eq, tag, obj);
        let pl = self.fb.load(PTR_MEM, addr, off + VALUE_PAYLOAD);
        let want = self.ptrc(word as i64);
        let same = self.fb.icmp(IntCC::Eq, pl, want);
        let ok = self.fb.binary(BinaryOp::Band, is_obj, same);
        self.guard_to(ok, miss);
    }

    /// Publish `pc` (a call op) as the running frame's call site, `Interp::cur_site`, before a
    /// helper that calls a function: the callee's `FnFrame` records it, and a stack trace maps
    /// it back to the call's source position (`bytecode::positions`). One immediate store —
    /// the only stack-trace work compiled code does.
    pub(super) fn set_call_site(&mut self, pc: usize) {
        let Some(o) = offs() else { return };
        let interp = self.fb.load(PTR_MEM, self.frame, FRAME_INTERP);
        let v = self.i32c((crate::interpreter::frames::SITE_PC | pc as u32) as i32 as i64);
        self.fb.store(MemKind::I32, interp, v, o.cur_site);
    }

    /// The call of an accessor site ([`AccSite`], validated by the caller): its function with
    /// `this` at `this_p` (borrowed, alive across the call) and the argument entry `arg`,
    /// pushed as a direct call's operands; the result is pushed (Boxed).
    pub(super) fn accessor_call(&mut self, pc: usize, this_p: V, arg: Option<Entry>) {
        let pin = self.plan.dacc[&pc].site.pin;
        self.stack.push(Entry::Ref(this_p, Src::Pin));
        let pv = self.ptrc(pin as i64);
        self.stack.push(Entry::Ref(pv, Src::Pin));
        if let Some(a) = arg {
            self.stack.push(a);
        }
        self.direct_call_full(pc, arg.is_some() as usize, true);
    }

    /// A direct call site's `Call` / `CallWithThis` (see the module docs): the inlined body of
    /// a small `this` method when there is one (see [`super::inline_this`]), else the call.
    pub(super) fn direct_call(&mut self, pc: usize, argc: usize, with_this: bool) {
        if let (true, Some(tb)) = (with_this, self.plan.dthis.get(&pc).cloned()) {
            if self.inline_this_call(pc, argc, &tb) {
                return;
            }
        }
        self.direct_call_full(pc, argc, with_this);
    }

    /// A self tail call planned as a loop (`Plan::tail_loops`): when the callee is the running
    /// function itself (same closure environment) in a plain activation, and the arguments fit
    /// the parameters' kinds, the arguments move into the parameter slots, every other slot is
    /// reset to `undefined` as at entry, and control jumps back to pc 0 — the frame is reused,
    /// as a proper tail call releases it. Anything else makes the direct call.
    pub(super) fn self_tail(&mut self, pc: usize, argc: usize, with_this: bool) {
        let site = self.plan.direct[&pc].clone();
        let wt = with_this as usize;
        let d = self.stack.len();
        let base = d - argc - 1 - wt;
        let ci = base + wt;
        let ab = ci + 1;
        // The arguments move into slots borrowed ones may point at.
        for k in ab..d {
            self.force(k);
        }
        let entries = self.stack.clone();
        let norm = self.fb.create_block();
        let z = self.i32c(0);
        // ---- guards: the running closure, called plainly ----
        match entries[ci] {
            Entry::Ref(_, Src::Pin) => {}
            Entry::Ref(p, _) => self.check_cell(p, 0, site.cell, norm),
            Entry::Boxed => {
                let off = self.soff(ci);
                let sp = self.stackp;
                self.check_cell(sp, off, site.cell, norm);
            }
            Entry::Num(_) | Entry::Bool(_) => self.guard_to(z, norm),
        }
        let cellp = self.ptrc(site.cell as i64);
        let ce = self.fb.load(PTR_MEM, cellp, CELL_ENV);
        let fe = self.fb.load(PTR_MEM, self.frame, FRAME_ENV);
        let ok = self.fb.icmp(IntCC::Eq, ce, fe);
        self.guard_to(ok, norm);
        match self.call_flags {
            Some(canon) => {
                let want = self.i32c(1 + site.strict as i64);
                let ok = self.fb.icmp(IntCC::Eq, canon, want);
                self.guard_to(ok, norm);
            }
            None => self.guard_to(z, norm),
        }
        // Parameters kept in SSA take arguments of their kind.
        let np = site.n_params.min(self.kinds.len());
        let mut vals: Vec<Option<V>> = vec![None; np];
        for k in 0..np {
            let (num, tag) = match self.kinds[k] {
                Kind::Num => (true, TAG_NUM),
                Kind::Bool => (false, TAG_BOOL),
                Kind::Boxed => continue,
            };
            let e = if k < argc { Some(entries[ab + k]) } else { None };
            let v = match e {
                Some(Entry::Num(x)) if num => x,
                Some(Entry::Bool(b)) if !num => b,
                Some(Entry::Boxed) => {
                    let t = self.stack_tag(ab + k);
                    let want = self.i32c(tag as i64);
                    let ok = self.fb.icmp(IntCC::Eq, t, want);
                    self.guard_to(ok, norm);
                    if num {
                        self.stack_num(ab + k)
                    } else {
                        self.stack_bool(ab + k)
                    }
                }
                _ => {
                    self.guard_to(z, norm);
                    if num {
                        self.fb.f64const(0.0)
                    } else {
                        self.i32c(0)
                    }
                }
            };
            vals[k] = Some(v);
        }
        // ---- commit ----
        // The operands below the arguments (the callee, a receiver, anything under the call
        // window) and surplus arguments are dropped.
        for k in (0..=ci).chain(ab + np.min(argc)..d) {
            if entries[k] == Entry::Boxed {
                self.drop_at(k);
            }
        }
        // The previous activation's slots.
        let n = self.kinds.len();
        let boxed: Vec<usize> = (0..n).filter(|&s| self.kinds[s] == Kind::Boxed).collect();
        if !boxed.is_empty() {
            let four = self.i32c(TAG_NUM as i64);
            let mut any = None;
            for &s in &boxed {
                let t = self.fb.load(MemKind::I32U8, self.slots, s as i32 * VALUE_SIZE);
                let big = self.fb.icmp(IntCC::Ugt, t, four);
                any = Some(match any {
                    None => big,
                    Some(a) => self.fb.binary(BinaryOp::Bor, a, big),
                });
            }
            let any = any.expect("a boxed slot");
            let drop_b = self.fb.create_block();
            let cont = self.fb.create_block();
            self.fb.brif(any, drop_b, &[], cont, &[]);
            self.fb.seal_block(drop_b);
            self.fb.switch_to_block(drop_b);
            let nv = self.i32c(n as i64);
            self.call(Helper::DropN, &[self.slots, nv]);
            self.fb.jump(cont, &[]);
            self.fb.seal_block(cont);
            self.fb.switch_to_block(cont);
            let u = self.i32c(TAG_UNDEFINED as i64);
            for &s in &boxed {
                let off = s as i32 * VALUE_SIZE;
                if s < np && s < argc {
                    match entries[ab + s] {
                        Entry::Num(x) => {
                            let t = self.i32c(TAG_NUM as i64);
                            self.fb.store(MemKind::I32U8, self.slots, t, off);
                            self.fb.store(MemKind::F64, self.slots, x, off + VALUE_PAYLOAD);
                        }
                        Entry::Bool(b) => {
                            let t = self.i32c(TAG_BOOL as i64);
                            self.fb.store(MemKind::I32U8, self.slots, t, off);
                            self.fb.store(MemKind::I32U8, self.slots, b, off + VALUE_BOOL);
                        }
                        _ => {
                            let so = self.soff(ab + s);
                            let (sp, sl) = (self.stackp, self.slots);
                            self.copy_value(sp, so, sl, off);
                            self.set_stack_tag(ab + s, TAG_UNDEFINED);
                        }
                    }
                } else {
                    self.fb.store(MemKind::I32U8, self.slots, u, off);
                }
            }
        }
        // SSA slots: the parameters' new values; locals `undefined` again.
        for s in 0..n {
            let Some(var) = self.vars[s] else { continue };
            if s < np {
                let v = vals[s].expect("a parameter value");
                self.fb.def_var(var, v);
            } else {
                let zv = if self.kinds[s] == Kind::Num {
                    self.fb.f64const(0.0)
                } else {
                    self.i32c(0)
                };
                self.fb.def_var(var, zv);
            }
            if let Some(f) = self.undef[s] {
                let one = self.i32c(1);
                self.fb.def_var(f, one);
            }
            if let Some(f) = self.tdz[s] {
                self.fb.def_var(f, z);
            }
        }
        self.stack.clear();
        self.invalidate_js();
        self.safepoint(0);
        match self.edge(true, 0) {
            Ok(b) => {
                self.fb.jump(b, &[]);
                self.flush_seals();
            }
            Err(e) => {
                self.err.get_or_insert(e);
                let w = self.i64c(0);
                self.fb.ret(&[w]);
            }
        }
        // ---- otherwise the direct call ----
        self.fb.seal_block(norm);
        self.fb.switch_to_block(norm);
        self.stack = entries;
        self.direct_call_full(pc, argc, with_this);
    }

    /// The call of a direct call site (see the module docs).
    pub(super) fn direct_call_full(&mut self, pc: usize, argc: usize, with_this: bool) {
        let site = match self.plan.direct.get(&pc) {
            Some(s) => s.clone(),
            None => self.plan.dacc[&pc].site.clone(),
        };
        let construct = site.construct;
        let o = offs().expect("planned with offsets");
        let wt = with_this as usize;
        let d = self.stack.len();
        let base = d - argc - 1 - wt;
        let ci = base + wt;
        // The callee's entry, `this`'s, the first argument's and the argument count: an adaptor
        // (see `Adaptor`) is at `ci`, its target elsewhere.
        let (fpos, this_pos, ab, nargs) = match site.adaptor {
            Adaptor::Call(..) => (base, Some(ci + 1), ci + 2, argc - 1),
            // (The list is an operand below the (runtime) arguments.)
            Adaptor::Apply(..) => (base, Some(ci + 1), d, 0),
            Adaptor::Bound => (ci, None, ci + 1, argc),
            Adaptor::None => (ci, with_this.then_some(base), ci + 1, argc),
        };
        // A receiver or callee borrowed from an environment could move (or die) while the
        // callee runs.
        let list_pos = (matches!(site.adaptor, Adaptor::Apply(..)) && argc == 2).then_some(ci + 2);
        for k in [Some(base), Some(ci), this_pos, list_pos].into_iter().flatten() {
            if matches!(self.stack[k], Entry::Ref(_, s) if s.in_env()) {
                self.force(k);
            }
        }
        // An unboxed `this` is read from memory.
        if let (Adaptor::Call(..) | Adaptor::Apply(..), Some(t)) = (site.adaptor, this_pos) {
            if let e @ (Entry::Num(_) | Entry::Bool(_)) = self.stack[t] {
                self.store_entry(t, e);
            }
        }
        // Surplus arguments gathered into a rest array are read from memory.
        if site.rest.is_some() && nargs > site.n_params {
            self.box_from(ab + site.n_params);
        }
        let entries = self.stack.clone();
        let below: Vec<Entry> = entries[..base].to_vec();
        let slow = self.fb.create_block();
        let join = self.fb.create_block();
        let st_join = self.fb.append_block_param(join, Type::I32);

        // ---- guards ----
        if let Adaptor::Call(w, _) | Adaptor::Apply(w, _) = site.adaptor {
            match entries[ci] {
                // Validated by its `GetMethod`.
                Entry::Ref(_, Src::Pin) => {}
                Entry::Ref(p, _) => self.check_callee(p, 0, w, slow),
                Entry::Boxed => {
                    let off = self.soff(ci);
                    let sp = self.stackp;
                    self.check_callee(sp, off, w, slow);
                }
                Entry::Num(_) | Entry::Bool(_) => {
                    self.fb.jump(slow, &[]);
                    let dead = self.fb.create_block();
                    self.fb.seal_block(dead);
                    self.fb.switch_to_block(dead);
                }
            }
        }
        // A bound function: its target and `this` (through its callable, which the operand
        // keeps alive for the call).
        let bound_this = if site.adaptor == Adaptor::Bound {
            let bp = match entries[ci] {
                Entry::Ref(p, _) => p,
                _ => self.sptr(ci),
            };
            let cellp = self.ptrc(site.cell as i64);
            let need = self.i32c((!site.strict && site.uses_this) as i64);
            let t = self
                .call(Helper::BoundThis, &[self.frame, cellp, bp, need])
                .expect("BoundThis returns a pointer");
            let zp = self.ptrc(0);
            let ok = self.fb.icmp(IntCC::Ne, t, zp);
            self.guard_to(ok, slow);
            Some(t)
        } else {
            None
        };
        let fentry = match site.adaptor {
            Adaptor::Bound => Entry::Boxed,
            _ => entries[fpos],
        };
        match fentry {
            _ if bound_this.is_some() => {}
            Entry::Ref(_, Src::Pin) => {}
            // (A rebound constructor is another closure of the same plain function: never a
            // class, which `inline_callee` refuses.)
            Entry::Ref(p, _) => self.check_cell(p, 0, site.cell, slow),
            Entry::Boxed => {
                let off = self.soff(fpos);
                let sp = self.stackp;
                self.check_cell(sp, off, site.cell, slow);
            }
            Entry::Num(_) | Entry::Bool(_) => {
                self.fb.jump(slow, &[]);
                let dead = self.fb.create_block();
                self.fb.seal_block(dead);
                self.fb.switch_to_block(dead);
            }
        }
        // An `apply` list whose elements can be read without running code.
        let list_p = match list_pos.map(|k| (k, entries[k])) {
            Some((k, e)) => {
                let lp = match e {
                    Entry::Ref(p, _) => p,
                    Entry::Boxed => self.sptr(k),
                    Entry::Num(_) | Entry::Bool(_) => self.ptrc(UNDEF.as_ptr() as usize as i64),
                };
                if matches!(e, Entry::Num(_) | Entry::Bool(_)) {
                    // (A primitive list throws: the ordinary path.)
                    let z = self.i32c(0);
                    self.guard_to(z, slow);
                } else {
                    let ok = self
                        .call(Helper::ApplyOk, &[self.frame, lp])
                        .expect("ApplyOk returns a flag");
                    let z = self.i32c(0);
                    let ok = self.fb.icmp(IntCC::Ne, ok, z);
                    self.guard_to(ok, slow);
                }
                Some(lp)
            }
            None => None,
        };
        let interp = self.fb.load(PTR_MEM, self.frame, FRAME_INTERP);
        let cp = self.ptrc(site.chunk as i64);
        let entry = if site.self_call {
            None
        } else {
            let e = self.fb.load(PTR_MEM, cp, CHUNK_DENTRY as i32);
            let z = self.ptrc(0);
            let ok = self.fb.icmp(IntCC::Ne, e, z);
            self.guard_to(ok, slow);
            Some(e)
        };
        let depth = self.fb.load(MemKind::I32, interp, o.depth);
        let lim = self.i32c(if site.tail {
            crate::bytecode::TAIL_NEST
        } else {
            crate::interpreter::MAX_EVAL_DEPTH
        } as i64);
        let ok = self.fb.icmp(IntCC::Ult, depth, lim);
        self.guard_to(ok, slow);
        // A plain call from a canonical activation of the callee's strictness changes no engine
        // flag (see `entry_flags`); anything else takes the ordinary path.
        // (Canonical for the other strictness, only `strict` and `tco_ok` differ: they are
        // switched to the callee's for the call and back after it.)
        let plain = match (construct, self.call_flags) {
            (false, Some(canon)) => {
                let z = self.i32c(0);
                let ok = self.fb.icmp(IntCC::Ne, canon, z);
                self.guard_to(ok, slow);
                true
            }
            _ => false,
        };
        if !site.arrow && !plain {
            let nt = self.fb.load(MemKind::I32U8, interp, o.new_target);
            let fi = self.fb.load(MemKind::I32U8, interp, o.field_init);
            let agb = self.fb.load(MemKind::I32U8, interp, o.agb);
            let a = self.fb.binary(BinaryOp::Bor, nt, fi);
            let a = self.fb.binary(BinaryOp::Bor, a, agb);
            let z = self.i32c(0);
            let ok = self.fb.icmp(IntCC::Eq, a, z);
            self.guard_to(ok, slow);
        }
        let tick = self.fb.load(MemKind::I32, interp, o.gc_tick);
        let one = self.i32c(1);
        let tick1 = self.fb.binary(BinaryOp::Iadd, tick, one);
        let mask = self.i32c(crate::interpreter::GC_CALL_POLL_MASK as i64);
        let m = self.fb.binary(BinaryOp::Band, tick1, mask);
        let term = self.fb.load(MemKind::I32U8, interp, o.terminating);
        let z = self.i32c(0);
        let m_ok = self.fb.icmp(IntCC::Ne, m, z);
        let t_ok = self.fb.icmp(IntCC::Eq, term, z);
        let ok = self.fb.binary(BinaryOp::Band, m_ok, t_ok);
        self.guard_to(ok, slow);
        let sh = self.ptrc(super::shadow() as usize as i64);
        // A `new` keeps its instance in the 16 bytes below the callee frame.
        let top0 = self.fb.load(PTR_MEM, sh, SHADOW_TOP);
        let top = if construct {
            let k = self.ptrc(VALUE_SIZE as i64);
            self.fb.binary(BinaryOp::Iadd, top0, k)
        } else {
            top0
        };
        let size = if site.self_call {
            let v = self.ptrc(0);
            self.self_sizes.push(v);
            v
        } else {
            self.fb.load(PTR_MEM, cp, CHUNK_DSIZE as i32)
        };
        let ntop = self.fb.binary(BinaryOp::Iadd, top, size);
        let end = self.fb.load(PTR_MEM, sh, SHADOW_END);
        let ok = self.fb.icmp(IntCC::Ule, ntop, end);
        self.guard_to(ok, slow);
        // The receiver's address (`this` of the callee).
        let this_p = match (construct, site.uses_this, site.adaptor, this_pos) {
            (true, ..) => top0,
            (_, true, Adaptor::Bound, _) => bound_this.expect("checked"),
            (_, true, _, Some(t)) => match entries[t] {
                Entry::Ref(p, _) => p,
                _ => self.sptr(t),
            },
            _ => self.ptrc(UNDEF.as_ptr() as usize as i64),
        };
        // (A bound `this` was checked by the planner.)
        if let (false, true, false, Some(t)) =
            (site.strict, site.uses_this, construct, this_pos)
        {
            // A sloppy callee binds a primitive or nullish receiver differently.
            match entries[t] {
                Entry::Num(_) | Entry::Bool(_) => {
                    self.fb.jump(slow, &[]);
                    let dead = self.fb.create_block();
                    self.fb.seal_block(dead);
                    self.fb.switch_to_block(dead);
                }
                _ => {
                    let tag = self.fb.load(MemKind::I32U8, this_p, 0);
                    let obj = self.i32c(TAG_OBJ as i64);
                    let ok = self.fb.icmp(IntCC::Eq, tag, obj);
                    self.guard_to(ok, slow);
                }
            }
        }
        if site.reflect {
            let fa = self.ptrc(crate::bytecode::reflect::enabled_addr() as i64);
            let f = self.fb.load(MemKind::I32U8, fa, 0);
            let z = self.i32c(0);
            let ok = self.fb.icmp(IntCC::Eq, f, z);
            self.guard_to(ok, slow);
        }

        // ---- commit: the call's bookkeeping ----
        self.fb.store(MemKind::I32, interp, tick1, o.gc_tick);
        let depth1 = self.fb.binary(BinaryOp::Iadd, depth, one);
        self.fb.store(MemKind::I32, interp, depth1, o.depth);
        let cellp = self.ptrc(site.cell as i64);
        let strict = self.i32c(site.strict as i64);
        let zp = self.ptrc(0);
        // Stack-trace bookkeeping: this call's pc is the caller's site (see `Interp::cur_site`).
        let here = self.i32c((crate::interpreter::frames::SITE_PC | pc as u32) as i32 as i64);
        let z = self.i32c(0);
        let saved = if plain {
            self.fb.store(MemKind::I32U8, interp, strict, o.strict);
            self.fb.store(MemKind::I32U8, interp, strict, o.tco);
            None
        } else {
            let s_strict = self.fb.load(MemKind::I32U8, interp, o.strict);
            let s_tco = self.fb.load(MemKind::I32U8, interp, o.tco);
            let s_ctor = self.fb.load(MemKind::I32U8, interp, o.ctor);
            self.fb.store(MemKind::I32U8, interp, strict, o.strict);
            self.fb.store(MemKind::I32U8, interp, strict, o.tco);
            self.fb.store(MemKind::I32U8, interp, z, o.ctor);
            Some((s_strict, s_tco, s_ctor))
        };
        self.fb.store(PTR_MEM, sh, ntop, SHADOW_TOP);
        if construct {
            self.call(Helper::NewThis, &[self.frame, cellp, top0]);
        }
        // The call record, then the frame header.
        let rec = top;
        let rb = self.ptrc(REC_BYTES as i64);
        let nf = self.fb.binary(BinaryOp::Iadd, rec, rb);
        let hdr = self.ptrc(FRAME_HDR as i64);
        let slots_p = self.fb.binary(BinaryOp::Iadd, nf, hdr);
        let so = self.ptrc(site.n_slots as i64 * VALUE_SIZE as i64);
        let stack_p = self.fb.binary(BinaryOp::Iadd, slots_p, so);
        self.fb.store(PTR_MEM, nf, slots_p, FRAME_SLOTS);
        let consts = self.ptrc(site.consts as i64);
        self.fb.store(PTR_MEM, nf, consts, FRAME_CONSTS);
        self.fb.store(PTR_MEM, nf, stack_p, FRAME_STACK);
        self.fb.store(PTR_MEM, nf, interp, FRAME_INTERP);
        self.fb.store(PTR_MEM, nf, cp, FRAME_CHUNK);
        let envp = self.fb.load(PTR_MEM, cellp, CELL_ENV);
        self.fb.store(PTR_MEM, nf, envp, FRAME_ENV);
        self.fb.store(PTR_MEM, nf, this_p, FRAME_THIS);
        // The callee's flags are canonical after a plain call, or after one that set them up
        // (a plain function called from elsewhere: `new.target` and the markers checked clear).
        let canon = !construct && (plain || !site.arrow);
        let cv = self.i32c(if canon { 1 + site.strict as i64 } else { 0 });
        self.fb.store(MemKind::I32U8, nf, cv, FRAME_CANON);
        // The lazy call record (see `super::sync_frames`), linked as the newest pending one.
        let fnp = match fentry {
            Entry::Ref(_, Src::Pin) if site.fn_ptr != 0 => self.ptrc(site.fn_ptr as i64),
            _ => self.fb.load(PTR_MEM, cellp, CELL_FN),
        };
        self.fb.store(PTR_MEM, rec, fnp, REC_FN);
        let coro = self.fb.load(MemKind::I32, interp, o.cur_coro);
        self.fb.store(MemKind::I32, rec, coro, REC_CORO);
        // Flags and site in one word.
        let rf = (if site.strict { REC_STRICT } else { 0 })
            | (if construct { REC_CONSTRUCT } else { 0 });
        let here_u = (crate::interpreter::frames::SITE_PC | pc as u32) as u64;
        let fs = self.i64c(((here_u << 32) | rf as u64) as i64);
        self.fb.store(MemKind::I64, rec, fs, REC_FLAGS);
        let head = self.fb.load(PTR_MEM, interp, o.jit_frames);
        self.fb.store(PTR_MEM, rec, head, REC_LINK);
        self.fb.store(PTR_MEM, interp, rec, o.jit_frames);
        let no_site = self.i32c(crate::interpreter::frames::NO_SITE as i32 as i64);
        self.fb.store(MemKind::I32, interp, no_site, o.cur_site);
        // (`exit_depth` is written by every exit that reads it, `ta_data` / `ta_len` by the
        // `TaView` probe before any read.)
        let z64 = self.i64c(0);
        self.fb.store(MemKind::I32U8, nf, z, FRAME_EXCEPTION);
        let budget = self.i64c(SAFEPOINT_BUDGET);
        self.fb.store(MemKind::I64, nf, budget, FRAME_BUDGET);
        self.fb.store(MemKind::I64, nf, z64, FRAME_EXIT_HSET);
        // The slots: arguments moved in, the rest `undefined`.
        let seed = nargs.min(site.n_params);
        for k in 0..site.n_slots {
            let off = k as i32 * VALUE_SIZE;
            if k >= seed {
                self.fb.store(MemKind::I32U8, slots_p, z, off);
                continue;
            }
            match entries[ab + k] {
                Entry::Num(x) => {
                    let t = self.i32c(TAG_NUM as i64);
                    self.fb.store(MemKind::I32U8, slots_p, t, off);
                    self.fb.store(MemKind::F64, slots_p, x, off + VALUE_PAYLOAD);
                }
                Entry::Bool(b) => {
                    let t = self.i32c(TAG_BOOL as i64);
                    self.fb.store(MemKind::I32U8, slots_p, t, off);
                    self.fb.store(MemKind::I32U8, slots_p, b, off + VALUE_BOOL);
                }
                Entry::Boxed => {
                    let so = self.soff(ab + k);
                    self.copy_value(self.stackp, so, slots_p, off);
                    self.set_stack_tag(ab + k, TAG_UNDEFINED);
                }
                Entry::Ref(p, _) => {
                    let kc = self.ptrc(off as i64);
                    let dst = self.fb.binary(BinaryOp::Iadd, slots_p, kc);
                    self.clone_mem(dst, p);
                }
            }
        }
        if let Some(lp) = list_p {
            let nv = self.i32c(site.n_params as i64);
            self.call(Helper::ApplySeed, &[lp, slots_p, nv]);
        }
        if let Some((s, base)) = site.virt {
            let n = nargs.saturating_sub(base as usize) as f64;
            let off = s as i32 * VALUE_SIZE;
            let t = self.i32c(TAG_NUM as i64);
            self.fb.store(MemKind::I32U8, slots_p, t, off);
            let x = self.fb.f64const(n);
            self.fb.store(MemKind::F64, slots_p, x, off + VALUE_PAYLOAD);
        }
        match site.rest {
            Some(r) => {
                let extra = nargs.saturating_sub(site.n_params);
                let rc = self.ptrc(r as i64 * VALUE_SIZE as i64);
                let dst = self.fb.binary(BinaryOp::Iadd, slots_p, rc);
                let src = self.sptr(ab + seed);
                let nv = self.i32c(extra as i64);
                self.call(Helper::MakeRest, &[self.frame, dst, src, nv]);
            }
            None => {
                for k in seed..nargs {
                    if entries[ab + k] == Entry::Boxed {
                        self.drop_at(ab + k);
                    }
                }
            }
        }
        // ---- the call ----
        let word = match entry {
            None => {
                let f = match self.self_fn {
                    Some(f) => f,
                    None => {
                        let f = self
                            .fb
                            .func
                            .import_function(Signature::new(vec![PTR], vec![Type::I64]), SELF_ID);
                        self.self_fn = Some(f);
                        f
                    }
                };
                self.fb.call_fn(f, &[nf])[0]
            }
            Some(e) => {
                let sig = self
                    .fb
                    .func
                    .import_signature(Signature::new(vec![PTR], vec![Type::I64]));
                self.fb.call_indirect(sig, e, &[nf])[0]
            }
        };
        let mask = self.i64c(0xff);
        let kind = self.fb.binary(BinaryOp::Band, word, mask);
        let ret = self.i64c(EXIT_RETURN as i64);
        let is_ret = self.fb.icmp(IntCC::Eq, kind, ret);
        let fast = if site.tails {
            let pt = self.fb.load(PTR_MEM, interp, o.pending_tail);
            let no_pt = self.fb.icmp(IntCC::Eq, pt, zp);
            self.fb.binary(BinaryOp::Band, is_ret, no_pt)
        } else {
            is_ret
        };
        let fin = self.fb.create_block();
        let post = self.fb.create_block();
        let st_post = self.fb.append_block_param(post, Type::I32);
        self.fb.brif(fast, post, &[z], fin, &[]);
        self.fb.seal_block(fin);
        self.fb.switch_to_block(fin);
        let ev = match entry {
            Some(e) => e,
            None => {
                let cell = self.ptrc(self.self_entry as i64);
                self.fb.load(PTR_MEM, cell, 0)
            }
        };
        let st = self
            .call(Helper::CallFinish, &[self.frame, nf, word, ev])
            .expect("CallFinish returns a status");
        self.fb.jump(post, &[st]);
        self.fb.seal_block(post);
        self.fb.switch_to_block(post);
        // ---- after the call ----
        if construct {
            self.call(Helper::NewDone, &[self.frame, cellp, top0, stack_p, st_post]);
        }
        // The callee, receiver and adaptor operands below the arguments.
        for k in base..ab {
            if entries[k] == Entry::Boxed {
                self.drop_at(k);
            }
        }
        let bo = self.soff(base);
        let sp = self.stackp;
        self.copy_value(stack_p, 0, sp, bo);
        self.fb.store(MemKind::I32U8, stack_p, z, 0);
        if (1..=DROP_INLINE_SLOTS).contains(&site.n_slots) {
            // Few slots: each released inline (a reference count step for an object or string).
            for k in 0..site.n_slots {
                self.drop_mem(slots_p, k as i32 * VALUE_SIZE);
            }
        } else if site.n_slots > 0 {
            let four = self.i32c(TAG_NUM as i64);
            let mut any = None;
            for k in 0..site.n_slots {
                let t = self.fb.load(MemKind::I32U8, slots_p, k as i32 * VALUE_SIZE);
                let big = self.fb.icmp(IntCC::Ugt, t, four);
                any = Some(match any {
                    None => big,
                    Some(a) => self.fb.binary(BinaryOp::Bor, a, big),
                });
            }
            let any = any.expect("at least one slot");
            let drop_b = self.fb.create_block();
            let cont = self.fb.create_block();
            self.fb.brif(any, drop_b, &[], cont, &[]);
            self.fb.seal_block(drop_b);
            self.fb.switch_to_block(drop_b);
            let nv = self.i32c(site.n_slots as i64);
            self.call(Helper::DropN, &[slots_p, nv]);
            self.fb.jump(cont, &[]);
            self.fb.seal_block(cont);
            self.fb.switch_to_block(cont);
        }
        // Pop the call record: out of `fn_frames` when a sync copied it there, and unlinked.
        let rf = self.fb.load(MemKind::I32, rec, REC_FLAGS);
        let synced = self.i32c(REC_SYNCED as i64);
        let sb = self.fb.binary(BinaryOp::Band, rf, synced);
        let was = self.fb.icmp(IntCC::Ne, sb, z);
        let pop_b = self.fb.create_block();
        let done = self.fb.create_block();
        self.fb.brif(was, pop_b, &[], done, &[]);
        self.fb.seal_block(pop_b);
        self.fb.switch_to_block(pop_b);
        self.call(Helper::PopFnFrame, &[self.frame]);
        self.fb.jump(done, &[]);
        self.fb.seal_block(done);
        self.fb.switch_to_block(done);
        let link = self.fb.load(PTR_MEM, rec, REC_LINK);
        self.fb.store(PTR_MEM, interp, link, o.jit_frames);
        // Leave the shadow stack as it was: header words trivially droppable (on 64-bit hosts
        // they already are, see `JitFrame`; `CallFinish` restores them after other exits),
        // `top` back.
        if cfg!(target_pointer_width = "32") {
            let mut off = 0;
            while off < FRAME_HDR {
                self.fb.store(MemKind::I32U8, nf, z, off as i32);
                off += 16;
            }
        }
        self.fb.store(PTR_MEM, sh, top0, SHADOW_TOP);
        if let Some((s_strict, s_tco, s_ctor)) = saved {
            self.fb.store(MemKind::I32U8, interp, s_strict, o.strict);
            self.fb.store(MemKind::I32U8, interp, s_tco, o.tco);
            self.fb.store(MemKind::I32U8, interp, s_ctor, o.ctor);
        } else if let Some(canon) = self.call_flags {
            let one = self.i32c(1);
            let s0 = self.fb.binary(BinaryOp::Isub, canon, one);
            self.fb.store(MemKind::I32U8, interp, s0, o.strict);
            self.fb.store(MemKind::I32U8, interp, s0, o.tco);
        }
        let d1 = self.fb.load(MemKind::I32, interp, o.depth);
        let d0 = self.fb.binary(BinaryOp::Isub, d1, one);
        self.fb.store(MemKind::I32, interp, d0, o.depth);
        self.fb.store(MemKind::I32, interp, here, o.cur_site);
        self.fb.jump(join, &[st_post]);

        // ---- the ordinary call ----
        self.fb.seal_block(slow);
        self.fb.switch_to_block(slow);
        self.stack = entries;
        self.box_from(base);
        let st = if construct {
            let st = self.call_status(Helper::GcPoll, &[self.frame]);
            let z = self.i32c(0);
            let ok = self.fb.icmp(IntCC::Eq, st, z);
            let go = self.fb.create_block();
            let bad = self.fb.create_block();
            self.fb.brif(ok, go, &[], bad, &[]);
            self.fb.seal_block(bad);
            self.fb.switch_to_block(bad);
            // The operands are the op's to consume.
            let (sp, nv) = (self.sptr(base), self.i32c((d - base) as i64));
            self.call(Helper::DropN, &[sp, nv]);
            self.fb.jump(join, &[st]);
            self.fb.seal_block(go);
            self.fb.switch_to_block(go);
            let (r, bv, dv) = (
                self.i32c(pc as i64),
                self.i32c(base as i64),
                self.i32c(d as i64),
            );
            self.call_status(Helper::Generic, &[self.frame, r, bv, dv])
        } else {
            let tail = if site.tail { helpers::CALL_TAIL as i64 } else { 0 };
            let (bv, av, wv) = (
                self.i32c(base as i64),
                self.i32c(argc as i64),
                self.i32c(with_this as i64 | self.await_fused(pc) | tail),
            );
            self.set_call_site(pc);
            self.call_status(Helper::Call, &[self.frame, bv, av, wv])
        };
        self.fb.jump(join, &[st]);

        self.fb.seal_block(join);
        self.fb.switch_to_block(join);
        self.invalidate_js();
        self.stack = below.clone();
        self.throw_if(st_join, pc, &below, base);
        self.soff(base);
        self.stack.push(Entry::Boxed);
    }
}

/// Record `dglobal` for the by-name callee producer at `pp` when its name cache is in
/// global-object mode on this realm's global scope (see [`Tr::direct_name`]).
fn global_name(interp: &Interp, chunk: &Chunk, pp: usize, op: Op, p: &mut Plan) {
    let (Op::LoadName(_, c) | Op::LoadNameForCall(_, c)) = op else { return };
    let Some(cell) = chunk.name_caches.get(c as usize) else { return };
    let ic = cell.get();
    if ic.env != std::rc::Rc::as_ptr(&interp.global_env) as usize | 1 {
        return;
    }
    let gp = crate::value::Gc::as_ptr(&interp.global) as usize;
    if !interp.ordinary_get_ptr(gp) {
        return;
    }
    p.dglobal
        .insert(pp, (gp, (ic.binding >> 32) as u32, ic.binding as u32));
    p.boxes.push(Box::new(interp.global.clone()));
}

/// [`DSite::virt`] of callee chunk `cc`.
fn virt_site(cc: &Chunk) -> Option<(u16, u16)> {
    let base = cc.virt_base?;
    Some((cc.arguments_slot.or(cc.rest_slot)?, base))
}
