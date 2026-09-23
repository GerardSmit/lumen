//! One function body: SSA values → wasm locals, CFG → structured control flow.
//!
//! **Control flow** follows Ramsey, "Beyond Relooper: Recursive Translation of Unstructured
//! Control Flow to Structured Control Flow" (ICFP 2022), as used by LLVM's and Cranelift's
//! wasm backends. Walking the dominator tree, each block `x` is emitted as
//!
//! ```text
//! loop?                    ; only when x is a loop header (target of a back edge)
//!   block                  ; one per merge child y_n … y_1 of x, outermost = latest in RPO
//!     …block
//!         <x's code>       ; ends in a branch
//!     end  <tree of y_1>
//!   end  <tree of y_n>
//! end?
//! ```
//!
//! where a *merge node* has two or more forward in-edges. A branch from `x` to `t` then becomes
//! `br` to the loop around `t` (back edge), `br` to the end of the `block` that `t` follows
//! (merge node), or — when `t` has a single forward in-edge and is therefore a dominator-tree
//! child of `x` — `t`'s tree emitted inline. Label depths come from a stack of the enclosing
//! constructs. Every emitted path ends in a branch, `return` or `unreachable`, so no construct
//! ever falls through except a `block` into the tree it is followed by; an `unreachable` after
//! each `if`/`loop` tells the validator so.
//!
//! An irreducible CFG (a retreating edge whose target does not dominate its source) is emitted
//! as a dispatch loop instead: a `loop` around a `br_table` over a label local selects the
//! block, and every edge sets the label and re-enters the loop.
//!
//! **Values.** Constants are rematerialized at each use; a pure single-result instruction with
//! one use in its own block is emitted as a nested expression at that use (moving a pure
//! operation later is safe: its operands' locals cannot change before the block's terminator).
//! Every other value lives in its own local; the entry block's parameters are the wasm
//! parameters. Branch arguments are parallel copies: every source is pushed on the operand
//! stack first, then the target's parameters are popped in reverse, so no temporaries are needed
//! when a parameter is also a source.

use super::encode::*;
use super::{Config, Types};
use crate::cfg::Cfg;
use crate::ir::*;

/// Where a value lives.
#[derive(Clone, Copy, PartialEq)]
enum Home {
    Local(u32),
    /// A constant, re-emitted at each use.
    Remat,
    /// Emitted as an expression at its single use.
    Inline,
    /// Never read.
    Unused,
}

/// An enclosing structured construct, for label depths.
#[derive(Clone, Copy, PartialEq)]
enum Frame {
    /// A `block` whose end is followed by the given block's code.
    Block(Block),
    /// The `loop` headed by the given block.
    Loop(Block),
    /// The dispatch loop of an irreducible function.
    Dispatch,
    /// An `if` or a `br_table` trampoline: never a branch target by name.
    Other,
}

struct Emitter<'a> {
    f: &'a Function,
    cfg: Cfg,
    config: &'a Config,
    types: &'a mut Types,
    resolve: &'a dyn Fn(u32) -> Option<u32>,
    home: Vec<Home>,
    /// Forward in-edges per block, counted per branch slot.
    fwd_in: Vec<u32>,
    loop_header: Vec<bool>,
    /// Dominator-tree children that are merge nodes, latest in RPO first.
    merge_children: Vec<Vec<Block>>,
    /// The label local of the dispatch loop, for an irreducible CFG.
    label: Option<u32>,
    stack: Vec<Frame>,
    code: Vec<u8>,
}

/// Compile one function to a code-section entry (locals and expression, without the size).
pub(super) fn compile(
    f: &Function,
    config: &Config,
    types: &mut Types,
    resolve: &dyn Fn(u32) -> Option<u32>,
) -> Result<Vec<u8>, String> {
    let cfg = Cfg::new(f);
    let n = f.blocks.len();

    // Use counts, and the block of each value's (last seen) use.
    let mut uses = vec![0u32; f.values.len()];
    let mut use_block = vec![None; f.values.len()];
    for &b in &cfg.rpo {
        for &inst in &f.blocks[b.index()].insts {
            f.inst(inst).for_each_arg(|v| {
                uses[v.index()] += 1;
                use_block[v.index()] = Some(b);
            });
        }
    }

    // Homes and locals: wasm parameters first, then one local per stored value.
    let mut home = vec![Home::Unused; f.values.len()];
    let entry = f.entry();
    for (i, &p) in f.blocks[entry.index()].params.iter().enumerate() {
        home[p.index()] = Home::Local(i as u32);
    }
    let mut locals: Vec<Type> = Vec::new();
    let mut next_local = f.sig.params.len() as u32;
    let mut new_local = |ty: Type, locals: &mut Vec<Type>| {
        locals.push(ty);
        next_local += 1;
        Home::Local(next_local - 1)
    };
    for &b in &cfg.rpo {
        if b != entry {
            for &p in &f.blocks[b.index()].params {
                if uses[p.index()] > 0 {
                    home[p.index()] = new_local(f.value_type(p), &mut locals);
                }
            }
        }
        for &inst in &f.blocks[b.index()].insts {
            let data = f.inst(inst);
            let results = f.results(inst);
            for &r in results {
                let u = uses[r.index()];
                home[r.index()] = if matches!(
                    data,
                    InstData::Iconst { .. } | InstData::F32const { .. } | InstData::F64const { .. }
                ) {
                    Home::Remat
                } else if u == 0 {
                    Home::Unused
                } else if data.is_pure() && results.len() == 1 && u == 1 && use_block[r.index()] == Some(b) {
                    Home::Inline
                } else {
                    new_local(f.value_type(r), &mut locals)
                };
            }
        }
    }

    // Loop headers, merge nodes, reducibility.
    let rpo_of = |b: Block| cfg.rpo_index[b.index()];
    let mut fwd_in = vec![0u32; n];
    let mut loop_header = vec![false; n];
    let mut reducible = true;
    for &b in &cfg.rpo {
        for t in f.successors(b) {
            if rpo_of(t) > rpo_of(b) {
                fwd_in[t.index()] += 1;
            } else {
                loop_header[t.index()] = true;
                reducible &= cfg.dominates(t, b);
            }
        }
    }
    let mut merge_children = cfg.dom_children();
    for ch in &mut merge_children {
        ch.retain(|c| fwd_in[c.index()] >= 2);
        ch.sort_by_key(|&c| std::cmp::Reverse(rpo_of(c)));
    }
    let label = (!reducible).then(|| match new_local(Type::I32, &mut locals) {
        Home::Local(l) => l,
        _ => unreachable!(),
    });

    let mut e = Emitter {
        f,
        cfg,
        config,
        types,
        resolve,
        home,
        fwd_in,
        loop_header,
        merge_children,
        label,
        stack: Vec::new(),
        code: Vec::new(),
    };
    if reducible {
        e.tree(entry)?;
    } else {
        e.dispatch()?;
    }

    // Locals, run-length encoded by type, then the expression.
    let mut out = Vec::new();
    let mut runs: Vec<(u32, Type)> = Vec::new();
    for &t in &locals {
        match runs.last_mut() {
            Some((c, rt)) if *rt == t => *c += 1,
            _ => runs.push((1, t)),
        }
    }
    vec(&mut out, &runs, |o, &(c, t)| {
        uleb(o, c as u64);
        o.push(valtype(t));
    });
    out.extend_from_slice(&e.code);
    out.push(0x0b);
    Ok(out)
}

impl Emitter<'_> {
    fn op(&mut self, b: u8) {
        self.code.push(b);
    }
    fn u(&mut self, v: u64) {
        uleb(&mut self.code, v);
    }
    fn open(&mut self, opcode: u8, frame: Frame) {
        self.code.extend_from_slice(&[opcode, 0x40]); // empty block type
        self.stack.push(frame);
    }
    fn close(&mut self) {
        self.stack.pop();
        self.op(0x0b);
    }

    /// Label depth of the innermost `frame` on the stack.
    fn depth(&self, frame: Frame) -> Result<u32, String> {
        let pos = self.stack.iter().rposition(|&f| f == frame);
        let pos = pos.ok_or("wasm: branch target not in scope")?;
        Ok((self.stack.len() - 1 - pos) as u32)
    }

    fn br(&mut self, frame: Frame) -> Result<(), String> {
        let d = self.depth(frame)?;
        self.op(0x0c);
        self.u(d as u64);
        Ok(())
    }

    // ---- control flow ---------------------------------------------------------------------------

    /// The dominator subtree rooted at `x`.
    fn tree(&mut self, x: Block) -> Result<(), String> {
        if self.loop_header[x.index()] {
            self.open(0x03, Frame::Loop(x));
            self.node_within(x, 0)?;
            self.close();
            self.op(0x00);
            Ok(())
        } else {
            self.node_within(x, 0)
        }
    }

    /// `x`'s code wrapped in blocks for its merge children from index `i` on.
    fn node_within(&mut self, x: Block, i: usize) -> Result<(), String> {
        let Some(&y) = self.merge_children[x.index()].get(i) else {
            self.body(x)?;
            return self.terminator(x);
        };
        self.open(0x02, Frame::Block(y));
        self.node_within(x, i + 1)?;
        self.close();
        self.tree(y)
    }

    /// Irreducible fallback: `loop { block … block { br_table } <b0> … } <bn-1> }`.
    fn dispatch(&mut self) -> Result<(), String> {
        let label = self.label.expect("dispatch label");
        let blocks = self.cfg.rpo.clone();
        self.open(0x03, Frame::Dispatch);
        for _ in &blocks {
            self.open(0x02, Frame::Other);
        }
        self.op(0x20);
        self.u(label as u64);
        self.op(0x0e);
        self.u(blocks.len() as u64);
        for i in 0..blocks.len() {
            self.u(i as u64);
        }
        self.u(0);
        // Label i names blocks[i] (RPO index), which follows the i-th innermost block.
        for &b in &blocks {
            self.close();
            self.body(b)?;
            self.terminator(b)?;
        }
        self.close();
        self.op(0x00);
        Ok(())
    }

    fn terminator(&mut self, x: Block) -> Result<(), String> {
        let f = self.f;
        let term = f.terminator(x).ok_or("wasm: block without terminator")?;
        match f.inst(term) {
            InstData::Jump { dest } => self.branch(x, dest),
            InstData::Brif { cond, then, else_ } => {
                self.operand(*cond)?;
                // A plain `br` arm (label target, no copies) becomes a `br_if`.
                if let Some(frame) = self.plain_br(x, then) {
                    let d = self.depth(frame)?;
                    self.op(0x0d);
                    self.u(d as u64);
                    return self.branch(x, else_);
                }
                if let Some(frame) = self.plain_br(x, else_) {
                    self.op(0x45); // i32.eqz
                    let d = self.depth(frame)?;
                    self.op(0x0d);
                    self.u(d as u64);
                    return self.branch(x, then);
                }
                self.open(0x04, Frame::Other);
                self.branch(x, then)?;
                self.op(0x05);
                self.branch(x, else_)?;
                self.close();
                self.op(0x00);
                Ok(())
            }
            InstData::BrTable {
                index,
                targets,
                default,
            } => {
                // br_table cannot copy per edge, so each distinct edge gets a trampoline block:
                // `block … block { br_table } <edge 0> … end <edge k-1>`.
                let mut edges: Vec<&BlockCall> = Vec::new();
                let mut map = Vec::with_capacity(targets.len() + 1);
                for c in targets.iter().chain(std::iter::once(default)) {
                    let slot = edges.iter().position(|e| *e == c).unwrap_or_else(|| {
                        edges.push(c);
                        edges.len() - 1
                    });
                    map.push(slot);
                }
                for _ in &edges {
                    self.open(0x02, Frame::Other);
                }
                self.operand(*index)?;
                self.op(0x0e);
                self.u(targets.len() as u64);
                for &m in &map {
                    self.u(m as u64);
                }
                for c in edges {
                    self.close();
                    self.branch(x, c)?;
                }
                Ok(())
            }
            InstData::Return { args } => {
                for &a in args {
                    self.operand(a)?;
                }
                self.op(0x0f);
                Ok(())
            }
            InstData::Trap { .. } => {
                self.op(0x00);
                Ok(())
            }
            d => Err(format!("wasm: bad terminator {d:?}")),
        }
    }

    /// The label frame an edge branches to when it needs no copies and no inline code.
    fn plain_br(&self, x: Block, call: &BlockCall) -> Option<Frame> {
        if self.label.is_some() || self.has_copies(call) {
            return None;
        }
        let t = call.block;
        if self.cfg.rpo_index[t.index()] <= self.cfg.rpo_index[x.index()] {
            Some(Frame::Loop(t))
        } else if self.fwd_in[t.index()] >= 2 {
            Some(Frame::Block(t))
        } else {
            None
        }
    }

    fn has_copies(&self, call: &BlockCall) -> bool {
        let params = &self.f.blocks[call.block.index()].params;
        call.args
            .iter()
            .zip(params)
            .any(|(&a, &p)| a != p && self.home[p.index()] != Home::Unused)
    }

    /// Take the edge `x → call`: assign the target's parameters, then transfer control.
    fn branch(&mut self, x: Block, call: &BlockCall) -> Result<(), String> {
        let params = &self.f.blocks[call.block.index()].params;
        let moves: Vec<(Value, u32)> = call
            .args
            .iter()
            .zip(params)
            .filter_map(|(&a, &p)| match self.home[p.index()] {
                Home::Local(l) if a != p => Some((a, l)),
                _ => None,
            })
            .collect();
        for &(a, _) in &moves {
            self.operand(a)?;
        }
        for &(_, l) in moves.iter().rev() {
            self.op(0x21);
            self.u(l as u64);
        }

        let t = call.block;
        if let Some(label) = self.label {
            self.op(0x41);
            sleb(&mut self.code, self.cfg.rpo_index[t.index()] as i64);
            self.op(0x21);
            self.u(label as u64);
            return self.br(Frame::Dispatch);
        }
        if self.cfg.rpo_index[t.index()] <= self.cfg.rpo_index[x.index()] {
            self.br(Frame::Loop(t))
        } else if self.fwd_in[t.index()] >= 2 {
            self.br(Frame::Block(t))
        } else {
            // The only forward edge into t: t is x's dominator-tree child, emitted here.
            self.tree(t)
        }
    }

    // ---- instructions ---------------------------------------------------------------------------

    /// A block's instructions other than the terminator.
    fn body(&mut self, b: Block) -> Result<(), String> {
        let f = self.f;
        let insts = &f.blocks[b.index()].insts;
        for &inst in &insts[..insts.len().saturating_sub(1)] {
            let data = f.inst(inst);
            let results = f.results(inst);
            let deferred = results
                .iter()
                .all(|r| matches!(self.home[r.index()], Home::Remat | Home::Inline | Home::Unused));
            if data.is_pure() && deferred {
                continue;
            }
            self.expr(inst)?;
            for &r in results.iter().rev() {
                match self.home[r.index()] {
                    Home::Local(l) => {
                        self.op(0x21);
                        self.u(l as u64);
                    }
                    _ => self.op(0x1a), // drop
                }
            }
        }
        Ok(())
    }

    /// Push `v`.
    fn operand(&mut self, v: Value) -> Result<(), String> {
        match self.home[v.index()] {
            Home::Local(l) => {
                self.op(0x20);
                self.u(l as u64);
                Ok(())
            }
            Home::Remat | Home::Inline => match self.f.values[v.index()].def {
                ValueDef::Result(inst, 0) => self.expr(inst),
                _ => Err(format!("wasm: {v} has no expression")),
            },
            Home::Unused => Err(format!("wasm: {v} used but has no home")),
        }
    }

    /// Push a 32-bit address operand (wrapping an I64 one).
    fn address(&mut self, v: Value) -> Result<(), String> {
        self.operand(v)?;
        match self.f.value_type(v) {
            Type::I64 => self.op(0xa7),
            Type::I32 if self.config.ptr32 => {}
            t => return Err(format!("wasm: {t:?} address {v} (Config::ptr32 is off)")),
        }
        Ok(())
    }

    /// Push the address part of `addr + offset`. Memarg offsets are unsigned, so a negative
    /// offset is added to the address here and [`Self::memarg`] encodes 0.
    fn mem_addr(&mut self, addr: Value, offset: i32) -> Result<(), String> {
        self.address(addr)?;
        if offset < 0 {
            self.op(0x41);
            sleb(&mut self.code, offset as i64);
            self.op(0x6a); // i32.add
        }
        Ok(())
    }

    fn memarg(&mut self, opcode: u8, bytes: u32, offset: i32) {
        self.op(opcode);
        self.u(bytes.trailing_zeros() as u64); // natural alignment (a hint only)
        self.u(offset.max(0) as u64);
    }

    fn call_indirect(&mut self, sig: SigRef) {
        let ty = self.types.index(&self.f.sigs[sig.index()]);
        self.op(0x11);
        self.u(ty as u64);
        self.op(0x00);
    }

    /// Push the results of `inst`.
    fn expr(&mut self, inst: Inst) -> Result<(), String> {
        let f = self.f;
        let data = f.inst(inst);
        let ty = |v: Value| f.value_type(v);
        match data {
            InstData::Iconst { ty: Type::I32, imm } => {
                self.op(0x41);
                sleb(&mut self.code, *imm as i32 as i64);
            }
            InstData::Iconst { imm, .. } => {
                self.op(0x42);
                sleb(&mut self.code, *imm);
            }
            InstData::F32const { bits } => {
                self.op(0x43);
                self.code.extend_from_slice(&bits.to_le_bytes());
            }
            InstData::F64const { bits } => {
                self.op(0x44);
                self.code.extend_from_slice(&bits.to_le_bytes());
            }
            InstData::Unary { op, arg } => {
                self.operand(*arg)?;
                self.code.extend_from_slice(unary(*op, ty(*arg))?);
            }
            InstData::Binary { op, args } => {
                self.operand(args[0])?;
                self.operand(args[1])?;
                self.op(binary(*op, ty(args[0]))?);
            }
            InstData::IntCmp { cc, args } => {
                self.operand(args[0])?;
                self.operand(args[1])?;
                use IntCC::*;
                let i = [Eq, Ne, Slt, Ult, Sgt, Ugt, Sle, Ule, Sge, Uge]
                    .iter()
                    .position(|c| c == cc)
                    .unwrap() as u8;
                self.op(if ty(args[0]) == Type::I32 { 0x46 } else { 0x51 } + i);
            }
            InstData::FloatCmp { cc, args } => {
                self.operand(args[0])?;
                self.operand(args[1])?;
                use FloatCC::*;
                let i = [Eq, Ne, Lt, Gt, Le, Ge].iter().position(|c| c == cc).unwrap() as u8;
                self.op(if ty(args[0]) == Type::F32 { 0x5b } else { 0x61 } + i);
            }
            InstData::Select {
                cond,
                if_true,
                if_false,
            } => {
                self.operand(*if_true)?;
                self.operand(*if_false)?;
                self.operand(*cond)?;
                self.op(0x1b);
            }
            InstData::Convert { op, to, arg } => {
                self.operand(*arg)?;
                self.code.extend_from_slice(convert(*op, ty(*arg), *to)?);
            }
            InstData::Load { kind, addr, offset } => {
                self.mem_addr(*addr, *offset)?;
                use MemKind::*;
                let opcode = match kind {
                    I32 => 0x28,
                    I64 => 0x29,
                    F32 => 0x2a,
                    F64 => 0x2b,
                    I32S8 => 0x2c,
                    I32U8 => 0x2d,
                    I32S16 => 0x2e,
                    I32U16 => 0x2f,
                    I64S8 => 0x30,
                    I64U8 => 0x31,
                    I64S16 => 0x32,
                    I64U16 => 0x33,
                    I64S32 => 0x34,
                    I64U32 => 0x35,
                };
                self.memarg(opcode, kind.bytes(), *offset);
            }
            InstData::Store {
                kind,
                addr,
                value,
                offset,
            } => {
                self.mem_addr(*addr, *offset)?;
                self.operand(*value)?;
                use MemKind::*;
                let opcode = match kind {
                    I32 => 0x36,
                    I64 => 0x37,
                    F32 => 0x38,
                    F64 => 0x39,
                    I32S8 | I32U8 => 0x3a,
                    I32S16 | I32U16 => 0x3b,
                    I64S8 | I64U8 => 0x3c,
                    I64S16 | I64U16 => 0x3d,
                    I64S32 | I64U32 => 0x3e,
                };
                self.memarg(opcode, kind.bytes(), *offset);
            }
            InstData::Call { func, args } => {
                for &a in args {
                    self.operand(a)?;
                }
                let ext = &f.funcs[func.index()];
                let idx = (self.resolve)(ext.id).ok_or_else(|| format!("wasm: unresolved fn {}", ext.id))?;
                self.op(0x41);
                sleb(&mut self.code, idx as i32 as i64);
                self.call_indirect(ext.sig);
            }
            InstData::CallIndirect { sig, callee, args } => {
                for &a in args {
                    self.operand(a)?;
                }
                self.address(*callee)?;
                self.call_indirect(*sig);
            }
            InstData::TrapIf { cond, .. } => {
                self.operand(*cond)?;
                self.code.extend_from_slice(&[0x04, 0x40, 0x00, 0x0b]); // if unreachable end
            }
            d => return Err(format!("wasm: {d:?} is not an expression")),
        }
        Ok(())
    }
}

fn unary(op: UnaryOp, ty: Type) -> Result<&'static [u8], String> {
    use UnaryOp::*;
    Ok(match (ty, op) {
        (Type::I32, Clz) => &[0x67],
        (Type::I32, Ctz) => &[0x68],
        (Type::I32, Popcnt) => &[0x69],
        (Type::I32, Eqz) => &[0x45],
        (Type::I32, Sext8) => &[0xc0],
        (Type::I32, Sext16) => &[0xc1],
        (Type::I64, Clz) => &[0x79],
        (Type::I64, Ctz) => &[0x7a],
        (Type::I64, Popcnt) => &[0x7b],
        (Type::I64, Eqz) => &[0x50],
        (Type::I64, Sext8) => &[0xc2],
        (Type::I64, Sext16) => &[0xc3],
        (Type::I64, Sext32) => &[0xc4],
        // f32 abs..sqrt is 0x8b..0x91 and f64 the same order from 0x99.
        (Type::F32 | Type::F64, _) => {
            const F32: [u8; 7] = [0x8b, 0x8c, 0x8d, 0x8e, 0x8f, 0x90, 0x91];
            const F64: [u8; 7] = [0x99, 0x9a, 0x9b, 0x9c, 0x9d, 0x9e, 0x9f];
            let i = match op {
                Fabs => 0,
                Fneg => 1,
                Ceil => 2,
                Floor => 3,
                Trunc => 4,
                Nearest => 5,
                Sqrt => 6,
                _ => return Err(format!("wasm: {op:?} on {ty:?}")),
            };
            let t = if ty == Type::F32 { &F32 } else { &F64 };
            std::slice::from_ref(&t[i])
        }
        _ => return Err(format!("wasm: {op:?} on {ty:?}")),
    })
}

fn binary(op: BinaryOp, ty: Type) -> Result<u8, String> {
    use BinaryOp::*;
    // Integer ops share one order from i32.add (0x6a) / i64.add (0x7c); float ops from
    // f32.add (0x92) / f64.add (0xa0).
    let int = [Iadd, Isub, Imul, Sdiv, Udiv, Srem, Urem, Band, Bor, Bxor, Ishl, Sshr, Ushr, Rotl, Rotr];
    let float = [Fadd, Fsub, Fmul, Fdiv, Fmin, Fmax, Fcopysign];
    let (base, i) = match ty {
        Type::I32 => (0x6a, int.iter().position(|&o| o == op)),
        Type::I64 => (0x7c, int.iter().position(|&o| o == op)),
        Type::F32 => (0x92, float.iter().position(|&o| o == op)),
        Type::F64 => (0xa0, float.iter().position(|&o| o == op)),
    };
    i.map(|i| base + i as u8).ok_or_else(|| format!("wasm: {op:?} on {ty:?}"))
}

fn convert(op: ConvOp, from: Type, to: Type) -> Result<&'static [u8], String> {
    use ConvOp::*;
    use Type::*;
    Ok(match (op, from, to) {
        (Wrap, I64, I32) => &[0xa7],
        (Sext, I32, I64) => &[0xac],
        (Uext, I32, I64) => &[0xad],
        (FromSint, I32, F32) => &[0xb2],
        (FromUint, I32, F32) => &[0xb3],
        (FromSint, I64, F32) => &[0xb4],
        (FromUint, I64, F32) => &[0xb5],
        (FromSint, I32, F64) => &[0xb7],
        (FromUint, I32, F64) => &[0xb8],
        (FromSint, I64, F64) => &[0xb9],
        (FromUint, I64, F64) => &[0xba],
        (ToSint, F32, I32) => &[0xa8],
        (ToUint, F32, I32) => &[0xa9],
        (ToSint, F64, I32) => &[0xaa],
        (ToUint, F64, I32) => &[0xab],
        (ToSint, F32, I64) => &[0xae],
        (ToUint, F32, I64) => &[0xaf],
        (ToSint, F64, I64) => &[0xb0],
        (ToUint, F64, I64) => &[0xb1],
        (ToSintSat, F32, I32) => &[0xfc, 0],
        (ToUintSat, F32, I32) => &[0xfc, 1],
        (ToSintSat, F64, I32) => &[0xfc, 2],
        (ToUintSat, F64, I32) => &[0xfc, 3],
        (ToSintSat, F32, I64) => &[0xfc, 4],
        (ToUintSat, F32, I64) => &[0xfc, 5],
        (ToSintSat, F64, I64) => &[0xfc, 6],
        (ToUintSat, F64, I64) => &[0xfc, 7],
        (Demote, F64, F32) => &[0xb6],
        (Promote, F32, F64) => &[0xbb],
        (Bitcast, F32, I32) => &[0xbc],
        (Bitcast, F64, I64) => &[0xbd],
        (Bitcast, I32, F32) => &[0xbe],
        (Bitcast, I64, F64) => &[0xbf],
        _ => return Err(format!("wasm: {op:?} {from:?} -> {to:?}")),
    })
}
