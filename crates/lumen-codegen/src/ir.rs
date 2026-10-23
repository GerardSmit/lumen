//! The IR: a typed SSA control-flow graph.
//!
//! A [`Function`] owns its instructions, values and blocks in flat arenas. Every value has one
//! definition — an instruction result or a block parameter — and a fixed [`Type`]. Blocks take
//! parameters instead of phi nodes; a branch passes one argument per target parameter.
//! Instructions are grouped into blocks in layout order, and each block ends with exactly one
//! terminator.
//!
//! Semantics are those of machine integers and IEEE floats, with a few rules the front ends rely
//! on (all documented on the opcode):
//! - integer division by zero and signed `MIN / -1` are undefined — front ends guard them;
//!   `srem(MIN, -1)` is defined as 0;
//! - shift and rotate amounts are taken modulo the bit width;
//! - float `min`/`max` propagate NaN and order `-0 < +0` (the WebAssembly rules);
//! - float-to-int conversions come in saturating and unchecked forms; the unchecked form is
//!   undefined out of range and front ends guard it.

use std::fmt;

/// A value's type. Pointers are `I64` on native targets and `I32` or `I64` on wasm32 (see
/// [`crate::wasm::Config::ptr32`]); an `I32` address is zero-extended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    I32,
    I64,
    F32,
    F64,
    V128,
}

impl Type {
    pub fn is_int(self) -> bool {
        matches!(self, Type::I32 | Type::I64)
    }
    pub fn is_float(self) -> bool {
        matches!(self, Type::F32 | Type::F64)
    }
    pub fn bits(self) -> u32 {
        match self {
            Type::I32 | Type::F32 => 32,
            Type::I64 | Type::F64 => 64,
            Type::V128 => 128,
        }
    }
}

macro_rules! entity {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub u32);
        impl $name {
            #[inline]
            pub fn index(self) -> usize {
                self.0 as usize
            }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($prefix, "{}"), self.0)
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($prefix, "{}"), self.0)
            }
        }
    };
}

entity!(Value, "v");
entity!(Block, "block");
entity!(Inst, "inst");
entity!(FuncRef, "fn");
entity!(SigRef, "sig");

/// A function signature. Generated code follows the platform C calling convention for it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct Signature {
    pub params: Vec<Type>,
    pub results: Vec<Type>,
}

impl Signature {
    pub fn new(params: Vec<Type>, results: Vec<Type>) -> Signature {
        Signature { params, results }
    }
}

/// A function a [`Call`](InstData::Call) can target, resolved to an address when the caller is
/// finalized.
#[derive(Clone, Debug)]
pub struct ExtFunc {
    pub sig: SigRef,
    /// Front-end identifier, passed back to the resolver.
    pub id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    /// Count leading zeros (int).
    Clz,
    /// Count trailing zeros (int).
    Ctz,
    /// Population count (int).
    Popcnt,
    /// `x == 0` as I32 (int).
    Eqz,
    /// Sign-extend the low 8 bits in place (int).
    Sext8,
    /// Sign-extend the low 16 bits in place (int).
    Sext16,
    /// Sign-extend the low 32 bits in place (I64).
    Sext32,
    Fneg,
    Fabs,
    Sqrt,
    Ceil,
    Floor,
    /// Round toward zero (float).
    Trunc,
    /// Round to nearest, ties to even (float).
    Nearest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    Iadd,
    Isub,
    Imul,
    /// Signed division; undefined for a zero divisor or `MIN / -1`.
    Sdiv,
    /// Unsigned division; undefined for a zero divisor.
    Udiv,
    /// Signed remainder; undefined for a zero divisor, and 0 for `MIN % -1`.
    Srem,
    /// Unsigned remainder; undefined for a zero divisor.
    Urem,
    Band,
    Bor,
    Bxor,
    /// Shift left by the amount modulo the width.
    Ishl,
    /// Logical shift right by the amount modulo the width.
    Ushr,
    /// Arithmetic shift right by the amount modulo the width.
    Sshr,
    Rotl,
    Rotr,
    Fadd,
    Fsub,
    Fmul,
    Fdiv,
    /// NaN-propagating minimum with `-0 < +0`.
    Fmin,
    /// NaN-propagating maximum with `-0 < +0`.
    Fmax,
    Fcopysign,
}

impl BinaryOp {
    pub fn is_commutative(self) -> bool {
        use BinaryOp::*;
        matches!(self, Iadd | Imul | Band | Bor | Bxor | Fadd | Fmul)
    }
}

/// Integer comparison conditions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IntCC {
    Eq,
    Ne,
    Slt,
    Sle,
    Sgt,
    Sge,
    Ult,
    Ule,
    Ugt,
    Uge,
}

impl IntCC {
    /// The condition with operands swapped (`a cc b` == `b cc.swap() a`).
    pub fn swap(self) -> IntCC {
        use IntCC::*;
        match self {
            Eq => Eq,
            Ne => Ne,
            Slt => Sgt,
            Sle => Sge,
            Sgt => Slt,
            Sge => Sle,
            Ult => Ugt,
            Ule => Uge,
            Ugt => Ult,
            Uge => Ule,
        }
    }
    /// The negated condition.
    pub fn inverse(self) -> IntCC {
        use IntCC::*;
        match self {
            Eq => Ne,
            Ne => Eq,
            Slt => Sge,
            Sle => Sgt,
            Sgt => Sle,
            Sge => Slt,
            Ult => Uge,
            Ule => Ugt,
            Ugt => Ule,
            Uge => Ult,
        }
    }
}

/// Ordered float comparisons (false when either operand is NaN), plus `Ne` (true when unordered).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FloatCC {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConvOp {
    /// I64 → I32, keep the low bits.
    Wrap,
    /// I32 → I64, sign-extend.
    Sext,
    /// I32 → I64, zero-extend.
    Uext,
    /// Signed int → float.
    FromSint,
    /// Unsigned int → float.
    FromUint,
    /// Float → signed int, truncating; undefined out of range or for NaN.
    ToSint,
    /// Float → unsigned int, truncating; undefined out of range or for NaN.
    ToUint,
    /// Float → signed int, truncating and saturating (NaN → 0).
    ToSintSat,
    /// Float → unsigned int, truncating and saturating (NaN → 0).
    ToUintSat,
    /// JavaScript ToInt32: truncate and reduce modulo 2^32; NaN/infinity -> 0.
    ToJsInt32,
    /// F32 → F64.
    Promote,
    /// F64 → F32.
    Demote,
    /// Same-width reinterpretation of the bits.
    Bitcast,
}

/// Width and extension of a memory access. The loaded value's type is [`MemKind::ty`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemKind {
    I32,
    I64,
    F32,
    F64,
    V128,
    I32S8,
    I32U8,
    I32S16,
    I32U16,
    I64S8,
    I64U8,
    I64S16,
    I64U16,
    I64S32,
    I64U32,
}

impl MemKind {
    /// Type of the value loaded or stored.
    pub fn ty(self) -> Type {
        use MemKind::*;
        match self {
            I32 | I32S8 | I32U8 | I32S16 | I32U16 => Type::I32,
            I64 | I64S8 | I64U8 | I64S16 | I64U16 | I64S32 | I64U32 => Type::I64,
            F32 => Type::F32,
            F64 => Type::F64,
            V128 => Type::V128,
        }
    }
    /// Bytes accessed.
    pub fn bytes(self) -> u32 {
        use MemKind::*;
        match self {
            I32S8 | I32U8 | I64S8 | I64U8 => 1,
            I32S16 | I32U16 | I64S16 | I64U16 => 2,
            I32 | F32 | I64S32 | I64U32 => 4,
            I64 | F64 => 8,
            V128 => 16,
        }
    }
    pub fn is_signed(self) -> bool {
        use MemKind::*;
        matches!(self, I32S8 | I32S16 | I64S8 | I64S16 | I64S32)
    }
}

/// A region of memory for alias analysis: accesses in two different known classes never overlap,
/// except that [`AliasClass::HEAP`] contains the parts numbered [`AliasClass::HEAP_PARTS`]
/// (disjoint from one another), which front ends use to tell apart kinds of heap memory.
/// [`AliasClass::UNKNOWN`] may overlap anything; the other predefined classes describe the JIT's
/// memory, and front ends may number further disjoint regions up to [`AliasClass::LAST`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct AliasClass(pub u8);

impl AliasClass {
    pub const UNKNOWN: AliasClass = AliasClass(0);
    /// The JIT frame header and its local slots.
    pub const FRAME: AliasClass = AliasClass(1);
    /// The JIT's operand stack and scratch slots.
    pub const STACK: AliasClass = AliasClass(2);
    /// Object fields, element storage and other engine heap memory.
    pub const HEAP: AliasClass = AliasClass(3);
    pub const OTHER: AliasClass = AliasClass(4);
    pub const LAST: u8 = 31;
    /// The numbers of the parts of [`AliasClass::HEAP`].
    pub const HEAP_PARTS: std::ops::RangeInclusive<u8> = 16..=23;

    pub fn is_known(self) -> bool {
        self.0 != 0
    }

    /// Whether accesses in the two classes may overlap.
    pub fn may_alias(self, other: AliasClass) -> bool {
        AliasSet::of(self).may_touch(other)
    }

    /// The class with its number clamped to [`AliasClass::LAST`], as [`AliasSet`] stores it.
    pub fn clamped(self) -> AliasClass {
        AliasClass(self.0.min(AliasClass::LAST))
    }
}

/// A set of [`AliasClass`]es (bit `n` is class `n`); a set holding [`AliasClass::UNKNOWN`]
/// is [`AliasSet::ALL`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AliasSet(pub u32);

impl AliasSet {
    pub const NONE: AliasSet = AliasSet(0);
    pub const ALL: AliasSet = AliasSet(u32::MAX);

    /// The memory an access in `class` may touch: the class, and for [`AliasClass::HEAP`] its
    /// parts, or for one of the parts the heap as a whole. [`AliasSet::may_touch`] tests one
    /// class, so a part's set does not reach its sibling parts.
    pub fn of(class: AliasClass) -> AliasSet {
        if !class.is_known() {
            return AliasSet::ALL;
        }
        let c = class.clamped().0;
        let parts = AliasClass::HEAP_PARTS;
        if c == AliasClass::HEAP.0 {
            let mask = (1u32 << (parts.end() + 1)) - (1u32 << parts.start());
            AliasSet(1 << c | mask)
        } else if parts.contains(&c) {
            AliasSet(1 << c | 1 << AliasClass::HEAP.0)
        } else {
            AliasSet(1 << c)
        }
    }

    pub fn with(self, class: AliasClass) -> AliasSet {
        self.union(AliasSet::of(class))
    }

    /// The set minus a known class (`ALL.without(FRAME)`: everything but the frame); without
    /// [`AliasClass::HEAP`], also without its parts.
    pub fn without(self, class: AliasClass) -> AliasSet {
        if !class.is_known() {
            return self;
        }
        let c = class.clamped().0;
        if c == AliasClass::HEAP.0 {
            AliasSet(self.0 & !AliasSet::of(class).0)
        } else {
            AliasSet(self.0 & !(1 << c))
        }
    }

    pub fn union(self, other: AliasSet) -> AliasSet {
        AliasSet(self.0 | other.0)
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether memory of `class` may be in the set.
    pub fn may_touch(self, class: AliasClass) -> bool {
        if class.is_known() {
            self.0 & (1 << class.clamped().0) != 0
        } else {
            self.0 != 0
        }
    }
}

/// What the optimizer may assume about a load or store; the default assumes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct MemFlags {
    pub class: AliasClass,
    /// The location is never written while the function runs (by it or anything it calls), so
    /// loads of it may be merged and moved past any store or call.
    pub readonly: bool,
    /// The address is always dereferenceable, so the load may run where the program would not
    /// have run it (hoisted out of a conditional or ahead of a loop's first test).
    pub notrap: bool,
}

impl MemFlags {
    pub const DEFAULT: MemFlags = MemFlags {
        class: AliasClass::UNKNOWN,
        readonly: false,
        notrap: false,
    };

    pub fn new(class: AliasClass) -> MemFlags {
        debug_assert!(class.0 <= AliasClass::LAST, "alias class {} out of range", class.0);
        MemFlags {
            class,
            ..MemFlags::DEFAULT
        }
    }

    pub fn readonly(mut self) -> MemFlags {
        self.readonly = true;
        self
    }

    pub fn notrap(mut self) -> MemFlags {
        self.notrap = true;
        self
    }
}

impl fmt::Display for MemFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.class {
            AliasClass::UNKNOWN => write!(f, "any")?,
            AliasClass::FRAME => write!(f, "frame")?,
            AliasClass::STACK => write!(f, "stack")?,
            AliasClass::HEAP => write!(f, "heap")?,
            AliasClass::OTHER => write!(f, "other")?,
            AliasClass(n) => write!(f, "class{n}")?,
        }
        if self.readonly {
            write!(f, " readonly")?;
        }
        if self.notrap {
            write!(f, " notrap")?;
        }
        Ok(())
    }
}

/// The memory a call may read and write; the default may touch everything. Calls are never
/// moved or removed; the effects only say which loads and known memory contents survive them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CallEffects {
    pub reads: AliasSet,
    pub writes: AliasSet,
}

impl Default for CallEffects {
    fn default() -> CallEffects {
        CallEffects::CLOBBERS_ALL
    }
}

impl CallEffects {
    pub const CLOBBERS_ALL: CallEffects = CallEffects {
        reads: AliasSet::ALL,
        writes: AliasSet::ALL,
    };
    /// Touches no memory the function can see.
    pub const READNONE: CallEffects = CallEffects {
        reads: AliasSet::NONE,
        writes: AliasSet::NONE,
    };
    /// Reads anything, writes nothing.
    pub const READONLY: CallEffects = CallEffects {
        reads: AliasSet::ALL,
        writes: AliasSet::NONE,
    };

    /// Reads anything, writes only `writes`.
    pub fn writes(writes: AliasSet) -> CallEffects {
        CallEffects {
            reads: AliasSet::ALL,
            writes,
        }
    }
}

/// A branch target with the arguments for its parameters.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlockCall {
    pub block: Block,
    pub args: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum InstData {
    /// Optional read-prefetch hint; never changes program semantics or accesses memory in the oracle.
    Prefetch {
        addr: Value,
        offset: i32,
    },
    Vzero,
    VectorBinary {
        op: VectorOp,
        args: [Value; 2],
    },
    Iconst {
        ty: Type,
        imm: i64,
    },
    /// Load a pointer from a symbolic GOT slot. `id` is resolved by the AOT linker.
    SymbolAddr {
        id: u32,
    },
    F32const {
        bits: u32,
    },
    F64const {
        bits: u64,
    },
    Unary {
        op: UnaryOp,
        arg: Value,
    },
    Binary {
        op: BinaryOp,
        args: [Value; 2],
    },
    /// Signed I32 arithmetic: wrapped I32 result and I32 overflow flag (0/1).
    CheckedBinary {
        op: CheckedOp,
        args: [Value; 2],
    },
    /// `args[0] cc args[1]` as I32 0/1.
    IntCmp {
        cc: IntCC,
        args: [Value; 2],
    },
    /// `args[0] cc args[1]` as I32 0/1.
    FloatCmp {
        cc: FloatCC,
        args: [Value; 2],
    },
    /// `cond != 0 ? if_true : if_false`; `cond` is I32.
    Select {
        cond: Value,
        if_true: Value,
        if_false: Value,
    },
    Convert {
        op: ConvOp,
        to: Type,
        arg: Value,
    },
    /// Load from the raw address `addr + offset` (`addr` is I64, or I32 on wasm32).
    Load {
        kind: MemKind,
        addr: Value,
        offset: i32,
    },
    /// Store `value`'s low [`MemKind::bytes`] to the raw address `addr + offset`.
    Store {
        kind: MemKind,
        addr: Value,
        value: Value,
        offset: i32,
    },
    /// Call a known function.
    Call {
        func: FuncRef,
        args: Vec<Value>,
    },
    /// Call the function at address `callee` (I64, or an I32 table index on wasm32) with
    /// signature `sig`.
    CallIndirect {
        sig: SigRef,
        callee: Value,
        args: Vec<Value>,
    },
    /// Leave through the function's trap exit with `code`.
    Trap {
        code: u32,
    },
    /// Trap with `code` when `cond` (I32) is nonzero.
    TrapIf {
        cond: Value,
        code: u32,
    },
    Jump {
        dest: BlockCall,
    },
    /// Branch to `then` when `cond` (I32) is nonzero, else to `else_`.
    Brif {
        cond: Value,
        then: BlockCall,
        else_: BlockCall,
    },
    /// Branch to `targets[index]`, or `default` when `index` (I32, unsigned) is out of range.
    BrTable {
        index: Value,
        targets: Vec<BlockCall>,
        default: BlockCall,
    },
    Return {
        args: Vec<Value>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CheckedOp {
    IaddOv,
    IsubOv,
    ImulOv,
}

/// Lane arithmetic wraps for I32; floating minimum/maximum have the scalar IEEE semantics.
/// Comparisons produce all-one or all-zero lane masks. AndNot is `a & !b`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VectorOp {
    I32x4Add,
    I32x4Mul,
    I32x4Min,
    I32x4Max,
    I32x4Eq,
    I32x4Lt,
    F64x2Add,
    F64x2Mul,
    F64x2Min,
    F64x2Max,
    F64x2Eq,
    F64x2Lt,
    And,
    AndNot,
    Or,
}

impl InstData {
    pub fn is_terminator(&self) -> bool {
        matches!(
            self,
            InstData::Trap { .. }
                | InstData::Jump { .. }
                | InstData::Brif { .. }
                | InstData::BrTable { .. }
                | InstData::Return { .. }
        )
    }

    /// Whether the instruction can be removed when its results are unused and moved or merged
    /// freely: no memory effects, no calls, no traps, no control flow.
    pub fn is_pure(&self) -> bool {
        matches!(
            self,
            InstData::Vzero
                | InstData::VectorBinary { .. }
                | InstData::Iconst { .. }
                | InstData::F32const { .. }
                | InstData::F64const { .. }
                | InstData::Unary { .. }
                | InstData::CheckedBinary { .. }
                | InstData::IntCmp { .. }
                | InstData::FloatCmp { .. }
                | InstData::Select { .. }
        ) || matches!(self, InstData::Binary { op, .. } if !matches!(op,
                BinaryOp::Sdiv | BinaryOp::Udiv | BinaryOp::Srem | BinaryOp::Urem))
            || matches!(self, InstData::Convert { op, .. } if !matches!(op,
                ConvOp::ToSint | ConvOp::ToUint))
    }

    /// Visit the value operands, including branch arguments.
    pub fn for_each_arg(&self, mut f: impl FnMut(Value)) {
        match self {
            InstData::Vzero => {}
            InstData::Iconst { .. }
            | InstData::F32const { .. }
            | InstData::F64const { .. }
            | InstData::Trap { .. } => {}
            InstData::SymbolAddr { .. } => {}
            InstData::Unary { arg, .. } | InstData::Convert { arg, .. } => f(*arg),
            InstData::Binary { args, .. }
            | InstData::VectorBinary { args, .. }
            | InstData::CheckedBinary { args, .. }
            | InstData::IntCmp { args, .. }
            | InstData::FloatCmp { args, .. } => {
                f(args[0]);
                f(args[1]);
            }
            InstData::Select {
                cond,
                if_true,
                if_false,
            } => {
                f(*cond);
                f(*if_true);
                f(*if_false);
            }
            InstData::Load { addr, .. } | InstData::Prefetch { addr, .. } => f(*addr),
            InstData::Store { addr, value, .. } => {
                f(*addr);
                f(*value);
            }
            InstData::Call { args, .. } | InstData::Return { args } => {
                args.iter().for_each(|&a| f(a))
            }
            InstData::CallIndirect { callee, args, .. } => {
                f(*callee);
                args.iter().for_each(|&a| f(a));
            }
            InstData::TrapIf { cond, .. } => f(*cond),
            InstData::Jump { dest } => dest.args.iter().for_each(|&a| f(a)),
            InstData::Brif { cond, then, else_ } => {
                f(*cond);
                then.args.iter().for_each(|&a| f(a));
                else_.args.iter().for_each(|&a| f(a));
            }
            InstData::BrTable {
                index,
                targets,
                default,
            } => {
                f(*index);
                for t in targets {
                    t.args.iter().for_each(|&a| f(a));
                }
                default.args.iter().for_each(|&a| f(a));
            }
        }
    }

    /// Rewrite every value operand in place.
    pub fn map_args(&mut self, mut f: impl FnMut(Value) -> Value) {
        fn call(c: &mut BlockCall, f: &mut impl FnMut(Value) -> Value) {
            for a in &mut c.args {
                *a = f(*a);
            }
        }
        match self {
            InstData::Vzero => {}
            InstData::Iconst { .. }
            | InstData::F32const { .. }
            | InstData::F64const { .. }
            | InstData::Trap { .. } => {}
            InstData::SymbolAddr { .. } => {}
            InstData::Unary { arg, .. } | InstData::Convert { arg, .. } => *arg = f(*arg),
            InstData::Binary { args, .. }
            | InstData::VectorBinary { args, .. }
            | InstData::CheckedBinary { args, .. }
            | InstData::IntCmp { args, .. }
            | InstData::FloatCmp { args, .. } => {
                args[0] = f(args[0]);
                args[1] = f(args[1]);
            }
            InstData::Select {
                cond,
                if_true,
                if_false,
            } => {
                *cond = f(*cond);
                *if_true = f(*if_true);
                *if_false = f(*if_false);
            }
            InstData::Load { addr, .. } | InstData::Prefetch { addr, .. } => *addr = f(*addr),
            InstData::Store { addr, value, .. } => {
                *addr = f(*addr);
                *value = f(*value);
            }
            InstData::Call { args, .. } | InstData::Return { args } => {
                for a in args {
                    *a = f(*a);
                }
            }
            InstData::CallIndirect { callee, args, .. } => {
                *callee = f(*callee);
                for a in args {
                    *a = f(*a);
                }
            }
            InstData::TrapIf { cond, .. } => *cond = f(*cond),
            InstData::Jump { dest } => call(dest, &mut f),
            InstData::Brif { cond, then, else_ } => {
                *cond = f(*cond);
                call(then, &mut f);
                call(else_, &mut f);
            }
            InstData::BrTable {
                index,
                targets,
                default,
            } => {
                *index = f(*index);
                for t in targets {
                    call(t, &mut f);
                }
                call(default, &mut f);
            }
        }
    }

    /// The branch targets of a terminator.
    pub fn successors(&self) -> Successors<'_> {
        match self {
            InstData::Jump { dest } => Successors {
                first: Some(dest),
                second: None,
                rest: &[],
                last: None,
            },
            InstData::Brif { then, else_, .. } => Successors {
                first: Some(then),
                second: Some(else_),
                rest: &[],
                last: None,
            },
            InstData::BrTable {
                targets, default, ..
            } => Successors {
                first: None,
                second: None,
                rest: targets,
                last: Some(default),
            },
            _ => Successors {
                first: None,
                second: None,
                rest: &[],
                last: None,
            },
        }
    }

    pub fn successor_mut(&mut self, slot: usize) -> &mut BlockCall {
        match self {
            InstData::Jump { dest } => dest,
            InstData::Brif { then, else_, .. } => {
                if slot == 0 {
                    then
                } else {
                    else_
                }
            }
            InstData::BrTable {
                targets, default, ..
            } => match targets.get_mut(slot) {
                Some(t) => t,
                None => default,
            },
            _ => panic!("not a branch"),
        }
    }

    pub fn for_each_successor_mut(&mut self, mut f: impl FnMut(&mut BlockCall)) {
        match self {
            InstData::Jump { dest } => f(dest),
            InstData::Brif { then, else_, .. } => {
                f(then);
                f(else_);
            }
            InstData::BrTable {
                targets, default, ..
            } => {
                targets.iter_mut().for_each(&mut f);
                f(default);
            }
            _ => {}
        }
    }
}

/// The branch targets of a terminator, in slot order, without allocating.
#[derive(Clone, Copy)]
pub struct Successors<'a> {
    first: Option<&'a BlockCall>,
    second: Option<&'a BlockCall>,
    rest: &'a [BlockCall],
    last: Option<&'a BlockCall>,
}

impl<'a> Successors<'a> {
    pub fn len(&self) -> usize {
        self.first.is_some() as usize
            + self.second.is_some() as usize
            + self.rest.len()
            + self.last.is_some() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn iter(&self) -> SuccessorIter<'a> {
        SuccessorIter { s: *self }
    }
}

impl std::ops::Index<usize> for Successors<'_> {
    type Output = BlockCall;
    fn index(&self, mut i: usize) -> &BlockCall {
        for single in [self.first, self.second].into_iter().flatten() {
            if i == 0 {
                return single;
            }
            i -= 1;
        }
        if i < self.rest.len() {
            return &self.rest[i];
        }
        self.last.expect("successor slot")
    }
}

pub struct SuccessorIter<'a> {
    s: Successors<'a>,
}

impl<'a> Iterator for SuccessorIter<'a> {
    type Item = &'a BlockCall;
    fn next(&mut self) -> Option<&'a BlockCall> {
        if let Some(c) = self.s.first.take() {
            return Some(c);
        }
        if let Some(c) = self.s.second.take() {
            return Some(c);
        }
        if let Some((c, rest)) = self.s.rest.split_first() {
            self.s.rest = rest;
            return Some(c);
        }
        self.s.last.take()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.s.len();
        (n, Some(n))
    }
}

impl ExactSizeIterator for SuccessorIter<'_> {}

impl<'a> IntoIterator for Successors<'a> {
    type Item = &'a BlockCall;
    type IntoIter = SuccessorIter<'a>;
    fn into_iter(self) -> SuccessorIter<'a> {
        self.iter()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueDef {
    /// Result `n` of an instruction.
    Result(Inst, u32),
    /// Parameter `n` of a block.
    Param(Block, u32),
    /// Replaced by another value (see [`Function::replace_uses`]); resolved by [`Function::resolve`].
    Alias(Value),
}

#[derive(Clone, Debug)]
pub struct ValueData {
    pub ty: Type,
    pub def: ValueDef,
}

#[derive(Clone, Debug, Default)]
pub struct BlockData {
    pub params: Vec<Value>,
    pub insts: Vec<Inst>,
    /// Rarely executed (a slow path or exit): values only such blocks read are computed there.
    pub cold: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Function {
    pub name: String,
    pub sig: Signature,
    pub insts: Vec<InstData>,
    /// Where each instruction's results start in `result_pool` (they run to the next
    /// instruction's start): one flat array instead of a vector per instruction.
    inst_results: Vec<u32>,
    result_pool: Vec<Value>,
    pub values: Vec<ValueData>,
    pub blocks: Vec<BlockData>,
    /// Blocks in layout order; the first is the entry, whose parameters are the function's.
    pub layout: Vec<Block>,
    pub sigs: Vec<Signature>,
    pub funcs: Vec<ExtFunc>,
    /// Some operand may still name an alias: set by [`Function::replace_uses`], cleared by
    /// [`Function::resolve_aliases`].
    pending_aliases: bool,
    /// Flags of each load and store, by instruction (shorter than `insts`: the rest default).
    mem_flags: Vec<MemFlags>,
    /// Effects of calls to each imported function, by [`FuncRef`] (the rest clobber all).
    func_effects: Vec<CallEffects>,
    /// Per-call-site effects, by instruction; they override `func_effects`.
    site_effects: Vec<Option<CallEffects>>,
}

impl Function {
    pub fn new(name: impl Into<String>, sig: Signature) -> Function {
        Function {
            name: name.into(),
            sig,
            ..Function::default()
        }
    }

    pub fn entry(&self) -> Block {
        self.layout[0]
    }

    pub fn value_type(&self, v: Value) -> Type {
        self.values[v.index()].ty
    }

    pub fn results(&self, inst: Inst) -> &[Value] {
        let i = inst.index();
        let end = self
            .inst_results
            .get(i + 1)
            .map_or(self.result_pool.len(), |&e| e as usize);
        &self.result_pool[self.inst_results[i] as usize..end]
    }

    pub fn inst(&self, inst: Inst) -> &InstData {
        &self.insts[inst.index()]
    }

    pub fn import_signature(&mut self, sig: Signature) -> SigRef {
        if let Some(i) = self.sigs.iter().position(|s| *s == sig) {
            return SigRef(i as u32);
        }
        self.sigs.push(sig);
        SigRef(self.sigs.len() as u32 - 1)
    }

    pub fn import_function(&mut self, sig: Signature, id: u32) -> FuncRef {
        let sig = self.import_signature(sig);
        self.funcs.push(ExtFunc { sig, id });
        FuncRef(self.funcs.len() as u32 - 1)
    }

    /// [`Function::import_function`] with the effects every call to it has.
    pub fn import_function_with_effects(
        &mut self,
        sig: Signature,
        id: u32,
        effects: CallEffects,
    ) -> FuncRef {
        let f = self.import_function(sig, id);
        self.set_func_effects(f, effects);
        f
    }

    pub fn set_func_effects(&mut self, func: FuncRef, effects: CallEffects) {
        let i = func.index();
        if self.func_effects.len() <= i {
            self.func_effects.resize(i + 1, CallEffects::CLOBBERS_ALL);
        }
        self.func_effects[i] = effects;
    }

    pub fn func_effects(&self, func: FuncRef) -> CallEffects {
        self.func_effects
            .get(func.index())
            .copied()
            .unwrap_or_default()
    }

    /// Effects of the call `inst` alone (overriding its function's).
    pub fn set_call_effects(&mut self, inst: Inst, effects: CallEffects) {
        let i = inst.index();
        if self.site_effects.len() <= i {
            self.site_effects.resize(i + 1, None);
        }
        self.site_effects[i] = Some(effects);
    }

    /// The effects of `inst` when it is a call.
    pub fn call_effects(&self, inst: Inst) -> CallEffects {
        if let Some(Some(e)) = self.site_effects.get(inst.index()) {
            return *e;
        }
        match self.inst(inst) {
            InstData::Call { func, .. } => self.func_effects(*func),
            _ => CallEffects::CLOBBERS_ALL,
        }
    }

    pub fn set_mem_flags(&mut self, inst: Inst, flags: MemFlags) {
        debug_assert!(
            flags.class.0 <= AliasClass::LAST,
            "alias class {} out of range",
            flags.class.0
        );
        let i = inst.index();
        if self.mem_flags.len() <= i {
            if flags == MemFlags::DEFAULT {
                return;
            }
            self.mem_flags.resize(i + 1, MemFlags::DEFAULT);
        }
        self.mem_flags[i] = flags;
    }

    pub fn mem_flags(&self, inst: Inst) -> MemFlags {
        self.mem_flags
            .get(inst.index())
            .copied()
            .unwrap_or_default()
    }

    /// Set the flags of the load defining `v`; false when `v` is not a load's result.
    pub fn mark_load(&mut self, v: Value, flags: MemFlags) -> bool {
        match self.values[self.resolve(v).index()].def {
            ValueDef::Result(inst, 0) if matches!(self.inst(inst), InstData::Load { .. }) => {
                self.set_mem_flags(inst, flags);
                true
            }
            _ => false,
        }
    }

    /// Carry `from`'s memory flags and call effects over to its copy `to`.
    pub fn copy_inst_meta(&mut self, from: Inst, to: Inst) {
        let flags = self.mem_flags(from);
        self.set_mem_flags(to, flags);
        if let Some(Some(e)) = self.site_effects.get(from.index()).copied() {
            self.set_call_effects(to, e);
        }
    }

    /// The memory `inst` may write.
    pub fn writes(&self, inst: Inst) -> AliasSet {
        match self.inst(inst) {
            InstData::Store { .. } => AliasSet::of(self.mem_flags(inst).class),
            InstData::Call { .. } | InstData::CallIndirect { .. } => self.call_effects(inst).writes,
            _ => AliasSet::NONE,
        }
    }

    pub fn create_block(&mut self) -> Block {
        self.blocks.push(BlockData::default());
        Block(self.blocks.len() as u32 - 1)
    }

    pub fn append_block_param(&mut self, block: Block, ty: Type) -> Value {
        let n = self.blocks[block.index()].params.len() as u32;
        let v = self.make_value(ty, ValueDef::Param(block, n));
        self.blocks[block.index()].params.push(v);
        v
    }

    fn make_value(&mut self, ty: Type, def: ValueDef) -> Value {
        self.values.push(ValueData { ty, def });
        Value(self.values.len() as u32 - 1)
    }

    /// The types an instruction produces: up to two directly, or a signature's results.
    fn result_types(&self, data: &InstData) -> ([Option<Type>; 2], Option<usize>) {
        let ty = |v: Value| self.value_type(v);
        let one = |t: Type| ([Some(t), None], None);
        match data {
            InstData::Vzero | InstData::VectorBinary { .. } => one(Type::V128),
            InstData::Iconst { ty, .. } => one(*ty),
            InstData::SymbolAddr { .. } => one(Type::I64),
            InstData::F32const { .. } => one(Type::F32),
            InstData::F64const { .. } => one(Type::F64),
            InstData::Unary { op, arg } => one(if *op == UnaryOp::Eqz {
                Type::I32
            } else {
                ty(*arg)
            }),
            InstData::Binary { args, .. } => one(ty(args[0])),
            InstData::CheckedBinary { .. } => ([Some(Type::I32), Some(Type::I32)], None),
            InstData::IntCmp { .. } | InstData::FloatCmp { .. } => one(Type::I32),
            InstData::Select { if_true, .. } => one(ty(*if_true)),
            InstData::Convert { to, .. } => one(*to),
            InstData::Load { kind, .. } => one(kind.ty()),
            InstData::Call { func, .. } => ([None, None], Some(self.funcs[func.index()].sig.index())),
            InstData::CallIndirect { sig, .. } => ([None, None], Some(sig.index())),
            _ => ([None, None], None),
        }
    }

    /// Create an instruction (not yet placed in a block) and its result values.
    pub fn make_inst(&mut self, data: InstData) -> Inst {
        let (fixed, sig) = self.result_types(&data);
        let inst = Inst(self.insts.len() as u32);
        self.insts.push(data);
        self.inst_results.push(self.result_pool.len() as u32);
        let mut n = 0;
        for t in fixed.into_iter().flatten() {
            let v = self.make_value(t, ValueDef::Result(inst, n));
            self.result_pool.push(v);
            n += 1;
        }
        if let Some(sig) = sig {
            for i in 0..self.sigs[sig].results.len() {
                let t = self.sigs[sig].results[i];
                let v = self.make_value(t, ValueDef::Result(inst, n));
                self.result_pool.push(v);
                n += 1;
            }
        }
        inst
    }

    /// Follow alias chains to the defining value.
    pub fn resolve(&self, mut v: Value) -> Value {
        while let ValueDef::Alias(to) = self.values[v.index()].def {
            v = to;
        }
        v
    }

    /// Make `from` an alias of `to`; operands are rewritten by [`Function::resolve_aliases`].
    pub fn replace_uses(&mut self, from: Value, to: Value) {
        debug_assert_eq!(self.value_type(from), self.value_type(to));
        debug_assert_ne!(self.resolve(to), from, "alias cycle");
        self.values[from.index()].def = ValueDef::Alias(to);
        self.pending_aliases = true;
    }

    pub fn has_pending_aliases(&self) -> bool {
        self.pending_aliases
    }

    /// Rewrite every operand through its alias chain.
    pub fn resolve_aliases(&mut self) {
        if !self.pending_aliases {
            return;
        }
        self.pending_aliases = false;
        let values = &self.values;
        for data in &mut self.insts {
            data.map_args(|mut v| {
                while let ValueDef::Alias(to) = values[v.index()].def {
                    v = to;
                }
                v
            });
        }
    }

    /// The terminator of `block`, if it has one.
    pub fn terminator(&self, block: Block) -> Option<Inst> {
        let last = *self.blocks[block.index()].insts.last()?;
        self.insts[last.index()].is_terminator().then_some(last)
    }

    pub fn successors(&self, block: Block) -> impl Iterator<Item = Block> + '_ {
        self.terminator(block)
            .into_iter()
            .flat_map(|t| self.insts[t.index()].successors().iter())
            .map(|c| c.block)
    }

    /// Predecessor lists for every block (duplicates kept per edge).
    pub fn predecessors(&self) -> Vec<Vec<Block>> {
        let mut preds = vec![Vec::new(); self.blocks.len()];
        for &b in &self.layout {
            for s in self.successors(b) {
                preds[s.index()].push(b);
            }
        }
        preds
    }
}

impl fmt::Display for Function {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "function {}(", self.name)?;
        for (i, t) in self.sig.params.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{t:?}")?;
        }
        write!(f, ") -> {:?} {{", self.sig.results)?;
        for (i, e) in self.funcs.iter().enumerate() {
            write!(f, "\n    fn{i} = import #{}", e.id)?;
        }
        for &b in &self.layout {
            let data = &self.blocks[b.index()];
            write!(f, "\n{b}(")?;
            for (i, &p) in data.params.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{p}: {:?}", self.value_type(p))?;
            }
            writeln!(f, "):{}", if data.cold { " cold" } else { "" })?;
            for &inst in &data.insts {
                let results = self.results(inst);
                write!(f, "    ")?;
                if !results.is_empty() {
                    let names: Vec<String> = results.iter().map(|v| v.to_string()).collect();
                    write!(f, "{} = ", names.join(", "))?;
                }
                write!(f, "{:?}", self.insts[inst.index()])?;
                let flags = self.mem_flags(inst);
                if flags != MemFlags::DEFAULT {
                    write!(f, " [{flags}]")?;
                }
                writeln!(f)?;
            }
        }
        write!(f, "}}")
    }
}
