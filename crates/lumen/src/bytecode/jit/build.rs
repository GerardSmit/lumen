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
//! # Bounds-check elimination hook
//!
//! Every element read goes through [`Tr::elem_read`], which a later pass can specialize for an
//! index proven in bounds (`i < a.length` loop tests over an unmodified `a`) by swapping the
//! [`layout::elem_get_num`] call for an unchecked load.

use super::helpers::{self, Helper};
use super::layout;
use super::*;
use crate::bytecode::{ArithKind, CmpKind, Op, UpdKind};
use lumen_codegen::{
    BinaryOp, Block, ConvOp, FloatCC, FuncRef, Function, FunctionBuilder, IntCC, MemKind,
    Signature, Type, UnaryOp, Value as V, Variable,
};

/// A translated region.
pub(crate) struct Built {
    /// `(PTR frame) -> I64 exit word`.
    pub func: lumen_codegen::Function,
    /// Entries `frame.stack` must provide.
    pub max_stack: usize,
    /// The representation chosen for each slot (`chunk.n_slots` long); `Num`/`Bool` slots are
    /// guarded at entry.
    pub kinds: Vec<Kind>,
}

/// Translate the loop `[header, backedge]` of `chunk`, specializing on the current `slots`.
/// `Err` (with a reason for `LUMEN_TIER_LOG`) when the region uses something unsupported.
pub(crate) fn build(
    chunk: &Chunk,
    header: usize,
    backedge: usize,
    slots: &[Value],
) -> Result<Built, String> {
    let an = analyze(chunk, header, backedge)?;

    let n_slots = chunk.n_slots;
    let mut kinds = vec![Kind::Boxed; n_slots];
    for s in 0..n_slots {
        if an.touched[s] {
            kinds[s] = slots.get(s).map(Kind::of).unwrap_or(Kind::Boxed);
        }
    }

    let mut func = Function::new(
        format!("loop_{header}_{backedge}"),
        Signature::new(vec![PTR], vec![Type::I64]),
    );
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
            helpers: vec![None; helpers::ALL.len()],
            leaders,
            stack: Vec::new(),
            max_stack: 1,
        };
        tr.entry(&an);
        tr.body(&an)?;
        let Tr {
            fb,
            max_stack,
            kinds: k,
            ..
        } = tr;
        fb.finish();
        kinds = k;
        max_stack
    };
    Ok(Built {
        func,
        max_stack,
        kinds,
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
}

/// Whether control can continue to `pc + 1`, and the jump target.
fn successors(op: &Op) -> (bool, Option<usize>) {
    let falls = !matches!(
        op,
        Op::Jump(_) | Op::Return | Op::ReturnUndef | Op::Throw | Op::IterAbortL(_)
    );
    (falls, crate::jit_ir::jump_target(op))
}

/// The slots `op` reads or writes.
fn op_slots(op: &Op) -> Vec<u16> {
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
        Op::IterStepL(a, b) | Op::JumpIfNotCmpLL(_, a, b, _) => vec![a, b],
        Op::JumpIfNotCmpLK(_, a, ..) => vec![a],
        Op::ArithLL(_, d, a, b) => vec![d, a, b],
        Op::ArithLK(_, d, a, _) => vec![d, a],
        Op::ForInStepL(a, b, c) => vec![a, b, c],
        _ => Vec::new(),
    }
}

fn analyze(chunk: &Chunk, header: usize, backedge: usize) -> Result<Analysis, String> {
    let ops = &chunk.ops;
    if header > backedge || backedge >= ops.len() {
        return Err(format!("bad region {header}..={backedge}"));
    }
    if !matches!(ops[backedge], Op::Jump(t) if t as usize == header) {
        return Err(format!(
            "op at {backedge} is not the backward jump to {header}"
        ));
    }
    let inr = |pc: usize| pc >= header && pc <= backedge;
    let n = backedge - header + 1;
    let mut depth: Vec<Option<usize>> = vec![None; n];
    depth[0] = Some(0);
    let mut work = vec![header];
    while let Some(pc) = work.pop() {
        let op = &ops[pc];
        match op {
            Op::PushHandler(_) | Op::PopHandler | Op::Await => {
                return Err(format!("unsupported op {op:?} at {pc}"));
            }
            Op::ForInStepL(..) | Op::IterStepL(..) | Op::IterCloseL(_) | Op::IterAbortL(_) => {
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
        let (falls, target) = successors(op);
        for q in falls.then_some(pc + 1).into_iter().chain(target) {
            if !inr(q) {
                continue;
            }
            match depth[q - header] {
                None => {
                    depth[q - header] = Some(after);
                    work.push(q);
                }
                Some(e) if e != after => {
                    return Err(format!("inconsistent stack depth at {q}"));
                }
                Some(_) => {}
            }
        }
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
    Ok(Analysis {
        depth,
        leader,
        preds,
        back,
        touched,
        tdz,
    })
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
}

/// An IR block starting a bytecode basic block.
struct Leader {
    block: Block,
    depth: usize,
    /// The abstract stack on entry, once known (see the module docs on merges).
    state: Option<Vec<Entry>>,
    /// Edges emitted so far.
    edges: u32,
    /// Backward edges not yet emitted; the block is sealed when it drops to zero.
    back_left: u32,
    has_back: bool,
}

/// The slow path of an op with an inline fast path.
#[derive(Clone, Copy)]
enum Slow {
    /// Run the op itself through [`Helper::Generic`].
    Generic,
    /// `GetElemLocal(s)`: clone the slot beneath the key and run the `GetElem` at `run`.
    ElemLocal { s: u16, run: usize },
    /// `GetPropLocal(s, ..)`: load the slot (TDZ-checked) and run the `GetProp` at `run`.
    PropLocal { s: u16, run: usize },
    /// `SetElemLocal[Drop](s)`: clone the slot beneath key and value and run the
    /// `SetElem[Drop]` at `run`.
    SetElemLocal { s: u16, run: usize },
    /// `SetPropLocalDrop(s, ..)`: load the slot beneath the value and run the `SetPropDrop` at
    /// `run`.
    SetPropLocal { s: u16, run: usize },
    /// No equivalent op to borrow: exit before the op.
    Exit,
}

/// An operand of a fused local op.
#[derive(Clone, Copy)]
enum Opnd {
    Slot(u16),
    Const(u32),
}

struct Tr<'a, 'f> {
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
    helpers: Vec<Option<FuncRef>>,
    leaders: Vec<Option<Leader>>,
    stack: Vec<Entry>,
    max_stack: usize,
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
        }
    }

    /// Store every unboxed entry to memory, so the whole stack is valid `Value`s (what
    /// [`Helper::Generic`] reads). The entries stay unboxed: memory now holds trivial copies.
    fn box_all(&mut self) {
        for i in 0..self.stack.len() {
            let e = self.stack[i];
            self.store_entry(i, e);
        }
    }

    /// Move the owned value at `from` to `to` bitwise (`to` holds a trivially droppable value;
    /// `from` must be overwritten or treated as dead afterwards).
    fn move_entry(&mut self, from: usize, to: usize) {
        let (fo, to_) = (self.soff(from), self.soff(to));
        let a = self.fb.load(MemKind::I64, self.stackp, fo);
        let b = self.fb.load(MemKind::I64, self.stackp, fo + 8);
        self.fb.store(MemKind::I64, self.stackp, a, to_);
        self.fb.store(MemKind::I64, self.stackp, b, to_ + 8);
    }

    fn clone_slot_to(&mut self, s: usize, d: usize) {
        let dst = self.sptr(d);
        let src = self.slot_ptr(s);
        self.call(Helper::Clone, &[dst, src]);
    }

    /// Drop the Boxed value at `frame.stack[d]` (a helper call only for refcounted tags).
    fn drop_at(&mut self, d: usize) {
        let tag = self.stack_tag(d);
        let four = self.i32c(TAG_NUM as i64);
        let big = self.fb.icmp(IntCC::Ugt, tag, four);
        let call_b = self.fb.create_block();
        let cont = self.fb.create_block();
        self.fb.brif(big, call_b, &[], cont, &[]);
        self.fb.seal_block(call_b);
        self.fb.switch_to_block(call_b);
        let p = self.sptr(d);
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
        for (i, &e) in stack.iter().enumerate() {
            self.store_entry(i, e);
        }
        self.write_back(store.map(|s| s.0));
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
    fn exit_now(&mut self, pc: usize) {
        let st = self.stack.clone();
        self.emit_exit(pc, EXIT_RESUME, &st, st.len(), None);
    }

    /// Exit before the op at `pc` when SSA slot `s` is in its TDZ (the interpreter throws).
    fn tdz_guard(&mut self, s: usize, pc: usize) {
        if let Some(flag) = self.tdz[s] {
            let f = self.fb.use_var(flag);
            let st = self.stack.clone();
            self.exit_if(f, pc, EXIT_RESUME, &st, st.len());
        }
    }

    fn clear_tdz(&mut self, s: usize) {
        if let Some(flag) = self.tdz[s] {
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
        let b = self.fb.load(MemKind::I64, self.frame, FRAME_BUDGET);
        let one = self.fb.iconst(Type::I64, 1);
        let b1 = self.fb.binary(BinaryOp::Isub, b, one);
        self.fb.store(MemKind::I64, self.frame, b1, FRAME_BUDGET);
        let zero = self.fb.iconst(Type::I64, 0);
        let low = self.fb.icmp(IntCC::Sle, b1, zero);
        let sp = self.fb.create_block();
        let cont = self.fb.create_block();
        self.fb.brif(low, sp, &[], cont, &[]);
        self.fb.seal_block(sp);
        self.fb.switch_to_block(sp);
        let st = self.call_status(Helper::Safepoint, &[self.frame]);
        let stack = self.stack.clone();
        self.throw_if(st, to, &stack, stack.len());
        self.fb.jump(cont, &[]);
        self.fb.seal_block(cont);
        self.fb.switch_to_block(cont);
    }

    // ---- driver ------------------------------------------------------------------------------

    /// The entry block: guard and load the SSA locals, then enter the header.
    fn entry(&mut self, an: &Analysis) {
        let fail = self.fb.create_block();
        for s in 0..self.kinds.len() {
            let (ty, tag) = match self.kinds[s] {
                Kind::Num => (Type::F64, TAG_NUM),
                Kind::Bool => (Type::I32, TAG_BOOL),
                Kind::Boxed => continue,
            };
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
            let var = self.fb.declare_var(ty);
            self.fb.def_var(var, v);
            self.vars[s] = Some(var);
            if an.tdz[s] {
                let flag = self.fb.declare_var(Type::I32);
                let z = self.i32c(0);
                self.fb.def_var(flag, z);
                self.tdz[s] = Some(flag);
            }
        }
        let header_block = self
            .edge(false, self.header)
            .expect("the header is a leader");
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
            if dead || self.fb.is_filled() || an.depth[i].is_none() {
                continue;
            }
            debug_assert_eq!(
                Some(self.stack.len()),
                an.depth[i],
                "abstract depth at {pc}"
            );
            self.op(pc)?;
        }
        Ok(())
    }

    // ---- speculation -------------------------------------------------------------------------

    /// Whether the value the op at `pc` leaves at stack index `at` is consumed by an op that
    /// wants a Number (so the producer speculates Num and exits after itself on anything else),
    /// looking ahead within the basic block.
    fn want_num(&self, pc: usize, at: usize) -> bool {
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

    /// Find an op to run through [`Helper::Generic`] in place of a slot-reading op: `Generic`
    /// runs the single op at a pc, and these ops' semantics do not depend on where they are.
    fn borrow(&self, want: impl Fn(&Op) -> bool) -> Option<usize> {
        self.ops.iter().position(want)
    }

    // ---- generic paths -----------------------------------------------------------------------

    /// Call [`Helper::Generic`] on `run` with `depth` Boxed entries in memory; a throw exits at
    /// `pc` with all `depth` entries (the helper wrote back what the op left).
    fn call_generic(&mut self, run: usize, depth: usize, pc: usize) {
        let (pops, pushes) = self.chunk.jit_stack_effect(run).unwrap_or((0, 0));
        self.max_stack = self
            .max_stack
            .max(depth)
            .max(depth.saturating_sub(pops) + pushes);
        let r = self.i32c(run as i64);
        let dv = self.i32c(depth as i64);
        let st = self.call_status(Helper::Generic, &[self.frame, r, dv]);
        self.throw_if(st, pc, &[], depth);
    }

    /// Run the op at `pc` through [`Helper::Generic`].
    fn generic(&mut self, pc: usize) {
        let (pops, pushes) = self.chunk.jit_stack_effect(pc).unwrap_or((0, 0));
        let d = self.stack.len();
        self.box_all();
        self.call_generic(pc, d, pc);
        self.stack.truncate(d - pops);
        self.stack.extend(std::iter::repeat_n(Entry::Boxed, pushes));
    }

    /// Emit `slow` for the op at `pc` from the current (pre-op) stack. Returns false when it
    /// exited; otherwise the stack is the op's post state with Boxed results.
    fn slow_path(&mut self, pc: usize, slow: Slow) -> bool {
        let (pops, pushes) = self.chunk.jit_stack_effect(pc).unwrap_or((0, 0));
        let d = self.stack.len();
        match slow {
            Slow::Exit => {
                self.exit_now(pc);
                return false;
            }
            Slow::Generic => {
                self.generic(pc);
                return true;
            }
            Slow::ElemLocal { s, run } => {
                self.box_all();
                self.move_entry(d - 1, d);
                self.clone_slot_to(s as usize, d - 1);
                self.call_generic(run, d + 1, pc);
            }
            Slow::PropLocal { s, run } => {
                self.box_all();
                let p = self.sptr(d);
                let sc = self.i32c(s as i64);
                let st = self.call_status(Helper::LoadLocal, &[self.frame, sc, p]);
                self.throw_if(st, pc, &[], d);
                self.call_generic(run, d + 1, pc);
            }
            Slow::SetElemLocal { s, run } => {
                self.box_all();
                self.move_entry(d - 1, d);
                self.move_entry(d - 2, d - 1);
                self.clone_slot_to(s as usize, d - 2);
                self.call_generic(run, d + 1, pc);
            }
            Slow::SetPropLocal { s, run } => {
                self.box_all();
                self.move_entry(d - 1, d);
                let p = self.sptr(d - 1);
                let sc = self.i32c(s as i64);
                let st = self.call_status(Helper::LoadLocal, &[self.frame, sc, p]);
                // The value moved up to `d` is live; `d - 1` is still trivially droppable.
                self.throw_if(st, pc, &[], d + 1);
                self.call_generic(run, d + 1, pc);
            }
        }
        self.stack.truncate(d - pops);
        self.stack.extend(std::iter::repeat_n(Entry::Boxed, pushes));
        true
    }

    /// An op with no fast path: its slow path, unless that is an unconditional exit — native
    /// code that leaves at an op on every execution would only add entry and exit overhead.
    fn slow_only(&mut self, pc: usize, slow: Slow) -> Result<(), String> {
        if matches!(slow, Slow::Exit) {
            return Err(format!("{:?} at {pc} has no native path", self.ops[pc]));
        }
        self.slow_path(pc, slow);
        Ok(())
    }

    // ---- numbers -----------------------------------------------------------------------------

    /// JS ToInt32 of an F64, branch-free. Below 2^63 in magnitude a saturating truncation to
    /// I64 is exact, and its low 32 bits are the result; at or above, the value is integral and
    /// `x - 2^32 * floor(x / 2^32)` (exact) brings it into [0, 2^32) first. NaN and ±Infinity
    /// come out of that reduction as NaN, which saturates to 0 — ToInt32's answer.
    fn to_int32(&mut self, x: V) -> V {
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

    /// `js_mod` through [`Helper::Binary`] (which computes Number ⊕ Number exactly like
    /// `run_vm`; never throws for two Numbers), using scratch entries above the stack.
    fn num_mod(&mut self, x: V, y: V) -> Result<V, String> {
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
        let d = self.stack.len();
        match op {
            Op::Const(k) => self.push_const(k as usize),
            Op::Undef => {
                self.set_stack_tag(d, TAG_UNDEFINED);
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
                } else {
                    self.set_stack_tag(d, TAG_EMPTY);
                    let p = self.sptr(d);
                    let sc = self.i32c(s as i64);
                    self.call(Helper::StoreLocal, &[self.frame, sc, p]);
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
                    Entry::Boxed => None,
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
            // Let the interpreter throw (and find its handler).
            Op::Throw => self.exit_now(pc),
            Op::GetElem => match (self.stack[d - 2], self.stack[d - 1]) {
                (Entry::Boxed, Entry::Num(key)) => {
                    let want = self.want_num(pc, d - 2);
                    let obj = self.sptr(d - 2);
                    self.elem_read(pc, obj, key, Some(d - 2), d - 2, want, Slow::Generic);
                }
                _ => self.generic(pc),
            },
            Op::GetElemLocal(s) => {
                if self.kinds[s as usize] != Kind::Boxed {
                    self.exit_now(pc);
                    return Ok(());
                }
                let slow = match self.borrow(|o| matches!(o, Op::GetElem)) {
                    Some(run) => Slow::ElemLocal { s, run },
                    None => Slow::Exit,
                };
                match self.stack[d - 1] {
                    Entry::Num(key) => {
                        let want = self.want_num(pc, d - 1);
                        let obj = self.slot_ptr(s as usize);
                        self.elem_read(pc, obj, key, None, d - 1, want, slow);
                    }
                    _ => self.slow_only(pc, slow)?,
                }
            }
            Op::SetElem | Op::SetElemDrop => {
                let keep = matches!(op, Op::SetElem);
                match (self.stack[d - 3], self.stack[d - 2], self.stack[d - 1]) {
                    (Entry::Boxed, Entry::Num(key), v @ (Entry::Num(_) | Entry::Boxed)) => {
                        let obj = self.sptr(d - 3);
                        self.elem_write(pc, obj, key, v, Some(d - 3), keep, d - 3, Slow::Generic);
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
                let slow = match self.borrow(|o| {
                    if keep {
                        matches!(o, Op::SetElem)
                    } else {
                        matches!(o, Op::SetElemDrop)
                    }
                }) {
                    Some(run) => Slow::SetElemLocal { s, run },
                    None => Slow::Exit,
                };
                match (self.stack[d - 2], self.stack[d - 1]) {
                    (Entry::Num(key), v @ (Entry::Num(_) | Entry::Boxed)) => {
                        let obj = self.slot_ptr(s as usize);
                        self.elem_write(pc, obj, key, v, None, keep, d - 2, slow);
                    }
                    _ => self.slow_only(pc, slow)?,
                }
            }
            Op::GetProp(n, c) => match self.stack[d - 1] {
                Entry::Boxed => {
                    let obj = self.sptr(d - 1);
                    self.prop_read(pc, obj, n, c, Some(d - 1), d - 1, Slow::Generic)?;
                }
                _ => self.generic(pc),
            },
            Op::GetPropThis(n, c) => {
                let obj = self.fb.load(PTR_MEM, self.frame, FRAME_THIS);
                self.prop_read(pc, obj, n, c, None, d, Slow::Generic)?;
            }
            Op::GetPropLocal(s, n, c) => {
                if self.kinds[s as usize] != Kind::Boxed {
                    self.exit_now(pc);
                    return Ok(());
                }
                let slow = match self.borrow(|o| matches!(o, Op::GetProp(m, _) if *m == n)) {
                    Some(run) => Slow::PropLocal { s, run },
                    None => Slow::Exit,
                };
                let obj = self.slot_ptr(s as usize);
                self.prop_read(pc, obj, n, c, None, d, slow)?;
            }
            Op::SetPropLocalDrop(s, n, _) => {
                if self.kinds[s as usize] != Kind::Boxed {
                    self.exit_now(pc);
                    return Ok(());
                }
                let slow = match self.borrow(|o| matches!(o, Op::SetPropDrop(m, _) if *m == n)) {
                    Some(run) => Slow::SetPropLocal { s, run },
                    None => Slow::Exit,
                };
                self.slow_only(pc, slow)?;
            }
            Op::ToPropKeyLocal(s) => {
                // Num and Str keys pass through untouched; anything else needs the coercion
                // (and the base check): the interpreter does it.
                if self.kinds[s as usize] != Kind::Boxed {
                    self.exit_now(pc);
                    return Ok(());
                }
                match self.stack[d - 1] {
                    Entry::Num(_) => {}
                    Entry::Bool(_) => self.exit_now(pc),
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
            self.call(Helper::Clone, &[dst, src]);
        }
        self.stack.push(e);
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
            let p = self.sptr(d);
            let sc = self.i32c(s as i64);
            let st = self.call_status(Helper::LoadLocal, &[self.frame, sc, p]);
            let snap = self.stack.clone();
            self.throw_if(st, pc, &snap, d);
            self.stack.push(Entry::Boxed);
        }
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
                let p = self.sptr(d - 1);
                let sc = self.i32c(s as i64);
                self.call(Helper::StoreLocal, &[self.frame, sc, p]);
            }
        }
        self.clear_tdz(s);
        self.stack.pop();
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
        let d = self.stack.len();
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
        } else {
            let d = self.stack.len();
            self.opnds_to_scratch(pc, a, b);
            let code = self.i32c(bin_code(&cmp_op(kind))? as i64);
            let (pa, pb, pr) = (self.sptr(d), self.sptr(d + 1), self.sptr(d + 2));
            let st = self.call_status(Helper::Binary, &[self.frame, code, pa, pb, pr]);
            let snap = self.stack.clone();
            self.throw_if(st, pc, &snap, d);
            self.call_status(Helper::ToBoolean, &[self.frame, pr])
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
            (Entry::Num(_) | Entry::Boxed, Entry::Num(_) | Entry::Boxed) => {
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
        self.box_all();
        let code = self.i32c(bin_code(&cmp_op(kind))? as i64);
        let (pa, pb, pr) = (self.sptr(d - 2), self.sptr(d - 1), self.sptr(d));
        let st = self.call_status(Helper::Binary, &[self.frame, code, pa, pb, pr]);
        let snap = self.stack[..d - 2].to_vec();
        self.throw_if(st, pc, &snap, d - 2);
        Ok(self.call_status(Helper::ToBoolean, &[self.frame, pr]))
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
            // A Boxed operand that holds a Number at run time still takes the inline path; the
            // result then lives in memory (Boxed) so both paths agree on the entry's kind.
            (Entry::Num(_) | Entry::Boxed, Entry::Num(_) | Entry::Boxed)
                if !matches!(op, Op::Mod) =>
            {
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
    ) {
        let pre = self.stack.clone();
        let miss = self.fb.create_block();
        let join = self.fb.create_block();
        let param = want.then(|| self.fb.append_block_param(join, Type::F64));
        let x = layout::elem_get_num(&mut self.fb, obj, key, miss);
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
        self.fb.seal_block(miss);
        self.fb.switch_to_block(miss);
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
    ) {
        let d = self.stack.len();
        let pre = self.stack.clone();
        let miss = self.fb.create_block();
        let join = self.fb.create_block();
        let vf = self.num_or_slow(v, d - 1, miss);
        layout::elem_set_num(&mut self.fb, obj, key, vf, miss);
        self.seal_current();
        if let Some(i) = drop {
            self.drop_at(i);
        }
        if keep && v == Entry::Boxed {
            self.store_entry(base, Entry::Num(vf));
        }
        self.fb.jump(join, &[]);
        self.fb.seal_block(miss);
        self.fb.switch_to_block(miss);
        self.stack = pre.clone();
        if self.slow_path(pc, slow) {
            self.fb.jump(join, &[]);
        }
        self.fb.seal_block(join);
        self.fb.switch_to_block(join);
        self.stack = pre[..base].to_vec();
        if keep {
            // An unboxed value stays valid: the slow path's result is a copy of the same Number.
            self.stack.push(v);
        }
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
    ) -> Result<(), String> {
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
                let x = layout::length(&mut self.fb, obj, miss);
                self.seal_current();
                if let Some(i) = drop {
                    self.drop_at(i);
                }
                self.fb.jump(join, &[x]);
            }
            Some(ic) => {
                let (tag, payload) = layout::prop_get(&mut self.fb, obj, ic, miss);
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
