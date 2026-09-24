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
//! ([`super::shadow`]): `[header | slots | stack area]`, [`frame_bytes`] long. The arguments
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
use crate::interpreter::FnFrame;
use lumen_codegen::{BinaryOp, IntCC, MemKind, Signature, Type, Value as V};
use std::sync::OnceLock;

/// Slots a directly called function may have (the site stores each one).
const MAX_DIRECT_SLOTS: usize = 64;

/// A direct call site (see the module docs).
#[derive(Clone, Debug)]
pub(super) struct DSite {
    /// The callee's payload word, as a `Value` stores it.
    pub word: usize,
    /// `Gc::as_ptr` of the callee (its `fn_frames` record).
    pub fn_ptr: usize,
    pub chunk: usize,
    pub consts: usize,
    /// The address of the callee's environment (a pinned `Env`).
    pub env: usize,
    /// The address of the pinned callee `Value` (an [`Src::Pin`] entry).
    pub pin: usize,
    pub strict: bool,
    pub arrow: bool,
    pub uses_this: bool,
    /// The callee records its arguments when reflection is enabled (see `bytecode::reflect`).
    pub reflect: bool,
    pub n_slots: usize,
    pub n_params: usize,
    /// A recursive call of the code being compiled.
    pub self_call: bool,
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
    /// `Interp::fn_frames`' pointer / length / capacity words.
    ff_ptr: i32,
    ff_len: i32,
    ff_cap: i32,
    fr_size: i64,
    fr_fn: i32,
    fr_coro: i32,
    fr_strict: i32,
    fr_extra: i32,
}

fn offs() -> Option<&'static Offs> {
    use crate::interpreter::Interp as I;
    use std::mem::{offset_of, size_of};
    static OFFS: OnceLock<Option<Offs>> = OnceLock::new();
    OFFS.get_or_init(|| {
        let i = |x: usize| i32::try_from(x).ok();
        let (vp, vl) = layout::vec_layout(FnFrame {
            fn_ptr: 0,
            coro: 0,
            strict: false,
            extra: None,
        })?;
        let w = size_of::<usize>();
        let vc = 3 * w - vp - vl;
        let ff = offset_of!(I, fn_frames);
        // `extra` (an `Option<Box<_>>`) is a nullable pointer: null is `None`.
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
            ff_ptr: i(ff + vp)?,
            ff_len: i(ff + vl)?,
            ff_cap: i(ff + vc)?,
            fr_size: size_of::<FnFrame>() as i64,
            fr_fn: i(offset_of!(FnFrame, fn_ptr))?,
            fr_coro: i(offset_of!(FnFrame, coro))?,
            fr_strict: i(offset_of!(FnFrame, strict))?,
            fr_extra: i(offset_of!(FnFrame, extra))?,
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
        let (argc, wt) = match ops[q] {
            Op::Call(a) => (a as usize, 0),
            Op::CallWithThis(a) => (a as usize, 1),
            _ => continue,
        };
        if p.math.values().any(|s| s.call == q) {
            continue;
        }
        let Some(base) = d.checked_sub(argc + 1 + wt) else { continue };
        let ci = base + wt;
        let Some((pp, pos)) = producer(q, ci) else { continue };
        if p.math.contains_key(&pp) || (pp > 0 && p.math.contains_key(&(pp - 1))) {
            continue;
        }
        if helpers::math_site_failed(chunk, pp) || p.dname.contains_key(&pp) {
            continue;
        }
        let (callee, by_name) = match ops[pp] {
            Op::LoadNameForCall(n, c) if wt == 1 && pos == 1 => {
                (helpers::name_value(interp, chunk, env, n, c), true)
            }
            Op::LoadName(n, c) if wt == 0 && pos == 0 => {
                (helpers::name_value(interp, chunk, env, n, c), true)
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
                let f = match (recv, name(m)) {
                    (Some(r), Some(nm)) => method_value(interp, &r, nm),
                    _ => None,
                };
                (f, false)
            }
            _ => continue,
        };
        let Some(callee @ Value::Obj(_)) = callee else { continue };
        let Some(ic) = crate::bytecode::inline_callee(interp, &callee) else {
            continue;
        };
        let cc: &Chunk = &ic.chunk;
        if cc.activation_layout.is_some()
            || cc.arguments_slot.is_some()
            || cc.rest_slot.is_some()
            || cc.derived
            || cc.n_slots > MAX_DIRECT_SLOTS
            || (!ic.strict && cc.uses_this() && wt == 0)
            || cc
                .var_force_resets
                .iter()
                .any(|&s| (s as usize) < cc.n_params.min(argc))
        {
            continue;
        }
        let Value::Obj(o) = &callee else { continue };
        let fn_ptr = crate::value::Gc::as_ptr(o) as usize;
        // SAFETY: a `Value` is `VALUE_SIZE` bytes with its payload at `VALUE_PAYLOAD`.
        let word = unsafe {
            *(&callee as *const Value as *const u8)
                .add(VALUE_PAYLOAD as usize)
                .cast::<usize>()
        };
        let self_call =
            func_mode && std::ptr::eq(cc, chunk) && !cfg!(target_arch = "wasm32");
        let pin: Box<Value> = Box::new(callee.clone());
        let env_box: Box<Env> = Box::new(ic.env.clone());
        let site = DSite {
            word,
            fn_ptr,
            chunk: cc as *const Chunk as usize,
            consts: cc.consts.as_ptr() as usize,
            env: &*env_box as *const Env as usize,
            pin: &*pin as *const Value as usize,
            strict: ic.strict,
            arrow: ic.arrow,
            uses_this: cc.uses_this(),
            reflect: cc.reflect_args,
            n_slots: cc.n_slots,
            n_params: cc.n_params,
            self_call,
        };
        p.boxes.push(pin);
        p.boxes.push(env_box);
        p.boxes.push(Box::new(ic.chunk.clone()));
        if by_name {
            p.dname.insert(pp, q);
            p.name_ref.remove(&pp);
        }
        p.direct.insert(q, site);
    }
}

impl Tr<'_, '_> {
    /// Continue in a fresh block when `ok` (I32) is nonzero, else branch to `miss`.
    fn guard_to(&mut self, ok: V, miss: Block) {
        let next = self.fb.create_block();
        self.fb.brif(ok, next, &[], miss, &[]);
        self.fb.seal_block(next);
        self.fb.switch_to_block(next);
    }

    fn i64c(&mut self, v: i64) -> V {
        self.fb.iconst(Type::I64, v)
    }

    /// The name load producing a direct site's callee: check it still resolves to the
    /// function (exit before the load otherwise, and leave the site out of the next compile),
    /// then push the pinned callee (and `LoadNameForCall`'s undefined receiver).
    pub(super) fn direct_name(&mut self, pc: usize, n: u32, c: u32, lfc: bool) {
        let q = self.plan.dname[&pc];
        let (word, pin) = {
            let s = &self.plan.direct[&q];
            (s.word, s.pin)
        };
        let d = self.stack.len();
        let cont = self.fb.create_block();
        let full = self.fb.create_block();
        // The name cache's scope mode, inline: the cache names this frame's scope, the scope's
        // map is structurally unchanged since the fill (so the binding pointer is live), and
        // the binding holds the function.
        if std::env::var_os("XX_DBG").is_some() {
            let so = scope_offs();
            eprintln!("XXDBG so={:?}", so.map(|s| (s.rc_value, s.borrow, s.gen, s.ic_env, s.ic_binding, s.ic_gen, s.b_value, s.b_init)));
            if let Some(cell) = self.chunk.name_caches.get(c as usize) { let ic = cell.get(); eprintln!("XXDBG ic env={:x} gen={} binding={:x}", ic.env, ic.gen, ic.binding); }
        }
        match (scope_offs(), self.chunk.name_caches.get(c as usize)) {
            (Some(so), Some(cell)) if !cfg!(target_arch = "wasm32") => {
                let icp = self.ptrc(cell as *const _ as usize as i64);
                let ic_env = self.fb.load(PTR_MEM, icp, so.ic_env);
                let envp = self.fb.load(PTR_MEM, self.frame, FRAME_ENV);
                let rc = self.fb.load(PTR_MEM, envp, 0);
                let off = self.ptrc(so.rc_value);
                let scope = self.fb.binary(BinaryOp::Iadd, rc, off);
                let ok = self.fb.icmp(IntCC::Eq, ic_env, scope);
                self.guard_to(ok, full);
                let flag = self.fb.load(PTR_MEM, scope, so.borrow);
                let z = self.ptrc(0);
                let ok = self.fb.icmp(IntCC::Sge, flag, z);
                self.guard_to(ok, full);
                let g = self.fb.load(MemKind::I32, scope, so.gen);
                let icg = self.fb.load(MemKind::I32, icp, so.ic_gen);
                let ok = self.fb.icmp(IntCC::Eq, g, icg);
                self.guard_to(ok, full);
                let bd = self.fb.load(PTR_MEM, icp, so.ic_binding);
                let init = self.fb.load(MemKind::I32U8, bd, so.b_init);
                let z32 = self.i32c(0);
                let ok = self.fb.icmp(IntCC::Ne, init, z32);
                self.guard_to(ok, full);
                self.check_callee(bd, so.b_value, word, full);
                self.fb.jump(cont, &[]);
            }
            _ => self.fb.jump(full, &[]),
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

    /// Branch to `miss` unless the `Value` at `addr + off` is the callee `word`.
    fn check_callee(&mut self, addr: V, off: i32, word: usize, miss: Block) {
        let tag = self.fb.load(MemKind::I32U8, addr, off);
        let obj = self.i32c(TAG_OBJ as i64);
        let is_obj = self.fb.icmp(IntCC::Eq, tag, obj);
        let pl = self.fb.load(PTR_MEM, addr, off + VALUE_PAYLOAD);
        let want = self.ptrc(word as i64);
        let same = self.fb.icmp(IntCC::Eq, pl, want);
        let ok = self.fb.binary(BinaryOp::Band, is_obj, same);
        self.guard_to(ok, miss);
    }

    /// A direct call site's `Call` / `CallWithThis` (see the module docs).
    pub(super) fn direct_call(&mut self, pc: usize, argc: usize, with_this: bool) {
        let site = self.plan.direct[&pc].clone();
        let o = offs().expect("planned with offsets");
        let wt = with_this as usize;
        let d = self.stack.len();
        let base = d - argc - 1 - wt;
        let ci = base + wt;
        let ab = ci + 1;
        // A receiver borrowed from an environment could move while the callee runs.
        if with_this && matches!(self.stack[base], Entry::Ref(_, s) if s.in_env()) {
            self.force(base);
        }
        let entries = self.stack.clone();
        let below: Vec<Entry> = entries[..base].to_vec();
        let slow = self.fb.create_block();
        let join = self.fb.create_block();
        let st_join = self.fb.append_block_param(join, Type::I32);

        // ---- guards ----
        match entries[ci] {
            Entry::Ref(_, Src::Pin) => {}
            Entry::Ref(p, _) => self.check_callee(p, 0, site.word, slow),
            Entry::Boxed => {
                let off = self.soff(ci);
                let sp = self.stackp;
                self.check_callee(sp, off, site.word, slow);
            }
            Entry::Num(_) | Entry::Bool(_) => {
                self.fb.jump(slow, &[]);
                let dead = self.fb.create_block();
                self.fb.seal_block(dead);
                self.fb.switch_to_block(dead);
            }
        }
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
        let lim = self.i32c(crate::interpreter::MAX_EVAL_DEPTH as i64);
        let ok = self.fb.icmp(IntCC::Ult, depth, lim);
        self.guard_to(ok, slow);
        if !site.arrow {
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
        let len = self.fb.load(PTR_MEM, interp, o.ff_len);
        let cap = self.fb.load(PTR_MEM, interp, o.ff_cap);
        let ok = self.fb.icmp(IntCC::Ult, len, cap);
        self.guard_to(ok, slow);
        let sh = self.ptrc(super::shadow() as usize as i64);
        let top = self.fb.load(PTR_MEM, sh, SHADOW_TOP);
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
        let this_p = if !site.uses_this || !with_this {
            self.ptrc(UNDEF.as_ptr() as usize as i64)
        } else {
            match entries[base] {
                Entry::Ref(p, _) => p,
                _ => self.sptr(base),
            }
        };
        if !site.strict && site.uses_this {
            // A sloppy callee binds a primitive or nullish receiver differently.
            match entries[base] {
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
        let fp = self.fb.load(PTR_MEM, interp, o.ff_ptr);
        let sz = self.ptrc(o.fr_size);
        let at = self.fb.binary(BinaryOp::Imul, len, sz);
        let rec = self.fb.binary(BinaryOp::Iadd, fp, at);
        let fnp = self.ptrc(site.fn_ptr as i64);
        self.fb.store(PTR_MEM, rec, fnp, o.fr_fn);
        let coro = self.fb.load(MemKind::I32, interp, o.cur_coro);
        self.fb.store(MemKind::I32, rec, coro, o.fr_coro);
        let strict = self.i32c(site.strict as i64);
        self.fb.store(MemKind::I32U8, rec, strict, o.fr_strict);
        let zp = self.ptrc(0);
        self.fb.store(PTR_MEM, rec, zp, o.fr_extra);
        let onep = self.ptrc(1);
        let len1 = self.fb.binary(BinaryOp::Iadd, len, onep);
        self.fb.store(PTR_MEM, interp, len1, o.ff_len);
        let s_strict = self.fb.load(MemKind::I32U8, interp, o.strict);
        let s_tco = self.fb.load(MemKind::I32U8, interp, o.tco);
        let s_ctor = self.fb.load(MemKind::I32U8, interp, o.ctor);
        self.fb.store(MemKind::I32U8, interp, strict, o.strict);
        self.fb.store(MemKind::I32U8, interp, strict, o.tco);
        let z = self.i32c(0);
        self.fb.store(MemKind::I32U8, interp, z, o.ctor);
        self.fb.store(PTR_MEM, sh, ntop, SHADOW_TOP);
        // The frame header.
        let nf = top;
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
        let envp = self.ptrc(site.env as i64);
        self.fb.store(PTR_MEM, nf, envp, FRAME_ENV);
        self.fb.store(PTR_MEM, nf, this_p, FRAME_THIS);
        let z64 = self.i64c(0);
        self.fb.store(MemKind::I64, nf, z64, FRAME_EXIT_DEPTH);
        self.fb.store(MemKind::I32U8, nf, z, FRAME_EXCEPTION);
        let budget = self.i64c(SAFEPOINT_BUDGET);
        self.fb.store(MemKind::I64, nf, budget, FRAME_BUDGET);
        self.fb.store(MemKind::I64, nf, z64, FRAME_EXIT_HSET);
        self.fb.store(PTR_MEM, nf, zp, FRAME_TA_DATA);
        self.fb.store(PTR_MEM, nf, zp, FRAME_TA_LEN);
        // The slots: arguments moved in, the rest `undefined`.
        let seed = argc.min(site.n_params);
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
                    let w0 = self.fb.load(MemKind::I64, self.stackp, so);
                    let w1 = self.fb.load(MemKind::I64, self.stackp, so + 8);
                    self.fb.store(MemKind::I64, slots_p, w0, off);
                    self.fb.store(MemKind::I64, slots_p, w1, off + 8);
                    self.set_stack_tag(ab + k, TAG_UNDEFINED);
                }
                Entry::Ref(p, _) => {
                    let kc = self.ptrc(off as i64);
                    let dst = self.fb.binary(BinaryOp::Iadd, slots_p, kc);
                    self.call(Helper::Clone, &[dst, p]);
                }
            }
        }
        for k in seed..argc {
            if entries[ab + k] == Entry::Boxed {
                self.drop_at(ab + k);
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
        let pt = self.fb.load(PTR_MEM, interp, o.pending_tail);
        let no_pt = self.fb.icmp(IntCC::Eq, pt, zp);
        let fast = self.fb.binary(BinaryOp::Band, is_ret, no_pt);
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
        if with_this && entries[base] == Entry::Boxed {
            self.drop_at(base);
        }
        if entries[ci] == Entry::Boxed {
            self.drop_at(ci);
        }
        let w0 = self.fb.load(MemKind::I64, stack_p, 0);
        let w1 = self.fb.load(MemKind::I64, stack_p, 8);
        let bo = self.soff(base);
        self.fb.store(MemKind::I64, self.stackp, w0, bo);
        self.fb.store(MemKind::I64, self.stackp, w1, bo + 8);
        self.fb.store(MemKind::I32U8, stack_p, z, 0);
        if site.n_slots > 0 {
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
        // Leave the shadow stack as it was: header words trivially droppable, `top` back.
        let mut off = 0;
        while off < FRAME_HDR {
            self.fb.store(MemKind::I32U8, nf, z, off as i32);
            off += 16;
        }
        self.fb.store(PTR_MEM, sh, nf, SHADOW_TOP);
        self.fb.store(MemKind::I32U8, interp, s_strict, o.strict);
        self.fb.store(MemKind::I32U8, interp, s_tco, o.tco);
        self.fb.store(MemKind::I32U8, interp, s_ctor, o.ctor);
        // Pop the `fn_frames` record (the buffer may have moved).
        let fp = self.fb.load(PTR_MEM, interp, o.ff_ptr);
        let len = self.fb.load(PTR_MEM, interp, o.ff_len);
        let len0 = self.fb.binary(BinaryOp::Isub, len, onep);
        let at = self.fb.binary(BinaryOp::Imul, len0, sz);
        let rec = self.fb.binary(BinaryOp::Iadd, fp, at);
        let extra = self.fb.load(PTR_MEM, rec, o.fr_extra);
        let has = self.fb.icmp(IntCC::Ne, extra, zp);
        let pop_b = self.fb.create_block();
        let set_b = self.fb.create_block();
        let done = self.fb.create_block();
        self.fb.brif(has, pop_b, &[], set_b, &[]);
        self.fb.seal_block(pop_b);
        self.fb.seal_block(set_b);
        self.fb.switch_to_block(pop_b);
        self.call(Helper::PopFnFrame, &[self.frame]);
        self.fb.jump(done, &[]);
        self.fb.switch_to_block(set_b);
        self.fb.store(PTR_MEM, interp, len0, o.ff_len);
        self.fb.jump(done, &[]);
        self.fb.seal_block(done);
        self.fb.switch_to_block(done);
        self.fb.store(MemKind::I32, interp, depth, o.depth);
        self.fb.jump(join, &[st_post]);

        // ---- the ordinary call ----
        self.fb.seal_block(slow);
        self.fb.switch_to_block(slow);
        self.stack = entries;
        self.box_from(base);
        let (bv, av, wv) = (
            self.i32c(base as i64),
            self.i32c(argc as i64),
            self.i32c(with_this as i64),
        );
        let st = self.call_status(Helper::Call, &[self.frame, bv, av, wv]);
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
