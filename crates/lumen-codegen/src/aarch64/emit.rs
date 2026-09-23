//! Allocated [`MInst`]s → machine code.
//!
//! Frame (the frame record at `fp` links the chain for debuggers and profilers; everything else
//! is addressed from `sp`, which is fixed after the prologue — the outgoing-argument area is
//! preallocated — so every offset is a positive scaled immediate):
//!
//! ```text
//! [fp + 16 ..]    incoming stack arguments
//! [fp + 8]        saved x30 (lr)
//! [fp]            saved x29 (caller's fp)
//!                 padding to 16
//! [sp + save]     saved callee-saved x19..x28, then d8..d15
//! [sp + ctx]      context pointer (parameter 0), for trap stubs
//! [sp + slots]    spill slots, 8 bytes each
//! [sp ..]         outgoing stack arguments
//! ```
//!
//! A spilled operand that must be in a register is reloaded into a scratch register (`x16`,
//! `x17`, `x30`, or `v30`/`v31`) and stored back after the instruction if written. `x16`/`x17`
//! also serve memory-to-memory moves and parallel-move cycles, and `x30` is free because the
//! prologue saved it.

use super::asm::{self, Asm, Cond, FInt, Label, LdSt, PairMode, Rrr, FP, LR, SP, X16, X17, ZR};
use super::inst::*;
use super::regs::FSCRATCH;
use super::{Reloc, TrapConfig};
use crate::machinst::*;
use crate::regalloc::Allocation;

/// Largest sp-relative frame the emitter addresses with scaled 12-bit offsets.
const MAX_FRAME: u32 = 32 * 1024;

pub struct Frame {
    gprs: Vec<u8>,
    fprs: Vec<u8>,
    /// Bytes between `sp` and `fp` after the prologue (a multiple of 16).
    size: u32,
    slot_base: u32,
    ctx_off: u32,
    save_off: u32,
    needs_ctx: bool,
}

impl Frame {
    pub fn new(alloc: &Allocation, outgoing: u32, needs_ctx: bool) -> Result<Frame, String> {
        let mut gprs = Vec::new();
        let mut fprs = Vec::new();
        for r in alloc.used_callee_saved.iter() {
            match r.class {
                RegClass::Int => gprs.push(r.hw),
                RegClass::Float => fprs.push(r.hw),
            }
        }
        let slot_base = outgoing.div_ceil(8) * 8;
        let ctx_off = slot_base + 8 * alloc.num_slots;
        let save_off = ctx_off + 8;
        let size = (save_off + 8 * (gprs.len() + fprs.len()) as u32).div_ceil(16) * 16;
        if size > MAX_FRAME {
            return Err(format!("aarch64: frame of {size} bytes is too large"));
        }
        Ok(Frame {
            gprs,
            fprs,
            size,
            slot_base,
            ctx_off,
            save_off,
            needs_ctx,
        })
    }

    fn slot(&self, s: u32) -> i64 {
        (self.slot_base + 8 * s) as i64
    }
}

/// `sp -= bytes` (`sub: true`) or `sp += bytes`, for any `bytes` below 16 MiB.
pub fn adjust_sp(a: &mut Asm, sub: bool, bytes: u32) {
    debug_assert!(bytes < 1 << 24);
    let (hi, lo) = (bytes >> 12, bytes & 0xfff);
    if hi != 0 {
        a.add_imm(true, sub, SP, SP, hi, true);
    }
    if lo != 0 {
        a.add_imm(true, sub, SP, SP, lo, false);
    }
}

pub struct Emitter<'a> {
    code: &'a VCode<MInst>,
    alloc: &'a Allocation,
    frame: &'a Frame,
    traps: Option<&'a TrapConfig>,
    pub a: Asm,
    blocks: Vec<Label>,
    trap_labels: Vec<(u32, Label)>,
    /// Literal pool: float constants (bits) and external function addresses (id).
    consts: Vec<(u64, Label)>,
    funcs: Vec<(u32, Label)>,
    tables: Vec<(Label, Vec<usize>)>,
    pub relocs: Vec<Reloc>,
    /// Scratch registers assigned to spilled operands of the current instruction.
    map: Vec<(VReg, PReg)>,
    post: Vec<(PReg, Loc)>,
}

const INT_SCRATCH: [u8; 3] = [X16, X17, LR];

impl<'a> Emitter<'a> {
    pub fn new(
        code: &'a VCode<MInst>,
        alloc: &'a Allocation,
        frame: &'a Frame,
        traps: Option<&'a TrapConfig>,
    ) -> Emitter<'a> {
        let mut a = Asm::new();
        let blocks = (0..code.blocks.len()).map(|_| a.new_label()).collect();
        Emitter {
            code,
            alloc,
            frame,
            traps,
            a,
            blocks,
            trap_labels: Vec::new(),
            consts: Vec::new(),
            funcs: Vec::new(),
            tables: Vec::new(),
            relocs: Vec::new(),
            map: Vec::new(),
            post: Vec::new(),
        }
    }

    fn loc(&self, v: VReg) -> Loc {
        self.alloc.loc(v)
    }

    fn scratch(class: RegClass, i: usize) -> PReg {
        match class {
            RegClass::Int => PReg::int(INT_SCRATCH[i]),
            RegClass::Float => FSCRATCH[i],
        }
    }

    // ----- moves -----

    fn slot_of(&self, l: Loc) -> i64 {
        match l {
            Loc::Stack(s) => self.frame.slot(s),
            l => panic!("aarch64: {l:?} is not a spill slot"),
        }
    }

    fn move_loc(&mut self, src: Loc, dst: Loc, class: RegClass) {
        if src == dst || dst == Loc::None {
            return;
        }
        let (ld, st) = match class {
            RegClass::Int => (LdSt::LdrX, LdSt::StrX),
            RegClass::Float => (LdSt::LdrD, LdSt::StrD),
        };
        match (src, dst) {
            (Loc::Reg(s), Loc::Reg(d)) => match class {
                RegClass::Int => self.a.mov(true, d.hw, s.hw),
                RegClass::Float => self.a.fmov(d.hw, s.hw),
            },
            (Loc::Reg(s), _) => self.a.ldst(st, s.hw, SP, self.slot_of(dst)),
            (_, Loc::Reg(d)) => self.a.ldst(ld, d.hw, SP, self.slot_of(src)),
            _ => {
                // Slot to slot: the raw bits through x16, for either class.
                self.a.ldst(LdSt::LdrX, X16, SP, self.slot_of(src));
                self.a.ldst(LdSt::StrX, X16, SP, self.slot_of(dst));
            }
        }
    }

    fn parallel(&mut self, moves: Vec<Move>) {
        let moves: Vec<Move> = moves.into_iter().filter(|m| m.dst != Loc::None).collect();
        let seq = sequence_parallel_moves(&moves, |c| match c {
            RegClass::Int => PReg::int(X17),
            RegClass::Float => FSCRATCH[1],
        });
        for m in seq {
            self.move_loc(m.src, m.dst, m.class);
        }
    }

    // ----- operands -----

    /// Assign scratch registers to spilled register operands and reload the ones read.
    fn prep(&mut self, inst: &MInst) {
        self.map.clear();
        self.post.clear();
        let mut ops = Vec::new();
        inst.operands(&mut ops);
        let mut used = [0usize; 2];
        for o in &ops {
            if o.constraint != Constraint::Reg || self.map.iter().any(|m| m.0 == o.vreg) {
                continue;
            }
            let l = self.loc(o.vreg);
            if matches!(l, Loc::Reg(_)) {
                continue;
            }
            let class = self.code.class(o.vreg);
            let ci = (class == RegClass::Float) as usize;
            let n = if ci == 0 {
                INT_SCRATCH.len()
            } else {
                FSCRATCH.len()
            };
            // Defs are written after every use is read, so a def may share a use's scratch.
            let r = if used[ci] == n && o.kind == OperandKind::Def {
                Self::scratch(class, 0)
            } else {
                assert!(
                    used[ci] < n,
                    "aarch64: out of scratch registers for {inst:?}"
                );
                used[ci] += 1;
                Self::scratch(class, used[ci] - 1)
            };
            self.map.push((o.vreg, r));
            if o.kind != OperandKind::Def {
                self.move_loc(l, Loc::Reg(r), class);
            }
            if o.kind != OperandKind::Use && l != Loc::None {
                self.post.push((r, l));
            }
        }
    }

    fn finish_inst(&mut self) {
        for (r, l) in std::mem::take(&mut self.post) {
            self.move_loc(Loc::Reg(r), l, r.class);
        }
    }

    /// The register holding `v` for the current instruction.
    fn r(&self, v: VReg) -> u8 {
        if let Some(m) = self.map.iter().find(|m| m.0 == v) {
            return m.1.hw;
        }
        match self.loc(v) {
            Loc::Reg(r) => r.hw,
            l => panic!("aarch64: {v:?} expected in a register, found {l:?}"),
        }
    }

    // ----- labels -----

    fn trap_label(&mut self, code: u32) -> Label {
        if let Some(&(_, l)) = self.trap_labels.iter().find(|t| t.0 == code) {
            return l;
        }
        let l = self.a.new_label();
        self.trap_labels.push((code, l));
        l
    }

    fn constant(&mut self, bits: u64) -> Label {
        if let Some(&(_, l)) = self.consts.iter().find(|c| c.0 == bits) {
            return l;
        }
        let l = self.a.new_label();
        self.consts.push((bits, l));
        l
    }

    fn func_label(&mut self, id: u32) -> Label {
        if let Some(&(_, l)) = self.funcs.iter().find(|c| c.0 == id) {
            return l;
        }
        let l = self.a.new_label();
        self.funcs.push((id, l));
        l
    }

    // ----- function -----

    pub fn emit(mut self) -> Result<(Vec<u8>, Vec<Reloc>), String> {
        self.prologue();
        for bi in 0..self.code.blocks.len() {
            let l = self.blocks[bi];
            self.a.bind(l);
            let b = &self.code.blocks[bi];
            for i in b.start..b.end {
                let inst = &self.code.insts[i];
                self.inst(inst, bi)?;
            }
        }
        // Out-of-line trap stubs: unwind to the entry trampoline with the trap code, exactly as
        // the guard-page fault handler does (see `super::trampoline`).
        let traps = std::mem::take(&mut self.trap_labels);
        for (code, l) in traps {
            let tc = self.traps.ok_or("aarch64: traps need a TrapConfig")?;
            self.a.bind(l);
            self.a.ldst(LdSt::LdrX, X16, SP, self.frame.ctx_off as i64);
            self.a
                .ldst_any(LdSt::LdrX, X16, X16, tc.entry_sp_offset as i64, X17);
            self.a.ldst_post(LdSt::LdrX, X17, X16, 8);
            self.a.mov_sp(SP, X16);
            self.a.mov_imm(0, code as u64 + 1);
            self.a.br(X17);
        }
        for (table, targets) in std::mem::take(&mut self.tables) {
            self.a.bind(table);
            for t in targets {
                let tl = self.blocks[t];
                self.a.table_entry(tl, table);
            }
        }
        if !self.consts.is_empty() || !self.funcs.is_empty() {
            self.a.align(8);
            for (bits, l) in std::mem::take(&mut self.consts) {
                self.a.bind(l);
                self.a.u64(bits);
            }
            for (id, l) in std::mem::take(&mut self.funcs) {
                self.a.bind(l);
                self.relocs.push(Reloc {
                    offset: self.a.pos(),
                    func_id: id,
                });
                self.a.u64(0);
            }
        }
        self.a.finish()?;
        Ok((self.a.buf, self.relocs))
    }

    fn prologue(&mut self) {
        let fr = self.frame;
        self.a.pair(false, false, PairMode::Pre, FP, LR, SP, -16);
        self.a.mov_sp(FP, SP);
        if fr.size > 0 {
            // Touch each page in order so a guard-page-grown stack (Windows) keeps up.
            for k in 1..=fr.size / 4096 {
                self.a.add_imm(true, true, X16, SP, k, true);
                self.a.ldst(LdSt::LdrX, X17, X16, 0);
            }
            adjust_sp(&mut self.a, true, fr.size);
        }
        let mut off = fr.save_off as i64;
        for &g in &fr.gprs {
            self.a.ldst(LdSt::StrX, g, SP, off);
            off += 8;
        }
        for &f in &fr.fprs {
            self.a.ldst(LdSt::StrD, f, SP, off);
            off += 8;
        }
        if fr.needs_ctx {
            self.a.ldst(LdSt::StrX, 0, SP, fr.ctx_off as i64);
        }
        if let Some(tc) = self.traps {
            if let Some((off, code)) = tc.stack_limit {
                // ldr x16, [ctx + limit]; cmp sp, x16; b.lo overflow
                self.a.ldst_any(LdSt::LdrX, X16, 0, off as i64, X17);
                self.a.cmp_ext(SP, X16);
                let l = self.trap_label(code);
                self.a.b_cond(Cond::Lo, l);
            }
        }
    }

    fn epilogue(&mut self) {
        let fr = self.frame;
        let mut off = fr.save_off as i64;
        for &g in &fr.gprs {
            self.a.ldst(LdSt::LdrX, g, SP, off);
            off += 8;
        }
        for &f in &fr.fprs {
            self.a.ldst(LdSt::LdrD, f, SP, off);
            off += 8;
        }
        self.a.mov_sp(SP, FP);
        self.a.pair(true, false, PairMode::Post, FP, LR, SP, 16);
        self.a.ret();
    }

    fn amode_ldst(&mut self, op: LdSt, rt: u8, addr: &Amode) {
        match *addr {
            Amode::Imm(b, off) => {
                let b = self.r(b);
                self.a.ldst(op, rt, b, off as i64);
            }
            Amode::Reg(b, i) => {
                let (b, i) = (self.r(b), self.r(i));
                self.a.ldst_reg(op, rt, b, i, false);
            }
        }
    }

    fn inst(&mut self, inst: &MInst, bi: usize) -> Result<(), String> {
        self.prep(inst);
        match inst {
            MInst::Args { regs, stack } => {
                let moves = regs
                    .iter()
                    .map(|&(v, p)| Move {
                        src: Loc::Reg(p),
                        dst: self.loc(v),
                        class: p.class,
                    })
                    .collect();
                self.parallel(moves);
                for &(v, off, bytes) in stack {
                    let class = self.code.class(v);
                    let src = 16 + off as i64;
                    let (iop, fop) = if bytes == 4 {
                        (LdSt::LdrW, LdSt::LdrS)
                    } else {
                        (LdSt::LdrX, LdSt::LdrD)
                    };
                    match self.loc(v) {
                        Loc::Reg(r) if class == RegClass::Int => {
                            self.a.ldst_any(iop, r.hw, FP, src, X17)
                        }
                        Loc::Reg(r) => self.a.ldst_any(fop, r.hw, FP, src, X17),
                        Loc::Stack(s) => {
                            self.a.ldst_any(iop, X16, FP, src, X17);
                            self.a.ldst(LdSt::StrX, X16, SP, self.frame.slot(s));
                        }
                        Loc::None => {}
                    }
                }
            }
            MInst::Mov { size, dst, src } => {
                let (s, d) = (self.loc(*src), self.loc(*dst));
                match (size, s, d) {
                    (_, _, Loc::None) => {}
                    (Size::S64, _, _) => self.move_loc(s, d, RegClass::Int),
                    (Size::S32, Loc::Reg(s), Loc::Reg(d)) => self.a.mov(false, d.hw, s.hw),
                    (Size::S32, _, Loc::Reg(d)) => {
                        self.a.ldst(LdSt::LdrW, d.hw, SP, self.slot_of(s))
                    }
                    (Size::S32, Loc::Reg(s), _) => {
                        self.a.mov(false, X16, s.hw);
                        self.a.ldst(LdSt::StrX, X16, SP, self.slot_of(d));
                    }
                    (Size::S32, _, _) => {
                        self.a.ldst(LdSt::LdrW, X16, SP, self.slot_of(s));
                        self.a.ldst(LdSt::StrX, X16, SP, self.slot_of(d));
                    }
                }
            }
            MInst::MovImm { dst, imm } => match self.loc(*dst) {
                Loc::None => {}
                Loc::Reg(r) => self.a.mov_imm(r.hw, *imm),
                l => {
                    if *imm == 0 {
                        self.a.ldst(LdSt::StrX, ZR, SP, self.slot_of(l));
                    } else {
                        self.a.mov_imm(X16, *imm);
                        self.a.ldst(LdSt::StrX, X16, SP, self.slot_of(l));
                    }
                }
            },
            MInst::FMov { dst, src } => {
                let (s, d) = (self.loc(*src), self.loc(*dst));
                self.move_loc(s, d, RegClass::Float);
            }
            MInst::FConst { double, dst, bits } => {
                let d = self.r(*dst);
                if *bits == 0 {
                    self.a.fzero(d);
                } else if let Some(imm8) = asm::fmov_imm8(*bits, *double) {
                    self.a.fmov_imm(*double, d, imm8);
                } else {
                    let l = self.constant(*bits);
                    self.a.ldr_lit(d, true, *double, l);
                }
            }
            MInst::Rrr {
                op,
                size,
                dst,
                a,
                b,
            } => {
                let (d, a, b) = (self.r(*dst), self.r(*a), self.r(*b));
                self.a.rrr(*op, size.w(), d, a, b);
            }
            MInst::AddImm {
                sub,
                size,
                dst,
                a,
                imm12,
                shift,
            } => {
                let (d, a) = (self.r(*dst), self.r(*a));
                self.a.add_imm(size.w(), *sub, d, a, *imm12 as u32, *shift);
            }
            MInst::LogImm {
                op,
                size,
                dst,
                a,
                enc,
            } => {
                let (d, a) = (self.r(*dst), self.r(*a));
                self.a.log_imm(*op, size.w(), d, a, *enc);
            }
            MInst::ShiftImm {
                op,
                size,
                dst,
                a,
                amt,
            } => {
                let (d, a, w, s) = (self.r(*dst), self.r(*a), size.w(), *amt as u32);
                match op {
                    ShiftOp::Lsl => self.a.lsl_imm(w, d, a, s),
                    ShiftOp::Lsr => self.a.lsr_imm(w, d, a, s),
                    ShiftOp::Asr => self.a.asr_imm(w, d, a, s),
                    ShiftOp::Ror => self.a.ror_imm(w, d, a, s),
                }
            }
            MInst::Neg { size, dst, src } => {
                let (d, s) = (self.r(*dst), self.r(*src));
                self.a.rrr(Rrr::Sub, size.w(), d, ZR, s);
            }
            MInst::Msub { size, dst, a, b, c } => {
                let (d, a, b, c) = (self.r(*dst), self.r(*a), self.r(*b), self.r(*c));
                self.a.msub(size.w(), d, a, b, c);
            }
            MInst::Bit { op, size, dst, src } => {
                let (d, s, w) = (self.r(*dst), self.r(*src), size.w());
                match op {
                    BitOp::Clz => self.a.clz(w, d, s),
                    BitOp::Ctz => {
                        self.a.rbit(w, d, s);
                        self.a.clz(w, d, d);
                    }
                    BitOp::Popcnt => {
                        // No scalar popcount: count bits per byte in a vector register and sum.
                        // I32 values are zero-extended, so the 64-bit move covers both widths.
                        let t = FSCRATCH[1].hw;
                        self.a.fcvt_int(FInt::FromGpr, true, true, t, s);
                        self.a.cnt8b(t, t);
                        self.a.addv8b(t, t);
                        self.a.fcvt_int(FInt::ToGpr, false, false, d, t);
                    }
                }
            }
            MInst::Sext {
                from,
                size,
                dst,
                src,
            } => {
                let (d, s) = (self.r(*dst), self.r(*src));
                self.a.sxt(size.w(), d, s, *from as u32);
            }
            MInst::Load { op, dst, addr } => {
                let d = self.r(*dst);
                self.amode_ldst(*op, d, addr);
            }
            MInst::Store { op, src, addr } => {
                let s = self.r(*src);
                self.amode_ldst(*op, s, addr);
            }
            MInst::Cmp { size, a, b } => {
                let ra = self.r(*a);
                match *b {
                    CmpRhs::Reg(b) => {
                        let rb = self.r(b);
                        self.a.rrr(Rrr::Subs, size.w(), ZR, ra, rb);
                    }
                    CmpRhs::Imm { imm12, shift, neg } => {
                        self.a.cmp_imm(size.w(), neg, ra, imm12 as u32, shift)
                    }
                }
            }
            MInst::FCmp { double, a, b } => {
                let (a, b) = (self.r(*a), self.r(*b));
                self.a.fcmp(*double, a, b);
            }
            MInst::CSet { cc, dst } => {
                let d = self.r(*dst);
                self.a.cset(d, *cc);
            }
            MInst::CSel { cc, dst, t, f } => {
                let (d, t, f) = (self.r(*dst), self.r(*t), self.r(*f));
                self.a.csel(true, d, t, f, *cc);
            }
            MInst::FCSel { cc, dst, t, f } => {
                let (d, t, f) = (self.r(*dst), self.r(*t), self.r(*f));
                self.a.fcsel(true, d, t, f, *cc);
            }
            MInst::FAlu {
                op,
                double,
                dst,
                a,
                b,
            } => {
                let (d, a, b) = (self.r(*dst), self.r(*a), self.r(*b));
                self.a.fop2(*op, *double, d, a, b);
            }
            MInst::FUn {
                op,
                double,
                dst,
                src,
            } => {
                let (d, s) = (self.r(*dst), self.r(*src));
                self.a.fop1(*op, *double, d, s);
            }
            MInst::FCvt {
                to_double,
                dst,
                src,
            } => {
                let (d, s) = (self.r(*dst), self.r(*src));
                self.a.fcvt(*to_double, d, s);
            }
            MInst::IntToF {
                signed,
                src_size,
                double,
                dst,
                src,
            } => {
                let (d, s) = (self.r(*dst), self.r(*src));
                let op = if *signed { FInt::Scvtf } else { FInt::Ucvtf };
                self.a.fcvt_int(op, src_size.w(), *double, d, s);
            }
            MInst::FToInt {
                signed,
                dst_size,
                double,
                dst,
                src,
            } => {
                let (d, s) = (self.r(*dst), self.r(*src));
                let op = if *signed { FInt::Fcvtzs } else { FInt::Fcvtzu };
                self.a.fcvt_int(op, dst_size.w(), *double, d, s);
            }
            MInst::GprToFpr { size, dst, src } => {
                let (d, s) = (self.r(*dst), self.r(*src));
                self.a.fcvt_int(FInt::FromGpr, size.w(), size.w(), d, s);
            }
            MInst::FprToGpr { size, dst, src } => {
                let (d, s) = (self.r(*dst), self.r(*src));
                self.a.fcvt_int(FInt::ToGpr, size.w(), size.w(), d, s);
            }
            MInst::Call {
                target,
                reg_args,
                stack_args,
                rets,
                ..
            } => {
                for &(v, off, bytes) in stack_args {
                    let off = off as i64;
                    let (iop, fop) = if bytes == 4 {
                        (LdSt::StrW, LdSt::StrS)
                    } else {
                        (LdSt::StrX, LdSt::StrD)
                    };
                    match (self.code.class(v), self.loc(v)) {
                        (RegClass::Int, Loc::Reg(r)) => self.a.ldst_any(iop, r.hw, SP, off, X17),
                        (RegClass::Float, Loc::Reg(r)) => self.a.ldst_any(fop, r.hw, SP, off, X17),
                        (_, l) => {
                            self.a.ldst(LdSt::LdrX, X16, SP, self.slot_of(l));
                            self.a.ldst_any(iop, X16, SP, off, X17);
                        }
                    }
                }
                let moves = reg_args
                    .iter()
                    .map(|&(v, p)| Move {
                        src: self.loc(v),
                        dst: Loc::Reg(p),
                        class: p.class,
                    })
                    .collect();
                self.parallel(moves);
                match target {
                    CallTarget::Func(id) => {
                        // ldr x16, =addr (patched literal); blr x16
                        let l = self.func_label(*id);
                        self.a.ldr_lit(X16, false, true, l);
                        self.a.blr(X16);
                    }
                    CallTarget::Reg(v) => match self.loc(*v) {
                        // A late use: never an argument or otherwise clobbered register.
                        Loc::Reg(r) => self.a.blr(r.hw),
                        l => {
                            self.a.ldst(LdSt::LdrX, X16, SP, self.slot_of(l));
                            self.a.blr(X16);
                        }
                    },
                }
                let moves = rets
                    .iter()
                    .map(|&(v, p)| Move {
                        src: Loc::Reg(p),
                        dst: self.loc(v),
                        class: p.class,
                    })
                    .collect();
                self.parallel(moves);
            }
            MInst::Jmp { target, args } => {
                let params = &self.code.blocks[*target].params;
                let moves = args
                    .iter()
                    .zip(params)
                    .map(|(&a, &p)| Move {
                        src: self.loc(a),
                        dst: self.loc(p),
                        class: self.code.class(a),
                    })
                    .collect();
                self.parallel(moves);
                if *target != bi + 1 {
                    let l = self.blocks[*target];
                    self.a.b(l);
                }
            }
            MInst::Jcc {
                cc,
                taken,
                not_taken,
            } => {
                let (t, n) = (self.blocks[*taken], self.blocks[*not_taken]);
                if taken == not_taken {
                    if *taken != bi + 1 {
                        self.a.b(t);
                    }
                } else if *not_taken == bi + 1 {
                    self.a.b_cond(*cc, t);
                } else if *taken == bi + 1 {
                    self.a.b_cond(cc.invert(), n);
                } else {
                    self.a.b_cond(*cc, t);
                    self.a.b(n);
                }
            }
            MInst::Cbnz {
                reg,
                taken,
                not_taken,
            } => {
                let r = self.r(*reg);
                let (t, n) = (self.blocks[*taken], self.blocks[*not_taken]);
                if taken == not_taken {
                    if *taken != bi + 1 {
                        self.a.b(t);
                    }
                } else if *not_taken == bi + 1 {
                    self.a.cbz(true, false, r, t);
                } else if *taken == bi + 1 {
                    self.a.cbz(false, false, r, n);
                } else {
                    self.a.cbz(true, false, r, t);
                    self.a.b(n);
                }
            }
            MInst::BrTable {
                index,
                targets,
                default,
            } => {
                // The index may be in x16 (a reload), so the table address goes in x17.
                let i = self.r(*index);
                let n = targets.len() as u64;
                if n < 4096 {
                    self.a.cmp_imm(false, false, i, n as u32, false);
                } else {
                    self.a.mov_imm(X17, n);
                    self.a.rrr(Rrr::Subs, false, ZR, i, X17);
                }
                let dl = self.blocks[*default];
                self.a.b_cond(Cond::Hs, dl);
                let table = self.a.new_label();
                // adr x17, table; ldrsw x16, [x17, xi, lsl #2]; add x17, x17, x16; br x17
                self.a.adr(X17, table);
                self.a.ldst_reg(LdSt::LdrSw, X16, X17, i, true);
                self.a.rrr(Rrr::Add, true, X17, X17, X16);
                self.a.br(X17);
                self.tables.push((table, targets.clone()));
            }
            MInst::TrapIf { cc, code } => {
                let l = self.trap_label(*code);
                self.a.b_cond(*cc, l);
            }
            MInst::TrapNz { reg, code } => {
                let r = self.r(*reg);
                let l = self.trap_label(*code);
                self.a.cbz(true, false, r, l);
            }
            MInst::Trap { code } => {
                let l = self.trap_label(*code);
                self.a.b(l);
            }
            MInst::Ret { vals } => {
                let moves = vals
                    .iter()
                    .map(|&(v, p)| Move {
                        src: self.loc(v),
                        dst: Loc::Reg(p),
                        class: p.class,
                    })
                    .collect();
                self.parallel(moves);
                self.epilogue();
            }
        }
        self.finish_inst();
        Ok(())
    }
}
