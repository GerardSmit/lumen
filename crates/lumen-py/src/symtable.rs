//! Scope analysis with CPython semantics: classifies every name of every scope as local,
//! global, free or cell.

use crate::ast::*;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

pub const DEF_GLOBAL: u32 = 1;
pub const DEF_NONLOCAL: u32 = 2;
pub const DEF_LOCAL: u32 = 4;
pub const DEF_PARAM: u32 = 8;
pub const DEF_USE: u32 = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScopeKind {
    Module,
    Function,
    Class,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sc {
    Unknown,
    Local,
    GlobalExplicit,
    GlobalImplicit,
    Free,
    Cell,
}

pub struct Sym {
    pub flags: u32,
    pub scope: Sc,
}

pub struct Scope {
    pub kind: ScopeKind,
    pub is_comp: bool,
    pub is_lambda: bool,
    pub name: Rc<str>,
    pub syms: Vec<(Rc<str>, Sym)>,
    pub index: HashMap<Rc<str>, usize>,
    pub children: Vec<usize>,
    pub is_gen: bool,
    pub is_async: bool,
    pub is_genexp: bool,
    pub has_class_cell: bool,
    pub private: Option<Rc<str>>,
    pub params: Vec<Rc<str>>,
    pub extra_free: Vec<Rc<str>>,
    pub line: u32,
}

impl Scope {
    pub fn sym_scope(&self, name: &str) -> Sc {
        match self.index.get(name) {
            Some(&i) => self.syms[i].1.scope,
            None => Sc::GlobalImplicit,
        }
    }
}

pub struct SymTable {
    pub scopes: Vec<Scope>,
    pub ids: HashMap<usize, usize>,
}

pub struct SymError {
    pub msg: String,
    pub line: u32,
}

pub fn mangle(private: &Option<Rc<str>>, name: &str) -> Rc<str> {
    if let Some(p) = private {
        if name.starts_with("__") && !name.ends_with("__") && !name.contains('.') {
            let p = p.trim_start_matches('_');
            if !p.is_empty() {
                return format!("_{}{}", p, name).into();
            }
        }
    }
    name.into()
}

struct Builder {
    scopes: Vec<Scope>,
    ids: HashMap<usize, usize>,
    cur: usize,
    line: u32,
    err: Option<SymError>,
}

pub fn build(module: &Module) -> Result<SymTable, SymError> {
    let mut b = Builder { scopes: Vec::new(), ids: HashMap::new(), cur: 0, line: 1, err: None };
    b.new_scope(ScopeKind::Module, "<module>".into(), None, false);
    b.visit_body(&module.body);
    if let Some(e) = b.err.take() {
        return Err(e);
    }
    let mut t = SymTable { scopes: b.scopes, ids: b.ids };
    let empty = HashSet::new();
    analyze(&mut t, 0, &empty)?;
    Ok(t)
}

pub fn build_expr(e: &Expr) -> Result<SymTable, SymError> {
    let mut b = Builder { scopes: Vec::new(), ids: HashMap::new(), cur: 0, line: 1, err: None };
    b.new_scope(ScopeKind::Module, "<module>".into(), None, false);
    b.visit_expr(e);
    if let Some(e) = b.err.take() {
        return Err(e);
    }
    let mut t = SymTable { scopes: b.scopes, ids: b.ids };
    let empty = HashSet::new();
    analyze(&mut t, 0, &empty)?;
    Ok(t)
}

fn analyze(t: &mut SymTable, id: usize, bound: &HashSet<Rc<str>>) -> Result<HashSet<Rc<str>>, SymError> {
    let kind = t.scopes[id].kind;
    let mut local: HashSet<Rc<str>> = HashSet::new();
    let mut free: HashSet<Rc<str>> = HashSet::new();
    let line = t.scopes[id].line;
    for (name, sym) in t.scopes[id].syms.iter_mut() {
        let f = sym.flags;
        if f & DEF_GLOBAL != 0 {
            sym.scope = Sc::GlobalExplicit;
        } else if f & DEF_NONLOCAL != 0 {
            if !bound.contains(name) {
                return Err(SymError { msg: format!("no binding for nonlocal '{}' found", name), line });
            }
            sym.scope = Sc::Free;
            free.insert(name.clone());
        } else if f & (DEF_LOCAL | DEF_PARAM) != 0 {
            sym.scope = Sc::Local;
            local.insert(name.clone());
        } else if bound.contains(name) {
            sym.scope = Sc::Free;
            free.insert(name.clone());
        } else {
            sym.scope = Sc::GlobalImplicit;
        }
    }
    let mut newbound = bound.clone();
    match kind {
        ScopeKind::Function => {
            newbound.extend(local.iter().cloned());
        }
        ScopeKind::Class => {
            newbound.insert("__class__".into());
        }
        ScopeKind::Module => {}
    }
    let children = t.scopes[id].children.clone();
    for c in children {
        let cf = analyze(t, c, &newbound)?;
        for name in cf {
            let s = &mut t.scopes[id];
            if kind == ScopeKind::Function && local.contains(&name) {
                if let Some(&i) = s.index.get(&name) {
                    s.syms[i].1.scope = Sc::Cell;
                }
            } else if kind == ScopeKind::Class && &*name == "__class__" {
                s.has_class_cell = true;
            } else if let Some(&i) = s.index.get(&name) {
                match s.syms[i].1.scope {
                    Sc::GlobalImplicit => s.syms[i].1.scope = Sc::Free,
                    Sc::Local | Sc::Cell if kind == ScopeKind::Class => s.extra_free.push(name.clone()),
                    _ => {}
                }
                free.insert(name.clone());
            } else if kind != ScopeKind::Module {
                let n = s.syms.len();
                s.syms.push((name.clone(), Sym { flags: DEF_USE, scope: Sc::Free }));
                s.index.insert(name.clone(), n);
                free.insert(name.clone());
            }
        }
    }
    if kind == ScopeKind::Class {
        free.remove("__class__");
    }
    Ok(free)
}

impl Builder {
    fn new_scope(&mut self, kind: ScopeKind, name: Rc<str>, key: Option<usize>, is_comp: bool) -> usize {
        let id = self.scopes.len();
        let private = match kind {
            ScopeKind::Class => Some(name.clone()),
            _ => self.scopes.get(self.cur).and_then(|s| s.private.clone()),
        };
        self.scopes.push(Scope {
            kind,
            is_comp,
            is_lambda: false,
            name,
            syms: Vec::new(),
            index: HashMap::new(),
            children: Vec::new(),
            is_gen: false,
            is_async: false,
            is_genexp: false,
            has_class_cell: false,
            private,
            params: Vec::new(),
            extra_free: Vec::new(),
            line: self.line,
        });
        if id > 0 {
            self.scopes[self.cur].children.push(id);
        }
        if let Some(k) = key {
            self.ids.insert(k, id);
        }
        id
    }

    fn error(&mut self, msg: String) {
        if self.err.is_none() {
            self.err = Some(SymError { msg, line: self.line });
        }
    }

    fn add(&mut self, scope: usize, name: &str, flags: u32) {
        let name = mangle(&self.scopes[scope].private, name);
        let s = &mut self.scopes[scope];
        match s.index.get(&name) {
            Some(&i) => s.syms[i].1.flags |= flags,
            None => {
                let n = s.syms.len();
                s.index.insert(name.clone(), n);
                s.syms.push((name, Sym { flags, scope: Sc::Unknown }));
            }
        }
    }

    fn def(&mut self, name: &str) {
        let cur = self.cur;
        if let Some(&i) = self.scopes[cur].index.get(&mangle(&self.scopes[cur].private, name)) {
            let f = self.scopes[cur].syms[i].1.flags;
            if f & DEF_GLOBAL != 0 && self.scopes[cur].kind == ScopeKind::Module {
                return;
            }
        }
        self.add(cur, name, DEF_LOCAL);
    }

    fn visit_body(&mut self, body: &[Stmt]) {
        for s in body {
            self.visit_stmt(s);
        }
    }

    fn visit_params(&mut self, args: &Arguments) {
        let cur = self.cur;
        let mut names: Vec<Rc<str>> = Vec::new();
        for a in args.posonlyargs.iter().chain(args.args.iter()) {
            names.push(a.arg.clone());
        }
        for a in &args.kwonlyargs {
            names.push(a.arg.clone());
        }
        if let Some(a) = &args.vararg {
            names.push(a.arg.clone());
        }
        if let Some(a) = &args.kwarg {
            names.push(a.arg.clone());
        }
        for n in names {
            self.add(cur, &n, DEF_PARAM);
            let m = mangle(&self.scopes[cur].private, &n);
            self.scopes[cur].params.push(m);
        }
    }

    fn visit_arg_exprs(&mut self, args: &Arguments) {
        for d in &args.defaults {
            self.visit_expr(d);
        }
        for d in args.kw_defaults.iter().flatten() {
            self.visit_expr(d);
        }
        for a in args.posonlyargs.iter().chain(args.args.iter()).chain(args.kwonlyargs.iter()) {
            if let Some(an) = &a.annotation {
                self.visit_expr(an);
            }
        }
        for a in [&args.vararg, &args.kwarg].into_iter().flatten() {
            if let Some(an) = &a.annotation {
                self.visit_expr(an);
            }
        }
    }

    fn visit_stmt(&mut self, s: &Stmt) {
        self.line = s.pos.line;
        match &s.kind {
            StmtKind::FunctionDef(f) => {
                self.def(&f.name);
                for d in &f.decorators {
                    self.visit_expr(d);
                }
                self.visit_arg_exprs(&f.args);
                if let Some(r) = &f.returns {
                    self.visit_expr(r);
                }
                let key = &**f as *const FunctionDef as usize;
                let id = self.new_scope(ScopeKind::Function, f.name.clone(), Some(key), false);
                self.scopes[id].is_async = f.is_async;
                let saved = self.cur;
                self.cur = id;
                self.visit_params(&f.args);
                self.visit_body(&f.body);
                self.cur = saved;
            }
            StmtKind::ClassDef(c) => {
                self.def(&c.name);
                for d in &c.decorators {
                    self.visit_expr(d);
                }
                for b in &c.bases {
                    self.visit_expr(b);
                }
                for k in &c.keywords {
                    self.visit_expr(&k.value);
                }
                let key = &**c as *const ClassDef as usize;
                let id = self.new_scope(ScopeKind::Class, c.name.clone(), Some(key), false);
                let saved = self.cur;
                self.cur = id;
                self.visit_body(&c.body);
                self.cur = saved;
            }
            StmtKind::Return(v) => {
                if let Some(v) = v {
                    self.visit_expr(v);
                }
            }
            StmtKind::Delete(ts) => {
                for t in ts {
                    self.visit_expr(t);
                }
            }
            StmtKind::Assign { targets, value } => {
                self.visit_expr(value);
                for t in targets {
                    self.visit_expr(t);
                }
            }
            StmtKind::AugAssign { target, value, .. } => {
                if let ExprKind::Name { id, .. } = &target.kind {
                    let cur = self.cur;
                    self.add(cur, id, DEF_USE);
                }
                self.visit_expr(target);
                self.visit_expr(value);
            }
            StmtKind::AnnAssign { target, annotation, value, .. } => {
                if let Some(v) = value {
                    self.visit_expr(v);
                }
                self.visit_expr(target);
                if self.scopes[self.cur].kind != ScopeKind::Function {
                    self.visit_expr(annotation);
                }
            }
            StmtKind::For { target, iter, body, orelse, .. } => {
                self.visit_expr(iter);
                self.visit_expr(target);
                self.visit_body(body);
                self.visit_body(orelse);
            }
            StmtKind::While { test, body, orelse } => {
                self.visit_expr(test);
                self.visit_body(body);
                self.visit_body(orelse);
            }
            StmtKind::If { test, body, orelse } => {
                self.visit_expr(test);
                self.visit_body(body);
                self.visit_body(orelse);
            }
            StmtKind::With { items, body, .. } => {
                for it in items {
                    self.visit_expr(&it.context_expr);
                    if let Some(v) = &it.optional_vars {
                        self.visit_expr(v);
                    }
                }
                self.visit_body(body);
            }
            StmtKind::Match { subject, cases } => {
                self.visit_expr(subject);
                for c in cases {
                    self.visit_pattern(&c.pattern);
                    if let Some(g) = &c.guard {
                        self.visit_expr(g);
                    }
                    self.visit_body(&c.body);
                }
            }
            StmtKind::Raise { exc, cause } => {
                if let Some(e) = exc {
                    self.visit_expr(e);
                }
                if let Some(e) = cause {
                    self.visit_expr(e);
                }
            }
            StmtKind::Try { body, handlers, orelse, finalbody, .. } => {
                self.visit_body(body);
                for h in handlers {
                    self.line = h.pos.line;
                    if let Some(t) = &h.typ {
                        self.visit_expr(t);
                    }
                    if let Some(n) = &h.name {
                        self.def(n);
                    }
                    self.visit_body(&h.body);
                }
                self.visit_body(orelse);
                self.visit_body(finalbody);
            }
            StmtKind::Assert { test, msg } => {
                self.visit_expr(test);
                if let Some(m) = msg {
                    self.visit_expr(m);
                }
            }
            StmtKind::Import(names) => {
                for a in names {
                    match &a.asname {
                        Some(n) => self.def(n),
                        None => {
                            let top = a.name.split('.').next().unwrap_or("").to_string();
                            self.def(&top);
                        }
                    }
                }
            }
            StmtKind::ImportFrom { names, .. } => {
                for a in names {
                    if &*a.name == "*" {
                        continue;
                    }
                    let n = a.asname.clone().unwrap_or_else(|| a.name.clone());
                    self.def(&n);
                }
            }
            StmtKind::Global(names) => {
                let cur = self.cur;
                for n in names {
                    let m = mangle(&self.scopes[cur].private, n);
                    if let Some(&i) = self.scopes[cur].index.get(&m) {
                        let f = self.scopes[cur].syms[i].1.flags;
                        if f & DEF_LOCAL != 0 && self.scopes[cur].kind != ScopeKind::Module {
                            self.error(format!("name '{}' is assigned to before global declaration", n));
                        }
                    }
                    self.add(cur, n, DEF_GLOBAL);
                }
            }
            StmtKind::Nonlocal(names) => {
                let cur = self.cur;
                if self.scopes[cur].kind == ScopeKind::Module {
                    self.error("nonlocal declaration not allowed at module level".into());
                }
                for n in names {
                    self.add(cur, n, DEF_NONLOCAL);
                }
            }
            StmtKind::Expr(e) => self.visit_expr(e),
            StmtKind::Pass | StmtKind::Break | StmtKind::Continue => {}
        }
    }

    fn visit_pattern(&mut self, p: &Pattern) {
        match p {
            Pattern::MatchValue(e) => self.visit_expr(e),
            Pattern::MatchSingleton(_) => {}
            Pattern::MatchSequence(ps) | Pattern::MatchOr(ps) => {
                for p in ps {
                    self.visit_pattern(p);
                }
            }
            Pattern::MatchMapping { keys, patterns, rest } => {
                for k in keys {
                    self.visit_expr(k);
                }
                for p in patterns {
                    self.visit_pattern(p);
                }
                if let Some(r) = rest {
                    self.def(r);
                }
            }
            Pattern::MatchClass { cls, patterns, kwd_patterns, .. } => {
                self.visit_expr(cls);
                for p in patterns.iter().chain(kwd_patterns.iter()) {
                    self.visit_pattern(p);
                }
            }
            Pattern::MatchStar(n) => {
                if let Some(n) = n {
                    self.def(n);
                }
            }
            Pattern::MatchAs { pattern, name } => {
                if let Some(p) = pattern {
                    self.visit_pattern(p);
                }
                if let Some(n) = name {
                    self.def(n);
                }
            }
        }
    }

    fn visit_comp(&mut self, e: &Expr, gens: &[Comprehension], elts: &[&Expr], name: &str, is_genexp: bool) {
        self.visit_expr(&gens[0].iter);
        let key = e as *const Expr as usize;
        let id = self.new_scope(ScopeKind::Function, name.into(), Some(key), true);
        self.scopes[id].is_genexp = is_genexp;
        self.scopes[id].is_gen = is_genexp;
        let saved = self.cur;
        self.cur = id;
        self.add(id, ".0", DEF_PARAM);
        self.scopes[id].params.push(".0".into());
        for (i, g) in gens.iter().enumerate() {
            if i > 0 {
                self.visit_expr(&g.iter);
            }
            if g.is_async {
                self.scopes[id].is_async = true;
            }
            self.visit_expr(&g.target);
            for c in &g.ifs {
                self.visit_expr(c);
            }
        }
        for el in elts {
            self.visit_expr(el);
        }
        self.cur = saved;
    }

    fn visit_expr(&mut self, e: &Expr) {
        self.line = e.pos.line;
        match &e.kind {
            ExprKind::BoolOp { values, .. } => {
                for v in values {
                    self.visit_expr(v);
                }
            }
            ExprKind::NamedExpr { target, value } => {
                self.visit_expr(value);
                if let ExprKind::Name { id, .. } = &target.kind {
                    let mut c = self.cur;
                    let mut path = Vec::new();
                    while self.scopes[c].is_comp {
                        path.push(c);
                        c = self.parent_of(c);
                    }
                    let target_kind = self.scopes[c].kind;
                    for p in &path {
                        if target_kind == ScopeKind::Function {
                            self.add(*p, id, DEF_NONLOCAL);
                        } else {
                            self.add(*p, id, DEF_GLOBAL);
                        }
                    }
                    if target_kind == ScopeKind::Function {
                        self.add(c, id, DEF_LOCAL);
                    } else {
                        let cur = self.cur;
                        if path.is_empty() {
                            self.add(cur, id, DEF_LOCAL);
                        }
                    }
                } else {
                    self.visit_expr(target);
                }
            }
            ExprKind::BinOp { left, right, .. } => {
                self.visit_expr(left);
                self.visit_expr(right);
            }
            ExprKind::UnaryOp { operand, .. } => self.visit_expr(operand),
            ExprKind::Lambda { args, body } => {
                self.visit_arg_exprs(args);
                let key = e as *const Expr as usize;
                let id = self.new_scope(ScopeKind::Function, "<lambda>".into(), Some(key), false);
                self.scopes[id].is_lambda = true;
                let saved = self.cur;
                self.cur = id;
                self.visit_params(args);
                self.visit_expr(body);
                self.cur = saved;
            }
            ExprKind::IfExp { test, body, orelse } => {
                self.visit_expr(test);
                self.visit_expr(body);
                self.visit_expr(orelse);
            }
            ExprKind::Dict { keys, values } => {
                for k in keys.iter().flatten() {
                    self.visit_expr(k);
                }
                for v in values {
                    self.visit_expr(v);
                }
            }
            ExprKind::Set(es) => {
                for x in es {
                    self.visit_expr(x);
                }
            }
            ExprKind::ListComp { elt, generators } => self.visit_comp(e, generators, &[elt], "<listcomp>", false),
            ExprKind::SetComp { elt, generators } => self.visit_comp(e, generators, &[elt], "<setcomp>", false),
            ExprKind::DictComp { key, value, generators } => {
                self.visit_comp(e, generators, &[key, value], "<dictcomp>", false)
            }
            ExprKind::GeneratorExp { elt, generators } => self.visit_comp(e, generators, &[elt], "<genexpr>", true),
            ExprKind::Await(v) => {
                self.visit_expr(v);
            }
            ExprKind::Yield(v) => {
                if let Some(v) = v {
                    self.visit_expr(v);
                }
                self.mark_gen();
            }
            ExprKind::YieldFrom(v) => {
                self.visit_expr(v);
                self.mark_gen();
            }
            ExprKind::Compare { left, comparators, .. } => {
                self.visit_expr(left);
                for c in comparators {
                    self.visit_expr(c);
                }
            }
            ExprKind::Call { func, args, keywords } => {
                if let ExprKind::Name { id, .. } = &func.kind {
                    if &**id == "super" && self.scopes[self.cur].kind == ScopeKind::Function {
                        let cur = self.cur;
                        self.add(cur, "__class__", DEF_USE);
                    }
                }
                self.visit_expr(func);
                for a in args {
                    self.visit_expr(a);
                }
                for k in keywords {
                    self.visit_expr(&k.value);
                }
            }
            ExprKind::JoinedStr(parts) => {
                for p in parts {
                    self.visit_expr(p);
                }
            }
            ExprKind::FormattedValue { value, format_spec, .. } => {
                self.visit_expr(value);
                if let Some(f) = format_spec {
                    self.visit_expr(f);
                }
            }
            ExprKind::Constant(_) => {}
            ExprKind::Attribute { value, .. } => self.visit_expr(value),
            ExprKind::Subscript { value, slice, .. } => {
                self.visit_expr(value);
                self.visit_expr(slice);
            }
            ExprKind::Starred { value, .. } => self.visit_expr(value),
            ExprKind::Name { id, ctx } => {
                let cur = self.cur;
                match ctx {
                    Ctx::Load => self.add(cur, id, DEF_USE),
                    Ctx::Store | Ctx::Del => self.def(id),
                }
            }
            ExprKind::List { elts, .. } | ExprKind::Tuple { elts, .. } => {
                for x in elts {
                    self.visit_expr(x);
                }
            }
            ExprKind::Slice { lower, upper, step } => {
                for x in [lower, upper, step].into_iter().flatten() {
                    self.visit_expr(x);
                }
            }
        }
    }

    fn parent_of(&self, id: usize) -> usize {
        (0..id).rev().find(|&p| self.scopes[p].children.contains(&id)).unwrap_or(0)
    }

    fn mark_gen(&mut self) {
        let c = self.cur;
        self.scopes[c].is_gen = true;
        if self.scopes[c].kind == ScopeKind::Module {
            self.error("'yield' outside function".into());
        }
    }
}
