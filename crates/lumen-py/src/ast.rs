//! Python abstract syntax tree.
//!
//! Mirrors CPython's `ast` module (Python 3.12 grammar) node for node, so the CPython
//! documentation for `ast` doubles as the spec for this file. Names are Rust-cased versions of
//! the CPython class names (`ast.AugAssign` is [`Stmt::AugAssign`], `ast.BoolOp` is
//! [`ExprKind::BoolOp`], ...). The parser produces it; the compiler consumes it.

use std::rc::Rc;

/// Source position: 1-based line, 0-based column (in chars), like CPython.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pos {
    pub line: u32,
    pub col: u32,
}

pub type Ident = Rc<str>;

#[derive(Clone, Debug, PartialEq)]
pub struct Module {
    pub body: Vec<Stmt>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stmt {
    pub pos: Pos,
    pub kind: StmtKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StmtKind {
    FunctionDef(Box<FunctionDef>),
    ClassDef(Box<ClassDef>),
    Return(Option<Expr>),
    Delete(Vec<Expr>),
    /// `a = b = value` has two targets.
    Assign {
        targets: Vec<Expr>,
        value: Expr,
    },
    AugAssign {
        target: Expr,
        op: BinOp,
        value: Expr,
    },
    /// `simple` is true for a bare name target not wrapped in parens.
    AnnAssign {
        target: Expr,
        annotation: Expr,
        value: Option<Expr>,
        simple: bool,
    },
    For {
        target: Expr,
        iter: Expr,
        body: Vec<Stmt>,
        orelse: Vec<Stmt>,
        is_async: bool,
    },
    While {
        test: Expr,
        body: Vec<Stmt>,
        orelse: Vec<Stmt>,
    },
    If {
        test: Expr,
        body: Vec<Stmt>,
        orelse: Vec<Stmt>,
    },
    With {
        items: Vec<WithItem>,
        body: Vec<Stmt>,
        is_async: bool,
    },
    Match {
        subject: Expr,
        cases: Vec<MatchCase>,
    },
    Raise {
        exc: Option<Expr>,
        cause: Option<Expr>,
    },
    Try {
        body: Vec<Stmt>,
        handlers: Vec<ExceptHandler>,
        orelse: Vec<Stmt>,
        finalbody: Vec<Stmt>,
        /// `except*`
        is_star: bool,
    },
    Assert {
        test: Expr,
        msg: Option<Expr>,
    },
    Import(Vec<Alias>),
    /// `from ..pkg.mod import a as b`; `level` is the number of leading dots.
    ImportFrom {
        module: Option<Ident>,
        names: Vec<Alias>,
        level: u32,
    },
    Global(Vec<Ident>),
    Nonlocal(Vec<Ident>),
    Expr(Expr),
    Pass,
    Break,
    Continue,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FunctionDef {
    pub name: Ident,
    pub args: Arguments,
    pub body: Vec<Stmt>,
    pub decorators: Vec<Expr>,
    pub returns: Option<Expr>,
    pub is_async: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClassDef {
    pub name: Ident,
    pub bases: Vec<Expr>,
    pub keywords: Vec<Keyword>,
    pub body: Vec<Stmt>,
    pub decorators: Vec<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub pos: Pos,
    pub kind: ExprKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    /// `a and b and c` is one node with three values.
    BoolOp {
        op: BoolOp,
        values: Vec<Expr>,
    },
    /// `target := value`
    NamedExpr {
        target: Box<Expr>,
        value: Box<Expr>,
    },
    BinOp {
        left: Box<Expr>,
        op: BinOp,
        right: Box<Expr>,
    },
    UnaryOp {
        op: UnaryOp,
        operand: Box<Expr>,
    },
    Lambda {
        args: Box<Arguments>,
        body: Box<Expr>,
    },
    IfExp {
        test: Box<Expr>,
        body: Box<Expr>,
        orelse: Box<Expr>,
    },
    /// `None` key means `**mapping` unpacking.
    Dict {
        keys: Vec<Option<Expr>>,
        values: Vec<Expr>,
    },
    Set(Vec<Expr>),
    ListComp {
        elt: Box<Expr>,
        generators: Vec<Comprehension>,
    },
    SetComp {
        elt: Box<Expr>,
        generators: Vec<Comprehension>,
    },
    DictComp {
        key: Box<Expr>,
        value: Box<Expr>,
        generators: Vec<Comprehension>,
    },
    GeneratorExp {
        elt: Box<Expr>,
        generators: Vec<Comprehension>,
    },
    Await(Box<Expr>),
    Yield(Option<Box<Expr>>),
    YieldFrom(Box<Expr>),
    /// `a < b <= c`: left = a, ops = [Lt, LtE], comparators = [b, c].
    Compare {
        left: Box<Expr>,
        ops: Vec<CmpOp>,
        comparators: Vec<Expr>,
    },
    Call {
        func: Box<Expr>,
        /// May contain [`ExprKind::Starred`] for `*args`.
        args: Vec<Expr>,
        /// `arg: None` means `**kwargs`.
        keywords: Vec<Keyword>,
    },
    /// An f-string: the concatenation of its parts. Plain string pieces are
    /// [`ExprKind::Constant`] with [`Constant::Str`].
    JoinedStr(Vec<Expr>),
    /// One `{value!conversion:format_spec}` replacement field inside an f-string.
    /// `conversion` is `None`, `Some('s')`, `Some('r')` or `Some('a')`; `format_spec` is
    /// itself a [`ExprKind::JoinedStr`].
    FormattedValue {
        value: Box<Expr>,
        conversion: Option<char>,
        format_spec: Option<Box<Expr>>,
    },
    Constant(Constant),
    Attribute {
        value: Box<Expr>,
        attr: Ident,
        ctx: Ctx,
    },
    Subscript {
        value: Box<Expr>,
        slice: Box<Expr>,
        ctx: Ctx,
    },
    Starred {
        value: Box<Expr>,
        ctx: Ctx,
    },
    Name {
        id: Ident,
        ctx: Ctx,
    },
    List {
        elts: Vec<Expr>,
        ctx: Ctx,
    },
    Tuple {
        elts: Vec<Expr>,
        ctx: Ctx,
    },
    /// Only valid as a subscript (possibly inside a Tuple subscript: `a[1:2, ::3]`).
    Slice {
        lower: Option<Box<Expr>>,
        upper: Option<Box<Expr>>,
        step: Option<Box<Expr>>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ctx {
    Load,
    Store,
    Del,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Constant {
    None,
    True,
    False,
    Ellipsis,
    /// Decimal digits of an integer literal of any size, with `_` removed and the radix
    /// prefix already applied, i.e. always base 10 (`0xff` arrives as `"255"`).
    Int(Rc<str>),
    Float(f64),
    /// Imaginary literal `3j` — the imaginary part.
    Complex(f64),
    Str(Rc<str>),
    Bytes(Rc<[u8]>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoolOp {
    And,
    Or,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mult,
    MatMult,
    Div,
    Mod,
    Pow,
    LShift,
    RShift,
    BitOr,
    BitXor,
    BitAnd,
    FloorDiv,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Invert,
    Not,
    UAdd,
    USub,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    NotEq,
    Lt,
    LtE,
    Gt,
    GtE,
    Is,
    IsNot,
    In,
    NotIn,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Comprehension {
    pub target: Expr,
    pub iter: Expr,
    pub ifs: Vec<Expr>,
    pub is_async: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExceptHandler {
    pub pos: Pos,
    pub typ: Option<Expr>,
    pub name: Option<Ident>,
    pub body: Vec<Stmt>,
}

/// Function parameters, CPython layout:
/// `def f(posonly, /, args, *vararg, kwonly, **kwarg)`.
/// `defaults` align with the *tail* of `posonlyargs + args`; `kw_defaults` align one-to-one
/// with `kwonlyargs` (`None` = no default).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Arguments {
    pub posonlyargs: Vec<Arg>,
    pub args: Vec<Arg>,
    pub vararg: Option<Arg>,
    pub kwonlyargs: Vec<Arg>,
    pub kw_defaults: Vec<Option<Expr>>,
    pub kwarg: Option<Arg>,
    pub defaults: Vec<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Arg {
    pub pos: Pos,
    pub arg: Ident,
    pub annotation: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Keyword {
    pub pos: Pos,
    /// `None` for `**kwargs`.
    pub arg: Option<Ident>,
    pub value: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Alias {
    /// Dotted name, e.g. `os.path`, or `*` for `from m import *`.
    pub name: Ident,
    pub asname: Option<Ident>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WithItem {
    pub context_expr: Expr,
    pub optional_vars: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MatchCase {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Vec<Stmt>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Pattern {
    /// A literal or dotted-name value pattern.
    MatchValue(Expr),
    /// `None`, `True`, `False`.
    MatchSingleton(Constant),
    MatchSequence(Vec<Pattern>),
    MatchMapping {
        keys: Vec<Expr>,
        patterns: Vec<Pattern>,
        rest: Option<Ident>,
    },
    MatchClass {
        cls: Expr,
        patterns: Vec<Pattern>,
        kwd_attrs: Vec<Ident>,
        kwd_patterns: Vec<Pattern>,
    },
    /// `*rest` inside a sequence pattern; `None` for `*_`.
    MatchStar(Option<Ident>),
    /// `pattern as name`; `pattern: None` is a capture (`name`) or wildcard (`_`, name `None`).
    MatchAs {
        pattern: Option<Box<Pattern>>,
        name: Option<Ident>,
    },
    MatchOr(Vec<Pattern>),
}
