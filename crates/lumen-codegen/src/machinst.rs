//! Target-independent machine code containers: virtual/physical registers, operand constraints,
//! and [`VCode`] — lowered target instructions over virtual registers, grouped into blocks —
//! which the register allocator ([`crate::regalloc`]) and the target emitters consume.
//!
//! Operand model (after RyuJIT's RefPositions, docs/jit-notes/lsra.md): instruction `i` reads its
//! uses at position `2i` and writes its defs at `2i + 1`. A *late* use is read at `2i + 1`, so it
//! cannot share a register with a def or a clobber of the same instruction (x64 `div`'s divisor).
//! A *mod* operand is read and written (two-address x64 forms). Fixed-register operands and
//! clobbers model calls, division and ABI boundaries; the emitter moves values into and out of
//! the fixed registers with parallel moves, so fixed operands never constrain allocation itself.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RegClass {
    Int,
    Float,
}

/// A physical register: class and hardware encoding.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PReg {
    pub class: RegClass,
    pub hw: u8,
}

impl PReg {
    pub const fn int(hw: u8) -> PReg {
        PReg {
            class: RegClass::Int,
            hw,
        }
    }
    pub const fn float(hw: u8) -> PReg {
        PReg {
            class: RegClass::Float,
            hw,
        }
    }
    /// Index into a [`RegSet`].
    pub fn bit(self) -> u32 {
        self.hw as u32 + if self.class == RegClass::Float { 32 } else { 0 }
    }
}

impl fmt::Debug for PReg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.class {
            RegClass::Int => write!(f, "r{}", self.hw),
            RegClass::Float => write!(f, "f{}", self.hw),
        }
    }
}

/// A set of physical registers (32 per class).
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct RegSet(pub u64);

impl RegSet {
    pub const EMPTY: RegSet = RegSet(0);
    pub fn of(regs: &[PReg]) -> RegSet {
        let mut s = RegSet(0);
        for &r in regs {
            s.insert(r);
        }
        s
    }
    pub fn insert(&mut self, r: PReg) {
        self.0 |= 1 << r.bit();
    }
    pub fn contains(self, r: PReg) -> bool {
        self.0 & (1 << r.bit()) != 0
    }
    pub fn union(self, o: RegSet) -> RegSet {
        RegSet(self.0 | o.0)
    }
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub fn iter(self) -> impl Iterator<Item = PReg> {
        (0..64).filter(move |b| self.0 & (1 << b) != 0).map(|b| {
            if b >= 32 {
                PReg::float(b as u8 - 32)
            } else {
                PReg::int(b as u8)
            }
        })
    }
}

impl fmt::Debug for RegSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VReg(pub u32);

impl VReg {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Debug for VReg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "%{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperandKind {
    Use,
    Def,
    /// Read and written in place.
    Mod,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Constraint {
    /// Must be in a register at the instruction (a spilled value is reloaded into a scratch).
    Reg,
    /// A register or the value's stack slot (x64 r/m operands, branch arguments).
    RegOrStack,
    /// The emitter moves the value into / out of this register around the instruction.
    Fixed(PReg),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Operand {
    pub vreg: VReg,
    pub kind: OperandKind,
    pub constraint: Constraint,
    /// A use read at the def position (see the module docs).
    pub late: bool,
}

impl Operand {
    pub fn use_reg(vreg: VReg) -> Operand {
        Operand {
            vreg,
            kind: OperandKind::Use,
            constraint: Constraint::Reg,
            late: false,
        }
    }
    pub fn use_any(vreg: VReg) -> Operand {
        Operand {
            vreg,
            kind: OperandKind::Use,
            constraint: Constraint::RegOrStack,
            late: false,
        }
    }
    pub fn use_fixed(vreg: VReg, r: PReg) -> Operand {
        Operand {
            vreg,
            kind: OperandKind::Use,
            constraint: Constraint::Fixed(r),
            late: false,
        }
    }
    pub fn use_late(vreg: VReg) -> Operand {
        Operand {
            late: true,
            ..Operand::use_reg(vreg)
        }
    }
    pub fn def_reg(vreg: VReg) -> Operand {
        Operand {
            vreg,
            kind: OperandKind::Def,
            constraint: Constraint::Reg,
            late: false,
        }
    }
    pub fn def_any(vreg: VReg) -> Operand {
        Operand {
            vreg,
            kind: OperandKind::Def,
            constraint: Constraint::RegOrStack,
            late: false,
        }
    }
    pub fn def_fixed(vreg: VReg, r: PReg) -> Operand {
        Operand {
            vreg,
            kind: OperandKind::Def,
            constraint: Constraint::Fixed(r),
            late: false,
        }
    }
    pub fn mod_reg(vreg: VReg) -> Operand {
        Operand {
            vreg,
            kind: OperandKind::Mod,
            constraint: Constraint::Reg,
            late: false,
        }
    }
}

/// A target instruction as the allocator sees it.
pub trait MachInst: Clone + fmt::Debug {
    /// Every register operand, in a stable order the emitter uses to look up allocations.
    fn operands(&self, out: &mut Vec<Operand>);
    /// Registers overwritten by the instruction (at its def position).
    fn clobbers(&self) -> RegSet {
        RegSet::EMPTY
    }
    /// `(dst, src)` for a register-to-register copy, used as an allocation hint.
    fn as_move(&self) -> Option<(VReg, VReg)> {
        None
    }
    /// Whether this is a call (values live across it prefer callee-saved registers).
    fn is_call(&self) -> bool {
        false
    }
    /// `(target block, args)` of a jump passing block arguments (hints: args and params prefer
    /// one register, so the edge move disappears).
    fn jump_args(&self) -> Option<(usize, &[VReg])> {
        None
    }
}

pub struct VBlock {
    /// Parameters, defined at the block's start position.
    pub params: Vec<VReg>,
    /// Instruction index range.
    pub start: usize,
    pub end: usize,
    pub succs: Vec<usize>,
    pub loop_depth: u32,
}

/// Lowered code for one function.
pub struct VCode<I: MachInst> {
    pub insts: Vec<I>,
    pub blocks: Vec<VBlock>,
    pub vreg_class: Vec<RegClass>,
}

impl<I: MachInst> VCode<I> {
    pub fn new() -> VCode<I> {
        VCode {
            insts: Vec::new(),
            blocks: Vec::new(),
            vreg_class: Vec::new(),
        }
    }
    pub fn new_vreg(&mut self, class: RegClass) -> VReg {
        self.vreg_class.push(class);
        VReg(self.vreg_class.len() as u32 - 1)
    }
    pub fn class(&self, v: VReg) -> RegClass {
        self.vreg_class[v.index()]
    }
}

impl<I: MachInst> Default for VCode<I> {
    fn default() -> Self {
        Self::new()
    }
}

impl<I: MachInst> fmt::Display for VCode<I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (bi, b) in self.blocks.iter().enumerate() {
            writeln!(f, "vblock{bi}{:?} (depth {}) -> {:?}:", b.params, b.loop_depth, b.succs)?;
            for i in b.start..b.end {
                writeln!(f, "  {i:4}: {:?}", self.insts[i])?;
            }
        }
        Ok(())
    }
}

/// Where the allocator put a virtual register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loc {
    Reg(PReg),
    /// Spill slot index (8 bytes each; the target maps it to a frame offset).
    Stack(u32),
    /// Never defined or used.
    None,
}

/// A move between locations. Stack-to-stack moves go through the class's scratch register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Move {
    pub src: Loc,
    pub dst: Loc,
    pub class: RegClass,
}

/// Order a parallel move (all sources read before any destination is written) into a sequence of
/// simple moves, breaking cycles through `scratch` (per class, not allocatable).
///
/// The ready-set algorithm (RyuJIT `resolveEdge`): emit any move whose destination is not a
/// pending source; when only cycles remain, save one source in the scratch register.
pub fn sequence_parallel_moves(moves: &[Move], scratch: impl Fn(RegClass) -> PReg) -> Vec<Move> {
    let mut pending: Vec<Move> = moves.iter().copied().filter(|m| m.src != m.dst).collect();
    let mut out = Vec::with_capacity(pending.len() + 1);
    while !pending.is_empty() {
        let ready = pending
            .iter()
            .position(|m| !pending.iter().any(|o| o.src == m.dst));
        match ready {
            Some(i) => out.push(pending.remove(i)),
            None => {
                // Every destination is still needed as a source: a cycle. Move one source aside.
                let m = pending[0];
                let tmp = Loc::Reg(scratch(m.class));
                out.push(Move {
                    src: m.src,
                    dst: tmp,
                    class: m.class,
                });
                for p in &mut pending {
                    if p.src == m.src {
                        p.src = tmp;
                    }
                }
            }
        }
    }
    out
}
