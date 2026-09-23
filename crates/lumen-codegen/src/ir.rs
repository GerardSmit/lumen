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
        }
    }
    pub fn is_signed(self) -> bool {
        use MemKind::*;
        matches!(self, I32S8 | I32S16 | I64S8 | I64S16 | I64S32)
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
    Iconst {
        ty: Type,
        imm: i64,
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
            InstData::Iconst { .. }
                | InstData::F32const { .. }
                | InstData::F64const { .. }
                | InstData::Unary { .. }
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
            InstData::Iconst { .. }
            | InstData::F32const { .. }
            | InstData::F64const { .. }
            | InstData::Trap { .. } => {}
            InstData::Unary { arg, .. } | InstData::Convert { arg, .. } => f(*arg),
            InstData::Binary { args, .. }
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
            InstData::Load { addr, .. } => f(*addr),
            InstData::Store { addr, value, .. } => {
                f(*addr);
                f(*value);
            }
            InstData::Call { args, .. } | InstData::Return { args } => args.iter().for_each(|&a| f(a)),
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
            InstData::Iconst { .. }
            | InstData::F32const { .. }
            | InstData::F64const { .. }
            | InstData::Trap { .. } => {}
            InstData::Unary { arg, .. } | InstData::Convert { arg, .. } => *arg = f(*arg),
            InstData::Binary { args, .. }
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
            InstData::Load { addr, .. } => *addr = f(*addr),
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
    pub fn successors(&self) -> Vec<&BlockCall> {
        match self {
            InstData::Jump { dest } => vec![dest],
            InstData::Brif { then, else_, .. } => vec![then, else_],
            InstData::BrTable {
                targets, default, ..
            } => targets.iter().chain(std::iter::once(default)).collect(),
            _ => Vec::new(),
        }
    }

    pub fn successors_mut(&mut self) -> Vec<&mut BlockCall> {
        match self {
            InstData::Jump { dest } => vec![dest],
            InstData::Brif { then, else_, .. } => vec![then, else_],
            InstData::BrTable {
                targets, default, ..
            } => targets.iter_mut().chain(std::iter::once(default)).collect(),
            _ => Vec::new(),
        }
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
}

#[derive(Clone, Debug, Default)]
pub struct Function {
    pub name: String,
    pub sig: Signature,
    pub insts: Vec<InstData>,
    pub inst_results: Vec<Vec<Value>>,
    pub values: Vec<ValueData>,
    pub blocks: Vec<BlockData>,
    /// Blocks in layout order; the first is the entry, whose parameters are the function's.
    pub layout: Vec<Block>,
    pub sigs: Vec<Signature>,
    pub funcs: Vec<ExtFunc>,
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
        &self.inst_results[inst.index()]
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

    /// The types an instruction produces.
    pub fn result_types(&self, data: &InstData) -> Vec<Type> {
        let ty = |v: Value| self.value_type(v);
        match data {
            InstData::Iconst { ty, .. } => vec![*ty],
            InstData::F32const { .. } => vec![Type::F32],
            InstData::F64const { .. } => vec![Type::F64],
            InstData::Unary { op, arg } => vec![if *op == UnaryOp::Eqz { Type::I32 } else { ty(*arg) }],
            InstData::Binary { args, .. } => vec![ty(args[0])],
            InstData::IntCmp { .. } | InstData::FloatCmp { .. } => vec![Type::I32],
            InstData::Select { if_true, .. } => vec![ty(*if_true)],
            InstData::Convert { to, .. } => vec![*to],
            InstData::Load { kind, .. } => vec![kind.ty()],
            InstData::Call { func, .. } => {
                self.sigs[self.funcs[func.index()].sig.index()].results.clone()
            }
            InstData::CallIndirect { sig, .. } => self.sigs[sig.index()].results.clone(),
            _ => Vec::new(),
        }
    }

    /// Create an instruction (not yet placed in a block) and its result values.
    pub fn make_inst(&mut self, data: InstData) -> Inst {
        let tys = self.result_types(&data);
        let inst = Inst(self.insts.len() as u32);
        self.insts.push(data);
        let results = tys
            .into_iter()
            .enumerate()
            .map(|(i, t)| self.make_value(t, ValueDef::Result(inst, i as u32)))
            .collect();
        self.inst_results.push(results);
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
    }

    /// Rewrite every operand through its alias chain.
    pub fn resolve_aliases(&mut self) {
        let resolved: Vec<Value> = (0..self.values.len() as u32)
            .map(|v| self.resolve(Value(v)))
            .collect();
        for data in &mut self.insts {
            data.map_args(|v| resolved[v.index()]);
        }
    }

    /// The terminator of `block`, if it has one.
    pub fn terminator(&self, block: Block) -> Option<Inst> {
        let last = *self.blocks[block.index()].insts.last()?;
        self.insts[last.index()].is_terminator().then_some(last)
    }

    pub fn successors(&self, block: Block) -> Vec<Block> {
        match self.terminator(block) {
            Some(t) => self.insts[t.index()]
                .successors()
                .iter()
                .map(|c| c.block)
                .collect(),
            None => Vec::new(),
        }
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
        for &b in &self.layout {
            let data = &self.blocks[b.index()];
            write!(f, "\n{b}(")?;
            for (i, &p) in data.params.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{p}: {:?}", self.value_type(p))?;
            }
            writeln!(f, "):")?;
            for &inst in &data.insts {
                let results = &self.inst_results[inst.index()];
                write!(f, "    ")?;
                if !results.is_empty() {
                    let names: Vec<String> = results.iter().map(|v| v.to_string()).collect();
                    write!(f, "{} = ", names.join(", "))?;
                }
                writeln!(f, "{:?}", self.insts[inst.index()])?;
            }
        }
        write!(f, "}}")
    }
}
