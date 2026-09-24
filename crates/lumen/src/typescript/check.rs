//! The local soundness checker (docs/typed-tier.md §4) and the `TypeTable` builder.
//!
//! The checker is an optimizer's proof obligation, not tsc: it checks one fully annotated
//! function at a time against declared signatures, never reports an error that stops a program,
//! and marks anything it does not understand "not sound (reason)". It reasons with the full
//! [`Type`]; the table it emits only carries [`TKind`] projections.
//!
//! Trust model. A static type is *trusted* only when the engine can rely on it without a check:
//! literals, arithmetic, parameters and `this` after their entry checks, locals of this
//! function that no closure writes, fields of classes whose layout is sound, results of direct
//! calls to sound functions, and the intrinsics of §4.4. Every other read (module variables,
//! captured variables, `declare`d bindings, properties of structural types, elements of
//! arrays, results of non-sound or dynamic calls, narrowed locals) gets a `SiteFact::Check`
//! against its declared type (§6.2). Values from outside the checked program (globals,
//! imports, lib calls) have type [`Type::External`]: every operation on them is dynamic and
//! they are checked where they flow into a typed location.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use super::ast::*;
use super::jsdoc::{self, JsDoc};
use super::options::CompilerOptions;
use super::table::*;
use super::types::*;
use super::walk::{self, for_each_child, Walk};

#[derive(Debug, Clone)]
struct Unsound {
    at: u32,
    msg: String,
}

type R<T> = Result<T, Unsound>;

fn fail<T>(at: u32, msg: impl Into<String>) -> R<T> {
    Err(Unsound {
        at,
        msg: msg.into(),
    })
}

fn is_dynamic(t: &Type) -> bool {
    matches!(t, Type::External | Type::Opaque(_) | Type::Param(_))
}

const MATH_FNS: &[&str] = &[
    "abs", "acos", "acosh", "asin", "asinh", "atan", "atanh", "atan2", "cbrt", "ceil", "clz32",
    "cos", "cosh", "exp", "expm1", "floor", "fround", "hypot", "imul", "log", "log1p", "log10",
    "log2", "max", "min", "pow", "random", "round", "sign", "sin", "sinh", "sqrt", "tan", "tanh",
    "trunc",
];
const MATH_CONSTS: &[&str] = &[
    "PI", "E", "LN2", "LN10", "LOG2E", "LOG10E", "SQRT1_2", "SQRT2",
];

enum TypeDecl {
    Alias {
        params: Vec<TypeParam>,
        ty: Type,
    },
    Interface {
        params: Vec<TypeParam>,
        bodies: Vec<ObjectType>,
        extends: Vec<Type>,
    },
    Class(ClassId),
    Enum(Type),
}

#[derive(Clone)]
enum ValueDecl {
    Func(FnId),
    Class(ClassId),
    Var {
        ty: Option<Type>,
        is_const: bool,
        /// A `const` initialized by a primitive literal: its type is trusted.
        literal: Option<Type>,
        declare: bool,
    },
    Enum(Type),
    External,
}

#[derive(Clone)]
struct FieldInfo {
    name: String,
    /// The declared (or inferred) type.
    declared: Type,
    /// The type the field is guaranteed to have whenever other code can observe the object
    /// (`declared | undefined` when it is not definitely assigned first, row 14).
    kind_type: Type,
    readonly: bool,
    loc: Loc,
}

struct ClassInfo {
    name: String,
    parent: Option<ClassId>,
    fields: Vec<FieldInfo>,
    /// Instance methods and accessors: name, function, kind.
    methods: Vec<(String, FnId, FnKind)>,
    ctor: Option<FnId>,
    tparams: Vec<String>,
    problem: Option<(u32, String)>,
}

struct SigParam {
    name: String,
    ty: Type,
    optional: bool,
    rest: bool,
    has_default: bool,
    loc: Loc,
}

struct Sig {
    params: Vec<SigParam>,
    this: Option<Type>,
    ret: Option<Type>,
    predicate: Option<Predicate>,
    tparams: Vec<String>,
    problem: Option<(u32, String)>,
}

pub(crate) struct Program<'m> {
    m: &'m Module,
    src: &'m str,
    opts: &'m CompilerOptions,
    types: HashMap<String, TypeDecl>,
    values: HashMap<String, ValueDecl>,
    classes: Vec<ClassInfo>,
    layout_sound: Vec<bool>,
    docs: Vec<JsDoc>,
    contextual: HashMap<FnId, FnType>,
    display: HashMap<FnId, String>,
    sigs: Vec<Sig>,
    /// Names assigned anywhere in the file.
    assigned: HashSet<String>,
    /// Names assigned by any function nested in each function.
    closure_written: Vec<HashSet<String>>,
    closure_unknown: Vec<bool>,
    dynamic_scope: bool,
    file_reason: Option<(u32, String)>,
    ignores: Vec<u32>,
    sound: Vec<bool>,
    sig_ids: RefCell<Vec<Signature>>,
}

struct TCx<'a> {
    tparams: &'a [String],
    this_class: Option<ClassId>,
    stack: Vec<String>,
}

impl ClassHierarchy for Program<'_> {
    fn parent(&self, class: u32) -> Option<u32> {
        self.classes
            .get(class as usize)
            .and_then(|c| c.parent)
            .map(|p| p as u32)
    }
    fn instance_members(&self, class: u32) -> Option<ObjectType> {
        let mut props = Vec::new();
        let mut c = Some(class as usize);
        while let Some(id) = c {
            let info = self.classes.get(id)?;
            for f in &info.fields {
                if !f.name.starts_with('#') && !props.iter().any(|p: &Property| p.name == f.name) {
                    props.push(Property {
                        name: f.name.clone(),
                        optional: false,
                        readonly: f.readonly,
                        method: false,
                        ty: f.kind_type.clone(),
                    });
                }
            }
            for (name, fid, kind) in &info.methods {
                if *kind == FnKind::Method && !props.iter().any(|p| &p.name == name) {
                    props.push(Property {
                        name: name.clone(),
                        optional: false,
                        readonly: true,
                        method: true,
                        ty: Type::Function(Box::new(self.fn_type_of(*fid))),
                    });
                }
            }
            c = info.parent;
        }
        Some(ObjectType {
            props,
            ..ObjectType::default()
        })
    }
}

fn directive(src: &str, c: &super::lexer::Comment, word: &str) -> bool {
    let text = &src[c.start as usize..c.end as usize];
    let body = text
        .trim_start_matches("//")
        .trim_start_matches("/*")
        .trim_start_matches('*')
        .trim_start();
    body.starts_with(word)
        && !body[word.len()..]
            .chars()
            .next()
            .is_some_and(|ch| ch.is_alphanumeric() || ch == '-')
}

impl<'m> Program<'m> {
    pub(crate) fn new(m: &'m Module, src: &'m str, opts: &'m CompilerOptions) -> Self {
        let n = m.funcs.len();
        let mut p = Program {
            m,
            src,
            opts,
            types: HashMap::new(),
            values: HashMap::new(),
            classes: Vec::new(),
            layout_sound: vec![false; m.classes.len()],
            docs: vec![JsDoc::default(); n],
            contextual: HashMap::new(),
            display: HashMap::new(),
            sigs: Vec::new(),
            assigned: HashSet::new(),
            closure_written: vec![HashSet::new(); n],
            closure_unknown: vec![false; n],
            dynamic_scope: false,
            file_reason: None,
            ignores: Vec::new(),
            sound: vec![false; n],
            sig_ids: RefCell::new(Vec::new()),
        };
        p.file_level();
        p.declarations(&m.stmts);
        if m.lang == Lang::Js {
            p.js_typedefs();
            for f in &m.funcs {
                if let Some(d) = p.doc_at(f.doc_anchor) {
                    p.docs[f.id] = d;
                }
            }
        }
        p.assignments();
        p.build_classes();
        p.contextual_pass();
        p.sigs = (0..n).map(|id| p.compute_sig(id)).collect();
        p.class_soundness();
        p
    }

    fn js(&self) -> bool {
        self.m.lang == Lang::Js
    }

    /// The parsed JSDoc comment attached to a declaration starting at `anchor` (JS only).
    fn doc_at(&self, anchor: u32) -> Option<JsDoc> {
        if !self.js() {
            return None;
        }
        let comments = &self.m.comments;
        let idx = comments.partition_point(|c| c.end <= anchor);
        let c = comments.get(idx.checked_sub(1)?)?;
        let gap = self.src.get(c.end as usize..anchor as usize)?;
        if !c.is_jsdoc(self.src) || !gap.chars().all(char::is_whitespace) {
            return None;
        }
        Some(jsdoc::parse_comment(jsdoc::comment_text(self.src, *c)))
    }

    fn file_level(&mut self) {
        let first_tok = self
            .m
            .stmts
            .first()
            .map(|s| s.loc().start)
            .unwrap_or(u32::MAX);
        let mut ts_check = false;
        let mut nocheck = false;
        for c in &self.m.comments {
            if directive(self.src, c, "@ts-ignore") || directive(self.src, c, "@ts-expect-error") {
                self.ignores.push(c.start);
            }
            if c.start < first_tok || first_tok == 0 {
                if directive(self.src, c, "@ts-check") {
                    ts_check = true;
                }
                if directive(self.src, c, "@ts-nocheck") {
                    nocheck = true;
                }
            }
        }
        // `first_tok == 0` covers a function/class statement whose `loc` is not tracked.
        self.file_reason = if nocheck {
            Some((0, "file has // @ts-nocheck (row 17)".into()))
        } else if !self.opts.strict_null_checks {
            Some((
                0,
                "strictNullChecks is off: file is hints-only (row 18)".into(),
            ))
        } else if self.js() && !ts_check && !self.opts.check_js {
            Some((
                0,
                "JSDoc is not checked (add // @ts-check or checkJs): hints only (§4.5)".into(),
            ))
        } else {
            None
        };
    }

    fn declarations(&mut self, stmts: &'m [Stmt]) {
        for s in stmts {
            self.declaration(s);
        }
    }

    fn declaration(&mut self, s: &'m Stmt) {
        match s {
            Stmt::Export { stmt, .. } => self.declaration(stmt),
            Stmt::Func(id) => {
                if let Some(n) = &self.m.funcs[*id].name {
                    // Overloads/`declare` signatures share the name; the implementation wins.
                    let has_body = self.m.funcs[*id].body.is_some();
                    let existing_body = matches!(self.values.get(n), Some(ValueDecl::Func(e)) if self.m.funcs[*e].body.is_some());
                    if has_body || !existing_body {
                        self.values.insert(n.clone(), ValueDecl::Func(*id));
                    }
                }
            }
            Stmt::Class(id) => {
                if let Some(n) = &self.m.classes[*id].name {
                    self.values.insert(n.clone(), ValueDecl::Class(*id));
                    self.types.insert(n.clone(), TypeDecl::Class(*id));
                }
            }
            Stmt::Var {
                kind,
                decls,
                declare,
                loc,
            } => {
                let doc_ty = self.doc_at(loc.start).and_then(|d| d.ty);
                for d in decls {
                    let Pattern::Ident { name, .. } = &d.pat else {
                        for (n, _) in d.pat.names() {
                            self.values.insert(n, ValueDecl::External);
                        }
                        continue;
                    };
                    let literal = match (&d.init, kind) {
                        (Some(init), VarKind::Const) => literal_type(init),
                        _ => None,
                    };
                    let ty = d.ty.clone().or_else(|| {
                        if decls.len() == 1 {
                            doc_ty.clone()
                        } else {
                            None
                        }
                    });
                    self.values.insert(
                        name.clone(),
                        ValueDecl::Var {
                            ty,
                            is_const: *kind == VarKind::Const,
                            literal,
                            declare: *declare,
                        },
                    );
                }
            }
            Stmt::TypeAlias {
                name, params, ty, ..
            } => {
                self.types.insert(
                    name.clone(),
                    TypeDecl::Alias {
                        params: params.clone(),
                        ty: ty.clone(),
                    },
                );
            }
            Stmt::Interface {
                name,
                params,
                extends,
                body,
                ..
            } => match self.types.get_mut(name) {
                Some(TypeDecl::Interface {
                    bodies,
                    extends: ex,
                    ..
                }) => {
                    bodies.push(body.clone());
                    ex.extend(extends.iter().cloned());
                }
                _ => {
                    self.types.insert(
                        name.clone(),
                        TypeDecl::Interface {
                            params: params.clone(),
                            bodies: vec![body.clone()],
                            extends: extends.clone(),
                        },
                    );
                }
            },
            Stmt::Enum { name, members, .. } => {
                // Numeric enums are `number` (row 15); all-string enums are `string`.
                let all_str = !members.is_empty()
                    && members.iter().all(|(_, i)| {
                        matches!(i.as_ref().map(|e| &e.kind), Some(ExprKind::Str(_)))
                    });
                let ty = if all_str { Type::String } else { Type::Number };
                self.types.insert(name.clone(), TypeDecl::Enum(ty.clone()));
                self.values.insert(name.clone(), ValueDecl::Enum(ty));
            }
            Stmt::Import { names, .. } => {
                for n in names {
                    if !n.type_only {
                        self.values.insert(n.local.clone(), ValueDecl::External);
                    }
                }
            }
            Stmt::ImportEquals { name, .. } => {
                self.values.insert(name.clone(), ValueDecl::External);
            }
            Stmt::Namespace { name, .. } => {
                self.values.insert(name.clone(), ValueDecl::External);
            }
            _ => {}
        }
    }

    fn js_typedefs(&mut self) {
        for c in &self.m.comments {
            if !c.is_jsdoc(self.src) {
                continue;
            }
            let text = jsdoc::comment_text(self.src, *c);
            if !text.contains("@typedef") && !text.contains("@callback") {
                continue;
            }
            for (name, ty) in jsdoc::parse_comment(text).typedefs {
                self.types.insert(
                    name,
                    TypeDecl::Alias {
                        params: Vec::new(),
                        ty,
                    },
                );
            }
        }
    }

    fn assignments(&mut self) {
        let m = self.m;
        let mut top = Vec::new();
        let mut unknown = false;
        let w = Walk {
            m,
            into_arrows: false,
            into_fns: false,
        };
        w.stmts(&m.stmts, &mut |e| {
            walk::collect_assigned(e, &mut top, &mut unknown)
        });
        // Class field initializers and static blocks run outside any function body.
        for c in &m.classes {
            for mem in &c.members {
                match mem {
                    Member::Field { init: Some(e), .. } => w.expr(e, &mut |x| {
                        walk::collect_assigned(x, &mut top, &mut unknown)
                    }),
                    Member::StaticBlock(ss, _) => w.stmts(ss, &mut |x| {
                        walk::collect_assigned(x, &mut top, &mut unknown)
                    }),
                    _ => {}
                }
            }
        }
        self.assigned.extend(top);
        for f in &m.funcs {
            let (names, unk) = walk::own_assigned(m, f.id);
            let mut anc = f.parent;
            while let Some(a) = anc {
                self.closure_written[a].extend(names.iter().cloned());
                self.closure_unknown[a] |= unk;
                anc = m.funcs[a].parent;
            }
            self.assigned.extend(names);
        }
        let mut dynamic = false;
        let w = Walk {
            m,
            into_arrows: true,
            into_fns: true,
        };
        let mut check = |e: &Expr| {
            if let ExprKind::Call { callee, .. } = &e.kind {
                if matches!(&callee.kind, ExprKind::Ident(n) if n == "eval") {
                    dynamic = true;
                }
            }
        };
        w.stmts(&m.stmts, &mut check);
        for c in &m.classes {
            let w2 = Walk {
                m,
                into_arrows: true,
                into_fns: true,
            };
            w2.class(c.id, &mut check);
        }
        fn has_with(s: &Stmt) -> bool {
            match s {
                Stmt::With { .. } => true,
                Stmt::Block(ss, _) => ss.iter().any(has_with),
                Stmt::If { cons, alt, .. } => {
                    has_with(cons) || alt.as_deref().is_some_and(has_with)
                }
                Stmt::For { body, .. }
                | Stmt::ForIn { body, .. }
                | Stmt::While { body, .. }
                | Stmt::DoWhile { body, .. }
                | Stmt::Labeled { body, .. } => has_with(body),
                Stmt::Try {
                    block,
                    handler,
                    finalizer,
                    ..
                } => {
                    block.iter().any(has_with)
                        || handler.as_ref().is_some_and(|h| h.iter().any(has_with))
                        || finalizer.as_ref().is_some_and(|h| h.iter().any(has_with))
                }
                Stmt::Switch { cases, .. } => cases.iter().any(|(_, b)| b.iter().any(has_with)),
                _ => false,
            }
        }
        let with_anywhere = m.stmts.iter().any(has_with)
            || m.funcs.iter().any(|f| match &f.body {
                Some(Body::Block(ss)) => ss.iter().any(has_with),
                _ => false,
            });
        self.dynamic_scope = dynamic || with_anywhere;
    }

    // ----- type resolution ---------------------------------------------------------------

    fn resolve_in(&self, t: &Type, tparams: &[String], this_class: Option<ClassId>) -> Type {
        let mut cx = TCx {
            tparams,
            this_class,
            stack: Vec::new(),
        };
        self.resolve(t, &mut cx).widen()
    }

    fn resolve(&self, t: &Type, cx: &mut TCx) -> Type {
        match t {
            Type::Reference { name, arguments } => self.resolve_ref(name, arguments, cx),
            Type::Array(e) => Type::Array(Box::new(self.resolve(e, cx))),
            Type::ReadonlyArray(e) => Type::ReadonlyArray(Box::new(self.resolve(e, cx))),
            Type::Tuple(ts) => Type::Tuple(ts.iter().map(|t| self.resolve(t, cx)).collect()),
            Type::Union(ts) => Type::union(ts.iter().map(|t| self.resolve(t, cx)).collect()),
            Type::Intersection(ts) => {
                let ms: Vec<Type> = ts.iter().map(|t| self.resolve(t, cx)).collect();
                if ms.iter().all(|m| matches!(m, Type::Object(_))) {
                    let mut o = ObjectType::default();
                    for m in ms {
                        if let Type::Object(x) = m {
                            o.props.extend(x.props);
                            o.index.extend(x.index);
                            o.calls.extend(x.calls);
                        }
                    }
                    Type::Object(o)
                } else {
                    Type::Intersection(ms)
                }
            }
            Type::Object(o) => {
                let o2 = ObjectType {
                    props: o
                        .props
                        .iter()
                        .map(|p| Property {
                            ty: self.resolve(&p.ty, cx),
                            ..p.clone()
                        })
                        .collect(),
                    index: o
                        .index
                        .iter()
                        .map(|i| IndexSignature {
                            key: self.resolve(&i.key, cx),
                            value: self.resolve(&i.value, cx),
                            readonly: i.readonly,
                        })
                        .collect(),
                    calls: o.calls.iter().map(|f| self.resolve_fn(f, cx)).collect(),
                    constructs: o
                        .constructs
                        .iter()
                        .map(|f| self.resolve_fn(f, cx))
                        .collect(),
                };
                if o2.props.is_empty()
                    && o2.index.is_empty()
                    && o2.constructs.is_empty()
                    && o2.calls.len() == 1
                {
                    return Type::Function(Box::new(o2.calls.into_iter().next().unwrap()));
                }
                Type::Object(o2)
            }
            Type::Function(f) => Type::Function(Box::new(self.resolve_fn(f, cx))),
            Type::This => match cx.this_class {
                Some(c) => Type::Instance(c as u32),
                None => Type::Opaque("this type".into()),
            },
            t => t.clone(),
        }
    }

    fn resolve_fn(&self, f: &FnType, cx: &mut TCx) -> FnType {
        let mut tps: Vec<String> = cx.tparams.to_vec();
        tps.extend(f.type_params.iter().map(|p| p.name.clone()));
        let mut inner = TCx {
            tparams: &tps,
            this_class: cx.this_class,
            stack: std::mem::take(&mut cx.stack),
        };
        let out = FnType {
            type_params: f.type_params.clone(),
            this: f.this.as_ref().map(|t| self.resolve(t, &mut inner)),
            params: f
                .params
                .iter()
                .map(|p| Param {
                    ty: self.resolve(&p.ty, &mut inner),
                    ..p.clone()
                })
                .collect(),
            ret: self.resolve(&f.ret, &mut inner),
            predicate: f.predicate.clone(),
            construct: f.construct,
        };
        cx.stack = inner.stack;
        out
    }

    fn resolve_ref(&self, name: &str, args: &[Type], cx: &mut TCx) -> Type {
        if cx.tparams.iter().any(|p| p == name) {
            return Type::Param(name.to_string());
        }
        let arg = |i: usize, cx: &mut TCx| -> Type {
            args.get(i)
                .map(|a| self.resolve(a, cx))
                .unwrap_or(Type::Unknown)
        };
        if !self.types.contains_key(name) {
            match (name, args.len()) {
                ("Array", 1) => return Type::Array(Box::new(arg(0, cx))),
                ("ReadonlyArray", 1) => return Type::ReadonlyArray(Box::new(arg(0, cx))),
                ("Readonly", 1) => {
                    return match arg(0, cx) {
                        Type::Array(e) => Type::ReadonlyArray(e),
                        t => t,
                    }
                }
                ("NonNullable", 1) => return arg(0, cx).without_nullish(),
                ("Record", 2) => {
                    let key = arg(0, cx);
                    let value = arg(1, cx);
                    return Type::Object(ObjectType {
                        index: vec![IndexSignature {
                            key,
                            value,
                            readonly: false,
                        }],
                        ..ObjectType::default()
                    });
                }
                // `Function` is callable with anything and returns `any` (row 1).
                ("Function", 0) => return Type::Any,
                ("Object", 0) => return Type::Object(ObjectType::default()),
                _ => return Type::Opaque(name.to_string()),
            }
        }
        if cx.stack.iter().any(|s| s == name) || cx.stack.len() > 16 {
            return Type::Opaque(format!("recursive {name}"));
        }
        cx.stack.push(name.to_string());
        let out = match self.types.get(name) {
            Some(TypeDecl::Class(id)) => Type::Instance(*id as u32),
            Some(TypeDecl::Enum(t)) => t.clone(),
            Some(TypeDecl::Alias { params, ty }) => {
                let map = self.type_args(params, args, cx);
                self.resolve(&subst(ty, &map), cx)
            }
            Some(TypeDecl::Interface {
                params,
                bodies,
                extends,
            }) => {
                let map = self.type_args(params, args, cx);
                let mut o = ObjectType::default();
                for base in extends {
                    if let Type::Object(b) = self.resolve(&subst(base, &map), cx) {
                        o.props.extend(b.props);
                        o.index.extend(b.index);
                        o.calls.extend(b.calls);
                    }
                }
                for body in bodies {
                    if let Type::Object(b) =
                        self.resolve(&subst(&Type::Object(body.clone()), &map), cx)
                    {
                        for p in b.props {
                            o.props.retain(|q| q.name != p.name);
                            o.props.push(p);
                        }
                        o.index.extend(b.index);
                        o.calls.extend(b.calls);
                        o.constructs.extend(b.constructs);
                    } else if let Type::Function(f) =
                        self.resolve(&subst(&Type::Object(body.clone()), &map), cx)
                    {
                        o.calls.push(*f);
                    }
                }
                if o.props.is_empty() && o.index.is_empty() && o.calls.len() == 1 {
                    Type::Function(Box::new(o.calls.pop().unwrap()))
                } else {
                    Type::Object(o)
                }
            }
            None => Type::Opaque(name.to_string()),
        };
        cx.stack.pop();
        out
    }

    fn type_args(
        &self,
        params: &[TypeParam],
        args: &[Type],
        cx: &mut TCx,
    ) -> HashMap<String, Type> {
        params
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let t = args
                    .get(i)
                    .or(p.default.as_ref())
                    .map(|a| self.resolve(a, cx))
                    .unwrap_or(Type::Unknown);
                (p.name.clone(), t)
            })
            .collect()
    }

    // ----- classes -----------------------------------------------------------------------

    fn build_classes(&mut self) {
        let m = self.m;
        for c in &m.classes {
            let parent = match c.extends.as_ref().map(|e| &e.kind) {
                Some(ExprKind::Ident(n)) => match self.values.get(n) {
                    Some(ValueDecl::Class(p)) if *p != c.id => Some(*p),
                    _ => None,
                },
                _ => None,
            };
            let mut problem = None;
            if c.extends.is_some() && parent.is_none() {
                problem = Some((
                    c.start,
                    "extends a base that is not a class of this file".to_string(),
                ));
            }
            let mut methods = Vec::new();
            let mut ctor = None;
            for mem in &c.members {
                if let Member::Method {
                    name,
                    func,
                    is_static,
                    computed,
                    ..
                } = mem
                {
                    let f = &m.funcs[*func];
                    if f.kind == FnKind::Constructor {
                        if f.body.is_some() {
                            ctor = Some(*func);
                        }
                    } else if !*is_static && !*computed && f.body.is_some() {
                        methods.push((name.clone(), *func, f.kind));
                    }
                }
            }
            self.classes.push(ClassInfo {
                name: c.name.clone().unwrap_or_else(|| "<class>".into()),
                parent,
                fields: Vec::new(),
                methods,
                ctor,
                tparams: c.type_params.iter().map(|t| t.name.clone()).collect(),
                problem,
            });
        }
        // Parents must not form a cycle (`class A extends B {} class B extends A {}`).
        for i in 0..self.classes.len() {
            let mut seen = HashSet::new();
            let mut c = Some(i);
            while let Some(id) = c {
                if !seen.insert(id) {
                    self.classes[i].parent = None;
                    self.classes[i].problem = Some((m.classes[i].start, "cyclic extends".into()));
                    break;
                }
                c = self.classes[id].parent;
            }
        }
        for c in &m.classes {
            self.class_fields(c);
        }
    }

    fn class_fields(&mut self, c: &'m ClassNode) {
        let m = self.m;
        let tparams = self.classes[c.id].tparams.clone();
        let mut fields: Vec<FieldInfo> = Vec::new();
        let mut problem = self.classes[c.id].problem.take();
        let note = |p: &mut Option<(u32, String)>, at: u32, msg: String| {
            if p.is_none() {
                *p = Some((at, msg));
            }
        };
        for mem in &c.members {
            let Member::Field {
                name,
                computed,
                loc,
                ty,
                optional,
                definite,
                readonly,
                is_static,
                declare,
                accessor,
                init,
                doc_anchor,
            } = mem
            else {
                continue;
            };
            if *is_static {
                continue;
            }
            if *computed {
                note(&mut problem, loc.start, "computed field name".into());
                continue;
            }
            if *accessor {
                note(&mut problem, loc.start, format!("auto-accessor `{name}`"));
                continue;
            }
            if *declare {
                note(
                    &mut problem,
                    loc.start,
                    format!("`declare` field `{name}` is not defined by the class (row 6)"),
                );
                continue;
            }
            let annotated = ty
                .clone()
                .or_else(|| self.doc_at(*doc_anchor).and_then(|d| d.ty));
            let declared = match annotated {
                Some(t) => self.resolve_in(&t, &tparams, Some(c.id)),
                None => match init.as_ref().and_then(literal_type) {
                    Some(t) => t.widen(),
                    None => {
                        note(
                            &mut problem,
                            loc.start,
                            format!("field `{name}` has no type"),
                        );
                        Type::Any
                    }
                },
            };
            if declared.contains_any() {
                note(&mut problem, loc.start, format!("field `{name}` is `any`"));
            }
            let declared = if *optional || *definite {
                // `x?: T` and `x!: T` (row 4) hold `undefined` until assigned.
                Type::union(vec![declared, Type::Undefined])
            } else {
                declared
            };
            fields.push(FieldInfo {
                name: name.clone(),
                kind_type: declared.clone(),
                declared,
                readonly: *readonly,
                loc: *loc,
            });
        }
        // Parameter properties need emit; the strip cannot run them.
        if let Some(ctor) = self.classes[c.id].ctor {
            if let Some(p) = m.funcs[ctor].params.iter().find(|p| p.property) {
                note(
                    &mut problem,
                    p.loc.start,
                    "parameter property needs emit (not erasable)".into(),
                );
            }
        }
        // JS: constructor `this.x = …` assignments define the layout (§4.5 "Fields").
        if self.js() {
            if let Some(ctor) = self.classes[c.id].ctor {
                if let Some(Body::Block(ss)) = &m.funcs[ctor].body {
                    for s in ss {
                        let Stmt::Expr(e) = s else { continue };
                        let ExprKind::Assign {
                            op: "=",
                            target,
                            value,
                        } = &e.kind
                        else {
                            continue;
                        };
                        let ExprKind::Member {
                            obj,
                            name,
                            name_loc,
                            ..
                        } = &target.kind
                        else {
                            continue;
                        };
                        if !matches!(obj.kind, ExprKind::This)
                            || fields.iter().any(|f| &f.name == name)
                        {
                            continue;
                        }
                        let declared = match self.doc_at(e.loc.start).and_then(|d| d.ty) {
                            Some(t) => self.resolve_in(&t, &tparams, Some(c.id)),
                            None => match literal_type(value) {
                                Some(t) => t.widen(),
                                None => {
                                    note(
                                        &mut problem,
                                        name_loc.start,
                                        format!("field `{name}` has no type"),
                                    );
                                    Type::Any
                                }
                            },
                        };
                        fields.push(FieldInfo {
                            name: name.clone(),
                            kind_type: declared.clone(),
                            declared,
                            readonly: false,
                            loc: *name_loc,
                        });
                    }
                }
            }
        }
        if self.opts.use_define_for_class_fields.eq(&false) {
            note(
                &mut problem,
                c.start,
                "useDefineForClassFields is off: class layouts are hints-only (§4.6)".into(),
            );
        }
        self.classes[c.id].fields = fields;
        self.classes[c.id].problem = problem;
    }

    /// Definite assignment (row 14) and override checks (row 12), then layout soundness.
    fn class_soundness(&mut self) {
        let m = self.m;
        let n = self.classes.len();
        // Definite assignment, parents first.
        let mut escapes = vec![None::<bool>; n];
        for i in 0..n {
            self.definite_assignment(i, &mut escapes);
        }
        for i in 0..n {
            // Row 12: fields invariant (readonly covariant); methods checked like functions.
            let mut problem = self.classes[i].problem.clone();
            if let Some(parent) = self.classes[i].parent {
                for f in &self.classes[i].fields {
                    if let Some(base) = self.field_in_chain(parent, &f.name) {
                        let ok = if base.readonly {
                            assignable_in(&f.declared, &base.declared, self)
                        } else {
                            equivalent_in(&f.declared, &base.declared, self)
                        };
                        if !ok && problem.is_none() {
                            problem = Some((
                                f.loc.start,
                                format!(
                                    "field `{}` overrides `{}` with `{}` (fields are invariant, row 12)",
                                    f.name, base.declared, f.declared
                                ),
                            ));
                        }
                    }
                }
                for (name, fid, kind) in &self.classes[i].methods {
                    if *kind != FnKind::Method {
                        continue;
                    }
                    if let Some((base_fid, _)) = self.method_in_chain(parent, name) {
                        let (sub, base) = (self.fn_type_of(*fid), self.fn_type_of(base_fid));
                        if !assignable_in(
                            &Type::Function(Box::new(sub.clone())),
                            &Type::Function(Box::new(base.clone())),
                            self,
                        ) && problem.is_none()
                        {
                            problem = Some((
                                m.funcs[*fid].start,
                                format!("method `{name}` overrides `{base}` with `{sub}` (row 12)"),
                            ));
                        }
                    }
                }
            }
            if let Some(r) = &self.file_reason {
                if problem.is_none() {
                    problem = Some(r.clone());
                }
            }
            self.classes[i].problem = problem;
        }
        // Layout soundness needs the parent's.
        for i in 0..n {
            let mut ok = true;
            let mut c = Some(i);
            while let Some(id) = c {
                if self.classes[id].problem.is_some() {
                    ok = false;
                    break;
                }
                c = self.classes[id].parent;
            }
            self.layout_sound[i] = ok;
        }
    }

    fn field_in_chain(&self, class: ClassId, name: &str) -> Option<&FieldInfo> {
        let mut c = Some(class);
        while let Some(id) = c {
            if let Some(f) = self.classes[id].fields.iter().find(|f| f.name == name) {
                return Some(f);
            }
            c = self.classes[id].parent;
        }
        None
    }

    fn method_in_chain(&self, class: ClassId, name: &str) -> Option<(FnId, ClassId)> {
        let mut c = Some(class);
        while let Some(id) = c {
            if let Some((_, fid, _)) = self.classes[id]
                .methods
                .iter()
                .find(|(n, _, k)| n == name && *k == FnKind::Method)
            {
                return Some((*fid, id));
            }
            c = self.classes[id].parent;
        }
        None
    }

    fn accessor_in_chain(&self, class: ClassId, name: &str, kind: FnKind) -> Option<FnId> {
        let mut c = Some(class);
        while let Some(id) = c {
            if let Some((_, fid, _)) = self.classes[id]
                .methods
                .iter()
                .find(|(n, _, k)| n == name && *k == kind)
            {
                return Some(*fid);
            }
            c = self.classes[id].parent;
        }
        None
    }

    /// Row 14. Returns whether `this` escapes anywhere during construction of class `i`.
    fn definite_assignment(&mut self, i: ClassId, memo: &mut Vec<Option<bool>>) -> bool {
        if let Some(e) = memo[i] {
            return e;
        }
        memo[i] = Some(true); // cycle guard
        let m = self.m;
        let node = &m.classes[i];
        let parent_escapes = match self.classes[i].parent {
            Some(p) => self.definite_assignment(p, memo),
            None => node.extends.is_some(),
        };
        let mut assigned: HashSet<String> = HashSet::new();
        let mut stopped = parent_escapes;
        let mut escaped_anywhere = parent_escapes;
        // Field initializers, in order.
        for mem in &node.members {
            if let Member::Field {
                name,
                init: Some(e),
                is_static: false,
                ..
            } = mem
            {
                if self.escapes_this(e, &assigned) {
                    stopped = true;
                    escaped_anywhere = true;
                } else if !stopped {
                    assigned.insert(name.clone());
                }
            }
        }
        if let Some(ctor) = self.classes[i].ctor {
            if let Some(Body::Block(ss)) = &m.funcs[ctor].body {
                let (esc, _) = self.da_stmts(ss, &mut assigned, &mut stopped);
                escaped_anywhere |= esc;
            }
        }
        for f in &mut self.classes[i].fields {
            if !assigned.contains(&f.name) && !f.declared.admits(&Type::Undefined) {
                f.kind_type = Type::union(vec![f.declared.clone(), Type::Undefined]);
            }
        }
        memo[i] = Some(escaped_anywhere);
        escaped_anywhere
    }

    /// Walks constructor statements: returns (escaped, stopped-by-return).
    fn da_stmts(
        &self,
        ss: &[Stmt],
        assigned: &mut HashSet<String>,
        stopped: &mut bool,
    ) -> (bool, bool) {
        let mut escaped = false;
        for s in ss {
            match s {
                Stmt::Expr(e) => match &e.kind {
                    ExprKind::Assign {
                        op: "=",
                        target,
                        value,
                    } if matches!(&target.kind, ExprKind::Member { obj, .. } if matches!(obj.kind, ExprKind::This)) =>
                    {
                        let ExprKind::Member { name, .. } = &target.kind else {
                            unreachable!()
                        };
                        if self.escapes_this(value, assigned) {
                            escaped = true;
                            *stopped = true;
                        } else if !*stopped {
                            assigned.insert(name.clone());
                        }
                    }
                    ExprKind::Call { callee, args, .. }
                        if matches!(callee.kind, ExprKind::Super) =>
                    {
                        if args.iter().any(|a| self.mentions_this(a)) {
                            escaped = true;
                            *stopped = true;
                        }
                    }
                    _ => {
                        if self.mentions_this(e) {
                            escaped = true;
                            *stopped = true;
                        }
                    }
                },
                Stmt::If {
                    test, cons, alt, ..
                } => {
                    if self.mentions_this(test) {
                        escaped = true;
                        *stopped = true;
                    }
                    let mut a = assigned.clone();
                    let mut b = assigned.clone();
                    let (mut sa, mut sb) = (*stopped, *stopped);
                    let (ea, _) = self.da_stmts(std::slice::from_ref(cons), &mut a, &mut sa);
                    let (eb, _) = match alt {
                        Some(alt) => self.da_stmts(std::slice::from_ref(alt), &mut b, &mut sb),
                        None => (false, false),
                    };
                    escaped |= ea || eb;
                    *stopped |= sa || sb;
                    let both: HashSet<String> = a.intersection(&b).cloned().collect();
                    *assigned = both;
                }
                Stmt::Block(inner, _) => {
                    let (e, _) = self.da_stmts(inner, assigned, stopped);
                    escaped |= e;
                }
                Stmt::Return(..) | Stmt::Throw(..) => {
                    *stopped = true;
                    let mut any_this = false;
                    Walk {
                        m: self.m,
                        into_arrows: true,
                        into_fns: false,
                    }
                    .stmt(s, &mut |e| any_this |= matches!(e.kind, ExprKind::This));
                    escaped |= any_this;
                }
                other => {
                    let mut any_this = false;
                    Walk {
                        m: self.m,
                        into_arrows: true,
                        into_fns: false,
                    }
                    .stmt(other, &mut |e| any_this |= matches!(e.kind, ExprKind::This));
                    // Loops and other control flow: assignments inside do not count.
                    if any_this {
                        escaped = true;
                        *stopped = true;
                    }
                }
            }
        }
        (escaped, *stopped)
    }

    fn mentions_this(&self, e: &Expr) -> bool {
        let mut found = false;
        Walk {
            m: self.m,
            into_arrows: true,
            into_fns: false,
        }
        .expr(e, &mut |x| {
            found |= matches!(x.kind, ExprKind::This | ExprKind::Super)
        });
        found
    }

    /// Whether evaluating `e` can let `this` be observed before all fields are set: any use of
    /// `this` except reads of already-assigned fields and arrow functions that are merely
    /// stored.
    fn escapes_this(&self, e: &Expr, assigned: &HashSet<String>) -> bool {
        match &e.kind {
            ExprKind::This | ExprKind::Super => true,
            ExprKind::Member { obj, name, .. } if matches!(obj.kind, ExprKind::This) => {
                !assigned.contains(name)
            }
            ExprKind::Func(_) => false,
            _ => {
                let mut esc = false;
                for_each_child(self.m, e, &mut |c| esc |= self.escapes_this(c, assigned));
                esc
            }
        }
    }

    // ----- signatures --------------------------------------------------------------------

    fn contextual_pass(&mut self) {
        let m = self.m;
        let mut ctx: Vec<(FnId, Type, Vec<String>, Option<ClassId>)> = Vec::new();
        let mut names: Vec<(FnId, String)> = Vec::new();
        let tparams_of = |fid: Option<FnId>| -> Vec<String> {
            let mut out = Vec::new();
            let mut f = fid;
            while let Some(id) = f {
                out.extend(m.funcs[id].type_params.iter().map(|t| t.name.clone()));
                f = m.funcs[id].parent;
            }
            out
        };
        let func_of = |e: &Expr| -> Option<FnId> {
            let mut e = e;
            while let ExprKind::Paren(inner) = &e.kind {
                e = inner;
            }
            match &e.kind {
                ExprKind::Func(id) => Some(*id),
                _ => None,
            }
        };
        // Variable declarations anywhere.
        let visit_stmt = |s: &Stmt, owner: Option<FnId>, ctx: &mut Vec<_>, names: &mut Vec<_>| {
            if let Stmt::Var { decls, loc, .. } = s {
                let doc_ty = self.doc_at(loc.start).and_then(|d| d.ty);
                for d in decls {
                    let (Pattern::Ident { name, .. }, Some(init)) = (&d.pat, &d.init) else {
                        continue;
                    };
                    if let Some(fid) = func_of(init) {
                        names.push((fid, name.clone()));
                        if let Some(t) = d.ty.clone().or_else(|| doc_ty.clone()) {
                            ctx.push((fid, t, tparams_of(owner), None));
                        }
                    }
                }
            }
        };
        fn stmts_rec(ss: &[Stmt], owner: Option<FnId>, f: &mut dyn FnMut(&Stmt, Option<FnId>)) {
            for s in ss {
                f(s, owner);
                match s {
                    Stmt::Export { stmt, .. } => stmts_rec(std::slice::from_ref(stmt), owner, f),
                    Stmt::Block(b, _) => stmts_rec(b, owner, f),
                    Stmt::If { cons, alt, .. } => {
                        stmts_rec(std::slice::from_ref(cons), owner, f);
                        if let Some(a) = alt {
                            stmts_rec(std::slice::from_ref(a), owner, f);
                        }
                    }
                    Stmt::For { init, body, .. } => {
                        if let Some(i) = init {
                            stmts_rec(std::slice::from_ref(i), owner, f);
                        }
                        stmts_rec(std::slice::from_ref(body), owner, f);
                    }
                    Stmt::ForIn { body, .. }
                    | Stmt::While { body, .. }
                    | Stmt::DoWhile { body, .. }
                    | Stmt::Labeled { body, .. } => stmts_rec(std::slice::from_ref(body), owner, f),
                    Stmt::Try {
                        block,
                        handler,
                        finalizer,
                        ..
                    } => {
                        stmts_rec(block, owner, f);
                        if let Some(h) = handler {
                            stmts_rec(h, owner, f);
                        }
                        if let Some(h) = finalizer {
                            stmts_rec(h, owner, f);
                        }
                    }
                    Stmt::Switch { cases, .. } => {
                        for (_, b) in cases {
                            stmts_rec(b, owner, f);
                        }
                    }
                    _ => {}
                }
            }
        }
        stmts_rec(&m.stmts, None, &mut |s, o| {
            visit_stmt(s, o, &mut ctx, &mut names)
        });
        for f in &m.funcs {
            if let Some(Body::Block(ss)) = &f.body {
                stmts_rec(ss, Some(f.id), &mut |s, o| {
                    visit_stmt(s, o, &mut ctx, &mut names)
                });
            }
        }
        // Class fields initialized with a function: `handler: (x: number) => void = (x) => …`.
        for c in &m.classes {
            for mem in &c.members {
                if let Member::Field {
                    ty,
                    init: Some(init),
                    doc_anchor,
                    name,
                    ..
                } = mem
                {
                    if let Some(fid) = func_of(init) {
                        names.push((
                            fid,
                            format!("{}.{name}", c.name.as_deref().unwrap_or("<class>")),
                        ));
                        if let Some(t) = ty
                            .clone()
                            .or_else(|| self.doc_at(*doc_anchor).and_then(|d| d.ty))
                        {
                            ctx.push((
                                fid,
                                t,
                                c.type_params.iter().map(|t| t.name.clone()).collect(),
                                Some(c.id),
                            ));
                        }
                    }
                }
            }
        }
        // JS: `/** @type {(x: number) => number} */ function f(x) {}`.
        if self.js() {
            for f in &m.funcs {
                if let Some(t) = self.docs[f.id].ty.clone() {
                    ctx.push((f.id, t, tparams_of(f.parent), None));
                }
            }
        }
        for (fid, t, tps, class) in ctx {
            if let Type::Function(ft) = self.resolve_in(&t, &tps, class) {
                self.contextual.insert(fid, *ft);
            }
        }
        self.display.extend(names);
    }

    fn compute_sig(&self, id: FnId) -> Sig {
        let m = self.m;
        let f = &m.funcs[id];
        let doc = &self.docs[id];
        let ctx = self.contextual.get(&id);
        let class = f.class;
        let mut tparams: Vec<String> = Vec::new();
        let mut a = Some(id);
        while let Some(x) = a {
            tparams.extend(m.funcs[x].type_params.iter().map(|t| t.name.clone()));
            tparams.extend(self.docs[x].templates.iter().map(|t| t.name.clone()));
            if let Some(c) = m.funcs[x].class {
                tparams.extend(self.classes[c].tparams.iter().cloned());
            }
            a = m.funcs[x].parent;
        }
        let mut problem: Option<(u32, String)> = None;
        let note = |p: &mut Option<(u32, String)>, at: u32, msg: String| {
            if p.is_none() {
                *p = Some((at, msg));
            }
        };
        if let Some(e) = doc.errors.first() {
            note(&mut problem, f.start, format!("unparsable JSDoc type {e}"));
        }
        let mut params = Vec::new();
        for (i, p) in f.params.iter().enumerate() {
            let name = match &p.pat {
                Pattern::Ident { name, .. } => name.clone(),
                Pattern::Destructure { .. } => {
                    note(
                        &mut problem,
                        p.loc.start,
                        "destructured parameter (not in the v1 subset)".into(),
                    );
                    "_".into()
                }
            };
            if p.property {
                note(
                    &mut problem,
                    p.loc.start,
                    "parameter property needs emit (not erasable)".into(),
                );
            }
            let tag = doc.params.iter().find(|t| t.name == name);
            let raw =
                p.ty.clone()
                    .or_else(|| tag.map(|t| t.ty.clone()))
                    .map(|t| (t, false))
                    .or_else(|| {
                        ctx.and_then(|c| c.params.get(i))
                            .map(|q| (q.ty.clone(), true))
                    });
            let (ty, optional) = match raw {
                Some((t, resolved)) => {
                    let t = if resolved {
                        t
                    } else {
                        self.resolve_in(&t, &tparams, class)
                    };
                    let t = if p.rest && tag.is_some_and(|t| t.rest) {
                        Type::Array(Box::new(t))
                    } else {
                        t
                    };
                    (t, p.optional || tag.is_some_and(|t| t.optional))
                }
                None => {
                    note(
                        &mut problem,
                        p.loc.start,
                        format!("parameter `{name}` has no type (implicit any)"),
                    );
                    (Type::Any, p.optional)
                }
            };
            if ty.contains_any() {
                note(
                    &mut problem,
                    p.loc.start,
                    format!("parameter `{name}` is `any` (row 1)"),
                );
            }
            params.push(SigParam {
                name,
                ty,
                optional,
                rest: p.rest,
                has_default: p.default.is_some(),
                loc: p.loc,
            });
        }
        let this = f
            .this_type
            .clone()
            .or_else(|| doc.this.clone())
            .map(|t| self.resolve_in(&t, &tparams, class))
            .or_else(|| ctx.and_then(|c| c.this.clone()));
        if this.as_ref().is_some_and(Type::contains_any) {
            note(&mut problem, f.start, "`this` is `any` (row 1)".into());
        }
        let ret = f
            .ret
            .clone()
            .or_else(|| doc.returns.clone())
            .map(|t| self.resolve_in(&t, &tparams, class))
            .or_else(|| {
                ctx.map(|c| c.ret.clone())
                    .filter(|r| !matches!(r, Type::Void) || f.kind != FnKind::Arrow || true)
            });
        if ret.as_ref().is_some_and(Type::contains_any) {
            note(&mut problem, f.start, "return type is `any` (row 1)".into());
        }
        Sig {
            params,
            this,
            ret,
            predicate: f.predicate.clone(),
            tparams,
            problem,
        }
    }

    /// The function's type as seen from outside (unannotated parts are `any`).
    fn fn_type_of(&self, id: FnId) -> FnType {
        let sig = &self.sigs[id];
        FnType {
            type_params: Vec::new(),
            this: sig.this.clone(),
            params: sig
                .params
                .iter()
                .map(|p| Param {
                    name: p.name.clone(),
                    ty: p.ty.clone(),
                    optional: p.optional || p.has_default,
                    rest: p.rest,
                })
                .collect(),
            ret: sig.ret.clone().unwrap_or(Type::External),
            predicate: sig.predicate.clone(),
            construct: false,
        }
    }

    /// The table key of a function: its `FnSource::Range.start` (the class start for a
    /// constructor).
    pub(crate) fn key_of(&self, id: FnId) -> u32 {
        let f = &self.m.funcs[id];
        match (f.kind, f.class) {
            (FnKind::Constructor, Some(c)) => self.m.classes[c].start,
            _ => f.start,
        }
    }

    // ----- projection --------------------------------------------------------------------

    fn intern_sig(&self, s: Signature) -> u32 {
        let mut ids = self.sig_ids.borrow_mut();
        if let Some(i) = ids.iter().position(|x| *x == s) {
            return i as u32;
        }
        ids.push(s);
        (ids.len() - 1) as u32
    }

    pub(crate) fn project(&self, t: &Type) -> TKind {
        match t {
            Type::Number | Type::NumberLiteral(_) => TKind::Num,
            Type::Boolean | Type::BooleanLiteral(_) => TKind::Bool,
            Type::String | Type::StringLiteral(_) => TKind::Str,
            Type::BigInt | Type::BigIntLiteral(_) => TKind::BigInt,
            Type::Undefined => TKind::Undef,
            Type::Null => TKind::Null,
            Type::Symbol => TKind::Sym,
            Type::Instance(c) => {
                if self.layout_sound.get(*c as usize).copied().unwrap_or(false) {
                    TKind::Class(*c)
                } else {
                    TKind::Object
                }
            }
            Type::Array(e) | Type::ReadonlyArray(e) => match self.project(e) {
                TKind::Num => TKind::NumArray,
                k => TKind::Array(Box::new(k)),
            },
            Type::Tuple(ts) => self.project(&Type::Array(Box::new(Type::union(ts.clone())))),
            Type::Function(f) => {
                let params = f
                    .params
                    .iter()
                    .map(|p| self.project(&FnType::param_type(p)))
                    .collect();
                let ret = self.project(&f.ret);
                TKind::Func(self.intern_sig(Signature { params, ret }))
            }
            Type::Object(_) | Type::NonPrimitive => TKind::Object,
            Type::Union(ms) => self.project_union(ms),
            _ => TKind::Any,
        }
    }

    fn project_union(&self, ms: &[Type]) -> TKind {
        let mut mask = 0u16;
        let mut classes = Vec::new();
        for m in ms {
            match self.project(m) {
                TKind::Any => return TKind::Any,
                TKind::Num => mask |= tag::NUM,
                TKind::Bool => mask |= tag::BOOL,
                TKind::Str => mask |= tag::STR,
                TKind::BigInt => mask |= tag::BIGINT,
                TKind::Undef => mask |= tag::UNDEFINED,
                TKind::Null => mask |= tag::NULL,
                TKind::Sym => mask |= tag::SYM,
                TKind::Tags(t) => mask |= t,
                TKind::Class(c) => classes.push(c),
                _ => mask |= tag::OBJ,
            }
        }
        classes.dedup();
        match (classes.len(), mask & tag::OBJ) {
            (1, 0) if mask == tag::NULL => TKind::NullableClass(classes[0]),
            (1, 0) if mask != 0 => TKind::ClassOr(classes[0], mask),
            (1, 0) => TKind::Class(classes[0]),
            (0, _) => match mask {
                tag::NUM => TKind::Num,
                tag::BOOL => TKind::Bool,
                tag::STR => TKind::Str,
                tag::BIGINT => TKind::BigInt,
                tag::UNDEFINED => TKind::Undef,
                tag::NULL => TKind::Null,
                tag::SYM => TKind::Sym,
                tag::OBJ => TKind::Object,
                m => TKind::Tags(m),
            },
            _ => TKind::Tags(mask | tag::OBJ),
        }
    }

    fn assignable(&self, s: &Type, t: &Type) -> bool {
        if is_dynamic(t) {
            return true;
        }
        assignable_in(s, t, self)
    }

    // ----- verdicts ----------------------------------------------------------------------

    fn precheck(&self, id: FnId) -> R<()> {
        let f = &self.m.funcs[id];
        if let Some((at, r)) = &self.file_reason {
            return fail(if *at == 0 { f.start } else { *at }, r.clone());
        }
        if f.is_async {
            return fail(f.start, "async function (hints only, row 24)");
        }
        if f.is_generator {
            return fail(f.start, "generator (hints only, row 24)");
        }
        let line_after = |off: u32| -> u32 {
            let rest = &self.src[off as usize..];
            let nl = rest
                .find('\n')
                .map(|i| off as usize + i + 1)
                .unwrap_or(self.src.len());
            let nl2 = self.src[nl..]
                .find('\n')
                .map(|i| nl + i)
                .unwrap_or(self.src.len());
            nl2 as u32
        };
        for &c in &self.ignores {
            // The innermost function containing the directive, or one starting on the next line.
            let inside = f.loc.start <= c && c < f.end && self.innermost_fn(c) == Some(id);
            let next_line = c < f.loc.start && f.loc.start < line_after(c);
            if inside || next_line {
                return fail(c, "suppressed by @ts-ignore / @ts-expect-error (row 17)");
            }
        }
        if let Some((at, r)) = &self.sigs[id].problem {
            return fail(*at, r.clone());
        }
        Ok(())
    }

    fn innermost_fn(&self, off: u32) -> Option<FnId> {
        self.m
            .funcs
            .iter()
            .filter(|f| f.body.is_some() && f.loc.start <= off && off < f.end)
            .min_by_key(|f| f.end - f.loc.start)
            .map(|f| f.id)
    }

    fn check_fn(&self, id: FnId, final_pass: bool) -> Result<CheckOut, Unsound> {
        self.precheck(id)?;
        let mut c = Checker::new(self, id, final_pass);
        c.run()?;
        Ok(CheckOut {
            locals: c.locals,
            sites: c.sites,
            ret: c.inferred_ret,
        })
    }
}

struct CheckOut {
    locals: Vec<(u32, TKind)>,
    sites: Vec<Site>,
    ret: Option<Type>,
}

/// A primitive literal initializer's type (for trusted module `const`s and field inference).
fn literal_type(e: &Expr) -> Option<Type> {
    match &e.kind {
        ExprKind::Num(_) => Some(Type::Number),
        ExprKind::Str(_) => Some(Type::String),
        ExprKind::Bool(_) => Some(Type::Boolean),
        ExprKind::BigInt(_) => Some(Type::BigInt),
        ExprKind::Template(parts) if parts.is_empty() => Some(Type::String),
        ExprKind::Unary { op: "-" | "+", arg } if matches!(arg.kind, ExprKind::Num(_)) => {
            Some(Type::Number)
        }
        ExprKind::Paren(inner) => literal_type(inner),
        _ => None,
    }
}

fn subst(t: &Type, map: &HashMap<String, Type>) -> Type {
    if map.is_empty() {
        return t.clone();
    }
    match t {
        Type::Reference { name, arguments } if arguments.is_empty() => {
            map.get(name).cloned().unwrap_or_else(|| t.clone())
        }
        Type::Reference { name, arguments } => Type::Reference {
            name: name.clone(),
            arguments: arguments.iter().map(|a| subst(a, map)).collect(),
        },
        Type::Array(e) => Type::Array(Box::new(subst(e, map))),
        Type::ReadonlyArray(e) => Type::ReadonlyArray(Box::new(subst(e, map))),
        Type::Tuple(ts) => Type::Tuple(ts.iter().map(|x| subst(x, map)).collect()),
        Type::Union(ts) => Type::Union(ts.iter().map(|x| subst(x, map)).collect()),
        Type::Intersection(ts) => Type::Intersection(ts.iter().map(|x| subst(x, map)).collect()),
        Type::Object(o) => Type::Object(ObjectType {
            props: o
                .props
                .iter()
                .map(|p| Property {
                    ty: subst(&p.ty, map),
                    ..p.clone()
                })
                .collect(),
            index: o
                .index
                .iter()
                .map(|i| IndexSignature {
                    key: subst(&i.key, map),
                    value: subst(&i.value, map),
                    readonly: i.readonly,
                })
                .collect(),
            calls: o.calls.iter().map(|f| subst_fn(f, map)).collect(),
            constructs: o.constructs.iter().map(|f| subst_fn(f, map)).collect(),
        }),
        Type::Function(f) => Type::Function(Box::new(subst_fn(f, map))),
        t => t.clone(),
    }
}

fn subst_fn(f: &FnType, map: &HashMap<String, Type>) -> FnType {
    let mut inner = map.clone();
    for tp in &f.type_params {
        inner.remove(&tp.name);
    }
    FnType {
        params: f
            .params
            .iter()
            .map(|p| Param {
                ty: subst(&p.ty, &inner),
                ..p.clone()
            })
            .collect(),
        ret: subst(&f.ret, &inner),
        this: f.this.as_ref().map(|t| subst(t, &inner)),
        ..f.clone()
    }
}

// ----- the per-function checker ------------------------------------------------------------

#[derive(Clone)]
struct Local {
    declared: Type,
    narrowed: Option<Type>,
    is_const: bool,
    /// Written by a nested function (or a captured `this`): never narrowed; reads are checked.
    untrusted: bool,
    /// Bound to a known function whose identity is stable in this scope.
    func: Option<FnId>,
    class: Option<ClassId>,
}

type Snapshot = Vec<HashMap<String, Option<Type>>>;

struct Checker<'p, 'm> {
    p: &'p Program<'m>,
    id: FnId,
    f: &'m Func,
    scopes: Vec<HashMap<String, Local>>,
    sites: Vec<Site>,
    locals: Vec<(u32, TKind)>,
    returns: Vec<Type>,
    inferred_ret: Option<Type>,
    this_ty: Option<Type>,
    this_untrusted: bool,
    closure_written: &'p HashSet<String>,
    closure_unknown: bool,
    /// Names assigned anywhere in this function (including nested functions).
    assigned_here: HashSet<String>,
    final_pass: bool,
}

impl<'p, 'm> Checker<'p, 'm> {
    fn new(p: &'p Program<'m>, id: FnId, final_pass: bool) -> Self {
        let f = &p.m.funcs[id];
        let (own, _) = walk::own_assigned(p.m, id);
        let mut assigned_here: HashSet<String> = own.into_iter().collect();
        assigned_here.extend(p.closure_written[id].iter().cloned());
        let sig = &p.sigs[id];
        // `this`: explicit annotation, the class for methods, the enclosing method for arrows.
        let mut this_ty = sig.this.clone();
        let mut this_untrusted = false;
        if this_ty.is_none() {
            let mut g = Some(id);
            while let Some(gid) = g {
                let gf = &p.m.funcs[gid];
                if gf.kind == FnKind::Arrow {
                    g = gf.parent;
                    continue;
                }
                this_ty = match (gf.class, gf.is_static) {
                    (Some(c), false) => Some(Type::Instance(c as u32)),
                    (Some(_), true) => Some(Type::External),
                    (None, _) => p.sigs[gid].this.clone(),
                };
                if gid != id || gf.kind == FnKind::Constructor {
                    this_untrusted = true;
                }
                break;
            }
            // Arrows at class-field level (`f = () => this.x`) have no enclosing method.
            if this_ty.is_none() && f.kind == FnKind::Arrow {
                this_ty = Some(Type::External);
            }
            if matches!(f.kind, FnKind::Method | FnKind::Getter | FnKind::Setter)
                && f.class.is_none()
            {
                // Object-literal methods: `this` is whatever the call site provides.
                this_ty = Some(Type::External);
            }
        }
        Checker {
            p,
            id,
            f,
            scopes: vec![HashMap::new()],
            sites: Vec::new(),
            locals: Vec::new(),
            returns: Vec::new(),
            inferred_ret: None,
            this_ty,
            this_untrusted,
            closure_written: &p.closure_written[id],
            closure_unknown: p.closure_unknown[id],
            assigned_here,
            final_pass,
        }
    }

    fn sig(&self) -> &'p Sig {
        &self.p.sigs[self.id]
    }

    fn run(&mut self) -> R<()> {
        let sig = self.sig();
        // Parameters.
        for (sp, pn) in sig.params.iter().zip(&self.f.params) {
            let declared = if sp.rest {
                sp.ty.clone()
            } else if sp.optional && !sp.has_default {
                Type::union(vec![sp.ty.clone(), Type::Undefined])
            } else {
                sp.ty.clone()
            };
            if let Some(d) = &pn.default {
                self.expect(d, &sp.ty)?;
            }
            self.declare(&sp.name, sp.loc, declared, false, None, None);
        }
        match &self.f.body {
            Some(Body::Block(ss)) => {
                self.hoist(ss);
                let falls = self.block_stmts(ss)?;
                if falls {
                    if let Some(r) = &sig.ret {
                        if !r.admits(&Type::Undefined) && !matches!(r, Type::Never) {
                            return fail(
                                self.f.end.saturating_sub(1),
                                format!("function can end without returning `{r}`"),
                            );
                        }
                    }
                    self.returns.push(Type::Undefined);
                }
            }
            Some(Body::Expr(e)) => match &sig.ret {
                Some(r) => {
                    let r = r.clone();
                    self.expect(e, &r)?;
                }
                None => {
                    let t = self.expr(e)?;
                    self.returns.push(t);
                }
            },
            None => {}
        }
        if sig.ret.is_none() {
            let t = Type::union(self.returns.iter().map(Type::widen).collect());
            if t.contains_any() {
                return fail(self.f.start, "inferred return type contains `any`");
            }
            self.inferred_ret = Some(t);
        }
        Ok(())
    }

    fn declare(
        &mut self,
        name: &str,
        loc: Loc,
        declared: Type,
        is_const: bool,
        func: Option<FnId>,
        class: Option<ClassId>,
    ) {
        let untrusted = self.closure_unknown || self.closure_written.contains(name);
        self.locals.push((loc.start, self.p.project(&declared)));
        self.scopes.last_mut().unwrap().insert(
            name.to_string(),
            Local {
                declared,
                narrowed: None,
                is_const,
                untrusted,
                func,
                class,
            },
        );
    }

    fn hoist(&mut self, ss: &[Stmt]) {
        for s in ss {
            let s = match s {
                Stmt::Export { stmt, .. } => stmt,
                s => s,
            };
            match s {
                Stmt::Func(fid) => {
                    let f = &self.p.m.funcs[*fid];
                    if let Some(n) = &f.name {
                        if f.body.is_none() {
                            continue;
                        }
                        let stable = !self.assigned_here.contains(n);
                        let t = Type::Function(Box::new(self.p.fn_type_of(*fid)));
                        self.scopes.last_mut().unwrap().insert(
                            n.clone(),
                            Local {
                                declared: t,
                                narrowed: None,
                                is_const: false,
                                untrusted: false,
                                func: stable.then_some(*fid),
                                class: None,
                            },
                        );
                    }
                }
                Stmt::Class(cid) => {
                    if let Some(n) = &self.p.m.classes[*cid].name {
                        self.scopes.last_mut().unwrap().insert(
                            n.clone(),
                            Local {
                                declared: Type::Opaque("class".into()),
                                narrowed: None,
                                is_const: true,
                                untrusted: false,
                                func: None,
                                class: Some(*cid),
                            },
                        );
                    }
                }
                _ => {}
            }
        }
    }

    fn lookup(&self, name: &str) -> Option<&Local> {
        self.scopes.iter().rev().find_map(|s| s.get(name))
    }

    fn lookup_mut(&mut self, name: &str) -> Option<&mut Local> {
        self.scopes.iter_mut().rev().find_map(|s| s.get_mut(name))
    }

    fn site(&mut self, loc: Loc, fact: SiteFact) {
        self.sites.push(Site {
            start: loc.start,
            end: loc.end,
            fact,
        });
    }

    fn check(&mut self, loc: Loc, k: TKind) {
        if k != TKind::Any {
            self.site(loc, SiteFact::Check(k));
        }
    }

    /// An untrusted read of a value declared `t`: checked against its projection.
    fn untrusted(&mut self, loc: Loc, t: Type) -> Type {
        let k = self.p.project(&t);
        if k == TKind::Any && !is_dynamic(&t) && !matches!(t, Type::Unknown) {
            return Type::External;
        }
        self.check(loc, k);
        t
    }

    // ----- flow ------------------------------------------------------------------------

    fn flow(&mut self, s: &Type, t: &Type, loc: Loc) -> R<()> {
        if self.p.assignable(s, t) {
            return Ok(());
        }
        let members = s.members();
        let has_dynamic = members.iter().any(|m| is_dynamic(m));
        if has_dynamic
            && members
                .iter()
                .all(|m| is_dynamic(m) || self.p.assignable(m, t))
        {
            let k = self.p.project(t);
            self.check(loc, k);
            return Ok(());
        }
        if s.contains_any() {
            return fail(
                loc.start,
                format!("an `any` value flows into `{t}` (row 1)"),
            );
        }
        if let (Type::Array(a), Type::Array(b)) = (s, t) {
            if self.p.assignable(a, b) {
                return fail(
                    loc.start,
                    format!("mutable array `{s}` used as `{t}`: arrays are invariant (row 8)"),
                );
            }
        }
        fail(loc.start, format!("`{s}` is not assignable to `{t}`"))
    }

    fn expect(&mut self, e: &Expr, t: &Type) -> R<()> {
        match (&e.kind, t) {
            (ExprKind::Paren(inner), _) => self.expect(inner, t),
            (ExprKind::Array(items), Type::Array(el) | Type::ReadonlyArray(el)) => {
                for it in items {
                    match it {
                        None => self.flow(&Type::Undefined, el, e.loc)?,
                        Some(x) if matches!(x.kind, ExprKind::Spread(_)) => {
                            return fail(x.loc.start, "spread element (not in the v1 subset)")
                        }
                        Some(x) => self.expect(x, el)?,
                    }
                }
                Ok(())
            }
            (ExprKind::Array(items), Type::Tuple(ts)) if items.len() == ts.len() => {
                for (it, el) in items.iter().zip(ts) {
                    match it {
                        None => self.flow(&Type::Undefined, el, e.loc)?,
                        Some(x) => self.expect(x, el)?,
                    }
                }
                Ok(())
            }
            (ExprKind::Object(props), Type::Object(o)) => {
                let mut seen = Vec::new();
                for prop in props {
                    match prop {
                        Prop::KeyValue { key, value } => {
                            let Some(name) = key.static_name() else {
                                return fail(e.loc.start, "computed key (not in the v1 subset)");
                            };
                            match o.props.iter().find(|p| p.name == name) {
                                Some(p) => self.expect(value, &p.ty)?,
                                None => match o.index.first() {
                                    Some(ix) => {
                                        let v = ix.value.clone();
                                        self.expect(value, &v)?
                                    }
                                    None => {
                                        self.expr(value)?;
                                    }
                                },
                            }
                            seen.push(name);
                        }
                        Prop::Shorthand { name, loc } => {
                            let ident = Expr {
                                kind: ExprKind::Ident(name.clone()),
                                loc: *loc,
                            };
                            match o.props.iter().find(|p| &p.name == name) {
                                Some(p) => {
                                    let pt = p.ty.clone();
                                    self.expect(&ident, &pt)?
                                }
                                None => {
                                    self.expr(&ident)?;
                                }
                            }
                            seen.push(name.clone());
                        }
                        Prop::Method { key, func } => {
                            let name = key.static_name().unwrap_or_default();
                            let ft = Type::Function(Box::new(self.p.fn_type_of(*func)));
                            if let Some(p) = o.props.iter().find(|p| p.name == name) {
                                let pt = p.ty.clone();
                                self.flow(&ft, &pt, e.loc)?;
                            }
                            seen.push(name);
                        }
                        Prop::Spread(x) => {
                            return fail(x.loc.start, "object spread (not in the v1 subset)")
                        }
                    }
                }
                if let Some(missing) = o
                    .props
                    .iter()
                    .find(|p| !p.optional && !seen.contains(&p.name))
                {
                    return fail(
                        e.loc.start,
                        format!("object literal is missing `{}`", missing.name),
                    );
                }
                Ok(())
            }
            (ExprKind::Cond { test, cons, alt }, _) => {
                self.expr(test)?;
                let snap = self.snapshot();
                let r = self.refine(test, true)?;
                self.apply(r);
                self.expect(cons, t)?;
                self.restore(&snap);
                let r = self.refine(test, false)?;
                self.apply(r);
                self.expect(alt, t)?;
                self.restore(&snap);
                Ok(())
            }
            _ => {
                let s = self.expr(e)?;
                self.flow(&s, t, e.loc)
            }
        }
    }

    // ----- narrowing ---------------------------------------------------------------------

    fn snapshot(&self) -> Snapshot {
        self.scopes
            .iter()
            .map(|s| {
                s.iter()
                    .map(|(k, v)| (k.clone(), v.narrowed.clone()))
                    .collect()
            })
            .collect()
    }

    fn restore(&mut self, snap: &Snapshot) {
        for (scope, saved) in self.scopes.iter_mut().zip(snap) {
            for (k, v) in scope.iter_mut() {
                if let Some(n) = saved.get(k) {
                    v.narrowed = n.clone();
                }
            }
        }
    }

    fn merge(&mut self, a: &Snapshot, b: &Snapshot) {
        for (i, scope) in self.scopes.iter_mut().enumerate() {
            let (Some(sa), Some(sb)) = (a.get(i), b.get(i)) else {
                continue;
            };
            for (k, v) in scope.iter_mut() {
                let (Some(na), Some(nb)) = (sa.get(k), sb.get(k)) else {
                    continue;
                };
                v.narrowed = if na == nb {
                    na.clone()
                } else {
                    let ta = na.clone().unwrap_or_else(|| v.declared.clone());
                    let tb = nb.clone().unwrap_or_else(|| v.declared.clone());
                    let u = Type::union(vec![ta, tb]);
                    if u == v.declared {
                        None
                    } else {
                        Some(u)
                    }
                };
            }
        }
    }

    fn invalidate(&mut self, names: &HashSet<String>) {
        for scope in &mut self.scopes {
            for (k, v) in scope.iter_mut() {
                if names.contains(k) {
                    v.narrowed = None;
                }
            }
        }
    }

    fn assigned_in(&self, s: &Stmt) -> HashSet<String> {
        let mut names = Vec::new();
        let mut unk = false;
        Walk {
            m: self.p.m,
            into_arrows: false,
            into_fns: false,
        }
        .stmt(s, &mut |e| walk::collect_assigned(e, &mut names, &mut unk));
        let mut out: HashSet<String> = names.into_iter().collect();
        if unk {
            for scope in &self.scopes {
                out.extend(scope.keys().cloned());
            }
        }
        out
    }

    fn assigned_in_expr(&self, e: &Expr) -> HashSet<String> {
        let mut names = Vec::new();
        let mut unk = false;
        Walk {
            m: self.p.m,
            into_arrows: false,
            into_fns: false,
        }
        .expr(e, &mut |x| walk::collect_assigned(x, &mut names, &mut unk));
        names.into_iter().collect()
    }

    fn current(&self, name: &str) -> Option<Type> {
        let l = self.lookup(name)?;
        if l.untrusted {
            return None;
        }
        Some(l.narrowed.clone().unwrap_or_else(|| l.declared.clone()))
    }

    fn apply(&mut self, refs: Vec<(String, Type)>) {
        for (name, t) in refs {
            if let Some(l) = self.lookup_mut(&name) {
                if !l.untrusted {
                    l.narrowed = if t == l.declared { None } else { Some(t) };
                }
            }
        }
    }

    fn narrowable_ident<'e>(&self, e: &'e Expr) -> Option<&'e str> {
        let mut e = e;
        while let ExprKind::Paren(inner) = &e.kind {
            e = inner;
        }
        match &e.kind {
            ExprKind::Ident(n) if self.lookup(n).is_some_and(|l| !l.untrusted) => Some(n),
            _ => None,
        }
    }

    /// Refinements implied by `cond` evaluating truthy (or falsy).
    fn refine(&mut self, cond: &Expr, truthy: bool) -> R<Vec<(String, Type)>> {
        let mut out = Vec::new();
        match &cond.kind {
            ExprKind::Paren(inner) => return self.refine(inner, truthy),
            ExprKind::Unary { op: "!", arg } => return self.refine(arg, !truthy),
            ExprKind::Binary {
                op: "&&",
                left,
                right,
            } => {
                if truthy {
                    let a = self.refine(left, true)?;
                    let snap = self.snapshot();
                    self.apply(a.clone());
                    let b = self.refine(right, true)?;
                    self.restore(&snap);
                    out.extend(a);
                    out.extend(b);
                }
            }
            ExprKind::Binary {
                op: "||",
                left,
                right,
            } => {
                if !truthy {
                    let a = self.refine(left, false)?;
                    let snap = self.snapshot();
                    self.apply(a.clone());
                    let b = self.refine(right, false)?;
                    self.restore(&snap);
                    out.extend(a);
                    out.extend(b);
                }
            }
            ExprKind::Binary {
                op: op @ ("===" | "!==" | "==" | "!="),
                left,
                right,
            } => {
                let positive = (*op == "===" || *op == "==") == truthy;
                let loose = *op == "==" || *op == "!=";
                // typeof x === "…"
                fn typeof_pair<'a>(a: &'a Expr, b: &'a Expr) -> Option<(&'a Expr, String)> {
                    match (&a.kind, &b.kind) {
                        (ExprKind::Unary { op: "typeof", arg }, ExprKind::Str(s)) => {
                            Some((arg, s.clone()))
                        }
                        _ => None,
                    }
                }
                let (l, r): (&Expr, &Expr) = (left, right);
                if let Some((arg, name)) = typeof_pair(l, r).or_else(|| typeof_pair(r, l)) {
                    if let Some(id) = self.narrowable_ident(arg) {
                        if let Some(cur) = self.current(id) {
                            out.push((id.to_string(), filter_typeof(&cur, &name, positive)));
                        }
                    }
                    return Ok(out);
                }
                let nullish = |e: &Expr| -> Option<Type> {
                    match &e.kind {
                        ExprKind::Null => Some(Type::Null),
                        ExprKind::Ident(n) if n == "undefined" => Some(Type::Undefined),
                        ExprKind::Unary { op: "void", .. } => Some(Type::Undefined),
                        _ => None,
                    }
                };
                let pair = match (nullish(r), nullish(l)) {
                    (Some(t), _) => Some((l, t)),
                    (_, Some(t)) => Some((r, t)),
                    _ => None,
                };
                if let Some((x, lit)) = pair {
                    if let Some(id) = self.narrowable_ident(x) {
                        if let Some(cur) = self.current(id) {
                            let set: Vec<Type> = if loose {
                                vec![Type::Null, Type::Undefined, Type::Void]
                            } else if lit == Type::Undefined {
                                vec![Type::Undefined, Type::Void]
                            } else {
                                vec![lit]
                            };
                            let t = if positive {
                                keep_members(&cur, |m| set.contains(m))
                            } else {
                                keep_members(&cur, |m| !set.contains(m))
                            };
                            out.push((id.to_string(), t));
                        }
                    }
                }
            }
            ExprKind::Binary {
                op: "instanceof",
                left,
                right,
            } => {
                if truthy {
                    if let (Some(id), ExprKind::Ident(cname)) =
                        (self.narrowable_ident(left), &right.kind)
                    {
                        if let Some(c) = self.class_named(cname) {
                            let cur = self.current(id).unwrap_or(Type::Unknown);
                            let t = match &cur {
                                Type::Instance(d)
                                    if assignable_in(
                                        &Type::Instance(*d),
                                        &Type::Instance(c as u32),
                                        self.p,
                                    ) =>
                                {
                                    cur.clone()
                                }
                                _ => Type::Instance(c as u32),
                            };
                            out.push((id.to_string(), t));
                        }
                    }
                }
            }
            ExprKind::Call { callee, args, .. } => {
                // User type predicate `x is T` (row 5): a check is inserted at the call.
                if truthy {
                    if let Some(fid) = self.callee_fn(callee) {
                        let sig = &self.p.sigs[fid];
                        if let Some(Predicate {
                            param,
                            ty: Some(_),
                            asserts: false,
                        }) = &sig.predicate
                        {
                            let pos = sig.params.iter().position(|p| &p.name == param);
                            let pt = self.p.m.funcs[fid]
                                .predicate
                                .as_ref()
                                .and_then(|p| p.ty.clone());
                            if let (Some(pos), Some(pt)) = (pos, pt) {
                                if let Some(arg) = args.get(pos) {
                                    let t = self.p.resolve_in(
                                        &pt,
                                        &sig.tparams,
                                        self.p.m.funcs[fid].class,
                                    );
                                    let k = self.p.project(&t);
                                    if !k.is_cast_checkable() && k != TKind::Object {
                                        return fail(
                                            cond.loc.start,
                                            format!(
                                                "type predicate narrows to unchecked `{t}` (row 5)"
                                            ),
                                        );
                                    }
                                    self.check(arg.loc, k);
                                    if let Some(id) = self.narrowable_ident(arg) {
                                        out.push((id.to_string(), t));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            ExprKind::Ident(_) if truthy => {
                if let Some(id) = self.narrowable_ident(cond) {
                    if let Some(cur) = self.current(id) {
                        out.push((
                            id.to_string(),
                            keep_members(&cur, |m| {
                                !matches!(m, Type::Null | Type::Undefined | Type::Void)
                            }),
                        ));
                    }
                }
            }
            _ => {}
        }
        Ok(out)
    }

    fn class_named(&self, name: &str) -> Option<ClassId> {
        if let Some(l) = self.lookup(name) {
            return l.class;
        }
        match self.p.values.get(name) {
            Some(ValueDecl::Class(c)) if !self.p.assigned.contains(name) => Some(*c),
            _ => None,
        }
    }

    /// The function a callee expression statically denotes, if its binding is stable.
    fn callee_fn(&self, callee: &Expr) -> Option<FnId> {
        let ExprKind::Ident(name) = &callee.kind else {
            return None;
        };
        if let Some(l) = self.lookup(name) {
            return l.func;
        }
        if self.p.dynamic_scope
            || self.p.assigned.contains(name)
            || self.captured_decl(name).is_some()
        {
            return None;
        }
        match self.p.values.get(name) {
            Some(ValueDecl::Func(f)) => Some(*f),
            _ => None,
        }
    }

    /// A binding of an enclosing function: `Some(declared type)` (`None` inside when it has no
    /// annotation).
    fn captured_decl(&self, name: &str) -> Option<Option<Type>> {
        let m = self.p.m;
        let mut anc = self.f.parent;
        while let Some(a) = anc {
            let af = &m.funcs[a];
            if let Some((i, _)) = af
                .params
                .iter()
                .enumerate()
                .find(|(_, p)| matches!(&p.pat, Pattern::Ident { name: n, .. } if n == name))
            {
                let t = self.p.sigs[a].params.get(i).map(|p| p.ty.clone());
                return Some(t.filter(|t| !t.contains_any()));
            }
            let mut found: Option<Option<Type>> = None;
            if let Some(Body::Block(ss)) = &af.body {
                find_decl(m, ss, name, &mut found);
            }
            if let Some(t) = found {
                let tps = &self.p.sigs[a].tparams;
                return Some(t.map(|t| self.p.resolve_in(&t, tps, af.class)));
            }
            anc = af.parent;
        }
        None
    }

    // ----- statements --------------------------------------------------------------------

    fn block_stmts(&mut self, ss: &[Stmt]) -> R<bool> {
        let mut falls = true;
        for s in ss {
            let f = self.stmt(s)?;
            falls = falls && f;
        }
        Ok(falls)
    }

    fn block(&mut self, ss: &[Stmt]) -> R<bool> {
        self.scopes.push(HashMap::new());
        self.hoist(ss);
        let r = self.block_stmts(ss);
        self.scopes.pop();
        r
    }

    fn stmt(&mut self, s: &Stmt) -> R<bool> {
        match s {
            Stmt::Var {
                kind,
                decls,
                declare,
                loc,
            } => {
                if *declare {
                    return Ok(true);
                }
                let doc_ty = self.p.doc_at(loc.start).and_then(|d| d.ty);
                for d in decls {
                    let Pattern::Ident { name, loc: nloc } = &d.pat else {
                        return fail(d.pat.loc().start, "destructuring (not in the v1 subset)");
                    };
                    let annotated =
                        d.ty.clone()
                            .or_else(|| {
                                if decls.len() == 1 {
                                    doc_ty.clone()
                                } else {
                                    None
                                }
                            })
                            .map(|t| self.p.resolve_in(&t, &self.sig().tparams, self.f.class));
                    if annotated.as_ref().is_some_and(Type::contains_any) {
                        return fail(nloc.start, format!("variable `{name}` is `any` (row 1)"));
                    }
                    let is_const = *kind == VarKind::Const;
                    let func = match d.init.as_ref().map(|e| &e.kind) {
                        Some(ExprKind::Func(fid))
                            if is_const || !self.assigned_here.contains(name) =>
                        {
                            Some(*fid)
                        }
                        _ => None,
                    };
                    let (declared, narrowed) = match (&annotated, &d.init) {
                        (Some(t), Some(init)) => {
                            self.expect(init, t)?;
                            let t = if d.definite {
                                Type::union(vec![t.clone(), Type::Undefined])
                            } else {
                                t.clone()
                            };
                            (t, None)
                        }
                        (Some(t), None) => {
                            // `let x: T;` / `let x!: T;` (row 4): `undefined` until assigned.
                            let decl = Type::union(vec![t.clone(), Type::Undefined]);
                            (decl, Some(Type::Undefined))
                        }
                        (None, Some(init)) => {
                            let t = self.expr(init)?.widen();
                            if t.contains_any() {
                                return fail(
                                    nloc.start,
                                    format!("variable `{name}` is `any` (row 1)"),
                                );
                            }
                            let t = match t {
                                // `[]` without annotation is `any[]` in TS (evolving array).
                                Type::Array(e) if matches!(*e, Type::Never) => {
                                    return fail(nloc.start, format!("`{name}` is an empty array literal without a type (implicit any[])"))
                                }
                                Type::Null | Type::Undefined if !is_const => {
                                    return fail(nloc.start, format!("variable `{name}` has an implicit `any` type"))
                                }
                                t => t,
                            };
                            (t, None)
                        }
                        (None, None) => {
                            return fail(
                                nloc.start,
                                format!("variable `{name}` has no type (implicit any)"),
                            );
                        }
                    };
                    self.declare(name, *nloc, declared, is_const, func, None);
                    if let Some(n) = narrowed {
                        if let Some(l) = self.lookup_mut(name) {
                            if !l.untrusted {
                                l.narrowed = Some(n);
                            }
                        }
                    }
                }
                Ok(true)
            }
            Stmt::Func(_) | Stmt::Class(_) => Ok(true),
            Stmt::TypeAlias { .. }
            | Stmt::Interface { .. }
            | Stmt::Empty(_)
            | Stmt::Debugger(_)
            | Stmt::Import { .. }
            | Stmt::ImportEquals { .. }
            | Stmt::ExportNamed { .. } => Ok(true),
            Stmt::Enum { loc, .. } => fail(loc.start, "enum declaration needs emit"),
            Stmt::Namespace { loc, .. } => fail(loc.start, "namespace needs emit"),
            Stmt::Export { stmt, .. } => self.stmt(stmt),
            Stmt::ExportDefaultExpr { expr, .. } | Stmt::ExportAssign { expr, .. } => {
                self.expr(expr)?;
                Ok(true)
            }
            Stmt::Expr(e) => {
                self.expr(e)?;
                Ok(true)
            }
            Stmt::Block(ss, _) => self.block(ss),
            Stmt::If {
                test, cons, alt, ..
            } => {
                self.expr(test)?;
                let snap = self.snapshot();
                let r = self.refine(test, true)?;
                self.apply(r);
                let ft_cons = self.scoped_stmt(cons)?;
                let after_cons = self.snapshot();
                self.restore(&snap);
                let r = self.refine(test, false)?;
                self.apply(r);
                let ft_alt = match alt {
                    Some(a) => self.scoped_stmt(a)?,
                    None => true,
                };
                let after_alt = self.snapshot();
                match (ft_cons, ft_alt) {
                    (true, true) => self.merge(&after_cons, &after_alt),
                    (true, false) => self.restore(&after_cons),
                    _ => self.restore(&after_alt),
                }
                Ok(ft_cons || ft_alt)
            }
            Stmt::While { test, body, .. } => {
                let mut names = self.assigned_in(body);
                names.extend(self.assigned_in_expr(test));
                self.invalidate(&names);
                self.expr(test)?;
                let snap = self.snapshot();
                let r = self.refine(test, true)?;
                self.apply(r);
                self.scoped_stmt(body)?;
                self.restore(&snap);
                self.invalidate(&names);
                let infinite = matches!(test.kind, ExprKind::Bool(true));
                Ok(!infinite || walk::has_break(body, false))
            }
            Stmt::DoWhile { body, test, .. } => {
                let mut names = self.assigned_in(body);
                names.extend(self.assigned_in_expr(test));
                self.invalidate(&names);
                self.scoped_stmt(body)?;
                self.invalidate(&names);
                self.expr(test)?;
                Ok(true)
            }
            Stmt::For {
                init,
                test,
                update,
                body,
                ..
            } => {
                self.scopes.push(HashMap::new());
                let r = (|| -> R<bool> {
                    if let Some(i) = init {
                        self.stmt(i)?;
                    }
                    let mut names = self.assigned_in(body);
                    if let Some(t) = test {
                        names.extend(self.assigned_in_expr(t));
                    }
                    if let Some(u) = update {
                        names.extend(self.assigned_in_expr(u));
                    }
                    self.invalidate(&names);
                    let snap = self.snapshot();
                    if let Some(t) = test {
                        self.expr(t)?;
                        let r = self.refine(t, true)?;
                        self.apply(r);
                    }
                    self.scoped_stmt(body)?;
                    if let Some(u) = update {
                        self.expr(u)?;
                    }
                    self.restore(&snap);
                    self.invalidate(&names);
                    Ok(test.is_some() || walk::has_break(body, false))
                })();
                self.scopes.pop();
                r
            }
            Stmt::ForIn {
                head,
                right,
                body,
                of,
                is_await,
                loc,
            } => {
                if *is_await {
                    return fail(loc.start, "for await (async, row 24)");
                }
                let rt = self.expr(right)?;
                let elem = if *of {
                    match &rt {
                        Type::Array(e) | Type::ReadonlyArray(e) => (**e).clone(),
                        Type::Tuple(ts) => Type::union(ts.clone()),
                        Type::String => Type::String,
                        t if is_dynamic(t) => Type::External,
                        t => return fail(right.loc.start, format!("for-of over `{t}`")),
                    }
                } else {
                    Type::String
                };
                self.scopes.push(HashMap::new());
                let r = (|| -> R<bool> {
                    match head {
                        ForHead::Var(kind, Pattern::Ident { name, loc }) => {
                            // Iteration runs the (patchable) iterator protocol: untrusted.
                            self.declare(name, *loc, elem, *kind == VarKind::Const, None, None);
                            if let Some(l) = self.lookup_mut(name) {
                                l.untrusted = true;
                            }
                        }
                        ForHead::Var(_, pat) => {
                            return fail(pat.loc().start, "destructuring (not in the v1 subset)")
                        }
                        ForHead::Expr(e) => {
                            return fail(e.loc.start, "for-in/of with an expression target")
                        }
                    }
                    let names = self.assigned_in(body);
                    self.invalidate(&names);
                    let snap = self.snapshot();
                    self.scoped_stmt(body)?;
                    self.restore(&snap);
                    self.invalidate(&names);
                    Ok(true)
                })();
                self.scopes.pop();
                r
            }
            Stmt::Return(e, loc) => {
                let ret = self.sig().ret.clone();
                match (e, ret) {
                    (Some(e), Some(r)) => self.expect(e, &r)?,
                    (Some(e), None) => {
                        let t = self.expr(e)?;
                        self.returns.push(t);
                    }
                    (None, Some(r)) => {
                        if !r.admits(&Type::Undefined) {
                            return fail(
                                loc.start,
                                format!("`return;` in a function returning `{r}`"),
                            );
                        }
                    }
                    (None, None) => self.returns.push(Type::Undefined),
                }
                Ok(false)
            }
            Stmt::Break(..) | Stmt::Continue(..) => Ok(false),
            Stmt::Throw(e, _) => {
                self.expr(e)?;
                Ok(false)
            }
            Stmt::Try {
                block,
                param,
                handler,
                finalizer,
                ..
            } => {
                let names = self.assigned_in(&Stmt::Block(block.clone(), Loc::default()));
                let ft_b = self.block(block)?;
                self.invalidate(&names);
                let ft_h = match handler {
                    Some(h) => {
                        self.scopes.push(HashMap::new());
                        let r = (|| -> R<bool> {
                            if let Some((pat, ty)) = param {
                                let Pattern::Ident { name, loc } = pat else {
                                    return fail(
                                        pat.loc().start,
                                        "destructuring (not in the v1 subset)",
                                    );
                                };
                                let t = match ty {
                                    Some(t) => {
                                        self.p.resolve_in(t, &self.sig().tparams, self.f.class)
                                    }
                                    None => Type::Unknown,
                                };
                                if t.contains_any() {
                                    return fail(loc.start, "catch variable is `any` (row 1)");
                                }
                                self.declare(name, *loc, t, false, None, None);
                            }
                            self.block(h)
                        })();
                        self.scopes.pop();
                        let hn = self.assigned_in(&Stmt::Block(h.clone(), Loc::default()));
                        self.invalidate(&hn);
                        r?
                    }
                    None => false,
                };
                let body_ft = if handler.is_some() {
                    ft_b || ft_h
                } else {
                    ft_b
                };
                match finalizer {
                    Some(fin) => {
                        let ft_f = self.block(fin)?;
                        Ok(body_ft && ft_f)
                    }
                    None => Ok(body_ft),
                }
            }
            Stmt::Switch { disc, cases, .. } => {
                self.expr(disc)?;
                let mut names = HashSet::new();
                for (_, b) in cases {
                    names.extend(self.assigned_in(&Stmt::Block(b.clone(), Loc::default())));
                }
                self.invalidate(&names);
                self.scopes.push(HashMap::new());
                let r = (|| -> R<bool> {
                    let snap = self.snapshot();
                    let mut last_ft = true;
                    let mut has_default = false;
                    let mut brk = false;
                    for (t, b) in cases {
                        self.restore(&snap);
                        match t {
                            Some(t) => {
                                self.expr(t)?;
                            }
                            None => has_default = true,
                        }
                        self.hoist(b);
                        last_ft = self.block_stmts(b)?;
                        brk |= b.iter().any(|s| walk::has_break(s, false));
                    }
                    self.restore(&snap);
                    Ok(!has_default || last_ft || brk)
                })();
                self.scopes.pop();
                self.invalidate(&names);
                r
            }
            Stmt::Labeled { body, .. } => self.stmt(body),
            Stmt::With { loc, .. } => fail(loc.start, "`with` statement (row 21)"),
        }
    }

    /// A statement in its own block scope (for `if` branches and loop bodies).
    fn scoped_stmt(&mut self, s: &Stmt) -> R<bool> {
        match s {
            Stmt::Block(ss, _) => self.block(ss),
            s => {
                self.scopes.push(HashMap::new());
                let r = self.stmt(s);
                self.scopes.pop();
                r
            }
        }
    }

    // ----- expressions -------------------------------------------------------------------

    fn expr(&mut self, e: &Expr) -> R<Type> {
        let at = e.loc.start;
        Ok(match &e.kind {
            ExprKind::Num(_) => Type::Number,
            ExprKind::Str(_) => Type::String,
            ExprKind::BigInt(_) => Type::BigInt,
            ExprKind::Bool(_) => Type::Boolean,
            ExprKind::Null => Type::Null,
            ExprKind::Regex => Type::Opaque("RegExp".into()),
            ExprKind::Template(parts) => {
                for p in parts {
                    let t = self.expr(p)?;
                    if t.contains_any() {
                        return fail(p.loc.start, "`any` in a template literal (row 1)");
                    }
                }
                Type::String
            }
            ExprKind::TaggedTemplate(tag, parts) => {
                self.expr(tag)?;
                for p in parts {
                    self.expr(p)?;
                }
                Type::External
            }
            ExprKind::Ident(name) => self.ident(name, e.loc)?,
            ExprKind::This => self.this_expr(e.loc)?,
            ExprKind::Super => Type::External,
            ExprKind::Array(items) => {
                let mut ts = Vec::new();
                for it in items {
                    match it {
                        None => ts.push(Type::Undefined),
                        Some(x) if matches!(x.kind, ExprKind::Spread(_)) => {
                            return fail(x.loc.start, "spread element (not in the v1 subset)")
                        }
                        Some(x) => ts.push(self.expr(x)?.widen()),
                    }
                }
                Type::Array(Box::new(Type::union(ts)))
            }
            ExprKind::Object(props) => self.object_literal(e, props)?,
            ExprKind::Func(fid) => Type::Function(Box::new(self.p.fn_type_of(*fid))),
            ExprKind::Class(_) => return fail(at, "class expression (not in the v1 subset)"),
            ExprKind::Member {
                obj,
                name,
                name_loc,
                optional,
            } => self.member(e, obj, name, *name_loc, *optional)?,
            ExprKind::Index {
                obj,
                index,
                optional,
            } => self.index(e, obj, index, *optional)?,
            ExprKind::Call {
                callee,
                args,
                optional,
                ..
            } => self.call(e, callee, args, *optional)?,
            ExprKind::New { callee, args, .. } => self.new_expr(e, callee, args)?,
            ExprKind::Unary { op, arg } => self.unary(e, op, arg)?,
            ExprKind::Update { arg, .. } => self.update(e, arg)?,
            ExprKind::Binary { op, left, right } => self.binary(e, op, left, right)?,
            ExprKind::Assign { op, target, value } => self.assign(e, op, target, value)?,
            ExprKind::Cond { test, cons, alt } => {
                self.expr(test)?;
                let snap = self.snapshot();
                let r = self.refine(test, true)?;
                self.apply(r);
                let a = self.expr(cons)?;
                self.restore(&snap);
                let r = self.refine(test, false)?;
                self.apply(r);
                let b = self.expr(alt)?;
                self.restore(&snap);
                Type::union(vec![a, b])
            }
            ExprKind::Seq(items) => {
                let mut t = Type::Undefined;
                for it in items {
                    t = self.expr(it)?;
                }
                t
            }
            ExprKind::Spread(_) => return fail(at, "spread (not in the v1 subset)"),
            ExprKind::As { expr, ty: None } => self.expr(expr)?,
            ExprKind::As { expr, ty: Some(t) }
            | ExprKind::TypeAssert { ty: t, expr }
            | ExprKind::JsDocCast { ty: t, expr } => self.cast(expr, t)?,
            ExprKind::Satisfies { expr, ty } => {
                let s = self.expr(expr)?;
                let t = self.p.resolve_in(ty, &self.sig().tparams, self.f.class);
                if !self.p.assignable(&s, &t) {
                    return fail(at, format!("`{s}` does not satisfy `{t}`"));
                }
                s
            }
            ExprKind::NonNull(inner) => {
                // Row 3: compiled as a check.
                let t = self.expr(inner)?;
                if t.contains_any() {
                    return fail(at, "non-null assertion on `any` (row 1)");
                }
                if t.admits(&Type::Null) || t.admits(&Type::Undefined) {
                    let nn = t.without_nullish();
                    let k = match self.p.project(&nn) {
                        k if k.is_cast_checkable() => k,
                        _ => TKind::Tags(tag::ALL & !tag::NULL & !tag::UNDEFINED),
                    };
                    self.check(inner.loc, k);
                    nn
                } else {
                    t
                }
            }
            ExprKind::Paren(inner) => self.expr(inner)?,
            ExprKind::Yield(_) | ExprKind::Await(_) => {
                return fail(at, "suspension point (row 24)")
            }
            ExprKind::MetaProp => Type::External,
            ExprKind::ImportCall(a) => {
                self.expr(a)?;
                Type::External
            }
            ExprKind::Unsupported(what) => {
                return fail(at, format!("{what} (not in the v1 subset)"))
            }
        })
    }

    fn object_literal(&mut self, e: &Expr, props: &[Prop]) -> R<Type> {
        let mut o = ObjectType::default();
        for p in props {
            match p {
                Prop::KeyValue { key, value } => {
                    let Some(name) = key.static_name() else {
                        return fail(e.loc.start, "computed key (not in the v1 subset)");
                    };
                    let t = self.expr(value)?.widen();
                    o.props.retain(|q| q.name != name);
                    o.props.push(Property {
                        name,
                        optional: false,
                        readonly: false,
                        method: false,
                        ty: t,
                    });
                }
                Prop::Shorthand { name, loc } => {
                    let t = self.ident(name, *loc)?.widen();
                    o.props.push(Property {
                        name: name.clone(),
                        optional: false,
                        readonly: false,
                        method: false,
                        ty: t,
                    });
                }
                Prop::Method { key, func } => {
                    let f = &self.p.m.funcs[*func];
                    if f.kind != FnKind::Method {
                        return fail(f.start, "object literal accessor (not in the v1 subset)");
                    }
                    o.props.push(Property {
                        name: key.static_name().unwrap_or_default(),
                        optional: false,
                        readonly: false,
                        method: true,
                        ty: Type::Function(Box::new(self.p.fn_type_of(*func))),
                    });
                }
                Prop::Spread(x) => {
                    return fail(x.loc.start, "object spread (not in the v1 subset)")
                }
            }
        }
        Ok(Type::Object(o))
    }

    fn ident(&mut self, name: &str, loc: Loc) -> R<Type> {
        if let Some(l) = self.lookup(name) {
            let l = l.clone();
            if l.untrusted {
                return Ok(self.untrusted(loc, l.declared));
            }
            return Ok(match l.narrowed {
                Some(n) => {
                    let kn = self.p.project(&n);
                    if kn != self.p.project(&l.declared) {
                        self.check(loc, kn);
                    }
                    n
                }
                None => l.declared,
            });
        }
        if let Some(t) = self.captured_decl(name) {
            // Captured from an enclosing function: that function may run as bytecode with any
            // value in the binding, so the read is checked.
            return Ok(match t {
                Some(t) => self.untrusted(loc, t),
                None => Type::External,
            });
        }
        match name {
            "undefined" => return Ok(Type::Undefined),
            "NaN" | "Infinity" => return Ok(Type::Number),
            "arguments" => return fail(loc.start, "uses `arguments` (row 21)"),
            "eval" => return fail(loc.start, "uses `eval` (row 21)"),
            _ => {}
        }
        Ok(match self.p.values.get(name).cloned() {
            Some(ValueDecl::Func(fid)) => Type::Function(Box::new(self.p.fn_type_of(fid))),
            Some(ValueDecl::Class(_)) => Type::Opaque("class".into()),
            Some(ValueDecl::Var {
                ty,
                is_const,
                literal,
                declare,
            }) => {
                if is_const && !declare {
                    if let Some(l) = literal {
                        return Ok(l);
                    }
                }
                // Module variables and `declare`d bindings (row 6) are untrusted.
                match ty {
                    Some(t) => {
                        let t = self.p.resolve_in(&t, &[], None);
                        if t.contains_any() {
                            return fail(loc.start, format!("`{name}` is `any` (row 1)"));
                        }
                        self.untrusted(loc, t)
                    }
                    None => Type::External,
                }
            }
            Some(ValueDecl::Enum(_)) => Type::Opaque("enum".into()),
            Some(ValueDecl::External) | None => Type::External,
        })
    }

    fn this_expr(&mut self, loc: Loc) -> R<Type> {
        match self.this_ty.clone() {
            Some(t) => {
                if self.this_untrusted {
                    Ok(self.untrusted(loc, t))
                } else {
                    Ok(t)
                }
            }
            None => fail(loc.start, "`this` has an implicit `any` type (row 13)"),
        }
    }

    fn math_shadowed(&self, name: &str) -> bool {
        self.lookup(name).is_some()
            || self.p.values.contains_key(name)
            || self.captured_decl(name).is_some()
    }

    fn member(
        &mut self,
        e: &Expr,
        obj: &Expr,
        name: &str,
        name_loc: Loc,
        optional: bool,
    ) -> R<Type> {
        if let ExprKind::Ident(o) = &obj.kind {
            if o == "Math" && !self.math_shadowed("Math") {
                return Ok(if MATH_CONSTS.contains(&name) {
                    Type::Number
                } else {
                    Type::External
                });
            }
            if let Some(ValueDecl::Enum(t)) = self.p.values.get(o) {
                if self.lookup(o).is_none() {
                    return Ok(t.clone());
                }
            }
        }
        let ot = self.expr(obj)?;
        self.member_of(e, &ot, name, name_loc, optional)
    }

    fn member_of(
        &mut self,
        e: &Expr,
        ot: &Type,
        name: &str,
        name_loc: Loc,
        optional: bool,
    ) -> R<Type> {
        let at = e.loc.start;
        if ot.contains_any() && matches!(ot, Type::Any) {
            return fail(at, format!("property `{name}` of an `any` value (row 1)"));
        }
        let nullish = ot.admits(&Type::Null) || ot.admits(&Type::Undefined);
        let base = if nullish && !is_dynamic(ot) && !matches!(ot, Type::Unknown) {
            if !optional {
                return fail(
                    at,
                    format!("property `{name}` of a possibly null/undefined `{ot}`"),
                );
            }
            ot.without_nullish()
        } else {
            ot.clone()
        };
        let t = match &base {
            t if is_dynamic(t) => Type::External,
            Type::Instance(c) => self.class_member(e, *c as usize, name, name_loc)?,
            Type::Array(_) | Type::ReadonlyArray(_) | Type::Tuple(_) | Type::String => {
                if name == "length" {
                    Type::Number
                } else {
                    Type::External
                }
            }
            Type::Number | Type::Boolean | Type::BigInt | Type::Symbol | Type::Function(_) => {
                Type::External
            }
            Type::Object(o) => {
                if let Some(p) = o.props.iter().find(|p| p.name == name) {
                    let t = if p.optional {
                        Type::union(vec![p.ty.clone(), Type::Undefined])
                    } else {
                        p.ty.clone()
                    };
                    // Structural types have no layout: reads are checked at use (row 10).
                    self.untrusted(e.loc, t)
                } else if let Some(ix) = o.index.iter().find(|i| matches!(i.key, Type::String)) {
                    let t = ix.value.clone();
                    let t = if self.p.opts.no_unchecked_indexed_access {
                        Type::union(vec![t, Type::Undefined])
                    } else {
                        t
                    };
                    self.untrusted(e.loc, t)
                } else {
                    return fail(name_loc.start, format!("no property `{name}` on `{base}`"));
                }
            }
            Type::Unknown => return fail(at, format!("property `{name}` of an `unknown` value")),
            Type::Never => Type::Never,
            Type::Union(_) => {
                return fail(
                    at,
                    format!("property `{name}` of a union `{base}` (not in the v1 subset)"),
                )
            }
            t => return fail(at, format!("property `{name}` of `{t}`")),
        };
        Ok(if nullish && optional {
            Type::union(vec![t, Type::Undefined])
        } else {
            t
        })
    }

    fn class_member(&mut self, e: &Expr, c: ClassId, name: &str, name_loc: Loc) -> R<Type> {
        let p = self.p;
        if let Some(field) = p.field_in_chain(c, name) {
            let t = field.kind_type.clone();
            if p.layout_sound[c] {
                let full = self.full_field_names(c);
                let index = full.iter().position(|n| n == name).unwrap_or(0) as u16;
                self.site(
                    name_loc,
                    SiteFact::Field {
                        class: c as u32,
                        index,
                    },
                );
                return Ok(t);
            }
            return Ok(self.untrusted(e.loc, t));
        }
        if let Some((fid, _)) = p.method_in_chain(c, name) {
            return Ok(Type::Function(Box::new(p.fn_type_of(fid))));
        }
        if let Some(g) = p.accessor_in_chain(c, name, FnKind::Getter) {
            let ret = p.sigs[g].ret.clone();
            return Ok(match ret {
                Some(r) => self.untrusted(e.loc, r),
                None => Type::External,
            });
        }
        fail(
            name_loc.start,
            format!("no property `{name}` on class `{}`", p.classes[c].name),
        )
    }

    fn full_field_names(&self, c: ClassId) -> Vec<String> {
        let mut chain = Vec::new();
        let mut k = Some(c);
        while let Some(id) = k {
            chain.push(id);
            k = self.p.classes[id].parent;
        }
        let mut out: Vec<String> = Vec::new();
        for id in chain.into_iter().rev() {
            for f in &self.p.classes[id].fields {
                if !out.contains(&f.name) {
                    out.push(f.name.clone());
                }
            }
        }
        out
    }

    fn number_operand(&mut self, e: &Expr) -> R<Type> {
        let t = self.expr(e)?;
        self.numeric(&t, e)
    }

    fn numeric(&mut self, t: &Type, e: &Expr) -> R<Type> {
        match t {
            Type::Number | Type::NumberLiteral(_) => Ok(Type::Number),
            Type::BigInt | Type::BigIntLiteral(_) => Ok(Type::BigInt),
            t if is_dynamic(t) => {
                self.check(e.loc, TKind::Num);
                Ok(Type::Number)
            }
            Type::Any => fail(e.loc.start, "arithmetic on `any` (row 1)"),
            t => fail(e.loc.start, format!("arithmetic on `{t}`")),
        }
    }

    fn index(&mut self, e: &Expr, obj: &Expr, index: &Expr, optional: bool) -> R<Type> {
        let ot = self.expr(obj)?;
        let nullish = ot.admits(&Type::Null) || ot.admits(&Type::Undefined);
        let base = if nullish && !is_dynamic(&ot) {
            if !optional {
                return fail(
                    e.loc.start,
                    format!("element of a possibly null/undefined `{ot}`"),
                );
            }
            ot.without_nullish()
        } else {
            ot.clone()
        };
        let unchecked = self.p.opts.no_unchecked_indexed_access;
        let t = match &base {
            Type::Array(el) | Type::ReadonlyArray(el) => {
                self.index_number(index)?;
                let el = (**el).clone();
                let k = self.p.project(&el);
                self.site(e.loc, SiteFact::Elem(k));
                if unchecked {
                    Type::union(vec![el, Type::Undefined])
                } else {
                    el
                }
            }
            Type::Tuple(ts) => {
                self.index_number(index)?;
                let el = match &index.kind {
                    ExprKind::Num(n) if (*n as usize) < ts.len() && n.fract() == 0.0 => {
                        ts[*n as usize].clone()
                    }
                    _ => Type::union(ts.clone()),
                };
                let k = self.p.project(&el);
                self.site(e.loc, SiteFact::Elem(k));
                el
            }
            Type::String => {
                self.index_number(index)?;
                self.site(e.loc, SiteFact::Elem(TKind::Str));
                if unchecked {
                    Type::union(vec![Type::String, Type::Undefined])
                } else {
                    Type::String
                }
            }
            Type::Object(o) if !o.index.is_empty() => {
                let it = self.expr(index)?;
                if it.contains_any() {
                    return fail(index.loc.start, "`any` index (row 1)");
                }
                let v = o.index[0].value.clone();
                let v = if unchecked {
                    Type::union(vec![v, Type::Undefined])
                } else {
                    v
                };
                // Index-signature reads are checked at use (row 7).
                self.untrusted(e.loc, v)
            }
            t if is_dynamic(t) => {
                self.expr(index)?;
                Type::External
            }
            t => return fail(e.loc.start, format!("element access on `{t}`")),
        };
        Ok(if nullish && optional {
            Type::union(vec![t, Type::Undefined])
        } else {
            t
        })
    }

    fn index_number(&mut self, index: &Expr) -> R<()> {
        let t = self.expr(index)?;
        match t {
            Type::Number | Type::NumberLiteral(_) => Ok(()),
            t if is_dynamic(&t) => {
                self.check(index.loc, TKind::Num);
                Ok(())
            }
            t => fail(index.loc.start, format!("array index of type `{t}`")),
        }
    }

    fn check_args(&mut self, params: &[Param], args: &[Expr], call: &Expr) -> R<()> {
        for (i, a) in args.iter().enumerate() {
            if matches!(a.kind, ExprKind::Spread(_)) {
                return fail(a.loc.start, "spread argument (not in the v1 subset)");
            }
            let p = params.get(i).or_else(|| params.last().filter(|p| p.rest));
            match p {
                Some(p) if p.rest => {
                    let el = match &p.ty {
                        Type::Array(e) | Type::ReadonlyArray(e) => (**e).clone(),
                        _ => Type::External,
                    };
                    self.expect(a, &el)?;
                }
                Some(p) => {
                    let t = FnType::param_type(p);
                    self.expect(a, &t)?;
                }
                None => {
                    self.expr(a)?;
                }
            }
        }
        if let Some(missing) = params
            .iter()
            .skip(args.len())
            .find(|p| !p.optional && !p.rest && !p.ty.admits(&Type::Undefined))
        {
            return fail(
                call.loc.start,
                format!("missing argument `{}`", missing.name),
            );
        }
        Ok(())
    }

    /// A call to a known function: arguments checked against its signature; a direct call to
    /// its typed entry when it is sound, else its result is checked.
    fn call_known(&mut self, e: &Expr, fid: FnId, args: &[Expr], receiver_ok: bool) -> R<Type> {
        let ft = self.p.fn_type_of(fid);
        self.check_args(&ft.params, args, e)?;
        let sig = &self.p.sigs[fid];
        let callee = &self.p.m.funcs[fid];
        if callee.is_async || callee.is_generator {
            return Ok(Type::External);
        }
        let Some(ret) = sig.ret.clone() else {
            return Ok(Type::External);
        };
        let generic = mentions_params(&ret);
        if receiver_ok && self.final_pass && self.p.sound[fid] && !generic {
            self.site(e.loc, SiteFact::Callee(self.p.key_of(fid)));
            return Ok(ret);
        }
        if generic {
            return Ok(Type::External);
        }
        Ok(self.untrusted(e.loc, ret))
    }

    /// A call through a function-typed value: arguments checked, result checked (§6.3).
    fn call_value(&mut self, e: &Expr, ft: &Type, args: &[Expr]) -> R<Type> {
        match ft {
            Type::Function(f) => {
                if f.construct {
                    return fail(e.loc.start, "calling a constructor type");
                }
                self.check_args(&f.params, args, e)?;
                if mentions_params(&f.ret) {
                    return Ok(Type::External);
                }
                Ok(self.untrusted(e.loc, f.ret.clone()))
            }
            Type::Object(o) if o.calls.len() == 1 => {
                let t = Type::Function(Box::new(o.calls[0].clone()));
                self.call_value(e, &t, args)
            }
            t if is_dynamic(t) => {
                for a in args {
                    if matches!(a.kind, ExprKind::Spread(_)) {
                        return fail(a.loc.start, "spread argument (not in the v1 subset)");
                    }
                    let at = self.expr(a)?;
                    if at.contains_any() {
                        return fail(a.loc.start, "`any` argument (row 1)");
                    }
                }
                Ok(Type::External)
            }
            Type::Any => fail(e.loc.start, "call of an `any` value (row 1)"),
            t => fail(e.loc.start, format!("call of a non-function `{t}`")),
        }
    }

    fn call(&mut self, e: &Expr, callee: &Expr, args: &[Expr], optional: bool) -> R<Type> {
        let _ = optional;
        match &callee.kind {
            ExprKind::Ident(name) if name == "eval" && self.lookup("eval").is_none() => {
                return fail(e.loc.start, "calls `eval` (row 21)")
            }
            ExprKind::Super => {
                for a in args {
                    self.expr(a)?;
                }
                return Ok(Type::Undefined);
            }
            _ => {}
        }
        if let Some(fid) = self.callee_fn(callee) {
            return self.call_known(e, fid, args, true);
        }
        if let ExprKind::Member { obj, name, .. } = &callee.kind {
            if let ExprKind::Ident(o) = &obj.kind {
                if o == "Math" && !self.math_shadowed("Math") && MATH_FNS.contains(&name.as_str()) {
                    for a in args {
                        self.number_operand(a)?;
                    }
                    return Ok(Type::Number);
                }
                if o == "Number"
                    && !self.math_shadowed("Number")
                    && matches!(
                        name.as_str(),
                        "isInteger" | "isFinite" | "isNaN" | "isSafeInteger"
                    )
                {
                    for a in args {
                        let t = self.expr(a)?;
                        if t.contains_any() {
                            return fail(a.loc.start, "`any` argument (row 1)");
                        }
                    }
                    return Ok(Type::Boolean);
                }
            }
            if matches!(obj.kind, ExprKind::Super) {
                for a in args {
                    self.expr(a)?;
                }
                return Ok(Type::External);
            }
            let ot = self.expr(obj)?;
            if ot.contains_any() && matches!(ot, Type::Any) {
                return fail(
                    obj.loc.start,
                    format!("method `{name}` of an `any` value (row 1)"),
                );
            }
            match &ot {
                Type::Instance(c) => {
                    let c = *c as usize;
                    if let Some((fid, _)) = self.p.method_in_chain(c, name) {
                        let direct = self.p.layout_sound[c] && !self.overridden(c, name);
                        return self.call_known(e, fid, args, direct);
                    }
                    let t = self.class_member(callee, c, name, callee.loc)?;
                    return self.call_value(e, &t, args);
                }
                Type::Array(el) | Type::ReadonlyArray(el)
                    if matches!(name.as_str(), "push" | "pop") =>
                {
                    if matches!(ot, Type::ReadonlyArray(_)) {
                        return fail(e.loc.start, format!("`{name}` on a readonly array (row 8)"));
                    }
                    let el = (**el).clone();
                    if name == "push" {
                        for a in args {
                            if matches!(a.kind, ExprKind::Spread(_)) {
                                return fail(a.loc.start, "spread argument (not in the v1 subset)");
                            }
                            self.expect(a, &el)?;
                        }
                        return Ok(Type::Number);
                    }
                    return Ok(Type::union(vec![el, Type::Undefined]));
                }
                Type::String if name == "charCodeAt" => {
                    for a in args {
                        self.number_operand(a)?;
                    }
                    return Ok(Type::Number);
                }
                _ => {
                    let t = self.member_of(callee, &ot, name, callee.loc, false)?;
                    return self.call_value(e, &t, args);
                }
            }
        }
        let t = self.expr(callee)?;
        self.call_value(e, &t, args)
    }

    /// Whether a subclass of `c` in this file overrides method `name` (class-hierarchy
    /// analysis; the engine adds the per-class watchpoint of §6.1).
    fn overridden(&self, c: ClassId, name: &str) -> bool {
        let defining = self.p.method_in_chain(c, name).map(|(_, k)| k);
        (0..self.p.classes.len()).any(|k| {
            k != c
                && Some(k) != defining
                && self.p.classes[k].methods.iter().any(|(n, _, _)| n == name)
                && {
                    let mut a = self.p.classes[k].parent;
                    let mut hit = false;
                    while let Some(x) = a {
                        if x == c {
                            hit = true;
                            break;
                        }
                        a = self.p.classes[x].parent;
                    }
                    hit
                }
        })
    }

    fn new_expr(&mut self, e: &Expr, callee: &Expr, args: &[Expr]) -> R<Type> {
        if let ExprKind::Ident(name) = &callee.kind {
            if let Some(c) = self.class_named(name) {
                let ctor = self.ctor_of(c);
                match ctor {
                    Some(fid) => {
                        let ft = self.p.fn_type_of(fid);
                        self.check_args(&ft.params, args, e)?;
                        if self.final_pass && self.p.sound[fid] {
                            self.site(e.loc, SiteFact::Callee(self.p.key_of(fid)));
                        }
                    }
                    None => {
                        for a in args {
                            self.expr(a)?;
                        }
                    }
                }
                return Ok(Type::Instance(c as u32));
            }
        }
        self.expr(callee)?;
        for a in args {
            if matches!(a.kind, ExprKind::Spread(_)) {
                return fail(a.loc.start, "spread argument (not in the v1 subset)");
            }
            let t = self.expr(a)?;
            if t.contains_any() {
                return fail(a.loc.start, "`any` argument (row 1)");
            }
        }
        Ok(Type::External)
    }

    fn ctor_of(&self, c: ClassId) -> Option<FnId> {
        let mut k = Some(c);
        while let Some(id) = k {
            if let Some(f) = self.p.classes[id].ctor {
                return Some(f);
            }
            k = self.p.classes[id].parent;
        }
        None
    }

    fn unary(&mut self, e: &Expr, op: &str, arg: &Expr) -> R<Type> {
        let t = self.expr(arg)?;
        if t.contains_any() {
            return fail(e.loc.start, format!("`{op}` on `any` (row 1)"));
        }
        Ok(match op {
            "!" | "delete" => Type::Boolean,
            "typeof" => Type::String,
            "void" => Type::Undefined,
            "+" => {
                if matches!(t, Type::BigInt) {
                    return fail(e.loc.start, "unary `+` on bigint");
                }
                Type::Number
            }
            "-" | "~" => {
                if matches!(t, Type::BigInt) {
                    Type::BigInt
                } else {
                    self.numeric(&t, arg)?
                }
            }
            _ => Type::External,
        })
    }

    fn update(&mut self, e: &Expr, arg: &Expr) -> R<Type> {
        let t = self.expr(arg)?;
        let r = self.numeric(&t, arg)?;
        self.assign_target(e, arg, &r)?;
        Ok(r)
    }

    fn binary_type(
        &mut self,
        e: &Expr,
        op: &str,
        l: Type,
        r: Type,
        left: &Expr,
        right: &Expr,
    ) -> R<Type> {
        if l.contains_any() || r.contains_any() {
            return fail(e.loc.start, format!("`{op}` with an `any` operand (row 1)"));
        }
        let numberish = |t: &Type| {
            matches!(
                t,
                Type::Number
                    | Type::Boolean
                    | Type::Null
                    | Type::Undefined
                    | Type::NumberLiteral(_)
            )
        };
        Ok(match op {
            "+" => {
                if matches!(l, Type::String) || matches!(r, Type::String) {
                    Type::String
                } else if numberish(&l) && numberish(&r) {
                    Type::Number
                } else if matches!(l, Type::BigInt) && matches!(r, Type::BigInt) {
                    Type::BigInt
                } else {
                    Type::External
                }
            }
            "-" | "*" | "/" | "%" | "**" => {
                let a = self.numeric(&l, left)?;
                let b = self.numeric(&r, right)?;
                if a != b {
                    return fail(e.loc.start, "mixing number and bigint");
                }
                a
            }
            "|" | "&" | "^" | "<<" | ">>" | ">>>" => {
                if matches!(l, Type::BigInt) && matches!(r, Type::BigInt) && op != ">>>" {
                    Type::BigInt
                } else if numberish(&l) && numberish(&r)
                    || (numberish(&l) && is_dynamic(&r))
                    || (is_dynamic(&l) && numberish(&r))
                {
                    Type::Number
                } else {
                    // An object operand runs `valueOf` (row 23), which could produce a bigint.
                    self.check(e.loc, TKind::Num);
                    Type::Number
                }
            }
            "<" | ">" | "<=" | ">=" | "==" | "!=" | "===" | "!==" | "instanceof" | "in" => {
                Type::Boolean
            }
            _ => Type::External,
        })
    }

    fn binary(&mut self, e: &Expr, op: &str, left: &Expr, right: &Expr) -> R<Type> {
        match op {
            "&&" | "||" | "??" => {
                let l = self.expr(left)?;
                let snap = self.snapshot();
                if op != "??" {
                    let r = self.refine(left, op == "&&")?;
                    self.apply(r);
                }
                let r = self.expr(right)?;
                self.restore(&snap);
                if l.contains_any() || r.contains_any() {
                    return fail(e.loc.start, format!("`{op}` with an `any` operand (row 1)"));
                }
                Ok(match op {
                    "&&" => Type::union(vec![l, r]),
                    _ => Type::union(vec![l.without_nullish(), r]),
                })
            }
            _ => {
                let l = self.expr(left)?;
                let r = self.expr(right)?;
                self.binary_type(e, op, l, r, left, right)
            }
        }
    }

    fn cast(&mut self, inner: &Expr, t: &Type) -> R<Type> {
        // Row 2: `as T`, `<T>x`, JSDoc casts.
        let target = self.p.resolve_in(t, &self.sig().tparams, self.f.class);
        if target.contains_any() {
            return fail(inner.loc.start, "cast to `any` (row 1)");
        }
        let s = self.expr(inner)?;
        if self.p.assignable(&s, &target) {
            return Ok(target);
        }
        if matches!(target, Type::Unknown) || is_dynamic(&target) {
            return Ok(target);
        }
        let k = self.p.project(&target);
        if k.is_cast_checkable() {
            self.check(inner.loc, k);
            return Ok(target);
        }
        fail(
            inner.loc.start,
            format!("unchecked cast to `{target}` (row 2: only primitives, classes and arrays are checkable)"),
        )
    }

    fn assign(&mut self, e: &Expr, op: &str, target: &Expr, value: &Expr) -> R<Type> {
        if op == "=" {
            let tt = self.target_type(target)?;
            match tt {
                Some(ref t) => self.expect(value, t)?,
                None => {
                    let v = self.expr(value)?;
                    if v.contains_any() {
                        return fail(value.loc.start, "`any` value (row 1)");
                    }
                }
            }
            let vt = self.value_type_after(value, &tt)?;
            self.assign_target(e, target, &vt)?;
            return Ok(vt);
        }
        // Compound: read, operate, write back.
        let cur = self.expr(target)?;
        let bin = op.trim_end_matches('=');
        let result = match bin {
            "&&" | "||" | "??" => {
                let v = self.expr(value)?;
                Type::union(vec![cur.without_nullish(), v])
            }
            _ => {
                let v = self.expr(value)?;
                self.binary_type(e, bin, cur, v, target, value)?
            }
        };
        if let Some(t) = self.target_type(target)? {
            self.flow(&result, &t, e.loc)?;
        }
        self.assign_target(e, target, &result)?;
        Ok(result)
    }

    /// The narrowed type a local takes after `x = value` (re-typing the value is avoided by
    /// using the target's type when the value was checked against it).
    fn value_type_after(&mut self, value: &Expr, target: &Option<Type>) -> R<Type> {
        Ok(match (&value.kind, target) {
            (ExprKind::Num(_), _) => Type::Number,
            (ExprKind::Str(_), _) => Type::String,
            (ExprKind::Bool(_), _) => Type::Boolean,
            (ExprKind::Null, _) => Type::Null,
            (ExprKind::Ident(n), _) if n == "undefined" => Type::Undefined,
            (_, Some(t)) => t.clone(),
            _ => Type::External,
        })
    }

    /// The declared type of an assignment target (`None`: no constraint).
    fn target_type(&mut self, target: &Expr) -> R<Option<Type>> {
        match &target.kind {
            ExprKind::Paren(inner) | ExprKind::NonNull(inner) => self.target_type(inner),
            ExprKind::Ident(name) => {
                if let Some(l) = self.lookup(name) {
                    if l.is_const {
                        return fail(target.loc.start, format!("assignment to const `{name}`"));
                    }
                    if l.func.is_some() || l.class.is_some() {
                        return fail(
                            target.loc.start,
                            format!("reassigning function/class `{name}`"),
                        );
                    }
                    return Ok(Some(l.declared.clone()));
                }
                if let Some(t) = self.captured_decl(name) {
                    return Ok(t);
                }
                match self.p.values.get(name) {
                    Some(ValueDecl::Var { ty: Some(t), .. }) => {
                        Ok(Some(self.p.resolve_in(t, &[], None)))
                    }
                    Some(ValueDecl::Func(_) | ValueDecl::Class(_)) => fail(
                        target.loc.start,
                        format!("reassigning function/class `{name}`"),
                    ),
                    _ => Ok(None),
                }
            }
            ExprKind::Member {
                obj,
                name,
                name_loc,
                ..
            } => {
                let ot = self.expr(obj)?;
                match &ot {
                    Type::Instance(c) => {
                        let c = *c as usize;
                        if let Some(f) = self.p.field_in_chain(c, name) {
                            let in_own_ctor = self.f.kind == FnKind::Constructor
                                && matches!(obj.kind, ExprKind::This);
                            if f.readonly && !in_own_ctor {
                                return fail(
                                    target.loc.start,
                                    format!("write to readonly field `{name}`"),
                                );
                            }
                            let declared = f.declared.clone();
                            if self.p.layout_sound[c] {
                                let full = self.full_field_names(c);
                                let index = full.iter().position(|n| n == name).unwrap_or(0) as u16;
                                self.site(
                                    *name_loc,
                                    SiteFact::Field {
                                        class: c as u32,
                                        index,
                                    },
                                );
                            }
                            return Ok(Some(declared));
                        }
                        if let Some(s) = self.p.accessor_in_chain(c, name, FnKind::Setter) {
                            return Ok(self.p.sigs[s].params.first().map(|p| p.ty.clone()));
                        }
                        if self.p.method_in_chain(c, name).is_some() {
                            return fail(
                                target.loc.start,
                                format!("method `{name}` reassigned (row 22)"),
                            );
                        }
                        fail(
                            name_loc.start,
                            format!("no property `{name}` on class `{}`", self.p.classes[c].name),
                        )
                    }
                    Type::Object(o) => match o.props.iter().find(|p| &p.name == name) {
                        Some(p) if p.readonly => {
                            fail(target.loc.start, format!("write to readonly `{name}`"))
                        }
                        Some(p) => Ok(Some(p.ty.clone())),
                        None => match o.index.first() {
                            Some(ix) if !ix.readonly => Ok(Some(ix.value.clone())),
                            _ => fail(name_loc.start, format!("no property `{name}` on `{ot}`")),
                        },
                    },
                    Type::Array(_) if name == "length" => Ok(Some(Type::Number)),
                    t if is_dynamic(t) => Ok(None),
                    t => fail(target.loc.start, format!("property write on `{t}`")),
                }
            }
            ExprKind::Index { obj, index, .. } => {
                let ot = self.expr(obj)?;
                match &ot {
                    Type::Array(el) => {
                        self.index_number(index)?;
                        let el = (**el).clone();
                        let k = self.p.project(&el);
                        self.site(target.loc, SiteFact::Elem(k));
                        Ok(Some(el))
                    }
                    Type::ReadonlyArray(_) => {
                        fail(target.loc.start, "write to a readonly array (row 8)")
                    }
                    Type::Object(o) if !o.index.is_empty() => {
                        self.expr(index)?;
                        if o.index[0].readonly {
                            return fail(
                                target.loc.start,
                                "write through a readonly index signature",
                            );
                        }
                        Ok(Some(o.index[0].value.clone()))
                    }
                    t if is_dynamic(t) => {
                        self.expr(index)?;
                        Ok(None)
                    }
                    t => fail(target.loc.start, format!("element write on `{t}`")),
                }
            }
            ExprKind::Unsupported(what) => {
                fail(target.loc.start, format!("{what} (not in the v1 subset)"))
            }
            _ => fail(target.loc.start, "invalid assignment target"),
        }
    }

    /// Records the effect of an assignment on narrowing.
    fn assign_target(&mut self, _e: &Expr, target: &Expr, vt: &Type) -> R<()> {
        let mut t = target;
        while let ExprKind::Paren(inner) = &t.kind {
            t = inner;
        }
        if let ExprKind::Ident(name) = &t.kind {
            if let Some(l) = self.lookup_mut(name) {
                if l.is_const {
                    return fail(target.loc.start, format!("assignment to const `{name}`"));
                }
                if !l.untrusted {
                    let n = if is_dynamic(vt) {
                        l.declared.clone()
                    } else {
                        vt.clone()
                    };
                    l.narrowed = if n == l.declared { None } else { Some(n) };
                }
            }
        }
        Ok(())
    }
}

fn keep_members(t: &Type, keep: impl Fn(&Type) -> bool) -> Type {
    match t {
        Type::Union(ms) => Type::union(ms.iter().filter(|m| keep(m)).cloned().collect()),
        Type::Boolean => t.clone(),
        t if keep(t) => t.clone(),
        _ => Type::Never,
    }
}

fn filter_typeof(t: &Type, name: &str, positive: bool) -> Type {
    let prim = match name {
        "number" => Some(Type::Number),
        "string" => Some(Type::String),
        "boolean" => Some(Type::Boolean),
        "bigint" => Some(Type::BigInt),
        "symbol" => Some(Type::Symbol),
        "undefined" => Some(Type::Undefined),
        _ => None,
    };
    let matches = |m: &Type| -> Option<bool> {
        Some(match m {
            Type::Number | Type::NumberLiteral(_) => name == "number",
            Type::String | Type::StringLiteral(_) => name == "string",
            Type::Boolean | Type::BooleanLiteral(_) => name == "boolean",
            Type::BigInt | Type::BigIntLiteral(_) => name == "bigint",
            Type::Symbol => name == "symbol",
            Type::Undefined | Type::Void => name == "undefined",
            Type::Null => name == "object",
            Type::Function(_) => name == "function",
            Type::Instance(_)
            | Type::Array(_)
            | Type::ReadonlyArray(_)
            | Type::Tuple(_)
            | Type::NonPrimitive => name == "object",
            _ => return None,
        })
    };
    let ms = t.members();
    let mut out = Vec::new();
    for m in ms {
        match matches(m) {
            Some(hit) => {
                if hit == positive {
                    out.push(m.clone());
                }
            }
            // Unknown/dynamic/structural members: a positive primitive test pins the type.
            None => {
                if positive {
                    match &prim {
                        Some(p) => out.push(p.clone()),
                        None => out.push(m.clone()),
                    }
                } else {
                    out.push(m.clone());
                }
            }
        }
    }
    Type::union(out)
}

fn mentions_params(t: &Type) -> bool {
    match t {
        Type::Param(_) | Type::This => true,
        Type::Array(e) | Type::ReadonlyArray(e) => mentions_params(e),
        Type::Tuple(ts) | Type::Union(ts) | Type::Intersection(ts) => {
            ts.iter().any(mentions_params)
        }
        Type::Object(o) => o.props.iter().any(|p| mentions_params(&p.ty)),
        Type::Function(f) => {
            f.params.iter().any(|p| mentions_params(&p.ty)) || mentions_params(&f.ret)
        }
        _ => false,
    }
}

/// Finds the declaration of `name` among a function's own statements (not nested functions):
/// `Some(Some(t))` annotated, `Some(None)` unannotated.
fn find_decl(m: &Module, ss: &[Stmt], name: &str, out: &mut Option<Option<Type>>) {
    for s in ss {
        if out.is_some() {
            return;
        }
        match s {
            Stmt::Var { decls, .. } => {
                for d in decls {
                    if d.pat.names().iter().any(|(n, _)| n == name) {
                        *out = Some(match &d.pat {
                            Pattern::Ident { .. } => d.ty.clone(),
                            _ => None,
                        });
                        return;
                    }
                }
            }
            Stmt::Func(id) if m.funcs[*id].name.as_deref() == Some(name) => {
                *out = Some(None);
                return;
            }
            Stmt::Class(id) if m.classes[*id].name.as_deref() == Some(name) => {
                *out = Some(None);
                return;
            }
            Stmt::Block(b, _) => find_decl(m, b, name, out),
            Stmt::If { cons, alt, .. } => {
                find_decl(m, std::slice::from_ref(cons), name, out);
                if let Some(a) = alt {
                    find_decl(m, std::slice::from_ref(a), name, out);
                }
            }
            Stmt::For { init, body, .. } => {
                if let Some(i) = init {
                    find_decl(m, std::slice::from_ref(i), name, out);
                }
                find_decl(m, std::slice::from_ref(body), name, out);
            }
            Stmt::ForIn {
                head: ForHead::Var(_, p),
                body,
                ..
            } => {
                if p.names().iter().any(|(n, _)| n == name) {
                    *out = Some(None);
                    return;
                }
                find_decl(m, std::slice::from_ref(body), name, out);
            }
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } | Stmt::Labeled { body, .. } => {
                find_decl(m, std::slice::from_ref(body), name, out)
            }
            Stmt::Try {
                block,
                param,
                handler,
                finalizer,
                ..
            } => {
                find_decl(m, block, name, out);
                if let Some((p, _)) = param {
                    if p.names().iter().any(|(n, _)| n == name) {
                        *out = Some(None);
                        return;
                    }
                }
                if let Some(h) = handler {
                    find_decl(m, h, name, out);
                }
                if let Some(h) = finalizer {
                    find_decl(m, h, name, out);
                }
            }
            Stmt::Switch { cases, .. } => {
                for (_, b) in cases {
                    find_decl(m, b, name, out);
                }
            }
            _ => {}
        }
    }
}

// ----- table construction ------------------------------------------------------------------

pub(crate) fn build_table(p: &mut Program) -> TypeTable {
    let m = p.m;
    let candidates: Vec<FnId> = m
        .funcs
        .iter()
        .filter(|f| f.body.is_some() && !f.declare)
        .map(|f| f.id)
        .collect();
    // Pass 1: verdicts. They do not depend on callee verdicts (a call to a non-sound callee
    // only turns into a checked result), so one pass suffices.
    let mut outs: HashMap<FnId, Result<CheckOut, Unsound>> = HashMap::new();
    for &id in &candidates {
        let r = p.check_fn(id, false);
        p.sound[id] = r.is_ok();
        outs.insert(id, r);
    }
    // Pass 2: facts for sound functions, now that callee verdicts are known.
    for &id in &candidates {
        if p.sound[id] {
            let r = p.check_fn(id, true);
            outs.insert(id, r);
        }
    }
    let mut table = TypeTable::default();
    for (i, c) in m.classes.iter().enumerate() {
        let info = &p.classes[i];
        table.classes.push(ClassLayout {
            name: info.name.clone(),
            start: c.start,
            end: c.end,
            parent: info.parent.map(|x| x as u32),
            fields: info
                .fields
                .iter()
                .map(|f| (f.name.clone(), p.project(&f.kind_type), f.readonly))
                .collect(),
            methods: info
                .methods
                .iter()
                .map(|(n, fid, _)| (n.clone(), p.key_of(*fid)))
                .collect(),
            sound: p.layout_sound[i],
        });
        if let Some((at, reason)) = &info.problem {
            table.report.push(SoundnessNote {
                subject: Subject::Class(i as u32),
                at: if *at == 0 { c.start } else { *at },
                reason: reason.clone(),
            });
        }
    }
    for &id in &candidates {
        let f = &m.funcs[id];
        let sig = &p.sigs[id];
        let key = p.key_of(id);
        let out = outs.remove(&id).unwrap();
        let sound = out.is_ok() && p.sound[id];
        let params: Vec<TKind> = sig
            .params
            .iter()
            .map(|sp| {
                let t = if sp.optional || sp.has_default {
                    Type::union(vec![sp.ty.clone(), Type::Undefined])
                } else {
                    sp.ty.clone()
                };
                p.project(&t)
            })
            .collect();
        let this = match (&sig.this, f.class, f.kind) {
            (Some(t), _, _) => p.project(t),
            (None, Some(c), k) if !f.is_static && k != FnKind::Constructor => {
                p.project(&Type::Instance(c as u32))
            }
            _ => TKind::Any,
        };
        let (ret, locals, sites) = match out {
            Ok(o) => {
                let r = match (f.kind, f.class) {
                    (FnKind::Constructor, Some(c)) => Some(Type::Instance(c as u32)),
                    _ => sig.ret.clone().or(o.ret),
                };
                (
                    r.map(|r| p.project(&r)).unwrap_or(TKind::Any),
                    o.locals,
                    o.sites,
                )
            }
            Err(u) => {
                table.report.push(SoundnessNote {
                    subject: Subject::Fn(key),
                    at: u.at,
                    reason: u.msg,
                });
                (
                    sig.ret.as_ref().map(|r| p.project(r)).unwrap_or(TKind::Any),
                    annotated_locals(p, id),
                    Vec::new(),
                )
            }
        };
        let sig_id = p.intern_sig(Signature {
            params: params.clone(),
            ret: ret.clone(),
        });
        let name = display_name(p, id);
        table.fns.push((
            key,
            FnTypes {
                name,
                kind: f.kind,
                start: key,
                end: match (f.kind, f.class) {
                    (FnKind::Constructor, Some(c)) => m.classes[c].end,
                    _ => f.end,
                },
                params,
                param_names: sig.params.iter().map(|sp| sp.name.clone()).collect(),
                this,
                ret,
                locals,
                sites,
                sound,
                sig: sig_id,
            },
        ));
    }
    table.fns.sort_by_key(|(k, _)| *k);
    table.fns.dedup_by_key(|(k, _)| *k);
    table.sigs = p.sig_ids.borrow().clone();
    table
}

fn display_name(p: &Program, id: FnId) -> String {
    let m = p.m;
    let f = &m.funcs[id];
    let class_name = f.class.map(|c| p.classes[c].name.clone());
    match (f.kind, &f.name, class_name) {
        (FnKind::Constructor, _, Some(c)) => c,
        (FnKind::Getter | FnKind::Setter, Some(n), Some(c)) => format!(
            "{} {c}{}{n}",
            if f.kind == FnKind::Getter {
                "get"
            } else {
                "set"
            },
            if f.is_static { "." } else { "#" }
        ),
        (FnKind::Getter, Some(n), None) => format!("get {n}"),
        (FnKind::Setter, Some(n), None) => format!("set {n}"),
        (_, Some(n), Some(c)) => {
            if f.is_static {
                format!("{c}.{n}")
            } else {
                format!("{c}#{n}")
            }
        }
        (_, Some(n), None) => n.clone(),
        (_, None, _) => p.display.get(&id).cloned().unwrap_or_else(|| {
            if f.kind == FnKind::Arrow {
                "<arrow>".into()
            } else {
                "<function>".into()
            }
        }),
    }
}

/// Stage-1 hints for a non-sound function: annotated locals only.
fn annotated_locals(p: &Program, id: FnId) -> Vec<(u32, TKind)> {
    let m = p.m;
    let f = &m.funcs[id];
    let sig = &p.sigs[id];
    let mut out: Vec<(u32, TKind)> = sig
        .params
        .iter()
        .filter(|sp| !sp.ty.contains_any())
        .map(|sp| (sp.loc.start, p.project(&sp.ty)))
        .collect();
    fn rec(
        p: &Program,
        ss: &[Stmt],
        tps: &[String],
        class: Option<ClassId>,
        out: &mut Vec<(u32, TKind)>,
    ) {
        for s in ss {
            match s {
                Stmt::Var { decls, loc, .. } => {
                    let doc = p.doc_at(loc.start).and_then(|d| d.ty);
                    for d in decls {
                        if let Pattern::Ident { loc: nl, .. } = &d.pat {
                            if let Some(t) = d.ty.clone().or_else(|| doc.clone()) {
                                out.push((nl.start, p.project(&p.resolve_in(&t, tps, class))));
                            }
                        }
                    }
                }
                Stmt::Block(b, _) => rec(p, b, tps, class, out),
                Stmt::If { cons, alt, .. } => {
                    rec(p, std::slice::from_ref(cons), tps, class, out);
                    if let Some(a) = alt {
                        rec(p, std::slice::from_ref(a), tps, class, out);
                    }
                }
                Stmt::For { init, body, .. } => {
                    if let Some(i) = init {
                        rec(p, std::slice::from_ref(i), tps, class, out);
                    }
                    rec(p, std::slice::from_ref(body), tps, class, out);
                }
                Stmt::ForIn { body, .. }
                | Stmt::While { body, .. }
                | Stmt::DoWhile { body, .. }
                | Stmt::Labeled { body, .. } => rec(p, std::slice::from_ref(body), tps, class, out),
                Stmt::Try {
                    block,
                    handler,
                    finalizer,
                    ..
                } => {
                    rec(p, block, tps, class, out);
                    if let Some(h) = handler {
                        rec(p, h, tps, class, out);
                    }
                    if let Some(h) = finalizer {
                        rec(p, h, tps, class, out);
                    }
                }
                Stmt::Switch { cases, .. } => {
                    for (_, b) in cases {
                        rec(p, b, tps, class, out);
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(Body::Block(ss)) = &f.body {
        rec(p, ss, &sig.tparams, f.class, &mut out);
    }
    out.retain(|(_, k)| *k != TKind::Any);
    out
}
