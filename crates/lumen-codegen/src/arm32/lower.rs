//! IR → ARM32 [`MInst`]s over virtual registers.
//!
//! - An `I32` value is one 32-bit virtual register, an `I64` value a pair `(low, high)` of
//!   them, and `F32`/`F64` values one float-class register. Every 64-bit operation is
//!   expanded here into 32-bit instructions (`adds`/`adc`, `umull`/`mla`, shift pairs, ...);
//!   the few without a short expansion (64-bit division, variable 64-bit shifts, 64-bit to
//!   float conversions, `floor`...) call a helper in [`super::rt`].
//! - Integer constants that fit an instruction's modified-immediate field become immediates of
//!   their user; constants needed in a register are rematerialized at each use.
//! - A compare whose only use is a `brif`/`select` in the same block is emitted right before
//!   it and feeds the condition flags directly; an `iadd` whose only use is a load or store
//!   address folds into the address mode.
//! - Anything the backend cannot do (vectors, traps, I64 addresses) is an error, so the
//!   caller falls back to another tier.

use super::asm::{encode_imm, Cond, Dp, Ext, FOp1, FOp2, LdSt, Shift};
use super::inst::*;
use super::regs::*;
use super::rt;
use crate::cfg::Cfg;
use crate::eval;
use crate::ir::*;
use crate::machinst::*;

pub struct Lowered {
    pub vcode: VCode<MInst>,
    /// Bytes of outgoing stack-argument space any call needs (a multiple of 8).
    pub outgoing: u32,
}

/// Where an argument (half) lives: a register, or `(offset from the first stack slot, bytes)`.
#[derive(Clone, Copy, Debug)]
pub enum ArgLoc {
    Reg(PReg),
    Stack(i32, u8),
}

/// Assign argument locations (AAPCS §6.4 with the VFP variant): one location per 32-bit half
/// (two for an `I64`, low half first), and the size of the stack argument area rounded to 8.
pub fn arg_locs(abi: &Abi, tys: &[Type]) -> Result<(Vec<Vec<ArgLoc>>, u32), String> {
    let mut out = Vec::with_capacity(tys.len());
    let (mut ncrn, mut nvfp, mut nsaa) = (0usize, 0usize, 0u32);
    for &t in tys {
        match t {
            Type::I32 => {
                if ncrn < 4 {
                    out.push(vec![ArgLoc::Reg(abi.int_args()[ncrn])]);
                    ncrn += 1;
                } else {
                    out.push(vec![ArgLoc::Stack(nsaa as i32, 4)]);
                    nsaa += 4;
                }
            }
            Type::I64 => {
                ncrn = ncrn.next_multiple_of(2);
                if ncrn + 2 <= 4 {
                    out.push(vec![
                        ArgLoc::Reg(abi.int_args()[ncrn]),
                        ArgLoc::Reg(abi.int_args()[ncrn + 1]),
                    ]);
                    ncrn += 2;
                } else {
                    ncrn = 4;
                    nsaa = nsaa.next_multiple_of(8);
                    out.push(vec![
                        ArgLoc::Stack(nsaa as i32, 4),
                        ArgLoc::Stack(nsaa as i32 + 4, 4),
                    ]);
                    nsaa += 8;
                }
            }
            Type::F64 => {
                if nvfp < abi.float_args().len() {
                    out.push(vec![ArgLoc::Reg(abi.float_args()[nvfp])]);
                    nvfp += 1;
                } else {
                    return Err("arm32: too many floating-point arguments".into());
                }
            }
            Type::F32 => return Err("arm32: single-precision arguments are unsupported".into()),
            Type::V128 => return Err("arm32: vector arguments are unsupported".into()),
        }
    }
    Ok((out, nsaa.next_multiple_of(8)))
}

fn ret_regs(t: Type) -> Vec<PReg> {
    match t {
        Type::I32 => vec![R0],
        Type::I64 => vec![R0, r(1)],
        _ => vec![D0],
    }
}

/// A lowered IR value.
#[derive(Clone, Copy, Debug)]
enum V {
    S(VReg),
    P(VReg, VReg),
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

/// Conditions after `vcmp; vmrs` that are false when either operand is NaN (except `Ne`).
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

fn load_op(k: MemKind) -> Option<LdSt> {
    use MemKind::*;
    Some(match k {
        I32 | I64 | I64S32 | I64U32 => LdSt::Ldr,
        I32S8 | I64S8 => LdSt::Ldrsb,
        I32U8 | I64U8 => LdSt::Ldrb,
        I32S16 | I64S16 => LdSt::Ldrsh,
        I32U16 | I64U16 => LdSt::Ldrh,
        F32 | F64 | V128 => return None,
    })
}

fn store_op(k: MemKind) -> Option<LdSt> {
    use MemKind::*;
    Some(match k {
        I32 | I64 | I64S32 | I64U32 => LdSt::Str,
        I32S8 | I32U8 | I64S8 | I64U8 => LdSt::Strb,
        I32S16 | I32U16 | I64S16 | I64U16 => LdSt::Strh,
        F32 | F64 | V128 => return None,
    })
}

fn enc(v: u32) -> bool {
    encode_imm(v).is_some()
}

struct Lower<'a> {
    f: &'a Function,
    abi: &'a Abi,
    /// ARMv7-A instructions are available (otherwise ARMv6).
    v7: bool,
    /// `sdiv`/`udiv` are available.
    idiv: bool,
    vc: VCode<MInst>,
    vals: Vec<Option<V>>,
    /// Instructions emitted at their single user instead of at their definition.
    sunk: Vec<bool>,
    bmap: Vec<usize>,
    outgoing: u32,
}

/// Lower `f` for ARMv7-A (`v7`) or ARMv6; `idiv`: the target has `sdiv`/`udiv`.
pub fn lower(
    f: &Function,
    cfg: &Cfg,
    abi: &Abi,
    v7: bool,
    idiv: bool,
) -> Result<Lowered, String> {
    if std::iter::once(&f.sig)
        .chain(f.sigs.iter())
        .any(|s| s.params.iter().chain(&s.results).any(|t| *t == Type::V128))
        || f.values.iter().any(|v| v.ty == Type::V128)
    {
        return Err("arm32: vectors are unsupported".into());
    }
    if f.sig.results.len() > 1 || f.sigs.iter().any(|s| s.results.len() > 1) {
        return Err("arm32: multiple results are not supported".into());
    }
    let nv = f.values.len();
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
        }
    }
    let mut sunk = vec![false; nv];
    // A checked operation directly followed by its flag's only consumer.
    for &b in &cfg.rpo {
        for pair in f.blocks[b.index()].insts.windows(2) {
            if !matches!(f.inst(pair[0]), InstData::CheckedBinary { .. }) {
                continue;
            }
            let flag = f.results(pair[0])[1];
            if uses[flag.index()] == 1
                && matches!(f.inst(pair[1]),
                    InstData::Brif { cond, .. } | InstData::Select { cond, .. } if *cond == flag)
            {
                sunk[flag.index()] = true;
            }
        }
    }
    for &b in &cfg.rpo {
        for &i in &f.blocks[b.index()].insts {
            let [r] = f.results(i) else { continue };
            let Some(u) = user[r.index()] else { continue };
            if uses[r.index()] != 1 || inst_block[u.index()] != Some(b) {
                continue;
            }
            sunk[r.index()] = match f.inst(i) {
                InstData::IntCmp { .. } | InstData::FloatCmp { .. } => {
                    matches!(f.inst(u),
                        InstData::Brif { cond, .. } | InstData::Select { cond, .. } if cond == r)
                }
                InstData::Binary {
                    op: BinaryOp::Iadd, ..
                } => {
                    f.value_type(*r) == Type::I32
                        && matches!(f.inst(u),
                            InstData::Load { addr, .. } | InstData::Store { addr, .. } if addr == r)
                        && !matches!(f.inst(u), InstData::Store { value, .. } if value == r)
                }
                _ => false,
            };
        }
    }

    let entry = f.entry();
    let pre_entry = !cfg.preds[entry.index()].is_empty();
    let order = crate::cfg::sink_cold(
        f,
        cfg,
        super::super::aarch64::lower::rotate_loops(
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
        v7,
        idiv,
        vc: VCode::with_capacity(2 * f.insts.len() + 16),
        vals: vec![None; nv],
        sunk,
        bmap,
        outgoing: 0,
    };

    if pre_entry {
        let params: Vec<V> = f.blocks[entry.index()]
            .params
            .iter()
            .map(|&p| l.fresh(f.value_type(p)))
            .collect();
        l.args(&params)?;
        let args = params.iter().flat_map(|v| regs_of(*v)).collect();
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
            let targets: Vec<V> = bparams.iter().map(|&p| l.raw(p)).collect();
            l.args(&targets)?;
            Vec::new()
        } else {
            bparams.iter().flat_map(|&p| regs_of(l.raw(p))).collect()
        };
        for &i in &f.blocks[b.index()].insts {
            l.inst(i)?;
        }
        let succs = f.successors(b).map(|s| l.bmap[s.index()]).collect();
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
    })
}

fn regs_of(v: V) -> Vec<VReg> {
    match v {
        V::S(a) => vec![a],
        V::P(a, b) => vec![a, b],
    }
}

impl Lower<'_> {
    fn push(&mut self, i: MInst) {
        self.vc.insts.push(i);
    }

    fn ty(&self, v: Value) -> Type {
        self.f.value_type(v)
    }

    fn fresh_int(&mut self) -> VReg {
        self.vc.new_vreg(RegClass::Int)
    }

    fn fresh_float(&mut self) -> VReg {
        self.vc.new_vreg(RegClass::Float)
    }

    fn fresh(&mut self, t: Type) -> V {
        match t {
            Type::I64 => V::P(self.fresh_int(), self.fresh_int()),
            t => V::S(self.vc.new_vreg(class_of(t))),
        }
    }

    /// The registers of `v` (allocated on first mention), without rematerializing constants.
    fn raw(&mut self, v: Value) -> V {
        let v = self.f.resolve(v);
        if let Some(r) = self.vals[v.index()] {
            return r;
        }
        let r = self.fresh(self.ty(v));
        self.vals[v.index()] = Some(r);
        r
    }

    fn def_inst(&self, v: Value) -> Option<&InstData> {
        match self.f.values[self.f.resolve(v).index()].def {
            ValueDef::Result(i, 0) => Some(self.f.inst(i)),
            _ => None,
        }
    }

    /// The constant bits of an integer constant (normalized to its type).
    fn iconst(&self, v: Value) -> Option<u64> {
        match self.def_inst(v) {
            Some(InstData::Iconst { ty, imm }) => Some(eval::iconst(*ty, *imm)),
            _ => None,
        }
    }

    fn mov_imm(&mut self, imm: u32) -> VReg {
        let r = self.fresh_int();
        self.push(MInst::MovImm { dst: r, imm });
        r
    }

    /// `v`, rematerializing constants at the use.
    fn val(&mut self, v: Value) -> V {
        match self.def_inst(v).cloned() {
            Some(InstData::Iconst { ty, imm }) => {
                let c = eval::iconst(ty, imm);
                if ty == Type::I64 {
                    V::P(self.mov_imm(c as u32), self.mov_imm((c >> 32) as u32))
                } else {
                    V::S(self.mov_imm(c as u32))
                }
            }
            Some(InstData::F32const { bits }) => {
                let dst = self.fresh_float();
                self.push(MInst::FConst {
                    double: false,
                    dst,
                    bits: bits as u64,
                });
                V::S(dst)
            }
            Some(InstData::F64const { bits }) => {
                let dst = self.fresh_float();
                self.push(MInst::FConst {
                    double: true,
                    dst,
                    bits,
                });
                V::S(dst)
            }
            _ => self.raw(v),
        }
    }

    fn s(&mut self, v: Value) -> VReg {
        match self.val(v) {
            V::S(a) => a,
            V::P(a, _) => a,
        }
    }

    /// The low word of an `I64` (or the only word of an `I32`).
    fn lo(&mut self, v: Value) -> VReg {
        if let Some(c) = self.iconst(v) {
            return self.mov_imm(c as u32);
        }
        match self.raw(v) {
            V::S(a) | V::P(a, _) => a,
        }
    }

    fn p(&mut self, v: Value) -> (VReg, VReg) {
        match self.val(v) {
            V::P(a, b) => (a, b),
            V::S(a) => panic!("arm32: {v:?} is not 64-bit ({a:?})"),
        }
    }

    fn def_s(&mut self, v: Value) -> VReg {
        match self.raw(v) {
            V::S(a) => a,
            V::P(..) => panic!("arm32: {v:?} is 64-bit"),
        }
    }

    fn def_p(&mut self, v: Value) -> (VReg, VReg) {
        match self.raw(v) {
            V::P(a, b) => (a, b),
            V::S(..) => panic!("arm32: {v:?} is not 64-bit"),
        }
    }

    fn set(&mut self, v: Value, x: V) {
        let v = self.f.resolve(v);
        self.vals[v.index()] = Some(x);
    }

    fn alu(&mut self, op: Dp, s: bool, dst: VReg, a: VReg, b: Rhs) {
        self.push(MInst::Alu { op, s, dst, a, b });
    }

    fn alu_new(&mut self, op: Dp, a: VReg, b: Rhs) -> VReg {
        let dst = self.fresh_int();
        self.alu(op, false, dst, a, b);
        dst
    }

    fn shift_new(&mut self, kind: Shift, a: VReg, amt: u8) -> VReg {
        let dst = self.fresh_int();
        self.push(MInst::Shift {
            kind,
            dst,
            a,
            amt: Amt::Imm(amt),
        });
        dst
    }

    fn copy(&mut self, dst: VReg, src: VReg) {
        self.push(MInst::Mov { dst, src });
    }

    /// Function entry: move parameters from their ABI locations into `targets`.
    fn args(&mut self, targets: &[V]) -> Result<(), String> {
        let tys = self.f.sig.params.clone();
        let (locs, _) = arg_locs(self.abi, &tys)?;
        let mut regs = Vec::new();
        let mut stack = Vec::new();
        for (t, locs) in targets.iter().zip(locs) {
            for (v, loc) in regs_of(*t).into_iter().zip(locs) {
                match loc {
                    ArgLoc::Reg(r) => regs.push((v, r)),
                    ArgLoc::Stack(off, bytes) => stack.push((v, off, bytes)),
                }
            }
        }
        self.push(MInst::Args { regs, stack });
        Ok(())
    }

    // ----- conditions -----

    fn cmp(&mut self, op: Dp, cc: Cond, a: VReg, b: Rhs) {
        self.push(MInst::Cmp { op, cc, a, b });
    }

    fn int_cmp(&mut self, cc: IntCC, a: Value, b: Value) -> Cond {
        if self.ty(a) == Type::I64 {
            return self.cmp64(cc, a, b);
        }
        let neg = |c: u32| c.wrapping_neg();
        if let Some(c) = self.iconst(b).map(|c| c as u32) {
            if enc(c) {
                let ra = self.s(a);
                self.cmp(Dp::Cmp, Cond::Al, ra, Rhs::Imm(c));
                return int_cc(cc);
            }
            if enc(neg(c)) {
                let ra = self.s(a);
                self.cmp(Dp::Cmn, Cond::Al, ra, Rhs::Imm(neg(c)));
                return int_cc(cc);
            }
        }
        if let Some(c) = self.iconst(a).map(|c| c as u32) {
            if enc(c) {
                let rb = self.s(b);
                self.cmp(Dp::Cmp, Cond::Al, rb, Rhs::Imm(c));
                return int_cc(cc.swap());
            }
        }
        let (ra, rb) = (self.s(a), self.s(b));
        self.cmp(Dp::Cmp, Cond::Al, ra, Rhs::Reg(rb));
        int_cc(cc)
    }

    fn cmp64(&mut self, cc: IntCC, a: Value, b: Value) -> Cond {
        let (x, y, cc) = match cc {
            IntCC::Sgt | IntCC::Sle | IntCC::Ugt | IntCC::Ule => (b, a, cc.swap()),
            _ => (a, b, cc),
        };
        let (xlo, xhi) = self.p(x);
        let (rlo, rhi) = match self.iconst(y) {
            Some(c) => (
                if enc(c as u32) {
                    Rhs::Imm(c as u32)
                } else {
                    Rhs::Reg(self.mov_imm(c as u32))
                },
                if enc((c >> 32) as u32) {
                    Rhs::Imm((c >> 32) as u32)
                } else {
                    Rhs::Reg(self.mov_imm((c >> 32) as u32))
                },
            ),
            None => {
                let (lo, hi) = self.p(y);
                (Rhs::Reg(lo), Rhs::Reg(hi))
            }
        };
        match cc {
            IntCC::Eq | IntCC::Ne => {
                self.cmp(Dp::Cmp, Cond::Al, xhi, rhi);
                self.cmp(Dp::Cmp, Cond::Eq, xlo, rlo);
            }
            _ => {
                self.cmp(Dp::Cmp, Cond::Al, xlo, rlo);
                let tmp = self.fresh_int();
                self.alu(Dp::Sbc, true, tmp, xhi, rhi);
            }
        }
        int_cc(cc)
    }

    fn float_cmp(&mut self, cc: FloatCC, a: Value, b: Value) -> Cond {
        let double = self.ty(a) == Type::F64;
        let (ra, rb) = (self.s(a), self.s(b));
        self.push(MInst::FCmp {
            double,
            a: ra,
            b: rb,
        });
        float_cc(cc)
    }

    /// Emit the sunk definition of `cond` and return the condition to test, or `None` when
    /// `cond` is a plain value.
    fn sunk_cond(&mut self, cond: Value) -> Option<Cond> {
        if !self.sunk[self.f.resolve(cond).index()] {
            return None;
        }
        if let ValueDef::Result(i, 1) = self.f.values[self.f.resolve(cond).index()].def {
            if let InstData::CheckedBinary { op, args } = *self.f.inst(i) {
                return Some(self.checked(op, args, self.f.results(i)[0]));
            }
        }
        match self.def_inst(cond).cloned() {
            Some(InstData::IntCmp { cc, args }) => Some(self.int_cmp(cc, args[0], args[1])),
            Some(InstData::FloatCmp { cc, args }) => Some(self.float_cmp(cc, args[0], args[1])),
            _ => None,
        }
    }

    /// Set the flags for `cond != 0` and return the condition to test. Nothing that changes
    /// the flags may be emitted between this and the consumer.
    fn cond(&mut self, cond: Value) -> Cond {
        if let Some(c) = self.sunk_cond(cond) {
            return c;
        }
        let r = self.s(cond);
        self.cmp(Dp::Cmp, Cond::Al, r, Rhs::Imm(0));
        Cond::Ne
    }

    fn checked(&mut self, op: CheckedOp, args: [Value; 2], result: Value) -> Cond {
        let a = self.s(args[0]);
        let b = self.s(args[1]);
        let dst = self.def_s(result);
        match op {
            CheckedOp::IaddOv | CheckedOp::IsubOv => {
                let o = if op == CheckedOp::IaddOv {
                    Dp::Add
                } else {
                    Dp::Sub
                };
                self.alu(o, true, dst, a, Rhs::Reg(b));
                Cond::Vs
            }
            CheckedOp::ImulOv => {
                let hi = self.fresh_int();
                self.push(MInst::LongMul {
                    signed: true,
                    lo: dst,
                    hi,
                    a,
                    b,
                });
                self.cmp(Dp::Cmp, Cond::Al, hi, Rhs::Shift(dst, Shift::Asr, 31));
                Cond::Ne
            }
        }
    }

    // ----- memory -----

    /// `addr + offset` as an address mode whose immediate satisfies `fits`. With `allow_reg`, a
    /// single-use `iadd` may become `[base, index]`; otherwise the result is always
    /// `Amode::Imm`, computing the sum when the offset does not fit.
    fn amode(
        &mut self,
        addr: Value,
        offset: i32,
        fits: &dyn Fn(i64) -> bool,
        allow_reg: bool,
    ) -> Result<(VReg, i32, Option<VReg>), String> {
        // An I64 address (a pointer taken from a 64-bit word, e.g. a NaN-boxed handle) uses its
        // low word, as wasm32 wraps it: addresses are 32 bits, and `s` and the folded `iadd`
        // below are exact modulo 2^32.
        if !matches!(self.ty(addr), Type::I32 | Type::I64) {
            return Err("arm32: addresses must be integers".into());
        }
        let key = self.f.resolve(addr);
        if self.sunk[key.index()] {
            if let Some(InstData::Binary { args: [a, b], .. }) = self.def_inst(addr).cloned() {
                for (x, c) in [(a, b), (b, a)] {
                    if let Some(c) = self.iconst(c) {
                        let off = offset as i64 + c as u32 as i32 as i64;
                        if fits(off) {
                            return Ok((self.s(x), off as i32, None));
                        }
                    }
                }
                if offset == 0 && allow_reg {
                    let (ra, rb) = (self.s(a), self.s(b));
                    return Ok((ra, 0, Some(rb)));
                }
            }
            // Not foldable: compute the sum where it stands.
            self.sunk[key.index()] = false;
            if let ValueDef::Result(i, _) = self.f.values[key.index()].def {
                self.inst(i)?;
            }
        }
        let base = self.s(addr);
        if fits(offset as i64) {
            Ok((base, offset, None))
        } else if allow_reg {
            let idx = self.mov_imm(offset as u32);
            Ok((base, 0, Some(idx)))
        } else {
            Ok((self.add_const(base, offset), 0, None))
        }
    }

    fn mem_amode(&mut self, addr: Value, offset: i32, op: LdSt) -> Result<Amode, String> {
        let (b, o, i) = self.amode(addr, offset, &|o| op.offset_ok(o), true)?;
        Ok(match i {
            Some(i) => Amode::Reg(b, i),
            None => Amode::Imm(b, o),
        })
    }

    /// A base register and a word-aligned offset in the `vldr` range.
    fn vfp_addr(&mut self, addr: Value, offset: i32) -> Result<(VReg, i32), String> {
        let (b, o, _) = self.amode(addr, offset, &|o| o % 4 == 0 && o.abs() <= 1020, false)?;
        Ok((b, o))
    }

    /// A base and offset such that both `off` and `off + 4` are word-addressable.
    fn pair_addr(&mut self, addr: Value, offset: i32) -> Result<(VReg, i32), String> {
        let (b, o, _) = self.amode(
            addr,
            offset,
            &|o| LdSt::Ldr.offset_ok(o) && LdSt::Ldr.offset_ok(o + 4),
            false,
        )?;
        Ok((b, o))
    }

    fn add_const(&mut self, base: VReg, c: i32) -> VReg {
        if enc(c as u32) {
            self.alu_new(Dp::Add, base, Rhs::Imm(c as u32))
        } else if enc((c as u32).wrapping_neg()) {
            self.alu_new(Dp::Sub, base, Rhs::Imm((c as u32).wrapping_neg()))
        } else {
            let k = self.mov_imm(c as u32);
            self.alu_new(Dp::Add, base, Rhs::Reg(k))
        }
    }

    fn load(&mut self, kind: MemKind, addr: Value, offset: i32, r: Value) -> Result<(), String> {
        match kind {
            MemKind::V128 => return Err("arm32: vectors are unsupported".into()),
            MemKind::F32 | MemKind::F64 if offset % 4 != 0 => {
                // `vldr` needs a word-aligned address; `ldr` does not.
                let dst = self.def_s(r);
                if kind == MemKind::F32 {
                    let a = self.mem_amode(addr, offset, LdSt::Ldr)?;
                    let t = self.fresh_int();
                    self.push(MInst::Load {
                        op: LdSt::Ldr,
                        dst: t,
                        addr: a,
                    });
                    self.push(MInst::GprToFpr { dst, src: t });
                } else {
                    let (lo, hi) = (self.fresh_int(), self.fresh_int());
                    let (base, off) = self.pair_addr(addr, offset)?;
                    for (d, o) in [(lo, off), (hi, off + 4)] {
                        self.push(MInst::Load {
                            op: LdSt::Ldr,
                            dst: d,
                            addr: Amode::Imm(base, o),
                        });
                    }
                    self.push(MInst::PairToF { dst, lo, hi });
                }
                return Ok(());
            }
            MemKind::F32 | MemKind::F64 => {
                let (base, off) = self.vfp_addr(addr, offset)?;
                let dst = self.def_s(r);
                self.push(MInst::FLoad {
                    double: kind == MemKind::F64,
                    dst,
                    base,
                    off,
                });
                return Ok(());
            }
            _ => {}
        }
        let op = load_op(kind).unwrap();
        if kind != MemKind::I64 {
            let addr = self.mem_amode(addr, offset, op)?;
            let signed = kind.is_signed();
            return match kind.ty() {
                Type::I32 => {
                    let dst = self.def_s(r);
                    self.push(MInst::Load { op, dst, addr });
                    Ok(())
                }
                _ => {
                    let (lo, hi) = self.def_p(r);
                    self.push(MInst::Load { op, dst: lo, addr });
                    if signed {
                        self.push(MInst::Shift {
                            kind: Shift::Asr,
                            dst: hi,
                            a: lo,
                            amt: Amt::Imm(31),
                        });
                    } else {
                        self.push(MInst::MovImm { dst: hi, imm: 0 });
                    }
                    Ok(())
                }
            };
        }
        let (lo, hi) = self.def_p(r);
        let (base, off) = self.pair_addr(addr, offset)?;
        self.push(MInst::Load {
            op,
            dst: lo,
            addr: Amode::Imm(base, off),
        });
        self.push(MInst::Load {
            op,
            dst: hi,
            addr: Amode::Imm(base, off + 4),
        });
        Ok(())
    }

    fn store(
        &mut self,
        kind: MemKind,
        addr: Value,
        value: Value,
        offset: i32,
    ) -> Result<(), String> {
        match kind {
            MemKind::V128 => return Err("arm32: vectors are unsupported".into()),
            MemKind::F32 | MemKind::F64 if offset % 4 != 0 => {
                let src = self.s(value);
                if kind == MemKind::F32 {
                    let t = self.fresh_int();
                    self.push(MInst::FprToGpr { dst: t, src });
                    let a = self.mem_amode(addr, offset, LdSt::Str)?;
                    self.push(MInst::Store {
                        op: LdSt::Str,
                        src: t,
                        addr: a,
                    });
                } else {
                    let (lo, hi) = (self.fresh_int(), self.fresh_int());
                    self.push(MInst::FToPair { lo, hi, src });
                    let (base, off) = self.pair_addr(addr, offset)?;
                    for (v, o) in [(lo, off), (hi, off + 4)] {
                        self.push(MInst::Store {
                            op: LdSt::Str,
                            src: v,
                            addr: Amode::Imm(base, o),
                        });
                    }
                }
                return Ok(());
            }
            MemKind::F32 | MemKind::F64 => {
                let src = self.s(value);
                let (base, off) = self.vfp_addr(addr, offset)?;
                self.push(MInst::FStore {
                    double: kind == MemKind::F64,
                    src,
                    base,
                    off,
                });
                return Ok(());
            }
            _ => {}
        }
        let op = store_op(kind).unwrap();
        if kind == MemKind::I64 {
            let (lo, hi) = self.p(value);
            let (base, off) = self.pair_addr(addr, offset)?;
            self.push(MInst::Store {
                op,
                src: lo,
                addr: Amode::Imm(base, off),
            });
            self.push(MInst::Store {
                op,
                src: hi,
                addr: Amode::Imm(base, off + 4),
            });
            return Ok(());
        }
        let src = self.lo(value);
        let addr = self.mem_amode(addr, offset, op)?;
        self.push(MInst::Store { op, src, addr });
        Ok(())
    }

    // ----- calls -----

    fn call_raw(
        &mut self,
        params: &[Type],
        ret: Option<Type>,
        target: CallTarget,
        args: &[V],
    ) -> Result<Option<V>, String> {
        let (locs, size) = arg_locs(self.abi, params)?;
        self.outgoing = self.outgoing.max(size);
        let mut reg_args = Vec::new();
        let mut stack_args = Vec::new();
        for (v, locs) in args.iter().zip(locs) {
            for (r, loc) in regs_of(*v).into_iter().zip(locs) {
                match loc {
                    ArgLoc::Reg(p) => reg_args.push((r, p)),
                    ArgLoc::Stack(off, bytes) => stack_args.push((r, off, bytes)),
                }
            }
        }
        let mut rets = Vec::new();
        let result = ret.map(|t| {
            let v = self.fresh(t);
            for (r, p) in regs_of(v).into_iter().zip(ret_regs(t)) {
                rets.push((r, p));
            }
            v
        });
        self.push(MInst::Call(Box::new(CallInfo {
            target,
            reg_args,
            stack_args,
            rets,
            clobbers: self.abi.call_clobbers,
        })));
        Ok(result)
    }

    /// Call a runtime helper by address.
    fn rt_call(
        &mut self,
        addr: usize,
        params: &[Type],
        ret: Option<Type>,
        args: &[V],
    ) -> Result<Option<V>, String> {
        self.call_raw(params, ret, CallTarget::Abs(addr), args)
    }

    fn ir_call(
        &mut self,
        sig: &Signature,
        target: CallTarget,
        args: &[Value],
        results: &[Value],
    ) -> Result<(), String> {
        let vals: Vec<V> = args.iter().map(|&a| self.val(a)).collect();
        let r = self.call_raw(&sig.params, sig.results.first().copied(), target, &vals)?;
        if let (Some(r), Some(&dst)) = (r, results.first()) {
            self.set(dst, r);
        }
        Ok(())
    }

    // ----- instructions -----

    fn inst(&mut self, i: Inst) -> Result<(), String> {
        let f = self.f;
        let data = f.inst(i);
        let res = f.results(i);
        if let [r] = res {
            if self.sunk[f.resolve(*r).index()] {
                return Ok(());
            }
        }
        match data {
            InstData::Prefetch { .. } => {}
            InstData::Vzero | InstData::VectorBinary { .. } => {
                return Err("arm32: vectors are unsupported".into())
            }
            InstData::SymbolAddr { .. } => {
                return Err("arm32: symbol addresses are unsupported".into())
            }
            InstData::Iconst { .. } | InstData::F32const { .. } | InstData::F64const { .. } => {}
            InstData::Unary { op, arg } => self.unary(*op, *arg, res[0])?,
            InstData::Binary { op, args } => self.binary(*op, args[0], args[1], res[0])?,
            InstData::CheckedBinary { op, args } => {
                if self.sunk[f.resolve(res[1]).index()] {
                    return Ok(());
                }
                let cc = self.checked(*op, *args, res[0]);
                let overflow = self.def_s(res[1]);
                self.push(MInst::SetCc { cc, dst: overflow });
            }
            InstData::IntCmp { cc, args } => {
                let dst = self.def_s(res[0]);
                let cc = self.int_cmp(*cc, args[0], args[1]);
                self.push(MInst::SetCc { cc, dst });
            }
            InstData::FloatCmp { cc, args } => {
                let dst = self.def_s(res[0]);
                let cc = self.float_cmp(*cc, args[0], args[1]);
                self.push(MInst::SetCc { cc, dst });
            }
            InstData::Select {
                cond,
                if_true,
                if_false,
            } => {
                let t = self.val(*if_true);
                let fv = self.val(*if_false);
                let d = self.raw(res[0]);
                let cc = self.cond(*cond);
                match (d, t, fv) {
                    (V::S(dst), V::S(t), V::S(fv)) => {
                        self.push(if self.ty(res[0]).is_float() {
                            MInst::FSel { cc, dst, t, f: fv }
                        } else {
                            MInst::Sel { cc, dst, t, f: fv }
                        });
                    }
                    (V::P(dl, dh), V::P(tl, th), V::P(fl, fh)) => {
                        self.push(MInst::Sel {
                            cc,
                            dst: dl,
                            t: tl,
                            f: fl,
                        });
                        self.push(MInst::Sel {
                            cc,
                            dst: dh,
                            t: th,
                            f: fh,
                        });
                    }
                    _ => unreachable!("select operand types agree"),
                }
            }
            InstData::Convert { op, to, arg } => self.convert(*op, *to, *arg, res[0])?,
            InstData::Load { kind, addr, offset } => self.load(*kind, *addr, *offset, res[0])?,
            InstData::Store {
                kind,
                addr,
                value,
                offset,
            } => self.store(*kind, *addr, *value, *offset)?,
            InstData::Call { func, args } => {
                let ext = &f.funcs[func.index()];
                let sig = f.sigs[ext.sig.index()].clone();
                self.ir_call(&sig, CallTarget::Func(ext.id), args, res)?;
            }
            InstData::CallIndirect { sig, callee, args } => {
                // As addresses: an I64 callee calls its low word.
                if !matches!(self.ty(*callee), Type::I32 | Type::I64) {
                    return Err("arm32: indirect callees must be integers".into());
                }
                let sig = f.sigs[sig.index()].clone();
                let c = self.s(*callee);
                self.ir_call(&sig, CallTarget::Reg(c), args, res)?;
            }
            InstData::Trap { .. } | InstData::TrapIf { .. } => {
                return Err("arm32: traps are unsupported".into())
            }
            InstData::Jump { dest } => {
                let args = dest
                    .args
                    .iter()
                    .flat_map(|&a| {
                        let v = self.val(a);
                        regs_of(v)
                    })
                    .collect();
                self.push(MInst::Jmp {
                    target: self.bmap[dest.block.index()],
                    args,
                });
            }
            InstData::Brif { cond, then, else_ } => {
                if !then.args.is_empty() || !else_.args.is_empty() {
                    return Err("arm32: brif with arguments (critical edges not split)".into());
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
                        let reg = self.s(*cond);
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
                    return Err("arm32: br_table with arguments (critical edges not split)".into());
                }
                let index = self.s(*index);
                self.push(MInst::BrTable {
                    index,
                    targets: targets.iter().map(|t| self.bmap[t.block.index()]).collect(),
                    default: self.bmap[default.block.index()],
                });
            }
            InstData::Return { args } => {
                let mut vals = Vec::new();
                for &a in args {
                    let t = self.ty(a);
                    let v = self.val(a);
                    for (r, p) in regs_of(v).into_iter().zip(ret_regs(t)) {
                        vals.push((r, p));
                    }
                }
                self.push(MInst::Ret { vals });
            }
        }
        Ok(())
    }

    fn unary(&mut self, op: UnaryOp, arg: Value, r: Value) -> Result<(), String> {
        let ty = self.ty(arg);
        match (op, ty) {
            (UnaryOp::Fneg | UnaryOp::Fabs | UnaryOp::Sqrt, _) => {
                let src = self.s(arg);
                let dst = self.def_s(r);
                let o = match op {
                    UnaryOp::Fneg => FOp1::Neg,
                    UnaryOp::Fabs => FOp1::Abs,
                    _ => FOp1::Sqrt,
                };
                self.push(MInst::FUn {
                    op: o,
                    double: ty == Type::F64,
                    dst,
                    src,
                });
            }
            (UnaryOp::Ceil | UnaryOp::Floor | UnaryOp::Trunc | UnaryOp::Nearest, _) => {
                let src = self.s(arg);
                let x = if ty == Type::F32 {
                    let w = self.fresh_float();
                    self.push(MInst::FCvt {
                        to_double: true,
                        dst: w,
                        src,
                    });
                    w
                } else {
                    src
                };
                let y = self.round_f64(op, x);
                let dst = self.def_s(r);
                if ty == Type::F32 {
                    self.push(MInst::FCvt {
                        to_double: false,
                        dst,
                        src: y,
                    });
                } else {
                    self.push(MInst::FMov { dst, src: y });
                }
            }
            (UnaryOp::Eqz, _) => unreachable!("legalized"),
            (UnaryOp::Sext8 | UnaryOp::Sext16, Type::I32) => {
                let src = self.s(arg);
                let dst = self.def_s(r);
                let e = if op == UnaryOp::Sext8 {
                    Ext::Sxtb
                } else {
                    Ext::Sxth
                };
                self.push(MInst::Ext { e, dst, src });
            }
            (UnaryOp::Sext8 | UnaryOp::Sext16 | UnaryOp::Sext32, Type::I64) => {
                let lo = self.lo(arg);
                let (dl, dh) = self.def_p(r);
                match op {
                    UnaryOp::Sext8 => self.push(MInst::Ext {
                        e: Ext::Sxtb,
                        dst: dl,
                        src: lo,
                    }),
                    UnaryOp::Sext16 => self.push(MInst::Ext {
                        e: Ext::Sxth,
                        dst: dl,
                        src: lo,
                    }),
                    _ => self.copy(dl, lo),
                }
                self.push(MInst::Shift {
                    kind: Shift::Asr,
                    dst: dh,
                    a: dl,
                    amt: Amt::Imm(31),
                });
            }
            (UnaryOp::Sext32, _) => return Err("arm32: sext32 of a non-I64".into()),
            (UnaryOp::Clz | UnaryOp::Ctz | UnaryOp::Popcnt, Type::I32) => {
                let src = self.s(arg);
                let dst = self.def_s(r);
                self.count32(op, src, dst);
            }
            (UnaryOp::Clz | UnaryOp::Ctz | UnaryOp::Popcnt, Type::I64) => {
                let (lo, hi) = self.p(arg);
                let (dl, dh) = self.def_p(r);
                match op {
                    UnaryOp::Popcnt => {
                        let (a, b) = (self.fresh_int(), self.fresh_int());
                        self.count32(op, lo, a);
                        self.count32(op, hi, b);
                        self.alu(Dp::Add, false, dl, a, Rhs::Reg(b));
                    }
                    UnaryOp::Clz => {
                        // hi != 0 ? clz(hi) : 32 + clz(lo)
                        let (a, b) = (self.fresh_int(), self.fresh_int());
                        self.count32(op, hi, a);
                        self.count32(op, lo, b);
                        let b32 = self.alu_new(Dp::Add, b, Rhs::Imm(32));
                        self.cmp(Dp::Cmp, Cond::Al, hi, Rhs::Imm(0));
                        self.push(MInst::Sel {
                            cc: Cond::Eq,
                            dst: dl,
                            t: b32,
                            f: a,
                        });
                    }
                    _ => {
                        // lo != 0 ? ctz(lo) : 32 + ctz(hi)
                        let (a, b) = (self.fresh_int(), self.fresh_int());
                        self.count32(op, lo, a);
                        self.count32(op, hi, b);
                        let b32 = self.alu_new(Dp::Add, b, Rhs::Imm(32));
                        self.cmp(Dp::Cmp, Cond::Al, lo, Rhs::Imm(0));
                        self.push(MInst::Sel {
                            cc: Cond::Eq,
                            dst: dl,
                            t: b32,
                            f: a,
                        });
                    }
                }
                self.push(MInst::MovImm { dst: dh, imm: 0 });
            }
            _ => return Err(format!("arm32: unsupported unary {op:?} on {ty:?}")),
        }
        Ok(())
    }

    fn fconst(&mut self, v: f64) -> VReg {
        let dst = self.fresh_float();
        self.push(MInst::FConst {
            double: true,
            dst,
            bits: v.to_bits(),
        });
        dst
    }

    fn falu(&mut self, op: FOp2, a: VReg, b: VReg) -> VReg {
        let dst = self.fresh_float();
        self.push(MInst::FAlu {
            op,
            double: true,
            dst,
            a,
            b,
        });
        dst
    }

    fn fun(&mut self, op: FOp1, src: VReg) -> VReg {
        let dst = self.fresh_float();
        self.push(MInst::FUn {
            op,
            double: true,
            dst,
            src,
        });
        dst
    }

    /// `cc(a, b) ? t : f` for doubles.
    fn fsel(&mut self, a: VReg, b: VReg, cc: Cond, t: VReg, f: VReg) -> VReg {
        self.push(MInst::FCmp { double: true, a, b });
        let dst = self.fresh_float();
        self.push(MInst::FSel { cc, dst, t, f });
        dst
    }

    /// `floor`, `ceil`, `trunc` or `nearest` (ties to even) of the double `x`, inline: VFPv2
    /// and VFPv3 have no rounding instruction. Below 2^52, `(|x| + 2^52) - 2^52` rounds |x| to
    /// the nearest integer (the default round-to-nearest-even mode; the engine never changes
    /// it); one compare adjusts that by ±1. A zero result takes the sign of `x` (`x * 0`).
    /// Larger magnitudes, infinities and NaN are already integral and returned as `x`.
    fn round_f64(&mut self, op: UnaryOp, x: VReg) -> VReg {
        const TWO52: f64 = 4503599627370496.0;
        let a = self.fun(FOp1::Abs, x);
        let big = self.fconst(TWO52);
        let zero = self.fconst(0.0);
        let one = self.fconst(1.0);
        let up = self.falu(FOp2::Add, a, big);
        let t = self.falu(FOp2::Sub, up, big);
        let r = match op {
            UnaryOp::Trunc => {
                // floor(|x|) with the sign of x.
                let tm1 = self.falu(FOp2::Sub, t, one);
                let f = self.fsel(t, a, Cond::Gt, tm1, t);
                let nf = self.fun(FOp1::Neg, f);
                self.fsel(x, zero, Cond::Mi, nf, f)
            }
            _ => {
                let nt = self.fun(FOp1::Neg, t);
                let s = self.fsel(x, zero, Cond::Mi, nt, t);
                match op {
                    UnaryOp::Floor => {
                        let sm1 = self.falu(FOp2::Sub, s, one);
                        self.fsel(s, x, Cond::Gt, sm1, s)
                    }
                    UnaryOp::Ceil => {
                        let sp1 = self.falu(FOp2::Add, s, one);
                        self.fsel(s, x, Cond::Mi, sp1, s)
                    }
                    _ => s,
                }
            }
        };
        let signed_zero = self.falu(FOp2::Mul, x, zero);
        let r = self.fsel(r, zero, Cond::Eq, signed_zero, r);
        // a < 2^52 (false for NaN): the rounded value; otherwise x itself.
        self.fsel(a, big, Cond::Mi, r, x)
    }

    /// 32-bit division or remainder `r = a op b` without a helper call: a constant divisor by
    /// shifts or a multiply-high ([`crate::divmagic`]), otherwise `sdiv`/`udiv` when the target
    /// has them. False when neither applies (the caller calls the helper). Semantics match
    /// [`rt`]: a zero divisor gives 0 and `MIN / -1` wraps.
    fn div32_inline(&mut self, op: BinaryOp, a: Value, b: Value, r: Value) -> bool {
        let signed = matches!(op, BinaryOp::Sdiv | BinaryOp::Srem);
        let rem = matches!(op, BinaryOp::Srem | BinaryOp::Urem);
        let c = self.iconst(b).map(|c| c as u32);
        let res = match c {
            Some(0) => self.mov_imm(0),
            Some(c) if signed && c as i32 != i32::MIN => {
                let ra = self.s(a);
                self.sdiv32_const(ra, c as i32, rem)
            }
            Some(c) if !signed => {
                let ra = self.s(a);
                self.udiv32_const(ra, c, rem)
            }
            _ if self.idiv => {
                let (ra, rb) = (self.s(a), self.s(b));
                let q = self.fresh_int();
                self.push(MInst::Div {
                    signed,
                    dst: q,
                    a: ra,
                    b: rb,
                });
                if rem {
                    // a - q * b, and 0 for a zero divisor (where q is 0, so a - q * b = a).
                    let p = self.fresh_int();
                    self.push(MInst::Mul { dst: p, a: q, b: rb });
                    let m = self.alu_new(Dp::Sub, ra, Rhs::Reg(p));
                    let z = self.mov_imm(0);
                    self.cmp(Dp::Cmp, Cond::Al, rb, Rhs::Imm(0));
                    let out = self.fresh_int();
                    self.push(MInst::Sel {
                        cc: Cond::Eq,
                        dst: out,
                        t: z,
                        f: m,
                    });
                    out
                } else {
                    q
                }
            }
            _ => return false,
        };
        let dst = self.def_s(r);
        self.push(MInst::Mov { dst, src: res });
        true
    }

    /// `a / d` or `a % d` (signed, truncating) for a constant `d` other than 0 and `MIN`.
    fn sdiv32_const(&mut self, a: VReg, d: i32, rem: bool) -> VReg {
        let ad = d.unsigned_abs();
        if ad == 1 {
            return if rem {
                self.mov_imm(0)
            } else if d < 0 {
                self.alu_new(Dp::Rsb, a, Rhs::Imm(0))
            } else {
                a
            };
        }
        // `q` is the quotient by |d| (by d when `exact`).
        let (q, exact) = if ad.is_power_of_two() {
            let k = ad.trailing_zeros() as u8;
            // Round toward zero: add 2^k - 1 to a negative dividend before the shift.
            let sign = self.shift_new(Shift::Asr, a, 31);
            let t = self.alu_new(Dp::Add, a, Rhs::Shift(sign, Shift::Lsr, 32 - k));
            if rem {
                // a - (t & -2^k): the bits of `a` below 2^k, with the sign of `a`.
                let hi = self.and_imm(t, !(ad - 1));
                return self.alu_new(Dp::Sub, a, Rhs::Reg(hi));
            }
            (self.shift_new(Shift::Asr, t, k), false)
        } else {
            let (m, s) = crate::divmagic::signed32(d);
            let mr = self.mov_imm(m as u32);
            let (lo, hi) = (self.fresh_int(), self.fresh_int());
            self.push(MInst::LongMul {
                signed: true,
                lo,
                hi,
                a,
                b: mr,
            });
            let mut q = hi;
            if d > 0 && m < 0 {
                q = self.alu_new(Dp::Add, q, Rhs::Reg(a));
            } else if d < 0 && m > 0 {
                q = self.alu_new(Dp::Sub, q, Rhs::Reg(a));
            }
            if s > 0 {
                q = self.shift_new(Shift::Asr, q, s as u8);
            }
            (self.alu_new(Dp::Add, q, Rhs::Shift(q, Shift::Lsr, 31)), true)
        };
        if rem {
            // a - q * d
            let dr = self.mov_imm(d as u32);
            let p = self.fresh_int();
            self.push(MInst::Mul { dst: p, a: q, b: dr });
            return self.alu_new(Dp::Sub, a, Rhs::Reg(p));
        }
        if d < 0 && !exact {
            self.alu_new(Dp::Rsb, q, Rhs::Imm(0))
        } else {
            q
        }
    }

    /// `a / d` or `a % d` (unsigned) for a constant `d` other than 0.
    fn udiv32_const(&mut self, a: VReg, d: u32, rem: bool) -> VReg {
        if d == 1 {
            return if rem { self.mov_imm(0) } else { a };
        }
        if d.is_power_of_two() {
            let k = d.trailing_zeros() as u8;
            return if rem {
                self.and_imm(a, d - 1)
            } else {
                self.shift_new(Shift::Lsr, a, k)
            };
        }
        let (m, add, s) = crate::divmagic::unsigned32(d);
        let mr = self.mov_imm(m);
        let (lo, hi) = (self.fresh_int(), self.fresh_int());
        self.push(MInst::LongMul {
            signed: false,
            lo,
            hi,
            a,
            b: mr,
        });
        let q = if add {
            let t = self.alu_new(Dp::Sub, a, Rhs::Reg(hi));
            let t = self.alu_new(Dp::Add, hi, Rhs::Shift(t, Shift::Lsr, 1));
            if s > 1 {
                self.shift_new(Shift::Lsr, t, (s - 1) as u8)
            } else {
                t
            }
        } else if s > 0 {
            self.shift_new(Shift::Lsr, hi, s as u8)
        } else {
            hi
        };
        if rem {
            let dr = self.mov_imm(d);
            let p = self.fresh_int();
            self.push(MInst::Mul { dst: p, a: q, b: dr });
            self.alu_new(Dp::Sub, a, Rhs::Reg(p))
        } else {
            q
        }
    }

    /// `a & mask`, with the mask as an immediate when encodable.
    fn and_imm(&mut self, a: VReg, mask: u32) -> VReg {
        if enc(mask) {
            self.alu_new(Dp::And, a, Rhs::Imm(mask))
        } else if enc(!mask) {
            self.alu_new(Dp::Bic, a, Rhs::Imm(!mask))
        } else {
            let m = self.mov_imm(mask);
            self.alu_new(Dp::And, a, Rhs::Reg(m))
        }
    }

    /// 32-bit `clz`, `ctz` or `popcnt` of `src` into `dst`.
    fn count32(&mut self, op: UnaryOp, src: VReg, dst: VReg) {
        match op {
            UnaryOp::Clz => self.push(MInst::Clz { dst, src }),
            UnaryOp::Ctz if self.v7 => {
                let t = self.fresh_int();
                self.push(MInst::Rbit { dst: t, src });
                self.push(MInst::Clz { dst, src: t });
            }
            UnaryOp::Ctz => {
                // No `rbit` before ARMv7: `~x & (x - 1)` keeps exactly the trailing zeros as
                // ones, so ctz = 32 - clz of it (32 for zero, whose mask is all ones).
                let (m, z, c) = (self.fresh_int(), self.fresh_int(), self.fresh_int());
                for i in [
                    MInst::Alu {
                        op: Dp::Sub,
                        s: false,
                        dst: m,
                        a: src,
                        b: Rhs::Imm(1),
                    },
                    MInst::Alu {
                        op: Dp::Bic,
                        s: false,
                        dst: z,
                        a: m,
                        b: Rhs::Reg(src),
                    },
                    MInst::Clz { dst: c, src: z },
                    MInst::Alu {
                        op: Dp::Rsb,
                        s: false,
                        dst,
                        a: c,
                        b: Rhs::Imm(32),
                    },
                ] {
                    self.push(i);
                }
            }
            _ => {
                // x - ((x >> 1) & 0x55555555); pairs, nibbles, then sum the bytes by multiply.
                let m1 = self.mov_imm(0x5555_5555);
                let m2 = self.mov_imm(0x3333_3333);
                let m4 = self.mov_imm(0x0f0f_0f0f);
                let h = self.fresh_int();
                self.push(MInst::Shift {
                    kind: Shift::Lsr,
                    dst: h,
                    a: src,
                    amt: Amt::Imm(1),
                });
                let h = self.alu_new(Dp::And, h, Rhs::Reg(m1));
                let x = self.alu_new(Dp::Sub, src, Rhs::Reg(h));
                let lo2 = self.alu_new(Dp::And, x, Rhs::Reg(m2));
                let hi2 = self.shift_new(Shift::Lsr, x, 2);
                let hi2 = self.alu_new(Dp::And, hi2, Rhs::Reg(m2));
                let x = self.alu_new(Dp::Add, lo2, Rhs::Reg(hi2));
                let y = self.alu_new(Dp::Add, x, Rhs::Shift(x, Shift::Lsr, 4));
                let y = self.alu_new(Dp::And, y, Rhs::Reg(m4));
                let k = self.mov_imm(0x0101_0101);
                let z = self.fresh_int();
                self.push(MInst::Mul { dst: z, a: y, b: k });
                self.push(MInst::Shift {
                    kind: Shift::Lsr,
                    dst,
                    a: z,
                    amt: Amt::Imm(24),
                });
            }
        }
    }

    fn binary(&mut self, op: BinaryOp, a: Value, b: Value, r: Value) -> Result<(), String> {
        let ty = self.ty(a);
        if ty.is_float() {
            return self.fbinary(op, a, b, r);
        }
        if ty == Type::I64 {
            return self.binary64(op, a, b, r);
        }
        use BinaryOp::*;
        match op {
            Iadd | Isub | Band | Bor | Bxor => {
                let (a, b) = if op.is_commutative()
                    && self.iconst(a).is_some()
                    && self.iconst(b).is_none()
                {
                    (b, a)
                } else {
                    (a, b)
                };
                let dst = self.def_s(r);
                let c = self.iconst(b).map(|c| c as u32);
                let ca = self.iconst(a).map(|c| c as u32);
                if op == Isub && c.is_none() {
                    if let Some(ca) = ca.filter(|&c| enc(c)) {
                        let rb = self.s(b);
                        self.alu(Dp::Rsb, false, dst, rb, Rhs::Imm(ca));
                        return Ok(());
                    }
                }
                let ra = self.s(a);
                let (o, rhs) = match (op, c) {
                    (Iadd, Some(c)) if enc(c) => (Dp::Add, Rhs::Imm(c)),
                    (Iadd, Some(c)) if enc(c.wrapping_neg()) => {
                        (Dp::Sub, Rhs::Imm(c.wrapping_neg()))
                    }
                    (Isub, Some(c)) if enc(c) => (Dp::Sub, Rhs::Imm(c)),
                    (Isub, Some(c)) if enc(c.wrapping_neg()) => {
                        (Dp::Add, Rhs::Imm(c.wrapping_neg()))
                    }
                    (Band, Some(c)) if enc(c) => (Dp::And, Rhs::Imm(c)),
                    (Band, Some(c)) if enc(!c) => (Dp::Bic, Rhs::Imm(!c)),
                    (Bor, Some(c)) if enc(c) => (Dp::Orr, Rhs::Imm(c)),
                    (Bxor, Some(c)) if enc(c) => (Dp::Eor, Rhs::Imm(c)),
                    _ => {
                        let o = match op {
                            Iadd => Dp::Add,
                            Isub => Dp::Sub,
                            Band => Dp::And,
                            Bor => Dp::Orr,
                            _ => Dp::Eor,
                        };
                        (o, Rhs::Reg(self.s(b)))
                    }
                };
                self.alu(o, false, dst, ra, rhs);
            }
            Imul => {
                let (ra, rb) = (self.s(a), self.s(b));
                let dst = self.def_s(r);
                self.push(MInst::Mul { dst, a: ra, b: rb });
            }
            Sdiv | Udiv | Srem | Urem if self.div32_inline(op, a, b, r) => {}
            Sdiv | Udiv | Srem | Urem => {
                let f: usize = match op {
                    Sdiv => rt::addr(rt::sdiv32 as extern "C" fn(i32, i32) -> i32),
                    Udiv => rt::addr(rt::udiv32 as extern "C" fn(u32, u32) -> u32),
                    Srem => rt::addr(rt::srem32 as extern "C" fn(i32, i32) -> i32),
                    _ => rt::addr(rt::urem32 as extern "C" fn(u32, u32) -> u32),
                };
                let (va, vb) = (self.val(a), self.val(b));
                let Some(res) = self.rt_call(f, &[Type::I32; 2], Some(Type::I32), &[va, vb])?
                else {
                    unreachable!()
                };
                self.set(r, res);
            }
            Ishl | Ushr | Sshr | Rotl | Rotr => {
                let kind = match op {
                    Ishl => Shift::Lsl,
                    Ushr => Shift::Lsr,
                    Sshr => Shift::Asr,
                    _ => Shift::Ror,
                };
                let ra = self.s(a);
                let dst = self.def_s(r);
                if let Some(c) = self.iconst(b) {
                    let n = (c & 31) as u8;
                    let n = if op == Rotl { (32 - n) & 31 } else { n };
                    if n == 0 {
                        self.copy(dst, ra);
                    } else {
                        self.push(MInst::Shift {
                            kind,
                            dst,
                            a: ra,
                            amt: Amt::Imm(n),
                        });
                    }
                } else {
                    let rb = self.s(b);
                    let amt = match op {
                        Rotr => rb,
                        Rotl => self.alu_new(Dp::Rsb, rb, Rhs::Imm(0)),
                        _ => self.alu_new(Dp::And, rb, Rhs::Imm(31)),
                    };
                    self.push(MInst::Shift {
                        kind,
                        dst,
                        a: ra,
                        amt: Amt::Reg(amt),
                    });
                }
            }
            _ => return Err(format!("arm32: unsupported integer op {op:?}")),
        }
        Ok(())
    }

    fn binary64(&mut self, op: BinaryOp, a: Value, b: Value, r: Value) -> Result<(), String> {
        use BinaryOp::*;
        match op {
            Iadd | Isub => {
                let (alo, ahi) = self.p(a);
                let (blo, bhi) = match self.iconst(b) {
                    Some(c) => (
                        if enc(c as u32) {
                            Rhs::Imm(c as u32)
                        } else {
                            Rhs::Reg(self.mov_imm(c as u32))
                        },
                        if enc((c >> 32) as u32) {
                            Rhs::Imm((c >> 32) as u32)
                        } else {
                            Rhs::Reg(self.mov_imm((c >> 32) as u32))
                        },
                    ),
                    None => {
                        let (l, h) = self.p(b);
                        (Rhs::Reg(l), Rhs::Reg(h))
                    }
                };
                let (dl, dh) = self.def_p(r);
                let (lo, hi) = if op == Iadd {
                    (Dp::Add, Dp::Adc)
                } else {
                    (Dp::Sub, Dp::Sbc)
                };
                self.alu(lo, true, dl, alo, blo);
                self.alu(hi, false, dh, ahi, bhi);
            }
            Band | Bor | Bxor => {
                let o = match op {
                    Band => Dp::And,
                    Bor => Dp::Orr,
                    _ => Dp::Eor,
                };
                let (alo, ahi) = self.p(a);
                let (dl, dh) = self.def_p(r);
                let (blo, bhi) = match self.iconst(b) {
                    Some(c) if enc(c as u32) && enc((c >> 32) as u32) => {
                        (Rhs::Imm(c as u32), Rhs::Imm((c >> 32) as u32))
                    }
                    _ => {
                        let (l, h) = self.p(b);
                        (Rhs::Reg(l), Rhs::Reg(h))
                    }
                };
                self.alu(o, false, dl, alo, blo);
                self.alu(o, false, dh, ahi, bhi);
            }
            Imul => {
                let (alo, ahi) = self.p(a);
                let (blo, bhi) = self.p(b);
                let (dl, dh) = self.def_p(r);
                let t = self.fresh_int();
                self.push(MInst::LongMul {
                    signed: false,
                    lo: dl,
                    hi: t,
                    a: alo,
                    b: blo,
                });
                let t2 = self.fresh_int();
                self.push(MInst::Mla {
                    dst: t2,
                    a: alo,
                    b: bhi,
                    c: t,
                });
                self.push(MInst::Mla {
                    dst: dh,
                    a: ahi,
                    b: blo,
                    c: t2,
                });
            }
            Sdiv | Udiv | Srem | Urem => {
                let f: usize = match op {
                    Sdiv => rt::addr(rt::sdiv64 as extern "C" fn(i64, i64) -> i64),
                    Udiv => rt::addr(rt::udiv64 as extern "C" fn(u64, u64) -> u64),
                    Srem => rt::addr(rt::srem64 as extern "C" fn(i64, i64) -> i64),
                    _ => rt::addr(rt::urem64 as extern "C" fn(u64, u64) -> u64),
                };
                let (va, vb) = (self.val(a), self.val(b));
                let Some(res) = self.rt_call(f, &[Type::I64; 2], Some(Type::I64), &[va, vb])?
                else {
                    unreachable!()
                };
                self.set(r, res);
            }
            Ishl | Ushr | Sshr | Rotl | Rotr => {
                if let Some(c) = self.iconst(b) {
                    let (lo, hi) = self.p(a);
                    let (dl, dh) = self.def_p(r);
                    self.shift64_const(op, (c & 63) as u32, lo, hi, dl, dh);
                } else {
                    let f: usize = match op {
                        Ishl => rt::addr(rt::shl64 as extern "C" fn(u64, u32) -> u64),
                        Ushr => rt::addr(rt::ushr64 as extern "C" fn(u64, u32) -> u64),
                        Sshr => rt::addr(rt::sshr64 as extern "C" fn(i64, u32) -> i64),
                        Rotl => rt::addr(rt::rotl64 as extern "C" fn(u64, u32) -> u64),
                        _ => rt::addr(rt::rotr64 as extern "C" fn(u64, u32) -> u64),
                    };
                    let va = self.val(a);
                    let amount = V::S(self.lo(b));
                    let Some(res) =
                        self.rt_call(f, &[Type::I64, Type::I32], Some(Type::I64), &[va, amount])?
                    else {
                        unreachable!()
                    };
                    self.set(r, res);
                }
            }
            _ => return Err(format!("arm32: unsupported 64-bit op {op:?}")),
        }
        Ok(())
    }

    /// `(lo, hi) = (alo, ahi) op n` for a constant `n` below 64.
    fn shift64_const(&mut self, op: BinaryOp, n: u32, lo: VReg, hi: VReg, dl: VReg, dh: VReg) {
        use BinaryOp::*;
        let sh = |l: &mut Self, kind, dst, a, amt: u32| {
            if amt == 0 {
                l.copy(dst, a);
            } else {
                l.push(MInst::Shift {
                    kind,
                    dst,
                    a,
                    amt: Amt::Imm(amt as u8),
                });
            }
        };
        // dst = (x >> k) | (y << (32 - k)), 0 < k < 32 (with the shift kinds given).
        let funnel = |l: &mut Self, dst, x, kx, y, ky, k: u32| {
            let t = l.shift_new(kx, x, k as u8);
            l.alu(Dp::Orr, false, dst, t, Rhs::Shift(y, ky, (32 - k) as u8));
        };
        if n == 0 {
            self.copy(dl, lo);
            self.copy(dh, hi);
            return;
        }
        match op {
            Ishl => {
                if n < 32 {
                    funnel(self, dh, lo, Shift::Lsr, hi, Shift::Lsl, 32 - n);
                    sh(self, Shift::Lsl, dl, lo, n);
                } else {
                    sh(self, Shift::Lsl, dh, lo, n - 32);
                    self.push(MInst::MovImm { dst: dl, imm: 0 });
                }
            }
            Ushr | Sshr => {
                let kind = if op == Ushr { Shift::Lsr } else { Shift::Asr };
                if n < 32 {
                    funnel(self, dl, lo, Shift::Lsr, hi, Shift::Lsl, n);
                    sh(self, kind, dh, hi, n);
                } else {
                    sh(self, kind, dl, hi, n - 32);
                    if op == Ushr {
                        self.push(MInst::MovImm { dst: dh, imm: 0 });
                    } else {
                        sh(self, Shift::Asr, dh, hi, 31);
                    }
                }
            }
            Rotl | Rotr => {
                let k = if op == Rotl { (64 - n) % 64 } else { n };
                if k == 0 {
                    self.copy(dl, lo);
                    self.copy(dh, hi);
                } else if k < 32 {
                    funnel(self, dl, lo, Shift::Lsr, hi, Shift::Lsl, k);
                    funnel(self, dh, hi, Shift::Lsr, lo, Shift::Lsl, k);
                } else if k == 32 {
                    self.copy(dl, hi);
                    self.copy(dh, lo);
                } else {
                    let m = k - 32;
                    funnel(self, dl, hi, Shift::Lsr, lo, Shift::Lsl, m);
                    funnel(self, dh, lo, Shift::Lsr, hi, Shift::Lsl, m);
                }
            }
            _ => unreachable!(),
        }
    }

    fn fbinary(&mut self, op: BinaryOp, a: Value, b: Value, r: Value) -> Result<(), String> {
        let ty = self.ty(a);
        let double = ty == Type::F64;
        let o = match op {
            BinaryOp::Fadd => FOp2::Add,
            BinaryOp::Fsub => FOp2::Sub,
            BinaryOp::Fmul => FOp2::Mul,
            BinaryOp::Fdiv => FOp2::Div,
            BinaryOp::Fmin | BinaryOp::Fmax => {
                let f: extern "C" fn(f64, f64) -> f64 = if op == BinaryOp::Fmin {
                    rt::f64_min
                } else {
                    rt::f64_max
                };
                let (mut x, mut y) = (self.s(a), self.s(b));
                if !double {
                    for v in [&mut x, &mut y] {
                        let w = self.fresh_float();
                        self.push(MInst::FCvt {
                            to_double: true,
                            dst: w,
                            src: *v,
                        });
                        *v = w;
                    }
                }
                let Some(V::S(z)) = self.rt_call(
                    rt::addr(f),
                    &[Type::F64, Type::F64],
                    Some(Type::F64),
                    &[V::S(x), V::S(y)],
                )?
                else {
                    unreachable!()
                };
                let dst = self.def_s(r);
                if double {
                    self.push(MInst::FMov { dst, src: z });
                } else {
                    self.push(MInst::FCvt {
                        to_double: false,
                        dst,
                        src: z,
                    });
                }
                return Ok(());
            }
            _ => return Err(format!("arm32: unsupported float op {op:?}")),
        };
        let (ra, rb) = (self.s(a), self.s(b));
        let dst = self.def_s(r);
        self.push(MInst::FAlu {
            op: o,
            double,
            dst,
            a: ra,
            b: rb,
        });
        Ok(())
    }

    fn convert(&mut self, op: ConvOp, to: Type, arg: Value, r: Value) -> Result<(), String> {
        let from = self.ty(arg);
        match op {
            ConvOp::Wrap => {
                let lo = self.lo(arg);
                self.set(r, V::S(lo));
            }
            ConvOp::Sext => {
                let x = self.s(arg);
                let hi = self.shift_new(Shift::Asr, x, 31);
                self.set(r, V::P(x, hi));
            }
            ConvOp::Uext => {
                let x = self.s(arg);
                let hi = self.mov_imm(0);
                self.set(r, V::P(x, hi));
            }
            ConvOp::FromSint | ConvOp::FromUint => {
                let signed = op == ConvOp::FromSint;
                let double = to == Type::F64;
                if from == Type::I32 {
                    let src = self.s(arg);
                    let dst = self.def_s(r);
                    self.push(MInst::IntToF {
                        signed,
                        double,
                        dst,
                        src,
                    });
                } else {
                    let f: usize = match (signed, double) {
                        (true, true) => rt::addr(rt::i64_to_f64 as extern "C" fn(i64) -> f64),
                        (false, true) => rt::addr(rt::u64_to_f64 as extern "C" fn(u64) -> f64),
                        (true, false) => rt::addr(rt::i64_to_f32 as extern "C" fn(i64) -> f32),
                        (false, false) => rt::addr(rt::u64_to_f32 as extern "C" fn(u64) -> f32),
                    };
                    let v = self.val(arg);
                    let Some(res) = self.rt_call(f, &[Type::I64], Some(to), &[v])? else {
                        unreachable!()
                    };
                    self.set(r, res);
                }
            }
            ConvOp::ToSint | ConvOp::ToUint | ConvOp::ToSintSat | ConvOp::ToUintSat => {
                let signed = matches!(op, ConvOp::ToSint | ConvOp::ToSintSat);
                let src = self.s(arg);
                if to == Type::I32 {
                    let dst = self.def_s(r);
                    self.push(MInst::FToInt {
                        signed,
                        double: from == Type::F64,
                        dst,
                        src,
                    });
                } else {
                    let x = if from == Type::F32 {
                        let w = self.fresh_float();
                        self.push(MInst::FCvt {
                            to_double: true,
                            dst: w,
                            src,
                        });
                        w
                    } else {
                        src
                    };
                    let f = if signed {
                        rt::addr(rt::f64_to_i64 as extern "C" fn(f64) -> i64)
                    } else {
                        rt::addr(rt::f64_to_u64 as extern "C" fn(f64) -> u64)
                    };
                    let Some(res) = self.rt_call(f, &[Type::F64], Some(Type::I64), &[V::S(x)])?
                    else {
                        unreachable!()
                    };
                    self.set(r, res);
                }
            }
            ConvOp::ToJsInt32 => {
                let src = self.s(arg);
                let x = if from == Type::F32 {
                    let w = self.fresh_float();
                    self.push(MInst::FCvt {
                        to_double: true,
                        dst: w,
                        src,
                    });
                    w
                } else {
                    src
                };
                let dst = self.def_s(r);
                self.push(MInst::JsInt32 { dst, src: x });
            }
            ConvOp::Promote | ConvOp::Demote => {
                let src = self.s(arg);
                let dst = self.def_s(r);
                self.push(MInst::FCvt {
                    to_double: op == ConvOp::Promote,
                    dst,
                    src,
                });
            }
            ConvOp::Bitcast => match (from, to) {
                (Type::I32, Type::F32) => {
                    let src = self.s(arg);
                    let dst = self.def_s(r);
                    self.push(MInst::GprToFpr { dst, src });
                }
                (Type::F32, Type::I32) => {
                    let src = self.s(arg);
                    let dst = self.def_s(r);
                    self.push(MInst::FprToGpr { dst, src });
                }
                (Type::I64, Type::F64) => {
                    let (lo, hi) = self.p(arg);
                    let dst = self.def_s(r);
                    self.push(MInst::PairToF { dst, lo, hi });
                }
                (Type::F64, Type::I64) => {
                    let src = self.s(arg);
                    let (lo, hi) = self.def_p(r);
                    self.push(MInst::FToPair { lo, hi, src });
                }
                _ => {
                    let v = self.val(arg);
                    self.set(r, v);
                }
            },
        }
        Ok(())
    }
}
