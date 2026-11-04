//! ARM32 machine instructions over virtual registers.
//!
//! Every integer virtual register is 32 bits wide (a 64-bit IR value is a pair of them), so
//! instructions mirror A32 data-processing forms directly. Instructions take register operands
//! only (plus the immediate forms), except moves, block and call arguments, which may use a spill
//! slot. A spilled register operand is reloaded into a scratch register by the emitter
//! (`r12`, `lr`, `r11`; `d6`, `d7`) and stored back after the instruction if written.
//!
//! Flags: `Alu { s: true }`, `Cmp` and `FCmp` set the condition flags for the instruction(s)
//! that follow (`Alu { op: Adc | Sbc }`, `Sel`, `SetCc`, `Jcc`, `FSel`). Nothing the emitter adds
//! around an instruction (reloads, stores, moves) changes the flags, so the lowering only has to
//! emit the flag consumer next.

use super::asm::{Cond, Dp, Ext, FOp1, FOp2, LdSt, Shift};
use crate::machinst::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rhs {
    Imm(u32),
    Reg(VReg),
    Shift(VReg, Shift, u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Amt {
    Imm(u8),
    Reg(VReg),
}

/// A load/store address: `base + imm` (encodable, see [`LdSt::offset_ok`]) or `base + index`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Amode {
    Imm(VReg, i32),
    Reg(VReg, VReg),
}

#[derive(Clone, Debug)]
pub enum CallTarget {
    /// An external function by front-end id, patched through a [`super::Reloc`].
    Func(u32),
    /// An absolute address known at compile time (a runtime helper).
    Abs(usize),
    Reg(VReg),
}

/// An argument passed on the stack: `(value, offset from the first stack slot, bytes)`.
pub type StackArg = (VReg, i32, u8);

#[derive(Clone, Debug)]
pub struct CallInfo {
    pub target: CallTarget,
    pub reg_args: Vec<(VReg, PReg)>,
    pub stack_args: Vec<StackArg>,
    pub rets: Vec<(VReg, PReg)>,
    pub clobbers: RegSet,
}

#[derive(Clone, Debug)]
pub enum MInst {
    /// Function entry: parameters arrive in fixed registers or incoming stack slots.
    Args {
        regs: Vec<(VReg, PReg)>,
        stack: Vec<StackArg>,
    },
    Mov {
        dst: VReg,
        src: VReg,
    },
    MovImm {
        dst: VReg,
        imm: u32,
    },
    /// `dst = a op b` for the arithmetic and logical opcodes (`s`: set the flags).
    Alu {
        op: Dp,
        s: bool,
        dst: VReg,
        a: VReg,
        b: Rhs,
    },
    /// `dst = ~b`
    Mvn {
        dst: VReg,
        b: Rhs,
    },
    /// `cmp`, `cmn`, `tst` or `teq`
    Cmp {
        op: Dp,
        cc: Cond,
        a: VReg,
        b: Rhs,
    },
    Shift {
        kind: Shift,
        dst: VReg,
        a: VReg,
        amt: Amt,
    },
    Mul {
        dst: VReg,
        a: VReg,
        b: VReg,
    },
    /// `dst = a * b + c`
    Mla {
        dst: VReg,
        a: VReg,
        b: VReg,
        c: VReg,
    },
    /// `{hi, lo} = a * b`
    LongMul {
        signed: bool,
        lo: VReg,
        hi: VReg,
        a: VReg,
        b: VReg,
    },
    /// `dst = a / b` (`sdiv`/`udiv`; 0 for a zero divisor, `MIN / -1` wraps).
    Div {
        signed: bool,
        dst: VReg,
        a: VReg,
        b: VReg,
    },
    Clz {
        dst: VReg,
        src: VReg,
    },
    Rbit {
        dst: VReg,
        src: VReg,
    },
    Ext {
        e: Ext,
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
    /// `vldr` of a double (or a single, in the low half) from `base + off`.
    FLoad {
        double: bool,
        dst: VReg,
        base: VReg,
        off: i32,
    },
    FStore {
        double: bool,
        src: VReg,
        base: VReg,
        off: i32,
    },
    /// `dst = cc ? t : f`
    Sel {
        cc: Cond,
        dst: VReg,
        t: VReg,
        f: VReg,
    },
    /// `dst = cc ? 1 : 0`
    SetCc {
        cc: Cond,
        dst: VReg,
    },
    FMov {
        dst: VReg,
        src: VReg,
    },
    FConst {
        double: bool,
        dst: VReg,
        bits: u64,
    },
    FAlu {
        op: FOp2,
        double: bool,
        dst: VReg,
        a: VReg,
        b: VReg,
    },
    /// `Neg`, `Abs` or `Sqrt`
    FUn {
        op: FOp1,
        double: bool,
        dst: VReg,
        src: VReg,
    },
    /// `vcmp` + `vmrs`: the flags for the float conditions.
    FCmp {
        double: bool,
        a: VReg,
        b: VReg,
    },
    FSel {
        cc: Cond,
        dst: VReg,
        t: VReg,
        f: VReg,
    },
    FCvt {
        to_double: bool,
        dst: VReg,
        src: VReg,
    },
    IntToF {
        signed: bool,
        double: bool,
        dst: VReg,
        src: VReg,
    },
    /// `vcvt` to a 32-bit integer: truncating, saturating, NaN → 0.
    FToInt {
        signed: bool,
        double: bool,
        dst: VReg,
        src: VReg,
    },
    /// JavaScript `ToInt32` of a double: a saturating conversion, with the (rare) out-of-range
    /// case reduced by a call that preserves every register.
    JsInt32 {
        dst: VReg,
        src: VReg,
    },
    /// Bit-exact `vmov` between a core register and a single register.
    GprToFpr {
        dst: VReg,
        src: VReg,
    },
    FprToGpr {
        dst: VReg,
        src: VReg,
    },
    PairToF {
        dst: VReg,
        lo: VReg,
        hi: VReg,
    },
    FToPair {
        lo: VReg,
        hi: VReg,
        src: VReg,
    },
    Call(Box<CallInfo>),
    Jmp {
        target: usize,
        args: Vec<VReg>,
    },
    Jcc {
        cc: Cond,
        taken: usize,
        not_taken: usize,
    },
    /// Branch on `reg != 0`.
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
    Ret {
        vals: Vec<(VReg, PReg)>,
    },
}

fn uses(out: &mut Vec<Operand>, vs: &[VReg]) {
    for (k, &v) in vs.iter().enumerate() {
        if !vs[..k].contains(&v) {
            out.push(Operand::use_reg(v));
        }
    }
}

fn rhs_uses(out: &mut Vec<Operand>, a: Option<VReg>, b: &Rhs) {
    let mut vs = Vec::with_capacity(2);
    vs.extend(a);
    if let Rhs::Reg(v) | Rhs::Shift(v, _, _) = b {
        vs.push(*v);
    }
    uses(out, &vs);
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
            Mov { dst, src } | FMov { dst, src } => {
                out.push(Operand::use_any(*src));
                out.push(Operand::def_any(*dst));
            }
            MovImm { dst, .. } | FConst { dst, .. } | SetCc { dst, .. } => {
                out.push(Operand::def_reg(*dst))
            }
            Alu { dst, a, b, .. } => {
                rhs_uses(out, Some(*a), b);
                out.push(Operand::def_reg(*dst));
            }
            Mvn { dst, b } => {
                rhs_uses(out, None, b);
                out.push(Operand::def_reg(*dst));
            }
            Cmp { a, b, .. } => rhs_uses(out, Some(*a), b),
            Shift { dst, a, amt, .. } => {
                match amt {
                    Amt::Imm(_) => uses(out, &[*a]),
                    Amt::Reg(s) => uses(out, &[*a, *s]),
                }
                out.push(Operand::def_reg(*dst));
            }
            Mul { dst, a, b } | Div { dst, a, b, .. } | FAlu { dst, a, b, .. } => {
                uses(out, &[*a, *b]);
                out.push(Operand::def_reg(*dst));
            }
            Mla { dst, a, b, c } => {
                uses(out, &[*a, *b, *c]);
                out.push(Operand::def_reg(*dst));
            }
            LongMul { lo, hi, a, b, .. } => {
                uses(out, &[*a, *b]);
                out.push(Operand::def_reg(*lo));
                out.push(Operand::def_reg(*hi));
            }
            Clz { dst, src }
            | Rbit { dst, src }
            | Ext { dst, src, .. }
            | FUn { dst, src, .. }
            | FCvt { dst, src, .. }
            | IntToF { dst, src, .. }
            | FToInt { dst, src, .. }
            | GprToFpr { dst, src }
            | FprToGpr { dst, src } => {
                out.push(Operand::use_reg(*src));
                out.push(Operand::def_reg(*dst));
            }
            JsInt32 { dst, src } => {
                out.push(Operand::use_reg(*src));
                out.push(Operand::def_any(*dst));
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
            FLoad { dst, base, .. } => {
                out.push(Operand::use_reg(*base));
                out.push(Operand::def_reg(*dst));
            }
            FStore { src, base, .. } => {
                out.push(Operand::use_reg(*base));
                out.push(Operand::use_reg(*src));
            }
            Sel { dst, t, f, .. } | FSel { dst, t, f, .. } => {
                uses(out, &[*t, *f]);
                out.push(Operand::def_reg(*dst));
            }
            FCmp { a, b, .. } => uses(out, &[*a, *b]),
            PairToF { dst, lo, hi } => {
                uses(out, &[*lo, *hi]);
                out.push(Operand::def_reg(*dst));
            }
            FToPair { lo, hi, src } => {
                out.push(Operand::use_reg(*src));
                out.push(Operand::def_reg(*lo));
                out.push(Operand::def_reg(*hi));
            }
            Call(c) => {
                let CallInfo {
                    target,
                    reg_args,
                    stack_args,
                    rets,
                    ..
                } = &**c;
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
            BrTable { index: r, .. } | Cbnz { reg: r, .. } => out.push(Operand::use_reg(*r)),
            Ret { vals } => {
                for &(v, r) in vals {
                    out.push(Operand::use_fixed(v, r));
                }
            }
            Jcc { .. } => {}
        }
    }

    fn clobbers(&self) -> RegSet {
        match self {
            MInst::Call(c) => c.clobbers,
            _ => RegSet::EMPTY,
        }
    }

    fn as_move(&self) -> Option<(VReg, VReg)> {
        match self {
            MInst::Mov { dst, src } | MInst::FMov { dst, src } => Some((*dst, *src)),
            _ => None,
        }
    }

    fn is_call(&self) -> bool {
        matches!(self, MInst::Call(_))
    }

    fn jump_args(&self) -> Option<(usize, &[VReg])> {
        match self {
            MInst::Jmp { target, args } => Some((*target, args)),
            _ => None,
        }
    }
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
