//! Bytecode region → lumen-codegen IR. See the contract in [`super`].
//!
//! The translator walks the region's ops in pc order, one IR block per bytecode basic block,
//! with an abstract operand stack of typed entries ([`Entry`]) and the region's `Num` / `Bool`
//! locals as IR variables (SSA via the builder's `def_var` / `use_var`). Unboxed arithmetic and
//! comparisons are emitted inline; everything else goes through a speculation guard (exit on
//! failure), a [`super::layout`] fast path, or a [`super::helpers`] call.
//!
//! # Exits
//!
//! Three flavours, all built by [`Tr::emit_exit`] (materialize the stack, write back the SSA
//! locals, return the exit word):
//! - *before* an op (`exit(pc, EXIT_RESUME)` with the op's operands in place): the interpreter
//!   re-executes it. Used whenever a guard fails before anything observable happened.
//! - *after* an op (`exit(pc + 1, EXIT_RESUME)`): the op ran (through a helper) but its result
//!   is not of the kind the following native code speculated on.
//! - *throw* (`exit(pc, EXIT_THROW)`): a helper threw; everything is already in memory.
//!
//! # Merges
//!
//! A basic block entered with a non-empty operand stack from more than one edge takes every
//! entry Boxed (the edges materialize unboxed entries); single-edge blocks inherit the exact
//! entries. SSA locals merge through the builder's variables.
//!
//! # Borrowed entries
//!
//! A `LoadLocal` of a Boxed slot, a captured variable (`LoadCap`), a free name that resolves to
//! a scope binding (`LoadName`), and `this` push an [`Entry::Ref`]: the *address* of the value
//! where it lives, with no clone. Consumers that only read (element / property reads, number
//! operands) use the address directly; every other consumer — and every exit or merge —
//! materializes a clone first ([`Tr::force`]). A slot or `this` address stays valid while the
//! slot is not written; a binding address only until JS runs, so env Refs that survive an op
//! which may run JS are materialized before it ([`Tr::prepare`]).
//!
//! # Region caches and facts
//!
//! Some run-time knowledge lives in IR variables that are reset (to 0) whenever it may have
//! become stale — after every helper that can run JS ([`Tr::call_js`]), and on writes to the
//! slot it depends on: binding addresses (`NamePtr` / `CapPtr` results), `Math` intrinsic
//! guards, and the bounds-check-elimination facts below.
//!
//! # Bounds-check elimination
//!
//! A loop test `i < a.length` (`i` a Num local, `a` a Boxed local, captured variable or binding)
//! records, next to the length it read, a view of `a`'s element storage (base, count, kind; see
//! [`layout::array_view`]) and whether `i` is an integer inside it. An element read `a[i]` while
//! that fact holds loads the element directly ([`layout::view_elem_num`]); the fact dies when
//! `i` or `a` is written or JS runs.

use super::helpers::{self, FastArg, Helper, MathFn};

#[path = "call.rs"]
mod call;
#[path = "inline.rs"]
mod inline;
#[path = "inline_this.rs"]
mod inline_this;
#[path = "new_plan.rs"]
pub(super) mod new_plan;
use super::layout;
use super::*;
use crate::bytecode::{ArithKind, CmpKind, Op, UpdKind};
use lumen_codegen::{
    BinaryOp, Block, ConvOp, FloatCC, FuncRef, Function, FunctionBuilder, IntCC, MemKind,
    Signature, Type, UnaryOp, Value as V, Variable,
};
use std::collections::{HashMap, HashSet};

/// A translated region.
pub(crate) struct Built {
    /// `(PTR frame) -> I64 exit word`.
    pub func: lumen_codegen::Function,
    /// Entries `frame.stack` must provide.
    pub max_stack: usize,
    /// The representation chosen for each slot (`chunk.n_slots` long); `Num`/`Bool` slots are
    /// guarded at entry.
    pub kinds: Vec<Kind>,
    /// Region handler sets referenced by `frame.exit_hset` (see the contract in [`super`]).
    pub hsets: Vec<Vec<(usize, usize)>>,
    /// Values the code's identity guards compare against (kept alive with the code).
    pub pins: Vec<Value>,
    /// External addresses, import id `EXT_BASE + index`.
    pub externs: Vec<u64>,
    /// Start pcs of the call sites guarded at every call (their guard exits there).
    pub guarded: Vec<usize>,
    /// Heap cells whose addresses the code embeds (see [`call`]).
    pub boxes: Vec<Box<dyn std::any::Any>>,
    /// The address of a `Cell<usize>` in `boxes` to receive the code's own entry address
    /// (read by direct self-calls that leave through the interpreter), or 0.
    pub self_entry: usize,
    /// Resume points `(pc, depth)` after the `await`s of function code (see [`Tr::entry`]).
    pub resumes: Vec<(usize, usize)>,
    /// Exit pcs (op pc + 1) of ops that speculated a Number result for their consumer (see
    /// [`Tr::want_num`]) and exit on anything else.
    pub num_exits: Vec<usize>,
}

/// Translate the loop `[header, backedge]` of `chunk`, specializing on the current `slots`.
/// `Err` (with a reason for `LUMEN_TIER_LOG`) when the region uses something unsupported.
pub(crate) fn build(
    interp: &Interp,
    env: &Env,
    chunk: &Chunk,
    header: usize,
    backedge: usize,
    slots: &[Value],
    this_val: &Value,
) -> Result<Built, String> {
    let an = analyze(chunk, header, backedge, false)?;
    let n_slots = chunk.n_slots;
    let mut kinds = vec![Kind::Boxed; n_slots];
    for s in 0..n_slots {
        if an.touched[s] {
            kinds[s] = slots.get(s).map(Kind::of).unwrap_or(Kind::Boxed);
        }
    }
    // Iterator-state slots stay Boxed: a protocol-free for-of state is a Number in the
    // iterator slot (see `bytecode::iter_fast`), stepped in memory by the helpers.
    for op in &chunk.ops[header..=backedge.min(chunk.ops.len() - 1)] {
        match *op {
            Op::IterStepL(a, b) | Op::IterRestL(a, b) => {
                kinds[a as usize] = Kind::Boxed;
                kinds[b as usize] = Kind::Boxed;
            }
            Op::IterCloseL(s) | Op::IterAbortL(s) => kinds[s as usize] = Kind::Boxed,
            _ => {}
        }
    }
    build_region(
        interp,
        env,
        chunk,
        header,
        backedge,
        slots,
        an,
        kinds,
        vec![false; n_slots],
        false,
        this_val,
    )
}

/// Translate the whole of `chunk` as function code entered at pc 0 (the whole-function tier):
/// the region is every op, the operand stack is empty and no handler is live at entry, and
/// `slots` holds the arguments of the call being entered. Slots that hold a value at entry
/// (parameters, `arguments`, the rest array) are specialized on it and guarded like a loop's
/// locals; locals that start out `undefined` are speculated per [`fn_kinds`]. `widen` forces
/// slots Boxed (an earlier speculation on them failed). Ops the translator does not handle
/// exit to the interpreter, which finishes the call.
/// Ops beyond which a function is left to the loop tier (compile time and code size).
const MAX_FN_OPS: usize = 4000;

pub(crate) fn build_fn(
    interp: &Interp,
    env: &Env,
    chunk: &Chunk,
    slots: &[Value],
    widen: &[bool],
    this_val: &Value,
) -> Result<Built, String> {
    let n = chunk.ops.len();
    if n == 0 {
        return Err("empty chunk".into());
    }
    if n > MAX_FN_OPS {
        return Err(format!("function too large ({n} ops)"));
    }
    let an = analyze(chunk, 0, n - 1, true)?;
    let (kinds, fresh) = fn_kinds(chunk, &an, slots, widen);
    build_region(interp, env, chunk, 0, n - 1, slots, an, kinds, fresh, true, this_val)
}

/// Function mode: the representation of each slot, and which `Num` slots start out
/// `undefined` (tracked by an [`Tr::undef`] flag instead of an entry guard).
///
/// Slots below `n_params` (and `arguments` / the rest array) hold their value at entry and take
/// its kind. Other locals are `undefined` at every entry; one is speculated `Num` when every
/// write stores a value that is likely a Number (a Number constant, arithmetic, an update, a
/// load of another such slot, an element / property read or a call result) and no op needs it
/// in memory. A wrong guess costs resume exits at the writes, after which the function
/// recompiles with the slot widened.
fn fn_kinds(chunk: &Chunk, an: &Analysis, slots: &[Value], widen: &[bool]) -> (Vec<Kind>, Vec<bool>) {
    let n_slots = chunk.n_slots;
    let ops = &chunk.ops;
    let widened = |s: usize| widen.get(s).copied().unwrap_or(false);
    let mut kinds = vec![Kind::Boxed; n_slots];
    let mut cand = vec![false; n_slots];
    for s in 0..n_slots {
        if !an.touched[s] || widened(s) {
            continue;
        }
        let at_entry = s < chunk.n_params
            || chunk.arguments_slot == Some(s as u16)
            || chunk.rest_slot == Some(s as u16);
        if at_entry {
            kinds[s] = slots.get(s).map(Kind::of).unwrap_or(Kind::Boxed);
        } else {
            cand[s] = true;
        }
    }
    let reach = |pc: usize| an.depth[pc].is_some();
    // Slots only memory-backed ops may touch.
    for pc in 0..ops.len() {
        if !reach(pc) {
            continue;
        }
        match ops[pc] {
            Op::GetPropLocal(s, ..)
            | Op::SetPropLocalDrop(s, ..)
            | Op::GetElemLocal(s)
            | Op::SetElemLocal(s)
            | Op::SetElemLocalDrop(s)
            | Op::ToPropKeyLocal(s)
            | Op::IterCloseL(s)
            | Op::IterAbortL(s) => cand[s as usize] = false,
            Op::IterStepL(a, b) | Op::IterRestL(a, b) => {
                cand[a as usize] = false;
                cand[b as usize] = false;
            }
            Op::ForInStepL(a, b, c) => {
                cand[a as usize] = false;
                cand[b as usize] = false;
                cand[c as usize] = false;
            }
            _ => {}
        }
    }
    let num_slot = |cand: &[bool], s: u16| cand[s as usize] || kinds[s as usize] == Kind::Num;
    loop {
        let mut changed = false;
        for pc in 0..ops.len() {
            if !reach(pc) {
                continue;
            }
            let (dst, ok) = match ops[pc] {
                Op::StoreLocal(s) if cand[s as usize] => {
                    // The producer of the stored value, in the same basic block.
                    let mut q = pc;
                    let ok = loop {
                        if q == 0 || an.leader[q] {
                            break false;
                        }
                        q -= 1;
                        break match ops[q] {
                            Op::Dup => continue,
                            Op::Const(k) => matches!(chunk.consts[k as usize], Value::Num(_)),
                            Op::Add
                            | Op::Sub
                            | Op::Mul
                            | Op::Div
                            | Op::Mod
                            | Op::BitAnd
                            | Op::BitOr
                            | Op::BitXor
                            | Op::Shl
                            | Op::Shr
                            | Op::UShr
                            | Op::Neg
                            | Op::Plus
                            | Op::BitNot
                            | Op::GetElem
                            | Op::GetElemLocal(_)
                            | Op::GetProp(..)
                            | Op::GetPropThis(..)
                            | Op::GetPropLocal(..)
                            | Op::Call(_)
                            | Op::CallWithThis(_) => true,
                            Op::UpdateLocal(_, k) => {
                                !matches!(k, UpdKind::IncDiscard | UpdKind::DecDiscard)
                            }
                            Op::LoadLocal(t) => num_slot(&cand, t),
                            _ => false,
                        };
                    };
                    (s, ok)
                }
                Op::ArithLL(k, d, a, b) if cand[d as usize] => (
                    d,
                    k != ArithKind::Add || (num_slot(&cand, a) && num_slot(&cand, b)),
                ),
                Op::ArithLK(k, d, a, c) if cand[d as usize] => (
                    d,
                    k != ArithKind::Add
                        || (num_slot(&cand, a)
                            && matches!(chunk.consts[c as usize], Value::Num(_))),
                ),
                _ => continue,
            };
            if !ok {
                cand[dst as usize] = false;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for s in 0..n_slots {
        if cand[s] {
            kinds[s] = Kind::Num;
        }
    }
    (kinds, cand)
}

#[allow(clippy::too_many_arguments)]
fn build_region(
    interp: &Interp,
    env: &Env,
    chunk: &Chunk,
    header: usize,
    backedge: usize,
    slots: &[Value],
    an: Analysis,
    kinds: Vec<Kind>,
    fresh: Vec<bool>,
    func_mode: bool,
    this_val: &Value,
) -> Result<Built, String> {
    let n_slots = chunk.n_slots;
    let mut kinds = kinds;
    if chunk.reflect_args && crate::bytecode::reflect::enabled() {
        // A reflective `f.arguments` read (from any call the code makes) sees the parameters'
        // current values in the slots: keep them there.
        for k in kinds.iter_mut().take(chunk.n_params) {
            *k = Kind::Boxed;
        }
    }
    if let Some(base) = chunk.virt_base {
        // A virtual `arguments` / rest object's count and elements are read by index from
        // memory (never written: see `Chunk::virt_base`).
        let s = chunk.arguments_slot.or(chunk.rest_slot).map_or(0, |s| s as usize);
        for k in (base as usize..chunk.n_params).chain([s]) {
            if let Some(k) = kinds.get_mut(k) {
                *k = Kind::Boxed;
            }
        }
    }
    let defd = Defd::new(chunk, header, backedge, &an, &fresh, None);
    let tdz_ssa: Vec<bool> = (0..n_slots)
        .map(|s| an.tdz[s] && kinds[s] != Kind::Boxed)
        .collect();
    let tdzd = Defd::new(chunk, header, backedge, &an, &tdz_ssa, Some(func_mode));
    let mut plan = plan(interp, env, chunk, header, backedge, slots, &an, &kinds, &defd, &tdzd);
    call::plan_direct(&mut plan, interp, env, chunk, header, backedge, slots, this_val, &an, &kinds, func_mode);
    // A `get` / `set` / ... site that became a direct JS call is a user method: keep that.
    {
        let (direct, dmethod) = (&plan.direct, &plan.dmethod);
        plan.colm
            .retain(|g, &mut (c, _)| !direct.contains_key(&c) && !dmethod.contains_key(g));
    }
    let mut an = an;
    let resumes_out = an.resumes.clone();
    let tail_loops = plan.tail_loops.len() as u32;
    an.back[0] += tail_loops;
    an.preds[0] += tail_loops;
    let num_exits;
    let pins = std::mem::take(&mut plan.pins);
    let mut boxes = std::mem::take(&mut plan.boxes);
    let externs = plan.externs.clone();
    let guarded: Vec<usize> = plan
        .math
        .iter()
        .filter(|(_, s)| s.slot.is_some())
        .map(|(&pc, _)| pc)
        .chain(plan.dname.keys().copied())
        .chain(plan.dmethod.keys().copied())
        .chain(plan.strm.keys().copied())
        .chain(plan.strn.keys().copied())
        .chain(plan.colm.keys().copied())
        .chain(plan.arrm.keys().copied())
        .collect();
    let self_cell: Box<std::cell::Cell<usize>> = Box::new(std::cell::Cell::new(0));
    let self_entry = &*self_cell as *const std::cell::Cell<usize> as usize;
    boxes.push(self_cell);

    let mut func = Function::new(
        if func_mode {
            format!("fn_{}", chunk.ops.len())
        } else {
            format!("loop_{header}_{backedge}")
        },
        Signature::new(vec![PTR], vec![Type::I64]),
    );
    let hsets;
    let max_stack = {
        let mut fb = FunctionBuilder::new(&mut func);
        let entry = fb.create_entry_block();
        let frame = fb.block_params(entry)[0];
        let slots_p = fb.load(PTR_MEM, frame, FRAME_SLOTS);
        let consts_p = fb.load(PTR_MEM, frame, FRAME_CONSTS);
        let stack_p = fb.load(PTR_MEM, frame, FRAME_STACK);

        let mut leaders: Vec<Option<Leader>> = Vec::with_capacity(an.depth.len());
        for i in 0..an.depth.len() {
            leaders.push(if an.leader[i] {
                let depth = an.depth[i].unwrap_or(0);
                Some(Leader {
                    block: fb.create_block(),
                    depth,
                    state: if depth == 0 {
                        Some(Vec::new())
                    } else if an.preds[i] > 1 {
                        Some(vec![Entry::Boxed; depth])
                    } else {
                        None
                    },
                    edges: 0,
                    visited: false,
                    back_left: an.back[i],
                    has_back: an.back[i] > 0,
                })
            } else {
                None
            });
        }

        let mut tr = Tr {
            pending_seal: Vec::new(),
            chunk,
            ops: &chunk.ops,
            header,
            backedge,
            fb,
            frame,
            slots: slots_p,
            stackp: stack_p,
            consts: consts_p,
            kinds,
            vars: vec![None; n_slots],
            tdz: vec![None; n_slots],
            undef: vec![None; n_slots],
            fresh,
            defd,
            tdzd,
            func_mode,
            helpers: vec![None; helpers::ALL.len()],
            leaders,
            stack: Vec::new(),
            max_stack: 1,
            plan,
            ptr_vars: HashMap::new(),
            arr_vars: HashMap::new(),
            idx_vars: HashMap::new(),
            math_vars: HashMap::new(),
            ta_vars: HashMap::new(),
            ta_len_vars: HashMap::new(),
            push_vars: HashMap::new(),
            ta_slots: (0..n_slots)
                .map(|s| match slots.get(s) {
                    Some(Value::Obj(o)) => interp
                        .typed_arrays
                        .contains_key(&(crate::value::Gc::as_ptr(o) as usize)),
                    _ => false,
                })
                .collect(),
            hs_cur: Vec::new(),
            skip: None,
            chaining: false,
            hsets: Vec::new(),
            err: None,
            self_fn: None,
            self_sizes: Vec::new(),
            self_entry,
            budget: None,
            call_flags: None,
            num_exits: Vec::new(),
        };
        tr.entry(&an);
        tr.body(&an)?;
        if let Some(e) = tr.err.take() {
            return Err(e);
        }
        let Tr {
            fb,
            max_stack,
            kinds: k,
            hsets: h,
            self_sizes,
            num_exits: ne,
            ..
        } = tr;
        num_exits = ne;
        fb.finish();
        kinds = k;
        hsets = h;
        // Direct self-calls reserve this code's own frame size, known only now.
        let size = frame_bytes(n_slots, max_stack) as i64;
        for v in self_sizes {
            if let lumen_codegen::ir::ValueDef::Result(inst, _) = func.values[v.index()].def {
                if let lumen_codegen::ir::InstData::Iconst { imm, .. } = &mut func.insts[inst.index()] {
                    *imm = size;
                }
            }
        }
        max_stack
    };
    Ok(Built {
        func,
        max_stack,
        kinds,
        hsets,
        pins,
        externs,
        guarded,
        boxes,
        self_entry,
        resumes: resumes_out,
        num_exits,
    })
}

// ---- static analysis ---------------------------------------------------------------------------

/// What the translator needs to know about the region before emitting anything. Vectors indexed
/// by `pc - header` unless noted.
struct Analysis {
    /// Operand-stack depth before the op; `None` = unreachable from the header.
    depth: Vec<Option<usize>>,
    /// Starts a basic block.
    leader: Vec<bool>,
    /// Incoming edges of a leader (the entry counts for the header).
    preds: Vec<u32>,
    /// Incoming backward edges of a leader.
    back: Vec<u32>,
    /// Per slot: read or written by a reachable op.
    touched: Vec<bool>,
    /// Per slot: put into its TDZ by a reachable `Tdz`.
    tdz: Vec<bool>,
    /// Region handlers live before the op, `(catch_pc, stack_depth)` outermost first (the
    /// handlers the interpreter had at entry are not included).
    hs: Vec<Vec<(usize, usize)>>,
    /// Function code: the op after each reachable `await` and the stack depth there — entries
    /// of the code (leaders with one more incoming edge, from the entry block).
    resumes: Vec<(usize, usize)>,
}

/// Whether control can continue to `pc + 1`, and the jump target.
fn successors(op: &Op) -> (bool, Option<usize>) {
    let falls = !matches!(
        op,
        Op::Jump(_)
            | Op::Return
            | Op::ReturnUndef
            | Op::Throw
            | Op::IterAbortL(_)
            | Op::DerivedReturn
    );
    // A `PushHandler`'s catch pc is no jump: its throw edge is added separately (with the
    // exception pushed and the handler popped).
    let target = match op {
        Op::PushHandler(_) => None,
        _ => crate::jit_ir::jump_target(op),
    };
    (falls, target)
}

/// Function mode: for each pc, the `fresh` slots (see [`fn_kinds`]) definitely written before
/// it on every path from the entry — their `undefined` flag is known clear there, so reads need
/// no check. Catch pads know nothing (conservative). Every translated use of a fresh (`Num`)
/// slot either writes it or exits unless it is written, so each such op defines it.
struct Defd {
    /// Bit index of each slot (`usize::MAX` = not fresh).
    idx: Vec<usize>,
    /// Per region pc (offset by `header`), the definitely-written bit set.
    sets: Vec<Vec<u64>>,
    header: usize,
}

impl Defd {
    /// `tdz`: track TDZ flags instead — a `Tdz` op sets the slot's flag, and in function mode
    /// every flag starts clear (slots hold `undefined` at entry).
    fn new(
        chunk: &Chunk,
        header: usize,
        backedge: usize,
        an: &Analysis,
        fresh: &[bool],
        tdz: Option<bool>,
    ) -> Defd {
        let mut idx = vec![usize::MAX; fresh.len()];
        let mut nf = 0;
        for (s, &f) in fresh.iter().enumerate() {
            if f {
                idx[s] = nf;
                nf += 1;
            }
        }
        if nf == 0 {
            return Defd {
                idx,
                sets: Vec::new(),
                header,
            };
        }
        let words = nf.div_ceil(64);
        let n = backedge - header + 1;
        let ops = &chunk.ops;
        let inr = |q: usize| q >= header && q <= backedge && an.depth[q - header].is_some();
        let mut catch = vec![false; n];
        for pc in header..=backedge {
            if let Op::PushHandler(t) = ops[pc] {
                if inr(t as usize) {
                    catch[t as usize - header] = true;
                }
            }
        }
        let mut sets: Vec<Option<Vec<u64>>> = vec![None; n];
        let mut queued = vec![false; n];
        let mut work = vec![header];
        sets[0] = Some(if tdz == Some(true) {
            vec![u64::MAX; words]
        } else {
            vec![0; words]
        });
        queued[0] = true;
        while let Some(pc) = work.pop() {
            queued[pc - header] = false;
            let mut out = sets[pc - header].clone().expect("queued pcs have a set");
            if let (Op::Tdz(sl), Some(_)) = (ops[pc], tdz) {
                let k = idx.get(sl as usize).copied().unwrap_or(usize::MAX);
                if k != usize::MAX {
                    out[k / 64] &= !(1 << (k % 64));
                }
            } else if !matches!(ops[pc], Op::Tdz(_)) {
                for sl in op_slots(&ops[pc]) {
                    let k = idx.get(sl as usize).copied().unwrap_or(usize::MAX);
                    if k != usize::MAX {
                        out[k / 64] |= 1 << (k % 64);
                    }
                }
            }
            let (falls, target) = successors(&ops[pc]);
            let mut succ: Vec<(usize, bool)> = falls
                .then_some(pc + 1)
                .into_iter()
                .chain(target)
                .map(|q| (q, false))
                .collect();
            // A catch pad is entered by throws: it knows nothing.
            if let Op::PushHandler(t) = ops[pc] {
                succ.push((t as usize, true));
            }
            for (q, _) in succ {
                if !inr(q) {
                    continue;
                }
                let r = q - header;
                let changed = if catch[r] {
                    if sets[r].is_none() {
                        sets[r] = Some(vec![0; words]);
                        true
                    } else {
                        false
                    }
                } else {
                    match &mut sets[r] {
                        None => {
                            sets[r] = Some(out.clone());
                            true
                        }
                        Some(cur) => {
                            let mut ch = false;
                            for (c, o) in cur.iter_mut().zip(&out) {
                                let v = *c & *o;
                                ch |= v != *c;
                                *c = v;
                            }
                            ch
                        }
                    }
                };
                if changed && !queued[r] {
                    queued[r] = true;
                    work.push(q);
                }
            }
        }
        Defd {
            idx,
            sets: sets.into_iter().map(|s| s.unwrap_or_default()).collect(),
            header,
        }
    }

    /// Whether fresh slot `s` may still hold its entry `undefined` before the op at `pc`.
    fn maybe_undef(&self, s: usize, pc: usize) -> bool {
        let k = self.idx.get(s).copied().unwrap_or(usize::MAX);
        if k == usize::MAX {
            return false;
        }
        match self.sets.get(pc.wrapping_sub(self.header)) {
            Some(set) if !set.is_empty() => set[k / 64] & (1 << (k % 64)) == 0,
            _ => true,
        }
    }
}

/// The slots `op` reads or writes.
pub(crate) fn op_slots(op: &Op) -> Vec<u16> {
    match *op {
        Op::LoadLocal(s)
        | Op::StoreLocal(s)
        | Op::UpdateLocal(s, _)
        | Op::Tdz(s)
        | Op::GetPropLocal(s, ..)
        | Op::SetPropLocalDrop(s, ..)
        | Op::GetElemLocal(s)
        | Op::SetElemLocal(s)
        | Op::SetElemLocalDrop(s)
        | Op::ToPropKeyLocal(s)
        | Op::IterCloseL(s)
        | Op::IterAbortL(s) => vec![s],
        Op::IterStepL(a, b) | Op::IterRestL(a, b) | Op::JumpIfNotCmpLL(_, a, b, _) => vec![a, b],
        Op::JumpIfNotCmpLK(_, a, ..) => vec![a],
        Op::ArithLL(_, d, a, b) => vec![d, a, b],
        Op::ArithLK(_, d, a, _) => vec![d, a],
        Op::ForInStepL(a, b, c) => vec![a, b, c],
        _ => Vec::new(),
    }
}

fn analyze(chunk: &Chunk, header: usize, backedge: usize, func: bool) -> Result<Analysis, String> {
    let ops = &chunk.ops;
    if header > backedge || backedge >= ops.len() {
        return Err(format!("bad region {header}..={backedge}"));
    }
    if !func && !matches!(ops[backedge], Op::Jump(t) if t as usize == header) {
        return Err(format!(
            "op at {backedge} is not the backward jump to {header}"
        ));
    }
    let inr = |pc: usize| pc >= header && pc <= backedge;
    let n = backedge - header + 1;
    let mut depth: Vec<Option<usize>> = vec![None; n];
    let mut hs: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
    depth[0] = Some(0);
    let mut work = vec![header];
    while let Some(pc) = work.pop() {
        let op = &ops[pc];
        match op {
            // Function code leaves it to the interpreter (an exit before it).
            Op::ForInStepL(..) if !func => {
                return Err(format!("iteration op {op:?} at {pc}"));
            }
            _ => {}
        }
        let d = depth[pc - header].expect("queued ops have a depth");
        let (pops, pushes) = chunk
            .jit_stack_effect(pc)
            .ok_or_else(|| format!("no stack effect for {op:?} at {pc}"))?;
        if d < pops {
            return Err(format!("stack underflow at {pc}"));
        }
        let after = d - pops + pushes;
        let h = hs[pc - header].clone();
        if matches!(op, Op::Await) && (!h.is_empty() || !func) {
            // Under a region handler, the exit's handlers would be finished by a nested driver
            // the suspension leaves. A loop would enter and exit its code once per iteration
            // (until resume entries exist).
            return Err(format!("await at {pc} inside a region handler or loop region"));
        }
        let mut h_after = h.clone();
        // (successor pc, depth, handlers): the op's own edges, plus the throw edge of a region
        // handler to its catch pad.
        let mut edges: Vec<(usize, usize, Vec<(usize, usize)>)> = Vec::new();
        match *op {
            Op::PushHandler(t) => {
                h_after.push((t as usize, d));
                edges.push((t as usize, d + 1, h.clone()));
            }
            Op::PopHandler => {
                if h_after.pop().is_none() {
                    return Err(format!("PopHandler at {pc} pops a handler from outside the loop"));
                }
            }
            _ => {}
        }
        let (falls, target) = successors(op);
        for q in falls.then_some(pc + 1).into_iter().chain(target) {
            edges.push((q, after, h_after.clone()));
        }
        for (q, dq, hq) in edges {
            if !inr(q) {
                continue;
            }
            match depth[q - header] {
                None => {
                    depth[q - header] = Some(dq);
                    hs[q - header] = hq;
                    work.push(q);
                }
                Some(e) if e != dq => {
                    return Err(format!("inconsistent stack depth at {q}"));
                }
                Some(_) if hs[q - header] != hq => {
                    return Err(format!("inconsistent handlers at {q}"));
                }
                Some(_) => {}
            }
        }
    }
    if !hs[0].is_empty() {
        return Err("handlers at the loop header".into());
    }

    let mut leader = vec![false; n];
    let mut preds = vec![0u32; n];
    let mut back = vec![0u32; n];
    let mut touched = vec![false; chunk.n_slots];
    let mut tdz = vec![false; chunk.n_slots];
    leader[0] = true;
    preds[0] = 1;
    for pc in header..=backedge {
        if depth[pc - header].is_none() {
            continue;
        }
        let op = &ops[pc];
        for s in op_slots(op) {
            let s = s as usize;
            if s >= chunk.n_slots {
                return Err(format!("slot {s} out of range at {pc}"));
            }
            touched[s] = true;
            if matches!(op, Op::Tdz(_)) {
                tdz[s] = true;
            }
        }
        if let Op::PushHandler(t) = *op {
            // A catch pad in the region: entered by throws (any number), always Boxed.
            let t = t as usize;
            if inr(t) {
                if t <= pc {
                    return Err(format!("catch pad {t} before its try at {pc}"));
                }
                leader[t - header] = true;
                preds[t - header] += 2;
            }
        }
        let (falls, target) = successors(op);
        if let Some(t) = target.filter(|&t| inr(t)) {
            leader[t - header] = true;
            preds[t - header] += 1;
            if t <= pc {
                back[t - header] += 1;
            }
        }
        if (target.is_some() || !falls) && inr(pc + 1) {
            leader[pc + 1 - header] = true;
        }
        if falls && inr(pc + 1) {
            preds[pc + 1 - header] += 1;
        }
    }
    // An async body's code pays an entry and an exit per stretch it runs: only worth it when
    // the stretch from pc 0 to the first `await` has a loop (ops reachable without passing an
    // `await`; see also `resume_on`).
    let awaits = func
        && (header..=backedge).any(|pc| depth[pc - header].is_some() && matches!(ops[pc], Op::Await));
    if awaits {
        let mut seen = vec![false; n];
        let mut work = vec![header];
        let mut looped = false;
        while let Some(pc) = work.pop() {
            if std::mem::replace(&mut seen[pc - header], true) || matches!(ops[pc], Op::Await) {
                continue;
            }
            let (falls, target) = successors(&ops[pc]);
            for q in falls.then_some(pc + 1).into_iter().chain(target) {
                if inr(q) {
                    looped |= q <= pc;
                    work.push(q);
                }
            }
        }
        if !looped {
            return Err("async body without a loop before its first await".into());
        }
    }
    // Resume points: the op after each `await` (no handler is live there, see above).
    let mut resumes = Vec::new();
    if awaits && resume_on() {
        for pc in header..backedge {
            if matches!(ops[pc], Op::Await) {
                if let Some(d) = depth[pc + 1 - header] {
                    leader[pc + 1 - header] = true;
                    preds[pc + 1 - header] += 1;
                    resumes.push((pc + 1, d));
                }
            }
        }
    }
    // `InEnv` runs together with the op after it: nothing may enter between them.
    for pc in header..=backedge {
        if matches!(ops[pc], Op::InEnv(_))
            && depth[pc - header].is_some()
            && (!inr(pc + 1) || leader[pc + 1 - header])
        {
            return Err(format!("InEnv at {pc} split from its op"));
        }
    }
    Ok(Analysis {
        depth,
        leader,
        preds,
        back,
        touched,
        tdz,
        hs,
        resumes,
    })
}

/// Where a borrowed ([`Entry::Ref`]) value lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Src {
    /// A Boxed frame slot: valid until the slot is written.
    Slot(u16),
    /// The binding free name `names[n]` resolves to: valid until JS runs.
    Name(u32),
    /// Captured binding `names[n]` in the activation env: valid until JS runs.
    Cap(u32),
    /// [`Src::Name`] proven mutable (a store target): valid until JS runs.
    NameW(u32),
    /// The global object's data property free name `names[n]` resolves to (a `Property`
    /// address, never a `Ref`): valid until JS runs.
    Glob(u32),
    /// `frame.this_val`: valid for the whole region.
    This,
    /// An object read out of a property without taking a reference, in the stack entry that
    /// holds it (see [`Tr::chain_read`]): valid until anything but its one consumer runs.
    Borrow,
    /// A callee `Value` the code itself keeps alive (a direct call site's resolved callee, see
    /// [`call`]): valid for the code's lifetime.
    Pin,
}

impl Src {
    /// Whether JS running elsewhere can change or move the value.
    fn in_env(self) -> bool {
        matches!(
            self,
            Src::Name(_) | Src::Cap(_) | Src::NameW(_) | Src::Glob(_) | Src::Borrow
        )
    }
}

/// What the translator decides before emitting anything (see [`plan`]).
#[derive(Default)]
struct Plan {
    /// `LoadName` sites translated as a borrowed binding address.
    name_ref: HashSet<usize>,
    /// `LoadName` sites read inline from a global object property ([`Src::Glob`]).
    glob_ref: HashSet<usize>,
    /// The `glob_ref` sites whose property held a Number at translation: read as one (any
    /// other value exits).
    glob_num: HashSet<usize>,
    /// `StoreNameCached` sites stored inline: to a binding ([`Src::NameW`]) or a global object
    /// property ([`Src::Glob`]).
    name_store: HashMap<usize, Src>,
    /// Name / capture addresses cached in region variables.
    ptr_srcs: Vec<Src>,
    /// Arrays whose element storage view is tracked (bounds-check elimination).
    arr_srcs: Vec<Src>,
    /// `(array, index slot)` pairs with an in-bounds fact.
    idx_facts: Vec<(Src, u16)>,
    /// Inline call sites (`Math` intrinsics and direct `#[op(fast)]` calls): first pc of the
    /// callee part → the site.
    math: HashMap<usize, Site>,
    /// `Math.f(..)` sites nested in another site's arguments (`Math.floor(Math.sqrt(x))`):
    /// inner site pc -> outer site pc. The outer site's guard also checks the inner one, so
    /// the inner site never exits (the outer placeholders are on the stack).
    nested: HashMap<usize, usize>,
    /// Direct fast calls, by [`Intr::Fast`] index.
    fast: Vec<FastSite>,
    /// Inlined callees, by [`Intr::Inline`] index.
    inl: Vec<inline::InlSite>,
    /// Callees the fast sites' guards compare against.
    pins: Vec<Value>,
    /// Fast entries' addresses (import id `EXT_BASE + index`).
    externs: Vec<u64>,
    /// Direct call sites, by call pc (see [`call`]).
    direct: HashMap<usize, call::DSite>,
    /// Name loads producing a direct site's callee (guarded by identity at the load): load pc
    /// -> call pc.
    dname: HashMap<usize, usize>,
    /// By-name callee producers whose name cache is in global-object mode at plan time: the
    /// global object's box address (pinned in `boxes`), its shape and the entry slot.
    dglobal: HashMap<usize, (usize, u32, u32)>,
    /// `GetMethod`s producing a direct site's callee, validated inline: op pc -> call pc.
    dmethod: HashMap<usize, usize>,
    /// Direct method sites whose body is inlined (see [`inline_this`]), by call pc.
    dthis: HashMap<usize, inline_this::ThisBody>,
    /// Property reads resolved to an inlined getter, by op pc.
    dget: HashMap<usize, inline_this::Getter>,
    /// Property stores resolved to an inlined setter, by op pc.
    dset: HashMap<usize, inline_this::Getter>,
    /// Property reads and stores resolved to a direct accessor call, by op pc.
    dacc: HashMap<usize, call::AccSite>,
    /// `new` sites built from their constructor's template (see [`new_plan`]), by pc.
    dnew: HashMap<usize, new_plan::NewSite>,
    /// Inline `String.prototype` intrinsic sites (`<string> GetMethod <Number>
    /// CallWithThis(1)`): `GetMethod` pc -> call pc.
    strm: HashMap<usize, usize>,
    /// Fused `String.prototype` method sites (`<string> GetMethod <pure args>
    /// CallWithThis(n)`, see `Tr::strn_begin`): `GetMethod` pc -> (call pc, address of the
    /// site's `Cell<helpers::StrSite>` in `boxes`).
    strn: HashMap<usize, (usize, usize)>,
    /// Fused Map/Set method sites (`<object>.<get|has|set|add>(<pure args>)`, see
    /// `Tr::colm_begin`): `GetMethod` pc -> (call pc, address of the site's `CollSite` cell).
    colm: HashMap<usize, (usize, usize)>,
    /// Inline `Array.prototype.push` sites (`<array> GetMethod(push) <value>
    /// CallWithThis(1)`): `GetMethod` pc -> call pc.
    arrm: HashMap<usize, usize>,
    /// Heap cells the direct sites' code embeds addresses of.
    boxes: Vec<Box<dyn std::any::Any>>,
    /// Direct self tail-call sites translated as a jump back to pc 0 (see `Tr::self_tail`),
    /// by call pc: each is a backward edge of the pc 0 leader.
    tail_loops: HashSet<usize>,
}

/// What an inline call site computes.
#[derive(Clone, Copy, Debug)]
enum Intr {
    Math(MathFn),
    /// Index into [`Plan::fast`].
    Fast(usize),
    /// Index into [`Plan::inl`].
    Inline(usize),
}

/// An inline call site: `LoadName GetMethod <args> CallWithThis` (`lfc == false`) or
/// `LoadNameForCall <args> CallWithThis` (`lfc`), the arguments pure unboxed operations, so no
/// placeholder of the callee part is ever materialized.
#[derive(Clone, Copy, Debug)]
struct Site {
    f: Intr,
    /// The `CallWithThis` pc.
    call: usize,
    lfc: bool,
    /// `LoadLocal(slot) <args> Call`: an inlined callee held in a (Boxed) local, guarded at
    /// every call (see [`Helper::SlotGuard`]).
    slot: Option<u16>,
}

/// A direct call of a `#[op(fast)]` entry.
#[derive(Clone, Debug)]
struct FastSite {
    args: Vec<FastArg>,
    ret: FastArg,
    /// The callee's object address (guarded, see [`Helper::FastGuard`]).
    expect: usize,
    /// Index into [`Plan::externs`].
    ext: usize,
}

/// Whether name cache `c` currently resolves through a scope binding (the modes
/// [`Helper::NamePtr`] can return an address for).
fn name_scope_mode(chunk: &Chunk, c: u32) -> bool {
    chunk.name_site_kind(c) == Some(false)
}

fn plan(
    interp: &Interp,
    env: &Env,
    chunk: &Chunk,
    header: usize,
    backedge: usize,
    slots: &[Value],
    an: &Analysis,
    kinds: &[Kind],
    defd: &Defd,
    tdzd: &Defd,
) -> Plan {
    let ops = &chunk.ops;
    let reach = |pc: usize| an.depth[pc - header].is_some();
    let lead = |pc: usize| an.leader[pc - header];
    let num_slot = |s: u16| kinds[s as usize] == Kind::Num;
    let is_len = |n: u32| chunk.names.get(n as usize).is_some_and(|x| &**x == "length");
    let mut p = Plan::default();
    let add_ptr = |p: &mut Plan, s: Src| {
        if !p.ptr_srcs.contains(&s) {
            p.ptr_srcs.push(s);
        }
    };

    // The arguments of an inline call site: pure unboxed operations in one basic block from
    // `from`, up to the `CallWithThis`. Returns the call pc and the argument kinds (true = Num,
    // false = Bool) when the whole window qualifies.
    // `plain`: the window ends in a `Call` (a callee without receiver) instead.
    // `nest`: the arguments may hold `Math.g(<Number args>)` sites (one level deep), each
    // collected into the third result by its `LoadName` pc.
    let site_args_n = |from: usize, plain: bool, nest: bool| -> Option<(usize, Vec<bool>, Vec<usize>)> {
        let mut st: Vec<bool> = Vec::new();
        let mut outer: Option<(Vec<bool>, usize, usize)> = None;
        let mut inner: Vec<usize> = Vec::new();
        let mut q = from;
        while q <= backedge {
            if lead(q) {
                return None;
            }
            match ops[q] {
                Op::LoadName(..) if nest && outer.is_none() && q + 1 <= backedge && !lead(q + 1) => {
                    let Op::GetMethod(m, _) = ops[q + 1] else { return None };
                    let f = helpers::math_intrinsic(chunk.names.get(m as usize)?)?;
                    if helpers::math_site_failed(chunk, q) {
                        return None;
                    }
                    outer = Some((std::mem::take(&mut st), q, f.arity()));
                    q += 2;
                    continue;
                }
                Op::CallWithThis(argc) if outer.is_some() => {
                    let (prev, at, arity) = outer.take().expect("nested site");
                    if argc as usize != st.len() || st.len() != arity || !st.iter().all(|&n| n) {
                        return None;
                    }
                    st = prev;
                    st.push(true);
                    inner.push(at);
                }
                Op::CallWithThis(argc) if !plain => {
                    return (argc as usize == st.len()).then_some((q, st, inner));
                }
                Op::Call(argc) if plain && outer.is_none() => {
                    return (argc as usize == st.len()).then_some((q, st, inner));
                }
                Op::LoadLocal(s)
                    if !tdzd.maybe_undef(s as usize, q)
                        && !defd.maybe_undef(s as usize, q)
                        && kinds[s as usize] != Kind::Boxed =>
                {
                    st.push(num_slot(s))
                }
                Op::Const(k) => match chunk.consts[k as usize] {
                    Value::Num(_) => st.push(true),
                    Value::Bool(_) => st.push(false),
                    _ => return None,
                },
                Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Div
                | Op::Mod
                | Op::BitAnd
                | Op::BitOr
                | Op::BitXor
                | Op::Shl
                | Op::Shr
                | Op::UShr
                    if st.len() >= 2 && st[st.len() - 1] && st[st.len() - 2] =>
                {
                    st.pop();
                }
                Op::Neg | Op::Plus | Op::BitNot if st.last() == Some(&true) => {}
                _ => return None,
            }
            q += 1;
        }
        None
    };
    let site_args = |from: usize, plain: bool| -> Option<(usize, Vec<bool>)> {
        site_args_n(from, plain, false).map(|(c, st, _)| (c, st))
    };
    // An inline `Math.f(args)` (`LoadName GetMethod <Number args> CallWithThis(arity)`) or a
    // direct call of a `#[op(fast)]` function resolved from a name (`ns.f(args)` or `f(args)`)
    // whose arguments match its unboxed signature, in one basic block, where nothing between
    // can exit (the receiver/callee entries are placeholders that must never be materialized).
    let site = |p: &mut Plan, pc: usize| -> Option<Site> {
        if helpers::math_site_failed(chunk, pc) {
            return None;
        }
        let lfc = match (ops[pc], ops.get(pc + 1).copied()) {
            (Op::LoadName(..), Some(Op::GetMethod(..))) if pc + 1 <= backedge && !lead(pc + 1) => {
                false
            }
            (Op::LoadNameForCall(..), _) => true,
            _ => return None,
        };
        let (call, args, inner) = site_args_n(pc + if lfc { 1 } else { 2 }, false, true)?;
        if !inner.is_empty() {
            // Nested sites: only for an outer `Math` intrinsic (its guard checks them too).
            let (false, Op::GetMethod(m, _)) = (lfc, ops[pc + 1]) else { return None };
            let f = helpers::math_intrinsic(chunk.names.get(m as usize)?)?;
            if args.len() != f.arity() || !args.iter().all(|&n| n) {
                return None;
            }
            for q in inner {
                p.nested.insert(q, pc);
            }
        }
        if let (false, Op::GetMethod(m, _)) = (lfc, ops[pc + 1]) {
            if let Some(f) = helpers::math_intrinsic(chunk.names.get(m as usize)?) {
                return (args.len() == f.arity() && args.iter().all(|&n| n)).then_some(Site {
                    f: Intr::Math(f),
                    call,
                    lfc,
                    slot: None,
                });
            }
        }
        let callee = helpers::site_callee(interp, chunk, env, pc)?;
        let Some((sig_args, ret, addr)) = helpers::fast_sig(interp, &callee) else {
            // A small pure user function: inline its body.
            let (ichunk, shape) = inline::candidate(interp, &callee, &args)?;
            let Value::Obj(o) = &callee else { return None };
            let expect = crate::value::Gc::as_ptr(o) as usize;
            p.pins.push(callee);
            p.inl.push(inline::InlSite {
                chunk: ichunk,
                shape,
                argc: args.len(),
                expect,
            });
            return Some(Site {
                f: Intr::Inline(p.inl.len() - 1),
                call,
                lfc,
                slot: None,
            });
        };
        if sig_args.len() != args.len()
            || sig_args.iter().zip(&args).any(|(k, &num)| match k {
                FastArg::F64 | FastArg::I32 | FastArg::U32 => !num,
                FastArg::Bool => num,
                FastArg::Void => true,
            })
        {
            return None;
        }
        let Value::Obj(o) = &callee else { return None };
        let expect = crate::value::Gc::as_ptr(o) as usize;
        let ext = match p.externs.iter().position(|&a| a == addr) {
            Some(k) => k,
            None => {
                p.externs.push(addr);
                p.externs.len() - 1
            }
        };
        p.pins.push(callee);
        p.fast.push(FastSite {
            args: sig_args,
            ret,
            expect,
            ext,
        });
        Some(Site {
            f: Intr::Fast(p.fast.len() - 1),
            call,
            lfc,
            slot: None,
        })
    };

    // `LoadLocal(s) <args> Call(argc)` with a small pure function in Boxed local `s` now: inline
    // it, guarded at every call on the local still holding a function with that body.
    let slot_site = |p: &mut Plan, pc: usize, s: u16| -> Option<Site> {
        let s = s as usize;
        if helpers::math_site_failed(chunk, pc) || kinds.get(s) != Some(&Kind::Boxed) {
            return None;
        }
        let callee = slots.get(s)?;
        if !matches!(callee, Value::Obj(_)) {
            return None;
        }
        let (call, args) = site_args(pc + 1, true)?;
        let (ichunk, shape) = inline::candidate(interp, callee, &args)?;
        // The payload word as stored in the slot (the fast identity check compares it).
        // SAFETY: a `Value` is `VALUE_SIZE` bytes with its payload at `VALUE_PAYLOAD`.
        let expect = unsafe {
            *(callee as *const Value as *const u8)
                .add(VALUE_PAYLOAD as usize)
                .cast::<u64>()
        } as usize;
        p.pins.push(callee.clone());
        p.inl.push(inline::InlSite {
            chunk: ichunk,
            shape,
            argc: args.len(),
            expect,
        });
        Some(Site {
            f: Intr::Inline(p.inl.len() - 1),
            call,
            lfc: false,
            slot: Some(s as u16),
        })
    };

    for pc in header..=backedge {
        if !reach(pc) {
            continue;
        }
        match ops[pc] {
            Op::LoadLocal(s) => {
                if let Some(m) = slot_site(&mut p, pc, s) {
                    p.math.insert(pc, m);
                }
            }
            Op::LoadNameForCall(..) => {
                if let Some(m) = site(&mut p, pc) {
                    p.math.insert(pc, m);
                }
            }
            Op::LoadName(n, c) => {
                if let Some(m) = site(&mut p, pc) {
                    p.math.insert(pc, m);
                } else if name_scope_mode(chunk, c) {
                    p.name_ref.insert(pc);
                    add_ptr(&mut p, Src::Name(n));
                } else if chunk.name_site_kind(c) == Some(true) {
                    p.glob_ref.insert(pc);
                    add_ptr(&mut p, Src::Glob(n));
                    let num = chunk.name_global_slot(interp, env, c).is_some_and(|s| {
                        interp.global.try_borrow().is_ok_and(|g| {
                            g.props.entry_at(s).is_some_and(|e| matches!(e.value(), Value::Num(_)))
                        })
                    });
                    if num {
                        p.glob_num.insert(pc);
                    }
                }
            }
            Op::StoreNameCached(n, c) => {
                let src = match chunk.name_site_kind(c) {
                    Some(false) => Src::NameW(n),
                    Some(true) => Src::Glob(n),
                    None => continue,
                };
                p.name_store.insert(pc, src);
                add_ptr(&mut p, src);
            }
            // `++name` on a scope binding: in place through the writable binding pointer.
            Op::UpdateNameCached(n, c, _) if chunk.name_site_kind(c) == Some(false) => {
                p.name_store.insert(pc, Src::NameW(n));
                add_ptr(&mut p, Src::NameW(n));
            }
            Op::LoadCap(n) | Op::UpdateCap(n, _) | Op::StoreCap(n) => {
                add_ptr(&mut p, Src::Cap(n));
            }
            _ => {}
        }
    }

    // A nested site must itself be an inline `Math` site; else its outer site goes too.
    let bad: Vec<usize> = p
        .nested
        .iter()
        .filter(|(q, _)| {
            !p.math
                .get(q)
                .is_some_and(|s| matches!(s.f, Intr::Math(_)) && !s.lfc && s.slot.is_none())
        })
        .map(|(_, &o)| o)
        .collect();
    for o in bad {
        p.math.remove(&o);
        p.nested.retain(|_, v| *v != o);
    }

    // `<string>.charCodeAt(<Number>)`: an inline `String.prototype` intrinsic, its receiver
    // and method guarded at the `GetMethod` (see `Tr::str_begin`).
    for pc in header..=backedge {
        if !reach(pc)
            || helpers::math_site_failed(chunk, pc)
            || p.math.contains_key(&pc.wrapping_sub(1))
        {
            continue;
        }
        let Op::GetMethod(m, _) = ops[pc] else { continue };
        if chunk.names.get(m as usize).and_then(|n| helpers::str_intrinsic(n)).is_none() {
            continue;
        }
        if let Some((call, args)) = site_args(pc + 1, false) {
            if args == [true] {
                p.strm.insert(pc, call);
            }
        }
    }

    // `<string>.<method>(<args>)` for any other native `String.prototype` method, its
    // arguments pure loads: the receiver checked a String at the `GetMethod`, the method read
    // and called by one helper at the call (see `Tr::strn_begin`, `helpers::str_method`).
    for pc in header..=backedge {
        if !reach(pc)
            || p.strm.contains_key(&pc)
            || helpers::math_site_failed(chunk, pc)
            || p.math.contains_key(&pc.wrapping_sub(1))
        {
            continue;
        }
        let Op::GetMethod(m, _) = ops[pc] else { continue };
        if !chunk
            .names
            .get(m as usize)
            .is_some_and(|n| helpers::str_native_method(interp, n))
        {
            continue;
        }
        // Each argument entry: whether it is known a Number (Number arithmetic runs no JS).
        let mut st: Vec<bool> = Vec::new();
        let mut q = pc + 1;
        let call = loop {
            if q > backedge || lead(q) || st.len() > 4 {
                break None;
            }
            match ops[q] {
                Op::CallWithThis(n) => break (n as usize == st.len()).then_some(q),
                Op::LoadLocal(s)
                    if !tdzd.maybe_undef(s as usize, q) && !defd.maybe_undef(s as usize, q) =>
                {
                    st.push(num_slot(s))
                }
                Op::Const(k) => st.push(matches!(chunk.consts[k as usize], Value::Num(_))),
                Op::Undef => st.push(false),
                Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Div
                | Op::Mod
                | Op::BitAnd
                | Op::BitOr
                | Op::BitXor
                | Op::Shl
                | Op::Shr
                | Op::UShr
                    if st.len() >= 2 && st[st.len() - 1] && st[st.len() - 2] =>
                {
                    st.pop();
                }
                Op::Neg | Op::Plus | Op::BitNot if st.last() == Some(&true) => {}
                _ => break None,
            }
            q += 1;
        };
        if let Some(call) = call {
            let cell: Box<std::cell::Cell<helpers::StrSite>> = Box::default();
            let addr = &*cell as *const std::cell::Cell<helpers::StrSite> as usize;
            p.boxes.push(cell);
            p.strn.insert(pc, (call, addr));
        }
    }

    // `<object>.<get|has|set|add>(<args>)` (Map / Set methods), its arguments pure loads: the
    // receiver checked an object at the `GetMethod`, the method read and (for the intrinsic
    // collection methods) run inline by one helper at the call (see `Tr::colm_begin`,
    // `helpers::coll_method`).
    for pc in header..=backedge {
        if !reach(pc)
            || p.strm.contains_key(&pc)
            || p.strn.contains_key(&pc)
            || helpers::math_site_failed(chunk, pc)
            || p.math.contains_key(&pc.wrapping_sub(1))
        {
            continue;
        }
        let Op::GetMethod(m, _) = ops[pc] else { continue };
        if !chunk
            .names
            .get(m as usize)
            .is_some_and(|n| crate::builtins::collections::lookup::coll_fast_name(n))
        {
            continue;
        }
        // Each argument entry: whether it is known a Number (Number arithmetic runs no JS).
        let mut st: Vec<bool> = Vec::new();
        let mut q = pc + 1;
        let call = loop {
            if q > backedge || lead(q) || st.len() > 2 {
                break None;
            }
            match ops[q] {
                Op::CallWithThis(n) => break (n as usize == st.len() && n > 0).then_some(q),
                Op::LoadLocal(s)
                    if !tdzd.maybe_undef(s as usize, q) && !defd.maybe_undef(s as usize, q) =>
                {
                    st.push(num_slot(s))
                }
                Op::Const(k) => st.push(matches!(chunk.consts[k as usize], Value::Num(_))),
                Op::Undef => st.push(false),
                Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Div
                | Op::Mod
                | Op::BitAnd
                | Op::BitOr
                | Op::BitXor
                | Op::Shl
                | Op::Shr
                | Op::UShr
                    if st.len() >= 2 && st[st.len() - 1] && st[st.len() - 2] =>
                {
                    st.pop();
                }
                Op::Neg | Op::Plus | Op::BitNot if st.last() == Some(&true) => {}
                _ => break None,
            }
            q += 1;
        };
        if let Some(call) = call {
            let cell: Box<std::cell::Cell<helpers::CollSite>> = Box::default();
            let addr = &*cell as *const std::cell::Cell<helpers::CollSite> as usize;
            p.boxes.push(cell);
            p.colm.insert(pc, (call, addr));
        }
    }

    // `<array>.push(<value>)`: the intrinsic push, receiver and method guarded at the
    // `GetMethod` (see `Tr::arr_begin`); the one argument is a pure value (it runs after the
    // guard, so it can't invalidate it).
    for pc in header..=backedge {
        if !reach(pc) || helpers::math_site_failed(chunk, pc) || p.math.contains_key(&pc.wrapping_sub(1)) {
            continue;
        }
        let Op::GetMethod(m, _) = ops[pc] else { continue };
        if chunk.names.get(m as usize).map(|n| &**n) != Some("push") {
            continue;
        }
        let call = match site_args(pc + 1, false) {
            Some((call, args)) if args.len() == 1 => Some(call),
            _ if pc + 2 <= backedge && !lead(pc + 1) && !lead(pc + 2) && matches!(ops[pc + 2], Op::CallWithThis(1)) => {
                match ops[pc + 1] {
                    Op::LoadLocal(s) if !tdzd.maybe_undef(s as usize, pc + 1) && !defd.maybe_undef(s as usize, pc + 1) => Some(pc + 2),
                    Op::Const(_) | Op::Undef => Some(pc + 2),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(call) = call {
            p.arrm.insert(pc, call);
        }
    }

    // `i < <a>.length` loop tests: the array source and the index slot.
    for pc in header..=backedge {
        if !reach(pc) || !matches!(ops[pc], Op::JumpIfNotCmp(CmpKind::Lt, _)) {
            continue;
        }
        let fact = if pc >= header + 2
            && !lead(pc)
            && !lead(pc - 1)
            && matches!(ops[pc - 1], Op::GetPropLocal(s, n, _) if is_len(n) && kinds[s as usize] == Kind::Boxed)
            && matches!(ops[pc - 2], Op::LoadLocal(i) if num_slot(i))
        {
            match (ops[pc - 1], ops[pc - 2]) {
                (Op::GetPropLocal(s, ..), Op::LoadLocal(i)) => Some((Src::Slot(s), i)),
                _ => None,
            }
        } else if pc >= header + 3
            && !lead(pc)
            && !lead(pc - 1)
            && !lead(pc - 2)
            && matches!(ops[pc - 1], Op::GetProp(n, _) if is_len(n))
        {
            let src = match ops[pc - 2] {
                Op::LoadLocal(s) if kinds[s as usize] == Kind::Boxed => Some(Src::Slot(s)),
                Op::LoadName(n, _) if p.name_ref.contains(&(pc - 2)) => Some(Src::Name(n)),
                Op::LoadCap(n) => Some(Src::Cap(n)),
                _ => None,
            };
            match (src, ops[pc - 3]) {
                (Some(src), Op::LoadLocal(i)) if num_slot(i) => Some((src, i)),
                _ => None,
            }
        } else {
            None
        };
        if let Some((src, i)) = fact {
            if !p.idx_facts.contains(&(src, i)) {
                p.idx_facts.push((src, i));
            }
            if !p.arr_srcs.contains(&src) {
                p.arr_srcs.push(src);
            }
        }
    }
    p
}

// ---- translation -------------------------------------------------------------------------------

/// An abstract operand-stack entry.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Entry {
    /// An unboxed F64.
    Num(V),
    /// An unboxed I32 0/1.
    Bool(V),
    /// An owned `Value` at `frame.stack[depth]`.
    Boxed,
    /// A borrowed value: the address of a `Value` owned elsewhere (see the module docs);
    /// `frame.stack[depth]` holds nothing live.
    Ref(V, Src),
}

/// The IR variables of an array's element-storage view (see [`layout::array_view`]).
#[derive(Clone, Copy)]
struct ArrVars {
    kind: Variable,
    base: Variable,
    count: Variable,
    /// The F64 `length` read next to the view; valid while `kind` is nonzero.
    len: Variable,
}

/// The IR variables of an element site's typed-array view cache ([`Helper::TaView`]): the
/// `Gc` handle word it was taken for, its [`helpers::ta_code`] (0 = none), element 0's address
/// and the length. Reset by [`Tr::invalidate_js`].
#[derive(Clone, Copy)]
struct TaVars {
    obj: Variable,
    kind: Variable,
    data: Variable,
    len: Variable,
}

/// A `length` read site's typed-array length cache: the `Gc` handle word, the length, and
/// whether it is set (I32). Reset by [`Tr::invalidate_js`].
#[derive(Clone, Copy)]
struct TaLenVars {
    obj: Variable,
    len: Variable,
    ok: Variable,
}

/// An IR block starting a bytecode basic block.
struct Leader {
    block: Block,
    depth: usize,
    /// The abstract stack on entry, once known (see the module docs on merges).
    state: Option<Vec<Entry>>,
    /// Edges emitted so far.
    edges: u32,
    /// The translator reached the block (edges added later would be missed by its seal).
    visited: bool,
    /// Backward edges not yet emitted; the block is sealed when it drops to zero.
    back_left: u32,
    has_back: bool,
}

/// The slow path of an op with an inline fast path.
#[derive(Clone, Copy)]
enum Slow {
    /// Run the op itself through [`Helper::Generic`] (slot-reading ops only on Boxed slots).
    Generic,
    /// A property read through [`Helper::GetProp`] with the result at the op's result index.
    Prop { n: u32, c: u32, obj: PropObj },
}

/// The receiver of a property read's slow path.
#[derive(Clone, Copy)]
enum PropObj {
    /// The entry at this stack index (consumed; an env Ref is materialized first).
    Stack(usize),
    /// A frame slot's address: TDZ-checked (exit before the op), borrowed.
    Slot(V),
    /// An address that stays valid across JS (`this`, a slot Ref), borrowed.
    Ptr(V),
}

/// An operand of a fused local op.
#[derive(Clone, Copy)]
enum Opnd {
    Slot(u16),
    Const(u32),
}

struct Tr<'a, 'f> {
    /// See [`Built::num_exits`].
    num_exits: Vec<usize>,
    /// The safepoint budget, kept in a variable (`frame.budget` is read once at entry: only
    /// this code and [`Helper::Safepoint`], which resets it, use it).
    budget: Option<Variable>,
    /// Leaders whose last backward edge was requested by [`Tr::edge`]: sealed by
    /// [`Tr::flush_seals`] once the branch instruction itself exists (sealing earlier would
    /// complete the block's SSA parameters before that edge is a predecessor).
    pending_seal: Vec<Block>,
    chunk: &'a Chunk,
    ops: &'a [Op],
    header: usize,
    backedge: usize,
    fb: FunctionBuilder<'f>,
    frame: V,
    slots: V,
    stackp: V,
    consts: V,
    kinds: Vec<Kind>,
    /// The SSA variable of each `Num` / `Bool` slot.
    vars: Vec<Option<Variable>>,
    /// For SSA slots the region puts into their TDZ: an I32 flag, 1 while the slot is `Empty`.
    tdz: Vec<Option<Variable>>,
    /// For SSA slots that start out `undefined` (function mode, see [`fn_kinds`]): an I32 flag,
    /// 1 until the first write. Reads exit while it is set; write-backs store `undefined`.
    undef: Vec<Option<Variable>>,
    /// The slots with an `undef` flag.
    fresh: Vec<bool>,
    /// Where the fresh slots are definitely assigned (their `undef` flag is known clear).
    defd: Defd,
    /// Where the SSA slots with a TDZ flag are known initialized (the flag is known clear).
    tdzd: Defd,
    /// Whole-function code (see [`build_fn`]) rather than a loop region.
    func_mode: bool,
    helpers: Vec<Option<FuncRef>>,
    leaders: Vec<Option<Leader>>,
    stack: Vec<Entry>,
    max_stack: usize,
    plan: Plan,
    /// Cached binding addresses (PTR, 0 = unknown), per [`Plan::ptr_srcs`].
    ptr_vars: HashMap<Src, Variable>,
    /// Element-storage views, per [`Plan::arr_srcs`].
    arr_vars: HashMap<Src, ArrVars>,
    /// In-bounds facts (I32), per [`Plan::idx_facts`].
    idx_vars: HashMap<(Src, u16), Variable>,
    /// Passed `Math` guards (I32), per [`Plan::math`] site.
    math_vars: HashMap<usize, Variable>,
    /// Typed-array views, per element-access pc.
    ta_vars: HashMap<usize, TaVars>,
    /// Slots holding a typed array when the region was compiled (see [`Tr::ta_first`]).
    ta_slots: Vec<bool>,
    /// Typed-array length caches, per `length` read pc.
    ta_len_vars: HashMap<usize, TaLenVars>,
    /// The receiver (payload word, 0 = none) that last passed a push site's guard, per
    /// [`Plan::arrm`] site: the same array needs no re-check until JS runs.
    push_vars: HashMap<usize, Variable>,
    /// Region handlers live at the op being translated (see [`Analysis::hs`]).
    hs_cur: Vec<(usize, usize)>,
    /// The op a chained read already emitted (see [`Tr::chain_read`]): skipped by `body`.
    skip: Option<usize>,
    /// A chained read's consumer is being emitted: it does not chain in turn (its own skip
    /// would be lost, and each level would double the code).
    chaining: bool,
    /// Handler sets named by resume exits (`frame.exit_hset - 1`).
    hsets: Vec<Vec<(usize, usize)>>,
    /// A translation failure found while emitting something that cannot return an error.
    err: Option<String>,
    /// The import of this code itself (direct self-calls).
    self_fn: Option<FuncRef>,
    /// Frame-size constants of direct self-calls, patched once `max_stack` is known.
    self_sizes: Vec<V>,
    /// See [`Built::self_entry`].
    self_entry: usize,
    /// The engine's per-activation call flags read at entry, for direct calls (see
    /// [`call`]'s `entry_flags`): `frame.canon`.
    call_flags: Option<V>,
}

fn cmp_of(op: &Op) -> Option<CmpKind> {
    Some(match op {
        Op::Lt => CmpKind::Lt,
        Op::Gt => CmpKind::Gt,
        Op::Le => CmpKind::Le,
        Op::Ge => CmpKind::Ge,
        Op::EqEq => CmpKind::EqEq,
        Op::NotEq => CmpKind::NotEq,
        Op::StrictEq => CmpKind::StrictEq,
        Op::StrictNotEq => CmpKind::StrictNotEq,
        _ => return None,
    })
}

fn cmp_op(k: CmpKind) -> Op {
    match k {
        CmpKind::Lt => Op::Lt,
        CmpKind::Gt => Op::Gt,
        CmpKind::Le => Op::Le,
        CmpKind::Ge => Op::Ge,
        CmpKind::EqEq => Op::EqEq,
        CmpKind::NotEq => Op::NotEq,
        CmpKind::StrictEq => Op::StrictEq,
        CmpKind::StrictNotEq => Op::StrictNotEq,
    }
}

fn arith_op(k: ArithKind) -> Op {
    match k {
        ArithKind::Add => Op::Add,
        ArithKind::Sub => Op::Sub,
        ArithKind::Mul => Op::Mul,
        ArithKind::Div => Op::Div,
        ArithKind::Mod => Op::Mod,
        ArithKind::BitAnd => Op::BitAnd,
        ArithKind::BitOr => Op::BitOr,
        ArithKind::BitXor => Op::BitXor,
        ArithKind::Shl => Op::Shl,
        ArithKind::Shr => Op::Shr,
        ArithKind::UShr => Op::UShr,
    }
}

/// IEEE comparison matching `CmpKind::num` (NaN: all false except the inequalities).
fn fcc(k: CmpKind) -> FloatCC {
    match k {
        CmpKind::Lt => FloatCC::Lt,
        CmpKind::Gt => FloatCC::Gt,
        CmpKind::Le => FloatCC::Le,
        CmpKind::Ge => FloatCC::Ge,
        CmpKind::EqEq | CmpKind::StrictEq => FloatCC::Eq,
        CmpKind::NotEq | CmpKind::StrictNotEq => FloatCC::Ne,
    }
}

fn is_eq_kind(k: CmpKind) -> bool {
    matches!(
        k,
        CmpKind::EqEq | CmpKind::NotEq | CmpKind::StrictEq | CmpKind::StrictNotEq
    )
}

fn bin_code(op: &Op) -> Result<u32, String> {
    helpers::binary_op_code(op).ok_or_else(|| format!("no binary code for {op:?}"))
}

impl<'a, 'f> Tr<'a, 'f> {
    // ---- small emitters ----------------------------------------------------------------------

    fn in_region(&self, pc: usize) -> bool {
        pc >= self.header && pc <= self.backedge
    }

    fn is_leader(&self, pc: usize) -> bool {
        self.in_region(pc) && self.leaders[pc - self.header].is_some()
    }

    fn helper(&mut self, h: Helper) -> FuncRef {
        let i = h as usize;
        if let Some(f) = self.helpers[i] {
            return f;
        }
        let f = self
            .fb
            .func
            .import_function(helpers::signature(h), h as u32);
        self.helpers[i] = Some(f);
        f
    }

    fn call(&mut self, h: Helper, args: &[V]) -> Option<V> {
        let f = self.helper(h);
        self.fb.call_fn(f, args).first().copied()
    }

    fn call_status(&mut self, h: Helper, args: &[V]) -> V {
        self.call(h, args).expect("helper returns a status")
    }

    /// Call a helper that may run JS: every region cache and fact is stale afterwards. Env
    /// Refs must not be live across it (see [`Tr::prepare`]).
    fn call_js(&mut self, h: Helper, args: &[V]) -> V {
        let st = self.call_status(h, args);
        self.invalidate_js();
        st
    }

    /// Reset every region cache and fact (JS may have run).
    fn invalidate_js(&mut self) {
        let vars: Vec<(Variable, Type)> = self
            .ptr_vars
            .values()
            .map(|&v| (v, PTR))
            .chain(self.arr_vars.values().map(|a| (a.kind, Type::I32)))
            .chain(self.idx_vars.values().map(|&v| (v, Type::I32)))
            .chain(self.math_vars.values().map(|&v| (v, Type::I32)))
            .chain(self.ta_vars.values().map(|t| (t.kind, Type::I32)))
            .chain(self.ta_len_vars.values().map(|t| (t.ok, Type::I32)))
            .chain(self.push_vars.values().map(|&v| (v, PTR)))
            .collect();
        self.reset_vars(&vars);
    }

    /// Reset the element-storage views and in-bounds facts only (an array grew or shrank
    /// natively; no JS ran).
    fn invalidate_elems(&mut self) {
        let vars: Vec<(Variable, Type)> = self
            .arr_vars
            .values()
            .map(|a| (a.kind, Type::I32))
            .chain(self.idx_vars.values().map(|&v| (v, Type::I32)))
            .collect();
        self.reset_vars(&vars);
    }

    fn reset_vars(&mut self, vars: &[(Variable, Type)]) {
        if vars.is_empty() {
            return;
        }
        let z32 = self.i32c(0);
        let zp = self.ptrc(0);
        for &(v, ty) in vars {
            self.fb.def_var(v, if ty == PTR { zp } else { z32 });
        }
    }

    /// Reset the facts about the value at `src` (it is being written).
    fn invalidate_src(&mut self, src: Src) {
        let mut vars = Vec::new();
        if let Some(a) = self.arr_vars.get(&src) {
            vars.push((a.kind, Type::I32));
        }
        for (&(s, i), &v) in &self.idx_vars {
            if s == src || Src::Slot(i) == src {
                vars.push((v, Type::I32));
            }
        }
        self.reset_vars(&vars);
    }

    /// Before writing slot `s`: materialize the Refs to it and drop the facts about it.
    fn slot_write(&mut self, s: usize) {
        for k in 0..self.stack.len() {
            if matches!(self.stack[k], Entry::Ref(_, Src::Slot(x)) if x as usize == s) {
                self.force(k);
            }
        }
        self.invalidate_src(Src::Slot(s as u16));
    }

    /// Materialize a Ref entry at `i` into an owned clone at `frame.stack[i]`.
    fn force(&mut self, i: usize) {
        if let Entry::Ref(p, _) = self.stack[i] {
            let dst = self.sptr(i);
            self.clone_mem(dst, p);
            self.stack[i] = Entry::Boxed;
        }
    }

    /// Make entries `from..` valid `Value`s in memory: unboxed ones get a trivial copy (and
    /// stay unboxed), Refs are materialized (and become Boxed).
    fn box_from(&mut self, from: usize) {
        for i in from..self.stack.len() {
            match self.stack[i] {
                Entry::Ref(..) => self.force(i),
                e => self.store_entry(i, e),
            }
        }
    }

    /// Per-op bookkeeping before translating the op at `pc`: materialize the operands of ops
    /// that cannot read through a Ref, the env Refs that survive an op that may run JS, and
    /// the Refs to a slot the op writes.
    fn prepare(&mut self, pc: usize) {
        let op = self.ops[pc];
        let (pops, _) = self.chunk.jit_stack_effect(pc).unwrap_or((0, 0));
        let d = self.stack.len();
        let pops = pops.min(d);
        let math_part = self.plan.math.contains_key(&pc)
            || self.plan.math.values().any(|s| s.call == pc)
            || self.plan.strm.values().any(|&c| c == pc)
            || self.plan.arrm.values().any(|&c| c == pc)
            || self.site_get_method(pc)
            || self.plan.dname.contains_key(&pc)
            || (self.plan.dmethod.contains_key(&pc)
                && !matches!(self.stack.last(), Some(Entry::Num(_) | Entry::Bool(_))));
        // A direct call reads its operands in place (it runs JS: env Refs below still go).
        // A plain property store writes through a borrowed receiver (see `Tr::ref_receiver`).
        let ref_aware = math_part
            || self.plan.direct.contains_key(&pc)
            || self.plan.dnew.contains_key(&pc)
            || (matches!(op, Op::AppendProp(..) | Op::SetPropDrop(..))
                && !self.plan.dacc.contains_key(&pc)
                && !self.plan.dset.contains_key(&pc))
            || matches!(
                op,
                Op::Pop
                    | Op::Void
                    | Op::Dup
                    | Op::Dup2
                    | Op::GetElem
                    | Op::GetProp(..)
                    | Op::SetElem
                    | Op::SetElemDrop
                    | Op::Add
                    | Op::Sub
                    | Op::Mul
                    | Op::Div
                    | Op::BitAnd
                    | Op::BitOr
                    | Op::BitXor
                    | Op::Shl
                    | Op::Shr
                    | Op::UShr
                    | Op::Lt
                    | Op::Gt
                    | Op::Le
                    | Op::Ge
                    | Op::EqEq
                    | Op::NotEq
                    | Op::StrictEq
                    | Op::StrictNotEq
                    | Op::JumpIfNotCmp(..)
            );
        if !ref_aware {
            for k in d - pops..d {
                self.force(k);
            }
        }
        let pure = math_part
            || self.plan.strm.contains_key(&pc)
            || self.plan.strn.contains_key(&pc)
            || self.plan.colm.contains_key(&pc)
            || self.plan.arrm.contains_key(&pc)
            || matches!(
                op,
                Op::Const(_)
                    | Op::Undef
                    | Op::LoadLocal(_)
                    | Op::LoadCap(_)
                    | Op::LoadThis
                    | Op::Dup
                    | Op::Dup2
                    | Op::Pop
                    | Op::Void
                    | Op::UpdateLocal(..)
                    | Op::StoreLocal(_)
                    | Op::Tdz(_)
                    | Op::JumpIfFalse(_)
                    | Op::JumpIfFalsePeek(_)
                    | Op::JumpIfTruePeek(_)
                    | Op::JumpIfNotNullishPeek(_)
                    | Op::Not
            )
            || (matches!(op, Op::LoadName(..)) && self.plan.name_ref.contains(&pc));
        if !pure {
            for k in 0..d - pops {
                if matches!(self.stack[k], Entry::Ref(_, src) if src.in_env()) {
                    self.force(k);
                }
            }
        }
        match op {
            Op::StoreLocal(s) | Op::UpdateLocal(s, _) | Op::Tdz(s) => self.slot_write(s as usize),
            Op::ArithLL(_, s, ..) | Op::ArithLK(_, s, ..) => self.slot_write(s as usize),
            _ => {}
        }
    }

    fn i32c(&mut self, v: i64) -> V {
        self.fb.iconst(Type::I32, v)
    }

    fn ptrc(&mut self, v: i64) -> V {
        self.fb.iconst(PTR, v)
    }

    /// Byte offset of `frame.stack[d]` (and account for it in `max_stack`).
    fn soff(&mut self, d: usize) -> i32 {
        self.max_stack = self.max_stack.max(d + 1);
        d as i32 * VALUE_SIZE
    }

    fn sptr(&mut self, d: usize) -> V {
        let off = self.soff(d) as i64;
        let c = self.ptrc(off);
        self.fb.binary(BinaryOp::Iadd, self.stackp, c)
    }

    fn slot_ptr(&mut self, s: usize) -> V {
        let c = self.ptrc(s as i64 * VALUE_SIZE as i64);
        self.fb.binary(BinaryOp::Iadd, self.slots, c)
    }

    fn const_ptr(&mut self, k: usize) -> V {
        let c = self.ptrc(k as i64 * VALUE_SIZE as i64);
        self.fb.binary(BinaryOp::Iadd, self.consts, c)
    }

    fn stack_tag(&mut self, d: usize) -> V {
        let o = self.soff(d);
        self.fb.load(MemKind::I32U8, self.stackp, o)
    }

    fn stack_num(&mut self, d: usize) -> V {
        let o = self.soff(d);
        self.fb.load(MemKind::F64, self.stackp, o + VALUE_PAYLOAD)
    }

    fn stack_bool(&mut self, d: usize) -> V {
        let o = self.soff(d);
        self.fb.load(MemKind::I32U8, self.stackp, o + VALUE_BOOL)
    }

    fn set_stack_tag(&mut self, d: usize, tag: u8) {
        let o = self.soff(d);
        let t = self.i32c(tag as i64);
        self.fb.store(MemKind::I32U8, self.stackp, t, o);
    }

    /// Store an unboxed entry at `frame.stack[d]` (which holds a trivially droppable value).
    fn store_entry(&mut self, d: usize, e: Entry) {
        match e {
            Entry::Num(x) => {
                self.set_stack_tag(d, TAG_NUM);
                let o = self.soff(d);
                self.fb
                    .store(MemKind::F64, self.stackp, x, o + VALUE_PAYLOAD);
            }
            Entry::Bool(b) => {
                self.set_stack_tag(d, TAG_BOOL);
                let o = self.soff(d);
                self.fb
                    .store(MemKind::I32U8, self.stackp, b, o + VALUE_BOOL);
            }
            Entry::Boxed => {}
            // An owned clone (exits and merges, where the entry itself is left behind).
            Entry::Ref(p, _) => {
                let dst = self.sptr(d);
                self.clone_mem(dst, p);
            }
        }
    }

    /// Drop the Boxed value at `frame.stack[d]` (leaving `undefined`): nothing for a trivially
    /// droppable tag, an inline release of an object that stays alive, else a helper call.
    fn drop_at(&mut self, d: usize) {
        let o = self.soff(d);
        let sp = self.stackp;
        self.drop_mem(sp, o);
    }

    /// [`Tr::drop_at`] for the `Value` at `base + o` (a frame slot or stack entry).
    fn drop_mem(&mut self, base: V, o: i32) {
        let tag = self.fb.load(MemKind::I32U8, base, o);
        let four = self.i32c(TAG_NUM as i64);
        let big = self.fb.icmp(IntCC::Ugt, tag, four);
        let ref_b = self.fb.create_block();
        let obj_b = self.fb.create_block();
        let dec_b = self.fb.create_block();
        let call_b = self.fb.create_block();
        let cont = self.fb.create_block();
        self.fb.brif(big, ref_b, &[], cont, &[]);
        self.fb.seal_block(ref_b);
        self.fb.switch_to_block(ref_b);
        let objt = self.i32c(TAG_OBJ as i64);
        let is_obj = self.fb.icmp(IntCC::Eq, tag, objt);
        let rc_so = layout::rc_strong_offset();
        let counted = match rc_so {
            Some(_) => {
                let strt = self.i32c(TAG_STR as i64);
                let is_str = self.fb.icmp(IntCC::Eq, tag, strt);
                self.fb.binary(BinaryOp::Bor, is_obj, is_str)
            }
            None => is_obj,
        };
        self.fb.brif(counted, obj_b, &[], call_b, &[]);
        self.fb.seal_block(obj_b);
        self.fb.switch_to_block(obj_b);
        let gc = self.fb.load(PTR_MEM, base, o + VALUE_PAYLOAD);
        let so = rc_so.unwrap_or(crate::value::GC_STRONG_OFFSET as i32);
        let strong = self.fb.load(PTR_MEM, gc, so);
        let one = self.ptrc(1);
        let live = self.fb.icmp(IntCC::Ugt, strong, one);
        let chk_b = self.fb.create_block();
        let pair_b = self.fb.create_block();
        self.fb.brif(live, chk_b, &[], call_b, &[]);
        self.fb.seal_block(chk_b);
        self.fb.switch_to_block(chk_b);
        // An object going down to one reference may be half of a function / `.prototype`
        // pair the reference count frees (`Gc`'s drop): the Rust release checks.
        let two = self.ptrc(2);
        let is2 = self.fb.icmp(IntCC::Eq, strong, two);
        let maybe = self.fb.binary(BinaryOp::Band, is2, is_obj);
        self.fb.brif(maybe, pair_b, &[], dec_b, &[]);
        self.fb.seal_block(pair_b);
        self.fb.switch_to_block(pair_b);
        let w = self.fb.load(PTR_MEM, gc, crate::value::GC_WEAK_OFFSET as i32);
        let flag = self.ptrc(crate::value::FN_PAIR_FLAG as i64);
        let fl = self.fb.binary(BinaryOp::Band, w, flag);
        let zp = self.ptrc(0);
        let paired = self.fb.icmp(IntCC::Ne, fl, zp);
        self.fb.brif(paired, call_b, &[], dec_b, &[]);
        self.fb.seal_block(dec_b);
        self.fb.seal_block(call_b);
        self.fb.switch_to_block(dec_b);
        let s1 = self.fb.binary(BinaryOp::Isub, strong, one);
        self.fb.store(PTR_MEM, gc, s1, so);
        let u = self.i32c(TAG_UNDEFINED as i64);
        self.fb.store(MemKind::I32U8, base, u, o);
        self.fb.jump(cont, &[]);
        self.fb.switch_to_block(call_b);
        let oc = self.ptrc(o as i64);
        let p = self.fb.binary(BinaryOp::Iadd, base, oc);
        self.call(Helper::Drop, &[p]);
        self.fb.jump(cont, &[]);
        self.fb.seal_block(cont);
        self.fb.switch_to_block(cont);
    }

    fn flush_seals(&mut self) {
        for b in std::mem::take(&mut self.pending_seal) {
            self.fb.seal_block(b);
        }
    }

    /// Continue in a fresh block that `cur` falls into; for fast paths returned by `layout`.
    fn seal_current(&mut self) {
        if let Some(b) = self.fb.current_block() {
            self.fb.seal_block(b);
        }
    }

    // ---- exits -------------------------------------------------------------------------------

    /// Write every SSA local back to its slot, except `skip`.
    fn write_back(&mut self, skip: Option<usize>) {
        for s in 0..self.vars.len() {
            let Some(var) = self.vars[s] else { continue };
            if skip == Some(s) {
                continue;
            }
            let off = s as i32 * VALUE_SIZE;
            let v = self.fb.use_var(var);
            let tag = match self.kinds[s] {
                Kind::Num => {
                    self.fb
                        .store(MemKind::F64, self.slots, v, off + VALUE_PAYLOAD);
                    TAG_NUM
                }
                Kind::Bool => {
                    self.fb
                        .store(MemKind::I32U8, self.slots, v, off + VALUE_BOOL);
                    TAG_BOOL
                }
                Kind::Boxed => continue,
            };
            let mut t = self.i32c(tag as i64);
            if let Some(flag) = self.undef[s] {
                let f = self.fb.use_var(flag);
                let u = self.i32c(TAG_UNDEFINED as i64);
                t = self.fb.select(f, u, t);
            }
            if let Some(flag) = self.tdz[s] {
                let f = self.fb.use_var(flag);
                let e = self.i32c(TAG_EMPTY as i64);
                t = self.fb.select(f, e, t);
            }
            self.fb.store(MemKind::I32U8, self.slots, t, off);
        }
    }

    /// Terminate the current block with an exit: materialize `stack` (entries above it up to
    /// `depth` are already in memory), write back the SSA locals and return the exit word.
    /// `store = Some((slot, d))` moves the Boxed `frame.stack[d]` into SSA slot `slot` instead of
    /// writing that slot back (an op that ran but produced a value of another kind).
    fn emit_exit(
        &mut self,
        pc: usize,
        kind: u64,
        stack: &[Entry],
        depth: usize,
        store: Option<(usize, usize)>,
    ) {
        if kind == EXIT_THROW {
            if let Some(&(catch, hd)) = self.hs_cur.last() {
                self.catch_throw(stack, depth, catch, hd);
                return;
            }
        }
        if kind == EXIT_RESUME && !self.hs_cur.is_empty() {
            let set = self.hs_cur.clone();
            let k = match self.hsets.iter().position(|h| *h == set) {
                Some(k) => k,
                None => {
                    self.hsets.push(set);
                    self.hsets.len() - 1
                }
            };
            let id = self.fb.iconst(Type::I64, k as i64 + 1);
            self.fb.store(MemKind::I64, self.frame, id, FRAME_EXIT_HSET);
        }
        for (i, &e) in stack.iter().enumerate() {
            self.store_entry(i, e);
        }
        // Function code that returns or throws out of the call leaves nothing that reads its
        // slots again (closures capture through the environment).
        if !(self.func_mode && kind != EXIT_RESUME) {
            self.write_back(store.map(|s| s.0));
        }
        if let Some((s, d)) = store {
            let p = self.sptr(d);
            let sc = self.i32c(s as i64);
            self.call(Helper::StoreLocal, &[self.frame, sc, p]);
        }
        let dep = self.fb.iconst(Type::I64, depth as i64);
        self.fb
            .store(MemKind::I64, self.frame, dep, FRAME_EXIT_DEPTH);
        let w = self.fb.iconst(Type::I64, exit(pc, kind) as i64);
        self.fb.ret(&[w]);
    }

    /// A throw (the exception in `frame.exception`) under the region handler `(catch, hd)`:
    /// `stack` / `depth` describe the operand stack as for an `EXIT_THROW` exit. Drop the
    /// entries above the handler's depth, move the exception on top and continue at the catch
    /// pad — in the region, or through a resume exit at it.
    fn catch_throw(&mut self, stack: &[Entry], depth: usize, catch: usize, hd: usize) {
        for k in hd..depth.max(stack.len()) {
            if matches!(stack.get(k), Some(Entry::Boxed) | None) {
                self.drop_at(k);
            }
        }
        let dst = self.sptr(hd);
        self.call(Helper::TakeException, &[self.frame, dst]);
        let mut st: Vec<Entry> = (0..hd)
            .map(|k| stack.get(k).copied().unwrap_or(Entry::Boxed))
            .collect();
        st.push(Entry::Boxed);
        let saved_h = self.hs_cur.clone();
        self.hs_cur.pop();
        let mut done = false;
        if self.in_region(catch) {
            match self.leaders[catch - self.header].as_ref() {
                Some(l) if !l.visited => {
                    let saved = std::mem::replace(&mut self.stack, st.clone());
                    match self.edge(false, catch) {
                        Ok(b) => {
                            self.fb.jump(b, &[]);
                            done = true;
                        }
                        Err(e) => {
                            self.err.get_or_insert(e);
                        }
                    }
                    self.stack = saved;
                }
                _ => {
                    self.err
                        .get_or_insert(format!("throw reaches catch pad {catch} after its block"));
                }
            }
        }
        if !done {
            self.emit_exit(catch, EXIT_RESUME, &st, hd + 1, None);
        }
        self.hs_cur = saved_h;
    }

    /// Exit when `cond` is nonzero; continue in a fresh block otherwise.
    fn exit_if(&mut self, cond: V, pc: usize, kind: u64, stack: &[Entry], depth: usize) {
        let ex = self.fb.create_block();
        let cont = self.fb.create_block();
        self.fb.brif(cond, ex, &[], cont, &[]);
        self.fb.seal_block(ex);
        self.fb.switch_to_block(ex);
        self.emit_exit(pc, kind, stack, depth, None);
        self.fb.seal_block(cont);
        self.fb.switch_to_block(cont);
    }

    /// Exit with `EXIT_THROW` at `pc` when a helper status says so.
    fn throw_if(&mut self, status: V, pc: usize, stack: &[Entry], depth: usize) {
        let z = self.i32c(STATUS_OK as i64);
        let c = self.fb.icmp(IntCC::Ne, status, z);
        self.exit_if(c, pc, EXIT_THROW, stack, depth);
    }

    /// Exit before the op at `pc`: the interpreter runs it.
    /// [`Helper::Call`]'s `CALL_AWAIT` flag for a call at `pc` whose result feeds straight into
    /// an `await` (the interpreter's `Call; Await` fusion, see `Interp::note_await_call`).
    pub(super) fn await_fused(&self, pc: usize) -> i64 {
        if matches!(self.ops.get(pc + 1), Some(Op::Await)) {
            helpers::CALL_AWAIT as i64
        } else {
            0
        }
    }

    fn exit_now(&mut self, pc: usize) {
        let st = self.stack.clone();
        self.emit_exit(pc, EXIT_RESUME, &st, st.len(), None);
    }

    /// Exit before the op at `pc` when SSA slot `s` is in its TDZ (the interpreter throws) or
    /// still holds its initial `undefined` (the interpreter reads it).
    fn tdz_guard(&mut self, s: usize, pc: usize) {
        let undef = self.undef[s].filter(|_| self.defd.maybe_undef(s, pc));
        let tdz = self.tdz[s].filter(|_| self.tdzd.maybe_undef(s, pc));
        let f = match (tdz, undef) {
            (Some(a), Some(b)) => {
                let a = self.fb.use_var(a);
                let b = self.fb.use_var(b);
                Some(self.fb.binary(BinaryOp::Bor, a, b))
            }
            (Some(a), None) | (None, Some(a)) => Some(self.fb.use_var(a)),
            (None, None) => None,
        };
        if let Some(f) = f {
            let st = self.stack.clone();
            self.exit_if(f, pc, EXIT_RESUME, &st, st.len());
        }
    }

    fn clear_tdz(&mut self, s: usize) {
        for flag in [self.tdz[s], self.undef[s]].into_iter().flatten() {
            let z = self.i32c(0);
            self.fb.def_var(flag, z);
        }
    }

    // ---- control flow ------------------------------------------------------------------------

    /// Account for an edge from the current state to leader `to` (conforming the stack to the
    /// leader's entry state) and return its block.
    fn edge(&mut self, back: bool, to: usize) -> Result<Block, String> {
        let i = to - self.header;
        let (block, state) = match &self.leaders[i] {
            Some(l) => (l.block, l.state.clone()),
            None => return Err(format!("jump into the middle of a block at {to}")),
        };
        match state {
            Some(st) => {
                if st.len() != self.stack.len() {
                    return Err(format!("stack depth differs at the merge at {to}"));
                }
                for k in 0..st.len() {
                    match (st[k], self.stack[k]) {
                        (Entry::Boxed, Entry::Boxed) => {}
                        (Entry::Boxed, e) => self.store_entry(k, e),
                        (a, b) if a == b => {}
                        _ => return Err(format!("operand kinds differ at the merge at {to}")),
                    }
                }
            }
            None => {
                let st = self.stack.clone();
                self.leaders[i].as_mut().expect("leader").state = Some(st);
            }
        }
        let l = self.leaders[i].as_mut().expect("leader");
        l.edges += 1;
        if back {
            l.back_left = l.back_left.saturating_sub(1);
            if l.back_left == 0 {
                self.pending_seal.push(block);
            }
        }
        Ok(block)
    }

    /// The IR target for a branch from `pc` to `to` with the current stack: a leader, or a fresh
    /// exit block (filled by [`Tr::fill_exit`] after the branch is emitted).
    fn dest(&mut self, pc: usize, to: usize) -> Result<(Block, Option<usize>), String> {
        if self.in_region(to) {
            Ok((self.edge(to <= pc, to)?, None))
        } else {
            Ok((self.fb.create_block(), Some(to)))
        }
    }

    fn fill_exit(&mut self, d: (Block, Option<usize>), stack: &[Entry]) {
        if let (b, Some(to)) = d {
            self.fb.seal_block(b);
            self.fb.switch_to_block(b);
            self.emit_exit(to, EXIT_RESUME, stack, stack.len(), None);
        }
    }

    /// Branch on `cond` (nonzero → `then_pc`), both successors taking the current stack.
    fn branch(&mut self, pc: usize, cond: V, then_pc: usize, else_pc: usize) -> Result<(), String> {
        // A Ref materialized for one successor's merge would leak on the other: settle them.
        for k in 0..self.stack.len() {
            self.force(k);
        }
        let st = self.stack.clone();
        let t = self.dest(pc, then_pc)?;
        let e = self.dest(pc, else_pc)?;
        self.fb.brif(cond, t.0, &[], e.0, &[]);
        self.flush_seals();
        self.fill_exit(t, &st);
        self.fill_exit(e, &st);
        Ok(())
    }

    /// The loop safepoint at a backward jump to `to`: count the budget down and, when it runs
    /// out, let the collector / interrupt check run (it may throw).
    fn safepoint(&mut self, to: usize) {
        let bv = self.budget.expect("defined at entry");
        let b = self.fb.use_var(bv);
        let one = self.fb.iconst(Type::I64, 1);
        let b1 = self.fb.binary(BinaryOp::Isub, b, one);
        self.fb.def_var(bv, b1);
        let zero = self.fb.iconst(Type::I64, 0);
        let low = self.fb.icmp(IntCC::Sle, b1, zero);
        let sp = self.fb.create_block();
        let cont = self.fb.create_block();
        self.fb.brif(low, sp, &[], cont, &[]);
        self.fb.seal_block(sp);
        self.fb.switch_to_block(sp);
        let st = self.call_js(Helper::Safepoint, &[self.frame]);
        let stack = self.stack.clone();
        self.throw_if(st, to, &stack, stack.len());
        let full = self.fb.iconst(Type::I64, SAFEPOINT_BUDGET);
        self.fb.def_var(bv, full);
        self.fb.jump(cont, &[]);
        self.fb.seal_block(cont);
        self.fb.switch_to_block(cont);
    }

    // ---- driver ------------------------------------------------------------------------------

    /// The entry block: guard and load the SSA locals, then enter the header.
    fn entry(&mut self, an: &Analysis) {
        let bv = self.fb.declare_var(Type::I64);
        let b = self.fb.load(MemKind::I64, self.frame, FRAME_BUDGET);
        self.fb.def_var(bv, b);
        self.budget = Some(bv);
        let fail = self.fb.create_block();
        for s in 0..self.kinds.len() {
            let ty = match self.kinds[s] {
                Kind::Num => Type::F64,
                Kind::Bool => Type::I32,
                Kind::Boxed => continue,
            };
            self.vars[s] = Some(self.fb.declare_var(ty));
            if self.fresh[s] {
                self.undef[s] = Some(self.fb.declare_var(Type::I32));
            }
            if an.tdz[s] {
                self.tdz[s] = Some(self.fb.declare_var(Type::I32));
            }
        }
        // Function code with resume points dispatches on `frame.resume`: a resume entry
        // reloads the SSA slots from what the suspended body left (see `resume_slots`).
        let resume = (!an.resumes.is_empty()).then(|| {
            let r = self.fb.load(MemKind::I64, self.frame, FRAME_RESUME);
            let normal = self.fb.create_block();
            let res = self.fb.create_block();
            let join = self.fb.create_block();
            let z = self.i64c(0);
            let is0 = self.fb.icmp(IntCC::Eq, r, z);
            self.fb.brif(is0, normal, &[], res, &[]);
            self.fb.seal_block(normal);
            self.fb.seal_block(res);
            self.fb.switch_to_block(res);
            self.resume_slots(an, fail);
            self.fb.jump(join, &[]);
            self.fb.switch_to_block(normal);
            (r, join)
        });
        for s in 0..self.kinds.len() {
            let tag = match self.kinds[s] {
                Kind::Num => TAG_NUM,
                Kind::Bool => TAG_BOOL,
                Kind::Boxed => continue,
            };
            let var = self.vars[s].expect("declared");
            if let Some(flag) = self.tdz[s] {
                let z = self.i32c(0);
                self.fb.def_var(flag, z);
            }
            if self.fresh[s] {
                // `undefined` at every entry: nothing to load or guard.
                let z = self.fb.f64const(0.0);
                self.fb.def_var(var, z);
                let one = self.i32c(1);
                self.fb.def_var(self.undef[s].expect("declared"), one);
                continue;
            }
            let off = s as i32 * VALUE_SIZE;
            let t = self.fb.load(MemKind::I32U8, self.slots, off);
            let want = self.i32c(tag as i64);
            let bad = self.fb.icmp(IntCC::Ne, t, want);
            let next = self.fb.create_block();
            self.fb.brif(bad, fail, &[], next, &[]);
            self.fb.seal_block(next);
            self.fb.switch_to_block(next);
            let v = match self.kinds[s] {
                Kind::Num => self.fb.load(MemKind::F64, self.slots, off + VALUE_PAYLOAD),
                _ => self.fb.load(MemKind::I32U8, self.slots, off + VALUE_BOOL),
            };
            self.fb.def_var(var, v);
        }
        if let Some((_, join)) = resume {
            self.fb.jump(join, &[]);
            self.fb.seal_block(join);
            self.fb.switch_to_block(join);
        }
        // Region caches start unknown; `Math` guards are checked here, so a replaced intrinsic
        // fails the entry (and after enough failures the loop recompiles without the site).
        let zp = self.ptrc(0);
        let z32 = self.i32c(0);
        for &src in &self.plan.ptr_srcs.clone() {
            let v = self.fb.declare_var(PTR);
            self.fb.def_var(v, zp);
            self.ptr_vars.insert(src, v);
        }
        for &src in &self.plan.arr_srcs.clone() {
            let a = ArrVars {
                kind: self.fb.declare_var(Type::I32),
                base: self.fb.declare_var(PTR),
                count: self.fb.declare_var(PTR),
                len: self.fb.declare_var(Type::F64),
            };
            self.fb.def_var(a.kind, z32);
            self.fb.def_var(a.base, zp);
            self.fb.def_var(a.count, zp);
            let zf = self.fb.f64const(0.0);
            self.fb.def_var(a.len, zf);
            self.arr_vars.insert(src, a);
        }
        // Typed-array caches only where the slots showed typed arrays at compile time (each is
        // a few loop-carried variables); other sites take a fresh view per access.
        let any_ta = self.ta_slots.iter().any(|&b| b);
        let ta_slot = |s: u16| self.ta_slots.get(s as usize).copied().unwrap_or(false);
        let mut elem_sites = Vec::new();
        let mut len_sites = Vec::new();
        for pc in self.header..=self.backedge {
            let op = self.ops[pc];
            let (elem, local) = match op {
                Op::GetElem | Op::SetElem | Op::SetElemDrop => (true, None),
                Op::GetElemLocal(s) | Op::SetElemLocal(s) | Op::SetElemLocalDrop(s) => (true, Some(s)),
                Op::GetProp(..) | Op::GetPropThis(..) => (false, None),
                Op::GetPropLocal(s, ..) => (false, Some(s)),
                _ => continue,
            };
            // Element sites on a local keep a view cache even when the slot held no typed
            // array at compile time: a region compiled for Arrays and later fed typed arrays
            // (one function serving both) would otherwise take a fresh view per access.
            if !local.map_or(any_ta, ta_slot) && !helpers::ta_hot(self.chunk, pc) {
                continue;
            }
            if elem {
                elem_sites.push(pc);
            } else if let Op::GetProp(n, _) | Op::GetPropThis(n, _) | Op::GetPropLocal(_, n, _) = op {
                if self.chunk.names.get(n as usize).is_some_and(|x| &**x == "length") {
                    len_sites.push(pc);
                }
            }
        }
        for pc in elem_sites {
            {
                let t = TaVars {
                    obj: self.fb.declare_var(PTR),
                    kind: self.fb.declare_var(Type::I32),
                    data: self.fb.declare_var(PTR),
                    len: self.fb.declare_var(PTR),
                };
                for (v, z) in [(t.obj, zp), (t.kind, z32), (t.data, zp), (t.len, zp)] {
                    self.fb.def_var(v, z);
                }
                self.ta_vars.insert(pc, t);
            }
        }
        for pc in len_sites {
            let t = TaLenVars {
                obj: self.fb.declare_var(PTR),
                len: self.fb.declare_var(Type::F64),
                ok: self.fb.declare_var(Type::I32),
            };
            let zf = self.fb.f64const(0.0);
            self.fb.def_var(t.obj, zp);
            self.fb.def_var(t.len, zf);
            self.fb.def_var(t.ok, z32);
            self.ta_len_vars.insert(pc, t);
        }
        for &key in &self.plan.idx_facts.clone() {
            let v = self.fb.declare_var(Type::I32);
            self.fb.def_var(v, z32);
            self.idx_vars.insert(key, v);
        }
        // (Local-callee sites check at every call instead: the local may change.)
        let mut sites: Vec<usize> = self
            .plan
            .math
            .iter()
            .filter(|(_, s)| s.slot.is_none())
            .map(|(&pc, _)| pc)
            .collect();
        sites.sort_unstable();
        for pc in sites {
            let ok = self.site_guard(pc);
            let zero = self.i32c(0);
            let bad = self.fb.icmp(IntCC::Eq, ok, zero);
            let next = self.fb.create_block();
            self.fb.brif(bad, fail, &[], next, &[]);
            self.fb.seal_block(next);
            self.fb.switch_to_block(next);
            let v = self.fb.declare_var(Type::I32);
            let one = self.i32c(1);
            self.fb.def_var(v, one);
            self.math_vars.insert(pc, v);
        }
        let mut pushes: Vec<usize> = self.plan.arrm.keys().copied().collect();
        pushes.sort_unstable();
        for pc in pushes {
            let v = self.fb.declare_var(PTR);
            let zp = self.ptrc(0);
            self.fb.def_var(v, zp);
            self.push_vars.insert(pc, v);
        }
        // String intrinsic sites check at the site (the guard needs the receiver).
        let mut strs: Vec<usize> = self.plan.strm.keys().copied().collect();
        strs.sort_unstable();
        for pc in strs {
            let v = self.fb.declare_var(Type::I32);
            let zero = self.i32c(0);
            self.fb.def_var(v, zero);
            self.math_vars.insert(pc, v);
        }
        if self.plan.direct.values().any(|s| !s.construct) || !self.plan.dacc.is_empty() {
            self.call_flags = self.entry_flags();
        }
        let header_block = self
            .edge(false, self.header)
            .expect("the header is a leader");
        if let Some((r, _)) = resume {
            for (k, &(rpc, d)) in an.resumes.iter().enumerate() {
                let go = self.fb.create_block();
                let next = self.fb.create_block();
                let kv = self.i64c(k as i64 + 1);
                let c = self.fb.icmp(IntCC::Eq, r, kv);
                self.fb.brif(c, go, &[], next, &[]);
                self.fb.seal_block(go);
                self.fb.seal_block(next);
                self.fb.switch_to_block(go);
                // The frame's operand stack, moved into the stack area by `enter`.
                self.stack = vec![Entry::Boxed; d];
                self.max_stack = self.max_stack.max(d);
                match self.edge(false, rpc) {
                    Ok(b) => self.fb.jump(b, &[]),
                    Err(e) => {
                        self.err.get_or_insert(e);
                        self.fb.jump(fail, &[]);
                    }
                }
                self.stack.clear();
                self.fb.switch_to_block(next);
            }
        }
        self.fb.jump(header_block, &[]);
        self.fb.seal_block(fail);
        self.fb.switch_to_block(fail);
        let z = self.fb.iconst(Type::I64, 0);
        self.fb.store(MemKind::I64, self.frame, z, FRAME_EXIT_DEPTH);
        let w = self
            .fb
            .iconst(Type::I64, exit(self.header, EXIT_ENTRY_FAIL) as i64);
        self.fb.ret(&[w]);
    }

    /// The SSA slots at a resume entry (after an `await`): each holds a value of its kind, or
    /// — a fresh local still unassigned — `undefined`, or — one in its TDZ — `Empty`; anything
    /// else fails the entry (the interpreter continues).
    fn resume_slots(&mut self, an: &Analysis, fail: Block) {
        for s in 0..self.kinds.len() {
            let (num, tag) = match self.kinds[s] {
                Kind::Num => (true, TAG_NUM),
                Kind::Bool => (false, TAG_BOOL),
                Kind::Boxed => continue,
            };
            let off = s as i32 * VALUE_SIZE;
            let t = self.fb.load(MemKind::I32U8, self.slots, off);
            let want = self.i32c(tag as i64);
            let mut ok = self.fb.icmp(IntCC::Eq, t, want);
            let und = self.i32c(TAG_UNDEFINED as i64);
            let is_undef = self.fb.icmp(IntCC::Eq, t, und);
            let emp = self.i32c(TAG_EMPTY as i64);
            let is_empty = self.fb.icmp(IntCC::Eq, t, emp);
            if self.fresh[s] {
                ok = self.fb.binary(BinaryOp::Bor, ok, is_undef);
            }
            if an.tdz[s] {
                ok = self.fb.binary(BinaryOp::Bor, ok, is_empty);
            }
            let next = self.fb.create_block();
            self.fb.brif(ok, next, &[], fail, &[]);
            self.fb.seal_block(next);
            self.fb.switch_to_block(next);
            let v = if num {
                self.fb.load(MemKind::F64, self.slots, off + VALUE_PAYLOAD)
            } else {
                self.fb.load(MemKind::I32U8, self.slots, off + VALUE_BOOL)
            };
            self.fb.def_var(self.vars[s].expect("declared"), v);
            if let Some(f) = self.undef[s] {
                self.fb.def_var(f, is_undef);
            }
            if let Some(f) = self.tdz[s] {
                self.fb.def_var(f, is_empty);
            }
        }
    }

    /// The template of the `MakeObject(start, n, tidx)` literal when it instantiates in one
    /// box ([`Props::fast_template`](crate::value::Props::fast_template)): its address (the
    /// chunk keeps it). Initialized here as the interpreter's first run would
    /// (`Interp::make_plain_object_templated`): template keys are distinct.
    fn lit_template(&self, start: u32, n: usize, tidx: u32) -> Option<usize> {
        let cell = self.chunk.obj_maps.get(tidx as usize)?;
        let keys = self.chunk.names.get(start as usize..start as usize + n)?;
        let map = cell.get_or_init(|| {
            let mut p = crate::value::Props::new();
            for k in keys {
                p.insert(k.clone(), crate::value::Property::plain(Value::Undefined));
            }
            p
        });
        map.fast_template(crate::value::INLINE_PROPS_WIDE)
            .then_some(map as *const crate::value::Props as usize)
    }

    fn body(&mut self, an: &Analysis) -> Result<(), String> {
        let mut dead = false;
        for pc in self.header..=self.backedge {
            let i = pc - self.header;
            if self.leaders[i].is_some() {
                if !dead && !self.fb.is_filled() {
                    let b = self.edge(false, pc)?;
                    self.fb.jump(b, &[]);
                    self.flush_seals();
                }
                let l = self.leaders[i].as_mut().expect("leader");
                l.visited = true;
                if l.edges == 0 && !l.has_back {
                    // No edge reached it (its predecessors all exited): dead code.
                    dead = true;
                    continue;
                }
                dead = false;
                let (block, has_back) = (l.block, l.has_back);
                let depth = l.depth;
                let st = l
                    .state
                    .get_or_insert_with(|| vec![Entry::Boxed; depth])
                    .clone();
                self.fb.switch_to_block(block);
                if !has_back {
                    self.fb.seal_block(block);
                }
                self.stack = st;
            }
            if self.skip == Some(pc) {
                self.skip = None;
                continue;
            }
            if dead || self.fb.is_filled() || an.depth[i].is_none() {
                continue;
            }
            debug_assert_eq!(
                Some(self.stack.len()),
                an.depth[i],
                "abstract depth at {pc}"
            );
            self.hs_cur.clone_from(&an.hs[i]);
            self.op(pc)?;
        }
        Ok(())
    }

    // ---- speculation -------------------------------------------------------------------------

    /// Whether the value the op at `pc` leaves at stack index `at` is consumed by an op that
    /// wants a Number (so the producer speculates Num and exits after itself on anything else),
    /// looking ahead within the basic block.
    fn want_num(&self, pc: usize, at: usize) -> bool {
        // The site kept producing non-Numbers (see `helpers::note_num_exit`).
        if helpers::no_num(self.chunk, pc) {
            return false;
        }
        let mut depth = at + 1;
        let mut q = pc + 1;
        while q <= self.backedge && !self.is_leader(q) {
            let Some((pops, pushes)) = self.chunk.jit_stack_effect(q) else {
                return false;
            };
            let Some(below) = depth.checked_sub(pops) else {
                return false;
            };
            if below <= at {
                let pos = at - below;
                return match self.ops[q] {
                    Op::Sub
                    | Op::Mul
                    | Op::Div
                    | Op::Mod
                    | Op::BitAnd
                    | Op::BitOr
                    | Op::BitXor
                    | Op::Shl
                    | Op::Shr
                    | Op::UShr
                    | Op::Neg
                    | Op::BitNot
                    | Op::Lt
                    | Op::Gt
                    | Op::Le
                    | Op::Ge => true,
                    Op::JumpIfNotCmp(k, _) => !is_eq_kind(k),
                    Op::StoreLocal(s) => self.kinds[s as usize] == Kind::Num,
                    // `+` concatenates strings: only speculate when the other side is a Number.
                    Op::Add => {
                        (pos == 1
                            && q == pc + 1
                            && at >= 1
                            && matches!(self.stack.get(at - 1), Some(Entry::Num(_))))
                            || (pos == 0
                                && q == pc + 2
                                && match self.ops[pc + 1] {
                                    Op::LoadLocal(s) => self.kinds[s as usize] == Kind::Num,
                                    Op::Const(k) => {
                                        matches!(self.chunk.consts[k as usize], Value::Num(_))
                                    }
                                    _ => false,
                                })
                    }
                    _ => false,
                };
            }
            depth = below + pushes;
            q += 1;
        }
        false
    }

    // ---- generic paths -----------------------------------------------------------------------

    /// Call [`Helper::Generic`] on `run` over `frame.stack[base..depth]` (valid Boxed values in
    /// memory); a throw exits at `pc` with the entries below `base` materialized and all
    /// `depth` entries (the helper wrote back what the op left).
    fn call_generic(&mut self, run: usize, base: usize, depth: usize, pc: usize) {
        let (pops, pushes) = self.chunk.jit_stack_effect(run).unwrap_or((0, 0));
        self.max_stack = self
            .max_stack
            .max(depth)
            .max(depth.saturating_sub(pops) + pushes);
        let r = self.i32c(run as i64);
        let bv = self.i32c(base as i64);
        let dv = self.i32c(depth as i64);
        let st = self.call_js(Helper::Generic, &[self.frame, r, bv, dv]);
        let below = self.stack[..base.min(self.stack.len())].to_vec();
        self.throw_if(st, pc, &below, depth);
    }

    /// Run the op at `pc` through [`Helper::Generic`].
    fn generic(&mut self, pc: usize) {
        let (pops, pushes) = self.chunk.jit_stack_effect(pc).unwrap_or((0, 0));
        let d = self.stack.len();
        self.box_from(d - pops);
        self.call_generic(pc, d - pops, d, pc);
        self.stack.truncate(d - pops);
        self.stack.extend(std::iter::repeat_n(Entry::Boxed, pushes));
    }

    /// Emit `slow` for the op at `pc` from the current (pre-op) stack. Returns false when it
    /// exited; otherwise the stack is the op's post state with Boxed results.
    fn slow_path(&mut self, pc: usize, slow: Slow) -> bool {
        let (pops, pushes) = self.chunk.jit_stack_effect(pc).unwrap_or((0, 0));
        let d = self.stack.len();
        match slow {
            Slow::Generic => {
                self.generic(pc);
                return true;
            }
            Slow::Prop { n, c, obj } => {
                // The result is the op's last push (`GetMethod` keeps its receiver below it).
                let at = d - pops + pushes - 1;
                self.box_from(at);
                let (objp, consume) = match obj {
                    PropObj::Stack(i) => {
                        self.force(i);
                        (self.sptr(i), 1)
                    }
                    PropObj::Slot(p) => {
                        self.slot_tdz_exit(p, pc);
                        (p, 0)
                    }
                    PropObj::Ptr(p) => (p, 0),
                };
                let (nv, cv, kv) = (self.i32c(n as i64), self.i32c(c as i64), self.i32c(consume));
                let dst = self.sptr(at);
                let st = self.call_js(Helper::GetProp, &[self.frame, nv, cv, objp, kv, dst]);
                let below = self.stack[..at].to_vec();
                self.throw_if(st, pc, &below, at);
            }
        }
        self.stack.truncate(d - pops);
        self.stack.extend(std::iter::repeat_n(Entry::Boxed, pushes));
        true
    }

    /// Exit before the op at `pc` when the slot at address `p` is in its TDZ (the interpreter
    /// throws the ReferenceError).
    fn slot_tdz_exit(&mut self, p: V, pc: usize) {
        let tag = self.fb.load(MemKind::I32U8, p, 0);
        let e = self.i32c(TAG_EMPTY as i64);
        let is = self.fb.icmp(IntCC::Eq, tag, e);
        let st = self.stack.clone();
        self.exit_if(is, pc, EXIT_RESUME, &st, st.len());
    }

    /// An op with no fast path: its slow path.
    fn slow_only(&mut self, pc: usize, slow: Slow) -> Result<(), String> {
        self.slow_path(pc, slow);
        Ok(())
    }

    // ---- numbers -----------------------------------------------------------------------------

    /// JS ToInt32 of an F64, branch-free. Below 2^63 in magnitude a saturating truncation to
    /// I64 is exact, and its low 32 bits are the result; at or above, the value is integral and
    /// `x - 2^32 * floor(x / 2^32)` (exact) brings it into [0, 2^32) first. NaN and ±Infinity
    /// come out of that reduction as NaN, which saturates to 0 — ToInt32's answer.
    fn to_int32(&mut self, x: V) -> V {
        // |x| < 2^63: the truncation to I64 is exact and its low 32 bits are ToInt32 (native
        // targets use the plain conversion, as in `to_index`; wasm32's traps, so it saturates).
        // Otherwise (huge, infinite, NaN) reduce modulo 2^32 first, out of line.
        let ax = self.fb.unary(UnaryOp::Fabs, x);
        let lim = self.fb.f64const(9223372036854775808.0);
        let small = self.fb.fcmp(FloatCC::Lt, ax, lim);
        let fast = self.fb.create_block();
        let big = self.fb.create_block();
        let done = self.fb.create_block();
        let r = self.fb.append_block_param(done, Type::I32);
        self.fb.brif(small, fast, &[], big, &[]);
        self.fb.seal_block(fast);
        self.fb.seal_block(big);
        self.fb.switch_to_block(fast);
        #[cfg(target_arch = "wasm32")]
        let op = ConvOp::ToSintSat;
        #[cfg(not(target_arch = "wasm32"))]
        let op = ConvOp::ToSint;
        let t = self.fb.convert(op, Type::I64, x);
        let w = self.fb.convert(ConvOp::Wrap, Type::I32, t);
        self.fb.jump(done, &[w]);
        self.fb.switch_to_block(big);
        let w = self.to_int32_big(x);
        self.fb.jump(done, &[w]);
        self.fb.seal_block(done);
        self.fb.switch_to_block(done);
        r
    }

    /// ToInt32 of any F64 (the modular reduction).
    fn to_int32_big(&mut self, x: V) -> V {
        let ax = self.fb.unary(UnaryOp::Fabs, x);
        let lim = self.fb.f64const(9223372036854775808.0);
        let small = self.fb.fcmp(FloatCC::Lt, ax, lim);
        let two32 = self.fb.f64const(4294967296.0);
        let q = self.fb.binary(BinaryOp::Fdiv, x, two32);
        let f = self.fb.unary(UnaryOp::Floor, q);
        let hi = self.fb.binary(BinaryOp::Fmul, f, two32);
        let m = self.fb.binary(BinaryOp::Fsub, x, hi);
        let y = self.fb.select(small, x, m);
        let t = self.fb.convert(ConvOp::ToSintSat, Type::I64, y);
        self.fb.convert(ConvOp::Wrap, Type::I32, t)
    }

    /// A PTR-typed integer from F64 `x`, only meaningful when `x` is an integer in range (the
    /// callers check the round trip). Native targets use the plain truncation (x86-64 yields
    /// the "integer indefinite" out of range, which never round-trips; AArch64 saturates);
    /// wasm32 must saturate (its plain truncation traps).
    fn to_index(&mut self, x: V) -> V {
        #[cfg(target_arch = "wasm32")]
        let op = ConvOp::ToSintSat;
        #[cfg(not(target_arch = "wasm32"))]
        let op = ConvOp::ToSint;
        self.fb.convert(op, PTR, x)
    }

    /// `js_mod` through [`Helper::Binary`] (which computes Number ⊕ Number exactly like
    /// `run_vm`; never throws for two Numbers), using scratch entries above the stack.
    ///
    /// Two int32 operands with a nonzero divisor take an integer remainder inline: a zero
    /// remainder is `x * 0` (-0 for a negative or -0 dividend, like `js_mod`), and a divisor
    /// of -1 is taken as 1 (same result; no `i32::MIN % -1` trap).
    fn num_mod(&mut self, x: V, y: V) -> Result<V, String> {
        #[cfg(target_arch = "wasm32")]
        let cv = ConvOp::ToSintSat;
        #[cfg(not(target_arch = "wasm32"))]
        let cv = ConvOp::ToSint;
        let xi = self.fb.convert(cv, Type::I32, x);
        let yi = self.fb.convert(cv, Type::I32, y);
        let xb = self.fb.convert(ConvOp::FromSint, Type::F64, xi);
        let yb = self.fb.convert(ConvOp::FromSint, Type::F64, yi);
        let xok = self.fb.fcmp(FloatCC::Eq, xb, x);
        let yok = self.fb.fcmp(FloatCC::Eq, yb, y);
        let zero = self.i32c(0);
        let ynz = self.fb.icmp(IntCC::Ne, yi, zero);
        let ok = self.fb.binary(BinaryOp::Band, xok, yok);
        let ok = self.fb.binary(BinaryOp::Band, ok, ynz);
        let fast = self.fb.create_block();
        let slow = self.fb.create_block();
        let join = self.fb.create_block();
        let r = self.fb.append_block_param(join, Type::F64);
        self.fb.brif(ok, fast, &[], slow, &[]);
        self.fb.seal_block(fast);
        self.fb.seal_block(slow);
        self.fb.switch_to_block(fast);
        let m1 = self.i32c(-1);
        let one = self.i32c(1);
        let is_m1 = self.fb.icmp(IntCC::Eq, yi, m1);
        let yd = self.fb.select(is_m1, one, yi);
        let ri = self.fb.binary(BinaryOp::Srem, xi, yd);
        let rf = self.fb.convert(ConvOp::FromSint, Type::F64, ri);
        let fz = self.fb.f64const(0.0);
        let xz = self.fb.binary(BinaryOp::Fmul, x, fz);
        let rz = self.fb.icmp(IntCC::Eq, ri, zero);
        let res = self.fb.select(rz, xz, rf);
        self.fb.jump(join, &[res]);
        self.fb.switch_to_block(slow);
        let res = self.num_mod_slow(x, y)?;
        self.fb.jump(join, &[res]);
        self.fb.seal_block(join);
        self.fb.switch_to_block(join);
        Ok(r)
    }

    fn num_mod_slow(&mut self, x: V, y: V) -> Result<V, String> {
        let d = self.stack.len();
        self.store_entry(d, Entry::Num(x));
        self.store_entry(d + 1, Entry::Num(y));
        let code = self.i32c(bin_code(&Op::Mod)? as i64);
        let (a, b, r) = (self.sptr(d), self.sptr(d + 1), self.sptr(d + 2));
        self.call(Helper::Binary, &[self.frame, code, a, b, r]);
        Ok(self.stack_num(d + 2))
    }

    /// `kind.num(x, y)`.
    fn num_arith(&mut self, kind: ArithKind, x: V, y: V) -> Result<V, String> {
        let bop = match kind {
            ArithKind::Add => return Ok(self.fb.binary(BinaryOp::Fadd, x, y)),
            ArithKind::Sub => return Ok(self.fb.binary(BinaryOp::Fsub, x, y)),
            ArithKind::Mul => return Ok(self.fb.binary(BinaryOp::Fmul, x, y)),
            ArithKind::Div => return Ok(self.fb.binary(BinaryOp::Fdiv, x, y)),
            ArithKind::Mod => return self.num_mod(x, y),
            ArithKind::BitAnd => BinaryOp::Band,
            ArithKind::BitOr => BinaryOp::Bor,
            ArithKind::BitXor => BinaryOp::Bxor,
            // The IR takes shift amounts modulo 32, like `& 31`.
            ArithKind::Shl => BinaryOp::Ishl,
            ArithKind::Shr => BinaryOp::Sshr,
            ArithKind::UShr => BinaryOp::Ushr,
        };
        let a = self.to_int32(x);
        let b = self.to_int32(y);
        let r = self.fb.binary(bop, a, b);
        Ok(if kind == ArithKind::UShr {
            let w = self.fb.convert(ConvOp::Uext, Type::I64, r);
            self.fb.convert(ConvOp::FromSint, Type::F64, w)
        } else {
            self.fb.convert(ConvOp::FromSint, Type::F64, r)
        })
    }

    /// The F64 of entry `e` at index `idx`: unboxed as is, Boxed after a Number tag check that
    /// branches to `slow` otherwise.
    fn num_or_slow(&mut self, e: Entry, idx: usize, slow: Block) -> V {
        match e {
            Entry::Num(x) => x,
            Entry::Ref(p, _) => {
                let tag = self.fb.load(MemKind::I32U8, p, 0);
                let four = self.i32c(TAG_NUM as i64);
                let is = self.fb.icmp(IntCC::Eq, tag, four);
                let ok = self.fb.create_block();
                self.fb.brif(is, ok, &[], slow, &[]);
                self.fb.seal_block(ok);
                self.fb.switch_to_block(ok);
                self.fb.load(MemKind::F64, p, VALUE_PAYLOAD)
            }
            _ => {
                let tag = self.stack_tag(idx);
                let four = self.i32c(TAG_NUM as i64);
                let is = self.fb.icmp(IntCC::Eq, tag, four);
                let ok = self.fb.create_block();
                self.fb.brif(is, ok, &[], slow, &[]);
                self.fb.seal_block(ok);
                self.fb.switch_to_block(ok);
                self.stack_num(idx)
            }
        }
    }

    /// ToBoolean of entry `e` at index `idx` as I32; a Boxed entry is consumed (left trivially
    /// droppable) when `consume`, else read through a clone in scratch.
    fn truthy(&mut self, e: Entry, idx: usize, consume: bool) -> V {
        match e {
            Entry::Num(x) => {
                let z = self.fb.f64const(0.0);
                let nz = self.fb.fcmp(FloatCC::Ne, x, z);
                let ord = self.fb.fcmp(FloatCC::Eq, x, x);
                self.fb.binary(BinaryOp::Band, nz, ord)
            }
            Entry::Bool(b) => b,
            // (Not reached: `prepare` forces the operand.) A clone the Boxed path consumes.
            Entry::Ref(..) if consume => {
                self.store_entry(idx, e);
                self.truthy(Entry::Boxed, idx, true)
            }
            Entry::Ref(p, _) => {
                let scratch = self.stack.len();
                let dst = self.sptr(scratch);
                self.call(Helper::Clone, &[dst, p]);
                let t = self.call_status(Helper::ToBoolean, &[self.frame, dst]);
                t
            }
            Entry::Boxed => {
                let tag = self.stack_tag(idx);
                let three = self.i32c(TAG_BOOL as i64);
                let isb = self.fb.icmp(IntCC::Eq, tag, three);
                let fast = self.fb.create_block();
                let slow = self.fb.create_block();
                let join = self.fb.create_block();
                let r = self.fb.append_block_param(join, Type::I32);
                self.fb.brif(isb, fast, &[], slow, &[]);
                self.fb.seal_block(fast);
                self.fb.seal_block(slow);
                self.fb.switch_to_block(fast);
                let byte = self.stack_bool(idx);
                self.fb.jump(join, &[byte]);
                self.fb.switch_to_block(slow);
                let p = if consume {
                    self.sptr(idx)
                } else {
                    let scratch = self.stack.len();
                    let (dst, src) = (self.sptr(scratch), self.sptr(idx));
                    self.call(Helper::Clone, &[dst, src]);
                    dst
                };
                let t = self.call_status(Helper::ToBoolean, &[self.frame, p]);
                self.fb.jump(join, &[t]);
                self.fb.seal_block(join);
                self.fb.switch_to_block(join);
                r
            }
        }
    }

    // ---- ops ---------------------------------------------------------------------------------

    fn op(&mut self, pc: usize) -> Result<(), String> {
        let op = self.ops[pc];
        self.prepare(pc);
        let d = self.stack.len();
        if let (true, Op::InEnv(s), Op::MakeClosure(k, n)) =
            (pc > 0, self.ops[pc.saturating_sub(1)], op)
        {
            // `InEnv` + `MakeClosure`: the closure over the block env.
            self.max_stack = self.max_stack.max(d + 1);
            let (sv, kv, nv, dst) = (
                self.i32c(s as i64),
                self.i32c(k as i64),
                self.i32c(n as i64),
                self.sptr(d),
            );
            self.call(Helper::MakeClosureIn, &[self.frame, sv, kv, nv, dst]);
            self.stack.push(Entry::Boxed);
            return Ok(());
        }
        if pc > 0 && matches!(self.ops[pc - 1], Op::InEnv(_)) {
            // `InEnv` and the closure-creating op after it, run as one generic op.
            let (pops, pushes) = self.chunk.jit_stack_effect(pc).unwrap_or((0, 0));
            self.box_from(d - pops);
            self.max_stack = self.max_stack.max(d - pops + pushes);
            self.call_generic(pc - 1, d - pops, d, pc - 1);
            self.stack.truncate(d - pops);
            self.stack.extend(std::iter::repeat_n(Entry::Boxed, pushes));
            return Ok(());
        }
        // The parts of an inline `Math` call.
        if self.plan.math.contains_key(&pc) {
            return self.math_begin(pc);
        }
        if self.site_get_method(pc) {
            // `GetMethod`: the placeholder receiver stays, a placeholder callee joins it.
            self.set_stack_tag(d, TAG_UNDEFINED);
            self.stack.push(Entry::Boxed);
            return Ok(());
        }
        if self.plan.strm.contains_key(&pc) {
            return self.str_begin(pc);
        }
        if self.plan.strm.values().any(|&c| c == pc) {
            return self.str_call(pc);
        }
        if self.plan.strn.contains_key(&pc) {
            return self.strn_begin(pc);
        }
        if let Some((&g, &(_, cell))) = self.plan.strn.iter().find(|(_, v)| v.0 == pc) {
            return self.strn_call(pc, g, cell, Helper::StrMethod);
        }
        if self.plan.colm.contains_key(&pc) {
            return self.colm_begin(pc);
        }
        if let Some((&g, &(_, cell))) = self.plan.colm.iter().find(|(_, v)| v.0 == pc) {
            return self.strn_call(pc, g, cell, Helper::CollMethod);
        }
        if self.plan.arrm.contains_key(&pc) {
            return self.arr_begin(pc);
        }
        if self.plan.arrm.values().any(|&c| c == pc) {
            return self.arr_call(pc);
        }
        if let Some(site) = self.plan.math.values().find(|s| s.call == pc).copied() {
            return match site.f {
                Intr::Math(f) => self.math_call(pc, f),
                Intr::Fast(k) => self.fast_call(pc, k),
                Intr::Inline(k) => self.inline_call(pc, k),
            };
        }
        match op {
            // Region handlers are static (see `Analysis::hs`): nothing to emit.
            Op::PushHandler(_) | Op::PopHandler => {}
            Op::IterStepL(is, ns) => self.iter_step(pc, is as usize, ns as usize),
            // Runs with the op after it (above).
            Op::InEnv(_) => {}
            // An async body suspends in the interpreter: native code runs up to the `await`
            // (async-design.md §4.6, stage 1), whose operand is materialized on the stack. A
            // loop region containing one re-enters at its header on the next backedge.
            Op::Await => self.exit_now(pc),
            Op::IterRestL(a, b) => {
                if self.kinds[a as usize] == Kind::Boxed && self.kinds[b as usize] == Kind::Boxed {
                    self.generic(pc);
                } else {
                    self.exit_now(pc);
                }
            }
            // Function code (the analysis rejects it in loop regions): the interpreter runs it.
            Op::ForInStepL(..) => self.exit_now(pc),
            Op::IterCloseL(s) => {
                if self.kinds[s as usize] == Kind::Boxed {
                    self.generic(pc);
                } else {
                    self.exit_now(pc);
                }
            }
            // Always abrupt (close in throw mode, rethrow): the interpreter runs it.
            Op::IterAbortL(_) => self.exit_now(pc),
            Op::Throw if !self.hs_cur.is_empty() => {
                // Caught by a region handler: hand the value over as the pending exception.
                self.box_from(d - 1);
                let src = self.sptr(d - 1);
                self.call(Helper::SetException, &[self.frame, src]);
                let below = self.stack[..d - 1].to_vec();
                self.emit_exit(pc, EXIT_THROW, &below, d - 1, None);
            }
            Op::Const(k) => self.push_const(k as usize),
            Op::LoadThis => {
                let p = self.fb.load(PTR_MEM, self.frame, FRAME_THIS);
                self.stack.push(Entry::Ref(p, Src::This));
            }
            Op::LoadCap(n) => {
                let p = self.src_ptr(Src::Cap(n), None);
                self.ptr_or_exit(p, pc);
                self.stack.push(Entry::Ref(p, Src::Cap(n)));
            }
            Op::LoadName(n, c) | Op::LoadNameForCall(n, c) if self.plan.dname.contains_key(&pc) => {
                self.direct_name(pc, n, c, matches!(op, Op::LoadNameForCall(..)));
            }
            Op::LoadName(n, c) if self.plan.name_ref.contains(&pc) => {
                let p = self.src_ptr(Src::Name(n), Some(c));
                self.ptr_or_exit(p, pc);
                self.stack.push(Entry::Ref(p, Src::Name(n)));
            }
            Op::LoadName(n, c) if self.plan.glob_num.contains(&pc) => {
                let p = self.src_ptr(Src::Glob(n), Some(c));
                self.ptr_or_exit(p, pc);
                let miss = self.fb.create_block();
                let bits = layout::entry_word(&mut self.fb, p, 0, miss);
                let f = layout::word_num(&mut self.fb, bits, miss);
                let cont = self.fb.create_block();
                self.fb.jump(cont, &[]);
                self.fb.seal_block(miss);
                self.fb.switch_to_block(miss);
                self.exit_now(pc);
                self.fb.seal_block(cont);
                self.fb.switch_to_block(cont);
                self.stack.push(Entry::Num(f));
            }
            Op::LoadName(n, c) if self.plan.glob_ref.contains(&pc) => {
                // The property's value word, copied (or retained) straight into the entry.
                let p = self.src_ptr(Src::Glob(n), Some(c));
                let slow = self.fb.create_block();
                let join = self.fb.create_block();
                let z = self.ptrc(0);
                let nonnull = self.fb.icmp(IntCC::Ne, p, z);
                let ok = self.fb.create_block();
                self.fb.brif(nonnull, ok, &[], slow, &[]);
                self.fb.seal_block(ok);
                self.fb.switch_to_block(ok);
                let bits = layout::entry_word(&mut self.fb, p, 0, slow);
                let (tag, payload, is_ref) = layout::word_value(&mut self.fb, bits, slow);
                let dst = self.sptr(d);
                let refb = self.fb.create_block();
                let plain = self.fb.create_block();
                self.fb.brif(is_ref, refb, &[], plain, &[]);
                self.fb.seal_block(refb);
                self.fb.seal_block(plain);
                self.fb.switch_to_block(refb);
                let zero = self.i32c(0);
                self.call(Helper::UnpackClone, &[dst, bits, zero]);
                self.fb.jump(join, &[]);
                self.fb.switch_to_block(plain);
                self.fb.store(MemKind::I32U8, dst, tag, 0);
                let byte = self.fb.convert(ConvOp::Wrap, Type::I32, payload);
                self.fb.store(MemKind::I32U8, dst, byte, VALUE_BOOL);
                self.fb.store(MemKind::I64, dst, payload, VALUE_PAYLOAD);
                self.fb.jump(join, &[]);
                self.fb.seal_block(slow);
                self.fb.switch_to_block(slow);
                self.load_name_helper(pc, n, c, Helper::LoadName);
                self.fb.jump(join, &[]);
                self.fb.seal_block(join);
                self.fb.switch_to_block(join);
                self.stack.push(Entry::Boxed);
            }
            Op::LoadName(n, c) | Op::LoadNameForCall(n, c) => {
                let h = if matches!(op, Op::LoadName(..)) {
                    Helper::LoadName
                } else {
                    Helper::LoadNameForCall
                };
                self.load_name_helper(pc, n, c, h);
                self.stack.push(Entry::Boxed);
                if h == Helper::LoadNameForCall {
                    self.stack.push(Entry::Boxed);
                }
            }
            Op::StoreNameCached(n, c) if self.plan.name_store.contains_key(&pc) => {
                self.store_name_inline(pc, n, c, self.plan.name_store[&pc]);
            }
            Op::StoreNameCached(n, c) => {
                self.store_entry(d - 1, self.stack[d - 1]);
                let (nv, cv, src) = (self.i32c(n as i64), self.i32c(c as i64), self.sptr(d - 1));
                let st = self.call_js(Helper::StoreName, &[self.frame, nv, cv, src]);
                let below = self.stack[..d - 1].to_vec();
                self.throw_if(st, pc, &below, d - 1);
                self.stack.pop();
            }
            Op::StoreCap(n) => self.store_cap(pc, n)?,
            Op::StoreCapInit(n) => {
                self.store_entry(d - 1, self.stack[d - 1]);
                let (nv, src, one) = (self.i32c(n as i64), self.sptr(d - 1), self.i32c(1));
                let st = self.call_js(Helper::StoreCap, &[self.frame, nv, src, one]);
                let below = self.stack[..d - 1].to_vec();
                self.throw_if(st, pc, &below, d - 1);
                self.stack.pop();
            }
            Op::UpdateCap(n, k) => self.update_ptr(pc, Src::Cap(n), None, k),
            Op::UpdateNameCached(n, c, k) if self.plan.name_store.contains_key(&pc) => {
                self.update_ptr(pc, Src::NameW(n), Some(c), k)
            }
            Op::New(argc) if self.plan.dnew.contains_key(&pc) => {
                self.plan_new(pc, argc as usize);
            }
            Op::Call(argc) | Op::CallWithThis(argc) | Op::New(argc)
                if self.plan.direct.contains_key(&pc) =>
            {
                self.direct_call(pc, argc as usize, matches!(op, Op::CallWithThis(_)));
            }
            // A proper tail call: directly within `TAIL_NEST` frames, a self tail call as a loop
            // where it can be. (Only a `this` method body that can neither call nor throw is
            // inlined, see `inline_this`: no frame to release, nothing observable.)
            Op::TailCall(argc, wt) if self.plan.tail_loops.contains(&pc) => {
                self.self_tail(pc, argc as usize, wt);
            }
            Op::TailCall(argc, wt) if self.plan.direct.contains_key(&pc) => {
                self.direct_call(pc, argc as usize, wt);
            }
            Op::Call(argc) | Op::CallWithThis(argc) | Op::TailCall(argc, _) => {
                let with_this = matches!(op, Op::CallWithThis(_) | Op::TailCall(_, true));
                let tail = if matches!(op, Op::TailCall(..)) {
                    helpers::CALL_TAIL as i64
                } else {
                    0
                };
                let base = d - argc as usize - 1 - with_this as usize;
                self.box_from(base);
                let (bv, av, wv) = (
                    self.i32c(base as i64),
                    self.i32c(argc as i64),
                    self.i32c(with_this as i64 | self.await_fused(pc) | tail),
                );
                self.set_call_site(pc);
                let st = self.call_js(Helper::Call, &[self.frame, bv, av, wv]);
                let below = self.stack[..base].to_vec();
                self.throw_if(st, pc, &below, base);
                self.stack.truncate(base);
                self.stack.push(Entry::Boxed);
            }
            Op::GetMethod(..) if self.plan.dmethod.contains_key(&pc) && self.direct_method(pc) => {}
            Op::GetMethod(n, c) => {
                // A property read that keeps its receiver (`Helper::GetMethod` is `GetProp`
                // on a borrowed receiver): the inline cache, else that helper.
                self.box_from(d - 1);
                let obj = self.sptr(d - 1);
                let slow = Slow::Prop {
                    n,
                    c,
                    obj: PropObj::Ptr(obj),
                };
                self.prop_read(pc, obj, n, c, None, d, slow, None)?;
            }
            Op::SetPropDrop(n, c) => {
                let obj = self.ref_receiver(d - 2);
                self.prop_write(pc, n, c, obj, Some(d - 2))?;
            }
            Op::SetPropThisDrop(n, c) => {
                let obj = self.fb.load(PTR_MEM, self.frame, FRAME_THIS);
                self.prop_write(pc, n, c, obj, None)?;
            }
            Op::SetPropLocalDrop(s, n, c) => {
                if self.kinds[s as usize] != Kind::Boxed {
                    self.exit_now(pc);
                    return Ok(());
                }
                let obj = self.slot_ptr(s as usize);
                self.slot_tdz_exit(obj, pc);
                self.prop_write(pc, n, c, obj, None)?;
            }
            Op::Undef => {
                self.set_stack_tag(d, TAG_UNDEFINED);
                self.stack.push(Entry::Boxed);
            }
            Op::MakeClosure(k, n) => {
                self.max_stack = self.max_stack.max(d + 1);
                let (kv, nv, dst) = (self.i32c(k as i64), self.i32c(n as i64), self.sptr(d));
                self.call(Helper::MakeClosure, &[self.frame, kv, nv, dst]);
                self.stack.push(Entry::Boxed);
            }
            Op::MakeArray(n) | Op::MakeObject(_, n, _) => {
                // The literal from the operands in place (no generic op round trip).
                let n = n as usize;
                self.box_from(d - n);
                self.max_stack = self.max_stack.max(d - n + 1);
                let (bp, nv) = (self.sptr(d - n), self.i32c(n as i64));
                match op {
                    Op::MakeArray(_) => {
                        self.call(Helper::AllocArr, &[self.frame, bp, nv]);
                    }
                    Op::MakeObject(start, _, tidx) if self.lit_template(start, n, tidx).is_some() => {
                        let t = self.lit_template(start, n, tidx).expect("checked");
                        let tp = self.ptrc(t as i64);
                        self.call(Helper::AllocObj, &[self.frame, tp, bp, nv]);
                    }
                    _ => {
                        let pv = self.i32c(pc as i64);
                        self.call(Helper::MakeLit, &[self.frame, pv, bp, nv]);
                    }
                }
                self.stack.truncate(d - n);
                self.stack.push(Entry::Boxed);
            }
            Op::ArrayAppend => {
                // One element onto the literal under it, in place (no generic op round trip).
                self.box_from(d - 2);
                let (ap, vp) = (self.sptr(d - 2), self.sptr(d - 1));
                self.call(Helper::ArrayAppend, &[ap, vp]);
                self.stack.truncate(d - 2);
                self.stack.push(Entry::Boxed);
            }
            Op::Dup => self.dup(d - 1),
            Op::Dup2 => {
                self.dup(d - 2);
                self.dup(d - 1);
            }
            Op::Pop => {
                if self.stack.pop() == Some(Entry::Boxed) {
                    self.drop_at(d - 1);
                }
            }
            Op::Void => {
                if self.stack.pop() == Some(Entry::Boxed) {
                    self.drop_at(d - 1);
                }
                self.set_stack_tag(d - 1, TAG_UNDEFINED);
                self.stack.push(Entry::Boxed);
            }
            Op::LoadLocal(s) => self.load_local(pc, s as usize),
            Op::StoreLocal(s) => self.store_local(pc, s as usize),
            Op::Tdz(s) => {
                let s = s as usize;
                if let Some(flag) = self.tdz[s] {
                    let one = self.i32c(1);
                    self.fb.def_var(flag, one);
                } else if self.vars[s].is_some() {
                    return Err(format!("Tdz of SSA slot {s} without a flag"));
                } else if !self.tdz_dead(pc, s) {
                    self.set_stack_tag(d, TAG_EMPTY);
                    self.move_to_slot(s, d);
                }
            }
            Op::UpdateLocal(s, k) => self.update_local(pc, s as usize, k),
            Op::ArithLL(k, dst, a, b) => {
                self.arith_local(pc, k, dst as usize, Opnd::Slot(a), Opnd::Slot(b))?
            }
            Op::ArithLK(k, dst, a, c) => {
                self.arith_local(pc, k, dst as usize, Opnd::Slot(a), Opnd::Const(c))?
            }
            Op::JumpIfNotCmpLL(k, a, b, t) => {
                self.cmp_local(pc, k, Opnd::Slot(a), Opnd::Slot(b), t as usize)?
            }
            Op::JumpIfNotCmpLK(k, a, c, t) => {
                self.cmp_local(pc, k, Opnd::Slot(a), Opnd::Const(c), t as usize)?
            }
            Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Mod
            | Op::BitAnd
            | Op::BitOr
            | Op::BitXor
            | Op::Shl
            | Op::Shr
            | Op::UShr
            | Op::Lt
            | Op::Gt
            | Op::Le
            | Op::Ge
            | Op::EqEq
            | Op::NotEq
            | Op::StrictEq
            | Op::StrictNotEq => self.binop(pc, op)?,
            Op::Neg | Op::Plus | Op::BitNot => match self.stack[d - 1] {
                Entry::Num(x) => {
                    let r = match op {
                        Op::Neg => self.fb.unary(UnaryOp::Fneg, x),
                        Op::Plus => x,
                        _ => {
                            let i = self.to_int32(x);
                            let m = self.i32c(-1);
                            let n = self.fb.binary(BinaryOp::Bxor, i, m);
                            self.fb.convert(ConvOp::FromSint, Type::F64, n)
                        }
                    };
                    self.stack[d - 1] = Entry::Num(r);
                }
                _ => self.generic(pc),
            },
            Op::Not => {
                let e = self.stack[d - 1];
                let t = self.truthy(e, d - 1, true);
                let one = self.i32c(1);
                let r = self.fb.binary(BinaryOp::Bxor, t, one);
                self.stack[d - 1] = Entry::Bool(r);
            }
            Op::TypeofIs(kind, neg) => {
                use crate::bytecode::TypeofKind;
                let known = match self.stack[d - 1] {
                    Entry::Num(_) => Some(kind == TypeofKind::Number),
                    Entry::Bool(_) => Some(kind == TypeofKind::Boolean),
                    Entry::Boxed | Entry::Ref(..) => None,
                };
                match known {
                    Some(k) => {
                        let r = self.i32c((k != neg) as i64);
                        self.stack[d - 1] = Entry::Bool(r);
                    }
                    None => self.generic(pc),
                }
            }
            Op::Jump(t) => {
                let t = t as usize;
                if self.in_region(t) {
                    if t <= pc {
                        self.safepoint(t);
                    }
                    let b = self.edge(t <= pc, t)?;
                    self.fb.jump(b, &[]);
                    self.flush_seals();
                } else {
                    // Let the interpreter take the jump: an outward backward jump is an outer
                    // loop's turn (its safepoint and OSR hook).
                    self.exit_now(pc);
                }
            }
            Op::JumpIfFalse(t) => {
                let e = self.stack.pop().expect("depth checked");
                let c = self.truthy(e, d - 1, true);
                self.branch(pc, c, pc + 1, t as usize)?;
            }
            Op::JumpIfFalsePeek(t) | Op::JumpIfTruePeek(t) => {
                let e = self.stack[d - 1];
                let c = self.truthy(e, d - 1, false);
                if matches!(op, Op::JumpIfTruePeek(_)) {
                    self.branch(pc, c, t as usize, pc + 1)?;
                } else {
                    self.branch(pc, c, pc + 1, t as usize)?;
                }
            }
            Op::JumpIfNotNullishPeek(t) => match self.stack[d - 1] {
                Entry::Boxed => {
                    let tag = self.stack_tag(d - 1);
                    let u = self.i32c(TAG_UNDEFINED as i64);
                    let n = self.i32c(TAG_NULL as i64);
                    let a = self.fb.icmp(IntCC::Eq, tag, u);
                    let b = self.fb.icmp(IntCC::Eq, tag, n);
                    let nullish = self.fb.binary(BinaryOp::Bor, a, b);
                    self.branch(pc, nullish, pc + 1, t as usize)?;
                }
                _ => {
                    let st = self.stack.clone();
                    let dst = self.dest(pc, t as usize)?;
                    self.fb.jump(dst.0, &[]);
                    self.flush_seals();
                    self.fill_exit(dst, &st);
                }
            },
            Op::JumpIfNotCmp(k, t) => {
                if k == CmpKind::Lt {
                    self.set_idx_fact(pc);
                }
                let c = self.cmp_stack(pc, k)?;
                self.branch(pc, c, pc + 1, t as usize)?;
            }
            Op::Return => {
                if d != 1 {
                    self.exit_now(pc);
                } else {
                    let st = self.stack.clone();
                    self.emit_exit(pc, EXIT_RETURN, &st, 1, None);
                }
            }
            Op::ReturnUndef => {
                if d != 0 {
                    self.exit_now(pc);
                } else {
                    self.set_stack_tag(0, TAG_UNDEFINED);
                    self.emit_exit(pc, EXIT_RETURN, &[], 1, None);
                }
            }
            // Let the interpreter throw (and find its handler) / apply the [[Construct]] rule.
            Op::Throw | Op::DerivedReturn => self.exit_now(pc),
            Op::GetElem => match (self.stack[d - 2], self.stack[d - 1]) {
                (Entry::Boxed, Entry::Num(key)) => {
                    let want = self.want_num(pc, d - 2);
                    let obj = self.sptr(d - 2);
                    self.elem_read(pc, obj, key, Some(d - 2), d - 2, want, Slow::Generic, None, None);
                }
                (Entry::Ref(obj, src), Entry::Num(key)) => {
                    let want = self.want_num(pc, d - 2);
                    let fact = self.idx_fact_at(pc, src);
                    self.elem_read(pc, obj, key, None, d - 2, want, Slow::Generic, fact, Some(src));
                }
                (o @ (Entry::Boxed | Entry::Ref(..)), Entry::Boxed) => {
                    // A String key on a plain object: a data property natively.
                    let (obj, consume) = match o {
                        Entry::Ref(p, _) => (p, 0),
                        _ => (self.sptr(d - 2), 1),
                    };
                    let (kp, dst) = (self.sptr(d - 1), self.sptr(d - 2));
                    let cv = self.i32c(consume);
                    let r = self
                        .call(Helper::ElemGetStr, &[self.frame, obj, kp, dst, cv])
                        .expect("ElemGetStr returns a flag");
                    let z = self.i32c(0);
                    let hit = self.fb.icmp(IntCC::Ne, r, z);
                    let gen = self.fb.create_block();
                    let join = self.fb.create_block();
                    self.fb.brif(hit, join, &[], gen, &[]);
                    self.fb.seal_block(gen);
                    self.fb.switch_to_block(gen);
                    self.generic(pc);
                    self.fb.jump(join, &[]);
                    self.fb.seal_block(join);
                    self.fb.switch_to_block(join);
                    self.stack.truncate(d - 2);
                    self.stack.push(Entry::Boxed);
                }
                _ => self.generic(pc),
            },
            // `obj[key](...)`: the element read leaves the receiver in place under the method.
            Op::GetMethodElem => match (self.stack[d - 2], self.stack[d - 1]) {
                (Entry::Boxed, Entry::Num(key)) => {
                    let obj = self.sptr(d - 2);
                    self.elem_read(pc, obj, key, None, d - 1, false, Slow::Generic, None, None);
                }
                _ => self.generic(pc),
            },
            Op::ArgsLen(s, _) if self.kinds[s as usize] == Kind::Boxed => {
                // The count (a Number) in the slot; a materialized object's `length` in the
                // interpreter.
                let p = self.slot_ptr(s as usize);
                let tag = self.fb.load(MemKind::I32U8, p, 0);
                let tn = self.i32c(TAG_NUM as i64);
                let other = self.fb.icmp(IntCC::Ne, tag, tn);
                let st = self.stack.clone();
                self.exit_if(other, pc, EXIT_RESUME, &st, d);
                let n = self.fb.load(MemKind::F64, p, VALUE_PAYLOAD);
                self.stack.push(Entry::Num(n));
            }
            Op::ArgsGet(s, tag) if self.kinds[s as usize] == Kind::Boxed => {
                let Entry::Num(k) = self.stack[d - 1] else {
                    self.generic(pc);
                    return Ok(());
                };
                // An integer key below the count reads its parameter slot.
                let base = (tag & !crate::bytecode::VIRT_REST) as i64;
                let p = self.slot_ptr(s as usize);
                let slow = self.fb.create_block();
                let join = self.fb.create_block();
                let chk = self.fb.create_block();
                let t = self.fb.load(MemKind::I32U8, p, 0);
                let tn = self.i32c(TAG_NUM as i64);
                let is_num = self.fb.icmp(IntCC::Eq, t, tn);
                self.fb.brif(is_num, chk, &[], slow, &[]);
                self.fb.seal_block(chk);
                self.fb.switch_to_block(chk);
                let n = self.fb.load(MemKind::F64, p, VALUE_PAYLOAD);
                let z = self.fb.f64const(0.0);
                let ge = self.fb.fcmp(FloatCC::Ge, k, z);
                let lt = self.fb.fcmp(FloatCC::Lt, k, n);
                let inr = self.fb.binary(BinaryOp::Band, ge, lt);
                let fast = self.fb.create_block();
                let idx_b = self.fb.create_block();
                self.fb.brif(inr, idx_b, &[], slow, &[]);
                self.fb.seal_block(idx_b);
                self.fb.switch_to_block(idx_b);
                let ki = self.to_index(k);
                let back = self.fb.convert(ConvOp::FromSint, Type::F64, ki);
                let int = self.fb.fcmp(FloatCC::Eq, back, k);
                self.fb.brif(int, fast, &[], slow, &[]);
                self.fb.seal_block(fast);
                self.fb.seal_block(slow);
                self.fb.switch_to_block(fast);
                let vs = self.ptrc(VALUE_SIZE as i64);
                let off = self.fb.binary(BinaryOp::Imul, ki, vs);
                let bo = self.ptrc(base * VALUE_SIZE as i64);
                let off = self.fb.binary(BinaryOp::Iadd, off, bo);
                let src = self.fb.binary(BinaryOp::Iadd, self.slots, off);
                let dst = self.sptr(d - 1);
                self.clone_mem(dst, src);
                self.fb.jump(join, &[]);
                self.fb.switch_to_block(slow);
                self.generic(pc);
                self.fb.jump(join, &[]);
                self.fb.seal_block(join);
                self.fb.switch_to_block(join);
            }
            Op::ArgsLen(..) | Op::ArgsGet(..) => self.exit_now(pc),
            Op::MakeRegExp(..) => {
                self.soff(d + 1);
                let (pcv, dst) = (self.i32c(pc as i64), self.sptr(d));
                let r = self
                    .call(Helper::MakeRegExp, &[self.frame, pcv, dst])
                    .expect("MakeRegExp returns a flag");
                self.hit_or_generic(pc, r);
            }
            Op::ToStr => {
                // A primitive other than a Symbol in place; anything else through the generic op.
                self.box_from(d - 1);
                let p = self.sptr(d - 1);
                let r = self
                    .call(Helper::ToStrPrim, &[self.frame, p])
                    .expect("ToStrPrim returns a flag");
                self.hit_or_generic(pc, r);
            }
            Op::Concat(n) => {
                let n = n as usize;
                self.box_from(d - n);
                let (base, nv) = (self.sptr(d - n), self.i32c(n as i64));
                let r = self
                    .call(Helper::ConcatStr, &[self.frame, base, nv])
                    .expect("ConcatStr returns a flag");
                self.hit_or_generic(pc, r);
            }
            Op::DestructureArr(n) => {
                // A pristine dense Array natively; anything else through the generic op.
                self.box_from(d - 1);
                self.max_stack = self.max_stack.max(d - 1 + n as usize);
                if (1..=8).contains(&n) {
                    self.destructure_inline(pc, d, n as usize);
                    return Ok(());
                }
                let p = self.sptr(d - 1);
                let nv = self.i32c(n as i64);
                let r = self
                    .call(Helper::DestructDense, &[self.frame, nv, p])
                    .expect("DestructDense returns a flag");
                let z = self.i32c(0);
                let hit = self.fb.icmp(IntCC::Ne, r, z);
                let slow = self.fb.create_block();
                let join = self.fb.create_block();
                self.fb.brif(hit, join, &[], slow, &[]);
                self.fb.seal_block(slow);
                self.fb.switch_to_block(slow);
                self.generic(pc);
                self.fb.jump(join, &[]);
                self.fb.seal_block(join);
                self.fb.switch_to_block(join);
            }
            Op::GetElemLocal(s) => {
                if self.kinds[s as usize] != Kind::Boxed {
                    self.exit_now(pc);
                    return Ok(());
                }
                match self.stack[d - 1] {
                    Entry::Num(key) => {
                        let want = self.want_num(pc, d - 1);
                        let obj = self.slot_ptr(s as usize);
                        let fact = self.idx_fact_at(pc, Src::Slot(s));
                        self.elem_read(pc, obj, key, None, d - 1, want, Slow::Generic, fact, Some(Src::Slot(s)));
                    }
                    _ => self.generic(pc),
                }
            }
            Op::SetElem | Op::SetElemDrop => {
                let keep = matches!(op, Op::SetElem);
                match (self.stack[d - 3], self.stack[d - 2], self.stack[d - 1]) {
                    (Entry::Boxed, Entry::Num(key), v @ (Entry::Num(_) | Entry::Boxed | Entry::Ref(..))) => {
                        let obj = self.sptr(d - 3);
                        self.elem_write(pc, obj, key, v, Some(d - 3), keep, d - 3, Slow::Generic, None);
                    }
                    (Entry::Ref(obj, src), Entry::Num(key), v @ (Entry::Num(_) | Entry::Boxed | Entry::Ref(..))) => {
                        self.elem_write(pc, obj, key, v, None, keep, d - 3, Slow::Generic, Some(src));
                    }
                    _ => self.generic(pc),
                }
            }
            Op::SetElemLocal(s) | Op::SetElemLocalDrop(s) => {
                let keep = matches!(op, Op::SetElemLocal(_));
                if self.kinds[s as usize] != Kind::Boxed {
                    self.exit_now(pc);
                    return Ok(());
                }
                match (self.stack[d - 2], self.stack[d - 1]) {
                    (Entry::Num(key), v @ (Entry::Num(_) | Entry::Boxed)) => {
                        let obj = self.slot_ptr(s as usize);
                        self.elem_write(pc, obj, key, v, None, keep, d - 2, Slow::Generic, Some(Src::Slot(s)));
                    }
                    _ => self.generic(pc),
                }
            }
            Op::GetProp(n, c) => match self.stack[d - 1] {
                Entry::Boxed => {
                    let obj = self.sptr(d - 1);
                    let slow = Slow::Prop {
                        n,
                        c,
                        obj: PropObj::Stack(d - 1),
                    };
                    self.prop_read(pc, obj, n, c, Some(d - 1), d - 1, slow, None)?;
                }
                Entry::Ref(obj, src) => {
                    let slow = Slow::Prop {
                        n,
                        c,
                        obj: if src.in_env() {
                            PropObj::Stack(d - 1)
                        } else {
                            PropObj::Ptr(obj)
                        },
                    };
                    if self.chains(pc, n, c) {
                        self.chain_read(pc, obj, c, d - 1, |t| {
                            t.prop_read(pc, obj, n, c, None, d - 1, slow, Some(src))
                        })?;
                    } else {
                        self.prop_read(pc, obj, n, c, None, d - 1, slow, Some(src))?;
                    }
                }
                _ => self.generic(pc),
            },
            Op::GetPropThis(n, c) => {
                let obj = self.fb.load(PTR_MEM, self.frame, FRAME_THIS);
                let slow = Slow::Prop {
                    n,
                    c,
                    obj: PropObj::Ptr(obj),
                };
                self.prop_read(pc, obj, n, c, None, d, slow, Some(Src::This))?;
            }
            Op::GetPropLocal(s, n, c) => {
                if self.kinds[s as usize] != Kind::Boxed {
                    self.exit_now(pc);
                    return Ok(());
                }
                let obj = self.slot_ptr(s as usize);
                let slow = Slow::Prop {
                    n,
                    c,
                    obj: PropObj::Slot(obj),
                };
                if self.chains(pc, n, c) {
                    self.chain_read(pc, obj, c, d, |t| {
                        t.prop_read(pc, obj, n, c, None, d, slow, Some(Src::Slot(s)))
                    })?;
                } else {
                    self.prop_read(pc, obj, n, c, None, d, slow, Some(Src::Slot(s)))?;
                }
            }
            Op::ToPropKey => {
                // As `ToPropKeyLocal`, with the base on the stack (forced by `prepare`).
                match self.stack[d - 1] {
                    Entry::Num(_) => {}
                    Entry::Bool(_) | Entry::Ref(..) => self.exit_now(pc),
                    Entry::Boxed => {
                        let tag = self.stack_tag(d - 1);
                        let four = self.i32c(TAG_NUM as i64);
                        let six = self.i32c(TAG_STR as i64);
                        let a = self.fb.icmp(IntCC::Ne, tag, four);
                        let b = self.fb.icmp(IntCC::Ne, tag, six);
                        let other = self.fb.binary(BinaryOp::Band, a, b);
                        let st = self.stack.clone();
                        self.exit_if(other, pc, EXIT_RESUME, &st, d);
                    }
                }
            }
            Op::AppendProp(n, c) => self.append_prop(pc, n, c)?,
            Op::ToPropKeyLocal(s) => {
                // Num and Str keys pass through untouched; anything else needs the coercion
                // (and the base check): the interpreter does it.
                if self.kinds[s as usize] != Kind::Boxed {
                    self.exit_now(pc);
                    return Ok(());
                }
                match self.stack[d - 1] {
                    Entry::Num(_) => {}
                    Entry::Bool(_) | Entry::Ref(..) => self.exit_now(pc),
                    Entry::Boxed => {
                        let tag = self.stack_tag(d - 1);
                        let four = self.i32c(TAG_NUM as i64);
                        let six = self.i32c(TAG_STR as i64);
                        let a = self.fb.icmp(IntCC::Ne, tag, four);
                        let b = self.fb.icmp(IntCC::Ne, tag, six);
                        let other = self.fb.binary(BinaryOp::Band, a, b);
                        let st = self.stack.clone();
                        self.exit_if(other, pc, EXIT_RESUME, &st, d);
                    }
                }
            }
            Op::BlkNew(dst, parent) => {
                let (a, b) = (self.i32c(dst as i64), self.i32c(parent as i64));
                self.call(Helper::BlkNew, &[self.frame, a, b]);
            }
            Op::BlkCopy(s) => {
                let a = self.i32c(s as i64);
                self.call(Helper::BlkCopy, &[self.frame, a]);
            }
            Op::BlkDecl(s, n, k) => {
                let (a, b, c) = (self.i32c(s as i64), self.i32c(n as i64), self.i32c(k as i64));
                self.call(Helper::BlkDecl, &[self.frame, a, b, c]);
            }
            Op::BlkLoad(s, n) => {
                self.max_stack = self.max_stack.max(d + 1);
                let (a, b, dst) = (self.i32c(s as i64), self.i32c(n as i64), self.sptr(d));
                let st = self.call_status(Helper::BlkLoad, &[self.frame, a, b, dst]);
                let below = self.stack.clone();
                self.throw_if(st, pc, &below, d);
                self.stack.push(Entry::Boxed);
            }
            Op::BlkUpdate(..) => {
                let (_, pushes) = self.chunk.jit_stack_effect(pc).unwrap_or((0, 0));
                self.max_stack = self.max_stack.max(d + pushes);
                let (pv, dst) = (self.i32c(pc as i64), self.sptr(d));
                if pushes > 0 {
                    self.set_stack_tag(d, TAG_UNDEFINED);
                }
                let st = self.call_status(Helper::BlkUpdate, &[self.frame, pv, dst]);
                let below = self.stack.clone();
                self.throw_if(st, pc, &below, d);
                self.stack.extend(std::iter::repeat_n(Entry::Boxed, pushes));
            }
            Op::BlkStore(s, n) | Op::BlkInit(s, n) => {
                self.box_from(d - 1);
                let init = matches!(op, Op::BlkInit(..));
                let (a, b, src, iv) = (
                    self.i32c(s as i64),
                    self.i32c(n as i64),
                    self.sptr(d - 1),
                    self.i32c(init as i64),
                );
                let st = self.call_status(Helper::BlkStore, &[self.frame, a, b, src, iv]);
                self.stack.truncate(d - 1);
                let below = self.stack.clone();
                self.throw_if(st, pc, &below, d - 1);
            }
            _ if helpers::generic_ok(&op) => self.generic(pc),
            _ => return Err(format!("unsupported op {op:?} at {pc}")),
        }
        Ok(())
    }

    fn push_const(&mut self, k: usize) {
        let d = self.stack.len();
        let e = match &self.chunk.consts[k] {
            Value::Num(n) => Entry::Num(self.fb.f64const(*n)),
            Value::Bool(b) => Entry::Bool(self.i32c(*b as i64)),
            Value::Undefined => {
                self.set_stack_tag(d, TAG_UNDEFINED);
                Entry::Boxed
            }
            Value::Null => {
                self.set_stack_tag(d, TAG_NULL);
                Entry::Boxed
            }
            _ => {
                let dst = self.sptr(d);
                let src = self.const_ptr(k);
                self.call(Helper::Clone, &[dst, src]);
                Entry::Boxed
            }
        };
        self.stack.push(e);
    }

    /// Push a copy of entry `i`.
    fn dup(&mut self, i: usize) {
        let d = self.stack.len();
        let e = self.stack[i];
        if e == Entry::Boxed {
            let (dst, src) = (self.sptr(d), self.sptr(i));
            self.clone_mem(dst, src);
        }
        self.stack.push(e);
    }

    /// Clone the `Value` at `src` into (uninitialized) `dst`: primitives are a two-word copy,
    /// an object (or a string, whose count sits where an object's does) takes its reference
    /// inline, and a BigInt or Symbol goes through [`Helper::Clone`].
    fn clone_mem(&mut self, dst: V, src: V) {
        let tag = self.fb.load(MemKind::I32U8, src, 0);
        let four = self.i32c(TAG_NUM as i64);
        let big = self.fb.icmp(IntCC::Ugt, tag, four);
        let copy_b = self.fb.create_block();
        let ref_b = self.fb.create_block();
        let inc_b = self.fb.create_block();
        let call_b = self.fb.create_block();
        let done = self.fb.create_block();
        self.fb.brif(big, ref_b, &[], copy_b, &[]);
        self.fb.seal_block(ref_b);
        self.fb.seal_block(copy_b);

        self.fb.switch_to_block(ref_b);
        let objt = self.i32c(TAG_OBJ as i64);
        let is_obj = self.fb.icmp(IntCC::Eq, tag, objt);
        let rc_so = layout::rc_strong_offset();
        let counted = match rc_so {
            Some(_) => {
                let strt = self.i32c(TAG_STR as i64);
                let is_str = self.fb.icmp(IntCC::Eq, tag, strt);
                self.fb.binary(BinaryOp::Bor, is_obj, is_str)
            }
            None => is_obj,
        };
        self.fb.brif(counted, inc_b, &[], call_b, &[]);
        self.fb.seal_block(inc_b);
        self.fb.seal_block(call_b);

        self.fb.switch_to_block(inc_b);
        let gc = self.fb.load(PTR_MEM, src, VALUE_PAYLOAD);
        let so = rc_so.unwrap_or(crate::value::GC_STRONG_OFFSET as i32);
        let strong = self.fb.load(PTR_MEM, gc, so);
        let one = self.ptrc(1);
        let s1 = self.fb.binary(BinaryOp::Iadd, strong, one);
        self.fb.store(PTR_MEM, gc, s1, so);
        self.fb.jump(copy_b, &[]);

        // Field by field (tag byte, a Boolean's byte, payload word): the tag was typically
        // just stored as a byte, and a wider reload of it can't be store-forwarded.
        self.fb.switch_to_block(copy_b);
        let b1 = self.fb.load(MemKind::I32U8, src, 1);
        let w1 = self.fb.load(MemKind::I64, src, VALUE_PAYLOAD);
        self.fb.store(MemKind::I32U8, dst, tag, 0);
        self.fb.store(MemKind::I32U8, dst, b1, 1);
        self.fb.store(MemKind::I64, dst, w1, VALUE_PAYLOAD);
        self.fb.jump(done, &[]);

        self.fb.switch_to_block(call_b);
        self.call(Helper::Clone, &[dst, src]);
        self.fb.jump(done, &[]);
        self.fb.seal_block(done);
        self.fb.switch_to_block(done);
    }

    fn ssa_entry(&mut self, s: usize) -> Entry {
        let var = self.vars[s].expect("SSA slot");
        let v = self.fb.use_var(var);
        if self.kinds[s] == Kind::Num {
            Entry::Num(v)
        } else {
            Entry::Bool(v)
        }
    }

    fn load_local(&mut self, pc: usize, s: usize) {
        let d = self.stack.len();
        if self.vars[s].is_some() {
            self.tdz_guard(s, pc);
            let e = self.ssa_entry(s);
            self.stack.push(e);
        } else {
            // Borrow the slot (the TDZ error comes from the interpreter).
            let _ = d;
            let p = self.slot_ptr(s);
            self.slot_tdz_exit(p, pc);
            self.stack.push(Entry::Ref(p, Src::Slot(s as u16)));
        }
    }

    // ---- names and captures ------------------------------------------------------------------

    /// The cached address of `src`'s binding value (resolving it when the cache is empty); 0
    /// when it cannot be resolved to an address.
    fn src_ptr(&mut self, src: Src, cache: Option<u32>) -> V {
        let Some(&var) = self.ptr_vars.get(&src) else {
            return self.resolve_ptr(src, cache);
        };
        let p0 = self.fb.use_var(var);
        let z = self.ptrc(0);
        let null = self.fb.icmp(IntCC::Eq, p0, z);
        let res = self.fb.create_block();
        let cont = self.fb.create_block();
        self.fb.brif(null, res, &[], cont, &[]);
        self.fb.seal_block(res);
        self.fb.switch_to_block(res);
        let p1 = self.resolve_ptr(src, cache);
        self.fb.def_var(var, p1);
        self.fb.jump(cont, &[]);
        self.fb.seal_block(cont);
        self.fb.switch_to_block(cont);
        self.fb.use_var(var)
    }

    fn resolve_ptr(&mut self, src: Src, cache: Option<u32>) -> V {
        match src {
            Src::Name(n) => {
                let nv = self.i32c(n as i64);
                let cv = self.i32c(cache.unwrap_or(u32::MAX) as i64);
                self.call(Helper::NamePtr, &[self.frame, nv, cv])
                    .expect("NamePtr returns an address")
            }
            Src::Cap(n) => {
                let nv = self.i32c(n as i64);
                self.call(Helper::CapPtr, &[self.frame, nv])
                    .expect("CapPtr returns an address")
            }
            Src::NameW(n) | Src::Glob(n) => {
                let h = if matches!(src, Src::Glob(_)) {
                    Helper::GlobPtr
                } else {
                    Helper::NamePtrW
                };
                let nv = self.i32c(n as i64);
                let cv = self.i32c(cache.unwrap_or(u32::MAX) as i64);
                self.call(h, &[self.frame, nv, cv]).expect("returns an address")
            }
            Src::Slot(s) => self.slot_ptr(s as usize),
            Src::This => self.fb.load(PTR_MEM, self.frame, FRAME_THIS),
            Src::Pin | Src::Borrow => {
                self.err.get_or_insert_with(|| "pinned or lent value resolved as a binding".into());
                self.ptrc(0)
            }
        }
    }

    /// `LoadName(n, c)` / `LoadNameForCall(n, c)` through helper `h` into the entries at the
    /// stack top (not pushed).
    fn load_name_helper(&mut self, pc: usize, n: u32, c: u32, h: Helper) {
        let d = self.stack.len();
        let (nv, cv, dst) = (self.i32c(n as i64), self.i32c(c as i64), self.sptr(d));
        if h == Helper::LoadNameForCall {
            self.soff(d + 1);
        }
        let st = self.call_status(h, &[self.frame, nv, cv, dst]);
        let snap = self.stack.clone();
        let one = self.i32c(STATUS_THROW as i64);
        let threw = self.fb.icmp(IntCC::Eq, st, one);
        self.exit_if(threw, pc, EXIT_THROW, &snap, d);
        // The full lookup may have run a getter: caches are stale then.
        let slow = self.i32c(helpers::STATUS_OK_SLOW as i64);
        let ran = self.fb.icmp(IntCC::Eq, st, slow);
        let inv = self.fb.create_block();
        let cont = self.fb.create_block();
        self.fb.brif(ran, inv, &[], cont, &[]);
        self.fb.seal_block(inv);
        self.fb.switch_to_block(inv);
        self.invalidate_js();
        self.fb.jump(cont, &[]);
        self.fb.seal_block(cont);
        self.fb.switch_to_block(cont);
    }

    /// `StoreNameCached(n, c)` in place: into a mutable binding holding a trivially droppable
    /// value ([`Src::NameW`]), or a Number / Boolean into a writable global data property
    /// holding one ([`Src::Glob`]); else [`Helper::StoreName`].
    fn store_name_inline(&mut self, pc: usize, n: u32, c: u32, src: Src) {
        let d = self.stack.len();
        let e = self.stack[d - 1];
        let glob = matches!(src, Src::Glob(_));
        let fast = !glob || matches!(e, Entry::Num(_) | Entry::Bool(_));
        let slow = self.fb.create_block();
        let join = self.fb.create_block();
        if fast {
            let p = self.src_ptr(src, Some(c));
            let z = self.ptrc(0);
            let nonnull = self.fb.icmp(IntCC::Ne, p, z);
            let ok1 = self.fb.create_block();
            self.fb.brif(nonnull, ok1, &[], slow, &[]);
            self.fb.seal_block(ok1);
            self.fb.switch_to_block(ok1);
            if glob {
                layout::entry_writable(&mut self.fb, p, 0, slow);
                let w = match e {
                    Entry::Num(x) => layout::num_word(&mut self.fb, x),
                    Entry::Bool(b) => layout::bool_word(&mut self.fb, b),
                    _ => unreachable!(),
                };
                layout::entry_store(&mut self.fb, p, 0, w);
            } else {
                let tag = self.fb.load(MemKind::I32U8, p, 0);
                let four = self.i32c(TAG_NUM as i64);
                let trivial = self.fb.icmp(IntCC::Ule, tag, four);
                let ok2 = self.fb.create_block();
                self.fb.brif(trivial, ok2, &[], slow, &[]);
                self.fb.seal_block(ok2);
                self.fb.switch_to_block(ok2);
                match e {
                    Entry::Num(x) => {
                        let t = self.i32c(TAG_NUM as i64);
                        self.fb.store(MemKind::I32U8, p, t, 0);
                        self.fb.store(MemKind::F64, p, x, VALUE_PAYLOAD);
                    }
                    Entry::Bool(b) => {
                        let t = self.i32c(TAG_BOOL as i64);
                        self.fb.store(MemKind::I32U8, p, t, 0);
                        self.fb.store(MemKind::I32U8, p, b, VALUE_BOOL);
                    }
                    _ => {
                        // Move the owned value in bitwise; its old home becomes trivially
                        // droppable.
                        self.store_entry(d - 1, e);
                        let o = self.soff(d - 1);
                        let a = self.fb.load(MemKind::I64, self.stackp, o);
                        let b = self.fb.load(MemKind::I64, self.stackp, o + 8);
                        self.fb.store(MemKind::I64, p, a, 0);
                        self.fb.store(MemKind::I64, p, b, 8);
                        self.set_stack_tag(d - 1, TAG_UNDEFINED);
                    }
                }
            }
            self.fb.jump(join, &[]);
        } else {
            self.fb.jump(slow, &[]);
        }
        self.fb.seal_block(slow);
        self.fb.switch_to_block(slow);
        self.store_entry(d - 1, e);
        let (nv, cv, sp) = (self.i32c(n as i64), self.i32c(c as i64), self.sptr(d - 1));
        let st = self.call_js(Helper::StoreName, &[self.frame, nv, cv, sp]);
        let below = self.stack[..d - 1].to_vec();
        self.throw_if(st, pc, &below, d - 1);
        self.fb.jump(join, &[]);
        self.fb.seal_block(join);
        self.fb.switch_to_block(join);
        self.invalidate_src(Src::Name(n));
        self.invalidate_src(src);
        self.stack.pop();
    }

    /// Exit before the op at `pc` when `p` is null (the interpreter does the full lookup).
    fn ptr_or_exit(&mut self, p: V, pc: usize) {
        let z = self.ptrc(0);
        let null = self.fb.icmp(IntCC::Eq, p, z);
        let st = self.stack.clone();
        self.exit_if(null, pc, EXIT_RESUME, &st, st.len());
    }

    /// `StoreCap(n)`: an in-place store when the binding holds a trivially droppable value,
    /// else [`Helper::StoreCap`].
    fn store_cap(&mut self, pc: usize, n: u32) -> Result<(), String> {
        let d = self.stack.len();
        let e = self.stack[d - 1];
        let p = self.src_ptr(Src::Cap(n), None);
        let slow = self.fb.create_block();
        let join = self.fb.create_block();
        let z = self.ptrc(0);
        let nonnull = self.fb.icmp(IntCC::Ne, p, z);
        let ok1 = self.fb.create_block();
        self.fb.brif(nonnull, ok1, &[], slow, &[]);
        self.fb.seal_block(ok1);
        self.fb.switch_to_block(ok1);
        let tag = self.fb.load(MemKind::I32U8, p, 0);
        let four = self.i32c(TAG_NUM as i64);
        let trivial = self.fb.icmp(IntCC::Ule, tag, four);
        let ok2 = self.fb.create_block();
        self.fb.brif(trivial, ok2, &[], slow, &[]);
        self.fb.seal_block(ok2);
        self.fb.switch_to_block(ok2);
        match e {
            Entry::Num(x) => {
                let t = self.i32c(TAG_NUM as i64);
                self.fb.store(MemKind::I32U8, p, t, 0);
                self.fb.store(MemKind::F64, p, x, VALUE_PAYLOAD);
            }
            Entry::Bool(b) => {
                let t = self.i32c(TAG_BOOL as i64);
                self.fb.store(MemKind::I32U8, p, t, 0);
                self.fb.store(MemKind::I32U8, p, b, VALUE_BOOL);
            }
            _ => {
                // Move the owned value in bitwise; its old home becomes trivially droppable.
                let o = self.soff(d - 1);
                let a = self.fb.load(MemKind::I64, self.stackp, o);
                let b = self.fb.load(MemKind::I64, self.stackp, o + 8);
                self.fb.store(MemKind::I64, p, a, 0);
                self.fb.store(MemKind::I64, p, b, 8);
                self.set_stack_tag(d - 1, TAG_UNDEFINED);
            }
        }
        self.fb.jump(join, &[]);
        self.fb.seal_block(slow);
        self.fb.switch_to_block(slow);
        self.store_entry(d - 1, e);
        let (nv, src, zero) = (self.i32c(n as i64), self.sptr(d - 1), self.i32c(0));
        let st = self.call_js(Helper::StoreCap, &[self.frame, nv, src, zero]);
        let below = self.stack[..d - 1].to_vec();
        self.throw_if(st, pc, &below, d - 1);
        self.fb.jump(join, &[]);
        self.fb.seal_block(join);
        self.fb.switch_to_block(join);
        self.invalidate_src(Src::Cap(n));
        self.stack.pop();
        Ok(())
    }

    /// `UpdateCap` / `UpdateNameCached` through `src`: in place on a Number, else the
    /// interpreter's op.
    fn update_ptr(&mut self, pc: usize, src: Src, cache: Option<u32>, k: UpdKind) {
        let d = self.stack.len();
        let pushes = !matches!(k, UpdKind::IncDiscard | UpdKind::DecDiscard);
        let p = self.src_ptr(src, cache);
        let slow = self.fb.create_block();
        let join = self.fb.create_block();
        let param = pushes.then(|| self.fb.append_block_param(join, Type::F64));
        let z = self.ptrc(0);
        let nonnull = self.fb.icmp(IntCC::Ne, p, z);
        let ok1 = self.fb.create_block();
        self.fb.brif(nonnull, ok1, &[], slow, &[]);
        self.fb.seal_block(ok1);
        self.fb.switch_to_block(ok1);
        let tag = self.fb.load(MemKind::I32U8, p, 0);
        let four = self.i32c(TAG_NUM as i64);
        let is = self.fb.icmp(IntCC::Eq, tag, four);
        let ok2 = self.fb.create_block();
        self.fb.brif(is, ok2, &[], slow, &[]);
        self.fb.seal_block(ok2);
        self.fb.switch_to_block(ok2);
        let old = self.fb.load(MemKind::F64, p, VALUE_PAYLOAD);
        let one = self.fb.f64const(1.0);
        let inc = matches!(k, UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard);
        let new = if inc {
            self.fb.binary(BinaryOp::Fadd, old, one)
        } else {
            self.fb.binary(BinaryOp::Fsub, old, one)
        };
        self.fb.store(MemKind::F64, p, new, VALUE_PAYLOAD);
        let r = match k {
            UpdKind::PostInc | UpdKind::PostDec => old,
            _ => new,
        };
        if pushes {
            self.fb.jump(join, &[r]);
        } else {
            self.fb.jump(join, &[]);
        }
        self.fb.seal_block(slow);
        self.fb.switch_to_block(slow);
        let pre = self.stack.clone();
        self.generic(pc);
        if pushes {
            self.slow_to_join(pc, d, true, join);
        } else {
            self.fb.jump(join, &[]);
        }
        self.fb.seal_block(join);
        self.fb.switch_to_block(join);
        self.invalidate_src(src);
        self.stack = pre;
        if let Some(r) = param {
            self.stack.push(Entry::Num(r));
        }
    }

    // ---- calls and properties ----------------------------------------------------------------

    /// `obj.<names[n]> = v` for `v` on top of the stack (popped), `obj` an address; `consume`:
    /// the stack index of a Boxed receiver dropped by the op.
    fn prop_write(
        &mut self,
        pc: usize,
        n: u32,
        c: u32,
        obj: V,
        consume: Option<usize>,
    ) -> Result<(), String> {
        let (pops, _) = self.chunk.jit_stack_effect(pc).unwrap_or((0, 0));
        self.prop_write_pops(pc, n, c, obj, consume, pops)
    }

    /// Whether the `Tdz(s)` at `pc` is dead: `StoreLocal(s)` follows in the same basic block
    /// (`const v = a.b` in a loop body) with only ops between that neither touch slot `s` nor
    /// yield, and no handler is live. Nothing can then read the uninitialized binding: a frame
    /// slot is not visible to other code, an exit resumes before the store (which still
    /// happens), and a throw leaves the frame. The store releases the old value instead.
    fn tdz_dead(&self, pc: usize, s: usize) -> bool {
        if !self.hs_cur.is_empty() {
            return false;
        }
        let other = |x: u16| x as usize != s;
        let mut q = pc + 1;
        while q <= self.backedge && !self.is_leader(q) {
            match self.ops[q] {
                Op::StoreLocal(x) if x as usize == s => return true,
                Op::LoadLocal(x) | Op::GetPropLocal(x, ..) if other(x) => {}
                Op::GetProp(..)
                | Op::GetPropThis(..)
                | Op::GetElem
                | Op::Const(_)
                | Op::Undef
                | Op::LoadThis
                | Op::LoadCap(_)
                | Op::Dup
                | Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Div
                | Op::Mod
                | Op::Neg
                | Op::Plus => {}
                _ => return false,
            }
            q += 1;
        }
        false
    }

    /// Whether the property read at `pc` (on a receiver it does not own) feeds straight into a
    /// plain property read of its result (`a.b.c`), so [`Tr::chain_read`] can lend it.
    fn chains(&self, pc: usize, n: u32, c: u32) -> bool {
        let q = pc + 1;
        !self.chaining
            && q <= self.backedge
            && !self.is_leader(q)
            && matches!(self.ops[q], Op::GetProp(..))
            && &*self.chunk.names[n as usize] != "length"
            && !self.plan.dget.contains_key(&pc)
            && !self.plan.dacc.contains_key(&pc)
            && layout::prop_ic(self.chunk, c).is_some()
    }

    /// A property read at `pc`, result at entry `at`, whose one consumer is the plain property
    /// read at `pc + 1` (see [`Tr::chains`]): when the inline cache finds an object, it is lent
    /// to the consumer — stored in the entry without taking a reference, as a [`Src::Borrow`]
    /// Ref the consumer reads in place (its slow path takes a reference first) — which saves
    /// the reference count's increment and the release after the read. Anything else reads
    /// the usual way (`owned`), then runs the consumer on that. Both paths emit the consumer;
    /// `body` skips it.
    fn chain_read(
        &mut self,
        pc: usize,
        obj: V,
        c: u32,
        at: usize,
        owned: impl FnOnce(&mut Self) -> Result<(), String>,
    ) -> Result<(), String> {
        let ic = layout::prop_ic(self.chunk, c).ok_or("chained read without a cache")?;
        let pre = self.stack.clone();
        let other = self.fb.create_block();
        let p = layout::prop_obj(&mut self.fb, obj, &ic, other);
        self.seal_current();
        let dst = self.sptr(at);
        let objt = self.i32c(TAG_OBJ as i64);
        self.fb.store(MemKind::I32U8, dst, objt, 0);
        self.fb.store(MemKind::I64, dst, p, VALUE_PAYLOAD);
        self.stack.truncate(at);
        self.stack.push(Entry::Ref(dst, Src::Borrow));
        self.chaining = true;
        let r = self.op(pc + 1);
        self.chaining = false;
        r?;
        // A result the consumer keeps unboxed leaves the lent object's bits in the entry: stack
        // memory above the depth must stay trivially droppable.
        if !self.fb.is_filled() && self.stack.get(at) != Some(&Entry::Boxed) {
            self.set_stack_tag(at, TAG_UNDEFINED);
        }
        let lent = (self.fb.current_block(), self.stack.clone());
        self.fb.seal_block(other);
        self.fb.switch_to_block(other);
        self.stack = pre;
        owned(self)?;
        if !self.fb.is_filled() {
            self.chaining = true;
            let r = self.op(pc + 1);
            self.chaining = false;
            r?;
        }
        let plain = (self.fb.current_block(), self.stack.clone());
        self.merge_paths(vec![lent, plain])?;
        self.skip = Some(pc + 1);
        Ok(())
    }

    /// Join straight-line paths (each its end block and stack state) into one: entries equal
    /// on every live path stay, Numbers and Booleans become block parameters, anything else is
    /// boxed into its stack slot. A path whose block already ended (it exited) is left out.
    fn merge_paths(&mut self, paths: Vec<(Option<Block>, Vec<Entry>)>) -> Result<(), String> {
        let mut live: Vec<(Block, Vec<Entry>)> = Vec::new();
        for (b, st) in paths {
            let Some(b) = b else { continue };
            self.fb.switch_to_block(b);
            if !self.fb.is_filled() {
                live.push((b, st));
            }
        }
        let Some((_, first)) = live.first() else {
            // Every path exited: a filled block stays current (`body` skips to the next leader).
            return Ok(());
        };
        let len = first.len();
        if live.iter().any(|(_, st)| st.len() != len) {
            return Err("chained read paths disagree on the stack depth".into());
        }
        #[derive(Clone, Copy, PartialEq)]
        enum How {
            Same,
            Num,
            Bool,
            Boxed,
        }
        let hows: Vec<How> = (0..len)
            .map(|k| {
                let e0 = live[0].1[k];
                if live.iter().all(|(_, st)| st[k] == e0) {
                    How::Same
                } else if live.iter().all(|(_, st)| matches!(st[k], Entry::Num(_))) {
                    How::Num
                } else if live.iter().all(|(_, st)| matches!(st[k], Entry::Bool(_))) {
                    How::Bool
                } else {
                    How::Boxed
                }
            })
            .collect();
        let join = self.fb.create_block();
        let mut merged = live[0].1.clone();
        for (k, h) in hows.iter().enumerate() {
            match h {
                How::Num => merged[k] = Entry::Num(self.fb.append_block_param(join, Type::F64)),
                How::Bool => merged[k] = Entry::Bool(self.fb.append_block_param(join, Type::I32)),
                How::Boxed => merged[k] = Entry::Boxed,
                How::Same => {}
            }
        }
        for (b, st) in &live {
            self.fb.switch_to_block(*b);
            self.stack = st.clone();
            let mut args = Vec::new();
            for (k, h) in hows.iter().enumerate() {
                match (h, st[k]) {
                    (How::Num, Entry::Num(x)) | (How::Bool, Entry::Bool(x)) => args.push(x),
                    (How::Boxed, e) => self.store_entry(k, e),
                    _ => {}
                }
            }
            self.fb.jump(join, &args);
        }
        self.fb.seal_block(join);
        self.fb.switch_to_block(join);
        self.stack = merged;
        Ok(())
    }

    /// The receiver of a plain property store at entry `i`: a borrowed (Ref) receiver is
    /// written through in place — the inline store runs no JS, so the value it borrows stays
    /// put, and the slow path takes an owned clone first ([`Tr::prop_write_plain`]) — which
    /// saves the clone and release of the receiver around every store.
    fn ref_receiver(&mut self, i: usize) -> V {
        match self.stack[i] {
            Entry::Ref(p, _) => p,
            _ => {
                self.force(i);
                self.sptr(i)
            }
        }
    }

    /// `obj.name += v` (`AppendProp`, stack `obj, lval, v`): with two Numbers it is exactly
    /// `Add` + `SetPropDrop` (a string append needs a string operand), so the sum goes through
    /// the property store's inline cache; anything else runs the fused op generically.
    fn append_prop(&mut self, pc: usize, n: u32, c: u32) -> Result<(), String> {
        let d = self.stack.len();
        let (lval, v) = (self.stack[d - 2], self.stack[d - 1]);
        let numeric = |e: Entry| matches!(e, Entry::Num(_) | Entry::Boxed | Entry::Ref(..));
        if !numeric(lval) || !numeric(v) {
            self.generic(pc);
            return Ok(());
        }
        let pre = self.stack.clone();
        let miss = self.fb.create_block();
        let done = self.fb.create_block();
        let a = self.num_or_slow(lval, d - 2, miss);
        let b = self.num_or_slow(v, d - 1, miss);
        let r = self.fb.binary(BinaryOp::Fadd, a, b);
        // Number operands own nothing: dropping them is a no-op.
        self.stack.truncate(d - 2);
        self.stack.push(Entry::Num(r));
        let obj = self.ref_receiver(d - 3);
        self.prop_write_pops(pc, n, c, obj, Some(d - 3), 2)?;
        self.fb.jump(done, &[]);
        self.fb.seal_block(miss);
        self.fb.switch_to_block(miss);
        self.stack = pre;
        self.generic(pc);
        self.fb.jump(done, &[]);
        self.fb.seal_block(done);
        self.fb.switch_to_block(done);
        Ok(())
    }

    /// [`Self::prop_write`] for a store whose value and object are the top `pops` entries.
    fn prop_write_pops(
        &mut self,
        pc: usize,
        n: u32,
        c: u32,
        obj: V,
        consume: Option<usize>,
        pops: usize,
    ) -> Result<(), String> {
        if let Some(a) = self.plan.dacc.get(&pc).cloned() {
            // A setter called directly (see `call::AccSite`).
            let d = self.stack.len();
            let base = d - pops;
            let pre = self.stack.clone();
            let miss = self.fb.create_block();
            let done = self.fb.create_block();
            layout::accessor_probe(&mut self.fb, obj, &a.shapes, a.slot, a.word as u64, true, miss);
            let arg = match pre[d - 1] {
                Entry::Boxed => Entry::Ref(self.sptr(d - 1), Src::Pin),
                e => e,
            };
            self.accessor_call(pc, obj, Some(arg));
            let r = self.stack.len() - 1;
            self.drop_at(r);
            self.stack.pop();
            if pre[d - 1] == Entry::Boxed {
                self.drop_at(d - 1);
            }
            if let Some(i) = consume {
                self.drop_at(i);
            }
            self.fb.jump(done, &[]);
            self.fb.seal_block(miss);
            self.fb.switch_to_block(miss);
            self.stack = pre;
            self.prop_write_plain(pc, n, c, obj, consume, pops)?;
            self.fb.jump(done, &[]);
            self.fb.seal_block(done);
            self.fb.switch_to_block(done);
            self.stack.truncate(base);
            return Ok(());
        }
        if let Some(g) = self.plan.dset.get(&pc).cloned() {
            let done = self.fb.create_block();
            if self.inline_setter(obj, consume, &g, done) {
                let base = self.stack.len() - pops;
                self.prop_write_plain(pc, n, c, obj, consume, pops)?;
                self.fb.jump(done, &[]);
                self.fb.seal_block(done);
                self.fb.switch_to_block(done);
                self.stack.truncate(base);
                return Ok(());
            }
        }
        self.prop_write_plain(pc, n, c, obj, consume, pops)
    }

    /// [`Self::prop_write_pops`] without an inlined setter.
    fn prop_write_plain(
        &mut self,
        pc: usize,
        n: u32,
        c: u32,
        obj: V,
        consume: Option<usize>,
        pops: usize,
    ) -> Result<(), String> {
        let d = self.stack.len();
        let base = d - pops;
        let v = self.stack[d - 1];
        let pre = self.stack.clone();
        let join = self.fb.create_block();
        let ic = if matches!(v, Entry::Num(_) | Entry::Boxed) {
            layout::prop_ic(self.chunk, c)
        } else {
            None
        };
        if let Some(ic) = &ic {
            let miss = self.fb.create_block();
            let x = self.num_or_slow(v, d - 1, miss);
            layout::prop_set_num(&mut self.fb, obj, ic, x, miss);
            self.seal_current();
            match consume {
                // A borrowed receiver owns nothing.
                Some(i) if !matches!(pre[i], Entry::Ref(..)) => self.drop_at(i),
                _ => {}
            }
            self.fb.jump(join, &[]);
            self.fb.seal_block(miss);
            self.fb.switch_to_block(miss);
        }
        self.stack = pre;
        self.box_from(d - 1);
        let objp = match consume {
            Some(i) => {
                self.force(i);
                self.sptr(i)
            }
            None => obj,
        };
        let (nv, cv) = (self.i32c(n as i64), self.i32c(c as i64));
        let (vp, kv) = (self.sptr(d - 1), self.i32c(consume.is_some() as i64));
        let st = self.call_js(Helper::SetProp, &[self.frame, nv, cv, objp, vp, kv]);
        let below = self.stack[..base].to_vec();
        self.throw_if(st, pc, &below, base);
        self.fb.jump(join, &[]);
        self.fb.seal_block(join);
        self.fb.switch_to_block(join);
        self.stack.truncate(base);
        // A property store (even an inline one) may be an array's `length`: drop the views.
        let views: Vec<(Variable, Type)> =
            self.arr_vars.values().map(|a| (a.kind, Type::I32)).collect();
        self.reset_vars(&views);
        Ok(())
    }

    /// After a helper that did the op at `pc` when its flag `r` is nonzero: the generic op
    /// otherwise. The result is one Boxed entry in place of the op's operands.
    fn hit_or_generic(&mut self, pc: usize, r: V) {
        let (pops, _) = self.chunk.jit_stack_effect(pc).unwrap_or((0, 0));
        let d = self.stack.len();
        let z = self.i32c(0);
        let hit = self.fb.icmp(IntCC::Ne, r, z);
        let gen = self.fb.create_block();
        let join = self.fb.create_block();
        self.fb.brif(hit, join, &[], gen, &[]);
        self.fb.seal_block(gen);
        self.fb.switch_to_block(gen);
        self.generic(pc);
        self.fb.jump(join, &[]);
        self.fb.seal_block(join);
        self.fb.switch_to_block(join);
        self.stack.truncate(d - pops);
        self.stack.push(Entry::Boxed);
    }

    /// `DestructureArr(n)` of the Boxed value at `d - 1`: when [`Helper::DestructProbe`] holds
    /// and the first `n` elements are dense data elements (`n <= length`), they are read
    /// natively into `d - 1 ..`; else `DestructDense`, then the generic op.
    fn destructure_inline(&mut self, pc: usize, d: usize, n: usize) {
        // The array moves to a scratch entry above the outputs while they are written.
        self.max_stack = self.max_stack.max(d + n);
        let p = self.sptr(d - 1);
        let slow = self.fb.create_block();
        let join = self.fb.create_block();
        let fast = self.fb.create_block();
        let r = self
            .call(Helper::DestructProbe, &[self.frame, p])
            .expect("DestructProbe returns a flag");
        let z = self.i32c(0);
        let ok = self.fb.icmp(IntCC::Ne, r, z);
        self.fb.brif(ok, fast, &[], slow, &[]);
        self.fb.seal_block(fast);
        self.fb.switch_to_block(fast);
        let base = layout::packed_prefix(&mut self.fb, p, n, slow);
        let mut elems = Vec::with_capacity(n);
        let mut all_num = None;
        for j in 0..n {
            let bits = layout::packed_word(&mut self.fb, base, j, slow);
            let num = layout::is_num_bits(&mut self.fb, bits);
            all_num = Some(match all_num {
                Some(a) => self.fb.binary(BinaryOp::Band, a, num),
                None => num,
            });
            elems.push(bits);
        }
        let all_num = all_num.expect("n >= 1");
        // Every check passed: nothing below can miss.
        let (from, to) = (self.soff(d - 1), self.soff(d - 1 + n));
        let w0 = self.fb.load(MemKind::I64, self.stackp, from);
        let w1 = self.fb.load(MemKind::I64, self.stackp, from + 8);
        self.fb.store(MemKind::I64, self.stackp, w0, to);
        self.fb.store(MemKind::I64, self.stackp, w1, to + 8);
        // Straight-line writes, only the words kept live (a branch per element, or a decoded
        // tag and payload per element, costs more than the writes): all Numbers stored
        // directly, else every element (any non-hole word) through `UnpackClone`.
        let refb = self.fb.create_block();
        let plain = self.fb.create_block();
        let done = self.fb.create_block();
        self.fb.brif(all_num, plain, &[], refb, &[]);
        self.fb.seal_block(refb);
        self.fb.seal_block(plain);
        self.fb.switch_to_block(refb);
        for (j, &bits) in elems.iter().enumerate() {
            let dst = self.sptr(d - 1 + j);
            let zero = self.i32c(0);
            self.call(Helper::UnpackClone, &[dst, bits, zero]);
        }
        self.fb.jump(done, &[]);
        self.fb.switch_to_block(plain);
        let four = self.i32c(TAG_NUM as i64);
        for (j, &bits) in elems.iter().enumerate() {
            let dst = self.sptr(d - 1 + j);
            self.fb.store(MemKind::I32U8, dst, four, 0);
            self.fb.store(MemKind::I64, dst, bits, VALUE_PAYLOAD);
        }
        self.fb.jump(done, &[]);
        self.fb.seal_block(done);
        self.fb.switch_to_block(done);
        self.drop_at(d - 1 + n);
        self.fb.jump(join, &[]);
        self.fb.seal_block(slow);
        self.fb.switch_to_block(slow);
        let nv = self.i32c(n as i64);
        let r = self
            .call(Helper::DestructDense, &[self.frame, nv, p])
            .expect("DestructDense returns a flag");
        let z = self.i32c(0);
        let hit = self.fb.icmp(IntCC::Ne, r, z);
        let gen = self.fb.create_block();
        self.fb.brif(hit, join, &[], gen, &[]);
        self.fb.seal_block(gen);
        self.fb.switch_to_block(gen);
        self.generic(pc);
        self.fb.jump(join, &[]);
        self.fb.seal_block(join);
        self.fb.switch_to_block(join);
    }

    // ---- Math intrinsics ---------------------------------------------------------------------

    /// `LoadName(Math)` of an inline `Math.f(..)` site: re-check the guard if JS may have run
    /// since the last check (exit before the site when it fails: the interpreter makes the
    /// call), then push the placeholder receiver.
    fn math_begin(&mut self, pc: usize) -> Result<(), String> {
        if let Some(site) = self.plan.math.get(&pc).copied().filter(|s| s.slot.is_some()) {
            return self.slot_site_begin(pc, site);
        }
        if self.plan.nested.contains_key(&pc) {
            // Checked with its outer site, and nothing between can run JS.
            self.inl_caps_check(pc);
            let d = self.stack.len();
            self.set_stack_tag(d, TAG_UNDEFINED);
            self.stack.push(Entry::Boxed);
            return Ok(());
        }
        let var = *self
            .math_vars
            .get(&pc)
            .ok_or_else(|| format!("no guard for the Math site at {pc}"))?;
        let ok = self.fb.use_var(var);
        let zero = self.i32c(0);
        let unknown = self.fb.icmp(IntCC::Eq, ok, zero);
        let check = self.fb.create_block();
        let cont = self.fb.create_block();
        self.fb.brif(unknown, check, &[], cont, &[]);
        self.fb.seal_block(check);
        self.fb.switch_to_block(check);
        let mut g = self.site_guard(pc);
        let mut inner: Vec<usize> =
            self.plan.nested.iter().filter(|(_, &o)| o == pc).map(|(&q, _)| q).collect();
        inner.sort_unstable();
        for q in inner {
            let gi = self.site_guard(q);
            g = self.fb.binary(BinaryOp::Band, g, gi);
        }
        let zero = self.i32c(0);
        let bad = self.fb.icmp(IntCC::Eq, g, zero);
        let st = self.stack.clone();
        self.exit_if(bad, pc, EXIT_RESUME, &st, st.len());
        let one = self.i32c(1);
        self.fb.def_var(var, one);
        self.fb.jump(cont, &[]);
        self.fb.seal_block(cont);
        self.fb.switch_to_block(cont);
        self.inl_caps_check(pc);
        let d = self.stack.len();
        self.set_stack_tag(d, TAG_UNDEFINED);
        self.stack.push(Entry::Boxed);
        if self.plan.math.get(&pc).is_some_and(|s| s.lfc) {
            // `LoadNameForCall`: receiver and callee placeholders at once.
            self.set_stack_tag(d + 1, TAG_UNDEFINED);
            self.stack.push(Entry::Boxed);
        }
        Ok(())
    }

    /// `LoadLocal(s)` of an inlined local-callee site: check the local still holds the function
    /// (by identity, else by body through [`Helper::SlotGuard`]) — exit before the site when it
    /// does not (the interpreter makes the call) — then push the placeholder callee.
    fn slot_site_begin(&mut self, pc: usize, site: Site) -> Result<(), String> {
        let s = site.slot.expect("local-callee site") as usize;
        let Intr::Inline(k) = site.f else {
            return Err(format!("local-callee site at {pc} is not inlined"));
        };
        let (expect, chunk_ptr, caps) = {
            let inl = &self.plan.inl[k];
            (
                inl.expect,
                std::rc::Rc::as_ptr(&inl.chunk) as usize,
                !inl.shape.caps.is_empty(),
            )
        };
        let off = s as i32 * VALUE_SIZE;
        let tag = self.fb.load(MemKind::I32U8, self.slots, off);
        let obj = self.i32c(TAG_OBJ as i64);
        let is_obj = self.fb.icmp(IntCC::Eq, tag, obj);
        let payload = self.fb.load(MemKind::I64, self.slots, off + VALUE_PAYLOAD);
        let want = self.fb.iconst(Type::I64, expect as i64);
        let same = self.fb.icmp(IntCC::Eq, payload, want);
        let hit = self.fb.binary(BinaryOp::Band, is_obj, same);
        let slow = self.fb.create_block();
        let cont = self.fb.create_block();
        self.fb.brif(hit, cont, &[], slow, &[]);
        self.fb.seal_block(slow);
        self.fb.switch_to_block(slow);
        let pcv = self.i32c(pc as i64);
        if caps {
            // The captured bindings are the planned closure's own: another closure of the same
            // code (e.g. one per loop iteration) closes over other bindings. Retire the site.
            self.call(Helper::SiteFailed, &[self.frame, pcv]);
            self.exit_now(pc);
            self.fb.seal_block(cont);
            self.fb.switch_to_block(cont);
            self.inl_caps_check(pc);
            let d = self.stack.len();
            self.set_stack_tag(d, TAG_UNDEFINED);
            self.stack.push(Entry::Boxed);
            return Ok(());
        }
        let sv = self.i32c(s as i64);
        let cp = self.ptrc(chunk_ptr as i64);
        let ok = self.call_status(Helper::SlotGuard, &[self.frame, pcv, sv, cp]);
        let zero = self.i32c(0);
        let bad = self.fb.icmp(IntCC::Eq, ok, zero);
        let st = self.stack.clone();
        self.exit_if(bad, pc, EXIT_RESUME, &st, st.len());
        self.fb.jump(cont, &[]);
        self.fb.seal_block(cont);
        self.fb.switch_to_block(cont);
        self.inl_caps_check(pc);
        let d = self.stack.len();
        self.set_stack_tag(d, TAG_UNDEFINED);
        self.stack.push(Entry::Boxed);
        Ok(())
    }

    /// The captured variables of the inlined callee at site `pc` still read in place with the
    /// planned kinds (see [`inline`]'s module docs): its closure scope is structurally
    /// unchanged (generation), and each binding is initialized and holds a value of its planned
    /// kind. Checked at every execution (an in-place store may change a kind without running
    /// JS); a miss retires the site and exits before it.
    fn inl_caps_check(&mut self, pc: usize) {
        let Some(Intr::Inline(k)) = self.plan.math.get(&pc).map(|s| s.f) else {
            return;
        };
        let sh = self.plan.inl[k].shape.clone();
        if sh.caps.is_empty() {
            return;
        }
        let so = call::scope_offs().expect("planned with scope offsets");
        let fail = self.fb.create_block();
        let scope = self.ptrc(sh.scope as i64);
        let g = self.fb.load(MemKind::I32, scope, so.gen);
        let want = self.i32c(sh.gen as i64);
        let ok = self.fb.icmp(IntCC::Eq, g, want);
        self.guard_to(ok, fail);
        for c in &sh.caps {
            let bd = self.ptrc(c.binding as i64);
            let init = self.fb.load(MemKind::I32U8, bd, so.b_init);
            let tag = self.fb.load(MemKind::I32U8, bd, so.b_value);
            let t = self.i32c(if c.num { TAG_NUM } else { TAG_BOOL } as i64);
            let same = self.fb.icmp(IntCC::Eq, tag, t);
            let z = self.i32c(0);
            let live = self.fb.icmp(IntCC::Ne, init, z);
            let ok = self.fb.binary(BinaryOp::Band, same, live);
            self.guard_to(ok, fail);
        }
        let cont = self.fb.create_block();
        self.fb.jump(cont, &[]);
        self.fb.seal_block(fail);
        self.fb.switch_to_block(fail);
        let pcv = self.i32c(pc as i64);
        self.call(Helper::SiteFailed, &[self.frame, pcv]);
        self.exit_now(pc);
        self.fb.seal_block(cont);
        self.fb.switch_to_block(cont);
    }

    /// Whether the op at `pc` is the `GetMethod` of an inline `LoadName GetMethod` site.
    fn site_get_method(&self, pc: usize) -> bool {
        pc > 0
            && self
                .plan
                .math
                .get(&(pc - 1))
                .is_some_and(|s| !s.lfc && s.slot.is_none())
    }

    /// The guard of the inline call site starting at `pc` (I32, nonzero = still valid).
    fn site_guard(&mut self, pc: usize) -> V {
        let pcv = self.i32c(pc as i64);
        match self.plan.math.get(&pc).map(|s| s.f) {
            Some(Intr::Fast(k)) => {
                let e = self.ptrc(self.plan.fast[k].expect as i64);
                self.call_status(Helper::FastGuard, &[self.frame, pcv, e])
            }
            Some(Intr::Inline(k)) => {
                let e = self.ptrc(self.plan.inl[k].expect as i64);
                self.call_status(Helper::FastGuard, &[self.frame, pcv, e])
            }
            _ => self.call_status(Helper::MathGuard, &[self.frame, pcv]),
        }
    }

    /// The `CallWithThis` of a direct `#[op(fast)]` call site: the unboxed arguments (the
    /// planner matched their kinds to the signature) go straight to the entry; receiver and
    /// callee are placeholders. The op cannot throw or run JS.
    fn fast_call(&mut self, pc: usize, k: usize) -> Result<(), String> {
        use FastArg as FastKind;
        let fs = self.plan.fast[k].clone();
        let d = self.stack.len();
        let argc = fs.args.len();
        let mut params = Vec::with_capacity(argc);
        let mut args = Vec::with_capacity(argc);
        for (e, kind) in self.stack[d - argc..].to_vec().into_iter().zip(fs.args.iter().copied()) {
            let v = match (e, kind) {
                (Entry::Num(x), FastKind::F64) => {
                    params.push(Type::F64);
                    x
                }
                (Entry::Num(x), FastKind::I32 | FastKind::U32) => {
                    params.push(Type::I32);
                    self.to_int32(x)
                }
                (Entry::Bool(b), FastKind::Bool) => {
                    params.push(Type::I32);
                    b
                }
                (other, kind) => {
                    return Err(format!("fast call argument {other:?} at {pc} is not {kind:?}"))
                }
            };
            args.push(v);
        }
        let ret: Vec<Type> = match fs.ret {
            FastKind::F64 => vec![Type::F64],
            FastKind::Void => vec![],
            _ => vec![Type::I32],
        };
        let f = self.fb.func.import_function(
            Signature::new(params, ret),
            EXT_BASE + fs.ext as u32,
        );
        let r = self.fb.call_fn(f, &args).first().copied();
        self.stack.truncate(d - argc - 2);
        let at = self.stack.len();
        let e = match (fs.ret, r) {
            (FastKind::F64, Some(x)) => Entry::Num(x),
            (FastKind::I32, Some(x)) => Entry::Num(self.fb.convert(ConvOp::FromSint, Type::F64, x)),
            (FastKind::U32, Some(x)) => {
                let w = self.fb.convert(ConvOp::Uext, Type::I64, x);
                Entry::Num(self.fb.convert(ConvOp::FromSint, Type::F64, w))
            }
            (FastKind::Bool, Some(x)) => {
                // Exactly 0 or 1 by contract; normalize anyway (a stray byte must not leak into
                // a `Bool` payload).
                let z = self.i32c(0);
                Entry::Bool(self.fb.icmp(IntCC::Ne, x, z))
            }
            _ => {
                self.set_stack_tag(at, TAG_UNDEFINED);
                Entry::Boxed
            }
        };
        self.stack.push(e);
        Ok(())
    }

    // ---- iteration ---------------------------------------------------------------------------

    /// `IterStepL(is, ns)`: [`Helper::IterStep`] (a pristine array iterator steps without
    /// running JS; anything else runs the full protocol), pushing the value and the has-value
    /// flag as an unboxed Bool for the loop's `JumpIfFalse`.
    fn iter_step(&mut self, pc: usize, is: usize, ns: usize) {
        if self.kinds[is] != Kind::Boxed || self.kinds[ns] != Kind::Boxed {
            self.exit_now(pc);
            return;
        }
        let d = self.stack.len();
        self.soff(d + 1);
        // An encoded Array state (`iter_fast`: the index in the iterator slot, the array in the
        // `next` slot) below the length, over a dense data element: stepped inline. (A
        // Map/Set state's target has no `length` here, and a done state's index is +inf.)
        let join = self.fb.create_block();
        let has_p = self.fb.append_block_param(join, Type::I32);
        let slow = self.fb.create_block();
        let ip = self.slot_ptr(is);
        let np = self.slot_ptr(ns);
        let t = self.fb.load(MemKind::I32U8, ip, 0);
        let four = self.i32c(TAG_NUM as i64);
        let is_num = self.fb.icmp(IntCC::Eq, t, four);
        self.guard_to(is_num, slow);
        let k = self.fb.load(MemKind::F64, ip, VALUE_PAYLOAD);
        let len = layout::length(&mut self.fb, np, slow);
        let inb = self.fb.fcmp(FloatCC::Lt, k, len);
        self.guard_to(inb, slow);
        let bits = layout::elem_get_word(&mut self.fb, np, k, slow);
        let (tag, payload, is_ref) = layout::word_value(&mut self.fb, bits, slow);
        let dst = self.sptr(d);
        let refb = self.fb.create_block();
        let plain = self.fb.create_block();
        let stepped = self.fb.create_block();
        self.fb.brif(is_ref, refb, &[], plain, &[]);
        self.fb.seal_block(refb);
        self.fb.seal_block(plain);
        self.fb.switch_to_block(refb);
        let zero = self.i32c(0);
        self.call(Helper::UnpackClone, &[dst, bits, zero]);
        self.fb.jump(stepped, &[]);
        self.fb.switch_to_block(plain);
        self.fb.store(MemKind::I32U8, dst, tag, 0);
        let byte = self.fb.convert(ConvOp::Wrap, Type::I32, payload);
        self.fb.store(MemKind::I32U8, dst, byte, VALUE_BOOL);
        self.fb.store(MemKind::I64, dst, payload, VALUE_PAYLOAD);
        self.fb.jump(stepped, &[]);
        self.fb.seal_block(stepped);
        self.fb.switch_to_block(stepped);
        let one = self.fb.f64const(1.0);
        let k1 = self.fb.binary(BinaryOp::Fadd, k, one);
        self.fb.store(MemKind::F64, ip, k1, VALUE_PAYLOAD);
        let yes = self.i32c(1);
        self.fb.jump(join, &[yes]);
        self.fb.seal_block(slow);
        self.fb.switch_to_block(slow);
        let (iv, nv, dst) = (self.i32c(is as i64), self.i32c(ns as i64), self.sptr(d));
        let r = self.call_status(Helper::IterStep, &[self.frame, iv, nv, dst]);
        let t = self.i32c(helpers::ITER_THREW as i64);
        let threw = self.fb.icmp(IntCC::Eq, r, t);
        let snap = self.stack.clone();
        self.exit_if(threw, pc, EXIT_THROW, &snap, d);
        let two = self.i32c(2);
        let slow = self.fb.binary(BinaryOp::Band, r, two);
        let inv = self.fb.create_block();
        let cont = self.fb.create_block();
        self.fb.brif(slow, inv, &[], cont, &[]);
        self.fb.seal_block(inv);
        self.fb.switch_to_block(inv);
        self.invalidate_js();
        self.fb.jump(cont, &[]);
        self.fb.seal_block(cont);
        self.fb.switch_to_block(cont);
        let one = self.i32c(1);
        let has = self.fb.binary(BinaryOp::Band, r, one);
        self.fb.jump(join, &[has]);
        self.fb.seal_block(join);
        self.fb.switch_to_block(join);
        self.stack.push(Entry::Boxed);
        self.stack.push(Entry::Bool(has_p));
    }

    /// The `CallWithThis` of an inline `Math.f(..)` site: the Number arguments (the planner
    /// admitted only ops producing unboxed Numbers) replace the placeholders with the result.
    fn math_call(&mut self, pc: usize, f: MathFn) -> Result<(), String> {
        let d = self.stack.len();
        let argc = f.arity();
        let mut args = Vec::with_capacity(argc);
        for e in &self.stack[d - argc..] {
            match *e {
                Entry::Num(x) => args.push(x),
                other => return Err(format!("Math argument {other:?} at {pc} is not a Number")),
            }
        }
        let x = args[0];
        let r = match f {
            MathFn::Sqrt => self.fb.unary(UnaryOp::Sqrt, x),
            MathFn::Abs => self.fb.unary(UnaryOp::Fabs, x),
            MathFn::Floor => self.fb.unary(UnaryOp::Floor, x),
            MathFn::Ceil => self.fb.unary(UnaryOp::Ceil, x),
            MathFn::Trunc => self.fb.unary(UnaryOp::Trunc, x),
            MathFn::Round => {
                // floor(x), +1 when the fraction is >= 0.5; -0 for x in [-0.5, 0). NaN, ±Inf and
                // ±0 come through unchanged (x - floor(x) is NaN or 0 for them).
                let fl = self.fb.unary(UnaryOp::Floor, x);
                let frac = self.fb.binary(BinaryOp::Fsub, x, fl);
                let half = self.fb.f64const(0.5);
                let up = self.fb.fcmp(FloatCC::Ge, frac, half);
                let one = self.fb.f64const(1.0);
                let fl1 = self.fb.binary(BinaryOp::Fadd, fl, one);
                let r = self.fb.select(up, fl1, fl);
                let zero = self.fb.f64const(0.0);
                let rz = self.fb.fcmp(FloatCC::Eq, r, zero);
                let neg = self.fb.fcmp(FloatCC::Lt, x, zero);
                let both = self.fb.binary(BinaryOp::Band, rz, neg);
                let nz = self.fb.f64const(-0.0);
                self.fb.select(both, nz, r)
            }
            MathFn::Max => self.fb.binary(BinaryOp::Fmax, x, args[1]),
            MathFn::Min => self.fb.binary(BinaryOp::Fmin, x, args[1]),
            MathFn::Imul => {
                let a = self.to_int32(x);
                let b = self.to_int32(args[1]);
                let m = self.fb.binary(BinaryOp::Imul, a, b);
                self.fb.convert(ConvOp::FromSint, Type::F64, m)
            }
        };
        // Receiver and callee are placeholders (trivially droppable, never materialized).
        self.stack.truncate(d - argc - 2);
        self.stack.push(Entry::Num(r));
        Ok(())
    }

    // ---- String intrinsics -------------------------------------------------------------------

    /// The `GetMethod` of an inline `String.prototype` site: the receiver (materialized by
    /// `prepare`) must be a String and the method the intrinsic, checked by
    /// [`Helper::StrGuard`] when JS may have run since the last check or when the receiver is
    /// not a String (the guard then fails: the site is remembered and the code retired). Exits
    /// before the site on failure (the interpreter makes the call); then pushes the placeholder
    /// callee.
    fn str_begin(&mut self, pc: usize) -> Result<(), String> {
        let d = self.stack.len();
        let var = *self
            .math_vars
            .get(&pc)
            .ok_or_else(|| format!("no guard for the String site at {pc}"))?;
        let st = self.stack.clone();
        if st.last() != Some(&Entry::Boxed) {
            // A Number or Boolean receiver: never the String method.
            let pcv = self.i32c(pc as i64);
            self.call(Helper::SiteFailed, &[self.frame, pcv]);
            let yes = self.i32c(1);
            self.exit_if(yes, pc, EXIT_RESUME, &st, d);
        } else {
            let recv = self.sptr(d - 1);
            let tag = self.fb.load(MemKind::I32U8, recv, 0);
            let str_tag = self.i32c(TAG_STR as i64);
            let is_str = self.fb.icmp(IntCC::Eq, tag, str_tag);
            let known = self.fb.use_var(var);
            let zero = self.i32c(0);
            let known = self.fb.icmp(IntCC::Ne, known, zero);
            let fast = self.fb.binary(BinaryOp::Band, is_str, known);
            let check = self.fb.create_block();
            let cont = self.fb.create_block();
            self.fb.brif(fast, cont, &[], check, &[]);
            self.fb.seal_block(check);
            self.fb.switch_to_block(check);
            let pcv = self.i32c(pc as i64);
            let g = self.call_status(Helper::StrGuard, &[self.frame, pcv, recv]);
            let zero = self.i32c(0);
            let bad = self.fb.icmp(IntCC::Eq, g, zero);
            self.exit_if(bad, pc, EXIT_RESUME, &st, d);
            let one = self.i32c(1);
            self.fb.def_var(var, one);
            self.fb.jump(cont, &[]);
            self.fb.seal_block(cont);
            self.fb.switch_to_block(cont);
        }
        self.set_stack_tag(d, TAG_UNDEFINED);
        self.stack.push(Entry::Boxed);
        Ok(())
    }

    /// The `CallWithThis(1)` of an inline `String.prototype.charCodeAt` site: the unit read
    /// inline from an all-ASCII string, else by [`Helper::StrCodeAt`]; the receiver is dropped,
    /// the callee is a placeholder.
    fn str_call(&mut self, pc: usize) -> Result<(), String> {
        let d = self.stack.len();
        let x = match self.stack.get(d.wrapping_sub(1)) {
            Some(&Entry::Num(x)) if d >= 3 => x,
            other => return Err(format!("String intrinsic argument {other:?} at {pc}")),
        };
        let recv = self.sptr(d - 3);
        let slow = self.fb.create_block();
        let join = self.fb.create_block();
        let r = self.fb.append_block_param(join, Type::F64);
        let hdr = self.fb.load(PTR_MEM, recv, VALUE_PAYLOAD);
        let u = layout::str_ascii_unit(&mut self.fb, hdr, x, slow);
        let f = self.fb.convert(ConvOp::FromSint, Type::F64, u);
        self.fb.jump(join, &[f]);
        self.fb.seal_block(slow);
        self.fb.switch_to_block(slow);
        let v = self
            .call(Helper::StrCodeAt, &[self.frame, recv, x])
            .expect("StrCodeAt returns a Number");
        self.fb.jump(join, &[v]);
        self.fb.seal_block(join);
        self.fb.switch_to_block(join);
        self.stack.truncate(d - 2);
        self.drop_at(d - 3);
        self.stack.pop();
        self.stack.push(Entry::Num(r));
        Ok(())
    }

    /// The `GetMethod` of a fused `String.prototype` method site: a receiver that is not a
    /// String exits before the site (the interpreter makes the call; the site is remembered, so
    /// the next compile leaves it to the generic path). Pushes the `undefined` placeholder
    /// callee, which the call's helper replaces by reading the method itself.
    fn strn_begin(&mut self, pc: usize) -> Result<(), String> {
        self.tagged_begin(pc, TAG_STR)
    }

    /// The `GetMethod` of a fused Map/Set method site (see [`Tr::strn_begin`]): the receiver
    /// must be an object.
    fn colm_begin(&mut self, pc: usize) -> Result<(), String> {
        self.tagged_begin(pc, TAG_OBJ)
    }

    /// A fused method site's `GetMethod`: a receiver without tag `want` exits before the site
    /// (remembered as failed); then the `undefined` placeholder callee is pushed.
    fn tagged_begin(&mut self, pc: usize, want: u8) -> Result<(), String> {
        let d = self.stack.len();
        let st = self.stack.clone();
        let pcv = self.i32c(pc as i64);
        if st.last() != Some(&Entry::Boxed) {
            self.call(Helper::SiteFailed, &[self.frame, pcv]);
            let yes = self.i32c(1);
            self.exit_if(yes, pc, EXIT_RESUME, &st, d);
        } else {
            let recv = self.sptr(d - 1);
            let tag = self.fb.load(MemKind::I32U8, recv, 0);
            let str_tag = self.i32c(want as i64);
            let is_str = self.fb.icmp(IntCC::Eq, tag, str_tag);
            let bad = self.fb.create_block();
            let cont = self.fb.create_block();
            self.fb.brif(is_str, cont, &[], bad, &[]);
            self.fb.seal_block(bad);
            self.fb.switch_to_block(bad);
            self.call(Helper::SiteFailed, &[self.frame, pcv]);
            let yes = self.i32c(1);
            self.exit_if(yes, pc, EXIT_RESUME, &st, d);
            self.fb.jump(cont, &[]);
            self.fb.seal_block(cont);
            self.fb.switch_to_block(cont);
        }
        self.set_stack_tag(d, TAG_UNDEFINED);
        self.stack.push(Entry::Boxed);
        Ok(())
    }

    /// The `CallWithThis(argc)` of a fused `String.prototype` method site: the arguments
    /// boxed, then [`Helper::StrMethod`] reads the method and calls it.
    fn strn_call(&mut self, pc: usize, get: usize, cell: usize, h: Helper) -> Result<(), String> {
        let d = self.stack.len();
        let Op::CallWithThis(argc) = self.ops[pc] else {
            return Err(format!("no call for the String method site at {pc}"));
        };
        let base = d - argc as usize - 2;
        self.box_from(base);
        let (gv, bv, av) = (
            self.i32c(get as i64),
            self.i32c(base as i64),
            self.i32c(argc as i64),
        );
        let cv = self.ptrc(cell as i64);
        self.set_call_site(pc);
        let st = self.call_js(h, &[self.frame, gv, bv, av, cv]);
        let below = self.stack[..base].to_vec();
        self.throw_if(st, pc, &below, base);
        self.stack.truncate(base);
        self.stack.push(Entry::Boxed);
        Ok(())
    }

    // ---- Array.prototype.push ----------------------------------------------------------------

    /// The `GetMethod(push)` of an inline push site: [`Helper::ArrPushGuard`] proves the
    /// receiver a plain Array reading the intrinsic push, else the code exits before the site
    /// (the interpreter makes the call; the site is remembered). Pushes the placeholder callee.
    fn arr_begin(&mut self, pc: usize) -> Result<(), String> {
        let d = self.stack.len();
        let st = self.stack.clone();
        if st.last() != Some(&Entry::Boxed) {
            let pcv = self.i32c(pc as i64);
            self.call(Helper::SiteFailed, &[self.frame, pcv]);
            let yes = self.i32c(1);
            self.exit_if(yes, pc, EXIT_RESUME, &st, d);
        } else {
            let var = *self
                .push_vars
                .get(&pc)
                .ok_or_else(|| format!("no guard for the push site at {pc}"))?;
            let recv = self.sptr(d - 1);
            let tag = self.fb.load(MemKind::I32U8, recv, 0);
            let obj_tag = self.i32c(TAG_OBJ as i64);
            let is_obj = self.fb.icmp(IntCC::Eq, tag, obj_tag);
            let payload = self.fb.load(PTR_MEM, recv, VALUE_PAYLOAD);
            let known = self.fb.use_var(var);
            let same = self.fb.icmp(IntCC::Eq, payload, known);
            let fast = self.fb.binary(BinaryOp::Band, is_obj, same);
            let check = self.fb.create_block();
            let cont = self.fb.create_block();
            self.fb.brif(fast, cont, &[], check, &[]);
            self.fb.seal_block(check);
            self.fb.switch_to_block(check);
            let pcv = self.i32c(pc as i64);
            let g = self.call_status(Helper::ArrPushGuard, &[self.frame, pcv, recv]);
            let zero = self.i32c(0);
            let bad = self.fb.icmp(IntCC::Eq, g, zero);
            self.exit_if(bad, pc, EXIT_RESUME, &st, d);
            self.fb.def_var(var, payload);
            self.fb.jump(cont, &[]);
            self.fb.seal_block(cont);
            self.fb.switch_to_block(cont);
        }
        self.set_stack_tag(d, TAG_UNDEFINED);
        self.stack.push(Entry::Boxed);
        Ok(())
    }

    /// The `CallWithThis(1)` of an inline push site: [`Helper::ArrPush`] appends (or runs the
    /// builtin when the dense append doesn't apply) and leaves the new length in the receiver's
    /// slot.
    fn arr_call(&mut self, pc: usize) -> Result<(), String> {
        let d = self.stack.len();
        if d < 3 {
            return Err(format!("push site stack at {pc}"));
        }
        self.box_from(d - 1);
        let (recv, arg) = (self.sptr(d - 3), self.sptr(d - 1));
        self.set_call_site(pc);
        let st = self.call_status(Helper::ArrPush, &[self.frame, recv, arg]);
        let below = self.stack[..d - 3].to_vec();
        let one = self.i32c(STATUS_THROW as i64);
        let threw = self.fb.icmp(IntCC::Eq, st, one);
        self.exit_if(threw, pc, EXIT_THROW, &below, d - 3);
        // The native append ran no JS: only element views of the (grown) array are stale.
        let slow = self.i32c(helpers::STATUS_OK_SLOW as i64);
        let ran = self.fb.icmp(IntCC::Eq, st, slow);
        let inv = self.fb.create_block();
        let fast = self.fb.create_block();
        let cont = self.fb.create_block();
        self.fb.brif(ran, inv, &[], fast, &[]);
        self.fb.seal_block(inv);
        self.fb.switch_to_block(inv);
        self.invalidate_js();
        self.fb.jump(cont, &[]);
        self.fb.seal_block(fast);
        self.fb.switch_to_block(fast);
        self.invalidate_elems();
        self.fb.jump(cont, &[]);
        self.fb.seal_block(cont);
        self.fb.switch_to_block(cont);
        self.stack.truncate(d - 3);
        self.stack.push(Entry::Boxed);
        Ok(())
    }

    // ---- bounds-check elimination ------------------------------------------------------------

    /// The in-bounds fact for `src[i]` when the element read at `pc` takes its key from
    /// `LoadLocal(i)` right before it in the same block.
    fn idx_fact_at(&self, pc: usize, src: Src) -> Option<(Src, u16)> {
        if pc == 0 || self.is_leader(pc) {
            return None;
        }
        match self.ops[pc - 1] {
            Op::LoadLocal(i) if self.idx_vars.contains_key(&(src, i)) => Some((src, i)),
            _ => None,
        }
    }

    /// At a `JumpIfNotCmp(Lt)` of `LoadLocal(i)` against `<a>.length` (see [`plan`]): record
    /// whether `i` is an integer index inside `a`'s element-storage view.
    fn set_idx_fact(&mut self, pc: usize) {
        let d = self.stack.len();
        if pc < self.header + 2 || self.is_leader(pc) || self.is_leader(pc - 1) {
            return;
        }
        let at = |q: usize| if q >= self.header { Some(self.ops[q]) } else { None };
        let fact = match (self.ops[pc - 1], self.ops[pc - 2]) {
            (Op::GetPropLocal(s, ..), Op::LoadLocal(i)) => Some((Src::Slot(s), i)),
            (Op::GetProp(..), obj) if !self.is_leader(pc - 2) => {
                let src = match obj {
                    Op::LoadLocal(s) => Some(Src::Slot(s)),
                    Op::LoadName(n, _) if self.plan.name_ref.contains(&(pc - 2)) => {
                        Some(Src::Name(n))
                    }
                    Op::LoadCap(n) => Some(Src::Cap(n)),
                    _ => None,
                };
                match (src, pc.checked_sub(3).and_then(at)) {
                    (Some(src), Some(Op::LoadLocal(i))) => Some((src, i)),
                    _ => None,
                }
            }
            _ => None,
        };
        let Some((src, i)) = fact else { return };
        let (Some(&var), Some(&a)) = (self.idx_vars.get(&(src, i)), self.arr_vars.get(&src)) else {
            return;
        };
        let Entry::Num(x) = self.stack[d - 2] else {
            return;
        };
        let kind = self.fb.use_var(a.kind);
        let count = self.fb.use_var(a.count);
        let ii = self.to_index(x);
        let back = self.fb.convert(ConvOp::FromSint, Type::F64, ii);
        let exact = self.fb.fcmp(FloatCC::Eq, back, x);
        let inb = self.fb.icmp(IntCC::Ult, ii, count);
        let zero = self.i32c(0);
        let has = self.fb.icmp(IntCC::Ne, kind, zero);
        let ok = self.fb.binary(BinaryOp::Band, exact, inb);
        let ok = self.fb.binary(BinaryOp::Band, ok, has);
        self.fb.def_var(var, ok);
    }

    fn store_local(&mut self, pc: usize, s: usize) {
        let d = self.stack.len();
        let e = self.stack[d - 1];
        match (self.kinds[s], e) {
            (Kind::Num, Entry::Num(x)) | (Kind::Bool, Entry::Bool(x)) => {
                self.fb.def_var(self.vars[s].expect("SSA slot"), x);
            }
            (Kind::Num | Kind::Bool, Entry::Boxed) => {
                // Guard the value keeps the slot's kind; otherwise the interpreter stores it
                // (and the next entry guard fails, recompiling for the new kind).
                let (tag, num) = match self.kinds[s] {
                    Kind::Num => (TAG_NUM, true),
                    _ => (TAG_BOOL, false),
                };
                let t = self.stack_tag(d - 1);
                let want = self.i32c(tag as i64);
                let bad = self.fb.icmp(IntCC::Ne, t, want);
                let snap = self.stack.clone();
                self.exit_if(bad, pc, EXIT_RESUME, &snap, d);
                let x = if num {
                    self.stack_num(d - 1)
                } else {
                    self.stack_bool(d - 1)
                };
                self.fb.def_var(self.vars[s].expect("SSA slot"), x);
            }
            (Kind::Num | Kind::Bool, _) => {
                self.exit_now(pc);
                return;
            }
            (Kind::Boxed, Entry::Num(x)) => {
                let sc = self.i32c(s as i64);
                self.call(Helper::StoreLocalNum, &[self.frame, sc, x]);
            }
            (Kind::Boxed, _) => {
                self.store_entry(d - 1, e);
                self.move_to_slot(s, d - 1);
            }
        }
        self.clear_tdz(s);
        self.stack.pop();
    }

    /// Move the value at `frame.stack[d]` (left `undefined`) into Boxed slot `s`, releasing the
    /// slot's old value inline (see [`Tr::drop_mem`]).
    fn move_to_slot(&mut self, s: usize, d: usize) {
        let off = s as i32 * VALUE_SIZE;
        let sl = self.slots;
        self.drop_mem(sl, off);
        let so = self.soff(d);
        let sp = self.stackp;
        self.copy_value(sp, so, sl, off);
        self.set_stack_tag(d, TAG_UNDEFINED);
    }

    fn update_local(&mut self, pc: usize, s: usize, k: UpdKind) {
        let inc = matches!(k, UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard);
        let (old, new) = match self.kinds[s] {
            Kind::Num => {
                self.tdz_guard(s, pc);
                let var = self.vars[s].expect("SSA slot");
                let old = self.fb.use_var(var);
                let one = self.fb.f64const(1.0);
                let new = if inc {
                    self.fb.binary(BinaryOp::Fadd, old, one)
                } else {
                    self.fb.binary(BinaryOp::Fsub, old, one)
                };
                self.fb.def_var(var, new);
                (old, new)
            }
            // `true++` turns the slot into a Number: let the interpreter change its kind.
            Kind::Bool => {
                self.exit_now(pc);
                return;
            }
            // A Boxed slot currently holding a Number updates in place (a Number needs no drop).
            Kind::Boxed => {
                let off = s as i32 * VALUE_SIZE;
                let tag = self.fb.load(MemKind::I32U8, self.slots, off);
                let four = self.i32c(TAG_NUM as i64);
                let bad = self.fb.icmp(IntCC::Ne, tag, four);
                let snap = self.stack.clone();
                self.exit_if(bad, pc, EXIT_RESUME, &snap, snap.len());
                let old = self.fb.load(MemKind::F64, self.slots, off + VALUE_PAYLOAD);
                let one = self.fb.f64const(1.0);
                let new = if inc {
                    self.fb.binary(BinaryOp::Fadd, old, one)
                } else {
                    self.fb.binary(BinaryOp::Fsub, old, one)
                };
                self.fb
                    .store(MemKind::F64, self.slots, new, off + VALUE_PAYLOAD);
                (old, new)
            }
        };
        match k {
            UpdKind::PreInc | UpdKind::PreDec => self.stack.push(Entry::Num(new)),
            UpdKind::PostInc | UpdKind::PostDec => self.stack.push(Entry::Num(old)),
            UpdKind::IncDiscard | UpdKind::DecDiscard => {}
        }
    }

    /// A fused-op operand's F64 when it is statically a Number (an SSA `Num` slot — after its
    /// TDZ guard — or a Number constant).
    fn opnd_num(&mut self, o: Opnd, pc: usize) -> Option<V> {
        match o {
            Opnd::Slot(s) if self.kinds[s as usize] == Kind::Num => {
                self.tdz_guard(s as usize, pc);
                let var = self.vars[s as usize].expect("SSA slot");
                Some(self.fb.use_var(var))
            }
            Opnd::Const(k) => match self.chunk.consts[k as usize] {
                Value::Num(n) => Some(self.fb.f64const(n)),
                _ => None,
            },
            _ => None,
        }
    }

    /// Whether [`Tr::quick_num`] can read `o`: a Number SSA slot or constant, or an in-memory
    /// slot (checked at run time).
    fn quick_num_ok(&self, o: Opnd) -> bool {
        match o {
            Opnd::Slot(s) => self.kinds[s as usize] == Kind::Num || self.vars[s as usize].is_none(),
            Opnd::Const(k) => matches!(self.chunk.consts[k as usize], Value::Num(_)),
        }
    }

    /// `o` as a Number, to `miss` when an in-memory slot holds anything else (a TDZ slot
    /// included).
    fn quick_num(&mut self, o: Opnd, pc: usize, miss: Block) -> V {
        if let Some(x) = self.opnd_num(o, pc) {
            return x;
        }
        let Opnd::Slot(s) = o else {
            unreachable!("quick_num_ok admitted a non-Number constant")
        };
        let p = self.slot_ptr(s as usize);
        let t = self.fb.load(MemKind::I32U8, p, 0);
        let four = self.i32c(TAG_NUM as i64);
        let is = self.fb.icmp(IntCC::Eq, t, four);
        self.guard_to(is, miss);
        self.fb.load(MemKind::F64, p, VALUE_PAYLOAD)
    }

    fn opnd_static_num(&self, o: Opnd) -> bool {
        match o {
            Opnd::Slot(s) => self.kinds[s as usize] == Kind::Num,
            Opnd::Const(k) => matches!(self.chunk.consts[k as usize], Value::Num(_)),
        }
    }

    /// Materialize fused-op operands `a`, `b` as owned values at scratch `d`, `d + 1` (the
    /// stack depth is `d`), in the interpreter's order: TDZ errors for `a` before `b`.
    fn opnds_to_scratch(&mut self, pc: usize, a: Opnd, b: Opnd) {
        let d = self.stack.len();
        // SSA TDZ guards exit before anything is cloned (nothing to drop on that exit); the
        // interpreter then raises whichever error comes first.
        for o in [a, b] {
            if let Opnd::Slot(s) = o {
                if self.vars[s as usize].is_some() {
                    self.tdz_guard(s as usize, pc);
                }
            }
        }
        for (i, o) in [a, b].into_iter().enumerate() {
            let at = d + i;
            match o {
                Opnd::Slot(s) if self.vars[s as usize].is_some() => {
                    let e = self.ssa_entry(s as usize);
                    self.store_entry(at, e);
                }
                Opnd::Slot(s) => {
                    let p = self.sptr(at);
                    let sc = self.i32c(s as i64);
                    let st = self.call_status(Helper::LoadLocal, &[self.frame, sc, p]);
                    let snap = self.stack.clone();
                    // A throw loading `b` leaves `a`'s clone at `d` live: include it.
                    self.throw_if(st, pc, &snap, at);
                }
                Opnd::Const(k) => match self.chunk.consts[k as usize] {
                    Value::Num(n) => {
                        let x = self.fb.f64const(n);
                        self.store_entry(at, Entry::Num(x));
                    }
                    _ => {
                        let dst = self.sptr(at);
                        let src = self.const_ptr(k as usize);
                        self.call(Helper::Clone, &[dst, src]);
                    }
                },
            }
        }
    }

    /// `ArithLL` / `ArithLK`: `slots[dst] = a <kind> b`.
    fn arith_local(
        &mut self,
        pc: usize,
        kind: ArithKind,
        dst: usize,
        a: Opnd,
        b: Opnd,
    ) -> Result<(), String> {
        if self.opnd_static_num(a) && self.opnd_static_num(b) {
            if self.kinds[dst] == Kind::Bool {
                self.exit_now(pc);
                return Ok(());
            }
            let x = self.opnd_num(a, pc).expect("static Num");
            let y = self.opnd_num(b, pc).expect("static Num");
            let r = self.num_arith(kind, x, y)?;
            match self.kinds[dst] {
                Kind::Num => {
                    self.fb.def_var(self.vars[dst].expect("SSA slot"), r);
                    self.clear_tdz(dst);
                }
                _ => {
                    let sc = self.i32c(dst as i64);
                    self.call(Helper::StoreLocalNum, &[self.frame, sc, r]);
                }
            }
            return Ok(());
        }
        // A Boxed (in-memory) destination with both operands Numbers at run time: the op
        // inline, its Number stored over the slot's trivially droppable old value.
        let quick = if self.kinds[dst] == Kind::Boxed
            && self.vars[dst].is_none()
            && matches!(
                kind,
                ArithKind::Add | ArithKind::Sub | ArithKind::Mul | ArithKind::Div
            )
            && self.quick_num_ok(a)
            && self.quick_num_ok(b)
        {
            let slow = self.fb.create_block();
            let x = self.quick_num(a, pc, slow);
            let y = self.quick_num(b, pc, slow);
            let p = self.slot_ptr(dst);
            let t = self.fb.load(MemKind::I32U8, p, 0);
            let four = self.i32c(TAG_NUM as i64);
            let trivial = self.fb.icmp(IntCC::Ule, t, four);
            self.guard_to(trivial, slow);
            let r = self.num_arith(kind, x, y)?;
            self.fb.store(MemKind::I32U8, p, four, 0);
            self.fb.store(MemKind::F64, p, r, VALUE_PAYLOAD);
            let done = self.fb.create_block();
            self.fb.jump(done, &[]);
            self.fb.seal_block(slow);
            self.fb.switch_to_block(slow);
            Some(done)
        } else {
            None
        };
        let d = self.stack.len();
        // `s += y` on a string local: append in place (the slot's string is uniquely owned
        // when only the local holds it) instead of copying it into the generic `+`.
        let append = matches!((kind, a), (ArithKind::Add, Opnd::Slot(s)) if s as usize == dst)
            && !matches!(b, Opnd::Slot(s) if s as usize == dst)
            && self.kinds[dst] == Kind::Boxed
            && self.vars[dst].is_none();
        let done = if append {
            let yp = match b {
                Opnd::Const(k) => self.const_ptr(k as usize),
                Opnd::Slot(s) if self.vars[s as usize].is_none() => self.slot_ptr(s as usize),
                Opnd::Slot(s) => {
                    self.tdz_guard(s as usize, pc);
                    let e = self.ssa_entry(s as usize);
                    self.store_entry(d + 1, e);
                    self.sptr(d + 1)
                }
            };
            let dv = self.i32c(dst as i64);
            let r = self
                .call(Helper::AppendLocal, &[self.frame, dv, yp])
                .expect("AppendLocal returns a flag");
            let z = self.i32c(0);
            let hit = self.fb.icmp(IntCC::Ne, r, z);
            let done = self.fb.create_block();
            let gen = self.fb.create_block();
            self.fb.brif(hit, done, &[], gen, &[]);
            self.fb.seal_block(gen);
            self.fb.switch_to_block(gen);
            Some(done)
        } else {
            None
        };
        self.opnds_to_scratch(pc, a, b);
        let code = self.i32c(bin_code(&arith_op(kind))? as i64);
        let (pa, pb, pr) = (self.sptr(d), self.sptr(d + 1), self.sptr(d + 2));
        let st = self.call_status(Helper::Binary, &[self.frame, code, pa, pb, pr]);
        let snap = self.stack.clone();
        self.throw_if(st, pc, &snap, d);
        match self.kinds[dst] {
            Kind::Boxed => {
                let sc = self.i32c(dst as i64);
                self.call(Helper::StoreLocal, &[self.frame, sc, pr]);
                if let Some(done) = done {
                    self.fb.jump(done, &[]);
                    self.fb.seal_block(done);
                    self.fb.switch_to_block(done);
                }
                if let Some(q) = quick {
                    self.fb.jump(q, &[]);
                    self.fb.seal_block(q);
                    self.fb.switch_to_block(q);
                }
            }
            k => {
                // The op ran; a result of another kind goes into the slot and the interpreter
                // continues after the op.
                let tag = if k == Kind::Num { TAG_NUM } else { TAG_BOOL };
                let t = self.stack_tag(d + 2);
                let want = self.i32c(tag as i64);
                let bad = self.fb.icmp(IntCC::Ne, t, want);
                let ex = self.fb.create_block();
                let cont = self.fb.create_block();
                self.fb.brif(bad, ex, &[], cont, &[]);
                self.fb.seal_block(ex);
                self.fb.switch_to_block(ex);
                self.emit_exit(pc + 1, EXIT_RESUME, &snap, d, Some((dst, d + 2)));
                self.fb.seal_block(cont);
                self.fb.switch_to_block(cont);
                let x = if k == Kind::Num {
                    self.stack_num(d + 2)
                } else {
                    self.stack_bool(d + 2)
                };
                self.fb.def_var(self.vars[dst].expect("SSA slot"), x);
                self.clear_tdz(dst);
            }
        }
        Ok(())
    }

    /// `JumpIfNotCmpLL` / `JumpIfNotCmpLK`.
    fn cmp_local(
        &mut self,
        pc: usize,
        kind: CmpKind,
        a: Opnd,
        b: Opnd,
        target: usize,
    ) -> Result<(), String> {
        let cond = if self.opnd_static_num(a) && self.opnd_static_num(b) {
            let x = self.opnd_num(a, pc).expect("static Num");
            let y = self.opnd_num(b, pc).expect("static Num");
            self.fb.fcmp(fcc(kind), x, y)
        } else if let (Some(neg), Some(pa), Some(pb)) = (
            match kind {
                CmpKind::StrictEq => Some(false),
                CmpKind::StrictNotEq => Some(true),
                _ => None,
            },
            self.opnd_ptr(a),
            self.opnd_ptr(b),
        ) {
            // `===` of memory operands, compared in place (no clones).
            let r = self.str_eq_ref(pa, pb);
            let two = self.i32c(2);
            let tdz = self.fb.icmp(IntCC::Eq, r, two);
            let join = self.fb.create_block();
            let res = self.fb.append_block_param(join, Type::I32);
            let gen = self.fb.create_block();
            let fast = self.fb.create_block();
            self.fb.brif(tdz, gen, &[], fast, &[]);
            self.fb.seal_block(fast);
            self.fb.switch_to_block(fast);
            let r = if neg {
                let one = self.i32c(1);
                self.fb.binary(BinaryOp::Bxor, r, one)
            } else {
                r
            };
            self.fb.jump(join, &[r]);
            self.fb.seal_block(gen);
            self.fb.switch_to_block(gen);
            let d = self.stack.len();
            self.opnds_to_scratch(pc, a, b);
            let (pa, pb) = (self.sptr(d), self.sptr(d + 1));
            let r = self.strict_cmp(kind, d, pa, pb).expect("strict kind");
            self.fb.jump(join, &[r]);
            self.fb.seal_block(join);
            self.fb.switch_to_block(join);
            res
        } else {
            let d = self.stack.len();
            self.opnds_to_scratch(pc, a, b);
            let (pa, pb) = (self.sptr(d), self.sptr(d + 1));
            match self.strict_cmp(kind, d, pa, pb) {
                Some(r) => r,
                None => {
                    let code = self.i32c(bin_code(&cmp_op(kind))? as i64);
                    let pr = self.sptr(d + 2);
                    let st = self.call_status(Helper::Binary, &[self.frame, code, pa, pb, pr]);
                    let snap = self.stack.clone();
                    self.throw_if(st, pc, &snap, d);
                    self.call_status(Helper::ToBoolean, &[self.frame, pr])
                }
            }
        };
        self.branch(pc, cond, pc + 1, target)
    }

    /// The truth of `a <kind> b` for the top two stack entries, popping them.
    fn cmp_stack(&mut self, pc: usize, kind: CmpKind) -> Result<V, String> {
        let d = self.stack.len();
        let (a, b) = (self.stack[d - 2], self.stack[d - 1]);
        let r = match (a, b) {
            (Entry::Num(x), Entry::Num(y)) => self.fb.fcmp(fcc(kind), x, y),
            (Entry::Bool(x), Entry::Bool(y)) if is_eq_kind(kind) => {
                let cc = if matches!(kind, CmpKind::EqEq | CmpKind::StrictEq) {
                    IntCC::Eq
                } else {
                    IntCC::Ne
                };
                self.fb.icmp(cc, x, y)
            }
            (
                Entry::Num(_) | Entry::Boxed | Entry::Ref(..),
                Entry::Num(_) | Entry::Boxed | Entry::Ref(..),
            ) => {
                let slow = self.fb.create_block();
                let join = self.fb.create_block();
                let r = self.fb.append_block_param(join, Type::I32);
                let x = self.num_or_slow(a, d - 2, slow);
                let y = self.num_or_slow(b, d - 1, slow);
                let c = self.fb.fcmp(fcc(kind), x, y);
                self.fb.jump(join, &[c]);
                self.fb.seal_block(slow);
                self.fb.switch_to_block(slow);
                let t = self.cmp_slow(pc, kind)?;
                self.fb.jump(join, &[t]);
                self.fb.seal_block(join);
                self.fb.switch_to_block(join);
                r
            }
            _ => self.cmp_slow(pc, kind)?,
        };
        self.stack.truncate(d - 2);
        Ok(r)
    }

    /// `Binary` + `ToBoolean` on the top two entries (the stack itself is left for the caller).
    fn cmp_slow(&mut self, pc: usize, kind: CmpKind) -> Result<V, String> {
        let d = self.stack.len();
        self.box_from(d - 2);
        let (pa, pb) = (self.sptr(d - 2), self.sptr(d - 1));
        if let Some(r) = self.strict_cmp(kind, d - 2, pa, pb) {
            self.stack.truncate(d - 2);
            self.stack.extend([Entry::Boxed, Entry::Boxed]);
            return Ok(r);
        }
        let code = self.i32c(bin_code(&cmp_op(kind))? as i64);
        let pr = self.sptr(d);
        let st = self.call_status(Helper::Binary, &[self.frame, code, pa, pb, pr]);
        let snap = self.stack[..d - 2].to_vec();
        self.throw_if(st, pc, &snap, d - 2);
        Ok(self.call_status(Helper::ToBoolean, &[self.frame, pr]))
    }

    /// The address of a fused local op's operand when it lives in memory (a constant, or a
    /// slot the translator keeps in the frame).
    fn opnd_ptr(&mut self, o: Opnd) -> Option<V> {
        match o {
            Opnd::Const(k) => Some(self.const_ptr(k as usize)),
            Opnd::Slot(s) if self.vars[s as usize].is_none() && self.kinds[s as usize] == Kind::Boxed => {
                Some(self.slot_ptr(s as usize))
            }
            Opnd::Slot(_) => None,
        }
    }

    /// [`Helper::StrictEqRef`] of the borrowed values at `pa` / `pb` (0 / 1, or 2 for a TDZ
    /// marker), deciding two Strings inline when it can: the same string, different byte
    /// lengths, or one byte each.
    fn str_eq_ref(&mut self, pa: V, pb: V) -> V {
        let join = self.fb.create_block();
        let r = self.fb.append_block_param(join, Type::I32);
        let call = self.fb.create_block();
        self.str_eq_inline(pa, pb, join, call);
        self.fb.seal_block(call);
        self.fb.switch_to_block(call);
        let v = self
            .call(Helper::StrictEqRef, &[self.frame, pa, pb])
            .expect("StrictEqRef returns a flag");
        self.fb.jump(join, &[v]);
        self.fb.seal_block(join);
        self.fb.switch_to_block(join);
        r
    }

    /// Decide `===` of the values at `pa` / `pb` inline when both are Strings and it can: the
    /// same string, different byte lengths, or one byte each. Jumps to `decided` (an I32 block
    /// parameter: the truth) or to `undecided`; neither block is sealed here.
    fn str_eq_inline(&mut self, pa: V, pb: V, decided: Block, undecided: Block) {
        let (join, call) = (decided, undecided);
        let strs = self.fb.create_block();
        let ta = self.fb.load(MemKind::I32U8, pa, 0);
        let tb = self.fb.load(MemKind::I32U8, pb, 0);
        let st = self.i32c(TAG_STR as i64);
        let a_str = self.fb.icmp(IntCC::Eq, ta, st);
        let b_str = self.fb.icmp(IntCC::Eq, tb, st);
        let both = self.fb.binary(BinaryOp::Band, a_str, b_str);
        self.fb.brif(both, strs, &[], call, &[]);
        self.fb.seal_block(strs);
        self.fb.switch_to_block(strs);
        let ha = self.fb.load(PTR_MEM, pa, VALUE_PAYLOAD);
        let hb = self.fb.load(PTR_MEM, pb, VALUE_PAYLOAD);
        let one = self.i32c(1);
        let zero = self.i32c(0);
        let same = self.fb.icmp(IntCC::Eq, ha, hb);
        let lens = self.fb.create_block();
        self.fb.brif(same, join, &[one], lens, &[]);
        self.fb.seal_block(lens);
        self.fb.switch_to_block(lens);
        let len_off = crate::lstr::LSTR_LEN_OFFSET as i32;
        let la = self.fb.load(MemKind::I32, ha, len_off);
        let lb = self.fb.load(MemKind::I32, hb, len_off);
        let differ = self.fb.icmp(IntCC::Ne, la, lb);
        let short = self.fb.create_block();
        self.fb.brif(differ, join, &[zero], short, &[]);
        self.fb.seal_block(short);
        self.fb.switch_to_block(short);
        let is_one = self.fb.icmp(IntCC::Eq, la, one);
        let bytes = self.fb.create_block();
        self.fb.brif(is_one, bytes, &[], call, &[]);
        self.fb.seal_block(bytes);
        self.fb.switch_to_block(bytes);
        let data = crate::lstr::LSTR_DATA_OFFSET as i32;
        let ba = self.fb.load(MemKind::I32U8, ha, data);
        let bb = self.fb.load(MemKind::I32U8, hb, data);
        let eq = self.fb.icmp(IntCC::Eq, ba, bb);
        self.fb.jump(join, &[eq]);
    }

    /// `===` / `!==` of the owned values at `pa` / `pb` (`frame.stack[da]` and `[da + 1]`,
    /// consumed) as an I32 truth value —
    /// never runs JS or throws; `None` for the other comparisons.
    fn strict_cmp(&mut self, kind: CmpKind, da: usize, pa: V, pb: V) -> Option<V> {
        let neg = match kind {
            CmpKind::StrictEq => false,
            CmpKind::StrictNotEq => true,
            _ => return None,
        };
        let join = self.fb.create_block();
        let r = self.fb.append_block_param(join, Type::I32);
        let decided = self.fb.create_block();
        let t = self.fb.append_block_param(decided, Type::I32);
        let call = self.fb.create_block();
        self.str_eq_inline(pa, pb, decided, call);
        self.fb.seal_block(decided);
        self.fb.switch_to_block(decided);
        self.drop_at(da);
        self.drop_at(da + 1);
        self.fb.jump(join, &[t]);
        self.fb.seal_block(call);
        self.fb.switch_to_block(call);
        let v = self
            .call(Helper::StrictEq, &[self.frame, pa, pb])
            .expect("StrictEq returns a flag");
        self.fb.jump(join, &[v]);
        self.fb.seal_block(join);
        self.fb.switch_to_block(join);
        Some(if neg {
            let one = self.i32c(1);
            self.fb.binary(BinaryOp::Bxor, r, one)
        } else {
            r
        })
    }

    /// Stack arithmetic and comparisons.
    fn binop(&mut self, pc: usize, op: Op) -> Result<(), String> {
        let d = self.stack.len();
        let (a, b) = (self.stack[d - 2], self.stack[d - 1]);
        let arith = ArithKind::of(&op);
        let cmp = cmp_of(&op);
        // Computes the result from two F64s.
        let compute = |tr: &mut Self, x: V, y: V| -> Result<Entry, String> {
            Ok(match (arith, cmp) {
                (Some(k), _) => Entry::Num(tr.num_arith(k, x, y)?),
                (_, Some(k)) => Entry::Bool(tr.fb.fcmp(fcc(k), x, y)),
                _ => unreachable!("binop called on {op:?}"),
            })
        };
        match (a, b) {
            (Entry::Num(x), Entry::Num(y)) => {
                self.stack.truncate(d - 2);
                let r = compute(self, x, y)?;
                self.stack.push(r);
            }
            (Entry::Bool(x), Entry::Bool(y)) if cmp.is_some_and(is_eq_kind) => {
                let cc = if matches!(op, Op::EqEq | Op::StrictEq) {
                    IntCC::Eq
                } else {
                    IntCC::Ne
                };
                let r = self.fb.icmp(cc, x, y);
                self.stack.truncate(d - 2);
                self.stack.push(Entry::Bool(r));
            }
            // A Boolean operand is ToNumber'd to 0/1 by arithmetic, relational and loose-equality
            // operators alike (strict equality never equates it with a Number).
            (Entry::Num(_) | Entry::Bool(_), Entry::Num(_) | Entry::Bool(_))
                if !matches!(op, Op::StrictEq | Op::StrictNotEq) =>
            {
                let num = |tr: &mut Self, e: Entry| match e {
                    Entry::Num(x) => x,
                    Entry::Bool(x) => tr.fb.convert(ConvOp::FromSint, Type::F64, x),
                    _ => unreachable!("Num or Bool"),
                };
                let (x, y) = (num(self, a), num(self, b));
                self.stack.truncate(d - 2);
                let r = compute(self, x, y)?;
                self.stack.push(r);
            }
            // A Boxed operand that holds a Number at run time still takes the inline path; the
            // result then lives in memory (Boxed) so both paths agree on the entry's kind.
            (
                Entry::Num(_) | Entry::Boxed | Entry::Ref(..),
                Entry::Num(_) | Entry::Boxed | Entry::Ref(..),
            ) if !matches!(op, Op::Mod) =>
            {
                // Ops whose result is a Number unless an operand is a BigInt: the join takes it
                // unboxed (the slow path exits on the rare other result), so later ops see a Num.
                let numeric = matches!(
                    op,
                    Op::Sub
                        | Op::Mul
                        | Op::Div
                        | Op::BitAnd
                        | Op::BitOr
                        | Op::BitXor
                        | Op::Shl
                        | Op::Shr
                        | Op::UShr
                );
                if numeric {
                    let pre = self.stack.clone();
                    let slow = self.fb.create_block();
                    let join = self.fb.create_block();
                    let param = self.fb.append_block_param(join, Type::F64);
                    let x = self.num_or_slow(a, d - 2, slow);
                    let y = self.num_or_slow(b, d - 1, slow);
                    let Entry::Num(r) = compute(self, x, y)? else {
                        unreachable!("arithmetic yields a Number")
                    };
                    self.fb.jump(join, &[r]);
                    self.fb.seal_block(slow);
                    self.fb.switch_to_block(slow);
                    self.stack = pre.clone();
                    self.generic(pc);
                    self.slow_to_join(pc, d - 2, true, join);
                    self.fb.seal_block(join);
                    self.fb.switch_to_block(join);
                    self.stack = pre[..d - 2].to_vec();
                    self.stack.push(Entry::Num(param));
                    return Ok(());
                }
                // Relational and equality operators always produce a Boolean: the slow path's
                // result joins unboxed too.
                if cmp.is_some() {
                    let pre = self.stack.clone();
                    let slow = self.fb.create_block();
                    let join = self.fb.create_block();
                    let param = self.fb.append_block_param(join, Type::I32);
                    let x = self.num_or_slow(a, d - 2, slow);
                    let y = self.num_or_slow(b, d - 1, slow);
                    let Entry::Bool(r) = compute(self, x, y)? else {
                        unreachable!("comparison yields a Boolean")
                    };
                    self.fb.jump(join, &[r]);
                    self.fb.seal_block(slow);
                    self.fb.switch_to_block(slow);
                    self.stack = pre.clone();
                    self.generic(pc);
                    let r = self.stack_bool(d - 2);
                    self.fb.jump(join, &[r]);
                    self.fb.seal_block(join);
                    self.fb.switch_to_block(join);
                    self.stack = pre[..d - 2].to_vec();
                    self.stack.push(Entry::Bool(param));
                    return Ok(());
                }
                let pre = self.stack.clone();
                let slow = self.fb.create_block();
                let join = self.fb.create_block();
                let x = self.num_or_slow(a, d - 2, slow);
                let y = self.num_or_slow(b, d - 1, slow);
                let r = compute(self, x, y)?;
                // Both operands are trivially droppable here (tag 4 or unboxed).
                self.store_entry(d - 2, r);
                self.fb.jump(join, &[]);
                self.fb.seal_block(slow);
                self.fb.switch_to_block(slow);
                self.stack = pre.clone();
                self.generic(pc);
                self.fb.jump(join, &[]);
                self.fb.seal_block(join);
                self.fb.switch_to_block(join);
                self.stack = pre[..d - 2].to_vec();
                self.stack.push(Entry::Boxed);
            }
            _ => self.generic(pc),
        }
        Ok(())
    }

    // ---- elements and properties -------------------------------------------------------------

    /// After a fast path's hit block jumped to `join` and the slow path left a Boxed result at
    /// `at`: forward it to `join` (as F64 when `want`, exiting after the op otherwise).
    fn slow_to_join(&mut self, pc: usize, at: usize, want: bool, join: Block) {
        if want {
            self.num_exits.push(pc + 1);
            let post = self.stack.clone();
            let tag = self.stack_tag(at);
            let four = self.i32c(TAG_NUM as i64);
            let bad = self.fb.icmp(IntCC::Ne, tag, four);
            self.exit_if(bad, pc + 1, EXIT_RESUME, &post, post.len());
            let f = self.stack_num(at);
            self.fb.jump(join, &[f]);
        } else {
            self.fb.jump(join, &[]);
        }
    }

    /// `obj[key]` with the result at stack index `at`. `drop`: the stack index of a consumed
    /// Boxed receiver.
    #[allow(clippy::too_many_arguments)]
    fn elem_read(
        &mut self,
        pc: usize,
        obj: V,
        key: V,
        drop: Option<usize>,
        at: usize,
        want: bool,
        slow: Slow,
        fact: Option<(Src, u16)>,
        src: Option<Src>,
    ) {
        let ta_first = self.ta_first(src) || helpers::ta_hot(self.chunk, pc);
        let pre = self.stack.clone();
        let miss = self.fb.create_block();
        let join = self.fb.create_block();
        let param = want.then(|| self.fb.append_block_param(join, Type::F64));
        let fact = fact.and_then(|k| Some((*self.idx_vars.get(&k)?, *self.arr_vars.get(&k.0)?)));
        if let Some((ok_var, a)) = fact {
            // `key` is an integer inside the array's element-storage view: load it directly.
            let ok = self.fb.use_var(ok_var);
            let fast = self.fb.create_block();
            let checked = self.fb.create_block();
            self.fb.brif(ok, fast, &[], checked, &[]);
            self.fb.seal_block(fast);
            self.fb.switch_to_block(fast);
            let kind = self.fb.use_var(a.kind);
            let base = self.fb.use_var(a.base);
            let ii = self.to_index(key);
            let x = layout::view_elem_num(&mut self.fb, kind, base, ii, checked);
            self.seal_current();
            if let Some(i) = drop {
                self.drop_at(i);
            }
            if want {
                self.fb.jump(join, &[x]);
            } else {
                self.store_entry(at, Entry::Num(x));
                self.fb.jump(join, &[]);
            }
            self.fb.seal_block(checked);
            self.fb.switch_to_block(checked);
        }
        // The typed-array path (`ic_plain` clear, which every ordinary path requires set) goes
        // first when the receiver is one now, else after the ordinary path missed.
        let slow_b = if ta_first { miss } else { self.fb.create_block() };
        if ta_first {
            let ord = self.fb.create_block();
            if let Some(x) = self.ta_access(pc, obj, key, None, ord, slow_b) {
                self.read_hit(drop, want, at, join, x);
            }
            self.fb.seal_block(ord);
            self.fb.switch_to_block(ord);
            self.stack = pre.clone();
        }
        let x = layout::elem_get_num(&mut self.fb, obj, key, miss);
        self.seal_current();
        self.read_hit(drop, want, at, join, x);
        if !ta_first {
            self.fb.seal_block(miss);
            self.fb.switch_to_block(miss);
            self.stack = pre.clone();
            if let Some(x) = self.ta_access(pc, obj, key, None, slow_b, slow_b) {
                self.read_hit(drop, want, at, join, x);
            }
        }
        self.fb.seal_block(slow_b);
        self.fb.switch_to_block(slow_b);
        self.stack = pre.clone();
        // Non-number elements (a string's characters, boxed array elements) through the
        // interpreter's fast paths before the generic op.
        if !want
            && matches!(slow, Slow::Generic)
            && matches!(self.ops[pc], Op::GetElem | Op::GetElemLocal(_) | Op::GetMethodElem)
        {
            // An object element, inline: a retained handle word.
            let helper_b = self.fb.create_block();
            let bits = layout::elem_get_word(&mut self.fb, obj, key, helper_b);
            let (is_obj, is_str, w) = layout::word_counted(&mut self.fb, bits);
            let rc_so = layout::rc_strong_offset();
            let counted = match rc_so {
                Some(_) => self.fb.binary(BinaryOp::Bor, is_obj, is_str),
                None => is_obj,
            };
            let obj_b = self.fb.create_block();
            self.fb.brif(counted, obj_b, &[], helper_b, &[]);
            self.fb.seal_block(obj_b);
            self.fb.switch_to_block(obj_b);
            let gc = if PTR == Type::I64 {
                w
            } else {
                self.fb.convert(ConvOp::Wrap, PTR, w)
            };
            let so = rc_so.unwrap_or(crate::value::GC_STRONG_OFFSET as i32);
            let strong = self.fb.load(PTR_MEM, gc, so);
            let one = self.ptrc(1);
            let s1 = self.fb.binary(BinaryOp::Iadd, strong, one);
            self.fb.store(PTR_MEM, gc, s1, so);
            if let Some(i) = drop {
                self.drop_at(i);
            }
            let o = self.soff(at);
            let to = self.i32c(TAG_OBJ as i64);
            let ts = self.i32c(TAG_STR as i64);
            let t = self.fb.select(is_obj, to, ts);
            self.fb.store(MemKind::I32U8, self.stackp, t, o);
            self.fb.store(PTR_MEM, self.stackp, gc, o + VALUE_PAYLOAD);
            self.fb.jump(join, &[]);
            self.fb.seal_block(helper_b);
            self.fb.switch_to_block(helper_b);
            let dst = self.sptr(at);
            let cv = self.i32c(drop.is_some() as i64);
            let r = self
                .call(Helper::ElemGet, &[self.frame, obj, key, dst, cv])
                .expect("ElemGet returns a flag");
            let z = self.i32c(0);
            let hit = self.fb.icmp(IntCC::Ne, r, z);
            let gen = self.fb.create_block();
            self.fb.brif(hit, join, &[], gen, &[]);
            self.fb.seal_block(gen);
            self.fb.switch_to_block(gen);
        }
        if self.slow_path(pc, slow) {
            self.slow_to_join(pc, at, want, join);
        }
        self.fb.seal_block(join);
        self.fb.switch_to_block(join);
        self.stack = pre[..at].to_vec();
        self.stack.push(match param {
            Some(p) => Entry::Num(p),
            None => Entry::Boxed,
        });
    }

    /// `obj[key] = v` (`v` Num, or Boxed guarded to hold a Number); `base` is the stack index
    /// the op's result (when `keep`) lands at.
    #[allow(clippy::too_many_arguments)]
    fn elem_write(
        &mut self,
        pc: usize,
        obj: V,
        key: V,
        v: Entry,
        drop: Option<usize>,
        keep: bool,
        base: usize,
        slow: Slow,
        src: Option<Src>,
    ) {
        let ta_first = self.ta_first(src) || helpers::ta_hot(self.chunk, pc);
        let d = self.stack.len();
        let pre = self.stack.clone();
        let slow_b = self.fb.create_block();
        let join = self.fb.create_block();
        let vf = self.num_or_slow(v, d - 1, slow_b);
        let boxed = matches!(v, Entry::Boxed | Entry::Ref(..));
        // Typed arrays first or after the ordinary path (see `elem_read`).
        if ta_first {
            let ord = self.fb.create_block();
            if self.ta_access(pc, obj, key, Some(vf), ord, slow_b).is_some() {
                self.write_hit(drop, keep && boxed, base, join, vf);
            }
            self.fb.seal_block(ord);
            self.fb.switch_to_block(ord);
            self.stack = pre.clone();
        }
        let miss = if ta_first { slow_b } else { self.fb.create_block() };
        layout::elem_set_num(&mut self.fb, obj, key, vf, miss);
        self.seal_current();
        self.write_hit(drop, keep && boxed, base, join, vf);
        if !ta_first {
            self.fb.seal_block(miss);
            self.fb.switch_to_block(miss);
            self.stack = pre.clone();
            if self.ta_access(pc, obj, key, Some(vf), slow_b, slow_b).is_some() {
                self.write_hit(drop, keep && boxed, base, join, vf);
            }
        }
        self.fb.seal_block(slow_b);
        self.fb.switch_to_block(slow_b);
        self.stack = pre.clone();
        if (matches!(v, Entry::Boxed) && !keep) || matches!(v, Entry::Num(_)) {
            // An owned value (a Number is a copy): a plain overwrite, a hole fill or a dense
            // append natively.
            self.store_entry(d - 1, v);
            let vp = self.sptr(d - 1);
            let r = self
                .call(Helper::ElemSetBoxed, &[self.frame, obj, key, vp])
                .expect("ElemSetBoxed returns a flag");
            let z = self.i32c(0);
            let hit = self.fb.icmp(IntCC::Ne, r, z);
            let hit_b = self.fb.create_block();
            let gen_b = self.fb.create_block();
            self.fb.brif(hit, hit_b, &[], gen_b, &[]);
            self.fb.seal_block(hit_b);
            self.fb.seal_block(gen_b);
            self.fb.switch_to_block(hit_b);
            // The storage may have grown (an append), and the old value was released.
            self.invalidate_elems();
            if let Some(i) = drop {
                self.drop_at(i);
            }
            self.fb.jump(join, &[]);
            self.fb.switch_to_block(gen_b);
        }
        if self.slow_path(pc, slow) {
            self.fb.jump(join, &[]);
        }
        self.fb.seal_block(join);
        self.fb.switch_to_block(join);
        self.stack = pre[..base].to_vec();
        if keep {
            // An unboxed value stays valid: the slow path's result is a copy of the same Number.
            self.stack.push(match v {
                Entry::Ref(..) => Entry::Boxed,
                e => e,
            });
        }
    }

    /// Whether an element site whose receiver is `src` tries the typed-array path first: the
    /// receiver slot held a typed array when the region was compiled.
    fn ta_first(&self, src: Option<Src>) -> bool {
        matches!(src, Some(Src::Slot(s)) if self.ta_slots.get(s as usize).copied().unwrap_or(false))
    }

    /// An element read's fast path produced `x`: drop the consumed receiver, deliver `x`.
    fn read_hit(&mut self, drop: Option<usize>, want: bool, at: usize, join: Block, x: V) {
        if let Some(i) = drop {
            self.drop_at(i);
        }
        if want {
            self.fb.jump(join, &[x]);
        } else {
            self.store_entry(at, Entry::Num(x));
            self.fb.jump(join, &[]);
        }
    }

    /// An element write's fast path stored `vf`: drop the consumed receiver, and when `put`
    /// leave the (Boxed) value's Number as the op's result.
    fn write_hit(&mut self, drop: Option<usize>, put: bool, base: usize, join: Block, vf: V) {
        if let Some(i) = drop {
            self.drop_at(i);
        }
        if put {
            self.store_entry(base, Entry::Num(vf));
        }
        self.fb.jump(join, &[]);
    }

    /// A typed-array element access at element site `pc`: `obj[key]` (returns the F64 element)
    /// or, with `store`, `obj[key] = store` with the element kind's conversion (returns a
    /// dummy). Jumps to `other` unless `obj` is an object with `ic_plain` clear (the ordinary
    /// paths need it set), then misses to `slow` unless it is a typed array [`Helper::TaView`]
    /// accepts and `key` an integer in bounds. The view is cached per site until JS runs; the
    /// cached handle is only trusted together with `ic_plain` being clear (a freed typed
    /// array's address can only be reused without JS by an ordinary object, which has it
    /// set). Sites without a cache ([`Tr::ta_vars`]) take a fresh view every time. Always
    /// `Some` (kept for the callers' shape).
    fn ta_access(
        &mut self,
        pc: usize,
        obj: V,
        key: V,
        store: Option<V>,
        other: Block,
        slow: Block,
    ) -> Option<V> {
        use helpers::ta_code as tc;
        let cache = self.ta_vars.get(&pc).copied();
        let gc = layout::side_table_object(&mut self.fb, obj, other);
        self.seal_current();
        // The cached view, or a fresh one.
        let have = self.fb.create_block();
        let hk = self.fb.append_block_param(have, Type::I32);
        let hd = self.fb.append_block_param(have, PTR);
        let hl = self.fb.append_block_param(have, PTR);
        if let Some(t) = cache {
            let refresh = self.fb.create_block();
            let (ck, co, cd, cl) = (
                self.fb.use_var(t.kind),
                self.fb.use_var(t.obj),
                self.fb.use_var(t.data),
                self.fb.use_var(t.len),
            );
            let z = self.i32c(0);
            let kset = self.fb.icmp(IntCC::Ne, ck, z);
            let same = self.fb.icmp(IntCC::Eq, gc, co);
            let hit = self.fb.binary(BinaryOp::Band, kset, same);
            self.fb.brif(hit, have, &[ck, cd, cl], refresh, &[]);
            self.fb.seal_block(refresh);
            self.fb.switch_to_block(refresh);
        }
        // An uncached site passes its pc: fed typed arrays often, it asks for a recompile
        // with a cache (exiting before the op).
        let flags = match cache {
            Some(_) => store.is_some() as i64,
            None => store.is_some() as i64 | 2 | (pc as i64) << 2,
        };
        let w = self.i32c(flags);
        let k = self
            .call(Helper::TaView, &[self.frame, obj, w])
            .expect("TaView returns a kind");
        if cache.is_none() {
            let rc = self.i32c(tc::RECOMPILE as i64);
            let hot = self.fb.icmp(IntCC::Eq, k, rc);
            let ex = self.fb.create_block();
            let cont = self.fb.create_block();
            self.fb.brif(hot, ex, &[], cont, &[]);
            self.fb.seal_block(ex);
            self.fb.switch_to_block(ex);
            let st = self.stack.clone();
            self.emit_exit(pc, EXIT_RESUME, &st, st.len(), None);
            self.fb.seal_block(cont);
            self.fb.switch_to_block(cont);
        }
        let data = self.fb.load(PTR_MEM, self.frame, FRAME_TA_DATA);
        let len = self.fb.load(PTR_MEM, self.frame, FRAME_TA_LEN);
        if let Some(t) = cache {
            self.fb.def_var(t.kind, k);
            self.fb.def_var(t.obj, gc);
            self.fb.def_var(t.data, data);
            self.fb.def_var(t.len, len);
        }
        let z = self.i32c(0);
        let none = self.fb.icmp(IntCC::Eq, k, z);
        self.fb.brif(none, slow, &[], have, &[k, data, len]);
        self.fb.seal_block(have);
        self.fb.switch_to_block(have);
        // An integer index in bounds (negative ones wrap above any length).
        let i = self.to_index(key);
        let back = self.fb.convert(ConvOp::FromSint, Type::F64, i);
        let exact = self.fb.fcmp(FloatCC::Eq, back, key);
        let inb = self.fb.icmp(IntCC::Ult, i, hl);
        let ok = self.fb.binary(BinaryOp::Band, exact, inb);
        let go = self.fb.create_block();
        self.fb.brif(ok, go, &[], slow, &[]);
        self.fb.seal_block(go);
        self.fb.switch_to_block(go);
        let done = self.fb.create_block();
        let res = store.is_none().then(|| self.fb.append_block_param(done, Type::F64));
        let blocks: Vec<Block> = (1..tc::END).map(|_| self.fb.create_block()).collect();
        let mut targets = vec![(slow, Vec::new())];
        targets.extend(blocks.iter().map(|&b| (b, Vec::new())));
        self.fb.br_table(hk, &targets, (slow, &[]));
        for (n, &b) in blocks.iter().enumerate() {
            let code = n as u32 + 1;
            self.fb.seal_block(b);
            self.fb.switch_to_block(b);
            let (mk, es) = match code {
                tc::I8 => (MemKind::I32S8, 1),
                tc::U8 | tc::U8C => (MemKind::I32U8, 1),
                tc::I16 => (MemKind::I32S16, 2),
                tc::U16 => (MemKind::I32U16, 2),
                tc::I32 => (MemKind::I32, 4),
                tc::U32 => (MemKind::I64U32, 4),
                tc::F32 => (MemKind::F32, 4),
                _ => (MemKind::F64, 8),
            };
            let at = if es == 1 {
                self.fb.binary(BinaryOp::Iadd, hd, i)
            } else {
                let c = self.fb.iconst(PTR, es);
                let off = self.fb.binary(BinaryOp::Imul, i, c);
                self.fb.binary(BinaryOp::Iadd, hd, off)
            };
            match store {
                None => {
                    let x = self.fb.load(mk, at, 0);
                    let f = match code {
                        tc::F64 => x,
                        tc::F32 => self.fb.convert(ConvOp::Promote, Type::F64, x),
                        _ => self.fb.convert(ConvOp::FromSint, Type::F64, x),
                    };
                    self.fb.jump(done, &[f]);
                }
                Some(n) => {
                    match code {
                        tc::F64 => self.fb.store(MemKind::F64, at, n, 0),
                        tc::F32 => {
                            let x = self.fb.convert(ConvOp::Demote, Type::F32, n);
                            self.fb.store(MemKind::F32, at, x, 0);
                        }
                        tc::U8C => {
                            // ToUint8Clamp: NaN and <= 0 give 0, >= 255 gives 255, else round
                            // half to even.
                            let zf = self.fb.f64const(0.0);
                            let top = self.fb.f64const(255.0);
                            let pos = self.fb.fcmp(FloatCC::Gt, n, zf);
                            let c = self.fb.select(pos, n, zf);
                            let lt = self.fb.fcmp(FloatCC::Lt, c, top);
                            let c = self.fb.select(lt, c, top);
                            let r = self.fb.unary(UnaryOp::Nearest, c);
                            let x = self.fb.convert(ConvOp::ToSintSat, Type::I32, r);
                            self.fb.store(MemKind::I32U8, at, x, 0);
                        }
                        _ => {
                            // ToInt32, then keep the low bits (ToInt8 / ToUint16 / ... wrap).
                            let x = self.to_int32(n);
                            let mk = match es {
                                1 => MemKind::I32U8,
                                2 => MemKind::I32U16,
                                _ => MemKind::I32,
                            };
                            self.fb.store(mk, at, x, 0);
                        }
                    }
                    self.fb.jump(done, &[]);
                }
            }
        }
        self.fb.seal_block(done);
        self.fb.switch_to_block(done);
        Some(match res {
            Some(r) => r,
            None => self.fb.f64const(0.0),
        })
    }

    /// `obj.<names[n]>` through the site's IC (or `length`) with the result at `at`.
    #[allow(clippy::too_many_arguments)]
    fn prop_read(
        &mut self,
        pc: usize,
        obj: V,
        n: u32,
        c: u32,
        drop: Option<usize>,
        at: usize,
        slow: Slow,
        src: Option<Src>,
    ) -> Result<(), String> {
        if let Some(g) = self.plan.dget.get(&pc).cloned() {
            if self.inline_getter(pc, obj, drop, at, slow, &g) {
                return Ok(());
            }
        }
        // A getter called directly (see `call::AccSite`): not on a receiver borrowed from an
        // environment, which could move while it runs.
        if let (Some(a), false) = (
            self.plan.dacc.get(&pc).cloned(),
            src.is_some_and(|s| s.in_env()),
        ) {
            let pre = self.stack.clone();
            let d = pre.len();
            let miss = self.fb.create_block();
            let join = self.fb.create_block();
            layout::accessor_probe(&mut self.fb, obj, &a.shapes, a.slot, a.word as u64, false, miss);
            self.accessor_call(pc, obj, None);
            if let Some(i) = drop {
                self.drop_at(i);
            }
            if at != d {
                let (so, dof) = (self.soff(d), self.soff(at));
                let sp = self.stackp;
                self.copy_value(sp, so, sp, dof);
            }
            self.fb.jump(join, &[]);
            self.fb.seal_block(miss);
            self.fb.switch_to_block(miss);
            self.stack = pre.clone();
            if self.slow_path(pc, slow) {
                self.slow_to_join(pc, at, false, join);
            }
            self.fb.seal_block(join);
            self.fb.switch_to_block(join);
            self.stack = pre[..at].to_vec();
            self.stack.push(Entry::Boxed);
            return Ok(());
        }
        let is_len = &*self.chunk.names[n as usize] == "length";
        let ic = if is_len {
            None
        } else {
            layout::prop_ic(self.chunk, c)
        };
        if !is_len && ic.is_none() {
            return self.slow_only(pc, slow);
        }
        let want = is_len || self.want_num(pc, at);
        let pre = self.stack.clone();
        let miss = self.fb.create_block();
        let join = self.fb.create_block();
        let param = want.then(|| self.fb.append_block_param(join, Type::F64));
        match &ic {
            None => {
                let tracked = src.and_then(|k| self.arr_vars.get(&k).copied());
                if let (Some(a), None) = (tracked, drop) {
                    // The view (and the length next to it) is still valid when nothing that
                    // could have changed the array's length or storage ran since it was taken:
                    // no JS, no write of its binding, no property store (element stores keep
                    // it: the inline path never appends). Loop-invariant in a JS-free loop.
                    let kind = self.fb.use_var(a.kind);
                    let len = self.fb.use_var(a.len);
                    let zero = self.i32c(0);
                    let valid = self.fb.icmp(IntCC::Ne, kind, zero);
                    let fresh = self.fb.create_block();
                    self.fb.brif(valid, join, &[len], fresh, &[]);
                    self.fb.seal_block(fresh);
                    self.fb.switch_to_block(fresh);
                }
                let x = layout::length(&mut self.fb, obj, miss);
                self.seal_current();
                // A tracked array: refresh its element-storage view next to its length.
                if let Some(a) = tracked {
                    let (kind, base, count) = layout::array_view(&mut self.fb, obj);
                    self.fb.def_var(a.kind, kind);
                    self.fb.def_var(a.base, base);
                    self.fb.def_var(a.count, count);
                    self.fb.def_var(a.len, x);
                }
                if let Some(i) = drop {
                    self.drop_at(i);
                }
                self.fb.jump(join, &[x]);
            }
            Some(ic) if want => {
                // A data property holding a non-Number: the slow path re-reads it (no getter
                // can run: the IC only bakes data properties).
                let f = layout::prop_get_num(&mut self.fb, obj, ic, miss);
                self.seal_current();
                if let Some(i) = drop {
                    self.drop_at(i);
                }
                self.fb.jump(join, &[f]);
            }
            Some(ic) => {
                // A refcounted value of a data property (not wanted as a Number): cloned out
                // by `Helper::UnpackClone` into the result entry (which may be the receiver's).
                let refc = (!want && drop.is_none_or(|i| i == at)).then(|| {
                    let b = self.fb.create_block();
                    let w = self.fb.append_block_param(b, Type::I64);
                    (b, w)
                });
                let (tag, payload) = layout::prop_get(&mut self.fb, obj, ic, miss, refc.map(|r| r.0));
                if let Some((b, w)) = refc {
                    let cont = self.fb.create_block();
                    self.fb.jump(cont, &[]);
                    self.fb.seal_block(b);
                    self.fb.switch_to_block(b);
                    let dst = self.sptr(at);
                    // An object (or string) takes its reference inline, before the receiver
                    // in the same entry is released; a BigInt or Symbol goes to the helper.
                    let (is_obj, is_str, payload) = layout::word_counted(&mut self.fb, w);
                    let rc_so = layout::rc_strong_offset();
                    let counted = match rc_so {
                        Some(_) => self.fb.binary(BinaryOp::Bor, is_obj, is_str),
                        None => is_obj,
                    };
                    let inc_b = self.fb.create_block();
                    let call_b = self.fb.create_block();
                    self.fb.brif(counted, inc_b, &[], call_b, &[]);
                    self.fb.seal_block(inc_b);
                    self.fb.seal_block(call_b);
                    self.fb.switch_to_block(inc_b);
                    let so = rc_so.unwrap_or(crate::value::GC_STRONG_OFFSET as i32);
                    let strong = self.fb.load(PTR_MEM, payload, so);
                    let one = self.ptrc(1);
                    let s1 = self.fb.binary(BinaryOp::Iadd, strong, one);
                    self.fb.store(PTR_MEM, payload, s1, so);
                    if drop.is_some() {
                        self.drop_at(at);
                    }
                    let objt = self.i32c(TAG_OBJ as i64);
                    let strt = self.i32c(TAG_STR as i64);
                    let tag = self.fb.select(is_obj, objt, strt);
                    self.fb.store(MemKind::I32U8, dst, tag, 0);
                    self.fb.store(MemKind::I64, dst, payload, VALUE_PAYLOAD);
                    self.fb.jump(join, &[]);
                    self.fb.switch_to_block(call_b);
                    let cv = self.i32c(drop.is_some() as i64);
                    self.call(Helper::UnpackClone, &[dst, w, cv]);
                    self.fb.jump(join, &[]);
                    self.fb.seal_block(cont);
                    self.fb.switch_to_block(cont);
                }
                self.seal_current();
                if want {
                    // A data property holding a non-Number: the slow path re-reads it (no
                    // getter can run: the IC only bakes data properties).
                    let four = self.i32c(TAG_NUM as i64);
                    let is = self.fb.icmp(IntCC::Eq, tag, four);
                    let ok = self.fb.create_block();
                    self.fb.brif(is, ok, &[], miss, &[]);
                    self.fb.seal_block(ok);
                    self.fb.switch_to_block(ok);
                    if let Some(i) = drop {
                        self.drop_at(i);
                    }
                    let f = self.fb.convert(ConvOp::Bitcast, Type::F64, payload);
                    self.fb.jump(join, &[f]);
                } else {
                    if let Some(i) = drop {
                        self.drop_at(i);
                    }
                    let o = self.soff(at);
                    self.fb.store(MemKind::I32U8, self.stackp, tag, o);
                    let byte = self.fb.convert(ConvOp::Wrap, Type::I32, payload);
                    self.fb
                        .store(MemKind::I32U8, self.stackp, byte, o + VALUE_BOOL);
                    self.fb
                        .store(MemKind::I64, self.stackp, payload, o + VALUE_PAYLOAD);
                    self.fb.jump(join, &[]);
                }
            }
        }
        self.fb.seal_block(miss);
        self.fb.switch_to_block(miss);
        if is_len {
            // No view on the slow path.
            if let Some(a) = src.and_then(|k| self.arr_vars.get(&k).copied()) {
                self.reset_vars(&[(a.kind, Type::I32)]);
            }
            // A typed array's `length` ([`Helper::TaLength`]), cached per site until JS runs
            // (only JS can resize or detach its buffer or give it an own `length`); the cached
            // handle is trusted only with `ic_plain` clear, as in `ta_access`.
            {
                let cache = self.ta_len_vars.get(&pc).copied();
                let slow_b = self.fb.create_block();
                let gc = layout::side_table_object(&mut self.fb, obj, slow_b);
                self.seal_current();
                let hit = self.fb.create_block();
                let hl = self.fb.append_block_param(hit, Type::F64);
                if let Some(t) = cache {
                    let refresh = self.fb.create_block();
                    let (ok, co, cl) = (
                        self.fb.use_var(t.ok),
                        self.fb.use_var(t.obj),
                        self.fb.use_var(t.len),
                    );
                    let same = self.fb.icmp(IntCC::Eq, gc, co);
                    let good = self.fb.binary(BinaryOp::Band, ok, same);
                    self.fb.brif(good, hit, &[cl], refresh, &[]);
                    self.fb.seal_block(refresh);
                    self.fb.switch_to_block(refresh);
                }
                let n = self
                    .call(Helper::TaLength, &[self.frame, obj])
                    .expect("TaLength returns a length");
                let zf = self.fb.f64const(0.0);
                let got = self.fb.fcmp(FloatCC::Ge, n, zf);
                if let Some(t) = cache {
                    self.fb.def_var(t.ok, got);
                    self.fb.def_var(t.obj, gc);
                    self.fb.def_var(t.len, n);
                }
                self.fb.brif(got, hit, &[n], slow_b, &[]);
                self.fb.seal_block(hit);
                self.fb.switch_to_block(hit);
                if let Some(i) = drop {
                    self.drop_at(i);
                }
                self.fb.jump(join, &[hl]);
                self.fb.seal_block(slow_b);
                self.fb.switch_to_block(slow_b);
            }
        }
        self.stack = pre.clone();
        if self.slow_path(pc, slow) {
            self.slow_to_join(pc, at, want, join);
        }
        self.fb.seal_block(join);
        self.fb.switch_to_block(join);
        self.stack = pre[..at].to_vec();
        self.stack.push(match param {
            Some(p) => Entry::Num(p),
            None => Entry::Boxed,
        });
        Ok(())
    }
}
