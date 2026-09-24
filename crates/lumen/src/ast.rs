//! The abstract syntax tree. Deliberately small: one `Stmt` enum and one `Expr` enum, with shared
//! sub-structures for functions and patterns. The interpreter walks this tree directly.

use std::rc::Rc;

pub type P<T> = Box<T>;

#[derive(Debug, Clone)]
pub enum Stmt {
    Expr(Expr),
    /// `var` / `let` / `const` declaration: kind + (target, optional initializer) pairs.
    VarDecl {
        kind: DeclKind,
        decls: Vec<(Pattern, Option<Expr>)>,
    },
    FuncDecl(Rc<Function>),
    Return(Option<Expr>),
    If {
        test: Expr,
        cons: P<Stmt>,
        alt: Option<P<Stmt>>,
    },
    Block(Vec<Stmt>),
    While {
        test: Expr,
        body: P<Stmt>,
    },
    DoWhile {
        body: P<Stmt>,
        test: Expr,
    },
    /// C-style `for (init; test; update) body`.
    For {
        init: Option<P<ForInit>>,
        test: Option<Expr>,
        update: Option<Expr>,
        body: P<Stmt>,
    },
    /// `for (left in right) body` / `for (left of right) body` (`is_await` for `for await … of`).
    ForInOf {
        decl: Option<DeclKind>,
        left: Pattern,
        right: Expr,
        of: bool,
        is_await: bool,
        body: P<Stmt>,
    },
    Break(Option<String>),
    Continue(Option<String>),
    Throw(Expr),
    Try {
        block: Vec<Stmt>,
        handler: Option<(Option<Pattern>, Vec<Stmt>)>,
        finalizer: Option<Vec<Stmt>>,
    },
    Switch {
        disc: Expr,
        cases: Vec<SwitchCase>,
    },
    Labeled {
        label: String,
        body: P<Stmt>,
    },
    /// `with (obj) body` — resolves identifiers against `obj` first (forbidden in strict mode).
    With {
        obj: Expr,
        body: P<Stmt>,
    },
    ClassDecl(Rc<Class>),
    Empty,
    Debugger,
    /// `import …from "spec"` (or a bare `import "spec"`).
    Import(ImportDecl),
    /// `export { a, b as c }` or `export { a } from "spec"`.
    ExportNamed {
        specs: Vec<ExportSpec>,
        source: Option<Rc<str>>,
    },
    /// `export const/let/var/function/class …` — the inner declaration plus its exported names.
    ExportDecl(P<Stmt>),
    /// `export default …` (expression, function, or class).
    ExportDefault(P<Stmt>),
    /// `export * from "spec"` or `export * as ns from "spec"`.
    ExportAll {
        source: Rc<str>,
        exported: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct ImportDecl {
    pub source: Rc<str>,
    pub specs: Vec<ImportSpec>,
    /// The `with { type: "..." }` import attribute (json/text/bytes), if present.
    pub attr_type: Option<String>,
}
#[derive(Debug, Clone)]
pub enum ImportSpec {
    /// `import x from "…"`
    Default(String),
    /// `import * as ns from "…"`
    Namespace(String),
    /// `import defer * as ns from "…"` — evaluation deferred until the namespace is accessed.
    DeferNamespace(String),
    /// `import source x from "…"` — a source-phase import binding the module's ModuleSource.
    Source(String),
    /// `import { imported as local } from "…"`
    Named { imported: String, local: String },
}
#[derive(Debug, Clone)]
pub struct ExportSpec {
    pub local: String,
    pub exported: String,
}

#[derive(Debug, Clone)]
pub struct Class {
    pub name: Option<String>,
    pub superclass: Option<P<Expr>>,
    pub members: Vec<ClassMember>,
    /// `@dec` decorators applied to the whole class (outermost last).
    pub decorators: Vec<Expr>,
    /// The class's source text (what the constructor's `toString` returns).
    pub source: FnSource,
}

/// Source text for `Function.prototype.toString`: either owned (a snapshot-decoded or synthesized
/// function) or a byte range into the shared source of the file it was parsed from, sliced on
/// demand so nested functions do not each carry a copy of their enclosing text.
#[derive(Clone, Default)]
pub enum FnSource {
    #[default]
    None,
    Text(Rc<str>),
    Range {
        src: Rc<str>,
        start: u32,
        end: u32,
    },
    /// A range into an ahead-of-time blob's kept function text, which stays compressed in the
    /// binary until the first `toString` that needs it (see `crate::precompiled::KeptRef`).
    Kept {
        text: Rc<crate::precompiled::KeptRef>,
        start: u32,
        end: u32,
    },
}

impl std::fmt::Debug for FnSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FnSource::None => f.write_str("None"),
            FnSource::Text(t) => f.debug_tuple("Text").field(t).finish(),
            FnSource::Range { start, end, .. } | FnSource::Kept { start, end, .. } => f
                .debug_struct("Range")
                .field("start", start)
                .field("end", end)
                .finish_non_exhaustive(),
        }
    }
}

impl FnSource {
    pub fn text(&self) -> Option<Rc<str>> {
        match self {
            FnSource::None => None,
            FnSource::Text(s) => Some(s.clone()),
            FnSource::Range { .. } | FnSource::Kept { .. } => self.as_str().map(Rc::from),
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            FnSource::None => None,
            FnSource::Text(s) => Some(s),
            FnSource::Range { src, start, end } => src.get(*start as usize..*end as usize),
            FnSource::Kept { text, start, end } => text.slice(*start, *end),
        }
    }
    pub fn is_some(&self) -> bool {
        !matches!(self, FnSource::None)
    }
}

/// A function body the parser skipped: the byte range of `{ ... }` in the shared file source,
/// the line the `{` is on, and the parser context the body inherits. Parsed on first call (see
/// [`Function::ensure_body`]) and kept afterwards, so a body the collector released can be
/// parsed again (see [`Function::release_cold_body`]).
#[derive(Clone)]
pub struct LazyBody {
    pub src: Rc<str>,
    pub start: u32,
    pub end: u32,
    pub line: u32,
    pub html_comments: bool,
    pub ctx: LazyCtx,
    /// The innermost class the body is written in (its private names, and its enclosing
    /// classes' through `parent`), for the body's private-name check.
    pub private_scope: Option<Rc<PrivateScope>>,
    /// An ahead-of-time function's body: encoded statements in the blob, decoded (instead of
    /// parsed from `src`, which is then empty) on first use — see `crate::precompiled`.
    pub aot: Option<Rc<crate::precompiled::AotBody>>,
}

impl std::fmt::Debug for LazyBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyBody")
            .field("start", &self.start)
            .field("end", &self.end)
            .field("line", &self.line)
            .finish_non_exhaustive()
    }
}

/// The private names one class declares, filled in once its members have all been parsed (a
/// method body may reference a name declared after it), chained to the enclosing class's.
#[derive(Debug, Default)]
pub struct PrivateScope {
    pub names: std::cell::OnceCell<Vec<String>>,
    pub parent: Option<Rc<PrivateScope>>,
}

impl PrivateScope {
    /// The declared names of this class and every enclosing one, innermost last.
    pub fn visible_names(self: &Rc<Self>) -> Vec<Vec<String>> {
        let mut chain = Vec::new();
        let mut cur = Some(self.clone());
        while let Some(s) = cur {
            chain.push(s.names.get().cloned().unwrap_or_default());
            cur = s.parent.clone();
        }
        chain.reverse();
        chain
    }
}

/// The parser flags a function body inherits from where it was written.
#[derive(Debug, Clone, Copy)]
pub struct LazyCtx {
    /// Strictness after the body's own directive prologue.
    pub strict: bool,
    pub in_generator: bool,
    pub in_async: bool,
    pub module: bool,
    pub allow_new_target: bool,
    pub super_prop_ok: bool,
    pub super_call_ok: bool,
    pub in_derived_class: bool,
    pub no_arguments_refs: bool,
    pub in_field_init: bool,
}

#[derive(Debug, Clone)]
pub struct ClassMember {
    pub key: PropKey,
    pub kind: MemberKind,
    pub is_static: bool,
    /// For methods/accessors/constructor.
    pub func: Option<Rc<Function>>,
    /// For fields (`x = init` / `x`).
    pub value: Option<Expr>,
    /// `@dec` decorators applied to this element.
    pub decorators: Vec<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    Constructor,
    Method,
    Get,
    Set,
    Field,
    /// `accessor x = init` — an auto-accessor: a private backing field plus a getter/setter pair.
    Accessor,
    /// `static { ... }` — runs once at class definition with `this` = the class.
    StaticBlock,
}

#[derive(Debug, Clone)]
pub enum ForInit {
    VarDecl {
        kind: DeclKind,
        decls: Vec<(Pattern, Option<Expr>)>,
    },
    Expr(Expr),
}

#[derive(Debug, Clone)]
pub struct SwitchCase {
    /// `None` is the `default:` clause.
    pub test: Option<Expr>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclKind {
    Var,
    Let,
    Const,
    /// `using x = expr;` — a block-scoped binding disposed (`[Symbol.dispose]()`) at scope exit.
    Using,
    /// `await using x = expr;` — disposed via `[Symbol.asyncDispose]()` (awaited) at scope exit.
    AwaitUsing,
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Ident(String),
    /// `[a, b = 1, ...rest]` — elements may be holes, may carry defaults, and the last may be a rest.
    Array(Vec<ArrayPatElem>),
    /// `{ a, b: x = 1, ...rest }`.
    Object(ObjectPat),
    /// A member-expression assignment target (`o.p`, `o[k]`) — only valid in assignment-style
    /// destructuring / `for (o.p of …)`, never in a declaration.
    Member(Box<Expr>),
}

#[derive(Debug, Clone)]
pub enum ArrayPatElem {
    Hole,
    Elem {
        pattern: Pattern,
        default: Option<Expr>,
    },
    Rest(Pattern),
}

#[derive(Debug, Clone)]
pub struct ObjectPat {
    pub props: Vec<ObjPatProp>,
    /// `...rest` — a plain identifier collecting the remaining own enumerable keys.
    pub rest: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ObjPatProp {
    pub key: PropKey,
    pub value: Pattern,
    pub default: Option<Expr>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // some node fields (regex body/flags) are parsed before they are interpreted
pub enum Expr {
    /// A parenthesized array/object literal or assignment — recorded so destructuring
    /// reinterpretation can reject it (parens block the pattern refinement); evaluates
    /// transparently.
    Paren(P<Expr>),
    Num(f64),
    BigInt(crate::bigint::JsBigInt),
    Str(Rc<str>),
    /// A template-literal substitution: evaluate the inner expression and apply ToString (which uses
    /// the `string` hint — toString before valueOf — unlike `+` which uses the `default` hint).
    ToStr(Box<Expr>),
    Bool(bool),
    Null,
    Undefined,
    Ident(String),
    This,
    Regex {
        body: Rc<str>,
        flags: Rc<str>,
    },
    Array(Vec<ArrayElem>),
    Object(Vec<PropDef>),
    Func(Rc<Function>),
    Class(Rc<Class>),
    /// `yield expr` / `yield* expr` (only inside a generator).
    Yield {
        delegate: bool,
        arg: Option<P<Expr>>,
    },
    /// `await expr` (only inside an async function).
    Await(P<Expr>),
    /// The bare `super` keyword (only valid as `super(...)` or `super.x` / `super[x]`).
    Super,
    Unary {
        op: &'static str,
        arg: P<Expr>,
    },
    Update {
        op: &'static str,
        prefix: bool,
        arg: P<Expr>,
    },
    Binary {
        op: &'static str,
        left: P<Expr>,
        right: P<Expr>,
    },
    Logical {
        op: &'static str,
        left: P<Expr>,
        right: P<Expr>,
    },
    Assign {
        op: &'static str,
        target: P<Expr>,
        value: P<Expr>,
    },
    Cond {
        test: P<Expr>,
        cons: P<Expr>,
        alt: P<Expr>,
    },
    /// `pos` is the call's source position for stack traces (see [`NO_POS`]).
    Call {
        callee: P<Expr>,
        args: Vec<ArrayElem>,
        optional: bool,
        pos: u32,
    },
    New {
        callee: P<Expr>,
        args: Vec<ArrayElem>,
        pos: u32,
    },
    Member {
        obj: P<Expr>,
        prop: String,
        optional: bool,
    },
    Index {
        obj: P<Expr>,
        index: P<Expr>,
        optional: bool,
    },
    Seq(Vec<Expr>),
    /// `tag\`a${x}b\`` — `quasis` are (cooked, raw) chunks (one more than `subs`). `site` is
    /// the template site's identity for GetTemplateObject (see `parser::template_site`): stable
    /// across a lazy re-parse of the function it sits in, unlike the node's address.
    TaggedTemplate {
        tag: P<Expr>,
        quasis: Box<[(Option<String>, String)]>,
        site: u32,
        subs: Vec<Expr>,
    },
    /// An optional chain (`a?.b.c`): evaluates the inner LHS, short-circuiting to `undefined` if any
    /// `?.` link sees a nullish base.
    OptionalChain(P<Expr>),
    /// Ergonomic brand check `#field in obj`: whether `obj` carries the private field.
    PrivateIn {
        name: String,
        obj: P<Expr>,
    },
    /// Dynamic `import(specifier)` / `import.source(...)` / `import.defer(...)` — returns a
    /// promise. The phase selects the import semantics.
    ImportCall {
        spec: P<Expr>,
        phase: ImportPhase,
        /// The optional second argument (`import(spec, { with: { type: "json" } })`).
        options: Option<P<Expr>>,
    },
    /// `import.meta`.
    ImportMeta,
    /// `new.target`.
    NewTarget,
}

/// The phase of a dynamic `import()` call: plain evaluation, `import.source(...)` (source-phase),
/// or `import.defer(...)` (deferred-evaluation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportPhase {
    Evaluation,
    Source,
    Defer,
}

/// An array element or call argument: a value, a spread (`...x`), or a hole (`[1,,3]`).
#[derive(Debug, Clone)]
pub enum ArrayElem {
    Item(Expr),
    Spread(Expr),
    Hole,
}

#[derive(Debug, Clone)]
pub enum PropDef {
    /// `key: value` or shorthand `{ x }`.
    KeyValue {
        key: PropKey,
        value: Expr,
    },
    /// CoverInitializedName (`{ x = default }`): only valid when the literal is reinterpreted as
    /// a destructuring pattern; the parser rejects it anywhere else. `value` is the
    /// `x = default` assignment.
    Cover {
        key: PropKey,
        value: Expr,
    },
    /// Concise method `key() {}` (incl. generator/async). Carries a [[HomeObject]] so `super`
    /// inside the body resolves against the literal's prototype.
    Method {
        key: PropKey,
        func: Rc<Function>,
    },
    /// `get key() {}` / `set key(v) {}`.
    Getter {
        key: PropKey,
        func: Rc<Function>,
    },
    Setter {
        key: PropKey,
        func: Rc<Function>,
    },
    Spread(Expr),
    /// The colon-form `__proto__: value` in an object literal — sets `[[Prototype]]` (when the
    /// value is an Object or Null) rather than creating a property. Only the non-computed,
    /// non-shorthand, non-method form. As a destructuring pattern it degrades to a normal
    /// `__proto__` keyed target.
    Proto(Expr),
}

#[derive(Debug, Clone)]
pub enum PropKey {
    Ident(String),
    Str(Rc<str>),
    Num(f64),
    Computed(Expr),
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // expr_body is recorded for a future `toString`/source-fidelity pass
pub struct Function {
    pub name: Option<String>,
    pub params: Vec<Param>,
    /// The parsed body, read through [`Function::body`]. `None` until the first `ensure_body`
    /// when `lazy` is set, and again after the collector releases a body that went cold (see
    /// [`Function::release_cold_body`]); a released body is re-parsed from `lazy` on demand.
    pub body: std::cell::RefCell<Option<Rc<Vec<Stmt>>>>,
    /// The unparsed body: the source range a lazy parse (and any re-parse after a release) reads.
    /// `None` for an eagerly parsed body, which is never released.
    pub lazy: std::cell::RefCell<Option<Box<LazyBody>>>,
    /// Set by every [`Function::body`]; cleared by the collector's flush pass. A body that was
    /// not read between two collections is released.
    pub body_used: std::cell::Cell<bool>,
    /// The error a lazy parse produced, replayed by every later `ensure_body`.
    pub lazy_error: std::cell::OnceCell<Box<crate::parser::ParseError>>,
    pub is_arrow: bool,
    pub is_strict: bool,
    /// Arrow with an expression body (`x => x+1`): the single statement is a synthetic `return`.
    pub expr_body: bool,
    pub is_generator: bool,
    pub is_async: bool,
    /// A concise method / getter / setter: has no own `prototype` and is not a constructor (the
    /// class `constructor` member is re-flagged false once identified).
    pub is_method: bool,
    /// A function *expression* (`(function f(){})`): its own name binds immutably inside the
    /// function. A declaration's name binds (mutably) in the enclosing scope instead.
    pub is_fn_expr: bool,
    /// The source text this function was parsed from, for `Function.prototype.toString`.
    pub source: FnSource,
    /// Lazily-computed body facts (see [`Function::scan_flags`]): bit 0 = scanned, bit 1 =
    /// references `arguments`, bit 2 = references `new.target`, bit 3 = references `this`.
    /// A direct `eval` sets all three (it can reach any of them dynamically).
    pub scan: std::cell::Cell<u8>,
    /// Lazily-computed hoisting plan for the body (the strict flag it was computed under plus the
    /// ops), so calls replay a flat list instead of re-walking the AST (see
    /// `interpreter::collect_hoist_ops`). Reset with the body: the ops reference its nested nodes.
    pub hoist: std::cell::RefCell<Option<(bool, Rc<Vec<HoistOp>>)>>,
    /// Bytecode-tier state: call count until tier-up, and the compile result once attempted
    /// (`None` = uses constructs outside the bytecode subset; runs in the tree-walker forever).
    pub calls: std::cell::Cell<u32>,
    pub code: std::cell::OnceCell<Option<Rc<crate::bytecode::Chunk>>>,
    /// Lazily-built object-map templates for closures of this function (see
    /// `Interp::make_function` and [`crate::value::FnMaps`]): the function object's `Props`
    /// (length/name and a `prototype` placeholder) and, for prototype-bearing kinds, the fresh
    /// `.prototype`'s `Props` — cloned per closure instance instead of rebuilt insert by insert,
    /// so key hashing and shape transitions are paid once per FUNCTION rather than once per
    /// closure, and a prototype-less closure shares the template's entry block outright.
    pub fn_maps: std::cell::OnceCell<Box<crate::value::FnMaps>>,
}

/// One pre-scanned hoisting action for a statement list, replayed against a scope at function
/// call / script entry (see `interpreter::collect_hoist_ops` — order matters: `var`s in
/// traversal order, then function declarations in source order, then Annex B promotions).
#[derive(Debug, Clone)]
pub enum HoistOp {
    /// Declare a `var` binding (undefined) unless the name is already bound.
    Var(String),
    /// Declare a `var` binding (undefined) unconditionally (`for` / `for-in/of` heads).
    VarForce(String),
    /// Bind a hoisted function declaration (`*default*` names as "default").
    Fn(String, Rc<Function>),
    /// Annex B.3.3: promote a sloppy block function to an (if-absent) var binding and register
    /// it for declaration-time sync.
    AnnexB(String, Rc<Function>),
}

/// A call/`new` without a source position ([`Expr::Call`]/[`Expr::New`] `pos`; synthesized
/// code). A position is otherwise a byte offset into the text the enclosing function (or
/// script, module, eval) was parsed from — V8's convention: an identifier or property-name
/// callee's first character (`f()`, `o.m()`), else the `(` (`o[k]()`, `(0, f)()`); `new` for
/// `new C()`. Stack traces map it to a line and column lazily.
pub const NO_POS: u32 = u32::MAX;

pub const SCAN_DONE: u8 = 1;
pub const SCAN_ARGUMENTS: u8 = 2;
pub const SCAN_NEW_TARGET: u8 = 4;
pub const SCAN_THIS: u8 = 8;
/// The body itself contains a loop statement (not counting nested functions): the bytecode tier
/// compiles such functions on their *first* call — a single call can run a million iterations
/// (a benchmark driver's `while (elapsed < 1000)`), so waiting for a call-count threshold leaves
/// the hottest code on the tree-walker.
pub const SCAN_HAS_LOOP: u8 = 16;
/// `scan` bits owned by [`Function::may_use_home`] (independent of `SCAN_DONE`: a rescan that
/// overwrites them only costs a recheck): bit 6 = checked, bit 7 = the method may reach its
/// [[HomeObject]].
pub const SCAN_HOME_CHECKED: u8 = 64;
pub const SCAN_NEEDS_HOME: u8 = 128;

impl Function {
    /// The body statements, parsed (or re-parsed after a release) on demand. Empty for a lazy
    /// body whose parse failed — every reader that can run before the first call goes through
    /// [`ensure_body`](Function::ensure_body) first, which is where the SyntaxError surfaces.
    /// The returned `Rc` keeps the statements alive across a collection that releases the cell,
    /// so a reader that walks them holds it rather than a slice borrowed from a temporary.
    pub fn body(&self) -> Rc<Vec<Stmt>> {
        self.body_used.set(true);
        if let Some(b) = self.parsed_body() {
            return b;
        }
        let _ = self.ensure_body();
        self.parsed_body().unwrap_or_default()
    }

    /// The body only if it is currently materialised: never parses. For the parse-time
    /// early-error scans, which a skipped body runs itself when it is parsed.
    pub fn parsed_body(&self) -> Option<Rc<Vec<Stmt>>> {
        self.body.borrow().clone()
    }

    /// The body is not decoded yet and comes from an ahead-of-time blob (already validated at
    /// build time, so decoding it can wait until something needs the statements).
    pub fn body_is_aot_deferred(&self) -> bool {
        self.body.borrow().is_none()
            && self.lazy.borrow().as_ref().is_some_and(|l| l.aot.is_some())
    }

    /// Parse a skipped body. A failed parse is remembered and returned again on every later
    /// call, as the SyntaxError the first call throws.
    pub fn ensure_body(&self) -> Result<(), crate::parser::ParseError> {
        if self.parsed_body().is_some() {
            return Ok(());
        }
        if let Some(e) = self.lazy_error.get() {
            return Err((**e).clone());
        }
        let lazy = self.lazy.borrow();
        let Some(lazy) = lazy.as_ref() else {
            *self.body.borrow_mut() = Some(Rc::new(Vec::new()));
            return Ok(());
        };
        let parsed = match &lazy.aot {
            Some(aot) => crate::precompiled::decode_body(aot).map_err(|message| {
                crate::parser::ParseError {
                    message,
                    line: 0,
                    at_eof: false,
                }
            }),
            None => crate::parser::parse_lazy_body(lazy, self),
        };
        match parsed {
            Ok(stmts) => {
                *self.body.borrow_mut() = Some(Rc::new(stmts));
                Ok(())
            }
            Err(e) => {
                let _ = self.lazy_error.set(Box::new(e.clone()));
                Err(e)
            }
        }
    }

    /// The collector's flush step for one lazily-parsed function: a materialised body that no
    /// [`body`](Function::body) read since the previous collection is released, with the
    /// caches derived from it (`scan`, `hoist`), and is re-parsed from `lazy` if the function
    /// ever runs again. An activation still executing the body holds its own `Rc` to the
    /// statements, so releasing the cell never frees code under a running frame. Returns
    /// whether a body was released.
    pub fn release_cold_body(&self) -> bool {
        if self.body_used.replace(false) {
            return false;
        }
        self.release_body()
    }

    /// Drop a materialised body now, whatever its use flag says (the caller only borrowed it —
    /// see the bytecode tier's capture scan). Same re-parse contract as `release_cold_body`.
    pub fn release_body(&self) -> bool {
        if self.lazy.borrow().is_none() || self.body.take().is_none() {
            return false;
        }
        // The home-object bits are decided from the source text, which a release does not
        // change (and re-deciding them reads it: for a precompiled function, decompressing kept
        // text). The body facts are recomputed from the re-materialised body.
        self.scan.set(self.scan.get() & (SCAN_HOME_CHECKED | SCAN_NEEDS_HOME));
        self.hoist.take();
        true
    }

    pub fn source(&self) -> Option<Rc<str>> {
        self.source.text()
    }

    /// What this function's own activation must provide: whether the body (or a nested arrow, or a
    /// possible direct `eval`) can observe `arguments`, `new.target`, or `this`. Ordinary nested
    /// functions are opaque (they get their own); arrows are transparent. Conservative on the
    /// safe side: a false positive only costs an unused binding.
    /// Whether this method's code can observe its [[HomeObject]] — only `super.x` (in the
    /// params, the body or a nested arrow) and a possible direct `eval` can. Decided from the
    /// source text, conservatively: any `super` / `eval` substring or any backslash (an escaped
    /// identifier can spell `eval`) says yes, and so does a missing source. A method that can't
    /// needs no per-closure home scope — which would otherwise form a literal <-> method cycle
    /// only the collector can free.
    pub fn may_use_home(&self) -> bool {
        let s = self.scan.get();
        if s & SCAN_HOME_CHECKED != 0 {
            return s & SCAN_NEEDS_HOME != 0;
        }
        let needs = match self.source.as_str() {
            None => true,
            Some(t) => t.contains("super") || t.contains("eval") || t.contains('\\'),
        };
        self.scan.set(
            self.scan.get() | SCAN_HOME_CHECKED | if needs { SCAN_NEEDS_HOME } else { 0 },
        );
        needs
    }

    pub fn scan_flags(&self) -> u8 {
        let cached = self.scan.get();
        if cached & SCAN_DONE != 0 {
            return cached;
        }
        let mut flags = SCAN_DONE;
        for p in &self.params {
            scan_pattern(&p.pattern, &mut flags);
            if let Some(d) = &p.default {
                scan_expr(d, &mut flags);
            }
        }
        // A lazy body that fails to parse never runs; the flags only need to be safe, and the
        // conservative set costs nothing more than an unused binding.
        if self.ensure_body().is_err() {
            flags |= SCAN_ARGUMENTS | SCAN_NEW_TARGET | SCAN_THIS;
        }
        scan_stmts(&self.body(), &mut flags);
        flags |= cached & (SCAN_HOME_CHECKED | SCAN_NEEDS_HOME);
        self.scan.set(flags);
        flags
    }
}

const SCAN_ALL: u8 = SCAN_DONE | SCAN_ARGUMENTS | SCAN_NEW_TARGET | SCAN_THIS | SCAN_HAS_LOOP;

fn scan_stmts(body: &[Stmt], flags: &mut u8) {
    for s in body {
        scan_stmt(s, flags);
        if *flags == SCAN_ALL {
            return;
        }
    }
}

fn scan_stmt(s: &Stmt, flags: &mut u8) {
    match s {
        Stmt::Expr(e) | Stmt::Throw(e) => scan_expr(e, flags),
        Stmt::VarDecl { kind: _, decls } => {
            for (pat, init) in decls {
                scan_pattern(pat, flags);
                if let Some(e) = init {
                    scan_expr(e, flags);
                }
            }
        }
        // A nested (non-arrow) function has its own arguments/new.target/this.
        Stmt::FuncDecl(_) => {}
        Stmt::Return(e) => {
            if let Some(e) = e {
                scan_expr(e, flags);
            }
        }
        Stmt::If { test, cons, alt } => {
            scan_expr(test, flags);
            scan_stmt(cons, flags);
            if let Some(a) = alt {
                scan_stmt(a, flags);
            }
        }
        Stmt::Block(b) => scan_stmts(b, flags),
        Stmt::While { test, body } | Stmt::DoWhile { body, test } => {
            *flags |= SCAN_HAS_LOOP;
            scan_expr(test, flags);
            scan_stmt(body, flags);
        }
        Stmt::For {
            init,
            test,
            update,
            body,
        } => {
            *flags |= SCAN_HAS_LOOP;
            match init.as_deref() {
                Some(ForInit::VarDecl { kind: _, decls }) => {
                    for (pat, e) in decls {
                        scan_pattern(pat, flags);
                        if let Some(e) = e {
                            scan_expr(e, flags);
                        }
                    }
                }
                Some(ForInit::Expr(e)) => scan_expr(e, flags),
                None => {}
            }
            if let Some(e) = test {
                scan_expr(e, flags);
            }
            if let Some(e) = update {
                scan_expr(e, flags);
            }
            scan_stmt(body, flags);
        }
        Stmt::ForInOf {
            decl: _,
            left,
            right,
            of: _,
            is_await: _,
            body,
        } => {
            *flags |= SCAN_HAS_LOOP;
            scan_pattern(left, flags);
            scan_expr(right, flags);
            scan_stmt(body, flags);
        }
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Empty | Stmt::Debugger => {}
        Stmt::Try {
            block,
            handler,
            finalizer,
        } => {
            scan_stmts(block, flags);
            if let Some((param, hbody)) = handler {
                if let Some(p) = param {
                    scan_pattern(p, flags);
                }
                scan_stmts(hbody, flags);
            }
            if let Some(f) = finalizer {
                scan_stmts(f, flags);
            }
        }
        Stmt::Switch { disc, cases } => {
            scan_expr(disc, flags);
            for c in cases {
                if let Some(t) = &c.test {
                    scan_expr(t, flags);
                }
                scan_stmts(&c.body, flags);
            }
        }
        Stmt::Labeled { label: _, body } => scan_stmt(body, flags),
        Stmt::With { obj, body } => {
            scan_expr(obj, flags);
            scan_stmt(body, flags);
        }
        Stmt::ClassDecl(c) => scan_class(c, flags),
        Stmt::Import(_) | Stmt::ExportNamed { .. } | Stmt::ExportAll { .. } => {}
        Stmt::ExportDecl(inner) | Stmt::ExportDefault(inner) => scan_stmt(inner, flags),
    }
}

fn scan_expr(e: &Expr, flags: &mut u8) {
    match e {
        Expr::Ident(n) => {
            if n == "arguments" {
                *flags |= SCAN_ARGUMENTS;
            }
        }
        Expr::This => *flags |= SCAN_THIS,
        Expr::NewTarget => *flags |= SCAN_NEW_TARGET,
        Expr::Num(_)
        | Expr::BigInt(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Undefined
        | Expr::Regex { .. }
        | Expr::ImportMeta => {}
        // `super.x` resolves its receiver through the `this` binding; `super()` initializes it.
        Expr::Super => *flags |= SCAN_THIS,
        Expr::Paren(inner)
        | Expr::ToStr(inner)
        | Expr::Await(inner)
        | Expr::OptionalChain(inner) => scan_expr(inner, flags),
        Expr::Array(elems) => {
            for el in elems {
                match el {
                    ArrayElem::Item(e) | ArrayElem::Spread(e) => scan_expr(e, flags),
                    ArrayElem::Hole => {}
                }
            }
        }
        Expr::Object(props) => {
            for p in props {
                match p {
                    PropDef::KeyValue { key, value } | PropDef::Cover { key, value } => {
                        scan_prop_key(key, flags);
                        scan_expr(value, flags);
                    }
                    // A concise method/accessor body is its own function scope; only its
                    // (computed) key evaluates here.
                    PropDef::Method { key, func: _ }
                    | PropDef::Getter { key, func: _ }
                    | PropDef::Setter { key, func: _ } => scan_prop_key(key, flags),
                    PropDef::Spread(e) | PropDef::Proto(e) => scan_expr(e, flags),
                }
            }
        }
        // An arrow is transparent (it closes over the enclosing activation); an ordinary
        // function expression is opaque.
        Expr::Func(f) => {
            if f.is_arrow {
                let inner = f.scan_flags();
                *flags |= inner & (SCAN_ARGUMENTS | SCAN_NEW_TARGET | SCAN_THIS);
            }
        }
        Expr::Class(c) => scan_class(c, flags),
        Expr::Yield { delegate: _, arg } => {
            if let Some(a) = arg {
                scan_expr(a, flags);
            }
        }
        Expr::Unary { op: _, arg }
        | Expr::Update {
            op: _,
            prefix: _,
            arg,
        } => scan_expr(arg, flags),
        Expr::Binary { op: _, left, right } | Expr::Logical { op: _, left, right } => {
            scan_expr(left, flags);
            scan_expr(right, flags);
        }
        Expr::Assign {
            op: _,
            target,
            value,
        } => {
            scan_expr(target, flags);
            scan_expr(value, flags);
        }
        Expr::Cond { test, cons, alt } => {
            scan_expr(test, flags);
            scan_expr(cons, flags);
            scan_expr(alt, flags);
        }
        Expr::Call { callee, args, .. } => {
            // A direct `eval` can name any of the three dynamically.
            if matches!(&**callee, Expr::Ident(n) if n == "eval") {
                *flags |= SCAN_ARGUMENTS | SCAN_NEW_TARGET | SCAN_THIS;
            }
            // `arr.map(x => …)` compiles to a loop (see `bytecode::inline_callback`).
            if crate::bytecode::inline_callback::is_loop_call(callee, args) {
                *flags |= SCAN_HAS_LOOP;
            }
            scan_expr(callee, flags);
            for a in args {
                match a {
                    ArrayElem::Item(e) | ArrayElem::Spread(e) => scan_expr(e, flags),
                    ArrayElem::Hole => {}
                }
            }
        }
        Expr::New { callee, args, .. } => {
            scan_expr(callee, flags);
            for a in args {
                match a {
                    ArrayElem::Item(e) | ArrayElem::Spread(e) => scan_expr(e, flags),
                    ArrayElem::Hole => {}
                }
            }
        }
        Expr::Member {
            obj,
            prop: _,
            optional: _,
        } => scan_expr(obj, flags),
        Expr::Index {
            obj,
            index,
            optional: _,
        } => {
            scan_expr(obj, flags);
            scan_expr(index, flags);
        }
        Expr::Seq(exprs) => {
            for e in exprs {
                scan_expr(e, flags);
            }
        }
        Expr::TaggedTemplate { tag, subs, .. } => {
            scan_expr(tag, flags);
            for e in subs {
                scan_expr(e, flags);
            }
        }
        Expr::PrivateIn { name: _, obj } => scan_expr(obj, flags),
        Expr::ImportCall {
            spec,
            phase: _,
            options,
        } => {
            scan_expr(spec, flags);
            if let Some(o) = options {
                scan_expr(o, flags);
            }
        }
    }
}

fn scan_class(c: &Class, flags: &mut u8) {
    // Heritage, decorators, and computed keys evaluate in the enclosing scope. Member bodies are
    // their own function scopes; field/accessor initializers and static blocks can't legally name
    // `arguments`, but walking them costs only a possible false positive.
    if let Some(sc) = &c.superclass {
        scan_expr(sc, flags);
    }
    for d in &c.decorators {
        scan_expr(d, flags);
    }
    for m in &c.members {
        scan_prop_key(&m.key, flags);
        for d in &m.decorators {
            scan_expr(d, flags);
        }
        if let Some(v) = &m.value {
            scan_expr(v, flags);
        }
    }
}

fn scan_prop_key(k: &PropKey, flags: &mut u8) {
    match k {
        PropKey::Ident(_) | PropKey::Str(_) | PropKey::Num(_) => {}
        PropKey::Computed(e) => scan_expr(e, flags),
    }
}

fn scan_pattern(p: &Pattern, flags: &mut u8) {
    match p {
        Pattern::Ident(n) => {
            if n == "arguments" {
                *flags |= SCAN_ARGUMENTS;
            }
        }
        Pattern::Array(elems) => {
            for el in elems {
                match el {
                    ArrayPatElem::Hole => {}
                    ArrayPatElem::Elem { pattern, default } => {
                        scan_pattern(pattern, flags);
                        if let Some(d) = default {
                            scan_expr(d, flags);
                        }
                    }
                    ArrayPatElem::Rest(pat) => scan_pattern(pat, flags),
                }
            }
        }
        Pattern::Object(op) => {
            for prop in &op.props {
                scan_prop_key(&prop.key, flags);
                scan_pattern(&prop.value, flags);
                if let Some(d) = &prop.default {
                    scan_expr(d, flags);
                }
            }
            if op.rest.as_deref() == Some("arguments") {
                *flags |= SCAN_ARGUMENTS;
            }
        }
        Pattern::Member(e) => scan_expr(e, flags),
    }
}

#[derive(Debug, Clone)]
pub struct Param {
    pub pattern: Pattern,
    pub default: Option<Expr>,
    pub rest: bool,
}
