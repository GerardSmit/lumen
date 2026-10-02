//! Bytecode: the instruction set and code objects.

use crate::ast::{BinOp, CmpOp};
use crate::object::{Obj, Value};
use std::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UnOp {
    Neg,
    Pos,
    Invert,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    Nop,
    Pop,
    Dup,
    DupTwo,
    /// Swap TOS with the n-th item from the top (`Swap(2)` swaps the top two).
    Swap(u32),
    /// `a b c -> c a b`
    Rot3,
    /// `a b c d -> d a b c`
    Rot4,

    LoadConst(u32),
    LoadFast(u32),
    StoreFast(u32),
    DelFast(u32),
    LoadName(u32),
    StoreName(u32),
    DelName(u32),
    LoadGlobal(u32),
    StoreGlobal(u32),
    DelGlobal(u32),
    LoadDeref(u32),
    StoreDeref(u32),
    DelDeref(u32),
    LoadClosure(u32),
    LoadClassDeref(u32),
    LoadAttr(u32),
    StoreAttr(u32),
    DelAttr(u32),
    LoadMethod(u32),
    Subscr,
    StoreSubscr,
    DelSubscr,

    Binary(BinOp),
    Inplace(BinOp),
    Unary(UnOp),
    Compare(CmpOp),

    Jump(u32),
    JumpIfFalse(u32),
    JumpIfTrue(u32),
    JumpIfFalseKeep(u32),
    JumpIfTrueKeep(u32),
    ForIter(u32),
    GetIter,

    BuildTuple(u32),
    BuildList(u32),
    BuildSet(u32),
    BuildSetConst(u32),
    BuildMap(u32),
    BuildSlice(u32),
    BuildString(u32),
    ListAppend(u32),
    SetAdd(u32),
    MapAdd(u32),
    ListExtend(u32),
    SetUpdate(u32),
    DictUpdate(u32),
    KwMerge,
    ListToTuple,
    UnpackSequence(u32),
    UnpackEx(u32, u32),
    FormatValue(u8, bool),

    MakeFunction(u32),
    Call(u32),
    CallKw(u32),
    CallEx(u32),
    /// Pops `(self, args...)` style method call prepared by `LoadMethod`: `CallMethod(argc)`.
    CallMethod(u32),
    ReturnValue,

    Raise(u32),
    SetupBlock(u32),
    PopBlock,
    PushExcInfo,
    PopExcInfo(u32),
    UnwindExc(u32),
    CheckExcMatch,
    ExcStarWrap,
    ExcStarSplit,
    ExcStarEnd,
    Reraise,
    CleanupReraise,
    EndFinally,

    SetupWith(u32),
    BeforeAsyncWith,
    WithCallExit,
    WithExceptStart,
    WithExceptEnd(u32),

    ImportName(u32),
    ImportFrom(u32),
    ImportStar,
    LoadBuildClass,

    YieldValue,
    YieldFrom,
    GetAwaitable,
    GetYieldFromIter,
    GetAIter,
    GetANext,
    EndAsyncFor(u32),
    AsyncGenWrap,

    LoadAssertionError,
    SetupAnnotations,

    /// Pushes the frame's local namespace (a class body's namespace).
    LoadLocals,
    /// Pops a mapping and pushes the value of cell `i`'s name in it, else the cell's value.
    LoadFromDictOrDeref(u32),
    /// Pops a mapping and pushes the value of `names[i]` in it, else the global or builtin.
    LoadFromDictOrGlobals(u32),
    /// `TOS = intrinsic(TOS)`, one of the `INTRINSIC1_*` functions.
    CallIntrinsic1(u32),
    /// `TOS = intrinsic(TOS1, TOS)`, one of the `INTRINSIC2_*` functions.
    CallIntrinsic2(u32),

    /// Pattern matching helpers.
    MatchClass(u32, u32),
    MatchMapping,
    MatchSequence,
    MatchKeys(u32),
    MatchLen(u32, bool),
    MatchStarSlice(u32, u32),
    MatchSeqItem(i32),
    MatchRest(u32),
}

pub const CO_VARARGS: u32 = 1;
pub const CO_VARKEYWORDS: u32 = 2;
pub const CO_GENERATOR: u32 = 4;
pub const CO_COROUTINE: u32 = 8;
pub const CO_ASYNC_GENERATOR: u32 = 16;
pub const CO_CLASS_BODY: u32 = 32;

pub const INTRINSIC1_TYPEVAR: u32 = 0;
pub const INTRINSIC1_PARAMSPEC: u32 = 1;
pub const INTRINSIC1_TYPEVARTUPLE: u32 = 2;
pub const INTRINSIC1_SUBSCRIPT_GENERIC: u32 = 3;
pub const INTRINSIC1_TYPEALIAS: u32 = 4;
/// Passes an interactive expression statement's value to `sys.displayhook`.
pub const INTRINSIC1_PRINT: u32 = 5;

pub const INTRINSIC2_TYPEVAR_WITH_BOUND: u32 = 0;
pub const INTRINSIC2_TYPEVAR_WITH_CONSTRAINTS: u32 = 1;
pub const INTRINSIC2_SET_FUNCTION_TYPE_PARAMS: u32 = 2;

pub const MF_DEFAULTS: u32 = 1;
pub const MF_KWDEFAULTS: u32 = 2;
pub const MF_ANNOTATIONS: u32 = 4;
pub const MF_CLOSURE: u32 = 8;

pub struct Code {
    pub name: Rc<str>,
    pub qualname: Rc<str>,
    pub filename: Rc<str>,
    pub first_line: u32,
    pub ops: Vec<Op>,
    pub lines: Vec<u32>,
    pub consts: Vec<Value>,
    pub names: Vec<Obj>,
    pub varnames: Vec<Rc<str>>,
    pub cellvars: Vec<Rc<str>>,
    pub freevars: Vec<Rc<str>>,
    pub argcount: u32,
    pub posonly: u32,
    pub kwonly: u32,
    pub flags: u32,
    /// (cell index, local index) pairs for arguments captured by closures.
    pub cell_args: Vec<(u32, u32)>,
    pub doc: Option<Value>,
}

impl Code {
    pub fn has(&self, flag: u32) -> bool {
        self.flags & flag != 0
    }

    pub fn line_at(&self, pc: usize) -> u32 {
        self.lines.get(pc).copied().unwrap_or(self.first_line)
    }
}
