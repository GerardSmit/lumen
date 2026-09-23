//! Allocated [`MInst`]s → machine code.
//!
//! Frame (rbp-based, so trap stubs and spill slots never depend on rsp):
//!
//! ```text
//! [rbp + 16 ..]   incoming stack arguments (Win64: after 32 bytes of home space)
//! [rbp + 8]       return address
//! [rbp]           caller's rbp
//! [rbp - 8 ..]    saved callee-saved GPRs
//!                 context pointer (parameter 0), for trap stubs
//!                 spill slots, 8 bytes each
//!                 saved callee-saved XMMs (Win64)
//! [rsp ..]        outgoing arguments
//! ```
//!
//! A spilled operand that must be in a register is reloaded into a scratch register (R10/R11,
//! or the ABI's float scratch pair) and stored back after the instruction if written.

use super::asm::{self, Asm, Label, RM};
use super::inst::*;
use super::regs::Abi;
use super::{Reloc, TrapConfig};
use crate::machinst::*;
use crate::regalloc::Allocation;

pub struct Frame {
    gprs: Vec<u8>,
    xmms: Vec<u8>,
    /// Bytes subtracted from rsp after the pushes.
    size: u32,
    ctx_off: i32,
    slot_base: i32,
    xmm_off: i32,
    needs_ctx: bool,
}

impl Frame {
    pub fn new(alloc: &Allocation, outgoing: u32, needs_ctx: bool) -> Frame {
        let mut gprs = Vec::new();
        let mut xmms = Vec::new();
        for r in alloc.used_callee_saved.iter() {
            match r.class {
                RegClass::Int => gprs.push(r.hw),
                RegClass::Float => xmms.push(r.hw),
            }
        }
        let base = 8 * gprs.len() as i32;
        let ctx_off = -(base + 8);
        let slot_base = -(base + 16);
        let after_slots = base + 8 + 8 * alloc.num_slots as i32;
        let xmm_off = -(after_slots + 16 * xmms.len() as i32);
        let mut size = (after_slots - base) as u32 + 16 * xmms.len() as u32 + outgoing;
        // rsp is 16-aligned at the call into us minus the return address; rbp is aligned.
        while (base as u32 + size) % 16 != 0 {
            size += 8;
        }
        Frame {
            gprs,
            xmms,
            size,
            ctx_off,
            slot_base,
            xmm_off,
            needs_ctx,
        }
    }

    fn slot(&self, s: u32) -> RM {
        RM::mem(asm::RBP, self.slot_base - 8 * s as i32)
    }
}

pub struct Emitter<'a> {
    code: &'a VCode<MInst>,
    alloc: &'a Allocation,
    abi: &'a Abi,
    frame: &'a Frame,
    traps: Option<&'a TrapConfig>,
    pub a: Asm,
    blocks: Vec<Label>,
    trap_labels: Vec<(u32, Label)>,
    consts: Vec<(u64, u64, Label)>,
    tables: Vec<(Label, Vec<usize>)>,
    pub relocs: Vec<Reloc>,
    /// Scratch registers assigned to spilled operands of the current instruction.
    map: Vec<(VReg, PReg)>,
    post: Vec<(PReg, Loc)>,
}

fn is_reg(l: Loc) -> bool {
    matches!(l, Loc::Reg(_))
}

impl<'a> Emitter<'a> {
    pub fn new(
        code: &'a VCode<MInst>,
        alloc: &'a Allocation,
        abi: &'a Abi,
        frame: &'a Frame,
        traps: Option<&'a TrapConfig>,
        short: Vec<bool>,
    ) -> Emitter<'a> {
        let mut a = Asm::new(short);
        let blocks = (0..code.blocks.len()).map(|_| a.new_label()).collect();
        Emitter {
            code,
            alloc,
            abi,
            frame,
            traps,
            a,
            blocks,
            trap_labels: Vec::new(),
            consts: Vec::new(),
            tables: Vec::new(),
            relocs: Vec::new(),
            map: Vec::new(),
            post: Vec::new(),
        }
    }

    fn loc(&self, v: VReg) -> Loc {
        self.alloc.loc(v)
    }

    fn scratch(&self, class: RegClass, i: usize) -> PReg {
        match class {
            RegClass::Int => [super::regs::R10, super::regs::R11][i],
            RegClass::Float => self.abi.fscratch[i],
        }
    }

    // ----- moves -----

    fn loc_rm(&self, l: Loc) -> RM {
        match l {
            Loc::Reg(r) => RM::Reg(r.hw),
            Loc::Stack(s) => self.frame.slot(s),
            Loc::None => panic!("x64: operand without a location"),
        }
    }

    fn move_loc(&mut self, src: Loc, dst: Loc, class: RegClass) {
        if src == dst || dst == Loc::None {
            return;
        }
        match (class, src, dst) {
            (RegClass::Int, Loc::Reg(s), Loc::Reg(d)) => self.a.mov_rr(d.hw, s.hw),
            (RegClass::Int, Loc::Reg(s), _) => self.a.mov_store(true, s.hw, self.loc_rm(dst)),
            (RegClass::Int, _, Loc::Reg(d)) => self.a.mov_load(true, d.hw, self.loc_rm(src)),
            (RegClass::Int, _, _) => {
                self.a.mov_load(true, asm::R10, self.loc_rm(src));
                self.a.mov_store(true, asm::R10, self.loc_rm(dst));
            }
            (RegClass::Float, Loc::Reg(s), Loc::Reg(d)) => self.movaps(d.hw, s.hw),
            (RegClass::Float, Loc::Reg(s), _) => self.movsd_store(s.hw, self.loc_rm(dst)),
            (RegClass::Float, _, Loc::Reg(d)) => self.movsd_load(d.hw, self.loc_rm(src)),
            (RegClass::Float, _, _) => {
                let t = self.abi.fscratch[0].hw;
                self.movsd_load(t, self.loc_rm(src));
                self.movsd_store(t, self.loc_rm(dst));
            }
        }
    }

    fn parallel(&mut self, moves: Vec<Move>) {
        let moves: Vec<Move> = moves.into_iter().filter(|m| m.dst != Loc::None).collect();
        let abi = self.abi;
        let seq = sequence_parallel_moves(&moves, |c| match c {
            RegClass::Int => super::regs::R11,
            RegClass::Float => abi.fscratch[1],
        });
        for m in seq {
            self.move_loc(m.src, m.dst, m.class);
        }
    }

    fn movaps(&mut self, d: u8, s: u8) {
        self.a.op(0, false, &[0x0f, 0x28], d, RM::Reg(s), false, 0);
    }
    fn movsd_load(&mut self, d: u8, rm: RM) {
        self.a.op(0xf2, false, &[0x0f, 0x10], d, rm, false, 0);
    }
    fn movsd_store(&mut self, s: u8, rm: RM) {
        self.a.op(0xf2, false, &[0x0f, 0x11], s, rm, false, 0);
    }
    fn xorps(&mut self, d: u8, s: u8) {
        self.a.op(0, false, &[0x0f, 0x57], d, RM::Reg(s), false, 0);
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
            if is_reg(l) {
                continue;
            }
            let class = self.code.class(o.vreg);
            let ci = (class == RegClass::Float) as usize;
            // Defs are written after every use is read, so a def may share a use's scratch.
            let r = if used[ci] == 2 && o.kind == OperandKind::Def {
                self.scratch(class, 0)
            } else {
                used[ci] += 1;
                self.scratch(class, used[ci] - 1)
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
            l => panic!("x64: {v:?} expected in a register, found {l:?}"),
        }
    }

    /// `v` as an r/m operand: its register or its spill slot.
    fn rm(&self, v: VReg) -> RM {
        if let Some(m) = self.map.iter().find(|m| m.0 == v) {
            return RM::Reg(m.1.hw);
        }
        self.loc_rm(self.loc(v))
    }

    fn amode(&self, a: &Amode) -> RM {
        RM::Mem {
            base: self.r(a.base),
            index: a.index.map(|i| (self.r(i), a.scale)),
            disp: a.disp,
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

    /// A 16-byte constant-pool entry.
    fn constant(&mut self, lo: u64, hi: u64) -> Label {
        if let Some(&(_, _, l)) = self.consts.iter().find(|c| c.0 == lo && c.1 == hi) {
            return l;
        }
        let l = self.a.new_label();
        self.consts.push((lo, hi, l));
        l
    }

    /// Jump to `l` when `cc` holds.
    fn jcc_to(&mut self, cc: CC, l: Label) {
        match cc {
            CC::FEq => {
                let skip = self.a.new_label();
                self.a.jcc(CC::P.code(), skip);
                self.a.jcc(CC::E.code(), l);
                self.a.bind(skip);
            }
            CC::FNe => {
                self.a.jcc(CC::NE.code(), l);
                self.a.jcc(CC::P.code(), l);
            }
            c => self.a.jcc(c.code(), l),
        }
    }

    // ----- function -----

    pub fn emit(mut self) -> Result<(Vec<u8>, Vec<bool>, Vec<Reloc>), String> {
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
        // Out-of-line trap stubs: unwind to the entry trampoline with the trap code.
        let traps = std::mem::take(&mut self.trap_labels);
        for (code, l) in traps {
            let tc = self.traps.ok_or("x64: traps need a TrapConfig")?;
            self.a.bind(l);
            self.a
                .mov_load(true, asm::R10, RM::mem(asm::RBP, self.frame.ctx_off));
            self.a
                .mov_load(true, asm::RSP, RM::mem(asm::R10, tc.entry_sp_offset));
            self.a.mov_imm(asm::RAX, code as u64 + 1, true);
            self.a.ret();
        }
        for (table, targets) in std::mem::take(&mut self.tables) {
            self.a.align(4, 0xcc);
            self.a.bind(table);
            for t in targets {
                let tl = self.blocks[t];
                self.a.table_entry(tl, table);
            }
        }
        if !self.consts.is_empty() {
            self.a.align(16, 0xcc);
            for (lo, hi, l) in std::mem::take(&mut self.consts) {
                self.a.bind(l);
                self.a.u64(lo);
                self.a.u64(hi);
            }
        }
        let fits = self.a.finish()?;
        Ok((self.a.buf, fits, self.relocs))
    }

    fn prologue(&mut self) {
        let fr = self.frame;
        self.a.push(asm::RBP);
        self.a.mov_rr(asm::RBP, asm::RSP);
        for &g in &fr.gprs {
            self.a.push(g);
        }
        if fr.size > 0 {
            // Touch each page in order so the OS guard page grows the stack (Win64).
            for k in 1..=fr.size / 4096 {
                let disp = -(4096 * k as i32);
                self.a.op(0, false, &[0x85], asm::RAX, RM::mem(asm::RSP, disp), false, 0);
            }
            self.a.alu_imm(true, 5, RM::Reg(asm::RSP), fr.size as i32);
        }
        for (k, &x) in fr.xmms.iter().enumerate() {
            let m = RM::mem(asm::RBP, fr.xmm_off + 16 * k as i32);
            self.a.op(0xf3, false, &[0x0f, 0x7f], x, m, false, 0);
        }
        let a0 = self.abi.int_args[0].hw;
        if fr.needs_ctx {
            self.a.mov_store(true, a0, RM::mem(asm::RBP, fr.ctx_off));
        }
        if let Some(tc) = self.traps {
            if let Some((off, code)) = tc.stack_limit {
                // cmp rsp, [ctx + limit]; jb overflow
                self.a.op(0, true, &[0x3b], asm::RSP, RM::mem(a0, off), false, 0);
                let l = self.trap_label(code);
                self.a.jcc(CC::B.code(), l);
            }
        }
    }

    fn epilogue(&mut self) {
        let fr = self.frame;
        for (k, &x) in fr.xmms.iter().enumerate() {
            let m = RM::mem(asm::RBP, fr.xmm_off + 16 * k as i32);
            self.a.op(0xf3, false, &[0x0f, 0x6f], x, m, false, 0);
        }
        if fr.gprs.is_empty() {
            self.a.mov_rr(asm::RSP, asm::RBP);
        } else {
            let disp = -(8 * fr.gprs.len() as i32);
            self.a.op(0, true, &[0x8d], asm::RSP, RM::mem(asm::RBP, disp), false, 0);
            for &g in fr.gprs.iter().rev() {
                self.a.pop(g);
            }
        }
        self.a.pop(asm::RBP);
        self.a.ret();
    }

    fn inst(&mut self, inst: &MInst, bi: usize) -> Result<(), String> {
        if let MInst::Store {
            kind,
            src,
            addr:
                Amode {
                    base,
                    index: Some(index),
                    scale,
                    disp,
                },
        } = inst
        {
            // Base, index and value all spilled: three reloads, two scratch registers.
            if self.code.class(*src) == RegClass::Int
                && [base, index, src].iter().all(|v| !is_reg(self.loc(**v)))
            {
                let int = RegClass::Int;
                self.move_loc(self.loc(*base), Loc::Reg(super::regs::R10), int);
                self.move_loc(self.loc(*index), Loc::Reg(super::regs::R11), int);
                let m = RM::Mem {
                    base: asm::R10,
                    index: Some((asm::R11, *scale)),
                    disp: *disp,
                };
                self.a.op(0, true, &[0x8d], asm::R10, m, false, 0);
                self.move_loc(self.loc(*src), Loc::Reg(super::regs::R11), int);
                self.store(*kind, asm::R11, RM::mem(asm::R10, 0));
                return Ok(());
            }
        }
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
                for &(v, off) in stack {
                    let class = self.code.class(v);
                    let src = RM::mem(asm::RBP, off);
                    match self.loc(v) {
                        Loc::Reg(r) if class == RegClass::Int => self.a.mov_load(true, r.hw, src),
                        Loc::Reg(r) => self.movsd_load(r.hw, src),
                        Loc::Stack(s) => {
                            self.a.mov_load(true, asm::R10, src);
                            self.a.mov_store(true, asm::R10, self.frame.slot(s));
                        }
                        Loc::None => {}
                    }
                }
            }
            MInst::Mov { size, dst, src } => {
                let (s, d) = (self.loc(*src), self.loc(*dst));
                match (size, d) {
                    (_, Loc::None) => {}
                    (Size::S64, _) => self.move_loc(s, d, RegClass::Int),
                    (Size::S32, Loc::Reg(d)) => self.a.mov_load(false, d.hw, self.loc_rm(s)),
                    (Size::S32, _) => {
                        self.a.mov_load(false, asm::R10, self.loc_rm(s));
                        self.a.mov_store(true, asm::R10, self.loc_rm(d));
                    }
                }
            }
            MInst::MovImm { dst, imm, .. } => match self.loc(*dst) {
                Loc::None => {}
                Loc::Reg(r) => self.a.mov_imm(r.hw, *imm, false),
                l => {
                    if let Ok(i) = i32::try_from(*imm as i64) {
                        self.a.op(0, true, &[0xc7], 0, self.loc_rm(l), false, 4);
                        self.a.u32(i as u32);
                    } else {
                        self.a.mov_imm(asm::R10, *imm, false);
                        self.a.mov_store(true, asm::R10, self.loc_rm(l));
                    }
                }
            },
            MInst::Alu { op, size, dst, src } => {
                let d = self.r(*dst);
                match src {
                    RegImm::Imm(i) => self.a.alu_imm(size.w(), op.digit(), RM::Reg(d), *i),
                    RegImm::Reg(s) => {
                        let rm = self.rm(*s);
                        self.a.op(0, size.w(), &[op.digit() << 3 | 3], d, rm, false, 0);
                    }
                }
            }
            MInst::Cmp { size, a, b } => {
                let ra = self.r(*a);
                match b {
                    RegImm::Imm(i) => self.a.alu_imm(size.w(), 7, RM::Reg(ra), *i),
                    RegImm::Reg(s) => {
                        let rm = self.rm(*s);
                        self.a.op(0, size.w(), &[0x3b], ra, rm, false, 0);
                    }
                }
            }
            MInst::Test { size, a, b } => {
                let (ra, rb) = (self.r(*a), self.r(*b));
                self.a.op(0, size.w(), &[0x85], rb, RM::Reg(ra), false, 0);
            }
            MInst::Imul { size, dst, src } => {
                let d = self.r(*dst);
                let rm = self.rm(*src);
                self.a.op(0, size.w(), &[0x0f, 0xaf], d, rm, false, 0);
            }
            MInst::Imul3 { size, dst, src, imm } => {
                let d = self.r(*dst);
                let rm = self.rm(*src);
                if let Ok(b) = i8::try_from(*imm) {
                    self.a.op(0, size.w(), &[0x6b], d, rm, false, 1);
                    self.a.byte(b as u8);
                } else {
                    self.a.op(0, size.w(), &[0x69], d, rm, false, 4);
                    self.a.u32(*imm as u32);
                }
            }
            MInst::Neg { size, dst } => {
                let d = self.r(*dst);
                self.a.op(0, size.w(), &[0xf7], 3, RM::Reg(d), false, 0);
            }
            MInst::Not { size, dst } => {
                let d = self.r(*dst);
                self.a.op(0, size.w(), &[0xf7], 2, RM::Reg(d), false, 0);
            }
            MInst::ShiftImm { op, size, dst, amt } => {
                let d = self.r(*dst);
                self.a.op(0, size.w(), &[0xc1], *op as u8, RM::Reg(d), false, 1);
                self.a.byte(*amt);
            }
            MInst::ShiftCl { op, size, dst, amt } => {
                let src = self.loc(*amt);
                self.move_loc(src, Loc::Reg(super::regs::RCX), RegClass::Int);
                let d = self.r(*dst);
                self.a.op(0, size.w(), &[0xd3], *op as u8, RM::Reg(d), false, 0);
            }
            MInst::ShiftX {
                op,
                size,
                dst,
                src,
                amt,
            } => {
                let pp = match op {
                    ShiftOp::Shl => 1,
                    ShiftOp::Sar => 2,
                    ShiftOp::Shr => 3,
                    _ => return Err("x64: no BMI2 rotate by register".into()),
                };
                let (d, s, n) = (self.r(*dst), self.r(*src), self.r(*amt));
                self.a.vex38(pp, size.w(), 0xf7, d, n, RM::Reg(s));
            }
            MInst::Div {
                signed,
                size,
                rem,
                dividend,
                divisor,
                dst,
            } => {
                let src = self.loc(*dividend);
                self.move_loc(src, Loc::Reg(super::regs::RAX), RegClass::Int);
                if *signed {
                    if size.w() {
                        self.a.bytes(&[0x48, 0x99]);
                    } else {
                        self.a.byte(0x99);
                    }
                } else {
                    self.a.bytes(&[0x31, 0xd2]);
                }
                let rm = self.rm(*divisor);
                self.a.op(0, size.w(), &[0xf7], if *signed { 7 } else { 6 }, rm, false, 0);
                let out = if *rem { super::regs::RDX } else { super::regs::RAX };
                let d = self.loc(*dst);
                self.move_loc(Loc::Reg(out), d, RegClass::Int);
            }
            MInst::Bit { op, size, dst, src } => {
                let d = self.r(*dst);
                let rm = self.rm(*src);
                let w = size.w();
                let bits: u32 = if w { 64 } else { 32 };
                match op {
                    BitOp::Lzcnt => self.a.op(0xf3, w, &[0x0f, 0xbd], d, rm, false, 0),
                    BitOp::Tzcnt => self.a.op(0xf3, w, &[0x0f, 0xbc], d, rm, false, 0),
                    BitOp::Popcnt => self.a.op(0xf3, w, &[0x0f, 0xb8], d, rm, false, 0),
                    BitOp::ClzBsr => {
                        // clz = (bits - 1) ^ bsr, and (2 * bits - 1) ^ (bits - 1) = bits for 0.
                        self.a.op(0, w, &[0x0f, 0xbd], d, rm, false, 0);
                        self.a.mov_imm(asm::R11, 2 * bits as u64 - 1, true);
                        self.a.op(0, w, &[0x0f, 0x44], d, RM::Reg(asm::R11), false, 0);
                        self.a.alu_imm(w, 6, RM::Reg(d), bits as i32 - 1);
                    }
                    BitOp::CtzBsf => {
                        self.a.op(0, w, &[0x0f, 0xbc], d, rm, false, 0);
                        self.a.mov_imm(asm::R11, bits as u64, true);
                        self.a.op(0, w, &[0x0f, 0x44], d, RM::Reg(asm::R11), false, 0);
                    }
                }
            }
            MInst::MovX { ext, dst, src } => {
                let d = self.r(*dst);
                let rm = self.rm(*src);
                self.movx(*ext, d, rm);
            }
            MInst::Load { kind, dst, addr } => {
                let d = self.r(*dst);
                let m = self.amode(addr);
                match kind {
                    LoadKind::Plain(s) => self.a.mov_load(s.w(), d, m),
                    LoadKind::Ext(e) => self.movx(*e, d, m),
                    LoadKind::F32 => self.a.op(0xf3, false, &[0x0f, 0x10], d, m, false, 0),
                    LoadKind::F64 => self.movsd_load(d, m),
                }
            }
            MInst::Store { kind, src, addr } => {
                let s = self.r(*src);
                let m = self.amode(addr);
                self.store(*kind, s, m);
            }
            MInst::Lea { size, dst, addr } => {
                let d = self.r(*dst);
                let m = self.amode(addr);
                self.a.op(0, size.w(), &[0x8d], d, m, false, 0);
            }
            MInst::Setcc { cc, dst } => {
                let d = self.r(*dst);
                let set = |a: &mut Asm, c: CC, r: u8| {
                    a.op(0, false, &[0x0f, 0x90 + c.code()], 0, RM::Reg(r), true, 0)
                };
                match cc {
                    CC::FEq => {
                        set(&mut self.a, CC::E, d);
                        set(&mut self.a, CC::NP, asm::R11);
                        self.a.op(0, false, &[0x20], asm::R11, RM::Reg(d), true, 0);
                    }
                    CC::FNe => {
                        set(&mut self.a, CC::NE, d);
                        set(&mut self.a, CC::P, asm::R11);
                        self.a.op(0, false, &[0x08], asm::R11, RM::Reg(d), true, 0);
                    }
                    c => set(&mut self.a, *c, d),
                }
                self.a.op(0, false, &[0x0f, 0xb6], d, RM::Reg(d), true, 0);
            }
            MInst::Cmov { cc, size, dst, src } => {
                let d = self.r(*dst);
                let rm = self.rm(*src);
                let w = size.w();
                match cc {
                    CC::FNe => {
                        self.a.op(0, w, &[0x0f, 0x40 + CC::NE.code()], d, rm, false, 0);
                        self.a.op(0, w, &[0x0f, 0x40 + CC::P.code()], d, rm, false, 0);
                    }
                    CC::FEq => {
                        let skip = self.a.new_label();
                        self.a.jcc(CC::P.code(), skip);
                        self.a.op(0, w, &[0x0f, 0x40 + CC::E.code()], d, rm, false, 0);
                        self.a.bind(skip);
                    }
                    c => self.a.op(0, w, &[0x0f, 0x40 + c.code()], d, rm, false, 0),
                }
            }
            MInst::XmmMov { dst, src, .. } => {
                let (s, d) = (self.loc(*src), self.loc(*dst));
                self.move_loc(s, d, RegClass::Float);
            }
            MInst::XmmConst { double, dst, bits } => {
                let d = self.r(*dst);
                if *bits == 0 {
                    self.xorps(d, d);
                } else {
                    let l = self.constant(*bits, 0);
                    let pfx = if *double { 0xf2 } else { 0xf3 };
                    self.a.op(pfx, false, &[0x0f, 0x10], d, RM::Rip(l), false, 0);
                }
            }
            MInst::XmmAlu {
                op,
                double,
                dst,
                src,
            } => {
                let d = self.r(*dst);
                let rm = self.rm(*src);
                self.sse(*double, *op as u8, d, rm);
            }
            MInst::XmmSqrt { double, dst, src } => {
                let d = self.r(*dst);
                let rm = self.rm(*src);
                self.sse(*double, 0x51, d, rm);
            }
            MInst::XmmMinMax {
                min,
                double,
                dst,
                src,
            } => {
                // WebAssembly min/max: NaN propagates, and -0 orders below +0 (RyuJIT's
                // `Math.Min` expansion): equal operands may be ±0, so combine their bits.
                let (d, s) = (self.r(*dst), self.r(*src));
                let (nan, eq, done) = (self.a.new_label(), self.a.new_label(), self.a.new_label());
                self.ucomis(*double, d, RM::Reg(s));
                self.a.jcc(CC::P.code(), nan);
                self.a.jcc(CC::E.code(), eq);
                self.sse(*double, if *min { 0x5d } else { 0x5f }, d, RM::Reg(s));
                self.a.jmp(done);
                self.a.bind(eq);
                let bitop = if *min { 0x56 } else { 0x54 };
                self.a.op(0, false, &[0x0f, bitop], d, RM::Reg(s), false, 0);
                self.a.jmp(done);
                self.a.bind(nan);
                self.sse(*double, 0x58, d, RM::Reg(s));
                self.a.bind(done);
            }
            MInst::XmmMask { op, dst, mask, .. } => {
                let d = self.r(*dst);
                let l = self.constant(*mask, *mask);
                self.a.op(0, false, &[0x0f, *op as u8], d, RM::Rip(l), false, 0);
            }
            MInst::XmmRound {
                double,
                mode,
                dst,
                src,
            } => {
                let d = self.r(*dst);
                let rm = self.rm(*src);
                let opc = if *double { 0x0b } else { 0x0a };
                self.a.op(0x66, false, &[0x0f, 0x3a, opc], d, rm, false, 1);
                self.a.byte(*mode);
            }
            MInst::Ucomis { double, a, b } => {
                let ra = self.r(*a);
                let rm = self.rm(*b);
                self.ucomis(*double, ra, rm);
            }
            MInst::XmmCmov { cc, dst, src, .. } => {
                let (d, s) = (self.r(*dst), self.r(*src));
                let skip = self.a.new_label();
                self.jcc_to(cc.invert(), skip);
                self.movaps(d, s);
                self.a.bind(skip);
            }
            MInst::CvtIntToFloat {
                src_size,
                double,
                dst,
                src,
            } => {
                let d = self.r(*dst);
                let rm = self.rm(*src);
                // Break the dependency on the destination's old upper lanes.
                self.xorps(d, d);
                let pfx = if *double { 0xf2 } else { 0xf3 };
                self.a.op(pfx, src_size.w(), &[0x0f, 0x2a], d, rm, false, 0);
            }
            MInst::CvtFloatToInt {
                dst_size,
                double,
                dst,
                src,
            } => {
                let d = self.r(*dst);
                let rm = self.rm(*src);
                let pfx = if *double { 0xf2 } else { 0xf3 };
                self.a.op(pfx, dst_size.w(), &[0x0f, 0x2c], d, rm, false, 0);
            }
            MInst::CvtFloatFloat {
                to_double,
                dst,
                src,
            } => {
                let d = self.r(*dst);
                let rm = self.rm(*src);
                self.xorps(d, d);
                let pfx = if *to_double { 0xf3 } else { 0xf2 };
                self.a.op(pfx, false, &[0x0f, 0x5a], d, rm, false, 0);
            }
            MInst::GprToXmm { size, dst, src } => {
                let d = self.r(*dst);
                let rm = self.rm(*src);
                self.a.op(0x66, size.w(), &[0x0f, 0x6e], d, rm, false, 0);
            }
            MInst::XmmToGpr { size, dst, src } => {
                let d = self.r(*dst);
                match self.rm(*src) {
                    RM::Reg(s) => self.a.op(0x66, size.w(), &[0x0f, 0x7e], s, RM::Reg(d), false, 0),
                    m => self.a.mov_load(size.w(), d, m),
                }
            }
            MInst::Call {
                target,
                reg_args,
                stack_args,
                rets,
                ..
            } => {
                for &(v, off) in stack_args {
                    let dst = RM::mem(asm::RSP, off);
                    match (self.code.class(v), self.loc(v)) {
                        (RegClass::Int, Loc::Reg(r)) => self.a.mov_store(true, r.hw, dst),
                        (RegClass::Float, Loc::Reg(r)) => self.movsd_store(r.hw, dst),
                        (_, l) => {
                            self.a.mov_load(true, asm::R10, self.loc_rm(l));
                            self.a.mov_store(true, asm::R10, dst);
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
                        // mov r11, imm64 (patched); call r11
                        self.a.bytes(&[0x49, 0xbb]);
                        self.relocs.push(Reloc {
                            offset: self.a.pos(),
                            func_id: *id,
                        });
                        self.a.u64(0);
                        self.a.bytes(&[0x41, 0xff, 0xd3]);
                    }
                    CallTarget::Reg(v) => {
                        let rm = self.rm(*v);
                        self.a.op(0, false, &[0xff], 2, rm, false, 0);
                    }
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
                    self.a.jmp(l);
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
                        self.a.jmp(t);
                    }
                } else if *not_taken == bi + 1 {
                    self.jcc_to(*cc, t);
                } else if *taken == bi + 1 {
                    self.jcc_to(cc.invert(), n);
                } else {
                    self.jcc_to(*cc, t);
                    self.a.jmp(n);
                }
            }
            MInst::BrTable {
                index,
                targets,
                default,
            } => {
                let i = self.r(*index);
                self.a.alu_imm(false, 7, RM::Reg(i), targets.len() as i32);
                let dl = self.blocks[*default];
                self.a.jcc(CC::AE.code(), dl);
                let table = self.a.new_label();
                // lea r11, [rip + table]; movsxd r10, [r11 + i*4]; add r10, r11; jmp r10
                self.a.op(0, true, &[0x8d], asm::R11, RM::Rip(table), false, 0);
                let entry = RM::Mem {
                    base: asm::R11,
                    index: Some((i, 2)),
                    disp: 0,
                };
                self.a.op(0, true, &[0x63], asm::R10, entry, false, 0);
                self.a.op(0, true, &[0x01], asm::R11, RM::Reg(asm::R10), false, 0);
                self.a.op(0, false, &[0xff], 4, RM::Reg(asm::R10), false, 0);
                self.tables.push((table, targets.clone()));
            }
            MInst::TrapIf { cc, code } => {
                let l = self.trap_label(*code);
                self.jcc_to(*cc, l);
            }
            MInst::Trap { code } => {
                let l = self.trap_label(*code);
                self.a.jmp(l);
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

    fn store(&mut self, kind: StoreKind, s: u8, m: RM) {
        match kind {
            StoreKind::B8 => self.a.op(0, false, &[0x88], s, m, true, 0),
            StoreKind::B16 => self.a.op(0x66, false, &[0x89], s, m, false, 0),
            StoreKind::B32 => self.a.mov_store(false, s, m),
            StoreKind::B64 => self.a.mov_store(true, s, m),
            StoreKind::F32 => self.a.op(0xf3, false, &[0x0f, 0x11], s, m, false, 0),
            StoreKind::F64 => self.movsd_store(s, m),
        }
    }

    fn sse(&mut self, double: bool, opc: u8, d: u8, rm: RM) {
        let pfx = if double { 0xf2 } else { 0xf3 };
        self.a.op(pfx, false, &[0x0f, opc], d, rm, false, 0);
    }

    fn ucomis(&mut self, double: bool, a: u8, rm: RM) {
        let pfx = if double { 0x66 } else { 0 };
        self.a.op(pfx, false, &[0x0f, 0x2e], a, rm, false, 0);
    }

    fn movx(&mut self, ext: Ext, d: u8, rm: RM) {
        match ext {
            Ext::S { from: 8, to } => self.a.op(0, to.w(), &[0x0f, 0xbe], d, rm, true, 0),
            Ext::S { from: 16, to } => self.a.op(0, to.w(), &[0x0f, 0xbf], d, rm, false, 0),
            Ext::S { .. } => self.a.op(0, true, &[0x63], d, rm, false, 0),
            Ext::Z { from: 8 } => self.a.op(0, false, &[0x0f, 0xb6], d, rm, true, 0),
            Ext::Z { .. } => self.a.op(0, false, &[0x0f, 0xb7], d, rm, false, 0),
        }
    }
}
