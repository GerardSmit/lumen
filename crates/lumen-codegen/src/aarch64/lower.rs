//! IR → AArch64 [`MInst`]s over virtual registers.
//!
//! Selection mirrors the x64 lowering (docs/jit-notes/lowering-emit.md):
//! - integer constants that fit an instruction's immediate field (add/sub imm12, logical
//!   bitmask immediates, shift amounts, compare imm12) become immediates of their user;
//!   constants needed in a register are rematerialized at each use;
//! - a compare whose only use is a `brif`/`select`/`trap_if` in the same block is emitted right
//!   before it and feeds `b.cond`/`csel` directly from the flags; any other condition is tested
//!   with `cbnz` (or `cmp #0`);
//! - an `iadd` whose only use is a load or store address folds into the address mode
//!   (`[base, #imm]` or `[base, index]`);
//! - `uext` is free: I32 values are always kept zero-extended in their 64-bit register.

use super::asm::{logical_imm, Cond, FOp1, FOp2, LdSt, LogImm, Rrr};
use super::inst::*;
use super::regs::*;
use crate::cfg::Cfg;
use crate::eval;
use crate::ir::*;
use crate::machinst::*;

pub struct Lowered {
    pub vcode: VCode<MInst>,
    /// Bytes of outgoing stack-argument space any call needs (a multiple of 8).
    pub outgoing: u32,
    pub has_traps: bool,
}

/// Where an argument lives: a register, or `(offset from the first stack slot, bytes)`.
#[derive(Clone, Copy, Debug)]
pub enum ArgLoc {
    Reg(PReg),
    Stack(i32, u8),
}

/// Assign argument locations (AAPCS64 §6.8.2; Apple packs stack arguments by natural size),
/// and the size of the stack argument area rounded up to 8.
pub fn arg_locs(abi: &Abi, tys: &[Type]) -> (Vec<ArgLoc>, u32) {
    let mut out = Vec::with_capacity(tys.len());
    let (mut ni, mut nf, mut off) = (0usize, 0usize, 0u32);
    for &t in tys {
        let (regs, n) = if t.is_float() {
            (abi.float_args(), &mut nf)
        } else {
            (abi.int_args(), &mut ni)
        };
        if *n < regs.len() {
            out.push(ArgLoc::Reg(regs[*n]));
            *n += 1;
            continue;
        }
        let bytes = t.bits() / 8;
        let slot = if abi.apple { bytes } else { 8 };
        off = off.div_ceil(slot) * slot;
        out.push(ArgLoc::Stack(off as i32, bytes as u8));
        off += slot;
    }
    (out, off.div_ceil(8) * 8)
}

pub fn ret_reg(t: Type) -> PReg {
    if t.is_float() {
        V0
    } else {
        X0
    }
}

fn size_of(t: Type) -> Size {
    if t.bits() == 64 {
        Size::S64
    } else {
        Size::S32
    }
}

fn class_of(t: Type) -> RegClass {
    if t.is_float() {
        RegClass::Float
    } else {
        RegClass::Int
    }
}

fn int_cc(cc: IntCC) -> Cond {
    match cc {
        IntCC::Eq => Cond::Eq,
        IntCC::Ne => Cond::Ne,
        IntCC::Slt => Cond::Lt,
        IntCC::Sle => Cond::Le,
        IntCC::Sgt => Cond::Gt,
        IntCC::Sge => Cond::Ge,
        IntCC::Ult => Cond::Lo,
        IntCC::Ule => Cond::Ls,
        IntCC::Ugt => Cond::Hi,
        IntCC::Uge => Cond::Hs,
    }
}

/// After `fcmp a, b`: unordered sets N=0 Z=0 C=1 V=1, so each ordered condition is a single
/// code that is false for NaN (and `Ne`, true when unordered, is plain `ne`).
fn float_cc(cc: FloatCC) -> Cond {
    match cc {
        FloatCC::Eq => Cond::Eq,
        FloatCC::Ne => Cond::Ne,
        FloatCC::Lt => Cond::Mi,
        FloatCC::Le => Cond::Ls,
        FloatCC::Gt => Cond::Gt,
        FloatCC::Ge => Cond::Ge,
    }
}

/// `v` as an add/sub/cmp immediate: `imm12` or `imm12 << 12`.
fn arith_imm(v: u64) -> Option<(u16, bool)> {
    if v < 4096 {
        Some((v as u16, false))
    } else if v & 0xfff == 0 && v >> 12 < 4096 {
        Some(((v >> 12) as u16, true))
    } else {
        None
    }
}

fn neg(v: u64, size: Size) -> u64 {
    match size {
        Size::S32 => (v as u32).wrapping_neg() as u64,
        Size::S64 => v.wrapping_neg(),
    }
}

fn load_op(k: MemKind) -> LdSt {
    use MemKind::*;
    match k {
        I32 | I64U32 => LdSt::LdrW,
        I64 => LdSt::LdrX,
        F32 => LdSt::LdrS,
        F64 => LdSt::LdrD,
        I32S8 => LdSt::LdrSb32,
        I32U8 | I64U8 => LdSt::LdrB,
        I32S16 => LdSt::LdrSh32,
        I32U16 | I64U16 => LdSt::LdrH,
        I64S8 => LdSt::LdrSb64,
        I64S16 => LdSt::LdrSh64,
        I64S32 => LdSt::LdrSw,
    }
}

fn store_op(k: MemKind) -> LdSt {
    match (k, k.bytes()) {
        (MemKind::F32, _) => LdSt::StrS,
        (MemKind::F64, _) => LdSt::StrD,
        (_, 1) => LdSt::StrB,
        (_, 2) => LdSt::StrH,
        (_, 4) => LdSt::StrW,
        _ => LdSt::StrX,
    }
}

struct Lower<'a> {
    f: &'a Function,
    abi: &'a Abi,
    vc: VCode<MInst>,
    vregs: Vec<Option<VReg>>,
    /// `uext` results map to their operand.
    alias: Vec<Value>,
    /// Instructions emitted at their single user instead of at their definition.
    sunk: Vec<bool>,
    bmap: Vec<usize>,
    outgoing: u32,
    has_traps: bool,
}

pub fn lower(f: &Function, cfg: &Cfg, abi: &Abi) -> Result<Lowered, String> {
    if f.sig.results.len() > 1 {
        return Err("aarch64: multiple results are not supported".into());
    }
    let nv = f.values.len();
    let mut alias: Vec<Value> = (0..nv as u32).map(Value).collect();
    let mut uses = vec![0u32; nv];
    let mut user: Vec<Option<Inst>> = vec![None; nv];
    let mut inst_block: Vec<Option<Block>> = vec![None; f.insts.len()];
    for &b in &cfg.rpo {
        for &i in &f.blocks[b.index()].insts {
            inst_block[i.index()] = Some(b);
            f.inst(i).for_each_arg(|a| {
                uses[a.index()] += 1;
                user[a.index()] = Some(i);
            });
            if let InstData::Convert {
                op: ConvOp::Uext,
                arg,
                ..
            } = f.inst(i)
            {
                let r = f.results(i)[0];
                alias[r.index()] = alias[arg.index()];
            }
        }
    }
    let mut sunk = vec![false; nv];
    for &b in &cfg.rpo {
        for &i in &f.blocks[b.index()].insts {
            let [r] = f.results(i) else { continue };
            let Some(u) = user[r.index()] else { continue };
            if uses[r.index()] != 1 || inst_block[u.index()] != Some(b) {
                continue;
            }
            let ok = match f.inst(i) {
                InstData::IntCmp { .. } | InstData::FloatCmp { .. } => matches!(f.inst(u),
                    InstData::Brif { cond, .. } | InstData::Select { cond, .. }
                        | InstData::TrapIf { cond, .. } if cond == r),
                InstData::Binary {
                    op: BinaryOp::Iadd, ..
                } => {
                    f.value_type(*r) == Type::I64
                        && matches!(f.inst(u),
                            InstData::Load { addr, .. } | InstData::Store { addr, .. } if addr == r)
                }
                _ => false,
            };
            sunk[r.index()] = ok;
        }
    }

    let entry = f.entry();
    let pre_entry = !cfg.preds[entry.index()].is_empty();
    let order = crate::cfg::sink_cold(
        f,
        cfg,
        rotate_loops(
            f,
            cfg,
            f.layout
                .iter()
                .copied()
                .filter(|&b| cfg.is_reachable(b))
                .collect(),
        ),
    );
    let mut bmap = vec![usize::MAX; f.blocks.len()];
    for (i, &b) in order.iter().enumerate() {
        bmap[b.index()] = i + pre_entry as usize;
    }

    let mut l = Lower {
        f,
        abi,
        vc: VCode::new(),
        vregs: vec![None; nv],
        alias,
        sunk,
        bmap,
        outgoing: 0,
        has_traps: false,
    };

    if pre_entry {
        let params: Vec<VReg> = f.blocks[entry.index()]
            .params
            .iter()
            .map(|&p| l.vc.new_vreg(class_of(f.value_type(p))))
            .collect();
        l.args(&params);
        let args = params.clone();
        l.vc.insts.push(MInst::Jmp { target: 1, args });
        l.vc.blocks.push(VBlock {
            params: Vec::new(),
            start: 0,
            end: l.vc.insts.len(),
            succs: vec![1],
            loop_depth: 0,
        });
    }

    for &b in &order {
        let start = l.vc.insts.len();
        let bparams: Vec<Value> = f.blocks[b.index()].params.clone();
        let params = if b == entry && !pre_entry {
            let targets: Vec<VReg> = bparams.iter().map(|&p| l.vreg(p)).collect();
            l.args(&targets);
            Vec::new()
        } else {
            bparams.iter().map(|&p| l.vreg(p)).collect()
        };
        for &i in &f.blocks[b.index()].insts {
            l.inst(i)?;
        }
        let succs = f.successors(b).iter().map(|s| l.bmap[s.index()]).collect();
        l.vc.blocks.push(VBlock {
            params,
            start,
            end: l.vc.insts.len(),
            succs,
            loop_depth: cfg.loop_depth[b.index()],
        });
    }

    Ok(Lowered {
        vcode: l.vc,
        outgoing: l.outgoing,
        has_traps: l.has_traps,
    })
}

/// Loop rotation by layout (as on x64): a loop header that ends in a conditional branch moves
/// to just after its last latch, so the back edge falls through into the exit test and each
/// iteration takes one branch instead of two.
fn rotate_loops(f: &Function, cfg: &Cfg, mut order: Vec<Block>) -> Vec<Block> {
    let is_brif = |b: Block| {
        f.terminator(b)
            .is_some_and(|t| matches!(f.inst(t), InstData::Brif { .. }))
    };
    let is_jump_to = |b: Block, h: Block| {
        f.terminator(b)
            .is_some_and(|t| matches!(f.inst(t), InstData::Jump { dest } if dest.block == h))
    };
    let headers: Vec<Block> = order
        .iter()
        .copied()
        .filter(|&h| h != f.entry() && is_brif(h))
        .collect();
    for h in headers {
        let pos = |order: &[Block], b: Block| order.iter().position(|&x| x == b);
        // Only a loop header has a latch: skip the linear searches for every other branch.
        if !cfg.preds[h.index()].iter().any(|&p| cfg.dominates(h, p)) {
            continue;
        }
        let Some(hp) = pos(&order, h) else { continue };
        let latch = cfg.preds[h.index()]
            .iter()
            .copied()
            .filter(|&p| cfg.dominates(h, p))
            .filter_map(|p| pos(&order, p).map(|i| (i, p)))
            .max();
        let Some((lp, l)) = latch else { continue };
        if lp <= hp || !is_jump_to(l, h) {
            continue;
        }
        order.remove(hp);
        order.insert(lp, h);
    }
    order
}

impl Lower<'_> {
    fn push(&mut self, i: MInst) {
        self.vc.insts.push(i);
    }

    fn ty(&self, v: Value) -> Type {
        self.f.value_type(v)
    }

    fn vreg(&mut self, v: Value) -> VReg {
        let v = self.alias[v.index()];
        if let Some(r) = self.vregs[v.index()] {
            return r;
        }
        let r = self.vc.new_vreg(class_of(self.ty(v)));
        self.vregs[v.index()] = Some(r);
        r
    }

    fn fresh(&mut self, t: Type) -> VReg {
        self.vc.new_vreg(class_of(t))
    }

    fn def(&self, v: Value) -> Option<&InstData> {
        match self.f.values[self.alias[v.index()].index()].def {
            ValueDef::Result(i, 0) => Some(self.f.inst(i)),
            _ => None,
        }
    }

    /// The constant bits of an integer constant (normalized to its type).
    fn iconst(&self, v: Value) -> Option<u64> {
        match self.def(v) {
            Some(InstData::Iconst { ty, imm }) => Some(eval::iconst(*ty, *imm)),
            _ => None,
        }
    }

    /// `v` in a virtual register, rematerializing constants at the use.
    fn use_val(&mut self, v: Value) -> VReg {
        match self.def(v).cloned() {
            Some(InstData::Iconst { .. })
            | Some(InstData::F32const { .. })
            | Some(InstData::F64const { .. }) => {
                let r = self.fresh(self.ty(self.alias[v.index()]));
                self.copy_into(r, v);
                r
            }
            _ => self.vreg(v),
        }
    }

    /// `dst = v`
    fn copy_into(&mut self, dst: VReg, v: Value) {
        match self.def(v).cloned() {
            Some(InstData::Iconst { .. }) => {
                let imm = self.iconst(v).unwrap();
                self.push(MInst::MovImm { dst, imm });
            }
            Some(InstData::F32const { bits }) => self.push(MInst::FConst {
                double: false,
                dst,
                bits: bits as u64,
            }),
            Some(InstData::F64const { bits }) => self.push(MInst::FConst {
                double: true,
                dst,
                bits,
            }),
            _ => {
                let src = self.vreg(v);
                if self.ty(v).is_float() {
                    self.push(MInst::FMov { dst, src });
                } else {
                    self.push(MInst::Mov {
                        size: Size::S64,
                        dst,
                        src,
                    });
                }
            }
        }
    }

    fn mov_imm(&mut self, imm: u64) -> VReg {
        let r = self.vc.new_vreg(RegClass::Int);
        self.push(MInst::MovImm { dst: r, imm });
        r
    }

    /// Function entry: move parameters from their ABI locations into `targets`, normalizing
    /// I32 parameters (the ABI leaves their upper halves undefined).
    fn args(&mut self, targets: &[VReg]) {
        let tys = self.f.sig.params.clone();
        let (locs, _) = arg_locs(self.abi, &tys);
        let mut regs = Vec::new();
        let mut stack = Vec::new();
        let mut post = Vec::new();
        for (i, (&t, loc)) in tys.iter().zip(locs).enumerate() {
            let d = if t == Type::I32 {
                let tmp = self.fresh(t);
                post.push(MInst::Mov {
                    size: Size::S32,
                    dst: targets[i],
                    src: tmp,
                });
                tmp
            } else {
                targets[i]
            };
            match loc {
                ArgLoc::Reg(r) => regs.push((d, r)),
                ArgLoc::Stack(off, bytes) => stack.push((d, off, bytes)),
            }
        }
        self.push(MInst::Args { regs, stack });
        for m in post {
            self.push(m);
        }
    }

    fn int_cmp(&mut self, cc: IntCC, a: Value, b: Value) -> Cond {
        let size = size_of(self.ty(a));
        let imm = |l: &Self, v: Value| -> Option<CmpRhs> {
            let c = l.iconst(v)?;
            if let Some((imm12, shift)) = arith_imm(c) {
                return Some(CmpRhs::Imm {
                    imm12,
                    shift,
                    neg: false,
                });
            }
            let (imm12, shift) = arith_imm(neg(c, size))?;
            Some(CmpRhs::Imm {
                imm12,
                shift,
                neg: true,
            })
        };
        let (x, rhs, cc) = if let Some(rhs) = imm(self, b) {
            (a, rhs, cc)
        } else if let Some(rhs) = imm(self, a) {
            (b, rhs, cc.swap())
        } else {
            let rb = self.use_val(b);
            (a, CmpRhs::Reg(rb), cc)
        };
        let ra = self.use_val(x);
        self.push(MInst::Cmp {
            size,
            a: ra,
            b: rhs,
        });
        int_cc(cc)
    }

    fn float_cmp(&mut self, cc: FloatCC, a: Value, b: Value) -> Cond {
        let double = self.ty(a) == Type::F64;
        let ra = self.use_val(a);
        let rb = self.use_val(b);
        self.push(MInst::FCmp {
            double,
            a: ra,
            b: rb,
        });
        float_cc(cc)
    }

    /// The sunk compare feeding `cond`, emitted now; `None` when `cond` is a plain value.
    fn sunk_cond(&mut self, cond: Value) -> Option<Cond> {
        if !self.sunk[cond.index()] {
            return None;
        }
        match self.def(cond).cloned() {
            Some(InstData::IntCmp { cc, args }) => Some(self.int_cmp(cc, args[0], args[1])),
            Some(InstData::FloatCmp { cc, args }) => Some(self.float_cmp(cc, args[0], args[1])),
            _ => None,
        }
    }

    /// Set the flags for `cond != 0` and return the condition to test. Nothing that clobbers
    /// flags may be emitted between this and the consumer.
    fn cond(&mut self, cond: Value) -> Cond {
        if let Some(c) = self.sunk_cond(cond) {
            return c;
        }
        let r = self.use_val(cond);
        self.push(MInst::Cmp {
            size: Size::S32,
            a: r,
            b: CmpRhs::Imm {
                imm12: 0,
                shift: false,
                neg: false,
            },
        });
        Cond::Ne
    }

    fn amode(&mut self, addr: Value, offset: i32, op: LdSt) -> Amode {
        let mut base = None;
        if self.sunk[addr.index()] {
            if let Some(InstData::Binary { args: [a, b], .. }) = self.def(addr).cloned() {
                for (x, y) in [(a, b), (b, a)] {
                    let disp = self
                        .iconst(y)
                        .and_then(|c| i32::try_from(c as i64).ok())
                        .and_then(|c| c.checked_add(offset));
                    if let Some(d) = disp.filter(|&d| op.offset_ok(d as i64)) {
                        return Amode::Imm(self.use_val(x), d);
                    }
                }
                let (ra, rb) = (self.use_val(a), self.use_val(b));
                if offset == 0 {
                    return Amode::Reg(ra, rb);
                }
                let t = self.vc.new_vreg(RegClass::Int);
                self.push(MInst::Rrr {
                    op: Rrr::Add,
                    size: Size::S64,
                    dst: t,
                    a: ra,
                    b: rb,
                });
                base = Some(t);
            }
        }
        let base = match base {
            Some(b) => b,
            None => self.use_val(addr),
        };
        if op.offset_ok(offset as i64) {
            Amode::Imm(base, offset)
        } else {
            let idx = self.mov_imm(offset as i64 as u64);
            Amode::Reg(base, idx)
        }
    }

    fn call(
        &mut self,
        sig: &Signature,
        target: CallTarget,
        args: &[Value],
        results: &[Value],
    ) -> Result<(), String> {
        if sig.results.len() > 1 {
            return Err("aarch64: calls with multiple results are not supported".into());
        }
        let vals: Vec<VReg> = args.iter().map(|&a| self.use_val(a)).collect();
        let (locs, size) = arg_locs(self.abi, &sig.params);
        self.outgoing = self.outgoing.max(size);
        let mut reg_args = Vec::new();
        let mut stack_args = Vec::new();
        for (v, loc) in vals.into_iter().zip(locs) {
            match loc {
                ArgLoc::Reg(r) => reg_args.push((v, r)),
                ArgLoc::Stack(off, bytes) => stack_args.push((v, off, bytes)),
            }
        }
        let mut rets = Vec::new();
        let mut post = None;
        if let (Some(&t), Some(&r)) = (sig.results.first(), results.first()) {
            let dst = self.vreg(r);
            if t == Type::I32 {
                let tmp = self.fresh(t);
                rets.push((tmp, ret_reg(t)));
                post = Some(MInst::Mov {
                    size: Size::S32,
                    dst,
                    src: tmp,
                });
            } else {
                rets.push((dst, ret_reg(t)));
            }
        }
        self.push(MInst::Call {
            target,
            reg_args,
            stack_args,
            rets,
            clobbers: self.abi.call_clobbers,
        });
        if let Some(m) = post {
            self.push(m);
        }
        Ok(())
    }

    fn inst(&mut self, i: Inst) -> Result<(), String> {
        let f = self.f;
        let data = f.inst(i);
        let res = f.results(i);
        if let [r] = res {
            if self.sunk[r.index()] {
                return Ok(());
            }
        }
        match data {
            InstData::Iconst { .. } | InstData::F32const { .. } | InstData::F64const { .. } => {}
            InstData::Unary { op, arg } => self.unary(*op, *arg, res[0])?,
            InstData::Binary { op, args } => self.binary(*op, args[0], args[1], res[0])?,
            InstData::IntCmp { cc, args } => {
                let dst = self.vreg(res[0]);
                let cc = self.int_cmp(*cc, args[0], args[1]);
                self.push(MInst::CSet { cc, dst });
            }
            InstData::FloatCmp { cc, args } => {
                let dst = self.vreg(res[0]);
                let cc = self.float_cmp(*cc, args[0], args[1]);
                self.push(MInst::CSet { cc, dst });
            }
            InstData::Select {
                cond,
                if_true,
                if_false,
            } => {
                let dst = self.vreg(res[0]);
                let t = self.use_val(*if_true);
                let fv = self.use_val(*if_false);
                let cc = self.cond(*cond);
                self.push(if self.ty(res[0]).is_float() {
                    MInst::FCSel { cc, dst, t, f: fv }
                } else {
                    MInst::CSel { cc, dst, t, f: fv }
                });
            }
            InstData::Convert { op, to, arg } => self.convert(*op, *to, *arg, res[0])?,
            InstData::Load { kind, addr, offset } => {
                let op = load_op(*kind);
                let addr = self.amode(*addr, *offset, op);
                let dst = self.vreg(res[0]);
                self.push(MInst::Load { op, dst, addr });
            }
            InstData::Store {
                kind,
                addr,
                value,
                offset,
            } => {
                let op = store_op(*kind);
                let src = self.use_val(*value);
                let addr = self.amode(*addr, *offset, op);
                self.push(MInst::Store { op, src, addr });
            }
            InstData::Call { func, args } => {
                let ext = &f.funcs[func.index()];
                let sig = f.sigs[ext.sig.index()].clone();
                self.call(&sig, CallTarget::Func(ext.id), args, res)?;
            }
            InstData::CallIndirect { sig, callee, args } => {
                let sig = f.sigs[sig.index()].clone();
                let c = self.use_val(*callee);
                self.call(&sig, CallTarget::Reg(c), args, res)?;
            }
            InstData::Trap { code } => {
                self.has_traps = true;
                self.push(MInst::Trap { code: *code });
            }
            InstData::TrapIf { cond, code } => {
                self.has_traps = true;
                match self.sunk_cond(*cond) {
                    Some(cc) => self.push(MInst::TrapIf { cc, code: *code }),
                    None => {
                        let reg = self.use_val(*cond);
                        self.push(MInst::TrapNz { reg, code: *code });
                    }
                }
            }
            InstData::Jump { dest } => {
                let args = dest.args.iter().map(|&a| self.use_val(a)).collect();
                self.push(MInst::Jmp {
                    target: self.bmap[dest.block.index()],
                    args,
                });
            }
            InstData::Brif { cond, then, else_ } => {
                if !then.args.is_empty() || !else_.args.is_empty() {
                    return Err("aarch64: brif with arguments (critical edges not split)".into());
                }
                let taken = self.bmap[then.block.index()];
                let not_taken = self.bmap[else_.block.index()];
                match self.sunk_cond(*cond) {
                    Some(cc) => self.push(MInst::Jcc {
                        cc,
                        taken,
                        not_taken,
                    }),
                    None => {
                        let reg = self.use_val(*cond);
                        self.push(MInst::Cbnz {
                            reg,
                            taken,
                            not_taken,
                        });
                    }
                }
            }
            InstData::BrTable {
                index,
                targets,
                default,
            } => {
                if targets.iter().chain([default]).any(|t| !t.args.is_empty()) {
                    return Err(
                        "aarch64: br_table with arguments (critical edges not split)".into(),
                    );
                }
                let index = self.use_val(*index);
                self.push(MInst::BrTable {
                    index,
                    targets: targets.iter().map(|t| self.bmap[t.block.index()]).collect(),
                    default: self.bmap[default.block.index()],
                });
            }
            InstData::Return { args } => {
                let vals = args
                    .iter()
                    .map(|&a| {
                        let r = ret_reg(self.ty(a));
                        (self.use_val(a), r)
                    })
                    .collect();
                self.push(MInst::Ret { vals });
            }
        }
        Ok(())
    }

    fn unary(&mut self, op: UnaryOp, arg: Value, r: Value) -> Result<(), String> {
        let ty = self.ty(arg);
        let size = size_of(ty);
        let double = ty == Type::F64;
        let dst = self.vreg(r);
        let src = self.use_val(arg);
        let fop = match op {
            UnaryOp::Clz | UnaryOp::Ctz | UnaryOp::Popcnt => {
                let bop = match op {
                    UnaryOp::Clz => BitOp::Clz,
                    UnaryOp::Ctz => BitOp::Ctz,
                    _ => BitOp::Popcnt,
                };
                self.push(MInst::Bit {
                    op: bop,
                    size,
                    dst,
                    src,
                });
                return Ok(());
            }
            UnaryOp::Sext8 | UnaryOp::Sext16 | UnaryOp::Sext32 => {
                let from = match op {
                    UnaryOp::Sext8 => 8,
                    UnaryOp::Sext16 => 16,
                    _ => 32,
                };
                self.push(MInst::Sext {
                    from,
                    size,
                    dst,
                    src,
                });
                return Ok(());
            }
            UnaryOp::Fneg => FOp1::Neg,
            UnaryOp::Fabs => FOp1::Abs,
            UnaryOp::Sqrt => FOp1::Sqrt,
            UnaryOp::Ceil => FOp1::RintP,
            UnaryOp::Floor => FOp1::RintM,
            UnaryOp::Trunc => FOp1::RintZ,
            UnaryOp::Nearest => FOp1::RintN,
            UnaryOp::Eqz => return Err("aarch64: eqz reached lowering".into()),
        };
        self.push(MInst::FUn {
            op: fop,
            double,
            dst,
            src,
        });
        Ok(())
    }

    fn binary(&mut self, op: BinaryOp, a: Value, b: Value, r: Value) -> Result<(), String> {
        use BinaryOp::*;
        let ty = self.ty(a);
        let size = size_of(ty);
        let double = ty == Type::F64;
        let dst = self.vreg(r);
        match op {
            Iadd | Isub => {
                let (a, b) = if op == Iadd && self.iconst(b).is_none() && self.iconst(a).is_some() {
                    (b, a)
                } else {
                    (a, b)
                };
                if let Some(c) = self.iconst(b) {
                    let sub = op == Isub;
                    let direct = arith_imm(c).map(|i| (sub, i));
                    let flipped = || arith_imm(neg(c, size)).map(|i| (!sub, i));
                    if let Some((sub, (imm12, shift))) = direct.or_else(flipped) {
                        let ra = self.use_val(a);
                        self.push(MInst::AddImm {
                            sub,
                            size,
                            dst,
                            a: ra,
                            imm12,
                            shift,
                        });
                        return Ok(());
                    }
                }
                let (ra, rb) = (self.use_val(a), self.use_val(b));
                let op = if op == Iadd { Rrr::Add } else { Rrr::Sub };
                self.push(MInst::Rrr {
                    op,
                    size,
                    dst,
                    a: ra,
                    b: rb,
                });
            }
            Band | Bor | Bxor => {
                let lop = match op {
                    Band => LogImm::And,
                    Bor => LogImm::Orr,
                    _ => LogImm::Eor,
                };
                for (x, y) in [(a, b), (b, a)] {
                    if let Some(enc) = self.iconst(y).and_then(|c| logical_imm(c, size.w())) {
                        let rx = self.use_val(x);
                        self.push(MInst::LogImm {
                            op: lop,
                            size,
                            dst,
                            a: rx,
                            enc,
                        });
                        return Ok(());
                    }
                }
                let rop = match op {
                    Band => Rrr::And,
                    Bor => Rrr::Orr,
                    _ => Rrr::Eor,
                };
                let (ra, rb) = (self.use_val(a), self.use_val(b));
                self.push(MInst::Rrr {
                    op: rop,
                    size,
                    dst,
                    a: ra,
                    b: rb,
                });
            }
            Imul | Sdiv | Udiv => {
                let rop = match op {
                    Imul => Rrr::Mul,
                    Sdiv => Rrr::Sdiv,
                    _ => Rrr::Udiv,
                };
                let (ra, rb) = (self.use_val(a), self.use_val(b));
                self.push(MInst::Rrr {
                    op: rop,
                    size,
                    dst,
                    a: ra,
                    b: rb,
                });
            }
            Srem | Urem => {
                // a - (a / b) * b; for MIN % -1, sdiv gives MIN and the product wraps back to MIN.
                let (ra, rb) = (self.use_val(a), self.use_val(b));
                let q = self.fresh(ty);
                let rop = if op == Srem { Rrr::Sdiv } else { Rrr::Udiv };
                self.push(MInst::Rrr {
                    op: rop,
                    size,
                    dst: q,
                    a: ra,
                    b: rb,
                });
                self.push(MInst::Msub {
                    size,
                    dst,
                    a: q,
                    b: rb,
                    c: ra,
                });
            }
            Ishl | Ushr | Sshr | Rotl | Rotr => {
                if let Some(c) = self.iconst(b) {
                    let bits = ty.bits() as u64;
                    let mut amt = (c & (bits - 1)) as u8;
                    let sop = match op {
                        Ishl => ShiftOp::Lsl,
                        Ushr => ShiftOp::Lsr,
                        Sshr => ShiftOp::Asr,
                        Rotl => {
                            amt = ((bits - amt as u64) % bits) as u8;
                            ShiftOp::Ror
                        }
                        _ => ShiftOp::Ror,
                    };
                    if amt == 0 {
                        self.copy_into(dst, a);
                    } else {
                        let ra = self.use_val(a);
                        self.push(MInst::ShiftImm {
                            op: sop,
                            size,
                            dst,
                            a: ra,
                            amt,
                        });
                    }
                    return Ok(());
                }
                let ra = self.use_val(a);
                let mut rb = self.use_val(b);
                if op == Rotl {
                    // rotl x, n == rotr x, -n (the amount is taken modulo the width).
                    let t = self.fresh(ty);
                    self.push(MInst::Neg {
                        size,
                        dst: t,
                        src: rb,
                    });
                    rb = t;
                }
                let rop = match op {
                    Ishl => Rrr::Lslv,
                    Ushr => Rrr::Lsrv,
                    Sshr => Rrr::Asrv,
                    _ => Rrr::Rorv,
                };
                self.push(MInst::Rrr {
                    op: rop,
                    size,
                    dst,
                    a: ra,
                    b: rb,
                });
            }
            Fadd | Fsub | Fmul | Fdiv | Fmin | Fmax => {
                // FMIN/FMAX propagate NaN and order -0 below +0: the IR's (WebAssembly) rules.
                let fop = match op {
                    Fadd => FOp2::Add,
                    Fsub => FOp2::Sub,
                    Fmul => FOp2::Mul,
                    Fdiv => FOp2::Div,
                    Fmin => FOp2::Min,
                    _ => FOp2::Max,
                };
                let (ra, rb) = (self.use_val(a), self.use_val(b));
                self.push(MInst::FAlu {
                    op: fop,
                    double,
                    dst,
                    a: ra,
                    b: rb,
                });
            }
            Fcopysign => return Err("aarch64: fcopysign reached lowering".into()),
        }
        Ok(())
    }

    fn convert(&mut self, op: ConvOp, to: Type, arg: Value, r: Value) -> Result<(), String> {
        let from = self.ty(arg);
        if op == ConvOp::Uext {
            return Ok(());
        }
        let dst = self.vreg(r);
        if op == ConvOp::Wrap {
            if let Some(c) = self.iconst(arg) {
                self.push(MInst::MovImm {
                    dst,
                    imm: c as u32 as u64,
                });
                return Ok(());
            }
        }
        let src = self.use_val(arg);
        self.push(match op {
            ConvOp::Wrap => MInst::Mov {
                size: Size::S32,
                dst,
                src,
            },
            ConvOp::Sext => MInst::Sext {
                from: 32,
                size: Size::S64,
                dst,
                src,
            },
            ConvOp::FromSint | ConvOp::FromUint => MInst::IntToF {
                signed: op == ConvOp::FromSint,
                src_size: size_of(from),
                double: to == Type::F64,
                dst,
                src,
            },
            // FCVTZS/FCVTZU saturate and map NaN to 0: exactly the saturating forms, and a valid
            // choice for the unchecked ones (undefined out of range).
            ConvOp::ToSint | ConvOp::ToUint | ConvOp::ToSintSat | ConvOp::ToUintSat => {
                MInst::FToInt {
                    signed: matches!(op, ConvOp::ToSint | ConvOp::ToSintSat),
                    dst_size: size_of(to),
                    double: from == Type::F64,
                    dst,
                    src,
                }
            }
            ConvOp::Promote | ConvOp::Demote => MInst::FCvt {
                to_double: op == ConvOp::Promote,
                dst,
                src,
            },
            ConvOp::Bitcast if to.is_float() => MInst::GprToFpr {
                size: size_of(to),
                dst,
                src,
            },
            ConvOp::Bitcast => MInst::FprToGpr {
                size: size_of(to),
                dst,
                src,
            },
            ConvOp::Uext => unreachable!(),
        });
        Ok(())
    }
}
