//! Allocated [`MInst`]s → machine code.
//!
//! Frame (addressed from `sp`, which is fixed after the prologue; the outgoing-argument area is
//! preallocated; `sp` is 8-byte aligned at every call):
//!
//! ```text
//! [sp + size + pushed ..]   incoming stack arguments
//!                           saved lr, r11, then the used callee-saved r4..r10   (push)
//!                           saved d8..dN, if any callee-saved double is used    (vpush)
//! [sp + slots + 8 * i]      spill slots, 8 bytes each
//! [sp ..]                   outgoing stack arguments
//! ```
//!
//! A spilled operand that must be in a register is reloaded into a scratch register (`r12`,
//! `lr`, `r11`; `d6`, `d7`) and stored back after the instruction if written. None of the
//! reloads, stores or moves the emitter inserts changes the condition flags.

use super::asm::{self, Asm, Cond, Dp, FOp1, Label, LdSt, Op2, LR, R11, R12, SP};
use super::inst::*;
use super::regs::{FSCRATCH, ISCRATCH};
use super::Reloc;
use crate::machinst::*;
use crate::regalloc::Allocation;

const MAX_FRAME: u32 = 1 << 20;

pub struct Frame {
    push_mask: u16,
    /// Callee-saved doubles `d8 .. d8 + fp_count` are pushed.
    fp_count: u8,
    /// Bytes `sp` is lowered by after the pushes.
    size: u32,
    slot_base: u32,
    /// Offset from `sp` (after the prologue) of the first incoming stack argument.
    incoming: u32,
}

impl Frame {
    pub fn new(alloc: &Allocation, outgoing: u32) -> Result<Frame, String> {
        let mut push_mask: u16 = 1 << 14 | 1 << 11;
        let mut fp_count = 0u8;
        for r in alloc.used_callee_saved.iter() {
            match r.class {
                RegClass::Int => push_mask |= 1 << r.hw,
                RegClass::Float => fp_count = fp_count.max(r.hw - 8 + 1),
            }
        }
        let pushed = 4 * push_mask.count_ones() + 8 * fp_count as u32;
        let slot_base = outgoing;
        let mut size = outgoing + 8 * alloc.num_slots;
        if (pushed + size) % 8 != 0 {
            size += 4;
        }
        if size > MAX_FRAME {
            return Err(format!("arm32: frame of {size} bytes is too large"));
        }
        Ok(Frame {
            push_mask,
            fp_count,
            size,
            slot_base,
            incoming: size + pushed,
        })
    }

    fn slot(&self, s: u32) -> i64 {
        (self.slot_base + 8 * s) as i64
    }
}

pub struct Emitter<'a> {
    code: &'a VCode<MInst>,
    alloc: &'a Allocation,
    frame: &'a Frame,
    pub a: Asm,
    blocks: Vec<Label>,
    pub relocs: Vec<Reloc>,
    /// Scratch registers assigned to spilled operands of the current instruction.
    map: Vec<(VReg, PReg)>,
    post: Vec<(PReg, Loc)>,
}

impl<'a> Emitter<'a> {
    /// `v7`: emit ARMv7-A code; otherwise ARMv6 (see [`Asm::v7`]).
    pub fn new(
        code: &'a VCode<MInst>,
        alloc: &'a Allocation,
        frame: &'a Frame,
        v7: bool,
    ) -> Emitter<'a> {
        let mut a = Asm::with_isa(v7);
        let blocks = (0..code.blocks.len()).map(|_| a.new_label()).collect();
        Emitter {
            code,
            alloc,
            frame,
            a,
            blocks,
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
            RegClass::Int => PReg::int(ISCRATCH[i]),
            RegClass::Float => FSCRATCH[i],
        }
    }

    // ----- memory relative to sp -----

    /// An integer scratch register that is neither in `avoid` nor `also`.
    fn temp(avoid: &[u8], also: &[u8]) -> u8 {
        *ISCRATCH
            .iter()
            .find(|r| !avoid.contains(r) && !also.contains(r))
            .expect("arm32: no free scratch register")
    }

    /// `ldr`/`str rt, [sp, #off]`, reaching any offset.
    fn mem_word(&mut self, load: bool, rt: u8, off: i64, avoid: &[u8]) {
        let op = if load { LdSt::Ldr } else { LdSt::Str };
        if op.offset_ok(off) {
            self.a.ldst(op, rt, SP, off);
            return;
        }
        let tmp = if load { rt } else { Self::temp(avoid, &[rt]) };
        self.a.mov_imm(tmp, off as u32);
        self.a.ldst_reg(op, rt, SP, tmp);
    }

    /// `vldr`/`vstr d, [sp, #off]`, reaching any offset.
    fn mem_vfp(&mut self, load: bool, d: u8, off: i64, avoid: &[u8]) {
        if off % 4 == 0 && (-1020..=1020).contains(&off) {
            self.a.vldst(load, true, d, SP, off);
            return;
        }
        let tmp = Self::temp(avoid, &[]);
        self.a.mov_imm(tmp, off as u32);
        self.a.dp(Cond::Al, Dp::Add, false, tmp, SP, Op2::Reg(tmp));
        self.a.vldst(load, true, d, tmp, 0);
    }

    fn slot_of(&self, l: Loc) -> i64 {
        match l {
            Loc::Stack(s) => self.frame.slot(s),
            l => panic!("arm32: {l:?} is not a spill slot"),
        }
    }

    fn move_loc(&mut self, src: Loc, dst: Loc, class: RegClass, avoid: &[u8]) {
        if src == dst || dst == Loc::None {
            return;
        }
        match (class, src, dst) {
            (RegClass::Int, Loc::Reg(s), Loc::Reg(d)) => self.a.mov(Cond::Al, d.hw, s.hw),
            (RegClass::Float, Loc::Reg(s), Loc::Reg(d)) => {
                self.a.fop1(Cond::Al, FOp1::Mov, true, d.hw, s.hw)
            }
            (RegClass::Int, Loc::Reg(s), _) => self.mem_word(false, s.hw, self.slot_of(dst), avoid),
            (RegClass::Int, _, Loc::Reg(d)) => self.mem_word(true, d.hw, self.slot_of(src), avoid),
            (RegClass::Float, Loc::Reg(s), _) => {
                self.mem_vfp(false, s.hw, self.slot_of(dst), avoid)
            }
            (RegClass::Float, _, Loc::Reg(d)) => self.mem_vfp(true, d.hw, self.slot_of(src), avoid),
            (_, _, _) => {
                // Slot to slot: the raw words through lr.
                let words = if class == RegClass::Float { 2 } else { 1 };
                let mut avoid: Vec<u8> = avoid.to_vec();
                avoid.push(LR);
                for k in 0..words {
                    self.mem_word(true, LR, self.slot_of(src) + 4 * k, &avoid);
                    self.mem_word(false, LR, self.slot_of(dst) + 4 * k, &avoid);
                }
            }
        }
    }

    fn parallel(&mut self, moves: Vec<Move>) {
        let moves: Vec<Move> = moves.into_iter().filter(|m| m.dst != Loc::None).collect();
        let seq = sequence_parallel_moves(&moves, |c| match c {
            RegClass::Int => PReg::int(R12),
            RegClass::Float => FSCRATCH[1],
        });
        for m in seq {
            self.move_loc(m.src, m.dst, m.class, &[R12]);
        }
    }

    // ----- operands -----

    /// Assign scratch registers to spilled register operands and reload the ones read.
    /// Floats first, so a large-offset float reload can borrow an integer scratch register.
    fn prep(&mut self, inst: &MInst) {
        self.map.clear();
        self.post.clear();
        let mut ops = Vec::new();
        inst.operands(&mut ops);
        for class in [RegClass::Float, RegClass::Int] {
            let n = if class == RegClass::Int {
                ISCRATCH.len()
            } else {
                FSCRATCH.len()
            };
            let mut used = 0usize;
            for o in &ops {
                if o.constraint != Constraint::Reg
                    || self.code.class(o.vreg) != class
                    || self.map.iter().any(|m| m.0 == o.vreg)
                {
                    continue;
                }
                let l = self.loc(o.vreg);
                if matches!(l, Loc::Reg(_)) {
                    continue;
                }
                // Defs are written after every use is read, so a def may share a use's scratch.
                let r = if used == n && o.kind == OperandKind::Def {
                    Self::scratch(class, 0)
                } else {
                    assert!(used < n, "arm32: out of scratch registers for {inst:?}");
                    used += 1;
                    Self::scratch(class, used - 1)
                };
                self.map.push((o.vreg, r));
                if o.kind != OperandKind::Def {
                    let avoid: Vec<u8> = self
                        .map
                        .iter()
                        .filter(|m| m.1.class == RegClass::Int)
                        .map(|m| m.1.hw)
                        .collect();
                    self.move_loc(l, Loc::Reg(r), class, &avoid);
                }
                if o.kind != OperandKind::Use && l != Loc::None {
                    self.post.push((r, l));
                }
            }
        }
    }

    fn finish_inst(&mut self) {
        let posts = std::mem::take(&mut self.post);
        let pending: Vec<u8> = posts
            .iter()
            .filter(|p| p.0.class == RegClass::Int)
            .map(|p| p.0.hw)
            .collect();
        for &(r, l) in posts.iter().filter(|p| p.0.class == RegClass::Float) {
            self.move_loc(Loc::Reg(r), l, r.class, &pending);
        }
        for &(r, l) in posts.iter().filter(|p| p.0.class == RegClass::Int) {
            self.move_loc(Loc::Reg(r), l, r.class, &pending);
        }
    }

    /// The register holding `v` for the current instruction.
    fn r(&self, v: VReg) -> u8 {
        if let Some(m) = self.map.iter().find(|m| m.0 == v) {
            return m.1.hw;
        }
        match self.loc(v) {
            Loc::Reg(r) => r.hw,
            l => panic!("arm32: {v:?} expected in a register, found {l:?}"),
        }
    }

    fn op2(&self, b: &Rhs) -> Op2 {
        match *b {
            Rhs::Imm(i) => Op2::Imm(i),
            Rhs::Reg(v) => Op2::Reg(self.r(v)),
            Rhs::Shift(v, k, n) => Op2::RegShiftImm(self.r(v), k, n),
        }
    }

    // ----- frame -----

    fn adjust_sp(&mut self, sub: bool, bytes: u32) {
        if bytes == 0 {
            return;
        }
        let op = if sub { Dp::Sub } else { Dp::Add };
        if asm::encode_imm(bytes).is_some() {
            self.a.dp(Cond::Al, op, false, SP, SP, Op2::Imm(bytes));
        } else {
            self.a.mov_imm(R12, bytes);
            self.a.dp(Cond::Al, op, false, SP, SP, Op2::Reg(R12));
        }
    }

    fn prologue(&mut self) {
        let fr = self.frame;
        self.a.push(fr.push_mask);
        if fr.fp_count > 0 {
            self.a.vpush(8, fr.fp_count);
        }
        self.adjust_sp(true, fr.size);
    }

    fn epilogue(&mut self) {
        let fr = self.frame;
        self.adjust_sp(false, fr.size);
        if fr.fp_count > 0 {
            self.a.vpop(8, fr.fp_count);
        }
        // lr is restored straight into pc.
        self.a.pop((fr.push_mask & !(1 << 14)) | 1 << 15);
    }

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
        self.a.finish()?;
        Ok((self.a.buf, self.relocs))
    }

    /// Store the stack-argument value `loc` to `[sp + off]`.
    fn store_stack_arg(&mut self, v: VReg, off: i64, bytes: u8) {
        let class = self.code.class(v);
        let l = self.loc(v);
        match (class, l) {
            (RegClass::Int, Loc::Reg(r)) => self.mem_word(false, r.hw, off, &[]),
            (RegClass::Float, Loc::Reg(r)) => {
                debug_assert_eq!(bytes, 8);
                self.mem_vfp(false, r.hw, off, &[])
            }
            (_, Loc::Stack(_)) => {
                for k in 0..(bytes as i64 / 4) {
                    self.mem_word(true, LR, self.slot_of(l) + 4 * k, &[LR]);
                    self.mem_word(false, LR, off + 4 * k, &[LR]);
                }
            }
            (_, Loc::None) => {}
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
                    let src = self.frame.incoming as i64 + off as i64;
                    let class = self.code.class(v);
                    match self.loc(v) {
                        Loc::Reg(r) if class == RegClass::Int => {
                            self.mem_word(true, r.hw, src, &[])
                        }
                        Loc::Reg(r) => self.mem_vfp(true, r.hw, src, &[]),
                        l @ Loc::Stack(_) => {
                            for k in 0..(bytes as i64 / 4) {
                                self.mem_word(true, LR, src + 4 * k, &[LR]);
                                self.mem_word(false, LR, self.slot_of(l) + 4 * k, &[LR]);
                            }
                        }
                        Loc::None => {}
                    }
                }
            }
            MInst::Mov { dst, src } | MInst::FMov { dst, src } => {
                let class = self.code.class(*src);
                self.move_loc(self.loc(*src), self.loc(*dst), class, &[]);
            }
            MInst::MovImm { dst, imm } => {
                let d = self.r(*dst);
                self.a.mov_imm(d, *imm);
            }
            MInst::Alu { op, s, dst, a, b } => {
                let (d, ra) = (self.r(*dst), self.r(*a));
                let o2 = self.op2(b);
                self.a.dp(Cond::Al, *op, *s, d, ra, o2);
            }
            MInst::Mvn { dst, b } => {
                let d = self.r(*dst);
                let o2 = self.op2(b);
                self.a.dp(Cond::Al, Dp::Mvn, false, d, 0, o2);
            }
            MInst::Cmp { op, cc, a, b } => {
                let ra = self.r(*a);
                let o2 = self.op2(b);
                self.a.dp(*cc, *op, true, 0, ra, o2);
            }
            MInst::Shift { kind, dst, a, amt } => {
                let (d, ra) = (self.r(*dst), self.r(*a));
                let o2 = match amt {
                    Amt::Imm(n) => Op2::RegShiftImm(ra, *kind, *n),
                    Amt::Reg(s) => Op2::RegShiftReg(ra, *kind, self.r(*s)),
                };
                self.a.dp(Cond::Al, Dp::Mov, false, d, 0, o2);
            }
            MInst::Mul { dst, a, b } => {
                let (d, ra, rb) = (self.r(*dst), self.r(*a), self.r(*b));
                self.a.mul(d, ra, rb);
            }
            MInst::Mla { dst, a, b, c } => {
                let (d, ra, rb, rc) = (self.r(*dst), self.r(*a), self.r(*b), self.r(*c));
                self.a.mla(d, ra, rb, rc);
            }
            MInst::LongMul {
                signed,
                lo,
                hi,
                a,
                b,
            } => {
                let (l, h, ra, rb) = (self.r(*lo), self.r(*hi), self.r(*a), self.r(*b));
                self.a.long_mul(*signed, l, h, ra, rb);
            }
            MInst::Div { signed, dst, a, b } => {
                let (d, ra, rb) = (self.r(*dst), self.r(*a), self.r(*b));
                self.a.div(*signed, d, ra, rb);
            }
            MInst::Clz { dst, src } => {
                let (d, s) = (self.r(*dst), self.r(*src));
                self.a.clz(d, s);
            }
            MInst::Rbit { dst, src } => {
                let (d, s) = (self.r(*dst), self.r(*src));
                if !self.a.v7 {
                    return Err("arm32: rbit needs ARMv7".into());
                }
                self.a.rbit(d, s);
            }
            MInst::Ext { e, dst, src } => {
                let (d, s) = (self.r(*dst), self.r(*src));
                self.a.ext(*e, d, s);
            }
            MInst::Load { op, dst, addr } => {
                let d = self.r(*dst);
                match *addr {
                    Amode::Imm(b, off) => {
                        let b = self.r(b);
                        self.a.ldst(*op, d, b, off as i64);
                    }
                    Amode::Reg(b, i) => {
                        let (b, i) = (self.r(b), self.r(i));
                        self.a.ldst_reg(*op, d, b, i);
                    }
                }
            }
            MInst::Store { op, src, addr } => {
                let s = self.r(*src);
                match *addr {
                    Amode::Imm(b, off) => {
                        let b = self.r(b);
                        self.a.ldst(*op, s, b, off as i64);
                    }
                    Amode::Reg(b, i) => {
                        let (b, i) = (self.r(b), self.r(i));
                        self.a.ldst_reg(*op, s, b, i);
                    }
                }
            }
            MInst::FLoad {
                double,
                dst,
                base,
                off,
            } => {
                let (d, b) = (self.r(*dst), self.r(*base));
                let d = if *double { d } else { 2 * d };
                self.a.vldst(true, *double, d, b, *off as i64);
            }
            MInst::FStore {
                double,
                src,
                base,
                off,
            } => {
                let (s, b) = (self.r(*src), self.r(*base));
                let s = if *double { s } else { 2 * s };
                self.a.vldst(false, *double, s, b, *off as i64);
            }
            MInst::Sel { cc, dst, t, f } => {
                let (d, t, f) = (self.r(*dst), self.r(*t), self.r(*f));
                if d == t && d == f {
                } else if d == t {
                    self.a.mov(cc.invert(), d, f);
                } else if d == f {
                    self.a.mov(*cc, d, t);
                } else {
                    self.a.mov(*cc, d, t);
                    self.a.mov(cc.invert(), d, f);
                }
            }
            MInst::SetCc { cc, dst } => {
                let d = self.r(*dst);
                self.a.dp(Cond::Al, Dp::Mov, false, d, 0, Op2::Imm(0));
                self.a.dp(*cc, Dp::Mov, false, d, 0, Op2::Imm(1));
            }
            MInst::FConst { double, dst, bits } => {
                let d = self.r(*dst);
                if *double {
                    self.a.mov_imm(R12, *bits as u32);
                    self.a.mov_imm(LR, (*bits >> 32) as u32);
                    self.a.vmov_dr(Cond::Al, false, d, R12, LR);
                } else {
                    self.a.mov_imm(R12, *bits as u32);
                    self.a.vmov_sr(Cond::Al, false, 2 * d, R12);
                }
            }
            MInst::FAlu {
                op,
                double,
                dst,
                a,
                b,
            } => {
                let (d, ra, rb) = (self.r(*dst), self.r(*a), self.r(*b));
                if *double {
                    self.a.fop2(Cond::Al, *op, true, d, ra, rb);
                } else {
                    self.a.fop2(Cond::Al, *op, false, 2 * d, 2 * ra, 2 * rb);
                }
            }
            MInst::FUn {
                op,
                double,
                dst,
                src,
            } => {
                let (d, s) = (self.r(*dst), self.r(*src));
                if *double {
                    self.a.fop1(Cond::Al, *op, true, d, s);
                } else {
                    self.a.fop1(Cond::Al, *op, false, 2 * d, 2 * s);
                }
            }
            MInst::FCmp { double, a, b } => {
                let (ra, rb) = (self.r(*a), self.r(*b));
                if *double {
                    self.a.vcmp(true, ra, rb);
                } else {
                    self.a.vcmp(false, 2 * ra, 2 * rb);
                }
                self.a.vmrs_flags();
            }
            MInst::FSel { cc, dst, t, f } => {
                let (d, t, f) = (self.r(*dst), self.r(*t), self.r(*f));
                if d == t && d == f {
                } else if d == t {
                    self.a.fop1(cc.invert(), FOp1::Mov, true, d, f);
                } else if d == f {
                    self.a.fop1(*cc, FOp1::Mov, true, d, t);
                } else {
                    self.a.fop1(*cc, FOp1::Mov, true, d, t);
                    self.a.fop1(cc.invert(), FOp1::Mov, true, d, f);
                }
            }
            MInst::FCvt {
                to_double,
                dst,
                src,
            } => {
                let (d, s) = (self.r(*dst), self.r(*src));
                if *to_double {
                    self.a.vcvt_f(true, d, 2 * s);
                } else {
                    self.a.vcvt_f(false, 2 * d, s);
                }
            }
            MInst::IntToF {
                signed,
                double,
                dst,
                src,
            } => {
                let (d, s) = (self.r(*dst), self.r(*src));
                self.a.vmov_sr(Cond::Al, false, 2 * d, s);
                self.a
                    .vcvt_from_int(*signed, *double, if *double { d } else { 2 * d }, 2 * d);
            }
            MInst::FToInt {
                signed,
                double,
                dst,
                src,
            } => {
                let (d, s) = (self.r(*dst), self.r(*src));
                let tmp = 2 * FSCRATCH[1].hw;
                self.a
                    .vcvt_to_int(*signed, *double, tmp, if *double { s } else { 2 * s });
                self.a.vmov_sr(Cond::Al, true, tmp, d);
            }
            MInst::JsInt32 { dst, src } => self.js_int32(*dst, *src),
            MInst::GprToFpr { dst, src } => {
                let (d, s) = (self.r(*dst), self.r(*src));
                self.a.vmov_sr(Cond::Al, false, 2 * d, s);
            }
            MInst::FprToGpr { dst, src } => {
                let (d, s) = (self.r(*dst), self.r(*src));
                self.a.vmov_sr(Cond::Al, true, 2 * s, d);
            }
            MInst::PairToF { dst, lo, hi } => {
                let (d, l, h) = (self.r(*dst), self.r(*lo), self.r(*hi));
                self.a.vmov_dr(Cond::Al, false, d, l, h);
            }
            MInst::FToPair { lo, hi, src } => {
                let (l, h, s) = (self.r(*lo), self.r(*hi), self.r(*src));
                self.a.vmov_dr(Cond::Al, true, s, l, h);
            }
            MInst::Call(c) => self.call(c),
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
            } => self.branch(*cc, *taken, *not_taken, bi),
            MInst::Cbnz {
                reg,
                taken,
                not_taken,
            } => {
                let r = self.r(*reg);
                self.a.dp(Cond::Al, Dp::Cmp, true, 0, r, Op2::Imm(0));
                self.branch(Cond::Ne, *taken, *not_taken, bi);
            }
            MInst::BrTable {
                index,
                targets,
                default,
            } => {
                let i = self.r(*index);
                let n = targets.len() as u32;
                if asm::encode_imm(n).is_some() {
                    self.a.dp(Cond::Al, Dp::Cmp, true, 0, i, Op2::Imm(n));
                } else {
                    self.a.mov_imm(R12, n);
                    self.a.dp(Cond::Al, Dp::Cmp, true, 0, i, Op2::Reg(R12));
                }
                let dl = self.blocks[*default];
                self.a.b_cond(Cond::Hs, dl);
                // add pc, pc, i, lsl #2: pc reads as this instruction + 8, which skips the
                // padding word and lands on the first `b` of the table.
                self.a.dp(
                    Cond::Al,
                    Dp::Add,
                    false,
                    asm::PC,
                    asm::PC,
                    Op2::RegShiftImm(i, asm::Shift::Lsl, 2),
                );
                self.a.nop();
                for &t in targets {
                    let tl = self.blocks[t];
                    self.a.b(tl);
                }
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

    fn branch(&mut self, cc: Cond, taken: usize, not_taken: usize, bi: usize) {
        let (t, n) = (self.blocks[taken], self.blocks[not_taken]);
        if taken == not_taken {
            if taken != bi + 1 {
                self.a.b(t);
            }
        } else if not_taken == bi + 1 {
            self.a.b_cond(cc, t);
        } else if taken == bi + 1 {
            self.a.b_cond(cc.invert(), n);
        } else {
            self.a.b_cond(cc, t);
            self.a.b(n);
        }
    }

    /// A sequentially consistent `atomics` intrinsic in place of its call. AAPCS puts the
    /// I64 address in r0:r1 (the pointer is r0), the operand (ADD32) or the expected value
    /// (COMPARE_EXCHANGE32) in r2 and the replacement in r3; the old value returns in r0. The
    /// call's clobbers cover r1, r3 and r12 as scratch.
    fn atomic32(&mut self, id: u32) {
        const CLREX: u32 = 0xf57f_f01f;
        let ldrex = |t: u32, n: u32| 0xe190_0f9f | n << 16 | t << 12;
        let strex = |d: u32, t: u32, n: u32| 0xe180_0f90 | n << 16 | d << 12 | t;
        let cmp_imm0 = |n: u32| 0xe350_0000 | n << 16;
        let cmp = |n: u32, m: u32| 0xe150_0000 | n << 16 | m;
        let add = |d: u32, n: u32, m: u32| 0xe080_0000 | n << 16 | d << 12 | m;
        let mov = |d: u32, m: u32| 0xe1a0_0000 | d << 12 | m;
        // r12 is a clobbered scratch register (the ARMv6 barrier needs one).
        self.a.barrier(R12);
        let retry = self.a.new_label();
        self.a.bind(retry);
        self.a.word(ldrex(1, 0)); // ldrex r1, [r0]
        if id == crate::atomics::ADD32 {
            self.a.word(add(12, 1, 2)); // add r12, r1, r2
            self.a.word(strex(3, 12, 0)); // strex r3, r12, [r0]
            self.a.word(cmp_imm0(3));
            self.a.b_cond(Cond::Ne, retry);
        } else {
            let mismatch = self.a.new_label();
            let done = self.a.new_label();
            self.a.word(cmp(1, 2));
            self.a.b_cond(Cond::Ne, mismatch);
            self.a.word(strex(12, 3, 0)); // strex r12, r3, [r0]
            self.a.word(cmp_imm0(12));
            self.a.b_cond(Cond::Ne, retry);
            self.a.b(done);
            self.a.bind(mismatch);
            // `clrex` is ARMv6K. Leaving the monitor open is harmless: every `strex` here
            // follows its own `ldrex`, and an exception return clears the monitor.
            if self.a.v7 {
                self.a.word(CLREX);
            }
            self.a.bind(done);
        }
        self.a.barrier(R12);
        self.a.word(mov(0, 1)); // the old value
    }

    fn call(&mut self, c: &CallInfo) {
        let CallInfo {
            target,
            reg_args,
            stack_args,
            rets,
            ..
        } = c;
        for &(v, off, bytes) in stack_args {
            self.store_stack_arg(v, off as i64, bytes);
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
            CallTarget::Func(id) if crate::atomics::signature(*id).is_some() => self.atomic32(*id),
            CallTarget::Func(id) => {
                self.relocs.push(Reloc {
                    offset: self.a.pos(),
                    func_id: *id,
                });
                self.a.mov_imm_patchable(R12, 0);
                self.a.blx(R12);
            }
            CallTarget::Abs(addr) => {
                self.a.mov_imm(R12, *addr as u32);
                self.a.blx(R12);
            }
            CallTarget::Reg(v) => match self.loc(*v) {
                // A late use: never an argument or otherwise clobbered register.
                Loc::Reg(r) => self.a.blx(r.hw),
                l => {
                    self.mem_word(true, R12, self.slot_of(l), &[]);
                    self.a.blx(R12);
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

    /// JavaScript `ToInt32` of the double in float register `src`. The saturating `vcvt` is
    /// exact unless it saturated; that rare case calls [`super::rt::js_to_int32`] with every
    /// caller-saved register (the allocator knows nothing about the call) saved around it.
    fn js_int32(&mut self, dst: VReg, src: VReg) {
        let s = self.r(src);
        let tmp = 2 * FSCRATCH[1].hw;
        let done = self.a.new_label();
        self.a.vcvt_to_int(true, true, tmp, s);
        self.a.vmov_sr(Cond::Al, true, tmp, R11);
        // r11 + 0x80000001 is below 2 only for 0x7fffffff and 0x80000000.
        self.a
            .dp(Cond::Al, Dp::Add, false, R12, R11, Op2::Imm(0x8000_0000));
        self.a.dp(Cond::Al, Dp::Add, false, R12, R12, Op2::Imm(1));
        self.a.dp(Cond::Al, Dp::Cmp, true, 0, R12, Op2::Imm(2));
        self.a.b_cond(Cond::Hs, done);
        // r0-r3, r4, r5, r12 and lr: eight words keep sp 8-byte aligned.
        self.a.push(0x4000 | 0x1000 | 0x30 | 0xf);
        self.a.vpush(0, 8);
        if s != 0 {
            self.a.fop1(Cond::Al, FOp1::Mov, true, 0, s);
        }
        self.a.mov_imm(
            R12,
            super::rt::addr(super::rt::js_to_int32 as extern "C" fn(f64) -> i32) as u32,
        );
        self.a.blx(R12);
        self.a.mov(Cond::Al, R11, 0);
        self.a.vpop(0, 8);
        self.a.pop(0x4000 | 0x1000 | 0x30 | 0xf);
        self.a.bind(done);
        let l = self.loc(dst);
        self.move_loc(Loc::Reg(PReg::int(R11)), l, RegClass::Int, &[]);
    }
}
