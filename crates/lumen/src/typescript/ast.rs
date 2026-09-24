//! The front end's own syntax tree. The engine never sees it: the two agree only on byte
//! offsets, which the location-preserving strip keeps identical (docs/typed-tier.md §3.1).

use super::lexer::Comment;
use super::types::{Predicate, Type, TypeParam};

/// A byte range in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Loc {
    pub start: u32,
    pub end: u32,
}

pub type FnId = usize;
pub type ClassId = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// `.ts`, `.mts`, `.cts`: type annotations are syntax.
    Ts,
    /// `.js`, `.mjs`, `.cjs`: types come from JSDoc comments only.
    Js,
}

#[derive(Debug)]
pub struct Module {
    pub lang: Lang,
    pub stmts: Vec<Stmt>,
    /// Every function in the file (declarations, expressions, arrows, methods, accessors,
    /// constructors), in the order their parsing started. Indexed by `FnId`.
    pub funcs: Vec<Func>,
    /// Every class (declarations and expressions). Indexed by `ClassId`.
    pub classes: Vec<ClassNode>,
    pub comments: Vec<Comment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKind {
    Var,
    Let,
    Const,
    Using,
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Ident {
        name: String,
        loc: Loc,
    },
    /// Object/array destructuring. The checker does not look inside (not in the v1 subset);
    /// the bound names are kept for scope analysis.
    Destructure {
        names: Vec<(String, Loc)>,
        defaults: Vec<Expr>,
        loc: Loc,
    },
}

impl Pattern {
    pub fn loc(&self) -> Loc {
        match self {
            Pattern::Ident { loc, .. } | Pattern::Destructure { loc, .. } => *loc,
        }
    }
    pub fn names(&self) -> Vec<(String, Loc)> {
        match self {
            Pattern::Ident { name, loc } => vec![(name.clone(), *loc)],
            Pattern::Destructure { names, .. } => names.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct VarDecl {
    pub pat: Pattern,
    pub ty: Option<Type>,
    /// `let x!: T` (row 4).
    pub definite: bool,
    pub init: Option<Expr>,
}

#[derive(Debug, Clone)]
pub enum ForHead {
    Var(VarKind, Pattern),
    Expr(Expr),
}

#[derive(Debug, Clone)]
pub struct ImportName {
    pub local: String,
    pub type_only: bool,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Var {
        kind: VarKind,
        decls: Vec<VarDecl>,
        declare: bool,
        loc: Loc,
    },
    Func(FnId),
    Class(ClassId),
    TypeAlias {
        name: String,
        params: Vec<TypeParam>,
        ty: Type,
        loc: Loc,
    },
    Interface {
        name: String,
        params: Vec<TypeParam>,
        extends: Vec<Type>,
        body: super::types::ObjectType,
        loc: Loc,
    },
    Enum {
        name: String,
        /// Member names with whether each has a string initializer.
        members: Vec<(String, Option<Expr>)>,
        is_const: bool,
        declare: bool,
        loc: Loc,
    },
    /// `namespace N {}`, `module N {}`, `declare module "x" {}`, `declare global {}`.
    Namespace {
        name: String,
        declare: bool,
        body: Vec<Stmt>,
        loc: Loc,
    },
    Import {
        names: Vec<ImportName>,
        type_only: bool,
        from: String,
        loc: Loc,
    },
    /// `import x = require("y")` / `import x = A.B`.
    ImportEquals {
        name: String,
        loc: Loc,
    },
    Export {
        stmt: Box<Stmt>,
        default: bool,
        loc: Loc,
    },
    ExportDefaultExpr {
        expr: Expr,
        loc: Loc,
    },
    /// `export { a, b }`, `export * from "m"`, `export type { T }`. `names` are the exported
    /// names of the value (not type-only) specifiers.
    ExportNamed {
        names: Vec<String>,
        type_only: bool,
        loc: Loc,
    },
    /// `export = x`.
    ExportAssign {
        expr: Expr,
        loc: Loc,
    },
    Expr(Expr),
    Block(Vec<Stmt>, Loc),
    If {
        test: Expr,
        cons: Box<Stmt>,
        alt: Option<Box<Stmt>>,
        loc: Loc,
    },
    For {
        init: Option<Box<Stmt>>,
        test: Option<Expr>,
        update: Option<Expr>,
        body: Box<Stmt>,
        loc: Loc,
    },
    ForIn {
        head: ForHead,
        right: Expr,
        body: Box<Stmt>,
        of: bool,
        is_await: bool,
        loc: Loc,
    },
    While {
        test: Expr,
        body: Box<Stmt>,
        loc: Loc,
    },
    DoWhile {
        body: Box<Stmt>,
        test: Expr,
        loc: Loc,
    },
    Return(Option<Expr>, Loc),
    Break(Option<String>, Loc),
    Continue(Option<String>, Loc),
    Throw(Expr, Loc),
    Try {
        block: Vec<Stmt>,
        param: Option<(Pattern, Option<Type>)>,
        handler: Option<Vec<Stmt>>,
        finalizer: Option<Vec<Stmt>>,
        loc: Loc,
    },
    Switch {
        disc: Expr,
        cases: Vec<(Option<Expr>, Vec<Stmt>)>,
        loc: Loc,
    },
    Labeled {
        label: String,
        body: Box<Stmt>,
        loc: Loc,
    },
    With {
        obj: Expr,
        body: Box<Stmt>,
        loc: Loc,
    },
    Empty(Loc),
    Debugger(Loc),
}

impl Stmt {
    pub fn loc(&self) -> Loc {
        match self {
            Stmt::Var { loc, .. }
            | Stmt::TypeAlias { loc, .. }
            | Stmt::Interface { loc, .. }
            | Stmt::Enum { loc, .. }
            | Stmt::Namespace { loc, .. }
            | Stmt::Import { loc, .. }
            | Stmt::ImportEquals { loc, .. }
            | Stmt::Export { loc, .. }
            | Stmt::ExportDefaultExpr { loc, .. }
            | Stmt::ExportNamed { loc, .. }
            | Stmt::ExportAssign { loc, .. }
            | Stmt::Block(_, loc)
            | Stmt::If { loc, .. }
            | Stmt::For { loc, .. }
            | Stmt::ForIn { loc, .. }
            | Stmt::While { loc, .. }
            | Stmt::DoWhile { loc, .. }
            | Stmt::Return(_, loc)
            | Stmt::Break(_, loc)
            | Stmt::Continue(_, loc)
            | Stmt::Throw(_, loc)
            | Stmt::Try { loc, .. }
            | Stmt::Switch { loc, .. }
            | Stmt::Labeled { loc, .. }
            | Stmt::With { loc, .. }
            | Stmt::Empty(loc)
            | Stmt::Debugger(loc) => *loc,
            Stmt::Expr(e) => e.loc,
            Stmt::Func(_) | Stmt::Class(_) => Loc::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FnKind {
    Decl,
    Expr,
    Arrow,
    Method,
    Getter,
    Setter,
    Constructor,
}

#[derive(Debug, Clone)]
pub struct ParamNode {
    pub pat: Pattern,
    pub ty: Option<Type>,
    pub optional: bool,
    pub rest: bool,
    pub default: Option<Expr>,
    /// A TS parameter property (`constructor(public x: number)`), which needs emit.
    pub property: bool,
    pub loc: Loc,
}

#[derive(Debug, Clone)]
pub enum Body {
    Block(Vec<Stmt>),
    Expr(Box<Expr>),
}

#[derive(Debug, Clone)]
pub struct Func {
    pub id: FnId,
    pub kind: FnKind,
    pub name: Option<String>,
    /// The engine's `FnSource::Range.start` for this function: the first token of the
    /// `toString` source text (`async`/`function`/`get`/`*`/method name/arrow parameters),
    /// after any TS-only modifiers. For a constructor the engine uses the class start instead
    /// (see [`super::table::FnTypes`]).
    pub start: u32,
    pub end: u32,
    /// The token that begins the declaration the function belongs to, for JSDoc attachment
    /// (the statement, class member or object property).
    pub doc_anchor: u32,
    pub is_async: bool,
    pub is_generator: bool,
    pub type_params: Vec<TypeParam>,
    /// A TS `this` parameter.
    pub this_type: Option<Type>,
    pub params: Vec<ParamNode>,
    pub ret: Option<Type>,
    pub predicate: Option<Predicate>,
    /// `None` for overload signatures, `declare` functions and abstract methods.
    pub body: Option<Body>,
    /// The class a method/accessor/constructor belongs to.
    pub class: Option<ClassId>,
    pub is_static: bool,
    /// The lexically enclosing function, if any.
    pub parent: Option<FnId>,
    pub declare: bool,
    /// The whole function including TS-only modifiers and decorators.
    pub loc: Loc,
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum Member {
    Field {
        name: String,
        computed: bool,
        loc: Loc,
        ty: Option<Type>,
        optional: bool,
        /// `x!: T` (row 4).
        definite: bool,
        readonly: bool,
        is_static: bool,
        declare: bool,
        /// `accessor x` (an auto-accessor, not a data field).
        accessor: bool,
        init: Option<Expr>,
        doc_anchor: u32,
    },
    Method {
        name: String,
        computed: bool,
        func: FnId,
        is_static: bool,
        is_abstract: bool,
    },
    Index(Loc),
    StaticBlock(Vec<Stmt>, Loc),
}

#[derive(Debug, Clone)]
pub struct ClassNode {
    pub id: ClassId,
    pub name: Option<String>,
    /// The `class` keyword (the engine keys the class — and its constructor — here).
    pub start: u32,
    pub end: u32,
    pub doc_anchor: u32,
    pub type_params: Vec<TypeParam>,
    pub extends: Option<Expr>,
    pub extends_args: Vec<Type>,
    pub implements: Vec<Type>,
    pub members: Vec<Member>,
    pub is_abstract: bool,
    pub declare: bool,
    pub is_expr: bool,
    pub parent_fn: Option<FnId>,
}

#[derive(Debug, Clone)]
pub enum PropKey {
    Ident(String),
    Str(String),
    Num(f64),
    Computed(Box<Expr>),
}

impl PropKey {
    pub fn static_name(&self) -> Option<String> {
        match self {
            PropKey::Ident(s) | PropKey::Str(s) => Some(s.clone()),
            PropKey::Num(n) => Some(super::fmt_num(*n)),
            PropKey::Computed(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Prop {
    KeyValue { key: PropKey, value: Expr },
    Shorthand { name: String, loc: Loc },
    Method { key: PropKey, func: FnId },
    Spread(Expr),
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub loc: Loc,
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Num(f64),
    Str(String),
    BigInt(String),
    Bool(bool),
    Null,
    Regex,
    Template(Vec<Expr>),
    TaggedTemplate(Box<Expr>, Vec<Expr>),
    Ident(String),
    This,
    Super,
    Array(Vec<Option<Expr>>),
    Object(Vec<Prop>),
    Func(FnId),
    Class(ClassId),
    Member {
        obj: Box<Expr>,
        name: String,
        name_loc: Loc,
        optional: bool,
    },
    Index {
        obj: Box<Expr>,
        index: Box<Expr>,
        optional: bool,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        optional: bool,
        type_args: Vec<Type>,
    },
    New {
        callee: Box<Expr>,
        args: Vec<Expr>,
        type_args: Vec<Type>,
    },
    Unary {
        op: &'static str,
        arg: Box<Expr>,
    },
    Update {
        op: &'static str,
        prefix: bool,
        arg: Box<Expr>,
    },
    Binary {
        op: &'static str,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Assign {
        op: &'static str,
        target: Box<Expr>,
        value: Box<Expr>,
    },
    Cond {
        test: Box<Expr>,
        cons: Box<Expr>,
        alt: Box<Expr>,
    },
    Seq(Vec<Expr>),
    Spread(Box<Expr>),
    /// `e as T` (`as const` has `ty: None`).
    As {
        expr: Box<Expr>,
        ty: Option<Type>,
    },
    Satisfies {
        expr: Box<Expr>,
        ty: Type,
    },
    NonNull(Box<Expr>),
    /// `<T>e`.
    TypeAssert {
        ty: Type,
        expr: Box<Expr>,
    },
    /// `/** @type {T} */ (e)` in a JS file.
    JsDocCast {
        ty: Type,
        expr: Box<Expr>,
    },
    Paren(Box<Expr>),
    Yield(Option<Box<Expr>>),
    Await(Box<Expr>),
    /// `new.target`, `import.meta`.
    MetaProp,
    /// `import(x)`.
    ImportCall(Box<Expr>),
    /// A destructuring assignment target or other construct the checker does not model.
    Unsupported(&'static str),
}
