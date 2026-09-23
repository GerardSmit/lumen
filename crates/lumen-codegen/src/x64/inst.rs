//! x86-64 machine instructions over virtual registers.
//!
//! Invariant: an I32 value's register always holds it zero-extended to 64 bits (every 32-bit
//! operation does that on x64); only values arriving from outside — parameters and call results —
//! are normalized explicitly.

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
}

/// Condition codes (hardware encoding 0..15), plus the two float equalities that need the
/// parity flag: `FEq` = ZF && !PF, `FNe` = !ZF || PF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CC {
    O,
    NO,
    B,
    AE,
    E,
    NE,
    BE,
    A,
    S,
    NS,
    P,
    NP,
    L,
    GE,
    LE,
    G,
    FEq,
    FNe,
}

impl CC {
    pub fn code(self) -> u8 {
        match self {
            CC::FEq | CC::FNe => panic!("{self:?} has no single encoding"),
            c => c as u8,
        }
    }
    pub fn invert(self) -> CC {
        match self {
            CC::FEq => CC::FNe,
            CC::FNe => CC::FEq,
            c => {
                let n = c as u8 ^ 1;
                // Safe: 0..15 are the hardware conditions in order.
                ALL[n as usize]
            }
        }
    }
}

const ALL: [CC; 16] = [
    CC::O,
    CC::NO,
    CC::B,
    CC::AE,
    CC::E,
    CC::NE,
    CC::BE,
    CC::A,
    CC::S,
    CC::NS,
    CC::P,
    CC::NP,
    CC::L,
    CC::GE,
    CC::LE,
    CC::G,
];

/// A memory operand `[base + index << scale + disp]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Amode {
    pub base: VReg,
    pub index: Option<VReg>,
    pub scale: u8,
    pub disp: i32,
}

/// A register (or its spill slot), or an immediate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegImm {
    Reg(VReg),
    Imm(i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AluOp {
    Add,
    Or,
    And,
    Sub,
    Xor,
}

impl AluOp {
    /// The `/digit` of the immediate group and the base of the `op r/m, r` opcode.
    pub fn digit(self) -> u8 {
        match self {
            AluOp::Add => 0,
            AluOp::Or => 1,
            AluOp::And => 4,
            AluOp::Sub => 5,
            AluOp::Xor => 6,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShiftOp {
    Rol = 0,
    Ror = 1,
    Shl = 4,
    Shr = 5,
    Sar = 7,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitOp {
    Lzcnt,
    Tzcnt,
    Popcnt,
    /// `bsr` + fixup: count leading zeros without LZCNT.
    ClzBsr,
    /// `bsf` + fixup: count trailing zeros without BMI1.
    CtzBsf,
}

/// Sign/zero extension of the low part of a register or memory operand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ext {
    /// `movsx`/`movzx` from 8 or 16 bits into 32 or 64; `movsxd` from 32 into 64.
    S { from: u8, to: Size },
    Z { from: u8 },
}

/// A load's width and extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadKind {
    /// `mov r32/r64, m`
    Plain(Size),
    Ext(Ext),
    F32,
    F64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreKind {
    B8,
    B16,
    B32,
    B64,
    F32,
    F64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XmmOp {
    Add = 0x58,
    Mul = 0x59,
    Sub = 0x5c,
    Div = 0x5e,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaskOp {
    And = 0x54,
    Xor = 0x57,
}

#[derive(Clone, Debug)]
pub enum CallTarget {
    /// An external function by front-end id, patched through a [`super::Reloc`].
    Func(u32),
    Reg(VReg),
}

#[derive(Clone, Debug)]
pub enum MInst {
    /// Function entry: parameters arrive in fixed registers or incoming stack slots
    /// (`[rbp + offset]`).
    Args {
        regs: Vec<(VReg, PReg)>,
        stack: Vec<(VReg, i32)>,
    },
    Mov {
        size: Size,
        dst: VReg,
        src: VReg,
    },
    MovImm {
        size: Size,
        dst: VReg,
        imm: u64,
    },
    /// `dst = dst op src`
    Alu {
        op: AluOp,
        size: Size,
        dst: VReg,
        src: RegImm,
    },
    Cmp {
        size: Size,
        a: VReg,
        b: RegImm,
    },
    Test {
        size: Size,
        a: VReg,
        b: VReg,
    },
    /// `dst = dst * src`
    Imul {
        size: Size,
        dst: VReg,
        src: VReg,
    },
    /// `dst = src * imm`
    Imul3 {
        size: Size,
        dst: VReg,
        src: VReg,
        imm: i32,
    },
    Neg {
        size: Size,
        dst: VReg,
    },
    Not {
        size: Size,
        dst: VReg,
    },
    ShiftImm {
        op: ShiftOp,
        size: Size,
        dst: VReg,
        amt: u8,
    },
    /// Shift by `cl`.
    ShiftCl {
        op: ShiftOp,
        size: Size,
        dst: VReg,
        amt: VReg,
    },
    /// BMI2 `shlx`/`shrx`/`sarx`: `dst = src op amt`.
    ShiftX {
        op: ShiftOp,
        size: Size,
        dst: VReg,
        src: VReg,
        amt: VReg,
    },
    /// Division: dividend in RAX, quotient (RAX) or remainder (RDX) out.
    Div {
        signed: bool,
        size: Size,
        rem: bool,
        dividend: VReg,
        divisor: VReg,
        dst: VReg,
    },
    Bit {
        op: BitOp,
        size: Size,
        dst: VReg,
        src: VReg,
    },
    MovX {
        ext: Ext,
        dst: VReg,
        src: VReg,
    },
    Load {
        kind: LoadKind,
        dst: VReg,
        addr: Amode,
    },
    Store {
        kind: StoreKind,
        src: VReg,
        addr: Amode,
    },
    Lea {
        size: Size,
        dst: VReg,
        addr: Amode,
    },
    /// `setcc` + zero-extension to 32 bits.
    Setcc {
        cc: CC,
        dst: VReg,
    },
    /// `if cc { dst = src }`; `cc` may be `FNe` but not `FEq`.
    Cmov {
        cc: CC,
        size: Size,
        dst: VReg,
        src: VReg,
    },
    XmmMov {
        double: bool,
        dst: VReg,
        src: VReg,
    },
    XmmConst {
        double: bool,
        dst: VReg,
        bits: u64,
    },
    /// `dst = dst op src`
    XmmAlu {
        op: XmmOp,
        double: bool,
        dst: VReg,
        src: VReg,
    },
    XmmSqrt {
        double: bool,
        dst: VReg,
        src: VReg,
    },
    /// WebAssembly `min`/`max` (NaN-propagating, `-0 < +0`) in place.
    XmmMinMax {
        min: bool,
        double: bool,
        dst: VReg,
        src: VReg,
    },
    /// `dst = dst op mask` with a constant mask (negate / abs).
    XmmMask {
        op: MaskOp,
        double: bool,
        dst: VReg,
        mask: u64,
    },
    /// SSE4.1 `roundsd`/`roundss` with the rounding-mode immediate.
    XmmRound {
        double: bool,
        mode: u8,
        dst: VReg,
        src: VReg,
    },
    Ucomis {
        double: bool,
        a: VReg,
        b: VReg,
    },
    /// `if cc { dst = src }` on float registers, via a branch.
    XmmCmov {
        cc: CC,
        double: bool,
        dst: VReg,
        src: VReg,
    },
    CvtIntToFloat {
        src_size: Size,
        double: bool,
        dst: VReg,
        src: VReg,
    },
    CvtFloatToInt {
        dst_size: Size,
        double: bool,
        dst: VReg,
        src: VReg,
    },
    CvtFloatFloat {
        to_double: bool,
        dst: VReg,
        src: VReg,
    },
    /// `movd`/`movq` from a general register.
    GprToXmm {
        size: Size,
        dst: VReg,
        src: VReg,
    },
    /// `movd`/`movq` to a general register.
    XmmToGpr {
        size: Size,
        dst: VReg,
        src: VReg,
    },
    Call {
        target: CallTarget,
        reg_args: Vec<(VReg, PReg)>,
        /// Outgoing stack arguments at `[rsp + offset]`.
        stack_args: Vec<(VReg, i32)>,
        rets: Vec<(VReg, PReg)>,
        clobbers: RegSet,
    },
    Jmp {
        target: usize,
        args: Vec<VReg>,
    },
    Jcc {
        cc: CC,
        taken: usize,
        not_taken: usize,
    },
    BrTable {
        index: VReg,
        targets: Vec<usize>,
        default: usize,
    },
    TrapIf {
        cc: CC,
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
    out.push(Operand::use_reg(a.base));
    if let Some(i) = a.index {
        out.push(Operand::use_reg(i));
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
                for &(v, _) in stack {
                    out.push(Operand::def_any(v));
                }
            }
            Mov { dst, src, .. } | XmmMov { dst, src, .. } => {
                out.push(Operand::use_any(*src));
                out.push(Operand::def_any(*dst));
            }
            MovImm { dst, .. } => out.push(Operand::def_any(*dst)),
            Alu { dst, src, .. } => {
                if let RegImm::Reg(s) = src {
                    out.push(Operand::use_any(*s));
                }
                out.push(Operand::mod_reg(*dst));
            }
            Cmp { a, b, .. } => {
                out.push(Operand::use_reg(*a));
                if let RegImm::Reg(s) = b {
                    out.push(Operand::use_any(*s));
                }
            }
            Test { a, b, .. } => {
                out.push(Operand::use_reg(*a));
                if a != b {
                    out.push(Operand::use_reg(*b));
                }
            }
            Imul { dst, src, .. } => {
                out.push(Operand::use_any(*src));
                out.push(Operand::mod_reg(*dst));
            }
            Imul3 { dst, src, .. } => {
                out.push(Operand::use_any(*src));
                out.push(Operand::def_reg(*dst));
            }
            Neg { dst, .. } | Not { dst, .. } | ShiftImm { dst, .. } => {
                out.push(Operand::mod_reg(*dst))
            }
            ShiftCl { dst, amt, .. } => {
                out.push(Operand::use_fixed(*amt, super::regs::RCX));
                out.push(Operand::mod_reg(*dst));
            }
            ShiftX { dst, src, amt, .. } => {
                out.push(Operand::use_reg(*src));
                out.push(Operand::use_reg(*amt));
                out.push(Operand::def_reg(*dst));
            }
            Div {
                rem,
                dividend,
                divisor,
                dst,
                ..
            } => {
                out.push(Operand::use_fixed(*dividend, super::regs::RAX));
                out.push(Operand {
                    late: true,
                    ..Operand::use_any(*divisor)
                });
                let r = if *rem { super::regs::RDX } else { super::regs::RAX };
                out.push(Operand::def_fixed(*dst, r));
            }
            Bit { dst, src, .. } | MovX { dst, src, .. } => {
                out.push(Operand::use_any(*src));
                out.push(Operand::def_reg(*dst));
            }
            Load { dst, addr, .. } => {
                amode_uses(addr, out);
                out.push(Operand::def_reg(*dst));
            }
            Store { src, addr, .. } => {
                amode_uses(addr, out);
                out.push(Operand::use_reg(*src));
            }
            Lea { dst, addr, .. } => {
                amode_uses(addr, out);
                out.push(Operand::def_reg(*dst));
            }
            Setcc { dst, .. } | XmmConst { dst, .. } => out.push(Operand::def_reg(*dst)),
            Cmov { dst, src, .. } => {
                out.push(Operand::use_any(*src));
                out.push(Operand::mod_reg(*dst));
            }
            XmmAlu { dst, src, .. } => {
                out.push(Operand::use_any(*src));
                out.push(Operand::mod_reg(*dst));
            }
            XmmMinMax { dst, src, .. } | XmmCmov { dst, src, .. } => {
                out.push(Operand::use_reg(*src));
                out.push(Operand::mod_reg(*dst));
            }
            XmmSqrt { dst, src, .. }
            | XmmRound { dst, src, .. }
            | CvtIntToFloat { dst, src, .. }
            | CvtFloatToInt { dst, src, .. }
            | GprToXmm { dst, src, .. }
            | XmmToGpr { dst, src, .. } => {
                out.push(Operand::use_any(*src));
                out.push(Operand::def_reg(*dst));
            }
            CvtFloatFloat { dst, src, .. } => {
                // Late: the emitter zeroes `dst` first to break the false dependency.
                out.push(Operand {
                    late: true,
                    ..Operand::use_any(*src)
                });
                out.push(Operand::def_reg(*dst));
            }
            XmmMask { dst, .. } => out.push(Operand::mod_reg(*dst)),
            Ucomis { a, b, .. } => {
                out.push(Operand::use_reg(*a));
                out.push(Operand::use_any(*b));
            }
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
                for &(v, _) in stack_args {
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
            BrTable { index, .. } => out.push(Operand::use_reg(*index)),
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
            MInst::Div { .. } => RegSet::of(&[super::regs::RAX, super::regs::RDX]),
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
            | MInst::XmmMov { dst, src, .. } => Some((*dst, *src)),
            _ => None,
        }
    }

    fn is_call(&self) -> bool {
        matches!(self, MInst::Call { .. })
    }
}
