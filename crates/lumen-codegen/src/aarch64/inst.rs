//! AArch64 machine instructions over virtual registers.
//!
//! Invariant (as on x64): an I32 value's register always holds it zero-extended to 64 bits —
//! every 32-bit (`w`) operation does that — so `uext` is free; only values arriving from outside
//! (parameters and call results, whose upper halves the ABI leaves undefined) are normalized.
//!
//! Instructions are three-address and take register operands only, except moves, block and call
//! arguments, which may use a spill slot directly. A spilled register operand is reloaded into a
//! scratch register by the emitter (`x16`, `x17`, `x30`; `v30`, `v31`).

use super::asm::{Cond, FOp1, FOp2, LdSt, Rrr};
use crate::machinst::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    S32,
    S64,
}

impl Size {
    pub fn w(self) -> bool {
        self == Size::S64
    }
    pub fn bits(self) -> u32 {
        if self.w() {
            64
        } else {
            32
        }
    }
}

/// The second operand of a compare: a register, or an arithmetic immediate
/// (`imm12 << (12 if shift)`, compared negated — `cmn` — when `neg`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpRhs {
    Reg(VReg),
    Imm { imm12: u16, shift: bool, neg: bool },
}

/// A load/store address: `base + imm` (encodable, see [`LdSt::offset_ok`]) or `base + index`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Amode {
    Imm(VReg, i32),
    Reg(VReg, VReg),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShiftOp {
    Lsl,
    Lsr,
    Asr,
    Ror,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitOp {
    Clz,
    Ctz,
    Popcnt,
}

#[derive(Clone, Debug)]
pub enum CallTarget {
    /// An external function by front-end id, patched through a [`super::Reloc`].
    Func(u32),
    Reg(VReg),
}

/// An argument passed on the stack: `(value, offset from the first stack slot, bytes)`.
pub type StackArg = (VReg, i32, u8);

#[derive(Clone, Debug)]
pub enum MInst {
    /// Function entry: parameters arrive in fixed registers or incoming stack slots
    /// (`[fp + 16 + offset]`, `bytes` wide).
    Args {
        regs: Vec<(VReg, PReg)>,
        stack: Vec<StackArg>,
    },
    /// Integer copy; `S32` zero-extends the low half.
    Mov {
        size: Size,
        dst: VReg,
        src: VReg,
    },
    MovImm {
        dst: VReg,
        imm: u64,
    },
    /// Float copy (all 64 bits).
    FMov {
        dst: VReg,
        src: VReg,
    },
    FConst {
        double: bool,
        dst: VReg,
        bits: u64,
    },
    /// `dst = a op b`
    Rrr {
        op: Rrr,
        size: Size,
        dst: VReg,
        a: VReg,
        b: VReg,
    },
    /// `dst = a ± (imm12 << (12 if shift))`
    AddImm {
        sub: bool,
        size: Size,
        dst: VReg,
        a: VReg,
        imm12: u16,
        shift: bool,
    },
    /// `dst = a op imm` with the pre-encoded logical immediate `enc`.
    LogImm {
        op: super::asm::LogImm,
        size: Size,
        dst: VReg,
        a: VReg,
        enc: u32,
    },
    ShiftImm {
        op: ShiftOp,
        size: Size,
        dst: VReg,
        a: VReg,
        amt: u8,
    },
    /// `dst = -src`
    Neg {
        size: Size,
        dst: VReg,
        src: VReg,
    },
    /// `dst = c - a * b`
    Msub {
        size: Size,
        dst: VReg,
        a: VReg,
        b: VReg,
        c: VReg,
    },
    Bit {
        op: BitOp,
        size: Size,
        dst: VReg,
        src: VReg,
    },
    /// Sign-extend the low `from` bits.
    Sext {
        from: u8,
        size: Size,
        dst: VReg,
        src: VReg,
    },
    Load {
        op: LdSt,
        dst: VReg,
        addr: Amode,
    },
    Store {
        op: LdSt,
        src: VReg,
        addr: Amode,
    },
    Cmp {
        size: Size,
        a: VReg,
        b: CmpRhs,
    },
    FCmp {
        double: bool,
        a: VReg,
        b: VReg,
    },
    /// `dst = cc ? 1 : 0`
    CSet {
        cc: Cond,
        dst: VReg,
    },
    /// `dst = cc ? t : f`
    CSel {
        cc: Cond,
        dst: VReg,
        t: VReg,
        f: VReg,
    },
    FCSel {
        cc: Cond,
        dst: VReg,
        t: VReg,
        f: VReg,
    },
    FAlu {
        op: FOp2,
        double: bool,
        dst: VReg,
        a: VReg,
        b: VReg,
    },
    FUn {
        op: FOp1,
        double: bool,
        dst: VReg,
        src: VReg,
    },
    FCvt {
        to_double: bool,
        dst: VReg,
        src: VReg,
    },
    IntToF {
        signed: bool,
        src_size: Size,
        double: bool,
        dst: VReg,
        src: VReg,
    },
    /// `fcvtzs`/`fcvtzu`: truncating, saturating, NaN → 0.
    FToInt {
        signed: bool,
        dst_size: Size,
        double: bool,
        dst: VReg,
        src: VReg,
    },
    /// Bit-exact `fmov` from a general register (`S32`: w → s, `S64`: x → d).
    GprToFpr {
        size: Size,
        dst: VReg,
        src: VReg,
    },
    FprToGpr {
        size: Size,
        dst: VReg,
        src: VReg,
    },
    Call {
        target: CallTarget,
        reg_args: Vec<(VReg, PReg)>,
        /// Outgoing stack arguments at `[sp + offset]`.
        stack_args: Vec<StackArg>,
        rets: Vec<(VReg, PReg)>,
        clobbers: RegSet,
    },
    Jmp {
        target: usize,
        args: Vec<VReg>,
    },
    Jcc {
        cc: Cond,
        taken: usize,
        not_taken: usize,
    },
    /// Branch on `reg != 0` (32-bit test).
    Cbnz {
        reg: VReg,
        taken: usize,
        not_taken: usize,
    },
    BrTable {
        index: VReg,
        targets: Vec<usize>,
        default: usize,
    },
    TrapIf {
        cc: Cond,
        code: u32,
    },
    /// Trap when `reg != 0` (32-bit test).
    TrapNz {
        reg: VReg,
        code: u32,
    },
    Trap {
        code: u32,
    },
    Ret {
        vals: Vec<(VReg, PReg)>,
    },
}

fn amode_uses(a: &Amode, out: &mut Vec<Operand>) {
    match *a {
        Amode::Imm(b, _) => out.push(Operand::use_reg(b)),
        Amode::Reg(b, i) => {
            out.push(Operand::use_reg(b));
            if i != b {
                out.push(Operand::use_reg(i));
            }
        }
    }
}

fn uses(out: &mut Vec<Operand>, vs: &[VReg]) {
    for (k, &v) in vs.iter().enumerate() {
        if !vs[..k].contains(&v) {
            out.push(Operand::use_reg(v));
        }
    }
}

impl MachInst for MInst {
    fn operands(&self, out: &mut Vec<Operand>) {
        use MInst::*;
        match self {
            Args { regs, stack } => {
                for &(v, r) in regs {
                    out.push(Operand::def_fixed(v, r));
                }
                for &(v, _, _) in stack {
                    out.push(Operand::def_any(v));
                }
            }
            Mov { dst, src, .. } | FMov { dst, src } => {
                out.push(Operand::use_any(*src));
                out.push(Operand::def_any(*dst));
            }
            MovImm { dst, .. } => out.push(Operand::def_any(*dst)),
            FConst { dst, .. } | CSet { dst, .. } => out.push(Operand::def_reg(*dst)),
            Rrr { dst, a, b, .. } | FAlu { dst, a, b, .. } => {
                uses(out, &[*a, *b]);
                out.push(Operand::def_reg(*dst));
            }
            CSel { dst, t, f, .. } | FCSel { dst, t, f, .. } => {
                uses(out, &[*t, *f]);
                out.push(Operand::def_reg(*dst));
            }
            AddImm { dst, a, .. }
            | LogImm { dst, a, .. }
            | ShiftImm { dst, a, .. }
            | Bit { dst, src: a, .. }
            | Neg { dst, src: a, .. }
            | Sext { dst, src: a, .. }
            | FUn { dst, src: a, .. }
            | FCvt { dst, src: a, .. }
            | IntToF { dst, src: a, .. }
            | FToInt { dst, src: a, .. }
            | GprToFpr { dst, src: a, .. }
            | FprToGpr { dst, src: a, .. } => {
                out.push(Operand::use_reg(*a));
                out.push(Operand::def_reg(*dst));
            }
            Msub { dst, a, b, c, .. } => {
                uses(out, &[*a, *b, *c]);
                out.push(Operand::def_reg(*dst));
            }
            Load { dst, addr, .. } => {
                amode_uses(addr, out);
                out.push(Operand::def_reg(*dst));
            }
            Store { src, addr, .. } => {
                let n = out.len();
                amode_uses(addr, out);
                if !out[n..].iter().any(|o| o.vreg == *src) {
                    out.push(Operand::use_reg(*src));
                }
            }
            Cmp { a, b, .. } => {
                out.push(Operand::use_reg(*a));
                if let CmpRhs::Reg(b) = b {
                    if b != a {
                        out.push(Operand::use_reg(*b));
                    }
                }
            }
            FCmp { a, b, .. } => uses(out, &[*a, *b]),
            Call {
                target,
                reg_args,
                stack_args,
                rets,
                ..
            } => {
                if let CallTarget::Reg(v) = target {
                    out.push(Operand {
                        late: true,
                        ..Operand::use_any(*v)
                    });
                }
                for &(v, r) in reg_args {
                    out.push(Operand::use_fixed(v, r));
                }
                for &(v, _, _) in stack_args {
                    out.push(Operand::use_any(v));
                }
                for &(v, r) in rets {
                    out.push(Operand::def_fixed(v, r));
                }
            }
            Jmp { args, .. } => {
                for &a in args {
                    out.push(Operand::use_any(a));
                }
            }
            BrTable { index: r, .. } | Cbnz { reg: r, .. } | TrapNz { reg: r, .. } => {
                out.push(Operand::use_reg(*r))
            }
            Ret { vals } => {
                for &(v, r) in vals {
                    out.push(Operand::use_fixed(v, r));
                }
            }
            Jcc { .. } | TrapIf { .. } | Trap { .. } => {}
        }
    }

    fn clobbers(&self) -> RegSet {
        match self {
            MInst::Call { clobbers, .. } => *clobbers,
            _ => RegSet::EMPTY,
        }
    }

    fn as_move(&self) -> Option<(VReg, VReg)> {
        match self {
            MInst::Mov {
                size: Size::S64,
                dst,
                src,
            }
            | MInst::FMov { dst, src } => Some((*dst, *src)),
            _ => None,
        }
    }

    fn is_call(&self) -> bool {
        matches!(self, MInst::Call { .. })
    }

    fn jump_args(&self) -> Option<(usize, &[VReg])> {
        match self {
            MInst::Jmp { target, args } => Some((*target, args)),
            _ => None,
        }
    }
}
